//! Shared database handle, hot-layer lifecycle, connection health, and opening paths.

use super::*;
use shogun_mcp::memory_readiness::MemoryUnavailable;

/// Run one database operation under the active Dream owner fence. The immediate transaction
/// makes the owner check and the operation indivisible to every other SQLite connection.
fn run_with_dream_effect_transaction<T, E>(
    conn: &mut Connection,
    now: i64,
    f: impl FnOnce(&mut Connection) -> Result<T, E>,
) -> Result<Result<T, E>, String> {
    let fenced = super::dream_cycle::begin_dream_effect_transaction(conn, now)?;
    if !fenced {
        return Ok(f(conn));
    }

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(conn)));
    match outcome {
        Ok(Ok(value)) => {
            conn.execute_batch("COMMIT")
                .map_err(|_| "dream effect transaction commit failed".to_string())?;
            Ok(Ok(value))
        }
        Ok(Err(error)) => {
            let _ = conn.execute_batch("ROLLBACK");
            Ok(Err(error))
        }
        Err(payload) => {
            let _ = conn.execute_batch("ROLLBACK");
            std::panic::resume_unwind(payload)
        }
    }
}

/// The shared connection handle. `Connection` is `Send` but not `Sync`, so it lives behind a
/// `Mutex`; the `Arc` lets every daemon component share the one handle.
pub type SharedConn = Arc<Mutex<Connection>>;

/// An injected millisecond clock (unix ms). Shared so every writer stamps from the same source.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Fixed backup failure vocabulary. Unlike [`MemoryFault`], these failures say nothing about
/// whether live capture/search can read the source store and never alter [`Db::memory_health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupFailureClass {
    SourceLock,
    Directory,
    Permission,
    Copy,
    Deadline,
    Verification,
    Publish,
}

impl From<shogun_memory::backup::BackupError> for BackupFailureClass {
    fn from(error: shogun_memory::backup::BackupError) -> Self {
        match error {
            shogun_memory::backup::BackupError::Directory => Self::Directory,
            shogun_memory::backup::BackupError::Permission => Self::Permission,
            shogun_memory::backup::BackupError::Copy => Self::Copy,
            shogun_memory::backup::BackupError::Deadline => Self::Deadline,
            shogun_memory::backup::BackupError::Verification => Self::Verification,
            shogun_memory::backup::BackupError::Publish => Self::Publish,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotPhase {
    Empty,
    Rebuilding,
    Ready,
    Failed,
}

impl HotPhase {
    fn as_u8(self) -> u8 {
        match self {
            Self::Empty => 0,
            Self::Rebuilding => 1,
            Self::Ready => 2,
            Self::Failed => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Rebuilding,
            2 => Self::Ready,
            3 => Self::Failed,
            _ => Self::Empty,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotStatus {
    pub phase: HotPhase,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub elapsed_ms: u64,
    pub bytes: usize,
    pub event_count: usize,
    pub folded_count: u64,
}

impl Default for HotStatus {
    fn default() -> Self {
        Self {
            phase: HotPhase::Empty,
            started_at_ms: None,
            finished_at_ms: None,
            elapsed_ms: 0,
            bytes: 0,
            event_count: 0,
            folded_count: 0,
        }
    }
}

pub(super) struct HotTaskOwner {
    pub(super) cancel: Arc<std::sync::atomic::AtomicBool>,
    pub(super) active: Arc<std::sync::atomic::AtomicBool>,
    pub(super) attempts: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) interrupt: rusqlite::InterruptHandle,
    repair_enabled: std::sync::atomic::AtomicBool,
    pub(super) pending_repair: std::sync::atomic::AtomicBool,
    pub(super) join: Mutex<Option<HotRun>>,
    #[cfg(test)]
    pub(super) finish_barrier: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
}

pub(super) struct HotRun {
    pub(super) handle: std::thread::JoinHandle<()>,
    completed: std::sync::mpsc::Receiver<()>,
    deadline: std::time::Instant,
}

struct HotCompletion(Option<std::sync::mpsc::SyncSender<()>>);

impl Drop for HotCompletion {
    fn drop(&mut self) {
        if let Some(completed) = self.0.take() {
            let _ = completed.send(());
        }
    }
}

impl HotTaskOwner {
    fn new(interrupt: rusqlite::InterruptHandle) -> Self {
        Self {
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            interrupt,
            repair_enabled: std::sync::atomic::AtomicBool::new(false),
            pending_repair: std::sync::atomic::AtomicBool::new(false),
            join: Mutex::new(None),
            #[cfg(test)]
            finish_barrier: Mutex::new(None),
        }
    }

    pub(super) fn shutdown(&self) {
        use std::sync::atomic::Ordering;

        self.cancel.store(true, Ordering::Release);
        if self.active.load(Ordering::Acquire) {
            self.interrupt.interrupt();
        }
        let run = match self.join.lock() {
            Ok(mut join) => join.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(run) = run {
            let remaining = run
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            let _ = run.completed.recv_timeout(remaining);
            while !run.handle.is_finished() && std::time::Instant::now() < run.deadline {
                std::thread::yield_now();
            }
            if run.handle.is_finished() {
                let _ = run.handle.join();
            }
        }
    }
}

impl Drop for HotTaskOwner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub(super) struct HotRebuilder {
    conn: std::sync::Weak<Mutex<Connection>>,
    clock: std::sync::Weak<dyn Fn() -> i64 + Send + Sync>,
    hot: std::sync::Weak<Mutex<shogun_memory::hot::HotLayer>>,
    status: std::sync::Weak<Mutex<HotStatus>>,
    phase: std::sync::Weak<std::sync::atomic::AtomicU8>,
    health: std::sync::Weak<crate::memory_health::MemoryHealth>,
}

/// Owns the connection lock for the duration of a hot rebuild, so the progress handler it
/// installs is always removed before the lock is released (registering a handler needs
/// `&mut Connection`, which a borrowed guard could not provide).
struct HotProgressGuard<'a> {
    conn: std::sync::MutexGuard<'a, Connection>,
    pub(super) active: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl std::ops::Deref for HotProgressGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl HotProgressGuard<'_> {
    /// Remove the progress handler and clear the active flag, keeping the connection lock.
    /// Idempotent; `drop` calls it too.
    fn release_handler(&mut self) {
        use std::sync::atomic::Ordering;

        // Removing a handler only clears SQLite client data; nothing useful can be done with a
        // failure here, and the next registration replaces the handler anyway.
        let _ = self.conn.progress_handler(0, None::<fn() -> bool>);
        if let Some(active) = self.active.take() {
            active.store(false, Ordering::Release);
        }
    }
}

impl Drop for HotProgressGuard<'_> {
    fn drop(&mut self) {
        self.release_handler();
    }
}

fn note_hot_rebuild_fault(
    health: &crate::memory_health::MemoryHealth,
    clock: &Clock,
    op: &'static str,
    fault: MemoryFault,
    class: &'static str,
) {
    health.record_fault(fault, clock());
    crate::elog!("[memory] {op} failed: {class}");
}

fn replace_hot_rebuild_status(
    status: &Mutex<HotStatus>,
    phase: &std::sync::atomic::AtomicU8,
    health: &crate::memory_health::MemoryHealth,
    clock: &Clock,
    replacement: HotStatus,
) {
    phase.store(
        replacement.phase.as_u8(),
        std::sync::atomic::Ordering::Release,
    );
    let mut guard = match status.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            note_hot_rebuild_fault(
                health,
                clock,
                "hot.status",
                MemoryFault::LockPoisoned,
                "lock_poisoned",
            );
            poisoned.into_inner()
        }
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    *guard = replacement;
}

impl HotRebuilder {
    pub(super) fn run(
        &self,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        deadline: std::time::Instant,
        active: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> bool {
        use std::sync::atomic::Ordering;
        use std::sync::TryLockError;

        let Some(conn_arc) = self.conn.upgrade() else {
            return false;
        };
        let Some(clock) = self.clock.upgrade() else {
            return false;
        };
        let Some(hot_arc) = self.hot.upgrade() else {
            return false;
        };
        let Some(status) = self.status.upgrade() else {
            return false;
        };
        let Some(phase) = self.phase.upgrade() else {
            return false;
        };
        let Some(health) = self.health.upgrade() else {
            return false;
        };
        let started_at_ms = clock();
        let started = std::time::Instant::now();
        replace_hot_rebuild_status(
            &status,
            &phase,
            &health,
            &clock,
            HotStatus {
                phase: HotPhase::Rebuilding,
                started_at_ms: Some(started_at_ms),
                ..HotStatus::default()
            },
        );
        let conn = loop {
            match conn_arc.try_lock() {
                Ok(conn) => break conn,
                Err(TryLockError::Poisoned(poisoned)) => {
                    note_hot_rebuild_fault(
                        &health,
                        &clock,
                        "hot.rebuild",
                        MemoryFault::LockPoisoned,
                        "lock_poisoned",
                    );
                    break poisoned.into_inner();
                }
                Err(TryLockError::WouldBlock)
                    if !cancel.load(Ordering::Acquire) && std::time::Instant::now() < deadline =>
                {
                    std::thread::yield_now();
                }
                Err(TryLockError::WouldBlock) => {
                    replace_hot_rebuild_status(
                        &status,
                        &phase,
                        &health,
                        &clock,
                        HotStatus {
                            phase: HotPhase::Failed,
                            started_at_ms: Some(started_at_ms),
                            finished_at_ms: Some(clock()),
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            ..HotStatus::default()
                        },
                    );
                    return false;
                }
            }
        };
        let progress_cancel = cancel.clone();
        let mut progress = HotProgressGuard { conn, active: None };
        if progress
            .conn
            .progress_handler(
                1_000,
                Some(move || {
                    progress_cancel.load(Ordering::Acquire)
                        || std::time::Instant::now() >= deadline
                }),
            )
            .is_err()
        {
            drop(progress);
            replace_hot_rebuild_status(
                &status,
                &phase,
                &health,
                &clock,
                HotStatus {
                    phase: HotPhase::Failed,
                    started_at_ms: Some(started_at_ms),
                    finished_at_ms: Some(clock()),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    ..HotStatus::default()
                },
            );
            return false;
        }
        if let Some(active) = &active {
            active.store(true, Ordering::Release);
        }
        progress.active = active;
        let mut repaired_poison = false;
        let mut hot = loop {
            match hot_arc.try_lock() {
                Ok(hot) => break hot,
                Err(TryLockError::Poisoned(poisoned)) => {
                    repaired_poison = true;
                    break poisoned.into_inner();
                }
                Err(TryLockError::WouldBlock)
                    if !cancel.load(Ordering::Acquire) && std::time::Instant::now() < deadline =>
                {
                    std::thread::yield_now();
                }
                Err(TryLockError::WouldBlock) => {
                    drop(progress);
                    replace_hot_rebuild_status(
                        &status,
                        &phase,
                        &health,
                        &clock,
                        HotStatus {
                            phase: HotPhase::Failed,
                            started_at_ms: Some(started_at_ms),
                            finished_at_ms: Some(clock()),
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            ..HotStatus::default()
                        },
                    );
                    return false;
                }
            }
        };
        let rebuilt = hot.rebuild_from_warm_in_place(&progress, clock(), || {
            cancel.load(Ordering::Acquire) || std::time::Instant::now() >= deadline
        });
        // The handler goes now; the connection lock is held until the status is published,
        // as before (lock order stays `conn`, then `hot`).
        progress.release_handler();
        let success = matches!(rebuilt, Ok(true));
        if !success {
            hot.clear();
        }
        let counts = (hot.byte_size(), hot.len(), hot.folded_count());
        drop(hot);
        if success && repaired_poison {
            hot_arc.clear_poison();
        }
        if let Err(error) = rebuilt {
            note_hot_rebuild_fault(
                &health,
                &clock,
                "hot.rebuild",
                MemoryFault::Query,
                error.fault_class(),
            );
        }
        replace_hot_rebuild_status(
            &status,
            &phase,
            &health,
            &clock,
            HotStatus {
                phase: if success {
                    HotPhase::Ready
                } else {
                    HotPhase::Failed
                },
                started_at_ms: Some(started_at_ms),
                finished_at_ms: Some(clock()),
                elapsed_ms: started.elapsed().as_millis() as u64,
                bytes: counts.0,
                event_count: counts.1,
                folded_count: counts.2,
            },
        );
        drop(progress);
        success
    }
}

/// The daemon's DB handle. Clone freely — clones share the same underlying connection.
pub struct Db {
    pub(super) conn: SharedConn,
    /// Meeting audit writes avoid the capture/search connection mutex.
    pub(super) meeting_audit_conn: Option<SharedConn>,
    /// Full erasure waits for pinned backup readers; capture/search never take this lock.
    pub(super) backup_copy: Arc<Mutex<()>>,
    pub(super) clock: Clock,
    /// Writer/rebuild lock order is always `conn`, then `hot`.
    pub(super) hot: Arc<Mutex<shogun_memory::hot::HotLayer>>,
    pub(super) hot_status: Arc<Mutex<HotStatus>>,
    pub(super) hot_phase: Arc<std::sync::atomic::AtomicU8>,
    pub(super) hot_task: Arc<HotTaskOwner>,
    /// The local embedding model, when one is loaded. `None` means search runs lexical-only:
    /// every result still comes back via FTS, just without the semantic half (FR-MEM-22 — an
    /// un-embedded event is never invisible).
    pub(super) embedder: Option<Arc<dyn shogun_memory::embed::Embedder>>,
    /// The compression config, when injected (Issue #63). `None` means the raw context path stays
    /// in effect — the desktop only supplies an `enabled` config behind a flag, so the default is
    /// unchanged behaviour.
    pub(super) compression_config: Option<shogun_fusion::compress::CompressionConfig>,
    /// The internal event bus, when wired (design §2.2 / E-49). `None` keeps every publish a
    /// no-op, so a `Db` opened without the daemon composition publishes nothing.
    pub(super) bus: Option<crate::bus::Bus>,
    /// Whether the store is answering (issue #121). Shared by every clone, because a failure the
    /// capture thread hits is the same failure the panel has to report.
    pub(super) health: Arc<crate::memory_health::MemoryHealth>,
    /// Last compression / tool-loop snapshot for `shogun metrics` (H1). Shared by clones the same
    /// way `health` is — desktop assemble and `shogun-api` read the same numbers. Counts only.
    pub(super) harness: Arc<Mutex<crate::metrics::HarnessCounters>>,
    pub(super) capture_paused_url: Arc<Mutex<Option<crate::capture::status::Status>>>,
    pub(super) daily_sources_checked_date: Arc<Mutex<Option<String>>>,
    /// Advances under the DB lock on user-data erasure. Native work snapshots it before its
    /// side effect, so stale post-erase activity accounting is rejected.
    pub(super) activity_erase_generation: Arc<std::sync::atomic::AtomicU64>,
    /// The anonymous-analytics handle, once the shell hands it over ([`Self::set_analytics`]).
    /// Shared by every clone, including those taken before it was set (the Dream driver starts
    /// before analytics is initialised). Empty = nothing is emitted.
    pub(super) analytics: Arc<std::sync::OnceLock<crate::analytics::AnalyticsHandle>>,
    /// Where the last Dream outcome and its reason are kept (`dream_outcome.json`, issue #285).
    /// `None` keeps recording a no-op: tests and in-memory stores never touch the app-data file.
    pub(super) dream_outcome_path: Option<Arc<std::path::PathBuf>>,
    /// `visual_recall.json`, so chat and context reads of saved screens honour the user's
    /// retention and the Off switch like every other read path (issue #782). `None` (tests, and
    /// any process that never set it) keeps the hard upper bound only.
    pub(super) visual_recall_settings_path: Option<Arc<std::path::PathBuf>>,
}

impl Clone for Db {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            meeting_audit_conn: self.meeting_audit_conn.clone(),
            backup_copy: self.backup_copy.clone(),
            clock: self.clock.clone(),
            hot: self.hot.clone(),
            hot_status: self.hot_status.clone(),
            hot_phase: self.hot_phase.clone(),
            hot_task: self.hot_task.clone(),
            embedder: self.embedder.clone(),
            compression_config: self.compression_config,
            bus: self.bus.clone(),
            health: self.health.clone(),
            harness: self.harness.clone(),
            capture_paused_url: self.capture_paused_url.clone(),
            daily_sources_checked_date: self.daily_sources_checked_date.clone(),
            activity_erase_generation: self.activity_erase_generation.clone(),
            analytics: self.analytics.clone(),
            dream_outcome_path: self.dream_outcome_path.clone(),
            visual_recall_settings_path: self.visual_recall_settings_path.clone(),
        }
    }
}

impl Db {
    /// Wrap an already-open, migrated connection.
    pub fn new(conn: Connection, clock: Clock) -> Self {
        // Commitment due dates ("by Friday") are read in the user's local time (issue #253).
        shogun_memory::due::set_local_offset_provider(crate::daemon::os_gmt_offset_secs);
        let interrupt = conn.get_interrupt_handle();
        Self {
            conn: Arc::new(Mutex::new(conn)),
            meeting_audit_conn: None,
            backup_copy: Arc::new(Mutex::new(())),
            clock,
            hot: Arc::new(Mutex::new(shogun_memory::hot::HotLayer::with_defaults())),
            hot_status: Arc::new(Mutex::new(HotStatus::default())),
            hot_phase: Arc::new(std::sync::atomic::AtomicU8::new(HotPhase::Empty.as_u8())),
            hot_task: Arc::new(HotTaskOwner::new(interrupt)),
            embedder: None,
            compression_config: None,
            bus: None,
            health: Arc::new(crate::memory_health::MemoryHealth::new()),
            harness: Arc::new(Mutex::new(crate::metrics::HarnessCounters::default())),
            capture_paused_url: Arc::new(Mutex::new(None)),
            daily_sources_checked_date: Arc::new(Mutex::new(None)),
            activity_erase_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            analytics: Arc::new(std::sync::OnceLock::new()),
            dream_outcome_path: None,
            visual_recall_settings_path: None,
        }
    }

    /// Attach a keyed connection to the same migrated store for meeting audit writes only.
    /// A small page cache keeps this auxiliary writer from duplicating the main cache.
    /// Keeps rusqlite's 5 s busy timeout, like the main handle: a maintenance or erase write
    /// must delay a chunk, not fail it and reconnect both live streams. If a pragma fails the
    /// connection is dropped and meeting audit stays on the shared writer; startup never fails.
    pub fn with_meeting_audit_connection(mut self, conn: Connection) -> Self {
        let configured = conn
            .pragma_update(None, "cache_size", -256)
            .and_then(|()| conn.pragma_update(None, "synchronous", "NORMAL"));
        if configured.is_ok() {
            self.meeting_audit_conn = Some(Arc::new(Mutex::new(conn)));
        }
        self
    }

    /// Wrap a connection whose lifecycle owner already migrated and checked it, then publish Hot.
    pub fn from_migrated_connection(conn: Connection, clock: Clock) -> Self {
        let db = Self::new(conn, clock);
        db.start_hot_rebuild();
        db
    }

    /// The live memory-health signal (issue #121): what the last store operation did, and how
    /// many have failed since launch. The shell polls this for the notch's degraded indicator.
    pub fn memory_health(&self) -> crate::memory_health::MemoryHealthSnapshot {
        self.health.snapshot()
    }

    /// Create one verified encrypted snapshot without mixing backup failures into live-memory
    /// health. Runtime copies use their own read-only connection instead of blocking the live
    /// connection during the bounded page-copy stage.
    pub fn create_verified_backup(
        &self,
        key: &shogun_memory::DbKey,
        coordinator: &shogun_memory::backup::BackupCoordinator,
        now_ms: i64,
        nonce: &str,
    ) -> Result<shogun_memory::backup::BackupStatus, BackupFailureClass> {
        // Only inspect the live path under the shared mutex. A dedicated read-only connection
        // lets capture/search continue throughout the (up to 120-second) online backup.
        let source_path = {
            let source = self
                .conn
                .lock()
                .map_err(|_| BackupFailureClass::SourceLock)?;
            source
                .path()
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from)
        };
        let mut reader = source_path
            .as_ref()
            .map(|path| {
                shogun_memory::open_encrypted_existing_read_only(path, key)
                    .map_err(|_| BackupFailureClass::Copy)
            })
            .transpose()?;
        let prepared = coordinator
            .prepare_snapshot(key, now_ms, nonce)
            .map_err(BackupFailureClass::from)?;
        let copy_guard = self
            .backup_copy
            .lock()
            .map_err(|_| BackupFailureClass::SourceLock)?;
        let pending = if let Some(source) = reader.as_mut() {
            // Pin a WAL read snapshot after capturing the wipe epoch. Otherwise each capture
            // write can restart the page copy. Drop this read transaction before verification.
            let snapshot = source.transaction().map_err(|_| BackupFailureClass::Copy)?;
            snapshot
                .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|_| BackupFailureClass::Copy)?;
            coordinator
                .copy_prepared_to_closed(&snapshot, prepared)
                .map_err(BackupFailureClass::from)?
        } else {
            // In-memory stores have no second connection; they are used only by tests.
            let source = self
                .conn
                .lock()
                .map_err(|_| BackupFailureClass::SourceLock)?;
            coordinator
                .copy_prepared_to_closed(&source, prepared)
                .map_err(BackupFailureClass::from)?
        };
        drop(reader);
        drop(copy_guard);
        coordinator
            .verify_publish_rotate(key, pending)
            .map_err(BackupFailureClass::from)
    }

    pub fn hot_status(&self) -> HotStatus {
        let mut status = match self.hot_status.lock() {
            Ok(status) => *status,
            Err(poisoned) => {
                self.note_fault("hot.status", MemoryFault::LockPoisoned, "lock_poisoned");
                *poisoned.into_inner()
            }
        };
        status.phase = self.hot_phase();
        status
    }

    pub(super) fn hot_phase(&self) -> HotPhase {
        HotPhase::from_u8(self.hot_phase.load(std::sync::atomic::Ordering::Acquire))
    }

    pub(super) fn replace_hot_status(&self, status: HotStatus) {
        self.hot_phase
            .store(status.phase.as_u8(), std::sync::atomic::Ordering::Release);
        let mut guard = match self.hot_status.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.note_fault("hot.status", MemoryFault::LockPoisoned, "lock_poisoned");
                poisoned.into_inner()
            }
        };
        *guard = status;
    }

    pub(super) fn mark_hot_failed(&self) {
        use std::sync::atomic::Ordering;

        self.hot_phase
            .store(HotPhase::Failed.as_u8(), Ordering::Release);
        let previous = self.hot_status();
        self.replace_hot_status(HotStatus {
            phase: HotPhase::Failed,
            ..previous
        });
        if self.hot_task.repair_enabled.load(Ordering::Acquire) {
            self.hot_task.pending_repair.store(true, Ordering::Release);
            self.start_hot_rebuild();
        }
    }

    /// Synchronous repair/test seam. Production starts this on a named background thread.
    pub fn rebuild_hot_now(&self) -> bool {
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.hot_rebuilder().run(
            cancel,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            None,
        )
    }

    pub(super) fn start_hot_rebuild(&self) {
        use std::sync::atomic::Ordering;

        self.hot_task.repair_enabled.store(true, Ordering::Release);
        let mut join = match self.hot_task.join.lock() {
            Ok(join) => join,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(run) = join.as_ref() {
            if !run.handle.is_finished() {
                if !self.hot_task.pending_repair.load(Ordering::Acquire) {
                    return;
                }
                while !run.handle.is_finished() && std::time::Instant::now() < run.deadline {
                    std::thread::yield_now();
                }
                if !run.handle.is_finished() {
                    return;
                }
            }
        }
        if let Some(finished) = join.take() {
            let _ = finished.handle.join();
        }
        self.hot_task.cancel.store(false, Ordering::Release);
        let cancel = self.hot_task.cancel.clone();
        let active = self.hot_task.active.clone();
        let attempts = self.hot_task.attempts.clone();
        let rebuilder = self.hot_rebuilder();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (completed_tx, completed_rx) = std::sync::mpsc::sync_channel(1);
        #[cfg(test)]
        let finish_barrier = match self.hot_task.finish_barrier.lock() {
            Ok(mut barrier) => barrier.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        let spawn = std::thread::Builder::new()
            .name("shogun-hot-rebuild".to_string())
            .spawn(move || {
                let _completed = HotCompletion(Some(completed_tx));
                attempts.fetch_add(1, Ordering::AcqRel);
                let _ = rebuilder.run(cancel, deadline, Some(active));
                #[cfg(test)]
                if let Some((reached, release)) = finish_barrier {
                    reached.wait();
                    release.wait();
                }
            });
        match spawn {
            Ok(handle) => {
                self.hot_task.pending_repair.store(false, Ordering::Release);
                *join = Some(HotRun {
                    handle,
                    completed: completed_rx,
                    deadline,
                });
            }
            Err(_) => {
                self.note_fault("hot.rebuild.spawn", MemoryFault::Query, "thread_spawn");
                self.replace_hot_status(HotStatus {
                    phase: HotPhase::Failed,
                    ..HotStatus::default()
                });
            }
        }
    }

    pub(super) fn hot_rebuilder(&self) -> HotRebuilder {
        HotRebuilder {
            conn: Arc::downgrade(&self.conn),
            clock: Arc::downgrade(&self.clock),
            hot: Arc::downgrade(&self.hot),
            status: Arc::downgrade(&self.hot_status),
            phase: Arc::downgrade(&self.hot_phase),
            health: Arc::downgrade(&self.health),
        }
    }

    /// Record a failure against the health signal and say so in the log — **operation name and
    /// failure class only**, never a row, a query, or captured text (コード規約).
    pub(super) fn note_fault(&self, op: &'static str, fault: MemoryFault, class: &'static str) {
        self.health.record_fault(fault, self.now_ms());
        crate::elog!("[memory] {op} failed: {class}");
    }

    /// Run `f` against the shared connection, recording what happened in the health signal.
    ///
    /// This is the seam issue #121 asks for: a lock failure and a query failure come back as
    /// distinct [`MemoryFault`]s instead of collapsing into the same empty value a genuinely
    /// empty table produces. Callers that legitimately cannot act on a failure still swallow the
    /// `Err` — but the failure is recorded and visible, not silent.
    pub(super) fn with_conn<T, E: FaultClass>(
        &self,
        op: &'static str,
        f: impl FnOnce(&Connection) -> Result<T, E>,
    ) -> MemoryResult<T> {
        // A poisoned std mutex stays poisoned forever, but the SQLite connection under it is
        // still valid (an unwinding writer's open transaction rolled back on drop). Refusing the
        // guard here would turn one panic anywhere in the process into "capture stops recording
        // until relaunch" — the exact failure the crash-resilience rule forbids. Record the fault
        // (visible, issue #121) and keep serving.
        let mut guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                self.note_fault(op, MemoryFault::LockPoisoned, "lock_poisoned");
                poisoned.into_inner()
            }
        };
        let result =
            match run_with_dream_effect_transaction(&mut guard, self.now_ms(), |conn| f(conn)) {
                Ok(result) => result,
                Err(_) => {
                    self.note_fault(op, MemoryFault::Query, "dream_fence");
                    return Err(MemoryFault::Query);
                }
            };
        match result {
            Ok(v) => {
                self.health.record_success();
                Ok(v)
            }
            Err(e) => {
                self.note_fault(op, MemoryFault::Query, e.fault_class());
                Err(MemoryFault::Query)
            }
        }
    }

    /// [`Self::with_conn`] for the writes that need `&mut Connection` (transactions).
    pub(super) fn with_conn_mut<T, E: FaultClass>(
        &self,
        op: &'static str,
        f: impl FnOnce(&mut Connection) -> Result<T, E>,
    ) -> MemoryResult<T> {
        let mut guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                // Recover rather than refuse — see `with_conn`.
                self.note_fault(op, MemoryFault::LockPoisoned, "lock_poisoned");
                poisoned.into_inner()
            }
        };
        let result = match run_with_dream_effect_transaction(&mut guard, self.now_ms(), f) {
            Ok(result) => result,
            Err(_) => {
                self.note_fault(op, MemoryFault::Query, "dream_fence");
                return Err(MemoryFault::Query);
            }
        };
        match result {
            Ok(v) => {
                self.health.record_success();
                Ok(v)
            }
            Err(e) => {
                self.note_fault(op, MemoryFault::Query, e.fault_class());
                Err(MemoryFault::Query)
            }
        }
    }

    /// [`Self::with_conn`] for helpers that return a plain value rather than a `Result` — the
    /// lock is still the part that can fail, and it is still recorded.
    pub(super) fn read_conn<T>(
        &self,
        op: &'static str,
        f: impl FnOnce(&Connection) -> T,
    ) -> MemoryResult<T> {
        self.with_conn(op, |c| Ok::<T, String>(f(c)))
    }

    /// Take the lock for a body that must hold the guard across a loop, recording a poisoned lock
    /// before giving up. The closure helpers cannot express those bodies (the guard outlives any
    /// single statement), but the failure still has to be visible rather than an early `return`
    /// into a default value.
    ///
    /// Dream job effects must use `with_conn*`: those helpers validate the lease and wrap the write
    /// atomically. All `lock_or_note` callers are ingestion or interactive-context paths, outside
    /// `DbDreamRunner`; the fenced full-cycle regression keeps this boundary load-bearing.
    pub(super) fn lock_or_note(
        &self,
        op: &'static str,
    ) -> Option<std::sync::MutexGuard<'_, Connection>> {
        let guard = match self.conn.lock() {
            Ok(g) => Some(g),
            Err(poisoned) => {
                // Recover rather than refuse — see `with_conn`.
                self.note_fault(op, MemoryFault::LockPoisoned, "lock_poisoned");
                Some(poisoned.into_inner())
            }
        }?;
        if super::dream_cycle::has_dream_effect_fence() {
            self.note_fault(op, MemoryFault::Query, "dream_fence");
            return None;
        }
        Some(guard)
    }

    /// Caller holds DB. Persisted readback prevents raw, uncommitted content entering Hot.
    pub(super) fn reconcile_hot_locked(&self, conn: &Connection, event_id: i64) {
        let event = match event_log::hot_event_by_id(conn, event_id) {
            Ok(Some(event)) => event,
            Ok(None) => {
                self.remove_hot_locked(event_id);
                return;
            }
            Err(error) => {
                self.note_fault("hot.reconcile", MemoryFault::Query, error.fault_class());
                self.mark_hot_failed();
                return;
            }
        };
        let mut hot = match self.hot.lock() {
            Ok(hot) => hot,
            Err(_) => {
                self.note_fault("hot.reconcile", MemoryFault::LockPoisoned, "lock_poisoned");
                self.mark_hot_failed();
                return;
            }
        };
        hot.reconcile(event, self.now_ms());
        let counts = (hot.byte_size(), hot.len(), hot.folded_count());
        drop(hot);
        self.refresh_hot_counts(counts);
    }

    pub(super) fn reconcile_meeting_index_locked(
        &self,
        conn: &Connection,
        indexed: &shogun_memory::meeting_index::Indexed,
    ) {
        for &id in &indexed.removed_event_ids {
            self.remove_hot_locked(id);
        }
        for &id in &indexed.changed_event_ids {
            self.reconcile_hot_locked(conn, id);
        }
    }

    pub(super) fn remove_hot_locked(&self, event_id: i64) {
        let mut hot = match self.hot.lock() {
            Ok(hot) => hot,
            Err(_) => {
                self.note_fault("hot.remove", MemoryFault::LockPoisoned, "lock_poisoned");
                self.mark_hot_failed();
                return;
            }
        };
        hot.remove(event_id);
        let counts = (hot.byte_size(), hot.len(), hot.folded_count());
        drop(hot);
        self.refresh_hot_counts(counts);
    }

    pub(super) fn clear_hot_locked(&self) {
        let mut hot = match self.hot.lock() {
            Ok(hot) => hot,
            Err(_) => {
                self.note_fault("hot.clear", MemoryFault::LockPoisoned, "lock_poisoned");
                self.mark_hot_failed();
                return;
            }
        };
        hot.clear();
        let counts = (hot.byte_size(), hot.len(), hot.folded_count());
        drop(hot);
        self.refresh_hot_counts(counts);
    }

    pub(super) fn remove_hot_since_locked(&self, cutoff_ts: i64) {
        let mut hot = match self.hot.lock() {
            Ok(hot) => hot,
            Err(_) => {
                self.note_fault(
                    "hot.remove_since",
                    MemoryFault::LockPoisoned,
                    "lock_poisoned",
                );
                self.mark_hot_failed();
                return;
            }
        };
        hot.remove_since(cutoff_ts);
        let counts = (hot.byte_size(), hot.len(), hot.folded_count());
        drop(hot);
        self.refresh_hot_counts(counts);
    }

    pub(super) fn refresh_hot_counts(&self, counts: (usize, usize, u64)) {
        let mut status = match self.hot_status.lock() {
            Ok(status) => status,
            Err(poisoned) => {
                self.note_fault("hot.status", MemoryFault::LockPoisoned, "lock_poisoned");
                poisoned.into_inner()
            }
        };
        status.bytes = counts.0;
        status.event_count = counts.1;
        status.folded_count = counts.2;
    }

    /// Snapshot and release Hot before callers take any durable fallback.
    pub(super) fn hot_thread_candidates(
        &self,
        thread_key: &str,
        limit: usize,
        excerpt_chars: usize,
    ) -> Option<shogun_memory::hot::HotThreadCandidates> {
        if self.hot_phase() != HotPhase::Ready {
            return None;
        }
        let mut hot = match self.hot.lock() {
            Ok(hot) => hot,
            Err(_) => {
                self.note_fault("hot.read", MemoryFault::LockPoisoned, "lock_poisoned");
                self.mark_hot_failed();
                return None;
            }
        };
        let rows = hot.thread_candidates(self.now_ms(), thread_key, limit, excerpt_chars)?;
        let counts = (hot.byte_size(), hot.len(), hot.folded_count());
        drop(hot);
        self.refresh_hot_counts(counts);
        Some(rows)
    }

    pub(super) fn hot_context_candidates(
        &self,
        sources: &[&str],
        per_source_limit: usize,
        excerpt_chars: usize,
    ) -> Option<shogun_memory::hot::HotContextCandidates> {
        if self.hot_phase() != HotPhase::Ready {
            return None;
        }
        let mut hot = match self.hot.lock() {
            Ok(hot) => hot,
            Err(_) => {
                self.note_fault("hot.read", MemoryFault::LockPoisoned, "lock_poisoned");
                self.mark_hot_failed();
                return None;
            }
        };
        let rows =
            hot.context_candidates(self.now_ms(), sources, per_source_limit, excerpt_chars)?;
        let counts = (hot.byte_size(), hot.len(), hot.folded_count());
        drop(hot);
        self.refresh_hot_counts(counts);
        Some(rows)
    }

    pub(super) fn hot_generation(&self) -> Option<u64> {
        if self.hot_phase() != HotPhase::Ready {
            return None;
        }
        match self.hot.lock() {
            Ok(hot) => Some(hot.generation()),
            Err(_) => {
                self.note_fault("hot.read", MemoryFault::LockPoisoned, "lock_poisoned");
                self.mark_hot_failed();
                None
            }
        }
    }

    /// [`Self::with_conn`] for the public APIs that already report a reason to their caller.
    ///
    /// Those signatures never turn a failure into a success (the shape issue #121 wants
    /// everywhere). The reason reaches the caller unchanged; the failure also registers as
    /// degraded memory.
    pub(super) fn with_conn_reported<T>(
        &self,
        op: &'static str,
        f: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                // Recover rather than refuse — see `with_conn`.
                self.note_fault(op, MemoryFault::LockPoisoned, "lock_poisoned");
                poisoned.into_inner()
            }
        };
        let result =
            match run_with_dream_effect_transaction(&mut guard, self.now_ms(), |conn| f(conn)) {
                Ok(result) => result,
                Err(error) => {
                    self.note_fault(op, MemoryFault::Query, "dream_fence");
                    return Err(error);
                }
            };
        match result {
            Ok(v) => {
                self.health.record_success();
                Ok(v)
            }
            Err(e) => {
                self.note_fault(op, MemoryFault::Query, "error");
                Err(e)
            }
        }
    }

    /// [`Self::with_conn_reported`] for the writes that need `&mut Connection`.
    pub(super) fn with_conn_mut_reported<T>(
        &self,
        op: &'static str,
        f: impl FnOnce(&mut Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                // Recover rather than refuse — see `with_conn`.
                self.note_fault(op, MemoryFault::LockPoisoned, "lock_poisoned");
                poisoned.into_inner()
            }
        };
        let result = match run_with_dream_effect_transaction(&mut guard, self.now_ms(), f) {
            Ok(result) => result,
            Err(error) => {
                self.note_fault(op, MemoryFault::Query, "dream_fence");
                return Err(error);
            }
        };
        match result {
            Ok(v) => {
                self.health.record_success();
                Ok(v)
            }
            Err(e) => {
                self.note_fault(op, MemoryFault::Query, "error");
                Err(e)
            }
        }
    }

    /// Attach the internal event bus (design §2.2). Same handoff pattern as [`Self::with_embedder`]:
    /// the daemon composition owns the [`crate::bus::Bus`] and hands this handle over; without it,
    /// ingest publishes nothing and behaviour is unchanged.
    pub fn with_bus(mut self, bus: crate::bus::Bus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Hand over the anonymous-analytics handle (the shell's opt-out PostHog path, #61), so core
    /// code can emit its own counts-only events (#286: `dream_batch_sent`). Every clone sees it,
    /// including ones taken earlier. The first handle wins; returns whether this one was taken.
    /// The handle's `opt_out` gate still decides whether anything leaves.
    pub fn set_analytics(&self, handle: crate::analytics::AnalyticsHandle) -> bool {
        self.analytics.set(handle).is_ok()
    }

    /// Attach the local embedding model, turning search from lexical-only into hybrid.
    ///
    /// Taken as a handle rather than constructed here so this crate stays free of the model
    /// runtime: the desktop loads the bundled model and hands it over, and tests inject a
    /// deterministic one.
    pub fn with_embedder(mut self, embedder: Arc<dyn shogun_memory::embed::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Inject the compression config (unset = the raw path stays in effect). Same handoff pattern
    /// as [`with_embedder`]: the desktop decides whether compression is on (behind a flag) and hands
    /// the config over.
    pub fn with_compression_config(
        mut self,
        config: shogun_fusion::compress::CompressionConfig,
    ) -> Self {
        self.compression_config = Some(config);
        self
    }

    /// Keep the last Dream outcome and its reason at `path` (issue #285) — the shared app-data
    /// `dream_outcome.json`, so the desktop and every API face read the same record. `None` (the
    /// default) records nothing.
    pub fn with_dream_outcome_path(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.dream_outcome_path = path.map(Arc::new);
        self
    }

    /// Read Visual recall's switch and retention from `path` (`visual_recall.json`) before chat and
    /// context assembly read saved screens (issue #782).
    pub fn with_visual_recall_settings_path(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.visual_recall_settings_path = path.map(Arc::new);
        self
    }

    /// The current compression config (`None` when unset).
    pub fn compression_config(&self) -> Option<&shogun_fusion::compress::CompressionConfig> {
        self.compression_config.as_ref()
    }

    /// The attached embedder, if any.
    pub fn embedder(&self) -> Option<&Arc<dyn shogun_memory::embed::Embedder>> {
        self.embedder.as_ref()
    }

    /// The query vector for `text`, or `None` when search must stay lexical: no embedder, a model
    /// failure, or an embedder whose model is not the store's active space yet (E24). The last
    /// case is a model switch whose re-embed job has not run: comparing the new model's query
    /// against the old model's vectors would rank nonsense, so the vector lane sits out instead.
    pub(crate) fn query_embedding(&self, text: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_deref()?;
        let model_id = embedder.model_id().to_string();
        let active = self
            .with_conn("embed.space", |conn| {
                shogun_memory::embedding_space::matches(conn, &model_id)
            })
            .unwrap_or(false);
        if !active {
            return None;
        }
        embedder.embed_query(text).ok()
    }

    /// [`Self::query_embedding`] for a caller that already holds the connection lock (taking it
    /// again would deadlock).
    pub(crate) fn query_embedding_on(&self, conn: &Connection, text: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_deref()?;
        if !shogun_memory::embedding_space::matches(conn, embedder.model_id()).unwrap_or(false) {
            return None;
        }
        embedder.embed_query(text).ok()
    }

    /// Embed canonical events and clean distilled memory that lack vectors. Both stay off the
    /// write path, so a slow model never delays capture or state persistence.
    pub fn embed_pending(&self, limit: usize) -> usize {
        let Some(e) = self.embedder.as_deref() else {
            return 0;
        };
        let events = self
            .with_conn_mut("embed.pending", |conn| {
                shogun_memory::embed_job::embed_all_pending(conn, e, limit)
            })
            .unwrap_or(0);
        let documents = self
            .with_conn_mut("embed.documents", |conn| {
                shogun_memory::document_vector::embed_all_pending(conn, e, limit, self.now_ms())
            })
            .unwrap_or(0);
        events.saturating_add(documents)
    }

    /// Open the on-disk database (runs migrations) and wrap it.
    pub fn open(path: impl AsRef<std::path::Path>, clock: Clock) -> Result<Self, MemoryError> {
        let db = Self::new(shogun_memory::open(path)?, clock);
        db.start_hot_rebuild();
        Ok(db)
    }

    /// Open the encrypted on-device database (memory at rest). The key comes from the caller —
    /// the macOS layer reads it from the Keychain, its only permitted home (invariant 7).
    pub fn open_encrypted(
        path: impl AsRef<std::path::Path>,
        key: &shogun_memory::DbKey,
        clock: Clock,
    ) -> Result<Self, MemoryError> {
        let db = Self::new(shogun_memory::open_encrypted(path, key)?, clock);
        db.start_hot_rebuild();
        Ok(db)
    }

    /// Open a fresh in-memory database (migrations applied) — for tests and ephemeral use.
    pub fn open_in_memory(clock: Clock) -> Result<Self, MemoryError> {
        let db = Self::new(shogun_memory::open_in_memory()?, clock);
        let _ = db.rebuild_hot_now();
        Ok(db)
    }
}

/// The store as a headless face (`shogun-mcp` / `shogun-api`) opens it (#800). It never migrates
/// and never creates the file: only the desktop upgrades the store, after its verified
/// pre-migration snapshot, so an MCP client starting first after an app update cannot run new
/// migrations unbacked or race the app. Memory is served only while the schema is exactly
/// [`shogun_memory::LATEST_SCHEMA_VERSION`]; [`Self::unavailable`] re-checks on every call, so
/// the app upgrading the store while this process runs needs no restart.
#[derive(Clone)]
pub struct DaemonDb {
    db: Db,
    /// Live pragmas applied and Hot started — done once, the first time the schema is current.
    started: Arc<std::sync::atomic::AtomicBool>,
}

impl DaemonDb {
    /// Open the existing encrypted store at `path` with the daemons' key (`SHOGUN_DB_KEY`, else
    /// the Keychain on macOS). A missing or still-plaintext store is the app's first launch, not
    /// this process's: refused with `memory_update_pending`, nothing created.
    pub fn open(path: impl AsRef<std::path::Path>, clock: Clock) -> Result<Self, String> {
        let path = path.as_ref();
        // A path that cannot be a database is named as such before any key is read (issue
        // #779): a directory or an empty file is not reported as "encrypted memory DB requires
        // SHOGUN_DB_KEY", and never prompts for the Keychain key on macOS.
        if path.is_dir() {
            return Err("this path is a directory, not a memory database file".to_string());
        }
        if path.metadata().is_ok_and(|meta| meta.is_file() && meta.len() == 0) {
            return Err("this file is empty, not a memory database".to_string());
        }
        if !path.exists() || shogun_memory::is_plaintext_db(path) {
            return Err(MemoryUnavailable::UpdatePending.line());
        }
        Self::open_with_key(path, &load_db_encryption_key()?, clock)
    }

    pub(crate) fn open_with_key(
        path: &std::path::Path,
        key: &shogun_memory::DbKey,
        clock: Clock,
    ) -> Result<Self, String> {
        // The opener refuses symlinks; resolve the folder (macOS `/var` → `/private/var`, a
        // symlinked home) but never the file itself.
        let resolved = match (path.parent(), path.file_name()) {
            (Some(dir), Some(name)) => dir
                .canonicalize()
                .map(|dir| dir.join(name))
                .unwrap_or_else(|_| path.to_path_buf()),
            _ => path.to_path_buf(),
        };
        // No create, no pragmas, no migrations: only reads `sqlite_schema` to prove the key.
        let conn = shogun_memory::open_encrypted_existing_for_upgrade(&resolved, key)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            db: Db::new(conn, clock),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// The handle every face wraps. Gate each memory call on [`Self::unavailable`].
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Why memory cannot be served right now, `None` when it can. One `MAX(version)` read per
    /// call — never a write — so an app upgrade (or a newer app's) takes effect on the next call.
    /// The first serve also requires [`shogun_memory::verify_history`] to pass.
    pub fn unavailable(&self) -> Option<MemoryUnavailable> {
        use std::sync::atomic::Ordering;
        {
            let mut conn = match self.db.conn.lock() {
                Ok(conn) => conn,
                Err(poisoned) => poisoned.into_inner(),
            };
            // An unreadable history (no table yet) is an uninitialised store: pending.
            let version = shogun_memory::schema_version(&conn).ok().flatten();
            if let Some(why) =
                MemoryUnavailable::for_schema(version, shogun_memory::LATEST_SCHEMA_VERSION)
            {
                return Some(why);
            }
            if self.started.load(Ordering::Acquire) {
                return None;
            }
            // Before the first serve, the whole history must be one the app's refinery run would
            // accept (no divergent row, no gap); MAX(version) alone cannot see either. Not
            // started on refusal, so the next call re-checks.
            if shogun_memory::verify_history(&mut conn).is_err()
                || shogun_memory::apply_live_pragmas(&conn).is_err()
            {
                return Some(MemoryUnavailable::UpdatePending);
            }
        }
        if !self.started.swap(true, Ordering::AcqRel) {
            self.db.start_hot_rebuild();
        }
        None
    }
}

/// The database key the daemons use: `SHOGUN_DB_KEY`, else the Keychain entry (the Mac app's
/// build, `desktop::keychain::memory_db_key`). Also used to verify the newest backup snapshot
/// before an immediate retention prune (#647).
#[cfg(feature = "db")]
pub(crate) fn load_db_encryption_key() -> Result<shogun_memory::DbKey, String> {
    if let Ok(hex) = std::env::var("SHOGUN_DB_KEY") {
        let trimmed = hex.trim();
        if !trimmed.is_empty() {
            return shogun_memory::DbKey::from_hex(trimmed)
                .ok_or_else(|| "SHOGUN_DB_KEY is not valid hex".to_string());
        }
    }
    #[cfg(feature = "desktop")]
    {
        crate::desktop::keychain::memory_db_key()
    }
    #[cfg(not(feature = "desktop"))]
    {
        Err("encrypted memory DB requires SHOGUN_DB_KEY".to_string())
    }
}

#[cfg(test)]
mod daemon_db_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use shogun_memory::{schema_version, DbKey, LATEST_SCHEMA_VERSION};

    const LATEST: i64 = LATEST_SCHEMA_VERSION as i64;

    struct Store(std::path::PathBuf);

    impl Drop for Store {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key() -> DbKey {
        DbKey::new([0x5a; 32])
    }

    /// Issue #779: a directory or an empty file is named for what it is, never reported as an
    /// encrypted store that needs a key (which on macOS would first read the Keychain).
    #[test]
    fn a_directory_or_an_empty_file_is_not_called_an_encrypted_store() {
        let (_guard, path) = store("not-a-db");
        let dir = path.parent().unwrap().join("isdir.db");
        std::fs::create_dir_all(&dir).unwrap();
        let clock: Clock = Arc::new(|| 0);
        let error = DaemonDb::open(&dir, clock.clone()).err().unwrap();
        assert!(error.contains("directory"), "{error}");
        let empty = path.parent().unwrap().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        let error = DaemonDb::open(&empty, clock).err().unwrap();
        assert!(error.contains("empty"), "{error}");
        assert!(!error.contains("SHOGUN_DB_KEY"), "{error}");
    }

    /// A current encrypted store, as the app leaves it.
    fn store(tag: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "shogun-daemon-db-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.db");
        drop(shogun_memory::open_encrypted(&path, &key()).unwrap());
        (Store(dir), path)
    }

    /// The app's own connection to the same file, without migrating it.
    fn app(path: &std::path::Path) -> Connection {
        let path = path.parent().unwrap().canonicalize().unwrap().join("memory.db");
        shogun_memory::open_encrypted_existing_for_upgrade(path, &key()).unwrap()
    }

    fn history(path: &std::path::Path) -> (Option<u32>, i64) {
        let conn = app(path);
        let rows = conn
            .query_row("SELECT count(*) FROM refinery_schema_history", [], |r| r.get(0))
            .unwrap();
        (schema_version(&conn).unwrap(), rows)
    }

    fn open(path: &std::path::Path) -> DaemonDb {
        DaemonDb::open_with_key(path, &key(), Arc::new(|| 1_000)).unwrap()
    }

    #[test]
    fn an_older_store_is_refused_untouched_then_served_once_the_app_upgrades_it() {
        let (_dir, path) = store("older");
        // The app's previous version: the newest migration not yet recorded.
        let conn = app(&path);
        let (name, applied_on, checksum): (String, String, String) = conn
            .query_row(
                "SELECT name, applied_on, checksum FROM refinery_schema_history WHERE version = ?1",
                [LATEST],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        conn.execute("DELETE FROM refinery_schema_history WHERE version = ?1", [LATEST])
            .unwrap();
        drop(conn);
        let before = history(&path);

        let daemon = open(&path);
        assert_eq!(daemon.unavailable(), Some(MemoryUnavailable::UpdatePending));
        assert_eq!(daemon.unavailable(), Some(MemoryUnavailable::UpdatePending));
        assert_eq!(history(&path), before, "the daemon ran no migration");
        // Below the newest; not necessarily newest − 1 (numbers reserved by other branches leave gaps).
        assert!(before.0.is_some_and(|v| v < LATEST_SCHEMA_VERSION), "{before:?}");

        // The app upgrades the store while the daemon keeps running.
        app(&path)
            .execute(
                "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![LATEST, name, applied_on, checksum],
            )
            .unwrap();
        assert_eq!(daemon.unavailable(), None, "the same handle serves on the next call");
        assert!(daemon.db().try_open_loops().is_ok());
    }

    #[test]
    fn a_current_store_is_served() {
        let (_dir, path) = store("current");
        let daemon = open(&path);
        assert_eq!(daemon.unavailable(), None);
        assert!(daemon.db().try_open_loops().is_ok());
    }

    #[test]
    fn a_tampered_history_at_latest_is_refused_untouched_then_served_once_restored() {
        let (_dir, path) = store("tampered");
        app(&path)
            .execute(
                "UPDATE refinery_schema_history SET name = 'brief_generation' WHERE version = 54",
                [],
            )
            .unwrap();
        let digest = |path: &std::path::Path| -> Vec<String> {
            let conn = app(path);
            let mut stmt = conn
                .prepare(
                    "SELECT json_array(version, name, applied_on, checksum) FROM refinery_schema_history ORDER BY version",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
            rows
        };
        let before = digest(&path);
        assert_eq!(history(&path).0, Some(LATEST_SCHEMA_VERSION), "max(version) alone looks current");

        let daemon = open(&path);
        assert_eq!(daemon.unavailable(), Some(MemoryUnavailable::UpdatePending));
        assert_eq!(daemon.unavailable(), Some(MemoryUnavailable::UpdatePending));
        assert_eq!(daemon.unavailable().map(MemoryUnavailable::code), Some("memory_update_pending"));
        assert_eq!(digest(&path), before, "the daemon wrote nothing");

        app(&path)
            .execute(
                "UPDATE refinery_schema_history SET name = 'dream_batch_send_stats' WHERE version = 54",
                [],
            )
            .unwrap();
        assert_eq!(daemon.unavailable(), None, "the same handle serves once the history matches");
        assert!(daemon.db().try_open_loops().is_ok());
    }

    #[test]
    fn a_store_newer_than_this_binary_is_refused() {
        let (_dir, path) = store("newer");
        app(&path)
            .execute(
                "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) VALUES (?1, 'future', '', '0')",
                [LATEST + 1],
            )
            .unwrap();
        let before = history(&path);
        let daemon = open(&path);
        assert_eq!(daemon.unavailable(), Some(MemoryUnavailable::NewerThanApp));
        assert_eq!(history(&path), before);
    }

    #[test]
    fn a_missing_store_is_refused_and_never_created() {
        let (dir, _) = store("missing");
        let missing = dir.0.join("absent.db");
        let refused = DaemonDb::open(&missing, Arc::new(|| 1_000)).err().unwrap();
        assert_eq!(refused, MemoryUnavailable::UpdatePending.line());
        assert!(!missing.exists());
    }
}
