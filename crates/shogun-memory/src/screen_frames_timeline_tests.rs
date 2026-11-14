//! #727: the Visual Recall timeline evaluates its full-text match once per request.
//!
//! The per-frame `EXISTS (… MATCH …)` it replaced is kept here as the oracle, verbatim apart from
//! the self-capture filter ([`RECALL_VISIBLE`]) that every Recall read gained after #727. What has
//! to hold:
//! - same frames, same order, same counts, same pages, on an encrypted database, across OR queries,
//!   quoted phrases, CJK, window-title matches, no-match, missing and deleted Accessibility links,
//!   timestamp ties, time and app filters, hidden self-captures (and visible lookalikes), and every
//!   page boundary in both directions;
//! - the statements read `event_fts` once, uncorrelated, so the MATCH does not run per frame.
//!
//! The wall-clock side (p50/p95 at 100 / 1k / 5k frames against the 500 ms local search SLO) is an
//! explicit release-mode gate in `tests/search_slo.rs`.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use super::*;
use crate::event_log::{insert as insert_event, NewEvent};

//
// The per-frame EXISTS shape exactly as it was, plus `AND {RECALL_VISIBLE}`: the self-capture
// filter is part of today's contract, so the oracle has to apply it too or hidden frames would
// count as a difference.

fn legacy_page(conn: &Connection, query: &TimelineQuery<'_>) -> Vec<FrameSummary> {
    let cursor_ts = query.cursor.map(|value| value.created_at_ms);
    let cursor_id = query.cursor.map(|value| value.id);
    let (cursor_predicate, order, reverse) = match query.direction {
        TimelineDirection::Older => (
            "f.created_at_ms < ?4 OR (f.created_at_ms = ?4 AND f.id < ?5)",
            "f.created_at_ms DESC, f.id DESC",
            false,
        ),
        TimelineDirection::Newer => (
            "f.created_at_ms > ?4 OR (f.created_at_ms = ?4 AND f.id > ?5)",
            "f.created_at_ms ASC, f.id ASC",
            true,
        ),
    };
    let where_base = format!(
        "f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
         AND (?3 IS NULL OR f.app_bundle_id = ?3)
         AND (?4 IS NULL OR {cursor_predicate}) AND {RECALL_VISIBLE}"
    );
    let limit = i64::try_from(query.limit).unwrap();
    let expr = crate::search::fts_query(query.query).expect("oracle covers FTS queries only");
    let mut stmt = conn
        .prepare(&format!(
            "{SUMMARY_SELECT}
             FROM screen_frames f
             {SUMMARY_JOINS}
             WHERE {where_base}
               AND EXISTS (
                   SELECT 1 FROM event_fts
                   WHERE event_fts MATCH ?6
                     AND event_fts.rowid IN (f.event_id, f.accessibility_event_id)
               )
             ORDER BY {order} LIMIT ?7"
        ))
        .unwrap();
    let rows = stmt
        .query_map(
            params![
                query.from_ms,
                query.to_ms,
                query.app_bundle_id,
                cursor_ts,
                cursor_id,
                expr,
                limit
            ],
            map_summary,
        )
        .unwrap();
    let mut frames: Vec<_> = rows.map(|r| r.unwrap()).collect();
    if reverse {
        frames.reverse();
    }
    frames
}

fn legacy_count_sql() -> String {
    format!(
        "SELECT count(*) FROM screen_frames f
         WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
           AND (?3 IS NULL OR f.app_bundle_id = ?3) AND {RECALL_VISIBLE}
           AND EXISTS (
               SELECT 1 FROM event_fts
               WHERE event_fts MATCH ?4
                 AND event_fts.rowid IN (f.event_id, f.accessibility_event_id)
           )"
    )
}

fn legacy_count(conn: &Connection, query: &str, from_ms: i64, to_ms: i64, app: Option<&str>) -> i64 {
    let expr = crate::search::fts_query(query).expect("oracle covers FTS queries only");
    conn.query_row(&legacy_count_sql(), params![from_ms, to_ms, app, expr], |row| row.get(0))
        .unwrap()
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shogun_vr_timeline_{}_{name}.db", std::process::id()))
}

fn remove(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// Deterministic, so a failure reproduces.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[usize::try_from(self.next()).unwrap() % items.len()]
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.next().is_multiple_of(n)
    }
}

const WORDS: &[&str] = &[
    "status", "update", "budget", "review", "sprint", "ticket", "vendor", "renewal", "latency",
    "dashboard", "incident", "release", "schedule", "invoice", "customer", "pricing", "draft",
];
const NEEDLES: &[&str] = &[
    "project comet",
    "quarterly roadmap slide",
    "Frankfurt residency decision",
    "請求書の再発行",
    "オンボーディング資料",
    "会議室の予約",
    "kestrelDbMigrator",
];
const TITLES: &[&str] = &["Inbox", "Standup notes", "Design review", "Terminal", "議事録"];
const APPS: &[&str] = &["com.apple.Safari", "com.apple.Mail", "com.tinyspeck.slackmacgap"];
/// SHOGUN's own windows, in the spellings [`RECALL_VISIBLE`] hides (exact ids, any case, isolated
/// build suffixes, the empty string). Their frames still match the MATCH and must never surface.
const SELF_APPS: &[&str] = &[
    "com.syogun.shogunai",
    "COM.SYOGUN.SHOGUNAI",
    "com.syogun.shogunai.wt727",
    "dev.shogun.spike",
    "dev.shogun.spike.dev",
    "SHOGUN",
    "shogun-desktop-spike",
    "",
];
/// Lookalikes [`RECALL_VISIBLE`] keeps visible: not SHOGUN, so they must still match and page.
const LOOKALIKE_APPS: &[&str] = &["com.syogun.shogunaix", "dev.shogun.spiked"];

fn text(rng: &mut Lcg) -> String {
    let mut out: Vec<&str> = (0..12).map(|_| rng.pick(WORDS)).collect();
    if rng.one_in(3) {
        out.push(rng.pick(NEEDLES));
    }
    out.join(" ")
}

fn event(conn: &Connection, rng: &mut Lcg, ts: i64, source: &str, n: usize) -> i64 {
    let content = text(rng);
    let title = rng.pick(TITLES);
    insert_event(
        conn,
        &NewEvent {
            ts,
            source,
            kind: "text",
            app_bundle_id: Some("com.apple.Safari"),
            window_title: Some(title),
            content: &content,
            content_hash: &format!("vr727-{source}-{n}"),
            dwell_ms: 1,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

/// 300 frames: timestamp ties (three frames per millisecond), a quarter without an Accessibility
/// link, some links nulled by deleting the Accessibility event, some OCR events shared by two
/// frames (sometimes one hidden, one visible), a NULL app, hidden self-captures and visible
/// lookalikes, and events that are never linked to any frame (FTS hits that must not count).
fn seeded(name: &str) -> (Connection, PathBuf) {
    let path = temp(name);
    remove(&path);
    let conn = crate::open_encrypted(&path, &crate::DbKey::new([0x27; 32])).unwrap();
    let mut rng = Lcg(727);
    let mut deleted_ax = Vec::new();
    let mut previous_ocr = None;
    for n in 0..300_usize {
        let ts = 10_000 + i64::try_from(n / 3).unwrap() * 1_000;
        // Unlinked noise the MATCH will hit.
        event(&conn, &mut rng, ts, "capture", 10_000 + n);
        let ocr = match previous_ocr {
            Some(shared) if rng.one_in(10) => shared,
            _ => event(&conn, &mut rng, ts, "screen_ocr", n),
        };
        previous_ocr = Some(ocr);
        let ax = (!rng.one_in(4)).then(|| event(&conn, &mut rng, ts, "capture", 20_000 + n));
        if let Some(ax) = ax {
            if rng.one_in(8) {
                deleted_ax.push(ax);
            }
        }
        let app = if rng.one_in(9) {
            Some(rng.pick(SELF_APPS))
        } else if rng.one_in(25) {
            Some(rng.pick(LOOKALIKE_APPS))
        } else {
            (!rng.one_in(12)).then(|| rng.pick(APPS))
        };
        insert(
            &conn,
            &NewFrame {
                created_at_ms: ts,
                event_id: Some(ocr),
                accessibility_event_id: ax,
                app_bundle_id: app,
                window_title: Some("w"),
                display_id: Some(1),
                width: 100,
                height: 50,
                ocr_region: OcrRegion::WholeFrame,
                jpeg: &[0xFF, 0xD8, 0xFF, 0xD9],
            },
        )
        .unwrap();
    }
    for ax in deleted_ax {
        // ON DELETE SET NULL: the frame keeps its OCR side only.
        conn.execute("DELETE FROM event_log WHERE id = ?1", [ax]).unwrap();
    }
    (conn, path)
}

const QUERIES: &[&str] = &[
    "comet",
    "project comet",
    "quarterly roadmap",
    "\"quarterly roadmap slide\"",
    "Frankfurt OR roadmap",
    "invoice customer pricing",
    "請求書の再発行",
    "オンボーディング",
    "会議室",
    "kestrelDbMigrator",
    "igrator",
    "Standup",
    "議事録",
    "zzqqxxnomatch",
];

fn page_ids(frames: &[FrameSummary]) -> Vec<i64> {
    frames.iter().map(|f| f.id).collect()
}

fn walk(
    conn: &Connection,
    base: TimelineQuery<'_>,
    page: impl Fn(&Connection, &TimelineQuery<'_>) -> Vec<FrameSummary>,
) -> Vec<Vec<i64>> {
    let mut pages = Vec::new();
    let mut query = base;
    loop {
        let mut frames = page(conn, &query);
        let has_more = trim_page_sentinel(&mut frames, base.limit - 1, query.direction);
        pages.push(page_ids(&frames));
        let edge = match query.direction {
            TimelineDirection::Older => frames.last(),
            TimelineDirection::Newer => frames.first(),
        };
        match (has_more, edge) {
            (true, Some(edge)) => {
                query.cursor = Some(TimelineCursor { created_at_ms: edge.created_at_ms, id: edge.id });
            }
            _ => return pages,
        }
    }
}

#[test]
fn timeline_matches_the_per_frame_exists_it_replaced() {
    let (conn, path) = seeded("parity");
    let (min_ts, max_ts): (i64, i64) = conn
        .query_row("SELECT min(created_at_ms), max(created_at_ms) FROM screen_frames", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let mid = (min_ts + max_ts) / 2;
    let windows = [(0, i64::MAX), (min_ts, max_ts), (mid, max_ts), (min_ts, mid), (mid, mid)];
    let apps = [
        None,
        Some("com.apple.Mail"),
        Some("com.apple.Safari"),
        Some("com.example.none"),
        // A hidden self-capture app: always empty, on both sides.
        Some("com.syogun.shogunai"),
        Some("com.syogun.shogunaix"),
    ];

    let mut non_empty = 0;
    for &query in QUERIES {
        for &(from_ms, to_ms) in &windows {
            for &app in &apps {
                // The oracle's per-frame MATCH is the slow part of this test: app filters get the
                // whole range and one narrowed window; every window runs without an app filter.
                if app.is_some() && !matches!((from_ms, to_ms), (0, i64::MAX)) && from_ms != mid {
                    continue;
                }
                let want = legacy_count(&conn, query, from_ms, to_ms, app);
                let got = timeline_count(&conn, query, from_ms, to_ms, app).unwrap();
                assert_eq!(got, want, "count: {query:?} {from_ms}..{to_ms} {app:?}");
                if want > 0 {
                    non_empty += 1;
                }

                // Full result in one page: identical rows, fields and order.
                let all = TimelineQuery {
                    query,
                    from_ms,
                    to_ms,
                    app_bundle_id: app,
                    cursor: None,
                    direction: TimelineDirection::Older,
                    limit: 1_000,
                };
                let legacy_all = legacy_page(&conn, &all);
                assert_eq!(timeline_page(&conn, &all).unwrap(), legacy_all, "page: {query:?}");
                assert_eq!(i64::try_from(legacy_all.len()).unwrap(), want);

                // Every page boundary, older then back newer, the way the desktop pages (limit
                // page_size + 1, sentinel trimmed). Walks are the expensive part (the oracle runs
                // its MATCH per frame), so they cover the unfiltered timeline and one narrowed
                // window + app, with one-row pages for three of the queries.
                let unfiltered = (from_ms, to_ms, app) == (0, i64::MAX, None);
                let narrowed = (from_ms, to_ms, app) == (mid, max_ts, Some("com.apple.Mail"));
                if !(unfiltered || narrowed) {
                    continue;
                }
                let sizes: &[usize] = if unfiltered && QUERIES[..3].contains(&query) {
                    &[1, 7, 80]
                } else {
                    &[4, 80]
                };
                for &page_size in sizes {
                    let older = TimelineQuery { limit: page_size + 1, ..all };
                    let new_pages = walk(&conn, older, |c, q| timeline_page(c, q).unwrap());
                    let old_pages = walk(&conn, older, legacy_page);
                    assert_eq!(new_pages, old_pages, "older pages: {query:?} size {page_size}");
                    assert_eq!(new_pages.concat(), page_ids(&legacy_all));

                    if let Some(oldest) = legacy_all.last() {
                        let newer = TimelineQuery {
                            cursor: Some(TimelineCursor {
                                created_at_ms: oldest.created_at_ms,
                                id: oldest.id,
                            }),
                            direction: TimelineDirection::Newer,
                            ..older
                        };
                        let new_pages = walk(&conn, newer, |c, q| timeline_page(c, q).unwrap());
                        assert_eq!(
                            new_pages,
                            walk(&conn, newer, legacy_page),
                            "newer pages: {query:?} size {page_size}"
                        );
                    }
                }
            }
        }
    }
    // The fixture exercises matches, not just empty sets; and the no-match query stays empty.
    assert!(non_empty > 100, "only {non_empty} non-empty cases");
    assert_eq!(timeline_count(&conn, "zzqqxxnomatch", 0, i64::MAX, None).unwrap(), 0);
    // Accessibility-only and OCR-only matches both count (the frame has both sides seeded).
    let ax_only: i64 = conn
        .query_row(
            "SELECT count(*) FROM screen_frames f
              WHERE f.accessibility_event_id IS NOT NULL
                AND f.accessibility_event_id IN (SELECT rowid FROM event_fts WHERE event_fts MATCH '\"comet\"')
                AND f.event_id NOT IN (SELECT rowid FROM event_fts WHERE event_fts MATCH '\"comet\"')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(ax_only > 0, "fixture needs Accessibility-only matches");
    // Hidden self-captures that the MATCH does hit (so the filter, not the corpus, keeps them out),
    // including some hit only through their Accessibility side; and visible lookalikes that match.
    let matching_term = |term: &str, visible: &str, extra: &str| -> i64 {
        conn.query_row(
            &format!(
                "SELECT count(*) FROM screen_frames f
                  WHERE {visible}{extra}
                    AND EXISTS (SELECT 1 FROM event_fts WHERE event_fts MATCH ?1
                                  AND event_fts.rowid IN (f.event_id, f.accessibility_event_id))"
            ),
            [term],
            |r| r.get(0),
        )
        .unwrap()
    };
    let matching = |visible: &str, extra: &str| matching_term("\"comet\"", visible, extra);
    let hidden_hits = matching(&format!("NOT {RECALL_VISIBLE}"), "");
    assert!(hidden_hits > 0, "fixture needs hidden self-captures the MATCH hits");
    let hidden_ax_only = matching(
        &format!("NOT {RECALL_VISIBLE}"),
        " AND f.event_id NOT IN (SELECT rowid FROM event_fts WHERE event_fts MATCH '\"comet\"')",
    );
    assert!(hidden_ax_only > 0, "fixture needs hidden frames matched only via Accessibility");
    let lookalikes = "f.app_bundle_id IN ('com.syogun.shogunaix', 'dev.shogun.spiked')";
    let lookalike_hits = matching_term("\"status\"", lookalikes, "");
    assert!(lookalike_hits > 0, "fixture needs visible lookalikes the MATCH hits");
    assert_eq!(
        timeline_count(&conn, "status", 0, i64::MAX, Some("com.syogun.shogunaix")).unwrap()
            + timeline_count(&conn, "status", 0, i64::MAX, Some("dev.shogun.spiked")).unwrap(),
        lookalike_hits,
        "lookalikes are listed, not hidden"
    );
    assert_eq!(
        timeline_count(&conn, "comet", 0, i64::MAX, None).unwrap() + hidden_hits,
        matching("1", ""),
        "every matching frame is either listed or hidden"
    );
    drop(conn);
    remove(&path);
}

/// Blank queries browse; non-indexable queries match literal linked text. The oracle uses
/// LIKE, as production does, so ASCII case is ignored ("db" matches "kestrelDbMigrator").
#[test]
fn only_blank_queries_list_everything_in_range() {
    let (conn, path) = seeded("no_terms");
    let visible = format!("SELECT count(*) FROM screen_frames f WHERE {RECALL_VISIBLE}");
    let total: i64 = conn.query_row(&visible, [], |r| r.get(0)).unwrap();
    let all: i64 = conn.query_row("SELECT count(*) FROM screen_frames", [], |r| r.get(0)).unwrap();
    assert!(total < all, "the fixture holds hidden self-captures");
    for query in ["", "  ", "db", "日", "日本", "中文", "the"] {
        let expected = if query.trim().is_empty() {
            total
        } else {
            conn.query_row(
                &format!("SELECT count(*) FROM screen_frames f {SUMMARY_JOINS}
                    WHERE {RECALL_VISIBLE} AND (coalesce(e.content, '') LIKE '%' || ?1 || '%'
                        OR coalesce(ax.content, '') LIKE '%' || ?1 || '%')"),
                [query], |row| row.get::<_, i64>(0),
            ).unwrap()
        };
        assert_eq!(timeline_count(&conn, query, 0, i64::MAX, None).unwrap(), expected);
        let frames = timeline_page(
            &conn,
            &TimelineQuery {
                query,
                from_ms: 0,
                to_ms: i64::MAX,
                app_bundle_id: None,
                cursor: None,
                direction: TimelineDirection::Older,
                limit: 1_000,
            },
        )
        .unwrap();
        assert_eq!(i64::try_from(frames.len()).unwrap(), expected);
    }
    drop(conn);
    remove(&path);
}

fn plan(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows = stmt.query_map(params, |r| r.get::<_, String>(3)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// The structural half of the SLO: `event_fts` is read once per statement and never from a
/// correlated subquery, so the MATCH cost does not multiply by the frames in range. Checked on
/// the shipped SQLite build, since the planner is what decides.
#[test]
fn the_fts_match_runs_once_per_request_not_per_frame() {
    let conn = crate::open_in_memory().unwrap();
    let expr = "\"comet\"";
    let fts_reads = |plan: &[String]| plan.iter().filter(|row| row.contains("event_fts")).count();
    // Per frame: a correlated subquery (SQLite 3.39), or the same EXISTS run as an inner join
    // loop (`SCAN event_fts EXISTS ...`, SQLite 3.53+'s EXISTS-to-join). Either way FTS is
    // consulted once for every frame in range.
    let per_frame = |plan: &[String]| {
        plan.iter().any(|row| {
            row.contains("CORRELATED") || (row.contains("event_fts") && row.contains(" EXISTS "))
        })
    };

    let legacy = plan(&conn, &legacy_count_sql(), params![0, 1, None::<&str>, expr]);
    assert!(per_frame(&legacy), "the oracle is the per-frame shape: {legacy:?}");

    let count = plan(&conn, &timeline_count_sql(true, false), params![0, 1, None::<&str>, expr]);
    let mut pages = Vec::new();
    for direction in [TimelineDirection::Older, TimelineDirection::Newer] {
        let (sql, _) = timeline_page_sql(direction, true, false);
        pages.push(plan(
            &conn,
            &sql,
            params![0, 1, None::<&str>, None::<i64>, None::<i64>, expr, 81],
        ));
    }
    for plan in std::iter::once(&count).chain(&pages) {
        assert_eq!(fts_reads(plan), 1, "one FTS read per statement: {plan:?}");
        assert!(!per_frame(plan), "no per-frame FTS lookup: {plan:?}");
        assert!(
            plan.iter().any(|row| row.contains("MATERIALIZE")),
            "the hit set is materialized once: {plan:?}"
        );
        // V58: frames are filtered off the covering timeline index, never their own rows (a frame
        // row carries its JPEG in overflow pages).
        assert!(
            plan.iter().any(|row| row.contains("COVERING INDEX idx_screen_frames_timeline")),
            "frames are filtered off the covering index: {plan:?}"
        );
    }
}
