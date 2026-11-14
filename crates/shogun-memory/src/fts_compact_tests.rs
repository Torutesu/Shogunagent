//! #714 step 3: the compaction primitives on a fragmented, encrypted index built the way the
//! product builds it (capture batches through the `event_log` triggers, then re-indexed older rows).
//!
//! What has to hold:
//! - the loop of bounded steps reaches one segment, the index gets smaller, and every match set
//!   and bm25 order is unchanged;
//! - each step is short and writes little WAL;
//! - a stop (even a closed connection) between steps loses nothing and the next run finishes;
//! - writes that land between steps never make the job restart the merge in progress;
//! - "done" is detected from the structure record, which is exact on the shipped engine.

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};

use crate::fts_compact::{
    db_pages, merge_step, plan, structure_record, Move, Structure, FTS_TABLES,
};
use crate::fts_page_size_tests::{
    corpus, insert_events, key, match_queries, open_encrypted_at, overflow_pages, ranked, remove,
    segments, temp, Row, NATURAL_QUERIES, PRE_V57,
};
use crate::open_encrypted;
use crate::search::fts_search;

type Snapshot = Vec<(String, Vec<(i64, f64)>)>;

/// Every representative query's full ranked result (rowid + bm25), plus the product's own
/// retrieval for the natural-language ones.
fn snapshot(conn: &Connection) -> Snapshot {
    let mut out: Snapshot = match_queries().into_iter().map(|q| (q.clone(), ranked(conn, &q))).collect();
    for natural in NATURAL_QUERIES {
        let hits = fts_search(conn, natural, 20).unwrap();
        out.push((format!("fts_search:{natural}"), hits.iter().map(|id| (*id, 0.0)).collect()));
    }
    for (query, rows) in &out {
        assert!(!rows.is_empty(), "{query} must match something, or the parity check is vacuous");
    }
    out
}

fn total_changes(conn: &Connection) -> i64 {
    conn.query_row("SELECT total_changes()", [], |r| r.get(0)).unwrap()
}

fn wal_bytes(path: &Path) -> u64 {
    std::fs::metadata(format!("{}-wal", path.display())).map(|m| m.len()).unwrap_or(0)
}

fn leaf_pages(conn: &Connection) -> u64 {
    plan(conn, "event_fts").unwrap().leaf_pages
}

/// Re-index older rows the way pre-V41 dedup touches and OCR re-scans did: the text is unchanged,
/// but each touch deletes and re-inserts the row's postings, one capture-sized transaction at a
/// time, leaving the old copies in deeper segments.
fn reindex_old_rows(conn: &Connection, rounds: i64) {
    for round in 0..rounds {
        conn.execute("UPDATE event_log SET content = content WHERE id % ?1 = 0", [round + 5]).unwrap();
    }
}

/// A fragmented encrypted index: the first half written before V57 (4050-byte leaves), the rest
/// after it, then re-indexed older rows.
fn fragmented(name: &str, rows: &[Row]) -> (std::path::PathBuf, Connection) {
    let path = temp(name);
    remove(&path);
    let (first, second) = rows.split_at(rows.len() / 2);
    drop({
        let conn = open_encrypted_at(&path, PRE_V57);
        insert_events(&conn, first);
        conn
    });
    let conn = open_encrypted(&path, &key()).unwrap();
    insert_events(&conn, second);
    reindex_old_rows(&conn, 12);
    (path, conn)
}

/// What a run of [`drive`] measured.
struct Run {
    steps: u32,
    max_step: Duration,
    max_wal: u64,
}

/// The job's inner loop, as the core runs it: plan, one bounded step, repeat — per table, until
/// every table is done or `max_steps` is reached.
fn drive(conn: &Connection, path: &Path, pages: u32, max_steps: u32) -> Run {
    let mut run = Run { steps: 0, max_step: Duration::ZERO, max_wal: 0 };
    for table in FTS_TABLES {
        loop {
            if run.steps >= max_steps {
                return run;
            }
            let state = plan(conn, table).unwrap();
            if state.next == Move::Done {
                break;
            }
            let started = Instant::now();
            let outcome = merge_step(conn, table, state.next, pages).unwrap();
            run.max_step = run.max_step.max(started.elapsed());
            run.max_wal = run.max_wal.max(wal_bytes(path));
            run.steps += 1;
            if !outcome.progressed {
                break;
            }
        }
    }
    run
}

#[test]
fn bounded_steps_shrink_a_fragmented_index_and_keep_every_result() {
    let rows = corpus(900);
    let (path, conn) = fragmented("compact_parity", &rows);

    let before = snapshot(&conn);
    let state = plan(&conn, "event_fts").unwrap();
    assert!(state.segments >= 2, "the fixture must be fragmented: {state:?}");
    assert_ne!(state.next, Move::Done, "a fragmented index needs work: {state:?}");
    let leaves_before = state.leaf_pages;
    let pages_before = db_pages(&conn).unwrap();

    let run = drive(&conn, &path, 16, 10_000);
    assert!(run.steps > 5, "many small steps, not one big rewrite: {}", run.steps);

    for table in FTS_TABLES {
        assert_eq!(plan(&conn, table).unwrap().next, Move::Done, "{table} finished");
    }
    assert_eq!(segments(&conn), 1, "one segment left");
    let leaves_after = leaf_pages(&conn);
    assert!(
        leaves_after * 100 < leaves_before * 80,
        "stale versions dropped: {leaves_before} -> {leaves_after} leaf pages"
    );
    assert_eq!(overflow_pages(&conn, "event_fts_data"), 0, "every leaf rewritten at pgsz 3960");
    let pages_after = db_pages(&conn).unwrap();
    assert!(
        pages_after.used_bytes() < pages_before.used_bytes(),
        "used bytes shrink: {} -> {}",
        pages_before.used_bytes(),
        pages_after.used_bytes()
    );
    assert!(pages_after.freelist_count > pages_before.freelist_count, "freed pages go to the freelist");
    assert_eq!(snapshot(&conn), before, "match sets and bm25 order unchanged");

    // Each step is its own short transaction. Generous bounds for an unoptimised test build; the
    // #714 measurement for a 64-page step on a 747 MB index was ≤ 238 ms and ≤ 16 MB of WAL.
    assert!(run.max_step < Duration::from_secs(2), "longest step {:?}", run.max_step);
    assert!(run.max_wal <= 8 * 1024 * 1024, "WAL peaked at {} bytes", run.max_wal);

    drop(conn);
    remove(&path);
}

#[test]
fn done_is_read_from_the_structure_record_on_the_shipped_engine() {
    let conn = crate::open_in_memory().unwrap();
    // The measurements, and the V1 structure format decoded here, are for this engine.
    let version: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0)).unwrap();
    // Re-verified on 3.54.1 (SQLCipher 4.20.0): `fts5StructureWrite` still writes V1 unless the
    // table is `contentless_delete`, the merge/optimize code is unchanged from 3.39.4, and the
    // decoder only gained a `pgnoLast > FTS5_MAX_PGNO` corruption check.
    assert_eq!(version, "3.54.1", "re-verify the completion signal on a new SQLite");

    insert_events(&conn, &corpus(120));
    reindex_old_rows(&conn, 3);
    let path = Path::new("");
    drive(&conn, path, 32, 10_000);
    assert_eq!(plan(&conn, "event_fts").unwrap().next, Move::Done);
    assert_eq!(segments(&conn), 1);

    // With one segment left, FTS5 has nothing to merge in either direction: the structure record
    // is byte-identical, and `total_changes()` still moves by one (the command row itself). So a
    // "changed less than 2" threshold works but the record comparison is exact.
    for command in [-64i64, 64] {
        let record = structure_record(&conn, "event_fts").unwrap();
        let changes = total_changes(&conn);
        conn.execute("INSERT INTO event_fts(event_fts, rank) VALUES ('merge', ?1)", [command]).unwrap();
        assert_eq!(total_changes(&conn) - changes, 1, "merge {command}");
        assert_eq!(structure_record(&conn, "event_fts").unwrap(), record, "merge {command}");
    }
    let outcome = merge_step(&conn, "event_fts", Move::Begin, 64).unwrap();
    assert!(!outcome.progressed, "a no-op step reports no progress");
}

#[test]
fn a_stop_between_steps_loses_nothing_and_the_next_run_finishes() {
    let rows = corpus(900);
    let (path, conn) = fragmented("compact_resume", &rows);
    let before = snapshot(&conn);

    // Start, then stop after a few steps, and close the connection as an app quit would.
    let first = drive(&conn, &path, 8, 6);
    assert_eq!(first.steps, 6);
    let midway = plan(&conn, "event_fts").unwrap();
    assert_eq!(midway.next, Move::Continue, "the merge in progress is kept: {midway:?}");
    assert_eq!(snapshot(&conn), before, "a half-merged index answers exactly the same");
    drop(conn);

    // Resume in a new process: the structure record says where the merge is; nothing else is stored.
    let conn = open_encrypted(&path, &key()).unwrap();
    assert_eq!(plan(&conn, "event_fts").unwrap(), midway, "resumes from the same place");
    drive(&conn, &path, 8, 10_000);
    for table in FTS_TABLES {
        assert_eq!(plan(&conn, table).unwrap().next, Move::Done, "{table} finished");
    }
    assert_eq!(segments(&conn), 1);
    assert_eq!(snapshot(&conn), before);
    drop(conn);
    remove(&path);
}

#[test]
fn captures_between_steps_never_restart_the_merge_in_progress() {
    let rows = corpus(900);
    let (fixture, extra) = rows.split_at(700);
    let (path, conn) = fragmented("compact_live", fixture);

    // Begin, then keep writing between steps like the capture path does. Each continue step must
    // shrink what the merge still has to read; a negative step here would fold the new segments
    // into a fresh merge and read the already-merged output again.
    if plan(&conn, "event_fts").unwrap().next == Move::Begin {
        merge_step(&conn, "event_fts", Move::Begin, 8).unwrap();
    }
    // The job's merge is the deepest one in progress: it holds every segment that existed when it
    // began. Recent segments may start small merges of their own below it (the positive merge
    // finishes those first); that pauses the big merge but never restarts it.
    let deepest = |conn: &Connection| -> Option<(u32, u64)> {
        let record = structure_record(conn, "event_fts").unwrap()?;
        let decoded = Structure::decode(&record)?;
        decoded
            .levels
            .iter()
            .enumerate()
            .rev()
            .find(|(_, level)| level.merging > 0)
            .map(|(i, level)| {
                let pending = level.segments.iter().take(level.merging as usize).map(|s| s.pages()).sum();
                (i as u32, pending)
            })
    };
    let (level, started_with) = deepest(&conn).expect("a merge is in progress");
    let mut pending = started_with;
    let mut steps = 0;
    for batch in extra.chunks(2) {
        insert_events(&conn, batch);
        // A capture's own automerge also works on the merge in progress, and may finish it.
        let Some((now_level, now_pending)) = deepest(&conn) else { break };
        assert_eq!(now_level, level, "the merge stays on its level");
        assert!(now_pending <= pending, "after capture {steps}: {pending} -> {now_pending}");
        assert_eq!(plan(&conn, "event_fts").unwrap().next, Move::Continue);
        assert!(merge_step(&conn, "event_fts", Move::Continue, 8).unwrap().progressed);
        let Some((_, stepped)) = deepest(&conn) else { break };
        assert!(stepped <= now_pending, "step {steps}: {now_pending} -> {stepped}");
        pending = stepped;
        steps += 1;
    }
    assert!(steps >= 5, "the merge must have run alongside several capture batches");
    assert!(pending < started_with, "the merge advanced: {started_with} -> {pending}");
    // Whatever the captures added meanwhile, the merge that began holds every older segment: once
    // it is done, the old stale versions are gone.
    drive(&conn, &path, 16, 10_000);
    assert_eq!(plan(&conn, "event_fts").unwrap().next, Move::Done);
    drop(conn);
    remove(&path);
}

#[test]
fn a_stalled_continue_step_progresses_and_restores_usermerge() {
    let conn = crate::open_in_memory().unwrap();
    insert_events(&conn, &corpus(200));
    // Test setup only (a small in-memory index): the product never runs 'optimize'.
    conn.execute("INSERT INTO event_fts(event_fts) VALUES ('optimize')", []).unwrap();
    assert_eq!(segments(&conn), 1);

    // Two segments, then a one-page negative step: a merge of 2 inputs is in progress.
    let one = |conn: &Connection, ts: i64| {
        conn.execute(
            "INSERT INTO event_log (ts, source, kind, window_title, content, content_hash, last_seen_at)
             VALUES (?1, 'capture', 'text', 'stall', 'stall probe text for a new segment', ?2, ?1)",
            params![ts, format!("stall-{ts}")],
        )
        .unwrap();
    };
    one(&conn, 90_000);
    merge_step(&conn, "event_fts", Move::Begin, 1).unwrap();
    let decoded = Structure::decode(&structure_record(&conn, "event_fts").unwrap().unwrap()).unwrap();
    assert!(decoded.merge_in_progress(), "{decoded:?}");
    // Two fresh segments below it: a plain positive merge picks their level (2 > 2 is false for
    // the merging level) and then refuses it (2 < usermerge 4) — no progress.
    one(&conn, 90_001);
    one(&conn, 90_002);
    let record = structure_record(&conn, "event_fts").unwrap();
    conn.execute("INSERT INTO event_fts(event_fts, rank) VALUES ('merge', 1)", []).unwrap();
    assert_eq!(structure_record(&conn, "event_fts").unwrap(), record, "the stall this handles");

    let usermerge = |conn: &Connection| -> Option<i64> {
        conn.query_row("SELECT v FROM event_fts_config WHERE k = 'usermerge'", [], |r| r.get(0)).ok()
    };
    assert_eq!(usermerge(&conn), None);
    assert!(merge_step(&conn, "event_fts", Move::Continue, 1).unwrap().progressed);
    assert_eq!(usermerge(&conn), Some(4), "restored to FTS5's default");
    let after = Structure::decode(&structure_record(&conn, "event_fts").unwrap().unwrap()).unwrap();
    assert!(after.merge_in_progress(), "the original merge was continued, not restarted: {after:?}");
    drive(&conn, Path::new(""), 64, 10_000);
    assert_eq!(plan(&conn, "event_fts").unwrap().next, Move::Done);
}

#[test]
fn leaves_written_before_v57_are_rewritten_even_without_stale_versions() {
    let rows = corpus(600);
    let (old, new) = rows.split_at(590);
    let path = temp("compact_pgsz");
    remove(&path);
    drop({
        let conn = open_encrypted_at(&path, PRE_V57);
        insert_events(&conn, old);
        // Test setup only: one segment of 4050-byte leaves, as a long-lived index would have.
        conn.execute("INSERT INTO event_fts(event_fts) VALUES ('optimize')", []).unwrap();
        conn
    });
    let conn = open_encrypted(&path, &key()).unwrap();
    insert_events(&conn, new);
    let before = snapshot(&conn);
    let state = plan(&conn, "event_fts").unwrap();
    assert!(state.pages_outside_largest * 10 < state.leaf_pages, "no bloat to speak of: {state:?}");
    assert!(state.oversized_leaves, "the big segment has pre-V57 leaves: {state:?}");
    assert_eq!(state.next, Move::Begin);
    assert!(overflow_pages(&conn, "event_fts_data") > 0, "4050-byte leaves spill under SQLCipher");

    drive(&conn, &path, 16, 10_000);
    let after = plan(&conn, "event_fts").unwrap();
    assert!(!after.oversized_leaves && after.next == Move::Done, "{after:?}");
    assert_eq!(overflow_pages(&conn, "event_fts_data"), 0, "every leaf rewritten at pgsz 3960");
    assert_eq!(snapshot(&conn), before);
    drop(conn);
    remove(&path);
}
