//! Map a parsed [`Command`] to the HTTP call the CLI makes against the local Memory API server
//! (§6.11). Timed pause resolves a wall-clock deadline; socket work is [`crate::http`]. This is the CLI's half of the REST
//! contract; the server's half is `shogun_mcp::rest`.

use crate::command::{
    is_mail_label_key, AgentSourceCommand, BackupCommand, CaptureExclusionsCommand, Command, ConnectionsCommand, DisputesCommand, ExportCommand, ImportCommand, StateCommand, ScheduleCommand, SkillsCommand, LessonsCommand, LessonsList, ListOrGet,
    MailCommand, MailLabelSet, MailTriageList, MicrophoneCommand, PatternsCommand, SpeakersCommand,
    TemplatesCommand, TranslateCommand, TranslateRegisterCommand, VisualRecallCommand, VoiceDictionaryCommand, WebCommand,
    ImportSource, RecapEditBody, RecapPrefsCommand, SearchIndexCommand,
};

/// Read a `voice-dictionary import` source (a file path or stdin) into text so [`to_call`] can
/// build the request. Every other command passes through untouched. The read happens here, in
/// the binary's I/O layer, so the wire mapping stays pure and testable.
pub fn resolve_import_source(command: Command) -> Result<Command, String> {
    let Command::VoiceDictionary(VoiceDictionaryCommand::Import { source }) = command else {
        return Ok(command);
    };
    let text = match source {
        ImportSource::Text(text) => text,
        ImportSource::Stdin => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .map_err(|error| format!("could not read stdin: {error}"))?;
            text
        }
        ImportSource::Path(path) => std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read {path}: {error}"))?,
    };
    if text.trim().is_empty() {
        return Err("the import source is empty".to_string());
    }
    Ok(Command::VoiceDictionary(VoiceDictionaryCommand::Import {
        source: ImportSource::Text(text),
    }))
}

/// An HTTP call: method + path (query folded in) + optional body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpCall {
    pub method: &'static str,
    pub path: String,
    pub body: Option<String>,
}

/// Minimal percent-encoding for a query value (space and the reserved delimiters that would break
/// the query string). Enough for a search term.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Percent-encoding for one path segment: like [`encode`], but `:` and `@` stay literal (they
/// are legal in a segment, and every thread key carries a colon), so ordinary keys read as typed.
fn encode_segment(s: &str) -> String {
    encode(s).replace("%3A", ":").replace("%40", "@")
}

fn lessons_list_path(opts: &LessonsList) -> String {
    let mut q = Vec::new();
    if opts.for_generation {
        q.push("for_generation=true".to_string());
    }
    if let Some(v) = &opts.app_bundle_id {
        q.push(format!("app_bundle_id={}", encode(v)));
    }
    if let Some(v) = &opts.person_id {
        q.push(format!("person_id={}", encode(v)));
    }
    if let Some(v) = &opts.project_id {
        q.push(format!("project_id={}", encode(v)));
    }
    if q.is_empty() {
        "/v1/lessons".to_string()
    } else {
        format!("/v1/lessons?{}", q.join("&"))
    }
}

/// `GET /v1/mail/triage?…` — the list filters as query values.
fn mail_triage_list_path(opts: &MailTriageList) -> String {
    let mut q = Vec::new();
    if let Some(v) = &opts.bucket {
        q.push(format!("bucket={}", encode(v)));
    }
    if let Some(v) = &opts.attention {
        q.push(format!("attention={}", encode(v)));
    }
    if let Some(v) = &opts.label {
        q.push(format!("label={}", encode(v)));
    }
    if opts.handled {
        q.push("include_handled=true".to_string());
    }
    if q.is_empty() {
        "/v1/mail/triage".to_string()
    } else {
        format!("/v1/mail/triage?{}", q.join("&"))
    }
}

/// The `POST /v1/mail/labels` body. A positional that is a label key (built-in or `custom:…`)
/// travels as `key`; anything else names a new custom label (`--name` still wins for the name).
fn mail_label_set_body(opts: &MailLabelSet) -> String {
    let mut body = serde_json::Map::new();
    if is_mail_label_key(&opts.key_or_name) {
        body.insert("key".into(), serde_json::json!(opts.key_or_name));
    } else if opts.name.is_none() {
        body.insert("name".into(), serde_json::json!(opts.key_or_name));
    }
    let fields: [(&str, Option<serde_json::Value>); 6] = [
        ("name", opts.name.as_ref().map(|v| serde_json::json!(v))),
        (
            "definition",
            opts.definition.as_ref().map(|v| serde_json::json!(v)),
        ),
        (
            "default_bucket",
            opts.bucket.as_ref().map(|v| serde_json::json!(v)),
        ),
        ("enabled", opts.enabled.map(|v| serde_json::json!(v))),
        ("auto_archive", opts.auto_archive.map(|v| serde_json::json!(v))),
        (
            "sync_to_gmail",
            opts.sync_to_gmail.map(|v| serde_json::json!(v)),
        ),
    ];
    for (field, value) in fields {
        if let Some(value) = value {
            body.insert(field.to_string(), value);
        }
    }
    if opts.confirm_standing {
        body.insert("confirm_standing".into(), serde_json::json!(true));
    }
    serde_json::Value::Object(body).to_string()
}

fn state_path(noun: &str, which: &ListOrGet) -> String {
    match which {
        ListOrGet::List => format!("/v1/state/{noun}"),
        ListOrGet::Get { id } => format!("/v1/state/{noun}/{id}"),
    }
}

/// The HTTP call for a command, or `None` for a purely local command (`help`). `include_low` adds
/// the read opt-in query.
pub fn to_call(command: &Command, include_low: bool) -> Option<HttpCall> {
    let get = |path: String| HttpCall {
        method: "GET",
        path,
        body: None,
    };
    let post = |path: String, body: String| HttpCall {
        method: "POST",
        path,
        body: Some(body),
    };
    let low = |mut path: String| {
        if include_low {
            path.push_str(if path.contains('?') {
                "&include_low"
            } else {
                "?include_low"
            });
        }
        path
    };

    Some(match command {
        Command::Forms(operation) => {
            use shogun_mcp::forms_api::FormOperation;
            shogun_mcp::forms_api::validate_operation(operation).ok()?;
            let mut body=serde_json::to_value(operation).ok()?;
            body.as_object_mut()?.remove("op");
            match operation {
                FormOperation::Inspect {} => post("/v1/forms/inspect".into(),body.to_string()),
                FormOperation::Plan { .. } => post("/v1/forms/plan".into(),body.to_string()),
                FormOperation::Status {session_id} => get(format!("/v1/forms/status/{session_id}")),
                FormOperation::Apply { .. } => post("/v1/forms/apply".into(),body.to_string()),
                FormOperation::Undo { .. } => post("/v1/forms/undo".into(),body.to_string()),
                FormOperation::Close { .. } => post("/v1/forms/close".into(),body.to_string()),
                FormOperation::ProposeSlackPost { .. } => return None,
            }
        },
        Command::Search {
            query,
            depth,
            from_ms,
            to_ms,
            source_kind,
            limit,
            ai,
        } => {
            let mut path = format!("/v1/memory/search?q={}", encode(query));
            if let Some(depth) = depth {
                let value = match depth {
                    shogun_mcp::backend::SearchDepthV1::Warm => "warm",
                    shogun_mcp::backend::SearchDepthV1::Auto => "auto",
                    shogun_mcp::backend::SearchDepthV1::All => "all",
                };
                path.push_str(&format!("&depth={value}"));
            }
            if let Some(from_ms) = from_ms {
                path.push_str(&format!("&from_ms={from_ms}"));
            }
            if let Some(to_ms) = to_ms {
                path.push_str(&format!("&to_ms={to_ms}"));
            }
            if let Some(source_kind) = source_kind {
                path.push_str(&format!("&source_kind={}", source_kind.as_str()));
            }
            if let Some(limit) = limit {
                path.push_str(&format!("&limit={limit}"));
            }
            if *ai {
                path.push_str("&mode=ai");
            }
            get(low(path))
        }
        Command::Periods { from_ms, to_ms, limit } => {
            let mut parts = Vec::new();
            if let Some(v) = from_ms { parts.push(format!("from_ms={v}")); }
            if let Some(v) = to_ms { parts.push(format!("to_ms={v}")); }
            if let Some(v) = limit { parts.push(format!("page_size={v}")); }
            let mut path = "/v1/memory/periods".to_string();
            if !parts.is_empty() { path.push('?'); path.push_str(&parts.join("&")); }
            get(path)
        }
        Command::Context => get(low("/v1/memory/context".to_string())),
        Command::Events { body } => post("/v1/memory/events".into(), body.clone()),
        Command::Session { body } => post("/v1/memory/session".into(), body.clone()),
        Command::Pack { query } => get(low(format!("/v1/memory/context_pack?q={}", encode(query)))),
        Command::Ingest { body } => post("/v1/memory/ingest".into(), body.clone()),
        Command::IngestWebhook { body } => post("/v1/memory/ingest-webhook".into(), body.clone()),
        Command::Recall { body } => post("/v1/memory/recall".into(), body.clone()),
        Command::Fs { body } => post("/v1/memory/fs".into(), body.clone()),
        Command::HandoffCreate { body } => post("/v1/handoff".into(), body.clone()),
        Command::HandoffAccept { body } => post("/v1/handoff/accept".into(), body.clone()),
        Command::Payments { tool, body } => match (tool, body) {
            // E46: `payments.*`, the tool's arguments as the body.
            (shogun_mcp::memory_api::Tool::PaymentsMandatesList, None) => get("/v1/payments/mandates".into()),
            (shogun_mcp::memory_api::Tool::PaymentsMandatesList, Some(body)) => post("/v1/payments/mandates/list".into(), body.clone()),
            (shogun_mcp::memory_api::Tool::PaymentsMandatesRequest, body) => post("/v1/payments/mandates/request".into(), body.clone().unwrap_or_default()),
            (shogun_mcp::memory_api::Tool::PaymentsMandatesRevoke, body) => post("/v1/payments/mandates/revoke".into(), body.clone().unwrap_or_default()),
            (_, body) => post("/v1/payments/spend".into(), body.clone().unwrap_or_default()),
        },
        Command::Approvals(approvals) => match approvals {
            crate::command::ApprovalsCommand::List => get("/v1/approvals".into()),
            crate::command::ApprovalsCommand::Get { id } => get(format!("/v1/approvals/{id}")),
            crate::command::ApprovalsCommand::Decide { id, body } => post(format!("/v1/approvals/{id}/decide"), body.clone()),
        },
        Command::Audit { since_ms, limit } => {
            let mut query = Vec::new();
            if let Some(since) = since_ms {
                query.push(format!("since_ms={since}"));
            }
            if let Some(limit) = limit {
                query.push(format!("limit={limit}"));
            }
            get(if query.is_empty() { "/v1/audit".into() } else { format!("/v1/audit?{}", query.join("&")) })
        }
        Command::AuditExport { since_ms, until_ms, format, principal, .. } => {
            let mut query = vec![format!("format={format}")];
            if let Some(since) = since_ms {
                query.push(format!("since_ms={since}"));
            }
            if let Some(until) = until_ms {
                query.push(format!("until_ms={until}"));
            }
            if let Some(principal) = principal {
                query.push(format!("principal={principal}"));
            }
            get(format!("/v1/audit/export?{}", query.join("&")))
        }
        // Verified locally, never sent (`main` handles it before any call is built).
        Command::AuditVerify { .. } => return None,
        Command::Acl(acl) => match acl {
            crate::command::AclCommand::List => get("/v1/acl".into()),
            crate::command::AclCommand::Grant { body } => post("/v1/acl/grant".into(), body.clone()),
            crate::command::AclCommand::Revoke { body } => post("/v1/acl/revoke".into(), body.clone()),
            crate::command::AclCommand::Teams => get("/v1/teams".into()),
            crate::command::AclCommand::SetTeam { body } => post("/v1/teams".into(), body.clone()),
        },
        Command::People(w) => get(low(state_path("people", w))),
        Command::Projects(w) => get(low(state_path("projects", w))),
        Command::Commitments(w) => get(low(state_path("commitments", w))),
        Command::OpenLoops(w) => get(low(state_path("open_loops", w))),
        Command::MemoryNotes { id } => get(id.map_or_else(|| "/v1/memory/user-notes".into(), |id| format!("/v1/memory/user-notes/{id}"))),
        Command::MemoryNoteChange { body } => post("/v1/memory/user-notes/change".into(), body.clone()),
        Command::Note { text } => post("/v1/memory/notes".into(), text.clone()),
        // `run` carries the action JSON spec (e.g. '{"kind":"local_search","query":"x"}').
        Command::Run { agent } => post("/v1/actions/execute".into(), agent.clone()),
        Command::ActionsPoll { approval_id } => get(format!("/v1/actions/status/{approval_id}")),
        Command::ActionsQuestions { id: None } => get("/v1/actions/questions".to_string()),
        Command::ActionsQuestions { id: Some(id) } => get(format!("/v1/actions/questions/{id}")),
        Command::ActionsAnswer { id, answer } => post(
            "/v1/actions/answer".to_string(),
            serde_json::json!({ "id": id, "answer": answer }).to_string(),
        ),
        Command::ActionsSteer { kind, id, text } => post(
            "/v1/actions/steer".to_string(),
            serde_json::json!({ "kind": kind, "id": id, "text": text }).to_string(),
        ),
        Command::Wrap => get("/v1/memory/wrap".to_string()),
        Command::Calendar { page_size } => get(match page_size {
            Some(size) => format!("/v1/calendar/upcoming?page_size={size}"),
            None => "/v1/calendar/upcoming".to_string(),
        }),
        Command::Onboarding => get("/v1/device/onboarding".to_string()),
        Command::Account => get("/v1/device/account".to_string()),
        Command::ReleaseNotes => get("/v1/device/release_notes".to_string()),
        Command::UpdateStatus => get("/v1/device/update".to_string()),
        Command::DreamStatus => get("/v1/dream/status".to_string()),
        Command::ProcessingStatus { id } => get(format!("/v1/memory/processing/{id}")),
        Command::DreamRetry => post("/v1/dream/retry".into(), "{}".to_string()),
        Command::Capture { paused: None, .. } => get("/v1/capture/status".into()),
        Command::Capture { paused: Some(paused), minutes } => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.as_millis() as u64).unwrap_or(0);
            post("/v1/capture/paused".into(), serde_json::json!({
                "paused": paused,
                "resume_at_ms": minutes.map(|minutes| now.saturating_add(u64::from(minutes) * 60_000)),
            }).to_string())
        },
        Command::Lessons(cmd) => match cmd {
            LessonsCommand::List(opts) => get(lessons_list_path(opts)),
            LessonsCommand::Enable { id } => post(
                "/v1/lessons/active".into(),
                format!(r#"{{"id":{id},"active":true}}"#),
            ),
            LessonsCommand::Disable { id } => post(
                "/v1/lessons/active".into(),
                format!(r#"{{"id":{id},"active":false}}"#),
            ),
        },
        Command::Disputes(cmd) => match cmd {
            DisputesCommand::List => get(low("/v1/state/disputes".to_string())),
            DisputesCommand::Resolve { id, resolution } => post(
                "/v1/state/disputes/resolve".into(),
                serde_json::json!({ "id": id, "resolution": resolution }).to_string(),
            ),
        },
        Command::State(cmd) => match cmd {
            StateCommand::Edit { body } => post("/v1/state/update".into(), body.clone()),
            StateCommand::Delete { body } => post("/v1/state/delete".into(), body.clone()),
            StateCommand::Create { body } => post("/v1/state/create".into(), body.clone()),
            StateCommand::Export => get(low("/v1/state/export".to_string())),
            StateCommand::ExportV1 { body } => post("/v1/export".into(), body.clone()),
            StateCommand::Split { body } => post("/v1/state/split".into(), body.clone()),
            // The table is a validated literal (`people` | `projects`), the id an integer.
            StateCommand::Evidence { table, id } => get(format!("/v1/state/{table}/{id}/evidence")),
            StateCommand::Merge { body } => post("/v1/state/merge".into(), body.clone()),
            StateCommand::KeepSeparate { body } => post("/v1/state/keep-separate".into(), body.clone()),
            StateCommand::Duplicates { table } => get(format!("/v1/state/{table}/merge-candidates")),
        },
        Command::Import(cmd) => match cmd {
            ImportCommand::Preview { body } => post("/v1/memory/import/preview".into(), body.clone()),
            ImportCommand::Commit { body } => post("/v1/memory/import".into(), body.clone()),
            ImportCommand::List => get("/v1/memory/imports".to_string()),
            ImportCommand::Show { id } => get(low(format!("/v1/memory/imports/{id}"))),
            ImportCommand::Delete { id } => post(format!("/v1/memory/imports/{id}/delete"), String::new()),
        },
        Command::Skills(cmd) => match cmd {
            SkillsCommand::List => get("/v1/skills".to_string()),
            // Skill names are validated slugs (lowercase, digits, '-'); anything else simply
            // matches no skill, so no URL encoding is needed.
            SkillsCommand::Show { name } => get(format!(
                "/v1/skills?q={}",
                name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>()
            )),
            SkillsCommand::Run { name, input } => post(
                "/v1/skills/run".into(),
                serde_json::json!({ "skill": name, "input": input }).to_string(),
            ),
            SkillsCommand::Status { id } => get(format!("/v1/skills/run/{id}")),
            SkillsCommand::Save { markdown } => post(
                "/v1/skills".into(),
                serde_json::json!({ "markdown": markdown }).to_string(),
            ),
        },
        Command::Web(cmd) => match cmd {
            WebCommand::Transcript { url } => get(format!("/v1/web/video_transcript?url={}", encode(url))),
            WebCommand::Post { url } => get(format!("/v1/web/social_post?url={}", encode(url))),
            WebCommand::Page { url } => get(format!("/v1/web/page?url={}", encode(url))),
        },
        Command::SupportReports => get("/v1/support/reports".to_string()),
        Command::Export(cmd) => match cmd {
            ExportCommand::Meeting { session_id, folder, name, csv, transcript } => {
                let mut body = serde_json::json!({
                    "folder": folder,
                    "format": if *csv { "csv" } else { "md" },
                    "include_transcript": transcript,
                });
                if let Some(id) = session_id {
                    body["session_id"] = serde_json::json!(id);
                }
                if let Some(name) = name {
                    body["name"] = serde_json::json!(name);
                }
                post("/v1/export/file".into(), body.to_string())
            }
            ExportCommand::Status { id } => get(format!("/v1/export/writes/{id}")),
            ExportCommand::History => get("/v1/export/writes".to_string()),
            ExportCommand::Folders => get("/v1/export/folders".to_string()),
            ExportCommand::FolderRemove { folder } => post(
                "/v1/export/folders/remove".into(),
                serde_json::json!({ "folder": folder }).to_string(),
            ),
        },
        Command::Schedule(cmd) => match cmd {
            ScheduleCommand::List => get("/v1/schedules".to_string()),
            ScheduleCommand::Add { skill, days, at, input } => post(
                "/v1/schedules".into(),
                serde_json::json!({ "skill": skill, "days": days, "at": at, "input": input }).to_string(),
            ),
            ScheduleCommand::Remove { id } => post(
                "/v1/schedules/remove".into(),
                serde_json::json!({ "id": id }).to_string(),
            ),
        },
        Command::Patterns(cmd) => match cmd {
            PatternsCommand::List => get("/v1/patterns".to_string()),
            PatternsCommand::Enable { id } => post(
                "/v1/patterns/active".into(),
                format!(r#"{{"id":{id},"active":true}}"#),
            ),
            PatternsCommand::Disable { id } => post(
                "/v1/patterns/active".into(),
                format!(r#"{{"id":{id},"active":false}}"#),
            ),
        },
        Command::ApiStatus => get("/v1/status".to_string()),
        Command::ApiTokens => get("/v1/api/tokens".to_string()),
        Command::ApiRestrict { client } => post(
            "/v1/api/connections/restrict".into(),
            serde_json::json!({ "client": client }).to_string(),
        ),
        Command::Metrics => get("/v1/metrics".to_string()),
        Command::Whoami => get("/v1/profile/whoami".to_string()),
        Command::ProfileSet { body } => post("/v1/profile".into(), body.clone()),
        Command::WritingVoice { action, .. } if action == "show" => get("/v1/writing_voice".into()),
        Command::WritingVoice { action, id } => post("/v1/writing_voice".into(), serde_json::json!({ "action": action, "consent": action == "enable", "id": id }).to_string()),
        Command::AgentSource(cmd) => match cmd {
            AgentSourceCommand::Show => get("/v1/agent/source".into()),
            AgentSourceCommand::Use { source, model } => {
                let mut body = serde_json::json!({ "source": source });
                if let Some(model) = model {
                    body["model"] = serde_json::json!(model);
                }
                // The person typing `use <cli>` is the attestation, as with `writing-style enable`;
                // the in-app approval is still what grants it. An API-key provider carries none.
                if shogun_core::llm::subscription::Delegate::parse(source).is_some() {
                    body["subscription_consent"] = serde_json::json!(true);
                }
                post("/v1/agent/source".into(), body.to_string())
            }
            AgentSourceCommand::Revoke => post(
                "/v1/agent/source".into(),
                serde_json::json!({ "subscription_consent": false }).to_string(),
            ),
            // #390: the same body MCP sends; the server refuses `true` (Settings only).
            AgentSourceCommand::OpenAiStore(on) => post(
                "/v1/agent/source".into(),
                serde_json::json!({ "openai_store": on }).to_string(),
            ),
        },
        // Issue #82: the same bodies MCP sends (invariant 6).
        Command::Connections(cmd) => match cmd {
            ConnectionsCommand::List => get("/v1/connections".into()),
            ConnectionsCommand::Disconnect { service } => post(
                "/v1/connections/disconnect".into(),
                serde_json::json!({ "service": service }).to_string(),
            ),
            ConnectionsCommand::GmailDraftOnly(on) => post(
                "/v1/connections/gmail".into(),
                serde_json::json!({ "draft_stop": on }).to_string(),
            ),
            ConnectionsCommand::ComposioRevoke => post(
                "/v1/connections/composio".into(),
                serde_json::json!({ "consent": false }).to_string(),
            ),
            ConnectionsCommand::ComposioDraftOnly(on) => post(
                "/v1/connections/composio".into(),
                serde_json::json!({ "draft_stop": on }).to_string(),
            ),
        },
        Command::VoiceDictionary(command) => match command {
            VoiceDictionaryCommand::AutoAdd => get("/v1/voice_dictionary/autoadd".to_string()),
            VoiceDictionaryCommand::ManageAutoAdd { action } => post("/v1/voice_dictionary/autoadd".to_string(), action.clone()),
            VoiceDictionaryCommand::List => get("/v1/voice_dictionary/terms".to_string()),
            VoiceDictionaryCommand::Create { term } => {
                post("/v1/voice_dictionary/terms".to_string(), term.clone())
            }
            VoiceDictionaryCommand::Update { id, term } => {
                post(format!("/v1/voice_dictionary/terms/{id}"), term.clone())
            }
            VoiceDictionaryCommand::Delete { id } => post(
                format!("/v1/voice_dictionary/terms/{id}/delete"),
                String::new(),
            ),
            // The binary resolves a path / stdin into `Text` first (`resolve_import_source`);
            // an unresolved source is a programming error, not a request to send.
            VoiceDictionaryCommand::Import { source: ImportSource::Text(text) } => post(
                "/v1/voice_dictionary/import".to_string(),
                serde_json::json!({ "text": text }).to_string(),
            ),
            VoiceDictionaryCommand::Import { .. } => return None,
            VoiceDictionaryCommand::Export => get("/v1/voice_dictionary/export".to_string()),
        },
        Command::Language { set: None } => get("/v1/settings/language".to_string()),
        Command::Language { set: Some(code) } => post(
            "/v1/settings/language".to_string(),
            serde_json::json!({ "language": code }).to_string(),
        ),
        Command::Translate(command) => match command {
            TranslateCommand::Text { text, target, formality, app } => post(
                "/v1/translate".to_string(),
                serde_json::json!({ "text": text, "target": target, "formality": formality, "app": app })
                    .to_string(),
            ),
            TranslateCommand::Reply { incoming, reply_in, adjustment, formality, app } => post(
                "/v1/translate/reply_draft".to_string(),
                serde_json::json!({
                    "incoming": incoming, "reply_in": reply_in, "adjustment": adjustment,
                    "formality": formality, "app": app,
                })
                .to_string(),
            ),
            TranslateCommand::Status { id } => get(format!("/v1/translate/jobs/{id}")),
            TranslateCommand::PairShow => get("/v1/translate/pair".to_string()),
            TranslateCommand::PairSet { mine, theirs } => post(
                "/v1/translate/pair".to_string(),
                serde_json::json!({ "mine": mine, "theirs": theirs }).to_string(),
            ),
            TranslateCommand::PairReset => post(
                "/v1/translate/pair".to_string(),
                serde_json::json!({ "reset": true }).to_string(),
            ),
        },
        Command::TranslateRegister(command) => match command {
            TranslateRegisterCommand::Show => get("/v1/translate/register".to_string()),
            TranslateRegisterCommand::Set { formality, app } => post(
                "/v1/translate/register".to_string(),
                serde_json::json!({ "formality": formality, "app": app }).to_string(),
            ),
        },
        Command::MeetingReminder { minutes: None } => get("/v1/meeting/reminder".into()),
        Command::MeetingReminder { minutes: Some(minutes) } => post("/v1/meeting/reminder".into(), serde_json::json!({"minutes": minutes}).to_string()),
        Command::MeetingMicrophone(command) => match command {
            MicrophoneCommand::Show => get("/v1/meeting/microphone".to_string()),
            MicrophoneCommand::Set { microphone } => post(
                "/v1/meeting/microphone".to_string(),
                serde_json::json!({ "microphone": microphone }).to_string(),
            ),
        },
        Command::MeetingRecordNow { in_person } => post(
            "/v1/meeting/start".to_string(),
            serde_json::json!({ "in_person": in_person }).to_string(),
        ),
        Command::MeetingSystemAudioStatus => get("/v1/meeting/system_audio".into()),
        Command::MeetingSystemAudioRetry => post("/v1/meeting/system_audio/retry".into(), serde_json::json!({}).to_string()),
        Command::MeetingStartStatus { id } => get(format!("/v1/meeting/start/{id}")),
        Command::MeetingList { before_id } => get(before_id.map_or_else(|| "/v1/meeting/list".to_string(), |id| format!("/v1/meeting/list/{id}"))),
        Command::MeetingRecap { session_id } => get(session_id.map_or_else(|| "/v1/meeting/recap".to_string(), |id| format!("/v1/meeting/recap/{id}"))),
        Command::MeetingTranscript { session_id } => get(session_id.map_or_else(|| "/v1/meeting/transcript".to_string(), |id| format!("/v1/meeting/transcript/{id}"))),
        Command::MeetingSearch { query } => get(format!("/v1/meeting/search?q={}", encode(query))),
        Command::MeetingDelete { session_id } => post(format!("/v1/meeting/{session_id}/delete"), String::new()),
        Command::MeetingRecapEdit { session_id, body } => {
            // The body is the user's JSON with the session folded in — the same object the MCP
            // face sends. Non-object JSON is sent as-is and refused by the server with a reason.
            let mut object = match body {
                RecapEditBody::Reset => serde_json::Map::from_iter([("reset".to_string(), serde_json::json!(true))]),
                RecapEditBody::Json(text) => match serde_json::from_str::<serde_json::Value>(text) {
                    Ok(serde_json::Value::Object(map)) => map,
                    _ => return Some(post("/v1/meeting/recap/edit".to_string(), text.clone())),
                },
            };
            if let Some(id) = session_id {
                object.insert("session_id".to_string(), serde_json::json!(id));
            }
            post("/v1/meeting/recap/edit".to_string(), serde_json::Value::Object(object).to_string())
        }
        Command::MeetingRecapPrefs(command) => match command {
            RecapPrefsCommand::Show => get("/v1/meeting/recap_prefs".to_string()),
            RecapPrefsCommand::Set { language, instruction } => {
                let mut body = serde_json::Map::new();
                if let Some(language) = language {
                    body.insert("recap_language".into(), serde_json::Value::String(language.clone()));
                }
                if let Some(instruction) = instruction {
                    body.insert(
                        "minutes_instruction".into(),
                        instruction.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null),
                    );
                }
                post("/v1/meeting/recap_prefs".to_string(), serde_json::Value::Object(body).to_string())
            }
        },
        Command::WikiEvidence { key } => get(format!("/v1/wiki/evidence?q={}", encode(key))),
        Command::WikiNeighbors { key } => get(low(format!("/v1/wiki/neighbors?q={}", encode(key)))),
        Command::WikiList => get(low("/v1/wiki/list".into())),
        Command::MeetingSpeakers(command) => match command {
            SpeakersCommand::Show => get("/v1/meeting/speakers".to_string()),
            SpeakersCommand::Name { key, name } => post(
                "/v1/meeting/speakers".to_string(),
                serde_json::json!({ "speaker_key": key, "display_name": name }).to_string(),
            ),
            // An empty display_name clears the assignment (the server's contract).
            SpeakersCommand::Unname { key } => post(
                "/v1/meeting/speakers".to_string(),
                serde_json::json!({ "speaker_key": key, "display_name": "" }).to_string(),
            ),
        },
        Command::MeetingTemplates(command) => match command {
            TemplatesCommand::Show => get("/v1/meeting/templates".to_string()),
            TemplatesCommand::Set { id } => post(
                "/v1/meeting/templates".to_string(),
                serde_json::json!({ "template_id": id }).to_string(),
            ),
        },
        Command::MeetingExport { transcript, session_id } => {
            let mut path = session_id.map_or_else(|| "/v1/meeting/recap/export".to_string(), |id| format!("/v1/meeting/recap/export/{id}"));
            if *transcript { path.push_str("?q=transcript"); }
            get(path)
        },
        Command::MeetingTrack { session_id, text, direction, counterparty_id } => {
            let mut body = serde_json::json!({ "text": text });
            if let Some(id) = session_id { body["session_id"] = serde_json::json!(id); }
            if let Some(d) = direction { body["direction"] = serde_json::json!(d); }
            if let Some(id) = counterparty_id { body["counterparty_id"] = serde_json::json!(id); }
            post("/v1/meeting/recap/track".to_string(), body.to_string())
        }
        Command::VoiceMicrophone(command) => match command {
            MicrophoneCommand::Show => get("/v1/voice/microphone".to_string()),
            MicrophoneCommand::Set { microphone } => post(
                "/v1/voice/microphone".to_string(),
                serde_json::json!({ "microphone": microphone }).to_string(),
            ),
        },
        Command::Backup(cmd) => match cmd {
            BackupCommand::Status => get("/v1/backup/status".to_string()),
            BackupCommand::Retention { days } => post(
                "/v1/backup/retention".into(),
                format!(r#"{{"days":{days}}}"#),
            ),
        },
        Command::SearchIndex(cmd) => match cmd {
            SearchIndexCommand::Status => get("/v1/search_index/status".to_string()),
            SearchIndexCommand::Compact => post("/v1/search_index/compact/start".into(), "{}".to_string()),
            SearchIndexCommand::Stop => post("/v1/search_index/compact/stop".into(), "{}".to_string()),
        },
        Command::StoragePreview => get("/v1/storage/preview".to_string()),
        Command::VisualRecall(cmd) => match cmd {
            VisualRecallCommand::Status => get("/v1/visual_recall/status".to_string()),
            VisualRecallCommand::DeleteHiddenSelfFrames => post(
                "/v1/visual_recall/frames/delete".to_string(),
                r#"{"hidden_self_frames":true}"#.to_string(),
            ),
            VisualRecallCommand::Enable => post(
                "/v1/visual_recall/enabled".into(),
                r#"{"enabled":true}"#.to_string(),
            ),
            VisualRecallCommand::Disable => post(
                "/v1/visual_recall/enabled".into(),
                r#"{"enabled":false}"#.to_string(),
            ),
            VisualRecallCommand::Retention { days } => post(
                "/v1/visual_recall/retention".into(),
                format!(r#"{{"days":{days}}}"#),
            ),
            VisualRecallCommand::Search {
                query,
                from_ms,
                to_ms,
                app_bundle_id,
            } => {
                let mut path = format!("/v1/visual_recall/frames/search?q={}", encode(query));
                if let Some(f) = from_ms {
                    path.push_str(&format!("&from_ms={f}"));
                }
                if let Some(t) = to_ms {
                    path.push_str(&format!("&to_ms={t}"));
                }
                if let Some(app) = app_bundle_id {
                    path.push_str(&format!("&app_bundle_id={}", encode(app)));
                }
                get(low(path))
            }
            VisualRecallCommand::FrameGet { id } => get(format!("/v1/visual_recall/frames/{id}")),
            VisualRecallCommand::FrameTextLines { id } => {
                get(format!("/v1/visual_recall/frames/{id}/text_lines"))
            }
            VisualRecallCommand::FrameRescan { id } => post(
                format!("/v1/visual_recall/frames/{id}/rescan"),
                String::new(),
            ),
            VisualRecallCommand::FrameDelete { id } => post(
                "/v1/visual_recall/frames/delete".into(),
                format!(r#"{{"id":{id}}}"#),
            ),
        },
        Command::CaptureExclusions(cmd) => match cmd {
            CaptureExclusionsCommand::List => get("/v1/capture_exclusions".to_string()),
            CaptureExclusionsCommand::Set { body } => post("/v1/capture_exclusions".into(), body.clone()),
        },
        // Mail triage (FR-TG-30). Thread and label keys are path segments, percent-encoded so a
        // label named in Japanese or with a `/` survives (issue #779); the server decodes them
        // and folds the key into the write body.
        Command::Mail(mail) => match mail {
            MailCommand::TriageList(opts) => get(low(mail_triage_list_path(opts))),
            MailCommand::TriageShow { thread_key } => get(format!("/v1/mail/triage/{}", encode_segment(thread_key))),
            MailCommand::TriageSet {
                thread_key,
                label,
                bucket,
                handled,
            } => {
                let mut body = serde_json::Map::new();
                if let Some(label) = label {
                    body.insert("label_key".into(), serde_json::json!(label));
                }
                if let Some(bucket) = bucket {
                    body.insert("bucket".into(), serde_json::json!(bucket));
                }
                if let Some(handled) = handled {
                    body.insert("handled".into(), serde_json::json!(handled));
                }
                post(
                    format!("/v1/mail/triage/{}", encode_segment(thread_key)),
                    serde_json::Value::Object(body).to_string(),
                )
            }
            MailCommand::TriageArchive { thread_key } => {
                post(format!("/v1/mail/triage/{}/archive", encode_segment(thread_key)), String::new())
            }
            MailCommand::TriageUnarchive { thread_key } => post(
                format!("/v1/mail/triage/{}/unarchive", encode_segment(thread_key)),
                String::new(),
            ),
            MailCommand::LabelsList => get("/v1/mail/labels".to_string()),
            MailCommand::LabelsSet(opts) => {
                post("/v1/mail/labels".to_string(), mail_label_set_body(opts))
            }
            MailCommand::LabelsRemove { key } => {
                post(format!("/v1/mail/labels/{}/delete", encode_segment(key)), String::new())
            }
            MailCommand::RulesList => get("/v1/mail/sender_rules".to_string()),
            MailCommand::RulesAdd {
                match_kind,
                match_value,
                label,
                bucket,
            } => {
                let mut body = serde_json::json!({
                    "match_kind": match_kind,
                    "match_value": match_value,
                    "label_key": label,
                });
                if let Some(bucket) = bucket {
                    body["bucket"] = serde_json::json!(bucket);
                }
                post("/v1/mail/sender_rules".to_string(), body.to_string())
            }
            MailCommand::RulesRemove { id } => {
                post(format!("/v1/mail/sender_rules/{id}/delete"), String::new())
            }
            MailCommand::History => get("/v1/mail/history".to_string()),
            MailCommand::Status => get("/v1/mail/settings".to_string()),
            MailCommand::SetEnabled { enabled } => post(
                "/v1/mail/settings".to_string(),
                serde_json::json!({ "enabled": enabled }).to_string(),
            ),
        },
        Command::Help | Command::Config { .. } | Command::Principal(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_reads_are_gets_with_the_link_encoded() -> Result<(), String> {
        let call = to_call(
            &Command::Web(WebCommand::Transcript {
                url: "https://www.youtube.com/watch?v=abc&t=10s".into(),
            }),
            false,
        )
        .ok_or("no call")?;
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/v1/web/video_transcript?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dabc%26t%3D10s");
        assert!(call.body.is_none());
        let post = to_call(&Command::Web(WebCommand::Post { url: "https://x.com/a/status/1".into() }), false).ok_or("no call")?;
        assert!(post.path.starts_with("/v1/web/social_post?url="));
        let page = to_call(&Command::Web(WebCommand::Page { url: "https://example.com/".into() }), false).ok_or("no call")?;
        assert!(page.path.starts_with("/v1/web/page?url="));
        Ok(())
    }

    #[test]
    fn update_status_is_a_read_with_no_action_subcommands() -> Result<(), String> {
        for args in [&["update"][..], &["update", "status"]] {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing call")?;
            assert_eq!((call.method, call.path.as_str()), ("GET", "/v1/device/update"));
            assert!(call.body.is_none());
        }
        for action in ["install", "restart", "on", "off"] {
            assert!(crate::parse::parse(&["update".to_string(), action.to_string()]).is_err());
        }
        Ok(())
    }

    #[test]
    fn language_parser_and_wire() -> Result<(), String> {
        let show = crate::parse::parse(&["language".to_string()]).map_err(|e| e.message())?;
        let call = to_call(&show.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("GET", "/v1/settings/language"));
        let set = crate::parse::parse(&["language".to_string(), "set".to_string(), "ja".to_string()])
            .map_err(|e| e.message())?;
        let call = to_call(&set.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/settings/language"));
        let body: serde_json::Value = serde_json::from_str(call.body.as_deref().ok_or("missing body")?).map_err(|e| e.to_string())?;
        assert_eq!(body["language"], "ja");
        assert!(crate::parse::parse(&["language".to_string(), "set".to_string()]).is_err());
        Ok(())
    }

    #[test]
    fn translate_register_parser_and_wire_carry_the_app() -> Result<(), String> {
        let show = crate::parse::parse(&["translate".to_string(), "register".to_string()]).map_err(|e| e.message())?;
        let call = to_call(&show.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("GET", "/v1/translate/register"));

        let args: Vec<String> = ["translate", "register", "set", "casual", "--app", "com.tinyspeck.slackmacgap"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let set = crate::parse::parse(&args).map_err(|e| e.message())?;
        let call = to_call(&set.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/translate/register"));
        let body: serde_json::Value = serde_json::from_str(call.body.as_deref().ok_or("missing body")?).map_err(|e| e.to_string())?;
        assert_eq!(body["formality"], "casual");
        assert_eq!(body["app"], "com.tinyspeck.slackmacgap");

        assert!(crate::parse::parse(&["translate".to_string(), "register".to_string(), "set".to_string()]).is_err());
        Ok(())
    }

    #[test]
    fn translate_text_reply_status_and_pair_map_to_the_rest_routes() -> Result<(), String> {
        use shogun_mcp::memory_api::{tool_level, ApiLevel, Tool};
        let parse = |words: &[&str]| {
            let args: Vec<String> = words.iter().map(|s| s.to_string()).collect();
            crate::parse::parse(&args).map_err(|e| e.message())
        };
        let body = |call: &HttpCall| -> Result<serde_json::Value, String> {
            serde_json::from_str(call.body.as_deref().ok_or("missing body")?).map_err(|e| e.to_string())
        };

        // Flags may sit anywhere; every other word is the text.
        let text = parse(&["translate", "text", "see", "you", "--to", "ja", "tomorrow", "--register", "polite"])?;
        assert_eq!(text.command.tool(), Some(Tool::TranslateText));
        assert_eq!(tool_level(Tool::TranslateText), ApiLevel::Write(shogun_agents::permission::Level::L1));
        let call = to_call(&text.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/translate"));
        let value = body(&call)?;
        assert_eq!(value["text"], "see you tomorrow");
        assert_eq!((value["target"].as_str(), value["formality"].as_str()), (Some("ja"), Some("polite")));

        let reply = parse(&["translate", "reply", "Can", "we", "meet?", "--tone", "shorter", "--in", "en", "--app", "com.apple.mail"])?;
        assert_eq!(reply.command.tool(), Some(Tool::TranslateReplyDraft));
        let call = to_call(&reply.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/translate/reply_draft"));
        let value = body(&call)?;
        assert_eq!(value["incoming"], "Can we meet?");
        assert_eq!(value["adjustment"], "shorter");
        assert_eq!(value["reply_in"], "en");
        assert_eq!(value["app"], "com.apple.mail");

        let status = parse(&["translate", "status", "4"])?;
        let call = to_call(&status.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("GET", "/v1/translate/jobs/4"));
        assert!(parse(&["translate", "status", "zero"]).is_err());
        assert!(parse(&["translate", "status", "0"]).is_err());
        assert!(parse(&["translate", "text"]).is_err());
        assert!(parse(&["translate", "text", "--to"]).is_err());
        assert!(parse(&["translate", "reply", "--tone", "warm"]).is_err());

        let show = parse(&["translate", "pair"])?;
        let call = to_call(&show.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("GET", "/v1/translate/pair"));
        let set = parse(&["translate", "pair", "set", "en"])?;
        assert_eq!(set.command.tool(), Some(Tool::TranslatePairSet));
        let call = to_call(&set.command, false).ok_or("missing call")?;
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/translate/pair"));
        assert_eq!(body(&call)?["mine"], "en");
        let reset = parse(&["translate", "pair", "reset"])?;
        let call = to_call(&reset.command, false).ok_or("missing call")?;
        assert_eq!(body(&call)?["reset"], true);
        assert!(crate::command::USAGE.contains("translate reply <message>"));
        assert!(crate::command::USAGE.contains("translate pair set en|ja [en|ja]"));
        Ok(())
    }

    #[test]
    fn writing_style_parser_and_wire_preserve_explicit_consent() -> Result<(), String> {
        for action in ["show", "enable", "disable", "clear"] {
            let args = vec!["writing-style".to_string(), action.to_string()];
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing writing style call")?;
            assert_eq!(call.path, "/v1/writing_voice");
            if action == "show" {
                assert_eq!(call.method, "GET");
            } else {
                assert_eq!(call.method, "POST");
                let value: serde_json::Value = serde_json::from_str(call.body.as_deref().ok_or("missing body")?).map_err(|e| e.to_string())?;
                assert_eq!(value["action"], action);
                assert_eq!(value["consent"], action == "enable");
            }
        }
        Ok(())
    }

    /// Issue #779: without the label's standing approval the request becomes a pending approval
    /// (nothing recorded); under it, the request is recorded directly.
    #[test]
    fn mail_archive_resolves_to_a_pending_approval_like_rest() -> Result<(), String> {
        use std::sync::Mutex;

        use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue};
        use shogun_agents::permission::{Level, SendAction};
        use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::mail_archive_api::{needs_approval, request, MailArchiveRequest};
        use shogun_mcp::memory_api::{TokenRegistry, Tool};
        use shogun_mcp::rest::{self, ApprovalSurface, Method, RestRequest, Routed};

        /// Thread keys with a standing approval, and the requests recorded so far.
        struct Mail {
            standing: Vec<&'static str>,
            recorded: Mutex<Vec<String>>,
        }
        impl Mail {
            fn key(body: &str) -> Result<String, String> {
                let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
                value["thread_key"].as_str().map(str::to_string).ok_or_else(|| "expected thread_key".to_string())
            }
        }
        impl MemoryBackend for Mail {
            fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn mail_archive_change(&self, tool: Tool, body: &str) -> Result<Option<MailArchiveRequest>, String> {
                let thread_key = Self::key(body)?;
                let unarchive = tool == Tool::MailTriageUnarchive;
                let direct = !unarchive && self.standing.contains(&thread_key.as_str());
                Ok((!direct).then_some(MailArchiveRequest { thread_key, unarchive }))
            }
            fn write(&self, tool: Tool, body: &str) -> WriteResult {
                if self.mail_archive_change(tool, body)?.is_some() {
                    return Err("invalid archive request: needs approval".into());
                }
                self.recorded.lock().map_err(|_| "lock".to_string())?.push(Self::key(body)?);
                Ok(None)
            }
        }

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let backend = Mail { standing: vec!["gmail:s"], recorded: Mutex::new(Vec::new()) };
        let request_for = |args: &[&str]| -> Result<RestRequest, String> {
            let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing mail call")?;
            Ok(RestRequest {
                method: Method::Post,
                path: call.path.clone(),
                token: Some("t".into()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: call.body.clone(),
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
            })
        };
        // The thread key rides in the path; REST folds it into the body the backend sees.
        let body_of = |key: &str| format!(r#"{{"thread_key":"{key}"}}"#);

        let mut queue = ApprovalQueue::new();
        for (args, tool, key) in [
            (&["mail", "triage", "archive", "gmail:a"][..], Tool::MailTriageArchive, "gmail:a"),
            (&["mail", "triage", "unarchive", "gmail:s"][..], Tool::MailTriageUnarchive, "gmail:s"),
        ] {
            let req = request_for(args)?;
            assert_eq!(rest::route(&req, &tokens, &ent), Routed::Write { tool, level: Level::L2 });
            let wanted = needs_approval(&backend, tool, Some(&body_of(key))).ok_or("needs approval")?;
            let (status, reply) = request(wanted, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
            assert_eq!(status, 202, "{reply}");
            assert!(reply.contains(r#""pending":true"#), "{reply}");
            assert!(reply.contains(r#""level":"L2""#), "{reply}");
            assert!(reply.contains(r#""origin":"api""#), "{reply}");
            // The queue-less path refuses rather than recording it.
            let (status, reply) = rest::respond_with(&req, &tokens, &ent, &backend);
            assert_eq!(status, 501, "{reply}");
        }
        let ids = queue.pending_ids();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            queue.action(ids[1]),
            Some(&SendAction::ArchiveMailThread { thread_key: "gmail:s".into(), unarchive: true })
        );
        assert!(backend.recorded.lock().map_err(|_| "lock")?.is_empty(), "nothing recorded");

        // Under the standing approval: recorded directly, nothing queued.
        let req = request_for(&["mail", "triage", "archive", "gmail:s"])?;
        assert!(needs_approval(&backend, Tool::MailTriageArchive, Some(&body_of("gmail:s"))).is_none());
        let (status, reply) = rest::respond_with(&req, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        assert!(reply.contains(r#""pending":true"#), "{reply}");
        assert!(!reply.contains("approval_id"), "{reply}");
        assert_eq!(*backend.recorded.lock().map_err(|_| "lock")?, vec!["gmail:s".to_string()]);
        assert_eq!(queue.pending_len(), 2);
        Ok(())
    }

    /// Issue #782: turning it on while it is off, or a longer retention, becomes a pending L3
    /// approval (nothing written); turning it off or a shorter retention is written directly.
    #[test]
    fn visual_recall_widening_resolves_to_a_pending_approval_like_rest() -> Result<(), String> {
        use std::sync::Mutex;

        use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue};
        use shogun_agents::permission::{Level, SendAction, VisualRecallWidening};
        use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::memory_api::{TokenRegistry, Tool};
        use shogun_mcp::rest::{self, ApprovalSurface, Method, RestRequest, Routed};
        use shogun_mcp::visual_recall_settings_api::{loosening, request_change};

        /// `(enabled, days)`. Refuses widening like the real backend.
        struct Settings(Mutex<(bool, u32)>);
        impl Settings {
            fn requested(tool: Tool, body: &str) -> Result<(Option<bool>, Option<u32>), String> {
                let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
                Ok(match tool {
                    Tool::VisualRecallSetEnabled => (value["enabled"].as_bool(), None),
                    _ => (None, value["days"].as_u64().and_then(|d| u32::try_from(d).ok())),
                })
            }
        }
        impl MemoryBackend for Settings {
            fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn visual_recall_change(&self, tool: Tool, body: &str) -> Result<Option<VisualRecallWidening>, String> {
                let (enable, days) = Self::requested(tool, body)?;
                let (enabled, current) = *self.0.lock().map_err(|_| "lock".to_string())?;
                let change = VisualRecallWidening {
                    enable: enable == Some(true) && !enabled,
                    retention_days: days.filter(|d| *d > current),
                    previous_retention_days: current,
                };
                Ok(change.widens().then_some(change))
            }
            fn write(&self, tool: Tool, body: &str) -> WriteResult {
                if self.visual_recall_change(tool, body)?.is_some() {
                    return Err("invalid visual recall request".into());
                }
                let mut state = self.0.lock().map_err(|_| "lock".to_string())?;
                match Self::requested(tool, body)? {
                    (Some(enabled), _) => state.0 = enabled,
                    (_, Some(days)) => state.1 = days,
                    _ => {}
                }
                Ok(None)
            }
        }

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let backend = Settings(Mutex::new((false, 3)));
        let request = |args: &[&str]| -> Result<RestRequest, String> {
            let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing visual recall call")?;
            Ok(RestRequest {
                method: Method::Post,
                path: call.path.clone(),
                token: Some("t".into()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: call.body.clone(),
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
            })
        };

        let mut queue = ApprovalQueue::new();
        for (args, tool) in [
            (&["visual-recall", "enable"][..], Tool::VisualRecallSetEnabled),
            (&["visual-recall", "retention", "3650"][..], Tool::VisualRecallSetRetention),
        ] {
            let req = request(args)?;
            assert_eq!(rest::route(&req, &tokens, &ent), Routed::Write { tool, level: Level::L3 });
            let change = loosening(&backend, tool, req.body.as_deref()).ok_or("widening needs approval")?;
            let (status, reply) = request_change(change, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
            assert_eq!(status, 202, "{reply}");
            assert!(reply.contains(r#""pending":true"#), "{reply}");
            assert!(reply.contains(r#""origin":"api""#), "{reply}");
        }
        let ids = queue.pending_ids();
        assert_eq!(ids.len(), 2);
        assert!(matches!(
            queue.action(ids[1]),
            Some(SendAction::WidenVisualRecall { change }) if change.retention_days == Some(3650)
        ));
        assert_eq!(*backend.0.lock().map_err(|_| "lock")?, (false, 3), "nothing written");

        // Narrowing: direct, nothing queued.
        for args in [&["visual-recall", "retention", "1"][..], &["visual-recall", "disable"][..]] {
            let req = request(args)?;
            assert!(loosening(&backend, Tool::VisualRecallSetRetention, req.body.as_deref()).is_none());
            let (status, reply) = rest::respond_with(&req, &tokens, &ent, &backend);
            assert_eq!(status, 202, "{reply}");
            assert!(!reply.contains("pending"), "{reply}");
        }
        assert_eq!(*backend.0.lock().map_err(|_| "lock")?, (false, 1));
        assert_eq!(queue.pending_len(), 2);
        Ok(())
    }

    /// Issue #657: a body that drops an exclusion becomes a pending L3 approval (nothing
    /// written); one that only adds is written directly and answers with the saved lists.
    #[test]
    fn capture_exclusions_removal_resolves_to_a_pending_approval_like_rest() -> Result<(), String> {
        use std::sync::Mutex;

        use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue};
        use shogun_agents::permission::{CaptureExclusionChange, Level, SendAction};
        use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::capture_exclusions_api::{loosening, request_change};
        use shogun_mcp::memory_api::{TokenRegistry, Tool};
        use shogun_mcp::rest::{self, ApprovalSurface, Method, RestRequest, Routed};

        /// Current apps = one exclusion; websites unused. Refuses removals like the real backend.
        struct Lists(Mutex<Vec<String>>);
        impl MemoryBackend for Lists {
            fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn read_structured(&self, _tool: Tool, _params: &ReadParams) -> Option<String> {
                let apps = self.0.lock().map_err(|_| ()).ok()?.clone();
                Some(serde_json::json!({ "apps": apps, "websites": [] }).to_string())
            }
            fn capture_exclusions_change(&self, body: &str) -> Result<Option<CaptureExclusionChange>, String> {
                let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
                let apps: Vec<String> = serde_json::from_value(value["apps"].clone()).map_err(|e| e.to_string())?;
                let current = self.0.lock().map_err(|_| "lock".to_string())?.clone();
                let resumed_apps: Vec<String> = current.into_iter().filter(|a| !apps.contains(a)).collect();
                let change = CaptureExclusionChange { apps, resumed_apps, ..Default::default() };
                Ok(change.resumes_capture().then_some(change))
            }
            fn write(&self, _tool: Tool, body: &str) -> WriteResult {
                if self.capture_exclusions_change(body)?.is_some() {
                    return Err("invalid capture exclusions request".into());
                }
                let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
                *self.0.lock().map_err(|_| "lock".to_string())? =
                    serde_json::from_value(value["apps"].clone()).map_err(|e| e.to_string())?;
                Ok(None)
            }
        }

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let backend = Lists(Mutex::new(vec!["com.example.Secret".into()]));
        let request = |json: &str| -> Result<RestRequest, String> {
            let args = vec!["capture-exclusions".to_string(), "set".to_string(), json.to_string()];
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing capture exclusions call")?;
            Ok(RestRequest {
                method: Method::Post,
                path: call.path.clone(),
                token: Some("t".into()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: call.body.clone(),
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
            })
        };

        // A removal (plus an addition): pending, and the lists stay put.
        let remove = request(r#"{"apps":["com.example.New"],"websites":[]}"#)?;
        assert_eq!(
            rest::route(&remove, &tokens, &ent),
            Routed::Write { tool: Tool::CaptureExclusionsSet, level: Level::L3 }
        );
        let change = loosening(&backend, remove.body.as_deref()).ok_or("a removal needs approval")?;
        let mut queue = ApprovalQueue::new();
        let (status, reply) = request_change(change, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
        assert_eq!(status, 202, "{reply}");
        assert!(reply.contains(r#""pending":true"#), "{reply}");
        assert!(reply.contains(r#""origin":"api""#), "{reply}");
        let id = queue.pending_ids()[0];
        assert!(matches!(
            queue.action(id),
            Some(SendAction::ResumeCapture { change }) if change.resumed_apps == vec!["com.example.Secret".to_string()]
        ));
        assert_eq!(*backend.0.lock().map_err(|_| "lock")?, vec!["com.example.Secret".to_string()]);

        // Additions only: direct, the saved lists come back, nothing queued.
        let add = request(r#"{"apps":["com.example.Secret","com.example.New"],"websites":[]}"#)?;
        assert!(loosening(&backend, add.body.as_deref()).is_none());
        let (status, reply) = rest::respond_with(&add, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        assert!(reply.contains("com.example.New"), "{reply}");
        assert_eq!(queue.pending_len(), 1);
        Ok(())
    }

    /// Issue #654: `writing-style enable` takes the same L3 route as REST, queued for the user's
    /// approval rather than applied, while `disable` and `clear` stay direct L1 writes.
    #[test]
    fn writing_style_enable_resolves_to_a_pending_approval_like_rest() -> Result<(), String> {
        use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue};
        use shogun_agents::permission::{Level, SendAction};
        use shogun_mcp::memory_api::{TokenRegistry, Tool};
        use shogun_mcp::rest::{self, ApprovalSurface, Method, RestRequest, Routed};
        use shogun_mcp::writing_voice_api::{needs_approval, parse, request_enable};

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        for action in ["enable", "disable", "clear"] {
            let args = vec!["writing-style".to_string(), action.to_string()];
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing writing style call")?;
            let request = RestRequest {
                method: Method::Post,
                path: call.path.clone(),
                token: Some("t".into()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: call.body.clone(),
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
            assert_eq!(
                rest::route(&request, &tokens, &ent),
                Routed::Write { tool: Tool::WritingVoiceSet, level: Level::L3 }
            );
            let body = call.body.as_deref();
            if action == "enable" {
                assert!(needs_approval(body));
                let mut queue = ApprovalQueue::new();
                let (status, reply) =
                    request_enable(body, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
                assert_eq!(status, 202, "{reply}");
                assert!(reply.contains(r#""pending":true"#), "{reply}");
                assert!(reply.contains(r#""origin":"api""#), "{reply}");
                let id = queue.pending_ids()[0];
                assert_eq!(queue.action(id), Some(&SendAction::ShareWritingStyle));
            } else {
                assert!(!needs_approval(body), "{action} is direct");
                let change = parse(body.ok_or("missing body")?).map_err(|e| e.code().to_string())?;
                assert_eq!(change.level(), Level::L1);
            }
        }
        Ok(())
    }

    /// Issue #82: the read and the two narrowing writes resolve like REST at L1; `composio draft-only off` reaches the server and is refused there
    /// (400, nothing written). The backend runs the real `shogun_mcp::connections_api` rules
    /// against temp files; the Keychain is never touched (the desktop applies a disconnect).
    #[test]
    fn connections_commands_resolve_like_rest_and_loosening_is_refused() -> Result<(), String> {
        use shogun_agents::permission::Level;
        use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::composio::Disclosures;
        use shogun_mcp::composio_policy;
        use shogun_mcp::connections_api::{self, ConnectionsPaths};
        use shogun_mcp::memory_api::{TokenRegistry, Tool};
        use shogun_mcp::rest::{self, Method, RestRequest, Routed};
        use shogun_mcp::scope::Service;

        struct Files(ConnectionsPaths);
        impl MemoryBackend for Files {
            fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn read_structured(&self, tool: Tool, _params: &ReadParams) -> Option<String> {
                (tool == Tool::ConnectionsList).then(|| connections_api::list_json(&self.0, 0).to_string())
            }
            fn write(&self, tool: Tool, body: &str) -> WriteResult {
                match tool {
                    Tool::ConnectionsDisconnect => {
                        connections_api::disconnect(&self.0, body).map(|_| None)
                    }
                    Tool::ConnectionsComposioSet => {
                        connections_api::composio_set(&self.0, body).map(|_| None)
                    }
                    Tool::ConnectionsGmailSet => {
                        connections_api::gmail_set(&self.0, body).map(|_| None)
                    }
                    _ => Err("not a write tool".into()),
                }
            }
        }

        let dir = std::env::temp_dir().join(format!("shogun-cli-connections-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = ConnectionsPaths {
            composio: dir.join("composio.json"),
            gmail: dir.join("gmail-policy.json"),
            status: dir.join(connections_api::STATUS_FILE),
            requests: dir.join(connections_api::REQUESTS_FILE),
        };
        composio_policy::update(&paths.composio, |p| {
            p.consent_acknowledged = true;
            p.disclosures = Disclosures { via_third_party: true, data_types: true, revocable: true };
            p.user_id = "user-123".into();
            p.gmail_connected = true;
            Ok(())
        })?;
        let backend = Files(paths.clone());
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let request = |args: &[&str]| -> Result<RestRequest, String> {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing connections call")?;
            assert!(call.path.starts_with("/v1/connections"), "{}", call.path);
            Ok(RestRequest {
                method: if call.method == "GET" { Method::Get } else { Method::Post },
                path: call.path.clone(),
                token: Some("t".into()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: call.body.clone(),
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
            })
        };

        let list = request(&["connections"])?;
        assert!(matches!(rest::route(&list, &tokens, &ent), Routed::Read { tool: Tool::ConnectionsList, .. }));
        let (status, reply) = rest::respond_with(&list, &tokens, &ent, &backend);
        assert_eq!(status, 200, "{reply}");
        assert!(reply.contains(r#""app_running":false"#), "{reply}");
        assert!(!reply.contains("user-123"), "no account id: {reply}");

        // Draft-only off: the server refuses it; nothing changes.
        let off = request(&["connections", "composio", "draft-only", "off"])?;
        assert_eq!(off.body.as_deref(), Some(r#"{"draft_stop":false}"#));
        let (status, reply) = rest::respond_with(&off, &tokens, &ent, &backend);
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("draft_stop_off_only_in_app"), "{reply}");
        assert!(composio_policy::load_effective(&paths.composio).draft_stop);

        let gmail_on = request(&["connections", "gmail", "draft-only", "on"])?;
        assert_eq!(gmail_on.path, "/v1/connections/gmail");
        assert_eq!(gmail_on.body.as_deref(), Some(r#"{"draft_stop":true}"#));
        assert_eq!(
            rest::route(&gmail_on, &tokens, &ent),
            Routed::Write {
                tool: Tool::ConnectionsGmailSet,
                level: Level::L1
            }
        );
        assert_eq!(
            rest::respond_with(&gmail_on, &tokens, &ent, &backend).0,
            202
        );
        let gmail_off = request(&["connections", "gmail", "draft-only", "off"])?;
        let (status, reply) = rest::respond_with(&gmail_off, &tokens, &ent, &backend);
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("draft_stop_off_only_in_app"), "{reply}");
        assert!(shogun_mcp::gmail_policy::load_effective(&paths.gmail).draft_stop);

        // Disconnect and revoke: direct L1, left for the desktop to apply.
        let disconnect = request(&["connections", "disconnect", "gcal"])?;
        assert_eq!(
            rest::route(&disconnect, &tokens, &ent),
            Routed::Write { tool: Tool::ConnectionsDisconnect, level: Level::L1 }
        );
        let (status, reply) = rest::respond_with(&disconnect, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        assert!(reply.contains(r#""level":"L1""#), "{reply}");
        let revoke = request(&["connections", "composio", "revoke"])?;
        assert_eq!(revoke.body.as_deref(), Some(r#"{"consent":false}"#));
        let (status, reply) = rest::respond_with(&revoke, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        let policy = composio_policy::load_effective(&paths.composio);
        assert!(!policy.consent_acknowledged && policy.draft_stop && !policy.gmail_connected);
        assert_eq!(
            connections_api::pending_disconnects(&paths.requests),
            vec![Service::GoogleCalendar]
        );
        let (status, _) = rest::respond_with(&request(&["connections", "composio", "draft-only", "on"])?, &tokens, &ent, &backend);
        assert_eq!(status, 202);
        let (status, reply) = rest::respond_with(&request(&["connections", "disconnect", "web"])?, &tokens, &ent, &backend);
        assert_eq!(status, 400, "{reply}");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// #940: once the trial has ended without a plan, the CLI's memory reads, searches and
    /// exports are refused with 403 `plan_required` before the store is touched. There is no
    /// keyword fallback: `search` and the `local_search` action are refused like everything else.
    #[test]
    fn an_expired_trial_refuses_cli_search_reads_and_exports_before_the_backend() -> Result<(), String> {
        use shogun_agents::entitlement::{entitlements, resolve_plan, BillingState, Plan, TRIAL_DURATION_MS};
        use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::memory_api::{TokenRegistry, Tool};
        use shogun_mcp::rest::{self, Method, RestRequest};

        struct Unreachable;
        impl MemoryBackend for Unreachable {
            fn read(&self, tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                panic!("{tool:?} reached the backend behind a locked plan")
            }
            fn write(&self, tool: Tool, _body: &str) -> WriteResult {
                panic!("{tool:?} reached the backend behind a locked plan")
            }
        }
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let request = |args: &[&str]| -> Result<RestRequest, String> {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing call")?;
            // The server splits the query string off before routing; the plan gate needs none of it.
            let path = call.path.split('?').next().unwrap_or_default().to_string();
            Ok(RestRequest {
                method: if call.method == "GET" { Method::Get } else { Method::Post },
                path,
                token: Some("t".into()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: call.body.clone(),
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
            })
        };
        let expired_at = 1_800_000_000_000_u64;
        let started = Some(expired_at - TRIAL_DURATION_MS);
        let locked_plans = [
            Plan::Trial { started_at_ms: started },
            resolve_plan(started, BillingState::Lapsed),
        ];
        let commands: &[&[&str]] = &[
            &["search", "delay", "the", "launch"],
            &["context"],
            &["pack", "launch"],
            &["people", "list"],
            &["commitments", "list"],
            &["state", "export"],
            &["meeting", "list", "42"],
            &["meeting", "export", "42", "--transcript"],
            &["meeting", "transcript", "42"],
            &["wiki", "list"],
            &["run", r#"{"kind":"local_search","query":"launch"}"#],
        ];
        for plan in locked_plans {
            let ent = entitlements(plan, expired_at);
            for args in commands {
                let req = request(args)?;
                let (status, body) = rest::respond_with(&req, &tokens, &ent, &Unreachable);
                assert_eq!(status, 403, "{args:?}: {body}");
                assert!(body.contains(r#""error":"plan_required""#), "{args:?}: {body}");
            }
        }
        // Buying a plan restores the same commands; Pro is the plan that carries the API faces.
        let pro = entitlements(resolve_plan(started, BillingState::Active(shogun_agents::entitlement::PaidPlan::Pro)), expired_at);
        let req = request(&["search", "launch"])?;
        assert!(!matches!(rest::route(&req, &tokens, &pro), shogun_mcp::rest::Routed::PlanLocked));
        Ok(())
    }

    /// Issue #400: a read-only token gets the same 403 `scope_insufficient` as REST on every
    /// change, and `api tokens` shows scopes but never a token.
    #[test]
    fn read_token_is_refused_every_cli_change_and_api_tokens_lists_scopes() -> Result<(), String> {
        use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::memory_api::{tool_level, ApiLevel, TokenRegistry, TokenScope, Tool};
        use shogun_mcp::rest::{self, Method, RestRequest, Routed};

        struct Nothing;
        impl MemoryBackend for Nothing {
            fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn write(&self, _tool: Tool, _body: &str) -> WriteResult {
                Ok(None)
            }
        }
        let mut tokens = TokenRegistry::new();
        tokens.issue_scoped("reader", TokenScope::Read);
        tokens.issue_scoped("writer", TokenScope::Full);
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let request = |args: &[&str], token: &str| -> Result<(RestRequest, Option<Tool>), String> {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing call")?;
            Ok((
                RestRequest {
                    method: if call.method == "GET" { Method::Get } else { Method::Post },
                    path: call.path.clone(),
                    token: Some(token.into()),
                    include_low: false,
                    query: None,
                    search_depth: None,
                    search_source_kind: None,
                    search_mode: None,
                    search_bound_invalid: false,
                    body: call.body.clone(),
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
                },
                parsed.command.tool(),
            ))
        };

        let changes: &[&[&str]] = &[
            &["note", "buy", "milk"],
            &["profile", "set", r#"{"role":"x"}"#],
            &["writing-style", "enable"],
            &["agent-source", "revoke"],
            &["connections", "disconnect", "gcal"],
            &["api", "restrict", "cursor"],
            &["visual-recall", "enable"],
            &["language", "set", "ja"],
            &["backup", "retention", "3"],
            &["run", r#"{"kind":"local_search","query":"q"}"#],
        ];
        for args in changes {
            let (req, tool) = request(args, "reader")?;
            let tool = tool.ok_or("every change maps to a tool")?;
            assert_ne!(tool_level(tool), ApiLevel::Read, "{args:?}");
            let (status, body) = rest::respond_with(&req, &tokens, &ent, &Nothing);
            assert_eq!(status, 403, "{args:?}: {body}");
            assert!(body.contains(r#""error":"scope_insufficient""#), "{args:?}: {body}");
            // The same command with a full token is not refused for scope.
            let (req, _) = request(args, "writer")?;
            assert!(!matches!(rest::route(&req, &tokens, &ent), Routed::ScopeInsufficient), "{args:?}");
        }

        // Reads added alongside scopes (#647, #285) pass the gate for a read token.
        for args in [&["backup", "status"][..], &["dream"][..], &["connections"][..]] {
            let (req, tool) = request(args, "reader")?;
            assert_eq!(tool.map(tool_level), Some(ApiLevel::Read), "{args:?}");
            assert!(!matches!(rest::route(&req, &tokens, &ent), Routed::ScopeInsufficient), "{args:?}");
        }

        let (req, tool) = request(&["api", "tokens"], "reader")?;
        assert_eq!(tool, Some(Tool::ApiTokensList));
        assert_eq!(req.path, "/v1/api/tokens");
        let (status, body) = rest::respond_with(&req, &tokens, &ent, &Nothing);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#""caller_scope":"read""#), "{body}");
        assert!(!body.contains("\"reader\"") && !body.contains("\"writer\""), "{body}");
        Ok(())
    }

    /// Issue #110: `agent-source use <cli>` takes the same L3 route as REST, queued for the user's
    /// approval rather than applied, while an API-key provider and `revoke` are direct L1 writes.
    /// The backend here runs the real `shogun_core::agent_source` rules against a temp `llm.json`.
    #[test]
    fn agent_source_subscription_resolves_to_a_pending_approval_like_rest() -> Result<(), String> {
        use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue};
        use shogun_agents::permission::{Level, SendAction};
        use shogun_core::agent_source;
        use shogun_core::llm::subscription::Delegate;
        use shogun_mcp::agent_source_api::{loosening, request_subscription};
        use shogun_mcp::backend::{AgentSubscriptionRequest, MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::memory_api::{TokenRegistry, Tool};
        use shogun_mcp::rest::{self, ApprovalSurface, Method, RestRequest, Routed};

        struct File(std::path::PathBuf);
        impl MemoryBackend for File {
            fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn agent_source_change(&self, body: &str) -> Result<Option<AgentSubscriptionRequest>, String> {
                Ok(agent_source::classify(&self.0, body)?.map(|d: Delegate| AgentSubscriptionRequest {
                    delegate: d.id().into(),
                    label: d.label().into(),
                    plan: d.plan_label().into(),
                }))
            }
            fn write(&self, _tool: Tool, body: &str) -> WriteResult {
                agent_source::apply_direct(&self.0, body).map(|_| None)
            }
        }

        let path = std::env::temp_dir().join(format!("shogun-cli-llm-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let backend = File(path.clone());
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let request = |args: &[&str]| -> Result<RestRequest, String> {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let parsed = crate::parse::parse(&args).map_err(|e| e.message())?;
            let call = to_call(&parsed.command, false).ok_or("missing agent source call")?;
            assert_eq!(call.path, "/v1/agent/source");
            Ok(RestRequest {
                method: if call.method == "GET" { Method::Get } else { Method::Post },
                path: call.path.clone(),
                token: Some("t".into()),
                include_low: false,
                query: None,
                search_depth: None,
                search_source_kind: None,
                search_mode: None,
                search_bound_invalid: false,
                body: call.body.clone(),
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
            })
        };

        assert!(matches!(
            rest::route(&request(&["agent-source"])?, &tokens, &ent),
            Routed::Read { tool: Tool::AgentSourceGet, .. }
        ));

        // A subscription: the L3 route, queued, and llm.json untouched.
        let use_cli = request(&["agent-source", "use", "claude-code"])?;
        assert_eq!(
            rest::route(&use_cli, &tokens, &ent),
            Routed::Write { tool: Tool::AgentSourceSet, level: Level::L3 }
        );
        let pending = loosening(&backend, use_cli.body.as_deref()).ok_or("a subscription needs approval")?;
        let mut queue = ApprovalQueue::new();
        let (status, reply) =
            request_subscription(&pending, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
        assert_eq!(status, 202, "{reply}");
        assert!(reply.contains(r#""pending":true"#), "{reply}");
        let id = queue.pending_ids()[0];
        assert_eq!(queue.action(id), Some(&SendAction::UseSubscriptionAgent { delegate: "claude-code".into() }));
        assert!(!path.exists(), "nothing written");
        // Without the server's queue it is refused, and the backend refuses it on its own too.
        let (status, reply) = rest::respond_with(&use_cli, &tokens, &ent, &backend);
        assert_eq!(status, 501, "{reply}");
        assert!(agent_source::apply_direct(&path, use_cli.body.as_deref().unwrap_or("")).is_err());
        assert!(!path.exists(), "nothing written");

        // An API-key provider and revoke: direct L1 writes.
        // #389: a model off the provider's list is a 400 here as on MCP and in the app's select.
        let unknown = request(&["agent-source", "use", "openai", "--model", "gpt-4o"])?;
        let (status, reply) = rest::respond_with(&unknown, &tokens, &ent, &backend);
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("unknown_model"), "{reply}");
        assert!(!path.exists(), "nothing written");
        let use_key = request(&["agent-source", "use", "openai", "--model", "gpt-6-astra"])?;
        assert!(loosening(&backend, use_key.body.as_deref()).is_none());
        let (status, reply) = rest::respond_with(&use_key, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        assert!(reply.contains(r#""level":"L1""#), "{reply}");
        assert_eq!(agent_source::load_strict(&path)?.provider, "openai");
        assert_eq!(agent_source::load_strict(&path)?.model, "gpt-6-astra");
        // Owner decision 2026-09-26: OpenRouter alone takes a free-form id (format-checked).
        let custom = request(&["agent-source", "use", "openrouter", "--model", "meta-llama/llama-4-maverick"])?;
        let (status, reply) = rest::respond_with(&custom, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        assert_eq!(agent_source::load_strict(&path)?.model, "meta-llama/llama-4-maverick");
        let bad = request(&["agent-source", "use", "openrouter", "--model", "sk-or-v1-abcdef"])?;
        let (status, reply) = rest::respond_with(&bad, &tokens, &ent, &backend);
        assert_eq!(status, 400, "{reply}");
        assert_eq!(agent_source::load_strict(&path)?.model, "meta-llama/llama-4-maverick");
        let reset = request(&["agent-source", "use", "openai", "--model", "gpt-6-astra"])?;
        assert_eq!(rest::respond_with(&reset, &tokens, &ent, &backend).0, 202);
        let revoke = request(&["agent-source", "revoke"])?;
        assert!(loosening(&backend, revoke.body.as_deref()).is_none());
        let (status, reply) = rest::respond_with(&revoke, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        assert!(!agent_source::load_strict(&path)?.subscription_consent);
        assert_eq!(queue.pending_len(), 1);

        // #390: `openai-store on` is refused (400, nothing queued or written); `off` is direct.
        let store_on = request(&["agent-source", "openai-store", "on"])?;
        assert!(loosening(&backend, store_on.body.as_deref()).is_none(), "never queued");
        let (status, reply) = rest::respond_with(&store_on, &tokens, &ent, &backend);
        assert_eq!(status, 400, "{reply}");
        assert!(!agent_source::load_strict(&path)?.openai_store);
        let mut on = agent_source::load_strict(&path)?;
        on.openai_store = true;
        agent_source::save(&path, &on)?;
        let store_off = request(&["agent-source", "openai-store", "off"])?;
        let (status, reply) = rest::respond_with(&store_off, &tokens, &ent, &backend);
        assert_eq!(status, 202, "{reply}");
        assert!(!agent_source::load_strict(&path)?.openai_store);
        assert_eq!(queue.pending_len(), 1);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn reads_are_gets_with_the_right_paths() {
        assert_eq!(
            to_call(&Command::People(ListOrGet::List), false).unwrap(),
            HttpCall {
                method: "GET",
                path: "/v1/state/people".into(),
                body: None
            }
        );
        assert_eq!(
            to_call(&Command::Commitments(ListOrGet::Get { id: 7 }), false)
                .unwrap()
                .path,
            "/v1/state/commitments/7"
        );
        assert_eq!(
            to_call(&Command::OpenLoops(ListOrGet::List), false)
                .unwrap()
                .path,
            "/v1/state/open_loops"
        );
        // `shogun calendar` reads the endpoint behind the Today page's "Coming up".
        assert_eq!(
            to_call(&Command::Calendar { page_size: None }, false).expect("test"),
            HttpCall {
                method: "GET",
                path: "/v1/calendar/upcoming".into(),
                body: None
            }
        );
        // Issue #10: `shogun wrap` reads the same endpoint the notch card's data comes from.
        assert_eq!(
            to_call(&Command::Wrap, false).unwrap(),
            HttpCall {
                method: "GET",
                path: "/v1/memory/wrap".into(),
                body: None
            }
        );
    }

    #[test]
    fn calendar_expansion_uses_the_shared_page_size_parameter() {
        let parsed = crate::parse::parse(&["calendar".into(), "upcoming".into(), "--page-size".into(), "100".into()]).expect("test");
        assert_eq!(to_call(&parsed.command, false).expect("test"), HttpCall {
            method: "GET", path: "/v1/calendar/upcoming?page_size=100".into(), body: None,
        });
        assert!(crate::parse::parse(&["calendar".into(), "--page-size".into(), "101".into()]).is_err());
    }

    #[test]
    fn periods_and_search_limit_ride_the_query_string() {
        let path = |c: Command| to_call(&c, false).unwrap().path;
        assert_eq!(path(Command::Periods { from_ms: None, to_ms: None, limit: None }), "/v1/memory/periods");
        assert_eq!(
            path(Command::Periods { from_ms: Some(1), to_ms: Some(2), limit: Some(5) }),
            "/v1/memory/periods?from_ms=1&to_ms=2&page_size=5"
        );
        let parsed = crate::parse::parse(&["search".into(), "x".into(), "--limit".into(), "7".into()]).unwrap();
        assert!(path(parsed.command).ends_with("&limit=7"));
    }

    #[test]
    fn search_encodes_the_query() {
        let call = to_call(
            &Command::Search {
                query: "budget review".into(),
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
        assert_eq!(call.path, "/v1/memory/search?q=budget%20review");
    }

    #[test]
    fn ai_search_asks_the_shared_rest_path_for_mode_ai() {
        let parsed = crate::parse::parse(&["search".into(), "--ai".into(), "when".into(), "did".into(), "we".into(), "ship".into()]).unwrap();
        assert!(matches!(&parsed.command, Command::Search { ai: true, query, .. } if query == "when did we ship"));
        let call = to_call(&parsed.command, false).unwrap();
        assert_eq!(call.path, "/v1/memory/search?q=when%20did%20we%20ship&mode=ai");
        assert_eq!(crate::http::read_timeout_for(&call.path), std::time::Duration::from_secs(45));
    }

    #[test]
    fn search_archive_controls_reach_the_shared_rest_path() {
        let call = to_call(
            &Command::Search {
                query: "budget".into(),
                depth: Some(shogun_mcp::backend::SearchDepthV1::All),
                from_ms: Some(10),
                to_ms: Some(20),
                source_kind: None,
                limit: None,
                ai: false,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            call.path,
            "/v1/memory/search?q=budget&depth=all&from_ms=10&to_ms=20"
        );
    }

    #[test]
    fn search_source_kind_reaches_shared_rest_path() {
        let call = to_call(
            &Command::Search {
                query: "Phoenix".into(),
                depth: None,
                from_ms: None,
                to_ms: None,
                source_kind: Some(shogun_mcp::backend::MemorySourceFamilyV1::Project),
                limit: None,
                ai: false,
            },
            false,
        )
        .unwrap();

        assert_eq!(call.path, "/v1/memory/search?q=Phoenix&source_kind=project");
    }

    #[test]
    fn include_low_is_appended_correctly() {
        // no existing query → ?include_low
        assert_eq!(
            to_call(&Command::People(ListOrGet::List), true)
                .unwrap()
                .path,
            "/v1/state/people?include_low"
        );
        // existing query → &include_low
        let s = to_call(
            &Command::Search {
                query: "x".into(),
                depth: None,
                from_ms: None,
                to_ms: None,
                source_kind: None,
                limit: None,
                ai: false,
            },
            true,
        )
        .unwrap()
        .path;
        assert_eq!(s, "/v1/memory/search?q=x&include_low");
    }

    #[test]
    fn writes_and_actions_are_posts_with_bodies() {
        assert_eq!(
            to_call(
                &Command::Note {
                    text: "call Bob".into()
                },
                false
            )
            .unwrap(),
            HttpCall {
                method: "POST",
                path: "/v1/memory/notes".into(),
                body: Some("call Bob".into())
            }
        );
        let run = to_call(
            &Command::Run {
                agent: r#"{"kind":"local_search","query":"x"}"#.into(),
            },
            false,
        )
        .unwrap();
        assert_eq!(run.method, "POST");
        assert_eq!(run.path, "/v1/actions/execute");
        assert_eq!(
            run.body.as_deref(),
            Some(r#"{"kind":"local_search","query":"x"}"#)
        );
    }

    #[test]
    fn disputes_calls_match_the_rest_contract() {
        assert_eq!(
            to_call(&Command::Disputes(DisputesCommand::List), false).unwrap(),
            HttpCall {
                method: "GET",
                path: "/v1/state/disputes".into(),
                body: None
            }
        );
        assert_eq!(
            to_call(&Command::Disputes(DisputesCommand::List), true)
                .unwrap()
                .path,
            "/v1/state/disputes?include_low"
        );
        assert_eq!(
            to_call(
                &Command::Disputes(DisputesCommand::Resolve {
                    id: 3,
                    resolution: "keep_later".into()
                }),
                false
            )
            .unwrap(),
            HttpCall {
                method: "POST",
                path: "/v1/state/disputes/resolve".into(),
                body: Some(r#"{"id":3,"resolution":"keep_later"}"#.into())
            }
        );
    }

    #[test]
    fn lessons_calls_match_the_rest_contract() {
        assert_eq!(
            to_call(
                &Command::Lessons(LessonsCommand::List(LessonsList::default())),
                false
            )
            .unwrap(),
            HttpCall {
                method: "GET",
                path: "/v1/lessons".into(),
                body: None
            }
        );
        assert_eq!(
            to_call(
                &Command::Lessons(LessonsCommand::List(LessonsList {
                    for_generation: true,
                    person_id: Some("alice@example.com".into()),
                    ..LessonsList::default()
                })),
                false
            )
            .unwrap()
            .path,
            "/v1/lessons?for_generation=true&person_id=alice%40example.com"
        );
        assert_eq!(
            to_call(&Command::Lessons(LessonsCommand::Enable { id: 7 }), false).unwrap(),
            HttpCall {
                method: "POST",
                path: "/v1/lessons/active".into(),
                body: Some(r#"{"id":7,"active":true}"#.into())
            }
        );
        assert_eq!(
            to_call(&Command::Lessons(LessonsCommand::Disable { id: 7 }), false)
                .unwrap()
                .body
                .as_deref(),
            Some(r#"{"id":7,"active":false}"#)
        );
    }

    #[test]
    fn backup_commands_call_the_symmetric_endpoints() {
        assert_eq!(
            to_call(&Command::Backup(BackupCommand::Status), false).unwrap(),
            HttpCall {
                method: "GET",
                path: "/v1/backup/status".into(),
                body: None,
            }
        );
        assert_eq!(
            to_call(&Command::Backup(BackupCommand::Retention { days: 3 }), false).unwrap(),
            HttpCall {
                method: "POST",
                path: "/v1/backup/retention".into(),
                body: Some(r#"{"days":3}"#.into()),
            }
        );
    }

    #[test]
    fn search_index_commands_call_the_symmetric_endpoints() {
        assert_eq!(
            to_call(&Command::SearchIndex(SearchIndexCommand::Status), false).unwrap(),
            HttpCall { method: "GET", path: "/v1/search_index/status".into(), body: None }
        );
        for (command, path) in [
            (SearchIndexCommand::Compact, "/v1/search_index/compact/start"),
            (SearchIndexCommand::Stop, "/v1/search_index/compact/stop"),
        ] {
            assert_eq!(
                to_call(&Command::SearchIndex(command), false).unwrap(),
                HttpCall { method: "POST", path: path.into(), body: Some("{}".into()) }
            );
        }
    }

    #[test]
    fn storage_preview_uses_read_endpoint() {
        assert_eq!(
            to_call(&Command::StoragePreview, false).unwrap(),
            HttpCall { method: "GET", path: "/v1/storage/preview".into(), body: None }
        );
    }

    #[test]
    fn hidden_self_cleanup_calls_existing_delete_endpoint_with_exclusive_intent() {
        assert_eq!(to_call(&Command::VisualRecall(VisualRecallCommand::DeleteHiddenSelfFrames), false).unwrap(),
            HttpCall { method: "POST", path: "/v1/visual_recall/frames/delete".into(), body: Some(r#"{"hidden_self_frames":true}"#.into()) });
    }

    #[test]
    fn visual_recall_retention_calls_the_symmetric_endpoint() {
        assert_eq!(
            to_call(
                &Command::VisualRecall(VisualRecallCommand::Retention { days: 30 }),
                false,
            )
            .unwrap(),
            HttpCall {
                method: "POST",
                path: "/v1/visual_recall/retention".into(),
                body: Some(r#"{"days":30}"#.into()),
            }
        );
    }

    #[test]
    fn visual_recall_search_encodes_app_and_time_filters() {
        let call = to_call(
            &Command::VisualRecall(VisualRecallCommand::Search {
                query: "project comet".into(),
                from_ms: Some(10),
                to_ms: Some(20),
                app_bundle_id: Some("com.apple.Safari Beta".into()),
            }),
            false,
        )
        .unwrap();
        assert_eq!(
            call.path,
            "/v1/visual_recall/frames/search?q=project%20comet&from_ms=10&to_ms=20&app_bundle_id=com.apple.Safari%20Beta"
        );
    }

    #[test]
    fn api_status_is_a_get_and_help_has_no_call() {
        assert_eq!(
            to_call(&Command::ApiStatus, false).unwrap().path,
            "/v1/status"
        );
        assert!(to_call(&Command::Help, false).is_none());
        assert_eq!(
            to_call(&Command::Whoami, false).unwrap().path,
            "/v1/profile/whoami"
        );
        assert_eq!(
            to_call(
                &Command::ProfileSet {
                    body: r#"{"role":"PM"}"#.into()
                },
                false
            )
            .unwrap()
            .body
            .as_deref(),
            Some(r#"{"role":"PM"}"#)
        );
    }

    /// Issue #394: the CLI speaks the REST Clarify contract (so its answers are recorded as
    /// `api`), and the answer route is L1 on both faces — never the L3 status/approval route.
    #[test]
    fn clarify_commands_parse_and_route_to_the_rest_contract() -> Result<(), String> {
        use crate::parse::parse;
        use shogun_mcp::memory_api::{tool_level, ApiLevel, Tool};
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let list = parse(&args(&["actions", "questions"])).map_err(|e| format!("{e:?}"))?;
        let call = to_call(&list.command, false).ok_or("no call")?;
        assert_eq!((call.method, call.path.as_str()), ("GET", "/v1/actions/questions"));
        let one = parse(&args(&["actions", "questions", "4"])).map_err(|e| format!("{e:?}"))?;
        assert_eq!(to_call(&one.command, false).ok_or("no call")?.path, "/v1/actions/questions/4");
        assert!(parse(&args(&["actions", "questions", "0"])).is_err());

        let answer = parse(&args(&["actions", "answer", "4", "Sam", "Ortiz"])).map_err(|e| format!("{e:?}"))?;
        assert_eq!(answer.command.tool(), Some(Tool::ActionsAnswer));
        assert_eq!(tool_level(Tool::ActionsAnswer), ApiLevel::Write(shogun_agents::permission::Level::L1));
        let call = to_call(&answer.command, false).ok_or("no call")?;
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/actions/answer"));
        let body: serde_json::Value =
            serde_json::from_str(call.body.as_deref().ok_or("no body")?).map_err(|e| e.to_string())?;
        assert_eq!(body, serde_json::json!({ "id": 4, "answer": "Sam Ortiz" }));
        assert!(parse(&args(&["actions", "answer", "4"])).is_err(), "an answer is required");
        assert!(parse(&args(&["actions", "answer", "x", "yes"])).is_err());
        Ok(())
    }

    /// Issue #395: `actions steer` speaks the REST contract (recorded as `api`) and is L1 on
    /// both faces; the note reaches a real store the runner reads, and a finished run is a 409.
    #[test]
    fn steer_command_routes_to_rest_and_reaches_the_runners_store() -> Result<(), String> {
        use crate::parse::parse;
        use shogun_agents::clarify::{RunKind, RunRef};
        use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use shogun_mcp::memory_api::{tool_level, ApiLevel, TokenRegistry, Tool};
        use shogun_mcp::rest::{self, Method, RestRequest};
        use shogun_mcp::steer_api;
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let steer = parse(&args(&["actions", "steer", "chat", "7", "make", "it", "shorter"]))
            .map_err(|e| format!("{e:?}"))?;
        assert_eq!(steer.command.tool(), Some(Tool::ActionsSteer));
        assert_eq!(tool_level(Tool::ActionsSteer), ApiLevel::Write(shogun_agents::permission::Level::L1));
        let call = to_call(&steer.command, false).ok_or("no call")?;
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/actions/steer"));
        let body: serde_json::Value =
            serde_json::from_str(call.body.as_deref().ok_or("no body")?).map_err(|e| e.to_string())?;
        assert_eq!(body, serde_json::json!({ "kind": "chat", "id": 7, "text": "make it shorter" }));
        assert!(parse(&args(&["actions", "steer", "chat", "7"])).is_err(), "a note is required");
        assert!(parse(&args(&["actions", "steer", "skill_run", "7", "x"])).is_err());
        assert!(parse(&args(&["actions", "steer", "chat", "x", "y"])).is_err());

        struct Store(std::path::PathBuf);
        impl MemoryBackend for Store {
            fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn write(&self, _tool: Tool, _body: &str) -> WriteResult {
                Err("unexpected write".into())
            }
            fn steer(&self, op: steer_api::SteerOperation) -> Result<String, String> {
                steer_api::apply(&self.0, &op, 5)
            }
        }
        let dir = std::env::temp_dir().join(format!("shogun-cli-steer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(steer_api::STORE_FILE);
        let backend = Store(path.clone());
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let ent = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        let request = RestRequest {
            method: Method::Post,
            path: call.path.clone(),
            token: Some("t".into()),
            include_low: false,
            query: None,
            search_depth: None,
            search_source_kind: None,
            search_mode: None,
            search_bound_invalid: false,
            body: call.body.clone(),
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
        let run = RunRef { kind: RunKind::Chat, id: 7 };
        // Not running yet: 404.
        let (status, reply) = rest::respond_with(&request, &tokens, &ent, &backend);
        assert_eq!(status, 404, "{reply}");
        // Running: accepted, and the runner reads it (origin `api`) at its next turn.
        steer_api::open_run(&path, run, 1)?;
        let (status, reply) = rest::respond_with(&request, &tokens, &ent, &backend);
        assert_eq!(status, 200, "{reply}");
        assert!(reply.contains(r#""delivery":"next_turn""#), "{reply}");
        let notes = steer_api::take_for_turn(&path, run)?;
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].text, "make it shorter");
        assert_eq!(notes[0].origin, shogun_agents::steer::SteerOrigin::Api);
        // Finished: 409.
        steer_api::close_run(&path, run, 9)?;
        let (status, reply) = rest::respond_with(&request, &tokens, &ent, &backend);
        assert_eq!(status, 409, "{reply}");
        assert!(reply.contains("run_finished"), "{reply}");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn meeting_audio_calls_match_rest_contract() {
        assert_eq!(to_call(&Command::MeetingSystemAudioStatus, false).unwrap().path, "/v1/meeting/system_audio");
        let retry = to_call(&Command::MeetingSystemAudioRetry, false).unwrap();
        assert_eq!(retry.path, "/v1/meeting/system_audio/retry");
        assert_eq!(retry.body.as_deref(), Some("{}"));
    }

    #[test]
    fn calendar_reminder_calls_match_rest_contract() {
        assert_eq!(
            to_call(&Command::MeetingReminder { minutes: None }, false)
                .unwrap()
                .path,
            "/v1/meeting/reminder"
        );
        let call = to_call(&Command::MeetingReminder { minutes: Some(10) }, false).unwrap();
        assert_eq!(call.path, "/v1/meeting/reminder");
        assert_eq!(call.body.as_deref(), Some(r#"{"minutes":10}"#));
    }

    #[test]
    fn meeting_microphone_calls_match_rest_contract() {
        assert_eq!(
            to_call(
                &Command::MeetingMicrophone(MicrophoneCommand::Show),
                false,
            )
                .unwrap()
                .path,
            "/v1/meeting/microphone"
        );
        assert_eq!(
            to_call(
                &Command::MeetingMicrophone(MicrophoneCommand::Set {
                    microphone: Some("Studio Mic".into()),
                }),
                false,
            )
            .unwrap()
            .body
            .as_deref(),
            Some(r#"{"microphone":"Studio Mic"}"#)
        );
    }

    #[test]
    fn meeting_start_posts_the_record_now_route() {
        let call = to_call(&Command::MeetingRecordNow { in_person: false }, false).unwrap();
        assert_eq!(call.path, "/v1/meeting/start");
        assert_eq!(call.body.as_deref(), Some(r#"{"in_person":false}"#));
        // `--in-person` (issue #262 FR-MC-01) rides the same route with the flag set.
        let room = to_call(&Command::MeetingRecordNow { in_person: true }, false).unwrap();
        assert_eq!(room.path, "/v1/meeting/start");
        assert_eq!(room.body.as_deref(), Some(r#"{"in_person":true}"#));
        let poll = to_call(&Command::MeetingStartStatus { id: 12 }, false).unwrap();
        assert_eq!(poll.path, "/v1/meeting/start/12");
        assert_eq!(poll.method, "GET");
    }

    #[test]
    fn dashboard_read_commands_parse_and_route_to_the_matching_read_tools() {
        for (args, tool, path) in [
            (vec!["meeting", "list", "42"], shogun_mcp::memory_api::Tool::MeetingList, "/v1/meeting/list/42"),
            (vec!["meeting", "export", "42", "--transcript"], shogun_mcp::memory_api::Tool::MeetingRecapExport, "/v1/meeting/recap/export/42?q=transcript"),
            (vec!["meeting", "recap"], shogun_mcp::memory_api::Tool::MeetingRecapGet, "/v1/meeting/recap"),
            (vec!["meeting", "recap", "42"], shogun_mcp::memory_api::Tool::MeetingRecapGet, "/v1/meeting/recap/42"),
            (vec!["meeting", "transcript"], shogun_mcp::memory_api::Tool::MeetingTranscriptGet, "/v1/meeting/transcript"),
            (vec!["meeting", "transcript", "42"], shogun_mcp::memory_api::Tool::MeetingTranscriptGet, "/v1/meeting/transcript/42"),
            (vec!["wiki", "evidence", "person-2"], shogun_mcp::memory_api::Tool::WikiEvidence, "/v1/wiki/evidence?q=person-2"),
            (vec!["wiki", "list", "--include-low"], shogun_mcp::memory_api::Tool::WikiList, "/v1/wiki/list?include_low"),
            (vec!["wiki", "evidence", "knowledge-3"], shogun_mcp::memory_api::Tool::WikiEvidence, "/v1/wiki/evidence?q=knowledge-3"),
            (vec!["wiki", "neighbors", "project-3", "--include-low"], shogun_mcp::memory_api::Tool::WikiNeighbors, "/v1/wiki/neighbors?q=project-3&include_low"),
        ] {
            let invocation = crate::parse::parse(&args.iter().map(|value| value.to_string()).collect::<Vec<_>>()).unwrap();
            assert_eq!(invocation.command.tool(), Some(tool));
            let call = to_call(&invocation.command, invocation.include_low).unwrap();
            assert_eq!(call.path, path);
            assert_eq!(call.method, "GET");
        }
        for args in [vec!["wiki", "evidence", "person--1"], vec!["meeting", "list", "0"], vec!["meeting", "list", "1", "extra"], vec!["meeting", "recap", "0"], vec!["meeting", "transcript", "3", "extra"]] {
            assert!(crate::parse::parse(&args.iter().map(|value| value.to_string()).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn meeting_speaker_template_and_export_calls_match_rest_contract() {
        assert_eq!(
            to_call(&Command::MeetingSpeakers(SpeakersCommand::Show), false)
                .unwrap()
                .path,
            "/v1/meeting/speakers"
        );
        let name = to_call(
            &Command::MeetingSpeakers(SpeakersCommand::Name {
                key: "other:0".into(),
                name: "Tanaka".into(),
            }),
            false,
        )
        .unwrap();
        assert_eq!(name.method, "POST");
        assert_eq!(
            name.body.as_deref(),
            Some(r#"{"display_name":"Tanaka","speaker_key":"other:0"}"#)
        );
        // Unname is an empty display_name — the server's clear contract.
        let unname = to_call(
            &Command::MeetingSpeakers(SpeakersCommand::Unname { key: "other:0".into() }),
            false,
        )
        .unwrap();
        assert_eq!(
            unname.body.as_deref(),
            Some(r#"{"display_name":"","speaker_key":"other:0"}"#)
        );
        assert_eq!(
            to_call(&Command::MeetingTemplates(TemplatesCommand::Show), false)
                .unwrap()
                .path,
            "/v1/meeting/templates"
        );
        assert_eq!(
            to_call(
                &Command::MeetingTemplates(TemplatesCommand::Set { id: "sales_call".into() }),
                false,
            )
            .unwrap()
            .body
            .as_deref(),
            Some(r#"{"template_id":"sales_call"}"#)
        );
        assert_eq!(
            to_call(&Command::MeetingExport { transcript: false, session_id: None }, false)
                .unwrap()
                .path,
            "/v1/meeting/recap/export"
        );
        assert_eq!(
            to_call(&Command::MeetingExport { transcript: true, session_id: None }, false)
                .unwrap()
                .path,
            "/v1/meeting/recap/export?q=transcript"
        );
    }

    #[test]
    fn export_commands_map_to_the_export_routes_at_l2() {
        // Issue #397: the CLI face of LocalAction::WriteFile — same tool, same L2, same route.
        use shogun_mcp::memory_api::{tool_level, ApiLevel, Tool};
        let call = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let command = crate::parse::parse(&args).unwrap().command;
            (command.tool(), to_call(&command, false).unwrap())
        };
        let (tool, save) = call(&["export", "--to", "/Users/me/Notes"]);
        assert_eq!(tool, Some(Tool::ExportWriteFile));
        assert_eq!(tool_level(Tool::ExportWriteFile), ApiLevel::Write(shogun_agents::permission::Level::L2));
        assert_eq!(save.path, "/v1/export/file");
        assert_eq!(
            save.body.as_deref(),
            Some(r#"{"folder":"/Users/me/Notes","format":"md","include_transcript":false}"#)
        );
        let (_, csv) = call(&["export", "meeting", "42", "--to", "/n", "--csv", "--name", "Q3", "--transcript"]);
        assert_eq!(
            csv.body.as_deref(),
            Some(r#"{"folder":"/n","format":"csv","include_transcript":true,"name":"Q3","session_id":42}"#)
        );
        let (tool, status) = call(&["export", "status", "7"]);
        assert_eq!((tool, status.path.as_str()), (Some(Tool::ExportWritesList), "/v1/export/writes/7"));
        assert_eq!(call(&["export", "history"]).1.path, "/v1/export/writes");
        let (tool, folders) = call(&["export", "folders"]);
        assert_eq!((tool, folders.path.as_str()), (Some(Tool::ExportFoldersList), "/v1/export/folders"));
        let (tool, rm) = call(&["export", "folders", "remove", "/n"]);
        assert_eq!((tool, rm.path.as_str()), (Some(Tool::ExportFoldersRemove), "/v1/export/folders/remove"));
        // No folder → refused by the parser, nothing is sent.
        let bare: Vec<String> = ["export", "meeting"].iter().map(|s| s.to_string()).collect();
        assert!(crate::parse::parse(&bare).is_err());
    }

    #[test]
    fn schedule_commands_map_to_the_schedule_routes() {
        let call = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let command = crate::parse::parse(&args).unwrap().command;
            (command.tool(), to_call(&command, false).unwrap())
        };
        assert_eq!(call(&["schedule"]).1.path, "/v1/schedules");
        let (tool, reports) = call(&["support", "reports"]);
        assert_eq!((tool, reports.path.as_str()), (Some(shogun_mcp::memory_api::Tool::SupportReportsList), "/v1/support/reports"));
        // Issue #285: the nightly review's status, bare or with `status`.
        for args in [&["dream"][..], &["dream", "status"][..]] {
            let (tool, dream) = call(args);
            assert_eq!((tool, dream.path.as_str()), (Some(shogun_mcp::memory_api::Tool::DreamStatus), "/v1/dream/status"));
            assert_eq!(dream.body, None);
        }
        let bad: Vec<String> = ["dream", "run"].iter().map(|s| s.to_string()).collect();
        assert!(crate::parse::parse(&bad).is_err(), "dream has no write verb");
        let (tool, add) = call(&["schedule", "add", "weekly", "fri", "16:00", "for", "the", "team"]);
        assert_eq!(tool, Some(shogun_mcp::memory_api::Tool::SchedulesAdd));
        assert_eq!(
            add.body.as_deref(),
            Some(r#"{"at":"16:00","days":"fri","input":"for the team","skill":"weekly"}"#)
        );
        let (tool, rm) = call(&["schedule", "remove", "3"]);
        assert_eq!((tool, rm.path.as_str()), (Some(shogun_mcp::memory_api::Tool::SchedulesRemove), "/v1/schedules/remove"));
        let short: Vec<String> = ["schedule", "add", "weekly", "fri"].iter().map(|s| s.to_string()).collect();
        assert!(crate::parse::parse(&short).is_err());
    }

    #[test]
    fn skills_commands_map_to_the_skill_routes() {
        let call = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let command = crate::parse::parse(&args).unwrap().command;
            (command.tool(), to_call(&command, false).unwrap())
        };
        let (tool, list) = call(&["skills"]);
        assert_eq!((tool, list.path.as_str()), (Some(shogun_mcp::memory_api::Tool::SkillsList), "/v1/skills"));
        assert_eq!(call(&["skills", "show", "weekly-report"]).1.path, "/v1/skills?q=weekly-report");
        let (tool, run) = call(&["skills", "run", "weekly-report", "this", "week"]);
        assert_eq!(tool, Some(shogun_mcp::memory_api::Tool::SkillsRun));
        assert_eq!((run.method, run.path.as_str()), ("POST", "/v1/skills/run"));
        assert_eq!(run.body.as_deref(), Some(r#"{"input":"this week","skill":"weekly-report"}"#));
        assert_eq!(call(&["skills", "status", "7"]).1.path, "/v1/skills/run/7");
        let file = std::env::temp_dir().join(format!("shogun-cli-skill-{}.md", std::process::id()));
        std::fs::write(&file, "---\nname: a\n---").unwrap();
        let (tool, save) = call(&["skills", "save", file.to_str().unwrap()]);
        assert_eq!((tool, save.path.as_str()), (Some(shogun_mcp::memory_api::Tool::SkillsSave), "/v1/skills"));
        assert_eq!(save.body.as_deref(), Some(r#"{"markdown":"---\nname: a\n---"}"#));
        let _ = std::fs::remove_file(&file);
        let bad: Vec<String> = ["skills", "status", "0"].iter().map(|s| s.to_string()).collect();
        assert!(crate::parse::parse(&bad).is_err());
    }

    #[test]
    fn meeting_track_parses_and_posts_the_recap_contract() {
        let args: Vec<String> = ["meeting", "track", "--session", "7", "--theirs", "--counterparty", "3", "Review", "the", "contract"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let command = crate::parse::parse(&args).unwrap().command;
        assert_eq!(command.tool(), Some(shogun_mcp::memory_api::Tool::MeetingRecapTrack));
        let call = to_call(&command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/meeting/recap/track"));
        let body: serde_json::Value = serde_json::from_str(call.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"text": "Review the contract", "session_id": 7, "direction": "theirs", "counterparty_id": 3})
        );
        let bare: Vec<String> = ["meeting", "track"].iter().map(|s| s.to_string()).collect();
        assert!(crate::parse::parse(&bare).is_err(), "text is required");
        let bad: Vec<String> = ["meeting", "track", "--session", "x", "a"].iter().map(|s| s.to_string()).collect();
        assert!(crate::parse::parse(&bad).is_err());
    }

    #[test]
    fn meeting_prefs_calls_match_rest_contract() {
        let show = to_call(&Command::MeetingRecapPrefs(RecapPrefsCommand::Show), false).unwrap();
        assert_eq!(show.method, "GET");
        assert_eq!(show.path, "/v1/meeting/recap_prefs");
        let parsed = crate::parse::parse(&["meeting", "prefs", "set", "--language", "japanese", "--instruction", "Group", "by", "team"].map(String::from)).unwrap();
        let set = to_call(&parsed.command, false).unwrap();
        assert_eq!(set.method, "POST");
        assert_eq!(set.body.as_deref(), Some(r#"{"minutes_instruction":"Group by team","recap_language":"japanese"}"#));
        let cleared = crate::parse::parse(&["meeting", "prefs", "set", "--clear-instruction"].map(String::from)).unwrap();
        assert_eq!(to_call(&cleared.command, false).unwrap().body.as_deref(), Some(r#"{"minutes_instruction":null}"#));
        assert!(crate::parse::parse(&["meeting", "prefs", "set"].map(String::from)).is_err(), "nothing to set");
        assert!(crate::parse::parse(&["meeting", "prefs", "set", "--bogus"].map(String::from)).is_err());
    }

    #[test]
    fn meeting_search_and_delete_map_to_their_routes() {
        let parsed = crate::parse::parse(&["meeting", "search", "vendor", "renewal"].map(String::from)).unwrap();
        let call = to_call(&parsed.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("GET", "/v1/meeting/search?q=vendor%20renewal"));
        assert_eq!(parsed.command.tool(), Some(shogun_mcp::memory_api::Tool::MeetingSearch));

        let parsed = crate::parse::parse(&["meeting", "delete", "7"].map(String::from)).unwrap();
        let call = to_call(&parsed.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/meeting/7/delete"));
        assert_eq!(parsed.command.tool(), Some(shogun_mcp::memory_api::Tool::MeetingDelete));

        assert!(crate::parse::parse(&["meeting", "search"].map(String::from)).is_err());
        assert!(crate::parse::parse(&["meeting", "delete"].map(String::from)).is_err());
        assert!(crate::parse::parse(&["meeting", "delete", "0"].map(String::from)).is_err());
        assert!(crate::parse::parse(&["meeting", "delete", "7", "now"].map(String::from)).is_err());
    }

    #[test]
    fn meeting_recap_edit_and_reset_post_the_shared_body() {
        let parsed = crate::parse::parse(&["meeting", "recap", "edit", "42", r#"{"summary":"We agreed."}"#].map(String::from)).unwrap();
        let call = to_call(&parsed.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/meeting/recap/edit"));
        let body: serde_json::Value = serde_json::from_str(call.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["session_id"], 42);
        assert_eq!(body["summary"], "We agreed.");

        // No id: the server picks the latest meeting; the JSON may span several argv words.
        let parsed = crate::parse::parse(&["meeting", "recap", "edit", "{\"decisions\":", "[\"renew\"]}"].map(String::from)).unwrap();
        let body: serde_json::Value = serde_json::from_str(to_call(&parsed.command, false).unwrap().body.as_deref().unwrap()).unwrap();
        assert!(body.get("session_id").is_none());
        assert_eq!(body["decisions"][0], "renew");

        let parsed = crate::parse::parse(&["meeting", "recap", "reset", "7"].map(String::from)).unwrap();
        assert_eq!(to_call(&parsed.command, false).unwrap().body.as_deref(), Some(r#"{"reset":true,"session_id":7}"#));
        assert_eq!(parsed.command.tool(), Some(shogun_mcp::memory_api::Tool::MeetingRecapEdit));

        assert!(crate::parse::parse(&["meeting", "recap", "edit"].map(String::from)).is_err());
        assert!(crate::parse::parse(&["meeting", "recap", "edit", "0", "{}"].map(String::from)).is_err());
        assert!(crate::parse::parse(&["meeting", "recap", "reset", "7", "extra"].map(String::from)).is_err());
    }

    #[test]
    fn voice_dictionary_import_and_export_calls_match_rest_contract() {
        let export = to_call(&Command::VoiceDictionary(VoiceDictionaryCommand::Export), false).unwrap();
        assert_eq!(export.method, "GET");
        assert_eq!(export.path, "/v1/voice_dictionary/export");
        assert!(export.body.is_none());

        // An unresolved source is never sent: the binary reads it into text first.
        assert!(to_call(
            &Command::VoiceDictionary(VoiceDictionaryCommand::Import {
                source: ImportSource::Path("terms.csv".into())
            }),
            false
        )
        .is_none());
        let resolved = resolve_import_source(Command::VoiceDictionary(VoiceDictionaryCommand::Import {
            source: ImportSource::Text("canonical,aliases\nSelect KK,セレクト".into()),
        }))
        .unwrap();
        let import = to_call(&resolved, false).unwrap();
        assert_eq!(import.method, "POST");
        assert_eq!(import.path, "/v1/voice_dictionary/import");
        let body: serde_json::Value = serde_json::from_str(import.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["text"], "canonical,aliases\nSelect KK,セレクト");

        // A blank source is refused before any request is built.
        assert!(resolve_import_source(Command::VoiceDictionary(VoiceDictionaryCommand::Import {
            source: ImportSource::Text("  \n".into()),
        }))
        .is_err());
        // Other commands pass through untouched.
        assert_eq!(
            resolve_import_source(Command::VoiceDictionary(VoiceDictionaryCommand::List)).unwrap(),
            Command::VoiceDictionary(VoiceDictionaryCommand::List)
        );

        let parsed = crate::parse::parse(&["voice-dictionary".to_string(), "import".to_string(), "-".to_string()]).unwrap();
        assert_eq!(
            parsed.command,
            Command::VoiceDictionary(VoiceDictionaryCommand::Import { source: ImportSource::Stdin })
        );
        assert!(crate::parse::parse(&["voice-dictionary".to_string(), "import".to_string()]).is_err());
    }

    #[test]
    fn voice_microphone_calls_match_rest_contract() {
        let show = to_call(&Command::VoiceMicrophone(MicrophoneCommand::Show), false).unwrap();
        assert_eq!(show.method, "GET");
        assert_eq!(show.path, "/v1/voice/microphone");

        let set = to_call(
            &Command::VoiceMicrophone(MicrophoneCommand::Set {
                microphone: Some("Studio Mic".into()),
            }),
            false,
        )
        .unwrap();
        assert_eq!(set.method, "POST");
        assert_eq!(set.path, "/v1/voice/microphone");
        assert_eq!(set.body.as_deref(), Some(r#"{"microphone":"Studio Mic"}"#));

        // The default-input reset is an explicit null on the wire, not an omitted field.
        assert_eq!(
            to_call(
                &Command::VoiceMicrophone(MicrophoneCommand::Set { microphone: None }),
                false,
            )
            .unwrap()
            .body
            .as_deref(),
            Some(r#"{"microphone":null}"#)
        );
    }

    #[test]
    fn mail_calls_match_the_rest_contract() {
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::TriageList(MailTriageList::default())),
                false
            )
            .unwrap(),
            HttpCall {
                method: "GET",
                path: "/v1/mail/triage".into(),
                body: None
            }
        );
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::TriageList(MailTriageList {
                    bucket: Some("other".into()),
                    attention: Some("needs_reply".into()),
                    label: Some("custom:vendors".into()),
                    handled: true,
                })),
                true
            )
            .unwrap()
            .path,
            "/v1/mail/triage?bucket=other&attention=needs_reply&label=custom%3Avendors&include_handled=true&include_low"
        );
        // The thread key is a plain path segment — the daemon reads it from the path.
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::TriageShow {
                    thread_key: "gmail:18f3".into()
                }),
                false
            )
            .unwrap()
            .path,
            "/v1/mail/triage/gmail:18f3"
        );
        let set = to_call(
            &Command::Mail(MailCommand::TriageSet {
                thread_key: "gmail:18f3".into(),
                label: Some("news".into()),
                bucket: Some("other".into()),
                handled: Some(true),
            }),
            false,
        )
        .unwrap();
        assert_eq!(set.method, "POST");
        assert_eq!(set.path, "/v1/mail/triage/gmail:18f3");
        assert_eq!(
            set.body.as_deref(),
            Some(r#"{"bucket":"other","handled":true,"label_key":"news"}"#)
        );
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::TriageArchive {
                    thread_key: "gmail:18f3".into()
                }),
                false
            )
            .unwrap(),
            HttpCall {
                method: "POST",
                path: "/v1/mail/triage/gmail:18f3/archive".into(),
                body: Some(String::new())
            }
        );
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::TriageUnarchive {
                    thread_key: "gmail:18f3".into()
                }),
                false
            )
            .unwrap()
            .path,
            "/v1/mail/triage/gmail:18f3/unarchive"
        );
        assert_eq!(
            to_call(&Command::Mail(MailCommand::LabelsList), false)
                .unwrap()
                .path,
            "/v1/mail/labels"
        );
        // A built-in key travels as `key`; a fresh name as `name`; flags and the standing
        // confirmation only when given.
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::LabelsSet(MailLabelSet {
                    key_or_name: "marketing".into(),
                    auto_archive: Some(true),
                    confirm_standing: true,
                    ..MailLabelSet::default()
                })),
                false
            )
            .unwrap(),
            HttpCall {
                method: "POST",
                path: "/v1/mail/labels".into(),
                body: Some(r#"{"auto_archive":true,"confirm_standing":true,"key":"marketing"}"#.into())
            }
        );
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::LabelsSet(MailLabelSet {
                    key_or_name: "Vendors".into(),
                    definition: Some("Invoices from suppliers".into()),
                    bucket: Some("other".into()),
                    sync_to_gmail: Some(false),
                    ..MailLabelSet::default()
                })),
                false
            )
            .unwrap()
            .body
            .as_deref(),
            Some(r#"{"default_bucket":"other","definition":"Invoices from suppliers","name":"Vendors","sync_to_gmail":false}"#)
        );
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::LabelsSet(MailLabelSet {
                    key_or_name: "custom:vendors".into(),
                    name: Some("Suppliers".into()),
                    ..MailLabelSet::default()
                })),
                false
            )
            .unwrap()
            .body
            .as_deref(),
            Some(r#"{"key":"custom:vendors","name":"Suppliers"}"#)
        );
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::LabelsRemove {
                    key: "custom:vendors".into()
                }),
                false
            )
            .unwrap()
            .path,
            "/v1/mail/labels/custom:vendors/delete"
        );
        // Issue #779: a label named in Japanese, or with a slash, is percent-encoded as one
        // segment; the colon stays literal.
        assert_eq!(
            to_call(
                &Command::Mail(MailCommand::LabelsRemove { key: "custom:会議/a".into() }),
                false
            )
            .unwrap()
            .path,
            "/v1/mail/labels/custom:%E4%BC%9A%E8%AD%B0%2Fa/delete"
        );
        assert_eq!(
            to_call(&Command::Mail(MailCommand::RulesList), false)
                .unwrap()
                .path,
            "/v1/mail/sender_rules"
        );
        let add = to_call(
            &Command::Mail(MailCommand::RulesAdd {
                match_kind: "domain".into(),
                match_value: "news.example.com".into(),
                label: "news".into(),
                bucket: Some("other".into()),
            }),
            false,
        )
        .unwrap();
        assert_eq!(add.path, "/v1/mail/sender_rules");
        assert_eq!(
            add.body.as_deref(),
            Some(r#"{"bucket":"other","label_key":"news","match_kind":"domain","match_value":"news.example.com"}"#)
        );
        assert_eq!(
            to_call(&Command::Mail(MailCommand::RulesRemove { id: 7 }), false)
                .unwrap()
                .path,
            "/v1/mail/sender_rules/7/delete"
        );
        assert_eq!(
            to_call(&Command::Mail(MailCommand::History), false)
                .unwrap()
                .path,
            "/v1/mail/history"
        );
    }

    /// Routing parity: the CLI reaches the same tools the MCP face exposes (invariant 6).
    #[test]
    fn voice_microphone_commands_map_to_the_shared_tools() {
        use shogun_mcp::memory_api::Tool;
        assert_eq!(
            Command::VoiceMicrophone(MicrophoneCommand::Show).tool(),
            Some(Tool::VoiceMicrophoneGet)
        );
        assert_eq!(
            Command::VoiceMicrophone(MicrophoneCommand::Set { microphone: None }).tool(),
            Some(Tool::VoiceMicrophoneSet)
        );
    }

    #[test]
    fn capture_exclusions_use_the_shared_rest_contract() {
        assert_eq!(to_call(&Command::CaptureExclusions(CaptureExclusionsCommand::List), false).unwrap().path, "/v1/capture_exclusions");
        assert_eq!(to_call(&Command::CaptureExclusions(CaptureExclusionsCommand::Set { body: "{}".into() }), false).unwrap().method, "POST");
    }

    #[test]
    fn state_commands_use_the_shared_rest_contract() {
        let edit = to_call(&Command::State(StateCommand::Edit { body: r#"{"table":"people","id":1,"name":"A"}"#.into() }), false).unwrap();
        assert_eq!((edit.method, edit.path.as_str()), ("POST", "/v1/state/update"));
        assert_eq!(edit.body.as_deref(), Some(r#"{"table":"people","id":1,"name":"A"}"#));
        let delete = to_call(&Command::State(StateCommand::Delete { body: "{}".into() }), false).unwrap();
        assert_eq!((delete.method, delete.path.as_str()), ("POST", "/v1/state/delete"));
        assert_eq!(to_call(&Command::State(StateCommand::Export), false).unwrap().path, "/v1/state/export");
        assert_eq!(to_call(&Command::State(StateCommand::Export), true).unwrap().path, "/v1/state/export?include_low");
        let preview = to_call(&Command::Import(ImportCommand::Preview { body: "{}".into() }), false).unwrap();
        assert_eq!((preview.method, preview.path.as_str(), preview.body.as_deref()), ("POST", "/v1/memory/import/preview", Some("{}")));
        let commit = to_call(&Command::Import(ImportCommand::Commit { body: "{}".into() }), false).unwrap();
        assert_eq!((commit.method, commit.path.as_str()), ("POST", "/v1/memory/import"));
        assert_eq!(to_call(&Command::Import(ImportCommand::List), false).unwrap().path, "/v1/memory/imports");
        assert_eq!(to_call(&Command::Import(ImportCommand::Show { id: 2 }), true).unwrap().path, "/v1/memory/imports/2?include_low");
        let delete_import = to_call(&Command::Import(ImportCommand::Delete { id: 2 }), false).unwrap();
        assert_eq!((delete_import.method, delete_import.path.as_str()), ("POST", "/v1/memory/imports/2/delete"));
        let split = to_call(&Command::State(StateCommand::Split { body: "{}".into() }), false).unwrap();
        assert_eq!((split.method, split.path.as_str(), split.body.as_deref()), ("POST", "/v1/state/split", Some("{}")));
        let evidence = to_call(&Command::State(StateCommand::Evidence { table: "projects".into(), id: 4 }), false).unwrap();
        assert_eq!((evidence.method, evidence.path.as_str()), ("GET", "/v1/state/projects/4/evidence"));
        let merge = to_call(&Command::State(StateCommand::Merge { body: "{}".into() }), false).unwrap();
        assert_eq!((merge.method, merge.path.as_str(), merge.body.as_deref()), ("POST", "/v1/state/merge", Some("{}")));
        let apart = to_call(&Command::State(StateCommand::KeepSeparate { body: "{}".into() }), false).unwrap();
        assert_eq!((apart.method, apart.path.as_str()), ("POST", "/v1/state/keep-separate"));
        let dupes = to_call(&Command::State(StateCommand::Duplicates { table: "people".into() }), false).unwrap();
        assert_eq!((dupes.method, dupes.path.as_str()), ("GET", "/v1/state/people/merge-candidates"));
    }
}
