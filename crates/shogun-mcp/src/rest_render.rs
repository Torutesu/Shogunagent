use shogun_agents::approval::{ApprovalId, ApprovalQueue, ApprovalStatus};
use shogun_agents::entitlement::Entitlements;
use shogun_agents::permission::Level;

use crate::backend::{
    search_request_from_wire, MemoryBackend, MemorySearchError, ReadParams, SearchBoundV1,
    SearchDepthWireV1, SearchSourceWireV1,
};
use crate::meeting_microphone_api;
use crate::memory_import_api::{
    http_status as memory_import_status, is_memory_import_tool, operation as memory_import_operation,
};
use crate::meeting_recap_prefs_api;
use crate::memory_api::{read_inclusion, ReadInclusion, TokenRegistry, Tool};
use crate::visual_recall_api::{is_structured_read, render_structured};
use crate::voice_dictionary_api::{
    parse_import, parse_term, render as render_voice_dictionary, VoiceDictionaryOperation,
};
use crate::voice_microphone_api;

use super::rest_routing::{mail_path_key, mail_write_body};
use super::{route, RestRequest, Routed};

pub fn status_code(routed: &Routed) -> u16 {
    match routed {
        Routed::Unauthorized => 401,
        Routed::PlanLocked | Routed::ScopeInsufficient => 403,
        Routed::NotFound => 404,
        Routed::MethodNotAllowed => 405,
        Routed::Read { .. } | Routed::ApprovalStatus { .. } | Routed::Status | Routed::Metrics => {
            200
        }
        // A write is accepted (L2 still confirms in the Notch); an action may be pending.
        Routed::Write { .. } | Routed::Action => 202,
    }
}

/// Expire stale pending work then expose only id, status, and an optional machine reason.
/// Previews never leave this path.
pub fn poll_approval(id: u64, approvals: &mut ApprovalQueue, now_ms: i64) -> String {
    approvals.expire_due(u64::try_from(now_ms).unwrap_or(0));
    let status = match approvals.status(ApprovalId(id)) {
        Some(ApprovalStatus::Pending) => "pending",
        Some(ApprovalStatus::Rejected) => "rejected",
        Some(ApprovalStatus::TimedOut) => "timed_out",
        Some(ApprovalStatus::Sent) => "sent",
        Some(ApprovalStatus::SendFailed) => "send_failed",
        Some(ApprovalStatus::DraftSaved) => "draft_saved",
        Some(ApprovalStatus::OutcomeUnknown) => "outcome_unknown",
        Some(ApprovalStatus::Approved) => "approved",
        None => "unknown",
    };
    match approvals.failure_reason(ApprovalId(id)) {
        Some(reason) => format!(
            r#"{{"approval_id":{id},"status":"{status}","reason":"{}"}}"#,
            escape(reason)
        ),
        None => format!(r#"{{"approval_id":{id},"status":"{status}"}}"#),
    }
}

/// The stable wire name of a tool (delegates to the shared name).
pub(super) fn tool_name(tool: Tool) -> &'static str {
    tool.wire_name()
}

/// Render backend read items to the API's confidence-gated JSON result (FR-API-06). Shared by the
/// REST and MCP faces so their read output is identical. Low-confidence items are dropped unless
/// `include_low`; medium ones are flagged `possibly`.
pub fn render_reads(tool: Tool, items: &[crate::backend::ReadItem], include_low: bool) -> String {
    let rendered: Vec<String> = items
        .iter()
        .filter_map(|item| match read_inclusion(item.confidence, include_low) {
            ReadInclusion::Included { possibly } => Some(format!(
                r#"{{"text":"{}","confidence":{},"possibly":{}{}{}{}{}}}"#,
                escape(&item.label),
                item.confidence,
                possibly,
                item.key.as_ref().map(|key| format!(r#", "key":"{}""#, escape(key))).unwrap_or_default(),
                item.row
                    .map(|(table, id)| format!(r#","table":"{table}","id":{id}"#))
                    .unwrap_or_default(),
                item.deferred_until.map(|t| format!(r#","deferred_until":{t}"#)).unwrap_or_default(),
                item.sources.as_deref().map(render_sources).unwrap_or_default()
            )),
            ReadInclusion::Excluded => None,
        })
        .collect();
    format!(
        r#"{{"tool":"{}","results":[{}]}}"#,
        tool_name(tool),
        rendered.join(",")
    )
}

/// `,"sources":[{"source":"gmail","count":3},{"source":"capture","app_bundle_id":"…","count":1}]`:
/// where a person's or project's evidence comes from, most-cited first. Only the event's source,
/// app and link count: never its content.
fn render_sources(sources: &[crate::backend::ReadItemSource]) -> String {
    let rows: Vec<String> = sources
        .iter()
        .map(|s| {
            format!(
                r#"{{"source":"{}"{},"count":{}}}"#,
                escape(&s.source),
                s.app_bundle_id.as_deref().map(|app| format!(r#","app_bundle_id":"{}""#, escape(app))).unwrap_or_default(),
                s.count
            )
        })
        .collect();
    format!(r#","sources":[{}]"#, rows.join(","))
}

/// Public JSON string escape (quotes, backslash, control chars) — used by the render helpers.
pub fn escape(s: &str) -> String {
    json_escape(s)
}

pub(crate) fn level_label(level: Level) -> &'static str {
    match level {
        Level::L1 => "L1",
        Level::L2 => "L2",
        Level::L3 => "L3",
    }
}

/// The `/v1/status` body. `port` is the port the server actually bound, which differs from the
/// configured one when that was taken (FR-API-01): `shogun api status | jq -r .port` reads it.
pub fn status_body(port: Option<u16>) -> String {
    match port {
        Some(port) => format!(r#"{{"status":"ok","service":"shogun-memory-api","port":{port}}}"#),
        None => r#"{"status":"ok","service":"shogun-memory-api"}"#.to_string(),
    }
}

/// The 403 body for a locked plan: `{"error":"plan_required"}` as ever, plus `reason` and
/// `message` when the cause is known (issue #744). Same vocabulary as the MCP error `data`.
pub fn plan_locked_body(lock: Option<crate::plan_source::PlanLock>) -> String {
    match lock {
        None => body_for(&Routed::PlanLocked),
        Some(lock) => serde_json::json!({
            "error": "plan_required",
            "reason": lock.code(),
            "message": lock.fix(),
        })
        .to_string(),
    }
}

/// The JSON body for a routing decision. Tool responses stub the data (`results: []`) until the
/// server's backend is wired; the auth/routing envelope is real. Hand-built JSON (no serde dep).
pub fn body_for(routed: &Routed) -> String {
    match routed {
        Routed::Unauthorized => r#"{"error":"unauthorized"}"#.to_string(),
        Routed::PlanLocked => r#"{"error":"plan_required"}"#.to_string(),
        Routed::ScopeInsufficient => serde_json::json!({
            "error": crate::memory_api::SCOPE_INSUFFICIENT,
            "scope": crate::memory_api::TokenScope::Read,
            "message": crate::memory_api::SCOPE_INSUFFICIENT_MESSAGE,
        })
        .to_string(),
        Routed::NotFound => r#"{"error":"not_found"}"#.to_string(),
        Routed::MethodNotAllowed => r#"{"error":"method_not_allowed"}"#.to_string(),
        // The server overrides this with the port it bound (`status_body`).
        Routed::Status => status_body(None),
        // The server overrides this with live metrics; the placeholder keeps the layer pure.
        Routed::Metrics => r#"{"metrics":[]}"#.to_string(),
        Routed::Read { tool, .. } => format!(r#"{{"tool":"{}","results":[]}}"#, tool_name(*tool)),
        Routed::Write { tool, level } => {
            format!(
                r#"{{"tool":"{}","level":"{}","accepted":true}}"#,
                tool_name(*tool),
                level_label(*level)
            )
        }
        Routed::Action => r#"{"tool":"actions.execute","status":"routed"}"#.to_string(),
        Routed::ApprovalStatus { id } => format!(r#"{{"approval_id":{id},"status":"routed"}}"#),
    }
}

/// Route + render with a stub body (no backend). The server uses [`respond_with`]; this stays for
/// callers/tests that don't need real data.
pub fn respond(req: &RestRequest, tokens: &TokenRegistry, ent: &Entitlements) -> (u16, String) {
    let routed = route(req, tokens, ent);
    (status_code(&routed), body_for(&routed))
}

/// A parsed `actions.execute` request: either an on-device action or an external send (with the
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Route + render **with real data** from `backend`. For a read tool, the backend supplies rows and
/// this applies the confidence gate (FR-API-06: Low excluded unless `?include_low`, Medium flagged
/// `possibly`); other decisions render as [`body_for`]. This is what the server calls.
/// HTTP status for a backend write error. [`MemoryBackend::write`] reports errors as strings, so
/// the split follows the shapes the backend produces: a body the caller can fix ("expected …"
/// parse errors, "invalid …", "conflicting …", profile-validation errors, "not a write tool") is a 400, a missing row
/// ("… not found") is a 404, and everything else is a real server-side failure (500). A blanket
/// 500 taught clients to retry bodies the backend can never accept, while a 400 for a genuine
/// server fault taught them to give up on requests that would succeed on retry.
fn write_error_status(reason: &str) -> u16 {
    if reason == "revision_changed" {
        409
    } else if reason == "invalid_change" || reason == "invalid_text" {
        400
    } else if reason == "not_found" || reason.ends_with("not found") {
        404
    } else if reason.ends_with("already exists") || reason.ends_with("already uses this name") {
        // A skill (or other named record) by that name is already there: retrying the same body
        // can never succeed, and the caller has to pick another name (issue #779).
        409
    } else if CALLER_FIXABLE_WRITE_REFUSALS.iter().any(|prefix| reason.starts_with(prefix)) {
        400
    } else if reason.starts_with("insufficient_disk:") {
        // "Compact search index" refused on a nearly full disk (#714 step 3): the caller is told
        // how much space it needs; retrying makes sense once space is freed.
        507
    } else if reason.starts_with("disk_unknown:") || reason.starts_with("purge_pending:") {
        // `purge_pending:` — Visual recall Off is committed but its saved screens are not
        // deleted yet; a fixed backend string, retryable, and the caller must see why.
        503
    } else if reason.starts_with("expected ")
        || reason.starts_with("invalid ")
        || reason.starts_with("conflicting ")
        || reason.starts_with("profile.")
        || reason.starts_with("profile field ")
        // The standing-approval refusal (FR-TG-30): the caller must re-send with
        // confirm_standing=true, so the text has to reach them.
        || reason.starts_with("Standing approval required")
        || reason == "not a write tool"
    {
        400
    } else {
        500
    }
}

/// Fixed backend refusals of a body the caller can correct, beyond the generic shapes
/// [`write_error_status`] already recognises (issue #779): schedule and skill validation, the
/// capture pause window, export folders and files. Each is a constant string in the backend,
/// so returning it with the 400 discloses no path or schema detail.
const CALLER_FIXABLE_WRITE_REFUSALS: &[&str] = &[
    "missing ",
    // memory.append_note over its length cap (`memory_api::APPEND_NOTE_TOO_LONG`).
    "text_too_long:",
    "Invalid ",
    "Resume time must",
    "at must be",
    "days must be",
    "format must be",
    "retention days must",
    // Export folders and file exports (`file_export::ExportError`, `export_folders::FolderError`).
    "no export folder",
    "that folder is not an export folder",
    "folder must be",
    "folder does not exist",
    "choose a folder",
    "too many export folders",
    "file exports need",
    "not a file export",
    "too many files with that name",
    // Skill validation (`skills::SkillError`).
    "larger than",
    "no frontmatter",
    "name must be",
    "permission_level must be",
    "unknown connection",
    "no instructions below",
];

fn structured_read_status(json: &str) -> u16 {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) => {
            let err = v.get("error").and_then(|e| e.as_str());
            match err {
                Some("not_found") | Some("missing_frame_id") => 404,
                Some("meeting_history_unavailable" | "evidence_unavailable" | "neighbors_unavailable" | "knowledge_unavailable" | "processing_unavailable") => 503,
                Some(_) => 400,
                None => 200,
            }
        }
        Err(_) => 500,
    }
}

pub fn respond_with<B: MemoryBackend + ?Sized>(
    req: &RestRequest,
    tokens: &TokenRegistry,
    ent: &Entitlements,
    backend: &B,
) -> (u16, String) {
    let routed = route(req, tokens, ent);
    if let Some(response) = forms_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = memory_import_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = clarify_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = activity_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = ingest_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = memory_fs_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = handoff_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = payments_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = governance_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = export_v1_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = steer_response(&routed, req, backend) {
        return response;
    }
    if let Some(response) = translate_response(&routed, req, backend) {
        return response;
    }
    match routed {
        // Issue #400: the registry the server authenticated against is the whole source; the
        // caller's own scope comes from the same lookup `route` just granted.
        Routed::Read {
            tool: Tool::ApiTokensList,
            ..
        } => match tokens.authenticate(req.token.as_deref()) {
            crate::memory_api::AuthResult::Granted(scope) => (
                200,
                crate::memory_api::render_token_listing(
                    &tokens.listing(),
                    &tokens.connections(),
                    scope,
                ),
            ),
            _ => (401, r#"{"error":"unauthorized"}"#.to_string()),
        },
        Routed::Read {
            tool: Tool::MeetingReminderGet,
            ..
        } => (200, crate::calendar_reminder_api::get()),
        Routed::Read {
            tool: Tool::MeetingSystemAudioStatus,
            ..
        } => (200, crate::meeting_system_audio_api::get()),
        Routed::Read {
            tool: Tool::MeetingMicrophoneGet,
            ..
        } => (200, meeting_microphone_api::get()),
        Routed::Read {
            tool: Tool::TranslateRegisterGet,
            ..
        } => (200, crate::translate_register_api::get()),
        Routed::Read {
            tool: Tool::TranslatePairGet,
            ..
        } => (200, crate::translate_register_api::pair_get()),
        Routed::Read {
            tool: Tool::UiLanguageGet,
            ..
        } => (200, crate::ui_language_api::get()),
        Routed::Read {
            tool: Tool::DeviceUpdateGet,
            ..
        } => (200, crate::update_status_api::get()),
        Routed::Read {
            tool: Tool::MeetingRecapPrefsGet,
            ..
        } => (200, meeting_recap_prefs_api::get()),
        Routed::Read {
            tool: Tool::MeetingStartStatus,
            id,
        } => match id
            .and_then(|id| u64::try_from(id).ok())
            .filter(|id| *id > 0)
        {
            Some(id) => match crate::meeting_record_now_api::status(id) {
                Ok(Some(json)) => (200, json),
                Ok(None) => (404, r#"{"error":"not_found"}"#.to_string()),
                Err(err) => (
                    err.http_status(),
                    serde_json::json!({ "error": err.code() }).to_string(),
                ),
            },
            None => (404, r#"{"error":"not_found"}"#.to_string()),
        },
        Routed::Read {
            tool: Tool::VoiceMicrophoneGet,
            ..
        } => (200, voice_microphone_api::get()),
        Routed::Read {
            tool: tool @ (Tool::VoiceDictionaryList | Tool::VoiceDictionaryExport),
            ..
        } => {
            let operation = if tool == Tool::VoiceDictionaryList {
                VoiceDictionaryOperation::List
            } else {
                VoiceDictionaryOperation::Export
            };
            match backend.manage_voice_dictionary(operation) {
                Ok(value) => (200, render_voice_dictionary(value)),
                Err(_) => (
                    500,
                    r#"{"error":"voice_dictionary_unavailable"}"#.to_string(),
                ),
            }
        }
        Routed::Read { tool, id } => {
            if tool == Tool::MemorySearch {
                // #940 B2b: `?mode=ai` — the desktop widens and reranks on the user's own model;
                // the answer is always results (AI or keyword fallback). The CLI speaks this too.
                match crate::ai_search_api::wants_ai(req.search_mode.as_deref()) {
                    Err(code) => return (400, crate::ai_search_api::refusal_json(code)),
                    Ok(true) => {
                        if req.search_depth.is_some()
                            || req.from_ms.is_some()
                            || req.to_ms.is_some()
                            || req.search_bound_invalid
                            || req.search_source_kind.is_some()
                        {
                            return (400, crate::ai_search_api::refusal_json(crate::ai_search_api::AI_SEARCH_OPTIONS));
                        }
                        let query = req.query.clone().unwrap_or_default();
                        if let Err(error) = crate::backend::validate_search_query(&query) {
                            return (400, format!(r#"{{"error":"{}"}}"#, error.as_str()));
                        }
                        let scope = match tokens.authenticate(req.token.as_deref()) {
                            crate::memory_api::AuthResult::Granted(scope) => scope,
                            _ => return (401, r#"{"error":"unauthorized"}"#.to_string()),
                        };
                        let gate = crate::ai_search_api::ai_gate(scope, ent);
                        return match backend.ai_search(&query, crate::translate_api::ORIGIN_API, gate) {
                            Ok(json) => (crate::ai_search_api::http_status(&json), json),
                            Err(_) => (500, r#"{"error":"search_unavailable"}"#.to_string()),
                        };
                    }
                    Ok(false) => {}
                }
                let request = match search_request_from_wire(
                    req.query.clone().unwrap_or_default(),
                    req.search_depth
                        .as_deref()
                        .map_or(SearchDepthWireV1::Absent, SearchDepthWireV1::String),
                    if req.search_bound_invalid {
                        SearchBoundV1::Invalid
                    } else {
                        req.from_ms
                            .map_or(SearchBoundV1::Absent, SearchBoundV1::Integer)
                    },
                    if req.search_bound_invalid {
                        SearchBoundV1::Invalid
                    } else {
                        req.to_ms
                            .map_or(SearchBoundV1::Absent, SearchBoundV1::Integer)
                    },
                    req.search_source_kind
                        .as_deref()
                        .map_or(SearchSourceWireV1::Absent, SearchSourceWireV1::String),
                ) {
                    Ok(request) => request.with_limit(req.mail.limit),
                    Err(error) => return (400, format!(r#"{{"error":"{}"}}"#, error.as_str())),
                };
                return match backend.search(&request) {
                    Ok(response) => match serde_json::to_string(&response) {
                        Ok(json) => (200, json),
                        Err(_) => (500, r#"{"error":"search_unavailable"}"#.to_string()),
                    },
                    Err(MemorySearchError::Policy(error)) => {
                        (400, format!(r#"{{"error":"{}"}}"#, error.as_str()))
                    }
                    Err(MemorySearchError::Unavailable) => {
                        (500, r#"{"error":"search_unavailable"}"#.to_string())
                    }
                };
            }
            // The context pack takes memory.search's query bound (issue #779), as on MCP.
            if tool == Tool::MemoryGetContextPack {
                if let Err(error) =
                    crate::backend::validate_search_query(req.query.as_deref().unwrap_or_default())
                {
                    return (400, format!(r#"{{"error":"{}"}}"#, error.as_str()));
                }
            }
            let params = ReadParams {
                include_account_labels: matches!(tokens.authenticate(req.token.as_deref()),
                    crate::memory_api::AuthResult::Granted(crate::memory_api::TokenScope::Full)),
                include_low: req.include_low,
                // The REST face carries `?include_scratch` in the call scope (server.rs), not here.
                include_scratch: false,
                id,
                key: mail_path_key(tool, &req.path),
                mail: crate::backend::MailListQuery {
                    include_low: req.include_low,
                    ..req.mail.clone()
                },
                query: req.query.clone(),
                from_ms: req.from_ms,
                to_ms: req.to_ms,
                cursor_ts: req.cursor_ts,
                cursor_id: req.cursor_id,
                cursor_direction: req.cursor_direction.clone(),
                page_size: req.page_size,
                for_generation: req.for_generation,
                app_bundle_id: req.app_bundle_id.clone(),
                person_id: req.person_id.clone(),
                project_id: req.project_id.clone(),
            };
            if is_structured_read(tool) {
                let json = backend
                    .read_structured(tool, &params)
                    .unwrap_or_else(|| r#"{"error":"unavailable"}"#.to_string());
                let status = structured_read_status(&json);
                (status, render_structured(tool, &json))
            } else {
                let items = backend.read(tool, &params);
                if items.is_empty() && crate::memory_api::tool_is_state_get(tool) {
                    return (404, r#"{"error":"not_found"}"#.to_string());
                }
                (200, render_reads(tool, &items, req.include_low))
            }
        }
        Routed::Write { tool, level } => {
            if tool == Tool::DictionaryAutoAddSet
                && crate::dictionary_autoadd_api::needs_approval(req.body.as_deref()) {
                return (501, r#"{"error":"no_approval_surface","detail":"dictionary auto-add needs the ShogunAI app running to confirm it"}"#.to_string());
            }
            if tool == Tool::WritingVoiceSet {
                // Issue #654: an enable is an L3 request for the approval queue, which only the
                // server holds (`server::handle` routes it there before this). With no queue here,
                // refuse it — never hand it to the backend. Every approval refusal below is the
                // same: the request belongs to the queue, so it never reaches the backend here.
                if crate::writing_voice_api::needs_approval(req.body.as_deref()) {
                    return (
                        501,
                        r#"{"error":"no_approval_surface","detail":"external sends need the ShogunAI app running to confirm them"}"#
                            .to_string(),
                    );
                }
            }
            // Issue #657: removing a capture exclusion is an L3 request.
            if tool == Tool::CaptureExclusionsSet
                && crate::capture_exclusions_api::loosening(backend, req.body.as_deref()).is_some()
            {
                return (
                    501,
                    r#"{"error":"no_approval_surface","detail":"removing a capture exclusion needs the ShogunAI app running to confirm it"}"#
                        .to_string(),
                );
            }
            // Issue #110: selecting a subscription CLI is an L3 request.
            if tool == Tool::AgentSourceSet
                && crate::agent_source_api::loosening(backend, req.body.as_deref()).is_some()
            {
                return (
                    501,
                    r#"{"error":"no_approval_surface","detail":"using a subscription needs the ShogunAI app running to confirm it"}"#
                        .to_string(),
                );
            }
            // Issue #897: ending the user's capture pause is an L3 request.
            if tool == Tool::CaptureSetPaused
                && crate::capture_pause_api::unpause_request(backend, req.body.as_deref()).is_some()
            {
                return (
                    501,
                    r#"{"error":"no_approval_surface","detail":"ending a capture pause needs the ShogunAI app running to confirm it"}"#
                        .to_string(),
                );
            }
            // Issue #782: turning Visual recall on or keeping saved screens longer is an L3
            // request.
            if crate::visual_recall_settings_api::loosening(backend, tool, req.body.as_deref()).is_some() {
                return (
                    501,
                    r#"{"error":"no_approval_surface","detail":"turning Visual recall on or keeping saved screens longer needs the ShogunAI app running to confirm it"}"#
                        .to_string(),
                );
            }
            // Issue #779: a Gmail archive / unarchive that no standing approval covers is an
            // approval-queue request.
            if crate::mail_archive_api::is_archive_write(tool) {
                let raw = req.body.as_deref().unwrap_or("");
                let body = mail_write_body(tool, &req.path, raw);
                if crate::mail_archive_api::needs_approval(backend, tool, Some(body.as_deref().unwrap_or(raw)))
                    .is_some()
                {
                    return (
                        501,
                        r#"{"error":"no_approval_surface","detail":"changing Gmail needs the ShogunAI app running to confirm it"}"#
                            .to_string(),
                    );
                }
            }
            // The level the call actually runs at: `writing_voice.set` disable/clear are L1, and
            // an `agent.source.set` or Visual recall setting that reaches the backend is a direct
            // (L1) change — the backend refuses anything that would need approval.
            let level = match (tool, req.body.as_deref().map(crate::writing_voice_api::parse)) {
                (Tool::WritingVoiceSet, Some(Ok(change))) => change.level(),
                (Tool::DictionaryAutoAddSet, _) => Level::L1,
                (Tool::AgentSourceSet, _) => Level::L1,
                (Tool::VisualRecallSetEnabled | Tool::VisualRecallSetRetention, _) => Level::L1,
                // A pause that reaches the backend narrows capture (#897); a resume was queued.
                (Tool::CaptureSetPaused, _) => Level::L1,
                _ => level,
            };
            if tool == Tool::VisualRecallRescanFrame {
                // The answer is the new OCR text: 200 with the text, 404 for an unknown frame.
                let id = req
                    .path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .nth(1)
                    .and_then(|segment| segment.parse::<i64>().ok());
                let json = crate::visual_recall_api::rescan_frame(backend, id);
                return (structured_read_status(&json), render_structured(tool, &json));
            }
            if tool == Tool::TranslateRegisterSet {
                return match crate::translate_register_api::set(req.body.as_deref().unwrap_or("")) {
                    Ok(value) => (202, value),
                    Err(_) => (
                        400,
                        r#"{"error":"translate_register_request_failed"}"#.to_string(),
                    ),
                };
            }
            if tool == Tool::TranslatePairSet {
                return match crate::translate_register_api::pair_set(req.body.as_deref().unwrap_or("")) {
                    Ok(value) => (202, value),
                    Err(_) => (400, r#"{"error":"translate_pair_request_failed"}"#.to_string()),
                };
            }
            if tool == Tool::UiLanguageSet {
                return match crate::ui_language_api::set(req.body.as_deref().unwrap_or("")) {
                    Ok(value) => (202, value),
                    Err(_) => (400, r#"{"error":"language_request_failed"}"#.to_string()),
                };
            }
            if tool == Tool::MeetingSystemAudioRetry {
                return match crate::meeting_system_audio_api::request_retry() {
                    Ok(value) => (202, value),
                    Err(_) => (409, r#"{"error":"meeting_audio_retry_unavailable"}"#.to_string()),
                };
            }
            if tool == Tool::MeetingReminderSet {
                return match crate::calendar_reminder_api::set(req.body.as_deref().unwrap_or("")) {
                    Ok(value) => (202, value),
                    Err(_) => (
                        400,
                        r#"{"error":"meeting_reminder_request_failed"}"#.to_string(),
                    ),
                };
            }
            if tool == Tool::MeetingMicrophoneSet {
                return match meeting_microphone_api::set(req.body.as_deref().unwrap_or("")) {
                    Ok(value) => (202, value),
                    Err(_) => (
                        400,
                        r#"{"error":"meeting_microphone_request_failed"}"#.to_string(),
                    ),
                };
            }
            if tool == Tool::MeetingRecapPrefsSet {
                return match meeting_recap_prefs_api::set(req.body.as_deref().unwrap_or("")) {
                    Ok(value) => (202, value),
                    Err(reason) => (
                        400,
                        serde_json::json!({
                            "error": "meeting_recap_prefs_request_failed",
                            "reason": reason,
                        })
                        .to_string(),
                    ),
                };
            }
            if tool == Tool::MeetingRecordNow {
                return match crate::meeting_record_now_api::request_with(
                    crate::meeting_record_now_api::in_person_from_body(req.body.as_deref().unwrap_or("")),
                ) {
                    Ok(value) => (202, value),
                    Err(err) => (
                        err.http_status(),
                        serde_json::json!({ "error": err.code() }).to_string(),
                    ),
                };
            }
            if tool == Tool::VoiceMicrophoneSet {
                // `reason` is one of this module's own fixed strings — the caller has to see *why*
                // the device was refused, or a rejected selection looks like a silent no-op (#246).
                return match voice_microphone_api::set(req.body.as_deref().unwrap_or("")) {
                    Ok(value) => (202, value),
                    Err(reason) => (
                        400,
                        serde_json::json!({
                            "error": "voice_microphone_request_failed",
                            "reason": reason,
                        })
                        .to_string(),
                    ),
                };
            }
            let voice_operation = match tool {
                Tool::VoiceDictionaryCreate => parse_term(req.body.as_deref().unwrap_or(""))
                    .map(VoiceDictionaryOperation::Create),
                Tool::VoiceDictionaryUpdate => {
                    let id = req
                        .path
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .and_then(|value| value.parse().ok());
                    match (id, parse_term(req.body.as_deref().unwrap_or(""))) {
                        (Some(id), Ok(term)) => Ok(VoiceDictionaryOperation::Update { id, term }),
                        _ => Err("invalid voice dictionary term".to_string()),
                    }
                }
                Tool::VoiceDictionaryDelete => {
                    let id = req
                        .path
                        .trim_end_matches('/')
                        .split('/')
                        .rev()
                        .nth(1)
                        .and_then(|value| value.parse().ok());
                    id.map(|id| VoiceDictionaryOperation::Delete { id })
                        .ok_or_else(|| "invalid voice dictionary term".to_string())
                }
                Tool::VoiceDictionaryImport => parse_import(req.body.as_deref().unwrap_or("")),
                _ => {
                    let raw_body = req.body.as_deref().unwrap_or("");
                    // A mail write-by-key folds the path's key into the body so the backend sees
                    // the same JSON the MCP face sends (invariant 6).
                    let injected = mail_write_body(tool, &req.path, raw_body);
                    let body = injected.as_deref().unwrap_or(raw_body);
                    // A Gmail-side change is recorded as an intent for the desktop's write-back
                    // tick, never executed here — the caller sees it as pending (FR-API-04).
                    // A skill run or a schedule needs agent execution but is not pending, on
                    // every face (issue #779).
                    let pending = if crate::memory_api::tool_write_is_pending(tool) {
                        r#","pending":true"#
                    } else {
                        ""
                    };
                    // A hosted disconnect answers what it did (E51), as MCP does: the standard
                    // write fields plus the vault's receipt with `vendor_revoke`.
                    if tool == Tool::ConnectionsDisconnect {
                        if let Some(receipt) = backend.connections_disconnect(body) {
                            return match receipt {
                                Ok(receipt) => (
                                    202,
                                    crate::connections_api::disconnect_answer(
                                        serde_json::json!({ "tool": tool_name(tool), "level": level_label(level), "accepted": true }),
                                        &receipt,
                                    ),
                                ),
                                Err(reason) => match write_error_status(&reason) {
                                    500 => (500, r#"{"error":"write_failed"}"#.to_string()),
                                    status => (status, format!(r#"{{"error":"write_failed","reason":"{}"}}"#, escape(&reason))),
                                },
                            };
                        }
                    }
                    return match backend.write(tool, body) {
                        // A user note: memory.notes.list / notes.change see it by this id.
                        Ok(Some(id)) if tool == Tool::MemoryAppendNote => (
                            202,
                            format!(
                                r#"{{"tool":"{}","level":"{}","note_id":{id},"accepted":true}}"#,
                                tool_name(tool),
                                level_label(level),
                            ),
                        ),
                        Ok(Some(id)) => (
                            202,
                            format!(
                                r#"{{"tool":"{}","level":"{}","id":{},"accepted":true{pending}}}"#,
                                tool_name(tool),
                                level_label(level),
                                id
                            ),
                        ),
                        Ok(None) if tool == Tool::CaptureExclusionsSet => (
                            202,
                            backend
                                .read_structured(Tool::CaptureExclusionsGet, &ReadParams::default())
                                .unwrap_or_else(|| r#"{"error":"unavailable"}"#.to_string()),
                        ),
                        Ok(None) => (
                            202,
                            format!(
                                r#"{{"tool":"{}","level":"{}","accepted":true{pending}}}"#,
                                tool_name(tool),
                                level_label(level)
                            ),
                        ),
                        Err(reason) => match write_error_status(&reason) {
                            // Backend failures can contain database paths or schema details.
                            // Keep them in logs, not the public REST response.
                            500 => (500, r#"{"error":"write_failed"}"#.to_string()),
                            status => (
                                status,
                                format!(
                                    r#"{{"error":"write_failed","reason":"{}"}}"#,
                                    escape(&reason)
                                ),
                            ),
                        },
                    };
                }
            };
            // A malformed request body/path is the caller's mistake (400); a backend failure is
            // classified like any write error — a db fault must not masquerade as a 400 the
            // client will never retry, nor a missing term as a server error.
            match voice_operation {
                Err(_) => (
                    400,
                    r#"{"error":"voice_dictionary_request_failed"}"#.to_string(),
                ),
                Ok(operation) => match backend.manage_voice_dictionary(operation) {
                    Ok(value) => (202, render_voice_dictionary(value)),
                    Err(reason) => match write_error_status(&reason) {
                        500 => (
                            500,
                            r#"{"error":"voice_dictionary_unavailable"}"#.to_string(),
                        ),
                        status => (
                            status,
                            format!(
                                r#"{{"error":"voice_dictionary_request_failed","reason":"{}"}}"#,
                                escape(&reason)
                            ),
                        ),
                    },
                },
            }
        }
        other => (status_code(&other), body_for(&other)),
    }
}

/// Memory import (#384 / #391) on REST: the body is the shared request JSON, the id comes from the
/// path (`/v1/memory/imports/{id}[/delete]`). `None` for every other route.
fn memory_import_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let tool = match routed {
        Routed::Read { tool, .. } | Routed::Write { tool, .. } if is_memory_import_tool(*tool) => *tool,
        _ => return None,
    };
    let path_id = req
        .path
        .trim_end_matches('/')
        .split('/')
        .skip_while(|seg| *seg != "imports")
        .nth(1)
        .and_then(|seg| seg.parse::<i64>().ok());
    let Some(operation) =
        memory_import_operation(tool, req.body.as_deref().unwrap_or(""), path_id, req.include_low)
    else {
        return Some((404, r#"{"error":"not_found"}"#.to_string()));
    };
    Some(match backend.memory_import(operation) {
        Ok(json) => (memory_import_status(tool, &json), json),
        // A store fault can carry schema detail: keep it in logs, not the response.
        Err(_) => (500, r#"{"error":"memory_import_unavailable"}"#.to_string()),
    })
}

/// `POST /v1/memory/events` and `POST /v1/memory/session`: the body is the same JSON the MCP
/// face takes as `arguments`. `None` for every other route.
fn activity_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let Routed::Read { tool, .. } = routed else {
        return None;
    };
    if !crate::activity_api::is_activity_tool(*tool) {
        return None;
    }
    let args = serde_json::from_str::<serde_json::Value>(req.body.as_deref().unwrap_or("{}"))
        .unwrap_or(serde_json::Value::Null);
    let refusal = |message: String| {
        let code = message.split(':').next().unwrap_or("invalid_request").to_string();
        (400, serde_json::json!({ "error": code, "message": message }).to_string())
    };
    if !args.is_object() {
        return Some(refusal("invalid_request: the body must be a JSON object.".into()));
    }
    let request = match crate::activity_api::request(*tool, &args) {
        Ok(request) => request,
        Err(message) => return Some(refusal(message)),
    };
    Some(match backend.activity(&request) {
        Ok(json) => (200, render_structured(*tool, &json)),
        Err(_) => (503, r#"{"error":"memory_unavailable"}"#.to_string()),
    })
}

/// `memory.ingest` / `memory.recall` on REST (E4-T3): `POST /v1/memory/ingest` (an L1 write
/// under an idempotency key) and `POST /v1/memory/recall` (a read with a body), the MCP
/// arguments as the body. One parser, one backend call and one envelope with MCP
/// ([`crate::ingest_api::serve`]). `None` for every other route.
fn ingest_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let tool = match routed {
        Routed::Read { tool, .. } | Routed::Write { tool, .. }
            if crate::ingest_api::is_ingest_tool(*tool) =>
        {
            *tool
        }
        _ => return None,
    };
    let args = serde_json::from_str::<serde_json::Value>(req.body.as_deref().unwrap_or("{}"))
        .unwrap_or(serde_json::Value::Null);
    Some(match crate::ingest_api::serve(tool, &args, backend) {
        Ok(text) => (200, text),
        Err(crate::ingest_api::Refusal::BadArguments(message)) => {
            let code = message.split(':').next().unwrap_or("invalid_request").to_string();
            (400, serde_json::json!({ "error": code, "message": message }).to_string())
        }
        Err(crate::ingest_api::Refusal::Unavailable(code)) => {
            (503, serde_json::json!({ "error": code }).to_string())
        }
    })
}

/// Export format v1 on REST (E7): `POST /v1/export` with the `state.export` arguments as the
/// body (`format` defaults to `export-v1` here; `export-v2` selects v2, E27). `GET
/// /v1/state/export` keeps the Markdown export.
fn export_v1_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    if !matches!(routed, Routed::Read { tool: Tool::StateExport, .. }) || req.method != super::Method::Post {
        return None;
    }
    let mut args = serde_json::from_str::<serde_json::Value>(req.body.as_deref().unwrap_or("{}"))
        .unwrap_or(serde_json::Value::Null);
    if let Some(object) = args.as_object_mut() {
        object.entry("format").or_insert_with(|| serde_json::Value::String(crate::export_api::FORMAT_ARG.into()));
    }
    Some(match crate::export_api::serve(&args, backend) {
        Ok(text) => (200, text),
        Err(crate::ingest_api::Refusal::BadArguments(message)) => {
            let code = message.split(':').next().unwrap_or("invalid_request").to_string();
            (400, serde_json::json!({ "error": code, "message": message }).to_string())
        }
        Err(crate::ingest_api::Refusal::Unavailable(code)) => {
            (503, serde_json::json!({ "error": code }).to_string())
        }
    })
}

/// `memory.fs` on REST (E6-T2): `POST /v1/memory/fs`, the MCP arguments as the body. A
/// tool-level error (a missing path, a duplicate `old_str`) is still a 200 with `result.error`,
/// as the memory tool answers it; a malformed body is 400. `None` for every other route.
fn memory_fs_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    if !matches!(routed, Routed::Write { tool: Tool::MemoryFs, .. }) {
        return None;
    }
    let args = serde_json::from_str::<serde_json::Value>(req.body.as_deref().unwrap_or("{}"))
        .unwrap_or(serde_json::Value::Null);
    Some(match crate::memfs_tool::serve(&args, backend) {
        Ok((text, _)) => (200, text),
        Err(crate::ingest_api::Refusal::BadArguments(message)) => {
            let code = message.split(':').next().unwrap_or("invalid_request").to_string();
            (400, serde_json::json!({ "error": code, "message": message }).to_string())
        }
        Err(crate::ingest_api::Refusal::Unavailable(code)) => {
            (503, serde_json::json!({ "error": code }).to_string())
        }
    })
}

/// The A2A handoff on REST (E35): `POST /v1/handoff` (`handoff.create`, a read with a body) and
/// `POST /v1/handoff/accept` (`handoff.accept`, an L1 write to the caller's scratch), the MCP
/// arguments as the body. One parser, one backend path and one envelope with MCP and the CLI
/// ([`crate::handoff_api::serve`]); a refusal names its code, message and `next_action`. `None`
/// for every other route.
fn handoff_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let tool = match routed {
        Routed::Read { tool, .. } | Routed::Write { tool, .. } if crate::handoff_api::is_handoff_tool(*tool) => *tool,
        _ => return None,
    };
    let args = serde_json::from_str::<serde_json::Value>(req.body.as_deref().unwrap_or("{}"))
        .unwrap_or(serde_json::Value::Null);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Some(match crate::handoff_api::serve(tool, &args, backend, now_ms) {
        Ok(text) => (200, text),
        Err(crate::ingest_api::Refusal::BadArguments(message)) => {
            let code = message.split(':').next().unwrap_or("invalid_request").to_string();
            let status = if code == "handoff_not_for_you" { 403 } else { 400 };
            let next = crate::handoff_api::refusal_next_action(&code);
            (status, serde_json::json!({ "error": code, "message": message, "next_action": next }).to_string())
        }
        Err(crate::ingest_api::Refusal::Unavailable(code)) => {
            (503, serde_json::json!({ "error": code, "next_action": "retry_after" }).to_string())
        }
    })
}

/// The JSON arguments of a `payments.*` REST call: the body (an empty body is `{}`), or the 400.
pub fn payments_args(body: Option<&str>) -> Result<serde_json::Value, (u16, String)> {
    match body.map(str::trim).filter(|b| !b.is_empty()) {
        None => Ok(serde_json::json!({})),
        Some(raw) => serde_json::from_str::<serde_json::Value>(raw).ok().filter(serde_json::Value::is_object).ok_or_else(|| {
            (
                400,
                serde_json::json!({ "error": "invalid_json", "message": "The body must be one JSON object: the tool's arguments.", "next_action": "fix_arguments" })
                    .to_string(),
            )
        }),
    }
}

/// `payments.*` on REST without an approval queue (E46): the server answers these routes with
/// its queue before this renderer; reaching here, an L3 request is refused as having no approval
/// surface, exactly like an `actions.execute` send on a headless face. `None` for other routes.
fn payments_response<B: MemoryBackend + ?Sized>(routed: &Routed, req: &RestRequest, backend: &B) -> Option<(u16, String)> {
    let tool = match routed {
        Routed::Read { tool, .. } | Routed::Write { tool, .. } if crate::payments_api::is_payments_tool(*tool) => *tool,
        _ => return None,
    };
    let args = match payments_args(req.body.as_deref()) {
        Ok(args) => args,
        Err(answer) => return Some(answer),
    };
    let actor = shogun_memory::actor::current_write().actor;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Some(crate::payments_api::serve(tool, &args, backend, actor.as_str(), now_ms, |_, _| {
        (
            501,
            r#"{"error":"no_approval_surface","detail":"payments outside a mandate and mandate grants need a human approval surface"}"#.to_string(),
        )
    }))
}

/// E18 on REST: `GET /v1/audit?since_ms=&limit=` and `GET /v1/acl`. The approval reads need the
/// queue and are answered by the server; reaching here they are unavailable. `None` for every
/// other route.
fn governance_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let Routed::Read { tool, .. } = routed else {
        return None;
    };
    let unavailable = |code: String| (503, serde_json::json!({ "error": code, "next_action": "retry_after" }).to_string());
    match tool {
        Tool::AuditList => {
            let mut args = serde_json::Map::new();
            if let Some(since) = req.mail.since_ms {
                args.insert("since_ms".into(), since.into());
            }
            if let Some(limit) = req.mail.limit {
                args.insert("limit".into(), limit.into());
            }
            Some(match crate::governance_api::audit_query(&serde_json::Value::Object(args)) {
                Ok(query) => match backend.audit_list(&query) {
                    Ok(value) => (200, value.to_string()),
                    Err(code) => unavailable(code),
                },
                Err(message) => {
                    let code = message.split(':').next().unwrap_or("invalid_request").to_string();
                    (400, serde_json::json!({ "error": code, "message": message }).to_string())
                }
            })
        }
        Tool::AclList => Some(match backend.acl(&crate::governance_api::AclOperation::List, "human:owner") {
            Ok(value) => (200, value.to_string()),
            Err(code) => unavailable(code),
        }),
        Tool::ApprovalsList | Tool::ApprovalsGet => Some(unavailable("approvals_unavailable".to_string())),
        _ => None,
    }
}

/// Clarify (issue #394) on REST: `GET /v1/actions/questions[/{id}]` and `POST /v1/actions/answer`
/// (`{"id", "answer"}`). The origin is this face's (`api` — REST and the CLI); the L3 approval
/// queue is never touched. `None` for every other route.
fn clarify_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let (tool, id) = match routed {
        Routed::Read { tool, id } if crate::clarify_api::is_clarify_tool(*tool) => (*tool, *id),
        Routed::Write { tool, .. } if crate::clarify_api::is_clarify_tool(*tool) => (*tool, None),
        _ => return None,
    };
    let operation = match crate::clarify_api::operation(
        tool,
        req.body.as_deref().unwrap_or(""),
        id,
        shogun_agents::approval::ApprovalOrigin::Api,
    ) {
        Ok(operation) => operation,
        Err(code) => return Some((400, crate::clarify_api::refusal_json(code, None))),
    };
    Some(match backend.clarify(operation) {
        Ok(json) => (crate::clarify_api::http_status(&json), json),
        // A store fault can carry schema detail: keep it out of the response.
        Err(_) => (503, crate::clarify_api::refusal_json("unavailable", None)),
    })
}

/// Steering (issue #395) on REST: `POST /v1/actions/steer` (`{"kind", "id", "text"}`). The origin
/// is this face's (`api` — REST and the CLI); the L3 approval queue is never touched. `None` for
/// every other route.
fn steer_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    if !matches!(routed, Routed::Write { tool: Tool::ActionsSteer, .. }) {
        return None;
    }
    let operation = match crate::steer_api::operation(
        req.body.as_deref().unwrap_or(""),
        shogun_agents::steer::SteerOrigin::Api,
    ) {
        Ok(operation) => operation,
        Err(code) => return Some((400, crate::steer_api::refusal_json(code))),
    };
    Some(match backend.steer(operation) {
        Ok(json) => (crate::steer_api::http_status(&json), json),
        // A store fault can carry detail: keep it out of the response.
        Err(_) => (503, crate::steer_api::refusal_json("unavailable")),
    })
}

/// Translation and reply drafts (issue #279) on REST: `POST /v1/translate`,
/// `POST /v1/translate/reply_draft`, `GET /v1/translate/jobs/<id>`. The origin is this face's
/// (`api` — REST and the CLI). `None` for every other route.
fn translate_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let (tool, id) = match routed {
        Routed::Write { tool, .. } if crate::translate_api::is_translate_job_tool(*tool) => (*tool, None),
        Routed::Read { tool, id } if crate::translate_api::is_translate_job_tool(*tool) => (*tool, *id),
        _ => return None,
    };
    let operation = match crate::translate_api::operation(
        tool,
        req.body.as_deref().unwrap_or(""),
        id,
        crate::translate_api::ORIGIN_API,
    ) {
        Ok(operation) => operation,
        Err(code) => return Some((400, crate::translate_api::refusal_json(code, None))),
    };
    Some(match backend.translate(operation) {
        Ok(json) => (crate::translate_api::http_status(&json), json),
        // A store fault can carry schema detail: keep it out of the response.
        Err(_) => (503, crate::translate_api::refusal_json("unavailable", None)),
    })
}

fn forms_response<B: MemoryBackend + ?Sized>(
    routed: &Routed,
    req: &RestRequest,
    backend: &B,
) -> Option<(u16, String)> {
    let tool = match routed {
        Routed::Read { tool, .. } | Routed::Write { tool, .. }
            if crate::forms_api::is_forms_tool(*tool) =>
        {
            *tool
        }
        _ => return None,
    };
    let path_id = if tool == Tool::FormsStatus {
        req.path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .and_then(|id| id.parse::<u64>().ok())
    } else {
        None
    };
    let operation =
        match crate::forms_api::operation(tool, req.body.as_deref().unwrap_or("{}"), path_id) {
            Ok(operation) => operation,
            Err(code) => return Some((400, crate::forms_api::refusal_json(code))),
        };
    let auth = crate::forms_api::FormsAuth {
        token: req.token.clone(),
        admitted_by_token: true,
        client: None,
    };
    let body = backend
        .forms(operation, auth)
        .unwrap_or_else(|_| crate::forms_api::refusal_json("form_unavailable"));
    Some((crate::forms_api::http_status(&body), body))
}

#[cfg(test)]
mod poll_tests {
    use super::*;
    use shogun_agents::approval::{
        ApprovalOrigin, ApprovalQueue, ApprovalStatus, ConfirmIntent, Preview, Route,
    };
    use shogun_agents::permission::SendAction;

    fn queue_with_failed_send(reason: Option<&str>) -> (ApprovalQueue, u64) {
        let mut q = ApprovalQueue::new();
        let action = SendAction::SendEmail {
            to: "a@b.com".into(),
        };
        let id = q
            .try_request(
                action.clone(),
                Preview::for_send(&action, "private body", Route::ViaComposio),
                ApprovalOrigin::Api,
                1,
            )
            .expect("id");
        assert!(matches!(
            q.confirm(id, ConfirmIntent::DedicatedButton, 2),
            shogun_agents::approval::Decision::Confirmed(_)
        ));
        assert!(q.mark_status_with_reason(id, ApprovalStatus::SendFailed, 3, reason));
        (q, id.0)
    }

    #[test]
    fn poll_includes_provider_policy_stop_reason() {
        let (mut q, id) = queue_with_failed_send(Some("provider_policy_stop"));
        let body = poll_approval(id, &mut q, 4);
        assert_eq!(
            body,
            format!(r#"{{"approval_id":{id},"status":"send_failed","reason":"provider_policy_stop"}}"#)
        );
        assert!(!body.contains("private"));
    }

    #[test]
    fn poll_omits_reason_when_none() {
        let (mut q, id) = queue_with_failed_send(None);
        let body = poll_approval(id, &mut q, 4);
        assert_eq!(body, format!(r#"{{"approval_id":{id},"status":"send_failed"}}"#));
        assert!(!body.contains("reason"));
    }
}

#[cfg(test)]
mod memory_note_error_tests {
    #[test]
    fn caller_can_recover_from_stale_or_invalid_changes() {
        assert_eq!(super::write_error_status("revision_changed"),409);
        assert_eq!(super::write_error_status("invalid_text"),400);
        assert_eq!(super::write_error_status("not_found"),404);
        assert_eq!(super::write_error_status("memory_unavailable"),500);
    }
}
