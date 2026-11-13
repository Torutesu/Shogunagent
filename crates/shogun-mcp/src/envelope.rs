//! The metered envelope every face adds to a core answer (E34, invariant 4: three faces, one
//! result). One module, so MCP, REST and the CLI (which prints the REST body it receives) cannot
//! drift:
//!
//! - **a refusal says what to do next** (`next_action`, CLAUDE.md "Every metered response returns
//!   … on error `next_action`"): MCP puts it in a JSON-RPC error's `data` and in an `isError`
//!   payload, REST in its error body. The value comes from the refusal's code through one table,
//!   [`next_action_for_code`]; MCP's numeric JSON-RPC codes are first named
//!   ([`rpc_refusal_code`]) so `-32001` and REST's `401 unauthorized` both answer
//!   `reauthenticate`. Agents branch on it, never on the message; an unknown value means "stop and
//!   tell the human" (`docs/api/deprecation-policy.md`).
//! - **a state read or `profile.whoami` carries its usage and latency**: `usage`
//!   `{unit: "read", quantity: 1, cost_micro_usd: 0}` (reads are free, invariant 10),
//!   `latency_ms` and `next_action: null`.
//!
//! Only envelope keys are added, and only when absent: a body that already names its own
//! `next_action` (`wait`, `narrow_query`, `approve_at_url`, `null` …) or its own `usage` keeps it.
//! `memory.search` and `memory.get_events` carry no read envelope: their answers are compared byte
//! for byte across the faces, and `memory.recall` renders its own.

use serde_json::{json, Map, Value};

use crate::memory_api::{tool_level, ApiLevel, Tool};

/// The `usage` of a free read (invariant 10).
pub fn read_usage() -> Value {
    json!({ "unit": "read", "quantity": 1, "cost_micro_usd": 0 })
}

/// Whether `tool`'s successful answer carries the read envelope: the state reads and
/// `profile.whoami` and memory processing status, on every face.
pub fn carries_read_envelope(tool: Tool) -> bool {
    tool_level(tool) == ApiLevel::Read && (tool.wire_name().starts_with("state.")
        || matches!(tool, Tool::ProfileWhoami | Tool::MemoryProcessingStatus))
}

/// The refusal code a JSON-RPC error code stands for, in REST's vocabulary.
pub fn rpc_refusal_code(code: i64) -> &'static str {
    match code {
        -32700 => "parse_error",
        -32600 => "invalid_request",
        -32601 => "method_not_found",
        -32602 => "invalid_params",
        crate::mcp::TOKEN_INVALID_CODE => "unauthorized",
        -32003 => "plan_required",
        crate::mcp::SCOPE_INSUFFICIENT_CODE => "scope_insufficient",
        crate::source_policy::SOURCE_NOT_ALLOWED_CODE => "source_not_allowed",
        crate::mcp::MEMORY_UNAVAILABLE_CODE => "memory_unavailable",
        crate::mcp::RATE_LIMITED_CODE => "rate_limited",
        _ => "internal",
    }
}

/// What a refusal with `code` tells the caller to do next, or `None` for a code this table does
/// not know (the caller then falls back on the kind of failure). The one table all faces use.
pub fn next_action_for_code(code: &str) -> Option<&'static str> {
    let fix_input = ["invalid_", "missing_", "bad_"].iter().any(|prefix| code.starts_with(prefix))
        && code != "invalid_backend_response";
    Some(match code {
        _ if fix_input => "fix_input",
        "parse_error" | "method_not_found" | "not_found" | "method_not_allowed" | "payload_too_large" => "fix_input",
        "unauthorized" => "reauthenticate",
        "plan_required" => "upgrade_plan",
        "scope_insufficient" => "request_scope",
        "source_not_allowed" | "principal_purged" => "ask_human",
        "revision_changed" => "reread",
        "rate_limited"
        | "memory_unavailable"
        | "processing_unavailable"
        | "memory_update_pending"
        | "memory_newer_than_app"
        | "unavailable"
        | "upstream_error"
        | "internal"
        | "invalid_backend_response"
        | "principal_busy"
        | "desktop_unavailable" => "retry_after",
        _ => return None,
    })
}

/// `next_action` of a JSON-RPC error: a fault the table does not name is retried.
pub fn next_action_for_rpc(code: i64) -> &'static str {
    next_action_for_code(rpc_refusal_code(code)).unwrap_or("retry_after")
}

/// `next_action` of an MCP `isError` payload (a tool that ran but had no answer): an unknown code
/// goes to the human.
pub fn next_action_for_tool_error(code: &str) -> &'static str {
    next_action_for_code(code).unwrap_or("ask_human")
}

/// `next_action` of a REST refusal: the code first (so it matches the MCP face), else the status —
/// a 4xx about the request is fixed by the caller, a 5xx is retried, anything else is the human's.
pub fn next_action_for_http(status: u16, code: &str) -> &'static str {
    next_action_for_code(code).unwrap_or(match status {
        400 | 404 | 405 | 409 | 413 | 422 => "fix_input",
        401 => "reauthenticate",
        429 => "retry_after",
        s if s >= 500 => "retry_after",
        _ => "ask_human",
    })
}

/// A JSON-RPC error object's `data` with its `next_action` (kept when already present). `false`
/// when `data` is not an object (left as it is).
pub fn add_rpc_next_action(error: &mut Map<String, Value>) -> bool {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
    match error.get_mut("data") {
        Some(Value::Object(data)) => {
            data.entry("next_action").or_insert_with(|| Value::String(next_action_for_rpc(code).into()));
            true
        }
        Some(_) => false,
        None => {
            error.insert("data".into(), json!({ "next_action": next_action_for_rpc(code) }));
            true
        }
    }
}

/// A JSON-RPC error response with its `next_action`, for the refusals answered before a tool runs
/// (the HTTP transport's header checks, a multi-principal bearer the resolver does not know).
pub fn rpc_error(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message, "data": { "next_action": next_action_for_rpc(code) } },
    })
    .to_string()
}

/// Add the read envelope to a successful answer's payload (keys already present are kept). `false`
/// when the answer already carries its own `usage`.
pub fn add_read_envelope(payload: &mut Map<String, Value>, latency_ms: u64) -> bool {
    if payload.contains_key("usage") {
        return false;
    }
    payload.insert("usage".into(), read_usage());
    payload.entry("latency_ms").or_insert_with(|| Value::from(latency_ms));
    payload.entry("next_action").or_insert(Value::Null);
    true
}

/// The REST face's envelope over one `(status, body)`: a refusal (4xx/5xx with a string `error`)
/// gets `next_action`; a 2xx answer of a tool that [`carries_read_envelope`] gets the read
/// envelope. Any other body, and any body that is not a JSON object, is returned unchanged. The
/// CLI prints this body as it receives it, so its JSON output is the same.
pub fn rest_body(status: u16, body: String, read: Option<Tool>, latency_ms: u64) -> String {
    let refusal = status >= 400;
    let read = !refusal && (200..300).contains(&status) && read.is_some_and(carries_read_envelope);
    if !refusal && !read {
        return body;
    }
    let Ok(Value::Object(mut payload)) = serde_json::from_str::<Value>(&body) else {
        return body;
    };
    if refusal {
        if payload.contains_key("next_action") {
            return body;
        }
        let Some(code) = payload.get("error").and_then(Value::as_str).map(str::to_string) else {
            return body;
        };
        payload.insert("next_action".into(), Value::String(next_action_for_http(status, &code).into()));
    } else if !add_read_envelope(&mut payload, latency_ms) {
        return body;
    }
    Value::Object(payload).to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The same refusal on each face answers the same `next_action`: the JSON-RPC code and the REST
    /// status/code pair it corresponds to.
    #[test]
    fn rpc_codes_and_rest_refusals_share_one_table() {
        for (rpc, status, code, next) in [
            (-32602, 400, "invalid_params", "fix_input"),
            (crate::mcp::TOKEN_INVALID_CODE, 401, "unauthorized", "reauthenticate"),
            (-32003, 403, "plan_required", "upgrade_plan"),
            (crate::mcp::SCOPE_INSUFFICIENT_CODE, 403, "scope_insufficient", "request_scope"),
            (crate::source_policy::SOURCE_NOT_ALLOWED_CODE, 403, "source_not_allowed", "ask_human"),
            (crate::mcp::MEMORY_UNAVAILABLE_CODE, 503, "memory_update_pending", "retry_after"),
            (crate::mcp::RATE_LIMITED_CODE, 429, "rate_limited", "retry_after"),
            (-32000, 500, "internal", "retry_after"),
        ] {
            assert_eq!(next_action_for_rpc(rpc), next, "{rpc}");
            assert_eq!(next_action_for_http(status, code), next, "{status} {code}");
        }
        // The `isError` codes and REST's bodies for the same refusals.
        for (code, status, next) in [
            ("not_found", 404, "fix_input"),
            ("revision_changed", 409, "reread"),
            ("missing_text", 400, "fix_input"),
            ("unavailable", 503, "retry_after"),
        ] {
            assert_eq!(next_action_for_tool_error(code), next);
            assert_eq!(next_action_for_http(status, code), next);
        }
        assert_eq!(next_action_for_tool_error("something_new"), "ask_human");
        assert_eq!(next_action_for_http(400, "translate_register_request_failed"), "fix_input");
        assert_eq!(next_action_for_http(502, "something_new"), "retry_after");
        assert_eq!(next_action_for_http(410, "principal_purged"), "ask_human");
    }

    #[test]
    fn the_rest_body_gains_only_envelope_keys() {
        let refused = rest_body(404, r#"{"error":"not_found"}"#.into(), Some(Tool::StatePeopleGet), 3);
        assert_eq!(refused, r#"{"error":"not_found","next_action":"fix_input"}"#);
        // Its own `next_action` (even null) wins.
        let own = r#"{"error":"cross_principal_forbidden","next_action":null}"#.to_string();
        assert_eq!(rest_body(403, own.clone(), None, 1), own);
        let read: Value = serde_json::from_str(&rest_body(200, r#"{"tool":"state.people.list","results":[]}"#.into(), Some(Tool::StatePeopleList), 7)).unwrap();
        assert_eq!(read["usage"], read_usage());
        assert_eq!(read["latency_ms"], 7);
        assert_eq!(read["next_action"], Value::Null);
        assert_eq!(read["results"], json!([]));
        // No read envelope on search (byte-identical across faces) or on a write's 202.
        let search = r#"{"tool":"memory.search","results":[]}"#.to_string();
        assert_eq!(rest_body(200, search.clone(), Some(Tool::MemorySearch), 1), search);
        let note = r#"{"accepted":true}"#.to_string();
        assert_eq!(rest_body(202, note.clone(), Some(Tool::MemoryAppendNote), 1), note);
        // Not JSON, or no `error`: unchanged.
        assert_eq!(rest_body(500, "oops".into(), None, 1), "oops");
        assert_eq!(rest_body(500, r#"{"metrics":[]}"#.into(), None, 1), r#"{"metrics":[]}"#);
    }

    #[test]
    fn an_rpc_error_names_its_next_action() {
        let v: Value = serde_json::from_str(&rpc_error(json!(1), crate::mcp::TOKEN_INVALID_CODE, "unauthorized: x")).unwrap();
        assert_eq!(v["error"]["data"]["next_action"], "reauthenticate");
        let mut kept = json!({ "code": -32029, "data": { "next_action": "wait" } });
        assert!(add_rpc_next_action(kept.as_object_mut().unwrap()));
        assert_eq!(kept["data"]["next_action"], "wait");
    }
}
