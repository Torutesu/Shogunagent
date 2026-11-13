//! Opt-in server processing. One sequential worker shares the server's principal store.
//! An OS lock serializes it with one-shot ticks and is released even after a process crash.

use std::{collections::HashMap, fs::{File, OpenOptions}, path::Path, sync::{Arc, OnceLock}, time::Duration};

use shogun_integrations::TokenStore;

use super::{credentials::try_relay_bearer, dream_tick, PrincipalStore};
use crate::llm::{relay::{ModelClass, RelayBatchClient, RelayConfig}, transport::ReqwestTransport};

pub const INTERVAL_VAR: &str = "SHOGUN_PROCESSING_INTERVAL_SECS";

/// Absent/zero disables automatic model work. Reject malformed intervals at startup.
pub fn interval_from_vars(vars: &HashMap<String, String>) -> Result<Option<Duration>, &'static str> {
    let Some(raw) = vars.get(INTERVAL_VAR) else { return Ok(None); };
    match raw.parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(seconds @ 30..=3600) => Ok(Some(Duration::from_secs(seconds))),
        _ => Err(INTERVAL_VAR),
    }
}

/// Retain this file for the entire pass; never unlink a lock file that another process may hold.
pub fn acquire(root: &Path) -> Result<File, &'static str> {
    std::fs::create_dir_all(root).map_err(|_| "processing_lock_unavailable")?;
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(root.join(".processing.lock")).map_err(|_| "processing_lock_unavailable")?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the live File owns this descriptor for the entire call and lock lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("processing_busy");
        }
    }
    #[cfg(not(unix))]
    return Err("processing_lock_unsupported");
    Ok(file)
}

/// Run the existing review/consolidation path with strict credentials. Automatic passes never
/// acknowledge model work through a local fallback. The explicit one-shot command retains the
/// intentional local-only mode for principals with no configured credential.
pub async fn tick(
    store: &PrincipalStore,
    tokens: &dyn TokenStore,
    now_ms: i64,
    allow_local: bool,
) -> Vec<dream_tick::PrincipalTick> {
    let transport: OnceLock<Option<ReqwestTransport>> = OnceLock::new();
    let principals = dream_tick::principals_on_disk(store);
    dream_tick::hourly_tick_checked(store, &principals, |principal, db| {
        let Some(bearer) = try_relay_bearer(tokens, principal).map_err(|_| "credential_unavailable")? else {
            return if allow_local { Ok(None) } else { Err("relay_not_configured") };
        };
        let transport = transport.get_or_init(|| ReqwestTransport::shared().ok()).clone().ok_or("transport_unavailable")?;
        Ok(Some(RelayBatchClient::new(transport, db.traceability_sink(), bearer, RelayConfig::new(ModelClass::Classify))))
    }, now_ms).await
}

/// The loop waits *after* each pass: no overlapping ticks and no burst of missed timer ticks.
/// Dropping the process releases the lock; review leases and retry deadlines live in SQLite.
pub async fn run(
    store: Arc<PrincipalStore>,
    tokens: Arc<dyn TokenStore>,
    clock: crate::daemon::Clock,
    interval: Duration,
    connectors: Arc<shogun_integrations::hosted_connect::HostedConnectors<crate::mcp_http::HttpTokenExchange>>,
) {
    loop {
        match acquire(store.root()) {
            Ok(_guard) => {
                let sync_store = store.clone();
                let vault = connectors.clone();
                let now = clock();
                // spawn_blocking cannot be cancelled. Its descriptor keeps the OS lock held
                // until read-sync stops even when the async worker is aborted at shutdown.
                let sync_guard = match _guard.try_clone() {
                    Ok(guard) => guard,
                    Err(_) => {
                        eprintln!("shogun-api: processing: processing_lock_unavailable");
                        tokio::time::sleep(interval).await;
                        continue;
                    }
                };
                let sync = tokio::task::spawn_blocking(move || {
                    let _guard = sync_guard;
                    let principals = dream_tick::principals_on_disk(&sync_store);
                    crate::provider_fetch::HttpProviderFetch::new().map(|http| {
                        super::connector_sync::drain_tick(&sync_store, &vault, &http, &principals, now)
                    })
                }).await;
                if !matches!(sync, Ok(Ok(_))) {
                    eprintln!("shogun-api: processing: connector_sync_unavailable");
                }
                let report = tick(&store, tokens.as_ref(), clock(), false).await;
                for line in &report {
                    if let Ok(open) = store.open(&line.principal) {
                        let code = match line.outcome {
                            dream_tick::TickOutcome::Unavailable(code) | dream_tick::TickOutcome::Failed(code)
                                | dream_tick::TickOutcome::Skipped(code) => code,
                            dream_tick::TickOutcome::Ran { complete: false } => "review_incomplete",
                            dream_tick::TickOutcome::Ran { complete: true } => "review_complete",
                            dream_tick::TickOutcome::NothingNew => "nothing_new",
                            dream_tick::TickOutcome::AlreadyDone => "already_done",
                        };
                        open.db().record_processing_tick(code, interval.as_millis() as i64, clock());
                    }
                }
                // Counts and fixed outcomes only. No principal identifiers, bodies or credentials.
                eprintln!("shogun-api: processing: {}", dream_tick::summary(&report));
            }
            Err(code) => eprintln!("shogun-api: processing: {code}"),
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_is_explicit_and_bad_intervals_fail_closed() {
        assert_eq!(interval_from_vars(&HashMap::new()), Ok(None));
        for (raw, valid) in [("0", true), ("30", true), ("60", true), ("3600", true), ("29", false), ("3601", false), ("-1", false), ("", false), ("secret", false)] {
            let vars = HashMap::from([(INTERVAL_VAR.to_string(), raw.to_string())]);
            assert_eq!(interval_from_vars(&vars).is_ok(), valid);
        }
    }

    #[test]
    fn separate_handles_cannot_process_the_same_root_concurrently() {
        let root = std::env::temp_dir().join(format!("shogun-processing-lock-{}", std::process::id()));
        let first = acquire(&root).expect("first pass");
        assert!(acquire(&root).is_err());
        drop(first);
        let recovered = acquire(&root).expect("released lock");
        drop(recovered);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn background_worker_reports_missing_credentials_without_consuming_the_source() {
        use super::super::{EnvKeyProvider, PrincipalId, credentials::local_store};
        let root = std::env::temp_dir().join(format!("shogun-processing-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let root = root.canonicalize().expect("absolute root");
        let keys = Arc::new(EnvKeyProvider::with_default(shogun_memory::DbKey::new([0x56; 32])));
        let clock: crate::daemon::Clock = Arc::new(|| 1_800_000_000_000);
        let store = Arc::new(PrincipalStore::new(&root, keys.clone(), clock.clone(), 4));
        let tokens: Arc<dyn TokenStore> = Arc::new(local_store(root.join(".tokens"), keys));
        let connectors = super::super::multi::hosted_connectors(tokens.clone(), &HashMap::new(), None).expect("vault");
        let open = store.open(&PrincipalId::parse("alice").expect("id")).expect("store");
        let request = shogun_mcp::ingest_api::ingest_request(&serde_json::json!({
            "source":"worker-test", "actor":"agent:alice", "thread_key":"work", "idempotency_key":"background",
            "events":[{"content":"The launch codeword is violet.", "ts_ms":1_799_990_000_000_i64}],
        })).expect("request");
        let event = open.db().agent_ingest(&request).expect("ingest").event_ids[0];
        let worker = tokio::spawn(run(store.clone(), tokens, clock, Duration::from_millis(30), connectors));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = open.db().memory_processing_status(event, false);
                if status["reason"] == "relay_not_configured" {
                    assert_eq!(status["state"], "failed");
                    assert_eq!(status["worker"]["heartbeat_fresh"], true);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("worker tick");
        worker.abort();
        let _ = worker.await;
        assert!(open.db().try_last_consolidated_to().expect("cursor").is_none());
        assert!(acquire(&root).is_ok(), "shutdown releases processing lock");
        drop(open);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }
}
