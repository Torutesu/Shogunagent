use shogun_agents::permission::Level;

use crate::memory_api::Tool;

use super::{Method, Routed};

pub(super) fn resolve(method: Method, path: &str) -> Result<Routed, RouteMiss> {
    // trim a trailing slash, then split.
    let path = path.strip_suffix('/').unwrap_or(path);
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    // A state noun endpoint: /v1/state/<noun>[/<id>]
    let state_tool = |noun: &str, has_id: bool| -> Option<Tool> {
        Some(match (noun, has_id) {
            ("people", false) => Tool::StatePeopleList,
            ("people", true) => Tool::StatePeopleGet,
            ("projects", false) => Tool::StateProjectsList,
            ("projects", true) => Tool::StateProjectsGet,
            ("commitments", false) => Tool::StateCommitmentsList,
            ("commitments", true) => Tool::StateCommitmentsGet,
            ("open_loops", false) => Tool::StateOpenLoopsList,
            ("open_loops", true) => Tool::StateOpenLoopsGet,
            _ => return None,
        })
    };

    match segs.as_slice() {
        ["v1", "forms", action @ ("inspect" | "plan" | "apply" | "undo" | "close")] => {
            let (tool, level) = match *action {
                "inspect" => (Tool::FormsInspect, Level::L2),
                "plan" => (Tool::FormsPlan, Level::L1),
                "apply" => (Tool::FormsApply, Level::L3),
                "undo" => (Tool::FormsUndo, Level::L3),
                _ => (Tool::FormsClose, Level::L1),
            };
            method_is(method, Method::Post, Routed::Write { tool, level })
        },
        ["v1", "forms", "status", id] => match id.parse::<u64>() {
            Ok(id) if id > 0 => method_is(method, Method::Get, Routed::Read { tool: Tool::FormsStatus, id: None }),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "capture", "status"] => method_is(method, Method::Get, Routed::Read { tool: Tool::CaptureStatus, id: None }),
        ["v1", "capture", "paused"] => method_is(method, Method::Post, Routed::Write { tool: Tool::CaptureSetPaused, level: Level::L3 }),
        ["v1", "wiki", "list"] => method_is(method, Method::Get, Routed::Read {tool:Tool::WikiList,id:None}),
        ["v1", "wiki", action @ ("evidence" | "neighbors")] => method_is(
            method, Method::Get, Routed::Read {
                tool: if *action == "evidence" { Tool::WikiEvidence } else { Tool::WikiNeighbors }, id: None,
            },
        ),
        ["v1", "meeting", "list"] => method_is(method, Method::Get, Routed::Read { tool: Tool::MeetingList, id: None }),
        ["v1", "meeting", "list", id] => match id.parse::<i64>() {
            Ok(id) if id > 0 => method_is(method, Method::Get, Routed::Read { tool: Tool::MeetingList, id: Some(id) }),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "status"] => method_is(method, Method::Get, Routed::Status),
        ["v1", "metrics"] => method_is(method, Method::Get, Routed::Metrics),
        ["v1", "memory", "search"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MemorySearch,
                id: None,
            },
        ),
        ["v1", "memory", "context"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MemoryGetContext,
                id: None,
            },
        ),
        // Open source events / read a span as sessions: reads that take a JSON body (the MCP
        // arguments), so they are POSTs, like the import preview.
        ["v1", "memory", "events"] => method_is(
            method,
            Method::Post,
            Routed::Read {
                tool: Tool::MemoryGetEvents,
                id: None,
            },
        ),
        ["v1", "memory", "session"] => method_is(
            method,
            Method::Post,
            Routed::Read {
                tool: Tool::MemoryReadSession,
                id: None,
            },
        ),
        // FR-API-08: the grounded context pack (`?q=` carries the task/question).
        ["v1", "memory", "context_pack"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MemoryGetContextPack,
                id: None,
            },
        ),
        // E4-T3: an agent's events / turns (an L1 write under an idempotency key) and the
        // budgeted recall (a read that takes a JSON body, so a POST like the events read).
        ["v1", "memory", "ingest"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MemoryIngest,
                level: Level::L1,
            },
        ),
        // E44: one provider webhook delivery through the ingest path (an L1 write).
        ["v1", "memory", "ingest-webhook"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MemoryIngestWebhook,
                level: Level::L1,
            },
        ),
        // E15 / E18 governance reads. Deciding (`POST /v1/approvals/{id}/decide`), granting
        // (`POST /v1/acl/grant|revoke`) and teams are the server's operator routes, answered before
        // this table and never reachable with an agent's bearer.
        ["v1", "approvals"] => method_is(method, Method::Get, Routed::Read { tool: Tool::ApprovalsList, id: None }),
        ["v1", "approvals", id] => match id.parse::<i64>() {
            Ok(id) if id > 0 => method_is(method, Method::Get, Routed::Read { tool: Tool::ApprovalsGet, id: Some(id) }),
            _ => Ok(Routed::NotFound),
        },
        ["v1", "audit"] => method_is(method, Method::Get, Routed::Read { tool: Tool::AuditList, id: None }),
        ["v1", "acl"] => method_is(method, Method::Get, Routed::Read { tool: Tool::AclList, id: None }),
        // E6-T2: the memory tool's file operations (`memory.fs`), the MCP arguments as the body.
        ["v1", "memory", "fs"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MemoryFs,
                level: Level::L1,
            },
        ),
        // E35: the A2A handoff. `create` is a read that takes a body (a POST, like the recall);
        // `accept` is an L1 write to the caller's own scratch.
        ["v1", "handoff"] => method_is(method, Method::Post, Routed::Read { tool: Tool::HandoffCreate, id: None }),
        ["v1", "handoff", "accept"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::HandoffAccept,
                level: Level::L1,
            },
        ),
        // E46: payment mandates and payments, the MCP arguments as the body. The list is a free
        // read (GET for everything, POST with a body for one mandate); a request and a spend are
        // L3 (queued, or pre-approved by a mandate); a revoke only narrows (L1).
        ["v1", "payments", "mandates"] => method_is(method, Method::Get, Routed::Read { tool: Tool::PaymentsMandatesList, id: None }),
        ["v1", "payments", "mandates", "list"] => {
            method_is(method, Method::Post, Routed::Read { tool: Tool::PaymentsMandatesList, id: None })
        }
        ["v1", "payments", "mandates", "request"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::PaymentsMandatesRequest, level: Level::L3 },
        ),
        ["v1", "payments", "mandates", "revoke"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::PaymentsMandatesRevoke, level: Level::L1 },
        ),
        ["v1", "payments", "spend"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::PaymentsSpend, level: Level::L3 },
        ),
        ["v1", "memory", "recall"] => method_is(
            method,
            Method::Post,
            Routed::Read {
                tool: Tool::MemoryRecall,
                id: None,
            },
        ),
        ["v1", "memory", "processing", id] => match id.parse::<i64>() {
            Ok(id) if id > 0 => method_is(method, Method::Get, Routed::Read {
                tool: Tool::MemoryProcessingStatus, id: Some(id),
            }),
            _ => Err(RouteMiss::NotFound),
        },
        // Memory import (#384 / #391). The preview is a read that takes a body, so it is a POST;
        // the commit and the undo are L1 local writes.
        ["v1", "memory", "import", "preview"] => method_is(
            method,
            Method::Post,
            Routed::Read {
                tool: Tool::MemoryImportPreview,
                id: None,
            },
        ),
        ["v1", "memory", "import"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MemoryImport,
                level: Level::L1,
            },
        ),
        ["v1", "memory", "imports"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MemoryImportList,
                id: None,
            },
        ),
        ["v1", "memory", "imports", id] => match id.parse::<i64>() {
            Ok(id) if id > 0 => method_is(
                method,
                Method::Get,
                Routed::Read {
                    tool: Tool::MemoryImportList,
                    id: Some(id),
                },
            ),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "memory", "imports", id, "delete"] => match id.parse::<i64>() {
            Ok(id) if id > 0 => method_is(
                method,
                Method::Post,
                Routed::Write {
                    tool: Tool::MemoryImportDelete,
                    level: Level::L1,
                },
            ),
            _ => Err(RouteMiss::NotFound),
        },
        // The Today page's "Coming up" (invariant 6): the synced calendar's next events, a read.
        ["v1", "calendar", "upcoming"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::CalendarUpcoming,
                id: None,
            },
        ),
        // Issue #10 (invariant 6): the Evening Wrap the notch card shows, as a read.
        ["v1", "memory", "wrap"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MemoryGetWrap,
                id: None,
            },
        ),
        // Issue #1066: stored hour/day/week/month summaries for a window (`?from_ms=&to_ms=`).
        ["v1", "memory", "periods"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MemoryGetPeriods,
                id: None,
            },
        ),
        ["v1", "profile", "whoami"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::ProfileWhoami,
                id: None,
            },
        ),
        ["v1", "profile"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::ProfileSet,
                level: Level::L1,
            },
        ),
        ["v1", "voice_dictionary", "terms"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::VoiceDictionaryList,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::VoiceDictionaryCreate,
                level: Level::L1,
            }),
        },
        ["v1", "voice_dictionary", "autoadd"] => match method {
            Method::Get => Ok(Routed::Read { tool: Tool::DictionaryAutoAddGet, id: None }),
            Method::Post => Ok(Routed::Write { tool: Tool::DictionaryAutoAddSet, level: Level::L3 }),
        },
        ["v1", "voice_dictionary", "import"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::VoiceDictionaryImport,
                level: Level::L1,
            },
        ),
        ["v1", "voice_dictionary", "export"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::VoiceDictionaryExport,
                id: None,
            },
        ),
        ["v1", "writing_voice"] => match method {
            Method::Get => Ok(Routed::Read { tool: Tool::WritingVoiceGet, id: None }),
            // L3 here for the same reason as in memory_api: enabling this opens an egress of
            // the user's own drafts, and the REST face must not be the cheaper way in. The server
            // queues an enable for approval; disable / clear run directly at L1 (issue #654).
            Method::Post => Ok(Routed::Write { tool: Tool::WritingVoiceSet, level: Level::L3 }),
        },
        ["v1", "agent", "source"] => match method {
            Method::Get => Ok(Routed::Read { tool: Tool::AgentSourceGet, id: None }),
            // L3 as in memory_api: selecting a subscription CLI is the opt-in to a consumer plan's
            // data handling. The server queues it for approval; switching to an API key or
            // withdrawing the opt-in runs directly at L1 (issue #110).
            Method::Post => Ok(Routed::Write { tool: Tool::AgentSourceSet, level: Level::L3 }),
        },
        // The Connections screen (issue #82): read it, disconnect one service, tighten the Mail
        // policy. Both writes only remove capability (L1); granting Mail consent or turning
        // draft-only mode off is refused by the backend, and connecting is only in the app.
        ["v1", "connections"] => method_is(method, Method::Get, Routed::Read { tool: Tool::ConnectionsList, id: None }),
        // Issue #400: the issued tokens' metadata and scopes. Read-only: issuing a token or
        // raising a scope happens only in Settings, so there is no POST here.
        ["v1", "api", "tokens"] => method_is(method, Method::Get, Routed::Read { tool: Tool::ApiTokensList, id: None }),
        // Owner decision 2026-10-20: withhold meeting notes and screen recall from one connected
        // app. Only narrows (L1); allowing them again is only in the app, so there is no route.
        ["v1", "api", "connections", "restrict"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::ApiConnectionsRestrict, level: Level::L1 },
        ),
        ["v1", "connections", "disconnect"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::ConnectionsDisconnect, level: Level::L1 },
        ),
        ["v1", "connections", "gmail"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::ConnectionsGmailSet,
                level: Level::L1,
            },
        ),
        ["v1", "connections", "composio"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::ConnectionsComposioSet, level: Level::L1 },
        ),
        ["v1", "voice_dictionary", "terms", id] => match id.parse::<i64>() {
            Ok(_) => method_is(
                method,
                Method::Post,
                Routed::Write {
                    tool: Tool::VoiceDictionaryUpdate,
                    level: Level::L1,
                },
            ),
            Err(_) => Err(RouteMiss::NotFound),
        },
        ["v1", "voice_dictionary", "terms", id, "delete"] => match id.parse::<i64>() {
            Ok(_) => method_is(
                method,
                Method::Post,
                Routed::Write {
                    tool: Tool::VoiceDictionaryDelete,
                    level: Level::L1,
                },
            ),
            Err(_) => Err(RouteMiss::NotFound),
        },
        ["v1", "meeting", "start"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MeetingRecordNow,
                level: Level::L2,
            },
        ),
        ["v1", "meeting", "start", id] => match (method, id.parse::<u64>()) {
            (Method::Get, Ok(id)) => match i64::try_from(id) {
                Ok(id) if id > 0 => Ok(Routed::Read {
                    tool: Tool::MeetingStartStatus,
                    id: Some(id),
                }),
                _ => Err(RouteMiss::NotFound),
            },
            (Method::Get, Err(_)) => Err(RouteMiss::NotFound),
            (_, Ok(_)) => Err(RouteMiss::MethodNotAllowed),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "settings", "language"] => match method {
            Method::Get => Ok(Routed::Read { tool: Tool::UiLanguageGet, id: None }),
            Method::Post => Ok(Routed::Write { tool: Tool::UiLanguageSet, level: Level::L1 }),
        },
        ["v1", "translate", "register"] => match method {
            Method::Get => Ok(Routed::Read { tool: Tool::TranslateRegisterGet, id: None }),
            Method::Post => Ok(Routed::Write { tool: Tool::TranslateRegisterSet, level: Level::L1 }),
        },
        // Issue #279: the language pair beside the register (same `translate.json`).
        ["v1", "translate", "pair"] => match method {
            Method::Get => Ok(Routed::Read { tool: Tool::TranslatePairGet, id: None }),
            Method::Post => Ok(Routed::Write { tool: Tool::TranslatePairSet, level: Level::L1 }),
        },
        // Issue #279: translate text / draft a reply (L1 — text only, run by the desktop on the
        // user's own model access), and read a request the face stopped waiting for.
        ["v1", "translate"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::TranslateText, level: Level::L1 },
        ),
        ["v1", "translate", "reply_draft"] => method_is(
            method,
            Method::Post,
            Routed::Write { tool: Tool::TranslateReplyDraft, level: Level::L1 },
        ),
        ["v1", "translate", "jobs", id] => match (method, id.parse::<i64>()) {
            (Method::Get, Ok(id)) if id > 0 => {
                Ok(Routed::Read { tool: Tool::TranslateStatus, id: Some(id) })
            }
            (Method::Get, _) => Err(RouteMiss::NotFound),
            (_, Ok(_)) => Err(RouteMiss::MethodNotAllowed),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "meeting", "system_audio"] => match method {
            Method::Get => Ok(Routed::Read { tool: Tool::MeetingSystemAudioStatus, id: None }),
            _ => Err(RouteMiss::MethodNotAllowed),
        },
        ["v1", "meeting", "system_audio", "retry"] => match method {
            Method::Post => Ok(Routed::Write { tool: Tool::MeetingSystemAudioRetry, level: Level::L2 }),
            _ => Err(RouteMiss::MethodNotAllowed),
        },
        ["v1", "meeting", "reminder"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::MeetingReminderGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MeetingReminderSet,
                level: Level::L1,
            }),
        },
        ["v1", "meeting", "microphone"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::MeetingMicrophoneGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MeetingMicrophoneSet,
                level: Level::L1,
            }),
        },
        ["v1", "meeting", "recap_prefs"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::MeetingRecapPrefsGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MeetingRecapPrefsSet,
                level: Level::L1,
            }),
        },
        ["v1", "meeting", "speakers"] => match method {
            // No path id → the most recent meeting.
            Method::Get => Ok(Routed::Read {
                tool: Tool::MeetingSpeakersGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MeetingSpeakersSet,
                level: Level::L1,
            }),
        },
        ["v1", "meeting", "speakers", id] => match (method, id.parse::<i64>()) {
            // A non-positive id names no meeting (issue #779), as on the recap reads.
            (Method::Get, Ok(id)) if id > 0 => Ok(Routed::Read {
                tool: Tool::MeetingSpeakersGet,
                id: Some(id),
            }),
            (Method::Get, Ok(_)) => Err(RouteMiss::NotFound),
            (Method::Post, _) => Err(RouteMiss::MethodNotAllowed),
            (_, Err(_)) => Err(RouteMiss::NotFound),
        },
        ["v1", "meeting", "templates"] => match method {
            // No path id → the most recent meeting's selection.
            Method::Get => Ok(Routed::Read {
                tool: Tool::MeetingTemplatesGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MeetingTemplatesSet,
                level: Level::L1,
            }),
        },
        ["v1", "meeting", "templates", id] => match (method, id.parse::<i64>()) {
            // A non-positive id names no meeting (issue #779), as on the recap reads.
            (Method::Get, Ok(id)) if id > 0 => Ok(Routed::Read {
                tool: Tool::MeetingTemplatesGet,
                id: Some(id),
            }),
            (Method::Get, Ok(_)) => Err(RouteMiss::NotFound),
            (Method::Post, _) => Err(RouteMiss::MethodNotAllowed),
            (_, Err(_)) => Err(RouteMiss::NotFound),
        },
        // Issue #251: the user's skill files (`?q=<name>` adds its instructions), asking for a
        // run, and one run's status/output.
        ["v1", "skills"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::SkillsList,
                id: None,
            }),
            // Issue #251: save a new skill file from Markdown.
            Method::Post => Ok(Routed::Write {
                tool: Tool::SkillsSave,
                level: Level::L1,
            }),
        },
        ["v1", "skills", "run"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::SkillsRun,
                level: Level::L1,
            },
        ),
        ["v1", "skills", "run", id] => match (method, id.parse::<i64>()) {
            (Method::Get, Ok(id)) if id > 0 => Ok(Routed::Read {
                tool: Tool::SkillsRunStatus,
                id: Some(id),
            }),
            (Method::Post, _) => Err(RouteMiss::MethodNotAllowed),
            _ => Err(RouteMiss::NotFound),
        },
        // Issue #555: public web reads. `?url=` names the link; the backend fetches it over the
        // traced transport and returns the text as JSON. Reads, Pro, behind the Web fetch consent.
        ["v1", "web", "video_transcript"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::WebVideoTranscript,
                id: None,
            },
        ),
        ["v1", "web", "social_post"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::WebSocialPost,
                id: None,
            },
        ),
        ["v1", "web", "page"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::WebPage,
                id: None,
            },
        ),
        // Issue #108: this Mac's support reports and their status.
        ["v1", "support", "reports"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::SupportReportsList,
                id: None,
            },
        ),
        // Issue #252: schedules — list / add on the collection, remove by id in the body.
        ["v1", "schedules"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::SchedulesList,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::SchedulesAdd,
                level: Level::L1,
            }),
        },
        ["v1", "schedules", "remove"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::SchedulesRemove,
                level: Level::L1,
            },
        ),
        // Issue #397: file exports into the user's export folders. POST asks for one (L2 —
        // pending until the user confirms in the desktop); GET lists them / one by id.
        ["v1", "export", "file"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::ExportWriteFile,
                level: Level::L2,
            },
        ),
        ["v1", "export", "writes"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::ExportWritesList,
                id: None,
            },
        ),
        ["v1", "export", "writes", id] => match (method, id.parse::<i64>()) {
            (Method::Get, Ok(id)) if id > 0 => Ok(Routed::Read {
                tool: Tool::ExportWritesList,
                id: Some(id),
            }),
            (Method::Post, _) => Err(RouteMiss::MethodNotAllowed),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "export", "folders"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::ExportFoldersList,
                id: None,
            },
        ),
        ["v1", "export", "folders", "remove"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::ExportFoldersRemove,
                level: Level::L1,
            },
        ),
        // FR-MT-17: confirm one Recap next action as a commitment (L1).
        ["v1", "meeting", "recap", "track"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MeetingRecapTrack,
                level: Level::L1,
            },
        ),
        // `?q=transcript` includes the transcript in the document.
        ["v1", "meeting", "recap", "export"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MeetingRecapExport,
                id: None,
            },
        ),
        ["v1", "meeting", "recap", "export", id] => match id.parse::<i64>() {
            Ok(parsed) => method_is(
                method,
                Method::Get,
                Routed::Read {
                    tool: Tool::MeetingRecapExport,
                    id: Some(parsed),
                },
            ),
            Err(_) => Err(RouteMiss::NotFound),
        },
        // FR-MT-22: the structured Recap and the transcript, as reads. The `export` arm above
        // matches first, so a numeric segment here can only be a session id.
        ["v1", "meeting", "search"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MeetingSearch,
                id: None,
            },
        ),
        ["v1", "meeting", id, "delete"] => match id.parse::<i64>() {
            Ok(parsed) if parsed > 0 => method_is(
                method,
                Method::Post,
                Routed::Write {
                    tool: Tool::MeetingDelete,
                    level: Level::L1,
                },
            ),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "meeting", "recap", "edit"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MeetingRecapEdit,
                level: Level::L1,
            },
        ),
        ["v1", "meeting", "recap"] => method_is(method, Method::Get, Routed::Read { tool: Tool::MeetingRecapGet, id: None }),
        ["v1", "meeting", "recap", id] => match id.parse::<i64>() {
            Ok(parsed) if parsed > 0 => method_is(method, Method::Get, Routed::Read { tool: Tool::MeetingRecapGet, id: Some(parsed) }),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "meeting", "transcript"] => method_is(method, Method::Get, Routed::Read { tool: Tool::MeetingTranscriptGet, id: None }),
        ["v1", "meeting", "transcript", id] => match id.parse::<i64>() {
            Ok(parsed) if parsed > 0 => method_is(method, Method::Get, Routed::Read { tool: Tool::MeetingTranscriptGet, id: Some(parsed) }),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "voice", "microphone"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::VoiceMicrophoneGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::VoiceMicrophoneSet,
                level: Level::L1,
            }),
        },
        ["v1", "device", "onboarding"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::DeviceOnboardingGet,
                id: None,
            },
        ),
        // #148 P2: the account, read-only (sign-in is UI-only).
        ["v1", "device", "account"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::DeviceAccountGet,
                id: None,
            },
        ),
        ["v1", "device", "release_notes"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::DeviceReleaseNotesGet,
                id: None,
            },
        ),
        // Issue #1296: read-only; install / restart stay in the app.
        ["v1", "device", "update"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::DeviceUpdateGet,
                id: None,
            },
        ),
        // Issue #285: the nightly Dream Cycle status and its morning reason.
        ["v1", "dream", "status"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::DreamStatus,
                id: None,
            },
        ),
        ["v1", "capture_exclusions"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::CaptureExclusionsGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::CaptureExclusionsSet,
                level: Level::L3,
            }),
        },
        ["v1", "visual_recall", "status"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::VisualRecallStatus,
                id: None,
            },
        ),
        ["v1", "visual_recall", "enabled"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::VisualRecallSetEnabled,
                level: Level::L3,
            },
        ),
        ["v1", "visual_recall", "retention"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::VisualRecallSetRetention,
                level: Level::L3,
            },
        ),
        // Encrypted database snapshots (issue #647): the same setting as Settings › Privacy.
        ["v1", "backup", "status"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::BackupStatus,
                id: None,
            },
        ),
        ["v1", "backup", "retention"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::BackupSetRetention,
                level: Level::L1,
            },
        ),
        // "Compact search index" (#714 step 3): the Settings › Memory control. The desktop runs the
        // job; these record a start / stop for it and read its published status.
        ["v1", "search_index", "status"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::SearchIndexStatus,
                id: None,
            },
        ),
        ["v1", "storage", "preview"] => method_is(
            method,
            Method::Get,
            Routed::Read { tool: Tool::StoragePreview, id: None },
        ),
        ["v1", "dream", "retry"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::DreamRetry,
                level: Level::L1,
            },
        ),
        ["v1", "search_index", "compact", "start"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::SearchIndexCompactStart,
                level: Level::L1,
            },
        ),
        ["v1", "search_index", "compact", "stop"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::SearchIndexCompactStop,
                level: Level::L1,
            },
        ),
        ["v1", "visual_recall", "frames", "search"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::VisualRecallSearchFrames,
                id: None,
            },
        ),
        // The frame id stays in the path (`rest_render` reads it from there).
        ["v1", "visual_recall", "frames", id, "rescan"] => match id.parse::<i64>() {
            Ok(_) => method_is(
                method,
                Method::Post,
                Routed::Write {
                    tool: Tool::VisualRecallRescanFrame,
                    level: Level::L1,
                },
            ),
            Err(_) => Err(RouteMiss::NotFound),
        },
        ["v1", "visual_recall", "frames", "delete"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::VisualRecallDeleteFrame,
                level: Level::L1,
            },
        ),
        ["v1", "visual_recall", "frames", id, "text_lines"] => match id.parse::<i64>() {
            Ok(parsed) => method_is(
                method,
                Method::Get,
                Routed::Read {
                    tool: Tool::VisualRecallFrameTextLines,
                    id: Some(parsed),
                },
            ),
            Err(_) => Err(RouteMiss::NotFound),
        },
        ["v1", "visual_recall", "frames", id] => match id.parse::<i64>() {
            Ok(parsed) => method_is(
                method,
                Method::Get,
                Routed::Read {
                    tool: Tool::VisualRecallGetFrame,
                    id: Some(parsed),
                },
            ),
            Err(_) => Err(RouteMiss::NotFound),
        },
        // Lessons (L5; invariant 6 symmetry with the Learned UI).
        ["v1", "patterns"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::PatternsList,
                id: None,
            },
        ),
        ["v1", "patterns", "active"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::PatternsSetActive,
                level: Level::L1,
            },
        ),
        ["v1", "lessons"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::LessonsList,
                id: None,
            },
        ),
        ["v1", "lessons", "active"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::LessonsSetActive,
                level: Level::L1,
            },
        ),
        ["v1", "memory", "user-notes"] => method_is(method, Method::Get, Routed::Read { tool: Tool::MemoryNotesList, id: None }),
        ["v1", "memory", "user-notes", "change"] => method_is(method, Method::Post, Routed::Write { tool: Tool::MemoryNotesChange, level: Level::L1 }),
        ["v1", "memory", "user-notes", id] => method_is(method, Method::Get, Routed::Read { tool: Tool::MemoryNotesList, id: Some(id.parse::<i64>().ok().filter(|id| *id > 0).ok_or(RouteMiss::NotFound)?) }),
        ["v1", "memory", "notes"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MemoryAppendNote,
                level: Level::L1,
            },
        ),
        ["v1", "state", "proposals"] => Err(RouteMiss::NotFound),
        // Contradiction flags (issue #253) — the Context Health pane's list and its decision.
        // Matched before the generic `state/<noun>` arms: disputes are not a get-by-id noun.
        ["v1", "state", "disputes"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::StateDisputesList,
                id: None,
            },
        ),
        ["v1", "state", "disputes", "resolve"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::StateDisputesResolve,
                level: Level::L1,
            },
        ),
        // Export format v1 (E7, plan §7.5): the `state.export` arguments as a JSON body.
        ["v1", "export"] => method_is(
            method,
            Method::Post,
            Routed::Read {
                tool: Tool::StateExport,
                id: None,
            },
        ),
        // World model correction + export (issue #290). Also matched before `state/<noun>`.
        ["v1", "state", "export"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::StateExport,
                id: None,
            },
        ),
        ["v1", "state", "update"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::StateUpdate,
                level: Level::L1,
            },
        ),
        ["v1", "state", "delete"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::StateDelete,
                level: Level::L1,
            },
        ),
        ["v1", "state", "create"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::StateCreate,
                level: Level::L1,
            },
        ),
        // Split a wrong merge (FR-ST-10) and the evidence it chooses from.
        ["v1", "state", "split"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::StateSplit,
                level: Level::L1,
            },
        ),
        // Merge duplicates the user confirmed, or keep two apart (issue #1362); the candidate list.
        ["v1", "state", "merge"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::StateMerge,
                level: Level::L1,
            },
        ),
        ["v1", "state", "keep-separate"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::StateKeepSeparate,
                level: Level::L1,
            },
        ),
        ["v1", "state", "people" | "projects", "merge-candidates"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::StateMergeCandidates,
                id: None,
            },
        ),
        ["v1", "state", "people" | "projects", id, "evidence"] => match id.parse::<i64>() {
            Ok(id) if id > 0 => method_is(
                method,
                Method::Get,
                Routed::Read {
                    tool: Tool::StateSplitEvidence,
                    id: Some(id),
                },
            ),
            _ => Err(RouteMiss::NotFound),
        },
        // Mail triage (issue #319, FR-TG-30). Thread keys (`gmail:<id>`) and label keys
        // (`custom:<slug>`) are plain path segments — a colon is a legal path character.
        ["v1", "mail", "triage"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MailTriageList,
                id: None,
            },
        ),
        ["v1", "mail", "triage", key] if !key.is_empty() => match method {
            // The key is read from the path by `mail_path_key` (it is not an integer id).
            Method::Get => Ok(Routed::Read {
                tool: Tool::MailTriageGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MailTriageSet,
                level: Level::L1,
            }),
        },
        ["v1", "mail", "triage", key, "archive"] if !key.is_empty() => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MailTriageArchive,
                level: Level::L2,
            },
        ),
        ["v1", "mail", "triage", key, "unarchive"] if !key.is_empty() => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MailTriageUnarchive,
                level: Level::L2,
            },
        ),
        ["v1", "mail", "labels"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::MailLabelsList,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MailLabelsUpsert,
                level: Level::L1,
            }),
        },
        ["v1", "mail", "labels", key, "delete"] if !key.is_empty() => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::MailLabelsDelete,
                level: Level::L1,
            },
        ),
        ["v1", "mail", "sender_rules"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::MailSenderRulesList,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MailSenderRulesAdd,
                level: Level::L1,
            }),
        },
        ["v1", "mail", "sender_rules", id, "delete"] => match id.parse::<i64>() {
            Ok(_) => method_is(
                method,
                Method::Post,
                Routed::Write {
                    tool: Tool::MailSenderRulesDelete,
                    level: Level::L1,
                },
            ),
            Err(_) => Err(RouteMiss::NotFound),
        },
        ["v1", "mail", "history"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::MailHistoryList,
                id: None,
            },
        ),
        // The master switch. GET reads it, POST sets it — the same shape as `/v1/mail/labels`,
        // so the pair does not need its own verb in the path.
        ["v1", "mail", "settings"] => match method {
            Method::Get => Ok(Routed::Read {
                tool: Tool::MailSettingsGet,
                id: None,
            }),
            Method::Post => Ok(Routed::Write {
                tool: Tool::MailSettingsSet,
                level: Level::L1,
            }),
        },
        ["v1", "actions", "execute"] => method_is(method, Method::Post, Routed::Action),
        ["v1", "actions", "status", id] => match id.parse::<u64>() {
            Ok(id) if id > 0 => method_is(method, Method::Get, Routed::ApprovalStatus { id }),
            _ => Err(RouteMiss::NotFound),
        },
        // Issue #394: the Clarify questions agent runs are waiting on, one by id, and the answer
        // (L1 — it grants nothing). Question ids are not approval ids.
        ["v1", "actions", "questions"] => method_is(
            method,
            Method::Get,
            Routed::Read {
                tool: Tool::ActionsQuestions,
                id: None,
            },
        ),
        ["v1", "actions", "questions", id] => match (method, id.parse::<i64>()) {
            (Method::Get, Ok(id)) if id > 0 => Ok(Routed::Read {
                tool: Tool::ActionsQuestions,
                id: Some(id),
            }),
            (Method::Post, _) => Err(RouteMiss::MethodNotAllowed),
            _ => Err(RouteMiss::NotFound),
        },
        ["v1", "actions", "answer"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::ActionsAnswer,
                level: Level::L1,
            },
        ),
        // Issue #395: one more instruction for a run that is still going (L1 — it grants
        // nothing; the run reads it at its next turn).
        ["v1", "actions", "steer"] => method_is(
            method,
            Method::Post,
            Routed::Write {
                tool: Tool::ActionsSteer,
                level: Level::L1,
            },
        ),
        // state list: /v1/state/<noun>
        ["v1", "state", noun] => match state_tool(noun, false) {
            Some(tool) => method_is(method, Method::Get, Routed::Read { tool, id: None }),
            None => Err(RouteMiss::NotFound),
        },
        // state get: /v1/state/<noun>/<id>
        ["v1", "state", noun, id] => match (state_tool(noun, true), id.parse::<i64>()) {
            (Some(tool), Ok(parsed)) => method_is(
                method,
                Method::Get,
                Routed::Read {
                    tool,
                    id: Some(parsed),
                },
            ),
            _ => Err(RouteMiss::NotFound),
        },
        _ => Err(RouteMiss::NotFound),
    }
}

pub(super) enum RouteMiss {
    NotFound,
    MethodNotAllowed,
}

/// The string key a mail get-by-key / write-by-key path carries — the thread key of
/// `/v1/mail/triage/{thread_key}[/archive|/unarchive]`, the label key of
/// `/v1/mail/labels/{key}/delete`, the rule id of `/v1/mail/sender_rules/{id}/delete`. `None`
/// for every other tool / path: list endpoints have no key, and no other tool reads one.
pub(super) fn mail_path_key(tool: Tool, path: &str) -> Option<String> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let key = match (tool, segs.as_slice()) {
        (Tool::MailTriageGet | Tool::MailTriageSet, ["v1", "mail", "triage", key]) => key,
        (
            Tool::MailTriageArchive | Tool::MailTriageUnarchive,
            ["v1", "mail", "triage", key, "archive" | "unarchive"],
        ) => key,
        (Tool::MailLabelsDelete, ["v1", "mail", "labels", key, "delete"]) => key,
        (Tool::MailSenderRulesDelete, ["v1", "mail", "sender_rules", id, "delete"]) => id,
        // Not mail, but the same "a path segment the backend needs" shape: the state table.
        (Tool::StateSplitEvidence, ["v1", "state", table @ ("people" | "projects"), _, "evidence"]) => table,
        (Tool::StateMergeCandidates, ["v1", "state", table @ ("people" | "projects"), "merge-candidates"]) => table,
        // Not mail, but the same shape: the meeting id rides in the path and the backend wants
        // it in the body as `session_id` (D6).
        (Tool::MeetingDelete, ["v1", "meeting", id, "delete"]) => id,
        _ => return None,
    };
    // HTTP clients percent-encode a path segment, so a label named in Japanese arrives as
    // `custom:%E4%BC%9A…`. Decode it here, after the route split, so REST hands the backend the
    // same key MCP does (issue #779).
    (!key.is_empty()).then(|| decode_path_segment(key))
}

/// RFC 3986 percent-decoding of one path segment: `%XX` becomes the byte, and everything else
/// (including `+`, which is literal in a path) is kept. A segment whose decoded bytes are not
/// UTF-8 is returned unchanged, so the backend refuses it with its own reason.
pub(super) fn decode_path_segment(segment: &str) -> String {
    if !segment.contains('%') {
        return segment.to_string();
    }
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| segment.to_string())
}

/// The body a mail write hands the backend: the request body (an empty body is `{}`) with the
/// path's key folded in — `thread_key` for the triage writes, `key` for a label delete, `id` for
/// a sender-rule delete — so the backend sees the same JSON shape the MCP face sends it
/// (invariant 6). `None` when the tool carries no path key, or when the body is not a JSON
/// object (the backend then refuses the raw body with its own reason).
pub(crate) fn mail_write_body(tool: Tool, path: &str, body: &str) -> Option<String> {
    let key = mail_path_key(tool, path)?;
    let mut value: serde_json::Value = if body.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(body).ok()?
    };
    let object = value.as_object_mut()?;
    let (field, value_for_key) = match tool {
        Tool::MailSenderRulesDelete => ("id", serde_json::json!(key.parse::<i64>().ok()?)),
        Tool::MailLabelsDelete => ("key", serde_json::json!(key)),
        Tool::MeetingDelete => ("session_id", serde_json::json!(key.parse::<i64>().ok()?)),
        _ => ("thread_key", serde_json::json!(key)),
    };
    object.insert(field.to_string(), value_for_key);
    Some(value.to_string())
}

fn method_is(actual: Method, expected: Method, ok: Routed) -> Result<Routed, RouteMiss> {
    if actual == expected {
        Ok(ok)
    } else {
        Err(RouteMiss::MethodNotAllowed)
    }
}

#[cfg(test)]
mod dashboard_tests {
    use super::*;

    /// The level is written twice — once here per route, once in `memory_api::tool_level` — and
    /// nothing made the two agree. `writing_voice.set` drifted apart exactly that way: L1 on both
    /// faces where the boundary it removes is the one `capture_exclusions.set` is L3 for. Pin the
    /// writes that open or close a privacy boundary, so a future one cannot be cheaper over REST.
    #[test]
    fn privacy_boundary_writes_carry_the_same_level_as_the_tool_table() {
        for (path, tool) in [
            ("/v1/writing_voice", Tool::WritingVoiceSet),
            ("/v1/capture_exclusions", Tool::CaptureExclusionsSet),
            ("/v1/visual_recall/enabled", Tool::VisualRecallSetEnabled),
            ("/v1/visual_recall/retention", Tool::VisualRecallSetRetention),
            ("/v1/agent/source", Tool::AgentSourceSet),
            ("/v1/capture/paused", Tool::CaptureSetPaused),
        ] {
            let Ok(Routed::Write { tool: found, level }) = resolve(Method::Post, path) else {
                panic!("{path} must resolve to a write");
            };
            assert_eq!(found, tool, "{path} routes to the wrong tool");
            if tool == Tool::WritingVoiceSet {
                assert_eq!(level, Level::L3, "writing-style learning requires approval");
            }
            if tool == Tool::AgentSourceSet {
                assert_eq!(level, Level::L3, "a subscription opt-in requires approval");
            }
            if matches!(tool, Tool::VisualRecallSetEnabled | Tool::VisualRecallSetRetention) {
                assert_eq!(level, Level::L3, "turning Visual recall on or keeping screens longer requires approval");
            }
            if tool == Tool::CaptureSetPaused {
                assert_eq!(level, Level::L3, "ending the user's capture pause requires approval");
            }
            assert_eq!(
                crate::memory_api::ApiLevel::Write(level),
                crate::memory_api::tool_level(tool),
                "{path} declares a level the tool table disagrees with"
            );
        }
    }

    #[test]
    fn connections_routes_carry_the_tool_table_levels() {
        // Issue #82: the same L1 on REST as in the tool table, and only these methods.
        for (path, tool) in [
            ("/v1/connections/disconnect", Tool::ConnectionsDisconnect),
            ("/v1/connections/composio", Tool::ConnectionsComposioSet),
            ("/v1/connections/gmail", Tool::ConnectionsGmailSet),
        ] {
            let Ok(Routed::Write { tool: found, level }) = resolve(Method::Post, path) else {
                panic!("{path} must resolve to a write");
            };
            assert_eq!(found, tool);
            assert_eq!(level, Level::L1);
            assert_eq!(crate::memory_api::ApiLevel::Write(level), crate::memory_api::tool_level(tool));
            assert!(matches!(resolve(Method::Get, path), Err(RouteMiss::MethodNotAllowed)));
        }
        assert!(matches!(
            resolve(Method::Get, "/v1/connections"),
            Ok(Routed::Read { tool: Tool::ConnectionsList, id: None })
        ));
        assert!(matches!(resolve(Method::Post, "/v1/connections"), Err(RouteMiss::MethodNotAllowed)));
        // Connecting is not an API route at all.
        assert!(resolve(Method::Post, "/v1/connections/connect").is_err());
    }

    #[test]
    fn the_steer_route_carries_the_tool_table_level() {
        let Ok(Routed::Write { tool, level }) = resolve(Method::Post, "/v1/actions/steer") else {
            panic!("the steer route must resolve to a write");
        };
        assert_eq!(tool, Tool::ActionsSteer);
        assert_eq!(crate::memory_api::ApiLevel::Write(level), crate::memory_api::tool_level(tool));
        assert!(matches!(resolve(Method::Get, "/v1/actions/steer"), Err(RouteMiss::MethodNotAllowed)));
    }

    #[test]
    fn clarify_routes_carry_the_tool_table_levels() {
        // Issue #394: the answer is L1 on REST exactly as in the tool table; listing is a read.
        let Ok(Routed::Write { tool, level }) = resolve(Method::Post, "/v1/actions/answer") else {
            panic!("the answer route must resolve to a write");
        };
        assert_eq!(tool, Tool::ActionsAnswer);
        assert_eq!(crate::memory_api::ApiLevel::Write(level), crate::memory_api::tool_level(tool));
        assert!(matches!(
            resolve(Method::Get, "/v1/actions/questions"),
            Ok(Routed::Read { tool: Tool::ActionsQuestions, id: None })
        ));
        assert!(matches!(
            resolve(Method::Get, "/v1/actions/questions/4"),
            Ok(Routed::Read { tool: Tool::ActionsQuestions, id: Some(4) })
        ));
        assert!(matches!(resolve(Method::Get, "/v1/actions/questions/0"), Err(RouteMiss::NotFound)));
        assert!(matches!(resolve(Method::Get, "/v1/actions/answer"), Err(RouteMiss::MethodNotAllowed)));
        // The L3 poller keeps its own route and id space.
        assert!(matches!(resolve(Method::Get, "/v1/actions/status/4"), Ok(Routed::ApprovalStatus { id: 4 })));
    }

    #[test]
    fn export_routes_carry_the_tool_table_levels() {
        // Issue #397: the file export is L2 on REST exactly as in the tool table.
        for (path, tool) in [
            ("/v1/export/file", Tool::ExportWriteFile),
            ("/v1/export/folders/remove", Tool::ExportFoldersRemove),
            ("/v1/api/connections/restrict", Tool::ApiConnectionsRestrict),
        ] {
            let Ok(Routed::Write { tool: found, level }) = resolve(Method::Post, path) else {
                panic!("{path} must resolve to a write");
            };
            assert_eq!(found, tool);
            assert_eq!(crate::memory_api::ApiLevel::Write(level), crate::memory_api::tool_level(tool));
        }
        assert!(matches!(
            resolve(Method::Get, "/v1/agent/source"),
            Ok(Routed::Read { tool: Tool::AgentSourceGet, id: None })
        ));
        assert!(matches!(
            resolve(Method::Get, "/v1/export/writes/3"),
            Ok(Routed::Read { tool: Tool::ExportWritesList, id: Some(3) })
        ));
        assert!(matches!(
            resolve(Method::Get, "/v1/export/folders"),
            Ok(Routed::Read { tool: Tool::ExportFoldersList, id: None })
        ));
        assert!(matches!(resolve(Method::Get, "/v1/export/file"), Err(RouteMiss::MethodNotAllowed)));
    }

    #[test]
    fn dashboard_routes_are_reads_and_keep_the_archive_cursor() {
        for (path, tool, id) in [
            ("/v1/device/release_notes", Tool::DeviceReleaseNotesGet, None),
            ("/v1/device/update", Tool::DeviceUpdateGet, None),
            ("/v1/device/account", Tool::DeviceAccountGet, None),
            ("/v1/dream/status", Tool::DreamStatus, None),
            ("/v1/wiki/evidence", Tool::WikiEvidence, None),
            ("/v1/wiki/list", Tool::WikiList, None),
            ("/v1/wiki/neighbors", Tool::WikiNeighbors, None),
            ("/v1/meeting/list", Tool::MeetingList, None),
            ("/v1/meeting/list/42", Tool::MeetingList, Some(42)),
            ("/v1/meeting/recap", Tool::MeetingRecapGet, None),
            ("/v1/meeting/recap/42", Tool::MeetingRecapGet, Some(42)),
            ("/v1/meeting/recap/export", Tool::MeetingRecapExport, None),
            ("/v1/meeting/transcript", Tool::MeetingTranscriptGet, None),
            ("/v1/meeting/transcript/42", Tool::MeetingTranscriptGet, Some(42)),
        ] {
            assert!(matches!(resolve(Method::Get, path), Ok(Routed::Read { tool: found, id: cursor }) if found == tool && cursor == id));
            assert!(matches!(resolve(Method::Post, path), Err(RouteMiss::MethodNotAllowed)));
        }
        assert!(resolve(Method::Get, "/v1/meeting/list/0").is_err());
        assert!(resolve(Method::Get, "/v1/meeting/recap/0").is_err());
        assert!(resolve(Method::Get, "/v1/meeting/transcript/x").is_err());
    }

    #[test]
    fn capture_control_routes_keep_their_method_and_permission_level() {
        assert!(matches!(
            resolve(Method::Get, "/v1/capture/status"),
            Ok(Routed::Read { tool: Tool::CaptureStatus, id: None })
        ));
        assert!(matches!(
            resolve(Method::Post, "/v1/capture/status"),
            Err(RouteMiss::MethodNotAllowed)
        ));
        assert!(matches!(
            resolve(Method::Post, "/v1/capture/paused"),
            // L3 ceiling: ending the user's pause waits for approval (#897); a pause runs at L1.
            Ok(Routed::Write { tool: Tool::CaptureSetPaused, level: Level::L3 })
        ));
        assert!(matches!(
            resolve(Method::Get, "/v1/capture/paused"),
            Err(RouteMiss::MethodNotAllowed)
        ));
    }
}
