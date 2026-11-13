//! Visual recall Memory API helpers — structured JSON responses (no confidence gate).

use crate::backend::{MemoryBackend, ReadParams};
use crate::memory_api::Tool;

/// Reads that return structured JSON, not [`crate::backend::ReadItem`] rows: the visual-recall
/// tools, and `lessons.list` (a lesson row is id + kind + scope + instruction + confidence +
/// evidence_count + active — richer than a label/confidence pair; the Low-band read gate does
/// not apply because the Learned list deliberately shows sleeping/weak rows too, exactly like
/// the human UI — invariant 6).
pub fn is_structured_read(tool: Tool) -> bool {
    if tool == Tool::MemoryNotesList { return true; }
    // Sessionized from the capture log (MCP v2 contract); the desktop's own panel keeps `read`.
    if tool == Tool::MemoryGetContext { return true; }
    matches!(
        tool,
        Tool::VisualRecallStatus
            // Snapshot retention (#647): the setting, its bounds and snapshot ages as JSON.
            | Tool::BackupStatus
            // The search-index compaction job and index shape (#714 step 3): counts as JSON.
            | Tool::SearchIndexStatus
            | Tool::StoragePreview
            | Tool::CaptureStatus
            | Tool::CaptureExclusionsGet
            | Tool::VisualRecallSearchFrames
            | Tool::VisualRecallGetFrame
            | Tool::VisualRecallFrameTextLines
            | Tool::LessonsList
            // The bundled release-notes catalogue (#67): the backend returns the same JSON the
            // What's new window renders (invariant 6).
            | Tool::DeviceReleaseNotesGet
            | Tool::DeviceOnboardingGet
            // The account snapshot plus the plan (#148 P2): structured JSON, no rows.
            | Tool::DeviceAccountGet
            // The nightly Dream Cycle status and its morning reason (issue #285).
            | Tool::DreamStatus
            | Tool::MemoryProcessingStatus
            // Contradiction flags (issue #253): a pair of commitments per row; the backend
            // applies the Low-band exclusion and the "possibly" cap per side.
            | Tool::StateDisputesList
            // The world-model export (issue #290): Markdown + rows; the backend applies the
            // Low-band exclusion and sets `possibly` per row.
            | Tool::StateExport
            // A row's evidence for a split (FR-ST-10): aliases + event metadata, never content.
            | Tool::StateSplitEvidence
            // Possible duplicates (#1362): pairs with ids, names and counts as JSON.
            | Tool::StateMergeCandidates
            | Tool::MemoryGetWrap
            // The schedule (Today's "Coming up"): events as JSON, no confidence rows.
            | Tool::CalendarUpcoming
            | Tool::MemoryGetPeriods
            | Tool::ProfileWhoami
            | Tool::MeetingSpeakersGet
            | Tool::MeetingTemplatesGet
            | Tool::MeetingRecapExport
            | Tool::MeetingRecapGet
            | Tool::MeetingTranscriptGet
            | Tool::MeetingList
            // The Meetings list's search box (D6): query-relevant hits with snippets.
            | Tool::MeetingSearch
            // Public web reads (issue #555): the backend fetches and returns the text as JSON.
            | Tool::WebVideoTranscript
            | Tool::WebSocialPost
            | Tool::WebPage
            // Review surfaces: the backend applies include_low and preserves uncertainty.
            | Tool::WikiEvidence
            | Tool::WikiNeighbors
            | Tool::WikiList
            | Tool::PatternsList
            | Tool::WritingVoiceGet
            | Tool::DictionaryAutoAddGet
            // The Agent lane's credential source (issue #110): presence booleans only.
            | Tool::AgentSourceGet
            // The Connections screen's rows and the Mail policy (issue #82): states and booleans.
            | Tool::ConnectionsList
            // Mail triage rows carry their own confidence / possibly / stage per row (FR-TG-30,
            // FR-API-06): the backend applies the Low-band exclusion, the face renders as-is.
            | Tool::MailTriageList
            | Tool::MailTriageGet
            | Tool::MailLabelsList
            | Tool::MailSenderRulesList
            | Tool::MailHistoryList
            | Tool::MailSettingsGet
            // Skills, schedules and support reports (#251 / #252 / #108): the backend answers
            // these in `read_structured`; without them here MCP and REST returned an empty list.
            | Tool::SkillsList
            | Tool::SkillsRunStatus
            | Tool::SchedulesList
            | Tool::SupportReportsList
            // Issue #397: the export ledger and the allowed folders.
            | Tool::ExportWritesList
            | Tool::ExportFoldersList
    )
}

/// `visual_recall.rescan_frame`: re-run OCR on one stored frame. Its answer is the new text, so
/// the backend answers it on its structured-JSON channel and both faces render the same
/// envelope. Returns the raw backend JSON; `None` id is `missing_frame_id`.
pub fn rescan_frame<B: MemoryBackend + ?Sized>(backend: &B, id: Option<i64>) -> String {
    let Some(id) = id else {
        return r#"{"error":"missing_frame_id"}"#.to_string();
    };
    let params = ReadParams {
        id: Some(id),
        ..ReadParams::default()
    };
    backend
        .read_structured(Tool::VisualRecallRescanFrame, &params)
        .unwrap_or_else(|| r#"{"error":"unavailable"}"#.to_string())
}

/// Wrap a JSON object/array as the standard tool response envelope.
pub fn render_structured(tool: Tool, json: &str) -> String {
    let result = serde_json::from_str::<serde_json::Value>(json)
        .unwrap_or_else(|_| serde_json::json!({ "error": "invalid_backend_response" }));
    // These answer flat, `{"tool", ...fields}`, not wrapped in `result`.
    if matches!(tool, Tool::MemoryGetContext | Tool::MemoryGetEvents | Tool::MemoryReadSession | Tool::MemoryProcessingStatus) {
        if let serde_json::Value::Object(mut fields) = result {
            fields.insert("tool".into(), tool.wire_name().into());
            return serde_json::Value::Object(fields).to_string();
        }
    }
    serde_json::json!({
        "tool": tool.wire_name(),
        "result": result,
    })
    .to_string()
}
