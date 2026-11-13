//! Commitments and open loops, Goldfish style (owner decision 2026-10-14).
//!
//! 1. The hourly review reports typed **signals** ([`record_signals`]): append-only, one row per
//!    (window, kind, normalised text), never shown and never closed.
//! 2. The **consolidator** — one Batch call after a review that produced signals, at most
//!    [`RUNS_PER_DAY`] a day ([`consolidation_due`]) — is shown the open list, the signals it has
//!    not read yet (newest first: this hour's always go in, an older backlog fills the rest) and
//!    their source excerpts ([`consolidation_input`]) and answers with at most [`MAX_OPEN`] items,
//!    resolutions and duplicates. [`apply`] is the only writer of new commitments / open loops:
//!    every item needs an exact quote of an event it was shown and a confidence of at least
//!    [`MIN_CONFIDENCE`], and that confidence is what is stored and shown.
//! 3. Rows retire on a **timer**, not a model ([`age_by_horizon`]): 3 / 14 / 45 days since last seen
//!    for same_day / short / project, unless due later or the user pinned or edited the row. No
//!    source is exempt. Aged and merged rows are kept: status cancelled / closed plus `aged_at` /
//!    `merged_into` (V83; the V1 CHECK has no 'aged').
//! 4. Review items earlier builds wrote straight into the tables are retired below the dashboard
//!    once, and the recent ones are handed to the consolidator as signals ([`retire_unconfirmed`]).

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::state::StateTable;
use crate::state_collapse::fold;
use crate::MemoryError;

/// The relay purpose of a consolidator call, also its traceability purpose.
pub const PURPOSE: &str = "loops_consolidate";
/// Consolidator runs per local day.
pub const RUNS_PER_DAY: i64 = 8;
/// Shortest gap between two runs.
pub const MIN_GAP_MS: i64 = 3_600_000;
/// Items below this are not stored; items at or above it are stored at their own confidence.
pub const MIN_CONFIDENCE: f64 = 0.84;
/// Items one answer may open or re-list.
pub const MAX_OPEN: usize = 6;
/// Source events one call carries.
pub const MAX_SOURCES: usize = 24;
const MAX_SIGNALS: usize = 80;
const MAX_OPEN_LIST: usize = 40;
const EXCERPT_CHARS: usize = 600;
const MAX_TEXT_CHARS: usize = 200;
const DAY_MS: i64 = 86_400_000;
/// The base confidence earlier builds gave a review item written straight into the state tables.
pub const REVIEWED_BASE: f64 = 0.85;
/// Old review rows seen this recently are handed to the consolidator as signals.
pub const RESEED_WINDOW_MS: i64 = 2 * DAY_MS;

/// One typed observation from the hourly review: `kind` is one of commitment, blocker, question,
/// decision, followup; `quote` is the resolved span of `event_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSignal {
    pub kind: &'static str,
    pub text: String,
    pub event_id: i64,
    pub quote: String,
}

/// The signal kind of a commitment direction or an open-loop kind.
pub fn signal_kind(direction_or_kind: &str) -> &'static str {
    match direction_or_kind {
        "mine" => "commitment",
        "theirs" | "waiting_on_them" => "blocker",
        "reply_needed" => "question",
        "decision_pending" => "decision",
        _ => "followup",
    }
}

/// Append signals from one review window. A repeat of (window, kind, normalised text) is ignored.
/// Returns the rows added.
pub fn record_signals(
    conn: &Connection,
    window_id: &str,
    signals: &[NewSignal],
    now: i64,
) -> Result<usize, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO loop_signals (window_id, kind, text, norm, event_id, quote, created_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7 WHERE EXISTS (SELECT 1 FROM event_log WHERE id = ?5)",
    )?;
    let mut added = 0;
    for s in signals {
        let clean = |raw: &str| crate::sanitize::persist_generated(raw.trim()).map(|b| b.text.into_owned());
        let (Some(text), Some(quote)) = (clean(&s.text), clean(&s.quote)) else { continue };
        let norm = fold(&text);
        if norm.is_empty() || quote.is_empty() {
            continue;
        }
        added += stmt.execute(params![window_id, s.kind, text, norm, s.event_id, quote, now])?;
    }
    Ok(added)
}

/// Signals the Batch lane may see, joined to their event (`s` is the signal).
fn eligible_signals() -> String {
    let excluded = crate::event_log::BATCH_EXCLUDED_SOURCES
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!("loop_signals s JOIN event_log e ON e.id = s.event_id AND e.source NOT IN ({excluded})")
}

/// The signals no consolidator call has read yet.
const UNREAD: &str = "NOT EXISTS (SELECT 1 FROM loop_signal_reads r WHERE r.signal_id = s.id)";

/// Mark signals read. `ids` are what [`ConsolidationInput::read`] carries: signal ids, and the
/// outcomes the call was shown (`loop_updates`, V104) as their negated ids.
fn mark_read(conn: &Connection, ids: &[i64], now: i64) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare_cached("INSERT OR IGNORE INTO loop_signal_reads (signal_id, read_at) VALUES (?1, ?2)")?;
    let mut outcome = conn.prepare_cached("UPDATE loop_updates SET read_at = ?2 WHERE id = ?1 AND read_at IS NULL")?;
    for id in ids {
        if *id < 0 {
            outcome.execute(params![-id, now])?;
        } else {
            stmt.execute(params![id, now])?;
        }
    }
    Ok(())
}

/// Outcomes read off later messages (`loop_updates`, E49) that no consolidator call has read, on
/// open rows the consolidator lists (>= 0.5), from events the Batch lane may see. `u` is the update.
fn unread_outcomes() -> String {
    let excluded = crate::event_log::BATCH_EXCLUDED_SOURCES
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "loop_updates u JOIN event_log e ON e.id = u.event_id AND e.source NOT IN ({excluded})
          WHERE u.read_at IS NULL AND (
            (u.state_table = 'commitments' AND EXISTS (SELECT 1 FROM commitments c WHERE c.id = u.state_id
               AND c.status IN ('open', 'overdue') AND c.aged_at IS NULL AND c.merged_into IS NULL AND c.confidence >= 0.5))
            OR (u.state_table = 'open_loops' AND EXISTS (SELECT 1 FROM open_loops l WHERE l.id = u.state_id
               AND l.status = 'open' AND l.aged_at IS NULL AND l.merged_into IS NULL AND l.confidence >= 0.5)))"
    )
}

/// Whether this event has inputs the consolidator can still consume. Use the same eligibility
/// rules as scheduling so an outcome for an already closed or hidden row cannot remain pending.
pub fn has_unread_for_event(conn: &Connection, event_id: i64) -> Result<bool, rusqlite::Error> {
    conn.query_row(&format!(
        "SELECT EXISTS(SELECT 1 FROM {} WHERE s.event_id = ?1 AND {UNREAD})
         OR EXISTS(SELECT 1 FROM {} AND u.event_id = ?1)",
        eligible_signals(), unread_outcomes(),
    ), [event_id], |r| r.get(0))
}

/// Outcomes one call carries.
const MAX_OUTCOMES: usize = 20;

/// The consolidator's last send: the traceability row written before every call, so a failed call
/// counts too. Looks back two days.
fn last_run(conn: &Connection, now: i64) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT MAX(ts) FROM traceability_log WHERE ts >= ?1 AND purpose = ?2",
        params![now - 2 * DAY_MS, PURPOSE],
        |r| r.get(0),
    )
}

/// Run now? Only when there are signals (or outcomes of listed items, E49) it has not read, the
/// last call is an hour old, and fewer than [`RUNS_PER_DAY`] calls went out since `day_start`
/// (local midnight).
pub fn consolidation_due(conn: &Connection, now: i64, day_start: i64) -> Result<bool, rusqlite::Error> {
    let last = last_run(conn, now)?;
    if last.is_some_and(|t| now - t < MIN_GAP_MS) {
        return Ok(false);
    }
    let runs: i64 = conn.query_row(
        "SELECT count(*) FROM traceability_log WHERE ts >= ?1 AND purpose = ?2",
        params![day_start, PURPOSE],
        |r| r.get(0),
    )?;
    if runs >= RUNS_PER_DAY {
        return Ok(false);
    }
    let signals: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {} WHERE {UNREAD})", eligible_signals()),
        [],
        |r| r.get(0),
    )?;
    // A later message said a listed item was done, called off or moved (E49): only the
    // consolidator may change a row it owns, so that is news for it too.
    Ok(signals || conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {})", unread_outcomes()), [], |r| r.get(0))?)
}

/// What one consolidator call is shown: `text` (raw — the caller redacts and fences it), the
/// event ids an answer may cite, and the signals it reads (marked by [`apply`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationInput {
    pub text: String,
    pub events: Vec<i64>,
    pub read: Vec<i64>,
}

/// `C12` / `L40`: the table and id an answer names.
fn row_ref(key: &str) -> Option<(StateTable, i64)> {
    let key = key.trim();
    let table = match key.get(..1)? {
        "C" | "c" => StateTable::Commitments,
        "L" | "l" => StateTable::OpenLoops,
        _ => return None,
    };
    Some((table, key.get(1..)?.parse().ok()?))
}

fn open_statuses(table: StateTable) -> &'static str {
    if table == StateTable::Commitments { "('open', 'overdue')" } else { "('open')" }
}

fn closed_status(table: StateTable) -> &'static str {
    if table == StateTable::Commitments { "cancelled" } else { "closed" }
}

/// `YYYY-MM-DD HH:MM`, local.
fn stamp(ms: i64) -> String {
    let d = crate::due::local_date(ms);
    let mins = (ms + i64::from(crate::due::local_offset_secs(ms)) * 1000).rem_euclid(DAY_MS) / 60_000;
    format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year, d.month, d.day, mins / 60, mins % 60)
}

/// The window of a source event around `quote`, behind a header line. `None` when the event is
/// gone or not allowed on the Batch lane.
fn source_excerpt(conn: &Connection, event_id: i64, quote: &str) -> Result<Option<String>, rusqlite::Error> {
    let Some(ev) = crate::event_log::batch_event_text(conn, event_id, 200_000)? else { return Ok(None) };
    let (ts, app, title): (i64, Option<String>, Option<String>) = conn.query_row(
        "SELECT ts, app_bundle_id, window_title FROM event_log WHERE id = ?1",
        [event_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let at = ev.content.find(quote).map_or(0, |b| ev.content[..b].chars().count());
    let excerpt: String = ev
        .content
        .chars()
        .skip(at.saturating_sub(EXCERPT_CHARS))
        .take(2 * EXCERPT_CHARS + quote.chars().count())
        .collect();
    Ok(Some(format!(
        "[E{event_id}] {} · {} · {}\n{}",
        app.unwrap_or_default(),
        title.unwrap_or_default(),
        stamp(ts),
        excerpt.trim()
    )))
}

/// The sections of a consolidator input, in the order [`consolidation_input`] writes them:
/// `(name, columns)`. The instruction the model reads (`shogun_core::dreamcycle::loops`) names
/// every one of them with its columns, and a test there holds the two together, so a section added
/// here cannot go undescribed.
pub const SECTIONS: [(&str, &str); 6] = [
    ("Now", ""),
    ("Open items", "id | kind | horizon | due | text | evidence"),
    ("Recently closed", "id | status | text"),
    ("New signals", "kind | event | text | quote"),
    ("Outcomes noticed in later messages", "id | outcome | event | quote | new due"),
    ("Sources", ""),
];

/// `Name (columns):` / `Name:`, the header line of section `i` of [`SECTIONS`].
pub fn section_header(i: usize) -> String {
    match SECTIONS.get(i) {
        Some((name, "")) => format!("{name}:"),
        Some((name, columns)) => format!("{name} ({columns}):"),
        None => String::new(),
    }
}

/// id, kind, horizon, due, text, last seen.
type OpenRow = (i64, String, Option<String>, Option<i64>, String, i64);

/// The consolidator's input at `now`: the unread signals, newest first — this hour's always go in,
/// an older backlog fills what is left of [`MAX_SOURCES`] events and 80 signals and waits for the
/// next call. `None` when nothing is unread.
pub fn consolidation_input(conn: &Connection, now: i64) -> Result<Option<ConsolidationInput>, rusqlite::Error> {
    let signals: Vec<(i64, String, String, i64, String)> = conn
        .prepare(&format!(
            "SELECT s.id, s.kind, s.text, s.event_id, s.quote FROM {} WHERE {UNREAD} ORDER BY s.id DESC LIMIT ?1",
            eligible_signals()
        ))?
        .query_map([(MAX_SIGNALS * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<Result<_, _>>()?;
    let mut sources: Vec<(i64, String)> = Vec::new();
    let mut signal_lines = Vec::new();
    let mut read = Vec::new();
    for (id, kind, text, event_id, quote) in &signals {
        if signal_lines.len() == MAX_SIGNALS {
            break;
        }
        if !sources.iter().any(|(e, _)| e == event_id) {
            if sources.len() == MAX_SOURCES {
                continue; // a later call; a signal on an event already shown still fits
            }
            match source_excerpt(conn, *event_id, quote)? {
                Some(excerpt) => sources.push((*event_id, excerpt)),
                None => {
                    read.push(*id); // unreadable source: consumed, never blocks the queue
                    continue;
                }
            }
        }
        signal_lines.push(format!("{kind} | E{event_id} | {text} | \"{quote}\""));
        read.push(*id);
    }
    // Outcomes of listed items read off later messages (E49), newest first, with their source.
    #[allow(clippy::type_complexity)]
    let outcomes: Vec<(i64, String, i64, String, i64, String, Option<i64>)> = conn
        .prepare(&format!(
            "SELECT u.id, u.state_table, u.state_id, u.kind, u.event_id, u.quote, u.due_at FROM {}
              ORDER BY u.id DESC LIMIT ?1",
            unread_outcomes()
        ))?
        .query_map([MAX_OUTCOMES as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut outcome_lines = Vec::new();
    for (id, table, state_id, kind, event_id, quote, due) in &outcomes {
        if !sources.iter().any(|(e, _)| e == event_id) {
            if sources.len() == MAX_SOURCES {
                continue; // a later call
            }
            match source_excerpt(conn, *event_id, quote)? {
                Some(excerpt) => sources.push((*event_id, excerpt)),
                None => {
                    read.push(-id);
                    continue;
                }
            }
        }
        let letter = if table == "commitments" { 'C' } else { 'L' };
        let due = due.map_or_else(|| "-".to_string(), |d| format!("due {}", stamp(d)));
        outcome_lines.push(format!("{letter}{state_id} | {kind} | E{event_id} | \"{quote}\" | {due}"));
        read.push(-id);
    }
    if signal_lines.is_empty() && outcome_lines.is_empty() {
        // Every unread signal lost its source: mark them read without a call.
        mark_read(conn, &read, now)?;
        return Ok(None);
    }
    // Shown oldest to newest, as the hours happened.
    signal_lines.reverse();
    outcome_lines.reverse();
    let mut events: Vec<i64> = sources.iter().map(|(id, _)| *id).collect();

    // Open items, same-day first then most recently seen (the display order).
    let mut open: Vec<(bool, i64, String)> = Vec::new();
    for (table, letter, kind) in [(StateTable::Commitments, 'C', "'commitment'"), (StateTable::OpenLoops, 'L', "kind")] {
        let due = if table == StateTable::Commitments { "due_at" } else { "NULL" };
        let rows: Vec<OpenRow> = conn
            .prepare(&format!(
                "SELECT id, {kind}, horizon, {due}, description, last_evidence_at FROM {t}
                  WHERE status IN {open} AND aged_at IS NULL AND merged_into IS NULL AND confidence >= 0.5
                  ORDER BY (horizon IS 'same_day') DESC, last_evidence_at DESC LIMIT ?1",
                t = table.as_str(),
                open = open_statuses(table),
            ))?
            .query_map([MAX_OPEN_LIST as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<_, _>>()?;
        for (id, kind, horizon, due, text, seen) in rows {
            let evidence: Option<(i64, String)> = conn
                .query_row(
                    "SELECT event_id, quote FROM state_provenance WHERE state_table = ?1 AND state_id = ?2
                       AND quote IS NOT NULL ORDER BY batch_reviewed DESC, id DESC LIMIT 1",
                    params![table.as_str(), id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            // Only evidence the Batch lane may see (meeting text never leaves the device).
            let evidence = match evidence {
                Some((event, quote)) if crate::event_log::batch_event_text(conn, event, 1)?.is_some() => {
                    events.push(event);
                    format!("E{event} \"{quote}\"")
                }
                _ => String::new(),
            };
            let due = due.map_or_else(|| "-".to_string(), |d| format!("due {}", stamp(d)));
            let line = format!(
                "{letter}{id} | {kind} | {} | {due} | {text} | {evidence}",
                horizon.as_deref().unwrap_or("-")
            );
            open.push((horizon.as_deref() == Some("same_day"), seen, line));
        }
    }
    open.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    open.truncate(MAX_OPEN_LIST);

    let mut closed = Vec::new();
    for (table, letter, done) in [(StateTable::Commitments, 'C', "('done', 'cancelled')"), (StateTable::OpenLoops, 'L', "('closed')")] {
        let rows: Vec<(i64, String, String)> = conn
            .prepare(&format!(
                "SELECT id, status, description FROM {t} WHERE status IN {done} AND aged_at IS NULL
                   AND merged_into IS NULL AND updated_at >= ?1 ORDER BY updated_at DESC LIMIT 10",
                t = table.as_str()
            ))?
            .query_map([now - 3 * DAY_MS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        closed.extend(rows.into_iter().map(|(id, status, text)| format!("{letter}{id} | {status} | {text}")));
    }

    let none = || vec!["(none)".to_string()];
    let or_none = |v: Vec<String>| if v.is_empty() { none() } else { v };
    let text = [
        format!("{} {}", section_header(0), stamp(now)),
        section_header(1),
        or_none(open.into_iter().map(|(_, _, l)| l).collect()).join("\n"),
        section_header(2),
        or_none(closed).join("\n"),
        section_header(3),
        or_none(signal_lines).join("\n"),
        section_header(4),
        or_none(outcome_lines).join("\n"),
        section_header(5),
        sources.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("\n\n"),
    ]
    .join("\n");
    events.sort_unstable();
    events.dedup();
    Ok(Some(ConsolidationInput { text, events, read }))
}

/// What [`apply`] did, counts only.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub opened: usize,
    pub relisted: usize,
    pub resolved: usize,
    pub merged: usize,
    /// Items, resolutions or duplicates refused by a check.
    pub dropped: usize,
}

/// `(user_pinned, wording_edited)` of a row that is open, not aged and not merged.
fn open_row(conn: &Connection, table: StateTable, id: i64) -> Result<Option<(bool, bool)>, rusqlite::Error> {
    conn.query_row(
        &format!(
            "SELECT user_pinned, wording_edited FROM {} WHERE id = ?1 AND status IN {}
               AND aged_at IS NULL AND merged_into IS NULL",
            table.as_str(),
            open_statuses(table)
        ),
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// The shortest quote that can still identify a statement: 8 characters, 6 in CJK text
/// ("明日送ります" is a whole promise).
pub fn min_quote_chars(quote: &str) -> usize {
    if crate::state_collapse::has_cjk(quote) {
        6
    } else {
        8
    }
}

/// The stored form of `quote` and its event's time, when the quote is an exact span of an event
/// the call was shown (`events`). Short quotes cannot identify a statement and are refused:
/// [`min_quote_chars`].
fn grounded(
    conn: &Connection,
    events: &[i64],
    event_id: Option<i64>,
    quote: Option<&str>,
) -> Result<Option<(String, i64)>, rusqlite::Error> {
    let (Some(event_id), Some(quote)) = (event_id, quote.map(str::trim)) else { return Ok(None) };
    if !events.contains(&event_id) || quote.chars().count() < min_quote_chars(quote) {
        return Ok(None);
    }
    let Some(clean) = crate::sanitize::persist_generated(quote) else { return Ok(None) };
    let row: Option<(String, i64)> = conn
        .query_row("SELECT content, ts FROM event_log WHERE id = ?1", [event_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(row
        .filter(|(content, _)| crate::state::source_contains_quote(content, &clean.text))
        .map(|(_, ts)| (clean.text.into_owned(), ts)))
}

/// Link `event_id` as reviewed evidence of a row, with its quote.
fn link(conn: &Connection, table: StateTable, id: i64, event_id: i64, quote: &str) -> Result<(), rusqlite::Error> {
    let t = table.as_str();
    conn.execute(
        "INSERT INTO state_provenance (state_table, state_id, event_id, weight, quote, batch_reviewed)
         SELECT ?1, ?2, ?3, 1.0, ?4, 1 WHERE NOT EXISTS
           (SELECT 1 FROM state_provenance WHERE state_table = ?1 AND state_id = ?2 AND event_id = ?3)",
        params![t, id, event_id, quote],
    )?;
    conn.execute(
        "UPDATE state_provenance SET quote = ?4, batch_reviewed = 1
          WHERE state_table = ?1 AND state_id = ?2 AND event_id = ?3",
        params![t, id, event_id, quote],
    )?;
    Ok(())
}

fn project(conn: &Connection, table: StateTable, id: i64, now: i64) {
    let _ = crate::search_documents::project_source_after_write(conn, &crate::state_edit::source_ref(table, id), now);
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// An open row of `table` whose text folds to the same key (the backstop when no id came back).
fn same_text(conn: &Connection, table: StateTable, text: &str) -> Result<Option<i64>, rusqlite::Error> {
    let key = fold(text);
    let rows: Vec<(i64, String)> = conn
        .prepare(&format!(
            "SELECT id, description FROM {} WHERE status IN {} AND aged_at IS NULL AND merged_into IS NULL ORDER BY id",
            table.as_str(),
            open_statuses(table)
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows.into_iter().find(|(_, d)| fold(d) == key).map(|(id, _)| id))
}

/// An open row of `table` the consolidator never confirmed that already cites this span of
/// `event_id`: an old review row re-seeded as a signal ([`retire_unconfirmed`]) is re-listed in
/// place. Confirmed rows are matched by id, text or `duplicates` instead, so two items from one
/// message stay two.
fn same_source(conn: &Connection, table: StateTable, event_id: i64, quote: &str) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        &format!(
            "SELECT s.id FROM {t} s JOIN state_provenance p ON p.state_table = ?1 AND p.state_id = s.id
              WHERE p.event_id = ?2 AND p.quote IS NOT NULL AND (instr(p.quote, ?3) > 0 OR instr(?3, p.quote) > 0)
                AND s.horizon IS NULL AND s.status IN {open} AND s.aged_at IS NULL AND s.merged_into IS NULL
              ORDER BY s.id LIMIT 1",
            t = table.as_str(),
            open = open_statuses(table)
        ),
        params![table.as_str(), event_id, quote],
        |r| r.get(0),
    )
    .optional()
}

/// A row of `table` that is closed (not aged, not merged) and already cites this span of
/// `event_id`.
fn closed_source(conn: &Connection, table: StateTable, event_id: i64, quote: &str) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {t} s JOIN state_provenance p ON p.state_table = ?1 AND p.state_id = s.id
              WHERE p.event_id = ?2 AND p.quote IS NOT NULL AND (instr(p.quote, ?3) > 0 OR instr(?3, p.quote) > 0)
                AND s.status NOT IN {open} AND s.aged_at IS NULL AND s.merged_into IS NULL)",
            t = table.as_str(),
            open = open_statuses(table)
        ),
        params![table.as_str(), event_id, quote],
        |r| r.get(0),
    )
}

/// A re-listed commitment whose cited words are newer than anything the row was seen in and name a
/// due date ("Update: the memo moves to Thursday", or the promise said again "by Monday") takes
/// that date (E49). The user's own date stands.
fn redate_on_relist(conn: &Connection, id: i64, quote: &str, said_at: i64, now: i64) -> Result<(), rusqlite::Error> {
    let lower = quote.to_lowercase();
    let Some(due) = crate::loop_updates::redated_due(&lower, said_at).or_else(|| crate::due::due_at(quote, said_at)) else {
        return Ok(());
    };
    conn.execute(
        "UPDATE commitments SET due_at = ?2, updated_at = MAX(updated_at, ?4),
                status = CASE WHEN status = 'overdue' AND ?2 > ?4 THEN 'open' ELSE status END
          WHERE id = ?1 AND last_evidence_at < ?3 AND user_pinned = 0 AND user_edited_at IS NULL
            AND (due_at IS NULL OR due_at != ?2)",
        params![id, due, said_at, now],
    )?;
    Ok(())
}

/// The open-loop kind for a new loop: `followup` says it; otherwise the kind of the signal its
/// quote came from.
fn loop_kind(conn: &Connection, kind: &str, event_id: i64) -> Result<&'static str, rusqlite::Error> {
    if kind == "followup" {
        return Ok("follow_up");
    }
    let signal: Option<String> = conn
        .query_row("SELECT kind FROM loop_signals WHERE event_id = ?1 ORDER BY id DESC LIMIT 1", [event_id], |r| r.get(0))
        .optional()?;
    Ok(match signal.as_deref() {
        Some("blocker") => "waiting_on_them",
        Some("question") => "reply_needed",
        Some("decision") => "decision_pending",
        Some("followup") => "follow_up",
        _ => "other",
    })
}

/// One `open` item: re-list (by id, or by the same text) or store a new row. `Some(true)` =
/// re-listed, `Some(false)` = new, `None` = refused.
fn open_item(conn: &mut rusqlite::Savepoint<'_>, item: &Value, events: &[i64], now: i64, ctx: &crate::actor::WriteContext) -> Result<Option<bool>, rusqlite::Error> {
    let confidence = item.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
    let (Some(kind), Some(horizon), Some(text)) = (str_of(item, "kind"), str_of(item, "horizon"), str_of(item, "text")) else {
        return Ok(None);
    };
    let kind_table = match kind {
        "commitment" => StateTable::Commitments,
        "loop" | "followup" | "project" => StateTable::OpenLoops,
        _ => return Ok(None),
    };
    if confidence < MIN_CONFIDENCE || !matches!(horizon, "same_day" | "short" | "project") {
        return Ok(None);
    }
    let Some(text) = crate::sanitize::persist_generated(text)
        .map(|b| b.text.into_owned())
        .filter(|t| t.chars().count() <= MAX_TEXT_CHARS)
    else {
        return Ok(None);
    };
    let event_id = item.get("source_event_id").and_then(Value::as_i64);
    let Some((quote, said_at)) = grounded(conn, events, event_id, str_of(item, "source_quote"))? else {
        return Ok(None);
    };
    let Some(event_id) = event_id else { return Ok(None) };
    let confidence = confidence.min(1.0);
    // Last seen = when the cited words were captured, so an item rebuilt from old history (a
    // backfilled window) is not shown as current, and a re-list advances only on newer evidence.
    let seen = said_at.min(now);
    let listed = match str_of(item, "id").and_then(row_ref) {
        Some((table, id)) if open_row(conn, table, id)?.is_some() => Some((table, id)),
        _ => None,
    };
    let other = if kind_table == StateTable::Commitments { StateTable::OpenLoops } else { StateTable::Commitments };
    let (table, existing) = match listed {
        Some((table, id)) => (table, Some(id)),
        None => match same_text(conn, kind_table, &text)?.or(same_source(conn, kind_table, event_id, &quote)?) {
            Some(id) => (kind_table, Some(id)),
            None => match same_source(conn, other, event_id, &quote)? {
                Some(id) => (other, Some(id)),
                None => (kind_table, None),
            },
        },
    };
    // The cited words are already the evidence of a row that is closed (done, called off, resolved):
    // re-listing them would reopen it on its old words (E49). New evidence opens it again.
    if existing.is_none() && (closed_source(conn, kind_table, event_id, &quote)? || closed_source(conn, other, event_id, &quote)?) {
        return Ok(None);
    }
    let t = table.as_str();
    let tx = conn.savepoint()?;
    // A hard-deleted or rewritten item has no row for the closed-source check above. Consult
    // its description digest before creating a replacement, using the cited event's real time
    // and the new row's explicit destination (which need not equal the current call scope).
    if existing.is_none() && crate::state_edit::is_suppressed_in_namespace(
        &tx, table, &text, said_at, &ctx.namespace_sql(),
    )? {
        return Ok(None);
    }
    let id = match existing {
        Some(id) => {
            if table == StateTable::Commitments {
                redate_on_relist(&tx, id, &quote, said_at, now)?;
            }
            tx.execute(
                &format!(
                    "UPDATE {t} SET horizon = ?2, last_evidence_at = MAX(last_evidence_at, ?6), updated_at = ?3,
                            confidence = CASE WHEN user_pinned = 1 THEN confidence ELSE ?4 END,
                            base_confidence = CASE WHEN user_pinned = 1 THEN base_confidence ELSE ?4 END,
                            description = CASE WHEN user_pinned = 1 OR wording_edited = 1 THEN description ELSE ?5 END,
                            actor_last = ?7
                      WHERE id = ?1"
                ),
                params![id, horizon, now, confidence, text, seen, ctx.actor_sql()],
            )?;
            id
        }
        None if table == StateTable::Commitments => {
            let due = crate::due::due_at(&quote, said_at).or_else(|| crate::due::due_at(&text, said_at));
            tx.execute(
                "INSERT INTO commitments (direction, description, due_at, status, confidence, base_confidence,
                                          created_at, updated_at, last_evidence_at, horizon, actor_last, namespace)
                 VALUES ('mine', ?1, ?2, 'open', ?3, ?3, ?4, ?4, ?6, ?5, ?7, ?8)",
                params![text, due, confidence, now, horizon, seen, ctx.actor_sql(), ctx.namespace_sql()],
            )?;
            tx.last_insert_rowid()
        }
        None => {
            tx.execute(
                "INSERT INTO open_loops (kind, description, opened_at, staleness_days, status, confidence,
                                         base_confidence, created_at, updated_at, last_evidence_at, horizon,
                                         actor_last, namespace)
                 VALUES (?1, ?2, ?3, 0, 'open', ?4, ?4, ?5, ?5, ?3, ?6, ?7, ?8)",
                params![loop_kind(&tx, kind, event_id)?, text, seen, confidence, now, horizon, ctx.actor_sql(), ctx.namespace_sql()],
            )?;
            tx.last_insert_rowid()
        }
    };
    link(&tx, table, id, event_id, &quote)?;
    tx.commit()?;
    project(conn, table, id, now);
    Ok(Some(existing.is_some()))
}

/// One `resolved` entry: closes an open row the user did not pin, with the quote that shows it.
fn resolve(conn: &mut rusqlite::Savepoint<'_>, r: &Value, events: &[i64], now: i64) -> Result<bool, rusqlite::Error> {
    let Some((table, id)) = str_of(r, "id").and_then(row_ref) else { return Ok(false) };
    if open_row(conn, table, id)?.map_or(true, |(pinned, _)| pinned) {
        return Ok(false);
    }
    let event_id = r.get("event_id").and_then(Value::as_i64);
    let Some((quote, _)) = grounded(conn, events, event_id, str_of(r, "quote"))? else { return Ok(false) };
    let Some(event_id) = event_id else { return Ok(false) };
    let done = if table == StateTable::Commitments { "done" } else { "closed" };
    let tx = conn.savepoint()?;
    tx.execute(&format!("UPDATE {} SET status = ?2, updated_at = ?3 WHERE id = ?1", table.as_str()), params![id, done, now])?;
    link(&tx, table, id, event_id, &quote)?;
    tx.commit()?;
    project(conn, table, id, now);
    Ok(true)
}

/// One `duplicates` entry: folds an open, unpinned row into another open row of the same table.
/// The duplicate is kept (`merged_into`, status cancelled / closed); its evidence is copied to the
/// target so the target holds the union.
fn merge(conn: &mut rusqlite::Savepoint<'_>, d: &Value, now: i64) -> Result<bool, rusqlite::Error> {
    let (Some((table, id)), Some((into_table, into))) =
        (str_of(d, "id").and_then(row_ref), str_of(d, "into_id").and_then(row_ref))
    else {
        return Ok(false);
    };
    if table != into_table || id == into || open_row(conn, table, into)?.is_none() {
        return Ok(false);
    }
    if open_row(conn, table, id)?.map_or(true, |(pinned, _)| pinned) {
        return Ok(false);
    }
    let t = table.as_str();
    let tx = conn.savepoint()?;
    tx.execute(
        "INSERT INTO state_provenance (state_table, state_id, event_id, weight, quote, batch_reviewed)
         SELECT state_table, ?2, event_id, weight, quote, batch_reviewed FROM state_provenance
          WHERE state_table = ?3 AND state_id = ?1
            AND event_id NOT IN (SELECT event_id FROM state_provenance WHERE state_table = ?3 AND state_id = ?2)",
        params![id, into, t],
    )?;
    tx.execute(
        &format!("UPDATE {t} SET merged_into = ?2, status = ?3 WHERE id = ?1"),
        params![id, into, closed_status(table)],
    )?;
    tx.execute(
        &format!(
            "UPDATE {t} SET last_evidence_at = MAX(last_evidence_at, (SELECT last_evidence_at FROM {t} WHERE id = ?1)),
                    updated_at = ?3 WHERE id = ?2"
        ),
        params![id, into, now],
    )?;
    tx.commit()?;
    project(conn, table, id, now);
    project(conn, table, into, now);
    Ok(true)
}

/// Apply one consolidator answer `{"open":[..], "resolved":[..], "duplicates":[..]}` and mark the
/// signals it read (`read`). `events` are the ids the call was shown
/// ([`ConsolidationInput::events`]); anything else is refused. An unusable answer (pass
/// `Value::Null`) still marks them read, so one bad batch cannot stall the queue.
pub fn apply(conn: &mut Connection, answer: &Value, events: &[i64], read: &[i64], now: i64) -> Result<Applied, MemoryError> {
    apply_as(conn, answer, events, read, now, &crate::actor::current_write())
}

/// [`apply`] as `ctx` (E2-T4): every row the consolidator opens or re-lists records `ctx.actor`
/// as its last writer and lands in `ctx.namespace`. The engine passes
/// [`crate::actor::Actor::system`]`("consolidator")`.
pub fn apply_as(conn: &mut Connection, answer: &Value, events: &[i64], read: &[i64], now: i64, ctx: &crate::actor::WriteContext) -> Result<Applied, MemoryError> {
    // State effects and acknowledging their inputs are one commit. A storage failure leaves
    // both untouched so a later processing retry cannot observe a half-applied answer.
    let mut savepoint = conn.savepoint()?;
    let conn = &mut savepoint;
    let list = |key: &str| answer.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Applied::default();
    let open = list("open");
    out.dropped += open.len().saturating_sub(MAX_OPEN);
    for item in open.iter().take(MAX_OPEN) {
        match open_item(conn, item, events, now, ctx)? {
            Some(true) => out.relisted += 1,
            Some(false) => out.opened += 1,
            None => out.dropped += 1,
        }
    }
    for r in &list("resolved") {
        if resolve(conn, r, events, now)? { out.resolved += 1 } else { out.dropped += 1 }
    }
    for d in &list("duplicates") {
        if merge(conn, d, now)? { out.merged += 1 } else { out.dropped += 1 }
    }
    mark_read(conn, read, now)?;
    savepoint.commit()?;
    Ok(out)
}

/// Retire rows not seen within their horizon (3 days same_day, 45 days project, 14 days otherwise —
/// including rows the consolidator never confirmed). Kept: a future due date, a snooze, a row the
/// user pinned or edited. `updated_at` is left alone (ageing is not activity). Idempotent.
pub fn age_by_horizon(conn: &Connection, now: i64) -> Result<usize, rusqlite::Error> {
    let mut aged = 0;
    for table in [StateTable::Commitments, StateTable::OpenLoops] {
        let due = if table == StateTable::Commitments { "AND (due_at IS NULL OR due_at <= ?1)" } else { "" };
        aged += conn.execute(
            &format!(
                "UPDATE {t} SET aged_at = ?1, status = '{closed}'
                  WHERE status IN {open} AND aged_at IS NULL AND merged_into IS NULL
                    AND user_pinned = 0 AND user_edited_at IS NULL
                    AND (deferred_until IS NULL OR deferred_until <= ?1) {due}
                    AND last_evidence_at < ?1 - CASE horizon WHEN 'same_day' THEN 3 * ?2 WHEN 'project' THEN 45 * ?2 ELSE 14 * ?2 END",
                t = table.as_str(),
                closed = closed_status(table),
                open = open_statuses(table),
            ),
            params![now, DAY_MS],
        )?;
    }
    Ok(aged)
}

/// Once, for review items earlier builds wrote straight into the state tables (base
/// [`REVIEWED_BASE`], no horizon, not pinned or edited by the user): those seen in the last
/// [`RESEED_WINDOW_MS`] go back to the consolidator as signals (their stored quote and event), so
/// it re-lists the real ones; all of them drop below the dashboard
/// ([`crate::extract::LOCAL_RULE_MAX_CONFIDENCE`]). Nothing is deleted. Idempotent: a retired row
/// no longer carries that base. Returns `(retired, signals added)`.
pub fn retire_unconfirmed(conn: &mut Connection, now: i64) -> Result<(usize, usize), rusqlite::Error> {
    const DASHBOARD_LINE: f64 = 0.5;
    // An earlier build's review row: no horizon (the consolidator always sets one), not the user's,
    // above the dashboard line, and lifted by a Batch-reviewed quote (base 0.85, or a corroborated
    // 0.6x). User quick-adds and local candidates carry no reviewed quote.
    const OLD_REVIEW: &str = "horizon IS NULL AND user_pinned = 0 AND user_edited_at IS NULL AND base_confidence >= ?1
        AND EXISTS(SELECT 1 FROM state_provenance r WHERE r.state_table = '{t}' AND r.state_id = {row}.id AND r.batch_reviewed = 1)";
    let tx = conn.savepoint()?;
    let (mut retired, mut seeded) = (0, 0);
    for (table, kind) in [(StateTable::Commitments, "direction"), (StateTable::OpenLoops, "kind")] {
        let t = table.as_str();
        let old = OLD_REVIEW.replace("{t}", t).replace("{row}", "s");
        let old_row = OLD_REVIEW.replace("{t}", t).replace("{row}", t);
        let signals: Vec<NewSignal> = tx
            .prepare(&format!(
                "SELECT s.{kind}, s.description, p.event_id, p.quote FROM {t} s
                   JOIN state_provenance p ON p.id = (SELECT id FROM state_provenance WHERE state_table = '{t}'
                        AND state_id = s.id AND quote IS NOT NULL ORDER BY batch_reviewed DESC, id DESC LIMIT 1)
                  WHERE {old} AND s.last_evidence_at >= ?2 ORDER BY s.last_evidence_at, s.id"
            ))?
            .query_map(params![DASHBOARD_LINE, now - RESEED_WINDOW_MS], |r| {
                Ok(NewSignal {
                    kind: signal_kind(&r.get::<_, String>(0)?),
                    text: r.get(1)?,
                    event_id: r.get(2)?,
                    quote: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        seeded += record_signals(&tx, "legacy", &signals, now)?;
        retired += tx.execute(
            &format!("UPDATE {t} SET confidence = MIN(confidence, ?2), base_confidence = ?2 WHERE {old_row}"),
            params![DASHBOARD_LINE, crate::extract::LOCAL_RULE_MAX_CONFIDENCE],
        )?;
    }
    tx.commit()?;
    Ok((retired, seeded))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::event_log::{insert, NewEvent};
    use serde_json::json;

    const HOUR: i64 = 3_600_000;
    const NOW: i64 = 1_792_000_000_000;
    const SAID: &str = "Toru: can you send me the launch deck by Friday? I need it for the board.";

    fn event(conn: &Connection, hash: &str, ts: i64, content: &str) -> i64 {
        insert(
            conn,
            &NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.tinyspeck.slackmacos"),
                window_title: Some("launch"),
                content,
                content_hash: hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn signal(event_id: i64, kind: &'static str, text: &str, quote: &str) -> NewSignal {
        NewSignal { kind, text: text.into(), event_id, quote: quote.into() }
    }

    fn item(event_id: i64, quote: &str, confidence: f64) -> Value {
        json!({"id": null, "kind": "loop", "horizon": "short", "source_event_id": event_id,
               "source_quote": quote, "confidence": confidence, "text": "Send Toru the launch deck"})
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// A finished run at `ts` that read every signal so far.
    fn ran(conn: &mut Connection, ts: i64) {
        trace(conn, ts);
        let read: Vec<i64> = conn.prepare("SELECT id FROM loop_signals").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        apply(conn, &Value::Null, &[], &read, ts).unwrap();
    }

    fn trace(conn: &Connection, ts: i64) {
        conn.execute(
            "INSERT INTO traceability_log (ts, route, purpose, destination, chunk_bytes, chunk_xxh64)
             VALUES (?1, 'batch_relay', ?2, 'relay', 1, 'x')",
            params![ts, PURPOSE],
        )
        .unwrap();
    }

    #[test]
    fn storage_failure_rolls_back_state_effects_and_input_acknowledgement() {
        let mut conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "atomic", NOW, SAID);
        record_signals(&conn, "w1", &[signal(ev, "question", "deck", "can you send me the launch deck")], NOW).unwrap();
        let read: i64 = conn.query_row("SELECT id FROM loop_signals LIMIT 1", [], |r| r.get(0)).unwrap();
        conn.execute_batch("CREATE TEMP TRIGGER reject_processing_ack BEFORE INSERT ON loop_signal_reads BEGIN SELECT RAISE(ABORT, 'injected storage failure'); END;").unwrap();
        let answer = json!({"open":[item(ev, "can you send me the launch deck", 0.9)], "resolved":[], "duplicates":[]});
        assert!(apply(&mut conn, &answer, &[ev], &[read], NOW).is_err());
        assert_eq!(count(&conn, "SELECT count(*) FROM open_loops"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM state_provenance"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM loop_signal_reads"), 0);
        conn.execute_batch("DROP TRIGGER reject_processing_ack").unwrap();
        assert_eq!(apply(&mut conn, &answer, &[ev], &[read], NOW).unwrap().opened, 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM loop_signal_reads"), 1);
    }

    #[test]
    fn signals_are_append_only_and_unique_per_window_kind_and_text() {
        let conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "h1", NOW, SAID);
        let s = signal(ev, "question", "Toru asks for the launch deck", "can you send me the launch deck by Friday?");
        assert_eq!(record_signals(&conn, "20261014-h10", &[s.clone(), s.clone()], NOW).unwrap(), 1);
        assert_eq!(record_signals(&conn, "20261014-h11", &[s], NOW).unwrap(), 1, "another window is a new sighting");
        assert_eq!(count(&conn, "SELECT count(*) FROM loop_signals"), 2);
        // Signals never reach the state tables.
        assert_eq!(count(&conn, "SELECT count(*) FROM open_loops"), 0);
    }

    #[test]
    fn runs_only_when_signals_are_new_and_under_the_daily_cap() {
        let mut conn = crate::open_in_memory().unwrap();
        let day_start = NOW - 10 * HOUR;
        assert!(!consolidation_due(&conn, NOW, day_start).unwrap(), "no signals");
        let ev = event(&conn, "h1", NOW, SAID);
        record_signals(&conn, "w1", &[signal(ev, "question", "deck", "can you send me the launch deck")], NOW - 2 * HOUR).unwrap();
        assert!(consolidation_due(&conn, NOW, day_start).unwrap());
        ran(&mut conn, NOW - HOUR); // a run that read those signals
        assert!(!consolidation_due(&conn, NOW, day_start).unwrap(), "nothing new since the last run");
        record_signals(&conn, "w2", &[signal(ev, "question", "deck again", "can you send me the launch deck")], NOW - HOUR / 2).unwrap();
        assert!(!consolidation_due(&conn, NOW - HOUR / 4, day_start).unwrap(), "within the hour gap");
        assert!(consolidation_due(&conn, NOW, day_start).unwrap());
        for k in 0..RUNS_PER_DAY - 1 {
            trace(&conn, day_start + k);
        }
        assert!(!consolidation_due(&conn, NOW, day_start).unwrap(), "daily cap reached");
    }

    #[test]
    fn an_item_needs_an_exact_quote_of_a_shown_event_and_the_bar() {
        let mut conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "h1", NOW, SAID);
        let other = event(&conn, "h2", NOW, "unrelated text that was never shown to the model");
        let events = [ev];
        let quote = "can you send me the launch deck by Friday?";
        let answer = json!({"open": [
            item(ev, "can you send me the launch deck on Monday?", 0.9), // not in the event
            item(other, "unrelated text that was never shown", 0.9),     // event not shown
            item(ev, quote, 0.8),                                        // under the bar
            item(ev, quote, 0.9),
        ]});
        let applied = apply(&mut conn, &answer, &events, &[], NOW).unwrap();
        assert_eq!((applied.opened, applied.dropped), (1, 3));
        let (confidence, horizon): (f64, String) =
            conn.query_row("SELECT confidence, horizon FROM open_loops", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((confidence, horizon.as_str()), (0.9, "short"), "stored at the model's confidence");
        assert_eq!(count(&conn, "SELECT count(*) FROM state_provenance WHERE batch_reviewed = 1 AND quote IS NOT NULL"), 1);
        // Last seen is when the words were captured: an item rebuilt from old history ages.
        let old = event(&conn, "h3", NOW - 15 * DAY_MS, "Dana: please review the pricing page copy before launch");
        let mut stale = item(old, "please review the pricing page copy", 0.9);
        stale["text"] = json!("Review the pricing page copy");
        assert_eq!(apply(&mut conn, &json!({ "open": [stale] }), &[old], &[], NOW).unwrap().opened, 1);
        assert_eq!(age_by_horizon(&conn, NOW).unwrap(), 1);
    }

    #[test]
    fn at_most_six_items_are_taken_and_the_same_text_relists() {
        let mut conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "h1", NOW, SAID);
        let quote = "can you send me the launch deck by Friday?";
        let mut open: Vec<Value> = (0..8)
            .map(|n| {
                let mut v = item(ev, quote, 0.9);
                v["text"] = json!(format!("Distinct follow-up number {n}"));
                v
            })
            .collect();
        open[1]["text"] = open[0]["text"].clone(); // the backstop key folds this onto the first
        let applied = apply(&mut conn, &json!({ "open": open }), &[ev], &[], NOW).unwrap();
        assert_eq!((applied.opened, applied.relisted, applied.dropped), (5, 1, 2));
        assert_eq!(count(&conn, "SELECT count(*) FROM open_loops"), 5);
    }

    #[test]
    fn resolved_needs_a_quote_and_duplicates_fold_keeping_both_rows() {
        let mut conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "h1", NOW, SAID);
        let done = event(&conn, "h2", NOW + 1, "Me: sent the launch deck to Toru just now, take a look");
        let quote = "can you send me the launch deck by Friday?";
        let mut second = item(ev, "I need it for the board.", 0.9);
        second["text"] = json!("Board needs the deck");
        apply(&mut conn, &json!({"open": [item(ev, quote, 0.9), second]}), &[ev], &[], NOW).unwrap();
        let ids: Vec<i64> = conn.prepare("SELECT id FROM open_loops ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        let (a, b) = (ids[0], ids[1]);

        let answer = json!({
            "resolved": [{"id": format!("L{a}"), "event_id": done, "quote": ""}],
            "duplicates": [{"id": format!("L{b}"), "into_id": format!("L{a}")}],
        });
        let applied = apply(&mut conn, &answer, &[ev, done], &[], NOW + 2).unwrap();
        assert_eq!((applied.resolved, applied.merged), (0, 1), "no quote, no close");
        let row = |conn: &Connection, id: i64| conn.query_row("SELECT status, merged_into FROM open_loops WHERE id = ?1", [id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        }).unwrap();
        assert_eq!(row(&conn, b), ("closed".into(), Some(a)), "kept, linked to the survivor");
        assert_eq!(row(&conn, a).0, "open");
        // The duplicate keeps its own evidence: copied to the survivor, never moved.
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM state_provenance WHERE state_table='open_loops' AND state_id={b}")), 1);

        let answer = json!({"resolved": [{"id": format!("L{a}"), "event_id": done, "quote": "sent the launch deck to Toru"}]});
        assert_eq!(apply(&mut conn, &answer, &[ev, done], &[], NOW + 3).unwrap().resolved, 1);
        assert_eq!(row(&conn, a).0, "closed");
    }

    #[test]
    fn short_quotes_ground_when_they_still_name_a_statement() {
        let conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "hq", NOW, "Me: 明日送ります。 Ok, will do.");
        assert!(grounded(&conn, &[ev], Some(ev), Some("明日送ります")).unwrap().is_some(), "six CJK characters");
        assert!(grounded(&conn, &[ev], Some(ev), Some("will do.")).unwrap().is_some(), "eight characters");
        assert!(grounded(&conn, &[ev], Some(ev), Some("Ok, w")).unwrap().is_none(), "too short to name anything");
    }

    #[test]
    fn rows_age_by_their_horizon_and_pinned_or_dated_rows_stay() {
        let conn = crate::open_in_memory().unwrap();
        let day = DAY_MS;
        conn.execute_batch(&format!(
            "INSERT INTO open_loops (id, kind, description, opened_at, status, confidence, created_at, updated_at, last_evidence_at, horizon, user_pinned) VALUES
               (1, 'other', 'same day, 4 days old', 0, 'open', 0.9, 0, 5, {a}, 'same_day', 0),
               (2, 'other', 'project, 4 days old', 0, 'open', 0.9, 0, 5, {a}, 'project', 0),
               (3, 'other', 'unconfirmed, 15 days old', 0, 'open', 0.3, 0, 5, {b}, NULL, 0),
               (4, 'other', 'pinned, 40 days old', 0, 'open', 1.0, 0, 5, {c}, 'short', 1);
             INSERT INTO commitments (id, direction, description, status, confidence, created_at, updated_at, last_evidence_at, due_at) VALUES
               (1, 'mine', 'due next week', 'open', 0.9, 0, 5, {b}, {d}),
               (2, 'mine', 'no due date', 'open', 0.9, 0, 5, {b}, NULL);",
            a = NOW - 4 * day, b = NOW - 15 * day, c = NOW - 40 * day, d = NOW + 7 * day,
        )).unwrap();
        assert_eq!(age_by_horizon(&conn, NOW).unwrap(), 3);
        assert_eq!(age_by_horizon(&conn, NOW).unwrap(), 0, "idempotent");
        let loops: Vec<(i64, String, bool, i64)> = conn
            .prepare("SELECT id, status, aged_at IS NOT NULL, updated_at FROM open_loops ORDER BY id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap()
            .collect::<Result<_, _>>().unwrap();
        assert_eq!(loops, vec![
            (1, "closed".into(), true, 5), (2, "open".into(), false, 5),
            (3, "closed".into(), true, 5), (4, "open".into(), false, 5),
        ]);
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments WHERE aged_at IS NOT NULL AND status = 'cancelled'"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments WHERE id = 1 AND status = 'open'"), 1);
    }

    #[test]
    fn old_review_rows_leave_the_dashboard_and_recent_ones_become_signals() {
        let mut conn = crate::open_in_memory().unwrap();
        let recent = event(&conn, "h1", NOW - DAY_MS, SAID);
        let old = event(&conn, "h2", NOW - 9 * DAY_MS, "Lee: can you check the invoice numbers for September?");
        let quote = "can you send me the launch deck by Friday?";
        conn.execute_batch(&format!(
            "INSERT INTO open_loops (id, kind, description, opened_at, status, confidence, base_confidence, created_at, updated_at, last_evidence_at, horizon, user_pinned) VALUES
               (1, 'reply_needed', 'Send Toru the deck', 0, 'open', 0.85, 0.85, 0, 0, {r}, NULL, 0),
               (2, 'other', 'Check the invoice numbers', 0, 'open', 0.85, 0.85, 0, 0, {o}, NULL, 0),
               (3, 'other', 'consolidator item', 0, 'open', 0.9, 0.9, 0, 0, {r}, 'short', 0),
               (4, 'other', 'pinned', 0, 'open', 0.85, 0.85, 0, 0, {r}, NULL, 1),
               (5, 'other', 'corroborated', 0, 'open', 0.62, 0.63, 0, 0, {o}, NULL, 0),
               (6, 'other', 'quick add', 0, 'open', 0.7, 0.7, 0, 0, {r}, NULL, 0);
             INSERT INTO state_provenance (state_table, state_id, event_id, quote, batch_reviewed) VALUES
               ('open_loops', 1, {recent}, '{quote}', 1),
               ('open_loops', 2, {old}, 'check the invoice numbers for September', 1),
               ('open_loops', 5, {old}, 'invoice numbers', 1);",
            r = NOW - DAY_MS, o = NOW - 9 * DAY_MS,
        )).unwrap();
        assert_eq!(retire_unconfirmed(&mut conn, NOW).unwrap(), (3, 1), "every reviewed row retired, only the recent one re-seeded");
        assert_eq!(retire_unconfirmed(&mut conn, NOW).unwrap(), (0, 0), "once");
        let c: Vec<f64> = conn.prepare("SELECT confidence FROM open_loops ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(c, vec![0.4, 0.4, 0.9, 0.85, 0.4, 0.7], "below the dashboard, nothing deleted, pinned and unreviewed rows untouched");
        let signal: (String, i64, String) = conn
            .query_row("SELECT kind, event_id, quote FROM loop_signals", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(signal, ("question".into(), recent, quote.into()));
        // The consolidator re-lists the real one in place, from its own evidence.
        let input = consolidation_input(&conn, NOW).unwrap().unwrap();
        let mut answer = item(recent, quote, 0.9);
        answer["text"] = json!("Send Toru the launch deck before the board");
        let applied = apply(&mut conn, &json!({ "open": [answer] }), &input.events, &input.read, NOW).unwrap();
        assert_eq!((applied.relisted, applied.opened), (1, 0));
        let row: (f64, Option<String>) =
            conn.query_row("SELECT confidence, horizon FROM open_loops WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(row, (0.9, Some("short".into())));
    }

    #[test]
    fn fresh_signals_go_first_and_the_backlog_fills_the_rest() {
        let mut conn = crate::open_in_memory().unwrap();
        let pr = |conn: &Connection, n: usize| {
            let ev = event(conn, &format!("b{n}"), NOW - HOUR, &format!("Sam: please review pull request number {n} today"));
            signal(ev, "followup", &format!("Review pull request {n}"), &format!("please review pull request number {n}"))
        };
        // A re-seeded backlog of 30, then this hour's 3.
        let backlog: Vec<NewSignal> = (0..30).map(|n| pr(&conn, n)).collect();
        record_signals(&conn, "legacy", &backlog, NOW - HOUR).unwrap();
        let fresh: Vec<NewSignal> = (100..103).map(|n| pr(&conn, n)).collect();
        record_signals(&conn, "20261014-h10", &fresh, NOW).unwrap();

        let first = consolidation_input(&conn, NOW).unwrap().unwrap();
        assert_eq!(first.events.len(), MAX_SOURCES);
        for n in [100, 101, 102, 29] {
            assert!(first.text.contains(&format!("Review pull request {n} ")), "{n} goes in");
        }
        assert!(!first.text.contains("Review pull request 0 "), "the oldest backlog waits");
        apply(&mut conn, &Value::Null, &first.events, &first.read, NOW).unwrap();
        // The next hour's signal still goes first; the backlog fills the rest, none skipped.
        record_signals(&conn, "20261014-h11", &[pr(&conn, 200)], NOW + HOUR).unwrap();
        let second = consolidation_input(&conn, NOW + HOUR).unwrap().unwrap();
        assert_eq!(second.events.len(), 1 + 30 + 3 - MAX_SOURCES);
        assert!(second.text.contains("Review pull request 200 ") && second.text.contains("Review pull request 0 "));
        apply(&mut conn, &Value::Null, &second.events, &second.read, NOW + HOUR).unwrap();
        assert!(consolidation_input(&conn, NOW + HOUR).unwrap().is_none());
    }

    #[test]
    fn the_input_carries_new_signals_with_their_source_and_the_open_list() {
        let conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "h1", NOW - HOUR, SAID);
        record_signals(&conn, "w1", &[signal(ev, "question", "Toru asks for the deck", "can you send me the launch deck by Friday?")], NOW - HOUR).unwrap();
        let input = consolidation_input(&conn, NOW).unwrap().unwrap();
        assert_eq!(input.events, vec![ev]);
        assert!(input.text.contains(&format!("question | E{ev} | Toru asks for the deck")));
        assert!(input.text.contains(&format!("[E{ev}] com.tinyspeck.slackmacos · launch")));
        assert!(input.text.contains("I need it for the board."), "the excerpt around the quote");
        // An open row whose evidence is a meeting transcript is listed without that evidence.
        let meeting = insert(&conn, &NewEvent { ts: NOW - HOUR, source: "meeting", kind: "transcript", app_bundle_id: None,
            window_title: None, content: "Me: I will send the budget numbers tonight", content_hash: "m1", dwell_ms: 0,
            display_id: None, window_bounds: None }).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO commitments (id, direction, description, status, confidence, created_at, updated_at, last_evidence_at)
               VALUES (9, 'mine', 'Send the budget numbers', 'open', 0.9, 0, 0, {NOW});
             INSERT INTO state_provenance (state_table, state_id, event_id, quote, batch_reviewed)
               VALUES ('commitments', 9, {meeting}, 'I will send the budget numbers tonight', 1);"
        )).unwrap();
        let input = consolidation_input(&conn, NOW).unwrap().unwrap();
        assert!(input.text.contains("C9 | commitment"));
        assert!(!input.text.contains("budget numbers tonight") && !input.events.contains(&meeting));
        let mut conn = conn;
        ran(&mut conn, NOW);
        assert!(consolidation_input(&conn, NOW + 1).unwrap().is_none(), "nothing new after a run");
    }

    fn threaded(conn: &Connection, hash: &str, ts: i64, thread: &str, content: &str) -> i64 {
        let id = event(conn, hash, ts, content);
        conn.execute("UPDATE event_log SET thread_key = ?2 WHERE id = ?1", params![id, thread]).unwrap();
        id
    }

    fn commitment_item(event_id: i64, quote: &str) -> Value {
        json!({"id": null, "kind": "commitment", "horizon": "short", "source_event_id": event_id,
               "source_quote": quote, "confidence": 0.9, "text": "Send Toru the launch deck"})
    }

    const PROMISE: &str = "I'll send Toru the launch deck by Friday.";

    /// E49: an outcome a later message reports for a row the consolidator owns is shown to it once,
    /// with its source, and only its quoted resolution closes the row.
    #[test]
    fn outcomes_of_listed_items_reach_the_consolidator_once() {
        let mut conn = crate::open_in_memory().unwrap();
        let day_start = NOW - 10 * HOUR;
        let ev = threaded(&conn, "h1", NOW - 3 * HOUR, "t1", PROMISE);
        apply(&mut conn, &json!({"open": [commitment_item(ev, PROMISE)]}), &[ev], &[], NOW - 3 * HOUR).unwrap();
        let id: i64 = conn.query_row("SELECT id FROM commitments", [], |r| r.get(0)).unwrap();
        let done = threaded(&conn, "h2", NOW - HOUR, "t1", "Sent the launch deck to Toru.");
        let found = crate::loop_updates::reconcile(&mut conn, NOW - HOUR / 2).unwrap();
        assert_eq!((found.recorded, found.closed), (1, 0), "a reviewed row is the consolidator's: {found:?}");
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments WHERE status = 'open'"), 1);

        assert!(consolidation_due(&conn, NOW, day_start).unwrap(), "an outcome of a listed item is news");
        let input = consolidation_input(&conn, NOW).unwrap().unwrap();
        assert!(input.text.contains("Outcomes noticed in later messages"), "{}", input.text);
        assert!(input.text.contains(&format!("C{id} | resolved | E{done} | \"Sent the launch deck to Toru\"")), "{}", input.text);
        assert!(input.text.contains(&format!("[E{done}]")), "its source is shown: {}", input.text);
        assert!(input.events.contains(&done));
        assert!(input.read.iter().any(|r| *r < 0), "the outcome is read by this call: {:?}", input.read);
        // Every section header of the input is one of SECTIONS, in order, each exactly once (the
        // instruction describes SECTIONS; shogun-core tests that side).
        let headers: Vec<&str> = input.text.lines().filter(|l| SECTIONS.iter().any(|(name, _)| l.starts_with(&format!("{name}:")) || l.starts_with(&format!("{name} (")))).collect();
        assert_eq!(headers.len(), SECTIONS.len(), "{headers:?}");
        for (i, line) in headers.iter().enumerate() {
            assert!(line.starts_with(&section_header(i)), "{line} / {}", section_header(i));
        }

        trace(&conn, NOW);
        let answer = json!({"resolved": [{"id": format!("C{id}"), "event_id": done, "quote": "Sent the launch deck to Toru"}]});
        assert_eq!(apply(&mut conn, &answer, &input.events, &input.read, NOW).unwrap().resolved, 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments WHERE status = 'done'"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM loop_updates WHERE read_at IS NULL"), 0);
        assert!(!consolidation_due(&conn, NOW + 2 * HOUR, day_start).unwrap(), "read once");
        assert!(consolidation_input(&conn, NOW + 2 * HOUR).unwrap().is_none());
    }

    /// E49: a re-list citing newer words with a date moves a commitment's due date; older words do
    /// not move it back; and a closed row is not reopened on the words it already cites.
    #[test]
    fn a_newer_relist_redates_and_a_closed_row_is_not_reopened_on_old_words() {
        let mut conn = crate::open_in_memory().unwrap();
        let ev = event(&conn, "h1", NOW, PROMISE);
        apply(&mut conn, &json!({"open": [commitment_item(ev, PROMISE)]}), &[ev], &[], NOW).unwrap();
        let (id, first): (i64, i64) = conn.query_row("SELECT id, due_at FROM commitments", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let moved = "Update: the launch deck for Toru moves to Monday.";
        let later = event(&conn, "h2", NOW + DAY_MS, moved);
        let mut relist = commitment_item(later, moved);
        relist["id"] = json!(format!("C{id}"));
        assert_eq!(apply(&mut conn, &json!({"open": [relist]}), &[later], &[], NOW + DAY_MS).unwrap().relisted, 1);
        let due = |conn: &Connection| conn.query_row("SELECT due_at FROM commitments WHERE id = ?1", [id], |r| r.get::<_, i64>(0)).unwrap();
        let monday = due(&conn);
        assert!(monday > first, "moved later");
        // The first words again (older than the move) do not move it back.
        let mut again = commitment_item(ev, PROMISE);
        again["id"] = json!(format!("C{id}"));
        apply(&mut conn, &json!({"open": [again]}), &[ev], &[], NOW + 2 * DAY_MS).unwrap();
        assert_eq!(due(&conn), monday);
        // A user's own date stands.
        conn.execute("UPDATE commitments SET user_edited_at = ?2 WHERE id = ?1", params![id, NOW]).unwrap();
        let other = "Heads up, the launch deck for Toru slips to Thursday.";
        let latest = event(&conn, "h4", NOW + 3 * DAY_MS, other);
        let mut relist = commitment_item(latest, other);
        relist["id"] = json!(format!("C{id}"));
        apply(&mut conn, &json!({"open": [relist]}), &[latest], &[], NOW + 3 * DAY_MS).unwrap();
        assert_eq!(due(&conn), monday);
        conn.execute("UPDATE commitments SET user_edited_at = NULL WHERE id = ?1", [id]).unwrap();

        let done = event(&conn, "h3", NOW + 4 * DAY_MS, "Sent the launch deck to Toru.");
        let answer = json!({"resolved": [{"id": format!("C{id}"), "event_id": done, "quote": "Sent the launch deck to Toru"}]});
        assert_eq!(apply(&mut conn, &answer, &[done], &[], NOW + 4 * DAY_MS).unwrap().resolved, 1);
        // The promise's own words, re-reported as new, are not a new promise.
        let applied = apply(&mut conn, &json!({"open": [commitment_item(ev, PROMISE)]}), &[ev], &[], NOW + 5 * DAY_MS).unwrap();
        assert_eq!((applied.opened, applied.dropped), (0, 1));
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 1);
    }
}
