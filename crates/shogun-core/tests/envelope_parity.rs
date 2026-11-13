//! Invariant 4 × E34: three faces, one envelope. The same tool called on MCP (`POST /mcp`), REST
//! and the CLI (argv → `shogun_cli::wire::to_call` → the loopback request `shogun` sends; the binary
//! prints that body as it receives it) answers the same envelope fields:
//!
//! - a state read and `profile.whoami` carry `usage` `{read, 1, cost 0}`, a `latency_ms` and
//!   `next_action: null`;
//! - a refusal names the same `next_action` (`reauthenticate`, `upgrade_plan`, `request_scope`,
//!   `fix_input`), from the one table in `shogun_mcp::envelope`;
//! - `memory.search` carries no read envelope on any face (its answer is compared byte for byte
//!   across the faces, so E34 left it as it was).
//!
//! One server, one store, one token list: `shogun-api`'s single-principal composition over a real
//! `DbBackend`.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use shogun_agents::approval::ApprovalQueue;
use shogun_agents::entitlement::{entitlements, Entitlements, Plan};
use shogun_core::daemon::{Clock, Db};
use shogun_core::db_backend::DbBackend;
use shogun_mcp::live_tokens::LiveTokens;
use shogun_mcp::mcp::McpServer;
use shogun_mcp::memory_api::{TokenRegistry, TokenScope};
use shogun_mcp::server::{bind_exact, serve_on, AppState};

const FULL: &str = "parity-full-token-0001";
const READ: &str = "parity-read-token-0001";
const WRONG: &str = "parity-not-a-token-0001";

fn clock() -> Clock {
    Arc::new(|| 1_700_000_000_000)
}

/// A store with one person in it; returns their id.
fn seeded() -> (Db, i64, i64) {
    use shogun_memory::state::{NewPerson, Provenance};
    let db = Db::open_in_memory(clock()).unwrap();
    let (event, _) = db
        .capture(&shogun_memory::event_log::NewEvent {
            ts: 1,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "Aiko owns the zebrafish forecast",
            content_hash: "parity-1",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .unwrap();
    let id = db
        .insert_person(&NewPerson { display_name: "Aiko", confidence: 0.9, now: 1, ..Default::default() }, &[Provenance::new(event)])
        .unwrap();
    (db, id, event)
}

/// Boot REST + `/mcp` on one listener. `locked` flips the plan for both faces.
fn boot(db: Db, locked: Arc<AtomicBool>) -> u16 {
    let registry = || {
        let mut tokens = TokenRegistry::new();
        tokens.issue_scoped(FULL, TokenScope::Full);
        tokens.issue_scoped(READ, TokenScope::Read);
        tokens
    };
    let live = Arc::new(LiveTokens::new(move || Ok(registry()), || None));
    let plan = move || -> Entitlements {
        let mut ent = entitlements(Plan::Pro, 0);
        ent.memory_api = !locked.load(Ordering::SeqCst);
        ent
    };
    let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
    let backend = Arc::new(DbBackend::new(db));
    let plan_mcp = plan.clone();
    let mcp = McpServer::new(backend.clone(), approvals.clone(), || 0, plan_mcp)
        .with_live_tokens(live.clone(), None);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = AppState::new(Arc::new(TokenRegistry::new()), backend, approvals, clock())
                .with_live_tokens(live)
                .with_mcp(Arc::new(mcp))
                .with_entitlements(Arc::new(plan));
            let listener = bind_exact(0).await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            let _ = serve_on(listener, state).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    port
}

/// The MCP face: one `tools/call`. Returns the JSON object the envelope lives in: `error.data`
/// for a JSON-RPC error, else the parsed text block.
fn mcp(port: u16, token: &str, name: &str, arguments: Value) -> Value {
    use std::io::{Read, Write};
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nMcp-Method: tools/call\r\nMcp-Name: {name}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let reply: Value = serde_json::from_str(text.split_once("\r\n\r\n").map_or("", |(_, body)| body)).unwrap();
    if reply.get("error").is_some() {
        return reply["error"]["data"].clone();
    }
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}


/// The CLI face: parse `args` as `shogun` does, build its call and send it the way the binary
/// does; the binary prints this body verbatim.
fn cli_raw(port: u16, token: &str, args: &[&str]) -> (u16, String) {
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let invocation = shogun_cli::parse::parse(&args).unwrap();
    let call = shogun_cli::wire::to_call(&invocation.command, invocation.include_low).unwrap();
    let response = shogun_cli::http::request(port, call.method, &call.path, Some(token), call.body.as_deref()).unwrap();
    (response.status, response.body)
}

/// The REST face: a request built by hand (`curl`), not through the CLI's grammar.
fn curl(port: u16, token: &str, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let response = shogun_cli::http::request(port, method, path, Some(token), body).unwrap();
    (response.status, serde_json::from_str(&response.body).unwrap_or_else(|_| panic!("not JSON: {}", response.body)))
}

/// The envelope fields of one answer; `latency_ms` is a measurement, so only its type is compared.
fn envelope(face: &str, value: &Value) -> Value {
    let latency = &value["latency_ms"];
    assert!(latency.is_null() || latency.is_u64(), "{face}: latency_ms {latency}");
    json!({
        "usage": value.get("usage").cloned().unwrap_or(Value::String("absent".into())),
        "latency_ms": if latency.is_u64() { "u64" } else { "absent" },
        "next_action": value.get("next_action").cloned().unwrap_or(Value::String("absent".into())),
    })
}

fn same_on_three_faces(what: &str, mcp: &Value, rest: &Value, cli: &Value, expected: Value) {
    let (mcp, rest, cli) = (envelope("mcp", mcp), envelope("rest", rest), envelope("cli", cli));
    assert_eq!(mcp, expected, "{what}: MCP");
    assert_eq!(rest, expected, "{what}: REST");
    assert_eq!(cli, expected, "{what}: CLI");
}

#[test]
fn the_three_faces_carry_the_same_envelope() {
    let (db, person, event) = seeded();
    let locked = Arc::new(AtomicBool::new(false));
    let port = boot(db, locked.clone());
    let id = person.to_string();
    let event_id = event.to_string();
    let processing_path = format!("/v1/memory/processing/{event}");
    let read = json!({ "usage": { "unit": "read", "quantity": 1, "cost_micro_usd": 0 }, "latency_ms": "u64", "next_action": null });
    let refusal = |next: &str| json!({ "usage": "absent", "latency_ms": "absent", "next_action": next });

    // State reads and whoami: usage, latency, next_action null. Same data on every face.
    let get_path = format!("/v1/state/people/{person}");
    for (tool, args, path, cli) in [
        ("state.people.list", json!({}), "/v1/state/people", vec!["people", "list"]),
        ("state.people.get", json!({ "id": person }), get_path.as_str(), vec!["people", "get", id.as_str()]),
        ("profile.whoami", json!({}), "/v1/profile/whoami", vec!["whoami"]),
        ("memory.processing_status", json!({"id":event}), processing_path.as_str(), vec!["processing", "status", event_id.as_str()]),
    ] {
        let on_mcp = mcp(port, FULL, tool, args);
        let (status, on_rest) = curl(port, FULL, "GET", path, None);
        assert_eq!(status, 200, "{tool}: {on_rest}");
        let (_, on_cli) = cli_raw(port, FULL, &cli);
        let on_cli: Value = serde_json::from_str(&on_cli).unwrap();
        same_on_three_faces(tool, &on_mcp, &on_rest, &on_cli, read.clone());
        // The answer itself is the same on every face; apart from the envelope's measured
        // `latency_ms`, MCP's list pages add their paging fields (`total`, `truncated`).
        let strip = |mut v: Value| {
            if let Some(object) = v.as_object_mut() {
                for key in ["latency_ms", "total", "truncated"] {
                    object.remove(key);
                }
            }
            v
        };
        assert_eq!(strip(on_rest.clone()), strip(on_cli.clone()), "{tool}: REST and CLI");
        assert_eq!(strip(on_mcp.clone()), strip(on_rest.clone()), "{tool}: the answer itself");
        if tool.starts_with("state.") {
            assert!(on_rest.to_string().contains("Aiko"), "{tool}: {on_rest}");
        }
    }

    // A record that is not there: fix_input.
    let missing = mcp(port, FULL, "state.people.get", json!({ "id": 999_999 }));
    let (status, on_rest) = curl(port, FULL, "GET", "/v1/state/people/999999", None);
    assert_eq!(status, 404, "{on_rest}");
    let (_, on_cli) = cli_raw(port, FULL, &["people", "get", "999999"]);
    same_on_three_faces("not found", &missing, &on_rest, &serde_json::from_str(&on_cli).unwrap(), refusal("fix_input"));

    let missing = mcp(port, FULL, "memory.processing_status", json!({"id":999_999}));
    let (status, on_rest) = curl(port, FULL, "GET", "/v1/memory/processing/999999", None);
    assert_eq!(status, 404, "{on_rest}");
    let (_, on_cli) = cli_raw(port, FULL, &["processing", "status", "999999"]);
    same_on_three_faces("processing not found", &missing, &on_rest,
        &serde_json::from_str(&on_cli).unwrap(), refusal("fix_input"));

    // A token that is not on the list: reauthenticate.
    let unknown = mcp(port, WRONG, "state.people.list", json!({}));
    let (status, on_rest) = curl(port, WRONG, "GET", "/v1/state/people", None);
    assert_eq!(status, 401, "{on_rest}");
    let (_, on_cli) = cli_raw(port, WRONG, &["people", "list"]);
    same_on_three_faces("unauthorized", &unknown, &on_rest, &serde_json::from_str(&on_cli).unwrap(), refusal("reauthenticate"));

    // A read-only token on a write: request_scope.
    let scoped = mcp(port, READ, "memory.append_note", json!({ "text": "parity note" }));
    let (status, on_rest) = curl(port, READ, "POST", "/v1/memory/notes", Some("parity note"));
    assert_eq!(status, 403, "{on_rest}");
    let (_, on_cli) = cli_raw(port, READ, &["note", "parity", "note"]);
    same_on_three_faces("scope", &scoped, &on_rest, &serde_json::from_str(&on_cli).unwrap(), refusal("request_scope"));

    // A bad argument: fix_input (MCP's -32602, REST's 400).
    let bad = mcp(port, FULL, "memory.search", json!({ "query": "x", "from_ms": "soon" }));
    let (status, on_rest) = curl(port, FULL, "GET", "/v1/memory/search?q=x&from_ms=soon", None);
    assert_eq!(status, 400, "{on_rest}");
    assert_eq!(bad["next_action"], "fix_input", "{bad}");
    assert_eq!(on_rest["next_action"], "fix_input", "{on_rest}");

    // memory.search: no read envelope on any face (E34 left it byte for byte).
    let found = mcp(port, FULL, "memory.search", json!({ "query": "zebrafish" }));
    let (status, on_rest) = curl(port, FULL, "GET", "/v1/memory/search?q=zebrafish", None);
    assert_eq!(status, 200, "{on_rest}");
    for (face, value) in [("mcp", &found), ("rest", &on_rest)] {
        assert!(value.get("usage").is_none(), "{face}: {value}");
    }

    // The plan locks: upgrade_plan.
    locked.store(true, Ordering::SeqCst);
    let plan = mcp(port, FULL, "state.people.list", json!({}));
    let (status, on_rest) = curl(port, FULL, "GET", "/v1/state/people", None);
    assert_eq!(status, 403, "{on_rest}");
    let (_, on_cli) = cli_raw(port, FULL, &["people", "list"]);
    same_on_three_faces("plan", &plan, &on_rest, &serde_json::from_str(&on_cli).unwrap(), refusal("upgrade_plan"));
}
