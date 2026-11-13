//! Issue #800: every shipped release's database must upgrade through the real product path, and a
//! database whose migration history disagrees with this binary must fail closed without a write.
//!
//! Shipped release heads (`git ls-tree <tag> crates/shogun-memory/src/migrations/`):
//! v0.1.0→V20, v0.2.0→V41, v0.2.1→V43, v0.2.2→V45, v0.2.3/v0.2.4/v0.2.5→V50, v0.2.6→V55,
//! v0.2.7→V67, v0.2.8→V70, v0.2.9→V77, v0.2.10→V79, v0.2.11/v0.2.12→V88. V20 predates `SEED`'s
//! tables; it and every release after these are covered, with no new line, by
//! `every_version_upgrades_atomically_to_latest`. Every file at or below
//! those heads is byte-identical on main (the tags only differ by additions), so a fresh database
//! migrated to `Target::Version(head)` from this tree carries exactly the refinery history
//! (version, name, checksum) a user of that release has on disk.
//!
//! The upgrade path mirrors `apps/desktop/src-tauri/src/lib.rs` for an existing store:
//! `open_encrypted_existing_for_upgrade` → (pre-migration snapshot) → `migrate_and_check_existing`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use refinery::error::Kind;
use rusqlite::Connection;
use shogun_memory::{
    migrate_and_check_existing, open_encrypted_existing_for_upgrade, schema_version, verify_history,
    DbKey, MemoryError, LATEST_SCHEMA_VERSION,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Same directory the crate embeds, so versions, names and checksums match the product's runner.
mod embedded {
    refinery::embed_migrations!("src/migrations");
}

fn key() -> DbKey {
    DbKey::new([0x5a; 32])
}

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("shogun_upgrade_lineage_{label}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The upgrade open uses SQLITE_OPEN_NOFOLLOW, which SQLite applies to every path
        // component: macOS's temp dir sits under the `/var` → `/private/var` symlink.
        Self(dir.canonicalize().unwrap())
    }

    fn path(&self) -> PathBuf {
        self.0.join("memory.db")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A keyed connection with no migrations and no pragmas beyond the key.
fn open_keyed(path: &Path) -> rusqlite::Result<Connection> {
    shogun_memory::vector::register_extension();
    let conn = Connection::open(path)?;
    conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", key().to_hex()))?;
    Ok(conn)
}

/// Create an encrypted store the way `open_encrypted` does, but stop at `version` — the state a
/// user of the release shipping that head has on disk.
fn create_encrypted_at(path: &Path, version: u32) -> TestResult {
    let mut conn = open_keyed(path)?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    embedded::migrations::runner()
        .set_target(refinery::Target::Version(version))
        .run(&mut conn)?;
    assert_eq!(schema_version(&conn)?, Some(version));
    Ok(())
}

/// The desktop's existing-store upgrade path.
fn product_upgrade(path: &Path) -> Result<Connection, MemoryError> {
    let mut conn = open_encrypted_existing_for_upgrade(path, &key())?;
    migrate_and_check_existing(&mut conn)?;
    Ok(conn)
}

const EVENT_ID: i64 = 1001;

/// Raw SQL valid at every shipped head (V41+): the V1 event/state/provenance columns and the V29
/// chat tables. Inserting into `event_log` fires the FTS trigger of that version.
const SEED: &str = "
    INSERT INTO event_log (id, ts, source, kind, app_bundle_id, window_title, content,
                           content_hash, last_seen_at, dwell_ms)
    VALUES (1001, 1760000000000, 'capture', 'text', 'com.apple.Notes', 'Budget review',
            'Kagemusha quarterly forecast draft for Aiko', 'lineage-hash-1', 1760000000500, 4200);
    INSERT INTO people (id, display_name, emails, relationship_summary, last_interaction_at,
                        confidence, created_at, updated_at, last_evidence_at)
    VALUES (2001, 'Aiko Tanaka', '[\"aiko@example.com\"]', 'Finance partner', 1760000000000,
            0.8, 1760000000000, 1760000000000, 1760000000000);
    INSERT INTO commitments (id, direction, counterparty_id, description, due_at, status,
                             confidence, created_at, updated_at, last_evidence_at)
    VALUES (3001, 'mine', 2001, 'Send the quarterly forecast to Aiko', 1760600000000, 'open',
            0.7, 1760000000000, 1760000000000, 1760000000000);
    INSERT INTO state_provenance (state_table, state_id, event_id, weight)
    VALUES ('people', 2001, 1001, 1.0), ('commitments', 3001, 1001, 0.9);
    INSERT INTO chat_threads (id, created_at_ms, updated_at_ms)
    VALUES (4001, 1760000001000, 1760000002000);
    INSERT INTO chat_messages (thread_id, pos, role, text, citations, ts)
    VALUES (4001, 0, 'me', 'What did Aiko ask for?', NULL, 1760000001000),
           (4001, 1, 'shogun', 'The quarterly forecast draft.', '[1001]', 1760000002000);
    UPDATE chat_state SET active_thread_id = 4001 WHERE id = 1;
";

/// The seeded rows as JSON, column by column, so "identical content" is a string compare.
fn seeded_rows(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let queries = [
        "SELECT json_array(id, ts, source, kind, app_bundle_id, window_title, content, content_hash,
                last_seen_at, dwell_ms) FROM event_log ORDER BY id",
        "SELECT json_array(id, display_name, emails, relationship_summary, last_interaction_at,
                confidence, created_at, updated_at, last_evidence_at) FROM people ORDER BY id",
        "SELECT json_array(id, direction, counterparty_id, description, due_at, status, confidence,
                created_at, updated_at, last_evidence_at) FROM commitments ORDER BY id",
        "SELECT json_array(state_table, state_id, event_id, weight) FROM state_provenance
         ORDER BY state_table, state_id",
        "SELECT json_array(id, created_at_ms, updated_at_ms) FROM chat_threads ORDER BY id",
        "SELECT json_array(thread_id, pos, role, text, citations, ts) FROM chat_messages
         ORDER BY thread_id, pos",
        "SELECT json_array(id, active_thread_id) FROM chat_state",
    ];
    let mut rows = Vec::new();
    for sql in queries {
        let mut stmt = conn.prepare(sql)?;
        rows.extend(stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?);
    }
    Ok(rows)
}

fn assert_shipped_upgrade(release: &str, head: u32) -> TestResult {
    let tmp = TempDb::new(&format!("shipped_v{head}"));
    let path = tmp.path();
    create_encrypted_at(&path, head)?;
    let before = {
        let conn = open_keyed(&path)?;
        conn.execute_batch(SEED)?;
        seeded_rows(&conn)?
    };
    assert_eq!(before.len(), 9, "{release}: seed must land at V{head}");

    let conn = product_upgrade(&path)
        .unwrap_or_else(|e| panic!("{release} (V{head}) store fails the product upgrade: {e}"));
    assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION), "{release} (V{head})");
    for table in ["event_processing_receipts", "event_processing_failures", "memory_processing_runtime"] {
        let count: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        assert_eq!(count, 0, "{release}: no fabricated processing history in {table}");
    }
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    assert_eq!(check, "ok", "{release} (V{head}): quick_check after upgrade");
    assert_eq!(seeded_rows(&conn)?, before, "{release} (V{head}): user rows changed by upgrade");
    assert_eq!(
        shogun_memory::search::fts_search(&conn, "quarterly forecast", 10)?,
        vec![EVENT_ID],
        "{release} (V{head}): pre-upgrade event not findable through search"
    );
    drop(conn);

    // A second launch on the upgraded store is a no-op, not a failure.
    let conn = product_upgrade(&path)
        .unwrap_or_else(|e| panic!("{release} (V{head}): relaunch after upgrade fails: {e}"));
    assert_eq!(seeded_rows(&conn)?, before, "{release} (V{head}): relaunch changed rows");
    Ok(())
}

#[test]
fn upgrade_from_release_v0_2_0_schema_v41() -> TestResult {
    assert_shipped_upgrade("v0.2.0", 41)
}

#[test]
fn v106_upgrade_preserves_sources_without_fabricating_processing_receipts() -> TestResult {
    assert_shipped_upgrade("pre-processing-receipts", 106)
}

#[test]
fn v105_suppressions_preserve_legacy_cutoffs_across_upgrade_reopen_and_snapshot_restore() -> TestResult {
    use shogun_memory::{
        actor::{Actor, WriteContext},
        state::StateTable,
        state_edit::{create_state_as, delete_state_as, is_suppressed_in_namespace, suppression_digest, CreateRequest},
    };

    fn legacy_rows(conn: &Connection) -> rusqlite::Result<Vec<String>> {
        conn.prepare("SELECT json_array(id, state_table, match_digest, reason, suppressed_at)
            FROM state_suppressions ORDER BY id")?
            .query_map([], |r| r.get(0))?.collect()
    }

    let tmp = TempDb::new("v105_suppression_namespaces");
    let snapshot = tmp.path();
    create_encrypted_at(&snapshot, 105)?;
    let tables = [StateTable::People, StateTable::Projects, StateTable::Commitments, StateTable::OpenLoops];
    let before = {
        let conn = open_keyed(&snapshot)?;
        for table in tables {
            conn.execute("INSERT INTO state_suppressions (state_table, match_digest, reason, suppressed_at)
                VALUES (?1, ?2, 'deleted', 1000)", rusqlite::params![table.as_str(), suppression_digest(table, "legacy belief")])?;
        }
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        legacy_rows(&conn)?
    };

    // Restore the same closed pre-upgrade synthetic snapshot twice. No history editing and no
    // inference of namespaces from digest-only legacy rows occurs on either upgrade.
    for attempt in 0..2 {
        let restored = tmp.0.join(format!("restored-{attempt}.db"));
        std::fs::copy(&snapshot, &restored)?;
        let mut conn = product_upgrade(&restored)?;
        assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION));
        assert_eq!(legacy_rows(&conn)?, before);
        assert_eq!(conn.query_row("SELECT count(*) FROM state_suppressions_scoped", [], |r| r.get::<_, i64>(0))?, 0);
        let alpha = WriteContext::scratch(Actor::agent("alpha")?)?;
        let req = CreateRequest {
            table: StateTable::Commitments, description: "legacy belief".into(),
            due_at: None, person_id: None,
        };
        let id = create_state_as(&mut conn, &req, 1500, &alpha).expect("create synthetic row").expect("synthetic row");
        assert!(delete_state_as(&mut conn, req.table, id, 2000, &WriteContext::owner()).expect("delete synthetic row"));
        assert_eq!(legacy_rows(&conn)?, before, "new deletions never alter old global cutoffs");
        drop(conn);

        for _ in 0..2 {
            let conn = product_upgrade(&restored)?;
            assert_eq!(legacy_rows(&conn)?, before);
            assert_eq!(conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))?, "ok");
            for table in tables {
                for namespace in ["world", "agent_scratch:alpha", "agent_scratch:beta"] {
                    assert!(is_suppressed_in_namespace(&conn, table, "legacy belief", 1000, namespace)?);
                    let scoped = table == StateTable::Commitments && namespace == "agent_scratch:alpha";
                    assert_eq!(is_suppressed_in_namespace(&conn, table, "legacy belief", 1001, namespace)?, scoped);
                    assert_eq!(is_suppressed_in_namespace(&conn, table, "legacy belief", 2000, namespace)?, scoped);
                    assert!(!is_suppressed_in_namespace(&conn, table, "legacy belief", 2001, namespace)?);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn upgrade_from_release_v0_2_1_schema_v43() -> TestResult {
    assert_shipped_upgrade("v0.2.1", 43)
}

#[test]
fn upgrade_from_release_v0_2_2_schema_v45() -> TestResult {
    assert_shipped_upgrade("v0.2.2", 45)
}

#[test]
fn upgrade_from_release_v0_2_3_to_v0_2_5_schema_v50() -> TestResult {
    assert_shipped_upgrade("v0.2.3/v0.2.4/v0.2.5", 50)
}

#[test]
fn upgrade_from_release_v0_2_6_schema_v55() -> TestResult {
    assert_shipped_upgrade("v0.2.6", 55)
}

#[test]
fn upgrade_from_release_v0_2_7_schema_v67() -> TestResult {
    assert_shipped_upgrade("v0.2.7", 67)
}

#[test]
fn upgrade_from_release_v0_2_8_schema_v70() -> TestResult {
    assert_shipped_upgrade("v0.2.8", 70)
}

#[test]
fn upgrade_from_release_v0_2_9_schema_v77() -> TestResult {
    assert_shipped_upgrade("v0.2.9", 77)
}

#[test]
fn upgrade_from_release_v0_2_10_schema_v79() -> TestResult {
    assert_shipped_upgrade("v0.2.10", 79)
}

#[test]
fn upgrade_from_release_v0_2_11_and_v0_2_12_schema_v88() -> TestResult {
    assert_shipped_upgrade("v0.2.11/v0.2.12", 88)
}

/// Ordinary tables only: virtual tables (FTS5, vec0) and their `<name>_*` shadow tables are
/// maintained by triggers and rebuilds, not by user rows.
fn user_tables(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name, sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         AND name <> 'refinery_schema_history' ORDER BY name",
    )?;
    let all = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let virtuals: Vec<&str> = all
        .iter()
        .filter(|(_, sql)| {
            sql.as_deref().is_some_and(|s| s.to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE"))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    Ok(all
        .iter()
        .filter(|(name, _)| {
            !virtuals.iter().any(|v| name == v || name.starts_with(&format!("{v}_")))
        })
        .map(|(name, _)| name.clone())
        .collect())
}

/// One row per ordinary table, whatever the schema version: every column gets a value of its
/// declared affinity, integer keys are 1 so foreign keys line up, and a row the schema rejects is
/// skipped. Run twice so a child row lands once its parent exists.
// ponytail: one generic value per type, so tables guarded by CHECK (status IN (...)) stay empty;
// hand-written seeds per release (SEED) cover those for the shipped heads.
fn seed_every_table(conn: &Connection) -> rusqlite::Result<()> {
    for _ in 0..2 {
        for table in user_tables(conn)? {
            let mut stmt = conn.prepare("SELECT name, type FROM pragma_table_info(?1)")?;
            let columns = stmt
                .query_map([&table], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let names: Vec<String> = columns.iter().map(|(n, _)| format!("\"{n}\"")).collect();
            let values: Vec<&str> = columns
                .iter()
                .map(|(_, ty)| {
                    let ty = ty.to_ascii_uppercase();
                    if ty.contains("CHAR") || ty.contains("CLOB") || ty.contains("TEXT") {
                        "'1'"
                    } else if ty.contains("BLOB") {
                        "x'01'"
                    } else {
                        "1"
                    }
                })
                .collect();
            let _ = conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO \"{table}\" ({}) VALUES ({})",
                    names.join(", "),
                    values.join(", ")
                ),
                [],
            );
        }
    }
    Ok(())
}

fn row_counts(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    user_tables(conn)?
        .into_iter()
        .map(|table| {
            let n = conn.query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0))?;
            Ok((table, n))
        })
        .collect()
}

/// The guard for every update, past and future: a store at ANY schema version, holding rows, is
/// interrupted while applying the next migration (its history row fails to commit — what a crash,
/// force-quit or power cut at that instant leaves), must come back exactly as it was, and the next
/// launch must then upgrade it to the latest schema without losing a row.
///
/// refinery's default run commits a migration and its history row in separate transactions; there
/// the injected failure leaves the migration applied but unrecorded, the retry re-runs it, and
/// memory fails to open on every launch. `migrate_and_check` commits them together.
#[test]
fn every_version_upgrades_atomically_to_latest() -> TestResult {
    // The embedded versions in order. Every one has a successor here except the newest; a gap
    // in the numbering (a version taken on another branch and merged later) is stepped over.
    let mut versions: Vec<u32> = embedded::migrations::runner().get_migrations().iter().map(|m| m.version()).collect();
    versions.sort_unstable();
    assert_eq!(versions.last().copied(), Some(LATEST_SCHEMA_VERSION));
    for pair in versions.windows(2) {
        let (version, next) = (pair[0], pair[1]);
        let tmp = TempDb::new(&format!("atomic_v{version}"));
        let path = tmp.path();
        create_encrypted_at(&path, version)?;
        let counts_before = {
            let conn = open_keyed(&path)?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            seed_every_table(&conn)?;
            row_counts(&conn)?
        };
        let before = digest(&path)?;

        {
            let mut conn = open_encrypted_existing_for_upgrade(&path, &key())?;
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER simulated_crash BEFORE INSERT ON refinery_schema_history
                 WHEN NEW.version = {next} BEGIN SELECT RAISE(ABORT, 'simulated crash'); END;"
            ))?;
            assert!(
                migrate_and_check_existing(&mut conn).is_err(),
                "V{version}: the injected failure at V{next} did not surface"
            );
        }
        assert_eq!(
            digest(&path)?,
            before,
            "V{version}: V{next} left a change behind although its history row never committed"
        );

        let conn = product_upgrade(&path)
            .unwrap_or_else(|e| panic!("V{version}: upgrade after an interrupted V{next} fails: {e}"));
        assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION), "V{version}");
        let mut stmt = conn.prepare("SELECT \"table\" FROM pragma_foreign_key_check")?;
        let broken = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        assert!(broken.is_empty(), "V{version}: foreign keys broken by upgrade in {broken:?}");
        let counts_after = row_counts(&conn)?;
        for (table, n) in &counts_before {
            let after = counts_after.iter().find(|(t, _)| t == table).map(|(_, n)| *n);
            assert!(
                after.is_some_and(|after| after >= *n),
                "V{version}: {table} had {n} rows before the upgrade, {after:?} after"
            );
        }
        drop(stmt);
        drop(conn);
        product_upgrade(&path)
            .unwrap_or_else(|e| panic!("V{version}: relaunch after upgrade fails: {e}"));
    }
    Ok(())
}

/// A shipped V41 store whose upgrade crashed part-way (refinery commits per migration, so the
/// crash leaves it at V50) finishes on the next launch: LATEST, rows intact, quick_check ok.
#[test]
fn interrupted_upgrade_resumes_on_next_launch() -> TestResult {
    let tmp = TempDb::new("interrupted");
    let path = tmp.path();
    create_encrypted_at(&path, 41)?;
    let before = {
        let mut conn = open_keyed(&path)?;
        conn.execute_batch(SEED)?;
        let rows = seeded_rows(&conn)?;
        embedded::migrations::runner().set_target(refinery::Target::Version(50)).run(&mut conn)?;
        assert_eq!(schema_version(&conn)?, Some(50));
        rows
    };

    let conn = product_upgrade(&path)?;
    assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION));
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    assert_eq!(check, "ok");
    assert_eq!(seeded_rows(&conn)?, before);
    Ok(())
}

/// Everything a migration could touch: schema objects, refinery history, per-table row counts,
/// and the header pragmas `migrate_and_check_existing` sets before running refinery.
fn digest(path: &Path) -> rusqlite::Result<Vec<String>> {
    let conn = open_keyed(path)?;
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT json_array(type, name, tbl_name, sql) FROM sqlite_master ORDER BY type, name",
    )?;
    out.extend(stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?);
    let mut stmt = conn.prepare(
        "SELECT json_array(version, name, applied_on, checksum) FROM refinery_schema_history
         ORDER BY version",
    )?;
    out.extend(stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?);
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
    let tables = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    for table in tables {
        let n: i64 =
            conn.query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0))?;
        out.push(format!("{table}={n}"));
    }
    for pragma in ["user_version", "auto_vacuum", "journal_mode"] {
        let v: String =
            conn.query_row(&format!("SELECT CAST({pragma} AS TEXT) FROM pragma_{pragma}"), [], |r| {
                r.get(0)
            })?;
        out.push(format!("{pragma}={v}"));
    }
    Ok(out)
}

fn tamper(path: &Path, sql: &str) -> TestResult {
    open_keyed(path)?.execute_batch(sql)?;
    Ok(())
}

/// Attempt the product upgrade; it must fail, and the store must be exactly as it was.
fn assert_fails_closed(path: &Path) -> Result<MemoryError, Box<dyn std::error::Error>> {
    let before = digest(path)?;
    let err = match product_upgrade(path) {
        Ok(conn) => panic!("upgrade must refuse, got schema {:?}", schema_version(&conn)),
        Err(e) => e,
    };
    assert_eq!(digest(path)?, before, "a refused upgrade must not write: {err}");
    Ok(err)
}

/// A store migrated by a branch that numbered `brief_generation` as V54 (main has
/// `dream_batch_send_stats` there). Refinery compares (version, name, checksum) of every applied
/// row against the embedded set before applying anything.
#[test]
fn divergent_lineage_fails_closed_without_writing() -> TestResult {
    let tmp = TempDb::new("divergent");
    let path = tmp.path();
    create_encrypted_at(&path, 53)?;
    tamper(
        &path,
        "INSERT INTO refinery_schema_history (version, name, applied_on, checksum)
         VALUES (54, 'brief_generation', '2026-08-20T00:00:00Z', '1234567890')",
    )?;

    let err = assert_fails_closed(&path)?;
    match &err {
        MemoryError::Migration(e) => match e.kind() {
            Kind::DivergentVersion(applied, embedded) => {
                assert_eq!((applied.version(), applied.name()), (54, "brief_generation"));
                assert_eq!((embedded.version(), embedded.name()), (54, "dream_batch_send_stats"));
            }
            other => panic!("expected DivergentVersion, got {other}"),
        },
        other => panic!("expected a refinery error, got {other}"),
    }
    Ok(())
}

/// An already-applied migration whose recorded checksum no longer matches the embedded file —
/// what every user store would look like if a shipped `V*.sql` were edited in place. This is why
/// CI must reject edits to shipped migrations: the next release would refuse to open them all.
#[test]
fn edited_shipped_migration_checksum_fails_closed_without_writing() -> TestResult {
    let tmp = TempDb::new("edited_checksum");
    let path = tmp.path();
    create_encrypted_at(&path, LATEST_SCHEMA_VERSION)?;
    tamper(&path, "UPDATE refinery_schema_history SET checksum = '1' WHERE version = 41")?;

    let err = assert_fails_closed(&path)?;
    match &err {
        MemoryError::Migration(e) => {
            assert!(
                matches!(e.kind(), Kind::DivergentVersion(applied, _) if applied.version() == 41),
                "expected DivergentVersion(41), got {}",
                e.kind()
            );
        }
        other => panic!("expected a refinery error, got {other}"),
    }
    Ok(())
}

/// A NULL checksum (row truncated/hand-edited) cannot be read as TEXT: an error, no write.
#[test]
fn missing_checksum_fails_closed_without_writing() -> TestResult {
    let tmp = TempDb::new("missing_checksum");
    let path = tmp.path();
    create_encrypted_at(&path, LATEST_SCHEMA_VERSION)?;
    tamper(&path, "UPDATE refinery_schema_history SET checksum = NULL WHERE version = 41")?;

    let err = assert_fails_closed(&path)?;
    assert!(matches!(err, MemoryError::Migration(_)), "expected a refinery error, got {err}");
    Ok(())
}

/// refinery 0.8.16's rusqlite driver `.expect()`s the stored checksum as a u64 and `.unwrap()`s
/// `applied_on` as RFC 3339, so a hand-edited history row panics inside refinery.
/// `migrate_and_check` catches that and returns `MigrationHistoryUnreadable`, with nothing written
/// and no stored value in the message.
#[test]
fn malformed_history_row_is_unreadable_error_without_writing() -> TestResult {
    for (label, sql, secret) in [
        (
            "garbage_checksum",
            "UPDATE refinery_schema_history SET checksum = 'secret-checksum' WHERE version = 41",
            "secret-checksum",
        ),
        (
            "garbage_applied_on",
            "UPDATE refinery_schema_history SET applied_on = 'secret-date' WHERE version = 41",
            "secret-date",
        ),
    ] {
        let tmp = TempDb::new(label);
        let path = tmp.path();
        create_encrypted_at(&path, LATEST_SCHEMA_VERSION)?;
        tamper(&path, sql)?;

        let err = assert_fails_closed(&path)?;
        assert!(
            matches!(err, MemoryError::MigrationHistoryUnreadable(_)),
            "{label}: expected MigrationHistoryUnreadable, got {err}"
        );
        assert!(!err.to_string().contains(secret), "{label}: stored value leaked: {err}");
    }
    Ok(())
}

/// A store last opened by a NEWER binary (history has a version this binary does not embed).
/// Refinery aborts with `MissingVersion` (abort_missing defaults to true) before any write, so an
/// older binary never runs against a schema it does not know. Consequence for downgrades: going
/// back a release locks the user out of the live store (desktop: `UpdateFailed`) until they restore
/// a snapshot taken by that older schema. The desktop also takes a pre-migration snapshot first,
/// because `schema_version != LATEST_SCHEMA_VERSION` counts as "backup required".
#[test]
fn store_from_newer_binary_fails_closed_without_writing() -> TestResult {
    let tmp = TempDb::new("newer_binary");
    let path = tmp.path();
    create_encrypted_at(&path, LATEST_SCHEMA_VERSION)?;
    tamper(
        &path,
        &format!(
            "INSERT INTO refinery_schema_history (version, name, applied_on, checksum)
             VALUES ({}, 'future_feature', '2026-10-01T00:00:00Z', '42')",
            LATEST_SCHEMA_VERSION + 1
        ),
    )?;

    let err = assert_fails_closed(&path)?;
    match &err {
        MemoryError::Migration(e) => {
            assert!(
                matches!(e.kind(), Kind::MissingVersion(m) if m.version() == LATEST_SCHEMA_VERSION + 1),
                "expected MissingVersion(LATEST+1), got {}",
                e.kind()
            );
        }
        other => panic!("expected a refinery error, got {other}"),
    }
    Ok(())
}

// verify_history (headless gate)

#[test]
fn verify_history_accepts_a_clean_store() -> TestResult {
    let tmp = TempDb::new("verify_clean");
    let path = tmp.path();
    create_encrypted_at(&path, LATEST_SCHEMA_VERSION)?;
    verify_history(&mut open_keyed(&path)?)?;
    Ok(())
}

/// Every history the desktop's refinery run refuses, the read-only check refuses too — without a
/// write and without echoing a stored value.
#[test]
fn verify_history_refuses_what_refinery_refuses_without_writing() -> TestResult {
    let mismatch = |at: u32| move |e: &MemoryError| matches!(e, MemoryError::HistoryMismatch(v) if *v == at);
    let unreadable = |e: &MemoryError| matches!(e, MemoryError::MigrationHistoryUnreadable(_));
    let refinery = |e: &MemoryError| matches!(e, MemoryError::Migration(_));
    type Expected<'a> = &'a dyn Fn(&MemoryError) -> bool;
    let cases: [(&str, &str, Expected); 6] = [
        (
            "divergent_name",
            "UPDATE refinery_schema_history SET name = 'brief_generation' WHERE version = 54",
            &mismatch(54),
        ),
        (
            "divergent_checksum",
            "UPDATE refinery_schema_history SET checksum = '1' WHERE version = 41",
            &mismatch(41),
        ),
        ("gap", "DELETE FROM refinery_schema_history WHERE version = 51", &mismatch(51)),
        (
            "non_numeric_checksum",
            "UPDATE refinery_schema_history SET checksum = 'secret-checksum' WHERE version = 41",
            &unreadable,
        ),
        (
            "malformed_applied_on",
            "UPDATE refinery_schema_history SET applied_on = 'secret-date' WHERE version = 41",
            &unreadable,
        ),
        (
            "null_checksum",
            "UPDATE refinery_schema_history SET checksum = NULL WHERE version = 41",
            &refinery,
        ),
    ];
    for (label, sql, expected) in cases {
        let tmp = TempDb::new(&format!("verify_{label}"));
        let path = tmp.path();
        create_encrypted_at(&path, LATEST_SCHEMA_VERSION)?;
        tamper(&path, sql)?;
        let before = digest(&path)?;

        let err = verify_history(&mut open_keyed(&path)?).expect_err(label);
        assert!(expected(&err), "{label}: {err:?}");
        assert!(!err.to_string().contains("secret"), "{label}: stored value leaked: {err}");
        assert!(!err.to_string().contains("brief_generation"), "{label}: stored value leaked: {err}");
        assert_eq!(digest(&path)?, before, "{label}: verify_history wrote");
        // The desktop's real run agrees: refused.
        assert_fails_closed(&path)?;
    }
    Ok(())
}
