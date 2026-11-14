use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use rusqlite::{params_from_iter, Connection, OptionalExtension};

use crate::memory_import::package::Platform;
use crate::memory_import::{search_imported, ImportedMatch};
use crate::search_documents::{
    self, EvidenceRef, SearchDocument, SearchDocumentError, SearchDocumentKind, SourceRef,
};

use super::filtering::query_asks_for_latest;
use super::lexical::ConceptWeight;
use super::retrieval::{is_meeting_cue, FtsSearchHit};
use super::terms::{normalize, query_concepts, Concept, MatchText};
use super::{
    excerpt, lexical_terms, search_cold_partitions, search_meetings_in_range, ColdScan,
    ColdScanStats, MemoryQueryFocus, MemorySourceFamily, QueryAnchor,
    SearchDepth, SearchRange, SourceConstraint, DEFAULT_MAX_COLD_PARTITIONS,
};

const RRF_K: f64 = 60.0;
const DEFAULT_PER_SOURCE_LIMIT: usize = 40;
const DEFAULT_EXCERPT_CHARS: usize = 480;
const MIN_QUALITY_WEIGHT: f64 = 0.2;
const CORROBORATION_RATE: f64 = 0.1;
const MAX_CORROBORATION_FRACTION: f64 = 0.1;
const SCORE_LANE_COUNT: usize = 8;
/// Imported claims (#391) rank below observed memory at equal match quality: another assistant's
/// summary is weaker evidence than something SHOGUN saw, so it fills gaps rather than displacing.
const IMPORTED_SCORE_WEIGHT: f64 = 0.5;
const SOURCE_PREFERENCE_BOOST: f64 = 0.1;
const MAX_ANSWER_ANCHORS: usize = 3;
const MIN_RARE_ANCHOR_CHARS: usize = 10;
// Stay below SQLite's default 999 bind-variable limit while retaining a little headroom for
// future predicates. Evidence sets can contain every event in a long meeting.
const EVIDENCE_BIND_CHUNK_SIZE: usize = 900;

/// Derived facts below this confidence remain searchable as raw evidence, but are not returned as
/// facts. This matches Fusion's existing Low band (`confidence < 0.5`).
pub const MIN_FACT_CONFIDENCE: f64 = 0.5;

/// Closed retrieval lanes. `Derived` retains the canonical document kind for attribution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FederatedSourceKind {
    UserNote,
    Event,
    Derived(SearchDocumentKind),
    Meeting,
    /// A claim the user imported from another assistant (#391), labelled with its origin. Never
    /// observed capture: capped below High, never answer-eligible, always read as `possibly`.
    Imported(Platform),
}

impl FederatedSourceKind {
    /// True for an imported claim — the one kind that may never be stated as fact.
    pub const fn is_imported(&self) -> bool {
        matches!(self, Self::Imported(_))
    }
}

/// Typed source identity. Derived hits name the canonical owner, not a rebuildable projection id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FederatedSourceId {
    UserNote(i64),
    Event(i64),
    Derived(SourceRef),
    Meeting(i64),
    /// `memory_import_items.id`.
    Imported(i64),
}

/// One source retained after a logical cluster is deduplicated.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FederatedSource {
    pub kind: FederatedSourceKind,
    pub id: FederatedSourceId,
}

/// Canonical provenance identity, with namespaces kept distinct.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CanonicalEvidenceId {
    UserNote(i64),
    Event(i64),
    Session(i64),
    TranscriptSegment(i64),
    SessionNote(i64),
    MeetingRecap(i64),
    Brief(String),
    FeedbackEvent(i64),
    PeriodSummary(i64),
    /// `memory_imports.id` — the import record an imported claim points at (never an event).
    MemoryImport(i64),
}

/// One-based ranks from every retrieval component. `None` means the lane did not find the hit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComponentRanks {
    pub event_fts: Option<usize>,
    pub derived_fts: Option<usize>,
    pub derived_knn: Option<usize>,
    pub meeting: Option<usize>,
    pub warm_knn: Option<usize>,
    pub cold_scan: Option<usize>,
    pub imported: Option<usize>,
}

impl ComponentRanks {
    /// A vector lane found this hit: evidence that shares meaning, not necessarily words, so the
    /// lexical relevance floor does not judge it.
    const fn semantic(&self) -> bool {
        self.warm_knn.is_some() || self.cold_scan.is_some() || self.derived_knn.is_some()
    }

    fn merge(&mut self, other: Self) {
        self.event_fts = min_rank(self.event_fts, other.event_fts);
        self.derived_fts = min_rank(self.derived_fts, other.derived_fts);
        self.derived_knn = min_rank(self.derived_knn, other.derived_knn);
        self.meeting = min_rank(self.meeting, other.meeting);
        self.warm_knn = min_rank(self.warm_knn, other.warm_knn);
        self.cold_scan = min_rank(self.cold_scan, other.cold_scan);
        self.imported = min_rank(self.imported, other.imported);
    }
}

fn min_rank(left: Option<usize>, right: Option<usize>) -> Option<usize> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(rank), None) | (None, Some(rank)) => Some(rank),
        (None, None) => None,
    }
}

/// Storage-tier attribution. `canonical` covers lexical canonical rows; semantic flags show which
/// vector tiers also contributed. Default federated search always leaves `cold` false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TierMetadata {
    pub canonical: bool,
    pub warm: bool,
    pub cold: bool,
}

impl TierMetadata {
    fn merge(&mut self, other: Self) {
        self.canonical |= other.canonical;
        self.warm |= other.warm;
        self.cold |= other.cold;
    }
}

/// Typed, provenance-preserving result from all local retrieval lanes.
#[derive(Debug, Clone, PartialEq)]
pub struct FederatedHit {
    pub source_kind: FederatedSourceKind,
    pub source_id: FederatedSourceId,
    /// Every source collapsed into this logical result, stable-sorted and deduplicated.
    pub sources: Vec<FederatedSource>,
    pub evidence_ids: Vec<CanonicalEvidenceId>,
    pub title: String,
    pub excerpt: String,
    pub confidence: f64,
    pub period_start_ms: Option<i64>,
    pub period_end_ms: Option<i64>,
    pub component_ranks: ComponentRanks,
    pub fused_score: f64,
    pub tiers: TierMetadata,
    /// False hits may remain useful suggestions but cannot support an assertive answer.
    pub answer_eligible: bool,
    /// Content-free P0 quality inputs used by the answerability layer.
    pub match_quality: MatchQualitySummary,
}

impl FederatedHit {
    /// Event ids behind this hit: its own event first, then evidence events newest first.
    pub fn event_ids(&self, max: usize) -> Vec<i64> {
        let own = match self.source_id {
            FederatedSourceId::Event(id) => Some(id),
            _ => None,
        };
        let evidence = self.evidence_ids.iter().rev().filter_map(|evidence| match evidence {
            CanonicalEvidenceId::Event(id) if Some(*id) != own => Some(*id),
            _ => None,
        });
        own.into_iter().chain(evidence).take(max).collect()
    }
}

/// When and in which app/window one event was captured (`when` / `where` on API hits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPlace {
    pub ts: i64,
    pub app: Option<String>,
    pub window_title: Option<String>,
}

/// One batched read of the event rows behind search hits. Missing ids are simply absent.
pub fn event_places(
    conn: &Connection,
    ids: &[i64],
) -> Result<BTreeMap<i64, EventPlace>, rusqlite::Error> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let sql = format!(
        "SELECT id, ts, app_bundle_id, window_title FROM event_log WHERE id IN ({})",
        vec!["?"; ids.len()].join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(ids), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            EventPlace {
                ts: row.get(1)?,
                app: row.get(2)?,
                window_title: row.get(3)?,
            },
        ))
    })?;
    rows.collect()
}

/// Relevance floor (E12-T1): the share of the question's content words — weighted by idf, source
/// cues like "meeting" never required — that the best hit must contain before the result counts
/// as evidence (`relevance_floor` has the exact rule).
///
/// Every search measures it and reports the verdict in [`FederatedSearchResult::relevance`];
/// whether a failing verdict also empties the result is the caller's choice
/// ([`FederatedSearchOptions::abstain_below_floor`]). With it set, a question nothing in memory
/// answers ("what did we decide about the kestrel compliance audit" over a log that only ever
/// mentions kestrel) comes back empty instead of handing a downstream agent rows that merely
/// share a project name. Without it the best candidates are still returned and the verdict
/// travels with them, so a suggestion surface keeps its candidates and reports the doubt
/// through its answerability signal instead. Calibrated on `memory-bench --workload
/// abstention`, where the best chatter row covers one of four words (0.25) or two of four
/// ("hardware budget" against "budget spreadsheet for kestrel", 0.50 with two words missing),
/// against the answer-bearing workloads whose answers cover two of three or more (≥ 0.67), and
/// checked against the golden search eval (`docs/shogunai-origin/search-eval.md`), where no
/// positive case may lose its result; see `docs/benchmarks/README.md`.
pub const RELEVANCE_FLOOR_COVERAGE: f64 = 0.5;

/// Upper bound of the newest-first boost applied when the question asks for the latest state
/// ([`query_asks_for_latest`] or [`FederatedSearchOptions::prefer_latest`]): among the rows that
/// match the question equally well, the newest scores `1 + LATEST_BOOST` times the oldest, which
/// outweighs any rank gap inside one lane (rank 1 against rank 40 is ×1.64), so order within that
/// group follows time rather than BM25 length normalisation.
const LATEST_BOOST: f64 = 1.0;

/// Why the relevance floor let a result through or blanked it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelevanceFloorReason {
    /// The best hit covered enough of the question.
    Passed,
    /// The question has no content words (only stop words, dates or source cues), so coverage
    /// is undefined and the floor does not apply.
    NotApplicable,
    /// No lane produced a candidate at all.
    NoCandidates,
    /// Candidates existed but none covered enough of the question: no more than
    /// [`RELEVANCE_FLOOR_COVERAGE`] of its weight, and a tie at the floor with two or more words
    /// missing (`relevance_floor` has the exact rule).
    BelowFloor,
}

/// The relevance floor's verdict for one search. `passed == false` means the best candidate did
/// not cover enough of the question to count as evidence: with
/// [`FederatedSearchOptions::abstain_below_floor`] the result is then empty on purpose; without
/// it the candidates are returned anyway and the caller maps the verdict onto its own signal
/// (`AnswerabilityV1 { answerable: false, .. }` in the Memory API).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct RelevanceFloor {
    pub passed: bool,
    pub reason: RelevanceFloorReason,
    /// idf-weighted share of the question's content words the best candidate contained, `None`
    /// when there was nothing to measure.
    pub coverage: Option<f64>,
    pub threshold: f64,
}

impl RelevanceFloor {
    /// The verdict of a search that measured nothing: an empty question, or a result assembled
    /// without running the search at all.
    pub const fn not_applicable() -> Self {
        Self {
            passed: true,
            reason: RelevanceFloorReason::NotApplicable,
            coverage: None,
            threshold: RELEVANCE_FLOOR_COVERAGE,
        }
    }
}

/// Bounded, content-free match measurements retained from P0 scoring.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MatchQualitySummary {
    pub lexical_quality: f64,
    pub term_coverage: f64,
    pub anchor_coverage: f64,
    pub anchor_count: usize,
}

/// Federated retrieval controls. Default is Warm-only and therefore never opens Cold storage.
#[derive(Debug, Clone)]
pub struct FederatedSearchOptions {
    pub since_ts: Option<i64>,
    pub to_ts: Option<i64>,
    pub focus: MemoryQueryFocus,
    pub source: SourceConstraint,
    pub anchors: Vec<QueryAnchor>,
    pub depth: SearchDepth,
    pub max_cold_partitions: usize,
    pub min_derived_confidence: f64,
    pub per_source_limit: usize,
    pub excerpt_chars: usize,
    /// Search raw event FTS across all history. Derived, meeting, and vector lanes remain inside
    /// `since_ts..to_ts`, avoiding archive-wide scans outside bounded indexed event text.
    pub widen_event_lexical: bool,
    /// Order the rows that match the question equally well newest-first (by when each was last
    /// seen), as a question containing "latest" / "currently" / 最新 does by itself
    /// ([`query_asks_for_latest`]). A caller that knows the question is about the present state
    /// — a "what is the current value" tool, the temporal benchmark — sets it explicitly.
    pub prefer_latest: bool,
    /// Return no hits at all when the best candidate falls below the relevance floor
    /// ([`RELEVANCE_FLOOR_COVERAGE`]). Off by default: the floor is always measured and reported
    /// in [`FederatedSearchResult::relevance`], but the candidates are returned, so a surface that
    /// shows suggestions (the daemon's context assembly, `memory.search` with its answerability
    /// verdict) keeps them and says how little they cover. A caller whose consumer would read any
    /// row as evidence — the abstention benchmark, an agent that cannot weigh the verdict — opts
    /// in and gets an empty result instead.
    pub abstain_below_floor: bool,
    /// The vector lane's relevance cutoff (largest L2 distance that counts as a semantic match).
    /// `None` reads it from the store's active embedding model
    /// ([`crate::embed::max_l2_distance_for`], a property of the model's [`crate::embed::ModelSpec`]);
    /// a value overrides it (calibration runs). Irrelevant without a query embedding.
    pub max_vector_distance: Option<f32>,
}

impl Default for FederatedSearchOptions {
    fn default() -> Self {
        Self {
            since_ts: None,
            to_ts: None,
            focus: MemoryQueryFocus::Any,
            source: SourceConstraint::Any,
            anchors: Vec::new(),
            depth: SearchDepth::WarmOnly,
            max_cold_partitions: DEFAULT_MAX_COLD_PARTITIONS,
            min_derived_confidence: MIN_FACT_CONFIDENCE,
            per_source_limit: DEFAULT_PER_SOURCE_LIMIT,
            excerpt_chars: DEFAULT_EXCERPT_CHARS,
            widen_event_lexical: false,
            prefer_latest: false,
            abstain_below_floor: false,
            max_vector_distance: None,
        }
    }
}

/// Federated hits plus Cold and latency accounting. Latency measures this synchronous local call.
#[derive(Debug, Clone)]
pub struct FederatedSearchResult {
    pub hits: Vec<FederatedHit>,
    /// Raw event candidates before cross-source clustering/truncation. Legacy in-process callers
    /// consume this lane while typed API callers retain the richer cluster-primary result.
    pub raw_event_ranks: Vec<(i64, f64)>,
    /// True only when bounded archive-wide raw-event BM25 completed.
    pub event_lexical_all_history: bool,
    /// Whether the best candidate covered enough of the question to count as evidence. When it
    /// did not and the caller asked to abstain ([`FederatedSearchOptions::abstain_below_floor`]),
    /// `hits` and `raw_event_ranks` are empty on purpose; otherwise they hold the best candidates
    /// and this verdict says how little of the question they cover.
    pub relevance: RelevanceFloor,
    pub cold: ColdScanStats,
    pub elapsed_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CandidateKey {
    UserNote(i64),
    Event(i64),
    Derived(i64),
    Meeting(i64),
    Imported(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ClusterKey {
    Session(i64),
    Candidate(CandidateKey),
}

struct Candidate {
    key: CandidateKey,
    source: FederatedSource,
    evidence: BTreeSet<CanonicalEvidenceId>,
    title: String,
    body: String,
    confidence: f64,
    period_start_ms: Option<i64>,
    period_end_ms: Option<i64>,
    /// Events only: `event_log.last_seen_at`. `insert_or_touch` collapses a byte-identical
    /// restatement onto its first row and advances this while `ts` stays at the first sighting,
    /// so the value asserted most recently can carry the *oldest* `ts`. Newest-first ordering
    /// reads this, never `ts` alone.
    last_seen_ms: Option<i64>,
    ranks: ComponentRanks,
    lane_scores: [f64; SCORE_LANE_COUNT],
    lexical_coverage: f64,
    answerability_coverage: f64,
    lexical_quality: f64,
    anchor_coverage: f64,
    anchor_count: usize,
    normalized_text: String,
    /// Per query concept: does this candidate's text match it (`terms.rs`).
    concept_hits: Vec<bool>,
    /// Source-quality prior: distilled memory and connector text over raw window / OCR chrome.
    source_weight: f64,
    answer_eligible: bool,
    score: f64,
    tiers: TierMetadata,
}

struct MeetingDetails {
    ended_at: Option<i64>,
    confidence: f64,
    evidence: BTreeSet<CanonicalEvidenceId>,
}

impl Candidate {
    fn cluster_key(&self) -> ClusterKey {
        self.evidence
            .iter()
            .find_map(|evidence| match evidence {
                CanonicalEvidenceId::Session(id) => Some(ClusterKey::Session(*id)),
                _ => None,
            })
            .unwrap_or_else(|| ClusterKey::Candidate(self.key.clone()))
    }

    fn record_score(&mut self, lane: ScoreLane, contribution: f64) {
        self.lane_scores[lane.index()] = sanitize_nonnegative(contribution);
        self.score = strongest_with_capped_corroboration(self.lane_scores.iter().copied());
    }

    fn record_quality(&mut self, quality: f64) {
        self.lexical_quality = self
            .lexical_quality
            .max(sanitize_nonnegative(quality).min(1.0));
    }

    /// When this candidate was last asserted: the later of its last sighting and its period end.
    fn latest_ms(&self) -> Option<i64> {
        [self.last_seen_ms, self.period_end_ms, self.period_start_ms]
            .into_iter()
            .flatten()
            .max()
    }

    fn scale(&mut self, multiplier: f64) {
        if multiplier != 1.0 {
            self.score *= multiplier;
            for score in &mut self.lane_scores {
                *score *= multiplier;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ScoreLane {
    UserNote,
    EventFts,
    DerivedFts,
    DerivedKnn,
    Meeting,
    WarmKnn,
    ColdScan,
    Imported,
}

impl ScoreLane {
    const fn index(self) -> usize {
        match self {
            Self::UserNote => 7,
            Self::EventFts => 0,
            Self::DerivedFts => 1,
            Self::DerivedKnn => 2,
            Self::Meeting => 3,
            Self::WarmKnn => 4,
            Self::ColdScan => 5,
            Self::Imported => 6,
        }
    }
}

/// Fuse event FTS, derived FTS, meeting search, Warm KNN, and explicitly requested Cold scan.
///
/// RRF uses fixed component names and one-based ranks. Final ties use typed source identity, so
/// identical inputs return byte-for-byte identical hit order even when SQL/HashMap iteration does
/// not. Meeting recap, meeting search, and raw meeting rows collapse on canonical `session_id`;
/// all source and evidence identities survive in the returned cluster.
pub fn search_federated(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&[f32]>,
    now_ms: i64,
    options: &FederatedSearchOptions,
    limit: usize,
) -> Result<FederatedSearchResult, SearchDocumentError> {
    let started = Instant::now();
    if query.trim().is_empty() || limit == 0 {
        return Ok(FederatedSearchResult {
            hits: Vec::new(),
            raw_event_ranks: Vec::new(),
            event_lexical_all_history: false,
            relevance: RelevanceFloor::not_applicable(),
            cold: ColdScanStats::default(),
            elapsed_micros: elapsed_micros(started),
        });
    }

    let source_limit = options.per_source_limit.max(limit).min(100);
    let raw_lanes_allowed = !matches!(options.source, SourceConstraint::Require(_));
    let meeting_lane_allowed = !matches!(
        options.source,
        SourceConstraint::Require(family) if family != MemorySourceFamily::Meeting
    );
    let document_focus = required_document_focus(options.source).unwrap_or(options.focus);
    // Either bound constrains candidate selection. An upper-only range must also reach the
    // lexical, Warm-vector and meeting lanes before their per-source limits are applied.
    let range = (options.since_ts.is_some() || options.to_ts.is_some()).then(|| SearchRange {
        from_ms: options.since_ts.unwrap_or(i64::MIN),
        to_ms: options.to_ts.unwrap_or(now_ms).min(now_ms),
    });
    // Raw-event lexical lane (`lexical.rs`): bounded AND-first / relaxed-OR retrieval over the
    // trigram and gram indexes, scored on one BM25 scale. Widening searches all history; an
    // explicit range stays exact.
    let (event_fts_hits, event_lexical_all_history, concept_weights) = if !raw_lanes_allowed {
        (Vec::new(), false, Vec::new())
    } else {
        let scope = if options.widen_event_lexical { None } else { range };
        let lexical = super::lexical::search_events(conn, query, scope, source_limit)?;
        let all_history = scope.is_none() && options.widen_event_lexical && lexical.complete;
        (lexical.hits, all_history, lexical.concept_weights)
    };
    let event_ids = event_fts_hits.iter().map(|hit| hit.id).collect::<Vec<_>>();
    // The vector lane's relevance cutoff is the active model's (a property of its ModelSpec):
    // every model places unrelated text at its own typical distance.
    let cutoff = match (options.max_vector_distance, query_embedding) {
        (Some(cutoff), _) => cutoff,
        (None, Some(_)) => crate::embedding_space::active(conn)?
            .map_or(crate::embed::DEFAULT_MAX_L2_DISTANCE, |space| crate::embed::max_l2_distance_for(&space.model_id)),
        (None, None) => crate::embed::DEFAULT_MAX_L2_DISTANCE,
    };
    let warm_hits = if raw_lanes_allowed {
        accepted_vector_hits(cutoff, match query_embedding {
            Some(embedding) => match range {
                Some(range) => crate::vector::knn_range_scored(
                    conn,
                    embedding,
                    range.from_ms,
                    range.to_ms,
                    source_limit,
                )?,
                None => crate::vector::knn_scored(conn, embedding, source_limit)?,
            },
            None => Vec::new(),
        })
    } else {
        Vec::new()
    };
    let warm_ids = warm_hits.iter().map(|hit| hit.id).collect::<Vec<_>>();
    let cold = if raw_lanes_allowed {
        cold_candidates(conn, query_embedding, now_ms, options, source_limit)?
    } else {
        ColdScan::default()
    };
    let document_candidates = super::lexical::search_documents(
        conn,
        query,
        options.since_ts,
        options.to_ts.map(|to| to.min(now_ms)),
        document_focus,
        source_limit,
    )?;
    let document_vector_hits = accepted_vector_hits(cutoff, match query_embedding {
        Some(embedding) => crate::document_vector::knn_filtered_scored(
            conn,
            embedding,
            options.since_ts,
            options.to_ts.map(|to| to.min(now_ms)),
            document_focus,
            source_limit,
        )?,
        None => Vec::new(),
    });
    let document_vector_ids = document_vector_hits
        .iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>();
    let meetings = if meeting_lane_allowed {
        search_meetings_in_range(conn, query, range, source_limit)?
    } else {
        Vec::new()
    };
    // Imported claims (#391) carry no period, like undated derived documents, so a time range does
    // not exclude them; an explicit source family or a meeting/brief/period/thread focus does.
    let imported = if imported_lane_allowed(options) {
        search_imported(
            conn,
            &lexical_terms(query),
            options.min_derived_confidence.max(MIN_FACT_CONFIDENCE),
            source_limit,
        )?
    } else {
        Vec::new()
    };

    let mut candidates: BTreeMap<CandidateKey, Candidate> = BTreeMap::new();
    hydrate_events(
        conn,
        query,
        options.excerpt_chars,
        &event_ids,
        &mut candidates,
    )?;
    hydrate_events(
        conn,
        query,
        options.excerpt_chars,
        &warm_ids,
        &mut candidates,
    )?;
    hydrate_events(
        conn,
        query,
        options.excerpt_chars,
        &cold.ids,
        &mut candidates,
    )?;
    let mut document_ids = document_candidates
        .iter()
        .map(|candidate| candidate.id)
        .collect::<Vec<_>>();
    document_ids.extend(document_vector_ids.iter().copied());
    let accepted_documents =
        hydrate_derived(conn, query, options, now_ms, &document_ids, &mut candidates)?;
    let accepted_meetings =
        hydrate_meetings(conn, query, options, now_ms, &meetings, &mut candidates)?;
    hydrate_imported(query, options.excerpt_chars, &imported, &mut candidates);
    if imported_lane_allowed(options) {
        for (index, note) in crate::memory_notes::search(conn, query, source_limit)?.into_iter().enumerate() {
            let key=CandidateKey::UserNote(note.id);
            let matched=match_fields(&query_concepts(query),&[&note.text]);
            let coverage=matched.lexical_coverage;
            let mut candidate=Candidate {
                key: key.clone(),
                source: FederatedSource { kind: FederatedSourceKind::UserNote, id: FederatedSourceId::UserNote(note.id) },
                evidence: BTreeSet::from([CanonicalEvidenceId::UserNote(note.id)]),
                title: "User memory".into(), body: excerpt(&note.text,query,options.excerpt_chars),
                confidence: 1.0, period_start_ms: None, period_end_ms: None, last_seen_ms: None,
                ranks: ComponentRanks::default(), lane_scores: [0.0;SCORE_LANE_COUNT],
                lexical_coverage: coverage, answerability_coverage: matched.answerability_coverage,
                lexical_quality: coverage, anchor_coverage: 1.0, anchor_count: 0,
                normalized_text: matched.normalized_text, concept_hits: matched.concept_hits,
                source_weight: DISTILLED_SOURCE_WEIGHT, answer_eligible: true, score: 0.0,
                tiers: TierMetadata { canonical:true, ..TierMetadata::default() },
            };
            candidate.record_score(ScoreLane::UserNote,quality_weighted_rrf(index+1,coverage));
            candidates.insert(key,candidate);
        }
    }

    apply_event_fts_ranks(&event_fts_hits, &mut candidates);
    apply_vector_ranks(&warm_hits, RankLane::Warm, cutoff, &mut candidates);
    apply_ranks(&cold.ids, RankLane::Cold, &mut candidates);
    apply_document_ranks(&document_candidates, &accepted_documents, &mut candidates);
    apply_document_vector_ranks(&document_vector_hits, &accepted_documents, cutoff, &mut candidates);
    apply_meeting_ranks(&meetings, &accepted_meetings, &mut candidates);
    apply_imported_ranks(&imported, &mut candidates);

    let concepts = query_concepts(query);
    let anchors = reliable_anchors(query, &options.anchors, &concepts, candidates.values());
    apply_evidence_resolution(conn, &mut candidates)?;
    for candidate in candidates.values_mut() {
        candidate.anchor_coverage = anchor_coverage(&anchors, candidate);
        candidate.anchor_count = anchors.len();
        let multiplier = source_intent_multiplier(options.source, &candidate.source.kind)
            * prior_multiplier(candidate, now_ms);
        candidate.scale(multiplier);
    }
    if options.prefer_latest || query_asks_for_latest(query) {
        apply_latest_ordering(&mut candidates);
    }
    let (relevance, clearing) =
        relevance_floor(&concepts, &concept_weights, &candidates, options, limit);
    if !relevance.passed && options.abstain_below_floor {
        return Ok(FederatedSearchResult {
            hits: Vec::new(),
            raw_event_ranks: Vec::new(),
            event_lexical_all_history,
            relevance,
            cold: cold.stats,
            elapsed_micros: elapsed_micros(started),
        });
    }
    // E51: the verdict passing says some candidate is evidence, not that every one is. A caller
    // that abstains below the floor reads every returned row as evidence, so each row must clear
    // the floor on its own: a row that only shares the project name does not ride along with an
    // unrelated row that passes. A cluster (rows of one session) stays when any member clears
    // it, since its members are the same evidence seen through different lanes.
    if options.abstain_below_floor {
        if let Some(clearing) = clearing {
            let kept_clusters = candidates
                .values()
                .filter(|candidate| clearing.contains(&candidate.key))
                .map(Candidate::cluster_key)
                .collect::<BTreeSet<_>>();
            candidates.retain(|_, candidate| kept_clusters.contains(&candidate.cluster_key()));
        }
    }

    let mut raw_event_ranks = candidates
        .values()
        .filter_map(|candidate| match candidate.source.id {
            FederatedSourceId::Event(event_id) if candidate.score > 0.0 => {
                Some((event_id, candidate.score))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    raw_event_ranks.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut clusters: BTreeMap<ClusterKey, Vec<Candidate>> = BTreeMap::new();
    for candidate in candidates.into_values().filter(|candidate| {
        candidate.score > 0.0 && source_accepts(options.source, &candidate.source.kind)
    }) {
        clusters
            .entry(candidate.cluster_key())
            .or_default()
            .push(candidate);
    }
    let mut hits = clusters
        .into_values()
        .map(|cluster| merge_cluster(query, options.excerpt_chars, cluster))
        .filter(|hit| focus_accepts(options.focus, &hit.source_kind))
        .collect::<Vec<_>>();
    scope_hits(conn, crate::actor::current_read(), &mut hits, &mut raw_event_ranks)?;
    raw_event_ranks.truncate(limit);
    hits.sort_by(|left, right| {
        right
            .fused_score
            .total_cmp(&left.fused_score)
            .then_with(|| {
                source_priority(&left.source_kind).cmp(&source_priority(&right.source_kind))
            })
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    hits.truncate(limit);

    Ok(FederatedSearchResult {
        hits,
        raw_event_ranks,
        event_lexical_all_history,
        relevance,
        cold: cold.stats,
        elapsed_micros: elapsed_micros(started),
    })
}

/// Every event id a hit rests on: its own, its collapsed sources', and its evidence.
pub fn hit_event_ids(hit: &FederatedHit) -> impl Iterator<Item = i64> + '_ {
    let own = match hit.source_id {
        FederatedSourceId::Event(id) => Some(id),
        _ => None,
    };
    let sources = hit.sources.iter().filter_map(|source| match source.id {
        FederatedSourceId::Event(id) => Some(id),
        _ => None,
    });
    let evidence = hit.evidence_ids.iter().filter_map(|id| match id {
        CanonicalEvidenceId::Event(id) => Some(*id),
        _ => None,
    });
    own.into_iter().chain(sources).chain(evidence)
}

/// E2-T5: leave out every hit (and raw event rank) resting on an event outside the call's read
/// scope — an agent's scratch row under the default world-only scope. Runs before the limit cut,
/// so a scratch hit never costs a world result its place. A hit that touches one scratch event
/// goes whole: its excerpt may quote it.
fn scope_hits(
    conn: &Connection,
    scope: crate::actor::ReadScope,
    hits: &mut Vec<FederatedHit>,
    raw_event_ranks: &mut Vec<(i64, f64)>,
) -> Result<(), rusqlite::Error> {
    if scope.unrestricted() {
        return Ok(());
    }
    let ids: BTreeSet<i64> = hits
        .iter()
        .flat_map(hit_event_ids)
        .chain(raw_event_ranks.iter().map(|(id, _)| *id))
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let out = crate::event_log::out_of_scope_ids(conn, &ids.into_iter().collect::<Vec<_>>(), scope)?;
    let note_ids: Vec<i64> = hits.iter().flat_map(hit_note_ids).collect();
    let notes_out = crate::memory_notes::out_of_scope_ids(conn, &note_ids, scope)?;
    if out.is_empty() && notes_out.is_empty() {
        return Ok(());
    }
    hits.retain(|hit| {
        !hit_event_ids(hit).any(|id| out.contains(&id)) && !hit_note_ids(hit).any(|id| notes_out.contains(&id))
    });
    raw_event_ranks.retain(|(id, _)| !out.contains(id));
    Ok(())
}

/// Every user-note id a hit rests on: its own, its collapsed sources', and its evidence.
fn hit_note_ids(hit: &FederatedHit) -> impl Iterator<Item = i64> + '_ {
    let own = match hit.source_id {
        FederatedSourceId::UserNote(id) => Some(id),
        _ => None,
    };
    let sources = hit.sources.iter().filter_map(|source| match source.id {
        FederatedSourceId::UserNote(id) => Some(id),
        _ => None,
    });
    let evidence = hit.evidence_ids.iter().filter_map(|id| match id {
        CanonicalEvidenceId::UserNote(id) => Some(*id),
        _ => None,
    });
    own.into_iter().chain(sources).chain(evidence)
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Newest-first ordering for a question about the present state (E12-T1).
///
/// Only the rows that match the question best — equal [`Candidate::lexical_coverage`] — take
/// part; a recent row that matches fewer of the question's words is not promoted over an older
/// exact match. Within that group the boost grows linearly with [`Candidate::latest_ms`] from ×1
/// (oldest) to ×(1 + [`LATEST_BOOST`]) (newest), so a value restated most recently outranks the
/// value it replaced even when BM25 length normalisation prefers the shorter, older sentence.
fn apply_latest_ordering(candidates: &mut BTreeMap<CandidateKey, Candidate>) {
    let best_coverage = candidates
        .values()
        .filter(|candidate| candidate.score > 0.0)
        .map(|candidate| candidate.lexical_coverage)
        .fold(0.0_f64, f64::max);
    if best_coverage <= 0.0 {
        return;
    }
    let group = candidates
        .values()
        .filter(|candidate| {
            candidate.score > 0.0 && (candidate.lexical_coverage - best_coverage).abs() < 1e-9
        })
        .filter_map(|candidate| Some((candidate.key.clone(), candidate.latest_ms()?)))
        .collect::<Vec<_>>();
    let Some(oldest) = group.iter().map(|(_, ts)| *ts).min() else {
        return;
    };
    let Some(newest) = group.iter().map(|(_, ts)| *ts).max() else {
        return;
    };
    if newest <= oldest {
        return;
    }
    let span = (newest - oldest) as f64;
    for (key, ts) in group {
        if let Some(candidate) = candidates.get_mut(&key) {
            let position = ((ts - oldest) as f64 / span).clamp(0.0, 1.0);
            candidate.scale(1.0 + LATEST_BOOST * position);
        }
    }
}

/// Does the best candidate cover enough of the question to be evidence at all?
///
/// Coverage is the idf-weighted share of the question's content concepts the candidate contains.
/// Source cues ("meeting", "standup", 会議 …) name where to look, not what for: they are never
/// required (「昨日の会議」 lists yesterday's meetings whatever their titles say) but count when
/// they match ("standup notes" is satisfied by a row titled Standup). A concept no row in scope
/// contains weighs like an average present one: it says nothing about the archive, and its
/// extreme idf would otherwise blank every question that uses a verb the log never wrote down
/// ("decide"). Without lexical weights (no raw-event lane) every concept weighs the same. A
/// candidate a vector lane found counts as full coverage: it shares meaning, not words.
///
/// CJK content is counted in the units the gram index holds (`terms::floor_units`): a
/// 2-character word is one unit, a longer compound or katakana word its bigrams, each weighing
/// what its concept weighs, and a unit matches when the candidate holds that gram. A Japanese or
/// Chinese question loses its particles to tokenisation, so counted as whole concepts
/// 「Plover 的合同有错误吗」 was two words and its project name alone made half of it. A question
/// written in two scripts is also covered by its CJK part alone (the archive holds each item in
/// one language), but never by its Latin part alone: beside CJK content the Latin words are
/// usually a project or product name, which says which thing is asked about, not what, so a
/// candidate must match at least one CJK unit (「Orchid のローンチ延期」 against an English row is
/// the vector lane's job, as for any cross-lingual question).
///
/// A candidate passes when it covers more than half of the question's weight, or exactly half
/// while missing a single unit (one of two words found is a plausible answer; two of four is a
/// question more specific than the row). The verdict considers the `limit` best by score among
/// those the source constraint accepts and passes if any of them does.
///
/// The second value is every accepted candidate that clears the floor on its own (E51), judged
/// with the same rule over all of them, not only the `limit` best, so a passing row below a
/// failing one can take its place; `None` when the floor does not apply (no content words). A
/// caller that abstains returns only these ([`FederatedSearchOptions::abstain_below_floor`]).
fn relevance_floor(
    concepts: &[Concept],
    weights: &[ConceptWeight],
    candidates: &BTreeMap<CandidateKey, Candidate>,
    options: &FederatedSearchOptions,
    limit: usize,
) -> (RelevanceFloor, Option<BTreeSet<CandidateKey>>) {
    let is_cue = |concept: &Concept| {
        super::query::is_source_cue(&concept.display) || is_meeting_cue(concept)
    };
    let content = (0..concepts.len())
        .filter(|&index| !is_cue(&concepts[index]))
        .collect::<Vec<_>>();
    if content.is_empty() {
        return (RelevanceFloor::not_applicable(), None);
    }
    let weighted = weights.len() == concepts.len();
    let present_idf = content
        .iter()
        .filter_map(|&index| weights.get(index))
        .filter(|weight| weight.present)
        .map(|weight| sanitize_nonnegative(weight.idf))
        .collect::<Vec<_>>();
    let absent_weight = if present_idf.is_empty() {
        1.0
    } else {
        present_idf.iter().sum::<f64>() / present_idf.len() as f64
    };
    let weight_of = |index: usize| -> f64 {
        if !weighted {
            return 1.0;
        }
        match weights.get(index) {
            Some(weight) if weight.present => {
                sanitize_nonnegative(weight.idf).max(f64::MIN_POSITIVE)
            }
            _ => absent_weight.max(f64::MIN_POSITIVE),
        }
    };

    // The question's content as the floor counts it: a Latin concept is one unit, a CJK concept
    // the grams the gram index holds for it, each weighing what its concept weighs.
    let units = content
        .iter()
        .flat_map(|&index| {
            let grams = super::terms::floor_units(&concepts[index]);
            let cjk = concepts[index].cjk;
            if grams.is_empty() {
                vec![FloorUnit { concept: index, gram: None, cjk }]
            } else {
                grams
                    .into_iter()
                    .map(|gram| FloorUnit { concept: index, gram: Some(gram), cjk })
                    .collect()
            }
        })
        .collect::<Vec<_>>();
    let cjk_units = (0..units.len()).filter(|&unit| units[unit].cjk).collect::<Vec<_>>();
    // Scopes: the whole question, and its CJK part on its own when it has a Latin part too. The
    // Latin part never stands alone beside CJK content: in a Japanese or Chinese question it is
    // usually a project or product name, which says which thing the question is about, not what.
    let mut scopes: Vec<Vec<usize>> = vec![(0..units.len()).collect()];
    let mixed = !cjk_units.is_empty() && cjk_units.len() < units.len();
    if mixed {
        scopes.push(cjk_units.clone());
    }

    let mut considered = candidates
        .values()
        .filter(|candidate| {
            candidate.score > 0.0 && source_accepts(options.source, &candidate.source.kind)
        })
        .collect::<Vec<_>>();
    if considered.is_empty() {
        return (
            RelevanceFloor {
                passed: false,
                reason: RelevanceFloorReason::NoCandidates,
                coverage: None,
                threshold: RELEVANCE_FLOOR_COVERAGE,
            },
            Some(BTreeSet::new()),
        );
    }
    considered.sort_by(|left, right| right.score.total_cmp(&left.score));

    let hit = |candidate: &Candidate, index: usize| {
        candidate.concept_hits.get(index).copied().unwrap_or(false)
    };
    let matched_cue_weight = |candidate: &Candidate| {
        (0..concepts.len())
            .filter(|&index| is_cue(&concepts[index]) && hit(candidate, index))
            .map(weight_of)
            .sum::<f64>()
    };
    // One candidate's verdict: does it clear the floor, and the best coverage it reached.
    let judge = |candidate: &Candidate| -> (bool, f64) {
        if candidate.ranks.semantic() {
            return (true, 1.0);
        }
        // A unit matches when its concept does, or when the candidate holds that gram (a
        // compound the row splits differently: 招聘会议 against 招聘…的会议).
        let mut grams = None;
        let unit_hits = units
            .iter()
            .map(|unit| {
                hit(candidate, unit.concept)
                    || unit.gram.as_ref().is_some_and(|gram| {
                        grams
                            .get_or_insert_with(|| {
                                super::terms::text_grams(&candidate.title, &candidate.body)
                            })
                            .contains(gram)
                    })
            })
            .collect::<Vec<_>>();
        // A mixed-script question needs some of its CJK content in the row, not only its name.
        if mixed && !cjk_units.iter().any(|&unit| unit_hits[unit]) {
            return (false, 0.0);
        }
        let cues = matched_cue_weight(candidate);
        let mut best = 0.0_f64;
        let mut clears = false;
        for (scope_index, scope) in scopes.iter().enumerate() {
            let total = scope.iter().map(|&unit| weight_of(units[unit].concept)).sum::<f64>();
            if total <= 0.0 {
                continue;
            }
            let matched = scope
                .iter()
                .filter(|&&unit| unit_hits[unit])
                .map(|&unit| weight_of(units[unit].concept))
                .sum::<f64>();
            let unmatched = scope.iter().filter(|&&unit| !unit_hits[unit]).count();
            let coverage = ((matched + cues) / total).clamp(0.0, 1.0);
            best = best.max(coverage);
            // The tie (exactly half, one unit missing) is allowed on the whole question only.
            // The CJK part on its own is judged without the name the question is about, so half
            // of it ("休暇" of 休暇 + 中止) is a row about the same topic for some other project:
            // it must cover more than half.
            let tie_allowed = scope_index == 0;
            if coverage > RELEVANCE_FLOOR_COVERAGE + 1e-9
                || (tie_allowed && coverage + 1e-9 >= RELEVANCE_FLOOR_COVERAGE && unmatched <= 1)
            {
                clears = true;
            }
        }
        (clears, best)
    };
    let mut best_coverage = 0.0_f64;
    let mut passed = false;
    let mut clearing = BTreeSet::new();
    for (position, candidate) in considered.iter().enumerate() {
        let (clears, coverage) = judge(candidate);
        if clears {
            clearing.insert(candidate.key.clone());
        }
        // The verdict (and the coverage it reports) reads only the `limit` best, and stops at
        // the first that passes, as it always has.
        if position < limit.max(1) && !passed {
            best_coverage = best_coverage.max(coverage);
            passed = clears;
        }
    }
    (
        RelevanceFloor {
            passed,
            reason: if passed {
                RelevanceFloorReason::Passed
            } else {
                RelevanceFloorReason::BelowFloor
            },
            coverage: Some(best_coverage),
            threshold: RELEVANCE_FLOOR_COVERAGE,
        },
        Some(clearing),
    )
}

/// One unit of a question's content as [`relevance_floor`] counts it.
struct FloorUnit {
    /// Index into the question's concepts; the unit weighs what this concept weighs.
    concept: usize,
    /// The gram this unit is, for a CJK concept (`terms::floor_units`); `None` for a Latin one.
    gram: Option<String>,
    cjk: bool,
}

/// Weight of distilled memory (meetings, derived facts, the user's own notes) against raw capture.
const DISTILLED_SOURCE_WEIGHT: f64 = 1.10;
/// Connector text (mail, calendar, chat, docs) — authored content, little chrome.
const CONNECTOR_SOURCE_WEIGHT: f64 = 1.05;
/// OCR of a window: the same words as the window plus its chrome, recognised imperfectly.
const OCR_SOURCE_WEIGHT: f64 = 0.85;
/// Upper bound of the recency prior: a hit from today scores at most this much more than an
/// equally good hit from long ago. Small on purpose — relevance must dominate (see
/// `ranking::apply_recency` for what an unbounded recency term did).
const RECENCY_BOOST: f64 = 0.10;
const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;
const DAY_MS: f64 = 86_400_000.0;

fn event_source_weight(source: &str) -> f64 {
    match source {
        "screen_ocr" | "screen_text" | "user_screenshot" => OCR_SOURCE_WEIGHT,
        "gmail" | "gcal" | "slack" | "notion" | "github" | "linear" | "granola" | "meeting" => {
            CONNECTOR_SOURCE_WEIGHT
        }
        _ => 1.0,
    }
}

fn derived_source_weight(kind: SearchDocumentKind) -> f64 {
    match kind {
        SearchDocumentKind::ThreadSummary | SearchDocumentKind::Lesson => CONNECTOR_SOURCE_WEIGHT,
        _ => DISTILLED_SOURCE_WEIGHT,
    }
}

/// Bounded priors applied to every lane score of a candidate: source quality times recency.
///
/// Measured on the golden eval (docs/search-eval.md): MRR 0.929 with neither prior, 0.941 with
/// recency alone, 0.947 with both; recency 0.05–0.2 and half-lives 7–30 days all within 0.004.
fn prior_multiplier(candidate: &Candidate, now_ms: i64) -> f64 {
    let recency = candidate
        .period_end_ms
        .or(candidate.period_start_ms)
        .map_or(1.0, |ts| {
            let age_days = (now_ms.saturating_sub(ts)).max(0) as f64 / DAY_MS;
            1.0 + RECENCY_BOOST * 0.5_f64.powf(age_days / RECENCY_HALF_LIFE_DAYS)
        });
    sanitize_nonnegative(candidate.source_weight * recency)
}

fn accepted_vector_hits(cutoff: f32, hits: Vec<crate::vector::VectorHit>) -> Vec<crate::vector::VectorHit> {
    hits.into_iter()
        .filter(|hit| hit.distance.is_finite() && hit.distance <= cutoff)
        .collect()
}

/// Concept-level match of one candidate's fields (title first, then body fields).
struct FieldMatch {
    /// Fraction of concepts matched, substring semantics (`art` is in `quarterly`).
    lexical_coverage: f64,
    /// Fraction matched as whole words for Latin terms; CJK and short tokens as matched.
    answerability_coverage: f64,
    normalized_text: String,
    concept_hits: Vec<bool>,
}

fn match_fields(concepts: &[Concept], fields: &[&str]) -> FieldMatch {
    let (title, body) = match fields {
        [only] => ("", (*only).to_owned()),
        [title, rest @ ..] => (*title, rest.join("\n")),
        [] => ("", String::new()),
    };
    let mut text = MatchText::new(title, &body);
    let concept_hits = concepts
        .iter()
        .map(|concept| text.matches(concept))
        .collect::<Vec<_>>();
    let tokens = fields
        .iter()
        .flat_map(|field| match_tokens(field))
        .collect::<BTreeSet<_>>();
    let answered = concepts
        .iter()
        .zip(&concept_hits)
        .filter(|(concept, hit)| {
            if concept.cjk || concept.short_word {
                **hit
            } else {
                concept
                    .surface
                    .as_deref()
                    .is_some_and(|surface| tokens.contains(surface))
            }
        })
        .count();
    let fraction = |count: usize| {
        if concepts.is_empty() {
            0.0
        } else {
            count as f64 / concepts.len() as f64
        }
    };
    FieldMatch {
        lexical_coverage: fraction(concept_hits.iter().filter(|hit| **hit).count()),
        answerability_coverage: fraction(answered),
        normalized_text: normalize_match_text(fields),
        concept_hits,
    }
}

#[cfg(test)]
fn lexical_coverage(query: &str, fields: &[&str]) -> f64 {
    match_fields(&query_concepts(query), fields).lexical_coverage
}

#[cfg(test)]
fn answerability_coverage(query: &str, fields: &[&str]) -> f64 {
    match_fields(&query_concepts(query), fields).answerability_coverage
}

/// Whole Latin words of a field, NFKC-lowercased.
fn match_tokens(text: &str) -> Vec<String> {
    normalize(text)
        .split(|character: char| !character.is_alphanumeric() || super::terms::is_cjk(character))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

fn bm25_strength(rank: f64) -> f64 {
    if rank.is_finite() {
        (-rank).max(0.0)
    } else {
        0.0
    }
}

fn normalized_lane_quality(strength: f64, strongest: f64, coverage: f64) -> f64 {
    let strength = sanitize_nonnegative(strength);
    let strongest = sanitize_nonnegative(strongest);
    let coverage = sanitize_nonnegative(coverage).min(1.0);
    let relative_strength = if strongest > 0.0 {
        (strength / strongest).min(1.0)
    } else {
        1.0
    };
    sanitize_nonnegative(relative_strength * coverage).min(1.0)
}

fn vector_quality(distance: f32, cutoff: f32) -> Option<f64> {
    if !distance.is_finite() || cutoff.is_nan() || cutoff <= 0.0 || distance > cutoff {
        return None;
    }
    let distance = distance.max(0.0) as f64;
    Some((1.0 - distance / f64::from(cutoff)).clamp(0.0, 1.0))
}

fn quality_weighted_rrf(rank: usize, quality: f64) -> f64 {
    if rank == 0 {
        return 0.0;
    }
    let quality = sanitize_nonnegative(quality).min(1.0);
    let weight = MIN_QUALITY_WEIGHT + (1.0 - MIN_QUALITY_WEIGHT) * quality;
    sanitize_nonnegative(weight / (RRF_K + rank as f64))
}

fn sanitize_nonnegative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn strongest_with_capped_corroboration(scores: impl IntoIterator<Item = f64>) -> f64 {
    let (strongest, total) = scores
        .into_iter()
        .fold((0.0_f64, 0.0_f64), |(max, sum), score| {
            let score = sanitize_nonnegative(score);
            (max.max(score), sum + score)
        });
    if strongest == 0.0 {
        return 0.0;
    }
    let corroboration =
        ((total - strongest) * CORROBORATION_RATE).min(strongest * MAX_CORROBORATION_FRACTION);
    sanitize_nonnegative(strongest + corroboration)
}

fn cluster_fused_score<'a>(
    candidate_lane_scores: impl IntoIterator<Item = &'a [f64; SCORE_LANE_COUNT]>,
) -> f64 {
    strongest_with_capped_corroboration(
        candidate_lane_scores
            .into_iter()
            .flat_map(|scores| scores.iter().copied()),
    )
}

fn cold_candidates(
    conn: &Connection,
    query_embedding: Option<&[f32]>,
    now_ms: i64,
    options: &FederatedSearchOptions,
    limit: usize,
) -> Result<ColdScan, rusqlite::Error> {
    let (SearchDepth::All, Some(embedding)) = (options.depth, query_embedding) else {
        return Ok(ColdScan::default());
    };
    search_cold_partitions(
        conn,
        embedding,
        (
            options.since_ts.unwrap_or(i64::MIN),
            options.to_ts.unwrap_or(now_ms).min(now_ms),
        ),
        options.max_cold_partitions,
        limit,
    )
}

fn hydrate_events(
    conn: &Connection,
    query: &str,
    excerpt_chars: usize,
    ids: &[i64],
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) -> Result<(), rusqlite::Error> {
    let concepts = query_concepts(query);
    let mut stmt = conn.prepare(
        "SELECT ts, source, window_title, content, session_id, last_seen_at
           FROM event_log WHERE id = ?1",
    )?;
    for &id in ids {
        let key = CandidateKey::Event(id);
        if candidates.contains_key(&key) {
            continue;
        }
        let row = stmt
            .query_row([id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })
            .optional()?;
        let Some((ts, source_label, title, content, session_id, last_seen_at)) = row else {
            continue;
        };
        let mut evidence = BTreeSet::from([CanonicalEvidenceId::Event(id)]);
        if let Some(session_id) = session_id {
            evidence.insert(CanonicalEvidenceId::Session(session_id));
        }
        let source_weight = event_source_weight(&source_label);
        let title = title.unwrap_or(source_label);
        let matched = match_fields(&concepts, &[&title, &content]);
        candidates.insert(
            key.clone(),
            Candidate {
                key,
                source: FederatedSource {
                    kind: FederatedSourceKind::Event,
                    id: FederatedSourceId::Event(id),
                },
                evidence,
                title,
                body: excerpt(&content, query, excerpt_chars),
                confidence: 1.0,
                period_start_ms: Some(ts),
                period_end_ms: Some(ts),
                last_seen_ms: last_seen_at.map(|seen| seen.max(ts)),
                ranks: ComponentRanks::default(),
                lane_scores: [0.0; SCORE_LANE_COUNT],
                lexical_coverage: matched.lexical_coverage,
                answerability_coverage: matched.answerability_coverage,
                lexical_quality: 0.0,
                anchor_coverage: 1.0,
                anchor_count: 0,
                normalized_text: matched.normalized_text,
                concept_hits: matched.concept_hits,
                source_weight,
                answer_eligible: true,
                score: 0.0,
                tiers: TierMetadata::default(),
            },
        );
    }
    Ok(())
}

fn hydrate_derived(
    conn: &Connection,
    query: &str,
    options: &FederatedSearchOptions,
    now_ms: i64,
    ids: &[i64],
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) -> Result<BTreeSet<i64>, SearchDocumentError> {
    let concepts = query_concepts(query);
    let documents = search_documents::hydrate_documents(conn, ids)?;
    let mut accepted = BTreeSet::new();
    let confidence_floor = options.min_derived_confidence.max(MIN_FACT_CONFIDENCE);
    for document in documents {
        if document.confidence < confidence_floor || !document_in_range(&document, options, now_ms)
        {
            continue;
        }
        let key = CandidateKey::Derived(document.id);
        accepted.insert(document.id);
        let mut evidence = document
            .evidence
            .iter()
            .map(evidence_id)
            .collect::<BTreeSet<_>>();
        if let SourceRef::PeriodSummary(summary_id) = &document.source {
            let mut stmt = conn.prepare(
                "SELECT child_summary_id FROM period_summary_child_provenance
                  WHERE summary_id = ?1 ORDER BY child_summary_id",
            )?;
            let children = stmt.query_map([summary_id], |row| row.get::<_, i64>(0))?;
            for child in children {
                evidence.insert(CanonicalEvidenceId::PeriodSummary(child?));
            }
        }
        let matched = match_fields(&concepts, &[&document.title, &document.body]);
        let source_weight = derived_source_weight(document.kind);
        candidates.insert(
            key.clone(),
            Candidate {
                key,
                source: FederatedSource {
                    kind: FederatedSourceKind::Derived(document.kind),
                    id: FederatedSourceId::Derived(document.source),
                },
                evidence,
                title: document.title,
                body: excerpt(&document.body, query, options.excerpt_chars),
                confidence: document.confidence,
                period_start_ms: document.period_start_ms,
                period_end_ms: document.period_end_ms,
                last_seen_ms: None,
                ranks: ComponentRanks::default(),
                lane_scores: [0.0; SCORE_LANE_COUNT],
                lexical_coverage: matched.lexical_coverage,
                answerability_coverage: matched.answerability_coverage,
                lexical_quality: 0.0,
                anchor_coverage: 1.0,
                anchor_count: 0,
                normalized_text: matched.normalized_text,
                concept_hits: matched.concept_hits,
                source_weight,
                answer_eligible: true,
                score: 0.0,
                tiers: TierMetadata::default(),
            },
        );
    }
    Ok(accepted)
}

fn hydrate_imported(
    query: &str,
    excerpt_chars: usize,
    matches: &[ImportedMatch],
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) {
    let concepts = query_concepts(query);
    for item in matches {
        let key = CandidateKey::Imported(item.item_id);
        let body = item.body();
        let matched = match_fields(&concepts, &[&item.name, &body]);
        let source_weight = 1.0;
        candidates.insert(
            key.clone(),
            Candidate {
                key,
                source: FederatedSource {
                    kind: FederatedSourceKind::Imported(item.platform),
                    id: FederatedSourceId::Imported(item.item_id),
                },
                evidence: BTreeSet::from([CanonicalEvidenceId::MemoryImport(item.import_id)]),
                title: item.name.clone(),
                body: excerpt(&body, query, excerpt_chars),
                // The V52 CHECK already caps storage; clamp again so no read path can lift it.
                confidence: item.confidence.min(crate::memory_import::CONFIDENCE_MAX),
                period_start_ms: None,
                period_end_ms: None,
                last_seen_ms: None,
                ranks: ComponentRanks::default(),
                lane_scores: [0.0; SCORE_LANE_COUNT],
                lexical_coverage: matched.lexical_coverage,
                answerability_coverage: matched.answerability_coverage,
                lexical_quality: 0.0,
                anchor_coverage: 1.0,
                anchor_count: 0,
                normalized_text: matched.normalized_text,
                concept_hits: matched.concept_hits,
                source_weight,
                answer_eligible: false,
                score: 0.0,
                tiers: TierMetadata::default(),
            },
        );
    }
}

fn apply_imported_ranks(
    matches: &[ImportedMatch],
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) {
    for (index, item) in matches.iter().enumerate() {
        let rank = index + 1;
        let Some(candidate) = candidates.get_mut(&CandidateKey::Imported(item.item_id)) else {
            continue;
        };
        let quality = candidate.lexical_coverage;
        candidate.record_score(
            ScoreLane::Imported,
            quality_weighted_rrf(rank, quality) * IMPORTED_SCORE_WEIGHT,
        );
        candidate.record_quality(quality);
        candidate.ranks.imported = Some(rank);
        candidate.tiers.canonical = true;
    }
}

fn imported_lane_allowed(options: &FederatedSearchOptions) -> bool {
    !matches!(options.source, SourceConstraint::Require(_))
        && imported_focus_accepts(options.focus)
}

/// Imported claims are preferences, goals, entities and open loops — never a meeting, brief,
/// period rollup or thread — so only those focuses may surface them.
const fn imported_focus_accepts(focus: MemoryQueryFocus) -> bool {
    matches!(
        focus,
        MemoryQueryFocus::Any
            | MemoryQueryFocus::Person
            | MemoryQueryFocus::Project
            | MemoryQueryFocus::State
    )
}

fn document_in_range(
    document: &SearchDocument,
    options: &FederatedSearchOptions,
    now_ms: i64,
) -> bool {
    let Some(since_ts) = options.since_ts else {
        return true;
    };
    let to_ts = options.to_ts.unwrap_or(now_ms).min(now_ms);
    match (document.period_start_ms, document.period_end_ms) {
        (Some(start), Some(end)) => start < to_ts && end > since_ts,
        _ => true,
    }
}

fn hydrate_meetings(
    conn: &Connection,
    query: &str,
    options: &FederatedSearchOptions,
    now_ms: i64,
    meetings: &[super::MeetingSearchHit],
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) -> Result<BTreeSet<i64>, rusqlite::Error> {
    let concepts = query_concepts(query);
    let mut accepted = BTreeSet::new();
    let confidence_floor = options.min_derived_confidence.max(MIN_FACT_CONFIDENCE);
    for meeting in meetings {
        if options.since_ts.is_some_and(|since| meeting.ts < since)
            || meeting.ts >= options.to_ts.unwrap_or(now_ms).min(now_ms)
        {
            continue;
        }
        let details = meeting_details(conn, meeting.session_id)?;
        let Some(details) = details else {
            continue;
        };
        if details.confidence < confidence_floor {
            continue;
        }
        let key = CandidateKey::Meeting(meeting.session_id);
        accepted.insert(meeting.session_id);
        let title = meeting
            .title
            .clone()
            .unwrap_or_else(|| "Meeting".to_owned());
        let matched = match_fields(&concepts, &[&title, &meeting.content]);
        let source_weight = DISTILLED_SOURCE_WEIGHT;
        candidates.insert(
            key.clone(),
            Candidate {
                key,
                source: FederatedSource {
                    kind: FederatedSourceKind::Meeting,
                    id: FederatedSourceId::Meeting(meeting.session_id),
                },
                evidence: details.evidence,
                title,
                body: excerpt(&meeting.content, query, options.excerpt_chars),
                confidence: details.confidence,
                period_start_ms: Some(meeting.ts),
                period_end_ms: details.ended_at.or(Some(meeting.ts)),
                last_seen_ms: None,
                ranks: ComponentRanks::default(),
                lane_scores: [0.0; SCORE_LANE_COUNT],
                lexical_coverage: matched.lexical_coverage,
                answerability_coverage: matched.answerability_coverage,
                lexical_quality: 0.0,
                anchor_coverage: 1.0,
                anchor_count: 0,
                normalized_text: matched.normalized_text,
                concept_hits: matched.concept_hits,
                source_weight,
                answer_eligible: true,
                score: 0.0,
                tiers: TierMetadata::default(),
            },
        );
    }
    Ok(accepted)
}

fn meeting_details(
    conn: &Connection,
    session_id: i64,
) -> Result<Option<MeetingDetails>, rusqlite::Error> {
    let session = conn
        .query_row(
            "SELECT ended_at, confidence FROM sessions WHERE id = ?1",
            [session_id],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, f64>(1)?)),
        )
        .optional()?;
    let Some((ended_at, confidence)) = session else {
        return Ok(None);
    };
    let mut evidence = BTreeSet::from([CanonicalEvidenceId::Session(session_id)]);
    collect_numeric_evidence(
        conn,
        "SELECT id FROM transcript_segments WHERE session_id = ?1 AND excluded = 0 ORDER BY id",
        session_id,
        CanonicalEvidenceId::TranscriptSegment,
        &mut evidence,
    )?;
    collect_numeric_evidence(
        conn,
        "SELECT id FROM session_notes WHERE session_id = ?1 ORDER BY id",
        session_id,
        CanonicalEvidenceId::SessionNote,
        &mut evidence,
    )?;
    collect_numeric_evidence(
        conn,
        "SELECT id FROM meeting_recaps WHERE session_id = ?1 ORDER BY id",
        session_id,
        CanonicalEvidenceId::MeetingRecap,
        &mut evidence,
    )?;
    collect_numeric_evidence(
        conn,
        "SELECT id FROM event_log WHERE session_id = ?1 ORDER BY id",
        session_id,
        CanonicalEvidenceId::Event,
        &mut evidence,
    )?;
    Ok(Some(MeetingDetails {
        ended_at,
        confidence,
        evidence,
    }))
}

fn collect_numeric_evidence(
    conn: &Connection,
    sql: &str,
    session_id: i64,
    wrap: fn(i64) -> CanonicalEvidenceId,
    evidence: &mut BTreeSet<CanonicalEvidenceId>,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([session_id], |row| row.get::<_, i64>(0))?;
    for row in rows {
        evidence.insert(wrap(row?));
    }
    Ok(())
}

fn evidence_id(evidence: &EvidenceRef) -> CanonicalEvidenceId {
    match evidence {
        EvidenceRef::Event { id, .. } => CanonicalEvidenceId::Event(*id),
        EvidenceRef::Session { id, .. } => CanonicalEvidenceId::Session(*id),
        EvidenceRef::TranscriptSegment { id, .. } => CanonicalEvidenceId::TranscriptSegment(*id),
        EvidenceRef::SessionNote { id, .. } => CanonicalEvidenceId::SessionNote(*id),
        EvidenceRef::MeetingRecap { id, .. } => CanonicalEvidenceId::MeetingRecap(*id),
        EvidenceRef::Brief { date, .. } => CanonicalEvidenceId::Brief(date.clone()),
        EvidenceRef::FeedbackEvent { id, .. } => CanonicalEvidenceId::FeedbackEvent(*id),
    }
}

#[derive(Clone, Copy)]
enum RankLane {
    Warm,
    Cold,
}

fn apply_event_fts_ranks(
    hits: &[FtsSearchHit],
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) {
    let strongest = hits
        .iter()
        .map(|hit| bm25_strength(hit.rank))
        .fold(0.0_f64, f64::max);
    for (index, hit) in hits.iter().enumerate() {
        let rank = index + 1;
        let Some(candidate) = candidates.get_mut(&CandidateKey::Event(hit.id)) else {
            continue;
        };
        let quality = normalized_lane_quality(
            bm25_strength(hit.rank),
            strongest,
            candidate.lexical_coverage,
        );
        candidate.record_score(ScoreLane::EventFts, quality_weighted_rrf(rank, quality));
        candidate.record_quality(quality);
        candidate.ranks.event_fts = Some(rank);
        candidate.tiers.canonical = true;
    }
}

fn apply_ranks(ids: &[i64], lane: RankLane, candidates: &mut BTreeMap<CandidateKey, Candidate>) {
    for (index, id) in ids.iter().enumerate() {
        let rank = index + 1;
        let Some(candidate) = candidates.get_mut(&CandidateKey::Event(*id)) else {
            continue;
        };
        match lane {
            RankLane::Cold => {
                candidate.record_score(ScoreLane::ColdScan, quality_weighted_rrf(rank, 1.0));
                candidate.ranks.cold_scan = Some(rank);
                candidate.tiers.cold = true;
            }
            RankLane::Warm => unreachable!("Warm vector ranks retain distance"),
        }
    }
}

fn apply_vector_ranks(
    hits: &[crate::vector::VectorHit],
    lane: RankLane,
    cutoff: f32,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) {
    for (index, hit) in hits.iter().enumerate() {
        let rank = index + 1;
        let Some(candidate) = candidates.get_mut(&CandidateKey::Event(hit.id)) else {
            continue;
        };
        let Some(quality) = vector_quality(hit.distance, cutoff) else {
            continue;
        };
        match lane {
            RankLane::Warm => {
                candidate.record_score(ScoreLane::WarmKnn, quality_weighted_rrf(rank, quality));
                candidate.ranks.warm_knn = Some(rank);
                candidate.tiers.warm = true;
            }
            RankLane::Cold => unreachable!("Cold ranks do not expose L2 distance"),
        }
    }
}

fn apply_document_ranks(
    ranked: &[search_documents::DocumentCandidate],
    accepted: &BTreeSet<i64>,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) {
    let strongest = ranked
        .iter()
        .filter(|document| accepted.contains(&document.id))
        .map(|document| bm25_strength(document.rank))
        .fold(0.0_f64, f64::max);
    for (index, document) in ranked
        .iter()
        .filter(|document| accepted.contains(&document.id))
        .enumerate()
    {
        let rank = index + 1;
        let Some(candidate) = candidates.get_mut(&CandidateKey::Derived(document.id)) else {
            continue;
        };
        let quality = normalized_lane_quality(
            bm25_strength(document.rank),
            strongest,
            candidate.lexical_coverage,
        );
        candidate.record_score(ScoreLane::DerivedFts, quality_weighted_rrf(rank, quality));
        candidate.record_quality(quality);
        candidate.ranks.derived_fts = Some(rank);
        candidate.tiers.canonical = true;
    }
}

fn apply_document_vector_ranks(
    hits: &[crate::vector::VectorHit],
    accepted: &BTreeSet<i64>,
    cutoff: f32,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) {
    for (index, hit) in hits
        .iter()
        .filter(|hit| accepted.contains(&hit.id))
        .enumerate()
    {
        let rank = index + 1;
        let Some(candidate) = candidates.get_mut(&CandidateKey::Derived(hit.id)) else {
            continue;
        };
        let Some(quality) = vector_quality(hit.distance, cutoff) else {
            continue;
        };
        candidate.record_score(ScoreLane::DerivedKnn, quality_weighted_rrf(rank, quality));
        candidate.ranks.derived_knn = Some(rank);
        candidate.tiers.warm = true;
    }
}

fn apply_meeting_ranks(
    meetings: &[super::MeetingSearchHit],
    accepted: &BTreeSet<i64>,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) {
    let strongest = meetings
        .iter()
        .filter(|meeting| accepted.contains(&meeting.session_id))
        .map(|meeting| sanitize_nonnegative(meeting.score))
        .fold(0.0_f64, f64::max);
    for (index, meeting) in meetings
        .iter()
        .filter(|meeting| accepted.contains(&meeting.session_id))
        .enumerate()
    {
        let rank = index + 1;
        let Some(candidate) = candidates.get_mut(&CandidateKey::Meeting(meeting.session_id)) else {
            continue;
        };
        let quality = normalized_lane_quality(meeting.score, strongest, candidate.lexical_coverage);
        candidate.record_score(ScoreLane::Meeting, quality_weighted_rrf(rank, quality));
        candidate.record_quality(quality);
        candidate.ranks.meeting = Some(rank);
        candidate.tiers.canonical = true;
    }
}

fn merge_cluster(query: &str, excerpt_chars: usize, mut cluster: Vec<Candidate>) -> FederatedHit {
    cluster.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                source_priority(&left.source.kind).cmp(&source_priority(&right.source.kind))
            })
            .then_with(|| left.source.cmp(&right.source))
    });
    let primary = &cluster[0];
    let mut sources = BTreeSet::new();
    let mut evidence = BTreeSet::new();
    let mut ranks = ComponentRanks::default();
    let mut tiers = TierMetadata::default();
    let confidence = primary.confidence;
    // A cluster may corroborate a fresh primary with a stale/deleted member. Keep
    // that cluster searchable, but never let unresolved evidence satisfy an answer.
    let answer_eligible = cluster.iter().all(|candidate| candidate.answer_eligible);
    let match_quality = MatchQualitySummary {
        lexical_quality: primary.lexical_quality,
        term_coverage: primary.answerability_coverage,
        anchor_coverage: primary.anchor_coverage,
        anchor_count: primary.anchor_count,
    };
    let mut period_start_ms = None;
    let mut period_end_ms = None;
    for candidate in &cluster {
        sources.insert(candidate.source.clone());
        evidence.extend(candidate.evidence.iter().cloned());
        ranks.merge(candidate.ranks);
        tiers.merge(candidate.tiers);
        period_start_ms = min_option(period_start_ms, candidate.period_start_ms);
        period_end_ms = max_option(period_end_ms, candidate.period_end_ms);
    }
    let fused_score = cluster_fused_score(cluster.iter().map(|candidate| &candidate.lane_scores));
    FederatedHit {
        source_kind: primary.source.kind.clone(),
        source_id: primary.source.id.clone(),
        sources: sources.into_iter().collect(),
        evidence_ids: evidence.into_iter().collect(),
        title: primary.title.clone(),
        excerpt: excerpt(&primary.body, query, excerpt_chars),
        confidence,
        period_start_ms,
        period_end_ms,
        component_ranks: ranks,
        fused_score,
        tiers,
        answer_eligible,
        match_quality,
    }
}

fn candidate_family(kind: &FederatedSourceKind) -> Option<MemorySourceFamily> {
    match kind {
        FederatedSourceKind::UserNote | FederatedSourceKind::Event | FederatedSourceKind::Imported(_) => None,
        FederatedSourceKind::Meeting => Some(MemorySourceFamily::Meeting),
        FederatedSourceKind::Derived(kind) => match kind {
            SearchDocumentKind::Person => Some(MemorySourceFamily::Person),
            SearchDocumentKind::Project => Some(MemorySourceFamily::Project),
            SearchDocumentKind::Commitment | SearchDocumentKind::OpenLoop => {
                Some(MemorySourceFamily::State)
            }
            SearchDocumentKind::ThreadSummary => Some(MemorySourceFamily::Thread),
            SearchDocumentKind::SessionSummary
            | SearchDocumentKind::MeetingRecap
            | SearchDocumentKind::MeetingNote => Some(MemorySourceFamily::Meeting),
            SearchDocumentKind::Brief => Some(MemorySourceFamily::Brief),
            SearchDocumentKind::PeriodSummary => Some(MemorySourceFamily::Period),
            SearchDocumentKind::Lesson => None,
        },
    }
}

fn required_document_focus(source: SourceConstraint) -> Option<MemoryQueryFocus> {
    let SourceConstraint::Require(family) = source else {
        return None;
    };
    Some(match family {
        MemorySourceFamily::Meeting => MemoryQueryFocus::Meeting,
        MemorySourceFamily::State => MemoryQueryFocus::State,
        MemorySourceFamily::Brief => MemoryQueryFocus::Brief,
        MemorySourceFamily::Person => MemoryQueryFocus::Person,
        MemorySourceFamily::Project => MemoryQueryFocus::Project,
        MemorySourceFamily::Thread => MemoryQueryFocus::Thread,
        MemorySourceFamily::Period => MemoryQueryFocus::Period,
    })
}

fn source_accepts(source: SourceConstraint, kind: &FederatedSourceKind) -> bool {
    match source {
        SourceConstraint::Any | SourceConstraint::Prefer(_) => true,
        SourceConstraint::Require(family) => candidate_family(kind) == Some(family),
    }
}

fn source_intent_multiplier(source: SourceConstraint, kind: &FederatedSourceKind) -> f64 {
    if matches!(source, SourceConstraint::Prefer(family) if candidate_family(kind) == Some(family))
    {
        1.0 + SOURCE_PREFERENCE_BOOST
    } else {
        1.0
    }
}

fn normalize_match_text(fields: &[&str]) -> String {
    let joined = fields.join(" ");
    let words = joined
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    format!(
        " {} ",
        words.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// One answerability anchor: an explicit/rare word checked on token boundaries, or a CJK concept
/// checked with the concept matcher (no spaces to delimit it, and okurigana may differ).
enum AnchorCheck {
    Text(String),
    Concept(usize),
}

fn reliable_anchors<'a>(
    query: &str,
    explicit: &[QueryAnchor],
    concepts: &[Concept],
    candidates: impl Iterator<Item = &'a Candidate>,
) -> Vec<AnchorCheck> {
    let candidates = candidates.collect::<Vec<_>>();
    let mut anchors = explicit
        .iter()
        .filter(|anchor| {
            anchor
                .normalized
                .chars()
                .take(super::query::MAX_ANSWER_ANCHOR_CHARS + 1)
                .count()
                <= super::query::MAX_ANSWER_ANCHOR_CHARS
        })
        .take(MAX_ANSWER_ANCHORS)
        .map(|anchor| AnchorCheck::Text(anchor.normalized.clone()))
        .collect::<Vec<_>>();
    // A Japanese compound is the CJK counterpart of a named English word: the thing asked about.
    // Without this every Japanese question had zero anchors and was never answerable.
    for (index, concept) in concepts.iter().enumerate() {
        if anchors.len() >= MAX_ANSWER_ANCHORS {
            break;
        }
        let bounded = concept
            .display
            .chars()
            .take(super::query::MAX_ANSWER_ANCHOR_CHARS + 1)
            .count()
            <= super::query::MAX_ANSWER_ANCHOR_CHARS;
        if bounded && (concept.cjk || is_identifier(concept)) {
            anchors.push(AnchorCheck::Concept(index));
        }
    }
    for term in lexical_terms(query) {
        if anchors.len() >= MAX_ANSWER_ANCHORS {
            break;
        }
        let normalized = term.to_lowercase();
        let character_count = normalized.chars().count();
        if !(MIN_RARE_ANCHOR_CHARS..=super::query::MAX_ANSWER_ANCHOR_CHARS)
            .contains(&character_count)
            || normalized.chars().any(super::terms::is_cjk)
            || super::query::is_stopword(&normalized)
            || super::query::is_source_cue(&normalized)
            || anchors
                .iter()
                .any(|anchor| matches!(anchor, AnchorCheck::Text(text) if *text == normalized))
        {
            continue;
        }
        let needle = format!(" {normalized} ");
        let document_frequency = candidates
            .iter()
            .filter(|candidate| candidate.normalized_text.contains(&needle))
            .take(2)
            .count();
        if document_frequency == 1 {
            anchors.push(AnchorCheck::Text(normalized));
        }
    }
    anchors
}

/// A Latin code the question names exactly: an acronym (NDA, INV), a short token (Q3, PR, UX) or
/// letters mixed with digits (v2, ops1182). Like a proper noun, the answer has to contain it.
fn is_identifier(concept: &Concept) -> bool {
    if concept.cjk {
        return false;
    }
    let letters = concept.display.chars().filter(|c| c.is_alphabetic()).count();
    let digits = concept.display.chars().filter(char::is_ascii_digit).count();
    concept.short_word
        || (letters > 0 && digits > 0)
        || (letters >= 2
            && concept
                .display
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(char::is_uppercase))
}

fn anchor_coverage(anchors: &[AnchorCheck], candidate: &Candidate) -> f64 {
    if anchors.is_empty() {
        return 1.0;
    }
    let matched = anchors
        .iter()
        .filter(|anchor| match anchor {
            AnchorCheck::Text(text) if text.chars().any(super::terms::is_cjk) => {
                candidate.normalized_text.contains(text.as_str())
            }
            AnchorCheck::Text(text) => candidate.normalized_text.contains(&format!(" {text} ")),
            AnchorCheck::Concept(index) => {
                candidate.concept_hits.get(*index).copied().unwrap_or(false)
            }
        })
        .count();
    matched as f64 / anchors.len() as f64
}

/// Resolve the distinct evidence identities once, then fail every candidate containing a missing
/// row. Grouping by canonical table removes the candidate × evidence query pattern without
/// relaxing the fail-closed answerability rule.
fn apply_evidence_resolution(
    conn: &Connection,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) -> Result<(), rusqlite::Error> {
    let expected = candidates
        .values()
        .flat_map(|candidate| candidate.evidence.iter().cloned())
        .collect::<BTreeSet<_>>();
    let resolved = resolved_evidence(conn, &expected)?;

    for candidate in candidates.values_mut() {
        // An imported claim can corroborate but never support an assertive answer, even when its
        // import record resolves.
        candidate.answer_eligible =
            !candidate.source.kind.is_imported() && candidate.evidence.is_subset(&resolved);
    }
    Ok(())
}

#[cfg(test)]
fn evidence_resolves(
    conn: &Connection,
    evidence: &BTreeSet<CanonicalEvidenceId>,
) -> Result<bool, rusqlite::Error> {
    Ok(evidence.is_subset(&resolved_evidence(conn, evidence)?))
}

fn resolved_evidence(
    conn: &Connection,
    evidence: &BTreeSet<CanonicalEvidenceId>,
) -> Result<BTreeSet<CanonicalEvidenceId>, rusqlite::Error> {
    let mut event_ids = BTreeSet::new();
    let mut session_ids = BTreeSet::new();
    let mut transcript_segment_ids = BTreeSet::new();
    let mut session_note_ids = BTreeSet::new();
    let mut meeting_recap_ids = BTreeSet::new();
    let mut brief_dates = BTreeSet::new();
    let mut feedback_event_ids = BTreeSet::new();
    let mut period_summary_ids = BTreeSet::new();
    let mut memory_import_ids = BTreeSet::new();
    let mut user_note_ids = BTreeSet::new();

    for id in evidence {
        match id {
            CanonicalEvidenceId::UserNote(id) => { user_note_ids.insert(*id); },
            CanonicalEvidenceId::Event(id) => {
                event_ids.insert(*id);
            }
            CanonicalEvidenceId::Session(id) => {
                session_ids.insert(*id);
            }
            CanonicalEvidenceId::TranscriptSegment(id) => {
                transcript_segment_ids.insert(*id);
            }
            CanonicalEvidenceId::SessionNote(id) => {
                session_note_ids.insert(*id);
            }
            CanonicalEvidenceId::MeetingRecap(id) => {
                meeting_recap_ids.insert(*id);
            }
            CanonicalEvidenceId::Brief(date) => {
                brief_dates.insert(date.as_str());
            }
            CanonicalEvidenceId::FeedbackEvent(id) => {
                feedback_event_ids.insert(*id);
            }
            CanonicalEvidenceId::PeriodSummary(id) => {
                period_summary_ids.insert(*id);
            }
            CanonicalEvidenceId::MemoryImport(id) => {
                memory_import_ids.insert(*id);
            }
        }
    }

    let mut resolved = BTreeSet::new();
    resolve_numeric_evidence(
        conn,
        "event_log",
        &event_ids,
        CanonicalEvidenceId::Event,
        &mut resolved,
    )?;
    resolve_numeric_evidence(
        conn,
        "sessions",
        &session_ids,
        CanonicalEvidenceId::Session,
        &mut resolved,
    )?;
    resolve_numeric_evidence(
        conn,
        "transcript_segments",
        &transcript_segment_ids,
        CanonicalEvidenceId::TranscriptSegment,
        &mut resolved,
    )?;
    resolve_numeric_evidence(
        conn,
        "session_notes",
        &session_note_ids,
        CanonicalEvidenceId::SessionNote,
        &mut resolved,
    )?;
    resolve_numeric_evidence(
        conn,
        "meeting_recaps",
        &meeting_recap_ids,
        CanonicalEvidenceId::MeetingRecap,
        &mut resolved,
    )?;
    resolve_text_evidence(
        conn,
        "briefs",
        "date",
        &brief_dates,
        CanonicalEvidenceId::Brief,
        &mut resolved,
    )?;
    resolve_numeric_evidence(
        conn,
        "feedback_events",
        &feedback_event_ids,
        CanonicalEvidenceId::FeedbackEvent,
        &mut resolved,
    )?;
    resolve_numeric_evidence(
        conn,
        "period_summaries",
        &period_summary_ids,
        CanonicalEvidenceId::PeriodSummary,
        &mut resolved,
    )?;
    resolve_numeric_evidence(
        conn,
        "memory_imports",
        &memory_import_ids,
        CanonicalEvidenceId::MemoryImport,
        &mut resolved,
    )?;
    resolve_numeric_evidence(conn,"user_memory_notes",&user_note_ids,CanonicalEvidenceId::UserNote,&mut resolved)?;
    Ok(resolved)
}

fn resolve_numeric_evidence(
    conn: &Connection,
    table: &str,
    ids: &BTreeSet<i64>,
    wrap: fn(i64) -> CanonicalEvidenceId,
    resolved: &mut BTreeSet<CanonicalEvidenceId>,
) -> Result<(), rusqlite::Error> {
    // A long thread or meeting cites thousands of events. Each `event_log` row fills about a
    // page (content is large), so probing them by id reads and decrypts one page per id — 13.5k
    // ids took ~100 ms CPU and seconds of I/O on a 1 GB store. `+id` stops SQLite seeking by
    // rowid, so it walks the smallest covering index (a few hundred pages) instead.
    // ponytail: the walk grows with total events (~50 ms at 1M); switch on ids.len() vs the
    // event count if that ever outweighs the probes.
    if table == "event_log" && ids.len() > EVIDENCE_BIND_CHUNK_SIZE {
        let (Some(&low), Some(&high)) = (ids.first(), ids.last()) else {
            return Ok(());
        };
        let mut stmt = conn.prepare("SELECT id FROM event_log WHERE +id BETWEEN ?1 AND ?2")?;
        for id in stmt.query_map([low, high], |row| row.get::<_, i64>(0))? {
            let id = id?;
            if ids.contains(&id) {
                resolved.insert(wrap(id));
            }
        }
        return Ok(());
    }
    for ids in ids
        .iter()
        .copied()
        .collect::<Vec<_>>()
        .chunks(EVIDENCE_BIND_CHUNK_SIZE)
    {
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT id FROM {table} WHERE id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(ids.iter().copied()), |row| {
            row.get::<_, i64>(0)
        })?;
        for id in rows {
            resolved.insert(wrap(id?));
        }
    }
    Ok(())
}

fn resolve_text_evidence(
    conn: &Connection,
    table: &str,
    key: &str,
    values: &BTreeSet<&str>,
    wrap: fn(String) -> CanonicalEvidenceId,
    resolved: &mut BTreeSet<CanonicalEvidenceId>,
) -> Result<(), rusqlite::Error> {
    for values in values
        .iter()
        .copied()
        .collect::<Vec<_>>()
        .chunks(EVIDENCE_BIND_CHUNK_SIZE)
    {
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT {key} FROM {table} WHERE {key} IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter().copied()), |row| {
            row.get::<_, String>(0)
        })?;
        for value in rows {
            resolved.insert(wrap(value?));
        }
    }
    Ok(())
}

fn source_priority(kind: &FederatedSourceKind) -> u8 {
    match kind {
        FederatedSourceKind::UserNote | FederatedSourceKind::Derived(_) => 0,
        FederatedSourceKind::Meeting => 1,
        FederatedSourceKind::Event => 2,
        FederatedSourceKind::Imported(_) => 3,
    }
}

fn focus_accepts(focus: MemoryQueryFocus, kind: &FederatedSourceKind) -> bool {
    if matches!(kind, FederatedSourceKind::UserNote) { return imported_focus_accepts(focus); }
    if kind.is_imported() {
        return imported_focus_accepts(focus);
    }
    match focus {
        MemoryQueryFocus::Any => true,
        MemoryQueryFocus::Meeting => matches!(
            kind,
            FederatedSourceKind::Meeting
                | FederatedSourceKind::Derived(
                    SearchDocumentKind::SessionSummary
                        | SearchDocumentKind::MeetingRecap
                        | SearchDocumentKind::MeetingNote
                )
        ),
        MemoryQueryFocus::State => matches!(
            kind,
            FederatedSourceKind::Derived(
                SearchDocumentKind::Commitment | SearchDocumentKind::OpenLoop
            )
        ),
        MemoryQueryFocus::Brief => {
            matches!(
                kind,
                FederatedSourceKind::Derived(SearchDocumentKind::Brief)
            )
        }
        MemoryQueryFocus::Period => matches!(
            kind,
            FederatedSourceKind::Derived(SearchDocumentKind::PeriodSummary)
        ),
        MemoryQueryFocus::Person => {
            matches!(kind, FederatedSourceKind::Derived(SearchDocumentKind::Person))
        }
        MemoryQueryFocus::Project => {
            matches!(kind, FederatedSourceKind::Derived(SearchDocumentKind::Project))
        }
        MemoryQueryFocus::Thread => matches!(
            kind,
            FederatedSourceKind::Derived(SearchDocumentKind::ThreadSummary)
        ),
    }
}

fn min_option(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn max_option(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod score_tests {
    use std::cell::Cell;

    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    use super::*;

    thread_local! {
        static EVIDENCE_SELECTS: Cell<usize> = const { Cell::new(0) };
    }

    fn count_evidence_selects(context: AuthContext<'_>) -> Authorization {
        if matches!(context.action, AuthAction::Select) {
            EVIDENCE_SELECTS.with(|selects| selects.set(selects.get() + 1));
        }
        Authorization::Allow
    }

    #[test]
    fn materially_stronger_lexical_score_wins_at_same_ordinal() {
        let strong_quality = normalized_lane_quality(12.0, 12.0, 1.0);
        let weak_quality = normalized_lane_quality(1.0, 12.0, 1.0);

        assert!(quality_weighted_rrf(1, strong_quality) > quality_weighted_rrf(1, weak_quality));
    }

    #[test]
    fn four_equal_representations_receive_only_capped_corroboration() {
        let strongest = quality_weighted_rrf(1, 1.0);
        let fused = strongest_with_capped_corroboration([strongest; 4]);

        assert_eq!(fused, strongest * 1.1);
    }

    #[test]
    fn candidate_and_session_corroboration_share_one_global_cap() {
        let strongest = quality_weighted_rrf(1, 1.0);
        let mut first_candidate = [0.0; SCORE_LANE_COUNT];
        first_candidate[ScoreLane::EventFts.index()] = strongest;
        first_candidate[ScoreLane::WarmKnn.index()] = strongest;
        let mut second_candidate = [0.0; SCORE_LANE_COUNT];
        second_candidate[ScoreLane::DerivedFts.index()] = strongest;
        second_candidate[ScoreLane::Meeting.index()] = strongest;
        let fused_cluster = cluster_fused_score([&first_candidate, &second_candidate]);

        assert!(fused_cluster <= strongest * 1.1);
    }

    #[test]
    fn non_finite_and_invalid_scores_cannot_escape_fusion() {
        assert_eq!(bm25_strength(f64::NAN), 0.0);
        assert_eq!(bm25_strength(f64::NEG_INFINITY), 0.0);
        assert_eq!(normalized_lane_quality(f64::INFINITY, 1.0, 1.0), 0.0);
        assert_eq!(vector_quality(f32::NAN, 1.0), None);
        assert_eq!(vector_quality(f32::INFINITY, 1.0), None);
        assert_eq!(vector_quality(-1.0, 1.0), Some(1.0));

        let fused =
            strongest_with_capped_corroboration([f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.5]);
        assert!(fused.is_finite());
        assert_eq!(fused, 0.5);
    }

    #[test]
    fn brief_period_and_thread_preferences_boost_without_filtering_other_families() {
        for (family, kind) in [
            (MemorySourceFamily::Brief, SearchDocumentKind::Brief),
            (
                MemorySourceFamily::Period,
                SearchDocumentKind::PeriodSummary,
            ),
            (
                MemorySourceFamily::Thread,
                SearchDocumentKind::ThreadSummary,
            ),
        ] {
            let preferred = FederatedSourceKind::Derived(kind);
            let meeting = FederatedSourceKind::Meeting;
            assert!(
                source_intent_multiplier(SourceConstraint::Prefer(family), &preferred)
                    > source_intent_multiplier(SourceConstraint::Prefer(family), &meeting)
            );
            assert!(source_accepts(SourceConstraint::Prefer(family), &meeting));
        }
    }

    #[test]
    fn deleted_evidence_is_not_answer_eligible() {
        let conn = crate::open_in_memory().unwrap();
        let event_id = crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "temporary evidence",
                content_hash: "temporary-evidence",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        conn.execute("DELETE FROM event_log WHERE id = ?1", [event_id])
            .unwrap();

        assert!(!evidence_resolves(
            &conn,
            &BTreeSet::from([CanonicalEvidenceId::Event(event_id)])
        )
        .unwrap());
    }

    #[test]
    fn evidence_resolution_batches_past_sqlite_bind_limit_and_stays_fail_closed() {
        let mut conn = crate::open_in_memory().unwrap();
        let evidence_count = EVIDENCE_BIND_CHUNK_SIZE * 2 + 37;
        let mut evidence = BTreeSet::new();
        for id in 1..=i64::try_from(evidence_count).unwrap() {
            conn.execute(
                "INSERT INTO event_log
                   (id, ts, source, kind, content, content_hash, last_seen_at, dwell_ms)
                 VALUES (?1, 1, 'capture', 'text', 'batch evidence', ?2, 1, 0)",
                rusqlite::params![id, format!("batch-evidence-{id}")],
            )
            .unwrap();
            evidence.insert(CanonicalEvidenceId::Event(id));
        }

        EVIDENCE_SELECTS.with(|selects| selects.set(0));
        conn.authorizer(Some(count_evidence_selects)).unwrap();
        let answerable = evidence_resolves(&conn, &evidence).unwrap();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();

        assert!(answerable, "all persisted evidence must resolve");
        assert_eq!(
            EVIDENCE_SELECTS.with(Cell::get),
            1,
            "a large event set is checked by one index walk, not one query per id or chunk"
        );

        conn.execute("DELETE FROM event_log WHERE id = ?1", [1_i64])
            .unwrap();
        assert!(
            !evidence_resolves(&conn, &evidence).unwrap(),
            "one deleted row invalidates the entire evidence set"
        );
    }

    #[test]
    fn p0_lexical_coverage_remains_substring_based() {
        assert_eq!(
            lexical_coverage("Phoenix art", &["Phoenix quarterly"]),
            1.0
        );
    }

    #[test]
    fn answerability_coverage_requires_unicode_token_boundaries() {
        assert_eq!(
            answerability_coverage("Phoenix art", &["Phoenix quarterly"]),
            0.5
        );
        assert_eq!(
            answerability_coverage("Élodie résumé", &["Élodie, résumé!"]),
            1.0
        );
    }

    #[test]
    fn zero_frequency_query_term_is_not_inferred_as_an_anchor() {
        let conn = crate::open_in_memory().unwrap();
        crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: 10,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "OrchidProtocol rollouttimeline happens Tuesday",
                content_hash: "zero-frequency-anchor",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();

        let result = search_federated(
            &conn,
            "OrchidProtocol rollouttimeline schedulewindow",
            None,
            20,
            &FederatedSearchOptions::default(),
            2,
        )
        .unwrap();

        assert_eq!(result.hits[0].match_quality.anchor_count, 2);
        assert_eq!(result.hits[0].match_quality.anchor_coverage, 1.0);
    }
}
