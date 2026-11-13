//! Processing receipts are metadata inside the same encrypted principal store as the sources.
//! Every status lookup checks event visibility before inspecting any processing metadata.

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use shogun_memory::actor::{current_read, with_read_scope, ReadScope};

use super::Db;

impl Db {
    /// Status of one visible event, never source text or another agent's private event metadata.
    /// Ready means review and any emitted signals were consumed; it does not promise a state row.
    pub fn memory_processing_status(&self, id: i64, include_scratch: bool) -> Value {
        let scope = ReadScope { include_scratch: include_scratch || current_read().include_scratch };
        let now = self.now_ms();
        let answer = with_read_scope(scope, || self.with_conn_reported("memory.processing_status", |conn| {
            let event: Option<(i64, String, String, i64)> = conn.query_row(
                &format!("SELECT ts, source, namespace, COALESCE((SELECT MAX(arrival_seq) FROM dream_event_arrivals WHERE event_id = e.id), 0) FROM event_log e WHERE id = ?1 AND {}", current_read().predicate("e.")),
                [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            ).optional().map_err(|_| "processing_unavailable".to_string())?;
            let Some((ts, source, namespace, arrival_seq)) = event else { return Ok(json!({"error":"not_found"})); };
            if shogun_mcp::source_policy::current().is_withheld() && shogun_mcp::source_policy::is_sensitive_source(&source) {
                return Ok(json!({"error":"not_found"}));
            }
            let query = || -> rusqlite::Result<Value> {
                let receipt: Option<(i64, String)> = conn.query_row(
                    "SELECT reviewed_at, lane FROM event_processing_receipts WHERE event_id = ?1",
                    [id], |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional()?;
                let failure: Option<(i64, String)> = conn.query_row(
                    "SELECT failed_at, code FROM event_processing_failures WHERE event_id = ?1",
                    [id], |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional()?;
                let cycle: Option<(String, i64, i64)> = conn.query_row(
                    "SELECT c.status, c.failure_count, c.updated_at FROM dream_cycle_inputs i
                     JOIN dream_cycles c ON c.cycle_id = i.cycle_id
                     WHERE i.event_id = ?1 AND i.disposition = 'active' AND c.status IN ('running','failed')
                     ORDER BY c.created_at LIMIT 1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).optional()?;
                let unread = shogun_memory::loops::has_unread_for_event(conn, id)?;
                let legacy: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM dream_cycles WHERE status = 'done' AND input_from_ts <= ?1 AND input_to_ts > ?1 AND snapshot_max_arrival_seq >= ?2)",
                    params![ts, arrival_seq], |r| r.get(0),
                )?;
                let first_window: Option<(i64, i64)> = conn.query_row(
                    "SELECT input_from_ts, snapshot_max_arrival_seq FROM dream_cycles ORDER BY created_at, cycle_id LIMIT 1",
                    [], |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional()?;
                let outside_initial_window = match first_window {
                    Some((from, snapshot)) => ts < from && arrival_seq <= snapshot,
                    None => ts < crate::dreamcycle::hourly::hourly_range(None, now, super::os_gmt_offset_secs(now)).0,
                };
                let runtime: Option<(i64, i64, String)> = conn.query_row(
                    "SELECT tick_at, interval_ms, outcome FROM memory_processing_runtime WHERE id = 1", [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).optional()?;
                let (mut state, mut stage, mut reason) = ("pending", "review", "awaiting_review");
                let mut retry_at = None;
                if namespace != "world" || shogun_memory::event_log::BATCH_EXCLUDED_SOURCES.contains(&source.as_str()) {
                    (state, stage, reason) = ("source_only", "source", "not_eligible_for_review");
                } else if let Some((_, code)) = failure.as_ref().filter(|_| unread) {
                    (state, stage, reason) = ("failed", "consolidation", code.as_str());
                } else if let Some((status, failures, updated)) = &cycle {
                    if status == "failed" {
                        (state, stage, reason) = ("failed", "review", "review_failed");
                        retry_at = Some(updated.saturating_add(crate::dreamcycle::hourly::retry_backoff_ms(
                            u32::try_from(*failures).unwrap_or(u32::MAX))));
                    } else {
                        (state, stage, reason) = ("pending", "review", "review_in_progress");
                    }
                } else if let Some((_, lane)) = &receipt {
                    if lane == "local" {
                        (state, stage, reason) = ("source_only", "review", "local_rules_only");
                    } else if unread {
                        (state, stage, reason) = ("pending", "consolidation", "awaiting_consolidation");
                    } else {
                        (state, stage, reason) = ("ready", "complete", "processing_complete");
                    }
                } else if legacy {
                    (state, stage, reason) = ("unknown", "review", "no_processing_receipt");
                } else if outside_initial_window {
                    (state, stage, reason) = ("source_only", "source", "outside_initial_review_window");
                }
                if state == "pending" {
                    if let Some((_, _, code)) = &runtime {
                        if matches!(code.as_str(), "relay_not_configured" | "credential_unavailable" | "transport_unavailable") {
                            (state, stage, reason) = ("failed", "configuration", code.as_str());
                        }
                    }
                }
                let reviewed_at = receipt.as_ref().map(|r| r.0);
                Ok(json!({"event_id":id, "state":state, "stage":stage, "reason":reason,
                    "source_available":true, "reviewed_at_ms":reviewed_at,
                    "review_age_ms":reviewed_at.map(|at| now.saturating_sub(at).max(0)),
                    "retry_not_before_ms":retry_at, "checked_at_ms":now,
                    "worker":runtime.map(|(at, interval, code)| json!({"last_tick_at_ms":at,
                        "interval_ms":interval, "last_outcome":code,
                        "heartbeat_fresh":interval > 0 && now.saturating_sub(at) <= interval.saturating_mul(2)}))}))
            };
            query().map_err(|_| "processing_unavailable".to_string())
        }));
        answer.unwrap_or_else(|_| json!({"error":"processing_unavailable"}))
    }

    #[cfg(feature = "daemon-server")]
    pub(crate) fn record_processing_tick(&self, outcome: &'static str, interval_ms: i64, now: i64) {
        let _ = self.with_conn_reported("memory.processing_tick", |conn| {
            conn.execute("INSERT INTO memory_processing_runtime (id, tick_at, interval_ms, outcome)
                VALUES (1, ?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET tick_at = excluded.tick_at,
                interval_ms = excluded.interval_ms, outcome = excluded.outcome", params![now, interval_ms, outcome])
                .map_err(|_| "processing_unavailable".to_string())
        });
    }

    /// Fixed failure codes only; these are not log messages and never contain provider output.
    pub(crate) fn record_processing_failure(&self, read: &[i64], code: Option<&str>, now: i64) {
        let _ = self.with_conn_mut_reported("memory.processing_failure", |conn| {
            let tx = conn.transaction().map_err(|_| "processing_unavailable".to_string())?;
            for input_id in read {
                let id: Option<i64> = conn_event_for_input(&tx, *input_id).map_err(|_| "processing_unavailable".to_string())?;
                let Some(id) = id else { continue; };
                let result = match code {
                    Some(code) => tx.execute("INSERT INTO event_processing_failures (event_id, failed_at, code)
                        SELECT id, ?2, ?3 FROM event_log WHERE id = ?1
                        ON CONFLICT(event_id) DO UPDATE SET failed_at = excluded.failed_at, code = excluded.code",
                        params![id, now, code]),
                    None => tx.execute("DELETE FROM event_processing_failures WHERE event_id = ?1", [id]),
                };
                result.map_err(|_| "processing_unavailable".to_string())?;
            }
            tx.commit().map_err(|_| "processing_unavailable".to_string())
        });
    }
}

fn conn_event_for_input(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Option<i64>> {
    if id < 0 {
        conn.query_row("SELECT event_id FROM loop_updates WHERE id = ?1", [-id], |r| r.get(0)).optional()
    } else {
        conn.query_row("SELECT event_id FROM loop_signals WHERE id = ?1", [id], |r| r.get(0)).optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_memory::actor::{Actor, CallScope, WriteContext, with_call_scope};

    fn fixture() -> (Db, i64) {
        let db = Db::open_in_memory(std::sync::Arc::new(|| 1_800_000_000_000)).expect("db");
        let req = shogun_mcp::ingest_api::ingest_request(&json!({
            "source":"processing-test", "actor":"agent:alice", "thread_key":"task",
            "idempotency_key":"processing-fixture", "events":[{"content":"The project codeword is violet."}],
            "namespace":"agent_scratch",
        })).expect("request");
        let id = db.agent_ingest(&req).expect("ingest").event_ids[0];
        (db, id)
    }

    #[test]
    fn inaccessible_events_have_no_status_and_reads_do_not_mutate_the_ledger() {
        let (db, id) = fixture();
        for actor in ["alice", "bob"] {
            let status = with_call_scope(CallScope {
                write: WriteContext::world(Actor::agent(actor).expect("actor")),
                read: ReadScope::WORLD_ONLY,
            }, || db.memory_processing_status(id, true));
            if actor == "alice" { assert_eq!(status["state"], "source_only"); }
            else { assert_eq!(status, json!({"error":"not_found"})); }
        }
        assert_eq!(db.memory_processing_status(id, false), json!({"error":"not_found"}));
        assert_eq!(db.memory_processing_status(i64::MAX, true), json!({"error":"not_found"}));
        assert!(db.try_last_consolidated_to().expect("cursor").is_none());
    }

    #[test]
    fn late_arrivals_are_pending_and_withheld_sources_reveal_nothing() {
        let db = Db::open_in_memory(std::sync::Arc::new(|| 1_800_000_000_000)).expect("db");
        db.with_conn_reported("test.processing", |conn| {
            conn.execute("INSERT INTO dream_cycles (cycle_id, input_from_ts, input_to_ts, status, batch_stage, snapshot_max_arrival_seq, created_at, updated_at) VALUES ('legacy', 0, 1800000000001, 'done', 'complete', 0, 1, 1)", []).expect("legacy cycle");
            Ok(())
        }).expect("legacy");
        let req = shogun_mcp::ingest_api::ingest_request(&json!({
            "source":"processing-test", "actor":"agent:alice", "thread_key":"task",
            "idempotency_key":"late-status", "events":[{"content":"An old source newly imported.", "ts_ms":1_799_990_000_000_i64}],
        })).expect("request");
        let id = db.agent_ingest(&req).expect("ingest").event_ids[0];
        assert_eq!(db.memory_processing_status(id, false)["state"], "pending");
        db.with_conn_reported("test.processing", |conn| {
            conn.execute("UPDATE dream_cycles SET snapshot_max_arrival_seq = 100 WHERE cycle_id = 'legacy'", []).expect("historical coverage");
            Ok(())
        }).expect("coverage");
        assert_eq!(db.memory_processing_status(id, false)["state"], "unknown");
        db.with_conn_reported("test.processing", |conn| {
            conn.execute("UPDATE event_log SET source = 'screen_text' WHERE id = ?1", [id]).expect("sensitive source");
            Ok(())
        }).expect("source");
        let status = shogun_mcp::source_policy::with_access(shogun_mcp::source_policy::SensitiveSources::Withheld,
            || db.memory_processing_status(id, false));
        assert_eq!(status, json!({"error":"not_found"}));
    }

    #[test]
    fn initial_history_outside_the_lookback_is_source_only_but_late_imports_are_pending() {
        let now = 1_800_000_000_000_i64;
        let db = Db::open_in_memory(std::sync::Arc::new(move || now)).expect("db");
        let (from, to) = crate::dreamcycle::hourly::hourly_range(None, now, super::super::os_gmt_offset_secs(now));
        let ingest = |key: &str| {
            let req = shogun_mcp::ingest_api::ingest_request(&json!({
                "source":"processing-test", "actor":"agent:alice", "thread_key":"task",
                "idempotency_key":key, "events":[{"content":format!("Old history {key}"), "ts_ms":from - 1}],
            })).expect("request");
            db.agent_ingest(&req).expect("ingest").event_ids[0]
        };
        let initial = ingest("initial");
        assert_eq!(db.memory_processing_status(initial, false)["reason"], "outside_initial_review_window");
        db.with_conn_reported("test.processing", |conn| {
            conn.execute("INSERT INTO dream_cycles (cycle_id, input_from_ts, input_to_ts, status, batch_stage, snapshot_max_arrival_seq, created_at, updated_at)
                SELECT 'initial', ?1, ?2, 'done', 'complete', MAX(arrival_seq), 1, 1 FROM dream_event_arrivals", params![from, to]).expect("first window");
            Ok(())
        }).expect("window");
        assert_eq!(db.memory_processing_status(initial, false)["state"], "source_only");
        let late = ingest("late");
        assert_eq!(db.memory_processing_status(late, false)["state"], "pending");
    }

    #[test]
    fn receipts_do_not_survive_source_replacement_or_deletion() {
        let (db, id) = fixture();
        db.with_conn_reported("test.processing", |conn| {
            conn.execute("INSERT INTO event_processing_receipts VALUES (?1, 1, 10, 'batch')", [id]).expect("receipt");
            conn.execute("UPDATE event_log SET content = 'Changed source' WHERE id = ?1", [id]).expect("update");
            let count: i64 = conn.query_row("SELECT count(*) FROM event_processing_receipts", [], |r| r.get(0)).expect("count");
            assert_eq!(count, 0);
            conn.execute("INSERT INTO event_processing_failures VALUES (?1, 10, 'consolidation_output')", [id]).expect("failure");
            conn.execute("DELETE FROM event_log WHERE id = ?1", [id]).expect("delete");
            let count: i64 = conn.query_row("SELECT count(*) FROM event_processing_failures", [], |r| r.get(0)).expect("count");
            assert_eq!(count, 0);
            Ok(())
        }).expect("checks");
    }
}
