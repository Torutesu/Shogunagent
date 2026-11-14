use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rusqlite::{params, Connection, ErrorCode};

use super::model::SearchHit;
use super::query::fts_query;
use super::ranking::reciprocal_rank_fusion;

/// Internal event-FTS candidate retaining SQLite's exact BM25 rank.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FtsSearchHit {
    pub id: i64,
    pub rank: f64,
}

/// FTS over one `event_log.source` tag (e.g. `screen_ocr` for visual recall).
pub fn fts_search_source(
    conn: &Connection,
    query: &str,
    source: &str,
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    let Some(expr) = fts_query(query) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT f.rowid FROM event_fts f
         INNER JOIN event_log e ON e.id = f.rowid
         WHERE event_fts MATCH ?1 AND e.source = ?2
         ORDER BY f.rank, f.rowid ASC LIMIT ?3",
    )?;
    let ids = stmt
        .query_map(params![expr, source, limit as i64], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Full-text search over the event log, best-match first, capped at `limit`. Returns event
/// ids ordered by bm25 relevance (SQLite's bm25 is more-negative-is-better, so ascending).
pub fn fts_search(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    Ok(fts_search_scored(conn, query, limit)?
        .into_iter()
        .map(|hit| hit.id)
        .collect())
}

pub(crate) fn fts_search_scored(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<FtsSearchHit>, rusqlite::Error> {
    fts_search_since_scored(conn, query, None, limit)
}

const ARCHIVE_FTS_MATCH_CAP: usize = 512;
const ARCHIVE_FTS_VM_GRANULARITY: i32 = 1_000;
const ARCHIVE_FTS_VM_CALLBACKS: usize = 1_000;

/// Attempt archive-wide BM25 only when its work is deterministically bounded.
///
/// FTS5 ranks every match before applying `LIMIT`. This preflight enumerates at most 513 matches
/// without ranking, under a SQLite VM-step budget. Only queries with at most 512 matches enter
/// BM25, also under a VM-step budget. `None` means callers must retain Warm results and report
/// Warm lexical scope.
pub fn fts_search_all_history_bounded(
    conn: &mut Connection,
    query: &str,
    limit: usize,
) -> Result<Option<Vec<i64>>, rusqlite::Error> {
    Ok(fts_search_all_history_bounded_scored(conn, query, limit)?
        .map(|hits| hits.into_iter().map(|hit| hit.id).collect()))
}

pub(crate) fn fts_search_all_history_bounded_scored(
    conn: &mut Connection,
    query: &str,
    limit: usize,
) -> Result<Option<Vec<FtsSearchHit>>, rusqlite::Error> {
    let Some(expr) = fts_query(query) else {
        return Ok(Some(Vec::new()));
    };

    let matches = with_archive_fts_budget(conn, |conn| {
        let mut stmt =
            conn.prepare("SELECT rowid FROM event_fts WHERE event_fts MATCH ?1 LIMIT ?2")?;
        let ids = stmt
            .query_map(params![&expr, (ARCHIVE_FTS_MATCH_CAP + 1) as i64], |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    })?;
    let Some(matches) = matches else {
        return Ok(None);
    };
    if matches.len() > ARCHIVE_FTS_MATCH_CAP {
        return Ok(None);
    }

    with_archive_fts_budget(conn, |conn| {
        let mut stmt = conn.prepare(
            "SELECT rowid, bm25(event_fts) FROM event_fts WHERE event_fts MATCH ?1
              ORDER BY rank, rowid ASC LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(params![&expr, limit as i64], |row| {
                Ok(FtsSearchHit {
                    id: row.get(0)?,
                    rank: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    })
}

/// Runs `operation` under the archive VM-step budget. The progress handler is always removed
/// before returning; failing to remove it is reported as the error, so a budget never outlives
/// the operation it bounds.
fn with_archive_fts_budget<T>(
    conn: &mut Connection,
    operation: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
) -> Result<Option<T>, rusqlite::Error> {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let handler_callbacks = Arc::clone(&callbacks);
    conn.progress_handler(
        ARCHIVE_FTS_VM_GRANULARITY,
        Some(move || {
            handler_callbacks.fetch_add(1, Ordering::Relaxed) + 1
                >= ARCHIVE_FTS_VM_CALLBACKS
        }),
    )?;
    let result = operation(conn);
    conn.progress_handler(0, None::<fn() -> bool>)?;

    match result {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == ErrorCode::OperationInterrupted =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Deterministic Warm fallback for queries too broad for archive-wide BM25.
///
/// The timestamp lookup, FTS walk, and exact timestamp filter all share the same SQLite VM-step
/// budget. A docid floor lets FTS5 skip old postings before the exact timestamp join. Results use
/// newest-docid order instead of BM25, so `LIMIT` stops the walk without scoring every match.
/// If the VM budget is exhausted, the safely collected prefix is returned.
pub fn fts_search_fallback_bounded(
    conn: &mut Connection,
    query: &str,
    range: SearchRange,
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    let Some(expr) = fts_query(query) else {
        return Ok(Vec::new());
    };
    if limit == 0 || range.from_ms >= range.to_ms {
        return Ok(Vec::new());
    }

    let callbacks = Arc::new(AtomicUsize::new(0));
    let handler_callbacks = Arc::clone(&callbacks);
    conn.progress_handler(
        ARCHIVE_FTS_VM_GRANULARITY,
        Some(move || {
            handler_callbacks.fetch_add(1, Ordering::Relaxed) + 1
                >= ARCHIVE_FTS_VM_CALLBACKS
        }),
    )?;
    let conn_ref: &Connection = conn;
    let result = (|| {
        let conn = conn_ref;
        let min_id = match conn.query_row(
            "SELECT id FROM event_log
              WHERE ts >= ?1 AND ts < ?2 ORDER BY id ASC LIMIT 1",
            params![range.from_ms, range.to_ms],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(id) => id,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut stmt = conn.prepare(
            "SELECT f.rowid FROM event_fts f
             INNER JOIN event_log e ON e.id = f.rowid
             WHERE event_fts MATCH ?1 AND f.rowid >= ?2
               AND e.ts >= ?3 AND e.ts < ?4
             ORDER BY f.rowid DESC LIMIT ?5",
        )?;
        let mut rows = stmt.query(params![
            expr,
            min_id,
            range.from_ms,
            range.to_ms,
            limit as i64
        ])?;
        let mut ids = Vec::with_capacity(limit);
        loop {
            match rows.next() {
                Ok(Some(row)) => ids.push(row.get::<_, i64>(0)?),
                Ok(None) => return Ok(ids),
                Err(error) if is_operation_interrupted(&error) => return Ok(ids),
                Err(error) => return Err(error),
            }
        }
    })();
    conn.progress_handler(0, None::<fn() -> bool>)?;

    match result {
        Err(error) if is_operation_interrupted(&error) => Ok(Vec::new()),
        other => other,
    }
}

fn is_operation_interrupted(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::OperationInterrupted
    )
}

/// [`fts_search`], optionally restricted to events at or after `since_ts`.
///
/// **Why the bound exists.** BM25 ranking through FTS5's `rank` has to score matching rows before it can
/// take the top `limit`, so the cost tracks how much of the log the query matches, not how much
/// is returned. Measured on an Apple Silicon device, an unbounded search over 40k events reached
/// 506ms against a 500ms budget — already over, and it grows with the log.
///
/// Restricting to a recent window is the 3-tier memory design (Warm is what ordinary search
/// targets), not a shortcut invented for latency: a question is nearly always about recent work.
/// The caller escalates to the full history when a bounded search comes back thin, so nothing
/// older becomes unreachable — see [`crate::search::WARM_WINDOW_MS`].
pub fn fts_search_since(
    conn: &Connection,
    query: &str,
    since_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    Ok(fts_search_since_scored(conn, query, since_ts, limit)?
        .into_iter()
        .map(|hit| hit.id)
        .collect())
}

pub(crate) fn fts_search_since_scored(
    conn: &Connection,
    query: &str,
    since_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<FtsSearchHit>, rusqlite::Error> {
    fts_search_window_scored(conn, query, since_ts, None, limit)
}

/// [`fts_search_since_scored`] with both ends of the window bounded.
///
/// `since_ts` is a floor and `until_ts` a ceiling; either may be `None`. The upper bound is what
/// makes an **as-of** question expressible at all — "what was the plan in March" needs to exclude
/// everything said since, and a floor alone cannot do that. It is what LongMemEval's
/// temporal-reasoning category (133 of its 500 questions) needs; see `research/COMPARISON.md`.
///
/// Both bounds are translated to docid bounds for the same reason the floor always was: FTS5
/// postings are ordered by docid, so `rowid >= / <=` skips postings instead of scoring them, while
/// a join against `event_log.ts` would score every hit first and save nothing.
///
/// The same approximation applies in both directions. Ids follow insertion order, which tracks
/// time closely but not exactly — a connector backfill can insert an older item later — so an
/// event whose `ts` is inside the window but whose id is outside it can be missed. That is
/// acceptable for a latency bound, and it is why this is a retrieval filter rather than a
/// correctness guarantee.
pub(crate) fn fts_search_window_scored(
    conn: &Connection,
    query: &str,
    since_ts: Option<i64>,
    until_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<FtsSearchHit>, rusqlite::Error> {
    let Some(expr) = fts_query(query) else {
        return Ok(Vec::new());
    };

    // An inverted window selects nothing. Answering it with an unbounded search would be worse
    // than answering it with nothing, because the caller would never learn its bounds were wrong.
    if let (Some(lo), Some(hi)) = (since_ts, until_ts) {
        if lo > hi {
            return Ok(Vec::new());
        }
    }

    // Resolve each time bound to a docid bound. A bound that matches no row at all means the
    // window is empty, which is a result, not a reason to widen the search.
    let max_id: Option<i64> = match until_ts {
        Some(until) => {
            let found = conn
                .query_row(
                    "SELECT id FROM event_log WHERE ts <= ?1 ORDER BY id DESC LIMIT 1",
                    params![until],
                    |r| r.get(0),
                )
                .ok();
            match found {
                Some(v) => Some(v),
                None => return Ok(Vec::new()),
            }
        }
        None => None,
    };

    if let Some(max_id) = max_id {
        return fts_search_bounded(conn, &expr, since_ts, Some(max_id), limit);
    }
    fts_search_bounded(conn, &expr, since_ts, None, limit)
}

/// Shared tail of [`fts_search_window_scored`], with the floor still expressed as a timestamp
/// because resolving it needs the same lookup the unbounded path already did.
fn fts_search_bounded(
    conn: &Connection,
    expr: &str,
    since_ts: Option<i64>,
    max_id: Option<i64>,
    limit: usize,
) -> Result<Vec<FtsSearchHit>, rusqlite::Error> {
    let read = |row: &rusqlite::Row<'_>| -> Result<FtsSearchHit, rusqlite::Error> {
        Ok(FtsSearchHit {
            id: row.get(0)?,
            rank: row.get(1)?,
        })
    };
    match since_ts {
        Some(since) => {
            // Translate the time floor into a docid floor. Joining event_log to filter on `ts`
            // does NOT help: SQLite resolves the MATCH and scores every hit before the join can
            // discard anything, so the bm25 cost is unchanged (measured: no improvement at all).
            // A `rowid >=` constraint is different in kind — FTS5 postings are ordered by docid,
            // so it skips them instead of scoring them.
            //
            // Ids are assigned in insertion order, which tracks time closely but not exactly (a
            // connector backfill can insert an older item later). That makes this bound an
            // approximation, which is fine for a latency bound: anything it misses is recovered
            // by the escalation in `search_warm_first`.
            let min_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM event_log WHERE ts >= ?1 ORDER BY id ASC LIMIT 1",
                    params![since],
                    |r| r.get(0),
                )
                .ok();
            let Some(min_id) = min_id else {
                return Ok(Vec::new()); // nothing in the window at all
            };
            match max_id {
                Some(max_id) => {
                    let mut stmt = conn.prepare(
                        "SELECT rowid, bm25(event_fts) FROM event_fts
                          WHERE event_fts MATCH ?1 AND rowid >= ?2 AND rowid <= ?3
                          ORDER BY rank, rowid ASC LIMIT ?4",
                    )?;
                    let hits = stmt
                        .query_map(params![expr, min_id, max_id, limit as i64], read)?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(hits)
                }
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT rowid, bm25(event_fts) FROM event_fts
                          WHERE event_fts MATCH ?1 AND rowid >= ?2
                          ORDER BY rank, rowid ASC LIMIT ?3",
                    )?;
                    let hits = stmt
                        .query_map(params![expr, min_id, limit as i64], read)?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(hits)
                }
            }
        }
        None => match max_id {
            Some(max_id) => {
                let mut stmt = conn.prepare(
                    "SELECT rowid, bm25(event_fts) FROM event_fts
                      WHERE event_fts MATCH ?1 AND rowid <= ?2
                      ORDER BY rank, rowid ASC LIMIT ?3",
                )?;
                let hits = stmt
                    .query_map(params![expr, max_id, limit as i64], read)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(hits)
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT rowid, bm25(event_fts) FROM event_fts WHERE event_fts MATCH ?1
                      ORDER BY rank, rowid ASC LIMIT ?2",
                )?;
                let hits = stmt
                    .query_map(params![expr, limit as i64], read)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(hits)
            }
        },
    }
}

/// The Warm window ordinary search covers (§ 3-tier memory: Hot 24h / Warm 30d / Cold all).
pub const WARM_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The Cold cutoff: events older than `now - COLD_CUTOFF_MS` have had their Warm f32 embedding
/// demoted to the int8 Cold archive ([`crate::cold`]), so a semantic hit on them can only come
/// from the Cold partition scan. Identical to [`WARM_WINDOW_MS`] by design — demotion and the
/// search boundary must agree or a band of events becomes semantically unreachable.
pub const COLD_CUTOFF_MS: i64 = WARM_WINDOW_MS;

/// Default cap on how many Cold partitions one query scans, newest first (design §2.1, E-09).
///
/// A partition is one [`crate::cold::PARTITION_MS`] period (30 days), so 6 partitions ≈ 7 months
/// of history counting the Warm window. The scan is a brute-force int8 dot product over every row
/// in each partition — bounding the partition count is what keeps the deep path's cost linear in
/// *recent* archive size instead of total history. Callers wanting the full archive pass a larger
/// cap explicitly (CLI/REST/MCP `depth: all` plumbing); the Warm default path never runs this scan.
pub const DEFAULT_MAX_COLD_PARTITIONS: usize = 6;

/// Half-open timestamp bounds used by canonical search. Every accepted candidate must satisfy
/// `from_ms <= ts < to_ms`; event ids are never treated as timestamp order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchRange {
    pub from_ms: i64,
    pub to_ms: i64,
}

impl SearchRange {
    fn contains(self, ts: i64) -> bool {
        (self.from_ms..self.to_ms).contains(&ts)
    }
}

/// Exact-range candidate sources before fixed-order RRF.
#[derive(Debug, Clone, Default)]
pub struct SearchCandidates {
    pub lexical: Vec<i64>,
    pub warm: Vec<i64>,
    pub cold: ColdScan,
}

/// Exact FTS retrieval. The timestamp join is the correctness filter; rowid remains only an id.
pub fn fts_search_range(
    conn: &Connection,
    query: &str,
    range: SearchRange,
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    Ok(fts_search_range_scored(conn, query, range, limit)?
        .into_iter()
        .map(|hit| hit.id)
        .collect())
}

pub(crate) fn fts_search_range_scored(
    conn: &Connection,
    query: &str,
    range: SearchRange,
    limit: usize,
) -> Result<Vec<FtsSearchHit>, rusqlite::Error> {
    let Some(expr) = fts_query(query) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT f.rowid, bm25(event_fts) FROM event_fts f
         INNER JOIN event_log e ON e.id = f.rowid
         WHERE event_fts MATCH ?1 AND e.ts >= ?2 AND e.ts < ?3
         ORDER BY f.rank, f.rowid ASC LIMIT ?4",
    )?;
    let hits = stmt
        .query_map(
            params![expr, range.from_ms, range.to_ms, limit as i64],
            |row| {
                Ok(FtsSearchHit {
                    id: row.get(0)?,
                    rank: row.get(1)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(hits)
}

/// Warm KNN over an exact timestamp candidate set. Filtering is pushed into sqlite-vec's row-id
/// candidate constraint, avoiding an all-Warm result allocation and per-id timestamp queries.
pub fn warm_knn_range(
    conn: &Connection,
    query_vec: &[f32],
    range: SearchRange,
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    if query_vec.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    crate::vector::knn_range(conn, query_vec, range.from_ms, range.to_ms, limit)
}

/// Count populated Cold partitions that contain at least one event inside exact bounds.
pub fn cold_partition_count_in_range(
    conn: &Connection,
    range: SearchRange,
) -> Result<usize, rusqlite::Error> {
    let count: i64 = conn.query_row(
        "SELECT count(DISTINCT c.partition)
         FROM cold_embeddings c
         INNER JOIN event_log e ON e.id = c.event_id
         WHERE c.partition BETWEEN ?1 AND ?2 AND e.ts >= ?3 AND e.ts < ?4",
        params![
            crate::cold::partition_of(range.from_ms),
            crate::cold::partition_of(range.to_ms.saturating_sub(1)),
            range.from_ms,
            range.to_ms,
        ],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

/// Retrieve all exact-range source lists. Callers preflight Cold partition count before setting
/// `include_cold`; this function then visits the complete permitted set.
pub fn search_exact_candidates(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&[f32]>,
    range: SearchRange,
    include_cold: bool,
    max_cold_partitions: usize,
    limit: usize,
) -> Result<SearchCandidates, rusqlite::Error> {
    let lexical = fts_search_range(conn, query, range, limit)?;
    let warm = match query_embedding {
        Some(embedding) => warm_knn_range(conn, embedding, range, limit)?,
        None => Vec::new(),
    };
    let cold = match (include_cold, query_embedding) {
        (true, Some(embedding)) => search_cold_partitions(
            conn,
            embedding,
            (range.from_ms, range.to_ms),
            max_cold_partitions,
            limit,
        )?,
        _ => ColdScan::default(),
    };
    Ok(SearchCandidates {
        lexical,
        warm,
        cold,
    })
}

/// Fixed source-order RRF followed by optional exact-range hydration.
pub fn fuse_and_hydrate(
    conn: &Connection,
    candidates: &SearchCandidates,
    range: Option<SearchRange>,
    limit: usize,
) -> Result<Vec<SearchHit>, rusqlite::Error> {
    let ranked = reciprocal_rank_fusion(
        &[&candidates.lexical, &candidates.warm, &candidates.cold.ids],
        60.0,
    );
    let mut hits = Vec::with_capacity(limit.min(ranked.len()));
    let mut stmt = conn.prepare(
        "SELECT ts, source, app_bundle_id, window_title, content FROM event_log WHERE id = ?1",
    )?;
    for (id, score) in ranked {
        let row = stmt
            .query_row(params![id], |row| {
                Ok(SearchHit {
                    event_id: id,
                    ts: row.get(0)?,
                    source: row.get(1)?,
                    app_bundle_id: row.get(2)?,
                    window_title: row.get(3)?,
                    content: row.get(4)?,
                    score,
                })
            })
            .ok();
        if row
            .as_ref()
            .is_some_and(|hit| range.is_none_or(|range| range.contains(hit.ts)))
        {
            hits.extend(row);
            if hits.len() == limit {
                break;
            }
        }
    }
    Ok(hits)
}

/// Hydrate ranked `(id, score)` pairs into full [`SearchHit`]s, preserving the ranked order.
/// Ids no longer present in the event log (e.g. moved to Cold) are skipped.
pub fn hydrate(
    conn: &Connection,
    ranked: &[(i64, f64)],
) -> Result<Vec<SearchHit>, rusqlite::Error> {
    let mut hits = Vec::with_capacity(ranked.len());
    let mut stmt = conn.prepare(
        "SELECT ts, source, app_bundle_id, window_title, content FROM event_log WHERE id = ?1",
    )?;
    for &(id, score) in ranked {
        let row = stmt
            .query_row(params![id], |r| {
                Ok(SearchHit {
                    event_id: id,
                    ts: r.get(0)?,
                    source: r.get(1)?,
                    app_bundle_id: r.get(2)?,
                    window_title: r.get(3)?,
                    content: r.get(4)?,
                    score,
                })
            })
            .ok();
        if let Some(hit) = row {
            hits.push(hit);
        }
    }
    Ok(hits)
}

/// FTS-only search — the lexical half alone. Kept for callers without an embedder.
pub fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, rusqlite::Error> {
    search_hybrid(conn, query, None, limit)
}

/// A meeting interval surfaced by query-relevant retrieval over `meeting_recaps` and
/// `transcript_segments` (FR-MT-22 chat grounding). `session_id` is the provenance key.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingSearchHit {
    pub session_id: i64,
    pub ts: i64,
    pub title: Option<String>,
    pub content: String,
    pub score: f64,
}

/// Lexical search over meeting minutes and transcripts. Query-relevant — not "latest session".
///
/// Scores each meeting interval by how many query terms hit its recap fields and transcript lines.
/// Returns best-first, capped at `limit`. Empty when the query has no usable terms or nothing
/// matches.
pub fn search_meetings(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<MeetingSearchHit>, rusqlite::Error> {
    search_meetings_in_range(conn, query, None, limit)
}

/// [`search_meetings`] constrained to a half-open session-start range.
pub fn search_meetings_in_range(
    conn: &Connection,
    query: &str,
    range: Option<SearchRange>,
    limit: usize,
) -> Result<Vec<MeetingSearchHit>, rusqlite::Error> {
    let concepts = super::terms::query_concepts(query);
    if concepts.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    // "yesterday's meeting" / 「昨日の会議」: when the question names only a time and "meeting",
    // every meeting in that time is the answer, whatever its title says.
    let browse = range.is_some() && concepts.iter().all(is_meeting_cue);
    // Uniform idf: the meeting lane is small and scanned in full, and its scores are only compared
    // with each other (federated search normalises each lane before fusing).
    let scorer = super::terms::Scorer::new(query, concepts, &[], 2.0, MEETING_AVG_CHARS);

    use std::collections::HashMap;

    struct Candidate {
        ts: i64,
        title: Option<String>,
        summary: Option<String>,
        decisions: Option<String>,
        next_actions: Option<String>,
        transcript: Vec<String>,
        score: f64,
    }

    let mut candidates: HashMap<i64, Candidate> = HashMap::new();

    let mut recap_stmt = conn.prepare(
        "SELECT mr.session_id, s.started_at, s.title, mr.summary, mr.decisions, mr.next_actions
         FROM meeting_recaps mr
         JOIN sessions s ON s.id = mr.session_id
         WHERE s.kind IN ('meeting', 'call')
           AND (?1 IS NULL OR (s.started_at >= ?1 AND s.started_at < ?2))",
    )?;
    let recap_rows = recap_stmt.query_map(
        params![range.map(|value| value.from_ms), range.map(|value| value.to_ms)],
        |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
        },
    )?;
    for row in recap_rows {
        let (sid, ts, title, summary, decisions, next_actions) = row?;
        let score = if browse {
            1.0
        } else {
            scorer
            .score(&mut super::terms::MatchText::new(
                title.as_deref().unwrap_or(""),
                &format!("{summary}\n{decisions}\n{next_actions}"),
            ))
            .score
        };
        if score <= 0.0 {
            continue;
        }
        candidates.insert(
            sid,
            Candidate {
                ts,
                title,
                summary: Some(summary),
                decisions: Some(decisions),
                next_actions: Some(next_actions),
                transcript: Vec::new(),
                score,
            },
        );
    }

    let mut tx_stmt = conn.prepare(
        "SELECT ts.session_id, s.started_at, s.title, ts.text
         FROM transcript_segments ts
         JOIN sessions s ON s.id = ts.session_id
         WHERE s.kind IN ('meeting', 'call') AND ts.excluded = 0
           AND (?1 IS NULL OR (s.started_at >= ?1 AND s.started_at < ?2))",
    )?;
    let tx_rows = tx_stmt.query_map(
        params![range.map(|value| value.from_ms), range.map(|value| value.to_ms)],
        |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
        ))
        },
    )?;
    for row in tx_rows {
        let (sid, ts, title, text) = row?;
        let line_score = scorer
            .score(&mut super::terms::MatchText::new("", &text))
            .score;
        if line_score <= 0.0 {
            continue;
        }
        candidates
            .entry(sid)
            .and_modify(|c| {
                c.score += line_score * 0.5;
                c.transcript.push(text.clone());
            })
            .or_insert_with(|| Candidate {
                ts,
                title,
                summary: None,
                decisions: None,
                next_actions: None,
                transcript: vec![text],
                score: line_score * 0.5,
            });
    }

    let mut hits: Vec<MeetingSearchHit> = candidates
        .into_iter()
        .map(|(session_id, c)| {
            let content = format_meeting_content(
                c.title.as_deref(),
                c.summary.as_deref(),
                c.decisions.as_deref(),
                c.next_actions.as_deref(),
                &c.transcript,
            );
            MeetingSearchHit {
                session_id,
                ts: c.ts,
                title: c.title,
                content,
                score: c.score,
            }
        })
        .filter(|h| !h.content.is_empty())
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.ts.cmp(&a.ts))
            .then(a.session_id.cmp(&b.session_id))
    });
    hits.truncate(limit);
    Ok(hits)
}

pub(crate) fn is_meeting_cue(concept: &super::terms::Concept) -> bool {
    let key = concept
        .surface
        .as_deref()
        .or_else(|| concept.grams.first().map(String::as_str))
        .unwrap_or("");
    matches!(
        key,
        "meeting"
            | "meetings"
            | "mtg"
            | "call"
            | "calls"
            | "standup"
            | "sync"
            | "session"
            | "recap"
            | "transcript"
            | "会議"
            | "打合"
            | "打ち合わせ"
            | "ミーティング"
    )
}

/// Typical length of a recap or transcript line, for BM25 length normalisation in the meeting lane.
const MEETING_AVG_CHARS: f64 = 200.0;

/// Assemble the searchable text for one meeting interval. Transcript lines are capped so a long
/// call cannot dominate the prompt budget before `excerpt` runs.
fn format_meeting_content(
    title: Option<&str>,
    summary: Option<&str>,
    decisions: Option<&str>,
    next_actions: Option<&str>,
    transcript: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        parts.push(format!("Meeting: {t}"));
    }
    if let Some(s) = summary.filter(|s| !s.is_empty()) {
        parts.push(format!("Summary: {s}"));
    }
    if let Some(d) = decisions.filter(|d| !d.is_empty() && *d != "[]") {
        parts.push(format!("Decisions: {d}"));
    }
    if let Some(n) = next_actions.filter(|n| !n.is_empty() && *n != "[]") {
        parts.push(format!("Next actions: {n}"));
    }
    if !transcript.is_empty() {
        let joined = transcript.join(" ");
        const TX_CAP: usize = 4_000;
        if joined.chars().count() > TX_CAP {
            let short: String = joined.chars().take(TX_CAP).collect();
            parts.push(format!("Transcript (excerpt): {short}…"));
        } else {
            parts.push(format!("Transcript: {joined}"));
        }
    }
    parts.join("\n")
}

/// Hybrid search (FR-MEM-20): fuse the FTS lexical list with the Warm-layer vector list via RRF.
/// The caller supplies the query embedding (so shogun-memory stays decoupled from the model);
/// pass `None` to run FTS-only. Both lists rank the same event ids, so the vector half simply
/// becomes a second input to the same fusion. Un-embedded events (no `event_vec` row yet,
/// FR-MEM-22) still appear via the FTS list.
pub fn search_hybrid(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&[f32]>,
    limit: usize,
) -> Result<Vec<SearchHit>, rusqlite::Error> {
    search_hybrid_since(conn, query, query_embedding, None, limit)
}

/// [`search_hybrid`] with the lexical half restricted to `since_ts` — see [`fts_search_since`].
pub fn search_hybrid_since(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&[f32]>,
    since_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<SearchHit>, rusqlite::Error> {
    let fts = fts_search_since(conn, query, since_ts, limit)?;
    let vec = match query_embedding {
        Some(emb) => crate::vector::knn(conn, emb, limit)?,
        None => Vec::new(),
    };
    let ranked = reciprocal_rank_fusion(&[&fts, &vec], 60.0);
    let capped: Vec<(i64, f64)> = ranked.into_iter().take(limit).collect();
    hydrate(conn, &capped)
}

/// Search the Warm window first and widen to the whole history only if that comes back thin.
///
/// This is what keeps the recency bound from silently losing answers: a question about something
/// from months ago still gets answered, it just costs the slower path. `now_ms` is passed in
/// rather than read so the behaviour stays deterministic in tests.
pub fn search_warm_first(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&[f32]>,
    now_ms: i64,
    limit: usize,
) -> Result<Vec<SearchHit>, rusqlite::Error> {
    let warm = search_hybrid_since(
        conn,
        query,
        query_embedding,
        Some(now_ms - WARM_WINDOW_MS),
        limit,
    )?;
    // "Thin" means the window plainly did not hold the answer. Half the asked-for results is a
    // deliberately low bar: widening is the expensive path and should be the exception.
    if warm.len() * 2 >= limit || warm.len() >= limit {
        return Ok(warm);
    }
    search_hybrid_since(conn, query, query_embedding, None, limit)
}

/// How deep the semantic half of hybrid search reaches (design §2.1, E-09).
///
/// `WarmOnly` is the default and matches every pre-existing call path: the vector list comes from
/// the Warm sqlite-vec KNN alone, and the Cold archive is never opened. `All` additionally runs
/// the Cold int8 partition scan ([`search_cold_partitions`]) and fuses its hits as a third RRF
/// source. FTS is unaffected either way — event text stays in `event_log`/FTS after demotion, so
/// the lexical half already spans Cold and must not be re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchDepth {
    /// Semantic search over the Warm window only (the NFR-SLO-04 fast path).
    #[default]
    WarmOnly,
    /// Also scan the Cold int8 archive, up to the configured partition cap.
    All,
}

/// Options for [`search_hybrid_with_options`]. `Default` reproduces [`search_hybrid`] exactly
/// (unbounded FTS, Warm-only semantics, no Cold scan).
#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    /// Lexical floor, as in [`fts_search_since`]. `None` = unbounded.
    pub since_ts: Option<i64>,
    /// Lexical ceiling — the **as-of** bound. `None` = unbounded, which is the default and
    /// reproduces the shipped behaviour exactly.
    ///
    /// With `since_ts` this makes a closed time window expressible, which is what a question like
    /// "what was the plan in March" requires: the answer is the best match *as of then*, and
    /// everything said afterwards has to be excluded rather than merely outranked. A floor alone
    /// cannot express that.
    ///
    /// Bounds the lexical half and the Cold scan. It does **not** bound the Warm vector half
    /// ([`crate::vector::knn`]), which contributes nothing in a distribution build because no
    /// embedder ships (issue #237) — but a developer build with `--features local-embedder` can
    /// return a row from outside the window through that half.
    pub until_ts: Option<i64>,
    /// Semantic reach. See [`SearchDepth`].
    pub depth: SearchDepth,
    /// Cap on Cold partitions scanned (newest first) when the Cold scan runs.
    /// See [`DEFAULT_MAX_COLD_PARTITIONS`].
    pub max_cold_partitions: usize,
    /// Weight of the recency signal blended into the fused score
    /// ([`crate::search::apply_recency`]). **`0.0` is the default and an exact no-op**, so the
    /// shipped ranking is unchanged until a measured sweep justifies a value.
    ///
    /// Applied to the hydrated window, so it reorders results and cannot change which rows are in
    /// them — recall@k is unaffected by construction.
    pub recency_epsilon: f64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            since_ts: None,
            until_ts: None,
            depth: SearchDepth::WarmOnly,
            max_cold_partitions: DEFAULT_MAX_COLD_PARTITIONS,
            recency_epsilon: 0.0,
        }
    }
}

/// What one Cold scan actually did — the honest-measurement counterpart of the partition cap.
/// `partitions_visited == 0` is the proof a Warm-only query never opened the archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColdScanStats {
    /// Non-empty partitions the scan iterated (bounded by the cap).
    pub partitions_visited: usize,
    /// Cold rows whose int8 dot product was computed.
    pub rows_scanned: usize,
}

/// Ranked outcome of a Cold partition scan: event ids best-first, plus scan stats.
#[derive(Debug, Clone, Default)]
pub struct ColdScan {
    /// Event ids ordered by descending dequantized dot product (ties: ascending id).
    pub ids: Vec<i64>,
    pub stats: ColdScanStats,
}

/// One Cold candidate held in the top-k heap. Ordering is total and deterministic:
/// higher score wins; equal scores prefer the smaller event id (matching the RRF tie-break).
struct ColdHit {
    score: f32,
    event_id: i64,
}

impl Ord for ColdHit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // total_cmp gives a total order over f32 (scores are finite here: bounded int8 codes,
        // finite scale and query components). Larger event_id compares *less* so that among
        // equal scores the smaller id survives eviction and ranks first.
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.event_id.cmp(&self.event_id))
    }
}

impl PartialOrd for ColdHit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ColdHit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for ColdHit {}

/// Semantic search over the Cold int8 archive (design §2.1, E-09): a bounded brute-force scan of
/// the period partitions intersecting `range = (from_ms, to_ms)`, newest partition first, at most
/// `max_partitions` of them, keeping a global top-`k` by dot product.
///
/// Each row is scored without dequantizing into an allocation: the stored int8 codes are read as
/// a borrowed blob and dotted against the f32 query on the fly (`code as f32 * q`), then corrected
/// by the row's per-vector scale. Warm embeddings are L2-normalized before quantization, so this
/// dot product ranks like cosine similarity. Rows whose dimension does not match the query (e.g.
/// archived under an older embedding model) are skipped rather than mis-scored.
///
/// This is the *deep* path only — never called from the Warm default path (FR-MEM-03: routine
/// vector search stays on Warm). Cost is `rows_in_visited_partitions × dim` multiplies; the
/// partition cap keeps that bounded regardless of total history size.
pub fn search_cold_partitions(
    conn: &Connection,
    query_vec: &[f32],
    range: (i64, i64),
    max_partitions: usize,
    k: usize,
) -> Result<ColdScan, rusqlite::Error> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let (from_ms, to_ms) = range;
    let mut stats = ColdScanStats::default();
    if query_vec.is_empty() || k == 0 || max_partitions == 0 || from_ms > to_ms {
        return Ok(ColdScan {
            ids: Vec::new(),
            stats,
        });
    }

    // Only partitions that actually hold rows count against the cap — an empty month costs nothing
    // and must not shadow older populated ones.
    let partitions: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT c.partition FROM cold_embeddings c
             INNER JOIN event_log e ON e.id = c.event_id
             WHERE c.partition BETWEEN ?1 AND ?2 AND e.ts >= ?3 AND e.ts < ?4
             ORDER BY c.partition DESC LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![
                crate::cold::partition_of(from_ms),
                crate::cold::partition_of(to_ms.saturating_sub(1)),
                from_ms,
                to_ms,
                max_partitions as i64,
            ],
            |r| r.get::<_, i64>(0),
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    // Min-heap of size ≤ k: the weakest of the current top-k sits on top and is evicted first.
    let mut heap: BinaryHeap<Reverse<ColdHit>> = BinaryHeap::with_capacity(k + 1);
    // Only rows archived in the active embedding space are scored (V102): a row from another
    // model is skipped, not compared.
    let mut stmt = conn.prepare(&format!(
        "SELECT c.event_id, c.scale, c.codes FROM cold_embeddings c
         INNER JOIN event_log e ON e.id = c.event_id
         WHERE c.partition = ?1 AND e.ts >= ?2 AND e.ts < ?3
           AND c.event_id IN ({})
         ORDER BY c.event_id ASC",
        crate::embedding_space::ACTIVE_COLD_ROWS
    ))?;
    for p in partitions {
        stats.partitions_visited += 1;
        let mut rows = stmt.query(params![p, from_ms, to_ms])?;
        while let Some(row) = rows.next()? {
            let event_id: i64 = row.get(0)?;
            let scale: f64 = row.get(1)?;
            let codes: &[u8] = row.get_ref(2)?.as_blob()?;
            stats.rows_scanned += 1;
            if codes.len() != query_vec.len() {
                continue; // archived under a different embedding dim — cannot be scored
            }
            let mut dot = 0.0f32;
            for (b, q) in codes.iter().zip(query_vec) {
                dot += (*b as i8) as f32 * q;
            }
            heap.push(Reverse(ColdHit {
                score: dot * scale as f32,
                event_id,
            }));
            if heap.len() > k {
                heap.pop();
            }
        }
    }

    let mut hits: Vec<ColdHit> = heap.into_iter().map(|r| r.0).collect();
    hits.sort_by(|a, b| b.cmp(a)); // best-first, deterministic (total order incl. id tie-break)
    Ok(ColdScan {
        ids: hits.into_iter().map(|h| h.event_id).collect(),
        stats,
    })
}

/// Hybrid result plus the Cold-scan measurement (sub-SLO accounting per design §2.1: the deep
/// path is measured separately from the Warm 500ms budget, never silently folded into it).
#[derive(Debug, Clone)]
pub struct DeepSearchResult {
    pub hits: Vec<SearchHit>,
    /// All-zero when the Cold archive was not opened.
    pub cold: ColdScanStats,
}

/// [`search_hybrid_since`] with explicit reach (design §2.1, E-09). The Cold int8 archive is
/// scanned and fused as a **third RRF source** when either
///
/// (a) `opts.depth == SearchDepth::All`, or
/// (b) the caller's explicit time range reaches past the Cold cutoff
///     (`opts.since_ts` set and older than `now_ms - COLD_CUTOFF_MS`) — asking for a window that
///     old *is* asking for Cold, and answering from Warm alone would be silently wrong.
///
/// Default options ([`SearchOptions::default`]) reproduce [`search_hybrid`] exactly: Warm-only,
/// archive untouched (`result.cold == ColdScanStats::default()`). FTS is never duplicated — the
/// lexical half already covers Cold text (demotion removes only the Warm vector), so the Cold
/// scan contributes semantics only. `now_ms` is passed in, not read, for deterministic tests.
pub fn search_hybrid_with_options(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&[f32]>,
    now_ms: i64,
    opts: &SearchOptions,
    limit: usize,
) -> Result<DeepSearchResult, rusqlite::Error> {
    let fts: Vec<i64> =
        fts_search_window_scored(conn, query, opts.since_ts, opts.until_ts, limit)?
            .into_iter()
            .map(|hit| hit.id)
            .collect();
    let vec = match query_embedding {
        Some(emb) => crate::vector::knn(conn, emb, limit)?,
        None => Vec::new(),
    };
    let reach_cold = opts.depth == SearchDepth::All
        || opts.since_ts.is_some_and(|s| s < now_ms - COLD_CUTOFF_MS);
    let cold = match (reach_cold, query_embedding) {
        (true, Some(emb)) => search_cold_partitions(
            conn,
            emb,
            // The Cold window closes at `until_ts` when one is set, so an as-of query does not
            // reach past its own bound through the semantic half. Without this the two halves
            // would disagree about which window was being searched.
            (
                opts.since_ts.unwrap_or(i64::MIN),
                opts.until_ts.unwrap_or(now_ms),
            ),
            opts.max_cold_partitions,
            limit,
        )?,
        _ => ColdScan::default(),
    };
    let ranked = reciprocal_rank_fusion(&[&fts, &vec, &cold.ids], 60.0);
    let capped: Vec<(i64, f64)> = ranked.into_iter().take(limit).collect();
    let mut hits = hydrate(conn, &capped)?;
    // Recency is blended after hydration because that is where `ts` is already available — no
    // extra query — and because reordering a window that has already been chosen cannot change
    // which rows are in it. Default weight 0.0 leaves this a no-op.
    reorder_by_recency_with(conn, &mut hits, opts.recency_epsilon)?;
    Ok(DeepSearchResult {
        hits,
        cold: cold.stats,
    })
}

/// Break near-ties in an already-hydrated window by recency, in place.
///
/// Reads **`last_seen_at`**, not the `ts` already on `SearchHit`. `insert_or_touch` collapses a
/// byte-identical restatement onto the original row and advances `last_seen_at` while leaving `ts`
/// at the first sighting — so a value re-asserted most recently still carries the *oldest* `ts`.
/// Ranking on `ts` was measured to promote the stale value harder, which is why this pays for one
/// extra query; it only runs when the tie-break is enabled.
fn reorder_by_recency_with(
    conn: &Connection,
    hits: &mut [SearchHit],
    epsilon: f64,
) -> Result<(), rusqlite::Error> {
    if epsilon <= 0.0 || hits.len() < 2 {
        return Ok(());
    }
    let mut stmt = conn.prepare("SELECT last_seen_at FROM event_log WHERE id = ?1")?;
    let mut scored: Vec<(i64, f64, i64)> = Vec::with_capacity(hits.len());
    for h in hits.iter() {
        // Fall back to `ts` if the row vanished between hydrate and here: a missing recency signal
        // must not drop the hit, only leave it where the lexical scorer put it.
        let seen: i64 = stmt.query_row(params![h.event_id], |r| r.get(0)).unwrap_or(h.ts);
        scored.push((h.event_id, h.score, seen));
    }
    crate::search::apply_recency(&mut scored, epsilon);
    let order: std::collections::HashMap<i64, usize> =
        scored.iter().enumerate().map(|(i, (id, _, _))| (*id, i)).collect();
    hits.sort_by_key(|h| order.get(&h.event_id).copied().unwrap_or(usize::MAX));
    Ok(())
}
