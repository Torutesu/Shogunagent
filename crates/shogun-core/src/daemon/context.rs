//! Context retrieval, assembly, grounding, and reply-context value types.

use super::*;
use rusqlite::OptionalExtension;

// Reserve context anchors before generic question labels consume FTS's 24-term budget.
#[cfg(feature = "desktop")]
fn form_context_query(profile: &[&str], questions: &[&str]) -> String {
    let anchors = profile
        .iter()
        .flat_map(|query| shogun_memory::search::lexical_terms(query))
        .collect::<Vec<_>>();
    let lists = questions
        .iter()
        .map(|query| shogun_memory::search::lexical_terms(query))
        .collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for term in &anchors {
        if seen.insert(term.to_lowercase()) {
            terms.push(term.as_str());
            if terms.len() == 12 {
                break;
            }
        }
    }
    // Round-robin still gives later questions a share of the remaining budget.
    for offset in 0..24 {
        for list in &lists {
            if let Some(term) = list.get(offset) {
                if seen.insert(term.to_lowercase()) {
                    terms.push(term.as_str());
                    if terms.len() == 24 {
                        return terms.join(" ");
                    }
                }
            }
        }
    }
    terms.join(" ")
}

#[cfg(all(test, feature = "desktop"))]
mod form_context_tests {
    use super::*;

    #[test]
    fn form_questions_do_not_push_the_profile_project_out_of_the_query() {
        let questions = (0..20)
            .map(|index| format!("Questiontopic{index} details"))
            .collect::<Vec<_>>();
        let refs = questions.iter().map(String::as_str).collect::<Vec<_>>();
        let query = form_context_query(
            &["Mira Rao", "User working on Nimbus desktop research tools"],
            &refs,
        );
        let terms = shogun_memory::search::lexical_terms(&query);
        assert!(terms.iter().any(|term| term.eq_ignore_ascii_case("Nimbus")));
        assert!(terms
            .iter()
            .any(|term| term.eq_ignore_ascii_case("Questiontopic0")));
        assert!(terms.len() <= 24);
    }

    #[test]
    fn shared_query_covers_later_questions_and_keeps_names_and_project_context() {
        let query = form_context_query(
            &["Mira Rao", "Builds Nimbus desktop tools"],
            &[
                "Full name",
                "Email address",
                "City of residence",
                "Country of residence",
                "Current role",
                "Personal or project website",
                "Name of product you are building",
                "What are you building and what problem does it solve?",
                "Who is it for and how would they use it?",
                "What have you worked on recently?",
                "Why would this residency be useful?",
            ],
        );
        let terms = shogun_memory::search::lexical_terms(&query);
        assert!(terms.len() <= 24);
        for expected in ["Mira", "Rao", "Nimbus", "country", "worked", "residency"] {
            assert!(
                query.to_lowercase().contains(&expected.to_lowercase()),
                "{expected}"
            );
        }
    }
}

/// Query-relevant distilled memory plus the minimum canonical evidence retained to ground it.
/// This is deliberately private: the existing event-only search API remains its compatibility
/// adapter while desktop context assembly can use the richer V21/V22 retrieval result.
#[derive(Debug, Default)]
pub(super) struct ContextRetrieval {
    summaries: Vec<ContextSummary>,
    evidence: Vec<Evidence>,
    screen_frames: Vec<ScreenFrameRef>,
}

impl ContextRetrieval {
    /// The distilled lines (already confidence-gated, `possibly:` where Medium) and the evidence.
    pub(super) fn into_lines_and_evidence(self) -> (Vec<String>, Vec<Evidence>) {
        (self.summaries.into_iter().map(|s| s.text).collect(), self.evidence)
    }
}

#[derive(Debug)]
pub(super) struct ContextSummary {
    /// Stable, request-local negative id used only by Fusion's private block map.
    block_id: i64,
    text: String,
    confidence: f64,
    grounding: Option<Evidence>,
}

pub(super) fn rank_context_hits(
    query_terms: &[String],
    mut hits: Vec<shogun_memory::search::FederatedHit>,
    limit: usize,
) -> Vec<shogun_memory::search::FederatedHit> {
    hits.retain(|hit| {
        lexical_match_count(query_terms, hit) > 0
            || hit.component_ranks.warm_knn.is_some()
            || hit.component_ranks.derived_knn.is_some()
            || hit.component_ranks.cold_scan.is_some()
    });
    hits.sort_by(|left, right| {
        right
            .fused_score
            .total_cmp(&left.fused_score)
            .then_with(|| {
                context_source_priority(&left.source_kind)
                    .cmp(&context_source_priority(&right.source_kind))
            })
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    hits.truncate(limit);
    hits
}

fn lexical_match_count(query_terms: &[String], hit: &shogun_memory::search::FederatedHit) -> usize {
    let searchable = format!("{} {}", hit.title, hit.excerpt).to_lowercase();
    query_terms
        .iter()
        .filter(|term| searchable.contains(term.as_str()))
        .count()
}

fn context_source_priority(kind: &shogun_memory::search::FederatedSourceKind) -> u8 {
    match kind {
        shogun_memory::search::FederatedSourceKind::UserNote | shogun_memory::search::FederatedSourceKind::Derived(_) => 0,
        shogun_memory::search::FederatedSourceKind::Meeting => 1,
        shogun_memory::search::FederatedSourceKind::Event => 2,
        shogun_memory::search::FederatedSourceKind::Imported(_) => 3,
    }
}

/// The stable identifiers known for one generation request. Global lessons always apply; scoped
/// lessons apply only when their matching identifier is present here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerationContext<'a> {
    pub app_bundle_id: Option<&'a str>,
    pub person_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
}

impl<'a> GenerationContext<'a> {
    /// Global always; app / person / project only when that identifier is present.
    pub fn lesson_scopes(self) -> Vec<shogun_memory::lessons::ScopeFilter<'a>> {
        use shogun_memory::lessons::{LessonScope, ScopeFilter};
        let mut scopes = vec![ScopeFilter {
            scope: LessonScope::Global,
            scope_ref: None,
        }];
        if let Some(app_bundle_id) = self.app_bundle_id.filter(|id| !id.is_empty()) {
            scopes.push(ScopeFilter {
                scope: LessonScope::App,
                scope_ref: Some(app_bundle_id),
            });
        }
        if let Some(person_id) = self.person_id.filter(|id| !id.is_empty()) {
            scopes.push(ScopeFilter {
                scope: LessonScope::Person,
                scope_ref: Some(person_id),
            });
        }
        if let Some(project_id) = self.project_id.filter(|id| !id.is_empty()) {
            scopes.push(ScopeFilter {
                scope: LessonScope::Project,
                scope_ref: Some(project_id),
            });
        }
        scopes
    }
}

/// How much of a thread a reply context carries. Enough to answer in the conversation's own
/// terms, bounded so assembly stays inside the pre-press budget.
pub(super) const REPLY_TURNS: usize = 12;
pub(super) const REPLY_TURN_CHARS: usize = 800;
const REPLY_RELATED: usize = 4;
const REPLY_RELATED_CHARS: usize = 300;

/// クエリ時のローカル圧縮に許す時間予算。超えたら raw にフォールバック（SLO +300ms 厳守）。
const COMPRESS_BUDGET_MS: u64 = 50;

/// When [`Db::assemble_context_compressed_within`] stops compressing and returns the raw pack.
///
/// Production always runs [`CompressBudget::WallClock`] with the 50 ms budget
/// ([`Db::assemble_context_compressed`]). The other two exist so a test can pin which branch it
/// is checking: a test about *what compression keeps* must not turn red because a loaded CI
/// runner took 51 ms to search an in-memory DB, and a test about *the fallback itself* must not
/// depend on the machine being slow enough to reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressBudget {
    /// Fall back once this many milliseconds have passed since assembly started.
    WallClock(u64),
    /// Never fall back: always compress.
    Unbounded,
    /// Always fall back, at the first budget check (right after retrieval).
    Exhausted,
}

impl CompressBudget {
    /// The production budget.
    pub const DEFAULT: Self = Self::WallClock(COMPRESS_BUDGET_MS);

    fn exceeded(self, started: std::time::Instant) -> bool {
        match self {
            Self::WallClock(ms) => started.elapsed().as_millis() as u64 > ms,
            Self::Unbounded => false,
            Self::Exhausted => true,
        }
    }
}

/// How long retrieval may take before the zero-hit widening retry is skipped (ms). Well inside
/// the 1 s first-token SLO once generation latency is added on top.
const RETRIEVAL_BUDGET_MS: u64 = 150;

/// How many learned lessons ride into one context assembly / generation prompt (default
/// top-k of 5). The token budget in `shogun_fusion::assemble::LESSON_BUDGET_TOKENS`
/// bounds them again by size.
pub(super) const LESSON_TOP_K: usize = 5;

/// ドラフトの本文がどこ由来か。融合の provenance（設計 §3）。
#[derive(Debug, Clone, PartialEq, Default)]
pub enum PayloadSource {
    /// 取得した実メール由来（高信頼）。thread_key が provenance（同期スレッドの識別子）。
    Fetched { thread_key: String },
    /// 取得データに解決できず、画面キャプチャの断片のみ。
    #[default]
    OnScreenOnly,
}

/// Everything a one-press reply needs, assembled before the press ([`Db::build_reply_context`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplyContext {
    pub thread_key: String,
    pub title: Option<String>,
    /// The thread's own recent events, oldest first.
    pub turns: Vec<Evidence>,
    /// Confidence-gated state facts (what is owed, what is waiting).
    pub facts: Vec<String>,
    /// Earlier threads that resemble this one.
    pub related: Vec<Evidence>,
    /// Recent AX text from this exact focused window. Ephemeral; never stored in the event log.
    pub visible_text: String,
    /// How long assembly took, in ms — the SLO measurement, carried with the data it describes.
    pub build_ms: u64,
    /// 本文の出所（融合の provenance）。
    pub payload_source: PayloadSource,
}

impl ReplyContext {
    pub fn set_visible_text(&mut self, text: &str) {
        self.set_visible_text_near(text, "");
    }

    /// Prefer the text around the focused editor when its value appears in the AX walk. A short
    /// value is too ambiguous to anchor; empty editors fall back to the window tail.
    pub fn set_visible_text_near(&mut self, text: &str, focused_value: &str) {
        let excerpt = crate::inline_context::visible_text_near(text, focused_value);
        self.visible_text = shogun_memory::sanitize::persist_body(&excerpt)
            .text
            .into_owned();
    }

    /// Flatten into the memory lines the inline composer takes.
    ///
    /// Order is deliberate: the thread's own words first (a reply is written *into* a
    /// conversation, so that is the primary material), then what is owed or waiting, then
    /// anything similar from before. `max_lines` bounds the prompt.
    pub fn as_memory_lines(&self, max_lines: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(t) = self.title.as_deref().filter(|t| !t.is_empty()) {
            out.push(format!("in view: {t}"));
        }
        for turn in &self.turns {
            out.push(turn.excerpt.clone());
        }
        out.extend(self.facts.iter().cloned());
        for r in &self.related {
            out.push(format!("earlier: {}", r.excerpt));
        }
        out.truncate(max_lines);
        out
    }

    /// True when there is nothing here worth preferring over the plain state facts.
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty() && self.facts.is_empty() && self.related.is_empty()
    }

    /// Preserve typed provenance while bounding this prewarmed reply for Option/Scribe prompts.
    pub fn to_context_pack(&self) -> ContextPack {
        let related_limit = self
            .related
            .len()
            .min(crate::context::PROMPT_EVIDENCE_LIMIT);
        let turn_limit = crate::context::PROMPT_EVIDENCE_LIMIT.saturating_sub(related_limit);
        let mut evidence: Vec<Evidence> = self
            .turns
            .iter()
            .skip(self.turns.len().saturating_sub(turn_limit))
            .cloned()
            .collect();
        evidence.extend(self.related.iter().take(related_limit).cloned());
        ContextPack {
            facts: self.facts.clone(),
            evidence,
            screen_frames: Vec::new(),
        }
        .into_prompt_pack()
    }

    /// Right Option uses the current conversation, leaving older recalled facts for explicit edits.
    pub fn to_inline_context_pack(&self) -> ContextPack {
        ContextPack {
            evidence: self
                .turns
                .iter()
                .skip(
                    self.turns
                        .len()
                        .saturating_sub(crate::context::PROMPT_EVIDENCE_LIMIT),
                )
                .cloned()
                .collect(),
            ..Default::default()
        }
        .into_prompt_pack()
    }
}

/// One candidate answer to "which thread is this question about".
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadCandidate {
    pub thread_key: String,
    pub title: Option<String>,
    pub score: f64,
}

/// The outcome of resolving a referring question ([`Db::resolve_referent`]). `candidates` is
/// best-first and is what the UI offers when the verdict is `Ambiguous`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReferentOutcome {
    pub verdict: shogun_memory::thread::Referent,
    pub candidates: Vec<ThreadCandidate>,
}

/// Fraction of the thread title's words that appear in the question — the lexical agreement term
/// of salience. Words of one or two characters are skipped as too common to carry signal.
fn title_overlap(question_lower: &str, title_lower: &str) -> f64 {
    replies::title_overlap(question_lower, title_lower)
}

impl Db {
    /// The confirmed speaker names for an interval (FR-MT-15's next stage). Empty on any failure —
    /// an unnamed speaker renders as its neutral slot label, never as a guess.
    pub fn meeting_speakers(
        &self,
        session_id: i64,
    ) -> Vec<shogun_memory::session_speakers::SpeakerName> {
        self.with_conn("meeting.speakers", |c| {
            shogun_memory::session_speakers::for_session(c, session_id)
        })
        .unwrap_or_default()
    }

    /// The speaker keys that actually occur in an interval's transcript (what the naming UI offers
    /// chips for), in order of first appearance.
    pub fn meeting_speaker_keys(&self, session_id: i64) -> Vec<String> {
        self.with_conn("meeting.speaker_keys", |c| {
            shogun_memory::session_speakers::observed_keys(c, session_id)
        })
        .unwrap_or_default()
    }

    /// Assign (or, with an empty name, clear) the display name of one speaker key. This is the
    /// user-confirmed path only — nothing calls it on inference (FR-MT-15: never guess).
    pub fn assign_meeting_speaker(
        &self,
        session_id: i64,
        speaker_key: &str,
        display_name: &str,
        source: &str,
    ) -> Result<bool, String> {
        // Validated here so the caller gets a reason; MemoryFault deliberately carries only a
        // class tag, never a message.
        if !shogun_memory::session_speakers::valid_speaker_key(speaker_key) {
            return Err("Invalid speaker key (expected me / other / other:N).".into());
        }
        let now = self.now_ms();
        self.with_conn("meeting.assign_speaker", |c| {
            // A name the user picked that is one of the linked event's invitees is recorded as
            // coming from the calendar (issue #263) — the user still made the call; the source
            // says where the name was vouched for. Anything else keeps the caller's source.
            let source = if source == "user"
                && shogun_memory::calendar::session_attendee_names(c, session_id)?
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(display_name.trim()))
            {
                "calendar"
            } else {
                source
            };
            shogun_memory::session_speakers::assign(
                c,
                session_id,
                speaker_key,
                display_name,
                source,
                now,
            )
        })
        .map_err(|e| e.as_str().to_string())
    }

    /// The share-ready Markdown document for a meeting's Recap (title, speakers, minutes, notes,
    /// optionally the transcript). `None` only when the interval does not exist. Producing the
    /// document is L1 — anything that would *send* it stays L3 (invariant 4).
    pub fn meeting_recap_markdown(
        &self,
        session_id: i64,
        include_transcript: bool,
    ) -> Option<String> {
        let recap = self.meeting_recap(session_id)?;
        let minutes = self
            .meeting_recap_full(session_id)
            .map(crate::meeting::minutes::MeetingMinutes::from_stored);
        let speakers: Vec<(String, String)> = self
            .meeting_speakers(session_id)
            .into_iter()
            .map(|s| (s.speaker_key, s.display_name))
            .collect();
        // Read once: the share line needs the lines even when the transcript itself is left out.
        let transcript: Vec<crate::meeting::export::ExportLine> = self
            .meeting_transcript(session_id)
            .into_iter()
            .map(|(_ts, speaker, text)| (speaker, text))
            .collect();
        let talk_share = crate::meeting::talk_share::talk_share(&transcript);
        Some(crate::meeting::export::recap_markdown(
            &crate::meeting::export::RecapExport {
                title: &recap.title,
                duration_minutes: recap.duration_minutes,
                speakers: &speakers,
                minutes: minutes.as_ref(),
                notes: recap.notes.as_deref(),
                transcript: &transcript,
                include_transcript,
                talk_share: &talk_share,
            },
        ))
    }

    /// The most recently started meeting interval, open or closed — the default target for
    /// speaker naming when an API caller names no session.
    pub fn latest_meeting_session_id(&self) -> Option<i64> {
        self.with_conn("meeting.latest_session", |c| {
            c.query_row(
                "SELECT id FROM sessions WHERE kind = 'meeting' ORDER BY started_at DESC, id DESC LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .ok()
        .flatten()
    }

    /// Name candidates for the speaker-naming UI. Four sources, in precision order: the invitees
    /// of the calendar event the meeting is linked to (issue #263 / FR-MT-06; the owner excluded,
    /// names normalised through `people`), the names the user confirmed in earlier meetings with
    /// the same title (the weekly sync has the same people in it —
    /// docs/meeting-gap-analysis-2026-09.md B4), participants mined from the meeting app's own
    /// captured screen (the AX lane sees the participant tiles), then people the memory already
    /// knows. Offered, never auto-applied (FR-MT-15).
    pub fn meeting_speaker_candidates(&self, session_id: i64, limit: usize) -> Vec<String> {
        self.with_conn("meeting.speaker_candidates", |c| {
            let mut out = shogun_memory::calendar::session_attendee_names(c, session_id)?;
            let later =
                shogun_memory::session_speakers::previous_names_for_title(c, session_id, limit)?
                    .into_iter()
                    .chain(shogun_memory::session_speakers::participant_candidates(
                        c, session_id, limit,
                    )?)
                    .chain(shogun_memory::session_speakers::name_candidates(c, limit)?);
            for name in later {
                if !out.iter().any(|have| have.eq_ignore_ascii_case(&name)) {
                    out.push(name);
                }
            }
            out.truncate(limit);
            Ok::<_, rusqlite::Error>(out)
        })
        .unwrap_or_default()
    }

    /// Confidence-gated memory lines for the inline draft prompt (`inline::compose_inline`):
    /// the commitments the user owes and the open loops in play, passed through the FR-ST-20 gate
    /// (High stated as fact, Medium prefixed `possibly:`, Low dropped) so a low-confidence guess is
    /// never handed to the model as a fact. Capped at `limit` lines.
    pub fn inline_memory(&self, limit: usize) -> Vec<String> {
        self.inline_memory_with_refs(limit)
            .into_iter()
            .map(|(s, _, _)| s)
            .collect()
    }

    /// `inline_memory` の provenance 付き版: (confidence ゲート済み fact, 由来テーブル, row id)。
    /// `inline_memory`（文字列版・公開 API）はこれに委譲する（DRY・API 不変）。
    /// commitments → open_loops の順、同じ confidence gate、同じ truncate。
    pub(super) fn inline_memory_with_refs(
        &self,
        limit: usize,
    ) -> Vec<(String, shogun_fusion::block::StateTable, i64)> {
        self.try_inline_memory_with_refs(limit).unwrap_or_default()
    }

    pub(super) fn inline_memory_for_query_with_refs(
        &self,
        query: &str,
        limit: usize,
    ) -> Vec<(String, shogun_fusion::block::StateTable, i64)> {
        let plan = shogun_memory::search::plan_memory_query(
            query,
            self.now_ms(),
            local_day_bounds(self.now_ms()),
        );
        if query.trim().is_empty() || plan.focus == shogun_memory::search::MemoryQueryFocus::State {
            return self.inline_memory_with_refs(limit);
        }
        let mut facts = self.inline_memory_with_refs(64);
        facts.retain(|(text, _, _)| {
            let lower = text.to_lowercase();
            plan.topical_terms
                .iter()
                .any(|term| lower.contains(&term.to_lowercase()))
        });
        facts.truncate(limit);
        facts
    }

    pub(super) fn inline_memory_for_query(&self, query: &str, limit: usize) -> Vec<String> {
        self.inline_memory_for_query_with_refs(query, limit)
            .into_iter()
            .map(|(text, _, _)| text)
            .collect()
    }

    /// [`Self::inline_memory_with_refs`] with the failure kept (issue #121).
    ///
    /// The grounding path is where an unreported failure does the most damage: an empty fact list
    /// reads to the model as "this user owes nothing and is waiting on nothing", and it will say
    /// so confidently. A caller that can tell the two apart can hedge instead.
    pub(super) fn try_inline_memory_with_refs(
        &self,
        limit: usize,
    ) -> MemoryResult<Vec<(String, shogun_fusion::block::StateTable, i64)>> {
        use shogun_fusion::block::StateTable;
        use shogun_fusion::confidence::{treat_fact, Treatment};
        let now = self.now_ms();
        let (commitments, open_loops) = self.with_conn("state.for_grounding", |conn| {
            Ok::<_, rusqlite::Error>((
                state::list_commitments(conn)?,
                state::list_open_loops(conn)?,
            ))
        })?;

        let mut out: Vec<(String, StateTable, i64)> = Vec::new();
        for c in commitments {
            // Mirror commitments_due: resolved and snoozed work is not current context.
            if c.status == "done" || c.status == "cancelled" || c.is_deferred(now) {
                continue;
            }
            let text = format!("you committed: {}", c.description);
            match treat_fact(&text, c.confidence) {
                Treatment::Fact(s) | Treatment::Possible(s) => {
                    out.push((s, StateTable::Commitments, c.id));
                }
                Treatment::Excluded => {}
            }
        }
        for l in open_loops {
            // Mirror open_loops: closed and snoozed work is not current context.
            if l.status == "closed" || l.is_deferred(now) {
                continue;
            }
            let text = format!("open loop: {}", l.description);
            match treat_fact(&text, l.confidence) {
                Treatment::Fact(s) | Treatment::Possible(s) => {
                    out.push((s, StateTable::OpenLoops, l.id));
                }
                Treatment::Excluded => {}
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Ingest turns recovered from an AI coding-tool session log.
    ///
    /// Each turn becomes an `ai_session` event, keyed by the tool's own session id so the whole
    /// conversation is one thread — which is exactly the grouping a later "what did we decide
    /// about X" needs. Re-importing a log is safe: the content hash is per turn, so already-seen
    /// turns touch their existing row instead of duplicating (a session log is append-only and
    /// gets re-read as it grows).
    ///
    /// Turns are extracted for commitments and open loops like any other source — a promise made
    /// while pairing with a tool is still a promise.
    pub fn ingest_ai_session(
        &self,
        turns: &[shogun_memory::ai_session::SessionTurn],
    ) -> IngestSummary {
        let now = self.now_ms();
        let Some(mut guard) = self.lock_or_note("ingest.ai_session") else {
            return IngestSummary::default();
        };
        let mut summary = IngestSummary::default();
        for t in turns {
            // The session id is the thread; hashing it with the turn keeps two identical messages
            // in different sessions distinct.
            let hash = Self::content_hash(&format!("{}:{}:{}", t.session_id, t.ts_ms, t.text));
            let title = format!(
                "{} · {}",
                t.role.as_str(),
                t.cwd.as_deref().unwrap_or("session")
            );
            let ev = NewEvent {
                ts: t.ts_ms,
                source: "ai_session",
                kind: "message",
                app_bundle_id: None,
                window_title: Some(&title),
                content: &t.text,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            };
            // The session id is the thread — passed explicitly so both sides of the conversation
            // land in one thread regardless of what the per-turn title says.
            let Ok((id, touched)) =
                event_log::insert_or_touch_with_thread(&guard, &ev, Some(&t.session_id))
            else {
                continue;
            };
            self.reconcile_hot_locked(&guard, id);
            summary.processed += 1;
            if touched {
                continue;
            }
            summary.newly_inserted += 1;
            let candidates = shogun_memory::extract::extract_untrusted(&t.text);
            if !candidates.is_empty() {
                let ids = shogun_memory::extract::persist_candidates_deduped(
                    &mut guard,
                    id,
                    &candidates,
                    t.ts_ms,
                    now,
                )
                .unwrap_or_default();
                summary.candidates += ids.len();
            }
        }
        summary
    }

    /// Build the reply context for a thread — everything a one-press draft needs, assembled
    /// **before** the press.
    ///
    /// The SLO is 150ms to offer the action and 1s to first token, which rules out collecting
    /// context on the press (CLAUDE.md: the cache is pre-assembled, never gathered on demand).
    /// The caller runs this off the focus path and holds the result; pressing the button then
    /// only starts generation.
    ///
    /// `build_ms` is carried on the pack so the assembly cost is measurable in place rather than
    /// inferred — the SLO is an acceptance criterion, so the measurement ships with the code.
    pub fn build_reply_context(&self, thread_key: &str) -> ReplyContext {
        let started = std::time::Instant::now();
        let hot_snapshot = self.hot_thread_candidates(thread_key, REPLY_TURNS, REPLY_TURN_CHARS);
        let (title, durable_ids, durable_turns) = {
            let Some(conn) = self.lock_or_note("reply_context.build") else {
                return ReplyContext {
                    thread_key: thread_key.to_string(),
                    ..Default::default()
                };
            };
            let title = shogun_memory::thread::recent(&conn, 50)
                .ok()
                .and_then(|rows| rows.into_iter().find(|t| t.thread_key == thread_key))
                .and_then(|t| t.title);
            let ids = shogun_memory::thread::recent_event_ids(&conn, thread_key, REPLY_TURNS)
                .unwrap_or_default();
            let needs_durable = match hot_snapshot.as_ref() {
                Some(snapshot) => {
                    snapshot.is_lossy()
                        || snapshot.newest_first_ids().len() < REPLY_TURNS
                        || snapshot.newest_first_ids() != ids
                }
                None => true,
            };
            let turns = if needs_durable {
                shogun_memory::thread::recent_events(&conn, thread_key, REPLY_TURNS)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            (title, ids, turns)
        };
        let hot_valid = hot_snapshot.as_ref().is_some_and(|snapshot| {
            !snapshot.is_lossy()
                && snapshot.newest_first_ids().len() >= REPLY_TURNS
                && snapshot.newest_first_ids() == durable_ids
                && self.hot_generation() == Some(snapshot.generation())
        });
        let thread_source = thread_key
            .split_once(':')
            .map_or("current-thread", |(source, _)| source);
        let durable_turns = if hot_valid || !durable_turns.is_empty() {
            durable_turns
        } else {
            self.with_conn("reply_context.turns", |conn| {
                shogun_memory::thread::recent_events(conn, thread_key, REPLY_TURNS)
            })
            .unwrap_or_default()
        };
        let mut turns: Vec<Evidence> = if hot_valid {
            hot_snapshot
                .into_iter()
                .flat_map(|snapshot| snapshot.into_oldest_first())
                .map(|(event_id, ts, excerpt)| Evidence {
                    event_id,
                    ts,
                    source: thread_source.into(),
                    title: title.clone(),
                    excerpt,
                    frame_id: None,
                })
                .collect()
        } else {
            durable_turns
                .into_iter()
                .map(|(event_id, ts, content)| Evidence {
                    event_id,
                    ts,
                    source: thread_source.into(),
                    title: title.clone(),
                    excerpt: shogun_memory::search::excerpt(&content, "", REPLY_TURN_CHARS),
                    frame_id: None,
                })
                .collect()
        };
        // Use the canonical grounded-memory path so derived facts keep their raw evidence.
        let grounded = title
            .as_deref()
            .filter(|title| !title.is_empty())
            .map(|title| self.assemble_context(title, REPLY_RELATED, REPLY_RELATED_CHARS))
            .unwrap_or_default();
        for turn in &mut turns {
            if let Some(retrieved) = grounded
                .evidence
                .iter()
                .find(|evidence| evidence.event_id == turn.event_id)
            {
                turn.source.clone_from(&retrieved.source);
                turn.title.clone_from(&retrieved.title);
            }
        }
        let current_ids: std::collections::HashSet<i64> =
            turns.iter().map(|turn| turn.event_id).collect();
        let related = grounded
            .evidence
            .into_iter()
            .filter(|evidence| !current_ids.contains(&evidence.event_id))
            .take(REPLY_RELATED)
            .collect();
        let facts = grounded
            .facts
            .into_iter()
            .take(crate::context::PROMPT_FACT_LIMIT)
            .collect();
        ReplyContext {
            thread_key: thread_key.to_string(),
            title,
            turns,
            facts,
            related,
            visible_text: String::new(),
            build_ms: started.elapsed().as_millis() as u64,
            payload_source: PayloadSource::OnScreenOnly,
        }
    }

    /// event log 上の gmail スレッド候補 `(thread_key, title)`。融合リンカの入力。
    pub fn gmail_thread_candidates(&self, limit: usize) -> Vec<(String, String)> {
        self.with_conn("thread.recent", |conn| {
            shogun_memory::thread::recent(conn, limit)
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.thread_key.starts_with("gmail:"))
        .filter_map(|t| t.title.map(|title| (t.thread_key, title)))
        .collect()
    }

    /// 画面セレクタ（on-screen のタイトル）を使って、取得済み gmail スレッドに解決してから
    /// 文脈を組む。解決できれば gmail スレッドの turns を使い `Fetched`、できなければ元の
    /// thread_key で `OnScreenOnly`（設計 §3）。
    pub fn build_reply_context_for_screen(
        &self,
        on_screen_thread_key: &str,
        on_screen_title: &str,
    ) -> ReplyContext {
        let candidates = self.gmail_thread_candidates(50);
        match shogun_memory::thread::link_on_screen_to_thread(on_screen_title, &candidates) {
            Some(gmail_key) => {
                let mut ctx = self.build_reply_context(&gmail_key);
                // 同期スレッドの識別子（thread_key）を provenance に。
                ctx.payload_source = PayloadSource::Fetched {
                    thread_key: gmail_key,
                };
                ctx
            }
            None => {
                let mut ctx = self.build_reply_context(on_screen_thread_key);
                ctx.payload_source = PayloadSource::OnScreenOnly;
                ctx
            }
        }
    }

    /// Resolve what a referring question ("how's that going?") is about.
    ///
    /// Ranks the recently-active threads by [`shogun_memory::thread::salience`] and classifies the
    /// result. When two threads are close the answer is [`Referent::Ambiguous`] and the caller must
    /// ask rather than pick: answering confidently about the wrong piece of someone's work is a
    /// worse failure than one clarifying question.
    ///
    /// `on_screen` is the thread the user is currently looking at, when known — the strongest
    /// single signal for what "that" means.
    pub fn resolve_referent(&self, query: &str, on_screen: Option<&str>) -> ReferentOutcome {
        use shogun_memory::thread;
        let now = self.now_ms();
        let Some(conn) = self.lock_or_note("referent.resolve") else {
            return ReferentOutcome::default();
        };
        let Ok(threads) = thread::recent(&conn, 20) else {
            self.note_fault("referent.resolve", MemoryFault::Query, "query");
            return ReferentOutcome::default();
        };
        let loops = thread::open_loop_counts(&conn).unwrap_or_default();
        // Lexical agreement between the question and the thread's own title.
        let q = query.to_lowercase();
        let mut scored: Vec<ThreadCandidate> = threads
            .into_iter()
            .map(|t| {
                let title = t.title.clone().unwrap_or_default();
                let lexical = title_overlap(&q, &title.to_lowercase());
                let score = thread::salience(thread::Salience {
                    age_ms: now - t.last_activity_at,
                    open_loops: loops.get(&t.thread_key).copied().unwrap_or(0),
                    on_screen: on_screen == Some(t.thread_key.as_str()),
                    lexical_match: lexical,
                });
                ThreadCandidate {
                    thread_key: t.thread_key,
                    title: t.title,
                    score,
                }
            })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        let scores: Vec<f64> = scored.iter().map(|c| c.score).collect();
        ReferentOutcome {
            verdict: thread::resolve(&scores),
            candidates: scored,
        }
    }

    /// Assemble the grounded context for one question.
    ///
    /// [`Self::inline_memory`] alone answers "what does SHOGUN track about me" but not "what
    /// happened with X" — it never looks at the event log, so mail, messages and captured
    /// windows would not reach an answer. This is the retrieval half: the question is run
    /// through hybrid search and the best-matching events come back as dated, attributed
    /// evidence next to the state facts.
    ///
    /// `max_hits` caps the evidence lines and `excerpt_chars` caps each one, so a single huge
    /// window capture cannot eat the whole prompt. Search is FTS-only until the embedding model
    /// lands (`search_hybrid` takes the vector half then, with no change here).
    pub fn assemble_context(
        &self,
        query: &str,
        max_hits: usize,
        excerpt_chars: usize,
    ) -> ContextPack {
        let retrieval = self.assemble_retrieval(query, max_hits, excerpt_chars);
        let mut facts = retrieval
            .summaries
            .into_iter()
            .map(|summary| summary.text)
            .collect::<Vec<_>>();
        facts.extend(self.inline_memory_for_query(query, FACT_LIMIT));
        ContextPack {
            facts,
            evidence: retrieval.evidence,
            screen_frames: retrieval.screen_frames,
        }
    }

    /// One shared lookup for the form, with the user's source-backed About You lines.
    /// Repeating federated search per field made a 12-field device lookup take 15 seconds.
    /// Form filling is the Mac app's (feature `desktop`, `docs/features/form-mode.md`).
    #[cfg(feature = "desktop")]
    pub fn assemble_form_context(
        &self,
        owner: Option<&str>,
        labels: &[&str],
    ) -> Vec<crate::forms::FormEvidence> {
        use crate::forms::{facts::usable_evidence, FormEvidence};
        let about = self.generated_about_profile().unwrap_or_default();
        let profile = owner
            .into_iter()
            .chain(
                about.lines.iter()
                    .filter(|line| !line.evidence.is_empty())
                    .map(|line| line.text.as_str()),
            )
            .collect::<Vec<_>>();
        let query = form_context_query(&profile, labels);
        let pack = self.assemble_context(&query, 12, 900);
        let mut items = Vec::new();
        // The user's own address, first so the evidence cap never drops it: a form's email field
        // otherwise depends on a search hit that happens to contain it (device 2026-10-16).
        let own_email = self
            .with_conn("calendar.self_email", shogun_memory::calendar::self_email)
            .ok()
            .flatten();
        eprintln!("[form] account_email={}", own_email.is_some());
        if let Some(email) = own_email {
            items.push(FormEvidence {
                id: "account:email".into(),
                text: format!("The user's own email address (their calendar account): {email}"),
            });
        }
        for (i, line) in about.lines.iter().take(4).enumerate() {
            if line.evidence.is_empty() {
                continue;
            }
            let quotes = line
                .evidence
                .iter()
                .take(2)
                .map(|e| e.quote.chars().take(550).collect::<String>())
                .collect::<Vec<_>>();
            items.push(FormEvidence {
                id: format!("about:{}", i + 1),
                text: format!(
                    "Observed user activity: {}. Supporting quotes: {}",
                    line.text,
                    quotes.join(" / ")
                ),
            });
        }
        items.extend(
            pack.facts
                .into_iter()
                .enumerate()
                .map(|(i, text)| FormEvidence {
                    id: format!("fact:{}", i + 1),
                    text: text.chars().take(1_500).collect(),
                }),
        );
        items.extend(pack.evidence.into_iter().map(|item| FormEvidence {
            id: format!("event:{}", item.event_id),
            text: item.excerpt,
        }));
        usable_evidence(items)
    }

    /// The evidence half of [`Self::assemble_context`]: hybrid-search the query and turn the best
    /// hits into dated, attributed [`Evidence`], each excerpt capped at `excerpt_chars`. Split out
    /// so the compressed path can take evidence WITHOUT also loading state facts (which it rebuilds
    /// from the ref version), instead of running the two state queries twice (Issue #63).
    ///
    /// Event-log hits and meeting-interval hits (recap + transcript) are merged by relevance score
    /// so a question about a specific past meeting surfaces that session, not whatever ended last.
    pub(super) fn assemble_retrieval(
        &self,
        query: &str,
        max_hits: usize,
        excerpt_chars: usize,
    ) -> ContextRetrieval {
        self.assemble_retrieval_in(query, None, max_hits, excerpt_chars)
    }

    /// [`Self::assemble_retrieval`] inside an explicit `[from, to)` window instead of the one the
    /// question's own words name: the chat's `search_memory` tool, where the model has already
    /// resolved "last week" against the turn's clock. A window older than Warm also scans Cold.
    pub(super) fn assemble_retrieval_in(
        &self,
        query: &str,
        window: Option<(i64, i64)>,
        max_hits: usize,
        excerpt_chars: usize,
    ) -> ContextRetrieval {
        if query.trim().is_empty() || max_hits == 0 {
            return ContextRetrieval::default();
        }
        let started = std::time::Instant::now();
        let now = self.now_ms();
        let local_days = local_day_bounds(now);
        let query_plan = shogun_memory::search::plan_memory_query(query, now, local_days);
        let screen_frames =
            if shogun_memory::search::query_wants_visual_recall(query, now, local_days) {
                self.recall_screen_frames(query, max_hits, excerpt_chars)
            } else {
                Vec::new()
            };
        let mut frame_by_event: std::collections::HashMap<i64, i64> = screen_frames
            .iter()
            .flat_map(|frame| {
                frame
                    .event_id
                    .into_iter()
                    .chain([frame.evidence_event_id])
                    .map(|event_id| (event_id, frame.frame_id))
            })
            .collect();
        let query_embedding = self.query_embedding(query);
        let mut options = shogun_memory::search::FederatedSearchOptions {
            since_ts: query_plan.from_ms,
            to_ts: query_plan.to_ms,
            focus: query_plan.focus,
            excerpt_chars,
            ..Default::default()
        };
        if let Some((from, to)) = window {
            options.since_ts = Some(from);
            options.to_ts = Some(to);
            if from < now.saturating_sub(shogun_memory::search::WARM_WINDOW_MS) {
                options.depth = shogun_memory::search::SearchDepth::All;
            }
        }
        let Some(conn) = self.lock_or_note("memory.context_federated") else {
            return ContextRetrieval {
                screen_frames,
                ..Default::default()
            };
        };
        let candidate_limit = max_hits.saturating_mul(4).max(max_hits).min(100);
        let result = match shogun_memory::search::search_federated(
            &conn,
            query,
            query_embedding.as_deref(),
            now,
            &options,
            candidate_limit,
        ) {
            Ok(result) => result,
            Err(_) => {
                drop(conn);
                self.note_fault("memory.context_federated", MemoryFault::Query, "query");
                return ContextRetrieval {
                    screen_frames,
                    ..Default::default()
                };
            }
        };
        let mut hits = rank_context_hits(&query_plan.topical_terms, result.hits, max_hits);
        // The zero-hit retry widens to 100 candidates per source — the most expensive search
        // this path can run, on exactly the vague questions where the first pass was already
        // slow. It only runs while the turn is still inside the retrieval budget; past it the
        // answer goes out ungrounded but on time (SLO-03 is measured from the press).
        let within_budget = started.elapsed().as_millis() as u64 <= RETRIEVAL_BUDGET_MS;
        if hits.is_empty() && options.per_source_limit < 100 && within_budget {
            options.per_source_limit = 100;
            let retry_query = query_plan.retry_query.as_deref().unwrap_or(query);
            // The connection is held here: check the space on it, never re-lock (E24).
            let retry_embedding = self.query_embedding_on(&conn, retry_query);
            if let Ok(retry) = shogun_memory::search::search_federated(
                &conn,
                retry_query,
                retry_embedding.as_deref(),
                now,
                &options,
                max_hits.saturating_mul(8).max(max_hits).min(100),
            ) {
                hits = rank_context_hits(&query_plan.topical_terms, retry.hits, max_hits);
            }
        }

        // Explicitly saved preferences remain available for paraphrased questions. Keep whole
        // notes within a fixed budget; older/larger notes still use the searchable lane.
        let recent_notes = shogun_memory::memory_notes::recent_context(&conn, 4096).unwrap_or_default();
        let recent_ids = recent_notes.iter().map(|note| note.id).collect::<std::collections::HashSet<_>>();
        // A reply saved as a note before the write guard is not the user's word: never read it back.
        let reply = super::auto_memory::reads_as_reply;
        let mut summaries = recent_notes.into_iter().filter(|note| !reply(&note.text)).enumerate().map(|(index, note)| ContextSummary {
            block_id: i64::MIN.saturating_add((hits.len() + index) as i64),
            text: format!("user-authored memory: {}", note.text),
            confidence: 1.0,
            grounding: None,
        }).collect::<Vec<_>>();
        let mut evidence = Vec::new();
        let mut seen_evidence = std::collections::HashSet::new();
        for (index, hit) in hits.into_iter().enumerate() {
            let event_id = canonical_grounding_event_id(&conn, &hit.evidence_ids);
            let session_id = hit.evidence_ids.iter().find_map(|id| match id {
                shogun_memory::search::CanonicalEvidenceId::Session(id) => Some(*id),
                _ => None,
            });
            let mut grounding = event_id.and_then(|id| {
                context_event_evidence(&conn, id, query, excerpt_chars, &frame_by_event)
            });
            if grounding.is_none()
                && hit.sources.iter().any(|source| {
                    matches!(
                        source.kind,
                        shogun_memory::search::FederatedSourceKind::Meeting
                    )
                })
            {
                grounding = session_id.map(|id| Evidence {
                    event_id: -id,
                    ts: hit.period_start_ms.unwrap_or(0),
                    source: "meeting".to_owned(),
                    title: Some(hit.title.clone()),
                    excerpt: hit.excerpt.clone(),
                    frame_id: None,
                });
            }

            // An imported claim (#391) has no captured evidence to ground it: it enters the pack
            // as a weak, attributed line — `possibly:` plus its `imported:<platform>` label and
            // import record — and never as a fact or an evidence row.
            if matches!(hit.source_kind, shogun_memory::search::FederatedSourceKind::UserNote) {
                let skip = match hit.source_id {
                    shogun_memory::search::FederatedSourceId::UserNote(id) => recent_ids.contains(&id)
                        || shogun_memory::memory_notes::get(&conn, id).ok().flatten().is_some_and(|note| reply(&note.text)),
                    _ => reply(&hit.excerpt),
                };
                if skip { continue; }
                summaries.push(ContextSummary {
                    block_id: i64::MIN.saturating_add(index as i64),
                    text: format!("user-authored memory: {}",hit.excerpt),
                    confidence: 1.0, grounding: None,
                });
                continue;
            }
            if let Some(text) = imported_claim_line(&hit) {
                summaries.push(ContextSummary {
                    block_id: i64::MIN.saturating_add(index as i64),
                    text,
                    confidence: hit.confidence,
                    grounding: None,
                });
                continue;
            }

            if matches!(
                hit.source_kind,
                shogun_memory::search::FederatedSourceKind::Derived(_)
            ) {
                let is_new_grounding = grounding
                    .as_ref()
                    .is_some_and(|item| !seen_evidence.contains(&item.event_id));
                if is_new_grounding && evidence.len() == max_hits {
                    continue;
                }
                let raw = format!("summary (unverified): {} — {}", hit.title, hit.excerpt);
                let text = match shogun_fusion::confidence::treat_fact(&raw, hit.confidence) {
                    shogun_fusion::confidence::Treatment::Fact(text)
                    | shogun_fusion::confidence::Treatment::Possible(text) => text,
                    shogun_fusion::confidence::Treatment::Excluded => continue,
                };
                if let Some(item) = grounding.as_ref() {
                    if seen_evidence.insert(item.event_id) {
                        evidence.push(item.clone());
                    }
                }
                summaries.push(ContextSummary {
                    block_id: i64::MIN.saturating_add(index as i64),
                    text,
                    confidence: hit.confidence,
                    grounding,
                });
                continue;
            }

            if let Some(item) = grounding {
                if seen_evidence.insert(item.event_id) && evidence.len() < max_hits {
                    evidence.push(item);
                }
            }
        }
        drop(conn);

        let selected_ids = evidence
            .iter()
            .filter(|item| item.event_id > 0)
            .map(|item| item.event_id)
            .collect::<Vec<_>>();
        for (event_id, frame_id) in self.frame_ids_for_events(&selected_ids) {
            frame_by_event.entry(event_id).or_insert(frame_id);
        }
        // One row per saved screen (#283). A frame's accessibility text and its OCR text are two
        // texts of one screen, and both may match; `search_for_recall` already collapses them to
        // one hit, so the chat evidence must not show the same screen twice. The accessibility
        // row wins when both are present (it is the more complete text), else the higher-ranked.
        let mut seen_frames = one_row_per_frame(&mut evidence);
        // Visual recall may surface a stored frame even when OCR did not enter the federated top
        // set. Fall back to stored frames alone, bounded by `max_hits`.
        for frame in &screen_frames {
            if evidence.len() == max_hits
                || seen_frames.contains(&frame.frame_id)
                || seen_evidence.contains(&frame.evidence_event_id)
                || frame.event_id.is_some_and(|id| seen_evidence.contains(&id))
            {
                continue;
            }
            seen_frames.insert(frame.frame_id);
            let mut excerpt = frame.text_excerpt.clone();
            if frame.needs_rescan && frame.text_source == "ocr" {
                excerpt.push_str(" [thin OCR — re-scan frame recommended]");
            }
            evidence.push(Evidence {
                event_id: frame.evidence_event_id,
                ts: frame.ts,
                source: format!("visual_recall_{}", frame.text_source),
                title: frame.window_title.clone(),
                excerpt,
                frame_id: Some(frame.frame_id),
            });
            seen_evidence.insert(frame.evidence_event_id);
        }
        if self.prioritize_knowledge_evidence(query, max_hits, excerpt_chars, &mut evidence) {
            summaries.retain(|summary| {
                summary.grounding.as_ref().is_none_or(|grounding| {
                    evidence
                        .iter()
                        .any(|item| item.event_id == grounding.event_id)
                })
            });
        }
        ContextRetrieval {
            summaries,
            evidence,
            screen_frames,
        }
    }

    /// 圧縮版のコンテキスト組み立て（Issue #63）。`config.enabled` が false のときは
    /// 呼び出し側が `assemble_context` を使う想定なので、ここは true 前提。ローカル処理のみ
    /// （LLM を呼ばない）。処理が `COMPRESS_BUDGET_MS` を超えた/失敗したら raw にフォールバック。
    pub fn assemble_context_compressed(
        &self,
        query: &str,
        max_hits: usize,
        excerpt_chars: usize,
        thread_keys: &[String],
        config: &shogun_fusion::compress::CompressionConfig,
    ) -> (ContextPack, shogun_fusion::compress::CompressionStats, bool) {
        self.assemble_context_compressed_within(
            query,
            max_hits,
            excerpt_chars,
            thread_keys,
            config,
            CompressBudget::DEFAULT,
        )
    }

    /// [`Self::assemble_context_compressed`] with an explicit fallback rule. Production callers
    /// use the method above; this one is for tests and harnesses that must choose a branch
    /// deterministically (see [`CompressBudget`]).
    pub fn assemble_context_compressed_within(
        &self,
        query: &str,
        max_hits: usize,
        excerpt_chars: usize,
        thread_keys: &[String],
        config: &shogun_fusion::compress::CompressionConfig,
        budget: CompressBudget,
    ) -> (ContextPack, shogun_fusion::compress::CompressionStats, bool) {
        use shogun_fusion::budget::HeuristicEstimator;
        use shogun_fusion::compress::{compress, Candidates};

        let started = std::time::Instant::now();
        // 支配コストは evidence 検索。まずそれだけ走らせ、直後に予算をチェックする。
        // fact は各由来で 1 回だけ読む（compressed は ref 版、fallback は文字列版）ので、ここで
        // assemble_context を呼んで両方を二重ロードすることはしない。
        let retrieval = self.assemble_retrieval(query, max_hits, excerpt_chars);
        let ContextRetrieval {
            summaries,
            evidence,
            screen_frames,
        } = retrieval;
        let est = HeuristicEstimator::default();

        // 検索直後の早期フォールバック: 支配コストの直後で予算を超えていたら、要約組み立てや
        // compress に進まず raw をそのまま返す（この guard こそが実際に総処理時間を縛る）。
        if budget.exceeded(started) {
            return self.raw_fallback(query, summaries, evidence, screen_frames, started);
        }

        // fact ブロックは実 state id 付きの ref 版から組み立てる（fallback 用の文字列版とは
        // 別。ここで二重に読まない）。
        let mut blocks = summaries
            .iter()
            .map(|summary| {
                let mut block = shogun_fusion::block::ContextBlock::new(
                    shogun_fusion::block::BlockRef::Event(summary.block_id),
                    shogun_fusion::block::SourceKind::Structured,
                    summary.text.clone(),
                    shogun_fusion::block::ScoreInputs {
                        relevance: 0.95,
                        freshness: 0.5,
                        task_link: 0.0,
                        confidence: summary.confidence,
                    },
                    &est,
                );
                // Summary and its one raw grounding row are one budget atom: Fusion can never
                // keep an assertion while silently dropping the evidence that justifies it.
                if let Some(grounding) = summary.grounding.as_ref() {
                    block.tokens = block.tokens.saturating_add(est.count(&grounding.excerpt));
                }
                block
            })
            .collect::<Vec<_>>();
        blocks.extend(facts_to_blocks(
            &self.inline_memory_for_query_with_refs(query, FACT_LIMIT),
            &est,
        ));
        let paired_evidence = summaries
            .iter()
            .filter_map(|summary| summary.grounding.as_ref().map(|item| item.event_id))
            .collect::<std::collections::HashSet<_>>();
        let standalone_evidence = evidence
            .iter()
            .filter(|item| !paired_evidence.contains(&item.event_id))
            .cloned()
            .collect::<Vec<_>>();
        blocks.extend(evidence_to_blocks(
            &standalone_evidence,
            EVIDENCE_RELEVANCE,
            &est,
        ));
        // 解決済みスレッドの保存済み要約を候補に加える（差し替えレバー）。
        blocks.extend(self.thread_summaries_to_blocks(thread_keys, &est));
        // 取得 evidence の属する session の保存済み要約を候補に加える（thread と対称・Issue #63）。
        // N+1 を避け 1 クエリで一括取得。既に ThreadSummary として消費済みの
        // thread_key を持つ session は重複になるのでスキップ。
        let consumed_threads: std::collections::HashSet<&str> =
            thread_keys.iter().map(String::as_str).collect();
        let evidence_event_ids: Vec<i64> = evidence.iter().map(|e| e.event_id).collect();
        let session_ids = self.session_ids_for_events(&evidence_event_ids);
        for (sid, thread_key, summary) in self.session_summaries_for(&session_ids) {
            if consumed_threads.contains(thread_key.as_str()) {
                // 同じ会話が ThreadSummary として既に候補に入っている。重複を避ける。
                continue;
            }
            let Some(s) = summary else { continue };
            blocks.push(shogun_fusion::block::ContextBlock::new(
                shogun_fusion::block::BlockRef::Session(sid),
                shogun_fusion::block::SourceKind::SessionSummary,
                s,
                // 参照先として retrieved evidence が属する＝関連度は高い。confidence は要約＝1.0。
                SESSION_SUMMARY_SCORE,
                &est,
            ));
        }

        // 最終 guard: 要約組み立て後・compress 前にもう一度予算をチェック（保険）。
        if budget.exceeded(started) {
            return self.raw_fallback(query, summaries, evidence, screen_frames, started);
        }

        let out = compress(Candidates { blocks }, config);
        // 圧縮済みブロックから ContextPack を再構成（facts と evidence に振り分け）。
        // evidence の ts/source/title を検索結果から復元するため、
        // event_id → &Evidence マップを事前に作る。
        let ev_by_id: std::collections::HashMap<i64, &Evidence> =
            evidence.iter().map(|e| (e.event_id, e)).collect();
        let summaries_by_id = summaries
            .iter()
            .map(|summary| (summary.block_id, summary))
            .collect::<std::collections::HashMap<_, _>>();
        let mut out_facts = Vec::new();
        let mut out_evidence = Vec::new();
        let mut emitted_evidence = std::collections::HashSet::new();
        for b in &out.blocks {
            match b.id_ref {
                shogun_fusion::block::BlockRef::Event(id) => {
                    if b.source_kind == SourceKind::Structured {
                        out_facts.push(b.text.clone());
                        if let Some(grounding) = summaries_by_id
                            .get(&id)
                            .and_then(|summary| summary.grounding.as_ref())
                        {
                            if emitted_evidence.insert(grounding.event_id) {
                                out_evidence.push(grounding.clone());
                            }
                        }
                        continue;
                    }
                    let (ts, source, title, frame_id) = ev_by_id
                        .get(&id)
                        .map(|e| (e.ts, e.source.clone(), e.title.clone(), e.frame_id))
                        .unwrap_or((0, String::new(), None, None));
                    if emitted_evidence.insert(id) {
                        out_evidence.push(Evidence {
                            event_id: id,
                            ts,
                            source,
                            title,
                            excerpt: b.text.clone(),
                            frame_id,
                        });
                    }
                }
                // State facts passed the confidence gate upstream (High asserted, Medium already
                // "possibly:"-prefixed) — they may stand as facts.
                shogun_fusion::block::BlockRef::State { .. } => out_facts.push(b.text.clone()),
                // Thread/session summaries are extractive or model text with NO confidence gate
                // and no provenance row. Handing them to the prompt naked would give a summary
                // the same authority as a gated fact — label them so the model treats them as
                // context, never as something it may assert.
                shogun_fusion::block::BlockRef::Thread(_)
                | shogun_fusion::block::BlockRef::Session(_) => {
                    out_facts.push(format!("summary (unverified): {}", b.text));
                }
                // Lesson blocks are built only inside `assemble` (D-5, ContextCache.lesson_lines)
                // — this compression path never constructs one. If one ever appears, treat it
                // like the unverified-summary case: context, never an assertable fact.
                shogun_fusion::block::BlockRef::Lesson(_) => {
                    out_facts.push(format!("summary (unverified): {}", b.text));
                }
            }
        }

        // 計測を記録（best-effort。本文は保存せず query_hash のみ）。
        let compress_ms = started.elapsed().as_millis() as i64;
        self.record_compression_metric(
            query,
            "compressed",
            out.stats.pre_tokens as i64,
            out.stats.post_tokens as i64,
            compress_ms,
            compress_ms,
        );
        self.record_harness_compression(crate::metrics::CompressionHarness {
            enabled: true,
            pre_tokens: out.stats.pre_tokens as u64,
            post_tokens: out.stats.post_tokens as u64,
            dropped: out.stats.dropped as u64,
        });

        (
            ContextPack {
                facts: out_facts,
                evidence: out_evidence,
                screen_frames,
            },
            out.stats,
            false,
        )
    }

    /// 予算超過時の raw フォールバック。fact は文字列版を「ここで」1 回だけ読む（compressed 経路の
    /// ref 版とは別・二重ロード回避）。AB を対称化するため "raw" 計測も best-effort で
    /// 記録する。返り値は `(pack, default_stats, true)`。
    pub(super) fn raw_fallback(
        &self,
        query: &str,
        summaries: Vec<ContextSummary>,
        evidence: Vec<Evidence>,
        screen_frames: Vec<ScreenFrameRef>,
        started: std::time::Instant,
    ) -> (ContextPack, shogun_fusion::compress::CompressionStats, bool) {
        use shogun_fusion::budget::TokenEstimator;
        let est = shogun_fusion::budget::HeuristicEstimator::default();
        // fact は文字列版を「ここで」1 回だけ読む（ref 版は読まない・二重ロード回避）。
        let mut facts = summaries
            .into_iter()
            .map(|summary| summary.text)
            .collect::<Vec<_>>();
        facts.extend(self.inline_memory_for_query(query, FACT_LIMIT));
        // pre==post（圧縮していない）。fact テキストと evidence 抜粋のトークン量を best-effort で
        // 見積もる（block 化と同じ est.count なので概ね一致）。
        let pre_tokens: usize = facts
            .iter()
            .map(|f| est.count(f))
            .chain(evidence.iter().map(|e| est.count(&e.excerpt)))
            .sum();
        let elapsed_ms = started.elapsed().as_millis() as i64;
        self.record_compression_metric(
            query,
            "raw",
            pre_tokens as i64,
            pre_tokens as i64,
            elapsed_ms,
            elapsed_ms,
        );
        self.record_harness_compression(crate::metrics::CompressionHarness {
            enabled: false,
            pre_tokens: pre_tokens as u64,
            post_tokens: pre_tokens as u64,
            dropped: 0,
        });
        (
            ContextPack {
                facts,
                evidence,
                screen_frames,
            },
            shogun_fusion::compress::CompressionStats::default(),
            true,
        )
    }

    /// 解決済みスレッドの保存済み要約を ThreadSummary ブロックにする。要約は raw ターンより
    /// 短くトークン効率が高いので、高 relevance を与えると予算逼迫時に raw を押しのけて残る
    /// （設計 §3.3/§3.4 の差し替えレバー）。summary 未設定のスレッドはスキップ。
    pub(super) fn thread_summaries_to_blocks(
        &self,
        thread_keys: &[String],
        est: &dyn TokenEstimator,
    ) -> Vec<ContextBlock> {
        thread_keys
            .iter()
            .filter_map(|tk| {
                self.thread_summary(tk).map(|s| {
                    ContextBlock::new(
                        BlockRef::Thread(tk.clone()),
                        SourceKind::ThreadSummary,
                        s,
                        // 参照先として解決済み＝関連度は高い。confidence は要約＝1.0。
                        THREAD_SUMMARY_SCORE,
                        est,
                    )
                })
            })
            .collect()
    }

    /// 圧縮計測を 1 行記録する。best-effort（失敗は握りつぶし、作業を止めない）。
    /// query は xxh64 化して保存（本文は保存しない、テレメトリ規約 G8）。ハッシュは capture /
    /// traceability と同じ twox-hash（seed 0）で、下位 16 桁の lower-hex に揃える。
    pub fn record_compression_metric(
        &self,
        query: &str,
        path: &str,
        pre_tokens: i64,
        post_tokens: i64,
        compress_ms: i64,
        assemble_ms: i64,
    ) {
        let query_hash = Self::content_hash(query);
        let _ = self.with_conn("compression_metrics.insert", |conn| {
            shogun_memory::compression_metrics::insert(
                conn,
                &shogun_memory::compression_metrics::MetricRow {
                    ts: self.now_ms(),
                    query_hash,
                    path: path.to_string(),
                    pre_tokens,
                    post_tokens,
                    compress_ms,
                    assemble_ms,
                },
            )
        });
    }

    /// A traceability sink that writes through this same handle (the LLM egress records here).
    pub fn traceability_sink(&self) -> DbTraceabilitySink {
        DbTraceabilitySink::new(self.conn.clone(), self.clock.clone())
    }

    /// One bounded audit worker shared by the microphone and system-audio streams.
    pub fn meeting_traceability_sink(&self) -> std::io::Result<crate::db_sink::MeetingAuditSink> {
        crate::db_sink::MeetingAuditSink::new(
            self.meeting_audit_conn
                .as_ref()
                .unwrap_or(&self.conn)
                .clone(),
            self.clock.clone(),
        )
    }

    /// A usage sink that writes `llm_usage` rows through this same handle (FR-HN-01 / #313 —
    /// token counts, cache hits, latency and outcome per LLM call, never text).
    pub fn usage_sink(&self) -> crate::db_sink::DbUsageSink {
        crate::db_sink::DbUsageSink::new(self.conn.clone(), self.clock.clone())
    }

    /// Convenience for LLM-client construction sites: the usage sink wrapped as a hook, tagged
    /// with the caller's registered prompt identity (FR-HN-02).
    pub fn usage_hook(&self, tag: crate::llm::prompts::PromptTag) -> crate::llm::usage::UsageHook {
        crate::llm::usage::UsageHook::new(std::sync::Arc::new(self.usage_sink()), tag)
    }

    /// Execute a confirmed L3 send (§6.14): persist a traceability row through this handle,
    /// then perform the send over `transport` — no send reaches the wire without a trace
    /// (invariant 3 / FR-TR-03). The row is written before the attempt, so a send that fails after
    /// reaching the provider is still recorded (issue #782); a precheck refusal sends and records
    /// nothing. The DB write is the same digest-only row every egress records (no body text, G8).
    #[cfg(feature = "daemon-server")]
    pub fn execute_confirmed_send<T: crate::send_exec::SendTransport + ?Sized>(
        &self,
        confirmed: &shogun_agents::approval::ConfirmedSend,
        transport: &T,
    ) -> crate::send_exec::SendExecOutcome {
        crate::send_exec::execute_send(confirmed, transport, &self.traceability_sink())
    }

    /// Read traceability rows, preserving failures for the privacy viewer.
    /// Bounded traceability viewer page.
    pub fn try_trace_page(&self, limit: i64, offset: i64) -> MemoryResult<Vec<(i64, TraceRow)>> {
        self.with_conn("traceability.page", |c| {
            shogun_memory::traceability::list_page_with_ids(c, &Filter::default(), limit, offset)
        })
    }

    /// Resolve a trace cursor by its persisted identity, never by display metadata.
    pub fn try_trace_row(&self, id: i64) -> MemoryResult<Option<TraceRow>> {
        self.with_conn("traceability.row", |c| shogun_memory::traceability::find(c, id))
    }

    /// Apply the same persisted filters before paging, keeping unrelated rows out of the page.
    pub fn try_filtered_trace_page(&self, filter: &Filter, limit: i64, offset: i64) -> MemoryResult<Vec<(i64, TraceRow)>> {
        self.with_conn("traceability.page", |c| {
            shogun_memory::traceability::list_page_with_ids(c, filter, limit, offset)
        })
    }

    /// Whole-log disclosure counts, independent of the visible page.
    pub fn try_trace_totals(&self) -> MemoryResult<(i64, i64)> {
        self.with_conn("traceability.totals", shogun_memory::traceability::disclosure_totals)
    }

    pub fn try_trace_rows(&self, filter: &Filter) -> MemoryResult<Vec<TraceRow>> {
        self.with_conn("traceability.list", |c| {
            shogun_memory::traceability::list(c, filter)
        })
    }

    /// Best-effort trace rows for diagnostics and internal callers.
    pub fn trace_rows(&self, filter: &Filter) -> Vec<TraceRow> {
        self.try_trace_rows(filter).unwrap_or_default()
    }

    /// Shared-service round-trip percentiles per route since `since_ts` (issue #29 §4-6). Empty
    /// on any read failure — a metrics read never interrupts anything.
    pub fn shared_service_latencies(
        &self,
        since_ts: i64,
    ) -> Vec<shogun_memory::shared_service_rtt::RouteLatency> {
        self.with_conn("shared_service_rtt.latencies", |c| {
            shogun_memory::shared_service_rtt::latencies_since(c, since_ts)
        })
        .unwrap_or_default()
    }

    /// The `shogun metrics` "shared service round trip" block (issue #29 §4-6): the last seven
    /// days per route, timings and counts only. `None` when the table cannot be read, which the
    /// renderer shows as unmeasured — never as a fabricated zero.
    pub fn shared_service_metrics(&self) -> Option<Vec<crate::metrics::SharedServiceLatency>> {
        const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
        let since = self.now_ms().saturating_sub(WEEK_MS);
        self.with_conn("shared_service_rtt.metrics", |c| {
            shogun_memory::shared_service_rtt::latencies_since(c, since)
        })
        .ok()
        .map(|rows| {
            rows.into_iter()
                .map(|l| crate::metrics::SharedServiceLatency {
                    route: l.route.as_str(),
                    count: l.count,
                    p50_ms: l.p50_ms,
                    p95_ms: l.p95_ms,
                    throttled: l.throttled,
                })
                .collect()
        })
    }

    /// The current time via the injected clock.
    pub fn now_ms(&self) -> i64 {
        (self.clock)()
    }
}

/// 検索 evidence を圧縮ブロックへ正規化する。relevance は呼び出し側が渡す検索スコア由来。
/// Calendar metadata is a structured fact; its description remains untrusted searchable evidence.
pub(super) fn evidence_to_blocks(
    evidence: &[Evidence],
    relevance: f64,
    est: &dyn shogun_fusion::budget::TokenEstimator,
) -> Vec<shogun_fusion::block::ContextBlock> {
    context_assembly::evidence_to_blocks(evidence, relevance, est)
}

/// Equal FTS scores: calendar metadata outranks AX / mail (issue #35 raw pack).
#[cfg(test)]
pub(super) fn cmp_ranked_evidence(a: &(f64, Evidence), b: &(f64, Evidence)) -> std::cmp::Ordering {
    b.0.partial_cmp(&a.0)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(evidence_source_rank(&b.1.source).cmp(&evidence_source_rank(&a.1.source)))
        .then(b.1.ts.cmp(&a.1.ts))
}

#[cfg(test)]
fn evidence_source_rank(source: &str) -> u8 {
    if context_assembly::is_calendar_source(source) {
        shogun_fusion::score::source_rank(SourceKind::Structured)
    } else {
        shogun_fusion::score::source_rank(SourceKind::Evidence)
    }
}

/// Whether an evidence row is the OCR text of a saved screen rather than ordinary memory.
fn ocr_derived(source: &str) -> bool {
    matches!(source, "screen_ocr" | "user_screenshot")
}

/// Keep one evidence row per saved screen, preferring the accessibility (window) text over the
/// OCR text and otherwise the earlier, higher-ranked row. Returns the frames still represented.
fn one_row_per_frame(evidence: &mut Vec<Evidence>) -> std::collections::HashSet<i64> {
    let mut keep: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for (index, item) in evidence.iter().enumerate() {
        let Some(frame_id) = item.frame_id else {
            continue;
        };
        let replace = match keep.get(&frame_id) {
            Some(&kept) => ocr_derived(&evidence[kept].source) && !ocr_derived(&item.source),
            None => true,
        };
        if replace {
            keep.insert(frame_id, index);
        }
    }
    let mut index = 0usize;
    evidence.retain(|item| {
        let current = index;
        index += 1;
        item.frame_id
            .is_none_or(|frame_id| keep.get(&frame_id) == Some(&current))
    });
    keep.into_keys().collect()
}

pub(super) fn context_event_evidence(
    conn: &Connection,
    event_id: i64,
    query: &str,
    excerpt_chars: usize,
    known_frames: &std::collections::HashMap<i64, i64>,
) -> Option<Evidence> {
    let (ts, source, title, content) = conn
        .query_row(
            &format!(
                "SELECT ts, source, window_title, content FROM event_log WHERE id = ?1 AND {}",
                shogun_memory::actor::current_read().predicate("")
            ),
            [event_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .ok()
        .flatten()?;
    let frame_id = known_frames.get(&event_id).copied().or_else(|| {
        shogun_memory::screen_frames::frame_ids_for_events(conn, &[event_id])
            .ok()
            .and_then(|frames| frames.get(&event_id).copied())
    });
    let excerpt = shogun_memory::search::excerpt(&content, query, excerpt_chars);
    Some(Evidence {
        event_id,
        ts,
        source,
        title,
        excerpt,
        frame_id,
    })
}

/// The context line for an imported claim, or `None` for any other hit (or a Low one, which never
/// reaches generation). Always `possibly:`-prefixed: the confidence is clamped into the Medium band
/// before the shared gate, so no stored value can promote an import to an asserted fact.
fn imported_claim_line(hit: &shogun_memory::search::FederatedHit) -> Option<String> {
    let shogun_memory::search::FederatedSourceKind::Imported(platform) = &hit.source_kind else {
        return None;
    };
    let record = hit.evidence_ids.iter().find_map(|id| match id {
        shogun_memory::search::CanonicalEvidenceId::MemoryImport(id) => Some(*id),
        _ => None,
    })?;
    let raw = format!(
        "imported claim (unverified; {}, memory_import:{record}): {}",
        platform.provenance(),
        hit.excerpt
    );
    let confidence = hit
        .confidence
        .min(shogun_memory::memory_import::CONFIDENCE_MAX);
    match shogun_fusion::confidence::treat_fact(&raw, confidence) {
        shogun_fusion::confidence::Treatment::Possible(text) => Some(text),
        shogun_fusion::confidence::Treatment::Fact(_)
        | shogun_fusion::confidence::Treatment::Excluded => None,
    }
}

fn canonical_grounding_event_id(
    conn: &Connection,
    evidence_ids: &[shogun_memory::search::CanonicalEvidenceId],
) -> Option<i64> {
    if let Some(event_id) = evidence_ids.iter().find_map(|id| match id {
        shogun_memory::search::CanonicalEvidenceId::Event(id) => Some(*id),
        _ => None,
    }) {
        return Some(event_id);
    }
    for summary_id in evidence_ids.iter().filter_map(|id| match id {
        shogun_memory::search::CanonicalEvidenceId::PeriodSummary(id) => Some(*id),
        _ => None,
    }) {
        let event_id = conn
            .query_row(
                "WITH RECURSIVE descendants(id) AS (
                     VALUES(?1)
                     UNION
                     SELECT p.child_summary_id
                       FROM period_summary_child_provenance p
                       JOIN descendants d ON d.id = p.summary_id
                 )
                 SELECT p.event_id
                   FROM period_summary_event_provenance p
                   JOIN descendants d ON d.id = p.summary_id
                  ORDER BY p.summary_id, p.event_id LIMIT 1",
                [summary_id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if event_id.is_some() {
            return event_id;
        }
    }
    None
}

/// Local-rule extraction is for speech-act prose (mail, chat, docs, AX). Structured-fact
/// sources skip it so a calendar description cannot become a 0.4 "possibly" commitment.
pub(super) fn extracts_untrusted_body(source: &str) -> bool {
    !matches!(shogun_mcp::scope::from_source(source), Some(svc) if svc.is_structured_fact())
}

/// confidence ゲートを通した facts を圧縮ブロックへ正規化する。facts は既に
/// `treat_fact` を通っている（低 confidence は除外済み）。
/// 各エントリは (表示テキスト, 由来テーブル, 実 row id) のタプル。
/// relevance はやや高めに固定（state は現在の作業に紐づく前提）、confidence は High 相当として 0.9。
fn facts_to_blocks(
    facts: &[(String, shogun_fusion::block::StateTable, i64)],
    est: &dyn shogun_fusion::budget::TokenEstimator,
) -> Vec<shogun_fusion::block::ContextBlock> {
    context_assembly::facts_to_blocks(facts, est)
}
