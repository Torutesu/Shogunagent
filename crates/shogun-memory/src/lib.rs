//! SHOGUN memory: SQLite schema, versioned migrations, the event log, 3-tier memory, state
//! tables, and hybrid search.
//!
//! The durable substrate is a WAL-mode connection with crash-safe pragmas (NFR-REL-01),
//! refinery-managed versioned migrations (FR-MEM-30 — no hand-written ALTER TABLE in app code),
//! and a startup integrity check. The schema is in `src/migrations/*.sql`; it is the source of
//! truth (FR-MEM-11 says "the migration is authoritative").
//!
//! Data centre of gravity is Rust (AR-04): the webview never opens this database.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;

use rusqlite::Connection;

pub mod about_profile;
pub mod acl;
pub mod approval_ledger;
pub mod payment_mandates;
pub mod actor;
pub mod audit;
pub mod ai_session;
pub mod activity;
pub mod backup;
pub mod batch_send_stats;
pub mod briefs;
pub mod calendar;
pub mod chat_thread;
pub mod clarify_questions;
pub mod cold;
pub mod compression_metrics;
pub mod disputes;
pub mod document_vector;
pub mod dream_cycles;
pub mod due;
pub mod embed;
pub mod embed_job;
pub mod embedding_space;
#[cfg(feature = "onnx")]
pub mod embed_onnx;
pub mod entity_dedupe;
pub mod entity_names;
pub mod entity_pages;
pub mod event_log;
pub mod export;
pub mod extract;
pub mod fts_compact;
pub mod fusion_supply;
pub mod knowledge;
pub mod memory_identity;
pub mod hot;
pub mod identity;
pub mod jobs;
pub mod lessons;
pub mod llm_usage;
pub mod mail_triage;
pub mod maintenance;
pub mod meeting_index;
pub mod meeting_recaps;
pub mod meeting_recovery;
pub mod memory_import;
pub mod memory_notes;
pub mod period_summaries;
pub mod quantize;
pub mod redact;
pub mod recap_track;
pub mod recompute;
pub mod retention;
pub mod people_quality;
pub mod sanitize;
pub mod scratch_files;
pub mod screen_frames;
pub mod search;
pub mod search_documents;
pub mod self_identity;
pub mod storage_preview;
pub mod scheduled_jobs;
pub mod session;
pub mod skill_runs;
pub mod local_writes;
pub mod session_notes;
pub mod session_speakers;
pub mod shared_service_rtt;
pub mod state;
pub mod state_collapse;
pub mod loops;
pub mod loop_updates;
pub mod state_edit;
pub mod state_merge;
pub mod state_split;
pub mod thread;
pub mod transcript_segments;
pub mod translate_jobs;
pub mod ai_search_jobs;
pub mod traceability;
pub mod work_patterns;
pub mod vector;
pub mod voice_terms;
pub mod voice_terms_import;
pub mod rewrite_loop;
pub mod writing_voice;

/// refinery embeds the `src/migrations/V*.sql` files at compile time; `migrations::runner()`
/// applies any not yet recorded in the `refinery_schema_history` table.
mod embedded {
    refinery::embed_migrations!("src/migrations");
    pub use migrations::runner;
}

/// Errors from opening / migrating / checking the database.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration: {0}")]
    Migration(#[from] refinery::Error),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    /// A state row was asked to be inserted with no provenance (FR-ST-02 forbids it).
    #[error("state row insert requires at least one provenance event (FR-ST-02)")]
    EmptyProvenance,
    /// A write was asked for with no actor (E2-T4 forbids it; every row records its writer).
    #[error("write requires a non-empty actor (`<kind>:<id>`, e.g. `agent:<id>`)")]
    EmptyActor,
    /// An actor that is not `<kind>:<id>`, or carries whitespace / control characters, or is too
    /// long. Carries no value.
    #[error("actor must be `<kind>:<id>` without whitespace, at most 128 characters")]
    InvalidActor,
    /// refinery panicked reading `refinery_schema_history` (a checksum that is not a u64, an
    /// `applied_on` that is not RFC 3339). Nothing was migrated. Carries refinery's own panic
    /// message only, which names the field, never a stored value.
    #[error("migration history is unreadable; memory left unchanged: {0}")]
    MigrationHistoryUnreadable(String),
    /// [`verify_history`]: the recorded history disagrees with this build at this version (a
    /// divergent name/checksum, a version this build does not embed, or a gap below the newest
    /// applied one). Carries the version only, never a stored name or checksum.
    #[error("migration history does not match this build at V{0}; memory left unchanged")]
    HistoryMismatch(refinery::SchemaVersion),
}

/// Apply the crash-safety pragmas (NFR-REL-01). `wal` is skipped for in-memory databases,
/// where journal modes other than MEMORY are not meaningful.
fn apply_pragmas(conn: &Connection, wal: bool) -> Result<(), rusqlite::Error> {
    if wal {
        // Freed pages can be handed back a slice at a time (`maintenance::reclaim_space`) instead
        // of the file living at its high-water mark forever. Only takes effect on a database
        // created after this pragma — it must precede the first write, and setting the journal
        // mode counts as one — so it comes before WAL; an existing file keeps its mode until a
        // `VACUUM` converts it, which `Db::delete_all` performs.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        // WAL + NORMAL sync survives power loss without the full fsync cost (NFR-REL-01).
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // SQLCipher decrypts every page it reads, so cache misses are the cost of a lookup. The 2 MiB
    // default thrashed: repeating ten searches re-read all 5,403 pages; at 16 MiB the repeat read
    // none (600 MB store copy, docs/migrations/V87-rollback.md). Negative = KiB.
    // ponytail: fixed size; raise it if larger stores show misses on repeated lookups again.
    conn.pragma_update(None, "cache_size", -16_384)?;
    Ok(())
}

// `migrate_and_check` relies on unwinding; `cfg(panic)` is stable, so this fails the build early.
#[cfg(panic = "abort")]
compile_error!(
    "shogun-memory's migrate_and_check relies on unwinding to turn refinery's history panics into \
     an error (#800); with panic = \"abort\" a hand-edited history row would crash every launch"
);

/// Run `f` (a refinery call that reads the history), turning a refinery panic into
/// [`MemoryError::MigrationHistoryUnreadable`] carrying refinery's own message only.
fn catch_history_panic<T>(f: impl FnOnce() -> T) -> Result<T, MemoryError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        MemoryError::MigrationHistoryUnreadable(message)
    })
}

/// Run migrations to the latest version, then a quick integrity check (NFR-REL-01).
///
/// Each pending migration commits in one transaction together with its history row. refinery's
/// default (ungrouped) run commits a migration's SQL and its history `INSERT` as two separate
/// transactions, so a quit or crash between them left the schema changed but unrecorded: the next
/// launch re-ran a migration that cannot run twice and memory failed to open on every launch.
/// One run per version (`set_grouped` + `Target::Version`) keeps an interrupted upgrade resumable
/// from the last finished migration instead of redoing one giant transaction.
fn migrate_and_check(conn: &mut Connection) -> Result<(), MemoryError> {
    // ponytail: refinery 0.8.16's sqlite driver `.expect()`s the stored checksum and `.unwrap()`s
    // `applied_on`, so a hand-edited history row would crash the app on every launch; catch it here.
    let has_history: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master
                        WHERE type = 'table' AND name = 'refinery_schema_history')",
        [],
        |r| r.get(0),
    )?;
    let applied: Vec<refinery::SchemaVersion> = if has_history {
        catch_history_panic(|| embedded::runner().get_applied_migrations(conn))??
            .iter()
            .map(|m| m.version())
            .collect()
    } else {
        Vec::new()
    };
    let pending: Vec<refinery::SchemaVersion> = embedded::runner()
        .get_migrations()
        .iter()
        .map(|m| m.version())
        .filter(|v| !applied.contains(v))
        .collect();
    for version in pending {
        catch_history_panic(|| {
            embedded::runner()
                .set_grouped(true)
                .set_target(refinery::Target::Version(version))
                .run(conn)
        })??;
    }
    // Always one plain run: with nothing pending the loop never verifies the history, and a store
    // from a newer binary or with an edited checksum must still be refused here.
    catch_history_panic(|| embedded::runner().run(conn))??;
    let status: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if status != "ok" {
        return Err(MemoryError::Integrity(status));
    }
    Ok(())
}

/// Open (creating if needed) the database at `path` in WAL mode, apply pragmas, migrate to the
/// latest schema, and run an integrity check. This is the product entry point.
pub fn open(path: impl AsRef<Path>) -> Result<Connection, MemoryError> {
    vector::register_extension();
    let mut conn = Connection::open(path)?;
    apply_pragmas(&conn, true)?;
    migrate_and_check(&mut conn)?;
    Ok(conn)
}

/// A 256-bit database encryption key.
///
/// The key never appears in a `Debug` render, so it cannot reach a log through a derived
/// `{:?}` on some enclosing struct. It is passed in rather than read here: this crate is
/// Linux-testable and must not know about the Keychain (the key's only permitted home,
/// invariant 7) — the desktop layer reads it and hands it over.
#[derive(Clone)]
pub struct DbKey([u8; 32]);

impl DbKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parse the 64-char hex form used to store the key in the Keychain.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(out))
    }

    /// The hex form, for storing in the Keychain.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Run `f` over the raw key bytes without copying them out. For a derivation that needs the
    /// bytes themselves (the L3 approval sealing key): it leaves no hex `String` behind that the
    /// allocator would free unzeroed. The borrow ends with `f`, so the bytes never outlive this key.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8; 32]) -> R) -> R {
        f(&self.0)
    }

    /// The SQLCipher `PRAGMA key` argument. Raw-key form (`x'…'`) so SQLCipher uses these bytes
    /// directly instead of running a passphrase KDF, and so the value can never need quoting or
    /// escaping — it is hex digits between fixed delimiters.
    fn pragma_value(&self) -> String {
        format!("\"x'{}'\"", self.to_hex())
    }
}

impl std::fmt::Debug for DbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbKey(***redacted***)")
    }
}

/// Apply the encryption key. Must run before any other statement on the connection — SQLCipher
/// reads the header on first access, and a key set afterwards is rejected.
fn apply_key(conn: &Connection, key: &DbKey) -> Result<(), MemoryError> {
    conn.execute_batch(&format!("PRAGMA key = {};", key.pragma_value()))?;
    Ok(())
}

/// Open (creating if needed) an **encrypted** database at `path` (FR-SEC: memory at rest).
///
/// Same contract as [`open`] otherwise. A wrong key surfaces as an integrity error from the first
/// read, not as silent garbage: SQLCipher cannot decrypt the header and the migration runner's
/// first query fails.
pub fn open_encrypted(path: impl AsRef<Path>, key: &DbKey) -> Result<Connection, MemoryError> {
    vector::register_extension();
    let mut conn = Connection::open(path)?;
    apply_key(&conn, key)?;
    apply_pragmas(&conn, true)?;
    migrate_and_check(&mut conn)?;
    Ok(conn)
}

/// Open an existing encrypted database read-only, without following a final-component symlink,
/// creating files, changing journal mode, or running migrations.
///
/// Backup validation uses this mode so validation cannot mutate the immutable snapshot it proves.
pub fn open_encrypted_existing_read_only(
    path: impl AsRef<Path>,
    key: &DbKey,
) -> Result<Connection, MemoryError> {
    vector::register_extension();
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    apply_key(&conn, key)?;
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get::<_, i64>(0))?;
    Ok(conn)
}

/// Open an existing encrypted live database for schema inspection before upgrade.
///
/// This mode cannot create a missing file and does not apply pragmas or run migrations. Lifecycle
/// code must create and verify any required pre-upgrade snapshot before calling
/// [`migrate_and_check_existing`] on the returned connection.
pub fn open_encrypted_existing_for_upgrade(
    path: impl AsRef<Path>,
    key: &DbKey,
) -> Result<Connection, MemoryError> {
    vector::register_extension();
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    apply_key(&conn, key)?;
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get::<_, i64>(0))?;
    Ok(conn)
}

/// Apply live-store pragmas, run pending migrations, and check an existing keyed connection.
///
/// This is separate from [`open_encrypted_existing_for_upgrade`] so lifecycle code can create a
/// verified pre-migration snapshot before refinery executes its first write.
pub fn migrate_and_check_existing(conn: &mut Connection) -> Result<(), MemoryError> {
    apply_pragmas(conn, true)?;
    migrate_and_check(conn)
}

/// Apply live-store pragmas to an existing keyed connection **without** migrating it — for a
/// process that must never upgrade the store (the headless Memory API faces, #800). The caller
/// checks [`schema_version`] against [`LATEST_SCHEMA_VERSION`] first.
pub fn apply_live_pragmas(conn: &Connection) -> Result<(), MemoryError> {
    Ok(apply_pragmas(conn, true)?)
}

/// Convert an existing plaintext database into an encrypted one at `dest`, via SQLCipher's
/// `sqlcipher_export`. The source is left untouched, so a failure loses nothing and the caller
/// decides when to swap the files.
///
/// This is the upgrade path for installs created before encryption: a plaintext database cannot
/// simply be given a key, its pages have to be rewritten.
pub fn encrypt_existing(
    plaintext: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    key: &DbKey,
) -> Result<(), MemoryError> {
    vector::register_extension();
    let conn = Connection::open(plaintext)?;
    let dest = dest.as_ref().to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!(
        "ATTACH DATABASE '{dest}' AS encrypted KEY {};
         SELECT sqlcipher_export('encrypted');
         DETACH DATABASE encrypted;",
        key.pragma_value()
    ))?;
    Ok(())
}

/// Whether the file at `path` is an unencrypted SQLite database — i.e. still needs
/// [`encrypt_existing`]. A plaintext database starts with SQLite's magic header; an encrypted one
/// starts with random salt, so the check is a 16-byte read and never opens the database.
pub fn is_plaintext_db(path: impl AsRef<Path>) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; 16];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    &head == b"SQLite format 3\0"
}

/// A plaintext store at `path` migrated only up to `version`: the shape a user of an older build
/// has on disk. For tests and tooling that exercise the upgrade path (the principal import, E2-T6);
/// never a product entry point.
#[doc(hidden)]
pub fn open_plaintext_at_version(path: impl AsRef<Path>, version: refinery::SchemaVersion) -> Result<Connection, MemoryError> {
    vector::register_extension();
    let mut conn = Connection::open(path)?;
    apply_pragmas(&conn, true)?;
    embedded::runner()
        .set_target(refinery::Target::Version(version))
        .run(&mut conn)?;
    Ok(conn)
}

/// Open an in-memory database, migrated to the latest schema — for tests and ephemeral use.
///
/// The migrations run once per process, into a template that every call copies page for page
/// (SQLite's backup API). Test suites open about a thousand of these; running all migrations for
/// each one was most of the Rust CI time. The copy carries the schema, the seed rows and
/// `refinery_schema_history`; per-connection pragmas are applied to each copy.
pub fn open_in_memory() -> Result<Connection, MemoryError> {
    static TEMPLATE: std::sync::OnceLock<std::sync::Mutex<Connection>> = std::sync::OnceLock::new();
    vector::register_extension();
    let template = match TEMPLATE.get() {
        Some(template) => template,
        None => {
            let mut fresh = Connection::open_in_memory()?;
            apply_pragmas(&fresh, false)?;
            migrate_and_check(&mut fresh)?;
            // A racing thread may have set it first; either migrated copy is identical.
            TEMPLATE.get_or_init(|| std::sync::Mutex::new(fresh))
        }
    };
    let mut conn = Connection::open_in_memory()?;
    apply_pragmas(&conn, false)?;
    // Only read under the lock, so a poisoned guard still holds a whole template.
    let source = template.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    rusqlite::backup::Backup::new(&source, &mut conn)?.run_to_completion(
        i32::MAX,
        std::time::Duration::ZERO,
        None,
    )?;
    drop(source);
    Ok(conn)
}

/// The highest migration bundled in `src/migrations`. Tests assert against this rather than a
/// literal so that adding a migration updates one place, not five — and so a *drop* in version
/// (a migration file lost in a merge) still fails loudly.
pub const LATEST_SCHEMA_VERSION: u32 = 107;

/// The schema version the migrations bring the database to (max applied version), or `None`
/// if no migrations are recorded.
pub fn schema_version(conn: &Connection) -> Result<Option<u32>, rusqlite::Error> {
    conn.query_row(
        "SELECT MAX(version) FROM refinery_schema_history",
        [],
        |r| r.get::<_, Option<u32>>(0),
    )
}

/// Whether the recorded `refinery_schema_history` is exactly what this build's runner would accept
/// (#800), without running it. Reads the rows through refinery's own reader (one SELECT in a
/// deferred transaction, no writes), so a row refinery cannot parse — a non-u64 checksum, an
/// `applied_on` that is not RFC 3339, a NULL — fails here exactly as it does on the desktop. Then
/// every applied (version, name, checksum) must match an embedded migration, and no embedded
/// migration at or below the newest applied version may be missing. For a process that must not
/// migrate (the headless faces) yet must refuse a store the desktop's refinery run would refuse.
pub fn verify_history(conn: &mut Connection) -> Result<(), MemoryError> {
    let runner = embedded::runner();
    let applied = catch_history_panic(|| runner.get_applied_migrations(conn))??;
    let embedded = runner.get_migrations();
    if let Some(unknown) = applied.iter().find(|app| !embedded.contains(app)) {
        return Err(MemoryError::HistoryMismatch(unknown.version()));
    }
    let newest = applied.iter().map(|app| app.version()).max();
    match embedded.iter().map(|m| m.version()).find(|v| {
        newest.is_some_and(|newest| *v <= newest) && !applied.iter().any(|app| app.version() == *v)
    }) {
        Some(gap) => Err(MemoryError::HistoryMismatch(gap)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod fts_page_size_tests;

#[cfg(test)]
mod fts_compact_tests;

#[cfg(test)]
mod event_link_index_tests;
#[cfg(test)]
mod actor_tests;

/// Encryption at rest. These tests assert the property that matters — the file on disk is not
/// readable without the key — rather than just that the API returns Ok.
#[cfg(test)]
mod encryption_tests {
    use super::*;

    fn key(seed: u8) -> DbKey {
        DbKey::new([seed; 32])
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("shogun_enc_{}_{}.db", std::process::id(), name))
    }

    #[test]
    fn hex_round_trips_and_rejects_malformed_keys() {
        let k = key(0xab);
        assert_eq!(k.to_hex().len(), 64);
        assert_eq!(k.with_bytes(|bytes| *bytes), [0xab; 32]);
        assert_eq!(DbKey::from_hex(&k.to_hex()).unwrap().to_hex(), k.to_hex());
        assert!(DbKey::from_hex("too short").is_none());
        assert!(DbKey::from_hex(&"z".repeat(64)).is_none(), "non-hex rejected");
    }

    #[test]
    fn the_key_never_renders_in_debug_output() {
        let rendered = format!("{:?}", key(0xff));
        assert!(!rendered.contains("ff"), "key bytes must not reach a log: {rendered}");
        assert_eq!(rendered, "DbKey(***redacted***)");
    }

    #[test]
    fn an_encrypted_database_is_not_readable_without_the_key() {
        let path = temp("locked");
        let _ = std::fs::remove_file(&path);
        {
            let conn = open_encrypted(&path, &key(1)).expect("create encrypted");
            conn.execute(
                "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at, dwell_ms)
                 VALUES (1, 'capture', 'text', 'the quarterly numbers', 'h1', 1, 0)",
                [],
            )
            .unwrap();
        }

        // The bytes on disk must not be a plaintext SQLite file, and must not contain the content.
        assert!(!is_plaintext_db(&path), "an encrypted DB must not carry the SQLite header");
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(21).any(|w| w == b"the quarterly numbers"),
            "captured content must not be readable in the file"
        );

        // Wrong key: refused, not silently empty.
        assert!(open_encrypted(&path, &key(2)).is_err(), "a wrong key must fail loudly");
        // Right key: the row is there.
        let conn = open_encrypted(&path, &key(1)).expect("reopen with the right key");
        let n: i64 = conn.query_row("SELECT count(*) FROM event_log", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_existing_plaintext_database_can_be_migrated_without_losing_data() {
        let plain = temp("plain");
        let enc = temp("converted");
        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_file(&enc);
        {
            let conn = open(&plain).expect("create plaintext");
            conn.execute(
                "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at, dwell_ms)
                 VALUES (1, 'capture', 'text', 'pre-existing memory', 'h1', 1, 0)",
                [],
            )
            .unwrap();
        }
        assert!(is_plaintext_db(&plain), "the old format is detectable");

        encrypt_existing(&plain, &enc, &key(7)).expect("convert");

        // Converted copy: same data, now encrypted. The source is untouched, so a failed upgrade
        // never destroys the user's memory.
        assert!(!is_plaintext_db(&enc));
        assert!(is_plaintext_db(&plain), "source left intact");
        let conn = open_encrypted(&enc, &key(7)).expect("open converted");
        let content: String =
            conn.query_row("SELECT content FROM event_log", [], |r| r.get(0)).unwrap();
        assert_eq!(content, "pre-existing memory");
        assert_eq!(schema_version(&conn).unwrap(), Some(LATEST_SCHEMA_VERSION), "schema carried over");

        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_file(&enc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn migrations_create_the_v1_schema() {
        let conn = open_in_memory().unwrap();
        let tables = table_names(&conn);
        for expected in [
            "event_log",
            "people",
            "projects",
            "commitments",
            "open_loops",
            "state_provenance",
            "traceability_log",
            "feedback_events",
            "lessons",
            "lesson_provenance",
            "briefs",
            "search_documents",
            "search_document_provenance",
            "search_document_invalidations",
            "search_documents_fts",
            "dream_cycles",
            "dream_batch_items",
            "dream_event_arrivals",
            "dream_cycle_inputs",
            "memory_imports",
            "memory_import_items",
            "clarify_questions",
            "dream_batch_send_stats",
            "dictionary_autoadd_settings",
            "dictionary_autoadd_corrections",
        ] {
            assert!(tables.iter().any(|t| t == expected), "missing table {expected}");
        }
        assert_eq!(schema_version(&conn).unwrap(), Some(LATEST_SCHEMA_VERSION));
    }

    #[test]
    fn in_memory_copies_are_migrated_and_independent() {
        let a = open_in_memory().unwrap();
        let b = open_in_memory().unwrap();
        a.execute_batch("CREATE TABLE only_in_a (x INTEGER)").unwrap();
        let in_b: i64 = b
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'only_in_a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(in_b, 0, "a write to one copy must not reach the template or another copy");
        assert_eq!(schema_version(&b).unwrap(), Some(LATEST_SCHEMA_VERSION));
        let fk: i64 = b.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk, 1, "pragmas are set on every copy, not inherited");
        let check: String = b.query_row("PRAGMA quick_check", [], |r| r.get(0)).unwrap();
        assert_eq!(check, "ok");
    }

    #[test]
    fn migrations_are_idempotent_on_reopen() {
        // Apply to a temp file, close, reopen — the second run must be a no-op, not re-run.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shogun_mem_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let conn = open(&path).unwrap();
            assert_eq!(schema_version(&conn).unwrap(), Some(LATEST_SCHEMA_VERSION));
        }
        {
            // Reopen: migrate_and_check runs again, finds nothing new, and passes quick_check.
            let conn = open(&path).unwrap();
            assert_eq!(schema_version(&conn).unwrap(), Some(LATEST_SCHEMA_VERSION));
            let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
            assert_eq!(mode.to_lowercase(), "wal");
        }
        let _ = std::fs::remove_file(&path);
    }

    fn schema52() -> Result<Connection, Box<dyn std::error::Error>> {
        vector::register_extension();
        let mut conn = Connection::open_in_memory()?;
        apply_pragmas(&conn, false)?;
        embedded::runner()
            .set_target(refinery::Target::Version(52))
            .run(&mut conn)?;
        Ok(conn)
    }


    #[test]
    fn live_relay_migration_preserves_prior_ledger_fields() -> Result<(), Box<dyn std::error::Error>> {
        vector::register_extension();
        let mut conn = Connection::open_in_memory()?;
        apply_pragmas(&conn, false)?;
        embedded::runner().set_target(refinery::Target::Version(90)).run(&mut conn)?;
        conn.execute_batch("INSERT INTO traceability_log (id, ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party, kind, url)
            VALUES (123, 100, 'web', 'read', 'example.com', 4, 'digest', 0, 'egress', 'https://example.com/page');
            INSERT INTO llm_usage (id, ts, route, prompt_id, prompt_version, model, input_tokens, outcome)
            VALUES (456, 100, 'messages_api', 'inline_chat', 1, 'old-model', 42, 'ok');")?;
        migrate_and_check(&mut conn)?;
        let old: (String, String, String) = conn.query_row("SELECT chunk_xxh64,kind,url FROM traceability_log WHERE id=123", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(old, ("digest".into(), "egress".into(), "https://example.com/page".into()));
        assert_eq!(conn.query_row("SELECT input_tokens FROM llm_usage WHERE id=456", [], |r| r.get::<_,i64>(0))?, 42);
        conn.execute_batch("INSERT INTO traceability_log (ts,route,purpose,destination,chunk_bytes,chunk_xxh64) VALUES (101,'live_relay','chat','api.shogunaios.com',1,'hash');
            INSERT INTO llm_usage (ts,route,prompt_id,prompt_version,model,outcome) VALUES (101,'live_relay','inline_chat',1,'hosted/chat','ok');")?;
        assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION));
        Ok(())
    }

    #[test]
    fn canonical_schema52_upgrades_to_latest() -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = schema52()?;
        assert_eq!(schema_version(&conn)?, Some(52));
        migrate_and_check(&mut conn)?;
        assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION));
        Ok(())
    }

    #[test]
    fn canonical_schema63_upgrades_to_latest_without_changing_vocabulary() -> Result<(), Box<dyn std::error::Error>> {
        vector::register_extension();
        let mut conn = Connection::open_in_memory()?;
        apply_pragmas(&conn, false)?;
        embedded::runner().set_target(refinery::Target::Version(63)).run(&mut conn)?;
        conn.execute("INSERT INTO voice_terms (canonical, scope, enabled, provenance, created_at_ms, updated_at_ms) VALUES ('Existing', 'global', 1, 'user', 1, 1)", [])?;
        migrate_and_check(&mut conn)?;
        let (version, term, enabled): (u32, String, i64) = (
            schema_version(&conn)?.ok_or("missing schema version")?,
            conn.query_row("SELECT canonical FROM voice_terms", [], |r| r.get(0))?,
            conn.query_row("SELECT enabled FROM dictionary_autoadd_settings WHERE id = 1", [], |r| r.get(0))?,
        );
        assert_eq!((version, term.as_str(), enabled), (LATEST_SCHEMA_VERSION, "Existing", 0));
        Ok(())
    }

    #[test]
    fn schema64_backfills_each_owned_alias_without_claiming_manual_edits() -> Result<(), Box<dyn std::error::Error>> {
        vector::register_extension();
        let mut conn = Connection::open_in_memory()?;
        apply_pragmas(&conn, false)?;
        embedded::runner().set_target(refinery::Target::Version(64)).run(&mut conn)?;
        conn.execute("INSERT INTO voice_terms (id, canonical, scope, scope_ref, enabled, provenance, created_at_ms, updated_at_ms)
                      VALUES (1, 'John', 'surface', 'voice_dictation', 1, 'user', 100, 200),
                             (2, 'Jane', 'surface', 'voice_dictation', 1, 'user', 100, 300)", [])?;
        conn.execute("INSERT INTO voice_term_aliases (id, term_id, alias, alias_normalized)
                      VALUES (1, 1, 'John', 'john'), (2, 1, 'Jon', 'jon'), (3, 1, 'Jhon', 'jhon'),
                             (4, 2, 'Jane', 'jane'), (5, 2, 'Jan', 'jan')", [])?;
        conn.execute("INSERT INTO dictionary_autoadd_corrections
                      (id, original, corrected, observations, last_observation_id, state, created_at_ms,
                       term_id, alias_id, canonical_alias_id, term_updated_at_ms, created_term)
                      VALUES (1, 'Jon', 'John', 2, 1, 'added', 100, 1, 2, 1, 100, 1),
                             (2, 'Jhon', 'John', 2, 2, 'added', 200, 1, 3, NULL, 200, 0),
                             (3, 'Jan', 'Jane', 2, 3, 'added', 250, 2, 5, 4, 250, 1)", [])?;
        migrate_and_check(&mut conn)?;
        let mut stmt = conn.prepare("SELECT correction_id FROM dictionary_autoadd_artifacts ORDER BY correction_id")?;
        let ids = stmt.query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(ids, vec![1, 2]);
        Ok(())
    }

    #[test]
    fn v69_quote_upgrade_preserves_provenance_pins_and_paid_identities(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = schema52()?;
        embedded::runner()
            .set_target(refinery::Target::Version(68))
            .run(&mut conn)?;
        conn.execute_batch(r#"INSERT INTO event_log(id,ts,source,kind,content,content_hash,last_seen_at)
            VALUES(9101,150,'capture','text','Original retained source','quote-upgrade-source',150);
            INSERT INTO commitments(id,direction,description,status,confidence,base_confidence,created_at,updated_at,last_evidence_at,user_pinned,user_edited_at)
            VALUES(7,'mine','Corrected wording','open',1.0,1.0,100,170,150,1,170);
            INSERT INTO state_provenance(state_table,state_id,event_id,weight) VALUES('commitments',7,9101,0.7);
            INSERT INTO brief_generation(date,input_from_ts,input_to_ts,output_language,sources_json,state,provider_id,generation,created_at,updated_at,expected_custom_id,submitted_at)
            VALUES('in-flight',100,200,'en','[[9101,"source-digest"]]','processing','paid-provider',3,1,2,'paid-custom',2);"#)?;
        dream_cycles::freeze_full_cycle(&mut conn, "paid-cycle", 100, 200, 170)?;
        conn.execute_batch("UPDATE dream_cycles SET batch_stage='submitted',provider_batch_id='paid-batch' WHERE cycle_id='paid-cycle';
            INSERT INTO dream_batch_items(cycle_id,custom_id) VALUES('paid-cycle','expected-custom');")?;
        let queries = [
            "SELECT json_array(id,ts,source,kind,content,content_hash,last_seen_at) FROM event_log WHERE id=9101",
            "SELECT json_array(id,description,confidence,base_confidence,last_evidence_at,user_pinned,user_edited_at,updated_at) FROM commitments WHERE id=7",
            "SELECT json_array(state_table,state_id,event_id,weight) FROM state_provenance WHERE state_id=7",
            "SELECT json_array(state,provider_id,expected_custom_id,submitted_at,sources_json,generation) FROM brief_generation WHERE date='in-flight'",
            "SELECT json_array(cycle_id,status,batch_stage,provider_batch_id,snapshot_max_arrival_seq) FROM dream_cycles WHERE cycle_id='paid-cycle'",
            "SELECT json_array(cycle_id,event_id,arrival_seq,event_ts,source,content,disposition) FROM dream_cycle_inputs WHERE cycle_id='paid-cycle'",
            "SELECT json_array(cycle_id,custom_id) FROM dream_batch_items WHERE cycle_id='paid-cycle'",
        ];
        let before = queries
            .iter()
            .map(|sql| conn.query_row(sql, [], |row| row.get::<_, String>(0)))
            .collect::<Result<Vec<_>, _>>()?;
        migrate_and_check(&mut conn)?;
        let after = queries
            .iter()
            .map(|sql| conn.query_row(sql, [], |row| row.get::<_, String>(0)))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(after, before);
        assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION));
        assert_eq!(
            conn.query_row(
                "SELECT quote FROM state_provenance WHERE state_id=7",
                [],
                |row| row.get::<_, Option<String>>(0)
            )?,
            None
        );
        assert!(!conn.query_row("SELECT batch_reviewed FROM state_provenance WHERE state_id=7", [], |row| row.get::<_, bool>(0))?);
        assert!(conn.query_row("SELECT wording_edited FROM commitments WHERE id=7", [], |row| row.get::<_, bool>(0))?);
        assert!(conn
            .execute(
                "UPDATE event_log SET content='must stay pinned' WHERE id=9101",
                []
            )
            .is_err());
        Ok(())
    }

    #[test]
    fn v70_send_rows_name_the_service_and_keep_everything_else(
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Issue #924. Fictional private targets in pre-V70 send rows.
        let mut conn = schema52()?;
        embedded::runner()
            .set_target(refinery::Target::Version(69))
            .run(&mut conn)?;
        let rows = [
            ("integration.send_email", "alice@example.com", "composio", 1, "gmail"),
            ("integration.create_calendar_event", "Medical appointment", "mcp", 0, "gcal"),
            ("integration.update_calendar_event", "Custody hearing", "mcp", 0, "gcal"),
            ("integration.create_document", "Resignation letter", "mcp", 0, "gdrive"),
            ("integration.update_document", "Salary review notes", "mcp", 0, "notion"),
            ("integration.post_message", "@dana-private", "mcp", 0, "slack"),
            ("integration.add_reaction", "C42/1700000000.000100", "mcp", 0, "slack"),
            ("integration.post_comment", "acme/secret-repo#7", "mcp", 0, "github"),
            ("integration.change_issue_status", "HR-12 Done", "mcp", 0, "linear"),
            // Not sends: left exactly as they were.
            ("draft_create", "gmail", "composio", 1, "gmail"),
            ("gmail_read", "gmail", "composio", 1, "gmail"),
            ("agent", "api.anthropic.com", "messages_api", 0, "api.anthropic.com"),
        ];
        for (i, (purpose, destination, route, third_party, _)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO traceability_log (ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![100 + i as i64, route, purpose, destination, 10 + i as i64, format!("x{i}"), third_party],
            )?;
        }
        let snapshot = |conn: &Connection| -> rusqlite::Result<Vec<String>> {
            let mut stmt = conn.prepare(
                "SELECT json_array(id, ts, route, purpose, chunk_bytes, chunk_xxh64, third_party, kind)
                 FROM traceability_log ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect();
            rows
        };
        let before = snapshot(&conn)?;
        migrate_and_check(&mut conn)?;
        assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION));
        let after = snapshot(&conn)?;
        assert_eq!(after, before, "only the destination may change");
        let mut stmt = conn.prepare("SELECT destination FROM traceability_log ORDER BY id")?;
        let destinations = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let expected: Vec<_> = rows.iter().map(|(.., d)| d.to_string()).collect();
        assert_eq!(destinations, expected);
        Ok(())
    }

    #[test]
    fn profile_source_upgrade_clears_prose_and_preserves_paid_attempts(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for (state, handle) in [("generating", Some("paid-batch")), ("uncertain", None)] {
            let mut conn = schema52()?;
            embedded::runner()
                .set_target(refinery::Target::Version(61))
                .run(&mut conn)?;
            conn.execute(
                "INSERT INTO about_profile_generation
                    (id,summary,sources_json,attempt_sources_json,state,request_id,created_at,updated_at,
                     provider_batch_id,expected_custom_id,submitted_at,last_success_at,attempt_context_digest)
                 VALUES(1,'stale overview','[[42,\"old\"]]','[[43,\"attempt\"]]',?1,'paid-request',1,2,
                        ?2,'paid-custom',2,1,'context-digest')",
                rusqlite::params![state, handle],
            )?;
            migrate_and_check(&mut conn)?;
            let actual: String = conn.query_row(
                "SELECT json_array(summary,sources_json,last_success_at,state,request_id,
                    provider_batch_id,expected_custom_id,submitted_at,attempt_sources_json,attempt_context_digest)
                 FROM about_profile_generation WHERE id=1", [], |row| row.get(0),
            )?;
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&actual)?,
                serde_json::json!([
                    null,
                    "[]",
                    null,
                    state,
                    "paid-request",
                    handle,
                    "paid-custom",
                    2,
                    "[[43,\"attempt\"]]",
                    "context-digest"
                ])
            );
        }
        Ok(())
    }

    #[test]
    fn main_v58_brief_ledger_upgrades_without_losing_live_handles_or_completed_briefs(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = schema52()?;
        embedded::runner()
            .set_target(refinery::Target::Version(58))
            .run(&mut conn)?;
        conn.execute(
            "INSERT INTO event_log(id,ts,source,kind,content,content_hash,last_seen_at)
             VALUES(9101,150,'gmail','message','source','source-hash',1)",
            [],
        )?;
        conn.execute(
            "INSERT INTO brief_generation(date,input_from_ts,input_to_ts,output_language,sources_json,state,provider_id,generation,created_at,updated_at)
             VALUES('in-flight',100,200,'en','[[9101,\"gmail\"]]','processing','resp_existing',3,1,2)",
            [],
        )?;
        conn.execute(
            "INSERT INTO brief_generation(date,input_from_ts,input_to_ts,output_language,sources_json,state,generation,created_at,updated_at)
             VALUES('pending',100,200,'en','[]','pending',1,1,2)",
            [],
        )?;
        conn.execute(
            "INSERT INTO brief_generation(date,input_from_ts,input_to_ts,output_language,sources_json,state,generation,created_at,updated_at)
             VALUES('completed',100,200,'en','[]','complete',2,1,2)",
            [],
        )?;
        conn.execute(
            "INSERT INTO briefs(date,payload,generated,built_at) VALUES('completed','{\"what_happened\":[\"preserved\"]}',1,2)",
            [],
        )?;

        migrate_and_check(&mut conn)?;
        assert_eq!(schema_version(&conn)?, Some(LATEST_SCHEMA_VERSION));
        let live: (String, Option<String>, Option<String>, Option<i64>) = conn.query_row(
            "SELECT state,provider_id,expected_custom_id,submitted_at FROM brief_generation WHERE date='in-flight'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(
            live,
            (
                "processing".into(),
                Some("resp_existing".into()),
                None,
                None
            )
        );
        assert_eq!(
            conn.query_row(
                "SELECT state FROM brief_generation WHERE date='pending'",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "pending"
        );
        assert_eq!(
            conn.query_row(
                "SELECT payload FROM briefs WHERE date='completed'",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "{\"what_happened\":[\"preserved\"]}"
        );
        assert_eq!(
            conn.query_row(
                "SELECT state FROM brief_generation WHERE date='completed'",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "complete"
        );
        Ok(())
    }

    #[test]
    fn feature_tables_without_refinery_history_fail_closed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE brief_generation(date TEXT PRIMARY KEY); INSERT INTO brief_generation VALUES('keep');")?;
        assert!(migrate_and_check(&mut conn).is_err());
        assert_eq!(
            conn.query_row("SELECT date FROM brief_generation", [], |r| r
                .get::<_, String>(0))?,
            "keep"
        );
        // Earlier migrations may commit, but the conflicting migration must not be recorded.
        assert!(schema_version(&conn)?.is_none_or(|version| version < 55));
        Ok(())
    }

    #[test]
    fn v63_preserves_completed_uncited_briefs_but_still_invalidates_cited_and_pending(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = schema52()?;
        embedded::runner()
            .set_target(refinery::Target::Version(62))
            .run(&mut conn)?;
        for id in 1..=4 {
            conn.execute(
                "INSERT INTO event_log(id,ts,source,kind,content,content_hash,last_seen_at)
                 VALUES(?1,150,'gmail','message','before','old-hash',1)",
                [id],
            )?;
        }
        for (date, state, sources) in [
            ("uncited-complete", "complete", "[]"),
            ("uncited-pending", "pending", "[]"),
            ("cited-update", "complete", "[[3,\"source\"]]"),
            ("cited-delete", "complete", "[[4,\"source\"]]"),
        ] {
            conn.execute(
                "INSERT INTO brief_generation(date,input_from_ts,input_to_ts,output_language,sources_json,state,
                   provider_id,expected_custom_id,submitted_at,created_at,updated_at)
                 VALUES(?1,100,200,'en',?3,?2,'batch-1','custom-1',1,1,1)",
                rusqlite::params![date, state, sources],
            )?;
            conn.execute(
                "INSERT INTO briefs(date,payload,generated,built_at) VALUES(?1,'{\"what_happened\":[\"kept\"]}',1,1)",
                [date],
            )?;
        }
        embedded::runner().run(&mut conn)?;
        conn.execute(
            "INSERT INTO event_log(id,ts,source,kind,content,content_hash,last_seen_at)
             VALUES(5,150,'gmail','message','low-salience noise','noise-hash',1)",
            [],
        )?;
        conn.execute(
            "UPDATE event_log SET content='after',content_hash='new-hash' WHERE id IN (1,2,3)",
            [],
        )?;
        conn.execute("DELETE FROM event_log WHERE id=4", [])?;

        type BriefState = (
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
        );
        let read = |date: &str| -> Result<BriefState, rusqlite::Error> {
            conn.query_row(
                "SELECT state,generated,payload,provider_id,expected_custom_id,submitted_at
                 FROM brief_generation JOIN briefs USING(date) WHERE date=?1",
                [date],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
        };
        let completed = read("uncited-complete")?;
        assert_eq!(completed.0, "complete");
        assert_eq!(completed.1, 1);
        assert!(completed.2.contains("kept"));
        let pending = read("uncited-pending")?;
        assert_eq!(pending.0, "pending");
        assert_eq!(pending.1, 1);
        assert!(pending.2.contains("kept"));
        for date in ["cited-update", "cited-delete"] {
            let cited = read(date)?;
            assert_eq!(cited.0, "expired");
            assert_eq!(cited.1, 0);
            assert!(cited.2.contains("[]"));
            assert_eq!((cited.3, cited.4, cited.5), (None, None, None));
        }
        Ok(())
    }

    #[test]
    fn v20_database_upgrades_to_v22_and_reopens_without_reapplying() {
        let path = std::env::temp_dir().join(format!(
            "shogun_mem_v20_to_v22_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            vector::register_extension();
            let mut conn = Connection::open(&path).unwrap();
            apply_pragmas(&conn, true).unwrap();
            embedded::runner()
                .set_target(refinery::Target::Version(20))
                .run(&mut conn)
                .unwrap();
            assert_eq!(schema_version(&conn).unwrap(), Some(20));
            conn.execute(
                "INSERT INTO event_log
                   (ts, source, kind, content, content_hash, last_seen_at)
                 VALUES (1, 'capture', 'text', 'upgrade evidence', 'upgrade-event', 1)",
                [],
            )
            .unwrap();
        }
        {
            let conn = open(&path).unwrap();
            assert_eq!(schema_version(&conn).unwrap(), Some(LATEST_SCHEMA_VERSION));
            let canonical: i64 = conn
                .query_row("SELECT count(*) FROM event_log", [], |row| row.get(0))
                .unwrap();
            assert_eq!(canonical, 1);
            let projection: i64 = conn
                .query_row("SELECT count(*) FROM search_documents", [], |row| row.get(0))
                .unwrap();
            assert_eq!(projection, 0, "migration does not backfill inside refinery");
            let backfill_cursors: i64 = conn
                .query_row(
                    "SELECT count(*) FROM search_document_backfill_state
                      WHERE last_source_rowid = 0 AND is_complete = 0",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(backfill_cursors, 10);

            conn.execute(
                "INSERT INTO period_summaries
                   (level, period_start_ms, period_end_ms, title, content, generation_mode,
                    generated, content_digest, generated_at_ms, created_at_ms, updated_at_ms)
                 VALUES ('hour', 0, 3600000, 'First hour', 'Period evidence', 'extractive',
                         0, 'period-digest', 2, 2, 2)",
                [],
            )
            .unwrap();
            let summary_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO search_documents
                   (document_kind, source_table, source_id, title, body,
                    period_start_ms, period_end_ms, confidence, generation_kind,
                    content_digest, source_updated_at_ms, last_projected_at_ms,
                    created_at_ms, updated_at_ms)
                 VALUES ('period_summary', 'period_summaries', ?1, 'First hour',
                         'Period evidence', 0, 3600000, 1.0, 'extractive',
                         'period-document-digest', 2, 2, 2, 2)",
                [summary_id.to_string()],
            )
            .unwrap();
        }
        {
            let conn = open(&path).unwrap();
            assert_eq!(schema_version(&conn).unwrap(), Some(LATEST_SCHEMA_VERSION));
            let period_documents: i64 = conn
                .query_row(
                    "SELECT count(*) FROM search_documents
                      WHERE document_kind = 'period_summary'
                        AND source_table = 'period_summaries'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(period_documents, 1);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_defers_full_backfill_and_bounded_batches_resume_after_reopen() {
        let path = std::env::temp_dir().join(format!(
            "shogun_mem_task6_bounded_open_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            vector::register_extension();
            let mut conn = Connection::open(&path).unwrap();
            apply_pragmas(&conn, true).unwrap();
            embedded::runner()
                .set_target(refinery::Target::Version(20))
                .run(&mut conn)
                .unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
                 VALUES(1,'capture','text','Backfill source','backfill-source',1)",
                [],
            )
            .unwrap();
            let event_id = tx.last_insert_rowid();
            for index in 0..205 {
                tx.execute(
                    "INSERT INTO people
                       (display_name, confidence, created_at, updated_at, last_evidence_at)
                     VALUES (?1, 0.8, 1, 1, 1)",
                    [format!("Backfill person {index}")],
                )
                .unwrap();
                let person_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO state_provenance(state_table,state_id,event_id,weight)
                     VALUES('people',?1,?2,1.0)",
                    rusqlite::params![person_id, event_id],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        let conn = open(&path).unwrap();
        let projected: i64 = conn
            .query_row("SELECT count(*) FROM search_documents", [], |row| row.get(0))
            .unwrap();
        assert_eq!(projected, 0, "open must not enumerate canonical projection sources");

        let first = search_documents::backfill_batch(&conn, 100, 10).unwrap();
        let first_projected: i64 = conn
            .query_row("SELECT count(*) FROM search_documents", [], |row| row.get(0))
            .unwrap();
        drop(conn);

        let conn = open(&path).unwrap();
        let after_reopen: i64 = conn
            .query_row("SELECT count(*) FROM search_documents", [], |row| row.get(0))
            .unwrap();
        let second = search_documents::backfill_batch(&conn, 100, 11).unwrap();
        let second_projected: i64 = conn
            .query_row("SELECT count(*) FROM search_documents", [], |row| row.get(0))
            .unwrap();

        assert_eq!(
            (
                first.visited,
                first_projected,
                after_reopen,
                second.visited,
                second_projected,
            ),
            (100, 100, 100, 100, 200),
            "each maintenance pass stays bounded and reopen resumes its durable cursor",
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fts_indexes_inserted_events() {
        let conn = open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (?1, 'capture', 'text', ?2, 'h1', ?1)",
            rusqlite::params![1000i64, "the quarterly roadmap review"],
        )
        .unwrap();
        // Trigram FTS match on a substring.
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM event_fts WHERE event_fts MATCH 'roadmap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn fts_follows_delete() {
        let conn = open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (1, 'capture', 'text', 'deletable content', 'h', 1)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM event_log WHERE content_hash='h'", []).unwrap();
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM event_fts WHERE event_fts MATCH 'deletable'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 0);
    }

    #[test]
    fn search_document_migration_syncs_fts_and_enforces_typed_provenance() {
        let conn = open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO event_log
                 (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (1, 'capture', 'text', 'canonical evidence', 'search-doc-event', 1)",
            [],
        )
        .unwrap();
        let event_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO search_documents
                 (document_kind, source_table, source_id, title, body, confidence,
                  generation_kind, content_digest, source_updated_at_ms,
                  last_projected_at_ms, created_at_ms, updated_at_ms)
             VALUES ('person', 'people', '1', 'Alice', 'Quarterly roadmap owner', 0.9,
                     'canonical', 'digest', 1, 1, 1, 1)",
            [],
        )
        .unwrap();
        let document_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO search_document_provenance
                 (document_id, evidence_kind, event_id, weight)
             VALUES (?1, 'event', ?2, 1.0)",
            rusqlite::params![document_id, event_id],
        )
        .unwrap();

        let fts_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_documents_fts
                 WHERE search_documents_fts MATCH 'roadmap'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 1);

        let duplicate = conn.execute(
            "INSERT INTO search_document_provenance
                 (document_id, evidence_kind, event_id, weight)
             VALUES (?1, 'event', ?2, 1.0)",
            rusqlite::params![document_id, event_id],
        );
        assert!(duplicate.is_err(), "partial unique index must reject duplicate evidence");

        let invented = conn.execute(
            "INSERT INTO search_document_provenance
                 (document_id, evidence_kind, event_id, weight)
             VALUES (?1, 'event', 999999, 1.0)",
            [document_id],
        );
        assert!(invented.is_err(), "typed FK must reject invented evidence");

        conn.execute(
            "INSERT INTO sessions
                 (kind, started_at, confidence, provenance, created_at, updated_at)
             VALUES ('meeting', 1, 1.0, '[]', 1, 1)",
            [],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO event_log
                 (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (2, 'capture', 'text', 'second evidence', 'search-doc-event-2', 2)",
            [],
        )
        .unwrap();
        let second_event_id = conn.last_insert_rowid();

        let mismatched_kind = conn.execute(
            "INSERT INTO search_document_provenance
                 (document_id, evidence_kind, event_id, weight)
             VALUES (?1, 'session', ?2, 1.0)",
            rusqlite::params![document_id, second_event_id],
        );
        assert!(
            mismatched_kind
                .unwrap_err()
                .to_string()
                .contains("CHECK constraint failed"),
            "evidence_kind must match its populated typed column"
        );

        let multiple_targets = conn.execute(
            "INSERT INTO search_document_provenance
                 (document_id, evidence_kind, event_id, session_id, weight)
             VALUES (?1, 'event', ?2, ?3, 1.0)",
            rusqlite::params![document_id, second_event_id, session_id],
        );
        assert!(
            multiple_targets
                .unwrap_err()
                .to_string()
                .contains("CHECK constraint failed"),
            "exactly one typed evidence target must be populated"
        );
    }

    #[test]
    fn confidence_out_of_range_is_rejected() {
        let conn = open_in_memory().unwrap();
        let bad = conn.execute(
            "INSERT INTO people (display_name, confidence, created_at, updated_at, last_evidence_at)
             VALUES ('x', 1.5, 0, 0, 0)",
            [],
        );
        assert!(bad.is_err(), "confidence 1.5 must violate the CHECK");
        let ok = conn.execute(
            "INSERT INTO people (display_name, confidence, created_at, updated_at, last_evidence_at)
             VALUES ('x', 0.7, 0, 0, 0)",
            [],
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn state_enums_are_constrained() {
        let conn = open_in_memory().unwrap();
        let bad = conn.execute(
            "INSERT INTO projects (name, status, confidence, created_at, updated_at, last_evidence_at)
             VALUES ('p', 'bogus', 0.9, 0, 0, 0)",
            [],
        );
        assert!(bad.is_err(), "status='bogus' must violate the CHECK");
    }

    #[test]
    fn strict_typing_rejects_wrong_type() {
        let conn = open_in_memory().unwrap();
        // ts is INTEGER in a STRICT table; a non-integer must be rejected.
        let bad = conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES ('not-a-number', 'capture', 'text', 'c', 'h', 1)",
            [],
        );
        assert!(bad.is_err(), "STRICT table must reject a text ts");
    }
}
