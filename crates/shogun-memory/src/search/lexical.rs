//! Bounded lexical retrieval over the trigram (`event_fts`) and gram (`event_grams_fts`) indexes,
//! scored in Rust on one BM25 scale (docs/search-eval.md).
//!
//! **Why not SQLite's `rank`.** FTS5 scores every match before `LIMIT` applies, so a query whose
//! terms match thousands of rows cannot be ranked within the search budget. The previous path
//! gave up above 512 matches and returned the newest matches unscored — a common word anywhere in
//! the question (予算の…) then buried the one row that matched all of it. And the gram index has
//! its own `bm25` scale, so the two indexes could not be ranked together anyway.
//!
//! **What this does instead.** Each concept's matching rowids are read newest-first, without
//! scoring, up to a cap ([`CONCEPT_POSTINGS_CAP`]). Rows matching
//! every concept (AND) are taken first; rows matching the most concepts (relaxed OR) fill the
//! rest. Only that bounded candidate set is read and scored, by [`Scorer`] — BM25 over concepts
//! with idf from the postings counts — so the cost is bounded by the caps, not by the archive.

use std::collections::{HashMap, HashSet};
use rusqlite::{params_from_iter, Connection};

use crate::search_documents::{DocumentCandidate, SearchDocumentError, SearchDocumentKind};

use super::query::MemoryQueryFocus;
use super::retrieval::{FtsSearchHit, SearchRange};
use super::terms::{query_concepts, Concept, MatchText, Scorer};

/// Newest matching rows read per concept and index. A concept with more matches is "common": its
/// idf is low and its postings are truncated to the most recent ones.
const CONCEPT_POSTINGS_CAP: usize = 50_000;
/// Rows matching every concept that are scored (newest first beyond this).
const AND_CANDIDATES_CAP: usize = 1_500;
/// Rows matching some concepts that are scored in addition, best concept coverage first.
const OR_CANDIDATES_CAP: usize = 300;
/// Derived documents read per lane before scoring.
const DOCUMENT_CANDIDATES_CAP: usize = 120;
const HYDRATE_CHUNK: usize = 100;
/// Event text read per query for scoring. Candidates are read in priority order (AND newest
/// first, then relaxed by idf) and reading stops once this much text is in hand. Without it a
/// common word read up to 1,800 full rows (~7 MB of random pages on a real 1 GB encrypted store,
/// 0.5–8 s under memory pressure) — the bulk of `memory.search` latency after the AND-first
/// rework. ponytail: fixed byte budget; tune it, or rank by index-only BM25 before reading rows,
/// if recall on very common words needs more.
const HYDRATE_BYTES_BUDGET: usize = 2 * 1024 * 1024;

/// Ranked raw-event lexical hits. `complete` is false when a concept's postings were truncated,
/// i.e. older matches of a common word may exist beyond what was scored.
pub(crate) struct EventLexical {
    pub hits: Vec<FtsSearchHit>,
    pub complete: bool,
    /// One entry per query concept, in `query_concepts` order: its idf over the archive and
    /// whether any row in scope contains it at all. The relevance floor in `federated.rs` weighs
    /// the question's words with these; empty when the lane did not run.
    pub concept_weights: Vec<ConceptWeight>,
}

/// How much one query concept weighs when judging whether a hit covers the question.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ConceptWeight {
    /// BM25 idf from the postings counts (the same number the scorer uses).
    pub idf: f64,
    /// False when no row in the searched scope contains the concept. Such a word tells nothing
    /// about the archive, so the floor weighs it like an average present word rather than by
    /// the extreme idf an absent word would get.
    pub present: bool,
}

struct Postings {
    ids: Vec<i64>,
    complete: bool,
}

fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

fn gram_table_present(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'event_grams_fts'",
        [],
        |row| row.get(0),
    )?;
    Ok(present > 0)
}

/// One rowid-only postings read, newest first, stopped by `LIMIT`.
///
/// No `bm25`, no `rank`: FTS5 walks the doclist in rowid order and stops at the cap, so the cost
/// is bounded by [`CONCEPT_POSTINGS_CAP`] rows per concept and index, not by how often the word
/// occurs. (No SQLite progress handler either: a connection has only one, and replacing it here
/// would silently disable a caller's own — the evidence-bounds test counts VM steps with one.)
fn read_postings(
    conn: &Connection,
    sql: &str,
    expression: &str,
    bounds: (i64, i64),
) -> Result<Postings, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    let ids = stmt
        .query_map(
            rusqlite::params![expression, bounds.0, bounds.1, CONCEPT_POSTINGS_CAP as i64],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let complete = ids.len() < CONCEPT_POSTINGS_CAP;
    Ok(Postings { ids, complete })
}

const TRIGRAM_POSTINGS: &str = "SELECT rowid FROM event_fts WHERE event_fts MATCH ?1
    AND rowid >= ?2 AND rowid <= ?3 ORDER BY rowid DESC LIMIT ?4";
const GRAM_POSTINGS: &str = "SELECT rowid FROM event_grams_fts WHERE event_grams_fts MATCH ?1
    AND rowid >= ?2 AND rowid <= ?3 ORDER BY rowid DESC LIMIT ?4";

fn concept_postings(
    conn: &Connection,
    concept: &Concept,
    bounds: (i64, i64),
    grams_available: bool,
) -> Result<Postings, rusqlite::Error> {
    let mut ids = Vec::new();
    let mut complete = true;
    if let Some(surface) = &concept.surface {
        let postings = read_postings(conn, TRIGRAM_POSTINGS, &quote(surface), bounds)?;
        complete &= postings.complete;
        ids.extend(postings.ids);
    }
    if grams_available && !concept.grams.is_empty() {
        let expression = concept
            .grams
            .iter()
            .map(|gram| quote(gram))
            .collect::<Vec<_>>()
            .join(" AND ");
        let postings = read_postings(conn, GRAM_POSTINGS, &expression, bounds)?;
        complete &= postings.complete;
        ids.extend(postings.ids);
    }
    ids.sort_unstable_by(|left, right| right.cmp(left));
    ids.dedup();
    Ok(Postings { ids, complete })
}

/// Exact docid bounds of a timestamp window (covering index on `ts` includes the rowid).
fn id_bounds(conn: &Connection, range: SearchRange) -> Result<Option<(i64, i64)>, rusqlite::Error> {
    conn.query_row(
        "SELECT min(id), max(id) FROM event_log WHERE ts >= ?1 AND ts < ?2",
        rusqlite::params![range.from_ms, range.to_ms],
        |row| {
            Ok(match (row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?) {
                (Some(low), Some(high)) => Some((low, high)),
                _ => None,
            })
        },
    )
}

/// Raw-event lexical search. `range = None` searches all history (bounded by the caps).
pub(crate) fn search_events(
    conn: &Connection,
    query: &str,
    range: Option<SearchRange>,
    limit: usize,
) -> Result<EventLexical, rusqlite::Error> {
    let concepts = query_concepts(query);
    if concepts.is_empty() || limit == 0 {
        return Ok(EventLexical {
            hits: Vec::new(),
            complete: true,
            concept_weights: Vec::new(),
        });
    }
    let bounds = match range {
        Some(range) => match id_bounds(conn, range)? {
            Some(bounds) => bounds,
            None => {
                return Ok(EventLexical {
                    hits: Vec::new(),
                    complete: true,
                    concept_weights: concepts
                        .iter()
                        .map(|_| ConceptWeight {
                            idf: 1.0,
                            present: false,
                        })
                        .collect(),
                })
            }
        },
        None => (i64::MIN, i64::MAX),
    };
    let grams_available = gram_table_present(conn)?;
    let mut postings = Vec::with_capacity(concepts.len());
    for concept in &concepts {
        postings.push(concept_postings(conn, concept, bounds, grams_available)?);
    }
    let mut complete = postings.iter().all(|postings| postings.complete);
    let sets = postings
        .iter()
        .map(|postings| postings.ids.iter().copied().collect::<HashSet<_>>())
        .collect::<Vec<_>>();

    let total = conn
        .query_row("SELECT max(id) FROM event_log", [], |row| row.get::<_, Option<i64>>(0))?
        .unwrap_or(0)
        .max(1) as f64;
    let document_frequency = postings
        .iter()
        .map(|postings| {
            let found = postings.ids.len() as f64;
            if postings.complete {
                found
            } else {
                // Truncated: a common word. Twice the cap is a floor, which is all idf needs.
                found.max(CONCEPT_POSTINGS_CAP as f64) * 2.0
            }
        })
        .collect::<Vec<_>>();
    let idf = document_frequency
        .iter()
        .map(|df| (1.0 + (total - df + 0.5) / (df + 0.5)).ln().max(0.01))
        .collect::<Vec<_>>();
    let concept_weights = postings
        .iter()
        .zip(&idf)
        .map(|(postings, idf)| ConceptWeight {
            idf: *idf,
            present: !postings.ids.is_empty(),
        })
        .collect::<Vec<_>>();
    let candidates = candidate_ids(&postings, &sets, &idf);

    let (rows, read_all) = hydrate(conn, &candidates)?;
    complete &= read_all;
    let mut texts = Vec::with_capacity(rows.len());
    for (id, ts, title, content) in rows {
        if range.is_some_and(|range| !(range.from_ms..range.to_ms).contains(&ts)) {
            continue;
        }
        texts.push((id, MatchText::new(title.as_deref().unwrap_or(""), &content)));
    }
    let avg_len = if texts.is_empty() {
        1.0
    } else {
        texts.iter().map(|(_, text)| text.len_chars() as f64).sum::<f64>() / texts.len() as f64
    };
    let boosts = entity_boosts(conn, &concepts)?;
    let scorer =
        Scorer::new(query, concepts, &document_frequency, total, avg_len).with_boosts(&boosts);
    let mut scored = texts
        .into_iter()
        .filter_map(|(id, mut text)| {
            let score = scorer.score(&mut text);
            (score.score > 0.0).then_some((id, score.score))
        })
        .collect::<Vec<_>>();
    // Equal relevance goes to the newer row: a restated value or a repeated window is most useful
    // in its latest form.
    scored.sort_by(|left, right| right.1.total_cmp(&left.1).then_with(|| right.0.cmp(&left.0)));
    scored.truncate(limit);
    Ok(EventLexical {
        hits: scored
            .into_iter()
            .map(|(id, score)| FtsSearchHit { id, rank: -score })
            .collect(),
        complete,
        concept_weights,
    })
}

/// Known people and projects read for the entity boost (newest first).
const ENTITY_NAMES_CAP: usize = 2_000;

/// idf multiplier per concept: a concept naming a known person or project (田中, Orchid) is what
/// the question is about, so a row containing it outranks one that only shares generic words.
fn entity_boosts(conn: &Connection, concepts: &[Concept]) -> Result<Vec<f64>, rusqlite::Error> {
    let boost = ENTITY_IDF_BOOST;
    let mut boosts = vec![1.0; concepts.len()];
    let mut stmt = conn.prepare_cached(
        "SELECT title FROM search_documents
          WHERE is_dirty = 0 AND document_kind IN ('person', 'project')
          ORDER BY id DESC LIMIT ?1",
    )?;
    let names = stmt.query_map([ENTITY_NAMES_CAP as i64], |row| row.get::<_, String>(0))?;
    for name in names {
        let mut text = MatchText::new(&name?, "");
        for (index, concept) in concepts.iter().enumerate() {
            if boosts[index] == 1.0 && text.matches(concept) {
                boosts[index] = boost;
            }
        }
        if boosts.iter().all(|value| *value != 1.0) {
            break;
        }
    }
    Ok(boosts)
}

/// Measured on the golden eval: 1.5, 2 and 3 all lift MRR by the same small step over 1.0.
const ENTITY_IDF_BOOST: f64 = 1.5;

/// AND rows first (newest first, capped), then the rows whose matched concepts are rarest.
///
/// A concept that matches nothing at all (a word the archive never contains, like "decide" in
/// "what did we decide about …") does not veto the AND: the AND runs over the concepts that
/// occur. The relaxed set is ordered by the summed idf of the concepts a row contains, so a row
/// holding the one rare word outranks rows holding only a common one.
fn candidate_ids(postings: &[Postings], sets: &[HashSet<i64>], idf: &[f64]) -> Vec<i64> {
    let present = (0..postings.len())
        .filter(|&index| !postings[index].ids.is_empty())
        .collect::<Vec<_>>();
    let mut and_ids = Vec::new();
    // Intersect from the smallest complete concept: its postings are exact, and a row is kept
    // when every other present concept contains it — or might, when that concept was truncated.
    let base = present
        .iter()
        .copied()
        .filter(|&index| postings[index].complete)
        .min_by_key(|&index| postings[index].ids.len())
        .or_else(|| present.iter().copied().min_by_key(|&index| postings[index].ids.len()));
    if let Some(base) = base {
        let base_complete = postings[base].complete;
        for id in &postings[base].ids {
            let in_all = present.iter().all(|&index| {
                index == base
                    || sets[index].contains(id)
                    || (base_complete && !postings[index].complete)
            });
            if in_all {
                and_ids.push(*id);
                if and_ids.len() >= AND_CANDIDATES_CAP {
                    break;
                }
            }
        }
    }
    if present.len() <= 1 {
        return and_ids;
    }
    let chosen = and_ids.iter().copied().collect::<HashSet<_>>();
    let mut weight: HashMap<i64, f64> = HashMap::new();
    for &index in &present {
        let concept_idf = idf.get(index).copied().unwrap_or(1.0);
        for id in &sets[index] {
            if !chosen.contains(id) {
                *weight.entry(*id).or_default() += concept_idf;
            }
        }
    }
    let mut relaxed = weight.into_iter().collect::<Vec<_>>();
    relaxed.sort_by(|left, right| right.1.total_cmp(&left.1).then_with(|| right.0.cmp(&left.0)));
    and_ids.extend(relaxed.into_iter().take(OR_CANDIDATES_CAP).map(|(id, _)| id));
    and_ids
}

type EventRow = (i64, i64, Option<String>, String);

/// Rows for `ids`, read chunk by chunk in `ids` order until [`HYDRATE_BYTES_BUDGET`] is spent.
/// The flag is false when candidates were left unread.
fn hydrate(conn: &Connection, ids: &[i64]) -> Result<(Vec<EventRow>, bool), rusqlite::Error> {
    let mut out = Vec::with_capacity(ids.len());
    let mut bytes = 0;
    for chunk in ids.chunks(HYDRATE_CHUNK) {
        if bytes >= HYDRATE_BYTES_BUDGET {
            return Ok((out, false));
        }
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, ts, window_title, content FROM event_log WHERE id IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        for row in rows {
            let row: EventRow = row?;
            bytes += row.3.len() + row.2.as_ref().map_or(0, String::len);
            out.push(row);
        }
    }
    Ok((out, true))
}

fn document_kind(kind: &str) -> Option<SearchDocumentKind> {
    Some(match kind {
        "person" => SearchDocumentKind::Person,
        "project" => SearchDocumentKind::Project,
        "commitment" => SearchDocumentKind::Commitment,
        "open_loop" => SearchDocumentKind::OpenLoop,
        "thread_summary" => SearchDocumentKind::ThreadSummary,
        "session_summary" => SearchDocumentKind::SessionSummary,
        "meeting_recap" => SearchDocumentKind::MeetingRecap,
        "meeting_note" => SearchDocumentKind::MeetingNote,
        "brief" => SearchDocumentKind::Brief,
        "lesson" => SearchDocumentKind::Lesson,
        "period_summary" => SearchDocumentKind::PeriodSummary,
        _ => return None,
    })
}

fn focus_allows(focus: MemoryQueryFocus, kind: SearchDocumentKind) -> bool {
    use SearchDocumentKind as K;
    match focus {
        MemoryQueryFocus::Any => true,
        MemoryQueryFocus::Meeting => {
            matches!(kind, K::SessionSummary | K::MeetingRecap | K::MeetingNote)
        }
        MemoryQueryFocus::State => matches!(kind, K::Commitment | K::OpenLoop),
        MemoryQueryFocus::Brief => kind == K::Brief,
        MemoryQueryFocus::Period => kind == K::PeriodSummary,
        MemoryQueryFocus::Person => kind == K::Person,
        MemoryQueryFocus::Project => kind == K::Project,
        MemoryQueryFocus::Thread => kind == K::ThreadSummary,
    }
}

/// Derived-document lexical search: trigram FTS for long terms plus a bounded substring scan for
/// two-character terms (the projection is small, and has no gram index), scored like events.
pub(crate) fn search_documents(
    conn: &Connection,
    query: &str,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    focus: MemoryQueryFocus,
    limit: usize,
) -> Result<Vec<DocumentCandidate>, SearchDocumentError> {
    let concepts = query_concepts(query);
    if concepts.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut ids = crate::search_documents::search_documents_fts_filtered(
        conn,
        query,
        from_ms,
        to_ms,
        focus,
        DOCUMENT_CANDIDATES_CAP,
    )?
    .into_iter()
    .map(|candidate| candidate.id)
    .collect::<Vec<_>>();
    let short = concepts
        .iter()
        .filter(|concept| concept.surface.is_none())
        .filter_map(|concept| concept.grams.first().cloned())
        .collect::<Vec<_>>();
    if !short.is_empty() {
        let predicates = (1..=short.len())
            .map(|index| format!("instr(lower(d.title || ' ' || d.body), ?{index}) > 0"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT d.id FROM search_documents d
              WHERE d.is_dirty = 0 AND ({predicates})
                AND NOT EXISTS (
                    SELECT 1 FROM search_document_invalidations i
                     WHERE i.source_table = d.source_table AND i.source_id = d.source_id)
              ORDER BY d.id DESC LIMIT {DOCUMENT_CANDIDATES_CAP}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(short.iter()), |row| row.get::<_, i64>(0))?;
        for row in rows {
            ids.push(row?);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut documents = Vec::new();
    for chunk in ids.chunks(HYDRATE_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, document_kind, title, body, period_start_ms, period_end_ms
               FROM search_documents WHERE id IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;
        for row in rows {
            let (id, kind, title, body, start, end) = row?;
            let Some(kind) = document_kind(&kind) else {
                continue;
            };
            let in_range = match (start, end) {
                (Some(start), Some(end)) => {
                    to_ms.is_none_or(|to| start < to) && from_ms.is_none_or(|from| end > from)
                }
                _ => true,
            };
            if in_range && focus_allows(focus, kind) {
                documents.push((id, kind, MatchText::new(&title, &body)));
            }
        }
    }

    let mut frequency: HashMap<usize, f64> = HashMap::new();
    for (_, _, text) in &mut documents {
        for (index, concept) in concepts.iter().enumerate() {
            if text.matches(concept) {
                *frequency.entry(index).or_default() += 1.0;
            }
        }
    }
    let document_frequency = (0..concepts.len())
        .map(|index| frequency.get(&index).copied().unwrap_or(0.0))
        .collect::<Vec<_>>();
    let avg_len = if documents.is_empty() {
        1.0
    } else {
        documents.iter().map(|(_, _, text)| text.len_chars() as f64).sum::<f64>()
            / documents.len() as f64
    };
    let total = documents.len() as f64 + 1.0;
    let boosts = entity_boosts(conn, &concepts)?;
    let scorer =
        Scorer::new(query, concepts, &document_frequency, total, avg_len).with_boosts(&boosts);
    let mut scored = documents
        .into_iter()
        .filter_map(|(id, kind, mut text)| {
            let score = scorer.score(&mut text);
            (score.score > 0.0).then_some(DocumentCandidate {
                id,
                kind,
                rank: -score.score,
            })
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| left.rank.total_cmp(&right.rank).then_with(|| left.id.cmp(&right.id)));
    scored.truncate(limit);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert, NewEvent};

    fn add(conn: &Connection, ts: i64, content: &str, hash: &str) -> i64 {
        insert(
            conn,
            &NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: Some("notes"),
                content,
                content_hash: hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn ids(conn: &Connection, query: &str) -> Vec<i64> {
        search_events(conn, query, None, 10)
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.id)
            .collect()
    }

    #[test]
    fn two_character_japanese_words_are_found() {
        let conn = crate::open_in_memory().unwrap();
        let meeting = add(&conn, 1, "明日の会議は10時から", "lex-1");
        add(&conn, 2, "ランチの店を探す", "lex-2");
        assert_eq!(ids(&conn, "会議"), vec![meeting]);
    }

    #[test]
    fn a_row_matching_every_concept_survives_a_flood_of_newer_partial_matches() {
        let conn = crate::open_in_memory().unwrap();
        let target = add(&conn, 1, "予算の承認: 下期のマーケティング予算が承認されました", "lex-target");
        for i in 0..700 {
            add(&conn, 10 + i, &format!("予算の確認 {i}"), &format!("lex-flood-{i}"));
        }
        assert_eq!(ids(&conn, "予算の承認").first(), Some(&target));
    }

    #[test]
    fn okurigana_variants_find_each_other() {
        let conn = crate::open_in_memory().unwrap();
        let written = add(&conn, 1, "明日の打ち合わせは本社で", "lex-okuri");
        assert_eq!(ids(&conn, "打合せ"), vec![written]);
    }

    #[test]
    fn short_ascii_tokens_match_whole_tokens_only() {
        let conn = crate::open_in_memory().unwrap();
        let pr = add(&conn, 1, "PR #1187 merged after review", "lex-pr");
        add(&conn, 2, "price list updated", "lex-price");
        assert_eq!(ids(&conn, "PR"), vec![pr]);
    }

    #[test]
    fn full_matches_outrank_partial_ones() {
        let conn = crate::open_in_memory().unwrap();
        let partial = add(&conn, 2, "q3 planning offsite", "lex-partial");
        let full = add(&conn, 1, "Q3 revenue review: churn 2.1%", "lex-full");
        let found = ids(&conn, "q3 revenue churn");
        assert_eq!(found.first(), Some(&full));
        assert!(found.contains(&partial));
    }

    #[test]
    fn a_range_is_exact() {
        let conn = crate::open_in_memory().unwrap();
        add(&conn, 100, "会議メモ 古い", "lex-old");
        let recent = add(&conn, 1_000, "会議メモ 新しい", "lex-new");
        let hits = search_events(
            &conn,
            "会議",
            Some(SearchRange {
                from_ms: 500,
                to_ms: 2_000,
            }),
            10,
        )
        .unwrap();
        assert_eq!(hits.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![recent]);
    }

    #[test]
    fn hydrate_stops_at_the_byte_budget_in_candidate_order() {
        let conn = crate::open_in_memory().unwrap();
        let big = "plain words ".repeat(HYDRATE_BYTES_BUDGET / HYDRATE_CHUNK / 12 + 1);
        let mut ids = (0..HYDRATE_CHUNK * 2)
            .map(|n| add(&conn, n as i64, &format!("{big} {n}"), &format!("lex-big-{n}")))
            .collect::<Vec<_>>();
        ids.reverse(); // candidate order: newest first
        let (rows, read_all) = hydrate(&conn, &ids).unwrap();
        assert!(!read_all);
        let mut read = rows.iter().map(|row| row.0).collect::<Vec<_>>();
        read.sort_unstable_by(|left, right| right.cmp(left));
        assert_eq!(read, ids[..HYDRATE_CHUNK]);
        let (rows, read_all) = hydrate(&conn, &ids[..10]).unwrap();
        assert!(read_all && rows.len() == 10);
    }
}
