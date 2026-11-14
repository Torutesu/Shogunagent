//! V57 (#714): FTS5 `pgsz` 3960, so index leaves fit in one SQLCipher page.
//!
//! What has to hold after the migration:
//! - every FTS5 table carries the setting, including after a plaintext → encrypted conversion;
//! - leaves written under the shipped cipher layout never spill into overflow pages;
//! - search is unchanged. Match sets and bm25 order are the same whether the leaves were written at
//!   FTS5's default 4050, at 3960, or as a mix of both (a database upgraded mid-life), and after
//!   old leaves are merged into new-size ones.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::search::{fts_query, fts_search};
use crate::{
    apply_key, apply_pragmas, embedded, encrypt_existing, open, open_encrypted, open_in_memory,
    schema_version, vector, DbKey, LATEST_SCHEMA_VERSION,
};

const PGSZ: i64 = 3960;
pub(crate) const PRE_V57: u32 = 56;
/// FTS5 `%_data` rowids put the segment id above 37 bits of dlidx/height/page number
/// (`fts5_dri` in fts5_index.c); the averages (1) and structure (10) records sit below it.
const SEGMENT_ID_SHIFT: u32 = 37;

pub(crate) fn key() -> DbKey {
    DbKey::new([0x5a; 32])
}

pub(crate) fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shogun_fts_pgsz_{}_{name}.db", std::process::id()))
}

pub(crate) fn remove(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// An encrypted database opened the way [`open_encrypted`] opens one, migrated only to `version`.
pub(crate) fn open_encrypted_at(path: &Path, version: u32) -> Connection {
    vector::register_extension();
    let mut conn = Connection::open(path).unwrap();
    apply_key(&conn, &key()).unwrap();
    apply_pragmas(&conn, true).unwrap();
    embedded::runner()
        .set_target(refinery::Target::Version(
            refinery::SchemaVersion::try_from(version).expect("test schema version fits refinery's"),
        ))
        .run(&mut conn)
        .unwrap();
    conn
}

fn fts5_tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
              WHERE type = 'table' AND lower(sql) LIKE 'create virtual table%using fts5%'
              ORDER BY name",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn pgsz(conn: &Connection, table: &str) -> Option<i64> {
    conn.query_row(&format!("SELECT v FROM \"{table}_config\" WHERE k = 'pgsz'"), [], |r| r.get(0))
        .ok()
}

pub(crate) fn overflow_pages(conn: &Connection, table: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM dbstat WHERE name = ?1 AND pagetype = 'overflow'",
        [table],
        |r| r.get(0),
    )
    .unwrap()
}

fn leaf_pages(conn: &Connection, table: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM dbstat WHERE name = ?1 AND pagetype = 'leaf'",
        [table],
        |r| r.get(0),
    )
    .unwrap()
}

pub(crate) fn segments(conn: &Connection) -> i64 {
    conn.query_row(
        &format!(
            "SELECT count(DISTINCT id >> {SEGMENT_ID_SHIFT}) FROM event_fts_data
              WHERE id >= (1 << {SEGMENT_ID_SHIFT})"
        ),
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// Deterministic synthetic capture text: English words in mixed case, hex-like identifiers (many
/// distinct trigrams, so leaves fill up), Japanese runs, and planted needles for the queries.
pub(crate) struct Row {
    ts: i64,
    title: String,
    content: String,
}

pub(crate) fn corpus(rows: usize) -> Vec<Row> {
    const EN: &[&str] = &[
        "quarterly", "revenue", "forecast", "invoice", "deploy", "release", "migration",
        "customer", "renewal", "pricing", "dashboard", "latency", "incident", "roadmap",
        "database", "onboarding", "contract", "schedule", "review", "phase", "kubernetes",
    ];
    const JA: &[char] = &[
        '請', '求', '書', '議', '事', '録', '共', '有', '会', '資', '料', '確', '認', '予', '算',
        '契', '約', 'の', 'を', 'に', 'は', 'が', 'で', 'て', 'す', 'ま', 'し', 'た', 'ア', 'イ',
        'カ', 'キ', 'ク', 'サ', 'シ', 'ス', 'テ', 'ト', 'ナ', 'ホ', 'マ', 'ミ', 'メ', 'ラ', 'リ',
    ];
    const HEX: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut state: u64 = 0x714_3960;
    let mut next = move |n: usize| -> usize {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    (0..rows)
        .map(|i| {
            let mut words: Vec<String> = Vec::with_capacity(130);
            for _ in 0..120 {
                match next(20) {
                    0..=10 => {
                        let w = EN[next(EN.len())];
                        words.push(match next(5) {
                            0 => w.to_uppercase(),
                            1 => format!("{}{}", w[..1].to_uppercase(), &w[1..]),
                            _ => w.to_string(),
                        });
                    }
                    11..=15 => {
                        let len = 5 + next(5);
                        words.push((0..len).map(|_| HEX[next(HEX.len())] as char).collect());
                    }
                    _ => {
                        let len = 3 + next(6);
                        words.push((0..len).map(|_| JA[next(JA.len())]).collect());
                    }
                }
            }
            if i % 7 == 0 {
                words.push("請求書の確認".into());
            }
            if i % 11 == 0 {
                words.push("Quarterly revenue forecast".into());
            }
            if i % 13 == 0 {
                words.push("KUBERNETES rollout".into());
            }
            if i % 19 == 0 {
                words.push("議事録を共有しました".into());
            }
            Row {
                ts: 1_000 + i as i64,
                title: format!("{} — {}", EN[i % EN.len()], JA[i % JA.len()]),
                content: words.join(" "),
            }
        })
        .collect()
}

/// Insert through `event_log` (the capture path: the V1/V41 triggers feed `event_fts`), a small
/// batch per transaction so FTS5 flushes many level-0 segments and automerges them.
pub(crate) fn insert_events(conn: &Connection, rows: &[Row]) {
    for batch in rows.chunks(15) {
        let tx = conn.unchecked_transaction().unwrap();
        for row in batch {
            tx.execute(
                "INSERT INTO event_log
                   (ts, source, kind, window_title, content, content_hash, last_seen_at)
                 VALUES (?1, 'capture', 'text', ?2, ?3, ?4, ?1)",
                params![row.ts, row.title, row.content, format!("pgsz-{}", row.ts)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
}

/// An OCR re-scan rewrites some rows (index delete + insert) and expiry deletes others.
pub(crate) fn churn(conn: &Connection) {
    conn.execute("UPDATE event_log SET content = content || ' rescan ' || id WHERE id % 23 = 0", [])
        .unwrap();
    conn.execute("DELETE FROM event_log WHERE id % 17 = 0", []).unwrap();
}

/// Representative MATCH expressions: quoted trigram phrases (one across a space), the OR-of-terms
/// shape the product builds, a CJK 3-character term, case variants, and a frequent trigram that
/// makes bm25 order many rows.
pub(crate) fn match_queries() -> Vec<String> {
    let mut queries: Vec<String> = [
        "\"revenue forecast\"",
        "\"kubernetes\"",
        "\"KUBERNETES\"",
        "\"請求書\"",
        "\"ase\"",
        "\"deploy\" OR \"migration\"",
    ]
    .iter()
    .map(|q| q.to_string())
    .collect();
    for natural in NATURAL_QUERIES {
        queries.push(fts_query(natural).unwrap());
    }
    queries
}

pub(crate) const NATURAL_QUERIES: &[&str] =
    &["quarterly revenue forecast", "議事録を共有", "How did the pricing renewal go?"];

pub(crate) fn ranked(conn: &Connection, query: &str) -> Vec<(i64, f64)> {
    let mut stmt = conn
        .prepare(
            "SELECT rowid, bm25(event_fts) FROM event_fts WHERE event_fts MATCH ?1
              ORDER BY rank, rowid",
        )
        .unwrap();
    let rows = stmt.query_map([query], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn assert_same_results(baseline: &Connection, candidate: &Connection, stage: &str) {
    for query in match_queries() {
        let expected = ranked(baseline, &query);
        assert!(!expected.is_empty(), "{query} must match something, or the check is vacuous");
        assert_eq!(expected, ranked(candidate, &query), "{stage}: {query}");
    }
    for natural in NATURAL_QUERIES {
        assert_eq!(
            fts_search(baseline, natural, 20).unwrap(),
            fts_search(candidate, natural, 20).unwrap(),
            "{stage}: product retrieval for {natural}"
        );
    }
}

#[test]
fn every_fts5_table_gets_pgsz_3960() {
    let conn = open_in_memory().unwrap();
    let tables = fts5_tables(&conn);
    assert_eq!(
        tables,
        ["event_fts", "event_grams_fts", "knowledge_entities_fts", "search_documents_fts", "user_memory_notes_fts"]
    );
    for table in &tables {
        let got = pgsz(&conn, table);
        assert_eq!(got, Some(PGSZ), "{table}: a new FTS5 table needs its own pgsz row");
    }

    // A pre-encryption install converted by `encrypt_existing` keeps the rows.
    let plain = temp("plain");
    let enc = temp("converted");
    remove(&plain);
    remove(&enc);
    drop(open(&plain).unwrap());
    encrypt_existing(&plain, &enc, &key()).unwrap();
    let conn = open_encrypted(&enc, &key()).unwrap();
    for table in &tables {
        assert_eq!(pgsz(&conn, table), Some(PGSZ), "{table} after sqlcipher_export");
    }
    drop(conn);
    remove(&plain);
    remove(&enc);
}

#[test]
fn upgrading_mid_life_keeps_match_sets_and_bm25_order() {
    let rows = corpus(900);
    let (first, second) = rows.split_at(450);
    let old_path = temp("never_upgraded");
    let new_path = temp("upgraded");
    remove(&old_path);
    remove(&new_path);

    // Baseline: never upgraded, every leaf at FTS5's default size.
    let old = open_encrypted_at(&old_path, PRE_V57);
    assert_eq!(pgsz(&old, "event_fts"), None, "pre-V57 uses the FTS5 default");
    insert_events(&old, &rows);
    churn(&old);

    // Upgraded mid-life: the first half is written at 4050, the product open path applies V57,
    // the second half and the churn are written at 3960.
    drop({
        let conn = open_encrypted_at(&new_path, PRE_V57);
        insert_events(&conn, first);
        conn
    });
    let new = open_encrypted(&new_path, &key()).unwrap();
    assert_eq!(schema_version(&new).unwrap(), Some(LATEST_SCHEMA_VERSION));
    for table in ["event_fts", "search_documents_fts", "knowledge_entities_fts"] {
        assert_eq!(pgsz(&new, table), Some(PGSZ), "{table} after the upgrade");
    }
    insert_events(&new, second);
    churn(&new);

    assert!(segments(&old) >= 2, "the baseline must span several segments");
    assert!(segments(&new) >= 2, "the upgraded index must span several segments");
    assert_eq!(ranked(&new, "\"kubernetes\""), ranked(&new, "\"KUBERNETES\""), "case-insensitive");
    assert!(ranked(&old, "\"ase\"").len() > 400, "the frequent trigram orders many rows");
    assert_same_results(&old, &new, "mixed 4050/3960 leaves");

    // Merge everything into one segment: every old leaf is rewritten at the new size.
    for _ in 0..10_000 {
        if segments(&new) <= 1 {
            break;
        }
        new.execute("INSERT INTO event_fts(event_fts, rank) VALUES ('merge', -16)", []).unwrap();
    }
    assert_eq!(segments(&new), 1, "a bounded negative merge loop reaches one segment");
    assert_same_results(&old, &new, "after merging old leaves");
    assert_eq!(overflow_pages(&new, "event_fts_data"), 0, "merged leaves fit in a page");

    drop(old);
    drop(new);
    remove(&old_path);
    remove(&new_path);
}

#[test]
fn new_leaves_fit_in_a_sqlcipher_page() {
    let rows = corpus(600);
    let path = temp("fresh");
    let control_path = temp("control");
    remove(&path);
    remove(&control_path);

    let conn = open_encrypted(&path, &key()).unwrap();
    insert_events(&conn, &rows);
    // The other two tables are external-content indexes too; fill them directly — only their
    // leaf layout is under test here.
    let tx = conn.unchecked_transaction().unwrap();
    for (i, row) in rows.iter().enumerate() {
        tx.execute(
            "INSERT INTO search_documents_fts (rowid, title, body, document_kind)
             VALUES (?1, ?2, ?3, 'thread')",
            params![i as i64 + 1, row.title, row.content],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO knowledge_entities_fts (rowid, name, context) VALUES (?1, ?2, ?3)",
            params![i as i64 + 1, row.title, row.content],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    for table in ["event_fts_data", "search_documents_fts_data", "knowledge_entities_fts_data"] {
        assert!(leaf_pages(&conn, table) > 100, "{table}: enough leaves to fill pages");
        let spilled = overflow_pages(&conn, table);
        assert_eq!(spilled, 0, "{table}: a leaf spilled into an overflow page");
    }
    // FTS5 can end a leaf a few bytes past `pgsz` (the rowid / poslist varints it appends after
    // its "page full" check). What matters is the SQLCipher bound: usable 4096 - 80 = 4016 bytes,
    // max local cell payload 4016 - 35 = 3981, minus a 4-byte record header for (NULL, blob).
    const MAX_LOCAL_BLOB: i64 = 3977;
    let largest: i64 = conn
        .query_row("SELECT max(length(block)) FROM event_fts_data", [], |r| r.get(0))
        .unwrap();
    assert!(largest > PGSZ - 100, "leaves must be full for this check to mean anything: {largest}");
    assert!(largest <= MAX_LOCAL_BLOB, "largest leaf {largest} cannot stay in its page");

    // Negative control: the same rows at the pre-V57 default do spill. If this stops holding, the
    // cipher layout changed (page size or reserve) and 3960 needs re-deriving.
    let control = open_encrypted_at(&control_path, PRE_V57);
    insert_events(&control, &rows);
    let spilled = overflow_pages(&control, "event_fts_data");
    assert!(spilled > 0, "4050-byte leaves overflow under SQLCipher");

    drop(conn);
    drop(control);
    remove(&path);
    remove(&control_path);
}
