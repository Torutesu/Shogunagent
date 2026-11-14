//! The daemon-side wiring skeleton: [`ConnectorRuntime`] ties the connection state
//! ([`shogun_mcp::connection`]), the gate ([`shogun_mcp::service_gate`]), the sync ingest
//! ([`shogun_mcp::sync`]), and the transport ([`crate::transport`]) into the two things the daemon
//! actually drives: **poll a read-sync** and **execute a confirmed write**.
//!
//! Data gravity stays in the core (invariant 1): the runtime never touches the DB. It hands synced
//! items to an [`IngestSink`] the daemon implements over its `Db` (mirrors the `MemoryBackend`
//! seam). The 15-minute scheduler (FR-INT-04) is the daemon's tokio loop calling [`ConnectorRuntime::poll_tick`];
//! this crate stays runtime-free and Linux-testable.
//!
//! Policy is never re-decided ad hoc: reads go through [`shogun_mcp::sync::collect_sync`] (which
//! calls the gate) and writes re-check [`shogun_mcp::service_gate::authorize_op`] before running
//! (the WP-F "double gate").

use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use shogun_mcp::connection::{ConnEvent, ConnState, ConnectionRegistry, DisconnectOutcome};
use shogun_mcp::scope::{Service, Wave, ALL_SERVICES};
use shogun_mcp::service_gate::{authorize_op, DenyReason, OpContext, OpDecision};
use shogun_mcp::sync::{
    authorize_on_demand, authorize_sync, collect_on_demand, collect_sync_page, IngestItem,
    IntegrationTransport, SyncCursor, SyncFailure,
};

use crate::transport::WriteExecutor;

/// The default first-layer poll cadence (FR-INT-04: 15-minute read-sync).
pub const DEFAULT_SYNC_INTERVAL_MS: i64 = 15 * 60 * 1000;
// Process-unique: replacing the whole runtime must not recreate an old authorization identity.
static NEXT_TOOL_AUTH_REVISION: AtomicU64 = AtomicU64::new(1);

/// Persists synced items into the event log. Implemented by the daemon over its `Db`
/// (`Db::ingest_integration`); returns the count of *newly inserted* items (what an
/// `IntegrationSynced` bus event reports).
pub trait IngestSink {
    fn ingest(&self, items: &[IngestItem]) -> usize;

    /// Ingest and say how many items the store refused (a write error, a lock it could not take).
    /// The runtime does not advance a service's cursor past a refused item (issue #781), so a
    /// sink that can fail should override this. The default reports no refusals.
    fn ingest_counted(&self, items: &[IngestItem]) -> IngestCount {
        IngestCount { inserted: self.ingest(items), rejected: 0 }
    }

    /// The stored incremental-sync cursor for a service (FR-TG-09). `None` = never synced (the
    /// transport then applies its default backfill window).
    fn cursor(&self, service: Service) -> Option<SyncCursor> {
        let _ = service;
        None
    }

    /// Persist the cursor after a completed tick. The default drops it (a sink that does not
    /// persist cursors simply re-fetches the recent window each tick).
    fn store_cursor(&self, service: Service, cursor: &SyncCursor) {
        let _ = (service, cursor);
    }

    /// Called once per fetch request, after the gate allowed it and **before** the request is
    /// sent (issue #782). A Gmail read sends the Composio user id and the query to a third party
    /// whether or not the answer comes back, so the traceability row belongs to the attempt, not
    /// to the success — the same boundary the LLM, ASR and tool-loop paths record at. The daemon
    /// writes a content-free row here; the default records nothing (tests, offline sinks).
    fn record_fetch_attempt(&self, service: Service, kind: FetchKind) {
        let _ = (service, kind);
    }
}

/// Which read a [`IngestSink::record_fetch_attempt`] call is about to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchKind {
    /// One page of the background read-sync (FR-INT-04 / FR-TG-09).
    SyncPage,
    /// A targeted on-demand fetch (§6.9 `read_on_demand`).
    OnDemand,
}

/// Re-reads whether the user's Composio (Gmail) consent still holds, without the runtime lock.
///
/// The runtime's own copy of the consent ([`ConnectorRuntime::set_composio_consent`]) is refreshed
/// by whoever holds the lock: the poller before a tick, Settings after it saved a change. A
/// revocation that lands while a sync is walking pages, or while the post-sync write-back runs,
/// cannot take that lock until the walk ends, so the runtime would keep fetching and writing under
/// a consent the user already withdrew (issue #781). The probe is asked again before every Gmail
/// page, after every Gmail page arrives, and before every Gmail op, so a revocation takes effect
/// at the next page boundary instead. `false` must mean "not known to be granted" (fail closed).
pub type ConsentProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// A [`ConsentProbe`] over the persisted policy (`composio.json`): the file Settings and the API
/// faces' `connections.composio.set` both write under its lock before they touch the runtime.
/// Reading it is what makes a revocation from either face visible to a sync already in flight.
/// A missing or unreadable file reads as no consent ([`shogun_mcp::composio_policy::load_effective`]).
pub fn persisted_consent_probe(path: std::path::PathBuf) -> ConsentProbe {
    Arc::new(move || shogun_mcp::composio_policy::load_effective(&path).consent_acknowledged)
}

/// Max items one read-sync tick ingests across pages (FR-TG-09) — bounds a first backfill.
pub const MAX_SYNC_ITEMS_PER_TICK: usize = 200;
/// Max pages one tick walks (a guard against a provider that keeps returning tokens).
const MAX_SYNC_PAGES_PER_TICK: usize = 20;
/// The first-sync backfill window when no cursor exists (7 days, FR-TG-09).
pub const DEFAULT_BACKFILL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// What one [`IngestSink::ingest_counted`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngestCount {
    /// Items new to memory.
    pub inserted: usize,
    /// Items the store refused; the tick keeps its starting cursor so they are fetched again.
    pub rejected: usize,
}

/// The result of one read-sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub fetched: usize,
    pub inserted: usize,
}

/// A service's connection state as the UI shows it (serializable for the Tauri command).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnUi {
    Connected,
    /// Amber — token expired/revoked; cached reads still serve, writes are blocked.
    NeedsReauth,
    /// Stored grant is unreadable; Retry must load it before any connector work.
    KeychainLocked,
    Disconnected,
    /// The service's wave is not rolled out yet (FR-INT-03) — shown as "Coming soon".
    ComingSoon,
}

impl ConnUi {
    /// The wire id: exactly what serde writes for the UI, and what `connections.list` reports.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnUi::Connected => "connected",
            ConnUi::NeedsReauth => "needs_reauth",
            ConnUi::KeychainLocked => "keychain_locked",
            ConnUi::Disconnected => "disconnected",
            ConnUi::ComingSoon => "coming_soon",
        }
    }
}

/// Publish the Connections screen's rows for the API faces' `connections.list` (issue #82):
/// service state only, stamped `now_ms` (unix ms). No account identities or tokens.
pub fn publish_statuses(
    path: &std::path::Path,
    now_ms: u64,
    rows: &[ServiceStatus],
) -> Result<(), String> {
    let pairs: Vec<_> = rows.iter().map(|row| (row.service, row.state.as_str())).collect();
    shogun_mcp::connections_api::write_status(path, now_ms, &pairs)
}

/// One row for the connections screen.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServiceStatus {
    /// The service (Rust-side; not serialized — the UI keys off `source`).
    #[serde(skip)]
    pub service: Service,
    /// Stable id the UI uses (`gmail` / `gcal` / `gdrive` / …).
    pub source: &'static str,
    pub state: ConnUi,
    /// Absolute unix-ms of the last successful sync, if any.
    pub last_sync_ms: Option<i64>,
    /// Whether a wired connect path exists for this service today: a first-layer official MCP
    /// endpoint, or the Composio relay for Gmail (issue #80 §2-A0 — Gmail has no first-layer
    /// endpoint but is always connectable through Composio). The UI's Connect button keys off
    /// this; wave release / env opt-in gating is applied separately by the caller.
    pub has_endpoint: bool,
    /// The access badge (issue #82): `read` / `read_draft` / `read_write`, the ceiling the scope
    /// table allows ([`shogun_mcp::scope::access`]). Derived here so the UI never keeps its own
    /// copy of what a connection may do.
    pub access: &'static str,
    #[serde(default)]
    pub account_label: Option<String>,
}

/// Owns per-service connection state and drives sync/write over a transport. Generic over the
/// transport so the daemon uses the live [`crate::RemoteMcpTransport`] and tests use a fake.
pub struct ConnectorRuntime<T> {
    // Keep unreadable grants separate from the registry's last known grant state.
    keychain_locked: std::collections::HashSet<Service>,
    focus_retry_used: bool,
    auth_state_changed: Option<Arc<dyn Fn() + Send + Sync>>,
    tool_auth_revision: u64,
    connection_revisions: std::collections::HashMap<Service, u64>,
    sync_revisions: std::collections::HashMap<Service, u64>,
    instance_revision: u64,
    transport: Arc<T>,
    registry: ConnectionRegistry,
    account_labels: std::collections::HashMap<Service, String>,
    highest_released: Wave,
    draft_stop: bool,
    /// Whether the Composio three-disclosure consent is acknowledged (FR-C2-02). Gates every
    /// Gmail operation, reads included (issue #80 §2-A0). Starts false — consent is always an
    /// explicit user act — and the desktop layer refreshes it from the persisted policy via
    /// [`ConnectorRuntime::set_composio_consent`] before each sync tick / write, like the plan.
    composio_consent: bool,
    gmail_direct: bool,
    /// Re-reads the persisted consent between pages and before each Gmail op (issue #781). `None`
    /// (tests, a runtime nobody attached one to) trusts `composio_consent` alone.
    consent_probe: Option<ConsentProbe>,
    /// Saved Web fetch consent; Web has no OAuth registry entry.
    web_consent: bool,
    /// The plan entitlements the gate applies (issue #97). Starts at the documented default
    /// (trial-not-started = paid capabilities blocked until a stamp/billing state is known); the desktop layer
    /// refreshes it via [`ConnectorRuntime::set_plan`] before each sync tick / write.
    plan: shogun_agents::entitlement::Entitlements,
}

impl<T> ConnectorRuntime<T> {
    pub fn new(transport: T, highest_released: Wave, draft_stop: bool) -> Self {
        let instance_revision = NEXT_TOOL_AUTH_REVISION.fetch_add(1, Ordering::Relaxed);
        Self {
            keychain_locked: std::collections::HashSet::new(),
            focus_retry_used: false,
            auth_state_changed: None,
            tool_auth_revision: instance_revision,
            instance_revision,
            connection_revisions: std::collections::HashMap::new(),
            sync_revisions: std::collections::HashMap::new(),
            transport: Arc::new(transport),
            registry: ConnectionRegistry::new(),
            account_labels: std::collections::HashMap::new(),
            highest_released,
            draft_stop,
            composio_consent: false,
            gmail_direct: false,
            consent_probe: None,
            web_consent: false,
            plan: shogun_agents::entitlement::Entitlements::trial_not_started(),
        }
    }

    /// Update only alongside a committed grant, so a failed reconnect keeps the prior identity.
    pub fn set_account_label(&mut self, service: Service, label: Option<String>) {
        self.account_labels.remove(&service);
        if let Some(label) = label {
            self.account_labels.insert(service, label);
        }
    }

    pub fn registry(&self) -> &ConnectionRegistry {
        &self.registry
    }

    /// Notify the desktop after an established connection needs sign-in again.
    pub fn set_auth_state_changed(&mut self, notify: Arc<dyn Fn() + Send + Sync>) {
        self.auth_state_changed = Some(notify);
    }

    /// Observe a provider failure without deleting or rewriting the stored grant.
    pub fn observe_provider_failure(&mut self, service: Service, reason: &str) {
        let before = self.registry.state(service);
        if before != ConnState::Disconnected && is_auth_failure(reason) {
            self.mark_token_expired(service);
            if self.registry.state(service) != before {
                if let Some(notify) = &self.auth_state_changed {
                    notify();
                }
            }
        }
    }

    /// Normalize MCP tool errors before callers can mistake them for successful writes.
    pub fn observe_provider_result(&mut self, service: Service, result: Result<Value, String>) -> Result<Value, String> {
        let result = result.and_then(|value| match crate::result::tool_error_reason(&value) {
            Some(reason) => Err(reason.to_string()),
            None => Ok(value),
        });
        if let Err(reason) = &result {
            self.observe_provider_failure(service, reason);
        }
        result
    }

    /// Identity of the current tool authorization. Sync freshness does not change it.
    pub fn tool_auth_revision(&self) -> u64 {
        self.tool_auth_revision
    }

    /// Invalidate pending tool conversations when credentials or policy change.
    fn invalidate_tools(&mut self) {
        self.tool_auth_revision = NEXT_TOOL_AUTH_REVISION.fetch_add(1, Ordering::Relaxed);
    }

    /// Cheap clone of the transport handle so a tool-loop read can drop the runtime lock
    /// before the HTTP round-trip (FR-HN-07 parallel reads).
    pub fn transport_handle(&self) -> Arc<T> {
        Arc::clone(&self.transport)
    }

    /// Connection snapshot the tool catalog consumes. Gmail without the Composio
    /// three-disclosure consent is folded to [`ConnState::Disconnected`] here
    /// (未同意 = 未接続扱い) so every caller does not re-state that rule.
    pub fn tool_loop_view(
        &self,
    ) -> (
        Vec<shogun_mcp::tool_catalog::ServiceState>,
        shogun_mcp::tool_catalog::ToolContext,
    ) {
        let services = ALL_SERVICES
            .iter()
            .copied()
            .map(|service| {
                let conn = if self.keychain_locked.contains(&service)
                    || (service == Service::Gmail && !self.composio_consent) {
                    ConnState::Disconnected
                } else if service == Service::Web {
                    if self.web_consent {
                        ConnState::Connected { last_sync_ms: 0 }
                    } else {
                        ConnState::Disconnected
                    }
                } else {
                    self.registry.state(service)
                };
                shogun_mcp::tool_catalog::ServiceState { service, conn }
            })
            .collect();
        let ctx = shogun_mcp::tool_catalog::ToolContext {
            highest_released: self.highest_released,
            draft_stop: self.draft_stop,
            plan: self.plan,
        };
        (services, ctx)
    }

    /// Fence an interactive connection against disconnects, reconnects and runtime replacement.
    pub fn connection_revision(&self, service: Service) -> u64 {
        self.connection_revisions
            .get(&service)
            .copied()
            .unwrap_or(self.instance_revision)
    }

    fn invalidate_connection(&mut self, service: Service) {
        self.connection_revisions.insert(
            service,
            NEXT_TOOL_AUTH_REVISION.fetch_add(1, Ordering::Relaxed),
        );
    }

    /// Refresh the direct Gmail draft-only setting. Legacy policy changes do not affect it.
    pub fn set_gmail_draft_stop(&mut self, on: bool) {
        if self.gmail_direct {
            self.set_draft_stop(on);
        }
    }

    /// Set the global Gmail draft-stop flag (§6.10) — flows into the gate context.
    pub fn set_draft_stop(&mut self, on: bool) {
        if self.draft_stop != on {
            self.invalidate_tools();
        }
        self.draft_stop = on;
    }

    /// Refresh the Composio three-disclosure consent state (FR-C2-02), held in `composio_consent`.
    ///
    /// Without consent Gmail is not connected (issue #781). Revoking from Settings or from an API
    /// face clears the durable connected flag, so after a relaunch Gmail is "Not connected" and
    /// only Connect brings it back. The in-memory registry has to agree, or the Connections screen
    /// and `connections.list` claim a connection the consent no longer covers, and granting consent
    /// again would quietly resume syncing for this session only. So consent off disconnects Gmail
    /// here, whichever path observed it, and granting it again never reconnects: Connect does.
    /// Direct Gmail consent is its provider OAuth grant, not legacy Composio settings.
    /// Old Composio commands must never disconnect or reconnect a direct Gmail account.
    pub fn use_direct_gmail(&mut self) {
        self.gmail_direct = true;
        self.composio_consent = true;
        self.consent_probe = None;
        self.invalidate_tools();
    }

    pub fn set_composio_consent(&mut self, acknowledged: bool) {
        if self.gmail_direct {
            return;
        }
        if self.composio_consent != acknowledged {
            self.invalidate_tools();
        }
        self.composio_consent = acknowledged;
        if !acknowledged && self.registry.state(Service::Gmail) != ConnState::Disconnected {
            self.disconnect(Service::Gmail, false);
        }
    }

    /// Attach the lock-free consent re-check (issue #781). The desktop attaches
    /// [`persisted_consent_probe`] over its `composio.json` when it builds the runtime.
    pub fn set_consent_probe(&mut self, probe: ConsentProbe) {
        if !self.gmail_direct {
            self.consent_probe = Some(probe);
        }
    }

    /// Whether Gmail may cross the Composio boundary right now: the runtime's refreshed copy of the
    /// consent and, when a probe is attached, the persisted one as it is at this moment.
    fn gmail_consent_live(&self) -> bool {
        if !self.composio_consent {
            return false;
        }
        match &self.consent_probe {
            Some(probe) => probe(),
            None => true,
        }
    }

    /// The probe saw a revocation the runtime's copy had not caught up with yet (a Gmail page
    /// arrived after the user revoked). Apply it now, exactly as the next refresh would: consent
    /// off, Gmail disconnected. A runtime that already held no consent changes nothing.
    fn observe_revocation(&mut self, service: Service) {
        if service == Service::Gmail && self.composio_consent {
            self.set_composio_consent(false);
        }
    }

    /// Web has no OAuth connection: its tool availability follows the saved fetch consent.
    pub fn set_web_consent(&mut self, acknowledged: bool) {
        if self.web_consent != acknowledged {
            self.invalidate_tools();
        }
        self.web_consent = acknowledged;
    }

    /// Refresh the plan entitlements the gate applies (issue #97). The desktop layer resolves the
    /// plan (onboarding trial stamp + billing) and pushes it here — the runtime never reads the
    /// clock or the plan sources itself.
    pub fn set_plan(&mut self, plan: shogun_agents::entitlement::Entitlements) {
        if self.plan != plan {
            self.invalidate_tools();
        }
        self.plan = plan;
    }

    /// Record a completed OAuth connection for a service (drives it out of `Disconnected`).
    pub fn mark_connected(&mut self, service: Service, now_ms: i64) {
        self.invalidate_connection(service);
        self.invalidate_tools();
        self.keychain_locked.remove(&service);
        self.registry.apply(service, ConnEvent::Connected { ts: now_ms });
    }

    /// Rehydrate a service the app already holds durable credentials for (FR-INT-03).
    ///
    /// The registry lives in memory and starts every service `Disconnected`, but a connection does
    /// not end when the process does — the token sits in the Keychain across relaunches. Without
    /// this the panel reports "Not connected" for a service that is connected, and — worse, because
    /// it is silent — [`Self::services_due`] skips it, so the 15-minute read-sync never runs again
    /// until the user redoes the whole browser OAuth flow.
    ///
    /// Restores with no last-sync time on purpose: the state is "connected, never synced this
    /// process", which makes the service due on the next tick instead of idling out one more
    /// interval, and shows no freshness rather than a fabricated one.
    pub fn restore_connected(&mut self, service: Service) {
        self.invalidate_tools();
        self.keychain_locked.remove(&service);
        self.registry.apply(service, ConnEvent::Connected { ts: 0 });
    }

    /// Restore without mistaking a denied read for an absent grant. No store writes here.
    pub fn restore_from_store(&mut self, service: Service, store: &impl crate::token::GrantStore) {
        match store.load(service) {
            Ok(Some(tokens)) => {
                self.restore_connected(service);
                self.set_account_label(service, tokens.account_label);
            }
            Ok(None) => {
                self.disconnect(service, false);
            }
            Err(_) => {
                self.invalidate_tools();
                if self.keychain_locked.insert(service) {
                    self.focus_retry_used = false;
                }
            }
        }
    }

    pub fn keychain_locked_services(&self) -> Vec<Service> {
        self.keychain_locked.iter().copied().collect()
    }

    /// A prompt can itself refocus the app: consume before retrying to prevent prompt loops.
    pub fn take_keychain_focus_retry(&mut self) -> bool {
        if self.keychain_locked.is_empty() {
            self.focus_retry_used = false;
            return false;
        }
        if self.focus_retry_used {
            return false;
        }
        self.focus_retry_used = true;
        true
    }

    /// Record that a service's token expired / was revoked (amber).
    pub fn mark_token_expired(&mut self, service: Service) {
        let before = self.registry.state(service);
        if self.registry.apply(service, ConnEvent::TokenExpired) != before { self.invalidate_tools(); }
    }

    /// Record that an interactive connect attempt failed mid-flight (browser denial, timeout,
    /// token exchange, persist) — amber with the reauth affordance (FR-INT-06), the same event an
    /// auth-failed sync applies. Never called for precondition problems (missing OAuth client
    /// config). A service without an established grant stays Disconnected.
    pub fn mark_connect_failed(&mut self, service: Service) {
        let before = self.registry.state(service);
        if self.registry.apply(service, ConnEvent::ConnectFailed) != before { self.invalidate_tools(); }
    }

    /// Disconnect a service (FR-INT-07): the token is deleted by the caller; this clears state.
    pub fn disconnect(&mut self, service: Service, delete_events: bool) -> DisconnectOutcome {
        self.keychain_locked.remove(&service);
        self.invalidate_connection(service);
        if self.registry.state(service) != ConnState::Disconnected {
            self.invalidate_tools();
        }
        self.account_labels.remove(&service);
        self.registry.disconnect(service, delete_events)
    }

    /// A UI-facing status for every service: connection state, freshness, wave availability, and
    /// whether a first-layer MCP endpoint exists yet. This is what the connections screen renders.
    pub fn statuses(&self, now_ms: i64) -> Vec<ServiceStatus> {
        ALL_SERVICES
            .iter()
            .copied()
            // Web (issue #555) has no OAuth row on the connections screen: its switch is the
            // Web fetch consent card, which the desktop renders on its own.
            .filter(|service| *service != Service::Web)
            .map(|service| {
                let released = service.is_released(self.highest_released);
                let state = if !released {
                    ConnUi::ComingSoon
                } else if self.keychain_locked.contains(&service) {
                    ConnUi::KeychainLocked
                } else if service == Service::Gmail && !self.composio_consent {
                    // Without consent nothing reaches the mailbox, so it is not connected, the
                    // same fold `tool_loop_view` applies (issue #781).
                    ConnUi::Disconnected
                } else {
                    match self.registry.state(service) {
                        ConnState::Connected { .. } => ConnUi::Connected,
                        ConnState::NeedsReauth { .. } => ConnUi::NeedsReauth,
                        ConnState::Disconnected => ConnUi::Disconnected,
                    }
                };
                ServiceStatus {
                    service,
                    source: service.source_str(),
                    state,
                    last_sync_ms: self.registry.freshness_ms(service, now_ms).map(|f| now_ms - f),
                    has_endpoint: crate::endpoints::has_endpoint(service)
                        || service == Service::Gmail,
                    access: shogun_mcp::scope::access(service).as_str(),
                    account_label: if matches!(state, ConnUi::Connected | ConnUi::NeedsReauth) {
                        self.account_labels.get(&service).cloned()
                    } else { None },
                }
            })
            .collect()
    }

    /// The gate's view of `service` right now. For Gmail the consent is re-read through the probe
    /// (issue #781), so every Gmail op judges the persisted consent as it is at that moment; other
    /// services never touch the policy file.
    fn ctx(&self, service: Service) -> OpContext {
        OpContext {
            highest_released: self.highest_released,
            conn: if self.keychain_locked.contains(&service) {
                ConnState::Disconnected // Gate operations while the grant is unreadable.
            } else {
                self.registry.state(service)
            },
            draft_stop: self.draft_stop,
            composio_consent: if service == Service::Gmail {
                self.gmail_consent_live()
            } else {
                self.composio_consent
            },
            plan: self.plan,
        }
    }
}

/// Whether an interactive connect that started at `revision` may still send its post-consent
/// probe. It never waits for the runtime lock: the sync poller, `sync_now` and the connections
/// watch hold it across network calls, and a busy lock is not a disconnect — treating it as one
/// threw away a fresh grant and showed "Needs reauth". So busy counts as still current; a
/// disconnect is still seen through the durable request marker and the caller's credential
/// revision, and the commit re-checks [`ConnectorRuntime::connection_revision`] under the lock
/// ([`crate::connect::commit_fence`]), so the loser of a race never saves a grant. A poisoned
/// lock fails closed.
pub fn connection_still_current<T>(
    runtime: &std::sync::Mutex<ConnectorRuntime<T>>,
    service: Service,
    revision: u64,
) -> bool {
    match runtime.try_lock() {
        Ok(rt) => rt.connection_revision(service) == revision,
        Err(std::sync::TryLockError::WouldBlock) => true,
        Err(std::sync::TryLockError::Poisoned(_)) => false,
    }
}

/// Credential failures require sign-in; permission-only 403s and transient faults do not.
/// These markers are the content-free errors produced by the provider adapters.
pub fn is_auth_failure(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    if (reason.contains("refresh(") || reason.contains("token endpoint http"))
        && !crate::token::is_transient_token_failure(&reason)
    {
        return true;
    }
    [
        "http 401", "unauthorized", "invalid_grant", "invalid_token", "invalid_auth",
        "token_revoked", "token_expired", "token expired", "not_authed", "account_inactive",
        "gmail credentials are unavailable", "needsreauth", "notconnected",
        "no access token", "no oauth client",
    ].iter().any(|marker| reason.contains(marker))
}

/// Run a sync without holding the shared runtime during provider HTTP. Each returned page and
/// final status is fenced against connection changes before it can affect memory or the registry.
pub fn sync_service_unlocked<T: IntegrationTransport, S: IngestSink>(
    runtime: &std::sync::Mutex<ConnectorRuntime<T>>,
    service: Service,
    now_ms: i64,
    sink: &S,
) -> Result<SyncReport, SyncFailure> {
    SyncRuntime::Shared(runtime).sync_service(service, now_ms, sink)
}

/// How long a shared sync waits for the runtime between pages (matches the desktop's 5s budget).
const SHARED_SYNC_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

// Keep the same paging, consent, ingest and cursor rules for exclusive and shared callers.
enum SyncRuntime<'a, T> {
    Exclusive(&'a mut ConnectorRuntime<T>),
    Shared(&'a std::sync::Mutex<ConnectorRuntime<T>>),
}

impl<T: IntegrationTransport> SyncRuntime<'_, T> {
    fn with_runtime<R>(
        &mut self,
        apply: impl FnOnce(&mut ConnectorRuntime<T>) -> R,
    ) -> Result<R, SyncFailure> {
        match self {
            Self::Exclusive(runtime) => Ok(apply(runtime)),
            Self::Shared(runtime) => {
                // Short holders (Settings, the connections watch, tool reads) must not fail a
                // sync tick that would otherwise wait 15 minutes; poisoned or long-held fails.
                let deadline = std::time::Instant::now() + SHARED_SYNC_LOCK_WAIT;
                let mut guard = loop {
                    match runtime.try_lock() {
                        Ok(guard) => break guard,
                        Err(std::sync::TryLockError::WouldBlock)
                            if std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => {
                            return Err(SyncFailure::Transport(
                                "connections busy or unavailable".into(),
                            ))
                        }
                    }
                };
                Ok(apply(&mut guard))
            }
        }
    }

    fn sync_service<S: IngestSink>(
        mut self,
        service: Service,
        now_ms: i64,
        sink: &S,
    ) -> Result<SyncReport, SyncFailure> {
        // Incremental, paged fetch (FR-TG-09): start from the sink's cursor (or the default
        // backfill window), walk pages until the provider stops or the per-tick cap is hit, and
        // store the advanced watermark only after every page of this tick ingested.
        let sync_revision = NEXT_TOOL_AUTH_REVISION.fetch_add(1, Ordering::Relaxed);
        let revision = self.with_runtime(|runtime| {
            runtime.sync_revisions.insert(service, sync_revision);
            runtime.connection_revision(service)
        })?;
        let mut cursor = sink.cursor(service).unwrap_or(SyncCursor {
            watermark_ms: (now_ms - DEFAULT_BACKFILL_MS).max(0),
            page_token: None,
        });
        let start_watermark = cursor.watermark_ms;
        let mut fetched = 0usize;
        let mut inserted = 0usize;
        let mut max_ts = cursor.watermark_ms;
        let mut pages = 0usize;
        let outcome = loop {
            // The gate is judged again before every page, not once per tick: consent revoked while
            // the walk is in flight stops it before the next request leaves (issue #781).
            let (ctx, transport, current) = self.with_runtime(|runtime| {
                (
                    runtime.ctx(service),
                    runtime.transport_handle(),
                    (
                        runtime.connection_revision(service),
                        runtime.sync_revisions.get(&service).copied(),
                    ),
                )
            })?;
            if current != (revision, Some(sync_revision)) {
                break Err(SyncFailure::Denied(DenyReason::NotConnected));
            }
            // Gate first, then record, then send: a denied read leaves no row, and a sent one
            // leaves its row even when the page never comes back (issue #782).
            if let Err(reason) = authorize_sync(service, &ctx) {
                break Err(SyncFailure::Denied(reason));
            }
            sink.record_fetch_attempt(service, FetchKind::SyncPage);
            let page = collect_sync_page(service, &ctx, transport.as_ref(), &cursor);
            let applied = self
                .with_runtime(|runtime| {
                    // A connect/disconnect during HTTP wins over this page, its cursor and errors.
                    if runtime.connection_revision(service) != revision
                        || runtime.sync_revisions.get(&service) != Some(&sync_revision)
                    {
                        return Err(SyncFailure::Denied(DenyReason::NotConnected));
                    }
                    authorize_sync(service, &runtime.ctx(service)).map_err(SyncFailure::Denied)?;
                    match page {
                        Ok((items, next)) => {
                            fetched += items.len();
                            max_ts = items.iter().map(|i| i.ts_ms).fold(max_ts, i64::max);
                            let count = sink.ingest_counted(&items);
                            inserted += count.inserted;
                            pages += 1;
                            if count.rejected > 0 {
                                // The store refused part of this page (disk full, I/O error, a write lock
                                // held by another process). Advancing the watermark would skip that mail
                                // for good, so stop the walk and keep the tick's starting watermark: the
                                // next tick fetches the same window again, and items that did land are
                                // deduplicated on the way in (issue #781).
                                cursor.page_token = None;
                                cursor.watermark_ms = start_watermark;
                                return Ok(true);
                            }
                            match next {
                                Some(token)
                                    if fetched < MAX_SYNC_ITEMS_PER_TICK
                                        && pages < MAX_SYNC_PAGES_PER_TICK =>
                                {
                                    cursor.page_token = Some(token);
                                    Ok(false)
                                }
                                Some(token) => {
                                    // Cap hit mid-walk: keep the token so the next tick resumes here
                                    // without advancing the watermark past unseen mail.
                                    cursor.page_token = Some(token);
                                    cursor.watermark_ms = start_watermark;
                                    Ok(true)
                                }
                                None => {
                                    cursor.page_token = None;
                                    cursor.watermark_ms = max_ts.max(start_watermark);
                                    Ok(true)
                                }
                            }
                        }
                        Err(error) => Err(error),
                    }
                })
                .and_then(std::convert::identity);
            match applied {
                Ok(true) => break Ok(()),
                Ok(false) => {}
                Err(error) => break Err(error),
            }
        };
        self.with_runtime(|runtime| {
            if runtime.connection_revision(service) != revision
                || runtime.sync_revisions.get(&service) != Some(&sync_revision)
            {
                return Err(SyncFailure::Denied(DenyReason::NotConnected));
            }
            let outcome = outcome.and_then(|()| {
                authorize_sync(service, &runtime.ctx(service)).map_err(SyncFailure::Denied)
            });
            match outcome {
                Ok(()) => {
                    sink.store_cursor(service, &cursor);
                    runtime
                        .registry
                        .apply(service, ConnEvent::Synced { ts: now_ms });
                    Ok(SyncReport { fetched, inserted })
                }
                Err(SyncFailure::Transport(e)) => {
                    // Drop a half-walked page token before giving up. It is only valid for the walk
                    // that just failed; keeping it would hand the same rejected token to every later
                    // tick and wedge this service's sync for good. The watermark is untouched, so the
                    // next tick restarts the walk and re-fetches at most one window.
                    if cursor.page_token.is_some() {
                        sink.store_cursor(
                            service,
                            &SyncCursor {
                                watermark_ms: start_watermark,
                                page_token: None,
                            },
                        );
                    }
                    // An auth-shaped fetch failure means the token/connection is bad → amber
                    // (FR-INT-06). A transient fault (network, 5xx) leaves the state alone: the
                    // service stays green and is retried next tick instead of flashing "Needs
                    // reauth" over a healthy connection.
                    runtime.observe_provider_failure(service, &e);
                    Err(SyncFailure::Transport(e))
                }
                // Consent revoked mid-walk (issue #781): the pages before the revocation are in memory,
                // the rest is not, and the stored cursor is untouched, so a later sync under a new
                // consent fetches the same window again (ingest deduplicates). Gmail is disconnected.
                Err(SyncFailure::Denied(DenyReason::ConsentRequired)) => {
                    runtime.observe_revocation(service);
                    Err(SyncFailure::Denied(DenyReason::ConsentRequired))
                }
                // Denied (unreleased wave / disconnected / unknown op): no state change.
                Err(other) => Err(other),
            }
        })?
    }
}

impl<T: IntegrationTransport> ConnectorRuntime<T> {
    /// Run one read-sync for a service: gate → fetch → ingest. On success, mark the service synced;
    /// on a transport failure, turn it amber (needs-reauth) without touching other services
    /// (FR-INT-06). A gate denial (unreleased / disconnected) changes no state.
    pub fn sync_service<S: IngestSink>(
        &mut self,
        service: Service,
        now_ms: i64,
        sink: &S,
    ) -> Result<SyncReport, SyncFailure> {
        SyncRuntime::Exclusive(self).sync_service(service, now_ms, sink)
    }

    /// Fetch a specific item on demand (§6.9 `read_on_demand`, L2) and ingest it — e.g. the Gmail
    /// thread the user just opened. Unlike a background sync this does not touch the poll schedule
    /// (it is a targeted fetch, not a full refresh); an auth-shaped transport failure still turns
    /// the service amber (a real credential problem). Denied for a service without a
    /// `read_on_demand` op or when the gate refuses.
    pub fn fetch_on_demand<S: IngestSink>(
        &mut self,
        service: Service,
        query: &str,
        sink: &S,
    ) -> Result<SyncReport, SyncFailure> {
        let ctx = self.ctx(service);
        // Same order as a sync page: gate, record the attempt, then send (issue #782).
        authorize_on_demand(service, &ctx).map_err(SyncFailure::Denied)?;
        sink.record_fetch_attempt(service, FetchKind::OnDemand);
        match collect_on_demand(service, query, &ctx, self.transport.as_ref()) {
            // Revoked while the fetch was in flight (issue #781): the answer is dropped unread.
            Ok(_) if service == Service::Gmail && !self.gmail_consent_live() => {
                self.observe_revocation(service);
                Err(SyncFailure::Denied(DenyReason::ConsentRequired))
            }
            Ok(items) => {
                let inserted = sink.ingest(&items);
                Ok(SyncReport { fetched: items.len(), inserted })
            }
            Err(SyncFailure::Transport(e)) => {
                // Same auth-vs-transient split as `sync_service`.
                self.observe_provider_failure(service, &e);
                Err(SyncFailure::Transport(e))
            }
            Err(other) => Err(other),
        }
    }

    /// Services whose read-sync is due now: released, connected-or-amber, and either never synced or
    /// staler than `interval_ms`. The scheduler basis for [`Self::poll_tick`].
    pub fn services_due(&self, now_ms: i64, interval_ms: i64) -> Vec<Service> {
        ALL_SERVICES
            .iter()
            .copied()
            // Only services whose scope table declares a background read_sync.
            .filter(|&s| shogun_mcp::scope::authorize(s, shogun_mcp::sync::READ_SYNC) == shogun_mcp::scope::Authorization::Background)
            .filter(|s| !self.keychain_locked.contains(s))
            .filter(|&s| s.is_released(self.highest_released))
            .filter(|&s| matches!(self.registry.state(s), ConnState::Connected { .. } | ConnState::NeedsReauth { .. }))
            .filter(|&s| self.registry.freshness_ms(s, now_ms).is_none_or(|f| f >= interval_ms))
            .collect()
    }

    /// Sync every due service once. This is the body the daemon's 15-minute tokio interval calls;
    /// it returns each service's outcome so the daemon can log/emit per service.
    pub fn poll_tick<S: IngestSink>(
        &mut self,
        now_ms: i64,
        interval_ms: i64,
        sink: &S,
    ) -> Vec<(Service, Result<SyncReport, SyncFailure>)> {
        self.services_due(now_ms, interval_ms)
            .into_iter()
            .map(|s| (s, self.sync_service(s, now_ms, sink)))
            .collect()
    }

    /// Execute a first-layer write that has already passed L2/L3 confirmation upstream (the approval
    /// queue in `shogun-agents`). Re-checks the gate as defense in depth (WP-F double gate) before
    /// touching the service; a Composio-routed op (Gmail send) is refused here — it is the second
    /// layer, not first-layer MCP.
    pub fn execute_write<W: WriteExecutor>(
        &mut self,
        service: Service,
        op_name: &str,
        arguments: Value,
        exec: &W,
    ) -> Result<Value, String> {
        self.precheck_write(service, op_name)?;
        let result = exec.execute(service, op_name, arguments);
        self.observe_provider_result(service, result)
    }

    /// Gate a write without performing it — the decision [`Self::execute_write`] makes first. A
    /// caller that must record traceability before the request leaves (issue #782) checks here,
    /// records, and then executes, so a write the gate refuses (nothing sent) leaves no row.
    pub fn precheck_write(&self, service: Service, op_name: &str) -> Result<(), String> {
        match authorize_op(service, op_name, &self.ctx(service)) {
            OpDecision::RequiresLevel(_) => Ok(()),
            OpDecision::RequiresComposio => {
                Err(format!("{}::{op_name} is second-layer (Composio), not first-layer MCP", service.source_str()))
            }
            other => Err(format!("write not authorized: {other:?}")),
        }
    }

    /// Gate a read without performing it: class must be Read, then the same authorize_op the
    /// poller uses. The tool-loop runner calls this under the runtime lock, then drops the lock
    /// before HTTP so two reads can overlap (FR-HN-07).
    pub fn precheck_read(&self, service: Service, op_name: &str) -> Result<(), String> {
        let class = shogun_mcp::scope::lookup(service, op_name).map(|o| o.class);
        if class != Some(shogun_mcp::scope::OpClass::Read) {
            return Err(format!("{}::{op_name} is not a read", service.source_str()));
        }
        let decision = authorize_op(service, op_name, &self.ctx(service));
        if decision.is_allowed() {
            Ok(())
        } else {
            Err(format!("read not authorized: {decision:?}"))
        }
    }

    /// Execute a single read op on demand — the tool loop's runner path (issue #81 step 4:
    /// e.g. `read_sync`, `free_busy`, `read_on_demand`). Re-checks the gate as defense in depth,
    /// exactly like [`Self::execute_write`], and additionally requires the op's **class to be
    /// [`OpClass::Read`]** before the gate is even consulted: a write, send, or unknown op that
    /// somehow reached this path is refused — so even if the loop's own classification were
    /// bypassed, nothing consequential can run through the read runner (invariant 4). Within the
    /// Read class, any allowed gate resolution executes (Background, or a leveled read like
    /// `free_busy` — the same rule the loop's `classify_call` applies).
    pub fn execute_read<W: WriteExecutor>(
        &mut self,
        service: Service,
        op_name: &str,
        arguments: Value,
        exec: &W,
    ) -> Result<Value, String> {
        self.precheck_read(service, op_name)?;
        let result = exec.execute(service, op_name, arguments);
        self.observe_provider_result(service, result)
    }
}

impl<T: IntegrationTransport + WriteExecutor> ConnectorRuntime<T> {
    /// The normal write path: gate + execute using the runtime's **own** transport as the executor
    /// (the live `RemoteMcpTransport` is a [`WriteExecutor`]). [`Self::execute_write`] keeps a
    /// separate-executor form for tests; production wiring uses this so it never has to hand the
    /// runtime its own transport from outside.
    pub fn execute_write_owned(
        &mut self,
        service: Service,
        op_name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let transport = self.transport_handle();
        self.execute_write(service, op_name, arguments, transport.as_ref())
    }

    /// [`Self::execute_read`] using the runtime's own transport — the production wiring for the
    /// tool loop's read runner.
    pub fn execute_read_owned(
        &mut self,
        service: Service,
        op_name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let transport = self.transport_handle();
        self.execute_read(service, op_name, arguments, transport.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use shogun_mcp::sync::{FetchPage, FetchedItem};

    #[test]
    fn shared_sync_releases_http_lock_and_discards_superseded_pages_and_errors() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{mpsc, Mutex};
        use std::time::Duration;

        struct BlockingFetch {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            first: AtomicBool,
            fail: bool,
        }
        impl IntegrationTransport for BlockingFetch {
            fn read_sync(&self, _: Service) -> Result<Vec<FetchedItem>, String> {
                if self.first.swap(false, Ordering::SeqCst) {
                    self.entered.send(()).expect("signal HTTP started");
                    self.release
                        .lock()
                        .expect("release channel")
                        .recv_timeout(Duration::from_secs(2))
                        .expect("release HTTP");
                    if self.fail {
                        return Err("invalid_token".into());
                    }
                }
                Ok(vec![item("page from original connection")])
            }
        }
        #[derive(Default)]
        struct Sink {
            inserted: RefCell<usize>,
            cursors: RefCell<usize>,
        }
        impl IngestSink for Sink {
            fn ingest(&self, items: &[IngestItem]) -> usize {
                *self.inserted.borrow_mut() += items.len();
                items.len()
            }
            fn store_cursor(&self, _: Service, _: &SyncCursor) {
                *self.cursors.borrow_mut() += 1;
            }
        }

        for change in ["none", "disconnect", "reconnect", "newer_sync"] {
            for fail in [false, true] {
                let (entered, started) = mpsc::channel();
                let (release, receive) = mpsc::channel();
                let mut runtime = ConnectorRuntime::new(
                    BlockingFetch {
                        entered,
                        release: Mutex::new(receive),
                        first: AtomicBool::new(true),
                        fail,
                    },
                    Wave::Three,
                    true,
                );
                runtime.set_plan(shogun_agents::entitlement::entitlements(
                    shogun_agents::entitlement::Plan::Pro,
                    0,
                ));
                runtime.mark_connected(Service::GoogleCalendar, 1);
                let shared = Arc::new(Mutex::new(runtime));
                let worker_runtime = Arc::clone(&shared);
                let worker = std::thread::spawn(move || {
                    let sink = Sink::default();
                    let result =
                        sync_service_unlocked(&worker_runtime, Service::GoogleCalendar, 10, &sink);
                    (
                        result,
                        sink.inserted.into_inner(),
                        sink.cursors.into_inner(),
                    )
                });
                started
                    .recv_timeout(Duration::from_secs(2))
                    .expect("HTTP started");
                let expected = {
                    // Settings/Disconnect must acquire this while HTTP is still blocked.
                    let mut runtime = shared.try_lock().expect("HTTP must release the runtime");
                    match change {
                        "disconnect" => {
                            runtime.disconnect(Service::GoogleCalendar, false);
                        }
                        "reconnect" => runtime.mark_connected(Service::GoogleCalendar, 20),
                        "newer_sync" => {
                            runtime
                                .sync_service(Service::GoogleCalendar, 20, &Sink::default())
                                .expect("newer sync");
                        }
                        _ => {}
                    }
                    runtime.registry().state(Service::GoogleCalendar)
                };
                release.send(()).expect("release blocked HTTP");
                let (result, inserted, cursors) = worker.join().expect("sync worker");
                let runtime = shared.lock().expect("runtime after sync");
                if change == "none" {
                    assert_eq!(result.is_ok(), !fail);
                    assert_eq!((inserted, cursors), if fail { (0, 0) } else { (1, 1) });
                    assert_eq!(
                        runtime.registry().state(Service::GoogleCalendar).is_amber(),
                        fail
                    );
                } else {
                    assert!(matches!(
                        result,
                        Err(SyncFailure::Denied(DenyReason::NotConnected))
                    ));
                    assert_eq!(
                        (inserted, cursors),
                        (0, 0),
                        "stale pages cannot write memory or cursors"
                    );
                    assert_eq!(runtime.registry().state(Service::GoogleCalendar), expected);
                }
            }
        }
    }

    #[test]
    fn provider_errors_map_only_auth_failures_to_amber_and_notify_once() {
        for reason in [
            "mcp http 401", "mcp http 403 invalid_token", "invalid_grant",
            "Refresh(\"token endpoint http 400\")", "Refresh(\"malformed token response\")",
            "NeedsReauth", "mcp tool unauthorized", "invalid_auth", "token_revoked",
            "Gmail token expired; reconnect Gmail.", "Gmail credentials are unavailable",
        ] {
            let mut rt = runtime(vec![], None);
            rt.mark_connected(Service::GoogleCalendar, 123);
            rt.mark_connected(Service::Gmail, 456);
            let notifications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = Arc::clone(&notifications);
            rt.set_auth_state_changed(Arc::new(move || { count.fetch_add(1, Ordering::Relaxed); }));
            let revision = rt.tool_auth_revision();
            rt.observe_provider_failure(Service::GoogleCalendar, reason);
            rt.observe_provider_failure(Service::GoogleCalendar, reason);
            assert_eq!(rt.registry().state(Service::GoogleCalendar), ConnState::NeedsReauth {
                reason: shogun_mcp::connection::ReauthReason::TokenExpired, last_sync_ms: 123,
            }, "{reason}");
            assert_eq!(rt.registry().state(Service::Gmail), ConnState::Connected { last_sync_ms: 456 });
            assert_ne!(rt.tool_auth_revision(), revision);
            assert_eq!(notifications.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn permission_and_transient_failures_preserve_connection_and_authorization() {
        for reason in [
            "mcp http 403", "403 Forbidden: insufficient_scope", "Gmail permission was denied; check scopes and reconnect.",
            "mcp tool returned an error", "mcp http 429", "mcp http 503", "mcp request timed out",
            "Refresh(\"token exchange failed\")", "Refresh(\"token endpoint http 503\")",
            "Refresh(\"token endpoint http 429\")",
        ] {
            let mut rt = runtime(vec![], None);
            rt.mark_connected(Service::GoogleCalendar, 123);
            let revision = rt.tool_auth_revision();
            rt.observe_provider_failure(Service::GoogleCalendar, reason);
            assert_eq!(rt.registry().state(Service::GoogleCalendar), ConnState::Connected { last_sync_ms: 123 }, "{reason}");
            assert_eq!(rt.tool_auth_revision(), revision);
        }
        let mut rt = runtime(vec![], None);
        rt.observe_provider_failure(Service::GoogleCalendar, "invalid_token");
        assert_eq!(rt.registry().state(Service::GoogleCalendar), ConnState::Disconnected);
    }

    #[test]
    fn read_and_confirmed_write_failures_update_state_without_retrying() {
        struct Rejected(bool, std::cell::Cell<usize>);
        impl WriteExecutor for Rejected {
            fn execute(&self, _: Service, _: &str, _: Value) -> Result<Value, String> {
                self.1.set(self.1.get() + 1);
                if self.0 { Err("invalid_grant".into()) }
                else { Ok(serde_json::json!({"isError": true, "content": [{"text": "invalid_token"}]})) }
            }
        }
        for write in [true, false] {
            for transport_error in [true, false] {
                let mut rt = runtime(vec![], None);
                rt.mark_connected(Service::GoogleCalendar, 123);
                let exec = Rejected(transport_error, std::cell::Cell::new(0));
                let result = if write {
                    rt.execute_write(Service::GoogleCalendar, "event_create", serde_json::json!({}), &exec)
                } else {
                    rt.execute_read(Service::GoogleCalendar, "free_busy", serde_json::json!({}), &exec)
                };
                assert!(result.is_err());
                assert!(rt.registry().state(Service::GoogleCalendar).is_amber());
                assert_eq!(exec.1.get(), 1, "a failed request must never be retried");
                assert!(rt.precheck_write(Service::GoogleCalendar, "event_create").is_err());
            }
        }
    }

    #[test]
    fn query_only_services_are_offered_for_search_but_excluded_from_all_sync_ticks() {
        let mut runtime = ConnectorRuntime::new(FakeTransport {items:vec![],err:None},Wave::Three,true);
        runtime.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro,0));
        for service in [Service::Slack,Service::Notion,Service::Granola] { runtime.mark_connected(service,1); }
        assert!(runtime.services_due(2,0).is_empty());
        assert!(runtime.services_due(1_000_000,15*60_000).is_empty());
        let (services,context) = runtime.tool_loop_view();
        let tools = shogun_mcp::tool_catalog::tool_definitions(&services,&context);
        for name in ["search_chat_messages","search_docs","list_granola_meetings","read_granola_meeting"] { assert!(tools.iter().any(|tool| tool["name"] == name)); }
    }

    #[test]
    fn auth_revision_changes_for_reconnect_and_policy_but_not_sync_refresh() -> Result<(), String> {
        let mut rt = ConnectorRuntime::new(FakeTransport { items: vec![], err: None }, Wave::One, true);
        let plan = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        rt.set_plan(plan);
        rt.mark_connected(Service::GoogleCalendar, 1);
        let revision = rt.tool_auth_revision();
        rt.set_plan(plan);
        rt.set_draft_stop(true);
        rt.set_composio_consent(false);
        rt.sync_service(Service::GoogleCalendar, 2, &CountingSink(RefCell::new(0))).map_err(|_| "sync failed")?;
        assert_eq!(rt.tool_auth_revision(), revision);
        rt.mark_connected(Service::GoogleCalendar, 3);
        assert_ne!(rt.tool_auth_revision(), revision, "successful reconnect changes credential identity");
        let revision = rt.tool_auth_revision();
        rt.set_composio_consent(true);
        assert_ne!(rt.tool_auth_revision(), revision);
        Ok(())
    }

    /// A transport yielding a fixed fetch or an error.
    struct FakeTransport {
        items: Vec<FetchedItem>,
        err: Option<String>,
    }
    impl IntegrationTransport for FakeTransport {
        fn read_sync(&self, _service: Service) -> Result<Vec<FetchedItem>, String> {
            match &self.err {
                Some(e) => Err(e.clone()),
                None => Ok(self.items.clone()),
            }
        }
        fn fetch_on_demand(&self, _service: Service, _query: &str) -> Result<Vec<FetchedItem>, String> {
            match &self.err {
                Some(e) => Err(e.clone()),
                None => Ok(self.items.clone()),
            }
        }
    }

    /// Counts ingested items.
    struct CountingSink(RefCell<usize>);
    impl IngestSink for CountingSink {
        fn ingest(&self, items: &[IngestItem]) -> usize {
            *self.0.borrow_mut() += items.len();
            items.len()
        }
    }

    fn item(body: &str) -> FetchedItem {
        FetchedItem { external_id: "x".into(), title: "t".into(), body: body.into(), ts_ms: 1, mail: None, calendar: None }
    }

    /// Hands out one page with a token, then fails — the shape of an expired page token.
    struct PagingThenFailing {
        calls: RefCell<usize>,
    }
    impl IntegrationTransport for PagingThenFailing {
        fn read_sync(&self, _service: Service) -> Result<Vec<FetchedItem>, String> {
            Ok(Vec::new())
        }
        fn read_sync_page(&self, _service: Service, cursor: &SyncCursor) -> Result<FetchPage, String> {
            let mut calls = self.calls.borrow_mut();
            *calls += 1;
            if cursor.page_token.is_some() {
                return Err("400 invalid page token".to_string());
            }
            Ok(FetchPage { items: vec![item("first page")], next_page_token: Some("tok".into()) })
        }
    }

    /// Records the cursor the runtime stores.
    #[derive(Default)]
    struct CursorSink {
        cursor: RefCell<Option<SyncCursor>>,
    }
    impl IngestSink for CursorSink {
        fn ingest(&self, items: &[IngestItem]) -> usize {
            items.len()
        }
        fn cursor(&self, _service: Service) -> Option<SyncCursor> {
            self.cursor.borrow().clone()
        }
        fn store_cursor(&self, _service: Service, cursor: &SyncCursor) {
            *self.cursor.borrow_mut() = Some(cursor.clone());
        }
    }

    /// A page token is only valid for the walk that produced it. When the next page fails the
    /// token must be dropped, or every later tick replays the same rejected request and the
    /// service never syncs again.
    #[test]
    fn a_failed_page_clears_the_stored_page_token_instead_of_wedging_the_service() {
        let mut rt = ConnectorRuntime::new(PagingThenFailing { calls: RefCell::new(0) }, Wave::One, false);
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        rt.mark_connected(Service::Gmail, 1_000);
        // Gmail rides Composio, so without consent the sync is denied before the transport is
        // touched — grant it here, since what this test pins is the paging cursor, not the gate.
        rt.set_composio_consent(true);
        let sink = CursorSink::default();
        // First tick: one page in, the walk is cut short by the failure on page two.
        assert!(rt.sync_service(Service::Gmail, 2_000, &sink).is_err());
        let stored = sink.cursor.borrow().clone().expect("a cursor was stored");
        assert_eq!(stored.page_token, None, "the half-walked token is not carried forward");
        // Second tick starts a fresh walk rather than replaying the rejected token.
        assert!(rt.sync_service(Service::Gmail, 3_000, &sink).is_err());
        assert_eq!(*rt.transport.calls.borrow(), 4, "each tick walks page one then fails on page two");
    }

    /// A sink whose store refuses everything (disk full, I/O error, a write lock held elsewhere).
    #[derive(Default)]
    struct RefusingSink {
        cursor: RefCell<Option<SyncCursor>>,
    }
    impl IngestSink for RefusingSink {
        fn ingest(&self, _items: &[IngestItem]) -> usize {
            0
        }
        fn ingest_counted(&self, items: &[IngestItem]) -> IngestCount {
            IngestCount { inserted: 0, rejected: items.len() }
        }
        fn cursor(&self, _service: Service) -> Option<SyncCursor> {
            self.cursor.borrow().clone()
        }
        fn store_cursor(&self, _service: Service, cursor: &SyncCursor) {
            *self.cursor.borrow_mut() = Some(cursor.clone());
        }
    }

    /// Issue #781: when the store refuses the fetched mail, the watermark must not jump to the
    /// newest message, or that mail is never fetched again once the store recovers.
    #[test]
    fn a_refused_ingest_keeps_the_starting_watermark() {
        let mut fetched = item("mail the store refuses");
        fetched.ts_ms = 9_000;
        let mut rt = runtime(vec![fetched], None);
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        rt.mark_connected(Service::Gmail, 1_000);
        rt.set_composio_consent(true);
        let sink = RefusingSink::default();
        *sink.cursor.borrow_mut() = Some(SyncCursor { watermark_ms: 5_000, page_token: None });

        let report = rt.sync_service(Service::Gmail, 10_000, &sink).expect("the fetch itself worked");
        assert_eq!(report.inserted, 0);
        let stored = sink.cursor.borrow().clone().expect("a cursor was stored");
        assert_eq!(stored.watermark_ms, 5_000, "the watermark stays where the tick started");
        assert_eq!(stored.page_token, None);

        // A sink that accepts the same page advances.
        let accepting = CursorSink::default();
        *accepting.cursor.borrow_mut() = Some(SyncCursor { watermark_ms: 5_000, page_token: None });
        rt.sync_service(Service::Gmail, 11_000, &accepting).expect("sync");
        assert_eq!(accepting.cursor.borrow().clone().unwrap().watermark_ms, 9_000);
    }

    #[test]
    fn statuses_report_coming_soon_connected_and_endpoint_availability() {
        let mut rt = runtime(vec![], None);
        rt.mark_connected(Service::Gmail, 1_000);
        let statuses = rt.statuses(2_000);
        let gmail = statuses.iter().find(|s| s.source == "gmail").unwrap();
        assert_eq!(gmail.state, ConnUi::Connected);
        assert!(gmail.has_endpoint);
        assert_eq!(gmail.last_sync_ms, Some(1_000));
        // Slack is Wave 2 → coming soon at Wave 1; its official MCP endpoint exists (OPEN-03).
        let slack = statuses.iter().find(|s| s.source == "slack").unwrap();
        assert_eq!(slack.state, ConnUi::ComingSoon);
        assert!(slack.has_endpoint);
        // All first-layer services have an official endpoint (Waves 1-3); Notion is Wave 3, so
        // still "coming soon" at Wave 1 but its endpoint exists.
        let notion = statuses.iter().find(|s| s.source == "notion").unwrap();
        assert_eq!(notion.state, ConnUi::ComingSoon);
        assert!(notion.has_endpoint);
        // Calendar released but not connected → disconnected.
        let cal = statuses.iter().find(|s| s.source == "gcal").unwrap();
        assert_eq!(cal.state, ConnUi::Disconnected);
    }

    #[test]
    fn labels_follow_the_connection_and_clear_on_disconnect() {
        let mut rt = runtime(vec![], None);
        rt.mark_connected(Service::Gmail, 1_000);
        rt.set_account_label(Service::Gmail, Some("you@example.com".into()));
        let label = |rt: &ConnectorRuntime<FakeTransport>| rt.statuses(2_000).into_iter()
            .find(|row| row.service == Service::Gmail).expect("test").account_label;
        assert_eq!(label(&rt).as_deref(), Some("you@example.com"));
        rt.disconnect(Service::Gmail, false);
        assert_eq!(label(&rt), None);
        rt.mark_connected(Service::Gmail, 3_000);
        assert_eq!(label(&rt), None);
    }

    #[test]
    fn stored_grants_distinguish_absent_locked_and_connected_without_writes() {
        use crate::token::{GrantStore, TokenLoadError};
        struct FakeStore(Result<Option<crate::oauth::TokenSet>, TokenLoadError>);
        impl GrantStore for FakeStore {
            fn load(&self, _: Service) -> Result<Option<crate::oauth::TokenSet>, TokenLoadError> {
                self.0.clone()
            }
            fn save(&self, _: Service, _: &crate::oauth::TokenSet) -> Result<(), String> {
                panic!("restoring must not overwrite a grant")
            }
            fn delete(&self, _: Service) -> Result<(), String> {
                panic!("restoring must not delete a grant")
            }
        }
        let mut rt = runtime(vec![], None);
        assert!(!rt.take_keychain_focus_retry());
        let token = crate::oauth::TokenSet {
            access_token: "test-only".into(), refresh_token: None, expires_at_ms: i64::MAX,
            account_label: None,
        };
        for (load, expected) in [
            (Ok(None), ConnUi::Disconnected),
            (Err(TokenLoadError::KeychainLocked), ConnUi::KeychainLocked),
            (Err(TokenLoadError::Unavailable), ConnUi::KeychainLocked),
            (Ok(Some(token)), ConnUi::Connected),
            (Err(TokenLoadError::KeychainLocked), ConnUi::KeychainLocked),
            (Ok(None), ConnUi::Disconnected),
        ] {
            rt.restore_from_store(Service::GoogleCalendar, &FakeStore(load));
            let row = rt.statuses(0).into_iter().find(|r| r.service == Service::GoogleCalendar).expect("test");
            assert_eq!(row.state, expected);
            if expected == ConnUi::KeychainLocked {
                assert!(!rt.services_due(0, 0).contains(&Service::GoogleCalendar));
            }
        }
        rt.mark_connected(Service::GoogleCalendar, 123);
        rt.restore_from_store(Service::GoogleCalendar, &FakeStore(Err(TokenLoadError::KeychainLocked)));
        assert_eq!(rt.registry().state(Service::GoogleCalendar), ConnState::Connected { last_sync_ms: 123 });
        assert_eq!(rt.ctx(Service::GoogleCalendar).conn, ConnState::Disconnected);
        assert!(rt.take_keychain_focus_retry());
        assert!(!rt.take_keychain_focus_retry());
        rt.disconnect(Service::GoogleCalendar, false);
        assert!(rt.keychain_locked_services().is_empty());
        assert!(!rt.take_keychain_focus_retry());

        let locked = FakeStore(Err(TokenLoadError::KeychainLocked));
        rt.restore_from_store(Service::GoogleCalendar, &locked);
        assert!(rt.take_keychain_focus_retry());
        // A failed retry of the same lock must not create a refocus/prompt loop.
        rt.restore_from_store(Service::GoogleCalendar, &locked);
        assert!(!rt.take_keychain_focus_retry());
        // A newly locked provider gets a retry even while another one remains locked.
        rt.restore_from_store(Service::Gmail, &locked);
        assert!(rt.take_keychain_focus_retry());
        assert!(!rt.take_keychain_focus_retry());
        rt.restore_connected(Service::GoogleCalendar);
        rt.mark_connected(Service::Gmail, 456);
        // Relock without an intervening focus event while the set was empty.
        rt.restore_from_store(Service::GoogleCalendar, &locked);
        assert!(rt.take_keychain_focus_retry());
        assert!(!rt.take_keychain_focus_retry());
    }

    #[test]
    fn statuses_carry_the_access_badge_from_the_scope_table() {
        // Issue #82: the badge is the scope table's ceiling, serialized for the UI — the webview
        // keeps no copy of what a connection may do.
        let rt = runtime(vec![], None);
        for row in rt.statuses(0) {
            assert_eq!(row.access, shogun_mcp::scope::access(row.service).as_str(), "{}", row.source);
        }
        let json = serde_json::to_value(rt.statuses(0)).unwrap();
        let gcal = json
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["source"] == "gcal")
            .unwrap();
        assert_eq!(gcal["access"], "read");
    }

    #[test]
    fn published_statuses_are_the_rows_the_ui_shows() {
        // Issue #82: `connections.list` reports the very states the Connections screen renders.
        for state in [ConnUi::Connected, ConnUi::NeedsReauth, ConnUi::KeychainLocked, ConnUi::Disconnected, ConnUi::ComingSoon] {
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
        }
        let mut rt = runtime(vec![], None);
        rt.mark_connected(Service::Gmail, 1_000);
        rt.set_account_label(Service::Gmail, Some("you@example.com".into()));
        rt.mark_connected(Service::GoogleCalendar, 1_000);
        rt.mark_connect_failed(Service::GoogleCalendar);
        let path = std::env::temp_dir().join(format!("shogun-publish-{}.json", std::process::id()));
        publish_statuses(&path, 50_000, &rt.statuses(2_000)).expect("publish UI statuses");
        let read = shogun_mcp::connections_api::read_status(&path, 50_000).unwrap();
        assert!(read.contains(&(Service::Gmail, "connected")));
        assert!(read.contains(&(Service::GoogleCalendar, "needs_reauth")));
        assert!(read.contains(&(Service::Slack, "coming_soon")));
        assert_eq!(read.len(), rt.statuses(2_000).len());
        let text = std::fs::read_to_string(&path).expect("read published statuses");
        assert!(!text.contains("account_label"));
        assert!(!text.contains("you@example.com"));
        assert!(!text.contains("last_sync"), "state only: {text}");
        let _ = std::fs::remove_file(path);
    }

    fn runtime(items: Vec<FetchedItem>, err: Option<String>) -> ConnectorRuntime<FakeTransport> {
        // Wave 1 released, draft-stop on (irrelevant to reads). Composio consent granted so the
        // pre-existing tests keep exercising their own layer; the consent gate has its own test.
        let mut rt = ConnectorRuntime::new(FakeTransport { items, err }, Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        rt.set_composio_consent(true);
        rt
    }

    #[test]
    fn gmail_without_consent_is_denied_and_transport_untouched() {
        // Issue #80 §2-A0: a fresh runtime has no Composio consent — Gmail sync and on-demand
        // fetch are denied before the transport runs (zero egress), while first-layer Calendar
        // still syncs. This is the Linux-tested form of the macOS poller's consent skip.
        let mut rt =
            ConnectorRuntime::new(FakeTransport { items: vec![item("b")], err: None }, Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        rt.mark_connected(Service::Gmail, 100);
        rt.mark_connected(Service::GoogleCalendar, 100);
        let sink = CountingSink(RefCell::new(0));
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert!(rt.fetch_on_demand(Service::Gmail, "q", &sink).is_err());
        assert_eq!(*sink.0.borrow(), 0, "no Gmail item may be ingested without consent");
        // Denial must not poison the connection state (it is a policy refusal, not a failure).
        assert_eq!(rt.registry().state(Service::Gmail), ConnState::Connected { last_sync_ms: 100 });
        assert!(rt.sync_service(Service::GoogleCalendar, 200, &sink).is_ok());
        // Granting consent opens the path.
        rt.set_composio_consent(true);
        assert!(rt.sync_service(Service::Gmail, 300, &sink).is_ok());
    }

    #[test]
    fn a_restored_connection_shows_connected_and_syncs_on_the_next_tick() {
        // The relaunch case: the credential outlived the process, so the runtime is told to
        // restore. Without it the registry starts Disconnected and stays there, which silently
        // excludes the service from `services_due` — the read-sync never runs.
        let mut rt = runtime(vec![], None);
        rt.restore_connected(Service::GoogleCalendar);

        assert!(
            matches!(rt.registry().state(Service::GoogleCalendar), ConnState::Connected { .. }),
            "a restored service reads as connected"
        );
        assert!(
            rt.services_due(60_000, 15 * 60_000).contains(&Service::GoogleCalendar),
            "restored with no last-sync time, so it is due immediately rather than idling out one \
             more interval"
        );
        assert_eq!(
            rt.registry().state(Service::Gmail),
            ConnState::Disconnected,
            "restoring one service does not touch another"
        );
    }

    #[test]
    fn mark_connect_failed_turns_only_an_established_service_amber() {
        let mut rt = runtime(vec![], None);
        rt.mark_connect_failed(Service::GoogleCalendar);
        assert_eq!(rt.registry().state(Service::GoogleCalendar), ConnState::Disconnected);
        assert!(!rt.services_due(1_000, 0).contains(&Service::GoogleCalendar));
        rt.mark_connected(Service::GoogleCalendar, 100);
        rt.mark_connect_failed(Service::GoogleCalendar);
        assert!(rt.registry().state(Service::GoogleCalendar).is_amber());
        assert_eq!(rt.registry().state(Service::Gmail), ConnState::Disconnected);
    }

    #[test]
    fn sync_ingests_and_marks_synced() {
        let mut rt = runtime(vec![item("hello"), item("world")], None);
        rt.mark_connected(Service::Gmail, 100);
        let sink = CountingSink(RefCell::new(0));
        let report = rt.sync_service(Service::Gmail, 200, &sink).unwrap();
        assert_eq!(report, SyncReport { fetched: 2, inserted: 2 });
        assert_eq!(*sink.0.borrow(), 2);
        // freshness now reflects the sync at t=200
        assert_eq!(rt.registry().freshness_ms(Service::Gmail, 250), Some(50));
    }

    #[test]
    fn on_demand_fetch_ingests_without_touching_the_poll_schedule() {
        let mut rt = runtime(vec![item("thread body")], None);
        rt.mark_connected(Service::Gmail, 1_000);
        let before = rt.registry().freshness_ms(Service::Gmail, 5_000);
        let sink = CountingSink(RefCell::new(0));
        let report = rt.fetch_on_demand(Service::Gmail, "thread-9", &sink).unwrap();
        assert_eq!(report.inserted, 1);
        assert_eq!(*sink.0.borrow(), 1);
        // last-sync (freshness basis) is unchanged — on-demand is not a background sync.
        assert_eq!(rt.registry().freshness_ms(Service::Gmail, 5_000), before);
    }

    #[test]
    fn on_demand_transport_failure_turns_service_amber() {
        let mut rt = runtime(vec![], Some("token expired".into()));
        rt.mark_connected(Service::Gmail, 1_000);
        let sink = CountingSink(RefCell::new(0));
        assert!(rt.fetch_on_demand(Service::Gmail, "q", &sink).is_err());
        assert!(rt.registry().state(Service::Gmail).is_amber());
    }

    #[test]
    fn a_transient_transport_blip_does_not_mark_a_healthy_connection_for_reauth() {
        // A DNS failure / connection reset / 5xx is not a credential problem: the service must
        // stay Connected (green) and simply retry on the next tick — freshness is untouched, so
        // it stays due.
        let mut rt = runtime(vec![], Some("mcp request failed: connection reset".into()));
        rt.mark_connected(Service::Gmail, 100);
        let sink = CountingSink(RefCell::new(0));
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert_eq!(
            rt.registry().state(Service::Gmail),
            ConnState::Connected { last_sync_ms: 100 },
            "a network blip must not show 'Needs reauth'"
        );
        // An auth-shaped failure still goes amber.
        let mut rt = runtime(vec![], Some("mcp http 401 Unauthorized".into()));
        rt.mark_connected(Service::Gmail, 100);
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert!(rt.registry().state(Service::Gmail).is_amber());
        // Same split on the on-demand path.
        let mut rt = runtime(vec![], Some("mcp http 503 Service Unavailable".into()));
        rt.mark_connected(Service::Gmail, 100);
        assert!(rt.fetch_on_demand(Service::Gmail, "q", &sink).is_err());
        assert!(!rt.registry().state(Service::Gmail).is_amber());
        // OAuth rate limits are transient too; an expired token can be refreshed on the next tick.
        let mut rt = runtime(vec![], Some("Refresh(\"token endpoint http 429 Too Many Requests\")".into()));
        rt.mark_connected(Service::Gmail, 100);
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert!(!rt.registry().state(Service::Gmail).is_amber());
    }

    #[test]
    fn credential_failure_turns_service_amber_only() {
        let mut rt = runtime(vec![], Some("invalid_grant".into()));
        rt.mark_connected(Service::Gmail, 100);
        rt.mark_connected(Service::GoogleCalendar, 100);
        let sink = CountingSink(RefCell::new(0));
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert!(rt.registry().state(Service::Gmail).is_amber());
        // isolation: calendar is untouched (FR-INT-06)
        assert!(!rt.registry().state(Service::GoogleCalendar).is_amber());
        assert_eq!(*sink.0.borrow(), 0, "nothing ingested on failure");
    }

    #[test]
    fn oauth_refresh_http_400_marks_service_for_reauth() {
        let mut rt = runtime(
            vec![],
            Some("Refresh(\"token endpoint http 400\")".into()),
        );
        rt.mark_connected(Service::Gmail, 100);
        let sink = CountingSink(RefCell::new(0));

        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert!(rt.registry().state(Service::Gmail).is_amber());
    }

    #[test]
    fn disconnected_service_is_denied_without_state_change() {
        let mut rt = runtime(vec![item("x")], None);
        // never connected
        let sink = CountingSink(RefCell::new(0));
        let err = rt.sync_service(Service::Gmail, 200, &sink).unwrap_err();
        assert!(matches!(err, SyncFailure::Denied(_)));
        assert_eq!(rt.registry().state(Service::Gmail), ConnState::Disconnected);
    }

    #[test]
    fn services_due_respects_release_connection_and_interval() {
        let mut rt = runtime(vec![], None);
        // Slack is Wave 2 — never due at Wave 1 even if we pretend it connected.
        rt.mark_connected(Service::Gmail, 1_000);
        rt.mark_connected(Service::GoogleCalendar, 1_000);
        // Within the interval of the last sync → not due.
        assert!(rt.services_due(2_000, DEFAULT_SYNC_INTERVAL_MS).is_empty());
        // After the interval elapses, both Google services are due; Slack (unreleased) is not.
        let due = rt.services_due(1_000 + DEFAULT_SYNC_INTERVAL_MS + 1, DEFAULT_SYNC_INTERVAL_MS);
        assert!(due.contains(&Service::Gmail) && due.contains(&Service::GoogleCalendar));
        assert!(!due.contains(&Service::Slack));
    }

    struct FakeExec(RefCell<Option<(Service, String)>>);
    impl WriteExecutor for FakeExec {
        fn execute(&self, service: Service, op_name: &str, _args: Value) -> Result<Value, String> {
            *self.0.borrow_mut() = Some((service, op_name.to_string()));
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    #[test]
    fn execute_read_carries_only_background_reads() {
        // Issue #81 step 4: the tool loop's runner path. A gate-allowed read executes; anything
        // that is not a Background read — a draft (L2), a send (Composio), an unknown op — is
        // refused even though the same executor could physically perform it (invariant 4).
        let mut rt = runtime(vec![], None);
        rt.mark_connected(Service::Gmail, 1_000);
        rt.mark_connected(Service::GoogleCalendar, 1_000);
        let exec = FakeExec(RefCell::new(None));

        let out = rt.execute_read(Service::GoogleCalendar, "free_busy", serde_json::json!({}), &exec);
        assert!(out.is_ok());
        assert_eq!(
            exec.0.borrow().clone(),
            Some((Service::GoogleCalendar, "free_busy".to_string()))
        );

        *exec.0.borrow_mut() = None;
        for (svc, op) in [
            (Service::Gmail, "draft_create_update"),
            (Service::Gmail, "send"),
            (Service::GoogleCalendar, "event_create"),
            (Service::Gmail, "nope"),
        ] {
            assert!(rt.execute_read(svc, op, serde_json::json!({}), &exec).is_err(), "{op}");
            assert_eq!(*exec.0.borrow(), None, "{op} must never reach the executor");
        }
    }

    #[test]
    fn execute_read_refuses_gmail_without_consent() {
        // The consent gate composes here too: an unconsented mailbox serves no on-demand read.
        let mut rt =
            ConnectorRuntime::new(FakeTransport { items: vec![], err: None }, Wave::One, true);
        rt.mark_connected(Service::Gmail, 1_000);
        let exec = FakeExec(RefCell::new(None));
        assert!(rt.execute_read(Service::Gmail, "read_on_demand", serde_json::json!({}), &exec).is_err());
        assert_eq!(*exec.0.borrow(), None);
    }

    #[test]
    fn execute_write_runs_direct_gmail_only_after_connection_and_draft_gate() {
        let mut rt = runtime(vec![], None);
        rt.use_direct_gmail();
        let exec = FakeExec(RefCell::new(None));
        assert!(rt
            .execute_write(Service::Gmail, "send", serde_json::json!({}), &exec)
            .is_err());
        assert!(exec.0.borrow().is_none());
        rt.mark_connected(Service::Gmail, 1_000);
        rt.set_gmail_draft_stop(true);
        assert!(rt
            .execute_write(Service::Gmail, "send", serde_json::json!({}), &exec)
            .is_err());
        rt.set_gmail_draft_stop(false);
        rt.set_composio_consent(false);
        rt.execute_write(Service::Gmail, "send", serde_json::json!({}), &exec)
            .unwrap();
        assert_eq!(
            exec.0.borrow().as_ref().unwrap(),
            &(Service::Gmail, "send".to_string())
        );
    }

    #[test]
    fn busy_runtime_is_not_a_disconnect_for_the_connect_probe() {
        let shared = std::sync::Mutex::new(runtime(vec![], None));
        let revision = shared.lock().unwrap().connection_revision(Service::Gmail);
        assert!(connection_still_current(&shared, Service::Gmail, revision));
        {
            // A sync tick or the connections watch holds the lock across the network.
            let _busy = shared.lock().unwrap();
            assert!(connection_still_current(&shared, Service::Gmail, revision));
        }
        shared.lock().unwrap().disconnect(Service::Gmail, false);
        assert!(!connection_still_current(&shared, Service::Gmail, revision));
        let poisoned = std::sync::Arc::new(std::sync::Mutex::new(runtime(vec![], None)));
        let revision = poisoned.lock().unwrap().connection_revision(Service::Gmail);
        let holder = poisoned.clone();
        let _ = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("poison the runtime lock");
        })
        .join();
        assert!(!connection_still_current(&poisoned, Service::Gmail, revision));
    }

    #[test]
    fn disconnect_fence_survives_cleanup_and_runtime_replacement() {
        let mut rt = runtime(vec![], None);
        let pending = rt.connection_revision(Service::Gmail);
        rt.disconnect(Service::Gmail, false);
        assert_ne!(pending, rt.connection_revision(Service::Gmail));
        let disconnected = rt.connection_revision(Service::Gmail);
        rt.mark_connected(Service::Gmail, 1);
        assert_ne!(disconnected, rt.connection_revision(Service::Gmail));
        let replacement = runtime(vec![], None);
        assert_ne!(
            rt.connection_revision(Service::Gmail),
            replacement.connection_revision(Service::Gmail)
        );
    }

    #[test]
    fn web_tools_follow_consent_without_an_oauth_connection() {
        use shogun_mcp::tool_catalog::tool_definitions;
        let mut rt = runtime(vec![], None);
        rt.set_plan(shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Pro,
            0,
        ));
        let has_web = |rt: &ConnectorRuntime<FakeTransport>| {
            let (services, ctx) = rt.tool_loop_view();
            tool_definitions(&services, &ctx).iter().any(|t| t["name"] == "read_web_page")
        };
        assert!(!has_web(&rt));
        let before = rt.tool_auth_revision();
        rt.set_web_consent(true);
        assert!(has_web(&rt));
        assert_ne!(before, rt.tool_auth_revision());
        let granted = rt.tool_auth_revision();
        rt.set_web_consent(true);
        assert_eq!(granted, rt.tool_auth_revision());
        rt.set_web_consent(false);
        assert!(!has_web(&rt));
        assert_ne!(granted, rt.tool_auth_revision());
    }

    #[test]
    fn tool_loop_view_folds_unconsented_gmail_to_disconnected() {
        use shogun_mcp::connection::ConnState;
        use shogun_mcp::tool_catalog::tool_definitions;

        let mut rt = runtime(vec![], None);
        rt.mark_connected(Service::Gmail, 1_000);
        rt.mark_connected(Service::GoogleCalendar, 1_000);
        rt.set_composio_consent(false);
        let (services, ctx) = rt.tool_loop_view();
        let gmail = services.iter().find(|s| s.service == Service::Gmail).unwrap();
        assert_eq!(gmail.conn, ConnState::Disconnected);
        let cal = services.iter().find(|s| s.service == Service::GoogleCalendar).unwrap();
        assert!(matches!(cal.conn, ConnState::Connected { .. }));
        let tools = tool_definitions(&services, &ctx);
        assert!(
            tools.iter().all(|t| t["name"].as_str() != Some("search_mail")),
            "unconsented gmail must not publish mail tools"
        );

        rt.set_composio_consent(true);
        rt.set_plan(shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Pro,
            0,
        ));
        // Revoking disconnected Gmail (issue #781): consent alone does not bring the tools back.
        let (services, ctx) = rt.tool_loop_view();
        let tools = tool_definitions(&services, &ctx);
        assert!(
            tools.iter().all(|t| t["name"].as_str() != Some("search_mail")),
            "re-granted consent without Connect publishes no mail tools"
        );
        rt.mark_connected(Service::Gmail, 2_000);
        let (services, ctx) = rt.tool_loop_view();
        let tools = tool_definitions(&services, &ctx);
        assert!(
            tools.iter().any(|t| t["name"].as_str() == Some("search_mail")),
            "consented, connected gmail on Pro publishes mail tools"
        );
    }

    // fetch attempts are recorded before the request (issue #782)

    /// Records every fetch attempt the runtime announces, in order.
    #[derive(Default)]
    struct AttemptSink {
        attempts: RefCell<Vec<(Service, FetchKind)>>,
    }
    impl IngestSink for AttemptSink {
        fn ingest(&self, items: &[IngestItem]) -> usize {
            items.len()
        }
        fn record_fetch_attempt(&self, service: Service, kind: FetchKind) {
            self.attempts.borrow_mut().push((service, kind));
        }
    }

    #[test]
    fn a_failed_sync_still_records_its_fetch_attempt() {
        let mut rt = runtime(vec![], Some("composio http 502".into()));
        rt.mark_connected(Service::Gmail, 100);
        let sink = AttemptSink::default();
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert_eq!(
            *sink.attempts.borrow(),
            vec![(Service::Gmail, FetchKind::SyncPage)],
            "the user id and query left even though no page came back"
        );
    }

    #[test]
    fn every_page_of_a_sync_is_its_own_attempt_including_the_one_that_failed() {
        let mut rt = ConnectorRuntime::new(PagingThenFailing { calls: RefCell::new(0) }, Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        rt.set_composio_consent(true);
        rt.mark_connected(Service::Gmail, 100);
        let sink = AttemptSink::default();
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert_eq!(sink.attempts.borrow().len(), 2, "one row per request sent: the page and the failed retry");
    }

    #[test]
    fn a_failed_on_demand_fetch_records_its_attempt() {
        let mut rt = runtime(vec![], Some("composio http 500".into()));
        rt.mark_connected(Service::Gmail, 100);
        let sink = AttemptSink::default();
        assert!(rt.fetch_on_demand(Service::Gmail, "thread-9", &sink).is_err());
        assert_eq!(*sink.attempts.borrow(), vec![(Service::Gmail, FetchKind::OnDemand)]);
    }

    #[test]
    fn a_denied_read_records_no_attempt() {
        // Never connected / no consent: the gate refuses before anything is sent.
        let mut rt = runtime(vec![item("x")], None);
        let sink = AttemptSink::default();
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert!(rt.fetch_on_demand(Service::Gmail, "q", &sink).is_err());
        rt.mark_connected(Service::Gmail, 100);
        rt.set_composio_consent(false);
        assert!(rt.sync_service(Service::Gmail, 200, &sink).is_err());
        assert!(rt.fetch_on_demand(Service::Gmail, "q", &sink).is_err());
        assert!(sink.attempts.borrow().is_empty(), "nothing left the device, so nothing is recorded");
    }

    #[test]
    fn precheck_write_matches_execute_write() {
        let mut rt = runtime(vec![], None);
        // Disconnected: refused by both.
        assert!(rt.precheck_write(Service::GoogleCalendar, "event_create").is_err());
        rt.mark_connected(Service::GoogleCalendar, 100);
        assert!(rt.precheck_write(Service::GoogleCalendar, "event_create").is_ok());
        // A Composio-routed send is never a first-layer write.
        rt.mark_connected(Service::Gmail, 100);
        let err = rt.precheck_write(Service::Gmail, "send").unwrap_err();
        assert!(err.contains("Composio") || err.contains("not authorized"), "got: {err}");
    }

    // issue #781: a revocation takes effect at the next page boundary

    use std::sync::atomic::AtomicBool;

    /// Serves `pages` Gmail pages ("page 1" …), each with a token to the next. While page
    /// `revoke_during` is in flight it runs `revoke` — the user pressing Revoke mid-walk.
    struct RevokedMidWalk {
        pages: usize,
        revoke_during: usize,
        revoke: Box<dyn Fn()>,
        calls: RefCell<usize>,
    }
    impl IntegrationTransport for RevokedMidWalk {
        fn read_sync(&self, _service: Service) -> Result<Vec<FetchedItem>, String> {
            Ok(Vec::new())
        }
        fn read_sync_page(&self, _service: Service, _cursor: &SyncCursor) -> Result<FetchPage, String> {
            let n = {
                let mut calls = self.calls.borrow_mut();
                *calls += 1;
                *calls
            };
            if n == self.revoke_during {
                (self.revoke)();
            }
            let mut page = item(&format!("page {n}"));
            page.external_id = format!("m{n}");
            page.ts_ms = 6_000 + n as i64;
            let next = (n < self.pages).then(|| format!("tok{n}"));
            Ok(FetchPage { items: vec![page], next_page_token: next })
        }
    }

    /// Records what reached memory, every fetch attempt, and the cursor the runtime stores.
    #[derive(Default)]
    struct LedgerSink {
        bodies: RefCell<Vec<String>>,
        attempts: RefCell<usize>,
        cursor: RefCell<Option<SyncCursor>>,
        stored: RefCell<usize>,
        on_ingest: Option<Box<dyn Fn()>>,
    }
    impl IngestSink for LedgerSink {
        fn ingest(&self, items: &[IngestItem]) -> usize {
            self.bodies.borrow_mut().extend(items.iter().map(|i| i.body.clone()));
            if let Some(hook) = &self.on_ingest {
                hook();
            }
            items.len()
        }
        fn cursor(&self, _service: Service) -> Option<SyncCursor> {
            self.cursor.borrow().clone()
        }
        fn store_cursor(&self, _service: Service, cursor: &SyncCursor) {
            *self.stored.borrow_mut() += 1;
            *self.cursor.borrow_mut() = Some(cursor.clone());
        }
        fn record_fetch_attempt(&self, _service: Service, _kind: FetchKind) {
            *self.attempts.borrow_mut() += 1;
        }
    }

    /// A runtime with Gmail connected and consented on Pro, whose consent probe reads `flag`.
    fn consented_gmail<T>(transport: T, flag: &Arc<AtomicBool>) -> ConnectorRuntime<T> {
        let mut rt = ConnectorRuntime::new(transport, Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        rt.set_composio_consent(true);
        rt.mark_connected(Service::Gmail, 1_000);
        let probe = Arc::clone(flag);
        rt.set_consent_probe(Arc::new(move || probe.load(Ordering::SeqCst)));
        rt
    }

    fn gmail_row(rt: &ConnectorRuntime<impl Sized>) -> ConnUi {
        rt.statuses(2_000).into_iter().find(|r| r.service == Service::Gmail).map(|r| r.state).unwrap()
    }

    #[test]
    fn a_revocation_mid_sync_drops_the_page_in_flight_and_fetches_nothing_more() {
        let flag = Arc::new(AtomicBool::new(true));
        let revoke = Arc::clone(&flag);
        let transport = RevokedMidWalk {
            pages: 4,
            revoke_during: 2,
            revoke: Box::new(move || revoke.store(false, Ordering::SeqCst)),
            calls: RefCell::new(0),
        };
        let mut rt = consented_gmail(transport, &flag);
        let sink = LedgerSink::default();
        *sink.cursor.borrow_mut() = Some(SyncCursor { watermark_ms: 5_000, page_token: None });

        let out = rt.sync_service(Service::Gmail, 10_000, &sink);
        assert!(matches!(out, Err(SyncFailure::Denied(DenyReason::ConsentRequired))), "got {out:?}");
        assert_eq!(*sink.bodies.borrow(), vec!["page 1".to_string()], "the page that arrived after Revoke is dropped");
        assert_eq!(*rt.transport.calls.borrow(), 2, "no request leaves after the revocation");
        assert_eq!(*sink.attempts.borrow(), 2);
        assert_eq!(*sink.stored.borrow(), 0, "the watermark does not move past the dropped page");
        assert_eq!(sink.cursor.borrow().clone(), Some(SyncCursor { watermark_ms: 5_000, page_token: None }));
        // The runtime caught up with the revocation on its own: consent off, Gmail disconnected.
        assert_eq!(rt.registry().state(Service::Gmail), ConnState::Disconnected);
        assert_eq!(gmail_row(&rt), ConnUi::Disconnected);
        assert!(!rt.services_due(20_000, 0).contains(&Service::Gmail));
    }

    #[test]
    fn a_revocation_between_pages_stops_before_the_next_request() {
        let flag = Arc::new(AtomicBool::new(true));
        let transport = RevokedMidWalk { pages: 3, revoke_during: 0, revoke: Box::new(|| {}), calls: RefCell::new(0) };
        let mut rt = consented_gmail(transport, &flag);
        let revoke = Arc::clone(&flag);
        let sink = LedgerSink { on_ingest: Some(Box::new(move || revoke.store(false, Ordering::SeqCst))), ..Default::default() };

        assert!(matches!(
            rt.sync_service(Service::Gmail, 10_000, &sink),
            Err(SyncFailure::Denied(DenyReason::ConsentRequired))
        ));
        assert_eq!(*rt.transport.calls.borrow(), 1, "page two is never requested");
        assert_eq!(*sink.attempts.borrow(), 1, "and leaves no traceability row");
        assert_eq!(sink.bodies.borrow().len(), 1);
        assert_eq!(*sink.stored.borrow(), 0);
    }

    #[test]
    fn an_on_demand_answer_that_arrives_after_revocation_is_dropped() {
        struct RevokingFetch(Arc<AtomicBool>);
        impl IntegrationTransport for RevokingFetch {
            fn read_sync(&self, _service: Service) -> Result<Vec<FetchedItem>, String> {
                Ok(Vec::new())
            }
            fn fetch_on_demand(&self, _service: Service, _query: &str) -> Result<Vec<FetchedItem>, String> {
                self.0.store(false, Ordering::SeqCst);
                Ok(vec![item("thread body")])
            }
        }
        let flag = Arc::new(AtomicBool::new(true));
        let mut rt = consented_gmail(RevokingFetch(Arc::clone(&flag)), &flag);
        let sink = LedgerSink::default();
        assert!(matches!(
            rt.fetch_on_demand(Service::Gmail, "q", &sink),
            Err(SyncFailure::Denied(DenyReason::ConsentRequired))
        ));
        assert!(sink.bodies.borrow().is_empty());
        assert_eq!(rt.registry().state(Service::Gmail), ConnState::Disconnected);
    }

    #[test]
    fn a_label_write_back_after_revocation_does_not_run() {
        // The post-sync write-back (`label_sync`, archive) runs after the sync released the lock,
        // so it can start before Settings has refreshed the runtime. Each write re-reads consent.
        let flag = Arc::new(AtomicBool::new(true));
        let mut rt = consented_gmail(FakeTransport { items: vec![], err: None }, &flag);
        let exec = FakeExec(RefCell::new(None));
        let args = serde_json::json!({ "op": "list_labels" });
        for op in ["label_sync", "archive"] {
            assert!(rt.execute_write(Service::Gmail, op, args.clone(), &exec).is_ok(), "consented: {op} runs");
        }
        *exec.0.borrow_mut() = None;

        flag.store(false, Ordering::SeqCst);
        for op in ["label_sync", "archive", "unarchive"] {
            assert!(rt.precheck_write(Service::Gmail, op).is_err(), "{op}");
            assert!(rt.execute_write(Service::Gmail, op, args.clone(), &exec).is_err(), "{op}");
        }
        assert!(rt.precheck_read(Service::Gmail, "read_sync").is_err());
        assert_eq!(*exec.0.borrow(), None, "nothing reached Gmail after the revocation");
        // Other services never consult the mail consent.
        assert!(rt.ctx(Service::GoogleCalendar).composio_consent);
    }

    #[test]
    fn revoking_consent_disconnects_gmail_and_granting_again_needs_connect() {
        let mut rt = runtime(vec![item("b")], None);
        rt.mark_connected(Service::Gmail, 1_000);
        rt.mark_connected(Service::GoogleCalendar, 1_000);
        let revision = rt.tool_auth_revision();

        rt.set_composio_consent(false);
        assert_eq!(rt.registry().state(Service::Gmail), ConnState::Disconnected);
        assert_eq!(gmail_row(&rt), ConnUi::Disconnected);
        assert!(matches!(rt.registry().state(Service::GoogleCalendar), ConnState::Connected { .. }), "only Gmail");
        assert_ne!(rt.tool_auth_revision(), revision);

        // Granting again does not resume this session's sync: the durable flag was cleared with
        // the revocation, so after a relaunch Gmail is not connected either. Connect restores it.
        rt.set_composio_consent(true);
        assert_eq!(gmail_row(&rt), ConnUi::Disconnected);
        assert!(!rt.services_due(5_000, 0).contains(&Service::Gmail));
        let sink = CountingSink(RefCell::new(0));
        assert!(matches!(
            rt.sync_service(Service::Gmail, 5_000, &sink),
            Err(SyncFailure::Denied(DenyReason::NotConnected))
        ));
        assert_eq!(*sink.0.borrow(), 0);
        rt.mark_connected(Service::Gmail, 6_000);
        assert_eq!(gmail_row(&rt), ConnUi::Connected);
        assert!(rt.sync_service(Service::Gmail, 7_000, &sink).is_ok());
    }

    #[test]
    fn statuses_never_show_gmail_connected_without_consent() {
        // A runtime that has not been told about consent yet (a fresh one) reports no mailbox
        // connection, on the screen and in the snapshot `connections.list` reads.
        let mut rt = ConnectorRuntime::new(FakeTransport { items: vec![], err: None }, Wave::One, true);
        rt.mark_connected(Service::Gmail, 1_000);
        assert_eq!(gmail_row(&rt), ConnUi::Disconnected);
        let path = std::env::temp_dir().join(format!("shogun-781-status-{}.json", std::process::id()));
        publish_statuses(&path, 50_000, &rt.statuses(2_000)).unwrap();
        let read = shogun_mcp::connections_api::read_status(&path, 50_000).unwrap();
        assert!(read.contains(&(Service::Gmail, "disconnected")));
        let _ = std::fs::remove_file(path);
    }

    /// A scratch directory holding the three files the faces share, with Mail consented and
    /// connected the way Settings leaves it.
    fn consented_policy_dir(tag: &str) -> shogun_mcp::connections_api::ConnectionsPaths {
        let dir = std::env::temp_dir().join(format!("shogun-781-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = shogun_mcp::connections_api::ConnectionsPaths {
            composio: dir.join(shogun_mcp::composio_policy::POLICY_FILE),
            gmail: dir.join(shogun_mcp::gmail_policy::POLICY_FILE),
            status: dir.join(shogun_mcp::connections_api::STATUS_FILE),
            requests: dir.join(shogun_mcp::connections_api::REQUESTS_FILE),
        };
        shogun_mcp::composio_policy::update(&paths.composio, |p| {
            p.consent_acknowledged = true;
            p.disclosures = shogun_mcp::composio::Disclosures { via_third_party: true, data_types: true, revocable: true };
            p.gmail_connected = true;
            Ok(())
        })
        .unwrap();
        paths
    }

    #[test]
    fn a_revocation_from_settings_or_an_api_face_stops_a_sync_in_flight() {
        // Both faces write `composio.json` under its lock before they touch the runtime; the
        // persisted probe is what lets an in-flight sync see either (invariant 6).
        type Revoker = fn(&shogun_mcp::connections_api::ConnectionsPaths);
        let settings: Revoker = |paths| {
            // What the desktop's `set_composio_policy` writes for Revoke.
            shogun_mcp::composio_policy::update(&paths.composio, |p| {
                p.consent_acknowledged = false;
                p.draft_stop = true;
                p.disclosures = Default::default();
                p.gmail_connected = false;
                Ok(())
            })
            .unwrap();
        };
        let api: Revoker = |paths| {
            // `connections.composio.set {"consent": false}` from MCP / REST / CLI.
            shogun_mcp::connections_api::composio_set(paths, r#"{"consent":false}"#).unwrap();
        };
        for (tag, revoker) in [("settings", settings), ("api", api)] {
            let paths = consented_policy_dir(tag);
            let for_transport = paths.clone();
            let transport = RevokedMidWalk {
                pages: 3,
                revoke_during: 2,
                revoke: Box::new(move || revoker(&for_transport)),
                calls: RefCell::new(0),
            };
            let mut rt = ConnectorRuntime::new(transport, Wave::One, true);
            rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
            rt.set_composio_consent(true);
            rt.mark_connected(Service::Gmail, 1_000);
            rt.set_consent_probe(persisted_consent_probe(paths.composio.clone()));
            let sink = LedgerSink::default();

            assert!(rt.sync_service(Service::Gmail, 10_000, &sink).is_err(), "{tag}");
            assert_eq!(*sink.bodies.borrow(), vec!["page 1".to_string()], "{tag}");
            assert_eq!(*rt.transport.calls.borrow(), 2, "{tag}");
            assert_eq!(gmail_row(&rt), ConnUi::Disconnected, "{tag}");
            // Durable and in-memory state agree: not connected until Connect.
            let policy = shogun_mcp::composio_policy::load_effective(&paths.composio);
            assert!(!policy.consent_acknowledged && !policy.gmail_connected, "{tag}");
            let _ = std::fs::remove_dir_all(paths.composio.parent().unwrap());
        }
    }

    #[test]
    fn a_missing_policy_file_reads_as_no_consent() {
        let path = std::env::temp_dir().join(format!("shogun-781-missing-{}/composio.json", std::process::id()));
        assert!(!persisted_consent_probe(path)());
    }
}
