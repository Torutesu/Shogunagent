//! `shogun-api` — the runnable Memory API server (§6.11). Composes the daemon's DB, the DB-backed
//! Memory API backend, the shared approval queue, and the localhost REST listener into one process.
//!
//! It is the composition root: every piece was unit-tested in its own crate; here they are wired.
//! Run it with:
//!
//! ```text
//! SHOGUN_DB_PATH="$HOME/Library/Application Support/com.syogun.shogunai/memory.db" \
//!   SHOGUN_API_TOKEN=dev-token cargo run -p shogun-core --features daemon-server --bin shogun-api
//! curl -s 127.0.0.1:7464/v1/status
//! curl -s -H "Authorization: Bearer dev-token" 127.0.0.1:7464/v1/state/commitments
//! ```
//!
//! It never creates or migrates the store (#800): only the desktop app does, after its pre-migration
//! snapshot. A missing (or still-plaintext) store exits at start-up with `memory_update_pending`,
//! so point `SHOGUN_DB_PATH` at a `memory.db` the app has opened. A store at another schema version
//! answers each memory call with `memory_update_pending` / `memory_newer_than_app` and recovers
//! without a restart once the versions match.
//!
//! The key comes from `SHOGUN_DB_KEY` (hex), else the app's Keychain entry (the Mac app's build,
//! feature `desktop`, on macOS).
//!
//! Config via env: `SHOGUN_DB_PATH` (default `./shogun.db`, which does not exist unless you put
//! the app's store there, so the default alone always exits), `SHOGUN_API_TOKEN` (issues one client
//! token; without it every call is 401), `SHOGUN_API_PORT` (default 7464; a busy port
//! stops the server build from starting (threat model R3), the Mac app's build falls back to an
//! ephemeral port). The port actually bound is written to `api.port` next to the DB for `shogun` to find.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use shogun_agents::approval::ApprovalQueue;
use shogun_core::daemon::{Clock, DaemonDb, Db};
use shogun_core::db_backend::DbBackend;
use shogun_core::metrics::{
    render_snapshots_json_with_shared_service, SanitizerCounters, SloRegistry,
};
use shogun_mcp::live_tokens::{revision_path, LiveTokens};
use shogun_mcp::mcp::McpServer;
use shogun_mcp::memory_api::TokenRegistry;
use shogun_mcp::memory_api_settings;
use shogun_mcp::startup_report;
use shogun_mcp::server::{bind_exact, serve_on, AppState, MetricsSource, DEFAULT_PORT};

/// The live SLO metrics source served at `GET /v1/metrics` (NFR-SLO-00). Wraps the shared
/// registry the runtime records into; here it starts empty, so every SLO reads as unmeasured until
/// the notch runtime populates it (silence ≠ success, spec §4.5). The D-6 lesson counters
/// (active lessons, feedback in the last 7 days) come from the DB; an unreadable DB renders
/// `lessons.measured:false` in the same convention. H1 harness counters (compression + tool loop)
/// ride next to them: unmeasured until an assemble or loop has run; never prompt text.
struct RegistryMetrics {
    registry: Arc<Mutex<SloRegistry>>,
    db: Db,
}

impl MetricsSource for RegistryMetrics {
    fn snapshot_json(&self) -> String {
        let lessons = self.db.lesson_counters();
        let harness = self.db.harness_counters();
        let patterns = self.db.pattern_counters();
        // Issue #29 §4-6: shared-service round trips, read from the DB the desktop records into.
        let shared = self.db.shared_service_metrics();
        let snap = shogun_memory::sanitize::snapshot();
        let sanitizer = SanitizerCounters {
            events_stripped: snap.events_stripped,
            chars_removed: snap.chars_removed,
            instruction_shaped_dropped: snap.instruction_shaped_dropped,
        };
        self.registry
            .lock()
            .map(|r| {
                render_snapshots_json_with_shared_service(
                    &r.snapshot_all(),
                    lessons,
                    harness,
                    sanitizer,
                    patterns,
                    shared.as_deref(),
                )
            })
            .unwrap_or_else(|_| {
                r#"{"metrics":[],"lessons":{"measured":false},"harness":{"measured":false},"sanitizer":{"events_stripped":0,"chars_removed":0,"instruction_shaped_dropped":0},"patterns":{"measured":false},"shared_service":{"measured":false}}"#
                    .to_string()
            })
    }
}

/// Build the metrics source for the API process (empty SLO registry + DB-backed lesson counters).
fn metrics_source(db: Db) -> Arc<dyn MetricsSource> {
    Arc::new(RegistryMetrics {
        registry: Arc::new(Mutex::new(SloRegistry::with_core_overlay())),
        db,
    })
}

/// A real wall-clock in unix ms (never panics; 0 before the epoch).
fn wall_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    })
}

fn visual_recall_settings_path(db_path: &str) -> Option<std::path::PathBuf> {
    std::env::var("SHOGUN_VISUAL_RECALL_SETTINGS")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::path::Path::new(db_path)
                .parent()
                .map(|p| p.join("visual_recall.json"))
        })
}

fn db_backend(db: Db) -> DbBackend {
    // `dream.status` reads the same last-outcome record the desktop's driver writes (#285).
    let db = db.with_dream_outcome_path(shogun_mcp::plan_source::dream_outcome_json_path());
    let db_path = std::env::var("SHOGUN_DB_PATH").unwrap_or_else(|_| "./shogun.db".to_string());
    let mut backend = DbBackend::new(db).with_forms_db_path(std::path::PathBuf::from(&db_path));
    if let Some(path) = shogun_core::user_config::default_path() {
        backend = backend.with_user_config_path(path);
    }
    if let Some(path) = visual_recall_settings_path(&db_path) {
        backend = backend.with_visual_recall_settings_path(path);
    }
    if let Some(path) = std::path::Path::new(&db_path).parent().map(|p| p.join("exclusions.json")) {
        backend = backend.with_capture_exclusions_path(path);
    }
    backend
        .with_memory_api_settings_path(memory_api_settings::resolve_settings_path(&db_path))
        // `api.connections.restrict` (owner decision 2026-10-20): withhold meeting notes and
        // screen recall from one connected app. Only narrows, so a token may do it.
        .with_connection_restrictor(shogun_core::db_backend::keychain_connection_restrictor(
            memory_api_settings::resolve_settings_path(&db_path),
        ))
        // The Agent lane's source (issue #110): the desktop's llm.json, presence-only probes.
        .with_agent_source(shogun_core::db_backend::AgentSourceConfig::for_daemon(&db_path))
        // Snapshot retention (issue #647): the desktop's backup.json and backups/.
        .with_backup_retention(shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path))
        // The Connections screen (issue #82): the desktop's published states, composio.json and
        // the disconnect requests it applies. No Keychain access from here.
        .with_connections(shogun_mcp::connections_api::ConnectionsPaths::for_db(&db_path))
        // Mid-run steering (issue #395): the store the desktop's runner reads at each turn.
        .with_steer_store(shogun_mcp::steer_api::resolve_path(&db_path))
        // "Compact search index" (#714 step 3): requests for the desktop's job, its status.
        .with_search_index(shogun_core::db_backend::SearchIndexConfig::for_daemon(&db_path))
        // `dream.status` actions (#285): plan and licence-token presence, re-read per answer.
        .with_dream_action_facts(shogun_core::db_backend::dream_action_facts_for_daemon)
}

/// The token list: the app's Keychain item in the Mac app's build (feature `desktop`, read
/// uncached — `shogun_core::desktop::tokens`, issue #744), else one client token from
/// `SHOGUN_API_TOKEN`.
fn load_token_registry() -> Result<TokenRegistry, String> {
    #[cfg(feature = "desktop")]
    {
        shogun_core::desktop::tokens::load_token_registry()
    }
    #[cfg(not(feature = "desktop"))]
    {
        let mut tokens = TokenRegistry::new();
        if let Ok(token) = std::env::var("SHOGUN_API_TOKEN") {
            if !token.is_empty() {
                tokens.issue(token);
            }
        }
        Ok(tokens)
    }
}

/// The name every stderr line starts with.
const FACE: &str = "shogun-api";

/// The principal root when this process runs multi-principal (E2-T2): `--multi [<root>]` on the
/// command line (the root then defaults to `SHOGUN_MULTI_ROOT`, else `./principals`), or
/// `SHOGUN_MULTI_ROOT` alone. `None` is the single-principal server, unchanged.
fn multi_root(args: impl IntoIterator<Item = String>) -> Option<std::path::PathBuf> {
    let mut args = args.into_iter();
    let env_root = std::env::var_os("SHOGUN_MULTI_ROOT")
        .filter(|root| !root.is_empty())
        .map(std::path::PathBuf::from);
    while let Some(arg) = args.next() {
        if arg == "--multi" {
            return args
                .next()
                .filter(|next| !next.starts_with("--"))
                .map(std::path::PathBuf::from)
                .or(env_root)
                .or_else(|| Some(std::path::PathBuf::from("./principals")));
        }
    }
    env_root
}

/// Exit unless the Memory API is on, naming the settings file checked and why it failed
/// (issue #744). The decision and the exit status (1) are unchanged.
fn gate_or_exit(db_path: &str, db_env_set: bool) {
    let settings_path = memory_api_settings::resolve_settings_path(db_path);
    if memory_api_settings::require_enabled(&settings_path).is_err() {
        let db = startup_report::DbPathFacts::of(
            std::path::Path::new(db_path),
            db_env_set,
            shogun_mcp::plan_source::app_data_dir().as_deref(),
        );
        let refusal = startup_report::settings_refusal(
            FACE,
            startup_report::settings_state(&settings_path),
            &settings_path,
            &db,
            startup_report::home_dir().as_deref(),
        );
        eprintln!("{refusal}");
        std::process::exit(1);
    }
}

/// The principal stores' key provider (E26): `--key-provider env|aws-kms|gcp-kms|vault`, else
/// `SHOGUN_KEY_PROVIDER`, else `env` (`SHOGUN_DB_KEY` / `SHOGUN_DB_KEY_<PRINCIPAL>`). A KMS provider
/// keeps each principal's wrapped data key next to its store under `root` and reads its settings
/// from the environment once (`deploy/README.md` lists every variable). A bad setting exits here,
/// naming the variable; a KMS that is down later makes that principal's store refuse to open.
fn principal_keys(root: &std::path::Path) -> Arc<dyn shogun_core::tenancy::KeyProvider> {
    use shogun_core::tenancy::{provider_from_env, KeyProviderKind, TenancyError};
    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    let exit = |e: TenancyError| -> ! {
        eprintln!("{FACE}: {e}");
        std::process::exit(1);
    };
    let kind = KeyProviderKind::select(std::env::args().skip(1), &vars).unwrap_or_else(|e| exit(e));
    let keys = provider_from_env(kind, root, &vars).unwrap_or_else(|e| exit(e));
    eprintln!("{FACE}: key provider: {}", kind.as_str());
    keys
}

/// Multi-principal mode (ShogunAgent E2-T2): `--multi` on the command line, or `SHOGUN_MULTI_ROOT`
/// set. One process serves `<root>/<principal>/memory.db` for every principal the bearer map
/// names, opening each file on demand (`SHOGUN_MULTI_MAX_OPEN`, default 64) with the key from
/// `SHOGUN_DB_KEY_<PRINCIPAL>` or `SHOGUN_DB_KEY`. Tokens come from
/// `SHOGUN_PRINCIPAL_TOKENS="principal:token[:agent_id],…"` until `apps/api` issues agent keys
/// (E3). None of the desktop gates (memory_api.json, plan files, Keychain, heartbeat) apply.
/// REST and the MCP face (`POST /mcp`) both resolve the principal per request from the bearer.
async fn serve_multi(root: std::path::PathBuf) -> std::io::Result<()> {
    use shogun_core::tenancy::{ChainedResolver, DerivedBearers, PrincipalStore, TokenPrincipalMap};
    let keys = principal_keys(&root);
    let listed = TokenPrincipalMap::from_env().unwrap_or_else(|e| {
        eprintln!("{FACE}: {e}");
        std::process::exit(1);
    });
    // E34 / threat model R4: the gateway's derived `sgw1` bearers, verified against the shared
    // secret. With it, `SHOGUN_PRINCIPAL_TOKENS` is not needed at all.
    let derived = DerivedBearers::from_env().unwrap_or_else(|e| {
        eprintln!("{FACE}: {e}");
        std::process::exit(1);
    });
    if listed.is_empty() && derived.is_none() {
        eprintln!(
            "{FACE}: no principal tokens loaded ({} or {}) — every memory call is 401 until one is set",
            DerivedBearers::ENV_VAR,
            TokenPrincipalMap::ENV_VAR
        );
    } else if derived.is_some() {
        eprintln!("{FACE}: derived gateway bearers accepted ({})", DerivedBearers::ENV_VAR);
    }
    let resolver = ChainedResolver { listed, derived };
    let max_open = std::env::var("SHOGUN_MULTI_MAX_OPEN")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64);
    let clock = wall_clock();
    let (embedder, line) = shogun_core::embed_models::load_configured();
    eprintln!("{FACE}: {line}");
    let keys: Arc<dyn shogun_core::tenancy::KeyProvider> = keys;
    // One connector token store (`SHOGUN_TOKEN_STORE_DIR`, else `<root>/.tokens`) for the
    // executor's sends and the operator's purge (E39), which removes a principal's entries with
    // its store.
    let credentials = principal_credentials(&root, keys.clone());
    // E50: the gateway's connector routes over the same store (a vendor token per router run, the
    // hosted connect flow, status and disconnect), behind SHOGUN_ADMIN_TOKEN and the principal's
    // own bearer. Every connect, refresh and disconnect is a traceability row in the principal's
    // own store (threat model R10); the sink finds the store once it exists.
    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    let audit_store: Arc<std::sync::OnceLock<std::sync::Weak<PrincipalStore>>> = Arc::default();
    let processing_interval = shogun_core::tenancy::processing_runtime::interval_from_vars(&vars)
        .map_err(|name| std::io::Error::other(format!("{name} must be 0 (off) or 30..3600 seconds")))?;
    let audit = shogun_core::tenancy::multi::connector_audit_sink(audit_store.clone());
    let connectors = shogun_core::tenancy::multi::hosted_connectors(credentials.clone(), &vars, Some(audit)).unwrap_or_else(|var| {
        eprintln!("{FACE}: {var} is malformed (see deploy/README.md, hosted connector credentials)");
        std::process::exit(1);
    });
    // Each principal's backend serves connections.list / connections.disconnect from the vault and
    // queues provider change notices for the read-sync (E44).
    let vault: Arc<dyn shogun_mcp::connector_vault::ConnectorVault> = connectors.clone();
    let store = Arc::new(
        PrincipalStore::new(root.clone(), keys.clone(), clock.clone(), max_open)
            .with_embedder(embedder)
            .with_connectors(Some(vault)),
    );
    let _ = audit_store.set(Arc::downgrade(&store));
    let executor = approval_executor(credentials.clone(), store.clone());
    let connectable = connectors.clients().connectable();
    eprintln!(
        "{FACE}: connector credentials served to the gateway (connect flow: {})",
        if connectable.is_empty() { "off: SHOGUN_CONNECT_REDIRECT_URI or the clients unset".to_string() } else { connectable.join(", ") }
    );
    let state = shogun_core::tenancy::multi::app_state_with_credentials(store.clone(), Arc::new(resolver), clock.clone(), Some(credentials.clone()))
        .with_approval_executor(executor)
        .with_connector_vault(connectors.clone());
    let port = std::env::var("SHOGUN_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    // Threat model R3: the hosted sidecar never moves to another port when its own is taken.
    let listener = bind_exact_or_exit(port).await;
    let addr = listener.local_addr()?;
    println!(
        "shogun-memory-api listening on http://{addr}  (multi-principal root: {}, max open: {max_open})",
        root.display()
    );
    let worker = processing_interval.map(|interval| tokio::spawn(
        shogun_core::tenancy::processing_runtime::run(store, credentials, clock, interval, connectors),
    ));
    let result = serve_on(listener, state.with_port(addr.port())).await;
    if let Some(worker) = worker { worker.abort(); }
    result
}

/// Bind exactly `port` on loopback or end the process with a message that names the port and the
/// fix (threat model R3): a server build never answers on a port other than the one configured.
async fn bind_exact_or_exit(port: u16) -> tokio::net::TcpListener {
    match bind_exact(port).await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "{FACE}: port {port} on 127.0.0.1 is already in use, so this server will not start. Stop the process that holds it, or set SHOGUN_API_PORT to a free port (and point the gateway's SHOGUN_SIDECAR_URL at it). It never moves to another port on its own."
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{FACE}: could not listen on 127.0.0.1:{port} ({}).", e.kind());
            std::process::exit(1);
        }
    }
}

/// The principals' connector token store in multi-principal mode: `SHOGUN_TOKEN_STORE_DIR`, else
/// `<root>/.tokens`, each file sealed with its principal's key.
fn principal_credentials(
    root: &std::path::Path,
    keys: Arc<dyn shogun_core::tenancy::KeyProvider>,
) -> Arc<dyn shogun_integrations::TokenStore> {
    use shogun_core::tenancy::credentials::{local_store, token_dir, TOKEN_STORE_DIR_VAR};
    Arc::new(local_store(
        token_dir(root, std::env::var_os(TOKEN_STORE_DIR_VAR).map(std::path::PathBuf::from)),
        keys,
    ))
}

/// The executor of remotely approved actions in multi-principal mode (E15 follow-up): each
/// principal's connector grants from the local token store ([`principal_credentials`]), the action
/// traced into the principal's own store before it leaves. A principal with no grant for the
/// action's service gets `send_failed` / `not_connected`, and nothing is sent.
fn approval_executor(
    tokens: Arc<dyn shogun_integrations::TokenStore>,
    store: Arc<shogun_core::tenancy::PrincipalStore>,
) -> Arc<dyn shogun_mcp::approval_exec::ApprovalExecutor> {
    use shogun_core::approval_exec::{ConnectorTransports, SendExecExecutor, SinkFor};
    use shogun_core::tenancy::PrincipalId;
    let payment_store = store.clone();
    let sinks: SinkFor = Arc::new(move |principal: Option<&str>| {
        let id = PrincipalId::parse(principal?).ok()?;
        let open = store.open(&id).ok()?;
        Some(Box::new(open.db().traceability_sink()) as Box<dyn shogun_core::llm::traceability::TraceabilitySink>)
    });
    // E46: an approved mandate grant or payment is applied in the principal's own store.
    let stores: shogun_core::approval_exec::StoreFor = Arc::new(move |principal: Option<&str>| {
        let id = PrincipalId::parse(principal?).ok()?;
        let open = payment_store.open(&id).ok()?;
        Some(open.db().clone())
    });
    Arc::new(SendExecExecutor::new(Arc::new(ConnectorTransports::new(tokens)), sinks).with_payments(stores))
}

/// How many backlog rows one embedding pass handles, and how often the pass runs.
const EMBED_BATCH: usize = 256;
const EMBED_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Attach the local embedding model when this build has `embed-local` and a fetched model loads
/// (E24), and embed the backlog off the request path on a timer. Otherwise search stays lexical;
/// either way one line on stderr says which.
fn attach_local_embedder(db: Db) -> Db {
    let (embedder, line) = shogun_core::embed_models::load_configured();
    eprintln!("{FACE}: {line}");
    let Some(embedder) = embedder else {
        return db;
    };
    let db = db.with_embedder(embedder);
    let worker = db.clone();
    std::thread::spawn(move || loop {
        while worker.embed_pending(EMBED_BATCH) > 0 {}
        std::thread::sleep(EMBED_EVERY);
    });
    db
}

/// `shogun-api --dream-tick [--multi <root>]` (plan E5-T4 / E5-T5): run the hourly Dream Cycle
/// once for every principal under the multi-principal root (`--multi <root>`, else
/// `SHOGUN_MULTI_ROOT`, else `./principals`) and exit. A principal with no new material since its
/// cursor is not reviewed and costs no model call. A principal whose token store
/// (`SHOGUN_TOKEN_STORE_DIR`, else `<root>/.tokens`, sealed with the principal's own key) holds a
/// Batch relay credential is reviewed through the relay with it, and then given the loop
/// consolidator's turn on the same relay lane when it is due (E49: unread signals or outcomes, at
/// most hourly and eight times a local day); every other principal with the on-device local rules
/// (no model, no egress, no consolidator). Prints one line of counts, never content.
///
/// Before the review, the tick drains each principal's connector read-sync queue (E44): the
/// Gmail, Calendar and Notion change notices `memory.ingest_webhook` queued are fetched with the
/// principal's own connector credentials (refreshed through the vault with the hosted clients of
/// `SHOGUN_CONNECT_*`) and stored in the principal's file, so the same tick reviews them.
async fn dream_tick(root: std::path::PathBuf) -> std::io::Result<()> {
    let _guard = shogun_core::tenancy::processing_runtime::acquire(&root).map_err(std::io::Error::other)?;
    use shogun_core::tenancy::credentials::{local_store, token_dir, TOKEN_STORE_DIR_VAR};
    use shogun_core::tenancy::{dream_tick, PrincipalStore};
    let keys = principal_keys(&root);
    let clock = wall_clock();
    let now = (clock)();
    let tokens = local_store(
        token_dir(&root, std::env::var_os(TOKEN_STORE_DIR_VAR).map(std::path::PathBuf::from)),
        keys.clone(),
    );
    // One file open at a time: the tick visits each principal once. Same open policy as
    // `--multi` (the sidecar owns these files); only files already on disk are visited.
    // The tick also embeds each visited principal's backlog when a local model is installed (E24).
    let (embedder, line) = shogun_core::embed_models::load_configured();
    eprintln!("{FACE}: {line}");
    let store = Arc::new(PrincipalStore::new(root, keys, clock, 1).with_embedder(embedder));
    let principals = dream_tick::principals_on_disk(&store);
    // E44 read-sync, before the review: one store read per principal when nothing is queued.
    let tokens: Arc<dyn shogun_integrations::TokenStore> = Arc::new(tokens);
    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    let audit_store: Arc<std::sync::OnceLock<std::sync::Weak<PrincipalStore>>> = Arc::default();
    let _ = audit_store.set(Arc::downgrade(&store));
    match shogun_core::tenancy::multi::hosted_connectors(tokens.clone(), &vars, Some(shogun_core::tenancy::multi::connector_audit_sink(audit_store))) {
        Ok(vault) => {
            let (store, principals) = (store.clone(), principals.clone());
            // A blocking reqwest client owns a runtime: create and drop it on the
            // blocking worker too, not just the requests it executes.
            let sync = tokio::task::spawn_blocking(move || {
                shogun_core::provider_fetch::HttpProviderFetch::new().map(|http| {
                    shogun_core::tenancy::connector_sync::drain_tick(&store, &vault, &http, &principals, now)
                })
            })
            .await
            .map_err(std::io::Error::other)?;
            match sync {
                Ok(sync) => println!("{FACE}: connector read-sync: {}", shogun_core::tenancy::connector_sync::summary(&sync)),
                Err(_) => eprintln!("{FACE}: connector read-sync skipped (http client unavailable)"),
            }
        }
        Err(var) => eprintln!("{FACE}: connector read-sync skipped ({var} is malformed)"),
    }
    let report = shogun_core::tenancy::processing_runtime::tick(&store, tokens.as_ref(), now, true).await;
    println!("{FACE}: dream tick: {}", dream_tick::summary(&report));
    Ok(())
}

/// `shogun-api --store-relay-credential <principal> [--multi <root>]` (E5-T5): read the
/// principal's Batch relay bearer from stdin (never from argv, which other users can read in the
/// process table) and seal it into the local token store `--dream-tick` reads. An empty line
/// removes it. Automatic processing waits for a credential; explicit ticks can use local rules
/// when no unfinished model review is present.
fn store_relay_credential(root: std::path::PathBuf, principal: &str) -> std::io::Result<()> {
    use shogun_core::llm::Secret;
    use shogun_core::tenancy::credentials::{local_store, token_dir, TOKEN_STORE_DIR_VAR};
    use shogun_core::tenancy::PrincipalId;
    use shogun_integrations::{CredentialSlot, TokenStore};
    let principal = PrincipalId::parse(principal).unwrap_or_else(|e| {
        eprintln!("{FACE}: {e}");
        std::process::exit(2);
    });
    let keys = principal_keys(&root);
    let store = local_store(
        token_dir(&root, std::env::var_os(TOKEN_STORE_DIR_VAR).map(std::path::PathBuf::from)),
        keys,
    );
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let value = Secret::new(line.trim());
    let result = if value.is_empty() {
        store.delete(principal.as_str(), CredentialSlot::BatchRelay)
    } else {
        store.put(principal.as_str(), CredentialSlot::BatchRelay, &value)
    };
    match result {
        Ok(()) if value.is_empty() => println!("{FACE}: relay credential removed"),
        Ok(()) => println!("{FACE}: relay credential stored (sealed)"),
        Err(e) => {
            eprintln!("{FACE}: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// `shogun-api --reinstate-principal <id> [--multi <root>]` (E39): lift the tombstone a purge
/// left, the operator's deliberate step before a deleted principal id is provisioned again. The
/// principal's next request starts an empty store under a fresh key; nothing of the purged store,
/// its key or its connector credentials comes back. Needs no key and opens no store.
fn reinstate_principal(root: std::path::PathBuf, principal: &str) -> std::io::Result<()> {
    use shogun_core::tenancy::{EnvKeyProvider, PrincipalId, PrincipalStore};
    let principal = PrincipalId::parse(principal).unwrap_or_else(|e| {
        eprintln!("{FACE}: {e}");
        std::process::exit(2);
    });
    let store = PrincipalStore::new(root, Arc::new(EnvKeyProvider::default()), wall_clock(), 1);
    match store.reinstate(&principal) {
        Ok(true) => println!("{FACE}: principal {} reinstated; its next request starts an empty store", principal.as_str()),
        Ok(false) => println!("{FACE}: principal {} was not purged; nothing to do", principal.as_str()),
        Err(e) => {
            eprintln!("{FACE}: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    if let Some(principal) =
        std::env::args().skip(1).skip_while(|arg| arg != "--reinstate-principal").nth(1)
    {
        let root = multi_root(std::env::args().skip(1)).unwrap_or_else(|| std::path::PathBuf::from("./principals"));
        return reinstate_principal(root, &principal);
    }
    if let Some(principal) =
        std::env::args().skip(1).skip_while(|arg| arg != "--store-relay-credential").nth(1)
    {
        let root = multi_root(std::env::args().skip(1)).unwrap_or_else(|| std::path::PathBuf::from("./principals"));
        return store_relay_credential(root, &principal);
    }
    if std::env::args().skip(1).any(|arg| arg == "--dream-tick") {
        let root = multi_root(std::env::args().skip(1)).unwrap_or_else(|| std::path::PathBuf::from("./principals"));
        return dream_tick(root).await;
    }
    if let Some(root) = multi_root(std::env::args().skip(1)) {
        return serve_multi(root).await;
    }
    // Audit 2026-10-20 M7: plan resolution reads the Keychain mirrors of the trial stamp and the
    // time high-water mark, so editing onboarding.json / billing.json cannot stretch the plan.
    // The Mac app's build only (feature `desktop`); the server has no Keychain to mirror.
    #[cfg(feature = "desktop")]
    shogun_core::plan_mirrors::install();
    let db_env = std::env::var("SHOGUN_DB_PATH").ok();
    let db_env_set = db_env.is_some();
    let db_path = db_env.unwrap_or_else(|| "./shogun.db".to_string());
    // Audit 2026-10-20 M6: check memory_api.json and the capture settings against the seals the
    // app writes, before the gate reads the first of them. This face never adopts an unsealed
    // file; the app does that on its first run after the upgrade.
    if let Some(key) = shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path)
        .key
        .and_then(|load| load())
    {
        shogun_core::settings_seal::install_from_db_key(&key, false);
    }
    gate_or_exit(&db_path, db_env_set);
    let port = std::env::var("SHOGUN_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let clock = wall_clock();
    // Never migrates (#800): only the desktop upgrades the store, after its verified snapshot.
    let store = DaemonDb::open(&db_path, clock.clone()).unwrap_or_else(|e| {
        eprintln!("{FACE}: open db {db_path}: {e}");
        std::process::exit(1);
    });
    if let Some(why) = store.unavailable() {
        eprintln!("{FACE}: {}", why.line());
    }
    let db = attach_local_embedder(store.db().clone());
    let metrics = metrics_source(db.clone());
    let backend = Arc::new(db_backend(db));

    // Issue #400: the token list is live. A token revoked in Settings (or with "Remove from …")
    // is 401 on its next request, and a new one works at once, with no restart. The first read
    // still fails the start-up if the Keychain cannot be read, as before.
    let live_tokens = Arc::new(LiveTokens::watching(
        revision_path(&memory_api_settings::resolve_settings_path(&db_path)),
        load_token_registry,
    ));
    let tokens = live_tokens.current();
    if tokens.is_unavailable() {
        // The reason the one failed read gave (a Keychain status): no second read, no prompt.
        let why = live_tokens.last_error().unwrap_or_default();
        eprintln!("{}", startup_report::tokens_unavailable(FACE, &why));
        std::process::exit(1);
    }
    if tokens.is_empty() {
        eprintln!("{FACE}: no Memory API tokens loaded yet — tool calls are 401 until one is issued in Settings (only /v1/status is open)");
    }

    let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
    // Desktop builds re-resolve the signed plan on every request. OSS builds use the
    // same server entitlement policy as --multi, without consulting desktop plan files.
    let plan_source = shogun_core::daemon::api_entitlements::ApiEntitlementSource::for_build();
    let plan_clock = clock.clone();
    // Issue #744: say once why a locked plan will answer 403, and name it in every 403 body.
    if let Some(lock) = plan_source.lock(u64::try_from((clock)()).unwrap_or(0)) {
        eprintln!("{}", startup_report::plan_lock_note(FACE, lock));
    }
    let lock_source = plan_source.clone();
    let lock_clock = clock.clone();
    let approvals_path = shogun_mcp::approval_store::resolve_store_path(&db_path);
    // Pending L3 bodies are sealed under a key derived from the memory DB key: the
    // same key this process opened the store with, through the daemons' loader (`SHOGUN_DB_KEY`,
    // else the Keychain). Without it, this face can poll but refuses to queue a new L3 request.
    let approval_seal = shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path)
        .key
        .and_then(|load| load())
        .map(|key| shogun_mcp::approval_store::ApprovalSealKey::from_db_key(&key));
    if approval_seal.is_none() {
        eprintln!("{FACE}: memory key unavailable for the approval store; L3 requests from this face will be refused");
    }
    let approval_store = shogun_mcp::approval_store::ApprovalStore::new(approvals_path, approval_seal);
    let heartbeat_path = shogun_mcp::desktop_heartbeat::resolve_path(&db_path);
    // 2026-10-20 cost audit: model-starting calls are limited per token, in a window shared
    // with the stdio faces through a file beside the memory DB. One limiter for both faces here.
    let llm_limiter = Arc::new(shogun_mcp::rate_limit::LlmRateLimiter::shared(
        shogun_mcp::rate_limit::resolve_store_path(&db_path),
    ));
    // E4-T1: the MCP face at `POST /mcp` (Streamable HTTP, stateless) over the same backend,
    // token list, approval queue, plan gate and store gate as REST. Every request is authenticated
    // by its own bearer against the live list (`handle_line_as`); there is no process session.
    // E15: one remote-approval configuration (base URL, signing key, ledger) for both faces, so an
    // approval queued over MCP and decided over REST is one record.
    let remote = shogun_mcp::remote_approvals::RemoteApprovals::from_env();
    let mcp = {
        let clock = clock.clone();
        let plan_source = plan_source.clone();
        let plan_clock = clock.clone();
        let lock_source = plan_source.clone();
        let lock_clock = clock.clone();
        let store = store.clone();
        let heartbeat_path = heartbeat_path.clone();
        McpServer::new(backend.clone(), approvals.clone(), move || (clock)(), move || {
            plan_source.resolve(u64::try_from((plan_clock)()).unwrap_or(0))
        })
        .with_live_session(live_tokens.clone(), None, true)
        .with_plan_lock(move || lock_source.lock(u64::try_from((lock_clock)()).unwrap_or(0)))
        .with_memory_gate(move || store.unavailable())
        .with_approval_store(approval_store.clone())
        .with_remote_approvals(remote.clone())
        .with_llm_rate_limiter(llm_limiter.clone())
        .with_desktop_running_check(move || {
            shogun_mcp::desktop_heartbeat::fresh(
                &heartbeat_path,
                shogun_mcp::desktop_heartbeat::now_ms(),
            )
        })
    };
    let state = AppState::new(tokens, backend, approvals, clock)
        .with_live_tokens(live_tokens)
        .with_mcp(Arc::new(mcp))
        .with_metrics(metrics)
        .with_entitlements(Arc::new(move || {
            plan_source.resolve(u64::try_from((plan_clock)()).unwrap_or(0))
        }))
        .with_plan_lock(move || lock_source.lock(u64::try_from((lock_clock)()).unwrap_or(0)))
        // #800: every memory request re-checks the store's schema; no restart after an upgrade.
        .with_memory_gate(move || store.unavailable())
        .with_approval_store(approval_store)
        .with_remote_approvals(remote)
        .with_llm_rate_limiter(llm_limiter)
        .with_desktop_running_check(move || {
            shogun_mcp::desktop_heartbeat::fresh(
                &heartbeat_path,
                shogun_mcp::desktop_heartbeat::now_ms(),
            )
        });

    // The Mac app's server moves to an ephemeral port when 7464 is busy (FR-API-01) and records
    // it in `api.port`; the server build refuses to start instead (threat model R3).
    #[cfg(feature = "desktop")]
    let listener = shogun_mcp::server::bind_local(port).await?;
    #[cfg(not(feature = "desktop"))]
    let listener = bind_exact_or_exit(port).await;
    let addr = listener.local_addr()?;
    if addr.port() != port && port != 0 {
        eprintln!(
            "{FACE}: port {port} is in use, so this server is on {} instead; `shogun api status` reports it.",
            addr.port()
        );
    }
    // Issue #776: record the port actually bound, so the CLI dials this server rather than
    // guessing 7464 and handing its token to whatever process holds that port.
    let port_file = shogun_mcp::api_port::port_file_path(std::path::Path::new(&db_path));
    if let Err(error) = shogun_mcp::api_port::write(&port_file, addr.port()) {
        eprintln!("{FACE}: could not record the port for `shogun` ({}); set SHOGUN_API_PORT={} for the CLI.", error.kind(), addr.port());
    }
    println!("shogun-memory-api listening on http://{addr}  (db: {db_path})");
    serve_on(listener, state.with_port(addr.port())).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Boot the real server on a background thread (own runtime) and return its port. Uses an
    /// in-memory DB seeded with nothing; the token `dev` is issued.
    fn boot_server() -> u16 {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let db = Db::open_in_memory(wall_clock()).unwrap();
                let metrics = metrics_source(db.clone());
                let backend = Arc::new(db_backend(db));
                let mut tokens = TokenRegistry::new();
                tokens.issue("dev");
                let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
                let state = AppState::new(Arc::new(tokens), backend, approvals, wall_clock())
                    .with_entitlements(Arc::new(|| {
                        shogun_agents::entitlement::entitlements(
                            shogun_agents::entitlement::Plan::Pro,
                            0,
                        )
                    }))
                    .with_metrics(metrics);
                let listener = bind_exact(0).await.unwrap();
                let port = listener.local_addr().unwrap().port();
                tx.send(port).unwrap();
                let _ = serve_on(listener, state).await;
            });
        });
        let port = rx.recv().unwrap();
        // give the listener a moment to start accepting
        std::thread::sleep(Duration::from_millis(300));
        port
    }

    #[test]
    fn cli_client_drives_the_real_server_end_to_end() {
        use shogun_cli::http::request;
        let port = boot_server();

        // status is open
        let r = request(port, "GET", "/v1/status", None, None).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body.contains("shogun-memory-api"));

        // a tool call without a token is 401 (FR-API-03)
        let r = request(port, "GET", "/v1/state/people", None, None).unwrap();
        assert_eq!(r.status, 401);

        // write a note, then search it back — the full write→persist→read loop over the socket
        let r = request(
            port,
            "POST",
            "/v1/memory/notes",
            Some("dev"),
            Some("call Bob about the roadmap"),
        )
        .unwrap();
        assert_eq!(r.status, 202);
        assert!(r.body.contains("\"note_id\":"));

        let r = request(
            port,
            "GET",
            "/v1/memory/search?q=roadmap",
            Some("dev"),
            None,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert!(
            r.body.contains("call Bob about the roadmap"),
            "search body: {}",
            r.body
        );

        // the in-product SLO snapshot is served, open like status (NFR-SLO-00); empty registry ⇒
        // every SLO reads unmeasured, never a false green (spec §4.5).
        let r = request(port, "GET", "/v1/metrics", None, None).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"metrics\":"), "metrics body: {}", r.body);
        assert!(r.body.contains("NFR-SLO-01"), "metrics body: {}", r.body);
        assert!(
            r.body.contains("\"measured\":false"),
            "unmeasured SLOs must not read as pass: {}",
            r.body
        );
        // the D-6 lesson counters ride on the same surface: a live DB reports real (zero) counts
        assert!(
            r.body.contains(
                r#""lessons":{"active_lessons":0,"feedback_events_last_7d":0,"edit_before_approve_last_7d":0,"approve_unchanged_last_7d":0,"lesson_hit_rate":{"measured":false},"measured":true}"#
            ),
            "lesson counters missing: {}",
            r.body
        );
        assert!(
            r.body.contains(r#""harness":{"measured":false}"#),
            "unmeasured harness must not read as zeros: {}",
            r.body
        );
        assert!(
            r.body.contains(r#""sanitizer":{"events_stripped":"#),
            "sanitizer counts missing: {}",
            r.body
        );

        // An external send never runs (FR-API-04, invariant 4). In THIS binary it is also never
        // accepted: standalone shogun-api has no confirm UI, so an enqueued L3 proposal would sit
        // until it expired and the caller would have been told "pending" for something that could
        // not be approved. Absent surface ⇒ 501, said out loud.
        let r = request(
            port,
            "POST",
            "/v1/actions/execute",
            Some("dev"),
            Some(r#"{"kind":"send_email","to":"a@b.com","subject":"s","body":"b"}"#),
        )
        .unwrap();
        assert_eq!(r.status, 501);
        assert!(r.body.contains("no_approval_surface"), "body: {}", r.body);
        assert!(
            !r.body.contains("\"pending\":true"),
            "must not read as accepted: {}",
            r.body
        );
    }
}
