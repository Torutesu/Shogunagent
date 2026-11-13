//! Meetings and screen recall for connections SHOGUN made on its own (owner decision 2026-10-20,
//! audit F4 mitigation).
//!
//! Since 2026-10-14 the app connects every AI app it finds with `read` access and no click
//! (`client_install::auto_connect_plan`). Any program running as the user can start the bundled
//! `shogun-mcp --client <id>` and read what that connection allows, and the Keychain trusts that
//! binary, so nothing asks. A connection the app made by itself therefore does not reach the two
//! most sensitive sources until the user allows them in the app:
//!
//! - **meeting notes**: transcripts, minutes, speakers and the meeting search rows;
//! - **Visual recall**: the stored screen frames and the OCR text kept in memory (`screen_text`).
//!
//! The marker is the connection's [`SensitiveSources`] (`memory_api_settings::ClientGrant`). This
//! module holds the rest of the rule, once for every face:
//!
//! - [`tool_handling`] sorts every tool (an exhaustive match, so a new tool does not compile until
//!   someone decides): tools that only exist to read those sources, or that hand memory to a model
//!   on the desktop where nothing here can filter it, are [`ToolHandling::Refused`]
//!   (`source_not_allowed`); tools whose results mix them in with the rest are
//!   [`ToolHandling::Filtered`]; everything else is untouched.
//! - The face runs a call under the session's access ([`with_access`]); the data layer
//!   (`shogun-core`'s `DbBackend` and the `Db` reads behind it) asks [`current`] and leaves out
//!   every row whose event source [`is_sensitive_source`], before anything is returned.
//!
//! Outside a call made for such a connection (the desktop, REST and CLI tokens, tests) the access
//! is [`SensitiveSources::Allowed`], which changes nothing. Lifting the restriction is in-app only,
//! like widening a connection to `full`; an API face can only list it or narrow it
//! (`api.connections.restrict`).

use std::cell::Cell;

use crate::memory_api::Tool;

/// Whether a connection may read meeting notes and screen recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensitiveSources {
    /// Every source, as before. A connection the user made or widened, and every token.
    #[default]
    Allowed,
    /// Meeting notes and screen recall are left out until the user allows them in the app.
    Withheld,
}

impl SensitiveSources {
    /// The stored / wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            SensitiveSources::Allowed => "allowed",
            SensitiveSources::Withheld => "withheld",
        }
    }

    /// Parse a stored / wire name (exact, lower-case).
    pub fn parse(value: &str) -> Option<SensitiveSources> {
        [SensitiveSources::Allowed, SensitiveSources::Withheld]
            .into_iter()
            .find(|access| access.as_str() == value)
    }

    pub fn is_withheld(self) -> bool {
        self == SensitiveSources::Withheld
    }
}

/// The machine code every face returns for a tool a withheld connection may not call.
pub const SOURCE_NOT_ALLOWED: &str = "source_not_allowed";

/// The sentence that goes with [`SOURCE_NOT_ALLOWED`]: where the user allows it.
pub const SOURCE_NOT_ALLOWED_MESSAGE: &str = "ShogunAI connected this app automatically, so it \
can't read meeting notes or screen recall. To allow it, turn on \"Include meetings and screen \
recall\" for this app in ShogunAI Settings > Memory API.";

/// The JSON-RPC error code for [`SOURCE_NOT_ALLOWED`] on the MCP face, next to `-32005`.
pub const SOURCE_NOT_ALLOWED_CODE: i64 = -32006;

/// Event-log sources a withheld connection never sees: meeting transcripts (`meeting`), and the
/// screen text Visual recall keeps (`screen_text`, the older `screen_ocr`, and the retired manual
/// `user_screenshot`).
pub const SENSITIVE_EVENT_SOURCES: &[&str] = &["meeting", "screen_text", "screen_ocr", "user_screenshot"];

/// Whether an `event_log.source` is one a withheld connection never sees.
pub fn is_sensitive_source(source: &str) -> bool {
    SENSITIVE_EVENT_SOURCES.contains(&source)
}

/// The event-log sources the capture-log reads (`memory.get_context`, `memory.read_session`) fold
/// into sessions for `access`, as SQL. `+source` keeps the planner on the `ts` index.
pub fn capture_sources_sql(access: SensitiveSources) -> &'static str {
    match access {
        SensitiveSources::Allowed => "+source IN ('capture','screen_ocr','screen_text')",
        SensitiveSources::Withheld => "+source IN ('capture')",
    }
}

/// What a withheld connection gets from a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolHandling {
    /// Nothing from meetings or screen recall can be in the answer.
    Unaffected,
    /// The answer mixes them in with the rest: the data layer leaves them out.
    Filtered,
    /// The tool exists to read them, or hands memory to a model on the desktop where nothing here
    /// can filter it: refused with [`SOURCE_NOT_ALLOWED`].
    Refused,
}

/// How a withheld connection is answered for `tool`. Exhaustive on purpose.
pub fn tool_handling(tool: Tool) -> ToolHandling {
    use ToolHandling::{Filtered, Refused, Unaffected};
    match tool {
        // Meeting notes: every tool that reads or changes a meeting.
        Tool::MeetingList
        | Tool::MeetingRecapGet
        | Tool::MeetingTranscriptGet
        | Tool::MeetingSearch
        | Tool::MeetingSpeakersGet
        | Tool::MeetingSpeakersSet
        | Tool::MeetingTemplatesGet
        | Tool::MeetingTemplatesSet
        | Tool::MeetingRecapExport
        | Tool::MeetingRecapTrack
        | Tool::MeetingRecapEdit
        | Tool::MeetingDelete
        // Visual recall: the frames and their text.
        | Tool::VisualRecallSearchFrames
        | Tool::VisualRecallGetFrame
        | Tool::VisualRecallFrameTextLines
        | Tool::VisualRecallRescanFrame
        | Tool::VisualRecallDeleteFrame
        // Writes a meeting's Recap to a file.
        | Tool::ExportWriteFile
        // The desktop runs these on the whole memory with a model; their text cannot be filtered
        // here.
        | Tool::SkillsRun
        | Tool::SkillsRunStatus
        | Tool::SchedulesAdd
        | Tool::FormsPlan
        | Tool::ActionsExecute => Refused,
        // Results that mix every source: the data layer drops meeting and screen-text rows.
        Tool::MemorySearch
        | Tool::MemoryGetContext
        | Tool::MemoryGetContextPack
        // The budgeted recall is the context-pack assembly (E4-T3): same rows, same filter.
        | Tool::MemoryRecall
        | Tool::MemoryProcessingStatus
        // The handoff object is built from that same recall (E35): same rows, same filter.
        | Tool::HandoffCreate
        | Tool::MemoryGetEvents
        | Tool::MemoryReadSession
        | Tool::WikiEvidence => Filtered,
        // State rows, summaries, settings, mail and device status: no meeting or screen text.
        Tool::FormsInspect
        | Tool::FormsStatus
        | Tool::FormsApply
        | Tool::FormsClose
        | Tool::FormsUndo
        | Tool::MemoryNotesList
        | Tool::MemoryNotesChange
        | Tool::MemoryGetWrap
        | Tool::CalendarUpcoming
        | Tool::MemoryGetPeriods
        | Tool::StatePeopleList
        | Tool::StatePeopleGet
        | Tool::StateProjectsList
        | Tool::StateProjectsGet
        | Tool::StateCommitmentsList
        | Tool::StateCommitmentsGet
        | Tool::StateOpenLoopsList
        | Tool::StateOpenLoopsGet
        | Tool::StateDisputesList
        | Tool::StateDisputesResolve
        | Tool::StateUpdate
        | Tool::StateDelete
        | Tool::StateCreate
        | Tool::StateExport
        | Tool::StateSplit
        | Tool::StateSplitEvidence
        | Tool::StateMergeCandidates
        | Tool::StateMerge
        | Tool::StateKeepSeparate
        | Tool::MemoryImportPreview
        | Tool::MemoryImport
        | Tool::MemoryImportList
        | Tool::MemoryImportDelete
        | Tool::MemoryAppendNote
        // An agent's own events join the log; they read nothing (E4-T3).
        | Tool::MemoryIngest
        // A provider delivery joins the log like an ingest; it reads nothing (E44).
        | Tool::MemoryIngestWebhook
        // The `/memories` tree is state rows and the caller's scratch: no meeting or screen text.
        | Tool::MemoryFs
        // Accepting a handoff appends to the caller's scratch; it reads nothing (E35).
        | Tool::HandoffAccept
        // Payment mandates and spends (E46): labels, ids and amounts from the payment ledger —
        // no meeting or screen text.
        | Tool::PaymentsMandatesList
        | Tool::PaymentsMandatesRequest
        | Tool::PaymentsMandatesRevoke
        | Tool::PaymentsSpend
        // Governance reads (E15 / E18): redacted approval summaries, audit labels and grants —
        // no meeting or screen text.
        | Tool::ApprovalsList
        | Tool::ApprovalsGet
        | Tool::AuditList
        | Tool::AclList
        | Tool::ActionsStatus
        | Tool::ActionsQuestions
        | Tool::ActionsAnswer
        | Tool::ActionsSteer
        | Tool::DeviceOnboardingGet
        | Tool::DeviceAccountGet
        | Tool::DeviceReleaseNotesGet
        | Tool::DeviceUpdateGet
        | Tool::DreamStatus
        | Tool::DreamRetry
        | Tool::CaptureStatus
        | Tool::CaptureSetPaused
        | Tool::LessonsList
        | Tool::LessonsSetActive
        | Tool::VisualRecallStatus
        | Tool::VisualRecallSetEnabled
        | Tool::VisualRecallSetRetention
        | Tool::BackupStatus
        | Tool::BackupSetRetention
        | Tool::SearchIndexStatus
        | Tool::StoragePreview
        | Tool::SearchIndexCompactStart
        | Tool::SearchIndexCompactStop
        | Tool::CaptureExclusionsGet
        | Tool::CaptureExclusionsSet
        | Tool::ProfileWhoami
        | Tool::ProfileSet
        | Tool::DictionaryAutoAddGet
        | Tool::DictionaryAutoAddSet
        | Tool::VoiceDictionaryList
        | Tool::VoiceDictionaryCreate
        | Tool::VoiceDictionaryUpdate
        | Tool::VoiceDictionaryDelete
        | Tool::VoiceDictionaryImport
        | Tool::VoiceDictionaryExport
        | Tool::MeetingReminderGet
        | Tool::MeetingReminderSet
        | Tool::MeetingMicrophoneGet
        | Tool::MeetingMicrophoneSet
        | Tool::MeetingRecordNow
        | Tool::MeetingStartStatus
        | Tool::MeetingSystemAudioStatus
        | Tool::MeetingSystemAudioRetry
        | Tool::MeetingRecapPrefsGet
        | Tool::MeetingRecapPrefsSet
        | Tool::VoiceMicrophoneGet
        | Tool::VoiceMicrophoneSet
        | Tool::WritingVoiceGet
        | Tool::WritingVoiceSet
        | Tool::AgentSourceGet
        | Tool::AgentSourceSet
        | Tool::ConnectionsList
        | Tool::ConnectionsDisconnect
        | Tool::ConnectionsComposioSet
        | Tool::ConnectionsGmailSet
        | Tool::ApiTokensList
        | Tool::ApiConnectionsRestrict
        | Tool::TranslateRegisterGet
        | Tool::UiLanguageGet
        | Tool::UiLanguageSet
        | Tool::TranslateRegisterSet
        | Tool::TranslateText
        | Tool::TranslateReplyDraft
        | Tool::TranslateStatus
        | Tool::TranslatePairGet
        | Tool::TranslatePairSet
        | Tool::SkillsList
        | Tool::SkillsSave
        | Tool::SchedulesList
        | Tool::SchedulesRemove
        | Tool::SupportReportsList
        | Tool::WebVideoTranscript
        | Tool::WebSocialPost
        | Tool::WebPage
        | Tool::WikiNeighbors
        | Tool::WikiList
        | Tool::PatternsList
        | Tool::PatternsSetActive
        | Tool::MailTriageList
        | Tool::MailTriageGet
        | Tool::MailTriageSet
        | Tool::MailTriageArchive
        | Tool::MailTriageUnarchive
        | Tool::MailLabelsList
        | Tool::MailLabelsUpsert
        | Tool::MailLabelsDelete
        | Tool::MailSenderRulesList
        | Tool::MailSenderRulesAdd
        | Tool::MailSenderRulesDelete
        | Tool::MailHistoryList
        | Tool::MailSettingsGet
        | Tool::MailSettingsSet
        | Tool::ExportWritesList
        | Tool::ExportFoldersList
        | Tool::ExportFoldersRemove => Unaffected,
    }
}

/// `api.connections.restrict`'s body: `{"client": "<connection id>"}`, the connection to
/// withhold meeting notes and screen recall from. There is no way to ask for the opposite: a
/// `sensitive_sources` other than `withheld` is refused (allowing them is the user's, in the app).
pub fn restrict_request(body: &str) -> Result<String, &'static str> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid connection request")?;
    let object = value.as_object().ok_or("invalid connection request")?;
    match object.get("sensitive_sources") {
        None => {}
        Some(serde_json::Value::String(access)) if access == SensitiveSources::Withheld.as_str() => {}
        Some(_) => return Err(ALLOW_IN_APP_ONLY),
    }
    let client = object
        .get("client")
        .and_then(serde_json::Value::as_str)
        .filter(|client| {
            !client.is_empty()
                && client.len() <= 64
                && client.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .ok_or("invalid connection request")?;
    Ok(client.to_string())
}

/// The refusal for asking an API face to allow meeting notes and screen recall.
pub const ALLOW_IN_APP_ONLY: &str = "invalid sensitive_sources: only the user can allow meeting \
notes and screen recall, in ShogunAI Settings > Memory API";

/// Whether a session with `access` may call `tool` at all. The one predicate the faces apply,
/// after scope and before anything runs; `tools/list` hides what it refuses.
pub fn tool_allowed(access: SensitiveSources, tool: Tool) -> bool {
    !(access.is_withheld() && tool_handling(tool) == ToolHandling::Refused)
}

thread_local! {
    static CURRENT: Cell<SensitiveSources> = const { Cell::new(SensitiveSources::Allowed) };
}

/// The access of the call this thread is serving: [`SensitiveSources::Allowed`] unless a face set
/// it with [`with_access`]. The data layer asks this before returning rows.
pub fn current() -> SensitiveSources {
    CURRENT.with(Cell::get)
}

/// Whether the call this thread is serving may see a row from event source `source`.
pub fn current_allows_source(source: &str) -> bool {
    !(current().is_withheld() && is_sensitive_source(source))
}

/// Restores the previous access when dropped, so a panic inside the call cannot leave a later
/// call on this thread with the wrong one.
struct Restore(SensitiveSources);

impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.with(|cell| cell.set(self.0));
    }
}

/// Run `f` (one tool call) under `access`. Nested calls keep the narrower access.
pub fn with_access<T>(access: SensitiveSources, f: impl FnOnce() -> T) -> T {
    let previous = current();
    let next = if previous.is_withheld() { previous } else { access };
    CURRENT.with(|cell| cell.set(next));
    let _restore = Restore(previous);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_api::{scope_allows, TokenScope, ALL_TOOLS};

    #[test]
    fn meeting_and_visual_recall_tools_are_refused_only_when_withheld() {
        for tool in [
            Tool::MeetingList,
            Tool::MeetingRecapGet,
            Tool::MeetingTranscriptGet,
            Tool::MeetingSearch,
            Tool::MeetingSpeakersGet,
            Tool::MeetingRecapExport,
            Tool::VisualRecallSearchFrames,
            Tool::VisualRecallGetFrame,
            Tool::VisualRecallFrameTextLines,
        ] {
            assert!(!tool_allowed(SensitiveSources::Withheld, tool), "{}", tool.wire_name());
            assert!(tool_allowed(SensitiveSources::Allowed, tool), "{}", tool.wire_name());
        }
        // Settings mirrors that hold no meeting content stay callable.
        for tool in [
            Tool::MeetingReminderGet,
            Tool::MeetingMicrophoneGet,
            Tool::MeetingRecapPrefsGet,
            Tool::VisualRecallStatus,
            Tool::ApiTokensList,
        ] {
            assert!(tool_allowed(SensitiveSources::Withheld, tool), "{}", tool.wire_name());
        }
    }

    #[test]
    fn every_read_a_withheld_read_connection_can_call_is_classified() {
        // A withheld connection is `read` unless an API narrowed a `full` one. Every read it can
        // call either returns nothing sensitive, is filtered, or is refused: the match above is
        // exhaustive, so this pins the filtered set.
        let filtered: Vec<&str> = ALL_TOOLS
            .iter()
            .filter(|tool| scope_allows(TokenScope::Read, **tool))
            .filter(|tool| tool_handling(**tool) == ToolHandling::Filtered)
            .map(|tool| tool.wire_name())
            .collect();
        assert_eq!(
            filtered,
            [
                "memory.search",
                "memory.get_context",
                "memory.get_context_pack",
                "memory.get_events",
                "memory.read_session",
                "memory.recall",
                "memory.processing_status",
                "handoff.create",
                "wiki.evidence",
            ]
        );
    }

    #[test]
    fn sensitive_sources_are_meetings_and_screen_text() {
        for source in ["meeting", "screen_text", "screen_ocr", "user_screenshot"] {
            assert!(is_sensitive_source(source), "{source}");
        }
        for source in ["capture", "gmail", "slack", "note", "gcal", "imported:chatgpt"] {
            assert!(!is_sensitive_source(source), "{source}");
        }
        assert!(capture_sources_sql(SensitiveSources::Allowed).contains("screen_text"));
        assert!(!capture_sources_sql(SensitiveSources::Withheld).contains("screen"));
    }

    #[test]
    fn access_is_scoped_to_one_call_and_never_widened_inside_it() {
        assert_eq!(current(), SensitiveSources::Allowed);
        let inner = with_access(SensitiveSources::Withheld, || {
            assert!(!current_allows_source("meeting"));
            assert!(current_allows_source("capture"));
            // A nested call cannot widen the outer one.
            with_access(SensitiveSources::Allowed, current)
        });
        assert_eq!(inner, SensitiveSources::Withheld);
        assert_eq!(current(), SensitiveSources::Allowed);
        assert!(current_allows_source("meeting"));
        // Restored after a panic too.
        let caught = std::panic::catch_unwind(|| {
            with_access(SensitiveSources::Withheld, || panic!("boom"));
        });
        assert!(caught.is_err());
        assert_eq!(current(), SensitiveSources::Allowed);
    }

    #[test]
    fn the_api_request_can_only_withhold() {
        assert_eq!(restrict_request(r#"{"client":"cursor"}"#), Ok("cursor".to_string()));
        assert_eq!(
            restrict_request(r#"{"client":"codex-cli","sensitive_sources":"withheld"}"#),
            Ok("codex-cli".to_string())
        );
        // No API way to allow them.
        assert_eq!(restrict_request(r#"{"client":"cursor","sensitive_sources":"allowed"}"#), Err(ALLOW_IN_APP_ONLY));
        assert_eq!(restrict_request(r#"{"client":"cursor","sensitive_sources":true}"#), Err(ALLOW_IN_APP_ONLY));
        for bad in [r#"{}"#, r#"{"client":""}"#, r#"{"client":"Cursor App"}"#, "[]", "nope"] {
            assert_eq!(restrict_request(bad), Err("invalid connection request"), "{bad}");
        }
    }

    #[test]
    fn wire_names_round_trip() {
        for access in [SensitiveSources::Allowed, SensitiveSources::Withheld] {
            assert_eq!(SensitiveSources::parse(access.as_str()), Some(access));
            let json = serde_json::to_string(&access).unwrap_or_default();
            assert_eq!(json, format!("\"{}\"", access.as_str()));
        }
        assert_eq!(SensitiveSources::parse("Allowed"), None);
    }
}
