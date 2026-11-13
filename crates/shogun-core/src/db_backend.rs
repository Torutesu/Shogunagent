//! The daemon's Memory API data backend (§6.11). Feature `db`. Implements shogun-mcp's
//! [`MemoryBackend`] over the daemon's [`Db`], so the REST / CLI / MCP faces read real state.
//!
//! Dependency-inverted: the trait is owned by the API layer (shogun-mcp) and implemented here by
//! the daemon — the API layer never depends on the core. The confidence gate stays in the API
//! layer (FR-API-06); this backend only supplies rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use shogun_mcp::backend::{
    CurrentContextAvailability, CurrentContextProvider, CurrentContextSnapshot, MemoryBackend,
    MemorySearchError, MemorySearchResponse, MemorySearchResult, ReadItem, ReadParams,
    SearchRequestV1, WriteResult,
};
use shogun_mcp::memory_api::Tool;
use shogun_mcp::memory_api_settings::{
    try_load_settings as load_memory_api_settings, save_settings as save_memory_api_settings,
    normalize_prefs, validate_profile, Settings as MemoryApiSettings,
};
use shogun_mcp::memory_import_api::MemoryImportOperation;
use shogun_mcp::voice_dictionary_api::{VoiceDictionaryOperation, VoiceDictionaryResult};

use crate::capture::visual_recall::{
    try_update_settings_with_commit, with_locked_settings, RetentionPolicy, Settings, DAY_MS,
};
use crate::daemon::{local_day_bounds, Db};

pub(crate) fn source_kind(hit: &shogun_memory::search::FederatedHit) -> String {
    match &hit.source_kind {
        shogun_memory::search::FederatedSourceKind::UserNote => "user_note".into(),
        shogun_memory::search::FederatedSourceKind::Event => "event".to_string(),
        shogun_memory::search::FederatedSourceKind::Derived(kind) => {
            format!("derived:{}", kind.as_str())
        }
        shogun_memory::search::FederatedSourceKind::Meeting => "meeting".to_string(),
        // `imported:<platform>` — the same provenance label the import screen and
        // `memory.import.list` show, so a caller can never mistake it for observed memory.
        shogun_memory::search::FederatedSourceKind::Imported(platform) => {
            platform.provenance().to_string()
        }
    }
}

pub(crate) fn source_id(hit: &shogun_memory::search::FederatedHit) -> String {
    match &hit.source_id {
        shogun_memory::search::FederatedSourceId::UserNote(id) => format!("user_note:{id}"),
        shogun_memory::search::FederatedSourceId::Event(id)
        | shogun_memory::search::FederatedSourceId::Meeting(id) => id.to_string(),
        shogun_memory::search::FederatedSourceId::Derived(source) => {
            format!("{}:{}", source.table(), source.id())
        }
        shogun_memory::search::FederatedSourceId::Imported(id) => {
            format!("memory_import_items:{id}")
        }
    }
}

pub(crate) fn evidence_id(evidence: &shogun_memory::search::CanonicalEvidenceId) -> String {
    use shogun_memory::search::CanonicalEvidenceId;

    match evidence {
        CanonicalEvidenceId::UserNote(id) => format!("user_note:{id}"),
        CanonicalEvidenceId::Event(id) => format!("event:{id}"),
        CanonicalEvidenceId::Session(id) => format!("session:{id}"),
        CanonicalEvidenceId::TranscriptSegment(id) => format!("transcript_segment:{id}"),
        CanonicalEvidenceId::SessionNote(id) => format!("session_note:{id}"),
        CanonicalEvidenceId::MeetingRecap(id) => format!("meeting_recap:{id}"),
        CanonicalEvidenceId::Brief(date) => format!("brief:{date}"),
        CanonicalEvidenceId::FeedbackEvent(id) => format!("feedback_event:{id}"),
        CanonicalEvidenceId::PeriodSummary(id) => format!("period_summary:{id}"),
        CanonicalEvidenceId::MemoryImport(id) => format!("memory_import:{id}"),
    }
}

/// Event ids a search hit's `open` hands to `memory.get_events` (own event first, then newest).
const SEARCH_OPEN_IDS: usize = 5;
/// Bound on a search hit's `text`.
const SEARCH_TEXT_CHARS: usize = 500;

/// `where` on a search hit: app and window title of the event, null keys omitted. `event_log`
/// stores no page URL, so `url` / `domain` never appear here.
fn where_json(place: &shogun_memory::search::EventPlace) -> Option<serde_json::Value> {
    let mut out = serde_json::Map::new();
    if let Some(app) = &place.app {
        out.insert("app".into(), app.clone().into());
    }
    if let Some(title) = &place.window_title {
        out.insert("window_title".into(), title.clone().into());
    }
    (!out.is_empty()).then(|| out.into())
}

/// Max search hits returned by `memory.search` over the API.
/// The Dream actions (`run_now`, `retry_full`) run only in the SHOGUN app: the run needs the
/// app's verified backup and its single-run guard, which no API process holds. The API face says
/// so on each action (`"api": false`, issue #779), so an agent that sees `run_now` enabled tells
/// the user where to press it instead of looking for a tool that does not exist. The app's own
/// Settings row reads the same status without this marker.
fn mark_dream_actions_app_only(status: &mut serde_json::Value) {
    if let Some(actions) = status.get_mut("actions").and_then(serde_json::Value::as_array_mut) {
        for action in actions.iter_mut().filter_map(serde_json::Value::as_object_mut) {
            action.insert("api".to_string(), serde_json::Value::Bool(false));
        }
    }
}

/// A write the store could not take right now (busy, locked, I/O). Deliberately not shaped like
/// a caller error or "… not found", so REST answers it with a retryable 500 (issue #779).
const MEMORY_WRITE_UNAVAILABLE: &str = "memory store unavailable";

const SEARCH_LIMIT: usize = 20;
/// Max frame hits for `visual_recall.search_frames`.
const FRAME_SEARCH_LIMIT: usize = 20;
/// OCR excerpt length for frame search / status previews.
const FRAME_EXCERPT_CHARS: usize = 200;
/// Recent durable activity included in query-free `memory.get_context` snapshots.
const CONTEXT_ACTIVITY_LIMIT: usize = 8;
/// Per-activity text cap keeps one captured window from consuming the whole snapshot.
const CONTEXT_ACTIVITY_EXCERPT_CHARS: usize = 320;
/// Evidence cap for `memory.get_context_pack` (FR-API-08) — matches the chat path's scale so the
/// pack an external AI receives is the same grounded slice the in-app chat reads.
const PACK_HITS: usize = 10;
/// Per-evidence excerpt cap for the pack, so one huge window capture cannot eat the whole pack.
const PACK_EXCERPT_CHARS: usize = 300;
/// Confidence assigned to a captured event in search results: events are ground truth, not inferred
/// state, so they always pass the confidence gate (they are not "possibly").
const EVENT_CONFIDENCE: f64 = 1.0;

fn text_sources(accessibility_text: &str, ocr_text: &str) -> Vec<&'static str> {
    let mut sources = Vec::with_capacity(2);
    if !accessibility_text.trim().is_empty() {
        sources.push("accessibility");
    }
    if !ocr_text.trim().is_empty() {
        sources.push("ocr");
    }
    sources
}
const CURRENT_CONTEXT_LINE_LIMIT: usize = 8;
const CURRENT_CONTEXT_LINE_CHARS: usize = 320;

/// The `capture_exclusions.set` body: the complete editable lists.
#[derive(serde::Deserialize)]
struct CaptureExclusionsPayload {
    apps: Vec<String>,
    websites: Vec<String>,
}

impl CaptureExclusionsPayload {
    fn parse(body: &str) -> Result<Self, String> {
        serde_json::from_str(body).map_err(|_| "invalid capture exclusions request".to_string())
    }
}

/// A [`MemoryBackend`] backed by the daemon's DB handle.
pub struct DbBackend {
    db: Db,
    forms_db_path: Option<PathBuf>,
    visual_recall_settings_path: Option<PathBuf>,
    capture_exclusions_path: Option<PathBuf>,
    memory_api_settings_path: Option<PathBuf>,
    onboarding_state_path: Option<PathBuf>,
    /// `account.json` (#148 P2), the desktop's account snapshot for `device.account.get`.
    account_state_path: Option<PathBuf>,
    /// `Shougun.md`, shared with the desktop Writing Style card. A configured daemon reloads it
    /// for each API read, matching the desktop file watcher without ever creating or rewriting it.
    user_config_path: Option<PathBuf>,
    current_context_provider: Option<Arc<dyn CurrentContextProvider>>,
    agent_source: Option<AgentSourceConfig>,
    backup: Option<BackupRetentionConfig>,
    connections: Option<shogun_mcp::connections_api::ConnectionsPaths>,
    /// A hosted sidecar's connector vault and the principal this store belongs to (E50 / E44):
    /// `connections.list` / `connections.disconnect` answer from the principal's own grants, and a
    /// provider change notice is queued for the read-sync. `None` everywhere else.
    hosted_connectors: Option<(Arc<dyn shogun_mcp::connector_vault::ConnectorVault>, String)>,
    /// `steer_runs.json` (issue #395): notes for runs the desktop is running.
    steer_store: Option<PathBuf>,
    /// "Compact search index" (#714 step 3): the desktop job's request / status files and the
    /// database volume's free space.
    search_index: Option<SearchIndexConfig>,
    /// What `dream.status` knows beyond the ledger when it lists the night's actions (issue #285).
    /// `None`: every such fact is unknown.
    dream_action_facts: Option<DreamActionFacts>,
    /// How long `translate.text` / `translate.reply_draft` wait for the desktop (issue #279).
    translate_wait: TranslateWait,
    /// How long `memory.search` with `mode: "ai"` waits for the desktop (issue #940 B2b).
    ai_search_wait: TranslateWait,
    /// `api.connections.restrict` (owner decision 2026-10-20): withhold meeting notes and screen
    /// recall from one connected app in the Keychain list. `None`: unavailable here.
    connection_restrictor: Option<ConnectionRestrictor>,
}

/// Withholds meeting notes and screen recall from one connection, given its id. `Ok(changed)`.
pub type ConnectionRestrictor = Arc<dyn Fn(&str) -> Result<bool, String> + Send + Sync>;

/// The standalone faces' [`ConnectionRestrictor`]: the Keychain item the app keeps the
/// connections in, rewritten only when something changed, then the revision marker beside
/// `memory_api.json` (`settings_path`) so running sessions re-read it at their next call. Only
/// ever narrows (`memory_api_settings::withhold_in_store`). The Mac app's build only
/// (`desktop::keychain::connection_restrictor`); `None` elsewhere.
pub fn keychain_connection_restrictor(settings_path: PathBuf) -> Option<ConnectionRestrictor> {
    #[cfg(feature = "desktop")]
    {
        crate::desktop::keychain::connection_restrictor(settings_path)
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = settings_path;
        None
    }
}

/// How long a translate request is held open (issue #279). The desktop claims a request within a
/// fraction of a second when it is running; the model then takes seconds.
#[derive(Debug, Clone, Copy)]
pub struct TranslateWait {
    /// Hand back a job id to poll after this long.
    pub total: std::time::Duration,
    /// A request nobody claimed after this long, while nothing else is running, means the app is
    /// not running: it is discarded and the caller told so.
    pub claim_grace: std::time::Duration,
    pub poll: std::time::Duration,
}

impl Default for TranslateWait {
    fn default() -> Self {
        Self {
            total: std::time::Duration::from_secs(45),
            claim_grace: std::time::Duration::from_secs(5),
            poll: std::time::Duration::from_millis(100),
        }
    }
}

/// Reads the facts `dream.status` needs beyond the ledger, at answer time (issue #285).
pub type DreamActionFacts =
    Arc<dyn Fn() -> crate::dreamcycle::actions::ExternalFacts + Send + Sync>;

/// The standalone faces' Dream action facts (issue #285): the plan from the desktop's
/// `onboarding.json` + `billing.json`, and whether this Mac is provisioned for the Batch relay —
/// an entitling licence or the running trial (`shogun_mcp::trial_relay::batch_provisioned`, the
/// same answer the desktop's Dream status gives). Both re-read on every answer. Battery and the
/// relay's remaining cap are not visible from here and stay unknown. No Keychain access; no token
/// is kept.
pub fn dream_action_facts_for_daemon() -> crate::dreamcycle::actions::ExternalFacts {
    use shogun_mcp::plan_source::FilePlanSource;
    use shogun_mcp::trial_relay::RelayFiles;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let plan_active = FilePlanSource::from_env()
        .resolve(now_ms)
        .background_intelligence;
    let batch_lane = RelayFiles::read().batch_provisioned(now_ms);
    crate::dreamcycle::actions::ExternalFacts {
        plan_active: Some(plan_active),
        batch_lane: Some(batch_lane),
        ..crate::dreamcycle::actions::ExternalFacts::unknown()
    }
}

/// Where `search_index.*` find the desktop job's files, and how they read free disk space.
pub struct SearchIndexConfig {
    pub paths: shogun_mcp::search_index_api::SearchIndexPaths,
    pub free: Arc<dyn crate::search_index_compact::FreeSpace>,
}

impl SearchIndexConfig {
    /// The daemons' configuration: the files beside the DB, free space on its volume.
    pub fn for_daemon(db_path: &str) -> Self {
        Self {
            paths: shogun_mcp::search_index_api::SearchIndexPaths::for_db(db_path),
            free: Arc::new(crate::search_index_compact::VolumeFreeSpace::for_db(db_path)),
        }
    }
}

/// Where `backup.status` / `backup.set_retention` find the snapshot retention setting and the
/// snapshots themselves (issue #647).
pub struct BackupRetentionConfig {
    /// `backup.json`, shared with the desktop.
    pub settings_path: PathBuf,
    /// `<app-data>/backups/`.
    pub backups_dir: PathBuf,
    /// Loads the database key so a shortened retention can prune at once: the newest snapshot has
    /// to verify before anything older is removed. `None` (or a key that will not load) saves the
    /// setting only; the desktop applies it at its next sweep (launch, then every 30 minutes).
    pub key: Option<DbKeyLoader>,
}

/// Loads the memory database key on demand (`None` = unavailable).
pub type DbKeyLoader = Arc<dyn Fn() -> Option<shogun_memory::DbKey> + Send + Sync>;

impl BackupRetentionConfig {
    /// The daemons' configuration: `backup.json` and `backups/` beside the database, and the key
    /// the daemon itself opened the database with.
    pub fn for_daemon(db_path: &str) -> Self {
        let db_path = Path::new(db_path);
        Self {
            settings_path: crate::backup_retention::resolve_path(db_path),
            backups_dir: db_path
                .parent()
                .map(|dir| dir.join("backups"))
                .unwrap_or_else(|| PathBuf::from("backups")),
            key: Some(Arc::new(|| crate::daemon::load_db_encryption_key().ok())),
        }
    }
}

/// Where `agent.source.*` finds the Agent lane settings, and how it observes what is available
/// (issue #110). The probes are injected so Linux tests never spawn a vendor CLI or touch a
/// Keychain.
pub struct AgentSourceConfig {
    /// `llm.json`, shared with the desktop.
    pub path: PathBuf,
    /// Runs each delegate's `--version` (no network, no quota, no credential read). `None`: the
    /// answer reports installation as unknown.
    pub delegates: Option<Arc<dyn crate::llm::subscription::CommandRunner>>,
    /// Whether a provider's API key is in the Keychain, answered without reading the key.
    /// `None` = unknown.
    pub key_present: KeyPresence,
}

/// Answers whether a provider's API key is in the Keychain, without reading it (`None` = unknown).
pub type KeyPresence = Arc<dyn Fn(&str) -> Option<bool> + Send + Sync>;

impl AgentSourceConfig {
    /// The daemons' configuration (`shogun-mcp`, `shogun-api`): `llm.json` beside the DB (or
    /// `SHOGUN_LLM_SETTINGS`), real `--version` probes, and a Keychain presence check that never
    /// reads the key (macOS; unknown elsewhere).
    pub fn for_daemon(db_path: &str) -> Self {
        Self {
            path: crate::agent_source::resolve_path(db_path),
            delegates: Some(Arc::new(crate::llm::subscription::ProcessRunner)),
            key_present: Arc::new(daemon_key_present),
        }
    }
}

#[cfg(feature = "desktop")]
fn daemon_key_present(provider: &str) -> Option<bool> {
    crate::desktop::keychain::daemon_key_present(provider)
}

#[cfg(not(feature = "desktop"))]
fn daemon_key_present(_provider: &str) -> Option<bool> {
    None
}

/// The most rows one `profile.whoami` list carries; `count` / `<list>_total` keep the real total.
const WHOAMI_LIST_CAP: usize = 20;
/// One whoami line's most chars (a commitment's wording can be a paragraph).
const WHOAMI_ITEM_CHARS: usize = 160;

struct WhoamiSplit {
    strong: Vec<String>,
    weak: Vec<String>,
}

/// High band → `strong`, Medium → `weak` (`work.inferred`), Low (unreviewed) → dropped.
fn whoami_split(items: impl Iterator<Item = (String, f64)>) -> WhoamiSplit {
    use shogun_fusion::confidence::{band, Band};
    let mut split = WhoamiSplit { strong: Vec::new(), weak: Vec::new() };
    for (name, confidence) in items {
        match band(confidence) {
            Band::High => split.strong.push(name),
            Band::Medium => split.weak.push(name),
            Band::Low => {}
        }
    }
    split
}

fn capped(names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .take(WHOAMI_LIST_CAP)
        .map(|n| match n.char_indices().nth(WHOAMI_ITEM_CHARS) {
            Some((cut, _)) => format!("{}…", &n[..cut]),
            None => n,
        })
        .collect()
}

impl DbBackend {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            forms_db_path: None,
            visual_recall_settings_path: None,
            capture_exclusions_path: None,
            memory_api_settings_path: None,
            onboarding_state_path: shogun_mcp::plan_source::onboarding_json_path(),
            account_state_path: shogun_mcp::plan_source::account_json_path(),
            user_config_path: None,
            current_context_provider: None,
            agent_source: None,
            steer_store: None,
            backup: None,
            connections: None,
            hosted_connectors: None,
            search_index: None,
            dream_action_facts: None,
            translate_wait: TranslateWait::default(),
            ai_search_wait: TranslateWait {
                // The refine stops at 20 s; the rest is the claim and a margin.
                total: std::time::Duration::from_secs(30),
                ..TranslateWait::default()
            },
            connection_restrictor: None,
        }
    }

    /// Where `api.connections.restrict` withholds meeting notes and screen recall (owner decision
    /// 2026-10-20); [`keychain_connection_restrictor`] in the standalone faces.
    pub fn with_connection_restrictor(mut self, restrictor: Option<ConnectionRestrictor>) -> Self {
        self.connection_restrictor = restrictor;
        self
    }

    /// The desktop form runtime lives beside the actual database, including debug fixtures.
    pub fn with_forms_db_path(mut self, path: PathBuf) -> Self {
        self.forms_db_path = Some(path);
        self
    }

    /// Explicit saved-state location for isolated fixtures or a desktop with another bundle id.
    pub fn with_onboarding_state_path(mut self, path: PathBuf) -> Self {
        self.onboarding_state_path = Some(path);
        self
    }

    /// Explicit account snapshot location for isolated fixtures (#148 P2).
    pub fn with_account_state_path(mut self, path: PathBuf) -> Self {
        self.account_state_path = Some(path);
        self
    }

    /// Change how long translate requests are held open (tests use milliseconds).
    pub fn with_translate_wait(mut self, wait: TranslateWait) -> Self {
        self.translate_wait = wait;
        self
    }

    /// Change how long AI search requests are held open (tests use milliseconds).
    pub fn with_ai_search_wait(mut self, wait: TranslateWait) -> Self {
        self.ai_search_wait = wait;
        self
    }

    /// `memory.search` with `mode: "ai"` (issue #940 B2b): queue the question for the desktop and
    /// wait for its answer; keyword results with a reason when AI may not run for this caller,
    /// nothing claims the request (the app is not running), the answer is late, or the desktop
    /// could not run it. The caller always gets results, never a job to poll.
    fn ai_search_request(
        &self,
        query: &str,
        origin: &str,
        ai: Result<(), &'static str>,
    ) -> Result<String, String> {
        use crate::ai_search::FallbackReason;
        use shogun_mcp::ai_search_api::{
            refusal_json, AI_NOT_IN_PLAN, READ_ONLY_TOKEN, SOURCES_WITHHELD,
        };
        let fallback = |reason: FallbackReason| -> Result<String, String> {
            Ok(match self.db.ai_search_keyword_fallback_json(query, reason) {
                Ok(answer) => answer.to_string(),
                Err(code) => refusal_json(code),
            })
        };
        match ai {
            Ok(()) => {}
            Err(code) if code == READ_ONLY_TOKEN => return fallback(FallbackReason::ReadOnlyToken),
            Err(code) if code == SOURCES_WITHHELD => {
                return fallback(FallbackReason::SourcesWithheld)
            }
            Err(code) if code == AI_NOT_IN_PLAN => return fallback(FallbackReason::AiNotInPlan),
            Err(_) => return fallback(FallbackReason::AiNotInPlan),
        }
        if query.trim().is_empty() {
            return Ok(refusal_json("empty_query"));
        }
        let job = self.db.ai_search_job_request(query, origin)?;
        let started = std::time::Instant::now();
        loop {
            std::thread::sleep(self.ai_search_wait.poll);
            let Some(current) = self.db.ai_search_job_take(job.id)? else {
                // Swept (older than the TTL): answer from the keyword index.
                return fallback(FallbackReason::ProviderTimeout);
            };
            if current.is_terminal() {
                return match (current.status.as_str(), current.result.as_deref()) {
                    ("done", Some(result)) => Ok(result.to_string()),
                    // The desktop refused (plan, empty question, a store fault): its code.
                    _ => Ok(refusal_json(current.error.as_deref().unwrap_or("search_unavailable"))),
                };
            }
            let waited = started.elapsed();
            if current.status == "pending"
                && waited >= self.ai_search_wait.claim_grace
                && !self.db.ai_search_jobs_any_running()
            {
                // Nothing claimed it and nothing else is running: the app is not there to hold
                // the user's model access. Leave no row behind for it to find later.
                self.db.ai_search_job_discard(job.id);
                return fallback(FallbackReason::AppNotRunning);
            }
            if waited >= self.ai_search_wait.total {
                self.db.ai_search_job_discard(job.id);
                return fallback(FallbackReason::ProviderTimeout);
            }
        }
    }

    /// Render a request the face is handing back: done, failed, or still going.
    fn translate_job_json(job: &shogun_memory::translate_jobs::TranslateJob) -> String {
        use shogun_mcp::translate_api::{done_json, pending_json, refusal_json};
        match job.status.as_str() {
            "done" => {
                let result = job
                    .result
                    .as_deref()
                    .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
                    .unwrap_or(serde_json::Value::Null);
                done_json(job.id, result)
            }
            "failed" => refusal_json(job.error.as_deref().unwrap_or("unavailable"), Some(job.id)),
            status => pending_json(job.id, status),
        }
    }

    /// Queue a translate / reply-draft request and wait for the desktop's answer (issue #279).
    fn translate_request(&self, kind: &str, body: &str, origin: &str) -> Result<String, String> {
        use shogun_mcp::translate_api::refusal_json;
        let request = match crate::translate_job::JobRequest::parse(kind, body) {
            Ok(request) => request,
            Err(e) => return Ok(refusal_json(e.code(), None)),
        };
        let job = self.db.translate_job_request(&request, origin)?;
        let started = std::time::Instant::now();
        loop {
            std::thread::sleep(self.translate_wait.poll);
            let Some(current) = self.db.translate_job_take(job.id)? else {
                // Swept (older than the TTL) or taken by a concurrent status read.
                return Ok(refusal_json("not_found", Some(job.id)));
            };
            if current.is_terminal() {
                return Ok(Self::translate_job_json(&current));
            }
            let waited = started.elapsed();
            if current.status == "pending"
                && waited >= self.translate_wait.claim_grace
                && !self.db.translate_jobs_any_running()
            {
                // Unclaimed and nothing else running, as in `ai_search_request`: no row is left.
                self.db.translate_job_discard(job.id);
                return Ok(refusal_json("app_not_running", None));
            }
            if waited >= self.translate_wait.total {
                return Ok(Self::translate_job_json(&current));
            }
        }
    }

    /// Let `dream.status` read the plan / Batch credential / battery / cap facts from `facts`
    /// when it lists the night's actions (issue #285).
    pub fn with_dream_action_facts(
        mut self,
        facts: impl Fn() -> crate::dreamcycle::actions::ExternalFacts + Send + Sync + 'static,
    ) -> Self {
        self.dream_action_facts = Some(Arc::new(facts));
        self
    }

    /// Serve `search_index.*` (#714 step 3).
    pub fn with_search_index(mut self, config: SearchIndexConfig) -> Self {
        self.search_index = Some(config);
        self
    }

    /// Configure the directives path shared with the desktop's live watcher.
    pub fn with_user_config_path(mut self, path: PathBuf) -> Self {
        self.user_config_path = Some(path);
        self
    }

    fn search_index(&self) -> Result<&SearchIndexConfig, String> {
        self.search_index
            .as_ref()
            .ok_or_else(|| "search index maintenance is not configured".to_string())
    }

    /// Serve `backup.status` / `backup.set_retention` (issue #647).
    pub fn with_backup_retention(mut self, config: BackupRetentionConfig) -> Self {
        self.backup = Some(config);
        self
    }

    fn backup_status_json(&self) -> String {
        let Some(config) = &self.backup else {
            return json!({ "error": "backup settings are not configured" }).to_string();
        };
        let retention = crate::backup_retention::load(&config.settings_path);
        let times = shogun_memory::backup::published_snapshot_times(&config.backups_dir).ok();
        crate::backup_retention::status_json(retention, times.as_deref()).to_string()
    }

    fn set_backup_retention(&self, body: &str) -> Result<(), String> {
        let Some(config) = &self.backup else {
            return Err("backup settings are not configured".to_string());
        };
        let retention = crate::backup_retention::parse_body(body)?;
        crate::backup_retention::save(&config.settings_path, retention)?;
        // Apply at once where this process can verify the newest snapshot. A failure here is not
        // the caller's: the setting is saved, and the desktop's sweep applies it (and retries).
        if let Err(reason) = prune_backups_now(config, retention, self.db.now_ms()) {
            eprintln!("[backup] retention saved; immediate prune deferred: {reason}");
        }
        Ok(())
    }

    /// Serve `connections.*` from the desktop's shared files (issue #82): its published connection
    /// states, Gmail draft-only policy, legacy Composio policy, and pending disconnects.
    pub fn with_connections(
        mut self,
        paths: shogun_mcp::connections_api::ConnectionsPaths,
    ) -> Self {
        self.connections = Some(paths);
        self
    }

    /// Serve `connections.list` / `connections.disconnect` from a hosted sidecar's connector vault
    /// for `principal` (E50), and queue provider change notices for its read-sync (E44).
    #[must_use]
    pub fn with_hosted_connectors(mut self, vault: Arc<dyn shogun_mcp::connector_vault::ConnectorVault>, principal: impl Into<String>) -> Self {
        self.hosted_connectors = Some((vault, principal.into()));
        self
    }

    /// `connections.list` on a hosted sidecar: the principal's grants as the vault reads them
    /// (state and expiry, never a token or an account identity), in the desktop answer's shape.
    fn hosted_connections_json(&self) -> Option<String> {
        let (vault, principal) = self.hosted_connectors.as_ref()?;
        let now = self.db.now_ms();
        Some(match vault.status(principal, now) {
            Ok(states) => {
                let services: Vec<serde_json::Value> = states
                    .iter()
                    .map(|state| {
                        let mut row = shogun_mcp::connector_vault::state_json(state);
                        row["access"] = json!(shogun_mcp::scope::access(state.service).as_str());
                        row["third_party"] = serde_json::Value::Null;
                        row["disconnect_pending"] = json!(false);
                        row
                    })
                    .collect();
                json!({ "hosted": true, "app_running": serde_json::Value::Null, "services": services, "gmail": serde_json::Value::Null, "composio": serde_json::Value::Null })
                    .to_string()
            }
            Err(_) => json!({ "error": "connections_unavailable" }).to_string(),
        })
    }

    /// `connections.disconnect` on a hosted sidecar: the vault deletes the grant and revokes it at
    /// the vendor where it can (`connections.list` then shows it disconnected). The receipt is the
    /// sidecar route's and the gateway's (`connector_vault::disconnected_json`): every face returns
    /// `vendor_revoke` (E51).
    fn hosted_disconnect(&self, body: &str) -> Option<Result<serde_json::Value, String>> {
        let (vault, principal) = self.hosted_connectors.as_ref()?;
        let service = match shogun_mcp::connections_api::parse_disconnect(body) {
            Ok(service) => service,
            Err(invalid) => return Some(Err(shogun_mcp::connections_api::refusal(invalid))),
        };
        Some(
            vault
                .disconnect(principal, service, self.db.now_ms())
                .map(|done| shogun_mcp::connector_vault::disconnected_json(&done))
                .map_err(|_| "connections_unavailable: the credential store did not answer; retry shortly".to_string()),
        )
    }

    fn connections_paths(&self) -> Result<&shogun_mcp::connections_api::ConnectionsPaths, String> {
        self.connections
            .as_ref()
            .ok_or_else(|| "connections are not configured".to_string())
    }

    /// Serve `actions.steer` from this `steer_runs.json` (issue #395), the store the desktop's
    /// runner opens runs in and reads notes from at each turn.
    pub fn with_steer_store(mut self, path: PathBuf) -> Self {
        self.steer_store = Some(path);
        self
    }

    /// Serve `agent.source.get` / `agent.source.set` from this `llm.json` (issue #110).
    pub fn with_agent_source(mut self, config: AgentSourceConfig) -> Self {
        self.agent_source = Some(config);
        self
    }

    fn agent_source_json(&self) -> String {
        let Some(config) = &self.agent_source else {
            return json!({ "error": "agent source settings are not configured" }).to_string();
        };
        let settings = crate::agent_source::load_effective(&config.path);
        let detected = config
            .delegates
            .as_ref()
            .map(|runner| crate::llm::subscription::detect_all(runner.as_ref()));
        crate::agent_source::status_json(&settings, detected.as_ref(), config.key_present.as_ref())
            .to_string()
    }

    /// Attach a host-owned read-only cache. Standalone constructors deliberately omit it.
    pub fn with_current_context_provider(
        mut self,
        provider: Arc<dyn CurrentContextProvider>,
    ) -> Self {
        self.current_context_provider = Some(provider);
        self
    }

    /// Path to `visual_recall.json` (same directory as `memory.db` in the desktop app).
    pub fn with_visual_recall_settings_path(mut self, path: PathBuf) -> Self {
        // Chat and context reads on this face honour the same switch and retention (#782).
        self.db = self.db.clone().with_visual_recall_settings_path(Some(path.clone()));
        self.visual_recall_settings_path = Some(path);
        self
    }

    pub fn with_capture_exclusions_path(mut self, path: PathBuf) -> Self {
        self.capture_exclusions_path = Some(path);
        self
    }

    /// `memory_api.json`: additional explicit opt-in and local profile data.
    pub fn with_memory_api_settings_path(mut self, path: PathBuf) -> Self {
        self.memory_api_settings_path = Some(path);
        self
    }

    fn visual_recall_settings_path(&self) -> Option<&Path> {
        self.visual_recall_settings_path.as_deref()
    }

    fn capture_exclusions_path(&self) -> Option<&Path> {
        self.capture_exclusions_path.as_deref()
    }

    fn capture_exclusions_json(&self) -> String {
        let policy = self.capture_exclusions_path().map_or_else(
            crate::capture::exclusion::ExclusionPolicy::new,
            crate::capture::exclusion::load_policy,
        );
        json!({
            "apps": policy.user_apps(),
            "websites": policy.user_websites(),
            "website_matching": "host and its subdomains; while website rules exist, a known browser whose URL is unavailable is not captured",
        }).to_string()
    }

    fn memory_api_settings_path(&self) -> Option<&Path> {
        self.memory_api_settings_path.as_deref()
    }

    fn load_vr_settings(&self) -> Settings {
        self.visual_recall_settings_path()
            .and_then(|path| with_locked_settings(path, Clone::clone).ok())
            .unwrap_or_default()
    }

    fn capture_control_path(&self) -> Result<PathBuf, String> {
        self.memory_api_settings_path().or(self.visual_recall_settings_path())
            .and_then(Path::parent).map(|dir| dir.join("capture_control.json"))
            .ok_or_else(|| "Capture settings path is not configured.".into())
    }

    /// Patch `visual_recall.json` under its locks; the patch may refuse after seeing the
    /// settings in force (issue #782), and then nothing is saved.
    fn try_update_vr_settings(
        &self,
        update: impl FnOnce(&mut Settings) -> Result<(), String>,
        after_commit: impl FnOnce(&Settings),
    ) -> Result<Settings, String> {
        let Some(path) = self.visual_recall_settings_path() else {
            return Err("visual_recall settings path not configured".to_string());
        };
        try_update_settings_with_commit(path, update, after_commit)
    }

    /// Enforce age expiry on every API face, including processes without the desktop poller.
    fn enforce_vr_retention(&self) -> Result<Settings, String> {
        let Some(path) = self.visual_recall_settings_path() else {
            return Err("visual_recall settings path not configured".to_string());
        };
        self.db.enforce_visual_recall_retention(path)
    }

    fn load_memory_api_settings(&self) -> Result<MemoryApiSettings, String> {
        self.memory_api_settings_path()
            .map(load_memory_api_settings)
            .unwrap_or_else(|| Ok(MemoryApiSettings::default()))
    }

    fn save_memory_api_settings(&self, settings: &MemoryApiSettings) -> Result<(), String> {
        let Some(path) = self.memory_api_settings_path() else {
            return Err("memory API settings path not configured".into());
        };
        save_memory_api_settings(path, settings)
    }

    fn whoami_json(&self) -> String {
        let profile = match self.load_memory_api_settings() {
            Ok(settings) => settings.profile,
            Err(error) => return json!({ "error": error }).to_string(),
        };
        // Confirmed (High band) items keep the original lists; Medium ones go to the additive
        // `inferred` block so a client never reads a guess as an established fact. Low-band rows
        // (unreviewed candidates) are left out entirely, as on every other read (FR-API-06).
        let people = whoami_split(self.db.people().into_iter().map(|r| (r.display_name, r.confidence)));
        let projects = whoami_split(self.db.projects().into_iter().map(|r| (r.name, r.confidence)));
        let commitments = whoami_split(
            self.db.commitments_due(self.db.now_ms()).into_iter().map(|r| (r.description, r.confidence)),
        );
        // `open_loops()` already caps an unreviewed mail guess below the High band (#926).
        let open_loops =
            whoami_split(self.db.open_loops().into_iter().map(|r| (r.description, r.confidence)));
        // About You is a local, generated projection. A failed refresh must not take down
        // profile.whoami; `null` tells every surface there is no current projection.
        let generated = self
            .db
            .generated_about_profile()
            .ok()
            .map(|generated| generated.with_declared_name(&profile.display_name));
        // The identity profile as one text (Name / Email header, then each section), the same
        // lines chat reads first.
        let identity = generated
            .as_ref()
            .map(|generated| generated.identity_text())
            .filter(|text| !text.is_empty());
        let mut work = json!({});
        let mut inferred = json!({});
        for (name, split) in [
            ("people", people),
            ("projects", projects),
            ("commitments", commitments),
            ("open_loops", open_loops),
        ] {
            work[name] = json!({ "count": split.strong.len(), "names": capped(split.strong) });
            inferred[format!("{name}_total")] = split.weak.len().into();
            inferred[name] = capped(split.weak).into();
        }
        work["inferred"] = inferred;
        json!({
            "profile": { "display_name": profile.display_name, "role": profile.role, "prefs": profile.prefs },
            "identity": identity,
            "generated": generated,
            "work": work,
        }).to_string()
    }

    fn writing_voice_json(&self) -> String {
        let config = self.user_config_path.as_deref().and_then(|path| {
            crate::user_config::load_report(path).ok().map(|(config, _)| config.style)
        });
        match self.db.writing_voice_scopes() {
            Ok(scopes) => json!({
                "enabled": self.db.voice_learning_enabled(),
                "scopes": scopes.into_iter().map(|scope| json!({
                    "app": scope.app,
                    "examples": scope.examples,
                })).collect::<Vec<_>>(),
                "config": config,
                // #1066: the notes the rewrite loop learned (the user's to delete / rate) and the
                // outcome tallies. Derived text only: never the user's drafts or sent messages.
                "style_notes": self.db.rewrite_style_notes().unwrap_or_default().into_iter().map(|n| json!({
                    "id": n.id, "scope": n.scope, "app": n.app, "kind": n.kind, "text": n.text,
                    "impressions": n.impressions, "confirmations": n.confirmations, "rejections": n.rejections,
                    "created_at": n.created_at,
                })).collect::<Vec<_>>(),
                "rewrite_outcomes": self.db.rewrite_summary().ok(),
            }).to_string(),
            Err(error) => json!({ "error": error }).to_string(),
        }
    }

    fn meeting_list_json(&self, params: &ReadParams) -> String {
        if params.id.is_some_and(|id| id <= 0) {
            return json!({"error": "invalid_meeting_cursor"}).to_string();
        }
        match self.db.recent_meetings(params.id, 50) {
            Ok(rows) => json!({"sessions": rows.into_iter().map(|row| json!({
                "id": row.id, "title": row.title, "started_at": row.started_at,
                "ended_at": row.ended_at, "app_bundle_id": row.app_bundle_id,
                "in_person": row.in_person,
            })).collect::<Vec<_>>()}),
            Err(_) => json!({"error": "meeting_history_unavailable"}),
        }.to_string()
    }

    fn wiki_review_json(&self, tool: Tool, params: &ReadParams) -> String {
        use shogun_mcp::memory_api::{read_inclusion, ReadInclusion};
        let Some((kind, id)) = params.query.as_deref()
            .and_then(shogun_mcp::backend::parse_knowledge_key) else {
            return json!({"error": "invalid_memory_key"}).to_string();
        };
        let key = format!("{kind}-{id}");
        if tool == Tool::WikiEvidence {
            return match self.db.knowledge_evidence(&key) {
                // A connection without meeting notes and screen recall (owner decision
                // 2026-10-20) gets no excerpt from either.
                Ok(mut rows) => {
                    rows.retain(|row| shogun_mcp::source_policy::current_allows_source(&row.source));
                    json!({"review_only": true, "evidence": rows})
                }
                Err(_) => json!({"error": "evidence_unavailable"}),
            }.to_string();
        }
        match self.db.knowledge_neighbors(&key) {
            Ok(rows) => {
                let neighbors: Vec<_> = rows.into_iter().filter_map(|row| {
                    let ReadInclusion::Included { possibly } = read_inclusion(row.confidence, params.include_low) else {
                        return None;
                    };
                    Some(json!({
                        "id": row.key,
                        "shared_sources": row.shared_sources,
                        "kind": row.kind,
                        "text": if possibly { format!("possibly: {}", row.name) } else { row.name },
                        "confidence": row.confidence,
                        "possibly": possibly,
                    }))
                }).collect();
                json!({"review_only": true, "relationship": "shared_source", "neighbors": neighbors})
            }
            Err(_) => json!({"error": "neighbors_unavailable"}),
        }.to_string()
    }

    fn apply_profile_set(&self, body: &str) -> Result<(), String> {
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|_| "expected profile JSON object".to_string())?;
        let patch = value.get("profile").unwrap_or(&value);
        let Some(object) = patch.as_object() else {
            return Err("expected profile JSON object".into());
        };
        let mut settings = self.load_memory_api_settings()?;
        let field = |key: &str| -> Result<Option<String>, String> {
            match object.get(key) {
                Some(value) => value
                    .as_str()
                    .map(|text| Some(text.trim().to_string()))
                    .ok_or_else(|| format!("profile.{key} must be a string")),
                None => Ok(None),
            }
        };
        if let Some(value) = field("display_name")? {
            settings.profile.display_name = value;
        }
        if let Some(value) = field("role")? {
            settings.profile.role = value;
        }
        if let Some(value) = field("prefs")? {
            settings.profile.prefs = normalize_prefs(&value);
        }
        validate_profile(&settings.profile)?;
        self.save_memory_api_settings(&settings)
    }

    /// DB-derived context plus optional host-owned, already-preassembled current context.
    fn get_context_items(&self) -> Vec<ReadItem> {
        let live = self.current_context();
        let metadata = match live.availability {
            CurrentContextAvailability::Fresh => {
                "metadata: current_context=fresh source=desktop_cache"
            }
            CurrentContextAvailability::Empty => {
                "metadata: current_context=empty source=desktop_cache live_text_withheld=true"
            }
            CurrentContextAvailability::Stale => {
                "metadata: current_context=stale source=desktop_cache live_text_withheld=true"
            }
            CurrentContextAvailability::Unavailable => {
                "metadata: current_context=unavailable source=standalone reason=desktop_cache_not_connected"
            }
        };
        let mut items = vec![ReadItem::new(metadata, EVENT_CONFIDENCE)];
        let current_count = live.items.len();
        if live.availability == CurrentContextAvailability::Fresh {
            items.extend(live.items);
        }

        items.extend(self.db.inline_memory(12).into_iter().map(|fact| {
            // `inline_memory` already applies the state confidence gate and prefixes medium
            // confidence facts with `possibly:`.
            ReadItem::new(format!("fact: {fact}"), EVENT_CONFIDENCE)
        }));
        items.extend(
            self.db
                .recent_note_texts()
                .into_iter()
                .map(|note| ReadItem::new(format!("note: {note}"), EVENT_CONFIDENCE)),
        );
        items.extend(
            self.db
                .recent_context_previews(
                    CONTEXT_ACTIVITY_LIMIT.saturating_sub(current_count),
                    CONTEXT_ACTIVITY_EXCERPT_CHARS,
                )
                .into_iter()
                // Owner decision 2026-10-20: no meeting or screen text for a withheld connection.
                .filter(|(source, _)| shogun_mcp::source_policy::current_allows_source(source))
                .map(|(source, event)| {
                    let title = event.window_title.as_deref().unwrap_or("-");
                    let app = event.app_bundle_id.as_deref().unwrap_or("-");
                    ReadItem::new(
                        format!(
                            "recent activity event:{} source:{} ts:{} title:{} app:{} :: {}",
                            event.id, source, event.ts, title, app, event.excerpt
                        ),
                        EVENT_CONFIDENCE,
                    )
                }),
        );
        items
    }

    /// `memory.get_context` on the API faces: sessions from the capture log (freshness, the
    /// current session, recent ones) plus the confidence-gated `fact:` and user `note:` lines.
    fn get_context_json(&self) -> String {
        let mut value = match self.db.current_activity_json() {
            Ok(value) => value,
            Err(error) => return json!({ "error": error }).to_string(),
        };
        let facts = self.db.inline_memory(12).into_iter().map(|fact| format!("fact: {fact}"));
        let notes = self.db.recent_note_texts().into_iter().map(|note| format!("note: {note}"));
        value["results"] = facts.chain(notes).collect::<Vec<_>>().into();
        value.to_string()
    }

    fn visual_recall_status_json(&self) -> String {
        let settings = self
            .enforce_vr_retention()
            .unwrap_or_else(|_| self.load_vr_settings());
        let now = self.db.now_ms();
        let retention_ms = settings.retention.retain_ms().unwrap_or(3 * DAY_MS);
        let frame_stats = self.db.screen_frame_stats();
        let hidden_self_stats = self.db.hidden_self_screen_frame_stats();
        let frames_24h = self
            .db
            .screen_frames_count_in_range(now.saturating_sub(DAY_MS), now);
        let frames_retained = self
            .db
            .screen_frames_count_in_range(now.saturating_sub(retention_ms), now);
        let estimated_daily_bytes = (frames_24h >= 2).then(|| {
            self.db
                .screen_frame_bytes_in_range(now.saturating_sub(DAY_MS), now)
        });
        let projected_retention_bytes = estimated_daily_bytes
            .and_then(|bytes| bytes.checked_mul(i64::from(settings.retention.days())));
        let recent: Vec<_> = self
            .db
            .screen_ocr_previews(5, 140)
            .into_iter()
            .map(|p| {
                json!({
                    "ts": p.ts,
                    "app": p.app_bundle_id,
                    "window": p.window_title,
                    "chars": p.content_len,
                    "dwell_ms": p.dwell_ms,
                    "display_id": p.display_id,
                    "excerpt": p.excerpt,
                })
            })
            .collect();
        let last_capture = self.db.latest_screen_frame_summary().map(|s| {
            json!({
                "frame_id": s.id,
                "ts": s.created_at_ms,
                "event_id": s.event_id,
                "app": s.app_bundle_id,
                "window": s.window_title,
                "width": s.width,
                "height": s.height,
                "jpeg_bytes": s.jpeg_bytes,
                "ocr_chars": s.ocr_text.chars().count(),
                "accessibility_chars": s.accessibility_text.chars().count(),
            })
        });
        json!({
            "enabled": settings.enabled,
            "retention_days": settings.retention.days(),
            "events_24h": self.db.screen_ocr_count_24h(),
            "frames_count": frame_stats.count,
            "frames_24h": frames_24h,
            "frames_retained": frames_retained,
            "frames_oldest_ms": frame_stats.oldest_ms,
            "frames_bytes": frame_stats.total_bytes,
            "hidden_self_frames_count": hidden_self_stats.count,
            "hidden_self_frames_bytes": hidden_self_stats.total_bytes,
            "estimated_daily_bytes": estimated_daily_bytes,
            "projected_retention_bytes": projected_retention_bytes,
            "capture_paused_storage": self.db.screen_frame_capture_paused(),
            "capture_paused_url": self.visual_recall_settings_path().and_then(|path| {
                crate::capture::status::read(&path.with_file_name(crate::capture::status::FILE_NAME),
                    shogun_mcp::desktop_heartbeat::now_ms())
            }),
            "capture_storage_limit_bytes": shogun_memory::retention::FRAME_CAPTURE_MAX_BYTES,
            "last_capture": last_capture,
            "recent": recent,
        })
        .to_string()
    }

    fn visual_recall_search_json(&self, params: &ReadParams) -> String {
        let query = params.query.as_deref().unwrap_or("");
        let now = self.db.now_ms();
        let retention_ms = self
            .enforce_vr_retention()
            .unwrap_or_else(|_| self.load_vr_settings())
            .retention
            .retain_ms()
            .unwrap_or(3 * DAY_MS);
        let retention_start = now.saturating_sub(retention_ms);
        let (requested_from, requested_to) = match (params.from_ms, params.to_ms) {
            (Some(f), Some(t)) => (f, t),
            (Some(f), None) => (f, now),
            (None, Some(t)) => (0, t),
            // No window given: `visual_recall_window` picks one.
            (None, None) => shogun_memory::search::visual_recall_window(
                query,
                now,
                local_day_bounds(now),
                retention_ms,
            ),
        };
        let from_ms = requested_from.max(retention_start);
        let to_ms = requested_to.min(now);
        if from_ms > to_ms {
            return json!({
                "query": query,
                "from_ms": from_ms,
                "to_ms": to_ms,
                "total_count": 0,
                "apps": [],
                "frames": [],
            })
            .to_string();
        }
        let cursor = match (params.cursor_ts, params.cursor_id) {
            (Some(created_at_ms), Some(id)) => {
                Some(shogun_memory::screen_frames::TimelineCursor { created_at_ms, id })
            }
            (None, None) => None,
            _ => return r#"{"error":"cursor_ts_and_cursor_id_required_together"}"#.to_string(),
        };
        let direction = match params.cursor_direction.as_deref().unwrap_or("older") {
            "older" => shogun_memory::screen_frames::TimelineDirection::Older,
            "newer" if cursor.is_some() => {
                shogun_memory::screen_frames::TimelineDirection::Newer
            }
            "newer" => return r#"{"error":"newer_cursor_requires_cursor"}"#.to_string(),
            _ => return r#"{"error":"invalid_cursor_direction"}"#.to_string(),
        };
        let page_size = params.page_size.unwrap_or(FRAME_SEARCH_LIMIT).clamp(1, 200);
        let mut hits = self.db.screen_frame_timeline_page(
            shogun_memory::screen_frames::TimelineQuery {
                query,
                from_ms,
                to_ms,
                app_bundle_id: params.app_bundle_id.as_deref(),
                cursor,
                direction,
                limit: page_size.saturating_add(1),
            },
        );
        let has_more_in_direction =
            shogun_memory::screen_frames::trim_page_sentinel(&mut hits, page_size, direction);
        let frames: Vec<_> = hits
            .iter()
            .map(|f| {
                json!({
                    "frame_id": f.id,
                    "event_id": f.event_id,
                    "accessibility_event_id": f.accessibility_event_id,
                    "ts": f.created_at_ms,
                    "app": f.app_bundle_id,
                    "window": f.window_title,
                    "width": f.width,
                    "height": f.height,
                    "ocr_excerpt": shogun_memory::search::excerpt(&f.ocr_text, query, FRAME_EXCERPT_CHARS),
                    "accessibility_excerpt": shogun_memory::search::excerpt(
                        &f.accessibility_text,
                        query,
                        FRAME_EXCERPT_CHARS,
                    ),
                    "text_sources": text_sources(&f.accessibility_text, &f.ocr_text),
                    "needs_rescan": f.needs_rescan(),
                    "source": f.source,
                })
            })
            .collect();
        let has_older = match direction {
            shogun_memory::screen_frames::TimelineDirection::Older => has_more_in_direction,
            shogun_memory::screen_frames::TimelineDirection::Newer => cursor.is_some(),
        };
        let has_newer = match direction {
            shogun_memory::screen_frames::TimelineDirection::Older => cursor.is_some(),
            shogun_memory::screen_frames::TimelineDirection::Newer => has_more_in_direction,
        };
        let next_cursor = has_older
            .then(|| {
                hits.last().map(|frame| {
                    json!({
                        "ts": frame.created_at_ms,
                        "id": frame.id,
                    })
                })
            })
            .flatten();
        let previous_cursor = has_newer
            .then(|| {
                hits.first().map(|frame| {
                    json!({
                        "ts": frame.created_at_ms,
                        "id": frame.id,
                    })
                })
            })
            .flatten();
        json!({
            "query": query,
            "from_ms": from_ms,
            "to_ms": to_ms,
            "app_bundle_id": params.app_bundle_id,
            "total_count": self.db.screen_frame_timeline_count(
                query,
                from_ms,
                to_ms,
                params.app_bundle_id.as_deref(),
            ),
            "apps": self.db.screen_frame_timeline_apps(from_ms, to_ms),
            "next_cursor": next_cursor,
            "previous_cursor": previous_cursor,
            "frames": frames,
        })
        .to_string()
    }

    fn visual_recall_get_frame_json(&self, params: &ReadParams) -> String {
        if self.enforce_vr_retention().is_err() {
            return r#"{"error":"retention_check_failed"}"#.to_string();
        }
        let Some(frame_id) = params.id else {
            return r#"{"error":"missing_frame_id"}"#.to_string();
        };
        let Some(s) = self.db.get_screen_frame_summary(frame_id) else {
            return json!({ "error": "not_found", "frame_id": frame_id }).to_string();
        };
        let needs_rescan = s.needs_rescan();
        let text_sources = text_sources(&s.accessibility_text, &s.ocr_text);
        json!({
            "frame_id": s.id,
            "event_id": s.event_id,
            "ts": s.created_at_ms,
            "app": s.app_bundle_id,
            "window": s.window_title,
            "display_id": s.display_id,
            "width": s.width,
            "height": s.height,
            "jpeg_bytes": s.jpeg_bytes,
            "ocr_text": s.ocr_text,
            "accessibility_text": s.accessibility_text,
            "accessibility_event_id": s.accessibility_event_id,
            "text_sources": text_sources,
            "needs_rescan": needs_rescan,
            "source": s.source,
        })
        .to_string()
    }

    /// `visual_recall.frame_text_lines`: the shared [`Db::screen_frame_text_lines`] (the desktop's
    /// "View OCR text"), read-only. Error wording matches the rescan face.
    fn visual_recall_text_lines_json(&self, params: &ReadParams) -> String {
        use crate::daemon::FrameRescanError;
        let Some(path) = self.visual_recall_settings_path() else {
            return r#"{"error":"retention_check_failed"}"#.to_string();
        };
        let Some(frame_id) = params.id else {
            return r#"{"error":"missing_frame_id"}"#.to_string();
        };
        match self.db.screen_frame_text_lines(path, frame_id) {
            Ok(lines) => json!({ "frame_id": frame_id, "lines": lines }).to_string(),
            Err(FrameRescanError::OcrFailed) => {
                json!({ "error": "ocr_failed", "frame_id": frame_id }).to_string()
            }
            Err(FrameRescanError::RetentionCheckFailed | FrameRescanError::EventUpdateFailed) => {
                r#"{"error":"retention_check_failed"}"#.to_string()
            }
            Err(FrameRescanError::NotFound) => {
                json!({ "error": "not_found", "frame_id": frame_id }).to_string()
            }
            Err(FrameRescanError::Expired) => {
                json!({ "error": "not_found", "frame_id": frame_id, "reason": "expired" })
                    .to_string()
            }
            Err(FrameRescanError::VisualRecallOff) => json!({
                "error": "not_found",
                "frame_id": frame_id,
                "reason": "visual_recall_off",
            })
            .to_string(),
        }
    }

    /// `visual_recall.rescan_frame`: the shared [`Db::rescan_screen_frame`] (the desktop's
    /// "Re-read text" runs the same function, issue #782), worded as this face's JSON with its own
    /// wire codes; a refused frame that is gone because of retention or Off adds a `reason` beside
    /// `not_found`.
    fn visual_recall_rescan_json(&self, params: &ReadParams) -> String {
        use crate::daemon::FrameRescanError;
        let Some(path) = self.visual_recall_settings_path() else {
            return r#"{"error":"retention_check_failed"}"#.to_string();
        };
        let Some(frame_id) = params.id else {
            return r#"{"error":"missing_frame_id"}"#.to_string();
        };
        match self.db.rescan_screen_frame(path, frame_id) {
            Ok(rescan) => {
                let excerpt =
                    shogun_memory::search::excerpt(&rescan.ocr_text, "", FRAME_EXCERPT_CHARS);
                json!({
                    "frame_id": frame_id,
                    "chars": rescan.ocr_text.chars().count(),
                    "excerpt": excerpt,
                    "ocr_text": rescan.ocr_text,
                })
                .to_string()
            }
            Err(FrameRescanError::RetentionCheckFailed) => {
                r#"{"error":"retention_check_failed"}"#.to_string()
            }
            Err(FrameRescanError::NotFound) => {
                json!({ "error": "not_found", "frame_id": frame_id }).to_string()
            }
            Err(FrameRescanError::Expired) => {
                json!({ "error": "not_found", "frame_id": frame_id, "reason": "expired" })
                    .to_string()
            }
            Err(FrameRescanError::VisualRecallOff) => json!({
                "error": "not_found",
                "frame_id": frame_id,
                "reason": "visual_recall_off",
            })
            .to_string(),
            Err(FrameRescanError::OcrFailed) => json!({
                "error": "ocr_failed",
                "frame_id": frame_id,
                "ocr_available": crate::capture::visual_recall::OCR_AVAILABLE,
            })
            .to_string(),
            Err(FrameRescanError::EventUpdateFailed) => {
                json!({ "error": "event_update_failed", "frame_id": frame_id }).to_string()
            }
        }
    }

    /// `state.disputes.list` (issue #253): open contradiction flags, each side with its evidence
    /// event. The FR-API-06 gate applies per side at the confidence Fusion sees — capped below the
    /// High band while disputed, so both sides always carry `possibly`. A pair with a Low side is
    /// left out unless `include_low` (the same opt-in the other state reads use).
    /// `state.export` (issue #290): the Markdown document plus the same rows as JSON, after the
    /// FR-API-06 band rule. Row text is the user's own derived state, never raw capture.
    fn world_model_export_json(&self, params: &ReadParams) -> String {
        match self.db.world_model(params.include_low) {
            Some(model) => json!({
                "format": "markdown",
                "markdown": shogun_memory::state_edit::to_markdown(&model, self.db.now_ms()),
                "tables": model,
            })
            .to_string(),
            None => json!({ "error": "unavailable" }).to_string(),
        }
    }

    /// `state.split.evidence` (FR-ST-10): a person's / project's aliases and provenance-event metadata
    /// (id, time, source, kind — never content), the list a split chooses from. The table is the
    /// read's `key` (MCP `table` argument, REST path segment).
    fn state_split_evidence_json(&self, params: &ReadParams) -> String {
        let (Some(table), Some(id)) = (params.key.as_deref(), params.id.filter(|id| *id > 0)) else {
            return json!({ "error": "invalid_request" }).to_string();
        };
        match self.db.state_split_evidence(table, id) {
            Ok(Some(evidence)) => serde_json::to_string(&evidence)
                .unwrap_or_else(|_| json!({ "error": "unavailable" }).to_string()),
            Ok(None) => json!({ "error": "not_found" }).to_string(),
            Err(e) if e.starts_with("invalid") => json!({ "error": "invalid_table" }).to_string(),
            Err(_) => json!({ "error": "unavailable" }).to_string(),
        }
    }

    /// `state.merge.candidates` (issue #1362): the related pairs nothing merged, newest first.
    /// The table is the read's `key` (MCP `table` argument, REST path segment).
    fn state_merge_candidates_json(&self, params: &ReadParams) -> String {
        let Some(table) = params.key.as_deref() else {
            return json!({ "error": "invalid_request" }).to_string();
        };
        match self.db.state_merge_candidates(table) {
            Ok(candidates) => json!({ "table": table, "candidates": candidates }).to_string(),
            Err(e) if e.starts_with("invalid") => json!({ "error": "invalid_table" }).to_string(),
            Err(_) => json!({ "error": "unavailable" }).to_string(),
        }
    }

    fn disputes_json(&self, params: &ReadParams) -> String {
        use shogun_mcp::memory_api::{read_inclusion, ReadInclusion};
        let side = |c: &shogun_memory::disputes::DisputedCommitment| {
            let confidence = c.confidence.min(shogun_memory::disputes::DISPUTED_CONFIDENCE_CAP);
            let ReadInclusion::Included { possibly } =
                read_inclusion(confidence, params.include_low)
            else {
                return None;
            };
            Some(json!({
                "id": c.id,
                "description": c.description,
                "due_at": c.due_at,
                "confidence": confidence,
                "possibly": possibly,
                "evidence_event_id": c.evidence_event_id,
                "evidence_ts": c.evidence_ts,
                "direction": c.direction,
                "counterparty_id": c.counterparty_id,
                "counterparty_name": c.counterparty_name,
            }))
        };
        let disputes: Vec<_> = self
            .db
            .state_disputes_open(50)
            .iter()
            .filter_map(|d| {
                Some(json!({
                    "id": d.id,
                    "field": d.field,
                    "earlier": side(&d.earlier)?,
                    "later": side(&d.later)?,
                    "created_at": d.created_at,
                }))
            })
            .collect();
        json!({ "disputes": disputes }).to_string()
    }

    /// `lessons.list` (L5): instruction + bookkeeping only — never `feedback_events` text.
    ///
    /// Default (no generation filters) is the Settings/management list: every row, including
    /// sleeping and person-scoped. Pass `for_generation` or a scope id for standing-prompt lookup
    /// so an Alice email lesson does not leak into unrelated drafts (issue #104).
    fn lessons_json(&self, params: &ReadParams) -> String {
        let scoped = params.for_generation
            || params.app_bundle_id.is_some()
            || params.person_id.is_some()
            || params.project_id.is_some();
        let lessons = if scoped {
            let ctx = crate::daemon::GenerationContext {
                app_bundle_id: params.app_bundle_id.as_deref(),
                person_id: params.person_id.as_deref(),
                project_id: params.project_id.as_deref(),
            };
            self.db.active_lessons(&ctx.lesson_scopes(), 32)
        } else {
            self.db.lessons_all()
        };
        let lessons: Vec<_> = lessons
            .into_iter()
            .map(|l| {
                json!({
                    "id": l.id,
                    "kind": l.kind.as_str(),
                    "scope": l.scope.as_str(),
                    "scope_ref": l.scope_ref,
                    "instruction": l.instruction,
                    "confidence": l.confidence,
                    "evidence_count": l.evidence_count,
                    "active": l.active,
                })
            })
            .collect();
        json!({ "lessons": lessons }).to_string()
    }

    /// `memory.get_wrap` (issue #10, invariant 6): today's Evening Wrap as JSON — the same
    /// `Db::evening_wrap` aggregation the notch card draws, so the API face and the human face
    /// cannot disagree. Local aggregation plus the Batch-written `what_happened`; the confidence gate is the
    /// assembler's (fusion), identical to the card's.
    fn evening_wrap_json(&self) -> String {
        let now = self.db.now_ms();
        let (day_start, tomorrow_end) = crate::daemon::local_wrap_window(now);
        // Tomorrow's synced calendar events (FR-EB-01), the same read the notch card makes. A
        // store failure leaves the section empty rather than failing the whole wrap.
        let tomorrow = self.db.calendar_day_lines(now, 1).unwrap_or_default();
        let wrap = self
            .db
            .evening_wrap(tomorrow, day_start, now, tomorrow_end);
        let line = |i: &shogun_fusion::brief::BriefItem| {
            json!({
                "text": i.text,
                "possibly": i.possibly,
                "provenance_event_id": i.provenance_event_id,
                "evidence_kind": i.provenance_event_id.is_none().then_some(shogun_memory::state::EvidenceKind::EditedContext),
            })
        };
        let date = crate::daemon::local_date_string(now);
        // The Batch-written lines the card shows on top (empty until today's request completes).
        let what_happened = self.db.wrap_lines(&date).map(|l| l.lines).unwrap_or_default();
        json!({
            "date": date,
            "what_happened": what_happened,
            "outcome": {
                "commitments_done": wrap.outcome.commitments_done,
                "loops_closed": wrap.outcome.loops_closed,
                "actions_decided": wrap.outcome.actions_decided,
                "actions_adopted": wrap.outcome.actions_adopted,
                // The mail lines the Evening Wrap card shows (FR-EB-05 parity, issue #779): an
                // agent summarising the day reads the same triage outcome the user sees.
                "mail_triaged": wrap.outcome.mail_triaged,
                "mail_auto_archived": wrap.outcome.mail_auto_archived,
                "mail_needs_reply": wrap.outcome.mail_needs_reply,
            },
            "still_open": wrap.still_open.iter().map(line).collect::<Vec<_>>(),
            "tomorrow_calendar": wrap
                .tomorrow_calendar
                .iter()
                .map(|c| json!({ "start_ms": c.start_ms, "title": c.title, "all_day": c.all_day, "updated": c.updated }))
                .collect::<Vec<_>>(),
            "tomorrow_commitments": wrap.tomorrow_commitments.iter().map(line).collect::<Vec<_>>(),
            "loose_ends": wrap.loose_ends.iter().map(line).collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// `calendar.upcoming` (invariant 6): the Today page's "Coming up" — the same
    /// [`crate::daemon::Db::calendar_agenda`] read, window and cap. Calendar events are the
    /// user's own calendar's facts, so there is no confidence gate and no `possibly`. A store
    /// failure is a stable error code, never an empty list that reads as "nothing scheduled".
    fn calendar_upcoming_json(&self, params: &ReadParams) -> String {
        let now = self.db.now_ms();
        let Ok((events, total)) = self.db.calendar_agenda_page(now, params.page_size.unwrap_or(crate::daemon::AGENDA_MAX)) else {
            return json!({ "error": "memory_unavailable" }).to_string();
        };
        json!({
            "date": crate::daemon::local_date_string(now),
            "days": crate::daemon::AGENDA_DAYS,
            "total": total,
            "hidden_count": total.saturating_sub(events.len()),
            "events": events
                .iter()
                .map(|e| json!({
                    "id": e.id,
                    "title": e.title,
                    "start_ms": e.start_ms,
                    "end_ms": e.end_ms,
                    "all_day": e.all_day,
                    "location": e.location,
                    "attendees": e.attendees,
                    "day": e.day,
                    "date": e.date,
                }))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// `meeting.speakers.get`: the diarized keys a session's transcript actually contains, any
    /// user-confirmed names, and name candidates — the same view the Recap chips render
    /// (invariant 6). `params.id` selects the session; absent → the most recent meeting.
    fn meeting_speakers_json(&self, params: &ReadParams) -> String {
        let Some(session_id) = params.id.or_else(|| self.db.latest_meeting_session_id()) else {
            return json!({ "error": "not_found" }).to_string();
        };
        // A named meeting that does not exist is not found (issue #779), like the recap read and
        // the speakers write — never an empty list that reads as "no speakers yet".
        if self.db.meeting_session(session_id).is_none() {
            return json!({ "error": "not_found" }).to_string();
        }
        let named = self.db.meeting_speakers(session_id);
        let speakers = self
            .db
            .meeting_speaker_keys(session_id)
            .into_iter()
            .map(|key| {
                let hit = named.iter().find(|n| n.speaker_key == key);
                json!({
                    "key": key,
                    "display_name": hit.map(|n| n.display_name.clone()),
                    "source": hit.map(|n| n.source.clone()),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "session_id": session_id,
            "speakers": speakers,
            "candidates": self.db.meeting_speaker_candidates(session_id, 8),
        })
        .to_string()
    }

    /// `meeting.templates.get`: the registry plus the selected id for one session (`params.id`,
    /// or the most recent meeting) — the same list the Recap's picker renders (invariant 6).
    fn meeting_templates_json(&self, params: &ReadParams) -> String {
        // An explicit id must name a meeting (issue #779), as the templates write requires. With
        // no id and no meeting yet, the registry is still answered for the picker.
        if let Some(id) = params.id {
            if self.db.meeting_session(id).is_none() {
                return json!({ "error": "not_found" }).to_string();
            }
        }
        let session_id = params.id.or_else(|| self.db.latest_meeting_session_id());
        let selected = crate::meeting::templates::resolve(
            session_id
                .and_then(|id| self.db.session_template(id))
                .as_deref(),
        )
        .id;
        json!({
            "session_id": session_id,
            "selected": selected,
            "templates": crate::meeting::templates::TEMPLATES
                .iter()
                .map(|t| json!({ "id": t.id, "name": t.name, "description": t.description }))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// `patterns.list`: the Work Patterns overview — the same live aggregates and profile rows
    /// the Patterns card renders plus bounded dashboard usage (FR-PAT-06, invariant 6). Counts
    /// and metadata only.
    fn patterns_json(&self) -> String {
        let (activities, patterns) = self.db.patterns_overview();
        let usage = self.db.try_dashboard_usage().ok();
        json!({
            "activities": activities
                .iter()
                .map(|a| json!({
                    "kind": a.kind,
                    "shown_7d": a.shown_7d,
                    "decided_7d": a.decided_7d,
                    "adopted_7d": a.adopted_7d,
                }))
                .collect::<Vec<_>>(),
            "patterns": patterns
                .iter()
                .map(|p| json!({
                    "id": p.id,
                    "kind": p.kind,
                    "key": p.key,
                    "scope": p.scope,
                    "scope_ref": p.scope_ref,
                    "count_7d": p.count_7d,
                    "count_30d": p.count_30d,
                    "trend": p.trend,
                    "confidence": p.confidence,
                    "active": p.active,
                }))
                .collect::<Vec<_>>(),
            "usage": usage,
        })
        .to_string()
    }

    /// `meeting.recap.export`: the same Markdown document the Recap's copy button produces
    /// (invariant 6). `query == "transcript"` includes the transcript.
    fn meeting_recap_export_json(&self, params: &ReadParams) -> String {
        let Some(session_id) = params.id.or_else(|| self.db.latest_meeting_session_id()) else {
            return json!({ "error": "not_found" }).to_string();
        };
        let include_transcript = params.query.as_deref() == Some("transcript");
        match self.db.meeting_recap_markdown(session_id, include_transcript) {
            Some(markdown) => {
                json!({ "session_id": session_id, "markdown": markdown }).to_string()
            }
            None => json!({ "error": "not_found" }).to_string(),
        }
    }

    // file exports (issue #397)

    /// `export.writes.list`: every export and where it stands (`id` → just that one). Digest and
    /// byte count only — the content lives in the user's file, nowhere else.
    fn local_writes_json(&self, params: &ReadParams) -> String {
        match params.id {
            Some(id) => match self.db.local_write(id) {
                Some(row) => json!({ "write": row }).to_string(),
                None => json!({ "error": "not_found" }).to_string(),
            },
            None => json!({ "writes": self.db.local_writes_recent(50) }).to_string(),
        }
    }

    /// `export.write_file`: record an L2 request for the desktop to confirm. Refused now (and
    /// never queued) when it could not land: unknown meeting, bad name, folder outside the
    /// user's export folders.
    fn export_write_file(&self, body: &str) -> Result<i64, String> {
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|_| "invalid export request".to_string())?;
        let folder = value
            .get("folder")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .ok_or_else(|| "missing folder".to_string())?;
        let format = match value.get("format").and_then(|v| v.as_str()) {
            None => shogun_agents::permission::ExportFormat::Markdown,
            Some(f) => shogun_agents::permission::ExportFormat::parse(f)
                .ok_or_else(|| "format must be md or csv".to_string())?,
        };
        let roots = shogun_mcp::export_folders::list();
        // `local_write_request` checks the folder before the meeting, the same for every face.
        // With no session named and no meeting at all, it is never reached, so the folder is
        // checked here first for that one case.
        let session_id = match value.get("session_id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => match self.db.latest_meeting_session_id() {
                Some(id) => id,
                None => {
                    crate::file_export::resolve_folder(&roots, folder)
                        .map_err(|e| e.as_str().to_string())?;
                    return Err("no meeting to export".to_string());
                }
            },
        };
        let ask = crate::daemon::ExportAsk {
            session_id,
            include_transcript: value
                .get("include_transcript")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            format,
            folder: folder.to_string(),
            name: value.get("name").and_then(|v| v.as_str()).map(str::to_string),
            origin: "api",
        };
        self.db.local_write_request(&ask, &roots).map(|row| row.id)
    }

    // user skills (issue #251)

    /// `skills.list`: the skill folder as the desktop's Skills card shows it. `q=<name>` adds
    /// that skill's instructions (an AI client can follow them in its own harness, at the same
    /// levels — sends still go through `actions.execute` and its L3 queue).
    fn skills_list_json(&self, params: &ReadParams) -> String {
        let Some(dir) = shogun_mcp::plan_source::skills_dir() else {
            return json!({ "skills": [], "errors": [] }).to_string();
        };
        let folder = shogun_agents::skills::load_dir(&dir);
        let wanted = params.query.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let skills: Vec<_> = folder
            .skills
            .iter()
            .map(|s| {
                let mut v = json!({
                    "name": s.name,
                    "description": s.description,
                    "level": shogun_agents::skills::level_str(s.level),
                    "declared_level": shogun_agents::skills::level_str(s.declared_level),
                    "required_connections": s.required_connections,
                    "inputs": s.inputs.iter().map(|i| json!({
                        "name": i.name,
                        "label": i.label,
                        "placeholder": i.placeholder,
                        "optional": i.optional,
                        "type": i.kind.as_str(),
                    })).collect::<Vec<_>>(),
                });
                if wanted == Some(s.name.as_str()) {
                    v["instructions"] = json!(s.body);
                }
                v
            })
            .collect();
        let errors: Vec<_> = folder
            .errors
            .iter()
            .map(|(file, e)| json!({ "file": file, "reason": e.as_str() }))
            .collect();
        json!({ "skills": skills, "errors": errors }).to_string()
    }

    /// `schedules.list` (issue #252).
    /// A public web read (issue #555 §4-8): the consent, then the link, then the fetch on the
    /// traced transport, then JSON. Consent is checked before the link is even looked at, so an
    /// unconsented caller learns about the card and nothing about what it named.
    fn web_read_json(&self, tool: Tool, params: &ReadParams) -> String {
        use crate::web_fetch::read::{error_json, WebReadKind};
        let kind = match tool {
            Tool::WebVideoTranscript => WebReadKind::VideoTranscript,
            Tool::WebSocialPost => WebReadKind::SocialPost,
            _ => WebReadKind::Page,
        };
        if !shogun_mcp::web_consent::consented() {
            return json!({
                "error": "consent_required",
                "message": "Web fetch needs your consent first: Settings → Connections → Web fetch.",
            })
            .to_string();
        }
        let Some(url) = params.query.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
            return json!({ "error": "missing_url", "message": "url is required" }).to_string();
        };
        #[cfg(feature = "net")]
        {
            let sink = self.db.traceability_sink();
            let mine = crate::web_fetch::read::mine_language_tag();
            match crate::web_fetch::read::read_blocking(kind, url, &mine, &sink) {
                Ok(read) => read.to_json().to_string(),
                Err(e) => error_json(&e).to_string(),
            }
        }
        #[cfg(not(feature = "net"))]
        {
            // The link is still checked, so a build without the transport is honest about a
            // bad link and equally honest about itself.
            match kind.resolve(url) {
                Err(e) => error_json(&e).to_string(),
                Ok(_) => json!({
                    "error": "unavailable",
                    "message": "this build has no network transport",
                })
                .to_string(),
            }
        }
    }

    fn schedules_json(&self) -> String {
        let schedules: Vec<_> = self
            .db
            .schedules()
            .into_iter()
            .map(|j| {
                json!({
                    "id": j.id,
                    "skill": j.skill,
                    "input": j.input,
                    "days": shogun_memory::scheduled_jobs::days_label(j.days),
                    "at": format!("{:02}:{:02}", j.minute_of_day / 60, j.minute_of_day % 60),
                    "enabled": j.enabled,
                    "origin": j.origin,
                    "last_slot_at": j.last_slot_at,
                    "last_outcome": j.last_outcome,
                    "last_run_id": j.last_run_id,
                })
            })
            .collect();
        json!({
            "schedules": schedules,
            "daily_run_cap": shogun_memory::scheduled_jobs::DAILY_RUN_CAP,
        })
        .to_string()
    }

    /// `meeting.recap.get`: the Recap as structured fields — exactly what the Recap panel
    /// renders (invariant 6, FR-MT-22): the interval, the chosen template, the generated minutes
    /// when the Batch lane has written them (`minutes: null` while only the degraded Recap
    /// exists), the confirmed speakers, and the user's note. Next actions are the same
    /// suggestions the panel shows; nothing here executes one (invariant 4).
    fn meeting_recap_get_json(&self, params: &ReadParams) -> String {
        let Some(session_id) = params.id.or_else(|| self.db.latest_meeting_session_id()) else {
            return json!({ "error": "not_found" }).to_string();
        };
        let (Some(session), Some(recap)) =
            (self.db.meeting_session(session_id), self.db.meeting_recap(session_id))
        else {
            return json!({ "error": "not_found" }).to_string();
        };
        let template = crate::meeting::templates::resolve(
            self.db.session_template(session_id).as_deref(),
        );
        // Where each item was said (D5): the transcript line whose words it shares, or null.
        let timed_lines = self.db.meeting_transcript(session_id);
        let anchor_lines: Vec<(i64, &str)> = timed_lines
            .iter()
            .map(|(ts, _speaker, text)| (*ts, text.as_str()))
            .collect();
        let minutes = self.db.meeting_recap_full(session_id).map(|stored| {
            let mut grounded = crate::meeting::minutes::MeetingMinutes::from_stored(stored.clone());
            let sources = self.db.meeting_transcript_sources(session_id).unwrap_or_default();
            crate::meeting::minutes::validate_stored_evidence(&mut grounded, &sources);
            let decisions: Vec<String> =
                serde_json::from_str(&stored.decisions_json).unwrap_or_default();
            let next_actions: Vec<crate::meeting::minutes::NextAction> =
                serde_json::from_str(&stored.next_actions_json).unwrap_or_default();
            let sections: Vec<crate::meeting::minutes::MinutesSection> = stored
                .sections_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let decision_anchors = crate::meeting::anchors::anchors(&decisions, &anchor_lines);
            let action_texts: Vec<String> = next_actions.iter().map(|a| a.text.clone()).collect();
            let next_action_anchors = crate::meeting::anchors::anchors(&action_texts, &anchor_lines);
            json!({
                "summary": stored.summary,
                "decisions": decisions,
                "next_actions": next_actions,
                "sections": sections,
                "model": stored.model,
                "edited_at": stored.edited_at,
                "evidence": grounded.evidence,
                "decision_anchors": decision_anchors,
                "next_action_anchors": next_action_anchors,
            })
        });
        let named = self.db.meeting_speakers(session_id);
        let names: Vec<(String, String)> = named
            .iter()
            .map(|s| (s.speaker_key.clone(), s.display_name.clone()))
            .collect();
        let speakers: Vec<serde_json::Value> = named
            .into_iter()
            .map(|s| json!({ "key": s.speaker_key, "display_name": s.display_name, "source": s.source }))
            .collect();
        // Who spoke how much, by transcript characters — never audio time (invariant 2).
        let transcript = self.db.transcript_for_recap(session_id);
        let talk_share: Vec<serde_json::Value> = crate::meeting::talk_share::talk_share(&transcript)
            .into_iter()
            .map(|t| json!({
                "speaker_key": t.speaker_key,
                "speaker": crate::meeting::export::speaker_label(t.speaker_key.as_deref(), &names),
                "chars": t.chars,
                "share": t.share,
            }))
            .collect();
        json!({
            "session_id": session_id,
            "title": recap.title,
            "started_at": session.started_at,
            "ended_at": session.ended_at,
            "duration_minutes": recap.duration_minutes,
            "app_bundle_id": session.app_bundle_id,
            "template": { "id": template.id, "name": template.name },
            "generation_status": self.connections.as_ref().map_or("unknown", |paths| {
                shogun_mcp::meeting_recap_status::read(
                    &paths.status.with_file_name(shogun_mcp::meeting_recap_status::FILE_NAME),
                    session_id,
                    shogun_mcp::desktop_heartbeat::now_ms(),
                )
            }),
            "degraded": minutes.is_none(),
            "minutes": minutes,
            "speakers": speakers,
            "talk_share": talk_share,
            "note": recap.notes,
            "captured_events": recap.captured_events,
        })
        .to_string()
    }

    /// `skills.run.status`: one run.
    fn skill_run_json(&self, params: &ReadParams) -> String {
        match params.id.and_then(|id| self.db.skill_run(id)) {
            Some(run) => json!({ "run": run }).to_string(),
            None => json!({ "error": "not_found" }).to_string(),
        }
    }

    /// `meeting.transcript.get`: the transcript lines the captions panel replays, with the same
    /// speaker labels the export renders (FR-MT-22). Text only — there is no audio to return.
    fn meeting_transcript_json(&self, params: &ReadParams) -> String {
        let Some(session_id) = params.id.or_else(|| self.db.latest_meeting_session_id()) else {
            return json!({ "error": "not_found" }).to_string();
        };
        if self.db.meeting_session(session_id).is_none() {
            return json!({ "error": "not_found" }).to_string();
        }
        let names: Vec<(String, String)> = self
            .db
            .meeting_speakers(session_id)
            .into_iter()
            .map(|s| (s.speaker_key, s.display_name))
            .collect();
        let Ok(rows) = self.db.meeting_transcript_edit_rows(session_id) else { return json!({"error":"read_failed"}).to_string(); };
        let excluded_segment_ids: Vec<i64> = rows.iter().filter(|row| row.excluded).map(|row| row.id).collect();
        let lines: Vec<serde_json::Value> = rows.into_iter().filter(|row| !row.excluded).map(|row| json!({
            "id": row.id, "ts": row.ts, "speaker_key": row.speaker,
            "speaker": crate::meeting::export::speaker_label(row.speaker.as_deref(), &names),
            "text": row.text, "edited_at": row.edited_at, "original_available": row.original_text.is_some(),
        })).collect();
        json!({ "session_id": session_id, "count": lines.len(), "lines": lines, "excluded_segment_ids": excluded_segment_ids }).to_string()
    }

    // mail triage (issue #319, FR-TG-30)

    /// `mail.triage.list`: the same rows the Mail pane lists (invariant 6), with the read
    /// confidence rule applied per row (FR-API-06): Low-band rows are dropped unless
    /// `include_low`, Medium rows carry `possibly`. Never a body; the list row's snippet is
    /// dropped too — the API sees estimates, not mail content.
    fn mail_triage_list_json(&self, params: &ReadParams) -> String {
        let filter = match mail_filter(&params.mail) {
            Ok(filter) => filter,
            Err(error) => return json!({ "error": error }).to_string(),
        };
        let rows: Vec<serde_json::Value> = self
            .db
            .mail_triage_list(&filter)
            .into_iter()
            .filter_map(|row| triage_row_json(&row, params.mail.include_low))
            .collect();
        json!({ "count": rows.len(), "threads": rows }).to_string()
    }

    /// `mail.triage.get`: one row by thread key, with its provenance event id (FR-ST-02). A
    /// direct lookup is not confidence-gated (the caller named the thread), so the row is
    /// returned at any band with its `possibly` flag.
    fn mail_triage_get_json(&self, params: &ReadParams) -> String {
        let Some(key) = params.key.as_deref().map(str::trim).filter(|k| !k.is_empty()) else {
            return json!({ "error": "missing_thread_key" }).to_string();
        };
        match self.db.mail_triage_get(key) {
            Some(row) => match triage_row_json(&row, true) {
                Some(thread) => json!({ "thread": thread }).to_string(),
                None => json!({ "error": "not_found" }).to_string(),
            },
            None => json!({ "error": "not_found" }).to_string(),
        }
    }

    /// `mail.labels.list`: the label registry as the Settings section shows it.
    fn mail_labels_json(&self) -> String {
        json!({ "labels": self.db.mail_labels() }).to_string()
    }

    /// `mail.sender_rules.list`: hand-made and learned sender rules.
    fn mail_sender_rules_json(&self) -> String {
        json!({ "rules": self.db.mail_sender_rules() }).to_string()
    }

    /// `mail.history.list`: the Gmail write-back history, newest first (the undo list).
    fn mail_history_json(&self, params: &ReadParams) -> String {
        let limit = params
            .mail
            .limit
            .unwrap_or(MAIL_HISTORY_DEFAULT_LIMIT)
            .clamp(1, MAIL_HISTORY_MAX_LIMIT);
        json!({ "history": self.db.mail_write_log_list(limit) }).to_string()
    }

    /// `mail.triage.set` (FR-TG-19, L1): a Stage U correction from the API face.
    fn apply_mail_triage_set(&self, body: &str) -> Result<(), String> {
        let value = mail_body(body)?;
        let thread_key = body_string(&value, "thread_key")?
            .ok_or_else(|| "expected thread_key".to_string())?;
        let change = shogun_memory::mail_triage::UserChange {
            label_key: body_string(&value, "label_key")?,
            bucket: body_string(&value, "bucket")?
                .map(|b| parse_bucket(&b))
                .transpose()?,
            handled: body_bool(&value, "handled")?,
        };
        if change.label_key.is_none() && change.bucket.is_none() && change.handled.is_none() {
            return Err("expected at least one of label_key, bucket, handled".to_string());
        }
        // Existence is checked here so a missing row / label answers "not found" (404) — the
        // memory layer's write path folds its refusals into one write-failed error.
        if self.db.mail_triage_get(&thread_key).is_none() {
            return Err("thread not found".to_string());
        }
        if let Some(label) = change.label_key.as_deref() {
            if !self.db.mail_labels().iter().any(|l| l.key == label) {
                return Err("label not found".to_string());
            }
        }
        self.db
            .mail_triage_set(
                &thread_key,
                &change,
                shogun_memory::mail_triage::FeedbackOrigin::Api,
            )
            .map(|_| ())
            .map_err(remap_mail_error)
    }

    /// `mail.labels.upsert` (L1): a custom label's name / definition / default bucket, or any
    /// label's flags. Granting `auto_archive` / `sync_to_gmail` is the standing approval
    /// (FR-TG-21): it lets SHOGUN change Gmail without asking, so only the user grants it, in
    /// Settings (issue #779). This API write refuses to turn either on — `confirm_standing` does
    /// not grant it — and answers with the disclosure the Settings toggle shows (FR-TG-30).
    /// Re-sending a flag that is already on, and turning one off, are allowed.
    fn apply_mail_label_upsert(&self, body: &str) -> Result<(), String> {
        use shogun_memory::mail_triage::{LabelFlags, LabelKind};
        let value = mail_body(body)?;
        let key = body_string(&value, "key")?;
        let name = body_string(&value, "name")?;
        let definition = body_string(&value, "definition")?;
        let default_bucket = body_string(&value, "default_bucket")?
            .map(|b| parse_bucket(&b))
            .transpose()?;
        let enabled = body_bool(&value, "enabled")?;
        let auto_archive = body_bool(&value, "auto_archive")?;
        let sync_to_gmail = body_bool(&value, "sync_to_gmail")?;
        // Still type-checked so a malformed body is refused; it grants nothing.
        let _ = body_bool(&value, "confirm_standing")?;
        let existing = key
            .as_deref()
            .and_then(|k| self.db.mail_labels().into_iter().find(|l| l.key == k));
        let grants_auto_archive =
            auto_archive == Some(true) && !existing.as_ref().is_some_and(|l| l.auto_archive);
        let grants_sync =
            sync_to_gmail == Some(true) && !existing.as_ref().is_some_and(|l| l.sync_to_gmail);
        if grants_auto_archive || grants_sync {
            return Err(shogun_mcp::memory_api::STANDING_APPROVAL_TEXT.to_string());
        }
        match existing {
            Some(label) if label.kind == LabelKind::Builtin => {
                // Built-ins: flags only. A different name or definition is a refusal, not a
                // silent drop (FR-TG-30).
                if name.as_deref().is_some_and(|n| n != label.name)
                    || definition
                        .as_deref()
                        .is_some_and(|d| Some(d) != label.definition.as_deref())
                {
                    return Err(
                        "invalid label change: built-in labels accept flag changes only"
                            .to_string(),
                    );
                }
                let flags = LabelFlags {
                    enabled,
                    auto_archive,
                    sync_to_gmail,
                    default_bucket,
                };
                if flags == LabelFlags::default() {
                    return Err("expected a flag to change: enabled, auto_archive, sync_to_gmail, default_bucket".to_string());
                }
                self.db
                    .mail_label_set_flags(&label.key, flags)
                    .map(|_| ())
                    .map_err(remap_mail_error)
            }
            existing => {
                // A custom label: create (key optional, derived from the name) or edit.
                let name = name
                    .or_else(|| existing.as_ref().map(|l| l.name.clone()))
                    .ok_or_else(|| "expected name".to_string())?;
                let definition = definition
                    .or_else(|| existing.as_ref().and_then(|l| l.definition.clone()))
                    .ok_or_else(|| "expected definition".to_string())?;
                let bucket = default_bucket
                    .or_else(|| existing.as_ref().map(|l| l.default_bucket))
                    .ok_or_else(|| "expected default_bucket".to_string())?;
                let key = self
                    .db
                    .mail_label_upsert_custom(key.as_deref(), &name, &definition, bucket)
                    .map_err(remap_mail_error)?;
                let flags = LabelFlags {
                    enabled,
                    auto_archive,
                    sync_to_gmail,
                    default_bucket: None,
                };
                if flags != LabelFlags::default() {
                    self.db
                        .mail_label_set_flags(&key, flags)
                        .map_err(remap_mail_error)?;
                }
                Ok(())
            }
        }
    }

    /// `mail.sender_rules.add` (L1). Returns the new rule id.
    fn apply_mail_sender_rule_add(&self, body: &str) -> Result<i64, String> {
        use shogun_memory::mail_triage::SenderRuleMatch;
        let value = mail_body(body)?;
        let match_kind = match body_string(&value, "match_kind")?.as_deref() {
            Some("addr") => SenderRuleMatch::Addr,
            Some("domain") => SenderRuleMatch::Domain,
            _ => return Err("invalid match_kind: expected addr|domain".to_string()),
        };
        let match_value = body_string(&value, "match_value")?
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "expected match_value".to_string())?;
        let label_key = body_string(&value, "label_key")?
            .ok_or_else(|| "expected label_key".to_string())?;
        let bucket = body_string(&value, "bucket")?
            .map(|b| parse_bucket(&b))
            .transpose()?;
        self.db
            .mail_sender_rule_add(match_kind, &match_value, &label_key, bucket)
            .map_err(remap_mail_error)
    }
}

/// Default / maximum rows for `mail.history.list`.
const MAIL_HISTORY_DEFAULT_LIMIT: usize = 50;
const MAIL_HISTORY_MAX_LIMIT: usize = 500;

/// A mail write body: a JSON object (an empty body is an empty object).
/// The `thread_key` of a `mail.triage.archive` / `unarchive` body.
fn mail_thread_key(body: &str) -> Result<String, String> {
    let value = mail_body(body)?;
    body_string(&value, "thread_key")?
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| "expected thread_key".to_string())
}

fn mail_body(body: &str) -> Result<serde_json::Value, String> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "expected a JSON object body".to_string())?;
    if !value.is_object() {
        return Err("expected a JSON object body".to_string());
    }
    Ok(value)
}

/// An optional string field; a present non-string is a refusal, not a silent omission.
fn body_string(value: &serde_json::Value, field: &str) -> Result<Option<String>, String> {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("expected {field} to be a string")),
    }
}

/// An optional bool field; a present non-bool is a refusal.
fn body_bool(value: &serde_json::Value, field: &str) -> Result<Option<bool>, String> {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("expected {field} to be a boolean")),
    }
}

fn parse_bucket(raw: &str) -> Result<shogun_memory::mail_triage::Bucket, String> {
    shogun_memory::mail_triage::Bucket::parse(raw.trim())
        .ok_or_else(|| "invalid bucket: expected important|other".to_string())
}

/// The list controls → the memory layer's filter. Unknown enum values are refused with a stable
/// reason (rendered as a 400 by the REST face) rather than dropped.
fn mail_filter(
    query: &shogun_mcp::backend::MailListQuery,
) -> Result<shogun_memory::mail_triage::TriageFilter, &'static str> {
    use shogun_memory::mail_triage::{Attention, Bucket, TriageFilter};
    let bucket = match query.bucket.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(Bucket::parse(raw).ok_or("invalid_bucket")?),
    };
    let attention = match query.attention.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(Attention::parse(raw).ok_or("invalid_attention")?),
    };
    Ok(TriageFilter {
        bucket,
        attention,
        label_key: query
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string),
        since_ms: query.since_ms,
        include_handled: query.include_handled,
        include_archived: false,
        limit: query.limit.unwrap_or(0),
    })
}

/// One triage row for the API: the stored estimate plus `possibly` and its provenance, without
/// the display snippet (no mail content on this face). `None` when the confidence rule
/// excludes the row (FR-API-06).
fn triage_row_json(
    row: &shogun_memory::mail_triage::TriageRow,
    include_low: bool,
) -> Option<serde_json::Value> {
    use shogun_mcp::memory_api::{read_inclusion, ReadInclusion};
    let possibly = match read_inclusion(row.confidence, include_low) {
        ReadInclusion::Included { possibly } => possibly,
        ReadInclusion::Excluded => return None,
    };
    let mut value = serde_json::to_value(row).ok()?;
    let object = value.as_object_mut()?;
    object.remove("snippet");
    object.insert("possibly".to_string(), json!(possibly));
    object.insert(
        "provenance".to_string(),
        json!([{ "event_id": row.latest_event_id }]),
    );
    Some(value)
}

/// Shape the memory layer's mail errors for the REST status split: a missing row reads as
/// "… not found" (404), a caller mistake starts with "invalid " (400), a real fault stays as it
/// is (500).
fn remap_mail_error(error: String) -> String {
    match error.as_str() {
        "unknown thread" => "thread not found".to_string(),
        "unknown label" => "label not found".to_string(),
        "built-in labels cannot be edited" => {
            "invalid label change: built-in labels accept flag changes only".to_string()
        }
        "built-in labels cannot be deleted" => {
            "invalid label delete: built-in labels cannot be deleted".to_string()
        }
        "custom label keys start with custom:" => format!("invalid key: {error}"),
        _ if error.starts_with("at most ") => format!("invalid label: {error}"),
        _ => error,
    }
}

/// Adapter from daemon's prewarmed reply cache to shared Memory API host seam.
pub struct ReplyContextProvider {
    cache: crate::daemon::ReplyContextCache,
    max_age: std::time::Duration,
}

impl ReplyContextProvider {
    pub fn new(cache: crate::daemon::ReplyContextCache, max_age: std::time::Duration) -> Self {
        Self { cache, max_age }
    }
}

impl CurrentContextProvider for ReplyContextProvider {
    fn current_context(&self) -> CurrentContextSnapshot {
        use crate::daemon::ReplyContextCacheSnapshot;

        match self.cache.snapshot(self.max_age) {
            ReplyContextCacheSnapshot::Fresh(context) => {
                let items = context
                    .as_memory_lines(CURRENT_CONTEXT_LINE_LIMIT)
                    .into_iter()
                    .map(|line| {
                        let bounded: String =
                            line.chars().take(CURRENT_CONTEXT_LINE_CHARS).collect();
                        ReadItem::new(format!("current: {bounded}"), EVENT_CONFIDENCE)
                    })
                    .collect::<Vec<_>>();
                CurrentContextSnapshot {
                    availability: if items.is_empty() {
                        CurrentContextAvailability::Empty
                    } else {
                        CurrentContextAvailability::Fresh
                    },
                    items,
                }
            }
            ReplyContextCacheSnapshot::Empty => CurrentContextSnapshot {
                availability: CurrentContextAvailability::Empty,
                items: Vec::new(),
            },
            ReplyContextCacheSnapshot::Stale => CurrentContextSnapshot {
                availability: CurrentContextAvailability::Stale,
                items: Vec::new(),
            },
        }
    }
}

/// `include_scratch` from a request, folded with the scope the face set for this call (E2-T5):
/// a request may widen reads to the agents' scratch namespaces, never narrow a scope already set.
fn read_scope(include_scratch: bool) -> shogun_memory::actor::ReadScope {
    let current = shogun_memory::actor::current_read();
    shogun_memory::actor::ReadScope { include_scratch: current.include_scratch || include_scratch }
}

impl MemoryBackend for DbBackend {
    fn search(&self, request: &SearchRequestV1) -> Result<MemorySearchResponse, MemorySearchError> {
        shogun_memory::actor::with_read_scope(read_scope(request.include_scratch), || {
        let response = self
            .db
            .search_request(request)
            .map_err(|error| match error {
                crate::daemon::SearchErrorV1::Policy(policy) => MemorySearchError::Policy(policy),
                crate::daemon::SearchErrorV1::Memory(_) => MemorySearchError::Unavailable,
            })?;
        let results = response
            .federated_hits
            .iter()
            .map(|hit| {
                let open_ids = hit.event_ids(SEARCH_OPEN_IDS);
                let place = open_ids
                    .first()
                    .and_then(|id| response.event_places.get(id));
                MemorySearchResult {
                    text: hit.excerpt.chars().take(SEARCH_TEXT_CHARS).collect(),
                    source_kind: source_kind(hit),
                    source_id: source_id(hit),
                    provenance: hit.evidence_ids.iter().map(evidence_id).collect(),
                    title: hit.title.clone(),
                    excerpt: hit.excerpt.clone(),
                    confidence: hit.confidence,
                    possibly: hit.confidence < 0.8,
                    score: hit.fused_score,
                    when: place
                        .map(|place| place.ts)
                        .or(hit.period_end_ms.or(hit.period_start_ms))
                        .map(crate::when::when_json),
                    place: place.and_then(where_json),
                    snippet: Some(shogun_memory::search::snippet(&hit.excerpt, &request.query)),
                    open: (!open_ids.is_empty())
                        .then(|| serde_json::json!({"tool": "memory.get_events", "ids": open_ids})),
                }
            })
            .collect();
        Ok(MemorySearchResponse {
            tool: Tool::MemorySearch.wire_name().to_string(),
            results,
            answerability: response.answerability,
            requested_depth: response.requested_depth,
            effective_depth: response.effective_depth,
            requested_range: response.requested_range,
            resolved_range: response.resolved_range,
            lexical_scope: match response.lexical_scope {
                crate::daemon::SearchLexicalScopeV1::Warm => "warm".to_string(),
                crate::daemon::SearchLexicalScopeV1::AllHistory => "all_history".to_string(),
            },
            complete: response.complete,
            cold_partitions_visited: response.cold_partitions_visited,
            cold_vectors_scanned: response.cold_vectors_scanned,
            elapsed_ms: response.elapsed_ms,
        })
        })
    }

    fn current_context(&self) -> CurrentContextSnapshot {
        self.current_context_provider
            .as_deref()
            .map(CurrentContextProvider::current_context)
            .unwrap_or_else(CurrentContextSnapshot::unavailable)
    }

    fn manage_voice_dictionary(
        &self,
        operation: VoiceDictionaryOperation,
    ) -> Result<VoiceDictionaryResult, String> {
        match operation {
            VoiceDictionaryOperation::List => {
                self.db.list_voice_terms().map(VoiceDictionaryResult::Terms)
            }
            VoiceDictionaryOperation::Create(term) => self
                .db
                .create_voice_term(&term)
                .map(VoiceDictionaryResult::Term),
            VoiceDictionaryOperation::Update { id, term } => self
                .db
                .update_voice_term(id, &term)?
                .map(VoiceDictionaryResult::Term)
                .ok_or_else(|| "voice dictionary term not found".to_string()),
            VoiceDictionaryOperation::Delete { id } => self
                .db
                .delete_voice_term(id)
                .map(VoiceDictionaryResult::Deleted),
            VoiceDictionaryOperation::Import { text } => self
                .db
                .import_voice_terms(&text)
                .map(VoiceDictionaryResult::Imported),
            VoiceDictionaryOperation::Export => self
                .db
                .export_voice_terms_csv()
                .map(VoiceDictionaryResult::Csv),
        }
    }

    /// Memory import on the API faces (#384 / #391) — the same `Db` methods the desktop's Import
    /// memory screen calls; an API commit is recorded as `via = api`.
    fn clarify(&self, operation: shogun_mcp::clarify_api::ClarifyOperation) -> Result<String, String> {
        self.db.clarify_api(operation)
    }

    /// Steering (issue #395): the note goes to the store the desktop's runner reads at its next
    /// turn. Never the L3 queue; the note is never logged or echoed.
    fn steer(&self, operation: shogun_mcp::steer_api::SteerOperation) -> Result<String, String> {
        let path = self
            .steer_store
            .as_deref()
            .ok_or_else(|| "steering is not configured".to_string())?;
        let now = u64::try_from(self.db.now_ms()).unwrap_or(0);
        shogun_mcp::steer_api::apply(path, &operation, now)
    }

    /// `memory.get_events` / `memory.read_session`: capture-log reads (`daemon::sessions`).
    fn activity(
        &self,
        request: &shogun_mcp::activity_api::ActivityRequest,
    ) -> Result<String, String> {
        use shogun_mcp::activity_api::ActivityRequest;
        let value = match request {
            ActivityRequest::Events { ids, max_chars } => self.db.events_json(ids, *max_chars)?,
            ActivityRequest::Session { from_ms, to_ms, app, limit } => {
                self.db.read_session_json(*from_ms, *to_ms, app.as_deref(), *limit)?
            }
        };
        Ok(value.to_string())
    }

    /// E44: a provider change notice goes to the principal's read-sync queue (hosted only).
    fn enqueue_connector_sync(
        &self,
        provider: &str,
        detail: &serde_json::Value,
    ) -> Result<shogun_mcp::connector_vault::SyncQueued, shogun_mcp::connector_vault::VaultError> {
        let (vault, principal) = self.hosted_connectors.as_ref().ok_or(shogun_mcp::connector_vault::VaultError::NotConfigured)?;
        vault.enqueue_sync(principal, provider, detail, self.db.now_ms())
    }

    /// `memory.ingest` (E4-T3): the store's append path under the request's idempotency key.
    fn ingest(
        &self,
        request: &shogun_mcp::ingest_api::IngestRequest,
    ) -> Result<shogun_mcp::ingest_api::IngestOutcome, String> {
        self.db.agent_ingest(request)
    }

    fn ingest_versioned_snapshot(
        &self,
        request: &shogun_mcp::ingest_api::IngestRequest,
    ) -> Result<shogun_mcp::ingest_api::IngestOutcome, String> {
        self.db.ingest_versioned_snapshot(request)
    }

    /// `memory.recall` (E4-T3): the context-pack assembly within the request's token budget.
    fn recall(
        &self,
        request: &shogun_mcp::ingest_api::RecallRequest,
    ) -> Result<shogun_mcp::ingest_api::RecallOutcome, String> {
        self.db.agent_recall(request)
    }

    fn audit_append(&self, record: &shogun_memory::audit::AuditRecord) -> Result<(), String> {
        self.db.audit_append(record)
    }

    fn audit_list(&self, query: &shogun_mcp::governance_api::AuditQuery) -> Result<serde_json::Value, String> {
        self.db.audit_list(query)
    }

    fn acl(&self, operation: &shogun_mcp::governance_api::AclOperation, by: &str) -> Result<serde_json::Value, String> {
        self.db.acl(operation, by)
    }

    fn approval_ledger(
        &self,
        op: shogun_mcp::remote_approvals::LedgerOp<'_>,
    ) -> Result<shogun_mcp::remote_approvals::LedgerReply, String> {
        self.db.approval_ledger(op)
    }

    fn audit_page(
        &self,
        after_id: i64,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        limit: usize,
    ) -> Result<Vec<shogun_memory::audit::AuditEntry>, String> {
        self.db.audit_page(after_id, since_ms, until_ms, limit)
    }

    fn principal_may_read(&self, grantee_principal: &str) -> bool {
        self.db.principal_may_read(grantee_principal)
    }

    /// `payments.*` (E46): the principal's payment ledger, and the payment rail this process is
    /// configured with (`SHOGUN_PAYMENT_RAIL`; none → every payment is refused before anything is
    /// recorded). A build without `exec` has no send path, so it has no rail either.
    fn payments(
        &self,
        op: shogun_mcp::payments_api::PaymentsOp<'_>,
    ) -> Result<shogun_mcp::payments_api::PaymentsReply, String> {
        #[cfg(feature = "exec")]
        use shogun_mcp::payments_api::{PaymentsOp, PaymentsReply};
        match op {
            #[cfg(feature = "exec")]
            PaymentsOp::PayeeClass(payee) => {
                let rail = crate::payment_rail::configured().ok_or("payment_rail_unavailable")?;
                Ok(PaymentsReply::Class(rail.payee_class(payee)))
            }
            #[cfg(feature = "exec")]
            PaymentsOp::PayReserved { confirmed } => {
                let rail = crate::payment_rail::configured().ok_or("payment_rail_unavailable")?;
                Ok(PaymentsReply::Done(matches!(
                    crate::payment_rail::pay_reserved(&self.db, rail, confirmed),
                    crate::payment_rail::Paid::Settled { .. }
                )))
            }
            other => self.db.payments_store(other),
        }
    }

    /// `memory.fs` (E6-T2): the memory tool's commands over `/memories`, as the call's writer.
    fn memory_fs(
        &self,
        command: &shogun_mcp::memfs_tool::FsCommand,
    ) -> Result<shogun_mcp::memfs_tool::FsReply, String> {
        self.db.memory_fs(command)
    }

    /// `state.export` in export format v1 (E7).
    fn export_v1(
        &self,
        request: &shogun_mcp::export_api::ExportRequest,
    ) -> Result<shogun_mcp::export_api::ExportOutput, String> {
        self.db.export_v1(request)
    }

    fn memory_import(&self, operation: MemoryImportOperation) -> Result<String, String> {
        use shogun_memory::memory_import::Via;
        let value = match operation {
            MemoryImportOperation::Preview { body } => self.db.memory_import_preview(&body)?,
            MemoryImportOperation::Commit { body } => {
                self.db.memory_import_commit_as(&body, Via::Api, &shogun_memory::actor::current_write())?
            }
            MemoryImportOperation::List { id, include_low } => self.db.memory_import_list(id, include_low)?,
            MemoryImportOperation::Delete { id } => self.db.memory_import_delete(id)?,
        };
        Ok(value.to_string())
    }

    /// Translation and reply drafts (issue #279): the desktop runs them on the Agent lane; this
    /// face queues the request and waits, or reads back one it stopped waiting for.
    fn ai_search(
        &self,
        query: &str,
        origin: &'static str,
        ai: Result<(), &'static str>,
    ) -> Result<String, String> {
        self.ai_search_request(query, origin, ai)
    }

    fn forms(&self, operation: shogun_mcp::forms_api::FormOperation, auth: shogun_mcp::forms_api::FormsAuth) -> Result<String, String> {
        let path = self.forms_db_path.as_ref().ok_or("form_unavailable")?;
        shogun_mcp::forms_api::request(path, operation, auth)
    }

    fn translate(
        &self,
        operation: shogun_mcp::translate_api::TranslateOperation,
    ) -> Result<String, String> {
        use shogun_mcp::translate_api::{refusal_json, TranslateOperation};
        match operation {
            TranslateOperation::Request { kind, body, origin } => {
                self.translate_request(kind, &body, origin)
            }
            TranslateOperation::Status { id } => Ok(match self.db.translate_job_take(id)? {
                Some(job) => Self::translate_job_json(&job),
                None => refusal_json("not_found", None),
            }),
        }
    }

    fn read(&self, tool: Tool, params: &ReadParams) -> Vec<ReadItem> {
        // E2-T5: `include_scratch` widens the call's read scope to the agents' scratch namespaces.
        shogun_memory::actor::with_read_scope(read_scope(params.include_scratch), || {
        // A single row `Option` → a 0/1-length result (for the `get` variants).
        fn one<T>(row: Option<T>, f: impl Fn(T) -> ReadItem) -> Vec<ReadItem> {
            row.into_iter().map(f).collect()
        }
        let id = params.id;

        match tool {
            Tool::MemorySearch => self
                .db
                .search(params.query.as_deref().unwrap_or(""), SEARCH_LIMIT)
                .into_iter()
                .map(|hit| ReadItem::new(hit.content, EVENT_CONFIDENCE))
                .collect(),
            // DB context plus optional host-owned RAM cache; standalone reports it unavailable.
            Tool::MemoryGetContext => self.get_context_items(),

            // FR-API-08: the grounded context pack for one task/question — the same
            // `Db::assemble_context` the in-app chat uses (invariant 6), flattened to labeled
            // rows. Facts are already confidence-gated by `inline_memory` (medium carries its
            // `possibly:` prefix), and evidence rows are ground-truth events, so both pass the
            // API gate at event confidence; each evidence line carries its provenance inline
            // (event id, source, timestamp).
            //
            // `pack.screen_frames` is deliberately NOT flattened in here. Those are stored JPEGs
            // (the invariant-2 exception), and reaching them is its own separately-gated tool —
            // `visual_recall.get_frame`, which answers to Visual recall's on/off state. Handing
            // an API caller frame ids as part of a text pack would route around that switch.
            Tool::MemoryGetContextPack => {
                let query = params.query.as_deref().unwrap_or("");
                if query.is_empty() {
                    return Vec::new();
                }
                let pack = self
                    .db
                    .assemble_context(query, PACK_HITS, PACK_EXCERPT_CHARS);
                pack.facts
                    .into_iter()
                    .map(|f| ReadItem::new(format!("fact: {f}"), EVENT_CONFIDENCE))
                    .chain(pack.evidence.into_iter()
                        // Owner decision 2026-10-20: a withheld connection's pack has no meeting
                        // or screen-text evidence.
                        .filter(|e| shogun_mcp::source_policy::current_allows_source(&e.source))
                        .map(|e| {
                        let title = e.title.as_deref().unwrap_or("-");
                        ReadItem::new(
                            format!(
                                "evidence event:{} source:{} ts:{} title:{} :: {}",
                                e.event_id, e.source, e.ts, title, e.excerpt
                            ),
                            EVENT_CONFIDENCE,
                        )
                    }))
                    .collect()
            }

            // Every state item carries its row (`table`, `id`), which `state get`, `state edit|delete`
            // and `state split` take (issue #779). People and projects also keep their wiki key.
            Tool::StatePeopleList => self
                .db
                .people()
                .into_iter()
                .map(|p| ReadItem { key: Some(format!("person-{}", p.id)), ..ReadItem::new(p.display_name, p.confidence).with_row("people", p.id) })
                .collect(),
            // One row also says where its evidence comes from (`sources`, issue #1362); lists do
            // not, so a long list stays one query.
            Tool::StatePeopleGet => one(id.and_then(|i| self.db.person(i)), |p| {
                ReadItem { key: Some(format!("person-{}", p.id)), ..ReadItem::new(p.display_name, p.confidence).with_row("people", p.id) }
                    .with_sources(self.db.evidence_sources(shogun_memory::state::StateTable::People, p.id))
            }),
            Tool::StateProjectsList => self
                .db
                .projects()
                .into_iter()
                .map(|p| ReadItem { key: Some(format!("project-{}", p.id)), ..ReadItem::new(p.name, p.confidence).with_row("projects", p.id) })
                .collect(),
            Tool::StateProjectsGet => one(id.and_then(|i| self.db.project(i)), |p| {
                ReadItem { key: Some(format!("project-{}", p.id)), ..ReadItem::new(p.name, p.confidence).with_row("projects", p.id) }
                    .with_sources(self.db.evidence_sources(shogun_memory::state::StateTable::Projects, p.id))
            }),
            // The same rows the Fusion supply lists (`commitments_due` / `open_loops`: resolved ones
            // left out, in the same order), read with their ids.
            // Snoozed rows stay off the lists, as on the dashboard; `get` returns them with
            // `deferred_until`.
            Tool::StateCommitmentsList => {
                let now = self.db.now_ms();
                self.db
                    .commitment_rows()
                    .into_iter()
                    .filter(|c| c.status != "done" && c.status != "cancelled" && !c.is_deferred(now))
                    .map(|c| ReadItem::new(c.description, c.confidence).with_row("commitments", c.id))
                    .collect()
            }
            Tool::StateCommitmentsGet => one(id.and_then(|i| self.db.commitment(i)), |c| {
                ReadItem::new(c.description, c.confidence).with_row("commitments", c.id).with_deferred(c.deferred_until)
            }),
            Tool::StateOpenLoopsList => {
                let now = self.db.now_ms();
                self.db
                    .open_loop_rows()
                    .into_iter()
                    .filter(|o| o.status != "closed" && !o.is_deferred(now))
                    // #926: a local mail guess reads `possibly` here as on the dashboard.
                    .map(|o| {
                        let confidence = shogun_fusion::confidence::wording_confidence(o.confidence, o.inferred);
                        ReadItem::new(o.description, confidence).with_row("open_loops", o.id)
                    })
                    .collect()
            }
            Tool::StateOpenLoopsGet => one(id.and_then(|i| self.db.open_loop(i)), |o| {
                let confidence = shogun_fusion::confidence::wording_confidence(o.confidence, o.inferred);
                ReadItem::new(o.description, confidence).with_row("open_loops", o.id).with_deferred(o.deferred_until)
            }),

            // Saved setup progress is shared-file structured JSON, never a confidence row.
            Tool::DeviceOnboardingGet | Tool::DeviceAccountGet => Vec::new(),
            Tool::CaptureStatus | Tool::CaptureSetPaused => Vec::new(),

            // Structured reads (visual recall, lessons.list, memory.get_wrap) use
            // [`Self::read_structured`]; the rest are not read tools (write / action) — never
            // routed here.
            Tool::MemoryGetWrap
            | Tool::CalendarUpcoming
            | Tool::MemoryGetPeriods
            // Answered by the typed [`MemoryBackend::activity`].
            | Tool::MemoryGetEvents
            | Tool::MemoryReadSession
            | Tool::DeviceReleaseNotesGet
            | Tool::DreamStatus
            | Tool::MemoryProcessingStatus
            | Tool::DreamRetry
            | Tool::LessonsList
            | Tool::StateDisputesList
            | Tool::StateDisputesResolve
            | Tool::StateUpdate
            | Tool::StateDelete
            | Tool::StateCreate
            | Tool::StateExport
            | Tool::StateSplit
            | Tool::StateSplitEvidence
            | Tool::StateMergeCandidates
            | Tool::StateMerge
            | Tool::StateKeepSeparate
            // Memory import is answered by the typed [`MemoryBackend::memory_import`].
            | Tool::MemoryImportPreview
            | Tool::MemoryImport
            | Tool::MemoryImportList
            | Tool::MemoryImportDelete
            | Tool::ProfileWhoami
            | Tool::VoiceDictionaryList
            | Tool::VoiceDictionaryExport
            // Meeting and dictation microphone settings live in the shared app-data files and are
            // intercepted before the DB backend by the MCP/REST adapter.
            | Tool::MeetingReminderGet
            | Tool::MeetingMicrophoneGet
            | Tool::VoiceMicrophoneGet
            | Tool::VoiceMicrophoneSet
            | Tool::WritingVoiceGet
            | Tool::WritingVoiceSet
            // The Agent lane's source (issue #110): a structured read and a settings write.
            | Tool::AgentSourceGet
            | Tool::AgentSourceSet
            // The Connections screen (issue #82): a structured read and two narrowing writes.
            | Tool::ConnectionsList
            | Tool::ConnectionsDisconnect
            | Tool::ConnectionsComposioSet
            | Tool::ConnectionsGmailSet
            // Issue #400: the token registry is the face's, never the DB's; answered by MCP / REST.
            | Tool::ApiTokensList
            | Tool::ApiConnectionsRestrict
            // The translation register lives in the shared `translate.json` (#279 FR-TL-07),
            // served by the MCP/REST adapter like the microphone settings.
            | Tool::TranslateRegisterGet
            | Tool::TranslateRegisterSet
            | Tool::TranslatePairGet
            | Tool::TranslatePairSet
            // Translation / reply drafts (#279) are answered by the typed
            // [`MemoryBackend::translate`].
            | Tool::TranslateText
            | Tool::TranslateReplyDraft
            | Tool::FormsInspect
            | Tool::FormsPlan
            | Tool::FormsStatus
            | Tool::FormsApply
            | Tool::FormsClose
            | Tool::FormsUndo
            | Tool::TranslateStatus
            | Tool::UiLanguageGet
            | Tool::UiLanguageSet
            | Tool::DeviceUpdateGet
            | Tool::VoiceDictionaryCreate
            | Tool::VoiceDictionaryUpdate
            | Tool::VoiceDictionaryDelete
            | Tool::VoiceDictionaryImport
            | Tool::MeetingReminderSet
            | Tool::MeetingMicrophoneSet
            | Tool::MeetingRecapPrefsGet
            | Tool::MeetingRecapPrefsSet
            | Tool::MeetingRecapEdit
            | Tool::MeetingSearch
            | Tool::MeetingDelete
            | Tool::MeetingRecordNow
            | Tool::MeetingSystemAudioStatus
            | Tool::MeetingSystemAudioRetry
            | Tool::MeetingStartStatus
            | Tool::LessonsSetActive
            | Tool::VisualRecallStatus
            | Tool::VisualRecallSearchFrames
            | Tool::VisualRecallGetFrame
            | Tool::VisualRecallFrameTextLines
            | Tool::VisualRecallRescanFrame
            | Tool::VisualRecallSetEnabled
            | Tool::VisualRecallSetRetention
            | Tool::BackupStatus
            | Tool::BackupSetRetention
            | Tool::SearchIndexStatus
            | Tool::StoragePreview
            | Tool::SearchIndexCompactStart
            | Tool::SearchIndexCompactStop
            | Tool::VisualRecallDeleteFrame
            | Tool::CaptureExclusionsGet
            | Tool::CaptureExclusionsSet
            | Tool::MemoryAppendNote
            // Served by `ingest` / `recall` (E4-T3), never through the generic read channel.
            | Tool::MemoryIngest
            | Tool::MemoryIngestWebhook
            | Tool::MemoryRecall
            // Served by `memory_fs` (E6-T2).
            | Tool::MemoryFs
            // Served by `handoff_api` over `recall` / `ingest` (E35).
            | Tool::HandoffCreate
            | Tool::HandoffAccept
            // Served by `payments_api` over `payments` (E46).
            | Tool::PaymentsMandatesList
            | Tool::PaymentsMandatesRequest
            | Tool::PaymentsMandatesRevoke
            | Tool::PaymentsSpend
            // Served by the approval queue (E15) and by `audit_list` / `acl` (E18).
            | Tool::ApprovalsList
            | Tool::ApprovalsGet
            | Tool::AuditList
            | Tool::AclList
            | Tool::MemoryNotesList
            | Tool::MemoryNotesChange
            | Tool::ProfileSet
            | Tool::DictionaryAutoAddGet
            | Tool::DictionaryAutoAddSet
            | Tool::ActionsExecute
            | Tool::ActionsStatus
            // Clarify (issue #394) is answered by the typed [`MemoryBackend::clarify`].
            | Tool::ActionsQuestions
            | Tool::ActionsAnswer
            // Steering (issue #395) is answered by the typed [`MemoryBackend::steer`].
            | Tool::ActionsSteer
            // Speakers and templates are structured reads / L1 writes, never a ReadItem list.
            | Tool::MeetingSpeakersGet
            | Tool::MeetingSpeakersSet
            | Tool::MeetingTemplatesGet
            | Tool::MeetingTemplatesSet
            | Tool::MeetingRecapTrack
            | Tool::MeetingRecapExport
            | Tool::SkillsList
            | Tool::SkillsRun
            | Tool::SkillsSave
            | Tool::SkillsRunStatus
            | Tool::SchedulesList
            | Tool::SupportReportsList
            | Tool::SchedulesAdd
            | Tool::SchedulesRemove
            | Tool::MeetingRecapGet
            | Tool::MeetingTranscriptGet
            | Tool::MeetingList
            | Tool::WebVideoTranscript
            | Tool::WebSocialPost
            | Tool::WebPage
            | Tool::WikiEvidence
            | Tool::WikiNeighbors
            | Tool::WikiList
            | Tool::PatternsList
            | Tool::PatternsSetActive
            // Mail triage: structured reads / L1-L2 writes, never a ReadItem list.
            | Tool::MailTriageList
            | Tool::MailTriageGet
            | Tool::MailTriageSet
            | Tool::MailTriageArchive
            | Tool::MailTriageUnarchive
            | Tool::MailLabelsList
            | Tool::MailLabelsUpsert
            | Tool::MailLabelsDelete
            | Tool::MailSenderRulesList
            | Tool::MailSenderRulesAdd
            | Tool::MailSenderRulesDelete
            | Tool::MailHistoryList
            | Tool::MailSettingsGet
            | Tool::MailSettingsSet
            // File exports (issue #397): structured reads / an L2 request / an L1 removal.
            | Tool::ExportWriteFile
            | Tool::ExportWritesList
            | Tool::ExportFoldersList
            | Tool::ExportFoldersRemove => Vec::new(),
        }
        })
    }

    fn read_structured(&self, tool: Tool, params: &ReadParams) -> Option<String> {
        Some(match tool {
            Tool::CaptureStatus => {
                let result = self.capture_control_path().and_then(|path| {
                    crate::capture::control::read_status(&path, crate::capture::control::now_ms())
                });
                match result {
                    // `capture_events_10m`: Accessibility capture events in the last 10 minutes,
                    // a count only (never text). One path for MCP / REST / CLI (invariant 6).
                    Ok(status) => serde_json::json!({
                        "paused": status.paused,
                        "resume_at_ms": status.resume_at_ms,
                        "capture_events_10m": self.db.capture_event_count_10m(),
                    })
                    .to_string(),
                    Err(error) => serde_json::json!({"error": error}).to_string(),
                }
            },
            Tool::DeviceOnboardingGet => shogun_mcp::onboarding_state_api::read(
                self.onboarding_state_path.as_deref(),
            ).to_string(),
            // The account snapshot the desktop writes, plus the plan every face resolves (#148
            // P2). Read-only; no token or device id is in either.
            Tool::DeviceAccountGet => shogun_mcp::account_status_api::read(
                self.account_state_path.as_deref(),
                &shogun_mcp::plan_source::FilePlanSource::from_env(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                    .unwrap_or(0),
            )
            .to_string(),
            Tool::VisualRecallStatus => self.visual_recall_status_json(),
            Tool::BackupStatus => self.backup_status_json(),
            // The desktop's compaction job as it last published it, plus the index shape read
            // here (#714 step 3): the same JSON Settings renders.
            Tool::SearchIndexStatus => match self.search_index() {
                Ok(config) => crate::search_index_compact::status_json(
                    &self.db,
                    &config.paths,
                    config.free.as_ref(),
                    None,
                )
                .to_string(),
                Err(error) => json!({ "error": error }).to_string(),
            },
            Tool::StoragePreview => match self.db.storage_preview() {
                Ok(preview) => json!(preview).to_string(),
                Err(_) => json!({ "error": "storage_preview_unavailable" }).to_string(),
            },
            // Static, compiled in: every face gets the real answer with no shared-store problem.
            // The notes describe the Mac app's releases, so only its build carries them (E1-T7).
            #[cfg(feature = "desktop")]
            Tool::DeviceReleaseNotesGet => crate::release_notes::catalog_json(),
            #[cfg(not(feature = "desktop"))]
            Tool::DeviceReleaseNotesGet => json!({ "error": "unavailable" }).to_string(),
            // The Settings "Nightly review" row, from the same ledger and the same shared
            // `dream_outcome.json` (issue #285, invariant 6).
            // `actions` (#285) adds what this process knows beyond the ledger.
            Tool::DreamStatus => {
                let mut status = match self.dream_action_facts.as_ref() {
                    Some(facts) => self.db.dream_status_json_with(&facts()),
                    None => self.db.dream_status_json(),
                };
                mark_dream_actions_app_only(&mut status);
                status.to_string()
            }
            Tool::MemoryProcessingStatus => match params.id.filter(|id| *id > 0) {
                Some(id) => self.db.memory_processing_status(id, params.include_scratch).to_string(),
                None => json!({"error":"invalid_event_id"}).to_string(),
            },
            Tool::WritingVoiceGet => self.writing_voice_json(),
            Tool::DictionaryAutoAddGet => serde_json::to_string(&self.db.dictionary_autoadd_view().ok()?).ok()?,
            Tool::VisualRecallSearchFrames => self.visual_recall_search_json(params),
            Tool::VisualRecallGetFrame => self.visual_recall_get_frame_json(params),
            Tool::VisualRecallFrameTextLines => self.visual_recall_text_lines_json(params),
            Tool::VisualRecallRescanFrame => self.visual_recall_rescan_json(params),
            Tool::CaptureExclusionsGet => self.capture_exclusions_json(),
            Tool::AgentSourceGet => self.agent_source_json(),
            Tool::ConnectionsList if self.connections.is_none() && self.hosted_connectors.is_some() => self.hosted_connections_json()?,
            Tool::ConnectionsList => match self.connections_paths() {
                Ok(paths) => {
                    let mut answer = shogun_mcp::connections_api::list_json(
                        paths,
                        shogun_mcp::desktop_heartbeat::now_ms(),
                    );
                    if params.include_account_labels {
                        if let Some(rows) = answer["services"].as_array_mut() {
                            for row in rows {
                                let label = match row["state"].as_str() {
                                    Some("connected" | "needs_reauth") => {
                                        #[cfg(feature = "desktop")]
                                        {
                                            crate::desktop::keychain::connection_account_label(row["service"].as_str())
                                        }
                                        #[cfg(not(feature = "desktop"))]
                                        { None::<String> }
                                    }
                                    _ => None,
                                };
                                row["account_label"] = json!(label);
                            }
                        }
                    }
                    answer.to_string()
                },
                Err(error) => json!({ "error": error }).to_string(),
            },
            Tool::MemoryNotesList => self.db.memory_notes_list(params.id).unwrap_or_else(|_| json!({"error":"memory_unavailable"})).to_string(),
            Tool::LessonsList => self.lessons_json(params),
            Tool::StateDisputesList => self.disputes_json(params),
            Tool::StateExport => self.world_model_export_json(params),
            Tool::StateSplitEvidence => self.state_split_evidence_json(params),
            Tool::StateMergeCandidates => self.state_merge_candidates_json(params),
            Tool::MemoryGetWrap => self.evening_wrap_json(),
            Tool::CalendarUpcoming => self.calendar_upcoming_json(params),
            Tool::MemoryGetPeriods => {
                let now = self.db.now_ms();
                // Omitted window = the last 24 hours (the usual "what happened today" ask).
                let to = params.to_ms.unwrap_or(now);
                let from = params.from_ms.unwrap_or_else(|| to.saturating_sub(24 * 3_600_000));
                if from >= to {
                    json!({"error":"invalid_range"}).to_string()
                } else {
                    let limit = params.page_size.unwrap_or(24).clamp(1, 48);
                    self.db
                        .period_summaries_json(from, to, limit)
                        .unwrap_or_else(|e| json!({"error": e}))
                        .to_string()
                }
            }
            Tool::ProfileWhoami => self.whoami_json(),
            Tool::MemoryGetContext => self.get_context_json(),
            Tool::MeetingSpeakersGet => self.meeting_speakers_json(params),
            Tool::MeetingTemplatesGet => self.meeting_templates_json(params),
            Tool::MeetingRecapExport => self.meeting_recap_export_json(params),
            Tool::SkillsList => self.skills_list_json(params),
            Tool::SkillsRunStatus => self.skill_run_json(params),
            Tool::SchedulesList => self.schedules_json(),
            Tool::SupportReportsList => shogun_mcp::support_reports::list_json(),
            Tool::ExportWritesList => self.local_writes_json(params),
            Tool::ExportFoldersList => shogun_mcp::export_folders::list_json(),
            Tool::MeetingList => self.meeting_list_json(params),
            Tool::WebVideoTranscript | Tool::WebSocialPost | Tool::WebPage => {
                self.web_read_json(tool, params)
            }
            Tool::MeetingRecapGet => self.meeting_recap_get_json(params),
            Tool::MeetingTranscriptGet => self.meeting_transcript_json(params),
            // `meeting.search` (D6): the Meetings list's search box. Lexical over minutes and
            // transcripts; the snippet is the matching content the retrieval layer chose.
            Tool::MeetingSearch => {
                let query = params.query.as_deref().unwrap_or("").trim();
                if query.is_empty() {
                    return Some(json!({ "error": "missing_query" }).to_string());
                }
                let hits: Vec<serde_json::Value> = self
                    .db
                    .search_meetings(query, 20)
                    .into_iter()
                    .map(|hit| json!({
                        "session_id": hit.session_id,
                        "started_at": hit.ts,
                        "title": hit.title,
                        "snippet": hit.content,
                        "score": hit.score,
                    }))
                    .collect();
                json!({ "query": query, "hits": hits }).to_string()
            }
            Tool::WikiEvidence | Tool::WikiNeighbors => self.wiki_review_json(tool, params),
            Tool::WikiList => match self.db.knowledge_rows() {
                Ok(rows) => json!({"review_only":true,"results":rows.into_iter().filter_map(|row| {
                    let shogun_mcp::memory_api::ReadInclusion::Included { possibly } = shogun_mcp::memory_api::read_inclusion(row.confidence,params.include_low) else { return None; };
                    Some(json!({"key":row.key,"kind":row.kind,"text":if possibly {format!("possibly: {}",row.name)} else {row.name},"detail":row.detail,"confidence":row.confidence,"possibly":possibly}))
                }).collect::<Vec<_>>()}),
                Err(_) => json!({"error":"knowledge_unavailable"}),
            }.to_string(),
            Tool::PatternsList => self.patterns_json(),
            Tool::MailTriageList => self.mail_triage_list_json(params),
            Tool::MailTriageGet => self.mail_triage_get_json(params),
            Tool::MailLabelsList => self.mail_labels_json(),
            Tool::MailSenderRulesList => self.mail_sender_rules_json(),
            Tool::MailHistoryList => self.mail_history_json(params),
            Tool::MailSettingsGet => {
                serde_json::json!({ "enabled": self.db.mail_triage_enabled() }).to_string()
            }
            // Voice dictionary reads go through the typed `manage_voice_dictionary` contract,
            // never the text seam.
            Tool::VoiceDictionaryList | Tool::VoiceDictionaryExport => return None,
            _ => return None,
        })
    }

    fn capture_unpause_requested(
        &self,
        body: &str,
    ) -> Result<Option<shogun_agents::permission::PausedCapture>, String> {
        let now = crate::capture::control::now_ms();
        let status = capture_pause_request(body, now)?;
        let path = self.capture_control_path()?;
        let current = paused_now(crate::capture::control::read_status(&path, now));
        // The request carries the pause it ends, so confirming after the user changed the pause
        // fails closed instead of ending a newer one.
        Ok((!status.paused && current.resumes_sooner(status, now)).then_some(
            shogun_agents::permission::PausedCapture { resume_at_ms: current.resume_at_ms },
        ))
    }

    fn capture_exclusions_change(
        &self,
        body: &str,
    ) -> Result<Option<shogun_agents::permission::CaptureExclusionChange>, String> {
        let payload = CaptureExclusionsPayload::parse(body)?;
        let path = self
            .capture_exclusions_path()
            .ok_or("capture exclusions path not configured")?;
        let plan = crate::capture::exclusion::plan_replacement(path, &payload.apps, &payload.websites)?;
        let resumes = plan.resumes_capture();
        Ok(resumes.then_some(shogun_agents::permission::CaptureExclusionChange {
            apps: plan.apps,
            websites: plan.websites,
            resumed_apps: plan.resumed_apps,
            resumed_websites: plan.resumed_websites,
        }))
    }

    fn visual_recall_change(
        &self,
        tool: Tool,
        body: &str,
    ) -> Result<Option<shogun_agents::permission::VisualRecallWidening>, String> {
        let path = self
            .visual_recall_settings_path()
            .ok_or("visual_recall settings path not configured")?;
        with_locked_settings(path, |settings| visual_recall_widening(tool, body, settings))?
    }

    fn mail_archive_change(
        &self,
        tool: Tool,
        body: &str,
    ) -> Result<Option<shogun_mcp::mail_archive_api::MailArchiveRequest>, String> {
        if !shogun_mcp::mail_archive_api::is_archive_write(tool) {
            return Err("not a mail archive request".to_string());
        }
        let thread_key = mail_thread_key(body)?;
        let unarchive = tool == Tool::MailTriageUnarchive;
        Ok(self
            .db
            .mail_archive_needs_approval(&thread_key, unarchive)?
            .then_some(shogun_mcp::mail_archive_api::MailArchiveRequest { thread_key, unarchive }))
    }

    fn agent_source_change(
        &self,
        body: &str,
    ) -> Result<Option<shogun_mcp::backend::AgentSubscriptionRequest>, String> {
        let config = self.agent_source.as_ref().ok_or("agent source settings are not configured")?;
        Ok(crate::agent_source::classify(&config.path, body)?.map(|delegate| {
            shogun_mcp::backend::AgentSubscriptionRequest {
                delegate: delegate.id().to_string(),
                label: delegate.label().to_string(),
                plan: delegate.plan_label().to_string(),
            }
        }))
    }

    /// E51: on a hosted sidecar the disconnect's receipt (with `vendor_revoke`) for every face; on
    /// the Mac `None`, and the faces queue it for the desktop through [`Self::write`].
    fn connections_disconnect(&self, body: &str) -> Option<Result<serde_json::Value, String>> {
        if self.connections.is_some() {
            return None;
        }
        self.hosted_disconnect(body)
    }

    fn write(&self, tool: Tool, body: &str) -> WriteResult {
        // Where a tool can widen something (grant a subscription, turn sharing or Visual recall on,
        // shorten a pause, drop an exclusion), the faces queue the request for the user's in-app
        // approval and the desktop applies it on confirm (`Db::apply_confirmed_setting`). A widening
        // request that reaches this match means a face skipped that queue, so it is refused here,
        // decided under the setting's own lock.
        match tool {
            Tool::DictionaryAutoAddSet => {
                let action: crate::daemon::dictionary_autoadd::AutoAddAction = serde_json::from_str(body)
                    .map_err(|_| "invalid dictionary auto-add action".to_string())?;
                self.db.dictionary_autoadd_action(action)?;
                Ok(None)
            }
            // Issue #82: both only remove capability. A disconnect is recorded for the desktop,
            // which alone can delete the Keychain credential; the Mail policy is written here and
            // `composio_set` refuses granting consent or turning draft-only mode off on its own.
            Tool::ConnectionsDisconnect => {
                if self.connections.is_none() {
                    if let Some(answer) = self.hosted_disconnect(body) {
                        return answer.map(|_| None);
                    }
                }
                shogun_mcp::connections_api::disconnect(self.connections_paths()?, body)?;
                Ok(None)
            }
            Tool::ConnectionsGmailSet => {
                shogun_mcp::connections_api::gmail_set(self.connections_paths()?, body)?;
                Ok(None)
            }
            Tool::ConnectionsComposioSet => {
                shogun_mcp::connections_api::composio_set(self.connections_paths()?, body)?;
                Ok(None)
            }
            Tool::AgentSourceSet => {
                // Issue #110: an API caller may switch to an API key, change the model, or
                // withdraw the subscription opt-in here. Selecting a subscription is the opt-in,
                // so `apply_direct` refuses it (re-read under the write lock).
                let config = self.agent_source.as_ref().ok_or("agent source settings are not configured")?;
                crate::agent_source::apply_direct(&config.path, body)?;
                Ok(None)
            }
            Tool::WritingVoiceSet => {
                use shogun_mcp::writing_voice_api::{parse, Change};
                match parse(body) {
                    // Issue #654: an API caller cannot turn sharing on; refuse rather than enable
                    // on the caller's own `consent` claim.
                    Ok(Change::Enable) => {
                        return Err("invalid writing voice request: enable needs approval in the ShogunAI app".into())
                    }
                    Ok(Change::Disable) => self.db.set_voice_learning_enabled(false)?,
                    Ok(Change::Clear) => self.db.clear_voice_learning()?,
                    Ok(Change::NoteDelete(id)) => {
                        if !self.db.rewrite_delete_note(id)? {
                            return Err("style note not found".into());
                        }
                    }
                    Ok(Change::NoteConfirm(id)) => {
                        if !self.db.rewrite_rate_note(id, true)? {
                            return Err("style note not found".into());
                        }
                    }
                    Ok(Change::NoteReject(id)) => {
                        if !self.db.rewrite_rate_note(id, false)? {
                            return Err("style note not found".into());
                        }
                    }
                    Err(_) => return Err("invalid writing voice request or missing consent".into()),
                }
                Ok(None)
            }
            Tool::CaptureSetPaused => {
                let path = self.capture_control_path()?;
                let guard = crate::capture::control::acquire(&path)?;
                let now = crate::capture::control::now_ms();
                let status = capture_pause_request(body, now)?;
                // Issue #897: an API caller may pause, or keep a pause at least as long; ending or
                // shortening the user's pause starts capture sooner. Re-checked under the lock, so
                // a pause set meanwhile still counts.
                if paused_now(guard.status(&path, now)).resumes_sooner(status, now) {
                    return Err(if status.paused { CAPTURE_SHORTEN_REFUSAL } else { CAPTURE_RESUME_REFUSAL }
                        .into());
                }
                guard.save(&path, status, now)?;
                Ok(None)
            },
            Tool::CaptureExclusionsSet => {
                let payload = CaptureExclusionsPayload::parse(body)?;
                let path = self.capture_exclusions_path().ok_or("capture exclusions path not configured")?;
                // Issue #657: an API caller may only add exclusions here; a replacement that drops
                // one resumes capture and is refused. The lists are re-read under the write lock,
                // so a concurrent change cannot slip one through.
                crate::capture::exclusion::replace_policy_resuming_at_most(
                    path,
                    &payload.apps,
                    &payload.websites,
                    &[],
                    &[],
                )?;
                Ok(None)
            }
            Tool::ProfileSet => {
                self.apply_profile_set(body)?;
                Ok(None)
            }
            // Confirm who a speaker key is (FR-MT-15) — the same explicit assignment the Recap
            // chips make; an empty display_name clears it. Source is recorded as the API face.
            Tool::MeetingSpeakersSet => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid speaker assignment request".to_string())?;
                let session_id = match value.get("session_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => self
                        .db
                        .latest_meeting_session_id()
                        .ok_or_else(|| "no meeting session to name speakers for".to_string())?,
                };
                let speaker_key = value
                    .get("speaker_key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing speaker_key".to_string())?;
                let display_name = value
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                self.db
                    .assign_meeting_speaker(session_id, speaker_key, display_name, "user")?;
                Ok(None)
            }
            // Delete one saved meeting for good (D6). The REST face folds the path id into the
            // body as `session_id`, so both faces send the same JSON.
            Tool::MeetingDelete => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid meeting delete request".to_string())?;
                let session_id = value
                    .get("session_id")
                    .and_then(|v| v.as_i64())
                    .filter(|id| *id > 0)
                    .ok_or_else(|| "missing session_id".to_string())?;
                if !self.db.delete_meeting(session_id)? {
                    return Err("session not found".to_string());
                }
                Ok(None)
            }
            // Rewrite part of a session's minutes by hand (D1) — the same edit the Recap panel
            // makes. `reset` hands edited minutes back to the model instead.
            Tool::MeetingRecapEdit => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid minutes edit request".to_string())?;
                if let Some(edits) = value.get("transcript_edits") {
                    let session_id = value.get("session_id").and_then(|id| id.as_i64()).filter(|id| *id > 0).ok_or("transcript edits require session_id")?;
                    if ["summary", "decisions", "next_actions", "reset"].iter().any(|key| value.get(*key).is_some()) { return Err("send transcript edits separately from minutes edits".into()); }
                    let edits: Vec<shogun_memory::transcript_segments::TranscriptEdit> = serde_json::from_value(edits.clone()).map_err(|_| "invalid transcript edits")?;
                    self.db.edit_meeting_transcript(session_id, &edits)?;
                    return Ok(None);
                }

                let session_id = match value.get("session_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => self
                        .db
                        .latest_meeting_session_id()
                        .ok_or_else(|| "no meeting session to edit".to_string())?,
                };
                if value.get("reset").and_then(|v| v.as_bool()) == Some(true) {
                    self.db.reset_meeting_recap_edit(session_id)?;
                    return Ok(None);
                }
                let edit: crate::meeting::minutes::MinutesEdit = serde_json::from_value(value)
                    .map_err(|_| "invalid minutes edit request".to_string())?;
                if !self.db.edit_meeting_recap(session_id, edit)? {
                    return Err("session not found".to_string());
                }
                Ok(None)
            }
            // Choose the template that shapes a session's minutes — the Recap picker's own L1
            // selection. 'general' clears back to the default, mirroring the desktop command.
            Tool::MeetingTemplatesSet => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid template request".to_string())?;
                let session_id = match value.get("session_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => self
                        .db
                        .latest_meeting_session_id()
                        .ok_or_else(|| "no meeting session to set a template for".to_string())?,
                };
                let template_id = value
                    .get("template_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing template_id".to_string())?;
                let cleared = template_id == crate::meeting::templates::GENERAL;
                let found = self
                    .db
                    .set_session_template(session_id, (!cleared).then_some(template_id))?;
                if !found {
                    return Err("session not found".to_string());
                }
                Ok(None)
            }
            // Ask for a skill run (issue #251). The desktop runs it; above L1 it waits for the
            // user there. The skill must exist in the folder now, and its level is the file's.
            Tool::SkillsRun => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid skill run request".to_string())?;
                let name = value
                    .get("skill")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .ok_or_else(|| "missing skill".to_string())?;
                let input = value.get("input").and_then(|v| v.as_str()).unwrap_or_default();
                let dir = shogun_mcp::plan_source::skills_dir()
                    .ok_or_else(|| "no skills folder on this machine".to_string())?;
                let folder = shogun_agents::skills::load_dir(&dir);
                let skill = folder.get(name).ok_or_else(|| "skill not found".to_string())?;
                let run = self.db.skill_run_request(
                    &skill.name,
                    shogun_agents::skills::level_str(skill.level),
                    "api",
                    input,
                )?;
                Ok(Some(run.id))
            }
            // Issue #397: a file export request — L2, pending until the user confirms it.
            Tool::ExportWriteFile => self.export_write_file(body).map(Some),
            // Owner decision 2026-10-20: withhold meeting notes and screen recall from one
            // connected app. Only narrows (L1); allowing them is only in the app.
            Tool::ApiConnectionsRestrict => {
                let client = shogun_mcp::source_policy::restrict_request(body).map_err(str::to_string)?;
                let restrict = self
                    .connection_restrictor
                    .as_ref()
                    .ok_or_else(|| "connections_unavailable".to_string())?;
                restrict(&client)?;
                Ok(None)
            }
            // Removing an export folder only narrows the boundary (L1).
            Tool::ExportFoldersRemove => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid export folder request".to_string())?;
                let folder = value
                    .get("folder")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing folder".to_string())?;
                shogun_mcp::export_folders::remove(folder).map_err(|e| e.as_str().to_string())?;
                Ok(None)
            }
            // Save a new skill file (issue #251 "save as skill"): validated like the loader,
            // never overwriting an existing name.
            Tool::SkillsSave => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid skill save request".to_string())?;
                let markdown = value
                    .get("markdown")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing markdown".to_string())?;
                let dir = shogun_mcp::plan_source::skills_dir()
                    .ok_or_else(|| "no skills folder on this machine".to_string())?;
                shogun_agents::skills::save_skill(&dir, markdown).map_err(|e| e.as_str())?;
                Ok(None)
            }
            // Schedule a skill (issue #252). The skill must exist now; its level is re-read at
            // each slot, and each slot is an ordinary skill run.
            Tool::SchedulesAdd => {
                use shogun_memory::scheduled_jobs::{parse_days, parse_time};
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid schedule request".to_string())?;
                let field = |k: &str| value.get(k).and_then(|v| v.as_str()).map(str::trim);
                let name = field("skill").ok_or_else(|| "missing skill".to_string())?;
                let days = field("days")
                    .and_then(parse_days)
                    .ok_or_else(|| "days must be like mon,wed,fri | weekdays | daily".to_string())?;
                let minute = field("at")
                    .and_then(parse_time)
                    .ok_or_else(|| "at must be HH:MM".to_string())?;
                let dir = shogun_mcp::plan_source::skills_dir()
                    .ok_or_else(|| "no skills folder on this machine".to_string())?;
                if shogun_agents::skills::load_dir(&dir).get(name).is_none() {
                    return Err("skill not found".to_string());
                }
                let job = self.db.schedule_add(name, field("input").unwrap_or(""), days, minute, "api")?;
                Ok(Some(job.id))
            }
            Tool::SchedulesRemove => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid schedule request".to_string())?;
                let id = value
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| "missing id".to_string())?;
                if self.db.schedule_remove(id)? {
                    Ok(Some(id))
                } else {
                    Err("schedule not found".to_string())
                }
            }
            // `[Track]` a Recap next action (FR-MT-17) — the Recap's own confirmation. Returns
            // the commitment id; a text that is not one of the session's suggestions is refused.
            Tool::MeetingRecapTrack => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid track request".to_string())?;
                let session_id = match value.get("session_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => self
                        .db
                        .latest_meeting_session_id()
                        .ok_or_else(|| "no meeting session to track from".to_string())?,
                };
                let text = value
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing text".to_string())?;
                let direction = value.get("direction").and_then(|v| v.as_str());
                let counterparty_id = value.get("counterparty_id").and_then(|v| v.as_i64());
                let tracked =
                    self.db.track_recap_action(session_id, text, direction, counterparty_id)?;
                Ok(Some(tracked.commitment_id))
            }
            // A pattern's ON/OFF switch — the Patterns card's own control (FR-PAT-06, L1).
            Tool::PatternsSetActive => {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "invalid pattern request".to_string())?;
                let id = value
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| "missing id".to_string())?;
                let active = value
                    .get("active")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| "missing active".to_string())?;
                // A storage fault is a server error the caller may retry, not a missing pattern
                // (issue #779).
                match self.db.try_set_work_pattern_active(id, active) {
                    Ok(true) => Ok(Some(id)),
                    Ok(false) => Err("pattern not found".to_string()),
                    Err(_) => Err(MEMORY_WRITE_UNAVAILABLE.to_string()),
                }
            }
            // Correct / delete one state record (issue #290, L1) — the World model pane's own
            // edit and delete, through the one shared request parser.
            Tool::StateUpdate => match self.db.state_update_as(body, &shogun_memory::actor::current_write())? {
                true => Ok(None),
                false => Err("state record not found".to_string()),
            },
            Tool::StateDelete => match self.db.state_delete_as(body, &shogun_memory::actor::current_write())? {
                true => Ok(None),
                false => Err("state record not found".to_string()),
            },
            // Split a wrong merge (FR-ST-10, L1): the new record's id comes back.
            // Quick-add a commitment / open loop (L1): the new row's id comes back.
            Tool::StateCreate => match self.db.state_create_as(body, &shogun_memory::actor::current_write())? {
                Some(id) => Ok(Some(id)),
                None => Err("person not found".to_string()),
            },
            Tool::StateSplit => match self.db.state_split(body)? {
                Some(outcome) => Ok(Some(outcome.new_id)),
                None => Err("state record not found".to_string()),
            },
            // Merge duplicates the user confirmed; keep two apart (issue #1362, L1).
            Tool::StateMerge => match self.db.state_merge(body)? {
                Some(outcome) => Ok(Some(outcome.survivor_id)),
                None => Err("state record not found".to_string()),
            },
            Tool::StateKeepSeparate => match self.db.state_keep_separate(body)? {
                Some(()) => Ok(shogun_memory::state_merge::parse_keep_separate(body).ok().map(|r| r.id)),
                None => Err("state record not found".to_string()),
            },
            // Settle a contradiction flag (issue #253, L1) — the Context Health pane's decision.
            Tool::StateDisputesResolve => {
                let (id, resolution) = parse_dispute_resolve_body(body)?;
                if self.db.state_dispute_resolve(id, resolution)? {
                    Ok(Some(id))
                } else {
                    Err("dispute not found".to_string())
                }
            }
            // Persist the note to the event log (L1, reversible).
            // An empty or whitespace-only note is refused on every face (issue #779): it saved
            // nothing the user wrote, and an empty row breaks the Hot tier.
            Tool::MemoryNotesChange => {
                let change = serde_json::from_str(body).map_err(|_| "invalid_change".to_string())?;
                let result = self.db.memory_notes_change(change)?;
                if let Some(error) = result["error"].as_str() { return Err(error.to_string()); }
                Ok(result["note"]["id"].as_i64())
            },
            // A user note (the notes.change save path: same validation and dedup), so
            // memory.notes.list shows it and notes.change can forget it.
            Tool::MemoryAppendNote if body.trim().is_empty() => Err("missing text".to_string()),
            Tool::MemoryAppendNote
                if body.chars().count() > shogun_mcp::memory_api::MAX_APPEND_NOTE_CHARS =>
            {
                Err(shogun_mcp::memory_api::APPEND_NOTE_TOO_LONG.to_string())
            }
            Tool::MemoryAppendNote => {
                let result = self.db.memory_notes_change(
                    shogun_mcp::memory_notes_api::NoteChange::Save { text: body.to_string() },
                )?;
                if let Some(error) = result["error"].as_str() {
                    return Err(error.to_string());
                }
                Ok(result["note"]["id"].as_i64())
            }
            // Flip a lesson's active switch (L1) — the same effect as the Learned UI toggle.
            Tool::LessonsSetActive => {
                let (id, active) = parse_lesson_active_body(body)?;
                match self.db.try_set_lesson_active(id, active) {
                    Ok(true) => Ok(Some(id)),
                    Ok(false) => Err("lesson not found".to_string()),
                    Err(_) => Err(MEMORY_WRITE_UNAVAILABLE.to_string()),
                }
            }
            Tool::VisualRecallSetEnabled => {
                let enabled = parse_enabled_body(body)?;
                // Issue #782: an API caller may only turn Visual recall off (or confirm it is
                // on) here. Turning it on while it is Off starts keeping images of the screen.
                let mut purge = Ok(0);
                self.try_update_vr_settings(
                    |settings| {
                        if enabled && !settings.enabled {
                            return Err(VISUAL_RECALL_WIDEN_REFUSAL.to_string());
                        }
                        settings.enabled = enabled;
                        Ok(())
                    },
                    |settings| {
                        if !settings.enabled {
                            purge = self.db.purge_auto_screen_frames();
                        }
                    },
                )?;
                // Off stays committed (capture has stopped), but the caller was promised the
                // passive frames are gone; a failed purge is reported so it can retry.
                match purge {
                    Ok(removed) if removed > 0 => eprintln!(
                        "[visual_recall] disabled via API — purged {removed} auto frame(s)"
                    ),
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!(
                            "[visual_recall] disabled via API; auto-frame purge failed: {error}"
                        );
                        return Err(VISUAL_RECALL_OFF_PURGE_FAILED.to_string());
                    }
                }
                Ok(None)
            }
            Tool::VisualRecallSetRetention => {
                let retention = parse_retention_body(body)?;
                // Issue #782: only an equal or shorter retention is a direct write; keeping saved
                // screens longer needs the user's in-app approval.
                self.try_update_vr_settings(
                    |settings| {
                        if retention.days() > settings.retention.days() {
                            return Err(VISUAL_RECALL_WIDEN_REFUSAL.to_string());
                        }
                        settings.retention = retention;
                        Ok(())
                    },
                    |settings| {
                        let purge = settings
                            .retention
                            .retain_ms()
                            .and_then(|retain_ms| self.db.purge_screen_frames(retain_ms));
                        if let Err(error) = purge {
                            eprintln!(
                                "[visual_recall] retention committed via API; expired-frame purge failed: {error}"
                            );
                        }
                    },
                )?;
                Ok(None)
            }
            Tool::BackupSetRetention => {
                self.set_backup_retention(body)?;
                Ok(None)
            }
            // "Compact search index" (#714 step 3, L1): the disk is checked here so a caller hears
            // a refusal at once; the desktop takes the request and runs the job.
            Tool::SearchIndexCompactStart => {
                shogun_mcp::search_index_api::parse_empty_body(body)?;
                let config = self.search_index()?;
                crate::search_index_compact::request_start(&self.db, &config.paths, config.free.as_ref())?;
                Ok(None)
            }
            Tool::DreamRetry => {
                shogun_mcp::search_index_api::parse_empty_body(body)?;
                shogun_mcp::plan_source::request_dream_retry()?;
                Ok(None)
            }
            Tool::SearchIndexCompactStop => {
                shogun_mcp::search_index_api::parse_empty_body(body)?;
                crate::search_index_compact::request_stop(&self.db, &self.search_index()?.paths)?;
                Ok(None)
            }
            Tool::VisualRecallDeleteFrame => {
                if parse_hidden_self_delete_body(body)? {
                    self.db.delete_hidden_self_screen_frames()?;
                    return Ok(None);
                }
                let id = parse_id_body(body)?;
                if self.db.delete_screen_frame(id)? {
                    Ok(None)
                } else {
                    Err("frame not found".to_string())
                }
            }
            // Mail triage (issue #319, FR-TG-30). Local state at L1; the two archive intents are
            // L2 and only *record* the request — the desktop's write-back tick executes it
            // under the label's standing approval or a one-tap confirm (FR-TG-21).
            Tool::MailTriageSet => {
                self.apply_mail_triage_set(body)?;
                Ok(None)
            }
            // Issue #779: recorded directly only under the label's standing approval (or when
            // nothing changes on Gmail), decided on the same connection as the write. Anything
            // else needs the user's one tap, so a request that skipped the queue is refused.
            Tool::MailTriageArchive | Tool::MailTriageUnarchive => {
                let thread_key = mail_thread_key(body)?;
                self.db
                    .mail_api_request_archive(&thread_key, tool == Tool::MailTriageUnarchive)?;
                Ok(None)
            }
            Tool::MailLabelsUpsert => {
                self.apply_mail_label_upsert(body)?;
                Ok(None)
            }
            Tool::MailLabelsDelete => {
                let value = mail_body(body)?;
                let key = body_string(&value, "key")?
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| "expected key".to_string())?;
                self.db.mail_label_delete(&key).map_err(remap_mail_error)?;
                Ok(None)
            }
            Tool::MailSettingsSet => {
                let value = mail_body(body)?;
                let enabled = value
                    .get("enabled")
                    .and_then(serde_json::Value::as_bool)
                    .ok_or_else(|| "expected enabled (boolean)".to_string())?;
                self.db.set_mail_triage_enabled(enabled)?;
                Ok(None)
            }
            Tool::MailSenderRulesAdd => Ok(Some(self.apply_mail_sender_rule_add(body)?)),
            Tool::MailSenderRulesDelete => {
                let id = parse_id_body(body)?;
                if self.db.mail_sender_rule_delete(id)? {
                    Ok(Some(id))
                } else {
                    Err("sender rule not found".to_string())
                }
            }
            // Not a write tool.
            _ => Err("not a write tool".to_string()),
        }
    }
}

// Bulk cleanup is a distinct explicit intent; ambiguous bodies must never widen deletion.
fn parse_hidden_self_delete_body(body: &str) -> Result<bool, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Ok(false);
    };
    let Some(hidden) = value.get("hidden_self_frames") else {
        return Ok(false);
    };
    if hidden != &serde_json::Value::Bool(true)
        || !value.as_object().is_some_and(|object| object.len() == 1)
    {
        return Err("expected only {\"hidden_self_frames\":true}".to_string());
    }
    Ok(true)
}

fn parse_id_body(body: &str) -> Result<i64, String> {
    if let Ok(id) = body.trim().parse::<i64>() {
        return Ok(id);
    }
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "expected frame id".to_string())?;
    v.get("id")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| "expected {\"id\":number}".to_string())
}

/// Parse the `state.disputes.resolve` body:
/// `{"id": <i64>, "resolution": "keep_earlier"|"keep_later"|"dismiss_both"|"keep_both"}`.
fn parse_dispute_resolve_body(
    body: &str,
) -> Result<(i64, shogun_memory::disputes::Resolution), String> {
    let err = || {
        "expected {\"id\":number,\"resolution\":\"keep_earlier|keep_later|dismiss_both|keep_both\"}"
            .to_string()
    };
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| err())?;
    let id = v.get("id").and_then(|x| x.as_i64()).ok_or_else(err)?;
    let resolution = v
        .get("resolution")
        .and_then(|x| x.as_str())
        .and_then(shogun_memory::disputes::Resolution::parse)
        .ok_or_else(err)?;
    Ok((id, resolution))
}

/// Parse the `lessons.set_active` body: `{"id": <i64>, "active": <bool>}`.
fn parse_lesson_active_body(body: &str) -> Result<(i64, bool), String> {
    let err = || "expected {\"id\":number,\"active\":bool}".to_string();
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| err())?;
    let id = v.get("id").and_then(|x| x.as_i64()).ok_or_else(err)?;
    let active = v.get("active").and_then(|x| x.as_bool()).ok_or_else(err)?;
    Ok((id, active))
}

fn parse_enabled_body(body: &str) -> Result<bool, String> {
    if body.trim() == "true" {
        return Ok(true);
    }
    if body.trim() == "false" {
        return Ok(false);
    }
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "expected {\"enabled\":bool}".to_string())?;
    v.get("enabled")
        .and_then(|x| x.as_bool())
        .ok_or_else(|| "expected {\"enabled\":bool}".to_string())
}

/// Prune expired snapshots under a just-saved retention (#647). Nothing to do without a backup
/// directory; nothing is removed without a key to verify the newest snapshot.
fn prune_backups_now(
    config: &BackupRetentionConfig,
    retention: crate::backup_retention::BackupRetention,
    now_ms: i64,
) -> Result<(), &'static str> {
    if !config.backups_dir.is_dir() {
        return Ok(());
    }
    let key = config
        .key
        .as_ref()
        .and_then(|load| load())
        .ok_or("key_unavailable")?;
    let coordinator = shogun_memory::backup::BackupCoordinator::production(
        &config.backups_dir,
        shogun_memory::backup::BackupPolicy::default(),
    )
    .map_err(|_| "directory")?;
    coordinator.set_max_age_days(retention.days());
    let report = coordinator
        .prune_expired(&key, now_ms)
        .map_err(|_| "prune")?;
    if report.failed > 0 {
        return Err("some_files_not_removed");
    }
    Ok(())
}

/// The direct-write refusal for a `capture.set_paused` that would start capture sooner (issue
/// #897). "invalid " makes REST answer 400: the caller sent a request this path never applies.
const CAPTURE_RESUME_REFUSAL: &str =
    "invalid capture pause request: ending a pause needs approval in the ShogunAI app";
/// The same refusal for a pause request that would end the pause in force sooner: `capture
/// pause 15m` while capture is paused until resumed is shortening it, not pausing.
const CAPTURE_SHORTEN_REFUSAL: &str =
    "invalid capture pause request: capture is already paused for longer; shorten or end the pause in the ShogunAI app";

/// A `capture.set_paused` body, validated as the save would validate it.
fn capture_pause_request(
    body: &str,
    now_ms: i64,
) -> Result<crate::capture::control::Status, String> {
    serde_json::from_str::<crate::capture::control::Status>(body)
        .map_err(|_| "Invalid capture pause request.".to_string())?
        .validate(now_ms)
}

/// The pause in force for the issue #897 check. A pause file that cannot be read stops capture
/// (`capture_control::begin` refuses to run on it), so it counts as a pause that waits to be
/// resumed: an API caller cannot clear it by "resuming".
fn paused_now(
    status: Result<crate::capture::control::Status, String>,
) -> crate::capture::control::Status {
    status.unwrap_or(crate::capture::control::Status { paused: true, resume_at_ms: None })
}

/// The direct-write refusal for a Visual recall widening (issue #782); "invalid " makes REST
/// answer 400, as for [`CAPTURE_RESUME_REFUSAL`].
const VISUAL_RECALL_WIDEN_REFUSAL: &str =
    "invalid visual recall request: turning Visual recall on or keeping saved screens longer needs the user's approval in ShogunAI";

/// Visual recall was turned Off but its passive frames could not be deleted. The
/// `purge_pending:` prefix makes REST answer 503 with this reason (not a bare 500), so REST/CLI
/// callers can tell "Off committed, deletion still owed" from "Off failed" and retry.
const VISUAL_RECALL_OFF_PURGE_FAILED: &str =
    "purge_pending: visual recall is off, but saved screens could not be deleted; retry to delete them";

/// What a `visual_recall.set_enabled` / `visual_recall.set_retention` body would widen against
/// the settings in force (issue #782): `Some` when it turns Visual recall on while it is Off, or
/// keeps saved screens longer than now. `Err` for an invalid body or another tool.
fn visual_recall_widening(
    tool: Tool,
    body: &str,
    settings: &Settings,
) -> Result<Option<shogun_agents::permission::VisualRecallWidening>, String> {
    let previous_retention_days = settings.retention.days();
    let change = match tool {
        Tool::VisualRecallSetEnabled => shogun_agents::permission::VisualRecallWidening {
            enable: parse_enabled_body(body)? && !settings.enabled,
            retention_days: None,
            previous_retention_days,
        },
        Tool::VisualRecallSetRetention => {
            let days = parse_retention_body(body)?.days();
            shogun_agents::permission::VisualRecallWidening {
                enable: false,
                retention_days: (days > previous_retention_days).then_some(days),
                previous_retention_days,
            }
        }
        _ => return Err("not a visual recall settings write".to_string()),
    };
    Ok(change.widens().then_some(change))
}

fn parse_retention_body(body: &str) -> Result<RetentionPolicy, String> {
    if let Ok(days) = body.trim().parse::<u32>() {
        return RetentionPolicy::try_days(days);
    }
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| "expected retention days between 1 and 3650".to_string())?;
    let candidates = [
        value.get("days"),
        value.get("retention_days"),
        value
            .get("retention")
            .and_then(|retention| retention.get("days")),
    ];
    let mut parsed = Vec::new();
    for candidate in candidates.into_iter().flatten() {
        let days = candidate
            .as_u64()
            .and_then(|days| u32::try_from(days).ok())
            .ok_or_else(|| "expected retention days between 1 and 3650".to_string())?;
        parsed.push(days);
    }
    let Some(&days) = parsed.first() else {
        return Err("expected retention days between 1 and 3650".to_string());
    };
    if parsed.iter().any(|candidate| *candidate != days) {
        return Err("conflicting retention day values".to_string());
    }
    RetentionPolicy::try_days(days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_memory::state::{CommitmentDirection, CommitmentStatus, NewCommitment, Provenance};
    use std::sync::Arc;

    #[test]
    fn wiki_list_exposes_extracted_tools_with_evidence_keys() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(||100))?;
        let (event_id,_) = db.capture(&shogun_memory::event_log::NewEvent {
            content:"Acme uses Cursor for Project Comet",..ev("tool-api-source")
        }).ok_or("capture failed")?;
        db.try_persist_candidates(event_id,&[shogun_memory::extract::Candidate::Entity(shogun_memory::knowledge::Candidate {
            kind:shogun_memory::knowledge::Kind::Tool,name:"Cursor".into(),context:"Acme".into(),quote:"Acme uses Cursor".into(), ..Default::default() })])?;
        let backend = DbBackend::new(db);
        let list = structured(&backend,Tool::WikiList,&ReadParams::default());
        assert_eq!(list["results"][0]["kind"],"tool");
        assert_eq!(list["results"][0]["text"],"possibly: Cursor");
        let key = list["results"][0]["key"].as_str().ok_or("missing key")?;
        let evidence = structured(&backend,Tool::WikiEvidence,&ReadParams {query:Some(key.into()),..Default::default()});
        assert_eq!(evidence["evidence"][0]["event_id"],event_id);
        Ok(())
    }

    #[test]
    fn dashboard_review_api_uses_real_sources_and_explicit_low_opt_in() -> Result<(), Box<dyn std::error::Error>> {
        use shogun_memory::state::NewPerson;
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let (event_id, _) = db.capture(&ev("wiki-api-source")).ok_or("capture failed")?;
        let first = db.insert_person(&NewPerson { display_name: "Alice", confidence: 0.9, now: 1, ..Default::default() }, &[Provenance::new(event_id)]).ok_or("person failed")?;
        db.insert_person(&NewPerson { display_name: "Bob", confidence: 0.4, now: 1, ..Default::default() }, &[Provenance::new(event_id)]).ok_or("person failed")?;
        let older = db.open_meeting(Some("Earlier"), None, 0.9, "{}").ok_or("meeting failed")?;
        let newer = db.open_meeting(Some("Later"), None, 0.9, "{}").ok_or("meeting failed")?;
        let backend = DbBackend::new(db);
        let mut params = ReadParams { query: Some(format!("person-{first}")), ..Default::default() };
        let listed = shogun_mcp::rest::render_reads(Tool::StatePeopleList, &backend.read(Tool::StatePeopleList, &params), false);
        let listed: serde_json::Value = serde_json::from_str(&listed)?;
        assert_eq!(listed["results"][0]["key"], format!("person-{first}"));
        let evidence = structured(&backend, Tool::WikiEvidence, &params);
        assert_eq!(evidence["evidence"][0]["event_id"], event_id);
        assert_eq!(evidence["review_only"], true);
        assert_eq!(structured(&backend, Tool::WikiNeighbors, &params)["neighbors"], json!([]));
        // A person below 0.5 is hidden everywhere it would be listed, include_low or not.
        params.include_low = true;
        let neighbors = structured(&backend, Tool::WikiNeighbors, &params);
        assert_eq!(neighbors["neighbors"], json!([]));
        let meetings = structured(&backend, Tool::MeetingList, &ReadParams { id: Some(newer), ..Default::default() });
        assert_eq!(meetings["sessions"].as_array().map(Vec::len), Some(1));
        assert_eq!(meetings["sessions"][0]["id"], older);
        params.query = Some("person--1".into());
        assert_eq!(structured(&backend, Tool::WikiEvidence, &params)["error"], "invalid_memory_key");
        Ok(())
    }

    #[test]
    fn onboarding_api_reads_the_saved_file_and_reports_missing_or_invalid_schema() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let root = std::env::temp_dir().join(format!("shogun-onboarding-api-{}-{}", std::process::id(), shogun_mcp::desktop_heartbeat::now_ms()));
        std::fs::create_dir_all(&root)?;
        let path = root.join("onboarding.json");
        let backend = DbBackend::new(db).with_onboarding_state_path(path.clone());
        assert!(shogun_mcp::visual_recall_api::is_structured_read(Tool::DeviceOnboardingGet));
        assert_eq!(structured(&backend, Tool::DeviceOnboardingGet, &params())["status"], "missing");
        for version in [1, 2] {
            let saved = json!({"version": version, "state": {"completed": false, "step": "plan", "plan": "pro", "trial_started_at": 42, "token": "must-not-escape"}}).to_string();
            std::fs::write(&path, &saved)?;
            let result = structured(&backend, Tool::DeviceOnboardingGet, &params());
            assert_eq!(result["status"], "saved");
            assert_eq!(result["state"]["step"], "plan");
            assert_eq!(result["state"]["trial_started_at"], 42);
            assert!(result["state"].get("token").is_none());
            assert_eq!(std::fs::read_to_string(&path)?, saved, "API reads never migrate the desktop file");
        }
        std::fs::write(&path, "not json")?;
        assert_eq!(structured(&backend, Tool::DeviceOnboardingGet, &params())["status"], "corrupt");
        std::fs::write(&path, r#"{"version":3,"state":{"completed":true}}"#)?;
        let result = structured(&backend, Tool::DeviceOnboardingGet, &params());
        assert_eq!(result["status"], "unsupported_schema");
        assert!(result["state"].is_null());
        std::fs::write(&path, r#"{"skipped":true}"#)?;
        assert_eq!(structured(&backend, Tool::DeviceOnboardingGet, &params())["state"]["completed"], true);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn account_api_reads_the_desktop_snapshot_without_ids_and_adds_the_plan() -> Result<(), Box<dyn std::error::Error>> {
        use shogun_license::account::{serialize_account_snapshot, AccountSnapshot, SnapshotDevice};
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let root = std::env::temp_dir().join(format!("shogun-account-api-{}-{}", std::process::id(), shogun_mcp::desktop_heartbeat::now_ms()));
        std::fs::create_dir_all(&root)?;
        let path = root.join("account.json");
        let backend = DbBackend::new(db).with_account_state_path(path.clone());
        assert!(shogun_mcp::visual_recall_api::is_structured_read(Tool::DeviceAccountGet));
        let out = structured(&backend, Tool::DeviceAccountGet, &params());
        assert_eq!(out["signed_in"], false, "no file: signed out, never a made-up account");
        assert!(out["plan"]["status"].is_string());
        let snap = AccountSnapshot {
            signed_in: true,
            email: Some("me@example.test".into()),
            devices: vec![SnapshotDevice { name: Some("MacBook Pro".into()), this_device: true, ..SnapshotDevice::default() }],
            licence_key_linked: true,
            ..AccountSnapshot::default()
        };
        std::fs::write(&path, serialize_account_snapshot(&snap))?;
        let signed = structured(&backend, Tool::DeviceAccountGet, &params());
        assert_eq!(signed["signed_in"], true);
        assert_eq!(signed["email"], "me@example.test");
        assert_eq!(signed["devices"][0]["this_device"], true);
        assert!(signed["devices"][0].get("device_id").is_none());
        assert!(signed.get("licence_key_linked").is_none());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn meeting_recap_api_reads_fresh_job_state_and_marks_stale_state_unknown() -> Result<(), Box<dyn std::error::Error>> {
        use shogun_mcp::{connections_api::ConnectionsPaths, desktop_heartbeat, meeting_recap_status};
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let sid = db.open_meeting(Some("Call"), None, 0.9, "{}").ok_or("meeting failed")?;
        let root = std::env::temp_dir().join(format!("shogun-recap-api-{}", std::process::id()));
        let paths = ConnectionsPaths::for_db(root.join("memory.db").to_str().ok_or("path invalid")?);
        let status_path = paths.status.with_file_name(meeting_recap_status::FILE_NAME);
        let backend = DbBackend::new(db).with_connections(paths);
        let params = ReadParams { id: Some(sid), ..Default::default() };
        assert_eq!(structured(&backend, Tool::MeetingRecapGet, &params)["generation_status"], "unknown");
        let mut jobs = meeting_recap_status::Jobs::default();
        let generation = jobs.start(sid);
        jobs.publish(&status_path, desktop_heartbeat::now_ms())?;
        assert_eq!(structured(&backend, Tool::MeetingRecapGet, &params)["generation_status"], "pending");
        jobs.set(sid, generation, "needs_plan");
        jobs.publish(&status_path, desktop_heartbeat::now_ms())?;
        assert_eq!(structured(&backend, Tool::MeetingRecapGet, &params)["generation_status"], "needs_plan");
        jobs.publish(&status_path, desktop_heartbeat::now_ms() - meeting_recap_status::MAX_AGE_MS - 1)?;
        assert_eq!(structured(&backend, Tool::MeetingRecapGet, &params)["generation_status"], "unknown");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn meeting_recap_edit_rewrites_the_minutes_every_reader_sees_and_survives_regeneration() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let sid = db.open_meeting(Some("Renewal call"), Some("us.zoom.xos"), 0.9, "{}").ok_or("meeting failed")?;
        assert!(db.save_meeting_recap(sid, "model prose", r#"["ship in Q3"]"#, r#"[{"text":"call vendor","owner":"Me"}]"#, None, "m"));
        let backend = DbBackend::new(db);

        // No session_id → the latest meeting; only the given parts change; blank items drop.
        let body = r#"{"summary":"  We agreed to renew.  ","next_actions":[{"text":" email the vendor ","owner":" Tanaka "},{"text":"   "}]}"#;
        assert!(matches!(backend.write(Tool::MeetingRecapEdit, body), Ok(None)));
        let recap = structured(&backend, Tool::MeetingRecapGet, &ReadParams::default());
        assert_eq!(recap["minutes"]["summary"], "We agreed to renew.");
        assert_eq!(recap["minutes"]["decisions"][0], "ship in Q3", "untouched part stays");
        assert_eq!(recap["minutes"]["next_actions"].as_array().map(Vec::len), Some(1));
        assert_eq!(recap["minutes"]["next_actions"][0]["owner"], "Tanaka");
        assert_eq!(recap["minutes"]["edited_at"], 100);
        assert_eq!(recap["degraded"], false);
        // The export renders the edited prose too.
        let markdown = backend.db.meeting_recap_markdown(sid, false).ok_or("no export")?;
        assert!(markdown.contains("We agreed to renew."));

        // A regeneration after the edit leaves the person's version alone …
        assert!(backend.db.save_meeting_recap(sid, "newer model prose", "[]", "[]", None, "m2"));
        assert_eq!(structured(&backend, Tool::MeetingRecapGet, &ReadParams::default())["minutes"]["summary"], "We agreed to renew.");
        // … until it is reset, after which the model may write again.
        assert!(matches!(backend.write(Tool::MeetingRecapEdit, &format!(r#"{{"session_id":{sid},"reset":true}}"#)), Ok(None)));
        assert!(backend.db.save_meeting_recap(sid, "newer model prose", "[]", "[]", None, "m2"));
        let recap = structured(&backend, Tool::MeetingRecapGet, &ReadParams::default());
        assert_eq!(recap["minutes"]["summary"], "newer model prose");
        assert!(recap["minutes"]["edited_at"].is_null());

        // Refusals carry a reason: nothing to change, an unknown session, malformed JSON.
        assert!(backend.write(Tool::MeetingRecapEdit, "{}").is_err());
        assert!(backend.write(Tool::MeetingRecapEdit, r#"{"session_id":9999,"summary":"x"}"#).is_err());
        assert!(backend.write(Tool::MeetingRecapEdit, "not json").is_err());
        Ok(())
    }

    #[test]
    fn meeting_search_finds_a_meeting_and_delete_removes_it_from_every_read() -> Result<(), Box<dyn std::error::Error>> {
        use shogun_memory::transcript_segments::Speaker;
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let sid = db.open_meeting(Some("Renewal call"), Some("us.zoom.xos"), 0.9, "{}").ok_or("meeting failed")?;
        assert!(db.append_transcript(sid, 1_000, Speaker::Me, "shall we renew the vendor contract", 0.9));
        assert!(db.append_transcript(sid, 2_000, Speaker::OtherSlot(0), "yes, renew for a year", 0.9));
        assert!(db.save_meeting_note(sid, "watch the discount"));
        db.assign_meeting_speaker(sid, "other:0", "Tanaka", "user").map_err(|e| e.to_string())?;
        assert!(db.save_meeting_recap(sid, "Agreed to renew the vendor contract.", r#"["Renew for a year"]"#, r#"[{"text":"email the vendor"}]"#, None, "m"));
        let backend = DbBackend::new(db);

        let hits = structured(&backend, Tool::MeetingSearch, &ReadParams { query: Some("renew vendor".into()), ..Default::default() });
        assert_eq!(hits["hits"][0]["session_id"], sid);
        assert_eq!(hits["hits"][0]["title"], "Renewal call");
        assert!(structured(&backend, Tool::MeetingSearch, &ReadParams { query: Some("   ".into()), ..Default::default() })["error"].is_string());

        // D5: the decision anchors to the transcript line where it was said.
        let recap = structured(&backend, Tool::MeetingRecapGet, &ReadParams { id: Some(sid), ..Default::default() });
        assert_eq!(recap["minutes"]["decision_anchors"][0], 2_000);
        assert_eq!(recap["minutes"]["next_action_anchors"].as_array().map(Vec::len), Some(1));

        assert!(matches!(backend.write(Tool::MeetingDelete, &format!(r#"{{"session_id":{sid}}}"#)), Ok(None)));
        assert!(backend.write(Tool::MeetingDelete, &format!(r#"{{"session_id":{sid}}}"#)).is_err(), "already gone");
        assert!(backend.write(Tool::MeetingDelete, "{}").is_err());
        assert_eq!(structured(&backend, Tool::MeetingRecapGet, &ReadParams { id: Some(sid), ..Default::default() })["error"], "not_found");
        assert_eq!(structured(&backend, Tool::MeetingTranscriptGet, &ReadParams { id: Some(sid), ..Default::default() })["error"], "not_found");
        let hits = structured(&backend, Tool::MeetingSearch, &ReadParams { query: Some("renew vendor".into()), ..Default::default() });
        assert_eq!(hits["hits"].as_array().map(Vec::len), Some(0));
        assert!(structured(&backend, Tool::MeetingSpeakersGet, &ReadParams { id: Some(sid), ..Default::default() })["speakers"].as_array().map_or(true, Vec::is_empty));
        Ok(())
    }

    #[test]
    fn voice_dictionary_import_export_round_trips_through_the_typed_contract() -> Result<(), Box<dyn std::error::Error>> {
        let backend = DbBackend::new(Db::open_in_memory(Arc::new(|| 100))?);

        let sheet ="canonical,aliases,scope,scope_ref\nSelect KK,セレクト;select k k,global,\nKamon,,surface,meeting\n,orphan,global,\n";
        let VoiceDictionaryResult::Imported(report) = backend
            .manage_voice_dictionary(VoiceDictionaryOperation::Import { text: sheet.to_string() })?
        else {
            return Err("expected an import report".into());
        };
        assert_eq!((report.created, report.updated), (2, 0));
        assert_eq!(report.skipped.len(), 1, "the row without a canonical is reported, not fatal");
        assert_eq!(report.skipped[0].0, 4);

        let VoiceDictionaryResult::Terms(terms) =
            backend.manage_voice_dictionary(VoiceDictionaryOperation::List)?
        else {
            return Err("expected terms".into());
        };
        assert_eq!(terms.len(), 2);

        let VoiceDictionaryResult::Csv(csv) =
            backend.manage_voice_dictionary(VoiceDictionaryOperation::Export)?
        else {
            return Err("expected csv".into());
        };
        assert!(csv.starts_with("canonical,aliases,locale,scope,scope_ref,priority,enabled\n"));
        assert!(csv.contains("Select KK,"), "{csv}");
        assert!(csv.contains("Kamon,,,surface,meeting,"), "{csv}");

        // Re-importing the export updates in place instead of duplicating.
        let VoiceDictionaryResult::Imported(again) =
            backend.manage_voice_dictionary(VoiceDictionaryOperation::Import { text: csv })?
        else {
            return Err("expected an import report".into());
        };
        assert_eq!((again.created, again.updated, again.skipped.len()), (0, 2, 0));

        // Text with no rows at all is refused rather than reported as an empty success.
        assert!(backend
            .manage_voice_dictionary(VoiceDictionaryOperation::Import { text: "\n\n".to_string() })
            .is_err());
        Ok(())
    }

    #[test]
    fn meeting_recap_and_transcript_reads_match_the_panel_view() -> Result<(), Box<dyn std::error::Error>> {
        use shogun_memory::transcript_segments::Speaker;
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let sid = db.open_meeting(Some("Renewal call"), Some("us.zoom.xos"), 0.9, "{}").ok_or("meeting failed")?;
        assert!(db.append_transcript(sid, 1_000, Speaker::Me, "shall we renew?", 0.9));
        assert!(db.append_transcript(sid, 2_000, Speaker::OtherSlot(0), "yes", 0.9));
        assert!(db.append_transcript(sid, 3_000, Speaker::Unknown, "…", 0.5));
        assert!(db.save_meeting_note(sid, "watch the discount"));
        db.assign_meeting_speaker(sid, "other:0", "Tanaka", "user").map_err(|e| e.to_string())?;
        let backend = DbBackend::new(db);

        // Before the Batch lane writes minutes: the degraded Recap, honestly labelled.
        let recap = structured(&backend, Tool::MeetingRecapGet, &ReadParams { id: Some(sid), ..Default::default() });
        assert_eq!(recap["session_id"], sid);
        assert_eq!(recap["title"], "Renewal call");
        assert_eq!(recap["degraded"], true);
        assert!(recap["minutes"].is_null());
        assert_eq!(recap["note"], "watch the discount");
        assert_eq!(recap["template"]["id"], "general");
        assert_eq!(recap["speakers"][0]["display_name"], "Tanaka");

        // After: the same structured fields the Recap panel renders.
        assert!(backend.db.save_meeting_recap(
            sid,
            "Agreed to renew.",
            r#"["Renew for 12 months"]"#,
            r#"[{"text":"Send the contract","owner":"Tanaka"}]"#,
            None,
            "claude-batch",
        ));
        let recap = structured(&backend, Tool::MeetingRecapGet, &ReadParams { id: Some(sid), ..Default::default() });
        assert_eq!(recap["degraded"], false);
        assert_eq!(recap["minutes"]["summary"], "Agreed to renew.");
        assert_eq!(recap["minutes"]["decisions"][0], "Renew for 12 months");
        assert_eq!(recap["minutes"]["next_actions"][0]["owner"], "Tanaka");
        assert_eq!(recap["minutes"]["sections"], json!([]));
        // Talk share by characters, labelled like the export: Me said 13 characters, Tanaka 3.
        assert_eq!(recap["talk_share"][0]["speaker"], "Me");
        assert_eq!(recap["talk_share"][0]["chars"], 13);
        assert_eq!(recap["talk_share"][1]["speaker"], "Tanaka");

        // The transcript, labelled the way the export labels it; omitted id = latest meeting.
        let transcript = structured(&backend, Tool::MeetingTranscriptGet, &ReadParams::default());
        assert_eq!(transcript["session_id"], sid);
        assert_eq!(transcript["count"], 3);
        assert_eq!(transcript["lines"][0]["speaker"], "Me");
        assert_eq!(transcript["lines"][0]["ts"], 1_000);
        assert_eq!(transcript["lines"][1]["speaker"], "Tanaka");
        assert_eq!(transcript["lines"][1]["speaker_key"], "other:0");
        assert_eq!(transcript["lines"][2]["speaker"], "Speaker");
        assert!(transcript["lines"][2]["speaker_key"].is_null());

        // An unknown session is not found on either read.
        assert_eq!(structured(&backend, Tool::MeetingRecapGet, &ReadParams { id: Some(9_999), ..Default::default() })["error"], "not_found");
        assert_eq!(structured(&backend, Tool::MeetingTranscriptGet, &ReadParams { id: Some(9_999), ..Default::default() })["error"], "not_found");
        Ok(())
    }

    fn ev<'a>(hash: &'a str) -> shogun_memory::event_log::NewEvent<'a> {
        shogun_memory::event_log::NewEvent {
            ts: 1,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "evidence",
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        }
    }

    fn seed() -> Db {
        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let (e, _) = db.capture(&ev("h1")).unwrap();
        db.insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the report",
                due_at: Some(50),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        db
    }

    fn backend_with_settings(db: Db) -> DbBackend {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shogun_vr_test_{}_{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        // A user who saved Visual recall Off: the shipped default is on (2026-10-20).
        let path = dir.join("visual_recall.json");
        crate::capture::visual_recall::update_settings(&path, |settings| settings.enabled = false)
            .expect("seed off");
        DbBackend::new(db)
            .with_visual_recall_settings_path(path)
            .with_capture_exclusions_path(dir.join("exclusions.json"))
    }

    /// Turn Visual recall on the way the desktop Settings switch does: the user is the actor
    /// there. An API write cannot (issue #782).
    fn enable_visual_recall_as_user(backend: &DbBackend) {
        let path = backend.visual_recall_settings_path().expect("settings path");
        crate::capture::visual_recall::update_settings(path, |settings| settings.enabled = true)
            .expect("enable");
    }

    fn params() -> ReadParams {
        ReadParams::default()
    }
    fn get(id: i64) -> ReadParams {
        ReadParams {
            id: Some(id),
            ..ReadParams::default()
        }
    }

    #[test]
    fn backend_reads_commitments_from_the_db() {
        let backend = DbBackend::new(seed());
        let items = backend.read(Tool::StateCommitmentsList, &params());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "send the report");
        assert_eq!(items[0].confidence, 0.9);
        // Issue #779: the commitment's row, which `state edit|delete commitments <id>` takes.
        let (table, id) = items[0].row.expect("a state row");
        assert_eq!(table, "commitments");
        assert_eq!(backend.read(Tool::StateCommitmentsGet, &get(id))[0].row, Some(("commitments", id)));
    }

    #[test]
    fn snoozed_commitment_leaves_the_list_and_get_carries_deferred_until() {
        let db = seed();
        let backend = DbBackend::new(db.clone());
        let id = backend.read(Tool::StateCommitmentsList, &params())[0].row.unwrap().1;
        db.state_update(&format!(r#"{{"table":"commitments","id":{id},"deferred_until":9999999}}"#)).unwrap();
        assert!(backend.read(Tool::StateCommitmentsList, &params()).is_empty());
        assert_eq!(backend.read(Tool::StateCommitmentsGet, &get(id))[0].deferred_until, Some(9_999_999));
    }

    #[test]
    fn backend_reads_people_list_and_get() {
        use shogun_memory::state::{NewPerson, Provenance};
        let db = Db::open_in_memory(Arc::new(|| 1)).unwrap();
        let (e, _) = db.capture(&ev("h1")).unwrap();
        let id = db
            .insert_person(
                &NewPerson {
                    display_name: "Alice",
                    confidence: 0.85,
                    now: 1,
                    ..Default::default()
                },
                &[Provenance::new(e)],
            )
            .unwrap();
        let backend = DbBackend::new(db);

        let list = backend.read(Tool::StatePeopleList, &params());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label, "Alice");
        // Issue #779: the row the edit / delete / split commands take.
        assert_eq!(list[0].row, Some(("people", id)));

        let got = backend.read(Tool::StatePeopleGet, &get(id));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].label, "Alice");
        // Issue #1362: one row says where its evidence comes from; the list does not.
        assert!(list[0].sources.is_none());
        let sources = got[0].sources.as_deref().expect("sources on get");
        assert_eq!(sources.len(), 1);
        assert_eq!((sources[0].source.as_str(), sources[0].count), ("capture", 1));
        let rendered = shogun_mcp::rest::render_reads(Tool::StatePeopleGet, &got, false);
        assert!(rendered.contains(r#""sources":[{"source":"capture","count":1}]"#), "{rendered}");
        assert!(!shogun_mcp::rest::render_reads(Tool::StatePeopleList, &list, false).contains("sources"));

        // a missing id → empty
        assert!(backend.read(Tool::StatePeopleGet, &get(9999)).is_empty());
    }

    #[test]
    fn backend_search_returns_event_content_as_ground_truth() {
        let db = Db::open_in_memory(Arc::new(|| 1)).unwrap();
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: 1,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "the quarterly roadmap review",
            content_hash: "h1",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .unwrap();
        let backend = DbBackend::new(db);
        let hits = backend.read(
            Tool::MemorySearch,
            &ReadParams {
                id: None,
                query: Some("roadmap".into()),
                ..ReadParams::default()
            },
        );
        assert_eq!(hits.len(), 1);
        assert!(hits[0].label.contains("roadmap"));
        assert_eq!(hits[0].confidence, EVENT_CONFIDENCE); // events always pass the gate
                                                          // empty query → no search
        assert!(backend.read(Tool::MemorySearch, &params()).is_empty());
    }

    #[test]
    fn search_hits_carry_when_where_snippet_and_open() {
        let db = Db::open_in_memory(Arc::new(|| 1_000)).unwrap();
        let content = format!("{} quarterly roadmap review {}", "lead ".repeat(200), "tail ".repeat(200));
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: 500,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.example.Notes"),
            window_title: Some("Planning"),
            content: &content,
            content_hash: "h1",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .unwrap();
        let backend = DbBackend::new(db);
        let response = MemoryBackend::search(
            &backend,
            &SearchRequestV1 {
                query: "roadmap".into(),
                limit: 5,
                ..SearchRequestV1::default()
            },
        )
        .unwrap();
        let hit = serde_json::to_value(&response.results[0]).unwrap();
        assert_eq!(hit["when"], crate::when::when_json(500));
        assert_eq!(
            hit["where"],
            serde_json::json!({"app": "com.example.Notes", "window_title": "Planning"})
        );
        let id = hit["source_id"].as_str().unwrap().parse::<i64>().unwrap();
        assert_eq!(hit["open"], serde_json::json!({"tool": "memory.get_events", "ids": [id]}));
        let snippet = hit["snippet"].as_str().unwrap();
        assert!(snippet.contains("roadmap") && snippet.starts_with('…') && snippet.ends_with('…'));
        assert!(snippet.chars().count() <= shogun_memory::search::SNIPPET_CHARS);
        assert!(hit["text"].as_str().unwrap().chars().count() <= SEARCH_TEXT_CHARS);
    }

    #[test]
    fn context_pack_returns_facts_and_cited_evidence() {
        // seed: one commitment (fact supply) + one captured event matching the query (evidence).
        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let (e, _) = db
            .capture(&shogun_memory::event_log::NewEvent {
                ts: 1,
                source: "gmail",
                kind: "message",
                app_bundle_id: None,
                window_title: Some("Vendor renewal"),
                content: "the vendor renewal was settled at 12k for the year",
                content_hash: "h1",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap();
        db.insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "confirm the vendor contract",
                due_at: Some(50),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let backend = DbBackend::new(db);

        let items = backend.read(
            Tool::MemoryGetContextPack,
            &ReadParams {
                id: None,
                query: Some("vendor renewal".into()),
                ..ReadParams::default()
            },
        );
        // a fact line and an evidence line, the latter citing its event id + source (provenance).
        assert!(
            items.iter().any(|i| i.label.starts_with("fact: ")),
            "items: {items:?}"
        );
        let evidence = items
            .iter()
            .find(|i| i.label.starts_with("evidence "))
            .unwrap_or_else(|| panic!("no evidence line: {items:?}"));
        assert!(evidence.label.contains(&format!("event:{e}")));
        assert!(evidence.label.contains("source:gmail"));
        assert!(evidence.label.contains("12k"));

        // no query → empty (the pack is per-question; there is no "pack of everything").
        assert!(backend
            .read(Tool::MemoryGetContextPack, &params())
            .is_empty());
    }

    #[test]
    fn get_context_includes_state_and_user_notes() {
        let db = seed();
        let backend = DbBackend::new(db.clone());
        assert!(backend
            .write(Tool::MemoryAppendNote, "remember the launch checklist")
            .is_ok());

        let items = backend.read(Tool::MemoryGetContext, &params());

        assert!(items
            .iter()
            .any(|item| item.label == "fact: you committed: send the report"));
        assert!(items
            .iter()
            .any(|item| item.label == "note: remember the launch checklist"));
        assert!(items
            .iter()
            .any(|item| item.label.contains("current_context=unavailable")));
    }

    #[test]
    fn standalone_context_reports_live_cache_unavailable() {
        let items = DbBackend::new(seed()).read(Tool::MemoryGetContext, &params());

        assert!(items.iter().any(|item| {
            item.label
                == "metadata: current_context=unavailable source=standalone reason=desktop_cache_not_connected"
        }));
    }

    #[test]
    fn fresh_desktop_context_preserves_wording_and_line_budget() {
        let db = seed();
        let cache = crate::daemon::ReplyContextCache::new();
        cache.put(crate::daemon::ReplyContext {
            facts: (0..20)
                .map(|index| format!("possibly: live fact {index}"))
                .collect(),
            ..crate::daemon::ReplyContext::default()
        });
        let backend = DbBackend::new(db).with_current_context_provider(Arc::new(
            ReplyContextProvider::new(cache, std::time::Duration::from_secs(10)),
        ));

        let current: Vec<_> = backend
            .read(Tool::MemoryGetContext, &params())
            .into_iter()
            .filter(|item| item.label.starts_with("current: "))
            .collect();

        assert_eq!(current.len(), CURRENT_CONTEXT_LINE_LIMIT);
        assert!(current
            .iter()
            .all(|item| item.label.starts_with("current: possibly: live fact ")));
    }

    #[test]
    fn stale_desktop_context_withholds_cached_text() {
        let cache = crate::daemon::ReplyContextCache::new();
        cache.put_at(
            crate::daemon::ReplyContext {
                facts: vec!["stale focus secret".into()],
                ..crate::daemon::ReplyContext::default()
            },
            std::time::Instant::now() - std::time::Duration::from_secs(20),
        );
        let backend = DbBackend::new(seed()).with_current_context_provider(Arc::new(
            ReplyContextProvider::new(cache, std::time::Duration::from_secs(10)),
        ));

        let items = backend.read(Tool::MemoryGetContext, &params());

        assert!(items.iter().any(|item| {
            item.label == "metadata: current_context=stale source=desktop_cache live_text_withheld=true"
        }));
        assert!(!items
            .iter()
            .any(|item| item.label.contains("stale focus secret")));
    }

    #[test]
    fn cleared_desktop_context_withholds_excluded_text() {
        let cache = crate::daemon::ReplyContextCache::new();
        cache.put(crate::daemon::ReplyContext {
            facts: vec!["excluded focus secret".into()],
            ..crate::daemon::ReplyContext::default()
        });
        cache.clear();
        let backend = DbBackend::new(seed()).with_current_context_provider(Arc::new(
            ReplyContextProvider::new(cache, std::time::Duration::from_secs(10)),
        ));

        let items = backend.read(Tool::MemoryGetContext, &params());

        assert!(items.iter().any(|item| {
            item.label == "metadata: current_context=empty source=desktop_cache live_text_withheld=true"
        }));
        assert!(!items
            .iter()
            .any(|item| item.label.contains("excluded focus secret")));
    }

    #[test]
    fn reading_desktop_context_does_not_persist_cached_text() {
        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let cache = crate::daemon::ReplyContextCache::new();
        cache.put(crate::daemon::ReplyContext {
            facts: vec!["ram only current context sentinel".into()],
            ..crate::daemon::ReplyContext::default()
        });
        let backend = DbBackend::new(db.clone()).with_current_context_provider(Arc::new(
            ReplyContextProvider::new(cache, std::time::Duration::from_secs(10)),
        ));

        let _ = backend.read(Tool::MemoryGetContext, &params());

        assert!(db
            .search("ram only current context sentinel", 10)
            .is_empty());
    }

    #[test]
    fn get_context_recent_activity_carries_event_provenance() {
        let db = Db::open_in_memory(Arc::new(|| 2)).unwrap();
        let (event_id, _) = db
            .capture(&shogun_memory::event_log::NewEvent {
                ts: 2,
                source: "gmail",
                kind: "email",
                app_bundle_id: Some("com.example.mail"),
                window_title: Some("Roadmap"),
                content: "quarterly roadmap review has three open decisions",
                content_hash: "roadmap-context",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap();
        let backend = DbBackend::new(db);

        let items = backend.read(Tool::MemoryGetContext, &params());

        assert!(items.iter().any(|item| {
            item.label
                .contains(&format!("event:{event_id} source:gmail ts:2"))
                && item.label.contains("title:Roadmap app:com.example.mail")
                && item.label.contains("three open decisions")
        }));
    }

    #[test]
    fn get_context_recent_activity_is_bounded_deduplicated_and_allowlisted() {
        let db = Db::open_in_memory(Arc::new(|| 20)).unwrap();
        for index in 0..10 {
            let content = format!("unique recent activity {index}");
            let hash = format!("recent-{index}");
            db.capture(&shogun_memory::event_log::NewEvent {
                ts: index,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: &content,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap();
        }
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: 19,
            source: "screen_ocr",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "unique recent activity 9",
            content_hash: "duplicate-across-source",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .unwrap();
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: 20,
            source: "private_internal",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "must stay outside context previews",
            content_hash: "not-allowlisted",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .unwrap();
        let backend = DbBackend::new(db);

        let activity: Vec<_> = backend
            .read(Tool::MemoryGetContext, &params())
            .into_iter()
            .filter(|item| item.label.starts_with("recent activity "))
            .collect();

        assert_eq!(activity.len(), CONTEXT_ACTIVITY_LIMIT);
        assert_eq!(
            activity
                .iter()
                .filter(|item| item.label.contains("unique recent activity 9"))
                .count(),
            1
        );
        assert!(!activity
            .iter()
            .any(|item| item.label.contains("private_internal")));
    }

    #[test]
    fn write_and_action_tools_are_not_reads() {
        let backend = DbBackend::new(seed());
        assert!(backend.read(Tool::MemoryAppendNote, &params()).is_empty());
        assert!(backend.read(Tool::ActionsExecute, &params()).is_empty());
    }

    #[test]
    fn style_notes_list_delete_and_rate_through_the_writing_voice_tools() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 555))?;
        let backend = DbBackend::new(db.clone());
        let id = db
            .rewrite_record_outcome(&shogun_memory::rewrite_loop::NewOutcome { app: "chat", partner: None, instruction: "", input: "yo send it", output: "Hello, please send it.", note_ids: &[] })
            .ok_or("outcome")?;
        db.rewrite_apply_sent(id, "yo send it pls");
        db.rewrite_apply_mining(&[id], &[crate::rewrite_mining::MinedNote { scope: "surface".into(), app: "chat".into(), partner: String::new(), text: "Say yo.".into() }], &[]);
        let get = || -> Result<serde_json::Value, Box<dyn std::error::Error>> {
            Ok(serde_json::from_str(&backend.read_structured(Tool::WritingVoiceGet, &params()).unwrap())?)
        };
        let v = get()?;
        assert_eq!(v["style_notes"][0]["text"], "Say yo.");
        assert_eq!(v["rewrite_outcomes"]["edited"], 1);
        let note = v["style_notes"][0]["id"].as_i64().ok_or("id")?;
        backend.write(Tool::WritingVoiceSet, &format!(r#"{{"action":"note_confirm","id":{note}}}"#))?;
        assert_eq!(get()?["style_notes"][0]["confirmations"], 1);
        backend.write(Tool::WritingVoiceSet, &format!(r#"{{"action":"note_delete","id":{note}}}"#))?;
        assert_eq!(get()?["style_notes"], serde_json::json!([]));
        assert!(backend.write(Tool::WritingVoiceSet, r#"{"action":"note_delete","id":999}"#).is_err());
        Ok(())
    }

    #[test]
    fn writing_voice_api_requires_explicit_consent_and_never_returns_samples() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 555))?;
        let dir = std::env::temp_dir().join(format!("shogun-writing-style-api-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let style_path = dir.join("Shougun.md");
        std::fs::write(
            &style_path,
            "# Style\n- Tone: Concise\n- Length: One paragraph\n- Format:\n  - Bullets when useful\n",
        )?;
        let backend = DbBackend::new(db.clone()).with_user_config_path(style_path.clone());
        assert!(backend.write(Tool::WritingVoiceSet, r#"{"action":"enable"}"#).is_err());
        assert!(db.voice_learning_enabled(), "V79: on by default for undecided users");
        db.set_voice_learning_enabled(false)?; // explicit off; the API cannot turn it back on
        // Issue #654: the caller's own `consent: true` is not enough — the backend never enables.
        assert!(backend.write(Tool::WritingVoiceSet, r#"{"action":"enable","consent":true}"#).is_err());
        let initial: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::WritingVoiceGet, &params()).unwrap(),
        )?;
        assert_eq!(initial["enabled"], false);
        assert_eq!(initial["scopes"], serde_json::json!([]));
        assert_eq!(initial["config"]["tone"], "Concise");
        assert_eq!(initial["config"]["format_hints"], serde_json::json!(["Bullets when useful"]));
        // Only the user's in-app confirmation of the queued approval turns it on.
        let (action, _) = shogun_agents::approval::writing_style_request();
        assert_eq!(db.apply_confirmed_setting(&action, None, None, None), Some(Ok(())));
        std::fs::write(&style_path, "# Style\n- Tone: Updated\n- Length: One line\n")?;
        let enabled: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::WritingVoiceGet, &params()).unwrap(),
        )?;
        assert_eq!(enabled["enabled"], true);
        assert_eq!(enabled["config"]["tone"], "Updated");
        let revision = db.voice_learning_revision();
        for n in 0..3 {
            db.observe_writing_draft("slack", &format!("keep this message private {n}"), "Generated text.", revision);
        }
        let learned: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::WritingVoiceGet, &params()).unwrap(),
        )?;
        assert_eq!(learned["scopes"], serde_json::json!([{ "app": "slack", "examples": 3 }]));
        assert!(!db.writing_voice_prompt_data("slack").is_empty());
        backend.write(Tool::WritingVoiceSet, r#"{"action":"clear"}"#)?;
        assert!(db.writing_voice_prompt_data("slack").is_empty());
        backend.write(Tool::WritingVoiceSet, r#"{"action":"disable"}"#)?;
        assert!(!db.voice_learning_enabled());
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn whoami_lists_weak_items_apart_from_confirmed_ones() {
        use shogun_memory::state::NewPerson;
        let db = seed();
        let (e, _) = db.capture(&ev("whoami-weak")).unwrap();
        db.insert_person(&NewPerson { display_name: "Alice", confidence: 0.9, now: 1, ..Default::default() }, &[Provenance::new(e)]).unwrap();
        db.insert_person(&NewPerson { display_name: "Bob", confidence: 0.6, now: 1, ..Default::default() }, &[Provenance::new(e)]).unwrap();
        let dir = std::env::temp_dir().join(format!("shogun-whoami-weak-{}", std::process::id()));
        let backend = DbBackend::new(db).with_memory_api_settings_path(dir.join("memory_api.json"));
        let w: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::ProfileWhoami, &params()).unwrap()).unwrap();
        // A listed person reads as a fact (entity_pages): both are confirmed, none inferred.
        assert_eq!(w["work"]["people"]["names"], serde_json::json!(["Alice", "Bob"]));
        assert_eq!(w["work"]["inferred"]["people"], serde_json::json!([]));
        assert_eq!(w["work"]["commitments"]["names"], serde_json::json!(["send the report"]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn whoami_drops_unreviewed_candidates_and_caps_lists() {
        use shogun_memory::state::NewPerson;
        let db = seed();
        let (e, _) = db.capture(&ev("whoami-cap")).unwrap();
        let names: Vec<String> = (0..30).map(|i| format!("Medium {i}")).collect();
        for name in &names {
            db.insert_person(&NewPerson { display_name: name, confidence: 0.6, now: 1, ..Default::default() }, &[Provenance::new(e)]).unwrap();
        }
        db.insert_person(&NewPerson { display_name: "Candidate", confidence: 0.3, now: 1, ..Default::default() }, &[Provenance::new(e)]).unwrap();
        let backend = DbBackend::new(db);
        let w: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::ProfileWhoami, &params()).unwrap()).unwrap();
        // Listed people read as facts, so the 30 land in the confirmed list, capped.
        assert_eq!(w["work"]["people"]["count"], 30);
        assert_eq!(w["work"]["people"]["names"].as_array().unwrap().len(), WHOAMI_LIST_CAP);
        assert!(!w.to_string().contains("Candidate"), "a <0.5 candidate leaked: {w}");
    }

    #[test]
    fn append_note_is_a_user_note_that_can_be_forgotten() {
        let backend = DbBackend::new(seed());
        let id = backend.write(Tool::MemoryAppendNote, "remember to call Alice").unwrap().unwrap();
        let notes: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::MemoryNotesList, &params()).unwrap()).unwrap();
        assert_eq!(notes["notes"][0]["id"], id);
        assert_eq!(notes["notes"][0]["text"], "remember to call Alice");
        // Same text again is the same note, not a duplicate.
        assert_eq!(backend.write(Tool::MemoryAppendNote, "remember to call Alice").unwrap(), Some(id));
        backend
            .write(Tool::MemoryNotesChange, &format!(r#"{{"action":"forget","id":{id},"revision":1}}"#))
            .unwrap();
        let too_long = "x".repeat(shogun_mcp::memory_api::MAX_APPEND_NOTE_CHARS + 1);
        assert_eq!(
            backend.write(Tool::MemoryAppendNote, &too_long).unwrap_err(),
            shogun_mcp::memory_api::APPEND_NOTE_TOO_LONG
        );
    }

    fn capture_at(db: &Db, ts: i64, app: &str, title: &str, text: &str) -> i64 {
        let hash = shogun_memory::event_log::content_hash(text);
        db.capture(&shogun_memory::event_log::NewEvent {
            ts,
            app_bundle_id: Some(app),
            window_title: Some(title),
            content: text,
            content_hash: &hash,
            ..ev("")
        })
        .unwrap()
        .0
    }

    #[test]
    fn get_events_opens_ids_bounds_text_and_lists_missing() {
        use shogun_mcp::activity_api::ActivityRequest;
        let db = Db::open_in_memory(Arc::new(|| 10_000)).unwrap();
        let id = capture_at(&db, 5_000, "com.google.Chrome", "Inbox", &"a".repeat(300));
        let backend = DbBackend::new(db);
        let v: serde_json::Value = serde_json::from_str(
            &backend.activity(&ActivityRequest::Events { ids: vec![id, 999], max_chars: 200 }).unwrap(),
        )
        .unwrap();
        let event = &v["events"][0];
        assert_eq!(event["id"], id);
        assert_eq!(event["when"]["ms"], 5_000);
        assert_eq!(event["where"]["app"], "com.google.Chrome");
        assert_eq!(event["source_kind"], "capture");
        assert_eq!(event["text"].as_str().unwrap().chars().count(), 200);
        assert_eq!(event["truncated"], true);
        assert_eq!(v["missing"], serde_json::json!([999]));
    }

    /// Owner decision 2026-10-20 (audit F4): a call made for a connection the app made on its own
    /// (`source_policy::with_access(Withheld, …)`, set by the MCP face) gets no meeting
    /// transcript and no kept screen text from search, events, sessions or context.
    #[test]
    fn withheld_calls_leave_out_meetings_and_screen_text() {
        use shogun_mcp::activity_api::ActivityRequest;
        use shogun_mcp::source_policy::{with_access, SensitiveSources};
        let db = Db::open_in_memory(Arc::new(|| 20_000)).unwrap();
        let add = |ts: i64, source: &'static str, app: Option<&'static str>, text: &str| {
            let hash = shogun_memory::event_log::content_hash(text);
            db.capture(&shogun_memory::event_log::NewEvent {
                ts,
                source,
                app_bundle_id: app,
                window_title: app.map(|_| "Plan"),
                content: text,
                content_hash: &hash,
                ..ev("")
            })
            .unwrap()
            .0
        };
        let typed = add(1_000, "capture", Some("com.example.Notes"), "zebrafish plan typed in the editor");
        let screen = add(2_000, "screen_text", Some("com.example.Viewer"), "zebrafish plan seen on the screen");
        let meeting = add(3_000, "meeting", None, "zebrafish plan said in the call");
        let backend = DbBackend::new(db);

        let found = |access: SensitiveSources| -> Vec<String> {
            with_access(access, || {
                MemoryBackend::search(
                    &backend,
                    &SearchRequestV1 { query: "zebrafish".into(), limit: 10, ..SearchRequestV1::default() },
                )
                .unwrap()
                .results
                .into_iter()
                .map(|hit| hit.text)
                .collect()
            })
        };
        let all = found(SensitiveSources::Allowed).join(" | ");
        assert!(all.contains("typed in the editor") && all.contains("seen on the screen"), "{all}");
        let withheld = found(SensitiveSources::Withheld).join(" | ");
        assert!(withheld.contains("typed in the editor"), "{withheld}");
        assert!(!withheld.contains("seen on the screen") && !withheld.contains("said in the call"), "{withheld}");
        // The legacy read channel goes through the same search.
        let labels = with_access(SensitiveSources::Withheld, || {
            backend.read(Tool::MemorySearch, &ReadParams { query: Some("zebrafish".into()), ..params() })
        });
        assert!(labels.iter().all(|item| !item.label.contains("screen") && !item.label.contains("call")), "{labels:?}");

        // Events by id: the withheld ones by id only, never their text.
        let events: serde_json::Value = serde_json::from_str(
            &with_access(SensitiveSources::Withheld, || {
                backend.activity(&ActivityRequest::Events { ids: vec![typed, screen, meeting], max_chars: 200 })
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(events["events"].as_array().unwrap().len(), 1);
        assert_eq!(events["events"][0]["id"], typed);
        assert_eq!(events["withheld"], serde_json::json!([screen, meeting]));
        assert_eq!(events["withheld_reason"], "source_not_allowed");
        assert!(!events.to_string().contains("seen on the screen"));
        let open: serde_json::Value = serde_json::from_str(
            &backend.activity(&ActivityRequest::Events { ids: vec![screen], max_chars: 200 }).unwrap(),
        )
        .unwrap();
        assert_eq!(open["events"][0]["source_kind"], "screen_text", "unrestricted outside such a call");

        // Sessions and context fold AX capture only.
        let sessions = with_access(SensitiveSources::Withheld, || {
            backend.activity(&ActivityRequest::Session { from_ms: 0, to_ms: 20_000, app: None, limit: 10 })
        })
        .unwrap();
        assert!(sessions.contains("typed in the editor") && !sessions.contains("seen on the screen"), "{sessions}");
        let context = with_access(SensitiveSources::Withheld, || {
            backend.read_structured(Tool::MemoryGetContext, &params())
        })
        .unwrap();
        assert!(!context.contains("seen on the screen"), "{context}");
        let context = backend.read_structured(Tool::MemoryGetContext, &params()).unwrap();
        assert!(context.contains("seen on the screen"), "{context}");
        // The context pack cites no meeting or screen-text evidence.
        let pack = with_access(SensitiveSources::Withheld, || {
            backend.read(Tool::MemoryGetContextPack, &ReadParams { query: Some("zebrafish".into()), ..params() })
        });
        assert!(
            pack.iter().all(|item| !item.label.contains("source:screen_text") && !item.label.contains("source:meeting")),
            "{pack:?}"
        );
    }

    #[test]
    fn read_session_and_get_context_sessionize_the_capture_log() {
        use shogun_mcp::activity_api::ActivityRequest;
        let db = Db::open_in_memory(Arc::new(|| 20_000)).unwrap();
        capture_at(&db, 1_000, "chrome", "Netflix", "ep 1");
        capture_at(&db, 3_000, "chrome", "Netflix", "ep 1 later");
        capture_at(&db, 5_000, "code", "main.rs", "fn main");
        let last = capture_at(&db, 10_000, "chrome", "Netflix", "ep 2");
        let backend = DbBackend::new(db);
        let v: serde_json::Value = serde_json::from_str(
            &backend
                .activity(&ActivityRequest::Session { from_ms: 0, to_ms: 20_000, app: Some("CHROME".into()), limit: 1 })
                .unwrap(),
        )
        .unwrap();
        // The app filter leaves the two Netflix stretches adjacent, so they fold into one;
        // the limit then has nothing more to cut.
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(v["sessions"][0]["visits"], 3);
        assert_eq!(v["sessions"][0]["text"], "ep 2");
        assert_eq!(v["truncated"], false);

        let c: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::MemoryGetContext, &params()).unwrap()).unwrap();
        assert_eq!(c["freshness"]["source"], "capture_log");
        assert_eq!(c["freshness"]["age_ms"], 10_000);
        assert_eq!(c["freshness"]["live"], true);
        assert_eq!(c["current"]["open"]["ids"], serde_json::json!([last]));
        assert_eq!(c["recent_sessions"].as_array().unwrap().len(), 2);
        assert_eq!(c["recent_sessions"][0]["where"]["window_title"], "main.rs");
        assert!(!c.to_string().contains("desktop_cache_not_connected"));
    }

    #[test]
    fn profile_set_over_the_api_keeps_multi_line_preferences() {
        let dir = std::env::temp_dir().join(format!("shogun-profile-api-{}", std::process::id()));
        let backend = DbBackend::new(seed()).with_memory_api_settings_path(dir.join("memory_api.json"));
        backend
            .write(Tool::ProfileSet, r#"{"display_name":"Toru","prefs":"創業者です。\r\n短く答えて。\r"}"#)
            .unwrap();
        let whoami: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::ProfileWhoami, &params()).unwrap()).unwrap();
        assert_eq!(whoami["profile"]["prefs"], "創業者です。\n短く答えて。");
        assert_eq!(whoami["profile"]["display_name"], "Toru");
        assert_eq!(whoami["generated"]["state"], "locked");
        // The name the user typed heads the identity profile, over any learned one.
        assert_eq!(whoami["generated"]["name"], "Toru");
        assert_eq!(whoami["identity"], "Name: Toru");
        // Single-line fields still refuse a line break, and the stored profile is unchanged.
        let refused = backend.write(Tool::ProfileSet, r#"{"role":"Found\ner"}"#).unwrap_err();
        assert_eq!(refused, "profile field contains control characters");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capture_pause_api_persists_and_rejects_invalid_deadlines() {
        let dir = std::env::temp_dir().join(format!("shogun-pause-api-{}", std::process::id()));
        let backend = DbBackend::new(seed())
            .with_memory_api_settings_path(dir.join("memory_api.json"));
        let status = || {
            serde_json::from_str::<serde_json::Value>(
                &backend.read_structured(Tool::CaptureStatus, &params()).unwrap(),
            ).unwrap()
        };
        backend.write(Tool::CaptureSetPaused, r#"{"paused":true}"#).unwrap();
        assert_eq!(status()["paused"], true);
        assert!(
            !backend.read(Tool::MemoryGetContext, &params()).is_empty(),
            "pausing capture must not hide stored memory"
        );
        assert!(backend.write(Tool::CaptureSetPaused, r#"{"paused":true,"resume_at_ms":1}"#).is_err());
        assert_eq!(status()["paused"], true, "invalid update preserves existing pause");

        // Issue #897: ending the user's pause over the API needs their approval. The faces queue
        // it; a resume that reaches the backend's write anyway is refused and the pause stays.
        // The request carries the pause it ends: one that waits to be resumed.
        let until_resumed = shogun_agents::permission::PausedCapture { resume_at_ms: None };
        assert_eq!(backend.capture_unpause_requested(r#"{"paused":false}"#), Ok(Some(until_resumed)));
        assert_eq!(backend.capture_unpause_requested(r#"{"paused":true}"#), Ok(None));
        let refused = backend.write(Tool::CaptureSetPaused, r#"{"paused":false}"#).unwrap_err();
        assert!(refused.starts_with("invalid capture pause request"), "{refused}");
        assert_eq!(status()["paused"], true);

        // The user resumes in the app, which is what confirming the approval does.
        let path = dir.join("capture_control.json");
        crate::capture::control::acquire(&path)
            .unwrap()
            .save(&path, Default::default(), crate::capture::control::now_ms())
            .unwrap();
        assert_eq!(status()["paused"], false);
        assert_eq!(
            backend.capture_unpause_requested(r#"{"paused":false}"#),
            Ok(None),
            "nothing to resume"
        );
        backend.write(Tool::CaptureSetPaused, r#"{"paused":false}"#).unwrap();

        // A timed pause may be kept or lengthened over the API, never ended early.
        let now = crate::capture::control::now_ms();
        let until = |ms: i64| format!(r#"{{"paused":true,"resume_at_ms":{ms}}}"#);
        backend.write(Tool::CaptureSetPaused, &until(now + 3_600_000)).unwrap();
        backend.write(Tool::CaptureSetPaused, &until(now + 7_200_000)).unwrap();
        assert!(backend.write(Tool::CaptureSetPaused, &until(now + 60_000)).is_err());
        assert_eq!(status()["resume_at_ms"], now + 7_200_000);
        // A request to end it carries that resume time, so it ends only this pause.
        assert_eq!(
            backend.capture_unpause_requested(r#"{"paused":false}"#),
            Ok(Some(shogun_agents::permission::PausedCapture { resume_at_ms: Some(now + 7_200_000) }))
        );

        std::fs::write(&path, "broken").unwrap();
        assert!(status()["error"].is_string(), "corruption is not reported as active capture");
        // An unreadable pause file counts as paused (see `paused_now`); pausing over it is still
        // allowed.
        assert_eq!(backend.capture_unpause_requested(r#"{"paused":false}"#), Ok(Some(until_resumed)));
        assert!(backend.write(Tool::CaptureSetPaused, r#"{"paused":false}"#).is_err());
        backend.write(Tool::CaptureSetPaused, r#"{"paused":true}"#).unwrap();
        assert_eq!(status()["paused"], true);
        std::fs::remove_file(dir.join("capture_control.json")).unwrap();
        let _ = std::fs::remove_file(dir.join("capture_control.lock"));
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn capture_status_counts_recent_capture_events_without_content() {
        use std::sync::atomic::{AtomicI64, Ordering};
        const NOW: i64 = 90_000_000;
        const TEN_MIN: i64 = 10 * 60 * 1000;
        let clock = Arc::new(AtomicI64::new(NOW));
        let tick = clock.clone();
        let db = Db::open_in_memory(Arc::new(move || tick.load(Ordering::SeqCst))).unwrap();
        let dir = std::env::temp_dir().join(format!("shogun-capture-count-{}", std::process::id()));
        let backend = DbBackend::new(db.clone())
            .with_memory_api_settings_path(dir.join("memory_api.json"));
        let status = || {
            serde_json::from_str::<serde_json::Value>(
                &backend.read_structured(Tool::CaptureStatus, &params()).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(status()["capture_events_10m"], 0, "no events yet");

        for (ts, source, hash) in [
            (NOW - 1_000, "capture", "a"),
            (NOW - 2_000, "capture", "b"),
            (NOW - TEN_MIN - 1, "capture", "too-old"),
            (NOW - 1_000, "screen_ocr", "ocr"),
        ] {
            db.capture(&shogun_memory::event_log::NewEvent { ts, source, content_hash: hash, ..ev(hash) })
                .unwrap();
        }
        let active = status();
        assert_eq!(active["paused"], false);
        assert_eq!(active["capture_events_10m"], 2);
        // Numbers and the pause state only: nothing a capture event carries is in the reply.
        let mut keys: Vec<_> = active.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["capture_events_10m", "paused", "resume_at_ms"]);
        assert!(!active.to_string().contains("evidence"));

        // Paused: nothing is ingested, so once the window has passed the count is zero.
        backend.write(Tool::CaptureSetPaused, r#"{"paused":true}"#).unwrap();
        assert_eq!(status()["capture_events_10m"], 2, "pausing does not delete what was captured");
        clock.store(NOW + TEN_MIN, Ordering::SeqCst);
        let paused = status();
        assert_eq!(paused["paused"], true);
        assert_eq!(paused["capture_events_10m"], 0);

        let _ = std::fs::remove_file(dir.join("capture_control.json"));
        let _ = std::fs::remove_file(dir.join("capture_control.lock"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dream_status_api_reads_the_same_morning_reason_as_settings() {
        use crate::dreamcycle::outcome::{self, DreamReason, OutcomeRecord, RecordKind};
        // Invariant 6 (issue #285): `dream.status` is the Settings row as a read, including why
        // last night has no fresh result. 09:07:30 UTC is never within a minute of a local 02:00
        // in any zone, so the record below falls inside the due night's window wherever this runs.
        let now = 1_790_294_400_000 + 9 * 3_600_000 + 450_000;
        let dir = std::env::temp_dir().join(format!("shogun-dream-api-{}", std::process::id()));
        let path = dir.join(outcome::FILE_NAME);
        outcome::save_at(
            &path,
            &OutcomeRecord::new("20260925", RecordKind::Failed, Some(DreamReason::RelayUnreachable), now - 60_000),
        )
        .unwrap();
        let db = Db::open_in_memory(Arc::new(move || now))
            .unwrap()
            .with_dream_outcome_path(Some(path.clone()));
        let backend = DbBackend::new(db);
        let v: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::DreamStatus, &params()).unwrap(),
        )
        .unwrap();
        assert_eq!(v["indicator"], "normal");
        assert!(v["last"].is_null());
        assert_eq!(v["done_tonight"], false);
        assert_eq!(v["night"]["status"], "failed");
        assert_eq!(v["night"]["reason"], "relay_unreachable");
        assert_eq!(v["night"]["stale"], false);
        assert!(v["night"]["night"].as_str().is_some_and(|n| n.len() == 8));
        assert!(v["night"]["result_night"].is_null());
        // #286: no Batch submission has been prepared yet, so no send telemetry.
        assert!(v["last_send"].is_null());
        // #285: the night's actions, known before the press. With no process facts the degraded
        // run is available and the retry is listed as not offered in this build.
        assert_eq!(
            v["actions"],
            json!([
                {"id": "run_now", "completes": "now", "enabled": true, "disabled_reason": null, "api": false},
                {"id": "retry_full", "completes": "later", "enabled": false, "disabled_reason": "not_offered", "api": false},
            ])
        );
        // It is a structured read, not a confidence-gated row list.
        assert!(backend.read(Tool::DreamStatus, &params()).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dream_status_actions_use_the_process_facts_and_carry_no_captured_text() {
        use crate::dreamcycle::actions::ExternalFacts;
        use crate::dreamcycle::outcome::{self, DreamReason, OutcomeRecord, RecordKind};
        // Same instant as above (the record falls inside the due night's window in every zone).
        // Only codes that do not depend on the local zone are asserted.
        let now = 1_790_294_400_000 + 9 * 3_600_000 + 450_000;
        let dir = std::env::temp_dir().join(format!("shogun-dream-actions-{}", std::process::id()));
        let path = dir.join(outcome::FILE_NAME);
        outcome::save_at(
            &path,
            &OutcomeRecord::new("20260925", RecordKind::Failed, Some(DreamReason::PlanInactive), now - 60_000),
        )
        .unwrap();
        let db = Db::open_in_memory(Arc::new(move || now))
            .unwrap()
            .with_dream_outcome_path(Some(path.clone()));
        const SENTINEL: &str = "SENTINEL-captured-text-7f3a";
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: now - 3_600_000,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.example.SENTINEL-app"),
            window_title: Some(SENTINEL),
            content: SENTINEL,
            content_hash: "dream-actions-h1",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .unwrap();
        let read = |backend: &DbBackend| -> (String, serde_json::Value) {
            let raw = backend.read_structured(Tool::DreamStatus, &params()).unwrap();
            let v = serde_json::from_str(&raw).unwrap();
            (raw, v)
        };

        // Nothing known beyond the ledger: last night's plan failure keeps both off.
        let (raw, v) = read(&DbBackend::new(db.clone()));
        assert!(!raw.contains("SENTINEL"), "dream.status must carry no captured text");
        assert_eq!(v["night"]["reason"], "plan_inactive");
        assert_eq!(v["actions"][0]["id"], "run_now");
        assert_eq!(v["actions"][0]["enabled"], false);
        assert_eq!(v["actions"][0]["disabled_reason"], "plan_inactive");
        assert_eq!(v["actions"][1]["disabled_reason"], "not_offered");

        // The process now sees an active plan (a renewal since last night): the run is back.
        let backend = DbBackend::new(db.clone()).with_dream_action_facts(|| ExternalFacts {
            plan_active: Some(true),
            ..ExternalFacts::unknown()
        });
        let (raw, v) = read(&backend);
        assert!(!raw.contains("SENTINEL"));
        assert_eq!(v["actions"][0]["enabled"], true);
        assert!(v["actions"][0]["disabled_reason"].is_null());

        // An inactive plan disables the run whatever the night's reason.
        let backend = DbBackend::new(db).with_dream_action_facts(|| ExternalFacts {
            plan_active: Some(false),
            ..ExternalFacts::unknown()
        });
        let (_, v) = read(&backend);
        assert_eq!(v["actions"][0]["disabled_reason"], "plan_inactive");
        // Every action entry is exactly the five documented keys (`api` is false on this face, #779).
        for action in v["actions"].as_array().unwrap() {
            let mut keys: Vec<&str> = action.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["api", "completes", "disabled_reason", "enabled", "id"]);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_structured_read_is_routed_as_one() {
        // A tool the backend answers in `read_structured` is unreachable over MCP / REST unless
        // the faces also route it there (`is_structured_read`); they would call `read` and render
        // an empty list instead. Skills, schedules and support reports shipped that way.
        let _env = SKILLS_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let db = Db::open_in_memory(Arc::new(|| 555)).unwrap();
        let backend = DbBackend::new(db);
        let params = ReadParams::default();
        for &tool in shogun_mcp::memory_api::ALL_TOOLS {
            if !matches!(shogun_mcp::memory_api::tool_level(tool), shogun_mcp::memory_api::ApiLevel::Read) {
                continue;
            }
            if backend.read_structured(tool, &params).is_some() {
                assert!(
                    shogun_mcp::visual_recall_api::is_structured_read(tool),
                    "{} answers in read_structured but the faces don't route it there",
                    tool.wire_name()
                );
            }
        }
    }

    #[test]
    fn skills_list_and_run_through_the_api_face() {
        let _env = SKILLS_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        // Issue #251: the folder is read as-is; a run is a queued row whose level is the file's
        // effective level (a sending body is L3, so an API run waits for the user).
        let dir = std::env::temp_dir().join(format!("shogun-skill-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("weekly.md"),
            "---\nname: weekly\ndescription: Weekly report\npermission_level: L1\n---\nList what I finished.",
        )
        .unwrap();
        std::fs::write(
            dir.join("chase.md"),
            "---\nname: chase\ndescription: Chase replies\npermission_level: L1\n---\nSend a reminder to everyone who owes me a reply.",
        )
        .unwrap();
        std::fs::write(dir.join("broken.md"), "nope").unwrap();
        std::env::set_var("SHOGUN_SKILLS_DIR", &dir);

        let db = Db::open_in_memory(Arc::new(|| 555)).unwrap();
        let backend = DbBackend::new(db.clone());
        let params = ReadParams { query: Some("weekly".into()), ..ReadParams::default() };
        let v: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::SkillsList, &params).unwrap()).unwrap();
        assert_eq!(v["skills"].as_array().unwrap().len(), 2);
        let chase = v["skills"].as_array().unwrap().iter().find(|s| s["name"] == "chase").unwrap();
        assert_eq!((chase["level"].as_str(), chase["declared_level"].as_str()), (Some("L3"), Some("L1")));
        assert!(chase.get("instructions").is_none());
        assert_eq!(chase["inputs"], serde_json::json!([]));
        let weekly = v["skills"].as_array().unwrap().iter().find(|s| s["name"] == "weekly").unwrap();
        assert_eq!(weekly["instructions"], "List what I finished.");
        assert_eq!(v["errors"][0]["file"], "broken.md");

        // A new skill saved over the API lands in the same folder (#251 "save as skill").
        let saved = backend.write(
            Tool::SkillsSave,
            r#"{"markdown":"---\nname: standup\ndescription: Daily standup\npermission_level: L1\n---\nList yesterday, today, blockers."}"#,
        );
        assert!(matches!(saved, Ok(None)), "{saved:?}");
        assert!(dir.join("standup.md").exists());
        assert!(backend
            .write(Tool::SkillsSave, r#"{"markdown":"---\nname: weekly\ndescription: d\npermission_level: L1\n---\nx"}"#)
            .is_err(), "an existing name is refused");
        let id = match backend.write(Tool::SkillsRun, r#"{"skill":"chase","input":"this week"}"#) {
            Ok(Some(id)) => id,
            other => panic!("run request failed: {other:?}"),
        };
        let status: serde_json::Value = serde_json::from_str(
            &backend
                .read_structured(Tool::SkillsRunStatus, &ReadParams { id: Some(id), ..ReadParams::default() })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status["run"]["status"], "awaiting_confirm");
        assert_eq!(status["run"]["origin"], "api");
        assert!(backend.write(Tool::SkillsRun, r#"{"skill":"missing"}"#).is_err());
        std::env::remove_var("SHOGUN_SKILLS_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `SHOGUN_SKILLS_DIR` is process-global; the tests that point it at a temp folder take turns.
    static SKILLS_DIR_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Issue #397 end to end on the API face: an export request waits for the user (L2), is
    /// refused outside the export folders before anything is written, writes exactly the Recap
    /// export text plus the provenance footer once confirmed, never overwrites, and records a
    /// `local_write` row with a digest and no content.
    #[test]
    fn file_export_waits_for_the_user_stays_in_the_folders_and_never_overwrites() {
        use shogun_agents::permission::ExportFormat;
        let dir = std::env::temp_dir().join(format!("shogun-export-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("Notes");
        let outside = dir.join("Elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::env::set_var("SHOGUN_EXPORT_FOLDERS_JSON", dir.join("export_folders.json"));
        let root = shogun_mcp::export_folders::add(root.to_str().unwrap()).unwrap();

        let db = Db::open_in_memory(Arc::new(|| 1_000)).unwrap();
        let sid = db.open_meeting(Some("Renewal call"), None, 0.9, "{}").unwrap();
        db.close_meeting(sid);
        assert!(db.save_meeting_recap(
            sid,
            "Agreed to renew.",
            r#"["Renew for 12 months"]"#,
            r#"[{"text":"Send the contract","owner":"Tanaka"}]"#,
            None,
            "test",
        ));
        let backend = DbBackend::new(db.clone());

        // Outside the export folders: refused, no row, no file.
        let body = json!({ "folder": outside.display().to_string(), "session_id": sid }).to_string();
        assert!(backend.write(Tool::ExportWriteFile, &body).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(db.local_writes_recent(10).is_empty());
        // The folder is authorized before the meeting resolves: an outside folder reports the
        // folder error even when the session does not exist or there is no meeting at all.
        let ghost = json!({ "folder": outside.display().to_string(), "session_id": sid + 999 }).to_string();
        assert_eq!(
            backend.write(Tool::ExportWriteFile, &ghost).unwrap_err(),
            "that folder is not an export folder"
        );
        let empty = DbBackend::new(Db::open_in_memory(Arc::new(|| 1_000)).unwrap());
        let no_session = json!({ "folder": outside.display().to_string() }).to_string();
        assert_eq!(
            empty.write(Tool::ExportWriteFile, &no_session).unwrap_err(),
            "that folder is not an export folder"
        );
        // A name that climbs out is refused too.
        let body = json!({ "folder": root.display().to_string(), "name": "../x" }).to_string();
        assert!(backend.write(Tool::ExportWriteFile, &body).is_err());

        // Inside: recorded as pending; nothing on disk until the user confirms.
        let body = json!({ "folder": root.display().to_string(), "session_id": sid }).to_string();
        let id = match backend.write(Tool::ExportWriteFile, &body) {
            Ok(Some(id)) => id,
            other => panic!("export request failed: {other:?}"),
        };
        let target = root.join("Renewal call.md");
        assert!(!target.exists(), "an API request never writes by itself");
        let pending: serde_json::Value = serde_json::from_str(
            &backend
                .read_structured(Tool::ExportWritesList, &ReadParams { id: Some(id), ..ReadParams::default() })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(pending["write"]["status"], "awaiting_confirm");
        assert_eq!(pending["write"]["kind"], "local_write");

        // The user confirms in the desktop → written; the file is the export text + footer.
        let roots = shogun_mcp::export_folders::list();
        let done = db.local_write_decide(id, true, &roots).unwrap().unwrap();
        assert_eq!(done.status, "written");
        let export: serde_json::Value = serde_json::from_str(
            &backend
                .read_structured(Tool::MeetingRecapExport, &ReadParams { id: Some(sid), ..ReadParams::default() })
                .unwrap(),
        )
        .unwrap();
        let expected = format!(
            "{}{}",
            export["markdown"].as_str().unwrap(),
            crate::meeting::export::recap_provenance_footer(sid)
        );
        let written = std::fs::read_to_string(&target).unwrap();
        assert_eq!(written, expected);
        assert_eq!(done.bytes, Some(expected.len() as i64));
        assert_eq!(done.content_xxh64.as_deref(), Some(crate::llm::traceability::digest(&expected).as_str()));
        assert_eq!(done.path.as_deref(), Some(target.to_str().unwrap()));

        // Saving again from the desktop never overwrites: the next free name.
        let again = db
            .local_write_request(
                &crate::daemon::ExportAsk {
                    session_id: sid,
                    include_transcript: false,
                    format: ExportFormat::Markdown,
                    folder: root.display().to_string(),
                    name: None,
                    origin: "ui",
                },
                &roots,
            )
            .unwrap();
        assert_eq!(again.status, "written");
        assert_eq!(again.path.as_deref(), Some(root.join("Renewal call (2).md").to_str().unwrap()));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), expected, "the first file is untouched");

        // The desktop face orders the checks the same as the API: an outside folder is refused
        // with the folder error even for a meeting that does not exist.
        let outside_ui = db.local_write_request(
            &crate::daemon::ExportAsk {
                session_id: sid + 999,
                include_transcript: false,
                format: ExportFormat::Markdown,
                folder: outside.display().to_string(),
                name: None,
                origin: "ui",
            },
            &roots,
        );
        assert_eq!(outside_ui.unwrap_err(), "that folder is not an export folder");

        // CSV: decisions and next actions with the session as provenance.
        let csv = db
            .local_write_request(
                &crate::daemon::ExportAsk {
                    session_id: sid,
                    include_transcript: false,
                    format: ExportFormat::Csv,
                    folder: root.display().to_string(),
                    name: Some("actions".into()),
                    origin: "ui",
                },
                &roots,
            )
            .unwrap();
        let csv_text = std::fs::read_to_string(csv.path.unwrap()).unwrap();
        assert!(csv_text.contains(&format!("Renewal call,Next action,Send the contract,Tanaka,{sid}")));

        // The ledger carries no content, only the digest.
        let list = backend.read_structured(Tool::ExportWritesList, &ReadParams::default()).unwrap();
        assert!(!list.contains("Agreed to renew"));
        assert!(!list.contains("Send the contract"));

        // A cancelled request writes nothing.
        let cancelled = backend.write(Tool::ExportWriteFile, &body).unwrap().unwrap();
        assert_eq!(db.local_write_decide(cancelled, false, &roots).unwrap().unwrap().status, "cancelled");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);

        // Removing the folder over the API turns the feature off for new requests.
        let remove = json!({ "folder": root.display().to_string() }).to_string();
        assert!(matches!(backend.write(Tool::ExportFoldersRemove, &remove), Ok(None)));
        let folders = backend.read_structured(Tool::ExportFoldersList, &ReadParams::default()).unwrap();
        assert_eq!(folders, r#"{"folders":[]}"#);
        assert!(backend.write(Tool::ExportWriteFile, &body).is_err());

        std::env::remove_var("SHOGUN_EXPORT_FOLDERS_JSON");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schedules_add_list_and_remove_through_the_api_face() {
        let _env = SKILLS_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("shogun-sched-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("weekly.md"),
            "---\nname: weekly\ndescription: Weekly report\npermission_level: L1\n---\nList what I finished.",
        )
        .unwrap();
        std::env::set_var("SHOGUN_SKILLS_DIR", &dir);
        let db = Db::open_in_memory(Arc::new(|| 555)).unwrap();
        let backend = DbBackend::new(db.clone());
        let id = match backend.write(Tool::SchedulesAdd, r#"{"skill":"weekly","days":"fri","at":"16:00"}"#) {
            Ok(Some(id)) => id,
            other => panic!("schedule add failed: {other:?}"),
        };
        assert!(backend.write(Tool::SchedulesAdd, r#"{"skill":"weekly","days":"funday","at":"16:00"}"#).is_err());
        assert!(backend.write(Tool::SchedulesAdd, r#"{"skill":"weekly","days":"fri","at":"25:00"}"#).is_err());
        assert!(backend.write(Tool::SchedulesAdd, r#"{"skill":"nope","days":"fri","at":"16:00"}"#).is_err());
        std::env::remove_var("SHOGUN_SKILLS_DIR");
        let v: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::SchedulesList, &ReadParams::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(v["schedules"][0]["days"], "fri");
        assert_eq!(v["schedules"][0]["at"], "16:00");
        assert_eq!(v["schedules"][0]["origin"], "api");
        assert_eq!(v["daily_run_cap"], 10);
        assert!(matches!(backend.write(Tool::SchedulesRemove, &format!(r#"{{"id":{id}}}"#)), Ok(Some(_))));
        assert!(backend.write(Tool::SchedulesRemove, &format!(r#"{{"id":{id}}}"#)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recap_track_confirms_only_the_recaps_own_suggestions() {
        // FR-MT-17 on the API face: the same [Track] the Recap offers, returning the commitment.
        let db = Db::open_in_memory(Arc::new(|| 555)).unwrap();
        let backend = DbBackend::new(db.clone());
        let sid = db.open_meeting(Some("Weekly sync"), Some("us.zoom.xos"), 0.7, "{}").unwrap();
        assert!(db.close_meeting(sid));
        assert!(db.save_meeting_recap(
            sid,
            "summary",
            "[]",
            r#"[{"text":"Send the deck","owner":"Me"}]"#,
            None,
            "m",
        ));
        let id = match backend.write(Tool::MeetingRecapTrack, r#"{"text":"Send the deck"}"#) {
            Ok(Some(id)) => id,
            other => panic!("track failed: {other:?}"),
        };
        assert_eq!(db.recap_tracked(sid), vec![Some(id)]);
        let again = backend.write(Tool::MeetingRecapTrack, r#"{"text":"Send the deck"}"#);
        assert!(matches!(again, Ok(Some(same)) if same == id), "tracking twice is idempotent");
        assert!(backend.write(Tool::MeetingRecapTrack, r#"{"text":"Wire the money"}"#).is_err());
        assert!(backend
            .write(Tool::MeetingRecapTrack, r#"{"text":"Send the deck","direction":"ours"}"#)
            .is_err());
    }

    #[test]
    fn meeting_speakers_round_trip_through_the_api_face() {
        // Invariant 6: the agent face sees the same speakers the Recap chips render, and naming
        // one is the same L1 assignment. Defaulting to the latest meeting keeps "name the person
        // in the meeting that just ended" a one-call operation.
        let db = Db::open_in_memory(Arc::new(|| 555)).unwrap();
        let backend = DbBackend::new(db.clone());
        let sid = db.open_meeting(Some("Weekly sync"), Some("us.zoom.xos"), 0.7, "{}").unwrap();
        db.append_transcript(
            sid,
            1_000,
            shogun_memory::transcript_segments::Speaker::OtherSlot(0),
            "hello",
            0.9,
        );

        let json = backend
            .read_structured(Tool::MeetingSpeakersGet, &ReadParams::default())
            .expect("structured read");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["session_id"], serde_json::json!(sid));
        assert_eq!(v["speakers"][0]["key"], "other:0");
        assert_eq!(v["speakers"][0]["display_name"], serde_json::Value::Null);

        let body = format!(
            r#"{{"session_id":{sid},"speaker_key":"other:0","display_name":"Tanaka"}}"#
        );
        assert!(matches!(backend.write(Tool::MeetingSpeakersSet, &body), Ok(None)));
        let json = backend
            .read_structured(Tool::MeetingSpeakersGet, &ReadParams::default())
            .expect("structured read");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["speakers"][0]["display_name"], "Tanaka");

        // An invented key is refused with a reason, never stored.
        let bad = format!(r#"{{"session_id":{sid},"speaker_key":"boss","display_name":"X"}}"#);
        assert!(backend.write(Tool::MeetingSpeakersSet, &bad).is_err());
    }

    #[test]
    fn patterns_round_trip_through_the_api_face() {
        // Invariant 6 / FR-PAT-06: the agent face sees the same overview the Patterns card
        // renders, and the ON/OFF switch is the same L1 write.
        let db = Db::open_in_memory(Arc::new(|| 700_000_000)).unwrap();
        let backend = DbBackend::new(db.clone());
        db.record_offer_impressions("notch", Some("us.zoom.xos"), &[("open_app".into(), Some(0))]);

        let json = backend
            .read_structured(Tool::PatternsList, &ReadParams::default())
            .expect("structured read");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["activities"][0]["kind"], "open_app");
        assert_eq!(v["activities"][0]["shown_7d"], 1);
        assert!(v["patterns"].as_array().unwrap().is_empty(), "no distilled rows yet");
        assert!(v["usage"].is_object(), "available usage is not an empty/error alias");
        assert_eq!(v["usage"]["ranges"].as_array().map(Vec::len), Some(4));
        assert!(v["usage"]["calendar_start"].is_string());
        assert!(v["usage"]["calendar_end"].is_string());

        // Toggling a missing pattern is refused with a reason, not silently accepted.
        assert!(backend
            .write(Tool::PatternsSetActive, r#"{"id":999,"active":false}"#)
            .is_err());
    }

    #[test]
    fn read_tool_through_the_write_lane_never_reports_success() {
        let backend = DbBackend::new(seed());
        assert!(backend.write(Tool::MemorySearch, "x").is_err());
    }

    #[test]
    fn visual_recall_status_reads_desktop_record_in_independent_backend(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let reader = backend_with_settings(Db::open_in_memory(Arc::new(|| 5_000))?);
        let path = reader
            .visual_recall_settings_path()
            .ok_or("settings path missing")?
            .with_file_name(crate::capture::status::FILE_NAME);
        for paused in [true, false] {
            crate::capture::status::write(&path, shogun_mcp::desktop_heartbeat::now_ms(), paused)?;
            let status = reader
                .read_structured(Tool::VisualRecallStatus, &params())
                .ok_or("status unavailable")?;
            let status: serde_json::Value = serde_json::from_str(&status)?;
            assert_eq!(status["capture_paused_url"], paused);
        }
        std::fs::remove_file(path)?;
        let status: serde_json::Value = serde_json::from_str(
            &reader
                .read_structured(Tool::VisualRecallStatus, &params())
                .ok_or("status unavailable")?,
        )?;
        assert!(status["capture_paused_url"].is_null());
        Ok(())
    }

    #[test]
    fn visual_recall_status_and_set_enabled() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let backend = backend_with_settings(db.clone());
        let status = || {
            backend
                .read_structured(Tool::VisualRecallStatus, &params())
                .expect("status")
        };
        assert!(status().contains("\"enabled\":false"));
        // Issue #782: the direct API write never turns it on; the faces queue that instead.
        let enable = r#"{"enabled":true}"#;
        let change = backend
            .visual_recall_change(Tool::VisualRecallSetEnabled, enable)
            .unwrap()
            .expect("turning it on needs approval");
        assert_eq!(
            change,
            shogun_agents::permission::VisualRecallWidening {
                enable: true,
                retention_days: None,
                previous_retention_days: 3,
            }
        );
        let refused = backend.write(Tool::VisualRecallSetEnabled, enable).unwrap_err();
        assert!(refused.starts_with("invalid "), "REST answers 400: {refused}");
        assert!(status().contains("\"enabled\":false"));
        // The user's confirmation of the queued change turns it on.
        let (action, _) = shogun_agents::approval::visual_recall_request(change);
        assert_eq!(db.apply_confirmed_setting(&action, None, None, None).map(|r| r.is_err()), Some(true));
        assert_eq!(
            db.apply_confirmed_setting(&action, None, None, backend.visual_recall_settings_path()),
            Some(Ok(()))
        );
        assert!(status().contains("\"enabled\":true"));
        // Already on: nothing widens, so the same body is a direct no-op write.
        assert_eq!(backend.visual_recall_change(Tool::VisualRecallSetEnabled, enable), Ok(None));
        assert!(backend.write(Tool::VisualRecallSetEnabled, enable).is_ok());
        // Turning it off is always direct.
        assert_eq!(backend.visual_recall_change(Tool::VisualRecallSetEnabled, r#"{"enabled":false}"#), Ok(None));
        assert!(backend.write(Tool::VisualRecallSetEnabled, r#"{"enabled":false}"#).is_ok());
        assert!(status().contains("\"enabled\":false"));
    }

    #[test]
    fn visual_recall_off_reports_a_failed_purge_and_a_retry_deletes() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let backend = backend_with_settings(db.clone());
        let (event, _, _) = db
            .ingest_screen_ocr(Some("com.example.App"), Some("Invoices"), "invoice totals", 0, Some(1))
            .unwrap();
        db.store_screen_frame(Some(event), None, Some("com.example.App"), Some("Invoices"), Some(1), 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, &[1])
            .unwrap();
        let off = r#"{"enabled":false}"#;
        db.test_block_screen_frame_deletes(true).unwrap();
        let error = backend.write(Tool::VisualRecallSetEnabled, off).unwrap_err();
        assert_eq!(error, VISUAL_RECALL_OFF_PURGE_FAILED, "Off must not report success");
        assert_eq!(db.screen_frame_stats().count, 1, "the frame is still there");
        let path = backend.visual_recall_settings_path().unwrap();
        assert!(!crate::capture::visual_recall::load_settings(path).enabled, "Off stays committed");
        db.test_block_screen_frame_deletes(false).unwrap();
        assert!(backend.write(Tool::VisualRecallSetEnabled, off).is_ok());
        assert_eq!(db.screen_frame_stats().count, 0, "the retry deletes it");
    }

    #[test]
    fn visual_recall_retention_lengthening_is_refused_directly_and_applied_only_as_approved() {
        // Issue #782: an API caller may shorten retention directly; keeping saved screens longer
        // waits for the user, and the confirmed change never goes beyond what the card showed.
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let backend = backend_with_settings(db.clone());
        let path = backend.visual_recall_settings_path().unwrap().to_path_buf();
        let days = || crate::capture::visual_recall::load_settings(&path).retention.days();
        assert_eq!(days(), 3);
        assert_eq!(backend.visual_recall_change(Tool::VisualRecallSetRetention, r#"{"days":3}"#), Ok(None));
        let longer = r#"{"days":3650}"#;
        let change = backend
            .visual_recall_change(Tool::VisualRecallSetRetention, longer)
            .unwrap()
            .expect("lengthening needs approval");
        assert_eq!(change.retention_days, Some(3650));
        assert_eq!(change.previous_retention_days, 3);
        assert!(!change.enable);
        let refused = backend.write(Tool::VisualRecallSetRetention, longer).unwrap_err();
        assert!(refused.starts_with("invalid "), "{refused}");
        assert_eq!(days(), 3);
        // Invalid bodies are errors to classify, never a widening.
        assert!(backend.visual_recall_change(Tool::VisualRecallSetRetention, r#"{"days":0}"#).is_err());
        assert!(backend.visual_recall_change(Tool::VisualRecallDeleteFrame, longer).is_err());

        // After the request the user shortened retention in Settings: confirming applies exactly
        // the requested 3650 days, and Visual recall stays off (the card did not turn it on).
        crate::capture::visual_recall::update_settings(&path, |settings| {
            settings.retention = RetentionPolicy::try_days(1).unwrap();
        })
        .unwrap();
        let (action, _) = shogun_agents::approval::visual_recall_request(change.clone());
        assert_eq!(db.apply_confirmed_setting(&action, None, None, Some(&path)), Some(Ok(())));
        assert_eq!(days(), 3650);
        assert!(!crate::capture::visual_recall::load_settings(&path).enabled);

        // A shorter request is a direct write; confirming the old card again never shortens the
        // user's later, longer choice and never goes beyond its own request.
        assert!(backend.write(Tool::VisualRecallSetRetention, r#"{"days":30}"#).is_ok());
        assert_eq!(days(), 30);
        let small = shogun_agents::permission::VisualRecallWidening {
            enable: false,
            retention_days: Some(7),
            previous_retention_days: 3,
        };
        let (action, _) = shogun_agents::approval::visual_recall_request(small);
        assert_eq!(db.apply_confirmed_setting(&action, None, None, Some(&path)), Some(Ok(())));
        assert_eq!(days(), 30);
    }

    #[test]
    fn a_confirmed_enable_fails_closed_when_retention_grew_past_what_the_card_showed() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let backend = backend_with_settings(db.clone());
        let path = backend.visual_recall_settings_path().unwrap().to_path_buf();
        let change = backend
            .visual_recall_change(Tool::VisualRecallSetEnabled, r#"{"enabled":true}"#)
            .unwrap()
            .expect("enable needs approval");
        // After the request, retention became longer than the 3 days the card showed.
        crate::capture::visual_recall::update_settings(&path, |settings| {
            settings.retention = RetentionPolicy::try_days(90).unwrap();
        })
        .unwrap();
        let (action, _) = shogun_agents::approval::visual_recall_request(change);
        assert!(matches!(db.apply_confirmed_setting(&action, None, None, Some(&path)), Some(Err(_))));
        assert!(!crate::capture::visual_recall::load_settings(&path).enabled);
        // A tampered, non-widening change is refused before anything is read.
        let bogus = shogun_agents::permission::SendAction::WidenVisualRecall {
            change: shogun_agents::permission::VisualRecallWidening::default(),
        };
        assert!(matches!(db.apply_confirmed_setting(&bogus, None, None, Some(&path)), Some(Err(_))));
    }

    #[test]
    fn the_shared_retention_ceiling_matches_the_settings_bound() {
        assert_eq!(
            shogun_agents::permission::VISUAL_RECALL_MAX_RETENTION_DAYS,
            crate::capture::visual_recall::MAX_CUSTOM_RETENTION_DAYS
        );
    }

    #[test]
    fn capture_exclusions_read_and_replace_share_one_file() {
        let backend = backend_with_settings(Db::open_in_memory(Arc::new(|| 5_000)).unwrap());
        assert!(backend.write(Tool::CaptureExclusionsSet, r#"{"apps":["com.example.Secret"],"websites":["private.example"]}"#).is_ok());
        let body = backend.read_structured(Tool::CaptureExclusionsGet, &params()).unwrap_or_default();
        assert!(body.contains("com.example.Secret"));
        assert!(body.contains("private.example"));
    }

    #[test]
    fn capture_exclusions_additions_are_direct_but_a_removal_is_refused_and_classified() {
        // Issue #657: the backend is the last line. An addition applies; a removal (even mixed
        // with an addition) never does here, and the faces learn exactly what it would resume.
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let backend = backend_with_settings(db.clone());
        let lists = |backend: &DbBackend| backend.read_structured(Tool::CaptureExclusionsGet, &params()).unwrap_or_default();
        let first = r#"{"apps":["com.example.Secret"],"websites":["private.example"]}"#;
        assert_eq!(backend.capture_exclusions_change(first), Ok(None));
        assert!(backend.write(Tool::CaptureExclusionsSet, first).is_ok());
        let grow = r#"{"apps":["com.example.Secret","com.example.New"],"websites":["https://Private.example/x","more.example"]}"#;
        assert_eq!(backend.capture_exclusions_change(grow), Ok(None), "normalized superset is an addition");
        assert!(backend.write(Tool::CaptureExclusionsSet, grow).is_ok());
        assert!(lists(&backend).contains("com.example.New"));

        let mixed = r#"{"apps":["com.example.New","com.example.Third"],"websites":["private.example","more.example"]}"#;
        let change = backend.capture_exclusions_change(mixed).unwrap().expect("a removal needs approval");
        assert_eq!(change.resumed_apps, vec!["com.example.Secret".to_string()]);
        assert!(change.resumed_websites.is_empty());
        assert_eq!(change.apps, vec!["com.example.New".to_string(), "com.example.Third".to_string()]);
        let before = lists(&backend);
        let refused = backend.write(Tool::CaptureExclusionsSet, mixed).unwrap_err();
        assert!(refused.starts_with("invalid "), "REST answers 400: {refused}");
        assert_eq!(lists(&backend), before, "nothing changed, not even the addition");

        // The user's confirmation of the queued change applies it atomically.
        let (action, _) = shogun_agents::approval::capture_resume_request(change);
        assert_eq!(db.apply_confirmed_setting(&action, None, None, None).map(|r| r.is_err()), Some(true));
        assert_eq!(db.apply_confirmed_setting(&action, backend.capture_exclusions_path(), None, None), Some(Ok(())));
        let after = lists(&backend);
        assert!(!after.contains("com.example.Secret"), "{after}");
        assert!(after.contains("com.example.Third"), "{after}");
    }

    #[test]
    fn a_confirmed_capture_change_is_refused_when_the_lists_moved_since_the_request() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let backend = backend_with_settings(db.clone());
        assert!(backend.write(Tool::CaptureExclusionsSet, r#"{"apps":["com.example.Secret"],"websites":[]}"#).is_ok());
        let change = backend
            .capture_exclusions_change(r#"{"apps":[],"websites":[]}"#)
            .unwrap()
            .expect("removal");
        // After the request, the user excluded another website in Settings.
        let path = backend.capture_exclusions_path().unwrap().to_path_buf();
        crate::capture::exclusion::replace_policy(
            &path,
            &["com.example.Secret".to_string()],
            &["later.example".to_string()],
        )
        .unwrap();
        let (action, _) = shogun_agents::approval::capture_resume_request(change);
        assert!(matches!(db.apply_confirmed_setting(&action, Some(&path), None, None), Some(Err(_))));
        let lists = backend.read_structured(Tool::CaptureExclusionsGet, &params()).unwrap_or_default();
        assert!(lists.contains("com.example.Secret") && lists.contains("later.example"), "{lists}");
    }

    #[test]
    fn agent_source_reads_presence_only_and_only_an_approval_selects_a_subscription() {
        // Issue #110 (invariant 6): the same llm.json the desktop runs on. An API key switch and
        // a withdrawal apply here; selecting a subscription is classified for the approval queue,
        // refused on the direct path, and applied only by the user's confirmation.
        use crate::llm::subscription::{out_ok, MockRunner, RunError};
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let path = std::env::temp_dir().join(format!(
            "shogun-agent-source-{}-{}.json",
            std::process::id(),
            db.now_ms()
        ));
        let _ = std::fs::remove_file(&path);
        // `claude --version` answers; the other two CLIs are not on PATH.
        let runner = MockRunner::new(
            vec![out_ok("2.1.0 (Claude Code)"), Err(RunError::NotFound), Err(RunError::NotFound)],
            Err(RunError::NotFound),
        );
        let backend = DbBackend::new(db.clone()).with_agent_source(AgentSourceConfig {
            path: path.clone(),
            delegates: Some(Arc::new(runner)),
            key_present: Arc::new(|provider: &str| Some(provider == "anthropic")),
        });

        let status: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::AgentSourceGet, &params()).unwrap()).unwrap();
        // No saved choice: SHOGUN, the hosted default (#1101, owner decision 2026-10-21). The API
        // face cannot attest the gateway's readiness for this Mac, so `configured` is unknown.
        assert_eq!(status["selected"], "shogun");
        assert_eq!(status["selected_kind"], "hosted");
        assert_eq!(status["subscription_consent"], false);
        assert_eq!(status["configured"], serde_json::Value::Null);
        let hosted = status["sources"].as_array().unwrap().iter().find(|s| s["id"] == "shogun").unwrap();
        assert_eq!(hosted["gateway"], "api.shogunaios.com");
        assert_eq!(hosted["stores_content"], false);
        assert_eq!(status["sources"][0]["id"], "claude-code");
        assert_eq!(status["sources"][0]["installed"], true);
        assert_eq!(status["sources"][1]["installed"], false);
        assert!(!status.to_string().contains("2.1.0"), "no vendor text: {status}");

        // Direct: an API key provider with one of its listed models (issue #389).
        assert_eq!(backend.agent_source_change(r#"{"source":"openai","model":"gpt-6-sol"}"#), Ok(None));
        assert!(backend.write(Tool::AgentSourceSet, r#"{"source":"openai","model":"gpt-6-sol"}"#).is_ok());
        assert_eq!(crate::agent_source::load_strict(&path).unwrap().provider, "openai");
        // A model the provider does not offer is refused on the same path the UI's select uses.
        assert_eq!(
            backend.agent_source_change(r#"{"model":"gpt-4o"}"#).err().as_deref(),
            Some("unknown_model")
        );
        let refused = backend.write(Tool::AgentSourceSet, r#"{"model":"gpt-4o"}"#).unwrap_err();
        assert!(refused.starts_with("invalid "), "REST answers 400: {refused}");
        assert_eq!(crate::agent_source::load_strict(&path).unwrap().model, "gpt-6-sol");
        let status: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::AgentSourceGet, &params()).unwrap()).unwrap();
        assert_eq!(status["model_effective"], "gpt-6-sol");
        assert_eq!(status["models"]["providers"]["openai"]["budget"], "gpt-6-luna");

        // #390 / #664: OpenAI response storage. `get` reports it (off by default); turning it on
        // is refused (not classified for approval), turning it off is a direct write.
        let status: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::AgentSourceGet, &params()).unwrap()).unwrap();
        assert_eq!(status["openai_store"], false);
        let store_on = r#"{"openai_store":true}"#;
        assert_eq!(backend.agent_source_change(store_on).err().as_deref(), Some("openai_store_only_in_app"));
        let refused = backend.write(Tool::AgentSourceSet, store_on).unwrap_err();
        assert!(refused.starts_with("invalid "), "REST answers 400: {refused}");
        assert!(!crate::agent_source::load_strict(&path).unwrap().openai_store);
        // The user turned it on in Settings (the desktop writes the file itself).
        let mut on = crate::agent_source::load_strict(&path).unwrap();
        on.openai_store = true;
        crate::agent_source::save(&path, &on).unwrap();
        let status: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::AgentSourceGet, &params()).unwrap()).unwrap();
        assert_eq!(status["openai_store"], true);
        assert_eq!(backend.agent_source_change(r#"{"openai_store":false}"#), Ok(None));
        assert!(backend.write(Tool::AgentSourceSet, r#"{"openai_store":false}"#).is_ok());
        assert!(!crate::agent_source::load_strict(&path).unwrap().openai_store);

        // A subscription: classified for approval, refused directly, nothing written.
        let body = r#"{"source":"claude-code","subscription_consent":true}"#;
        let request = backend.agent_source_change(body).unwrap().expect("needs approval");
        assert_eq!(request.delegate, "claude-code");
        assert_eq!(request.plan, "Claude Pro / Max");
        let refused = backend.write(Tool::AgentSourceSet, body).unwrap_err();
        assert!(refused.starts_with("invalid "), "REST answers 400: {refused}");
        assert!(backend.write(Tool::AgentSourceSet, r#"{"source":"claude-code"}"#).unwrap_err().starts_with("invalid "));
        assert!(backend.write(Tool::AgentSourceSet, r#"{"subscription_consent":true}"#).unwrap_err().starts_with("invalid "));
        assert_eq!(crate::agent_source::load_strict(&path).unwrap().provider, "openai");

        // The user's confirmation of the queued card selects it and records the opt-in.
        let (action, _) = shogun_agents::approval::subscription_agent_request(&request.delegate, &request.label, &request.plan);
        assert_eq!(db.apply_confirmed_setting(&action, None, None, None).map(|r| r.is_err()), Some(true));
        assert_eq!(db.apply_confirmed_setting(&action, None, Some(&path), None), Some(Ok(())));
        let now = crate::agent_source::load_strict(&path).unwrap();
        assert_eq!((now.provider.as_str(), now.subscription_consent), ("claude-code", true));

        // Withdrawing is direct.
        assert!(backend.write(Tool::AgentSourceSet, r#"{"subscription_consent":false}"#).is_ok());
        assert!(!crate::agent_source::load_strict(&path).unwrap().subscription_consent);
        let _ = std::fs::remove_file(path);

        // Unconfigured: an honest error, never a guess.
        let bare = DbBackend::new(db);
        assert!(bare.read_structured(Tool::AgentSourceGet, &params()).unwrap().contains("error"));
        assert!(bare.agent_source_change(body).is_err());
        assert!(bare.write(Tool::AgentSourceSet, body).is_err());
    }

    #[test]
    fn connections_serve_the_shared_files_and_only_ever_narrow() {
        // Issue #82 (invariant 6): the daemons read the desktop's published states and
        // composio.json, record disconnects for the desktop to apply, write only tightening, and
        // refuse granting Mail consent or turning draft-only mode off.
        use shogun_mcp::composio_policy;
        use shogun_mcp::connections_api::{self, ConnectionsPaths};
        use shogun_mcp::scope::Service;
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let dir = std::env::temp_dir().join(format!("shogun-connections-{}-{}", std::process::id(), db.now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = ConnectionsPaths::for_db(dir.join("memory.db").to_str().unwrap());
        assert_eq!(paths.requests, dir.join(connections_api::REQUESTS_FILE));
        let paths = ConnectionsPaths { composio: dir.join("composio.json"), ..paths };
        composio_policy::update(&paths.composio, |p| {
            p.consent_acknowledged = true;
            p.disclosures = shogun_mcp::composio::Disclosures { via_third_party: true, data_types: true, revocable: true };
            p.user_id = "user-123".into();
            p.gmail_connected = true;
            Ok(())
        })
        .unwrap();
        connections_api::write_status(
            &paths.status,
            shogun_mcp::desktop_heartbeat::now_ms(),
            &[(Service::Gmail, "connected"), (Service::GoogleDrive, "needs_reauth")],
        )
        .unwrap();
        let backend = DbBackend::new(db.clone()).with_connections(paths.clone());

        let list: serde_json::Value =
            serde_json::from_str(&backend.read_structured(Tool::ConnectionsList, &params()).unwrap()).unwrap();
        assert_eq!(list["app_running"], true);
        let gdrive = list["services"].as_array().unwrap().iter().find(|s| s["service"] == "gdrive").unwrap();
        assert_eq!(gdrive["state"], "needs_reauth");
        assert_eq!(list["composio"]["consent"], true);
        assert!(!list.to_string().contains("user-123"));
        assert!(backend.read(Tool::ConnectionsList, &params()).is_empty(), "never a ReadItem list");

        // Refused: nothing written.
        for body in [r#"{"draft_stop":false}"#, r#"{"consent":true,"draft_stop":false}"#] {
            let err = backend.write(Tool::ConnectionsComposioSet, body).unwrap_err();
            assert!(err.starts_with("invalid "), "REST answers 400: {err}");
        }
        assert!(composio_policy::load_effective(&paths.composio).draft_stop);

        // Direct Gmail policy is independent of legacy Composio consent. API callers can
        // reduce sending capability but cannot enable sends, including with unknown fields.
        for body in [
            r#"{"draft_stop":false}"#,
            r#"{"draft_stop":true,"consent":true}"#,
            r#"{}"#,
        ] {
            assert!(backend
                .write(Tool::ConnectionsGmailSet, body)
                .unwrap_err()
                .starts_with("invalid "));
        }
        assert_eq!(
            backend.write(Tool::ConnectionsGmailSet, r#"{"draft_stop":true}"#),
            Ok(None)
        );
        assert!(shogun_mcp::gmail_policy::load_effective(&paths.gmail).draft_stop);

        // Narrowing: direct.
        assert_eq!(backend.write(Tool::ConnectionsDisconnect, r#"{"service":"gdrive"}"#), Ok(None));
        assert_eq!(backend.write(Tool::ConnectionsComposioSet, r#"{"consent":false}"#), Ok(None));
        let policy = composio_policy::load_effective(&paths.composio);
        assert!(!policy.consent_acknowledged && policy.draft_stop && !policy.gmail_connected);
        assert_eq!(policy.user_id, "user-123", "the account setting is the desktop's to change");
        assert_eq!(
            connections_api::pending_disconnects(&paths.requests),
            vec![Service::GoogleDrive]
        );
        assert!(backend.write(Tool::ConnectionsComposioSet, r#"{"consent":true}"#).unwrap_err().contains("consent_only_in_app"));
        let _ = std::fs::remove_dir_all(&dir);

        // Unconfigured: an honest error, never a guess.
        let bare = DbBackend::new(db);
        assert!(bare.read_structured(Tool::ConnectionsList, &params()).unwrap().contains("error"));
        assert!(bare.write(Tool::ConnectionsDisconnect, r#"{"service":"gcal"}"#).is_err());
        assert!(bare.write(Tool::ConnectionsComposioSet, r#"{"consent":false}"#).is_err());
    }

    #[test]
    fn capture_exclusions_reject_invalid_payloads() {
        let backend = backend_with_settings(Db::open_in_memory(Arc::new(|| 5_000)).unwrap());
        assert_eq!(
            backend
                .write(Tool::CaptureExclusionsSet, r#"{"apps":[1],"websites":[]}"#)
                .err()
                .as_deref(),
            Some("invalid capture exclusions request")
        );
        assert_eq!(
            backend
                .write(Tool::CaptureExclusionsSet, r#"{"apps":[""],"websites":[]}"#)
                .err()
                .as_deref(),
            Some("invalid capture exclusion app")
        );
        assert_eq!(
            backend
                .write(
                    Tool::CaptureExclusionsSet,
                    r#"{"apps":[],"websites":["https://"]}"#,
                )
                .err()
                .as_deref(),
            Some("invalid capture exclusion website")
        );
    }

    #[test]
    fn visual_recall_search_intersects_app_filter_without_jpeg_payload() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        for (hash, app) in [("safari-frame", "com.apple.Safari"), ("mail-frame", "com.apple.Mail")] {
            let event = shogun_memory::event_log::NewEvent {
                ts: 4_000,
                source: "screen_ocr",
                kind: "text",
                app_bundle_id: Some(app),
                window_title: Some("Project Comet"),
                content: "project comet planning",
                content_hash: hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            };
            let (event_id, _) = db.capture(&event).unwrap();
            db.store_screen_frame(
                Some(event_id),
                None,
                Some(app),
                Some("Project Comet"),
                None,
                10,
                10,
                shogun_memory::screen_frames::OcrRegion::WholeFrame,
                b"jpeg-secret",
            )
            .unwrap();
        }
        let backend = backend_with_settings(db);
        enable_visual_recall_as_user(&backend);
        let body = backend
            .read_structured(
                Tool::VisualRecallSearchFrames,
                &ReadParams {
                    query: Some("project comet".into()),
                    app_bundle_id: Some("com.apple.Safari".into()),
                    from_ms: Some(0),
                    to_ms: Some(5_000),
                    ..ReadParams::default()
                },
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["total_count"], 1);
        assert_eq!(value["frames"][0]["app"], "com.apple.Safari");
        assert!(!body.contains("jpeg_base64"));
        assert!(!body.contains("jpeg-secret"));
    }

    #[test]
    fn visual_recall_search_pages_back_to_newer_rows_and_rejects_half_cursor() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        for index in 0..3 {
            let event = shogun_memory::event_log::NewEvent {
                ts: 4_000 + index,
                source: "screen_ocr",
                kind: "text",
                app_bundle_id: Some("com.apple.Safari"),
                window_title: Some("Cursor test"),
                content: "cursor test",
                content_hash: &format!("cursor-{index}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            };
            let (event_id, _) = db.capture(&event).unwrap();
            assert!(db
                .store_screen_frame(
                    Some(event_id),
                    None,
                    Some("com.apple.Safari"),
                    Some("Cursor test"),
                    None,
                    10,
                    10,
                    shogun_memory::screen_frames::OcrRegion::WholeFrame,
                    b"jpeg",
                )
                .is_some());
        }
        let backend = backend_with_settings(db);
        enable_visual_recall_as_user(&backend);
        let root: serde_json::Value = serde_json::from_str(
            &backend
                .read_structured(
                    Tool::VisualRecallSearchFrames,
                    &ReadParams {
                        query: Some("cursor".into()),
                        from_ms: Some(0),
                        to_ms: Some(5_000),
                        page_size: Some(1),
                        ..ReadParams::default()
                    },
                )
                .unwrap(),
        )
        .unwrap();
        let newest_id = root["frames"][0]["frame_id"].as_i64().unwrap();
        let next = &root["next_cursor"];
        assert!(root["previous_cursor"].is_null());

        let older: serde_json::Value = serde_json::from_str(
            &backend
                .read_structured(
                    Tool::VisualRecallSearchFrames,
                    &ReadParams {
                        query: Some("cursor".into()),
                        from_ms: Some(0),
                        to_ms: Some(5_000),
                        cursor_ts: next["ts"].as_i64(),
                        cursor_id: next["id"].as_i64(),
                        page_size: Some(1),
                        ..ReadParams::default()
                    },
                )
                .unwrap(),
        )
        .unwrap();
        let previous = &older["previous_cursor"];
        assert!(previous.is_object());

        let newer: serde_json::Value = serde_json::from_str(
            &backend
                .read_structured(
                    Tool::VisualRecallSearchFrames,
                    &ReadParams {
                        query: Some("cursor".into()),
                        from_ms: Some(0),
                        to_ms: Some(5_000),
                        cursor_ts: previous["ts"].as_i64(),
                        cursor_id: previous["id"].as_i64(),
                        cursor_direction: Some("newer".into()),
                        page_size: Some(1),
                        ..ReadParams::default()
                    },
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(newer["frames"][0]["frame_id"], newest_id);

        let invalid = backend
            .read_structured(
                Tool::VisualRecallSearchFrames,
                &ReadParams {
                    cursor_ts: Some(5_000),
                    ..ReadParams::default()
                },
            )
            .unwrap();
        assert!(invalid.contains("cursor_ts_and_cursor_id_required_together"));
    }

    #[test]
    fn visual_recall_newer_page_does_not_skip_the_row_next_to_its_cursor() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let mut frame_ids = Vec::new();
        for index in 0..7 {
            let event = shogun_memory::event_log::NewEvent {
                ts: 4_000 + index,
                source: "screen_ocr",
                kind: "text",
                app_bundle_id: Some("com.apple.Safari"),
                window_title: Some("Newer cursor test"),
                content: "newer cursor test",
                content_hash: &format!("newer-cursor-{index}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            };
            let (event_id, _) = db.capture(&event).unwrap();
            frame_ids.push(
                db.store_screen_frame(
                    Some(event_id),
                    None,
                    Some("com.apple.Safari"),
                    Some("Newer cursor test"),
                    None,
                    10,
                    10,
                    shogun_memory::screen_frames::OcrRegion::WholeFrame,
                    b"jpeg",
                )
                .unwrap(),
            );
        }
        let backend = backend_with_settings(db);
        enable_visual_recall_as_user(&backend);
        let body = backend
            .read_structured(
                Tool::VisualRecallSearchFrames,
                &ReadParams {
                    query: Some("newer cursor".into()),
                    from_ms: Some(0),
                    to_ms: Some(5_000),
                    cursor_ts: Some(5_000),
                    cursor_id: Some(frame_ids[2]),
                    cursor_direction: Some("newer".into()),
                    page_size: Some(2),
                    ..ReadParams::default()
                },
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let actual: Vec<_> = value["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|frame| frame["frame_id"].as_i64().unwrap())
            .collect();

        assert_eq!(actual, vec![frame_ids[4], frame_ids[3]]);
    }

    /// `backup.status` / `backup.set_retention` (issue #647): the shared settings file, the
    /// bounds, and an immediate prune that keeps the newest verified snapshot.
    #[test]
    fn backup_retention_is_read_and_set_through_the_backend_and_prunes_at_once() {
        use shogun_memory::backup::{BackupCoordinator, BackupPolicy, DAY_MS as D};

        let root = std::env::temp_dir().join(format!("shogun_db_backend_backup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = shogun_memory::DbKey::new([46; 32]);
        let source = shogun_memory::open_encrypted(root.join("memory.db"), &key).unwrap();
        let coordinator =
            BackupCoordinator::production(root.join("backups"), BackupPolicy::default()).unwrap();
        coordinator.set_max_age_days(30);
        // Preserve verified generations outside rotation to model an older app's backups.
        let mut old_snapshots = Vec::new();
        for (at, nonce) in [(D, "a"), (2 * D, "b"), (9 * D, "c")] {
            let pending = coordinator.copy_to_closed_temp(&source, &key, at, nonce).unwrap();
            let path = coordinator.verify_publish_rotate(&key, pending).unwrap().latest.unwrap().path;
            let staged = root.join(path.file_name().unwrap());
            std::fs::rename(&path, &staged).unwrap();
            old_snapshots.push(staged);
        }
        for path in old_snapshots {
            std::fs::rename(&path, coordinator.directory().join(path.file_name().unwrap())).unwrap();
        }

        let db = Db::open_in_memory(Arc::new(|| 10 * D)).unwrap();
        let key_for_loader = key.clone();
        let backend = DbBackend::new(db).with_backup_retention(BackupRetentionConfig {
            settings_path: root.join("backup.json"),
            backups_dir: root.join("backups"),
            key: Some(Arc::new(move || Some(key_for_loader.clone()))),
        });
        let status = |backend: &DbBackend| -> serde_json::Value {
            serde_json::from_str(&backend.read_structured(Tool::BackupStatus, &params()).unwrap())
                .unwrap()
        };

        let before = status(&backend);
        assert_eq!(before["retention_days"], 7, "default");
        assert_eq!(before["min_days"], 1);
        assert_eq!(before["max_days"], 30);
        assert_eq!(before["snapshots"]["count"], 3);

        for invalid in [r#"{"days":0}"#, r#"{"days":31}"#, r#"{"days":"7"}"#, "{}"] {
            let error = backend.write(Tool::BackupSetRetention, invalid).unwrap_err();
            assert!(error.starts_with("expected "), "a 400 on REST: {error}");
        }
        assert!(!root.join("backup.json").exists(), "a refused value is never saved");
        assert_eq!(status(&backend)["snapshots"]["count"], 3);

        // Saving 7 prunes at once: cutoff 3d, so the 1d and 2d snapshots go.
        assert_eq!(backend.write(Tool::BackupSetRetention, r#"{"days":7}"#), Ok(None));
        let after = status(&backend);
        assert_eq!(after["retention_days"], 7);
        assert_eq!(after["snapshots"]["count"], 1);
        assert_eq!(after["snapshots"]["newest_ms"], 9 * D);
        assert_eq!(crate::backup_retention::load(&root.join("backup.json")).days(), 7);

        // One day puts even the newest past the cap, but it is the only verified copy: kept.
        assert_eq!(backend.write(Tool::BackupSetRetention, "1"), Ok(None));
        let tight = status(&backend);
        assert_eq!(tight["retention_days"], 1);
        assert_eq!(tight["snapshots"]["count"], 1);

        // Lengthening applies directly too (L1) and removes nothing.
        assert_eq!(backend.write(Tool::BackupSetRetention, r#"{"days":30}"#), Ok(None));
        assert_eq!(status(&backend)["retention_days"], 30);

        // Without a key the setting still saves; the desktop's sweep applies it.
        let keyless = DbBackend::new(Db::open_in_memory(Arc::new(|| 10 * D)).unwrap())
            .with_backup_retention(BackupRetentionConfig {
                settings_path: root.join("backup.json"),
                backups_dir: root.join("backups"),
                key: None,
            });
        assert_eq!(keyless.write(Tool::BackupSetRetention, r#"{"days":2}"#), Ok(None));
        assert_eq!(status(&keyless)["retention_days"], 2);

        // A backend without the config answers plainly instead of guessing a path.
        let bare = DbBackend::new(Db::open_in_memory(Arc::new(|| 0)).unwrap());
        assert!(status(&bare)["error"].is_string());
        assert!(bare.write(Tool::BackupSetRetention, r#"{"days":7}"#).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `search_index.*` (#714 step 3): the API process records a start / stop for the desktop's
    /// job (refusing a start at once on a nearly full disk), and reads the same status JSON
    /// Settings renders — counts only.
    #[test]
    fn search_index_requests_reach_the_desktop_files_and_the_status_is_shared() {
        use crate::search_index_compact::FreeSpace;
        use shogun_mcp::search_index_api::{self, Action, SearchIndexPaths};

        struct Free(Option<u64>);
        impl FreeSpace for Free {
            fn available_bytes(&self) -> Option<u64> {
                self.0
            }
        }
        let root = std::env::temp_dir().join(format!("shogun_db_backend_index_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = SearchIndexPaths::for_db(root.join("memory.db"));
        let backend = |free: Option<u64>| {
            DbBackend::new(Db::open_in_memory(Arc::new(|| 5_000)).unwrap()).with_search_index(
                SearchIndexConfig { paths: paths.clone(), free: Arc::new(Free(free)) },
            )
        };
        let roomy = backend(Some(u64::MAX));
        let status = |b: &DbBackend| -> serde_json::Value {
            serde_json::from_str(&b.read_structured(Tool::SearchIndexStatus, &params()).unwrap()).unwrap()
        };
        let idle = status(&roomy);
        assert_eq!(idle["job"]["state"], "idle");
        assert_eq!(idle["index"]["tables"].as_array().unwrap().len(), 3);
        assert_eq!(idle["disk"]["enough"], true);
        assert!(roomy.read(Tool::SearchIndexStatus, &params()).is_empty(), "never a ReadItem list");

        // Start: preflighted here, taken by the desktop from the file.
        assert_eq!(roomy.write(Tool::SearchIndexCompactStart, "{}"), Ok(None));
        assert_eq!(search_index_api::pending(&paths.requests, 5_000), Some(Action::Start));
        assert_eq!(status(&roomy)["job"]["state"], "queued");
        assert_eq!(roomy.write(Tool::SearchIndexCompactStop, ""), Ok(None));
        assert_eq!(search_index_api::pending(&paths.requests, 5_000), Some(Action::Stop));
        assert!(roomy.write(Tool::SearchIndexCompactStart, r#"{"force":true}"#).unwrap_err().starts_with("invalid "));

        // A nearly full disk refuses the start at once and queues nothing.
        let _ = std::fs::remove_file(&paths.requests);
        let full = backend(Some(1));
        let refused = full.write(Tool::SearchIndexCompactStart, "{}").unwrap_err();
        assert!(refused.starts_with("insufficient_disk:"), "{refused}");
        assert!(!paths.requests.exists());
        assert!(backend(None).write(Tool::SearchIndexCompactStart, "{}").unwrap_err().starts_with("disk_unknown:"));

        // Without the config, a plain error rather than a guessed path.
        let bare = DbBackend::new(Db::open_in_memory(Arc::new(|| 0)).unwrap());
        assert!(status(&bare)["error"].is_string());
        assert!(bare.write(Tool::SearchIndexCompactStop, "{}").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hidden_self_delete_requires_exclusive_true_intent_and_status_exposes_storage() {
        for body in [r#"{"hidden_self_frames":false}"#, r#"{"hidden_self_frames":"true"}"#,
            r#"{"hidden_self_frames":null}"#, r#"{"hidden_self_frames":true,"id":1}"#,
            r#"{"hidden_self_frames":true,"all":true}"#] {
            assert!(parse_hidden_self_delete_body(body).is_err(), "{body}");
        }
        assert_eq!(parse_hidden_self_delete_body(r#"{"hidden_self_frames":true}"#), Ok(true));
        assert_eq!(parse_hidden_self_delete_body(r#"{"id":12}"#), Ok(false));
        assert_eq!(parse_hidden_self_delete_body("12"), Ok(false));
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let backend = backend_with_settings(db);
        let status: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::VisualRecallStatus, &params()).unwrap()).unwrap();
        assert_eq!(status["hidden_self_frames_count"], 0);
        assert_eq!(status["hidden_self_frames_bytes"], 0);
        assert!(backend.write(Tool::VisualRecallDeleteFrame, "{}").is_err());
        assert_eq!(backend.write(Tool::VisualRecallDeleteFrame, r#"{"hidden_self_frames":true}"#), Ok(None));
    }

    #[test]
    fn retention_write_validates_boundaries_and_purges_immediately_by_age() {
        use std::sync::atomic::{AtomicI64, Ordering};

        let clock = Arc::new(AtomicI64::new(0));
        let clock_for_db = clock.clone();
        let db = Db::open_in_memory(Arc::new(move || clock_for_db.load(Ordering::SeqCst))).unwrap();
        let (old_event, _) = db.capture(&ev("old-frame")).unwrap();
        assert!(db
            .store_screen_frame(Some(old_event), None, None, None, None, 10, 10, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"old")
            .is_some());

        clock.store(2 * DAY_MS, Ordering::SeqCst);
        let (recent_event, _) = db.capture(&ev("recent-frame")).unwrap();
        assert!(db
            .store_screen_frame(Some(recent_event), None, None, None, None, 10, 10, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"recent")
            .is_some());
        assert_eq!(db.screen_frame_stats().count, 2);

        let backend = backend_with_settings(db.clone());
        assert!(backend
            .write(Tool::VisualRecallSetRetention, r#"{"days":1}"#)
            .is_ok());
        assert_eq!(db.screen_frame_stats().count, 1);
        let status = backend
            .read_structured(Tool::VisualRecallStatus, &params())
            .expect("status");
        assert!(status.contains("\"retention_days\":1"));

        for invalid in [r#"{"days":0}"#, r#"{"days":3651}"#, r#"{"days":"forever"}"#] {
            assert!(backend
                .write(Tool::VisualRecallSetRetention, invalid)
                .is_err());
        }
    }

    #[test]
    fn visual_recall_api_exposes_accessibility_text_and_searches_both_lanes() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let (ocr_event_id, _, _) = db
            .ingest_screen_ocr(
                Some("org.telegram.desktop"),
                Some("Orchid"),
                "fallback canvas glyphs",
                1,
                Some(1),
            )
            .unwrap();
        let (accessibility_event_id, _, _) = db
            .ingest_capture(
                Some("org.telegram.desktop"),
                Some("Orchid"),
                "Maya owns project Orchid",
                1,
            )
            .unwrap();
        let frame_id = db
            .store_screen_frame(
                Some(ocr_event_id),
                Some(accessibility_event_id),
                Some("org.telegram.desktop"),
                Some("Orchid"),
                Some(1),
                10,
                10,
                shogun_memory::screen_frames::OcrRegion::WholeFrame,
                b"jpeg",
            )
            .unwrap();
        let backend = backend_with_settings(db);
        enable_visual_recall_as_user(&backend);

        let frame: serde_json::Value = serde_json::from_str(
            &backend
                .read_structured(
                    Tool::VisualRecallGetFrame,
                    &ReadParams { id: Some(frame_id), ..ReadParams::default() },
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(frame["accessibility_text"], "Maya owns project Orchid");
        assert_eq!(frame["ocr_text"], "fallback canvas glyphs");
        assert_eq!(frame["accessibility_event_id"], accessibility_event_id);
        assert_eq!(frame["text_sources"], json!(["accessibility", "ocr"]));

        for query in ["Orchid", "glyphs"] {
            let search: serde_json::Value = serde_json::from_str(
                &backend
                    .read_structured(
                        Tool::VisualRecallSearchFrames,
                        &ReadParams {
                            query: Some(query.to_string()),
                            from_ms: Some(0),
                            to_ms: Some(10_000),
                            ..ReadParams::default()
                        },
                    )
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(search["frames"][0]["frame_id"], frame_id);
            assert_eq!(
                search["frames"][0]["text_sources"],
                json!(["accessibility", "ocr"])
            );
        }
    }

    #[test]
    fn retention_parser_accepts_legacy_and_nested_shapes() {
        assert_eq!(parse_retention_body("7").unwrap().days(), 7);
        assert_eq!(
            parse_retention_body(r#"{"retention_days":14}"#)
                .unwrap()
                .days(),
            14
        );
        assert_eq!(
            parse_retention_body(r#"{"retention":{"days":30}}"#)
                .unwrap()
                .days(),
            30
        );
        assert_eq!(
            parse_retention_body(r#"{"days":7,"retention_days":7}"#)
                .unwrap()
                .days(),
            7
        );
        assert!(parse_retention_body(r#"{"days":7,"retention_days":14}"#).is_err());
    }

    #[test]
    fn expired_frame_id_is_unavailable_without_desktop_poller() {
        use std::sync::atomic::{AtomicI64, Ordering};

        let clock = Arc::new(AtomicI64::new(0));
        let clock_for_db = clock.clone();
        let db = Db::open_in_memory(Arc::new(move || clock_for_db.load(Ordering::SeqCst))).unwrap();
        let (event_id, _) = db.capture(&ev("lazy-expiry")).unwrap();
        let frame_id = db
            .store_screen_frame(Some(event_id), None, None, None, None, 10, 10, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"jpeg")
            .expect("stored frame");
        let backend = backend_with_settings(db.clone());

        clock.store(4 * DAY_MS, Ordering::SeqCst);
        let result = backend
            .read_structured(
                Tool::VisualRecallGetFrame,
                &ReadParams {
                    id: Some(frame_id),
                    ..ReadParams::default()
                },
            )
            .expect("structured result");
        assert!(result.contains("not_found"));
        assert_eq!(db.screen_frame_stats().count, 0);
    }

    #[test]
    fn rescan_frame_api_applies_off_and_retention_before_reading() {
        use std::sync::atomic::{AtomicI64, Ordering};

        let rescan = |backend: &DbBackend, id: i64| -> serde_json::Value {
            let json = shogun_mcp::visual_recall_api::rescan_frame(backend, Some(id));
            serde_json::from_str(&json).expect("rescan JSON")
        };
        let clock = Arc::new(AtomicI64::new(DAY_MS));
        let clock_for_db = clock.clone();
        let db = Db::open_in_memory(Arc::new(move || clock_for_db.load(Ordering::SeqCst))).unwrap();
        let backend = backend_with_settings(db.clone());
        let seed = |db: &Db| {
            let (event, _, _) = db
                .ingest_screen_ocr(Some("com.example.App"), Some("Invoices"), "lnvoice tota1s", 0, Some(1))
                .expect("ocr event");
            db.store_screen_frame(Some(event), None, Some("com.example.App"), Some("Invoices"), Some(1), 10, 10, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"jpeg")
                .expect("stored frame")
        };

        // Off (the default): the passive frame is purged, never re-read.
        let off_frame = seed(&db);
        let off = rescan(&backend, off_frame);
        assert_eq!(off["error"], "not_found");
        assert_eq!(off["reason"], "visual_recall_off");

        enable_visual_recall_as_user(&backend);
        let fresh = seed(&db);
        // The stored bytes are not an image, so recognition reads nothing on every platform.
        assert_eq!(rescan(&backend, fresh)["error"], "ocr_failed");
        assert_eq!(rescan(&backend, fresh + 1_000)["error"], "not_found");
        assert!(rescan(&backend, fresh + 1_000).get("reason").is_none());

        // Past the default 3-day retention, before any sweep: refused as expired.
        clock.store(5 * DAY_MS, Ordering::SeqCst);
        let expired = rescan(&backend, fresh);
        assert_eq!(expired["error"], "not_found");
        assert_eq!(expired["reason"], "expired");
        assert_eq!(db.screen_frame_stats().count, 0);
    }

    /// Seed a distilled lesson via the real feedback → distill → upsert path. The draft bodies
    /// carry an unmistakable secret marker so tests can assert it never reaches the API.
    fn seed_lesson(db: &Db) -> i64 {
        use shogun_memory::lessons::{distill, FeedbackKind, LessonScope, NewFeedback};
        const SECRET: &str = "SECRET_FEEDBACK_BODY_q1";
        for i in 0..3 {
            let before = format!(
                "Hi team,\nA longer draft body about the {SECRET} quarterly numbers, line {i}.\nMore detail follows in the tracker.\nBest, Taro"
            );
            let after = format!(
                "Hi team,\nA longer draft body about the {SECRET} quarterly numbers, line {i}.\nMore detail follows in the tracker."
            );
            db.record_feedback(
                FeedbackKind::EditBeforeApprove,
                LessonScope::App,
                &NewFeedback {
                    ts_ms: i,
                    action_kind: Some("draft_reply"),
                    scope_ref: Some("com.apple.Mail"),
                    before_text: Some(&before),
                    after_text: Some(&after),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let candidates = distill(&db.feedback_after(0));
        assert_eq!(candidates.len(), 1);
        db.upsert_lesson(&candidates[0], 100).unwrap()
    }

    #[test]
    fn get_periods_is_structured_bounded_and_rejects_an_inverted_window() {
        let db = Db::open_in_memory(Arc::new(|| 72_000_000)).unwrap();
        let backend = DbBackend::new(db);
        let read = |from_ms, to_ms| {
            let p = ReadParams { from_ms, to_ms, ..ReadParams::default() };
            let json = backend.read_structured(Tool::MemoryGetPeriods, &p).expect("structured");
            serde_json::from_str::<serde_json::Value>(&json).unwrap()
        };
        // Default window (last 24h) on an empty store: no periods, a note that says where else to look.
        let v = read(None, None);
        assert_eq!(v["periods"].as_array().map(Vec::len), Some(0));
        assert!(v["note"].as_str().is_some_and(|n| n.contains("memory.search")));
        assert_eq!(read(Some(10), Some(5))["error"], "invalid_range");
        assert!(backend.read(Tool::MemoryGetPeriods, &params()).is_empty());
    }

    #[test]
    fn get_wrap_is_structured_and_carries_the_wrap_shape() {
        // Issue #10 (invariant 6): the API face serves the same evening_wrap aggregation the
        // card draws. Shape only here — the aggregation itself is pinned by the daemon's test.
        let db = Db::open_in_memory(Arc::new(|| 72_000_000)).unwrap();
        let backend = DbBackend::new(db);
        let json = backend
            .read_structured(Tool::MemoryGetWrap, &params())
            .expect("structured");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            v["date"].as_str().is_some_and(|d| d.len() == 10),
            "local date key"
        );
        assert_eq!(v["outcome"]["commitments_done"], 0);
        assert!(v["still_open"].as_array().is_some());
        assert!(v["tomorrow_calendar"].as_array().is_some());
        assert!(v["loose_ends"].as_array().is_some());
        // And the plain-read face has nothing for it (structured-only tool).
        assert!(backend.read(Tool::MemoryGetWrap, &params()).is_empty());
    }

    #[test]
    fn calendar_upcoming_serves_the_today_agenda_and_the_wrap_sees_tomorrow() {
        const DAY: i64 = 24 * 3_600_000;
        // Linux tests run in UTC: day 100, 12:00.
        let now = 100 * DAY + 12 * 3_600_000;
        let db = Db::open_in_memory(Arc::new(move || now)).unwrap();
        let event = |id: &str, title: &str, start: i64, all_day: bool| shogun_mcp::sync::IngestItem {
            source: "gcal",
            kind: "calendar_event",
            title: title.into(),
            body: title.into(),
            ts_ms: now - DAY,
            external_id: id.into(),
            mail: None,
            calendar: Some(shogun_mcp::sync::CalendarMeta {
                start_ms: start,
                end_ms: start + if all_day { DAY } else { 3_600_000 },
                all_day,
                attendees: vec![
                    shogun_mcp::sync::CalendarAttendee { is_self: true, ..Default::default() },
                    shogun_mcp::sync::CalendarAttendee { email: Some("a@example.com".into()), ..Default::default() },
                ],
                ..Default::default()
            }),
        };
        db.ingest_integration(&[
            event("review", "Design review", now + 3_600_000, false),
            event("offsite", "Offsite", 101 * DAY, true),
        ]);
        let backend = DbBackend::new(db);
        let v: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::CalendarUpcoming, &params()).expect("structured"),
        )
        .unwrap();
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(v["total"], 2);
        assert_eq!(v["hidden_count"], 0);
        let mut one = params();
        one.page_size = Some(1);
        let limited: serde_json::Value = serde_json::from_str(&backend.read_structured(Tool::CalendarUpcoming, &one).expect("test")).expect("test");
        assert_eq!(limited["events"].as_array().expect("test").len(), 1);
        assert_eq!(limited["hidden_count"], 1);
        assert_eq!(events[0]["title"], "Design review");
        assert_eq!((events[0]["day"].as_u64(), events[0]["attendees"].as_u64()), (Some(0), Some(1)));
        assert_eq!((events[1]["title"].as_str(), events[1]["all_day"].as_bool()), (Some("Offsite"), Some(true)));
        assert!(events.iter().all(|e| e.get("possibly").is_none()), "calendar facts are not hedged");
        assert!(backend.read(Tool::CalendarUpcoming, &params()).is_empty());

        let wrap: serde_json::Value = serde_json::from_str(
            &backend.read_structured(Tool::MemoryGetWrap, &params()).expect("structured"),
        )
        .unwrap();
        assert_eq!(wrap["tomorrow_calendar"][0]["title"], "Offsite");
    }

    /// Issue #779 (FR-EB-05 parity): every outcome figure the Evening Wrap card draws is in the
    /// API's `outcome`. The exhaustive destructure stops compiling when the card gains a field,
    /// so the API cannot fall behind it silently again.
    #[test]
    fn get_wrap_outcome_carries_every_field_the_card_shows() {
        let shogun_fusion::wrap::WrapOutcome {
            commitments_done: _,
            loops_closed: _,
            actions_decided: _,
            actions_adopted: _,
            mail_triaged: _,
            mail_auto_archived: _,
            mail_needs_reply: _,
        } = shogun_fusion::wrap::WrapOutcome::default();
        let card_fields = [
            "commitments_done",
            "loops_closed",
            "actions_decided",
            "actions_adopted",
            "mail_triaged",
            "mail_auto_archived",
            "mail_needs_reply",
        ];
        let backend = DbBackend::new(Db::open_in_memory(Arc::new(|| 72_000_000)).unwrap());
        let json = backend
            .read_structured(Tool::MemoryGetWrap, &params())
            .expect("structured");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for field in card_fields {
            assert!(v["outcome"][field].is_u64(), "outcome.{field} missing: {v}");
        }
    }

    #[test]
    fn lessons_list_is_structured_and_never_exposes_feedback_text() {
        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let id = seed_lesson(&db);
        let backend = DbBackend::new(db);

        let json = backend
            .read_structured(Tool::LessonsList, &params())
            .expect("structured");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let row = &v["lessons"][0];
        assert_eq!(row["id"], id);
        assert_eq!(row["kind"], "style");
        assert_eq!(row["scope"], "app");
        assert_eq!(row["scope_ref"], "com.apple.Mail");
        assert_eq!(row["evidence_count"], 3);
        assert_eq!(row["active"], true);
        assert!(row["instruction"].as_str().unwrap().contains("Best, Taro"));
        assert!(row["confidence"].as_f64().unwrap() >= 0.5);
        // The invariant: feedback_events text never leaves the DB through the API.
        assert!(
            !json.contains("SECRET_FEEDBACK_BODY_q1"),
            "feedback text leaked: {json}"
        );
        assert!(
            !json.contains("quarterly numbers"),
            "feedback text leaked: {json}"
        );
    }

    #[test]
    fn lessons_list_for_generation_drops_person_lessons_until_scoped() {
        use shogun_memory::lessons::{
            FeedbackKind, LessonCandidate, LessonKind, LessonScope, NewFeedback,
        };

        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let seed = |scope: LessonScope, scope_ref: Option<&str>, instruction: &str| {
            let evidence = (0..3)
                .map(|ts_ms| {
                    db.record_feedback(
                        FeedbackKind::EditBeforeApprove,
                        scope,
                        &NewFeedback {
                            ts_ms,
                            scope_ref,
                            action_kind: Some("draft_reply"),
                            ..Default::default()
                        },
                    )
                    .expect("feedback")
                })
                .collect();
            db.upsert_lesson(
                &LessonCandidate {
                    kind: LessonKind::Style,
                    scope,
                    scope_ref: scope_ref.map(str::to_owned),
                    instruction: instruction.to_owned(),
                    evidence,
                },
                100,
            )
            .expect("lesson");
        };
        seed(LessonScope::Global, None, "GLOBAL_LESSON");
        seed(
            LessonScope::Person,
            Some("alice@example.com"),
            "ALICE_ONLY_LESSON",
        );
        let backend = DbBackend::new(db);

        let all = backend
            .read_structured(Tool::LessonsList, &params())
            .expect("structured");
        assert!(all.contains("GLOBAL_LESSON"), "{all}");
        assert!(
            all.contains("ALICE_ONLY_LESSON"),
            "management list keeps person rows: {all}"
        );

        let lookup = backend
            .read_structured(
                Tool::LessonsList,
                &ReadParams {
                    for_generation: true,
                    ..ReadParams::default()
                },
            )
            .expect("structured");
        assert!(lookup.contains("GLOBAL_LESSON"), "{lookup}");
        assert!(
            !lookup.contains("ALICE_ONLY_LESSON"),
            "unscoped generation lookup must not leak person lessons: {lookup}"
        );

        let alice = backend
            .read_structured(
                Tool::LessonsList,
                &ReadParams {
                    person_id: Some("alice@example.com".into()),
                    ..ReadParams::default()
                },
            )
            .expect("structured");
        assert!(alice.contains("GLOBAL_LESSON"), "{alice}");
        assert!(alice.contains("ALICE_ONLY_LESSON"), "{alice}");
    }

    /// A Db holding one open contradiction flag (issue #253): the same promise captured twice
    /// with different due dates. Returns the db, the dispute id and the (earlier, later) ids.
    fn seed_dispute() -> (Db, i64, i64, i64) {
        use shogun_memory::event_log::{content_hash, insert as insert_event, NewEvent};
        use shogun_memory::extract::{extract, persist_candidates};
        const DAY: i64 = 24 * 60 * 60 * 1000;
        let wed = 20_719 * DAY + 10 * 3_600_000;
        let mut conn = shogun_memory::open_in_memory().unwrap();
        let mut capture = |text: &str, at: i64| {
            let hash = content_hash(text);
            let event = NewEvent {
                ts: at,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: text,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            };
            let event_id = insert_event(&conn, &event).unwrap();
            persist_candidates(&mut conn, event_id, &extract(text), at, at).unwrap()[0]
        };
        let earlier = capture("I'll send the report by Friday.", wed);
        let later = capture("I'll send the report on Monday.", wed + 3_600_000);
        let dispute = shogun_memory::disputes::list_open(&conn, 10).unwrap()[0].id;
        (Db::new(conn, Arc::new(move || wed + DAY)), dispute, earlier, later)
    }

    #[test]
    fn state_update_delete_and_export_share_one_backend() {
        let (db, _dispute, earlier, later) = seed_dispute();
        let backend = DbBackend::new(db.clone());
        // bad bodies are caller errors (400 on REST), unknown rows are not-found (404)
        for bad in [
            "not json",
            r#"{"table":"events","id":1}"#,
            r#"{"table":"commitments","id":1,"confidence":1}"#,
        ] {
            let err = backend.write(Tool::StateUpdate, bad).unwrap_err();
            assert!(err.starts_with("invalid "), "{err}");
        }
        assert_eq!(
            backend.write(Tool::StateUpdate, r#"{"table":"commitments","id":9999,"status":"done"}"#),
            Err("state record not found".to_string())
        );
        assert!(backend
            .write(Tool::StateDelete, &format!(r#"{{"table":"commitments","id":{later},"status":"done"}}"#))
            .unwrap_err()
            .starts_with("invalid "));

        // an edit pins the row: confidence 1.0, and the export says so
        assert_eq!(
            backend.write(
                Tool::StateUpdate,
                &format!(r#"{{"table":"commitments","id":{earlier},"description":"send the Q3 report"}}"#)
            ),
            Ok(None)
        );
        let edited = db.commitment(earlier).unwrap();
        assert_eq!(edited.description, "send the Q3 report");
        assert_eq!(edited.confidence, 1.0);

        assert_eq!(
            backend.write(Tool::StateDelete, &format!(r#"{{"table":"commitments","id":{later}}}"#)),
            Ok(None)
        );
        assert!(db.commitment(later).is_none());
        assert!(db.state_disputes_open(10).is_empty(), "the dispute went with its row");

        let body = backend
            .read_structured(Tool::StateExport, &ReadParams { include_low: true, ..ReadParams::default() })
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let md = v["markdown"].as_str().unwrap();
        assert!(md.contains("## Commitments (1)"), "{md}");
        assert!(md.contains("send the Q3 report"), "{md}");
        assert!(md.contains("edited"), "{md}");
        assert_eq!(v["tables"]["commitments"][0]["pinned"], true);
        assert_eq!(v["tables"]["commitments"][0]["possibly"], false);
        assert!(backend.read(Tool::StateExport, &params()).is_empty());
    }

    /// Issue #394 end to end on the shared DB: a run asks, every API face lists and answers
    /// through the same store, the run polls its answer back; a double answer, an unknown id and a
    /// late answer are refused; expiry closes a question with nothing executed.
    #[test]
    fn steer_goes_to_the_runners_store_on_the_db_clock_and_is_off_without_one() {
        // Issue #395: the daemon's backend writes the note to `steer_runs.json`; without a store
        // configured it is a store fault (503 / MCP error), never a silent success.
        use shogun_agents::clarify::{RunKind, RunRef};
        use shogun_agents::steer::SteerOrigin;
        use shogun_mcp::steer_api::{self, SteerOperation};

        let db = Db::new(shogun_memory::open_in_memory().unwrap(), Arc::new(|| 5_000));
        let run = RunRef { kind: RunKind::Notch, id: 2 };
        let op = SteerOperation { run, text: "only this week".into(), origin: SteerOrigin::Api };
        assert!(DbBackend::new(db.clone()).steer(op.clone()).is_err());

        let dir = std::env::temp_dir().join(format!("shogun-db-steer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(steer_api::STORE_FILE);
        let backend = DbBackend::new(db).with_steer_store(path.clone());
        let v: serde_json::Value = serde_json::from_str(&backend.steer(op.clone()).unwrap()).unwrap();
        assert_eq!(v["error"], "unknown_run");
        steer_api::open_run(&path, run, 4_000).unwrap();
        let v: serde_json::Value = serde_json::from_str(&backend.steer(op).unwrap()).unwrap();
        assert_eq!(v["accepted"], true);
        let notes = steer_api::take_for_turn(&path, run).unwrap();
        assert_eq!((notes[0].text.as_str(), notes[0].at_ms), ("only this week", 5_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clarify_ask_list_answer_poll_and_expire_on_the_shared_db() {
        use shogun_agents::approval::ApprovalOrigin;
        use shogun_agents::clarify::{append_to_user_half, Question, QuestionState, RunKind, RunRef};
        use shogun_mcp::clarify_api::ClarifyOperation as Op;
        use std::sync::atomic::{AtomicI64, Ordering};

        let clock = Arc::new(AtomicI64::new(100_000));
        let tick = clock.clone();
        let db = Db::new(shogun_memory::open_in_memory().unwrap(), Arc::new(move || tick.load(Ordering::SeqCst)));
        let backend = DbBackend::new(db.clone());
        let json = |op: Op| -> serde_json::Value { serde_json::from_str(&backend.clarify(op).unwrap()).unwrap() };

        let run = RunRef { kind: RunKind::SkillRun, id: 7 };
        let q = Question::new("Which Sam — Sam Lee or Sam Ortiz?", &["Sam Lee", "Sam Ortiz"], true, 100_000, None).unwrap();
        let id = db.clarify_ask(run, &q).unwrap();
        assert_eq!(db.clarify_ask(run, &q), Err(crate::daemon::ClarifyAskError::AlreadyOpen), "one at a time");
        let other = RunRef { kind: RunKind::Chat, id: 1 };
        let short = Question::new("Which folder?", &[], true, 100_000, Some(30_000)).unwrap();
        let late = db.clarify_ask(other, &short).unwrap();

        let listed = json(Op::List);
        assert_eq!(listed["count"], 2);
        assert_eq!(listed["questions"][0]["question_id"], late, "newest first");
        assert_eq!(listed["questions"][1]["status"], "awaiting_input");
        assert_eq!(listed["questions"][1]["run"], serde_json::json!({"kind":"skill_run","id":7}));

        // Not an option but free text is allowed; the MCP face's origin is recorded.
        let answered = json(Op::Answer { id, answer: "Sam Park".into(), origin: ApprovalOrigin::Mcp });
        assert_eq!(answered["status"], "answered", "{answered}");
        assert_eq!(answered["answered_via"], "mcp");
        let again = json(Op::Answer { id, answer: "Sam Lee".into(), origin: ApprovalOrigin::Api });
        assert_eq!(again["error"], "already_answered");
        assert_eq!(json(Op::Answer { id: 999, answer: "x".into(), origin: ApprovalOrigin::Api })["error"], "unknown_question");

        // The run polls its answer back and appends it to its untrusted user half.
        let polled = db.clarify_poll(id).unwrap();
        assert_eq!((polled.run, polled.state), (run, QuestionState::Answered));
        let (question, answer) = polled.resumed.unwrap();
        assert_eq!(answer.text(), "Sam Park");
        assert!(append_to_user_half("ctx", &question, &answer).ends_with("Answer: Sam Park"));

        // Past its deadline the other question expires; an answer is refused.
        clock.store(130_000, Ordering::SeqCst);
        assert_eq!(json(Op::Answer { id: late, answer: "Docs".into(), origin: ApprovalOrigin::Ui })["error"], "expired");
        assert_eq!(json(Op::Get { id: late })["question"]["status"], "expired");
        assert_eq!(json(Op::List)["count"], 0);
        assert_eq!(json(Op::Get { id: 999 })["error"], "not_found");
        let polled = db.clarify_poll(late).unwrap();
        assert_eq!((polled.state, polled.resumed.is_none()), (QuestionState::Expired, true));

        // A finished run's question is closed.
        let next = db.clarify_ask(other, &Question::new("Which tab?", &[], true, 130_000, None).unwrap()).unwrap();
        assert!(db.clarify_cancel_run(other));
        assert_eq!(json(Op::Answer { id: next, answer: "x".into(), origin: ApprovalOrigin::Ui })["error"], "cancelled");
        // Nothing reached the event log: a question and its answer are not capture.
        assert!(backend.read(Tool::MemorySearch, &ReadParams { query: Some("Sam".into()), ..ReadParams::default() }).is_empty());
    }

    #[test]
    fn memory_import_goes_through_the_shared_db_and_records_api_origin() {
        let db = Db::new(shogun_memory::open_in_memory().unwrap(), Arc::new(|| 9_000));
        let backend = DbBackend::new(db);
        let body = r#"{"package":{"format":"shogun-memory-import/v1","source":{"platform":"chatgpt"},
            "profile":{"goals":[{"value":"Ship v1","confidence":"medium","evidence":"e","status":"active"}]}},
            "select":["goals/0"]}"#;
        let preview: serde_json::Value = serde_json::from_str(
            &backend.memory_import(MemoryImportOperation::Preview { body: body.into() }).unwrap(),
        )
        .unwrap();
        assert_eq!(preview["items"][0]["action"]["type"], "new");
        let out: serde_json::Value = serde_json::from_str(
            &backend.memory_import(MemoryImportOperation::Commit { body: body.into() }).unwrap(),
        )
        .unwrap();
        let id = out["import_id"].as_i64().unwrap();
        let one: serde_json::Value = serde_json::from_str(
            &backend.memory_import(MemoryImportOperation::List { id: Some(id), include_low: false }).unwrap(),
        )
        .unwrap();
        assert_eq!(one["import"]["via"], "api");
        assert_eq!(one["import"]["items"][0]["provenance"], "imported:chatgpt");
        // Nothing reached the event log: an import is never passed off as capture.
        assert!(backend.read(Tool::MemorySearch, &ReadParams { query: Some("Ship".into()), ..ReadParams::default() }).is_empty());
    }

    /// `memory.search` (MCP / REST / CLI) surfaces a committed imported claim with its origin as
    /// the source kind, the import record as provenance, and `possibly` — never as observed memory.
    #[test]
    fn memory_search_marks_imported_claims_and_drops_them_after_delete() {
        let db = Db::new(shogun_memory::open_in_memory().unwrap(), Arc::new(|| 9_000));
        let backend = DbBackend::new(db);
        let body = r#"{"package":{"format":"shogun-memory-import/v1","source":{"platform":"chatgpt"},
            "profile":{"goals":[{"value":"Ship the Plover redesign","confidence":"high","evidence":"e","status":"active"}]}},
            "select":"default"}"#;
        let out: serde_json::Value = serde_json::from_str(
            &backend.memory_import(MemoryImportOperation::Commit { body: body.into() }).unwrap(),
        )
        .unwrap();
        let id = out["import_id"].as_i64().unwrap();
        let request = SearchRequestV1 { query: "Plover redesign".into(), ..SearchRequestV1::default() };

        let response = backend.search(&request).unwrap();
        assert_eq!(response.results.len(), 1);
        let hit = &response.results[0];
        assert_eq!(hit.source_kind, "imported:chatgpt");
        assert!(hit.source_id.starts_with("memory_import_items:"));
        assert_eq!(hit.provenance, [format!("memory_import:{id}")]);
        assert!(hit.possibly);
        assert!(hit.confidence <= shogun_memory::memory_import::CONFIDENCE_MAX);
        assert!(!response.answerability.answerable, "an imported claim alone never answers as fact");

        backend.memory_import(MemoryImportOperation::Delete { id }).unwrap();
        assert!(backend.search(&request).unwrap().results.is_empty());
    }

    #[test]
    fn merge_review_shares_one_backend() {
        let conn = shogun_memory::open_in_memory().unwrap();
        let add = |name: &str, created: i64| -> i64 {
            conn.execute(
                "INSERT INTO people(display_name,confidence,base_confidence,created_at,updated_at,last_evidence_at,user_pinned,mention_count)
                 VALUES(?1,0.6,0.6,?2,?2,?2,0,2)",
                rusqlite::params![name, created],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let alex = add("Alex", 100);
        let alex_m = add("Alex Morgan", 200);
        let kim = add("Kim", 300);
        let kim_l = add("Kim Lee", 400);
        let db = Db::new(conn, Arc::new(|| 5_000));
        let backend = DbBackend::new(db);

        let read = |table: &str| -> serde_json::Value {
            let params = ReadParams { key: Some(table.into()), ..ReadParams::default() };
            serde_json::from_str(&backend.read_structured(Tool::StateMergeCandidates, &params).unwrap()).unwrap()
        };
        let before = read("people");
        assert_eq!(before["table"], "people");
        assert_eq!(before["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(before["candidates"][0]["ids"], serde_json::json!([kim, kim_l]), "newest first");
        assert_eq!(before["candidates"][1]["names"], serde_json::json!(["Alex", "Alex Morgan"]));
        assert_eq!(read("commitments")["error"], "invalid_table");

        for bad in ["{}", r#"{"table":"people","ids":[1]}"#, r#"{"table":"open_loops","id":1,"other_id":2}"#] {
            let tool = if bad.contains("other_id") { Tool::StateKeepSeparate } else { Tool::StateMerge };
            let err = backend.write(tool, bad).unwrap_err();
            assert!(err.starts_with("invalid ") || err.contains("must") || err.contains("at least"), "{err}");
        }
        assert_eq!(
            backend.write(Tool::StateMerge, &format!(r#"{{"table":"people","ids":[{alex},9999]}}"#)),
            Err("state record not found".to_string())
        );

        // Keep separate answers with the first id and removes the pair from the candidates.
        assert_eq!(
            backend.write(Tool::StateKeepSeparate, &format!(r#"{{"table":"people","id":{kim},"other_id":{kim_l}}}"#)),
            Ok(Some(kim))
        );
        let after_block = read("people");
        assert_eq!(after_block["candidates"].as_array().unwrap().len(), 1);
        // Merge answers with the survivor (the older row) and empties the list.
        assert_eq!(
            backend.write(Tool::StateMerge, &format!(r#"{{"table":"people","ids":[{alex_m},{alex}]}}"#)),
            Ok(Some(alex))
        );
        assert!(read("people")["candidates"].as_array().unwrap().is_empty());
        // The blocked pair stays refused even when asked directly.
        let err = backend.write(Tool::StateMerge, &format!(r#"{{"table":"people","ids":[{kim},{kim_l}]}}"#)).unwrap_err();
        assert!(err.contains("cannot be merged"), "{err}");
    }

    #[test]
    fn split_and_evidence_share_one_backend() {
        let mut conn = shogun_memory::open_in_memory().unwrap();
        let e: Vec<i64> = (0..3)
            .map(|i| {
                shogun_memory::event_log::insert(
                    &conn,
                    &shogun_memory::event_log::NewEvent {
                        ts: 1_000 + i,
                        source: "capture",
                        kind: "text",
                        app_bundle_id: None,
                        window_title: None,
                        content: "evidence",
                        content_hash: &format!("split{i}"),
                        dwell_ms: 0,
                        display_id: None,
                        window_bounds: None,
                    },
                )
                .unwrap()
            })
            .collect();
        let alex = shogun_memory::state::insert_person(
            &mut conn,
            &shogun_memory::state::NewPerson {
                display_name: "Alex",
                emails_json: Some(r#"["alex.k@acme.com","alex.m@acme.com"]"#),
                confidence: 0.9,
                now: 1,
                ..Default::default()
            },
            &e.iter().copied().map(shogun_memory::state::Provenance::new).collect::<Vec<_>>(),
        )
        .unwrap();
        let db = Db::new(conn, Arc::new(|| 5_000));
        let backend = DbBackend::new(db);

        let read = |table: &str, id: i64| -> serde_json::Value {
            let params = ReadParams { key: Some(table.into()), id: Some(id), ..ReadParams::default() };
            serde_json::from_str(&backend.read_structured(Tool::StateSplitEvidence, &params).unwrap()).unwrap()
        };
        let ev = read("people", alex);
        assert_eq!(ev["event_count"], 3);
        assert_eq!(ev["aliases"][1]["value"], "alex.m@acme.com");
        assert_eq!(ev["events"][0]["id"], e[2], "newest first");
        assert!(ev["events"][0].get("content").is_none(), "metadata only");
        assert_eq!(read("people", 9_999)["error"], "not_found");
        assert_eq!(read("commitments", alex)["error"], "invalid_table");

        for bad in [
            "{}",
            r#"{"table":"commitments","id":1,"name":"x","event_ids":[1]}"#,
            &format!(r#"{{"table":"people","id":{alex},"name":"B","event_ids":[{},{},{}]}}"#, e[0], e[1], e[2]),
        ] {
            let err = backend.write(Tool::StateSplit, bad).unwrap_err();
            assert!(err.starts_with("invalid "), "{err}");
        }
        assert_eq!(
            backend.write(Tool::StateSplit, r#"{"table":"people","id":9999,"name":"B","event_ids":[1]}"#),
            Err("state record not found".to_string())
        );
        let new_id = backend
            .write(
                Tool::StateSplit,
                &format!(
                    r#"{{"table":"people","id":{alex},"name":"Alex Morgan","event_ids":[{}],"aliases":["alex.m@acme.com"]}}"#,
                    e[2]
                ),
            )
            .unwrap()
            .unwrap();
        let moved = read("people", new_id);
        assert_eq!(moved["title"], "Alex Morgan");
        assert_eq!(moved["event_count"], 1);
        assert_eq!(moved["aliases"][0]["value"], "alex.m@acme.com");
        assert_eq!(read("people", alex)["event_count"], 2);
    }

    #[test]
    fn disputes_list_is_gated_and_capped_below_fact() {
        let (db, dispute, earlier, later) = seed_dispute();
        assert_eq!(db.state_disputes_open(10).len(), 1);
        let backend = DbBackend::new(db.clone());
        let low = ReadParams {
            include_low: true,
            ..ReadParams::default()
        };
        let body = backend
            .read_structured(Tool::StateDisputesList, &low)
            .expect("state.disputes.list is a structured read");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let d = &v["disputes"][0];
        assert_eq!(d["id"], dispute);
        assert_eq!(d["field"], "due_at");
        assert_eq!(d["earlier"]["id"], earlier);
        assert_eq!(d["later"]["id"], later);
        for side in ["earlier", "later"] {
            // Never passed as fact: capped below the High band and flagged `possibly`.
            assert_eq!(d[side]["possibly"], true, "{body}");
            assert!(d[side]["confidence"].as_f64().unwrap() < 0.8, "{body}");
            assert!(d[side]["evidence_event_id"].is_i64(), "{body}");
        }
        // Without include_low, a pair is listed only when neither side is Low.
        let gated = backend
            .read_structured(Tool::StateDisputesList, &params())
            .unwrap();
        let gated: serde_json::Value = serde_json::from_str(&gated).unwrap();
        let low_side = ["earlier", "later"]
            .iter()
            .any(|s| d[*s]["confidence"].as_f64().unwrap() < 0.5);
        assert_eq!(gated["disputes"].as_array().unwrap().is_empty(), low_side);
        // Not a plain ReadItem read.
        assert!(backend.read(Tool::StateDisputesList, &params()).is_empty());
    }

    #[test]
    fn disputes_resolve_settles_once_and_rejects_bad_input() {
        let (db, dispute, earlier, later) = seed_dispute();
        let backend = DbBackend::new(db.clone());
        // Bad bodies are caller errors (400 on REST): wrong JSON, missing id, unknown resolution.
        for bad in [
            "not json",
            r#"{"resolution":"keep_later"}"#,
            &format!(r#"{{"id":{dispute},"resolution":"newest"}}"#),
        ] {
            let err = backend.write(Tool::StateDisputesResolve, bad).unwrap_err();
            assert!(err.starts_with("expected "), "{err}");
        }
        // Unknown id is not-found (404 on REST).
        assert_eq!(
            backend.write(
                Tool::StateDisputesResolve,
                r#"{"id":9999,"resolution":"keep_later"}"#
            ),
            Err("dispute not found".to_string())
        );
        assert_eq!(
            backend.write(
                Tool::StateDisputesResolve,
                &format!(r#"{{"id":{dispute},"resolution":"keep_later"}}"#)
            ),
            Ok(Some(dispute))
        );
        assert!(db.state_disputes_open(10).is_empty());
        // The earlier reading lost: cancelled, not deleted; the later one stays open.
        assert_eq!(
            db.commitment(earlier).map(|c| c.status),
            Some("cancelled".to_string())
        );
        assert_eq!(
            db.commitment(later).map(|c| c.status),
            Some("open".to_string())
        );
        // Already resolved → not found, and the Db method reports it the same way.
        assert_eq!(
            backend.write(
                Tool::StateDisputesResolve,
                &format!(r#"{{"id":{dispute},"resolution":"keep_both"}}"#)
            ),
            Err("dispute not found".to_string())
        );
        assert_eq!(
            db.state_dispute_resolve(dispute, shogun_memory::disputes::Resolution::KeepBoth),
            Ok(false)
        );
    }

    #[test]
    fn lessons_set_active_flips_the_switch_and_rejects_bad_input() {
        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let id = seed_lesson(&db);
        let backend = DbBackend::new(db.clone());

        // off…
        assert_eq!(
            backend.write(
                Tool::LessonsSetActive,
                &format!(r#"{{"id":{id},"active":false}}"#)
            ),
            Ok(Some(id))
        );
        assert!(!db.lessons_all()[0].active);
        // …and back on
        assert_eq!(
            backend.write(
                Tool::LessonsSetActive,
                &format!(r#"{{"id":{id},"active":true}}"#)
            ),
            Ok(Some(id))
        );
        assert!(db.lessons_all()[0].active);
        // unknown id and malformed bodies error, and lessons.list is not a plain read
        assert!(backend
            .write(Tool::LessonsSetActive, r#"{"id":9999,"active":false}"#)
            .is_err());
        assert!(backend.write(Tool::LessonsSetActive, "not json").is_err());
        assert!(backend
            .write(Tool::LessonsSetActive, r#"{"id":1}"#)
            .is_err());
        assert!(backend.read(Tool::LessonsList, &params()).is_empty());
    }

    #[test]
    fn full_stack_disputes_through_rest_respond_with() {
        use shogun_mcp::memory_api::TokenRegistry;
        use shogun_mcp::rest::{respond_with, Method, RestRequest};

        let (db, dispute, _, _) = seed_dispute();
        let backend = DbBackend::new(db);
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let req = |method, path: &str, body: Option<&str>| RestRequest {
            method,
            path: path.into(),
            token: Some("t".into()),
            include_low: true,
            query: None,
            search_depth: None,
            search_source_kind: None,
            search_mode: None,
            search_bound_invalid: false,
            body: body.map(str::to_string),
            from_ms: None,
            to_ms: None,
            for_generation: false,
            app_bundle_id: None,
            cursor_ts: None,
            cursor_id: None,
            cursor_direction: None,
            page_size: None,
            person_id: None,
            project_id: None,
            mail: Default::default(),
        };

        let (status, body) = respond_with(
            &req(Method::Get, "/v1/state/disputes", None),
            &tokens,
            &ent,
            &backend,
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"tool\":\"state.disputes.list\""), "{body}");
        assert!(body.contains("send the report"), "{body}");

        let resolve = |body: &str| {
            respond_with(
                &req(Method::Post, "/v1/state/disputes/resolve", Some(body)),
                &tokens,
                &ent,
                &backend,
            )
        };
        // bad resolution → 400; unknown id → 404; then the real decision → 202, and again → 404
        assert_eq!(resolve(&format!(r#"{{"id":{dispute},"resolution":"x"}}"#)).0, 400);
        assert_eq!(resolve(r#"{"id":9999,"resolution":"keep_later"}"#).0, 404);
        let (status, body) = resolve(&format!(r#"{{"id":{dispute},"resolution":"keep_both"}}"#));
        assert_eq!(status, 202, "{body}");
        assert!(body.contains("\"accepted\":true"), "{body}");
        assert_eq!(resolve(&format!(r#"{{"id":{dispute},"resolution":"keep_both"}}"#)).0, 404);
    }

    #[test]
    fn full_stack_lessons_through_rest_respond_with() {
        use shogun_mcp::memory_api::TokenRegistry;
        use shogun_mcp::rest::{respond_with, Method, RestRequest};

        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let id = seed_lesson(&db);
        let backend = DbBackend::new(db.clone());
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let req = |method, path: &str, body: Option<&str>| RestRequest {
            method,
            path: path.into(),
            token: Some("t".into()),
            include_low: false,
            query: None,
            search_depth: None,
            search_source_kind: None,
            search_mode: None,
            search_bound_invalid: false,
            body: body.map(str::to_string),
            from_ms: None,
            to_ms: None,
            for_generation: false,
            app_bundle_id: None,
            cursor_ts: None,
            cursor_id: None,
            cursor_direction: None,
            page_size: None,
            person_id: None,
            project_id: None,
            mail: Default::default(),
        };

        // GET /v1/lessons lists the lesson (no feedback text — pinned above per-payload)
        let (status, body) = respond_with(
            &req(Method::Get, "/v1/lessons", None),
            &tokens,
            &ent,
            &backend,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"tool\":\"lessons.list\""), "{body}");
        assert!(body.contains("Best, Taro"), "{body}");
        assert!(!body.contains("SECRET_FEEDBACK_BODY_q1"), "{body}");

        let (status, lookup) = respond_with(
            &RestRequest {
                for_generation: true,
                ..req(Method::Get, "/v1/lessons", None)
            },
            &tokens,
            &ent,
            &backend,
        );
        assert_eq!(status, 200);
        assert!(
            !lookup.contains("Best, Taro"),
            "unscoped generation lookup must drop app lessons: {lookup}"
        );
        let (status, scoped) = respond_with(
            &RestRequest {
                app_bundle_id: Some("com.apple.Mail".into()),
                ..req(Method::Get, "/v1/lessons", None)
            },
            &tokens,
            &ent,
            &backend,
        );
        assert_eq!(status, 200);
        assert!(scoped.contains("Best, Taro"), "{scoped}");

        // POST /v1/lessons/active flips it off through the same face
        let (status, body) = respond_with(
            &req(
                Method::Post,
                "/v1/lessons/active",
                Some(&format!(r#"{{"id":{id},"active":false}}"#)),
            ),
            &tokens,
            &ent,
            &backend,
        );
        assert_eq!(status, 202);
        assert!(body.contains("\"level\":\"L1\""), "{body}");
        assert!(!db.lessons_all()[0].active);
    }

    #[test]
    fn full_stack_note_write_through_rest_respond_with() {
        use shogun_mcp::memory_api::TokenRegistry;
        use shogun_mcp::rest::{respond_with, Method, RestRequest};

        let backend = DbBackend::new(Db::open_in_memory(Arc::new(|| 1)).unwrap());
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let req = RestRequest {
            method: Method::Post,
            path: "/v1/memory/notes".into(),
            token: Some("t".into()),
            include_low: false,
            query: None,
            search_depth: None,
            search_source_kind: None,
            search_mode: None,
            search_bound_invalid: false,
            body: Some("ship v1 on friday".into()),
            from_ms: None,
            to_ms: None,
            for_generation: false,
            app_bundle_id: None,
            cursor_ts: None,
            cursor_id: None,
            cursor_direction: None,
            page_size: None,
            person_id: None,
            project_id: None,
            mail: Default::default(),
        };
        let (status, body) = respond_with(
            &req,
            &tokens,
            &shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
            &backend,
        );
        assert_eq!(status, 202);
        assert!(body.contains("memory.append_note"));
        assert!(body.contains("\"level\":\"L1\""));
        assert!(
            body.contains("\"note_id\":"),
            "persisted note id missing: {body}"
        );
    }

    #[test]
    fn full_stack_db_backend_through_rest_respond_with() {
        use shogun_mcp::memory_api::TokenRegistry;
        use shogun_mcp::rest::{respond_with, Method, RestRequest};

        let backend = DbBackend::new(seed());
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let req = RestRequest {
            method: Method::Get,
            path: "/v1/state/commitments".into(),
            token: Some("t".into()),
            include_low: false,
            query: None,
            search_depth: None,
            search_source_kind: None,
            search_mode: None,
            search_bound_invalid: false,
            body: None,
            from_ms: None,
            to_ms: None,
            for_generation: false,
            app_bundle_id: None,
            cursor_ts: None,
            cursor_id: None,
            cursor_direction: None,
            page_size: None,
            person_id: None,
            project_id: None,
            mail: Default::default(),
        };
        let (status, body) = respond_with(
            &req,
            &tokens,
            &shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
            &backend,
        );
        assert_eq!(status, 200);
        // real DB data rendered through the API layer's confidence-gated JSON
        assert!(body.contains("send the report"), "body: {body}");
        assert!(body.contains("state.commitments.list"));
    }

    #[test]
    fn real_backend_search_contract_is_identical_across_cli_rest_and_mcp() {
        use shogun_agents::approval::ApprovalQueue;
        use shogun_mcp::mcp::McpServer;
        use shogun_mcp::rest::{respond_with, Method, RestRequest};
        use shogun_memory::event_log::NewEvent;
        use shogun_memory::state::{NewProject, ProjectStatus, Provenance};

        fn rest_request(call: &shogun_cli::wire::HttpCall) -> RestRequest {
            let (path, query) = call.path.split_once('?').expect("search query string");
            let mut request = RestRequest {
                method: Method::Get,
                path: path.to_string(),
                token: Some("contract-token".to_string()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: None,
                from_ms: None,
                to_ms: None,
                cursor_ts: None,
                cursor_id: None,
                cursor_direction: None,
                page_size: None,
                for_generation: false,
                app_bundle_id: None,
                person_id: None,
                project_id: None,
                mail: Default::default(),
            };
            for pair in query.split('&') {
                let (key, value) = pair.split_once('=').expect("key=value");
                match key {
                    "q" => request.query = Some(value.to_string()),
                    "depth" => request.search_depth = Some(value.to_string()),
                    "source_kind" => request.search_source_kind = Some(value.to_string()),
                    "from_ms" => request.from_ms = value.parse().ok(),
                    "to_ms" => request.to_ms = value.parse().ok(),
                    "mode" => request.search_mode = Some(value.to_string()),
                    other => panic!("unexpected CLI search control: {other}"),
                }
            }
            request
        }

        fn without_elapsed(mut response: serde_json::Value) -> serde_json::Value {
            response.as_object_mut().unwrap().remove("elapsed_ms");
            response
        }

        let now = shogun_memory::search::WARM_WINDOW_MS * 2;
        let db = Db::open_in_memory(Arc::new(move || now)).unwrap();
        let evidence = db
            .capture(&NewEvent {
                ts: now - 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "ordinary source evidence",
                content_hash: "real-cross-face-project",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap()
            .0;
        db.insert_project(
            &NewProject {
                name: "Orchid",
                status: ProjectStatus::Active,
                summary: Some("orchildprotocol derived-only discovery"),
                participants_json: None,
                sources_json: None,
                confidence: 0.9,
                now: now - 1,
            },
            &[Provenance::new(evidence)],
        )
        .unwrap();

        let mut tokens = shogun_mcp::memory_api::TokenRegistry::new();
        tokens.issue("contract-token");
        let range = (now - shogun_memory::search::WARM_WINDOW_MS, now);
        for (depth, from_ms, to_ms) in [
            (None, None, None),
            (
                Some(shogun_mcp::backend::SearchDepthV1::Warm),
                Some(range.0),
                Some(range.1),
            ),
            (
                Some(shogun_mcp::backend::SearchDepthV1::Auto),
                Some(range.0),
                Some(range.1),
            ),
            (
                Some(shogun_mcp::backend::SearchDepthV1::All),
                Some(range.0),
                Some(range.1),
            ),
        ] {
            let command = shogun_cli::command::Command::Search {
                query: "orchildprotocol".to_string(),
                depth,
                from_ms,
                to_ms,
                source_kind: None,
                limit: None,
                ai: false,
            };
            let call = shogun_cli::wire::to_call(&command, false).unwrap();
            let request = rest_request(&call);
            let (status, body) = respond_with(
                &request,
                &tokens,
                &shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
                &DbBackend::new(db.clone()),
            );
            assert_eq!(status, 200, "CLI-generated REST request: {call:?}: {body}");
            let rest_payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(rest_payload["tool"], "memory.search");
            assert_eq!(rest_payload["complete"], true);
            assert_eq!(rest_payload["results"][0]["source_kind"], "derived:project");
            assert!(rest_payload["results"][0]["provenance"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == &format!("event:{evidence}")));

            let mut arguments = serde_json::json!({ "query": "orchildprotocol" });
            if let Some(depth) = depth {
                arguments["depth"] = serde_json::json!(match depth {
                    shogun_mcp::backend::SearchDepthV1::Warm => "warm",
                    shogun_mcp::backend::SearchDepthV1::Auto => "auto",
                    shogun_mcp::backend::SearchDepthV1::All => "all",
                });
                arguments["from_ms"] = serde_json::json!(from_ms.unwrap());
                arguments["to_ms"] = serde_json::json!(to_ms.unwrap());
            }
            let server = McpServer::new(
                DbBackend::new(db.clone()),
                Arc::new(std::sync::Mutex::new(ApprovalQueue::new())),
                move || now,
                || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
            );
            let line = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "memory.search", "arguments": arguments },
            })
            .to_string();
            let mcp_response: serde_json::Value =
                serde_json::from_str(&server.handle_line(&line).unwrap()).unwrap();
            let mcp_payload: serde_json::Value = serde_json::from_str(
                mcp_response["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(without_elapsed(rest_payload), without_elapsed(mcp_payload));
        }

        let default = shogun_cli::wire::to_call(
            &shogun_cli::command::Command::Search {
                query: "orchildprotocol".to_string(),
                depth: None,
                from_ms: None,
                to_ms: None,
                source_kind: None,
                limit: None,
                ai: false,
            },
            false,
        )
        .unwrap();
        let (status, body) = respond_with(
            &rest_request(&default),
            &tokens,
            &shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
            &DbBackend::new(db),
        );
        let default_payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, 200);
        assert_eq!(default_payload["requested_depth"], "warm");
        assert_eq!(default_payload["cold_partitions_visited"], 0);
        assert_eq!(default_payload["cold_vectors_scanned"], 0);
    }

    // mail triage (issue #319, FR-TG-30)

    fn seed_mail_thread(db: &Db) {
        use shogun_mcp::sync::{IngestItem, MailMeta, MailMessageMeta};
        db.add_mail_identity("me@select.co.jp");
        let item = IngestItem {
            source: "gmail",
            kind: "email",
            title: "Contract draft".into(),
            body: "Could you send the signed draft by Friday?".into(),
            ts_ms: 900_000,
            external_id: "t-42".into(),
            calendar: None,
            mail: Some(MailMeta {
                thread_id: "t-42".into(),
                messages: vec![MailMessageMeta {
                    message_id: "m1".into(),
                    from_addr: "alice@select.co.jp".into(),
                    from_name: Some("Alice".into()),
                    to_addrs: vec!["me@select.co.jp".into()],
                    gmail_label_ids: vec!["INBOX".into()],
                    internal_date_ms: 900_000,
                    ..Default::default()
                }],
            }),
        };
        assert_eq!(
            db.ingest_integration(std::slice::from_ref(&item))
                .newly_inserted,
            1
        );
        assert!(db.mail_triage_get("gmail:t-42").is_some());
    }

    fn mail_params(key: Option<&str>) -> ReadParams {
        ReadParams {
            key: key.map(str::to_string),
            ..ReadParams::default()
        }
    }

    fn structured(backend: &DbBackend, tool: Tool, params: &ReadParams) -> serde_json::Value {
        serde_json::from_str(&backend.read_structured(tool, params).expect("structured read"))
            .expect("json")
    }

    #[test]
    fn mail_triage_list_get_and_set_through_the_api_face() {
        use shogun_memory::mail_triage::{Bucket, Stage};
        let db = Db::open_in_memory(Arc::new(|| 1_000_000)).unwrap();
        seed_mail_thread(&db);
        let backend = DbBackend::new(db.clone());

        // list: the estimate with confidence / possibly / stage / provenance — no snippet.
        let list = structured(&backend, Tool::MailTriageList, &params());
        assert_eq!(list["count"], 1, "{list}");
        let row = &list["threads"][0];
        assert_eq!(row["thread_key"], "gmail:t-42");
        assert_eq!(row["thread_id"], "t-42");
        assert_eq!(row["subject"], "Contract draft");
        assert!(row["possibly"].is_boolean(), "{row}");
        assert!(row["confidence"].is_number(), "{row}");
        assert!(row["stage"].is_string(), "{row}");
        assert!(row["provenance"][0]["event_id"].is_number(), "{row}");
        assert!(row.get("snippet").is_none(), "no mail content on the API face: {row}");

        // an unknown bucket / attention is refused, not dropped.
        let bad = structured(
            &backend,
            Tool::MailTriageList,
            &ReadParams {
                mail: shogun_mcp::backend::MailListQuery {
                    bucket: Some("nonsense".into()),
                    ..Default::default()
                },
                ..ReadParams::default()
            },
        );
        assert_eq!(bad["error"], "invalid_bucket");

        // get by key, and not_found for an unknown key / a missing key.
        let got = structured(&backend, Tool::MailTriageGet, &mail_params(Some("gmail:t-42")));
        assert_eq!(got["thread"]["thread_key"], "gmail:t-42");
        assert_eq!(
            structured(&backend, Tool::MailTriageGet, &mail_params(Some("gmail:nope")))["error"],
            "not_found"
        );
        assert_eq!(
            structured(&backend, Tool::MailTriageGet, &mail_params(None))["error"],
            "missing_thread_key"
        );

        // set: a correction is a Stage U row; an empty change or an unknown thread is refused.
        assert_eq!(
            backend.write(
                Tool::MailTriageSet,
                r#"{"thread_key":"gmail:t-42","bucket":"other"}"#
            ),
            Ok(None)
        );
        let row = db.mail_triage_get("gmail:t-42").unwrap();
        assert_eq!(row.bucket, Bucket::Other);
        assert_eq!(row.stage, Stage::User);
        assert_eq!(
            backend.write(Tool::MailTriageSet, r#"{"thread_key":"gmail:nope","handled":true}"#),
            Err("thread not found".to_string())
        );
        assert!(backend
            .write(Tool::MailTriageSet, r#"{"thread_key":"gmail:t-42"}"#)
            .is_err_and(|e| e.starts_with("expected ")));
        assert!(backend
            .write(Tool::MailTriageSet, r#"{"thread_key":"gmail:t-42","bucket":"sideways"}"#)
            .is_err_and(|e| e.starts_with("invalid bucket")));

        // handled rows leave the default list and come back with include_handled.
        assert_eq!(
            backend.write(Tool::MailTriageSet, r#"{"thread_key":"gmail:t-42","handled":true}"#),
            Ok(None)
        );
        assert_eq!(structured(&backend, Tool::MailTriageList, &params())["count"], 0);
        let handled = structured(
            &backend,
            Tool::MailTriageList,
            &ReadParams {
                mail: shogun_mcp::backend::MailListQuery {
                    include_handled: true,
                    ..Default::default()
                },
                ..ReadParams::default()
            },
        );
        assert_eq!(handled["count"], 1, "{handled}");
    }

    #[test]
    fn mail_archive_requests_are_recorded_as_pending_intents_never_executed() {
        use shogun_memory::mail_triage::{ArchiveStatus, LabelFlags};
        let db = Db::open_in_memory(Arc::new(|| 1_000_000)).unwrap();
        seed_mail_thread(&db);
        let backend = DbBackend::new(db.clone());
        let body = r#"{"thread_key":"gmail:t-42"}"#;
        let status = || db.mail_triage_get("gmail:t-42").unwrap().archive_status;
        let label_key = db.mail_triage_get("gmail:t-42").unwrap().label_key;

        // Issue #779: without a standing approval an API archive needs the user's tap. The
        // backend classifies it for the faces' queue, and its direct write refuses it.
        assert_eq!(
            backend.mail_archive_change(Tool::MailTriageArchive, body),
            Ok(Some(shogun_mcp::mail_archive_api::MailArchiveRequest {
                thread_key: "gmail:t-42".into(),
                unarchive: false,
            }))
        );
        assert_eq!(
            backend.write(Tool::MailTriageArchive, body),
            Err(crate::daemon::MAIL_ARCHIVE_NEEDS_APPROVAL.to_string())
        );
        assert_eq!(status(), ArchiveStatus::None, "nothing recorded");

        // Under the label's standing Auto Archive approval it is recorded directly.
        db.mail_label_set_flags(&label_key, LabelFlags { auto_archive: Some(true), ..LabelFlags::default() })
            .unwrap();
        assert_eq!(backend.mail_archive_change(Tool::MailTriageArchive, body), Ok(None));
        assert_eq!(backend.write(Tool::MailTriageArchive, body), Ok(None));
        assert_eq!(status(), ArchiveStatus::Pending);
        // idempotent while pending; the inverse withdraws it (nothing reaches Gmail).
        assert_eq!(backend.write(Tool::MailTriageArchive, body), Ok(None));
        assert_eq!(backend.mail_archive_change(Tool::MailTriageUnarchive, body), Ok(None));
        assert_eq!(backend.write(Tool::MailTriageUnarchive, body), Ok(None));
        assert_eq!(status(), ArchiveStatus::None);
        assert!(backend
            .write(Tool::MailTriageUnarchive, body)
            .is_err_and(|e| e.starts_with("invalid unarchive request")));
        assert_eq!(
            backend.write(Tool::MailTriageArchive, r#"{"thread_key":"gmail:nope"}"#),
            Err("thread not found".to_string())
        );
        assert_eq!(
            backend.mail_archive_change(Tool::MailTriageArchive, r#"{"thread_key":"gmail:nope"}"#),
            Err("thread not found".to_string())
        );
        assert!(backend
            .write(Tool::MailTriageArchive, "{}")
            .is_err_and(|e| e.starts_with("expected thread_key")));

        // Archived on Gmail: no standing approval covers putting it back.
        db.mail_set_archive_status("gmail:t-42", ArchiveStatus::Done);
        assert_eq!(
            backend.mail_archive_change(Tool::MailTriageUnarchive, body),
            Ok(Some(shogun_mcp::mail_archive_api::MailArchiveRequest {
                thread_key: "gmail:t-42".into(),
                unarchive: true,
            }))
        );
        assert_eq!(
            backend.write(Tool::MailTriageUnarchive, body),
            Err(crate::daemon::MAIL_ARCHIVE_NEEDS_APPROVAL.to_string())
        );
        assert!(backend
            .write(Tool::MailTriageArchive, body)
            .is_err_and(|e| e.ends_with("already archived")));
        assert_eq!(status(), ArchiveStatus::Done);
        // The user's confirm of the queued unarchive records it, re-checked.
        let unarchive = shogun_agents::permission::SendAction::ArchiveMailThread {
            thread_key: "gmail:t-42".into(),
            unarchive: true,
        };
        assert_eq!(db.apply_confirmed_setting(&unarchive, None, None, None), Some(Ok(())));
        assert_eq!(status(), ArchiveStatus::UnarchivePending);
        db.mail_set_archive_status("gmail:t-42", ArchiveStatus::None);

        // Full stack over REST: the path carries the key, the response says pending (FR-API-04).
        use shogun_mcp::memory_api::TokenRegistry;
        use shogun_mcp::rest::{respond_with, Method, RestRequest};
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let req = RestRequest {
            method: Method::Post,
            path: "/v1/mail/triage/gmail:t-42/archive".into(),
            token: Some("t".into()),
            include_low: false,
            query: None,
            search_depth: None,
            search_source_kind: None,
            search_mode: None,
            search_bound_invalid: false,
            body: None,
            from_ms: None,
            to_ms: None,
            for_generation: false,
            app_bundle_id: None,
            cursor_ts: None,
            cursor_id: None,
            cursor_direction: None,
            page_size: None,
            person_id: None,
            project_id: None,
            mail: Default::default(),
        };
        let (status, body) = respond_with(
            &req,
            &tokens,
            &shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
            &backend,
        );
        assert_eq!(status, 202, "{body}");
        assert!(body.contains("\"pending\":true"), "{body}");
        assert!(body.contains("\"level\":\"L2\""), "{body}");
        assert_eq!(
            db.mail_triage_get("gmail:t-42").unwrap().archive_status,
            ArchiveStatus::Pending
        );
        // …and the get read hands the same key through the path.
        let (status, body) = respond_with(
            &RestRequest {
                method: Method::Get,
                path: "/v1/mail/triage/gmail:t-42".into(),
                ..req
            },
            &tokens,
            &shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
            &backend,
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"tool\":\"mail.triage.get\""), "{body}");
        assert!(body.contains("\"archive_status\":\"pending\""), "{body}");
    }

    #[test]
    fn a_thread_an_api_caller_moved_under_a_standing_label_still_needs_the_users_tap() {
        // Issue #779: relabelling first must not skip the per-thread confirm.
        use shogun_memory::mail_triage::{FeedbackOrigin, LabelFlags, UserChange};
        let db = Db::open_in_memory(Arc::new(|| 1_000_000)).unwrap();
        seed_mail_thread(&db);
        let backend = DbBackend::new(db.clone());
        let body = r#"{"thread_key":"gmail:t-42"}"#;
        db.mail_label_set_flags("marketing", LabelFlags { auto_archive: Some(true), ..LabelFlags::default() })
            .unwrap();
        assert_eq!(
            backend.write(
                Tool::MailTriageSet,
                r#"{"thread_key":"gmail:t-42","label_key":"marketing","bucket":"other"}"#
            ),
            Ok(None)
        );
        assert_eq!(db.mail_triage_get("gmail:t-42").unwrap().label_key, "marketing");
        assert!(backend.mail_archive_change(Tool::MailTriageArchive, body).is_ok_and(|r| r.is_some()));
        assert_eq!(
            backend.write(Tool::MailTriageArchive, body),
            Err(crate::daemon::MAIL_ARCHIVE_NEEDS_APPROVAL.to_string())
        );
        // Filed there by the user, the standing approval covers it.
        let back = UserChange { label_key: Some("news".into()), ..UserChange::default() };
        db.mail_triage_set("gmail:t-42", &back, FeedbackOrigin::Ui).unwrap();
        let to_marketing = UserChange { label_key: Some("marketing".into()), ..UserChange::default() };
        db.mail_triage_set("gmail:t-42", &to_marketing, FeedbackOrigin::Ui).unwrap();
        assert_eq!(backend.mail_archive_change(Tool::MailTriageArchive, body), Ok(None));
        assert_eq!(backend.write(Tool::MailTriageArchive, body), Ok(None));
    }

    #[test]
    fn mail_label_upsert_guards_standing_approval_and_builtin_edits() {
        use shogun_mcp::memory_api::STANDING_APPROVAL_TEXT;
        let db = Db::open_in_memory(Arc::new(|| 1_000_000)).unwrap();
        let backend = DbBackend::new(db.clone());
        let label = |key: &str| {
            db.mail_labels()
                .into_iter()
                .find(|l| l.key == key)
                .expect("label")
        };

        // Issue #779: an API write never grants a standing approval, with or without
        // confirm_standing — only the user does, in Settings.
        for body in [
            r#"{"key":"marketing","auto_archive":true}"#,
            r#"{"key":"marketing","sync_to_gmail":true,"confirm_standing":false}"#,
            r#"{"key":"marketing","auto_archive":true,"confirm_standing":true}"#,
            r#"{"key":"marketing","sync_to_gmail":true,"confirm_standing":true}"#,
            r#"{"name":"Vendors","definition":"Invoices","default_bucket":"other","auto_archive":true,"confirm_standing":true}"#,
        ] {
            assert_eq!(
                backend.write(Tool::MailLabelsUpsert, body),
                Err(STANDING_APPROVAL_TEXT.to_string()),
                "{body}"
            );
        }
        assert!(!label("marketing").auto_archive);
        assert!(!label("marketing").sync_to_gmail);
        assert!(!db.mail_labels().iter().any(|l| l.key == "custom:vendors"), "refused whole");
        assert!(backend
            .write(Tool::MailLabelsUpsert, r#"{"key":"marketing","confirm_standing":"yes"}"#)
            .is_err_and(|e| e.starts_with("expected ")));
        // Granted by the user in Settings, a re-sent `true` changes nothing and is allowed;
        // turning it off never needs confirmation.
        db.mail_label_set_flags(
            "marketing",
            shogun_memory::mail_triage::LabelFlags { auto_archive: Some(true), ..Default::default() },
        )
        .unwrap();
        assert_eq!(
            backend.write(Tool::MailLabelsUpsert, r#"{"key":"marketing","auto_archive":true}"#),
            Ok(None)
        );
        assert!(label("marketing").auto_archive);
        assert_eq!(
            backend.write(Tool::MailLabelsUpsert, r#"{"key":"marketing","auto_archive":false}"#),
            Ok(None)
        );
        assert!(!label("marketing").auto_archive);
        // Built-ins: flags only.
        assert!(backend
            .write(Tool::MailLabelsUpsert, r#"{"key":"marketing","name":"Promotions"}"#)
            .is_err_and(|e| e.starts_with("invalid label change")));
        assert_eq!(label("marketing").name, "Marketing");
        assert!(backend
            .write(Tool::MailLabelsUpsert, r#"{"key":"marketing"}"#)
            .is_err_and(|e| e.starts_with("expected ")));

        // A custom label: key derived from the name; edits keep unspecified fields.
        assert_eq!(
            backend.write(
                Tool::MailLabelsUpsert,
                r#"{"name":"Vendors","definition":"Invoices from suppliers","default_bucket":"other"}"#
            ),
            Ok(None)
        );
        let vendors = label("custom:vendors");
        assert_eq!(vendors.kind, shogun_memory::mail_triage::LabelKind::Custom);
        assert_eq!(vendors.definition.as_deref(), Some("Invoices from suppliers"));
        assert_eq!(
            backend.write(
                Tool::MailLabelsUpsert,
                r#"{"key":"custom:vendors","definition":"Invoices and receipts"}"#
            ),
            Ok(None)
        );
        let vendors = label("custom:vendors");
        assert_eq!(vendors.name, "Vendors");
        assert_eq!(vendors.definition.as_deref(), Some("Invoices and receipts"));
        assert!(backend
            .write(Tool::MailLabelsUpsert, r#"{"name":"Nope"}"#)
            .is_err_and(|e| e == "expected definition"));
        let listed = structured(&backend, Tool::MailLabelsList, &params());
        assert!(
            listed["labels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["key"] == "custom:vendors"),
            "{listed}"
        );

        // Delete: custom only.
        assert!(backend
            .write(Tool::MailLabelsDelete, r#"{"key":"marketing"}"#)
            .is_err_and(|e| e.starts_with("invalid label delete")));
        assert_eq!(
            backend.write(Tool::MailLabelsDelete, r#"{"key":"custom:vendors"}"#),
            Ok(None)
        );
        assert_eq!(
            backend.write(Tool::MailLabelsDelete, r#"{"key":"custom:vendors"}"#),
            Err("label not found".to_string())
        );
    }

    #[test]
    fn mail_sender_rules_add_list_and_delete_through_the_api_face() {
        let db = Db::open_in_memory(Arc::new(|| 1_000_000)).unwrap();
        let backend = DbBackend::new(db);
        let id = match backend.write(
            Tool::MailSenderRulesAdd,
            r#"{"match_kind":"domain","match_value":"News.Example.com","label_key":"news"}"#,
        ) {
            Ok(Some(id)) => id,
            other => panic!("expected a rule id, got {other:?}"),
        };
        let rules = structured(&backend, Tool::MailSenderRulesList, &params());
        assert_eq!(rules["rules"][0]["id"], id, "{rules}");
        assert_eq!(rules["rules"][0]["match_value"], "news.example.com");
        assert_eq!(rules["rules"][0]["label_key"], "news");
        assert_eq!(rules["rules"][0]["source"], "user");
        assert_eq!(
            backend.write(
                Tool::MailSenderRulesAdd,
                r#"{"match_kind":"addr","match_value":"x@y.z","label_key":"nope"}"#
            ),
            Err("label not found".to_string())
        );
        assert!(backend
            .write(
                Tool::MailSenderRulesAdd,
                r#"{"match_kind":"regex","match_value":"x","label_key":"news"}"#
            )
            .is_err_and(|e| e.starts_with("invalid match_kind")));
        assert_eq!(
            backend.write(Tool::MailSenderRulesDelete, &format!(r#"{{"id":{id}}}"#)),
            Ok(Some(id))
        );
        assert_eq!(
            backend.write(Tool::MailSenderRulesDelete, &format!(r#"{{"id":{id}}}"#)),
            Err("sender rule not found".to_string())
        );
        assert_eq!(
            structured(&backend, Tool::MailSenderRulesList, &params())["rules"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn mail_history_lists_the_write_log_newest_first() {
        use shogun_memory::mail_triage::WriteApproval;
        let db = Db::open_in_memory(Arc::new(|| 1_000_000)).unwrap();
        seed_mail_thread(&db);
        db.mail_write_log(
            "gmail:t-42",
            "archive_thread",
            &[],
            &["INBOX".to_string()],
            WriteApproval::L2Standing,
            true,
            None,
        );
        db.mail_write_log(
            "gmail:t-42",
            "unarchive_thread",
            &["INBOX".to_string()],
            &[],
            WriteApproval::L2,
            true,
            None,
        );
        let backend = DbBackend::new(db);
        let history = structured(&backend, Tool::MailHistoryList, &params());
        assert_eq!(history["history"].as_array().map(Vec::len), Some(2), "{history}");
        assert_eq!(history["history"][0]["action_kind"], "unarchive_thread");
        assert_eq!(history["history"][0]["approval"], "l2");
        assert_eq!(history["history"][1]["approval"], "l2_standing");
        let one = structured(
            &backend,
            Tool::MailHistoryList,
            &ReadParams {
                mail: shogun_mcp::backend::MailListQuery {
                    limit: Some(1),
                    ..Default::default()
                },
                ..ReadParams::default()
            },
        );
        assert_eq!(one["history"].as_array().map(Vec::len), Some(1));
    }

    fn quick_wait() -> TranslateWait {
        TranslateWait {
            total: std::time::Duration::from_millis(1_500),
            claim_grace: std::time::Duration::from_millis(150),
            poll: std::time::Duration::from_millis(5),
        }
    }

    struct Answer(&'static str);
    impl crate::llm::AgentClient for Answer {
        fn complete(&self, _prompt: &str) -> Result<String, crate::llm::LlmError> {
            Ok(self.0.to_string())
        }
    }

    /// A stand-in for the desktop runner: claim the next request and run it on `agent`.
    fn desktop(db: Db, agent: Answer, delay: std::time::Duration) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            for _ in 0..400 {
                if let Some(job) = db.translate_job_claim_next() {
                    std::thread::sleep(delay);
                    let settings = crate::translate_job::JobSettings {
                        pair: crate::translate::LanguagePair::from_locale("ja"),
                        ..Default::default()
                    };
                    db.translate_job_execute(&job, true, &settings, Some(&agent));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    }

    fn translate_op(kind: &'static str, body: &str) -> shogun_mcp::translate_api::TranslateOperation {
        shogun_mcp::translate_api::TranslateOperation::Request { kind, body: body.into(), origin: "mcp" }
    }

    #[test]
    fn a_translate_request_waits_for_the_desktop_and_leaves_no_row() {
        // Issue #279: the face queues, the desktop runs it on the user's model access, the face
        // hands the answer over and nothing stays behind (FR-TL-20).
        let db = Db::open_in_memory(Arc::new(|| 50_000)).unwrap();
        let backend = DbBackend::new(db.clone()).with_translate_wait(quick_wait());
        let runner = desktop(db.clone(), Answer("こんにちは"), std::time::Duration::ZERO);
        let out = backend.translate(translate_op("translate", r#"{"text":"hello"}"#)).unwrap();
        runner.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "done", "{out}");
        assert_eq!(v["result"]["text"], "こんにちは");
        assert_eq!(v["result"]["target"], "ja");
        let id = v["job_id"].as_i64().unwrap();
        assert!(db.translate_job_take(id).unwrap().is_none(), "handed over, then gone");
    }

    #[test]
    fn a_slow_request_hands_back_a_job_id_and_status_reads_it_once() {
        let db = Db::open_in_memory(Arc::new(|| 50_000)).unwrap();
        let wait = TranslateWait { total: std::time::Duration::from_millis(60), ..quick_wait() };
        let backend = DbBackend::new(db.clone()).with_translate_wait(wait);
        let raw = "###DRAFT###\nThursday works.\n###BACK###\n木曜日で大丈夫です。";
        let runner = desktop(db.clone(), Answer(raw), std::time::Duration::from_millis(300));
        let out = backend
            .translate(translate_op("reply_draft", r#"{"incoming":"Can we meet on Thursday?"}"#))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "running", "{out}");
        assert_eq!(v["poll"], "translate.status");
        let id = v["job_id"].as_i64().unwrap();
        runner.join().unwrap();
        let status = shogun_mcp::translate_api::TranslateOperation::Status { id };
        let done: serde_json::Value = serde_json::from_str(&backend.translate(status.clone()).unwrap()).unwrap();
        assert_eq!(done["result"]["draft"], "Thursday works.");
        assert_eq!(done["result"]["sent"], false);
        let again: serde_json::Value = serde_json::from_str(&backend.translate(status).unwrap()).unwrap();
        assert_eq!(again["error"], "not_found", "a result is handed over once");
    }

    #[test]
    fn no_desktop_means_app_not_running_and_a_bad_body_never_becomes_a_row() {
        let db = Db::open_in_memory(Arc::new(|| 50_000)).unwrap();
        let backend = DbBackend::new(db.clone()).with_translate_wait(quick_wait());
        let out = backend.translate(translate_op("translate", r#"{"text":"hello"}"#)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"], "app_not_running", "{out}");
        assert!(db.translate_job_claim_next().is_none(), "the unclaimed request was discarded");

        let out = backend.translate(translate_op("translate", r#"{"text":"hi","target":"fr"}"#)).unwrap();
        assert!(out.contains("unknown_language"), "{out}");
        assert!(db.translate_job_claim_next().is_none());
    }

    /// Expands to "postpone release" and ranks the candidate that mentions Orchid.
    struct OrchidRanker;

    impl crate::ai_search::SearchModel for OrchidRanker {
        fn complete(
            &self,
            step: crate::ai_search::ModelStep,
            _system: &str,
            user: &str,
            _deadline: std::time::Instant,
        ) -> Result<String, crate::llm::LlmError> {
            Ok(match step {
                crate::ai_search::ModelStep::Expand => r#"{"queries": ["postpone release"]}"#.to_string(),
                crate::ai_search::ModelStep::Rerank => {
                    let id = user
                        .lines()
                        .find(|l| l.contains("Orchid"))
                        .and_then(|l| Some(l[1..l.find(']')?].to_string()))
                        .unwrap_or_default();
                    format!(r#"{{"ranked": ["{id}"]}}"#)
                }
            })
        }
    }

    fn ai_search_db() -> Db {
        let now = 1_791_201_600_000;
        let db = Db::open_in_memory(Arc::new(move || now)).unwrap();
        for (i, content) in ["Launch party catering confirmed", "Agreed: postpone the Orchid release to November"]
            .iter()
            .enumerate()
        {
            let hash = format!("ai-search-backend-{i}");
            db.capture(&shogun_memory::event_log::NewEvent {
                ts: now - (i as i64 + 1) * 86_400_000,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.Notes"),
                window_title: Some("Notes"),
                content,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap();
        }
        db
    }

    #[test]
    fn an_ai_search_waits_for_the_desktop_and_leaves_no_row() {
        // #940 B2b: the translate flow, with the desktop running the AI step.
        let db = ai_search_db();
        let backend = DbBackend::new(db.clone()).with_ai_search_wait(quick_wait());
        let desktop = {
            let db = db.clone();
            std::thread::spawn(move || {
                for _ in 0..400 {
                    if let Some(job) = db.ai_search_job_claim_next() {
                        let ent = shogun_agents::entitlement::entitlements(
                            shogun_agents::entitlement::Plan::Pro,
                            0,
                        );
                        let _ = db.ai_search_job_execute(
                            &job,
                            &ent,
                            &crate::capture::exclusion::ExclusionPolicy::new(),
                            Some(&OrchidRanker),
                        );
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };
        let out = backend.ai_search("when did we decide to delay the launch", "mcp", Ok(())).unwrap();
        desktop.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "ai_reranked", "{out}");
        assert_eq!(v["results"][0]["ai_rank"], 1);
        assert!(v["results"][0]["excerpt"].as_str().unwrap().contains("Orchid"));
        assert!(db.ai_search_job_claim_next().is_none(), "handed over, then gone");
    }

    #[test]
    fn no_desktop_answers_keyword_results_as_app_not_running() {
        let db = ai_search_db();
        let backend = DbBackend::new(db.clone()).with_ai_search_wait(quick_wait());
        let out = backend.ai_search("Orchid release", "api", Ok(())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!((v["status"].as_str(), v["reason"].as_str()), (Some("keyword_fallback"), Some("app_not_running")), "{out}");
        assert!(v["results"].as_array().is_some_and(|r| !r.is_empty()), "the caller still gets results");
        assert!(db.ai_search_job_claim_next().is_none(), "the unclaimed request was discarded");
    }

    #[test]
    fn a_caller_that_may_not_spend_model_access_gets_keyword_results_without_a_queue() {
        let db = ai_search_db();
        let backend = DbBackend::new(db.clone()).with_ai_search_wait(quick_wait());
        for (gate, reason) in [
            (shogun_mcp::ai_search_api::READ_ONLY_TOKEN, "read_only_token"),
            (shogun_mcp::ai_search_api::AI_NOT_IN_PLAN, "ai_not_in_plan"),
        ] {
            let started = std::time::Instant::now();
            let out = backend.ai_search("Orchid release", "mcp", Err(gate)).unwrap();
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["reason"], reason, "{out}");
            assert_eq!(v["model_calls"], 0);
            assert!(started.elapsed() < std::time::Duration::from_millis(100), "no wait");
            assert!(db.ai_search_job_claim_next().is_none(), "nothing was queued");
        }
        let empty = backend.ai_search("   ", "mcp", Ok(())).unwrap();
        assert!(empty.contains("empty_query"), "{empty}");
    }

    #[test]
    fn ai_search_answers_alike_on_cli_rest_and_mcp_and_honours_the_token_scope() {
        use shogun_agents::approval::ApprovalQueue;
        use shogun_mcp::mcp::McpServer;
        use shogun_mcp::memory_api::{TokenRegistry, TokenScope};
        use shogun_mcp::rest::{respond_with, Method, RestRequest};

        let db = ai_search_db();
        let pro = || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let mut tokens = TokenRegistry::new();
        tokens.issue_scoped("full", TokenScope::Full);
        tokens.issue_scoped("read", TokenScope::Read);
        let call = shogun_cli::wire::to_call(
            &shogun_cli::parse::parse(&["search".into(), "--ai".into(), "Orchid".into(), "release".into()])
                .unwrap()
                .command,
            false,
        )
        .unwrap();
        let (path, query) = call.path.split_once('?').unwrap();
        let rest = |token: &str| {
            let mut request = RestRequest {
                method: Method::Get,
                path: path.to_string(),
                token: Some(token.to_string()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: None,
                from_ms: None,
                to_ms: None,
                cursor_ts: None,
                cursor_id: None,
                cursor_direction: None,
                page_size: None,
                for_generation: false,
                app_bundle_id: None,
                person_id: None,
                project_id: None,
                mail: Default::default(),
            };
            for pair in query.split('&') {
                match pair.split_once('=').unwrap() {
                    ("q", value) => request.query = Some(value.replace("%20", " ")),
                    ("mode", value) => request.search_mode = Some(value.to_string()),
                    (other, _) => panic!("unexpected control {other}"),
                }
            }
            let backend = DbBackend::new(db.clone()).with_ai_search_wait(quick_wait());
            let (status, body) = respond_with(&request, &tokens, &pro(), &backend);
            assert_eq!(status, 200, "{body}");
            serde_json::from_str::<serde_json::Value>(&body).unwrap()
        };
        // No desktop: keyword results, labelled.
        let full = rest("full");
        assert_eq!((full["status"].as_str(), full["reason"].as_str()), (Some("keyword_fallback"), Some("app_not_running")));
        assert_eq!(full["mode"], "ai");
        assert!(full["results"].as_array().is_some_and(|r| !r.is_empty()));
        // A read-only token never spends the user's model access, and is told why.
        assert_eq!(rest("read")["reason"], "read_only_token");

        let server = McpServer::new(
            DbBackend::new(db.clone()).with_ai_search_wait(quick_wait()),
            Arc::new(std::sync::Mutex::new(ApprovalQueue::new())),
            || 0,
            pro,
        );
        let line = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "memory.search", "arguments": { "query": "Orchid release", "mode": "ai" } },
        })
        .to_string();
        let response: serde_json::Value = serde_json::from_str(&server.handle_line(&line).unwrap()).unwrap();
        let mcp: serde_json::Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let strip = |mut v: serde_json::Value| {
            v.as_object_mut().unwrap().remove("elapsed_ms");
            v
        };
        assert_eq!(strip(mcp), strip(full), "the same answer on every face");

        // AI mode refuses the keyword-only archive controls instead of ignoring them.
        let bad = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "memory.search", "arguments": { "query": "Orchid", "mode": "ai", "depth": "all" } },
        })
        .to_string();
        assert!(server.handle_line(&bad).unwrap().contains("ai_search_options_unsupported"));
        let unknown = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "memory.search", "arguments": { "query": "Orchid", "mode": "smart" } },
        })
        .to_string();
        assert!(server.handle_line(&unknown).unwrap().contains("invalid_search_mode"));
    }
}
