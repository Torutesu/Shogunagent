//! State-quality regression: ingest -> hourly review -> consolidator -> state -> recall.
//!
//! The default test uses hand-authored model answers, NOT a measured model. The ignored
//! live test sends the same production prompts to an explicitly selected provider. Its
//! adapter exists only in this evaluation binary; production Dream Cycle routing is unchanged.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    atomic::{AtomicI64, AtomicUsize, Ordering},
    Arc,
};

use chrono::{Local, TimeZone};
use serde_json::{json, Value};
use shogun_core::daemon::Db;
use shogun_core::llm::anthropic::{BatchHandle, BatchItem, BatchLane, BatchResult};
use shogun_core::llm::traceability::{Route, TraceRecord, TraceabilitySink};
use shogun_core::llm::{AgentClient, LlmError};
use shogun_core::tenancy::{
    dream_tick::{tick_principal_batch, TickOutcome},
    EnvKeyProvider, PrincipalId, PrincipalStore,
};
use shogun_mcp::ingest_api::{ingest_request, recall_request, RecallOutcome};
use shogun_memory::actor::{with_call_scope, Actor, CallScope, ReadScope, WriteContext};
use shogun_memory::{state::StateTable, DbKey};

const HOUR: i64 = 3_600_000;
const CONTEXT: &str = "Toru is the product lead at Nebula Labs. Nebula is our launch project. We work together on the Nebula launch deck over Slack.";
const PROMISE: &str = "I'll send Toru the Nebula launch deck by Friday.";
const MOVED: &str = "Update: the Nebula launch deck for Toru moves to next Monday.";
const DONE: &str = "Sent the Nebula launch deck to Toru.";
const WAITING: &str = "Toru, can you confirm the Nebula launch venue?";
const ANSWERED: &str = "Received the Nebula launch venue confirmation from Toru.";
const PRIVATE: &str = "Orion-private: Bob promised Maya a separate budget review.";
const ACTION: &str = "Send Toru the Nebula launch deck";
const QUESTION: &str = "Wait for Toru to confirm the Nebula launch venue";
const DISTRACTOR: &str = "Maya promised Ken the Atlas budget; I am not involved.";

struct Scenario {
    store: PrincipalStore,
    root: std::path::PathBuf,
    now: Arc<AtomicI64>,
}

impl Scenario {
    fn new() -> Self {
        static NEXT_SCENARIO: AtomicUsize = AtomicUsize::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "shogun-state-quality-{}-{nonce}-{}",
            std::process::id(),
            NEXT_SCENARIO.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let now = Arc::new(AtomicI64::new(
            Local
                .with_ymd_and_hms(2026, 10, 26, 10, 30, 0)
                .unwrap()
                .timestamp_millis(),
        ));
        let clock = now.clone();
        let bob = PrincipalId::parse("bob").unwrap();
        let keys = EnvKeyProvider::with_default(DbKey::new([0x51; 32]))
            .with_principal_key(&bob, DbKey::new([0xb0; 32]));
        let store = PrincipalStore::new(
            &root,
            Arc::new(keys),
            Arc::new(move || clock.load(Ordering::SeqCst)),
            4,
        );
        Self { store, root, now }
    }

    fn request(stage: usize, messages: &[&str]) -> shogun_mcp::ingest_api::IngestRequest {
        // The entity-list policy requires observations in two distinct hours, as in
        // examples/maya_handoff.rs. Keep the initial context older than the promise.
        let context_ts = Local
            .with_ymd_and_hms(2026, 10, 26, 9, 30, 0)
            .unwrap()
            .timestamp_millis();
        ingest_request(&json!({
            "events": messages.iter().map(|text| {
                let mut event = json!({"kind": "message", "title": "Chat with Toru", "content": text});
                if *text == CONTEXT { event["ts_ms"] = json!(context_ts); }
                event
            }).collect::<Vec<_>>(),
            "source": "synthetic-quality-eval", "actor": "agent:quality-writer", "thread_key": "nebula-with-toru",
            "idempotency_key": format!("stage-{stage}"),
        })).unwrap()
    }

    fn ingest(&self, db: &Db, stage: usize, messages: &[&str]) -> Vec<i64> {
        let request = Self::request(stage, messages);
        let result = db.agent_ingest(&request).unwrap();
        assert_eq!(result.inserted, messages.len());
        assert_eq!(result.rejected, 0);
        let replay = db.agent_ingest(&request).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.inserted, 0);
        assert_eq!(replay.event_ids, result.event_ids);
        result.event_ids
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn recall(db: &Db, query: &str) -> RecallOutcome {
    // Two agent identities share one principal through the real call-scope mechanism.
    // This exercises engine calls, not two running application clients.
    with_call_scope(
        CallScope {
            write: WriteContext::world(Actor::agent("quality-reader").unwrap()),
            read: ReadScope::WORLD_ONLY,
        },
        || {
            db.agent_recall(
                &recall_request(&json!({"query": query, "budget_tokens": 3000})).unwrap(),
            )
            .unwrap()
        },
    )
}

/// Scripted answers are deliberately independent of the expected state assertions. The
/// transport maps only session/line anchors and source ids assigned by the real store.
struct EvaluationLane {
    db: Db,
    stage: usize,
    sources: Vec<i64>,
    calls: AtomicUsize,
    provider: Option<Box<dyn AgentClient>>,
    invalid: Option<&'static str>,
}

impl EvaluationLane {
    fn answer(&self, item: &BatchItem) -> Value {
        if item.purpose == shogun_memory::loops::PURPOSE {
            let input = &item.chunk;
            let commitment = self
                .db
                .commitment_rows()
                .into_iter()
                .find(|r| r.description.contains("Nebula") && r.confidence >= 0.84);
            let question = self
                .db
                .open_loop_rows()
                .into_iter()
                .find(|r| r.description.contains("venue") && r.confidence >= 0.84);
            let open = |id: Value, kind: &str, event: i64, quote: &str, text: &str| {
                json!({
                    "id": id, "kind": kind, "horizon": "short", "source_event_id": event,
                    "source_quote": quote, "confidence": 0.94, "text": text,
                })
            };
            return match self.stage {
                0 => {
                    json!({"open": [open(Value::Null, "commitment", self.sources[0], PROMISE, ACTION),
                    open(Value::Null, "loop", self.sources[1], WAITING, QUESTION)], "resolved": [], "duplicates": []})
                }
                1 => {
                    let id = format!(
                        "C{}",
                        commitment.expect("first review produced a commitment").id
                    );
                    assert!(
                        input.contains(&id) && input.contains(MOVED),
                        "update and existing item reach the model"
                    );
                    json!({"open": [open(json!(id), "commitment", self.sources[0], MOVED, ACTION)], "resolved": [], "duplicates": []})
                }
                2 => {
                    assert!(
                        input.contains(DONE) && input.contains(ANSWERED),
                        "completion sources reach the model"
                    );
                    json!({"open": [], "resolved": [
                    {"id": format!("C{}", commitment.unwrap().id), "event_id": self.sources[0], "quote": DONE},
                    {"id": format!("L{}", question.unwrap().id), "event_id": self.sources[1], "quote": ANSWERED}
                ], "duplicates": []})
                }
                _ => unreachable!(),
            };
        }
        let mut sessions: Vec<Value> = Vec::new();
        for line in item.chunk.lines() {
            if let Some(id) = line.strip_prefix("Session: ") {
                sessions.push(
                    json!({"id": id.trim(), "commitments": [], "open_loops": [], "entities": []}),
                );
                continue;
            }
            let Some((number, text)) = line.strip_prefix('[').and_then(|s| s.split_once("] "))
            else {
                continue;
            };
            let (Ok(number), Some(session)) = (number.parse::<usize>(), sessions.last_mut()) else {
                continue;
            };
            let anchored = |description: &str| {
                json!({"description": description, "line": number,
                "anchor": text.chars().take(40).collect::<String>(), "anchor_len": text.chars().count()})
            };
            if text.contains(CONTEXT) || text.contains(PROMISE) {
                let mut person = anchored("Toru is a contact for the Nebula launch deck.");
                person["kind"] = json!("person");
                person["name"] = json!("Toru");
                person["context"] = json!("Nebula launch deck");
                person["person_class"] = json!("contact");
                person["channel"] = json!("Slack");
                session["entities"].as_array_mut().unwrap().push(person);
                let mut project = anchored("Nebula is the user's launch project with Toru.");
                project["kind"] = json!("project");
                project["name"] = json!("Nebula");
                project["context"] = json!("Nebula launch deck");
                project["project_class"] = json!("user_project");
                session["entities"].as_array_mut().unwrap().push(project);
            }
            if text.contains(PROMISE) {
                let mut candidate = anchored(ACTION);
                candidate["direction"] = json!("mine");
                session["commitments"]
                    .as_array_mut()
                    .unwrap()
                    .push(candidate);
            }
            if text.contains(WAITING) {
                let mut candidate = anchored(QUESTION);
                candidate["kind"] = json!("waiting_on_them");
                session["open_loops"]
                    .as_array_mut()
                    .unwrap()
                    .push(candidate);
            }
        }
        assert!(!sessions.is_empty(), "production review carries a session");
        json!({"sessions": sessions})
    }
}

impl BatchLane for EvaluationLane {
    async fn review(&self, items: &[BatchItem]) -> Result<Vec<BatchResult>, LlmError> {
        let mut replies = Vec::new();
        for item in items {
            assert!(
                !item.chunk.contains(PRIVATE),
                "another principal's source must never reach inference"
            );
            assert!(
                self.calls.fetch_add(1, Ordering::SeqCst) < 2,
                "at most one review and one consolidation per stage"
            );
            // The scheduler's consolidation budget reads the same per-principal ledger as
            // production. Record counts/digests only; synthetic prompts are never printed.
            self.db
                .traceability_sink()
                .try_record(TraceRecord::for_chunk(
                    Route::BatchRelay,
                    item.purpose.clone(),
                    "quality-eval",
                    &item.chunk,
                    false,
                ))?;
            let text = match &self.provider {
                Some(provider) => provider
                    .complete_split(item.system.as_deref().unwrap_or_default(), &item.chunk)?,
                None => {
                    let mut answer = self.answer(item);
                    if item.purpose == shogun_memory::loops::PURPOSE {
                        for row in answer["open"].as_array_mut().unwrap() {
                            match self.invalid {
                                Some("invented quote") => {
                                    row["source_quote"] = json!("This sentence was never captured.")
                                }
                                Some("unshown event") => row["source_event_id"] = json!(999_999),
                                Some("low confidence") => row["confidence"] = json!(0.2),
                                Some("foreign evidence") => row["source_quote"] = json!(PRIVATE),
                                None => {}
                                Some(_) => unreachable!(),
                            }
                        }
                    }
                    answer.to_string()
                }
            };
            // Malformed live output is a failure, never repaired into a scripted answer.
            serde_json::from_str::<Value>(&text)
                .map_err(|_| LlmError::Parse("quality evaluation requires JSON".into()))?;
            replies.push(BatchResult {
                custom_id: item.custom_id.clone(),
                text: Some(text),
                error: None,
            });
        }
        Ok(replies)
    }

    fn synchronous_review(&self) -> bool {
        true
    }
    async fn submit(&self, _: &[BatchItem], _: Option<&str>) -> Result<BatchHandle, LlmError> {
        Err(LlmError::Provider("evaluation uses review".into()))
    }
    async fn poll(&self, _: &str) -> Result<BatchHandle, LlmError> {
        Err(LlmError::Provider("evaluation does not poll".into()))
    }
    async fn results(&self, _: &str) -> Result<Vec<BatchResult>, LlmError> {
        Err(LlmError::Provider("evaluation has no batch handle".into()))
    }
}

fn run_scenario(mut provider: impl FnMut(&Db) -> Option<Box<dyn AgentClient>>) {
    let scenario = Scenario::new();
    let mut alice = scenario
        .store
        .open(&PrincipalId::parse("alice").unwrap())
        .unwrap();
    let bob = scenario
        .store
        .open(&PrincipalId::parse("bob").unwrap())
        .unwrap();
    scenario.ingest(bob.db(), 99, &[PRIVATE]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut commitment_id = None;
    let mut loop_id = None;
    let mut friday = None;
    let mut entity_ids = None;
    let mut total_calls = 0;
    for (stage, messages) in [
        vec![PROMISE, WAITING, DISTRACTOR, CONTEXT],
        vec![MOVED],
        vec![DONE, ANSWERED],
    ]
    .iter()
    .enumerate()
    {
        let sources = scenario.ingest(alice.db(), stage, messages);
        for id in &sources {
            assert_eq!(alice.db().memory_processing_status(*id, false)["state"], "pending");
        }
        if stage == 0 {
            assert!(
                recall(alice.db(), "Nebula Toru").items.is_empty(),
                "raw ingestion alone must not present unreviewed candidates as facts"
            );
        }
        scenario.now.fetch_add(HOUR, Ordering::SeqCst);
        let now = scenario.now.load(Ordering::SeqCst);
        let lane = EvaluationLane {
            db: alice.db().clone(),
            stage,
            sources: sources.clone(),
            calls: AtomicUsize::new(0),
            provider: provider(alice.db()),
            invalid: None,
        };
        let (outcome, applied) = rt.block_on(tick_principal_batch(alice.db(), &lane, now));
        assert_eq!(
            outcome,
            TickOutcome::Ran { complete: true },
            "stage {stage}: hourly review"
        );
        assert!(applied.is_some(), "stage {stage}: consolidator ran");
        for id in &sources {
            let status = alice.db().memory_processing_status(*id, false);
            assert_eq!(status["state"], "ready", "stage {stage}: {status}");
            assert!(status["reviewed_at_ms"].is_i64());
            assert!(!status.to_string().contains(PROMISE));
        }
        assert_eq!(
            lane.calls.load(Ordering::SeqCst),
            2,
            "both model steps ran at stage {stage}"
        );
        total_calls += lane.calls.load(Ordering::SeqCst);
        let pack = recall(alice.db(), "Nebula Toru");
        let mut ids = Vec::new();
        for (kind, name) in [("person", "Toru"), ("project", "Nebula")] {
            let item = pack
                .items
                .iter()
                .find(|item| item.kind == kind)
                .expect("agent B recalls the reviewed person and project");
            assert_eq!(item.text, name);
            assert!(matches!(item.band, "high" | "medium"));
            let event_id = item
                .event_id
                .expect("typed entity cites a stored source event");
            let source = alice.db().events_json(&[event_id], 4000).unwrap();
            let quote = item
                .quote
                .as_deref()
                .expect("typed entity has a proving quote");
            assert!(!quote.is_empty());
            assert!(
                source["events"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains(quote),
                "typed entity quote is verbatim stored evidence"
            );
            ids.push(item.id);
        }
        if stage == 0 {
            entity_ids = Some(ids.clone());
        }
        assert_eq!(
            Some(ids),
            entity_ids,
            "typed entity identities survive later reviews"
        );
        let rows = alice.db().try_commitment_rows().unwrap();
        let active: Vec<_> = rows
            .iter()
            .filter(|r| r.confidence >= 0.84 && r.status == "open")
            .collect();
        if stage < 2 {
            assert_eq!(
                active.len(),
                1,
                "one current commitment, stage {stage}: {rows:?}"
            );
            let row = active[0];
            assert!(row.description.contains("Nebula") && row.description.contains("Toru"));
            if stage == 0 {
                commitment_id = Some(row.id);
                friday = row.due_at;
            }
            assert_eq!(
                Some(row.id),
                commitment_id,
                "a changed deadline updates the existing row"
            );
            let due = Local
                .timestamp_millis_opt(row.due_at.expect("deadline extracted"))
                .unwrap();
            assert_eq!(
                due.format("%Y-%m-%d").to_string(),
                if stage == 0 {
                    "2026-10-30"
                } else {
                    "2026-11-02"
                }
            );
            if stage == 1 {
                assert!(row.due_at > friday, "newer evidence supersedes Friday");
                assert!(
                    pack.items
                        .iter()
                        .filter(|item| item.kind == "commitment")
                        .all(|item| !item.text.contains("Friday")
                            && !item.quote.as_deref().unwrap_or_default().contains("Friday")),
                    "current recalled commitment must not carry the superseded deadline"
                );
            }
            let loops = alice.db().try_open_loop_rows().unwrap();
            let outstanding: Vec<_> = loops
                .iter()
                .filter(|r| r.status == "open" && r.confidence >= 0.84)
                .collect();
            assert_eq!(outstanding.len(), 1, "one outstanding venue question");
            let question = outstanding[0];
            assert!(
                question.description.contains("venue") && question.description.contains("Toru")
            );
            if stage == 0 {
                loop_id = Some(question.id);
            }
            assert_eq!(Some(question.id), loop_id);
            let pack = recall(alice.db(), "Nebula");
            let waiting = pack
                .items
                .iter()
                .find(|r| r.kind == "open_loop" && r.id == question.id)
                .expect("venue question is recallable");
            assert!(WAITING.contains(waiting.quote.as_deref().expect("venue evidence")));
            let item = pack
                .items
                .iter()
                .find(|r| r.kind == "commitment" && r.id == row.id)
                .expect("current commitment is recallable");
            assert!(matches!(item.band, "high" | "medium"));
            assert!(item.confidence >= 0.84);
            let quote = item
                .quote
                .as_deref()
                .expect("recall carries its proving quote");
            assert!(
                messages.iter().any(|m| m.contains(quote)),
                "quote is verbatim current evidence: {quote}"
            );
            assert!(
                sources.contains(&item.event_id.unwrap()),
                "provenance follows the latest evidence"
            );
        } else {
            assert!(
                active.is_empty(),
                "completion removes the open commitment: {rows:?}"
            );
            assert_eq!(
                alice
                    .db()
                    .commitment(commitment_id.unwrap())
                    .unwrap()
                    .status,
                "done"
            );
            assert!(
                alice
                    .db()
                    .try_open_loop_rows()
                    .unwrap()
                    .iter()
                    .all(|r| r.status != "open" || r.confidence < 0.84),
                "the answered venue question is closed"
            );
            assert_eq!(
                alice.db().open_loop(loop_id.unwrap()).unwrap().status,
                "closed"
            );
            let pack = recall(alice.db(), "Nebula");
            assert!(
                pack.items
                    .iter()
                    .all(|r| !matches!(r.kind, "commitment" | "open_loop")),
                "completed work must not appear as outstanding state: {:?}",
                pack.items
            );
            assert!(
                pack.facts
                    .iter()
                    // Historical retrieval summaries are explicitly labelled unverified;
                    // current authoritative state must not ask the agent to do closed work.
                    .filter(|fact| !fact.starts_with("summary (unverified):"))
                    .all(|fact| !fact.contains(ACTION) && !fact.contains(QUESTION)),
                "closed work is not a current fact: {:?}",
                pack.facts
            );
            assert!(
                pack.evidence.iter().any(|e| e.excerpt.contains(DONE)),
                "completion remains available as dated evidence"
            );
        }
        assert!(recall(bob.db(), "Nebula").items.is_empty());
        assert!(recall(bob.db(), "Nebula").evidence.is_empty());
        assert!(recall(alice.db(), "Orion-private").evidence.is_empty());
        assert!(bob
            .db()
            .try_commitment_rows()
            .unwrap()
            .iter()
            .all(|r| !r.description.contains("Nebula")));
        let before = lane.calls.load(Ordering::SeqCst);
        let _ = rt.block_on(tick_principal_batch(alice.db(), &lane, now));
        assert_eq!(
            lane.calls.load(Ordering::SeqCst),
            before,
            "same-hour replay makes no model call"
        );
        let before_restart = recall(alice.db(), "Nebula Toru");
        drop(lane);
        drop(alice);
        assert!(scenario.store.close(&PrincipalId::parse("alice").unwrap()));
        assert_eq!(
            scenario.store.live_handles(),
            1,
            "only Bob's store remains open"
        );
        alice = scenario
            .store
            .open(&PrincipalId::parse("alice").unwrap())
            .unwrap();
        assert_eq!(
            recall(alice.db(), "Nebula Toru"),
            before_restart,
            "stage {stage}: state and evidence survive reopening the encrypted store"
        );
        let replay = alice
            .db()
            .agent_ingest(&Scenario::request(stage, messages))
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.inserted, 0);
        assert_eq!(
            replay.event_ids, sources,
            "stage {stage}: ingest receipts survive reopening the encrypted store"
        );
        for id in &sources {
            assert_eq!(alice.db().memory_processing_status(*id, false)["state"], "ready",
                "processing receipts survive restart and replay");
        }
        println!(
            "PASS stage {stage}: state, grounding, deadline/status, tenant isolation, replay and restart"
        );
    }
    assert_eq!(total_calls, 6);
    assert!(!alice
        .db()
        .evidence_sources(StateTable::Commitments, commitment_id.unwrap())
        .is_empty());
}

#[test]
fn commitment_changes_and_completion_preserve_grounded_state() {
    println!("SYNTHETIC REPLAY: real engine, scripted model answers; no model or network calls.");
    run_scenario(|_| None);
}

#[test]
fn owner_correction_survives_newer_automatic_suggestion_replay_and_restart() {
    println!("SYNTHETIC REPLAY: owner correction against scripted automatic suggestions; no model or network calls.");
    const CORRECTED: &str = "Send Toru the revised Nebula launch deck by Wednesday";
    let scenario = Scenario::new();
    let principal = PrincipalId::parse("alice").unwrap();
    let mut alice = scenario.store.open(&principal).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sources = scenario.ingest(alice.db(), 0, &[PROMISE, WAITING, CONTEXT]);
    scenario.now.fetch_add(HOUR, Ordering::SeqCst);
    let lane = EvaluationLane {
        db: alice.db().clone(),
        stage: 0,
        sources,
        calls: AtomicUsize::new(0),
        provider: None,
        invalid: None,
    };
    let (outcome, applied) = rt.block_on(tick_principal_batch(
        alice.db(),
        &lane,
        scenario.now.load(Ordering::SeqCst),
    ));
    assert_eq!(outcome, TickOutcome::Ran { complete: true });
    assert!(applied.is_some());
    let id = recall(alice.db(), "Nebula")
        .items
        .iter()
        .find(|item| item.kind == "commitment")
        .unwrap()
        .id;
    drop(lane);

    let corrected_due = Local
        .with_ymd_and_hms(2026, 11, 4, 17, 0, 0)
        .unwrap()
        .timestamp_millis();
    // Changing just the due-date field preserves the original source as context for
    // the unchanged wording. That old quote is not proof of the owner-entered deadline.
    assert!(alice
        .db()
        .state_update_as(
            &json!({"table": "commitments", "id": id, "due_at": corrected_due}).to_string(),
            &WriteContext::owner(),
        )
        .unwrap());
    let date_only = recall(alice.db(), "Nebula");
    let date_only = date_only
        .items
        .iter()
        .find(|item| item.id == id && item.kind == "commitment")
        .expect("date-only correction remains recallable");
    assert_eq!(date_only.text, ACTION);
    assert_eq!(date_only.quote.as_deref(), Some(PROMISE));
    assert!(date_only.event_id.is_some());
    assert_eq!(
        alice.db().commitment(id).unwrap().due_at,
        Some(corrected_due)
    );

    assert!(alice
        .db()
        .state_update_as(
            &json!({"table": "commitments", "id": id,
            "description": CORRECTED, "due_at": corrected_due})
            .to_string(),
            &WriteContext::owner(),
        )
        .unwrap());
    let assert_corrected =
        |db: &Db| {
            let row = db.commitment(id).expect("same corrected commitment exists");
            assert_eq!(row.description, CORRECTED);
            assert_eq!(row.due_at, Some(corrected_due));
            assert_eq!(row.confidence, 1.0);
            let model = db
                .world_model(false)
                .expect("world model remains exportable");
            let exported = model.commitments.iter().find(|row| row.id == id).unwrap();
            assert!(exported.pinned, "owner correction stays pinned");
            assert!(
                !exported.provenance.is_empty(),
                "correction retains source history"
            );
            let pack = recall(db, "Nebula Toru");
            let commitments: Vec<_> = pack
                .items
                .iter()
                .filter(|item| item.kind == "commitment")
                .collect();
            assert_eq!(
                commitments.len(),
                1,
                "automatic review cannot create a stale twin"
            );
            assert_eq!(commitments[0].id, id);
            assert_eq!(commitments[0].text, CORRECTED);
            assert_eq!(commitments[0].quote, None,
            "retained automatic evidence must not masquerade as proof of owner-corrected wording");
            assert_eq!(
                commitments[0].event_id, None,
                "owner-corrected wording has no supporting captured event"
            );
            for kind in ["person", "project", "open_loop"] {
                assert!(
                    pack.items.iter().any(|item| item.kind == kind),
                    "correction must preserve other typed state: {kind}"
                );
            }
            pack
        };
    let before_restart = assert_corrected(alice.db());
    drop(alice);
    assert!(scenario.store.close(&principal));
    assert_eq!(scenario.store.live_handles(), 0);
    alice = scenario.store.open(&principal).unwrap();
    assert_eq!(assert_corrected(alice.db()), before_restart);

    // This newer raw event and scripted relist target the existing row with Monday
    // and the old wording. The real consolidator must preserve the owner's correction.
    scenario.now.fetch_add(1, Ordering::SeqCst);
    let sources = scenario.ingest(alice.db(), 1, &[MOVED]);
    scenario.now.fetch_add(HOUR, Ordering::SeqCst);
    let now = scenario.now.load(Ordering::SeqCst);
    let lane = EvaluationLane {
        db: alice.db().clone(),
        stage: 1,
        sources: sources.clone(),
        calls: AtomicUsize::new(0),
        provider: None,
        invalid: None,
    };
    let (outcome, applied) = rt.block_on(tick_principal_batch(alice.db(), &lane, now));
    assert_eq!(outcome, TickOutcome::Ran { complete: true });
    assert!(
        applied.is_some(),
        "new automatic suggestion reaches the consolidator"
    );
    assert_eq!(lane.calls.load(Ordering::SeqCst), 2);
    let before_restart = assert_corrected(alice.db());
    let _ = rt.block_on(tick_principal_batch(alice.db(), &lane, now));
    assert_eq!(
        lane.calls.load(Ordering::SeqCst),
        2,
        "replay makes no model call"
    );
    drop(lane);
    drop(alice);
    assert!(scenario.store.close(&principal));
    assert_eq!(scenario.store.live_handles(), 0);
    let alice = scenario.store.open(&principal).unwrap();
    let replay = alice
        .db()
        .agent_ingest(&Scenario::request(1, &[MOVED]))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.inserted, 0);
    assert_eq!(replay.event_ids, sources);
    assert_eq!(assert_corrected(alice.db()), before_restart);
}

#[test]
fn snoozed_state_stays_out_of_recall_until_wake_time_across_restarts() {
    let scenario = Scenario::new();
    let principal = PrincipalId::parse("alice").unwrap();
    let alice = scenario.store.open(&principal).unwrap();
    let sources = scenario.ingest(alice.db(), 0, &[PROMISE, WAITING]);
    scenario.now.fetch_add(HOUR, Ordering::SeqCst);
    let now = scenario.now.load(Ordering::SeqCst);
    let lane = EvaluationLane {
        db: alice.db().clone(),
        stage: 0,
        sources,
        calls: AtomicUsize::new(0),
        provider: None,
        invalid: None,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (outcome, _) = rt.block_on(tick_principal_batch(alice.db(), &lane, now));
    assert_eq!(outcome, TickOutcome::Ran { complete: true });
    let pack = recall(alice.db(), "Nebula");
    let commitment = pack
        .items
        .iter()
        .find(|item| item.kind == "commitment")
        .unwrap()
        .id;
    let open_loop = pack
        .items
        .iter()
        .find(|item| item.kind == "open_loop")
        .unwrap()
        .id;
    let wake_at = now + HOUR;
    for (table, id) in [("commitments", commitment), ("open_loops", open_loop)] {
        assert!(alice
            .db()
            .state_update(
                &json!({
                    "table": table, "id": id, "deferred_until": wake_at,
                })
                .to_string()
            )
            .unwrap());
    }
    let assert_snoozed = |db: &Db| {
        let pack = recall(db, "Nebula");
        assert!(
            pack.items
                .iter()
                .all(|item| !matches!(item.kind, "commitment" | "open_loop")),
            "snoozed work must not appear as current state: {:?}",
            pack.items
        );
        assert!(
            pack.facts
                .iter()
                .chain(&pack.possibly)
                .filter(|fact| !fact.starts_with("summary (unverified):"))
                .all(|fact| !fact.contains(ACTION) && !fact.contains(QUESTION)),
            "snoozed work must not return through context facts: {:?}",
            pack.facts
        );
        assert!(
            pack.evidence.iter().any(|e| e.excerpt.contains(PROMISE)),
            "snoozing must preserve searchable source history"
        );
    };
    assert_snoozed(alice.db());

    // Release every DB handle, not just the LRU entry, before opening the encrypted store again.
    drop(lane);
    drop(alice);
    assert!(scenario.store.close(&principal));
    assert_eq!(scenario.store.live_handles(), 0);
    let alice = scenario.store.open(&principal).unwrap();
    scenario.now.store(wake_at - 1, Ordering::SeqCst);
    assert_snoozed(alice.db());

    scenario.now.store(wake_at, Ordering::SeqCst);
    let pack = recall(alice.db(), "Nebula");
    for (kind, id, quote) in [
        ("commitment", commitment, PROMISE),
        ("open_loop", open_loop, WAITING),
    ] {
        let item = pack
            .items
            .iter()
            .find(|item| item.kind == kind && item.id == id)
            .expect("work returns at the exact wake time without another model call");
        assert_eq!(item.quote.as_deref(), Some(quote));
    }

    // Explicitly clearing a later snooze also makes both rows visible immediately.
    for (table, id) in [("commitments", commitment), ("open_loops", open_loop)] {
        for deferred_until in [Some(wake_at + HOUR), None] {
            assert!(alice
                .db()
                .state_update(
                    &json!({
                        "table": table, "id": id, "deferred_until": deferred_until,
                    })
                    .to_string()
                )
                .unwrap());
        }
    }
    let pack = recall(alice.db(), "Nebula");
    assert!(pack
        .items
        .iter()
        .any(|item| item.kind == "commitment" && item.id == commitment));
    assert!(pack
        .items
        .iter()
        .any(|item| item.kind == "open_loop" && item.id == open_loop));
}

#[test]
fn unsafe_model_answers_do_not_become_authoritative_state() {
    for invalid in [
        "invented quote",
        "unshown event",
        "low confidence",
        "foreign evidence",
    ] {
        let scenario = Scenario::new();
        let alice = scenario
            .store
            .open(&PrincipalId::parse("alice").unwrap())
            .unwrap();
        let bob = scenario
            .store
            .open(&PrincipalId::parse("bob").unwrap())
            .unwrap();
        scenario.ingest(bob.db(), 99, &[PRIVATE]);
        let sources = scenario.ingest(alice.db(), 0, &[PROMISE, WAITING]);
        scenario.now.fetch_add(HOUR, Ordering::SeqCst);
        let lane = EvaluationLane {
            db: alice.db().clone(),
            stage: 0,
            sources,
            calls: AtomicUsize::new(0),
            provider: None,
            invalid: Some(invalid),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (outcome, applied) = rt.block_on(tick_principal_batch(
            alice.db(),
            &lane,
            scenario.now.load(Ordering::SeqCst),
        ));
        assert_eq!(outcome, TickOutcome::Ran { complete: true });
        let applied = applied.expect("the untrusted response was checked");
        assert_eq!((applied.opened, applied.dropped), (0, 2), "{invalid}");
        assert!(
            alice
                .db()
                .try_commitment_rows()
                .unwrap()
                .iter()
                .all(|r| r.confidence < 0.84),
            "{invalid}"
        );
        assert!(
            alice
                .db()
                .try_open_loop_rows()
                .unwrap()
                .iter()
                .all(|r| r.confidence < 0.84),
            "{invalid}"
        );
        let pack = recall(alice.db(), "Nebula");
        assert!(pack.items.iter().all(|r| r.band != "high"), "{invalid}");
        assert!(
            pack.possibly
                .iter()
                .all(|line| line.starts_with("possibly: ")),
            "{invalid}"
        );
        println!("SYNTHETIC PASS reject {invalid}");
    }
}

#[test]
fn malformed_provider_output_never_falls_back_to_scripted_answers() {
    struct BrokenOutput;
    impl AgentClient for BrokenOutput {
        fn complete(&self, _: &str) -> Result<String, LlmError> {
            Ok("not JSON".into())
        }
    }
    let scenario = Scenario::new();
    let alice = scenario
        .store
        .open(&PrincipalId::parse("alice").unwrap())
        .unwrap();
    let sources = scenario.ingest(alice.db(), 0, &[PROMISE, WAITING]);
    scenario.now.fetch_add(HOUR, Ordering::SeqCst);
    let lane = EvaluationLane {
        db: alice.db().clone(),
        stage: 0,
        sources,
        calls: AtomicUsize::new(0),
        provider: Some(Box::new(BrokenOutput)),
        invalid: None,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (outcome, applied) = rt.block_on(tick_principal_batch(
        alice.db(),
        &lane,
        scenario.now.load(Ordering::SeqCst),
    ));
    assert_ne!(outcome, TickOutcome::Ran { complete: true });
    assert!(applied.is_none());
    assert_eq!(lane.calls.load(Ordering::SeqCst), 1);
    assert!(alice
        .db()
        .try_commitment_rows()
        .unwrap()
        .iter()
        .all(|r| r.confidence < 0.84));
}

#[test]
#[ignore = "live state-quality evaluation: explicitly configure SHOGUN_STATE_QUALITY_LIVE"]
fn live_state_quality() {
    use shogun_core::llm::subscription::{Delegate, ProcessRunner, SubscriptionAgentClient};
    let mode = std::env::var("SHOGUN_STATE_QUALITY_LIVE")
        .expect("set SHOGUN_STATE_QUALITY_LIVE=codex, claude-code or gemini-cli");
    let delegate = Delegate::parse(&mode).expect("unsupported state-quality evaluation provider");
    println!("LIVE state-quality provider: {mode}; at most six synthetic-text completions");
    run_scenario(|db| {
        Some(Box::new(
            SubscriptionAgentClient::new(ProcessRunner, db.traceability_sink(), delegate)
                .with_purpose("state_quality_eval"),
        ))
    });
}
