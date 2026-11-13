//! The Memory API surface (§6.11, FR-API-01..06) — the AI-facing access to SHOGUN's memory, kept
//! **completely symmetric with the human UI** (invariant 6). The three faces (MCP / CLI / REST) are
//! thin wrappers over the same internal API, so the policy lives here once:
//! - the v1 tool set and each tool's permission level (FR-API-02), reusing the same L1/L2/L3 as the
//!   UI (FR-API-04 — an API-driven L3 goes through the very same approval flow);
//! - token auth (FR-API-03): no token → everything denied, reads included;
//! - the read confidence rule (FR-API-06): <0.5 excluded by default, `possibly` flag on medium,
//!   low included only when explicitly requested — the same [`shogun_fusion::confidence`] bands the
//!   UI uses.

use sha2::{Digest, Sha256};
use shogun_agents::entitlement::Entitlements;
use shogun_agents::permission::Level;
use shogun_fusion::confidence::{band, Band};
use subtle::ConstantTimeEq;

/// The longest `memory.append_note` text, in chars (a user note; notes.change allows longer).
pub const MAX_APPEND_NOTE_CHARS: usize = 2_000;
/// The refusal for a longer note, on every face.
pub const APPEND_NOTE_TOO_LONG: &str = "text_too_long: notes are at most 2000 characters.";

/// The v1 Memory API tools (FR-API-02). Symmetric with the UI's corresponding features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Capture focused form fields for review (L2).
    FormsInspect,
    /// Match exact form facts with Jev: through SHOGUN's relay on the company TypeSafe key, or the
    /// user's own key when one is saved (issue #1144; L1; human review).
    FormsPlan,
    /// Read one live form review session.
    FormsStatus,
    /// Open pending human review, never approve or fill from the API (L3).
    FormsApply,
    /// Cancel and close a form review session (L1).
    FormsClose,
    /// Ask the human to undo the last fill (L3).
    FormsUndo,
    MemoryNotesList,
    MemoryNotesChange,
    MemorySearch,
    MemoryGetContext,
    MemoryGetContextPack,
    /// Open the source events behind a search hit or context-pack line, by event id (a read).
    MemoryGetEvents,
    /// What happened in one app / window over a span: capture rows merged into sessions (a read).
    MemoryReadSession,
    /// Today's Evening Wrap (issue #10, §6.17): the same deterministic aggregation the notch card
    /// shows — outcome counts, still-open, tomorrow, loose ends. A read; the confidence gate is
    /// applied by the assembler (fusion) exactly as for the card (invariant 6).
    MemoryGetWrap,
    /// The user's upcoming schedule from the synced calendar: the same events, window and cap
    /// as the Today page's "Coming up" (invariant 6). Calendar facts, never hedged; a read.
    CalendarUpcoming,
    MemoryGetPeriods,
    StatePeopleList,
    StatePeopleGet,
    StateProjectsList,
    StateProjectsGet,
    StateCommitmentsList,
    StateCommitmentsGet,
    StateOpenLoopsList,
    StateOpenLoopsGet,
    /// Open contradiction flags on state (issue #253): two commitments that read as the same
    /// promise with different due dates — the same pairs the Context Health pane lists. Both
    /// sides are passed as "possibly" (capped below the High band), never as fact.
    StateDisputesList,
    /// Settle one dispute — keep the earlier / the later reading, dismiss both, or keep both. The
    /// same local decision the Context Health pane offers (L1; the loser is cancelled, not deleted).
    StateDisputesResolve,
    /// Correct one state record (issue #290): name / summary / description / status / due, per
    /// table. The row is pinned at confidence 1.0 so no automatic pass overwrites it — the same
    /// edit the World model pane offers. Local, the user's own memory (L1).
    StateUpdate,
    /// Delete one state record outright (issue #290). Its source events stay; the same old events
    /// cannot re-create it. The World model pane's delete (L1, local).
    StateDelete,
    /// Add a commitment or open loop the user wrote (quick-add): pinned at confidence 1.0, no
    /// event provenance. The World model's "+ Add" (L1, local).
    StateCreate,
    /// The whole world model as a portable export: Markdown (one section per table, each row with
    /// confidence and provenance event ids) plus the same rows as JSON. A read (FR-API-06 applies).
    StateExport,
    /// Split a wrong entity merge (FR-ST-10, issue #290): move some of a person's / project's
    /// evidence (provenance events, and for people aliases) to a new row, and record that the two
    /// must not be fused again. The World model pane's Split (L1, local).
    StateSplit,
    /// What a person / project row rests on — its aliases and provenance-event metadata (id, time,
    /// source, kind; never content) — the list a split chooses from. A read.
    StateSplitEvidence,
    /// The pairs of people (or projects) the lexical rule relates that nothing merged (issue
    /// #1362): the Memory page's "Possible duplicates". A read; the person decides.
    StateMergeCandidates,
    /// Merge the records the user named into one (issue #1362): provenance, links and aliases
    /// move to the survivor; nothing is deleted from the event log. The inverse of `state.split`
    /// (L1, local).
    StateMerge,
    /// Record that two records are different people / projects: they are not suggested again and
    /// no automatic pass fuses them (the same block a split writes). L1, local.
    StateKeepSeparate,
    /// Validate a `shogun-memory-import/v1` package (or Computer History Markdown the user handed
    /// over) and propose an action per item — new / merge into an existing record / duplicate.
    /// Nothing is written (issues #384 / #391). A read.
    MemoryImportPreview,
    /// Commit the selected items of an import under a new import record (`imported:<platform>`
    /// provenance, confidence capped below High). Local write to the user's own memory (L1).
    MemoryImport,
    /// Past imports, or one import with its items. A read (FR-API-06 applies to the items).
    MemoryImportList,
    /// Undo one import: its record and every item it stored (L1, local).
    MemoryImportDelete,
    MemoryAppendNote,
    /// Append events or conversation turns an agent hands over to the event log (E4-T3 / E5-T1):
    /// the same sanitize / redact / dedup append path capture and integrations use, under an
    /// `idempotency_key` that replays on a repeat. Local write to the principal's own memory (L1).
    MemoryIngest,
    /// One provider webhook delivery (GitHub, Linear, Slack, Notion, Google Calendar, Gmail) as
    /// text events with provenance through the `memory.ingest` path (E44, `webhook_ingest`); a
    /// delivery that carries only ids is acknowledged as changed and stores nothing. L1 like
    /// `memory.ingest`.
    MemoryIngestWebhook,
    /// A budgeted read of what memory holds on a question (E4-T3 / E5-T3): the context-pack
    /// assembly (facts + dated evidence) cut to `budget_tokens`, High facts as facts and Medium
    /// ones prefixed `possibly:`. A read; free (invariant 10).
    MemoryRecall,
    /// Read processing progress for one source event without starting model work.
    MemoryProcessingStatus,
    /// `/memories` as files (E6-T2): Anthropic's `memory_20250818` operations over the world-model
    /// projection and the caller's scratch files. Writes to the caller's scratch edit it; writes
    /// elsewhere become `memory.ingest` events. Local, L1.
    MemoryFs,
    /// The A2A state-of-work handoff for a task (E35): one budgeted recall mapped onto the typed
    /// `shogunagent-handoff/v1` object (people, projects, commitments, open loops with band and
    /// provenance). A read; free (invariant 10).
    HandoffCreate,
    /// Accept a received handoff into the caller's own `agent_scratch` namespace through the
    /// `memory.ingest` path, under an idempotency key derived from the handoff id (E35). Local,
    /// never the principal's world: L1 like `memory.ingest`.
    HandoffAccept,
    /// Payment mandates (E46): the mandates a human granted this principal's agents, with their
    /// bounds, what has been spent against each and their status. A read; free (invariant 10).
    PaymentsMandatesList,
    /// Ask a human for a payment mandate, or to widen / extend one: queued as an L3 approval
    /// (approval URL). Terms that only narrow an existing mandate apply directly.
    PaymentsMandatesRequest,
    /// Revoke a mandate: only narrows what an agent may pay, so direct from any face (L1).
    PaymentsMandatesRevoke,
    /// Make a payment (E46). L3 by type: inside a mandate the human's grant is the L3
    /// confirmation and it executes through the payment rail; outside every mandate it is queued
    /// for a per-payment human approval.
    PaymentsSpend,
    /// Remote approvals (E15): the approvals this principal's agents queued, each as its redacted
    /// summary (kind, tier, recipient class, a masked preview, times, status). A read. Deciding is
    /// never a tool: a human decides on the approval page, in Slack, or with the operator's CLI.
    ApprovalsList,
    /// One approval's redacted summary by id (E15). A read.
    ApprovalsGet,
    /// The principal's audit log (E18): one record per write and per approval decision — actor,
    /// principal, tool or route, tier, outcome, time, request id; never a body. A read.
    AuditList,
    /// Access grants on the principal's store (E18): which agent may read or write another agent's
    /// scratch, and which team member may read the world state. A read; granting and revoking are
    /// the operator's (REST / CLI with the admin credential), never an agent's.
    AclList,
    ActionsExecute,
    /// Poll a body-free L3 approval outcome.
    ActionsStatus,
    /// L2 Clarify (issue #394): the questions agent runs are waiting on (`awaiting_input`), or one
    /// question by id with where it stands. A read. Question ids are their own number space —
    /// never an `approval_id`.
    ActionsQuestions,
    /// Answer one question (issue #394): the run that asked resumes with the answer on its
    /// untrusted user half. L1 — it grants nothing: it confirms no action and approves no send;
    /// whatever the run does next is gated at its own level.
    ActionsAnswer,
    /// Add one more instruction to an agent run that is still going (issue #395): the run reads
    /// it on its user half at its next model turn. L1 — like an answer it grants nothing: no
    /// level changes, and a send the run proposes afterwards still waits for the L3 confirm.
    ActionsSteer,
    /// The device's onboarding / first-run setup state (issue #6). A read: an agent needs to know
    /// how far this device is configured, symmetrically with the human UI (invariant 6).
    DeviceOnboardingGet,
    /// The account on this device (#148 P2, FR-ACC-07): signed in or out, the address, sign-in
    /// methods, devices without ids, and the plan in force. Read-only — creating an account and
    /// signing in happen in the app only, the same split as #963 (invariant 6).
    DeviceAccountGet,
    /// The release notes compiled into this build (issue #67): the same catalogue the What's new
    /// modal shows, readable from the agent side so MCP / REST / CLI can say what changed in the
    /// version that is running (invariant 6). Static data, no plan-specific rows.
    DeviceReleaseNotesGet,
    /// The app update state (issue #1296): whether Automatic updates is on, the staged or
    /// available version, and the next scheduled check. Read-only: install, restart and the
    /// switch stay in Settings, so an AI client can never relaunch the app (invariant 6 exception).
    DeviceUpdateGet,
    /// The nightly Dream Cycle's status (issue #285): the same view the Settings "Nightly review"
    /// row reads — the failure indicator, the last run's counts, and the morning report (the night
    /// a result is expected for, whether it produced one, why not, and whether the result on
    /// screen is older than last night). Dates, counts and reason codes only. A read.
    DreamStatus,
    /// Retry the Batch review backlog now (the "Run now" force-drain): recovers by provider
    /// batch id first, never double-submits, idempotent. L1 local request; the app runs it.
    DreamRetry,
    CaptureStatus,
    CaptureSetPaused,
    /// Learned lessons (L5): the same rows the Learned UI lists — id, kind, scope,
    /// instruction, confidence, evidence_count, active. Never `feedback_events` text.
    LessonsList,
    /// Flip one lesson's active switch — the same toggle the Learned UI offers (invariant 6).
    LessonsSetActive,
    VisualRecallStatus,
    VisualRecallSetEnabled,
    VisualRecallSetRetention,
    /// Encrypted database snapshots (issue #647): the retention cap in days, its bounds, the
    /// pruning rule, and how many snapshots exist and how old — from file names, never content.
    BackupStatus,
    /// Set the snapshot retention cap, 1–30 days (L1, the same as the Settings control).
    BackupSetRetention,
    /// The search index's shape and the "Compact search index" job (#714 step 3): the job's state
    /// and progress, what each FTS index still needs, the database's page accounting, and the
    /// free-disk check. Counts, sizes and state names only — never indexed text.
    SearchIndexStatus,
    /// Explicit bounded read-only preview of canonical text duplication (#709).
    StoragePreview,
    /// Start (or resume) compacting the search index: the Settings button (L1, local
    /// maintenance, nothing leaves the device). Refused at once when the disk is too full; the
    /// desktop runs it in bounded, paced steps.
    SearchIndexCompactStart,
    /// Stop the compaction job between two steps (L1). A later start resumes it.
    SearchIndexCompactStop,
    VisualRecallSearchFrames,
    VisualRecallGetFrame,
    VisualRecallFrameTextLines,
    VisualRecallRescanFrame,
    VisualRecallDeleteFrame,
    CaptureExclusionsGet,
    CaptureExclusionsSet,
    ProfileWhoami,
    ProfileSet,
    DictionaryAutoAddGet,
    DictionaryAutoAddSet,
    VoiceDictionaryList,
    VoiceDictionaryCreate,
    VoiceDictionaryUpdate,
    VoiceDictionaryDelete,
    /// Bulk upsert from pasted CSV / TSV / Markdown-table text (gap C2).
    VoiceDictionaryImport,
    /// The whole dictionary as CSV, the same columns import reads back.
    VoiceDictionaryExport,
    MeetingReminderGet,
    MeetingReminderSet,
    MeetingMicrophoneGet,
    MeetingMicrophoneSet,
    /// Queue a Record now offer. The desktop consumes the request within a second and
    /// shows Offered — it does not start the microphone. L2: an agent is not the Record
    /// now button (invariant 6).
    MeetingRecordNow,
    /// Poll a `meeting.start` L2 approval. Separate from [`Tool::ActionsStatus`] (L3 sends).
    MeetingStartStatus,
    MeetingSystemAudioStatus,
    MeetingSystemAudioRetry,
    /// The dictation input device — the same selection the Voice settings card offers (#246).
    VoiceMicrophoneGet,
    VoiceMicrophoneSet,
    WritingVoiceGet,
    WritingVoiceSet,
    /// Which credential the Agent lane runs on (issue #110, invariant 6): the selected source, the
    /// subscription opt-in, the vendor CLIs that are installed, and whether each API-key provider
    /// has a key. Presence booleans only, never a key or any part of one.
    AgentSourceGet,
    /// Change the Agent lane's source. Switching to an API-key provider, changing the model, or
    /// withdrawing the subscription opt-in narrows egress and runs directly (L1). Selecting a
    /// subscription CLI is the opt-in consent itself, so an API caller can only request it: it
    /// waits in the L3 approval queue until the user confirms it in the app.
    AgentSourceSet,
    /// Every service on the Connections screen (issue #82, invariant 6): its state as the app
    /// shows it (unknown while the app is not running), access ceiling, whether it runs through a
    /// third party, a pending disconnect, Gmail draft-only mode and legacy Composio consent. States and
    /// booleans only, never a token, an account or an id.
    ConnectionsList,
    /// Disconnect one service. It only removes access, so it is direct (L1): the desktop deletes
    /// the stored sign-in and stops syncing, exactly like its Disconnect button. Connecting stays
    /// in the app (a browser sign-in or a pasted key).
    ConnectionsDisconnect,
    /// Tighten the legacy Composio policy directly (L1). Direct Gmail is independent of it.
    /// Granting consent or turning legacy draft-only mode off is refused (`connections_api`).
    ConnectionsComposioSet,
    /// Turn Gmail draft-only mode on (L1). Turning it off requires the user in Settings.
    ConnectionsGmailSet,
    /// The Memory API tokens issued on this Mac (issue #400): id, client name, created time and
    /// scope (`read` / `full`), plus the caller's own scope. Never a token or its verifier.
    /// Read-only on purpose: issuing a token or raising its scope happens only in Settings.
    ApiTokensList,
    /// Withhold meeting notes and screen recall from one connected app (owner decision
    /// 2026-10-20, audit F4). It only narrows, so it is direct (L1); allowing them again is the
    /// user's switch in Settings and has no API at all (`source_policy`).
    ApiConnectionsRestrict,
    /// The translation register (politeness) remembered per app — the same `translate.json` the
    /// Reply draft panel's chips write (issue #279 FR-TL-07, invariant 6).
    TranslateRegisterGet,
    /// The app's display language (en default; ja / es / de).
    UiLanguageGet,
    UiLanguageSet,
    /// Set the register for one app, or the default; `match` for an app forgets its entry.
    /// Local and reversible (L1).
    TranslateRegisterSet,
    /// Translate text with the panel's prompt and output gate (issue #279, invariant 6). Runs in
    /// the desktop on the Agent lane with the user's own credential (invariant 5); text only,
    /// nothing is sent or stored (L1).
    TranslateText,
    /// Draft a reply to a message in its own language, with a back-translation into the user's
    /// language (FR-TL-11/12). Same lane and level as [`Tool::TranslateText`]; the draft is never
    /// sent (FR-TL-14).
    TranslateReplyDraft,
    /// A translate / reply-draft request the face stopped waiting for: its status, and the result
    /// once, when it has one. A read.
    TranslateStatus,
    /// The language pair translation and reply drafts use (FR-TL-03): the user's choice, or that
    /// the desktop follows the system language. The same `translate.json` as the register.
    TranslatePairGet,
    /// Choose the pair, or go back to following the system language. Local, reversible (L1).
    TranslatePairSet,
    /// A meeting's speakers: the diarized keys observed in the transcript plus any user-confirmed
    /// names — the same view the Recap's naming chips render (invariant 6, FR-MT-15/-22).
    MeetingSpeakersGet,
    /// Confirm (or clear) who one speaker key is. The same explicit assignment the Recap offers;
    /// local and reversible (L1). Never invoked on inference — a caller states, SHOGUN records.
    MeetingSpeakersSet,
    /// The meeting-template registry plus a session's current selection — the same list the
    /// Recap's picker renders (invariant 6).
    MeetingTemplatesGet,
    /// Choose the template that shapes a session's minutes (L1, local, reversible).
    MeetingTemplatesSet,
    /// The share-ready Markdown for a session's Recap — the same document the Recap's copy
    /// button produces (invariant 6). A read: producing text is local; sending stays L3.
    MeetingRecapExport,
    /// `[Track]` one of a session's Recap next actions (FR-MT-17): the explicit confirmation that
    /// turns a candidate into a commitment at confidence 1.0 — the same tap the Recap offers.
    /// Only the session's own suggestions can be tracked; local and reversible (L1).
    MeetingRecapTrack,
    /// The user's skill files (issue #251): name, description, effective level, required
    /// connections, plus each file that did not load and why. `q=<name>` adds that skill's
    /// instructions. A read — the files are the user's own, on this Mac.
    SkillsList,
    /// Ask for a skill to run (issue #251). The desktop runs it on the Agent lane; a skill above
    /// L1 waits for the user to confirm it there. Produces text only — a skill never sends.
    SkillsRun,
    /// One skill run: status and, once done, its text.
    SkillsRunStatus,
    /// Save a new skill file from Markdown (issue #251 "save as skill"). Validated exactly as the
    /// folder loader reads it; an existing name is never overwritten (L1, local).
    SkillsSave,
    /// The user's schedules (issue #252): which skill runs on which weekdays at what local time,
    /// and what the last slot did (queued / skipped and why).
    SchedulesList,
    /// Schedule a skill (issue #252). A due slot enqueues an ordinary skill run: L1 runs, above L1
    /// waits for the user, nothing sends; at most 10 scheduled runs a day.
    SchedulesAdd,
    /// Delete a schedule (L1, local).
    SchedulesRemove,
    /// The bug reports / questions this Mac sent from Help & Support and where each stands
    /// (issue #108): ticket id, category, status, the release that fixed it, an open question.
    /// Never the report text, never the reporter token.
    SupportReportsList,
    /// Bounded saved-session archive; id is an exclusive older-session cursor.
    MeetingList,
    /// A public YouTube video's captions (issue #555): title, channel, language, timestamped
    /// segments. SHOGUN fetches it itself over the traced transport; needs the Web fetch consent
    /// and Pro. `url` is the one input.
    WebVideoTranscript,
    /// One public X post from its link (issue #555): author, date, text.
    WebSocialPost,
    /// The readable text of a public https page (issue #555).
    WebPage,
    /// A session's Recap as structured fields — the same summary / decisions / next actions /
    /// sections / speakers / note the Recap panel renders (invariant 6, FR-MT-22). A read: the
    /// minutes are local text; nothing here sends or executes a next action (invariant 4).
    MeetingRecapGet,
    /// A session's transcript lines with timestamps and resolved speaker labels — the same rows
    /// the captions panel replays (FR-MT-22). Text only; the audio never existed on disk.
    MeetingTranscriptGet,
    /// The Recap preferences the Settings card offers: minutes language and the custom minutes
    /// instruction (invariant 6). Shared `meeting.json`, like the microphone setting.
    MeetingRecapPrefsGet,
    /// Set either preference (L1: local, reversible, nothing leaves the device).
    MeetingRecapPrefsSet,
    /// Rewrite part of a session's minutes by hand — the same edit the Recap panel offers
    /// (D1, invariant 6). Local text; a regeneration never overwrites it. `reset: true` hands
    /// the minutes back to the model.
    MeetingRecapEdit,
    /// Lexical search over saved meetings' minutes and transcripts — the Meetings list's own
    /// search box (D6). Query-relevant, best first; a read.
    MeetingSearch,
    /// Delete one saved meeting: its transcript, notes, minutes, speaker names, and the
    /// meeting-derived search rows (D6). The list's own ⋯ Delete; local, L1.
    MeetingDelete,
    /// Retained source excerpts for one person/project, for review rather than generation.
    WikiEvidence,
    /// Review-only neighbors sharing retained source events, not semantic relationships.
    WikiNeighbors,
    /// Bounded source-backed entity list, including topics, tools and organizations.
    WikiList,
    /// The Work Patterns overview (FR-PAT-06) plus bounded dashboard usage: live 7-day
    /// aggregates, profile rows, captured-time ranges, and a sparse 20-week local calendar.
    /// Captured time is post-instrumentation sampled foreground metadata, never a historic
    /// dwell backfill or hours-worked estimate. Daily presentation buckets are conservatively
    /// forgotten by their latest UTC activity instant, independent of timezone changes. Counts
    /// and metadata only.
    PatternsList,
    /// Flip one pattern's active switch — the same per-row control the Patterns card offers.
    /// Local and reversible (L1); OFF stays visible but is never consumed.
    PatternsSetActive,
    /// Mail triage (issue #319, FR-TG-30): the per-thread estimates the Mail pane lists —
    /// label / bucket / attention with confidence, stage, reasons and provenance. No bodies.
    MailTriageList,
    /// One triage row by thread key (`gmail:<id>`).
    MailTriageGet,
    /// A Stage U correction — relabel / move bucket / mark handled. Local state only (L1).
    MailTriageSet,
    /// Ask for one thread to leave the Gmail inbox. An L2 write that records the intent; the
    /// desktop's write-back tick executes it under the label's standing approval or surfaces a
    /// one-tap approval — the API never touches Gmail itself (FR-TG-21, FR-API-04).
    MailTriageArchive,
    /// The inverse of [`Tool::MailTriageArchive`] — same L2 path, same write-back tick.
    MailTriageUnarchive,
    /// The label registry (built-in + custom) with each label's flags.
    MailLabelsList,
    /// Create / edit a custom label, or change a label's flags. Granting `auto_archive` or
    /// `sync_to_gmail` is the standing approval (FR-TG-21) and needs `confirm_standing=true`.
    MailLabelsUpsert,
    /// Delete a custom label (built-ins cannot be deleted).
    MailLabelsDelete,
    /// The sender rules (hand-made and learned from two same-direction corrections).
    MailSenderRulesList,
    /// Add a sender rule by hand.
    MailSenderRulesAdd,
    /// Delete a sender rule.
    MailSenderRulesDelete,
    /// The Gmail write-back history (archive / label sync), newest first — the undo list.
    MailHistoryList,
    /// Whether triage is running at all (the master switch, issue #319).
    MailSettingsGet,
    /// Turn triage on or off. Off stops classification, Batch refinement and every Gmail-side
    /// write; it does **not** disconnect Gmail or discard what is already classified.
    MailSettingsSet,
    /// Save a meeting Recap as a Markdown / CSV file into one of the folders the user allowed
    /// (issue #397, `LocalAction::WriteFile`). L2: the request waits until the user confirms it in
    /// the desktop; nothing is overwritten, nothing is sent.
    ExportWriteFile,
    /// File exports and where each stands (awaiting_confirm / written / cancelled / failed), with
    /// the written path, byte count and content digest — never the content.
    ExportWritesList,
    /// The folders the user allowed exports into (empty = the feature is off).
    ExportFoldersList,
    /// Remove an allowed folder (L1 — it only narrows where files may appear). Adding one is the
    /// desktop folder picker's job: the pick is the consent.
    ExportFoldersRemove,
}

/// Every tool, for exhaustive iteration (settings / tests).
pub const ALL_TOOLS: &[Tool] = &[
    Tool::FormsInspect,
    Tool::FormsPlan,
    Tool::FormsStatus,
    Tool::FormsApply,
    Tool::FormsClose,
    Tool::FormsUndo,
    Tool::MemoryNotesList,
    Tool::MemoryNotesChange,
    Tool::MemorySearch,
    Tool::MemoryGetContext,
    Tool::MemoryGetContextPack,
    Tool::MemoryGetEvents,
    Tool::MemoryReadSession,
    Tool::MemoryGetWrap,
    Tool::CalendarUpcoming,
    Tool::MemoryGetPeriods,
    Tool::StatePeopleList,
    Tool::StatePeopleGet,
    Tool::StateProjectsList,
    Tool::StateProjectsGet,
    Tool::StateCommitmentsList,
    Tool::StateCommitmentsGet,
    Tool::StateOpenLoopsList,
    Tool::StateOpenLoopsGet,
    Tool::StateDisputesList,
    Tool::StateDisputesResolve,
    Tool::StateUpdate,
    Tool::StateDelete,
    Tool::StateCreate,
    Tool::StateExport,
    Tool::StateSplit,
    Tool::StateSplitEvidence,
    Tool::StateMergeCandidates,
    Tool::StateMerge,
    Tool::StateKeepSeparate,
    Tool::MemoryImportPreview,
    Tool::MemoryImport,
    Tool::MemoryImportList,
    Tool::MemoryImportDelete,
    Tool::MemoryAppendNote,
    Tool::MemoryIngest,
    Tool::MemoryIngestWebhook,
    Tool::MemoryRecall,
    Tool::MemoryProcessingStatus,
    Tool::MemoryFs,
    Tool::HandoffCreate,
    Tool::HandoffAccept,
    Tool::PaymentsMandatesList,
    Tool::PaymentsMandatesRequest,
    Tool::PaymentsMandatesRevoke,
    Tool::PaymentsSpend,
    Tool::ApprovalsList,
    Tool::ApprovalsGet,
    Tool::AuditList,
    Tool::AclList,
    Tool::ActionsExecute,
    Tool::ActionsStatus,
    Tool::ActionsQuestions,
    Tool::ActionsAnswer,
    Tool::ActionsSteer,
    Tool::DeviceOnboardingGet,
    Tool::DeviceAccountGet,
    Tool::DeviceReleaseNotesGet,
    Tool::DeviceUpdateGet,
    Tool::DreamStatus,
    Tool::DreamRetry,
    Tool::CaptureStatus,
    Tool::CaptureSetPaused,
    Tool::LessonsList,
    Tool::LessonsSetActive,
    Tool::VisualRecallStatus,
    Tool::VisualRecallSetEnabled,
    Tool::VisualRecallSetRetention,
    Tool::BackupStatus,
    Tool::BackupSetRetention,
    Tool::SearchIndexStatus,
    Tool::StoragePreview,
    Tool::SearchIndexCompactStart,
    Tool::SearchIndexCompactStop,
    Tool::VisualRecallSearchFrames,
    Tool::VisualRecallGetFrame,
    Tool::VisualRecallFrameTextLines,
    Tool::VisualRecallRescanFrame,
    Tool::VisualRecallDeleteFrame,
    Tool::CaptureExclusionsGet,
    Tool::CaptureExclusionsSet,
    Tool::ProfileWhoami,
    Tool::ProfileSet,
    Tool::DictionaryAutoAddGet,
    Tool::DictionaryAutoAddSet,
    Tool::VoiceDictionaryList,
    Tool::VoiceDictionaryCreate,
    Tool::VoiceDictionaryUpdate,
    Tool::VoiceDictionaryDelete,
    Tool::VoiceDictionaryImport,
    Tool::VoiceDictionaryExport,
    Tool::MeetingReminderGet,
    Tool::MeetingReminderSet,
    Tool::MeetingMicrophoneGet,
    Tool::MeetingMicrophoneSet,
    Tool::MeetingRecordNow,
    Tool::MeetingStartStatus,
    Tool::MeetingSystemAudioStatus,
    Tool::MeetingSystemAudioRetry,
    Tool::VoiceMicrophoneGet,
    Tool::VoiceMicrophoneSet,
    Tool::WritingVoiceGet,
    Tool::WritingVoiceSet,
    Tool::AgentSourceGet,
    Tool::AgentSourceSet,
    Tool::ConnectionsList,
    Tool::ConnectionsDisconnect,
    Tool::ConnectionsComposioSet,
    Tool::ConnectionsGmailSet,
    Tool::ApiTokensList,
    Tool::ApiConnectionsRestrict,
    Tool::TranslateRegisterGet,
    Tool::UiLanguageGet,
    Tool::UiLanguageSet,
    Tool::TranslateRegisterSet,
    Tool::TranslateText,
    Tool::TranslateReplyDraft,
    Tool::TranslateStatus,
    Tool::TranslatePairGet,
    Tool::TranslatePairSet,
    Tool::MeetingSpeakersGet,
    Tool::MeetingSpeakersSet,
    Tool::MeetingTemplatesGet,
    Tool::MeetingTemplatesSet,
    Tool::MeetingRecapExport,
    Tool::MeetingRecapTrack,
    Tool::SkillsList,
    Tool::SkillsRun,
    Tool::SkillsRunStatus,
    Tool::SkillsSave,
    Tool::SchedulesList,
    Tool::SchedulesAdd,
    Tool::SchedulesRemove,
    Tool::SupportReportsList,
    Tool::MeetingList,
    Tool::WebVideoTranscript,
    Tool::WebSocialPost,
    Tool::WebPage,
    Tool::MeetingRecapGet,
    Tool::MeetingTranscriptGet,
    Tool::MeetingRecapPrefsGet,
    Tool::MeetingRecapPrefsSet,
    Tool::MeetingRecapEdit,
    Tool::MeetingSearch,
    Tool::MeetingDelete,
    Tool::WikiEvidence,
    Tool::WikiNeighbors,
    Tool::WikiList,
    Tool::PatternsList,
    Tool::PatternsSetActive,
    Tool::MailTriageList,
    Tool::MailTriageGet,
    Tool::MailTriageSet,
    Tool::MailTriageArchive,
    Tool::MailTriageUnarchive,
    Tool::MailLabelsList,
    Tool::MailLabelsUpsert,
    Tool::MailLabelsDelete,
    Tool::MailSenderRulesList,
    Tool::MailSenderRulesAdd,
    Tool::MailSenderRulesDelete,
    Tool::MailHistoryList,
    Tool::MailSettingsGet,
    Tool::MailSettingsSet,
    Tool::ExportWriteFile,
    Tool::ExportWritesList,
    Tool::ExportFoldersList,
    Tool::ExportFoldersRemove,
];

impl Tool {
    /// The stable wire name (the same string the CLI, REST, and MCP faces use).
    pub fn wire_name(self) -> &'static str {
        match self {
            Tool::MemoryNotesList => "memory.notes.list",
            Tool::MemoryNotesChange => "memory.notes.change",
            Tool::MemorySearch => "memory.search",
            Tool::MemoryGetContext => "memory.get_context",
            Tool::MemoryGetContextPack => "memory.get_context_pack",
            Tool::MemoryGetEvents => "memory.get_events",
            Tool::MemoryReadSession => "memory.read_session",
            Tool::MemoryGetWrap => "memory.get_wrap",
            Tool::CalendarUpcoming => "calendar.upcoming",
            Tool::MemoryGetPeriods => "memory.get_periods",
            Tool::StatePeopleList => "state.people.list",
            Tool::StatePeopleGet => "state.people.get",
            Tool::StateProjectsList => "state.projects.list",
            Tool::StateProjectsGet => "state.projects.get",
            Tool::StateCommitmentsList => "state.commitments.list",
            Tool::StateCommitmentsGet => "state.commitments.get",
            Tool::StateOpenLoopsList => "state.open_loops.list",
            Tool::StateOpenLoopsGet => "state.open_loops.get",
            Tool::StateDisputesList => "state.disputes.list",
            Tool::StateDisputesResolve => "state.disputes.resolve",
            Tool::StateUpdate => "state.update",
            Tool::StateDelete => "state.delete",
            Tool::StateCreate => "state.create",
            Tool::StateExport => "state.export",
            Tool::StateSplit => "state.split",
            Tool::StateSplitEvidence => "state.split.evidence",
            Tool::StateMergeCandidates => "state.merge.candidates",
            Tool::StateMerge => "state.merge",
            Tool::StateKeepSeparate => "state.keep_separate",
            Tool::MemoryImportPreview => "memory.import.preview",
            Tool::MemoryImport => "memory.import",
            Tool::MemoryImportList => "memory.import.list",
            Tool::MemoryImportDelete => "memory.import.delete",
            Tool::MemoryAppendNote => "memory.append_note",
            Tool::MemoryIngest => "memory.ingest",
            Tool::MemoryIngestWebhook => "memory.ingest_webhook",
            Tool::MemoryRecall => "memory.recall",
            Tool::MemoryProcessingStatus => "memory.processing_status",
            Tool::MemoryFs => "memory.fs",
            Tool::HandoffCreate => "handoff.create",
            Tool::HandoffAccept => "handoff.accept",
            Tool::PaymentsMandatesList => "payments.mandates.list",
            Tool::PaymentsMandatesRequest => "payments.mandates.request",
            Tool::PaymentsMandatesRevoke => "payments.mandates.revoke",
            Tool::PaymentsSpend => "payments.spend",
            Tool::ApprovalsList => "approvals.list",
            Tool::ApprovalsGet => "approvals.get",
            Tool::AuditList => "audit.list",
            Tool::AclList => "acl.list",
            Tool::ActionsExecute => "actions.execute",
            Tool::ActionsStatus => "actions.status",
            Tool::ActionsQuestions => "actions.questions",
            Tool::ActionsAnswer => "actions.answer",
            Tool::ActionsSteer => "actions.steer",
            Tool::DeviceOnboardingGet => "device.onboarding.get",
            Tool::DeviceAccountGet => "device.account.get",
            Tool::DeviceReleaseNotesGet => "device.release_notes.get",
            Tool::DeviceUpdateGet => "device.update.get",
            Tool::DreamStatus => "dream.status",
            Tool::DreamRetry => "dream.retry",
            Tool::CaptureStatus => "capture.status",
            Tool::CaptureSetPaused => "capture.set_paused",
            Tool::LessonsList => "lessons.list",
            Tool::LessonsSetActive => "lessons.set_active",
            Tool::VisualRecallStatus => "visual_recall.status",
            Tool::VisualRecallSetEnabled => "visual_recall.set_enabled",
            Tool::VisualRecallSetRetention => "visual_recall.set_retention",
            Tool::BackupStatus => "backup.status",
            Tool::BackupSetRetention => "backup.set_retention",
            Tool::SearchIndexStatus => "search_index.status",
            Tool::StoragePreview => "storage.preview",
            Tool::SearchIndexCompactStart => "search_index.compact.start",
            Tool::SearchIndexCompactStop => "search_index.compact.stop",
            Tool::VisualRecallSearchFrames => "visual_recall.search_frames",
            Tool::VisualRecallGetFrame => "visual_recall.get_frame",
            Tool::VisualRecallFrameTextLines => "visual_recall.frame_text_lines",
            Tool::VisualRecallRescanFrame => "visual_recall.rescan_frame",
            Tool::VisualRecallDeleteFrame => "visual_recall.delete_frame",
            Tool::CaptureExclusionsGet => "capture_exclusions.get",
            Tool::CaptureExclusionsSet => "capture_exclusions.set",
            Tool::ProfileWhoami => "profile.whoami",
            Tool::ProfileSet => "profile.set",
            Tool::DictionaryAutoAddGet => "voice_dictionary.autoadd",
            Tool::DictionaryAutoAddSet => "voice_dictionary.autoadd_manage",
            Tool::VoiceDictionaryList => "voice_dictionary.list",
            Tool::VoiceDictionaryCreate => "voice_dictionary.create",
            Tool::VoiceDictionaryUpdate => "voice_dictionary.update",
            Tool::VoiceDictionaryDelete => "voice_dictionary.delete",
            Tool::VoiceDictionaryImport => "voice_dictionary.import",
            Tool::VoiceDictionaryExport => "voice_dictionary.export",
            Tool::MeetingReminderGet => "meeting.reminder.get",
            Tool::MeetingReminderSet => "meeting.reminder.set",
            Tool::MeetingMicrophoneGet => "meeting.microphone.get",
            Tool::MeetingMicrophoneSet => "meeting.microphone.set",
            Tool::MeetingRecordNow => "meeting.start",
            Tool::MeetingStartStatus => "meeting.start.status",
            Tool::MeetingSystemAudioStatus => "meeting.system_audio.status",
            Tool::MeetingSystemAudioRetry => "meeting.system_audio.retry",
            Tool::VoiceMicrophoneGet => "voice.microphone.get",
            Tool::VoiceMicrophoneSet => "voice.microphone.set",
            Tool::WritingVoiceGet => "writing_voice.get",
            Tool::WritingVoiceSet => "writing_voice.set",
            Tool::AgentSourceGet => "agent.source.get",
            Tool::AgentSourceSet => "agent.source.set",
            Tool::ConnectionsList => "connections.list",
            Tool::ApiTokensList => "api.tokens.list",
            Tool::ApiConnectionsRestrict => "api.connections.restrict",
            Tool::ConnectionsDisconnect => "connections.disconnect",
            Tool::ConnectionsComposioSet => "connections.composio.set",
            Tool::ConnectionsGmailSet => "connections.gmail.set",
            Tool::TranslateRegisterGet => "translate.register.get",
            Tool::UiLanguageGet => "settings.language.get",
            Tool::UiLanguageSet => "settings.language.set",
            Tool::TranslateRegisterSet => "translate.register.set",
            Tool::TranslateText => "translate.text",
            Tool::FormsInspect => "forms.inspect",
            Tool::FormsPlan => "forms.plan",
            Tool::FormsStatus => "forms.status",
            Tool::FormsApply => "forms.apply",
            Tool::FormsClose => "forms.close",
            Tool::FormsUndo => "forms.undo",

            Tool::TranslateReplyDraft => "translate.reply_draft",
            Tool::TranslateStatus => "translate.status",
            Tool::TranslatePairGet => "translate.pair.get",
            Tool::TranslatePairSet => "translate.pair.set",
            Tool::MeetingSpeakersGet => "meeting.speakers.get",
            Tool::MeetingSpeakersSet => "meeting.speakers.set",
            Tool::MeetingTemplatesGet => "meeting.templates.get",
            Tool::MeetingTemplatesSet => "meeting.templates.set",
            Tool::MeetingRecapExport => "meeting.recap.export",
            Tool::MeetingRecapTrack => "meeting.recap.track",
            Tool::SkillsList => "skills.list",
            Tool::SkillsRun => "skills.run",
            Tool::SkillsRunStatus => "skills.run.status",
            Tool::SkillsSave => "skills.save",
            Tool::SchedulesList => "schedules.list",
            Tool::SchedulesAdd => "schedules.add",
            Tool::SchedulesRemove => "schedules.remove",
            Tool::SupportReportsList => "support.reports.list",
            Tool::MeetingList => "meeting.list",
            Tool::WebVideoTranscript => "web.video_transcript",
            Tool::WebSocialPost => "web.social_post",
            Tool::WebPage => "web.page",
            Tool::MeetingRecapGet => "meeting.recap.get",
            Tool::MeetingTranscriptGet => "meeting.transcript.get",
            Tool::MeetingRecapPrefsGet => "meeting.recap_prefs.get",
            Tool::MeetingRecapPrefsSet => "meeting.recap_prefs.set",
            Tool::MeetingRecapEdit => "meeting.recap.edit",
            Tool::MeetingSearch => "meeting.search",
            Tool::MeetingDelete => "meeting.delete",
            Tool::WikiEvidence => "wiki.evidence",
            Tool::WikiNeighbors => "wiki.neighbors",
            Tool::WikiList => "wiki.list",
            Tool::PatternsList => "patterns.list",
            Tool::PatternsSetActive => "patterns.set_active",
            Tool::MailTriageList => "mail.triage.list",
            Tool::MailTriageGet => "mail.triage.get",
            Tool::MailTriageSet => "mail.triage.set",
            Tool::MailTriageArchive => "mail.triage.archive",
            Tool::MailTriageUnarchive => "mail.triage.unarchive",
            Tool::MailLabelsList => "mail.labels.list",
            Tool::MailLabelsUpsert => "mail.labels.upsert",
            Tool::MailLabelsDelete => "mail.labels.delete",
            Tool::MailSenderRulesList => "mail.sender_rules.list",
            Tool::MailSenderRulesAdd => "mail.sender_rules.add",
            Tool::MailSenderRulesDelete => "mail.sender_rules.delete",
            Tool::MailHistoryList => "mail.history.list",
            Tool::MailSettingsGet => "mail.settings.get",
            Tool::MailSettingsSet => "mail.settings.set",
            Tool::ExportWriteFile => "export.write_file",
            Tool::ExportWritesList => "export.writes.list",
            Tool::ExportFoldersList => "export.folders.list",
            Tool::ExportFoldersRemove => "export.folders.remove",
        }
    }

    /// Parse a wire name back to a tool.
    pub fn from_wire(name: &str) -> Option<Tool> {
        ALL_TOOLS.iter().copied().find(|t| t.wire_name() == name)
    }
}

/// A tool's permission requirement (FR-API-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiLevel {
    /// A read (still requires a valid token, FR-API-03; confidence rule applies, FR-API-06).
    Read,
    /// A write gated at this level (append_note = L1, propose_update = L2).
    Write(Level),
    /// `actions.execute` — the level is that of the launched action (L3 sends use the approval flow).
    PerAction,
}

/// The permission requirement for a tool (FR-API-02 table).
pub fn tool_level(tool: Tool) -> ApiLevel {
    match tool {
        Tool::FormsInspect => ApiLevel::Write(Level::L2),
        Tool::FormsPlan | Tool::FormsClose => ApiLevel::Write(Level::L1),
        Tool::FormsStatus => ApiLevel::Read,
        Tool::FormsApply | Tool::FormsUndo => ApiLevel::Write(Level::L3),
        Tool::MemoryNotesList
        | Tool::MemorySearch
        | Tool::MemoryGetContext
        | Tool::MemoryGetContextPack
        | Tool::MemoryGetEvents
        | Tool::MemoryReadSession
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
        | Tool::StateExport
        | Tool::StateSplitEvidence
        | Tool::StateMergeCandidates
        | Tool::MemoryImportPreview
        | Tool::MemoryImportList
        | Tool::DeviceOnboardingGet
        | Tool::DeviceAccountGet
        | Tool::DeviceReleaseNotesGet
        | Tool::DreamStatus
        | Tool::CaptureStatus
        | Tool::LessonsList
        | Tool::VisualRecallStatus
        | Tool::BackupStatus
        | Tool::SearchIndexStatus
        | Tool::StoragePreview
        | Tool::VisualRecallSearchFrames
        | Tool::VisualRecallGetFrame
        | Tool::VisualRecallFrameTextLines
        | Tool::CaptureExclusionsGet
        // A budgeted read of the context-pack assembly: nothing is written (E4-T3).
        | Tool::MemoryRecall
        | Tool::MemoryProcessingStatus
        | Tool::ProfileWhoami => ApiLevel::Read,
        Tool::DictionaryAutoAddGet
        | Tool::VoiceDictionaryList
        | Tool::VoiceDictionaryExport
        | Tool::MeetingReminderGet
        | Tool::MeetingMicrophoneGet
        | Tool::MeetingStartStatus
        | Tool::MeetingSystemAudioStatus
        | Tool::VoiceMicrophoneGet
        | Tool::WritingVoiceGet
        | Tool::AgentSourceGet
        | Tool::ConnectionsList
        | Tool::ApiTokensList
        | Tool::TranslateRegisterGet
        | Tool::TranslatePairGet
        | Tool::TranslateStatus
        | Tool::UiLanguageGet
        | Tool::DeviceUpdateGet
        | Tool::MeetingSpeakersGet
        | Tool::MeetingTemplatesGet
        | Tool::MeetingRecapExport
        | Tool::SkillsList
        | Tool::SkillsRunStatus
        | Tool::SchedulesList
        | Tool::SupportReportsList
        | Tool::MeetingList
        // A public fetch is a read: the URL leaves, nothing is written anywhere (issue #555).
        | Tool::WebVideoTranscript
        | Tool::WebSocialPost
        | Tool::WebPage
        | Tool::MeetingRecapGet
        | Tool::MeetingTranscriptGet
        | Tool::MeetingSearch
        | Tool::MeetingRecapPrefsGet
        | Tool::WikiEvidence
        | Tool::WikiNeighbors
        | Tool::WikiList
        | Tool::PatternsList => ApiLevel::Read,
        Tool::TranslateRegisterSet => ApiLevel::Write(Level::L1),
        Tool::TranslatePairSet => ApiLevel::Write(Level::L1),
        // Translating and drafting (issue #279) produce text and nothing else: no send, no insert,
        // no stored draft (FR-TL-14 / FR-TL-20). The text goes to the model the user chose, on
        // their own credential, like a chat turn — not an external send, so not L3 (invariant 4).
        // L1 rather than a read so a read-only token cannot spend the user's model access.
        Tool::TranslateText | Tool::TranslateReplyDraft => ApiLevel::Write(Level::L1),
        Tool::UiLanguageSet => ApiLevel::Write(Level::L1),
        // append a user note to the event log — local, reversible.
        Tool::MemoryAppendNote => ApiLevel::Write(Level::L1),
        // An agent's events / turns appended to the event log under an idempotency key (E4-T3):
        // local, replayable, nothing leaves the device — L1 like a note (plan §7.1).
        Tool::MemoryIngest => ApiLevel::Write(Level::L1),
        // E44: a verified provider delivery through the same append path — L1 like `memory.ingest`.
        Tool::MemoryIngestWebhook => ApiLevel::Write(Level::L1),
        // `/memories` file operations (E6-T2): a view reads, a write edits the caller's own
        // scratch or records an ingest event — local and reversible, L1 like `memory.ingest`.
        Tool::MemoryFs => ApiLevel::Write(Level::L1),
        // E35: the handoff object is one budgeted recall, reshaped: a read, free.
        Tool::HandoffCreate => ApiLevel::Read,
        // E35: accepting writes only the caller's own scratch, through the ingest path — L1 like
        // `memory.ingest` and a scratch edit through `memory.fs`.
        Tool::HandoffAccept => ApiLevel::Write(Level::L1),
        // E46 payment mandates. Listing is a free read. A payment and a mandate grant are L3 by
        // type (`SendAction::Pay` / `GrantPaymentMandate`): `payments.spend` executes only under a
        // mandate a human granted or after a per-payment human approval, and a request that
        // widens or extends a mandate is queued for a human. A request that only narrows applies
        // directly; that is the lower half of an L3 ceiling, like `visual_recall.set_retention`.
        // Revoking only narrows: L1, from any face.
        Tool::PaymentsMandatesList => ApiLevel::Read,
        Tool::PaymentsMandatesRequest | Tool::PaymentsSpend => ApiLevel::Write(Level::L3),
        Tool::PaymentsMandatesRevoke => ApiLevel::Write(Level::L1),
        // Governance reads (E15 / E18): summaries, the audit trail, the grant list. Free reads.
        Tool::ApprovalsList | Tool::ApprovalsGet | Tool::AuditList | Tool::AclList => ApiLevel::Read,
        // Pausing narrows what SHOGUN reads and runs directly (L1). Ending the user's pause
        // starts reading the screen again: the same "removes a privacy boundary" reason
        // `CaptureExclusionsSet` is L3, so the ceiling is L3 and that half is queued for the
        // user's in-app approval on every face (#897, `capture_pause_api`). The desktop's own
        // pause switch is the user's.
        Tool::CaptureSetPaused => ApiLevel::Write(Level::L3),
        Tool::MemoryNotesChange | Tool::LessonsSetActive => ApiLevel::Write(Level::L1),
        Tool::StateDisputesResolve => ApiLevel::Write(Level::L1),
        Tool::StateUpdate | Tool::StateDelete | Tool::StateCreate => ApiLevel::Write(Level::L1),
        Tool::StateSplit => ApiLevel::Write(Level::L1),
        Tool::StateMerge | Tool::StateKeepSeparate => ApiLevel::Write(Level::L1),
        Tool::MemoryImport | Tool::MemoryImportDelete => ApiLevel::Write(Level::L1),
        // Visual recall's master switch and its finite encrypted-frame retention (issue #782).
        // Turning it on, or keeping saved screens longer, starts or lengthens keeping images of
        // the user's screen: the same "removes a privacy boundary" reason `CaptureExclusionsSet`
        // below is L3, so the ceiling is L3 and that half is queued for the user's in-app
        // approval on every face. Turning it off (which deletes the automatically captured
        // frames) or shortening retention only narrows what is kept and runs directly at L1
        // (`visual_recall_settings_api`). The desktop Settings controls are the user's own.
        Tool::VisualRecallSetEnabled | Tool::VisualRecallSetRetention => ApiLevel::Write(Level::L3),
        // Snapshot retention (#647), both directions: shorter deletes local copies sooner; longer
        // keeps a local encrypted safety copy, within the 30-day ceiling. Nothing leaves the
        // device either way, so no L3.
        Tool::BackupSetRetention => ApiLevel::Write(Level::L1),
        Tool::SearchIndexCompactStart | Tool::SearchIndexCompactStop => ApiLevel::Write(Level::L1),
        Tool::DreamRetry => ApiLevel::Write(Level::L1),
        Tool::VisualRecallDeleteFrame => ApiLevel::Write(Level::L1),
        // Re-running OCR replaces the frame event's stored OCR text: a local write to the user's
        // own memory, the same tier as deleting that frame (L1), not a read. Nothing leaves the
        // device (on-device OCR). (issue #410)
        Tool::VisualRecallRescanFrame => ApiLevel::Write(Level::L1),
        // Replacing the list can remove a privacy boundary, so API callers must treat it as L3.
        Tool::CaptureExclusionsSet => ApiLevel::Write(Level::L3),
        Tool::ProfileSet => ApiLevel::Write(Level::L1),
        Tool::DictionaryAutoAddSet => ApiLevel::Write(Level::L3),
        Tool::VoiceDictionaryCreate
        | Tool::VoiceDictionaryUpdate
        | Tool::VoiceDictionaryDelete
        // A bulk import is many of the same local, reversible edits; the same level.
        | Tool::VoiceDictionaryImport => ApiLevel::Write(Level::L1),
        // Picking the capture device is local and reversible — same L1 as the meeting input.
        Tool::MeetingReminderSet | Tool::MeetingMicrophoneSet | Tool::VoiceMicrophoneSet => ApiLevel::Write(Level::L1),
        // Turning this on starts sending the user's own past drafts to the selected provider on
        // every rewrite, which is the same "removes a privacy boundary" reason
        // `CaptureExclusionsSet` above is L3 — and the `consent` flag cannot stand in for the
        // approval, because the caller asserting it is the caller being gated (invariant 4:
        // L1 never opens an external send). L3 is the tool's ceiling: `enable` is queued for the
        // user's in-app approval on every face, while `disable` / `clear` only reduce sharing and
        // run directly at L1 (issue #654, `writing_voice_api`).
        Tool::WritingVoiceSet => ApiLevel::Write(Level::L3),
        // The caller asserting consent is the caller being gated, so the tool's ceiling is L3
        // (`agent_source_api`). Issue #110.
        Tool::AgentSourceSet => ApiLevel::Write(Level::L3),
        // Every move here only removes capability, so it is direct at L1 (issue #82). The
        // loosening moves are not requestable at any level over the API (`connections_api`).
        Tool::ConnectionsDisconnect | Tool::ConnectionsComposioSet | Tool::ConnectionsGmailSet => ApiLevel::Write(Level::L1),
        // Withholding meetings and screen recall from a connection only narrows it (2026-10-20).
        Tool::ApiConnectionsRestrict => ApiLevel::Write(Level::L1),
        // Starting notes from an agent is one-tap confirm (Offered), not auto-run. Live ASR
        // leaves the device; the human Record now button remains the explicit consent.
        Tool::MeetingRecordNow | Tool::MeetingSystemAudioRetry => ApiLevel::Write(Level::L2),
        Tool::MeetingSpeakersSet => ApiLevel::Write(Level::L1),
        Tool::MeetingTemplatesSet => ApiLevel::Write(Level::L1),
        Tool::MeetingRecapTrack => ApiLevel::Write(Level::L1),
        Tool::SkillsRun => ApiLevel::Write(Level::L1),
        Tool::SkillsSave => ApiLevel::Write(Level::L1),
        Tool::SchedulesAdd | Tool::SchedulesRemove => ApiLevel::Write(Level::L1),
        Tool::MeetingRecapPrefsSet => ApiLevel::Write(Level::L1),
        Tool::MeetingRecapEdit => ApiLevel::Write(Level::L1),
        Tool::MeetingDelete => ApiLevel::Write(Level::L1),
        Tool::PatternsSetActive => ApiLevel::Write(Level::L1),
        Tool::MailTriageList
        | Tool::MailTriageGet
        | Tool::MailLabelsList
        | Tool::MailSenderRulesList
        | Tool::MailHistoryList
        | Tool::MailSettingsGet => ApiLevel::Read,
        // Local mail state (a correction, the label registry, a sender rule) — L1, reversible.
        // `mail.labels.upsert` cannot grant a standing approval (issue #779): turning
        // `auto_archive` / `sync_to_gmail` on is refused on every API face, so what remains is
        // local and L1.
        Tool::MailTriageSet
        | Tool::MailLabelsUpsert
        | Tool::MailLabelsDelete
        | Tool::MailSenderRulesAdd
        | Tool::MailSenderRulesDelete
        // The master switch changes nothing outside this Mac and flips back in one call — L1,
        // not L2. Turning triage *off* also removes capability rather than adding it. (Unlike
        // `visual_recall.set_enabled`, turning triage on keeps no new data about the screen.)
        | Tool::MailSettingsSet => ApiLevel::Write(Level::L1),
        Tool::MailTriageArchive | Tool::MailTriageUnarchive => ApiLevel::Write(Level::L2),
        Tool::ExportWritesList | Tool::ExportFoldersList => ApiLevel::Read,
        Tool::ExportWriteFile => ApiLevel::Write(Level::L2),
        Tool::ExportFoldersRemove => ApiLevel::Write(Level::L1),
        // launch a preset agent — level follows the action it runs.
        Tool::ActionsExecute => ApiLevel::PerAction,
        Tool::ActionsStatus => ApiLevel::Read,
        Tool::ActionsQuestions => ApiLevel::Read,
        Tool::ActionsAnswer => ApiLevel::Write(Level::L1),
        Tool::ActionsSteer => ApiLevel::Write(Level::L1),
    }
}

/// Which MCP toolset a tool is listed under (issue #410, `docs/specs/mcp-tool-surface.md`).
///
/// Grouping only decides what `tools/list` **shows** when `SHOGUN_MCP_TOOLSETS` narrows the list.
/// It never decides what runs: `tools/call` resolves every wire name whatever is listed, the level
/// stays [`tool_level`], and REST / CLI ignore groups entirely (invariant 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolGroup {
    /// Live form review and grounded suggestions.
    Forms,
    /// Always listed: the frozen core set (plan §7.1, `docs/api/core-tools.md`; 21 tools): the
    /// day-to-day reads, `memory.append_note`, `memory.ingest`, `memory.recall` and
    /// `actions.execute`. Wire names, input schemas and annotations are frozen by
    /// `tests/wire_freeze.rs`.
    Core,
    /// Memory import and its undo (`memory.import.*`).
    Import,
    /// World-model reads by id, corrections, disputes, split and export (`state.*` beyond core).
    State,
    /// Meeting notes: the four reads that were in core until E4-T3 (list, search, recap,
    /// transcript) plus start, speakers, templates, minutes edits, delete. Served by the Mac app
    /// only, so listed only with feature `desktop`.
    Meeting,
    /// Mail triage, labels, sender rules and write-back history.
    Mail,
    /// Stored screen frames: search, get, rescan, delete.
    VisualRecall,
    /// The voice dictionary.
    Voice,
    /// Skill files, skill runs and schedules, and the Clarify questions a run asks
    /// (`actions.questions` / `actions.answer`, issue #394).
    Skills,
    /// Wiki, work patterns and learned lessons.
    Wiki,
    /// Public web reads.
    Web,
    /// File exports and the allowed export folders.
    Export,
    /// Translating text and drafting replies (issue #279). Text only; nothing is sent.
    Translate,
    /// The `/memories` virtual filesystem (`memory.fs`, E6): the world model as files plus the
    /// caller's scratch files. Served by every build and listed by default (`all`).
    Files,
    /// The A2A state-of-work handoff between agents (E35): `handoff.create` / `handoff.accept`.
    /// Served by every build and listed by default (`all`).
    Handoff,
    /// Agent payment mandates and payments (E46): `payments.*`. Served by every build and listed
    /// by default (`all`).
    Payments,
    /// Governance (E15 / E18): remote approvals, the audit log and the access grants. Reads
    /// only; served by every build and listed by default (`all`).
    Governance,
    /// Preferences that mirror Settings panes, plus device / support status.
    Settings,
}

/// Every group this build serves, in the order docs and warnings list them (E4-T6).
///
/// The default build is the server: it lists the groups the engine answers from the principal's
/// store alone — core, import, state, skills (runs, Clarify questions, steering) and wiki. The
/// groups only the ShogunAI Mac app can serve (meetings, mail triage, screen recall, the voice
/// dictionary, public web reads on the desktop's lane, file exports, translation jobs, the
/// Settings-pane mirrors and live form review) are listed only with feature `desktop`. Listing
/// only: every wire name still resolves on every build ([`ALL_TOOLS`] is the same), so a call by
/// name answers what the backend answers, and the desktop build lists exactly what it did.
#[cfg(feature = "desktop")]
pub const ALL_GROUPS: &[ToolGroup] = EVERY_GROUP;
/// See the `desktop` variant: the server's groups.
#[cfg(not(feature = "desktop"))]
pub const ALL_GROUPS: &[ToolGroup] = &[
    ToolGroup::Core,
    ToolGroup::Files,
    ToolGroup::Handoff,
    ToolGroup::Payments,
    ToolGroup::Governance,
    ToolGroup::Import,
    ToolGroup::State,
    ToolGroup::Skills,
    ToolGroup::Wiki,
];

/// Every group that exists, whatever the build serves: the desktop build's [`ALL_GROUPS`], and
/// the table a test or a doc walks to name every tool's group.
pub const EVERY_GROUP: &[ToolGroup] = &[
    ToolGroup::Core,
    ToolGroup::Files,
    ToolGroup::Handoff,
    ToolGroup::Payments,
    ToolGroup::Governance,
    ToolGroup::Import,
    ToolGroup::State,
    ToolGroup::Meeting,
    ToolGroup::Mail,
    ToolGroup::VisualRecall,
    ToolGroup::Voice,
    ToolGroup::Skills,
    ToolGroup::Wiki,
    ToolGroup::Web,
    ToolGroup::Export,
    ToolGroup::Translate,
    ToolGroup::Settings,
    ToolGroup::Forms,
];

/// Whether this build lists `group` (E4-T6): always on the desktop build; on the server only the
/// groups in [`ALL_GROUPS`].
pub fn group_is_served(group: ToolGroup) -> bool {
    ALL_GROUPS.contains(&group)
}

/// Whether this build lists `tool`'s group. Listing only: the wire name resolves either way.
pub fn tool_is_served(tool: Tool) -> bool {
    group_is_served(tool_group(tool))
}

/// The deprecation note of a tool definition (E4-T5, `docs/api/deprecation-policy.md`): `None`
/// while the tool is current; `Some("<since>: use <replacement>; removed after <date>")` once it
/// is deprecated, surfaced as the `deprecated` field of its `tools/list` descriptor. A wire name
/// never changes in place (invariant 11): a changed tool gets a new name and the old one a note
/// here for the 12-month window.
pub fn tool_deprecation(tool: Tool) -> Option<&'static str> {
    DEPRECATED_TOOLS
        .iter()
        .find(|(deprecated, _)| *deprecated == tool)
        .map(|(_, note)| *note)
}

/// The deprecated tools and their notes (E4-T5). Empty today: a row is added when a tool is
/// deprecated and removed with the tool after its 12-month window.
const DEPRECATED_TOOLS: &[(Tool, &str)] = &[];

impl ToolGroup {
    /// The id a user writes in `SHOGUN_MCP_TOOLSETS`.
    pub fn id(self) -> &'static str {
        match self {
            ToolGroup::Core => "core",
            ToolGroup::Forms => "forms",
            ToolGroup::Import => "import",
            ToolGroup::State => "state",
            ToolGroup::Meeting => "meeting",
            ToolGroup::Mail => "mail",
            ToolGroup::VisualRecall => "visual_recall",
            ToolGroup::Voice => "voice",
            ToolGroup::Skills => "skills",
            ToolGroup::Wiki => "wiki",
            ToolGroup::Web => "web",
            ToolGroup::Export => "export",
            ToolGroup::Translate => "translate",
            ToolGroup::Files => "files",
            ToolGroup::Handoff => "handoff",
            ToolGroup::Payments => "payments",
            ToolGroup::Governance => "governance",
            ToolGroup::Settings => "settings",
        }
    }

    /// Parse a group id (exact, lower-case). Only the groups this build serves parse
    /// ([`ALL_GROUPS`]): on the server `meeting` is unknown, like a typo, so an entry that asks
    /// for it is told rather than shown an empty group.
    pub fn from_id(id: &str) -> Option<ToolGroup> {
        ALL_GROUPS.iter().copied().find(|g| g.id() == id)
    }
}

/// The group a tool is listed under. Exhaustive on purpose: a new [`Tool`] does not compile until
/// someone picks its group, and nothing lands in [`ToolGroup::Core`] by default.
pub fn tool_group(tool: Tool) -> ToolGroup {
    match tool {
        Tool::MemoryProcessingStatus => ToolGroup::State,
        Tool::FormsInspect | Tool::FormsPlan | Tool::FormsStatus | Tool::FormsApply | Tool::FormsClose | Tool::FormsUndo => ToolGroup::Forms,
        // Core (plan §7.1, E4-T3): the frozen set. Reads, `memory.append_note`, `memory.ingest`,
        // `memory.recall` and `actions.execute` (per-action — the only route to an L3 send,
        // which still waits in the approval queue). The four meeting reads moved to `Meeting`.
        Tool::MemorySearch
        | Tool::MemoryGetContext
        | Tool::MemoryGetContextPack
        | Tool::MemoryGetEvents
        | Tool::MemoryReadSession
        | Tool::MemoryGetWrap
        // The routing guide sends "what is coming up" here, so clients must see it.
        | Tool::CalendarUpcoming
        | Tool::MemoryGetPeriods
        | Tool::MemoryAppendNote
        | Tool::MemoryIngest
        | Tool::MemoryRecall
        | Tool::ProfileWhoami
        | Tool::StateCommitmentsList
        | Tool::StateOpenLoopsList
        | Tool::StatePeopleList
        | Tool::StateProjectsList
        | Tool::StatePeopleGet
        | Tool::StateProjectsGet
        | Tool::WikiEvidence
        | Tool::ActionsExecute
        | Tool::ActionsStatus => ToolGroup::Core,
        Tool::MemoryFs => ToolGroup::Files,
        Tool::HandoffCreate | Tool::HandoffAccept => ToolGroup::Handoff,
        Tool::PaymentsMandatesList | Tool::PaymentsMandatesRequest | Tool::PaymentsMandatesRevoke | Tool::PaymentsSpend => {
            ToolGroup::Payments
        }
        Tool::ApprovalsList | Tool::ApprovalsGet | Tool::AuditList | Tool::AclList => ToolGroup::Governance,
        // Imports are user-reviewed flows (preview, then commit what the user chose) that write
        // to memory and carry the largest input schema; not an everyday mid-task tool.
        Tool::MemoryImportPreview
        | Tool::MemoryImport
        | Tool::MemoryImportList
        | Tool::MemoryImportDelete
        // E44: provider webhook deliveries, another way memory is filled from outside.
        | Tool::MemoryIngestWebhook => ToolGroup::Import,
        Tool::MemoryNotesList
        | Tool::MemoryNotesChange
        | Tool::StateCommitmentsGet
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
        | Tool::StateKeepSeparate => ToolGroup::State,
        // The four meeting reads left core in E4-T3 (plan §7.1): the Mac app serves them.
        Tool::MeetingTranscriptGet
        | Tool::MeetingList
        | Tool::MeetingSearch
        | Tool::MeetingRecapGet
        | Tool::MeetingRecordNow
        | Tool::MeetingSystemAudioRetry
        | Tool::MeetingStartStatus
        | Tool::MeetingSystemAudioStatus
        | Tool::MeetingSpeakersGet
        | Tool::MeetingSpeakersSet
        | Tool::MeetingTemplatesGet
        | Tool::MeetingTemplatesSet
        | Tool::MeetingRecapExport
        | Tool::MeetingRecapTrack
        | Tool::MeetingRecapEdit
        | Tool::MeetingDelete => ToolGroup::Meeting,
        Tool::MailTriageList
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
        | Tool::MailHistoryList => ToolGroup::Mail,
        Tool::VisualRecallSearchFrames
        | Tool::VisualRecallGetFrame
        | Tool::VisualRecallFrameTextLines
        | Tool::VisualRecallRescanFrame
        | Tool::VisualRecallDeleteFrame => ToolGroup::VisualRecall,
        Tool::DictionaryAutoAddGet
        | Tool::DictionaryAutoAddSet
        | Tool::VoiceDictionaryList
        | Tool::VoiceDictionaryCreate
        | Tool::VoiceDictionaryUpdate
        | Tool::VoiceDictionaryDelete
        | Tool::VoiceDictionaryImport
        | Tool::VoiceDictionaryExport => ToolGroup::Voice,
        Tool::SkillsList
        | Tool::SkillsRun
        | Tool::SkillsRunStatus
        | Tool::SkillsSave
        | Tool::SchedulesList
        | Tool::SchedulesAdd
        | Tool::SchedulesRemove
        // Clarify (issue #394): the questions agent runs ask. Listed with the runs rather than in
        // core so the always-listed surface (and its descriptor budget) does not grow; both stay
        // callable by name under any toolset.
        | Tool::ActionsQuestions
        | Tool::ActionsAnswer
        // Steering a running run (issue #395) sits with the runs and their questions.
        | Tool::ActionsSteer => ToolGroup::Skills,
        Tool::LessonsList
        | Tool::LessonsSetActive
        | Tool::WikiNeighbors
        | Tool::WikiList
        | Tool::PatternsList
        | Tool::PatternsSetActive => ToolGroup::Wiki,
        Tool::WebVideoTranscript | Tool::WebSocialPost | Tool::WebPage => ToolGroup::Web,
        Tool::ExportWriteFile
        | Tool::ExportWritesList
        | Tool::ExportFoldersList
        | Tool::ExportFoldersRemove => ToolGroup::Export,
        // Issue #279: the two Agent-lane requests and their poll. The register and the pair stay
        // with the other Settings mirrors.
        Tool::TranslateText | Tool::TranslateReplyDraft | Tool::TranslateStatus => {
            ToolGroup::Translate
        }
        // Settings-pane mirrors and device status (issue #410): rarely needed mid-task.
        Tool::MeetingReminderGet
        | Tool::MeetingReminderSet
        | Tool::MeetingMicrophoneGet
        | Tool::MeetingMicrophoneSet
        | Tool::VoiceMicrophoneGet
        | Tool::VoiceMicrophoneSet
        | Tool::UiLanguageGet
        | Tool::UiLanguageSet
        | Tool::TranslateRegisterGet
        | Tool::TranslateRegisterSet
        | Tool::TranslatePairGet
        | Tool::TranslatePairSet
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
        | Tool::MeetingRecapPrefsGet
        | Tool::MeetingRecapPrefsSet
        | Tool::MailSettingsGet
        | Tool::MailSettingsSet
        | Tool::CaptureStatus
        | Tool::CaptureSetPaused
        | Tool::CaptureExclusionsGet
        | Tool::CaptureExclusionsSet
        | Tool::VisualRecallStatus
        | Tool::VisualRecallSetEnabled
        | Tool::VisualRecallSetRetention
        | Tool::BackupStatus
        | Tool::BackupSetRetention
        | Tool::SearchIndexStatus
        | Tool::StoragePreview
        | Tool::SearchIndexCompactStart
        | Tool::SearchIndexCompactStop
        | Tool::DeviceOnboardingGet
        | Tool::DeviceAccountGet
        | Tool::DeviceReleaseNotesGet
        | Tool::DeviceUpdateGet
        | Tool::DreamStatus
        | Tool::DreamRetry
        | Tool::SupportReportsList
        | Tool::ProfileSet => ToolGroup::Settings,
    }
}

/// Tools that change something on Gmail (FR-TG-31): beyond the Memory API itself they need the
/// `agent_execution` entitlement (every paid plan; locked on an expired trial) — the same gate
/// the Mail pane's Archive action sits behind.
pub fn tool_requires_agent_execution(tool: Tool) -> bool {
    // A skill run spends Agent-lane credits exactly like a Notch agent action (Pro, issue #251).
    // A schedule spends it unattended, so creating one needs the same plan (issue #252).
    // A Web read (issue #555) exists for the Pro chat that summarises it on the Agent lane; the
    // API face carries the same plan line so an AI caller cannot fetch what the chat could not.
    // Translating and drafting over the API (issue #279) run on the Agent lane, like a reply draft
    // in the panel, so they carry the same line.
    matches!(
        tool,
        Tool::MailTriageArchive
            | Tool::MailTriageUnarchive
            | Tool::SkillsRun
            | Tool::SchedulesAdd
            | Tool::WebVideoTranscript
            | Tool::WebSocialPost
            | Tool::WebPage
            | Tool::TranslateText
            | Tool::TranslateReplyDraft
            | Tool::FormsPlan
    )
}

/// A state read by id (`state.<table>.get`). An id that names no row is `not_found` on every
/// face (issue #779), like a meeting or a frame, rather than an empty result and exit 0.
pub fn tool_is_state_get(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::StatePeopleGet
            | Tool::StateProjectsGet
            | Tool::StateCommitmentsGet
            | Tool::StateOpenLoopsGet
    )
}

/// Writes the API only records for the desktop to carry out after the user's one tap (the caller
/// sees `pending`): Gmail-side changes (FR-TG-21) and file exports (issue #397). A Gmail archive
/// or unarchive that no standing approval covers is not even recorded: it waits in the approval
/// queue for the user's confirm and answers with an `approval_id` (issue #779,
/// `mail_archive_api`). One the label's standing approval covers is recorded for the desktop's
/// write-back tick.
pub fn tool_write_is_pending(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::MailTriageArchive
            | Tool::MailTriageUnarchive
            | Tool::ExportWriteFile
            | Tool::FormsApply
            | Tool::FormsUndo
    )
}

/// Whether the plan in force allows a tool: the Memory API itself (issue #97, every tool) plus
/// the per-tool `agent_execution` requirement for Gmail write-backs (FR-TG-31). One predicate
/// for every face, so the MCP / REST / CLI gates cannot drift.
pub fn plan_allows(tool: Tool, ent: &Entitlements) -> bool {
    ent.memory_api && (!tool_requires_agent_execution(tool) || ent.agent_execution)
}

/// The standing-approval disclosure (FR-TG-21 / FR-TG-30): what granting `auto_archive` or
/// `sync_to_gmail` on a label means, and why the API refuses to grant either (issue #779). A
/// standing approval lets SHOGUN change Gmail without asking each time, so it is the user's own
/// consent, given in the app — the same way an API caller cannot turn on writing-style sharing,
/// stop excluding an app, or turn on Visual recall by itself. No flag such as
/// `confirm_standing=true` grants it: a client steered by text in an email could set that as
/// easily as any other field. Turning either flag off only narrows and stays an API write.
pub const STANDING_APPROVAL_TEXT: &str = "Standing approval required: threads under this label \
will be changed on Gmail without a per-thread confirmation; each change is logged and can be \
undone for 7 days. Only the user can grant it, in the ShogunAI app (Settings > Connections > \
Triage); an API call cannot, with or without confirm_standing. Turning it off works here.";

// auth (FR-API-03)

/// What an issued Memory API token may do (issue #400, owner decision 2026-09-25).
///
/// Two scopes for now. Finer ones (per toolset group, a `propose` tier between read and write)
/// can be added later as new variants: the scope is stored by name next to the token's verifier,
/// and an unknown name fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenScope {
    /// Only [`ApiLevel::Read`] tools. Every [`ApiLevel::Write`] tool and `actions.execute`
    /// ([`ApiLevel::PerAction`]) is refused with `scope_insufficient`, and MCP `tools/list` hides
    /// them.
    Read,
    /// Every tool, each at its own level: the behaviour of every token before scopes existed.
    Full,
}

impl TokenScope {
    /// Every scope, narrowest first.
    pub const ALL: [TokenScope; 2] = [TokenScope::Read, TokenScope::Full];
    /// The scope a newly issued (mounted) token gets unless the user picks another: read-only.
    pub const DEFAULT_FOR_NEW: TokenScope = TokenScope::Read;
    /// A token issued before scopes existed could call every tool, and keeps doing so.
    pub const LEGACY: TokenScope = TokenScope::Full;

    /// The stored / wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenScope::Read => "read",
            TokenScope::Full => "full",
        }
    }

    /// Parse a stored / wire name (exact, lower-case). Anything else is `None`.
    pub fn parse(value: &str) -> Option<TokenScope> {
        TokenScope::ALL.into_iter().find(|scope| scope.as_str() == value)
    }
}

/// The machine code every face returns when a token's scope does not cover a tool.
pub const SCOPE_INSUFFICIENT: &str = "scope_insufficient";

/// The sentence that goes with [`SCOPE_INSUFFICIENT`]. A read-only token cannot raise itself: the
/// user issues a full-access token in the app.
pub const SCOPE_INSUFFICIENT_MESSAGE: &str = "This Memory API token is read-only. To let this \
client change anything, issue a full-access token in ShogunAI Settings > Memory API.";

/// Whether a token of `scope` may call `tool` (issue #400). The one scope predicate: REST (and so
/// the CLI) applies it in [`crate::rest::route`], MCP in `tools/list` and `tools/call`, the pure
/// gate in [`crate::dispatch::MemoryApi`]. `read` allows exactly the [`ApiLevel::Read`] tools, so
/// a new write tool is refused to read tokens the moment it gets its level.
pub fn scope_allows(scope: TokenScope, tool: Tool) -> bool {
    match scope {
        TokenScope::Full => true,
        TokenScope::Read => tool_level(tool) == ApiLevel::Read,
    }
}

/// The result of authenticating an API call. No token → denied, reads included (FR-API-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthResult {
    /// A valid token, with what it may do.
    Granted(TokenScope),
    DeniedNoToken,
    DeniedInvalidToken,
}

/// One issued token as `api.tokens.list` and Settings show it: never the token or its verifier.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TokenListing {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub scope: TokenScope,
    /// The app the token was issued for from Settings' "Add to …" flow (a
    /// [`crate::client_config::McpClient::id`], e.g. `claude-code`), so "Remove from …" can
    /// revoke it too. `null` for a token issued by name only, or before this existed.
    pub client: Option<String>,
}

struct TokenRecord {
    digest: [u8; 32],
    listing: TokenListing,
}

/// Per-client API tokens (FR-API-03). Real tokens live in the Keychain; this registry holds only
/// SHA-256 digests plus each token's non-secret metadata and scope. REST/HTTP binds to localhost
/// (NFR-SEC-03) — a runtime concern outside this pure model.
#[derive(Default)]
pub struct TokenRegistry {
    records: Vec<TokenRecord>,
    /// Per-app connections (issue #963): connection id and scope. Not secrets; a stdio session
    /// started with `--client <id>` runs while its connection is on this list.
    grants: Vec<crate::memory_api_settings::ClientGrant>,
    /// The token list could not be read (a live reload failed). Every call is refused, and so is
    /// process trust: an unreadable list must not read as "no token issued yet".
    unavailable: bool,
    /// Bearers that verify on their own instead of being listed (E34: the gateway's derived `sgw1`
    /// sidecar bearers, checked by the multi-principal resolver). Asked only for a bearer no
    /// record matched; a `true` grants full scope, like a listed multi-mode token.
    verifier: Option<BearerVerifier>,
}

/// A check for an unlisted bearer ([`TokenRegistry::with_verifier`]).
pub type BearerVerifier = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

impl TokenRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that refuses everything, process trust included: what a live reload
    /// ([`crate::live_tokens::LiveTokens`]) serves when the Keychain list cannot be read.
    pub fn unavailable() -> Self {
        Self {
            records: Vec::new(),
            grants: Vec::new(),
            unavailable: true,
            verifier: None,
        }
    }

    /// Also accept bearers `verifier` answers `true` for (E34, multi-principal mode only), with
    /// full scope. Listed records are scanned first, whole, as before.
    #[must_use]
    pub fn with_verifier(mut self, verifier: BearerVerifier) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Register everything a Keychain token list holds: each token's verifier and scope, and each
    /// app connection (issue #963). The one loader the standalone faces use.
    pub fn register_blob(
        &mut self,
        blob: &crate::memory_api_settings::TokenBlob,
    ) -> Result<(), String> {
        for token in &blob.tokens {
            self.issue_record(token)?;
        }
        for grant in &blob.grants {
            self.grant(grant);
        }
        Ok(())
    }

    /// Register a persisted connection (issue #963).
    pub fn grant(&mut self, grant: &crate::memory_api_settings::ClientGrant) {
        self.grants.push(grant.clone());
    }

    /// Authenticate a stdio session that names its app (`shogun-mcp --client <id>`, issue #963):
    /// the connection's scope, or `None` when that app is not connected (never added, removed, or
    /// the list cannot be read). The app id is resolved to its connection id first, so either app
    /// sharing a config file finds the shared connection. No process trust on this path: naming
    /// an app is asking for that app's connection, which must exist.
    pub fn authenticate_client(&self, client: &str) -> Option<TokenScope> {
        self.client_access(client).map(|(scope, _)| scope)
    }

    /// [`TokenRegistry::authenticate_client`] plus whether the connection may read meeting notes
    /// and screen recall (owner decision 2026-10-20, [`crate::source_policy`]): what the stdio
    /// face runs each call under.
    pub fn client_access(
        &self,
        client: &str,
    ) -> Option<(TokenScope, crate::source_policy::SensitiveSources)> {
        if self.unavailable {
            return None;
        }
        let id = crate::memory_api_settings::grant_id(client).unwrap_or(client);
        self.grants
            .iter()
            .find(|grant| grant.client == id)
            .map(|grant| (grant.scope, grant.sensitive_sources))
    }

    /// Every app connection as `api.tokens.list` shows it (issue #963). Not secret.
    pub fn connections(&self) -> Vec<crate::memory_api_settings::GrantListing> {
        crate::memory_api_settings::grant_listing(&crate::memory_api_settings::TokenBlob {
            tokens: Vec::new(),
            grants: self.grants.clone(),
        })
    }

    /// Register a raw token with the legacy scope (`full`): the dev/test path and the non-macOS
    /// `SHOGUN_API_TOKEN` env token, which predate scopes.
    pub fn issue(&mut self, token: impl Into<String>) {
        self.issue_scoped(token, TokenScope::LEGACY);
    }

    /// Register a raw token with an explicit scope (tests and the dev env path).
    pub fn issue_scoped(&mut self, token: impl Into<String>, scope: TokenScope) {
        self.records.push(TokenRecord {
            digest: verifier_bytes(&token.into()),
            listing: TokenListing {
                id: "env".into(),
                name: crate::client_config::API_TOKEN_ENV.into(),
                created_at: 0,
                scope,
                client: None,
            },
        });
    }

    /// Register a persisted Keychain record: its verifier (never a raw token, so raw input cannot
    /// self-register), its metadata, and its scope. There is deliberately no way to register a
    /// verifier without a scope, so a composition root cannot silently widen a read token.
    pub fn issue_record(
        &mut self,
        record: &crate::memory_api_settings::IssuedToken,
    ) -> Result<(), String> {
        let digest = crate::memory_api_settings::persisted_verifier_bytes(&record.verifier)?;
        self.records.push(TokenRecord {
            digest,
            listing: TokenListing {
                id: record.id.clone(),
                name: record.name.clone(),
                created_at: record.created_at,
                scope: record.scope,
                client: record.client.clone(),
            },
        });
        Ok(())
    }

    /// Whether this is [`TokenRegistry::unavailable`]: the list could not be read.
    pub fn is_unavailable(&self) -> bool {
        self.unavailable
    }

    /// Whether nothing has been issued: no bearer record and no app connection. Diagnostics only
    /// (a start-up note); it grants nothing (audit 2026-10-20 M5).
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.grants.is_empty() && !self.unavailable
    }

    /// Revoke a token id (Full UI revocation).
    pub fn revoke(&mut self, token: &str) {
        let digest = verifier_bytes(token);
        self.records
            .retain(|stored| stored.digest.ct_eq(&digest).unwrap_u8() == 0);
    }

    /// Authenticate a call. `None` (no token presented) is denied — reads included (FR-API-03).
    /// A granted call carries the token's scope; if the same token were somehow on file twice
    /// with different scopes, the narrower one wins.
    pub fn authenticate(&self, token: Option<&str>) -> AuthResult {
        match token {
            None => AuthResult::DeniedNoToken,
            Some(_) if self.unavailable => AuthResult::DeniedInvalidToken,
            Some(token)
                if token.len() <= 512
                    && !token.starts_with(crate::memory_api_settings::TOKEN_VERIFIER_PREFIX) =>
            {
                let digest = verifier_bytes(token);
                let mut matched = 0u8;
                let mut read_only = 0u8;
                for stored in &self.records {
                    // Full scan: a caller cannot learn which client record matched.
                    let hit = stored.digest.ct_eq(&digest).unwrap_u8();
                    matched |= hit;
                    read_only |= hit & u8::from(stored.listing.scope == TokenScope::Read);
                }
                match (matched, read_only) {
                    (1, 1) => AuthResult::Granted(TokenScope::Read),
                    (1, _) => AuthResult::Granted(TokenScope::Full),
                    _ if self.verifier.as_ref().is_some_and(|verify| verify(token)) => {
                        AuthResult::Granted(TokenScope::Full)
                    }
                    _ => AuthResult::DeniedInvalidToken,
                }
            }
            Some(_) => AuthResult::DeniedInvalidToken,
        }
    }

    /// Authenticate a stdio process that names no app (no `--client`): the scope of the token it
    /// presents, or `None` to refuse it.
    ///
    /// There is no process trust (audit 2026-10-20 M5 / F1). Until then an empty registry (no
    /// app connected, no token issued) admitted any local process with full scope, and an empty
    /// registry is exactly what "Delete everything" and the last "Remove from …" leave behind, so
    /// any program running as the user could read and change the whole memory. Now an entry must
    /// name a connected app (`--client <id>`, [`TokenRegistry::authenticate_client`]) or present
    /// an issued token, whatever the registry holds. Pre-#963 entries were written with a token
    /// (`SHOGUN_API_TOKEN`), which still works; an entry with neither is re-added from Settings
    /// (or by auto-connect), which writes the `--client` form.
    pub fn authenticate_process(&self, token: Option<&str>) -> Option<TokenScope> {
        if self.unavailable {
            return None;
        }
        match self.authenticate(token) {
            AuthResult::Granted(scope) => Some(scope),
            AuthResult::DeniedNoToken | AuthResult::DeniedInvalidToken => None,
        }
    }

    /// The issued id of `token` when it is on the list (the per-client rate limit counts a token
    /// session by it, never by the token). Scans every record, like [`TokenRegistry::authenticate`].
    pub fn token_id(&self, token: &str) -> Option<String> {
        if self.unavailable
            || token.len() > 512
            || token.starts_with(crate::memory_api_settings::TOKEN_VERIFIER_PREFIX)
        {
            return None;
        }
        let digest = verifier_bytes(token);
        let mut found = None;
        for stored in &self.records {
            if stored.digest.ct_eq(&digest).unwrap_u8() == 1 && found.is_none() {
                found = Some(stored.listing.id.clone());
            }
        }
        found
    }

    /// Every registered token's metadata and scope, in issue order. Never a token or a verifier.
    pub fn listing(&self) -> Vec<TokenListing> {
        self.records.iter().map(|record| record.listing.clone()).collect()
    }
}

/// The id of a tool's level as the "what the agent can see" panel shows it.
pub fn api_level_id(level: ApiLevel) -> &'static str {
    match level {
        ApiLevel::Read => "read",
        ApiLevel::Write(Level::L1) => "l1",
        ApiLevel::Write(Level::L2) => "l2",
        ApiLevel::Write(Level::L3) => "l3",
        ApiLevel::PerAction => "per_action",
    }
}

/// One tool in [`access_listing`]: its wire name, its level, and every scope that may call it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolAccess {
    pub name: &'static str,
    /// [`api_level_id`] of [`tool_level`].
    pub level: &'static str,
    /// The scopes [`scope_allows`] lets call it, narrowest first.
    pub scopes: Vec<TokenScope>,
}

/// One toolset group in [`access_listing`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GroupAccess {
    /// [`ToolGroup::id`].
    pub group: &'static str,
    pub tools: Vec<ToolAccess>,
}

/// What a mounted agent can call, per token scope (issue #400, "what the agent can see"): every
/// tool in its [`tool_group`], in [`ALL_GROUPS`] then [`ALL_TOOLS`] order, with its level and the
/// scopes [`scope_allows`] grants. Settings and `api.tokens.list` show this list; neither decides
/// anything from it.
pub fn access_listing() -> Vec<GroupAccess> {
    ALL_GROUPS
        .iter()
        .map(|&group| GroupAccess {
            group: group.id(),
            tools: ALL_TOOLS
                .iter()
                .copied()
                .filter(|&tool| tool_group(tool) == group)
                .map(|tool| ToolAccess {
                    name: tool.wire_name(),
                    level: api_level_id(tool_level(tool)),
                    scopes: TokenScope::ALL
                        .into_iter()
                        .filter(|&scope| scope_allows(scope, tool))
                        .collect(),
                })
                .collect(),
        })
        .collect()
}

/// The `api.tokens.list` body, the same on MCP, REST and the CLI: every token's metadata and
/// scope, plus the caller's own scope. Built from [`TokenListing`], which has no field that could
/// carry a token or a verifier.
///
/// Issue #400 adds two generated, token-free blocks the Settings panel shows too: `access`
/// ([`access_listing`]) and `mount` ([`crate::client_config::mount_setup`]). The API faces fill
/// the entries with the docs placeholders: a response never carries this Mac's paths (the user
/// name is in them) to a caller whose transcript may leave the device; Settings fills in the
/// real database path locally.
pub fn render_token_listing(
    tokens: &[TokenListing],
    connections: &[crate::memory_api_settings::GrantListing],
    caller_scope: TokenScope,
) -> String {
    use crate::client_config::{mount_setup, ServerParams, API_TOKEN_ENV, SERVER_NAME};
    let docs = ServerParams::DOCS;
    serde_json::json!({
        "tokens": tokens,
        // The apps SHOGUN is connected to (issue #963): which, and with what scope. Made and
        // widened only in the app's Settings, like tokens.
        "connections": connections,
        "caller_scope": caller_scope,
        "scopes": TokenScope::ALL,
        "default_scope": TokenScope::DEFAULT_FOR_NEW,
        "access": access_listing(),
        "mount": {
            "server_name": SERVER_NAME,
            "token_env": API_TOKEN_ENV,
            "clients": mount_setup(docs.command, docs.db_path),
            "discovery": crate::client_discovery::scan(),
        },
    })
    .to_string()
}

fn verifier_bytes(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

// read confidence rule (FR-API-06)

/// Whether/how a read result item is included in an API response (FR-API-06). Mirrors the UI
/// confidence rule (FR-ST-20) so API and UI reads agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadInclusion {
    /// Excluded from the response (Low confidence, and low not explicitly requested).
    Excluded,
    /// Included, with the `possibly` flag the response must carry.
    Included { possibly: bool },
}

/// Decide inclusion for a read item of the given confidence. Low (<0.5) is excluded unless
/// `include_low` is set (explicit opt-in); Medium is included and flagged `possibly`; High is
/// included plainly (FR-API-06).
pub fn read_inclusion(confidence: f64, include_low: bool) -> ReadInclusion {
    match band(confidence) {
        Band::High => ReadInclusion::Included { possibly: false },
        Band::Medium => ReadInclusion::Included { possibly: true },
        Band::Low => {
            if include_low {
                ReadInclusion::Included { possibly: true }
            } else {
                ReadInclusion::Excluded
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tools_are_reads_writes_carry_expected_levels() {
        assert_eq!(tool_level(Tool::MemorySearch), ApiLevel::Read);
        assert_eq!(tool_level(Tool::StatePeopleGet), ApiLevel::Read);
        assert_eq!(
            tool_level(Tool::MemoryAppendNote),
            ApiLevel::Write(Level::L1)
        );
        assert_eq!(tool_level(Tool::ActionsExecute), ApiLevel::PerAction);
    }

    #[test]
    fn clarify_tools_are_a_read_and_an_l1_answer_listed_with_the_runs() {
        assert_eq!(Tool::from_wire("actions.questions"), Some(Tool::ActionsQuestions));
        assert_eq!(Tool::from_wire("actions.answer"), Some(Tool::ActionsAnswer));
        assert_eq!(tool_level(Tool::ActionsQuestions), ApiLevel::Read);
        assert_eq!(tool_level(Tool::ActionsAnswer), ApiLevel::Write(Level::L1));
        assert!(!tool_write_is_pending(Tool::ActionsAnswer));
        assert!(!tool_requires_agent_execution(Tool::ActionsAnswer));
        assert_eq!(tool_group(Tool::ActionsQuestions), ToolGroup::Skills);
        assert_eq!(tool_group(Tool::ActionsAnswer), ToolGroup::Skills);
        // actions.status stays the L3 poller; it is not the question tool.
        assert_ne!(tool_group(Tool::ActionsStatus), tool_group(Tool::ActionsAnswer));
    }

    #[test]
    fn steering_is_an_l1_note_listed_with_the_runs() {
        assert_eq!(Tool::from_wire("actions.steer"), Some(Tool::ActionsSteer));
        assert_eq!(Tool::ActionsSteer.wire_name(), "actions.steer");
        assert_eq!(tool_level(Tool::ActionsSteer), ApiLevel::Write(Level::L1));
        assert!(!tool_write_is_pending(Tool::ActionsSteer));
        assert!(!tool_requires_agent_execution(Tool::ActionsSteer));
        assert_eq!(tool_group(Tool::ActionsSteer), ToolGroup::Skills);
        assert!(!scope_allows(TokenScope::Read, Tool::ActionsSteer), "a read token cannot steer");
    }

    #[test]
    fn onboarding_state_is_a_symmetric_read_tool() {
        // Invariant 6 (issue #6): the device's onboarding/first-run state is readable from the
        // agent side, on the same surface as every other read, at Read level.
        assert_eq!(
            Tool::from_wire("device.onboarding.get"),
            Some(Tool::DeviceOnboardingGet)
        );
        assert_eq!(
            Tool::DeviceOnboardingGet.wire_name(),
            "device.onboarding.get"
        );
        assert_eq!(tool_level(Tool::DeviceOnboardingGet), ApiLevel::Read);
        assert!(ALL_TOOLS.contains(&Tool::DeviceOnboardingGet));
    }

    #[test]
    fn account_status_is_a_symmetric_read_tool() {
        // Invariant 6 (#148 P2, FR-ACC-07): the account is readable on every face, at Read level;
        // there is no account write tool at all (sign-in and creation are UI-only).
        assert_eq!(Tool::from_wire("device.account.get"), Some(Tool::DeviceAccountGet));
        assert_eq!(Tool::DeviceAccountGet.wire_name(), "device.account.get");
        assert_eq!(tool_level(Tool::DeviceAccountGet), ApiLevel::Read);
        assert!(ALL_TOOLS.contains(&Tool::DeviceAccountGet));
        assert!(
            ALL_TOOLS.iter().all(|t| !t.wire_name().starts_with("device.account.") || *t == Tool::DeviceAccountGet),
            "no account tool besides the read"
        );
    }

    #[test]
    fn release_notes_are_a_symmetric_read_tool() {
        // Invariant 6 (issue #67): what the What's new modal shows is readable from the agent
        // side too, on the same surface, at Read level.
        assert_eq!(
            Tool::from_wire("device.release_notes.get"),
            Some(Tool::DeviceReleaseNotesGet)
        );
        assert_eq!(tool_level(Tool::DeviceReleaseNotesGet), ApiLevel::Read);
        assert!(ALL_TOOLS.contains(&Tool::DeviceReleaseNotesGet));
    }

    #[test]
    fn update_status_is_read_only_on_every_face() {
        // Issue #1296: the state is readable; install / restart / the switch have no tool.
        assert_eq!(Tool::from_wire("device.update.get"), Some(Tool::DeviceUpdateGet));
        assert_eq!(tool_level(Tool::DeviceUpdateGet), ApiLevel::Read);
        assert!(ALL_TOOLS.contains(&Tool::DeviceUpdateGet));
        for name in ["device.update.install", "device.update.restart", "device.update.set"] {
            assert_eq!(Tool::from_wire(name), None);
        }
    }

    #[test]
    fn dream_retry_is_an_l1_settings_write_and_read_tokens_cannot_call_it() {
        assert_eq!(Tool::from_wire("dream.retry"), Some(Tool::DreamRetry));
        assert_eq!(Tool::DreamRetry.wire_name(), "dream.retry");
        assert_eq!(tool_level(Tool::DreamRetry), ApiLevel::Write(Level::L1));
        assert_eq!(tool_group(Tool::DreamRetry), ToolGroup::Settings);
        assert!(!scope_allows(TokenScope::Read, Tool::DreamRetry));
        assert!(ALL_TOOLS.contains(&Tool::DreamRetry));
    }

    #[test]
    fn dream_status_is_a_symmetric_read_tool() {
        // Invariant 6 (issue #285): the Settings row's nightly status and its morning reason are
        assert_eq!(Tool::from_wire("dream.status"), Some(Tool::DreamStatus));
        assert_eq!(Tool::DreamStatus.wire_name(), "dream.status");
        assert_eq!(tool_level(Tool::DreamStatus), ApiLevel::Read);
        assert_eq!(tool_group(Tool::DreamStatus), ToolGroup::Settings);
        assert!(!tool_requires_agent_execution(Tool::DreamStatus));
        assert!(crate::visual_recall_api::is_structured_read(Tool::DreamStatus));
        assert!(ALL_TOOLS.contains(&Tool::DreamStatus));
    }

    #[test]
    fn search_index_compaction_is_a_settings_mirror_at_l1() {
        for (wire, tool, level) in [
            ("search_index.status", Tool::SearchIndexStatus, ApiLevel::Read),
            ("search_index.compact.start", Tool::SearchIndexCompactStart, ApiLevel::Write(Level::L1)),
            ("search_index.compact.stop", Tool::SearchIndexCompactStop, ApiLevel::Write(Level::L1)),
        ] {
            assert_eq!(Tool::from_wire(wire), Some(tool));
            assert_eq!(tool.wire_name(), wire);
            assert_eq!(tool_level(tool), level, "{wire}");
            assert_eq!(tool_group(tool), ToolGroup::Settings, "{wire}");
            assert!(!tool_requires_agent_execution(tool), "{wire}");
            assert!(!tool_write_is_pending(tool), "{wire}: the job runs; nothing waits for a tap");
        }
    }

    #[test]
    fn web_reads_are_reads_that_need_agent_execution() {
        // Issue #555: three reads on the wire, all Pro (the chat they serve is Agent lane).
        for (wire, tool) in [
            ("web.video_transcript", Tool::WebVideoTranscript),
            ("web.social_post", Tool::WebSocialPost),
            ("web.page", Tool::WebPage),
        ] {
            assert_eq!(Tool::from_wire(wire), Some(tool));
            assert_eq!(tool_level(tool), ApiLevel::Read);
            assert!(tool_requires_agent_execution(tool), "{wire} must need agent execution");
        }
    }

    #[test]
    fn read_scope_allows_exactly_the_read_tools_and_full_allows_all() {
        // Issue #400: derived from the tool table, so a new write tool is refused to read tokens
        // the moment it is given its level.
        let mut refused = 0;
        for &t in ALL_TOOLS {
            assert!(scope_allows(TokenScope::Full, t), "{}", t.wire_name());
            let read = tool_level(t) == ApiLevel::Read;
            assert_eq!(scope_allows(TokenScope::Read, t), read, "{}", t.wire_name());
            refused += usize::from(!read);
        }
        assert!(refused > 0);
        assert!(!scope_allows(TokenScope::Read, Tool::ActionsExecute));
        assert!(scope_allows(TokenScope::Read, Tool::ApiTokensList));
        assert!(!scope_allows(TokenScope::Read, Tool::BackupSetRetention));
        assert!(scope_allows(TokenScope::Read, Tool::BackupStatus));
        assert!(scope_allows(TokenScope::Read, Tool::DreamStatus));
        assert!(!scope_allows(TokenScope::Read, Tool::ConnectionsDisconnect));
        assert!(!scope_allows(
            TokenScope::Read,
            Tool::ConnectionsComposioSet
        ));
        assert!(!scope_allows(TokenScope::Read, Tool::ConnectionsGmailSet));
        assert!(scope_allows(TokenScope::Read, Tool::SearchIndexStatus));
        assert!(!scope_allows(TokenScope::Read, Tool::SearchIndexCompactStart));
        assert!(!scope_allows(TokenScope::Read, Tool::SearchIndexCompactStop));
        assert_eq!(TokenScope::DEFAULT_FOR_NEW, TokenScope::Read);
        assert_eq!(TokenScope::LEGACY, TokenScope::Full);
        for scope in TokenScope::ALL {
            assert_eq!(TokenScope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(TokenScope::parse("Full"), None);
        assert_eq!(TokenScope::parse("write"), None);
    }

    #[test]
    fn registry_grants_each_token_its_scope_and_lists_no_secret() {
        let mut tokens = TokenRegistry::new();
        // Audit 2026-10-20 M5: an empty registry trusts no process.
        assert_eq!(tokens.authenticate_process(None), None, "no process trust before any token");
        assert_eq!(tokens.authenticate_process(Some("anything")), None);
        let mut blob = crate::memory_api_settings::TokenBlob::default();
        crate::memory_api_settings::issue_into(&mut blob, "r".into(), "Cursor", 7, "shogun_reader", TokenScope::Read).unwrap();
        crate::memory_api_settings::issue_into(&mut blob, "f".into(), "Codex", 8, "shogun_writer", TokenScope::Full).unwrap();
        for record in &blob.tokens {
            tokens.issue_record(record).unwrap();
        }
        assert_eq!(tokens.authenticate(Some("shogun_reader")), AuthResult::Granted(TokenScope::Read));
        assert_eq!(tokens.authenticate(Some("shogun_writer")), AuthResult::Granted(TokenScope::Full));
        assert_eq!(tokens.authenticate(Some("nope")), AuthResult::DeniedInvalidToken);
        assert_eq!(tokens.authenticate_process(Some("shogun_reader")), Some(TokenScope::Read));
        assert_eq!(tokens.authenticate_process(None), None);
        // The narrower scope wins if one token were on file twice.
        tokens.issue_scoped("shogun_writer", TokenScope::Read);
        assert_eq!(tokens.authenticate(Some("shogun_writer")), AuthResult::Granted(TokenScope::Read));
        let body = render_token_listing(&tokens.listing(), &[], TokenScope::Read);
        for secret in ["shogun_reader", "shogun_writer", "sha256:", &blob.tokens[0].verifier] {
            assert!(!body.contains(secret), "{body}");
        }
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["caller_scope"], "read");
        assert_eq!(v["default_scope"], "read");
        assert_eq!(v["tokens"][0]["scope"], "read");
        assert_eq!(v["tokens"][1]["scope"], "full");
        assert_eq!(v["tokens"][0]["name"], "Cursor");
        let mut keys: Vec<&String> = v["tokens"][0].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["client", "created_at", "id", "name", "scope"], "metadata only");
        assert_eq!(v["tokens"][0]["client"], serde_json::Value::Null);
    }

    #[test]
    fn access_listing_is_every_tool_once_with_the_scope_predicate() {
        // Issue #400, "what the agent can see": derived from the tool table, never hand-kept.
        let listing = access_listing();
        assert_eq!(listing.len(), ALL_GROUPS.len());
        let mut seen = 0;
        let mut read_only = 0;
        for (group, entry) in ALL_GROUPS.iter().zip(&listing) {
            assert_eq!(entry.group, group.id());
            for access in &entry.tools {
                let tool = Tool::from_wire(access.name).unwrap();
                assert_eq!(tool_group(tool), *group);
                assert_eq!(access.level, api_level_id(tool_level(tool)));
                let expected: Vec<TokenScope> =
                    TokenScope::ALL.into_iter().filter(|&s| scope_allows(s, tool)).collect();
                assert_eq!(access.scopes, expected, "{}", access.name);
                seen += 1;
                read_only += usize::from(access.scopes.contains(&TokenScope::Read));
            }
        }
        // The listing covers the groups this build serves (E4-T6): every tool on the desktop.
        let served: Vec<Tool> = ALL_TOOLS.iter().copied().filter(|&t| tool_is_served(t)).collect();
        assert_eq!(seen, served.len());
        assert_eq!(served.len() == ALL_TOOLS.len(), cfg!(feature = "desktop"));
        assert_eq!(read_only, served.iter().filter(|&&t| scope_allows(TokenScope::Read, t)).count());
        let core = &listing[0];
        assert_eq!(core.group, "core");
        let execute = core.tools.iter().find(|t| t.name == "actions.execute").unwrap();
        assert_eq!((execute.level, execute.scopes.as_slice()), ("per_action", &[TokenScope::Full][..]));
    }

    /// Audit 2026-10-20 M5 / F1: what "Delete everything" and the last "Remove from …" leave
    /// behind (an empty token list) refuses a stdio process with neither a connection nor a
    /// token; a granted `--client` works.
    #[test]
    fn an_empty_registry_refuses_and_a_granted_client_works() {
        use crate::memory_api_settings::{grant_into, GrantOrigin, TokenBlob};
        let mut registry = TokenRegistry::new();
        registry.register_blob(&TokenBlob::default()).unwrap();
        assert!(registry.is_empty());
        assert_eq!(registry.authenticate_process(None), None);
        assert_eq!(registry.authenticate_process(Some("")), None);
        assert_eq!(registry.authenticate_process(Some("shogun_guess")), None);
        assert_eq!(registry.authenticate_client("cursor"), None);
        let mut blob = TokenBlob::default();
        grant_into(&mut blob, "cursor", TokenScope::Full, 1, GrantOrigin::User).unwrap();
        let mut granted = TokenRegistry::new();
        granted.register_blob(&blob).unwrap();
        assert_eq!(granted.authenticate_client("cursor"), Some(TokenScope::Full));
        assert_eq!(granted.authenticate_process(None), None, "a grant is not process trust");
    }

    #[test]
    fn an_app_connection_admits_only_that_app_and_ends_process_trust() {
        use crate::memory_api_settings::{grant_into, GrantOrigin, TokenBlob};
        let mut blob = TokenBlob::default();
        grant_into(&mut blob, "chatgpt-desktop", TokenScope::Read, 1, GrantOrigin::User).unwrap();
        let mut registry = TokenRegistry::new();
        assert_eq!(registry.authenticate_process(None), None);
        assert_eq!(registry.authenticate_client("codex-cli"), None, "nothing connected yet");
        registry.register_blob(&blob).unwrap();
        // Either app sharing the file finds the shared connection; others find nothing.
        assert_eq!(registry.authenticate_client("codex-cli"), Some(TokenScope::Read));
        assert_eq!(registry.authenticate_client("chatgpt-desktop"), Some(TokenScope::Read));
        assert_eq!(registry.authenticate_client("cursor"), None);
        assert_eq!(registry.authenticate_client("some-new-app"), None);
        // Once an app is connected, an entry with neither a connection nor a token is refused.
        assert!(!registry.is_empty());
        assert_eq!(registry.authenticate_process(None), None);
        // A connection is not a bearer: no token authenticates through it.
        assert_eq!(registry.authenticate(Some("codex-cli")), AuthResult::DeniedInvalidToken);
        // An unreadable list refuses connections too.
        assert_eq!(TokenRegistry::unavailable().authenticate_client("codex-cli"), None);
    }

    #[test]
    fn token_listing_carries_access_and_a_token_free_mount_setup() {
        let body = render_token_listing(&[], &[], TokenScope::Read);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["access"], serde_json::to_value(access_listing()).unwrap());
        let mount = &v["mount"];
        assert_eq!(mount["server_name"], crate::client_config::SERVER_NAME);
        assert!(mount["discovery"]["tools"].is_array());
        assert_eq!(mount["token_env"], crate::client_config::API_TOKEN_ENV);
        let clients = mount["clients"].as_array().unwrap();
        assert_eq!(clients.len(), crate::client_config::ALL_CLIENTS.len());
        assert_eq!(clients[0]["id"], "claude-code");
        assert!(clients[0]["cli_command"].as_str().unwrap().starts_with("claude mcp add "));
        // One-click setup is previewed, never applied, from here: every client says so.
        let methods: Vec<&str> =
            clients.iter().map(|c| c["install"]["method"].as_str().unwrap()).collect();
        assert_eq!(methods, ["vendor_cli", "config_file", "config_file", "config_file", "config_file"]);
        assert_eq!(v["connections"], serde_json::json!([]));
        assert!(clients.iter().all(|c| c["install"]["apply"] == "in_app_only"));
        assert!(!crate::client_install::looks_like_token(&body), "{body}");
        // The API faces never carry this Mac's paths: only the docs placeholders.
        let docs = crate::client_config::ServerParams::DOCS;
        assert!(body.contains(docs.db_path) && body.contains(docs.command));
        assert_eq!(body.matches("/Users/").count(), body.matches("/Users/YOU/").count(), "{body}");
    }

    #[test]
    fn keychain_blob_loads_into_the_registry_with_each_scope() -> Result<(), String> {
        // The exact shape of `load_token_registry` in the shogun-api / shogun-mcp binaries (their
        // Keychain half is macOS-only): a pre-scope record and a read record, one blob.
        let raw = format!(
            r#"{{"tokens":[{{"id":"old","name":"a","created_at":1,"verifier":"{}"}},{{"id":"new","name":"b","created_at":2,"verifier":"{}","scope":"read"}}]}}"#,
            crate::memory_api_settings::token_verifier("legacy"),
            crate::memory_api_settings::token_verifier("mounted")
        );
        let mut tokens = TokenRegistry::new();
        let blob = crate::memory_api_settings::load_token_blob_with_migration(
            || Ok(Some(raw.into_bytes())),
            |_| Err("no migration expected".to_string()),
        )?;
        for token in &blob.tokens {
            tokens.issue_record(token)?;
        }
        assert_eq!(tokens.authenticate(Some("legacy")), AuthResult::Granted(TokenScope::Full));
        assert_eq!(tokens.authenticate(Some("mounted")), AuthResult::Granted(TokenScope::Read));
        Ok(())
    }

    #[test]
    fn translate_tools_are_text_only_writes_and_never_pending_or_l3() {
        // Issue #279 / invariant 4: translating and drafting produce text, so they are L1 (never
        // an L3 send), never queued for a tap, refused to read-only tokens (they spend the user's
        // model access), and listed in their own group.
        for (tool, wire, level) in [
            (Tool::TranslateText, "translate.text", ApiLevel::Write(Level::L1)),
            (Tool::TranslateReplyDraft, "translate.reply_draft", ApiLevel::Write(Level::L1)),
            (Tool::TranslateStatus, "translate.status", ApiLevel::Read),
            (Tool::TranslatePairGet, "translate.pair.get", ApiLevel::Read),
            (Tool::TranslatePairSet, "translate.pair.set", ApiLevel::Write(Level::L1)),
        ] {
            assert_eq!(Tool::from_wire(wire), Some(tool), "{wire}");
            assert_eq!(tool_level(tool), level, "{wire}");
            assert!(!tool_write_is_pending(tool), "{wire}");
        }
        assert!(!scope_allows(TokenScope::Read, Tool::TranslateText));
        assert!(!scope_allows(TokenScope::Read, Tool::TranslateReplyDraft));
        assert!(scope_allows(TokenScope::Read, Tool::TranslateStatus));
        assert_eq!(tool_group(Tool::TranslateText), ToolGroup::Translate);
        assert_eq!(tool_group(Tool::TranslatePairSet), ToolGroup::Settings);
        // The translate group is the Mac app's: parsed on the desktop build only (E4-T6).
        assert_eq!(ToolGroup::from_id("translate"), cfg!(feature = "desktop").then_some(ToolGroup::Translate));
    }

    #[test]
    fn file_export_is_l2_pending_and_folder_tools_are_symmetric() {
        assert_eq!(Tool::from_wire("export.write_file"), Some(Tool::ExportWriteFile));
        assert_eq!(tool_level(Tool::ExportWriteFile), ApiLevel::Write(Level::L2));
        assert_eq!(
            tool_level(Tool::ExportWriteFile),
            ApiLevel::Write(
                shogun_agents::permission::Action::Local(
                    shogun_agents::permission::LocalAction::WriteFile {
                        path_in_allowed_root: String::new(),
                        format: shogun_agents::permission::ExportFormat::Markdown,
                        body: String::new(),
                    }
                )
                .required_level()
            )
        );
        assert!(tool_write_is_pending(Tool::ExportWriteFile));
        assert_eq!(tool_level(Tool::ExportWritesList), ApiLevel::Read);
        assert_eq!(tool_level(Tool::ExportFoldersList), ApiLevel::Read);
        assert_eq!(tool_level(Tool::ExportFoldersRemove), ApiLevel::Write(Level::L1));
        assert!(!tool_requires_agent_execution(Tool::ExportWriteFile));
        for t in [Tool::ExportWriteFile, Tool::ExportWritesList, Tool::ExportFoldersList, Tool::ExportFoldersRemove] {
            assert_eq!(Tool::from_wire(t.wire_name()), Some(t));
        }
    }

    #[test]
    fn rescanning_a_frame_is_an_l1_local_write() {
        assert_eq!(
            Tool::from_wire("visual_recall.rescan_frame"),
            Some(Tool::VisualRecallRescanFrame)
        );
        assert_eq!(
            tool_level(Tool::VisualRecallRescanFrame),
            ApiLevel::Write(Level::L1)
        );
        assert_eq!(
            tool_level(Tool::VisualRecallRescanFrame),
            tool_level(Tool::VisualRecallDeleteFrame)
        );
        assert!(!tool_write_is_pending(Tool::VisualRecallRescanFrame));
        assert!(!tool_requires_agent_execution(Tool::VisualRecallRescanFrame));
        assert!(!crate::visual_recall_api::is_structured_read(
            Tool::VisualRecallRescanFrame
        ));
    }

    #[test]
    fn visual_recall_settings_writes_have_an_l3_ceiling() {
        // Issue #782: turning Visual recall on or keeping screens longer waits for the user, so
        // neither tool can be cheaper than the approval it may need.
        for tool in [Tool::VisualRecallSetEnabled, Tool::VisualRecallSetRetention] {
            assert_eq!(tool_level(tool), ApiLevel::Write(Level::L3), "{tool:?}");
            assert_eq!(tool_level(tool), tool_level(Tool::CaptureExclusionsSet), "{tool:?}");
            assert!(!tool_write_is_pending(tool), "{tool:?}");
        }
    }

    #[test]
    fn the_agent_source_tools_are_a_settings_read_and_an_l3_ceiling_write() {
        assert_eq!(Tool::from_wire("agent.source.get"), Some(Tool::AgentSourceGet));
        assert_eq!(Tool::from_wire("agent.source.set"), Some(Tool::AgentSourceSet));
        assert_eq!(tool_level(Tool::AgentSourceGet), ApiLevel::Read);
        assert_eq!(tool_level(Tool::AgentSourceSet), ApiLevel::Write(Level::L3));
        assert_eq!(tool_group(Tool::AgentSourceGet), ToolGroup::Settings);
        assert_eq!(tool_group(Tool::AgentSourceSet), ToolGroup::Settings);
        assert!(!tool_write_is_pending(Tool::AgentSourceSet));
        assert!(!tool_requires_agent_execution(Tool::AgentSourceSet));
    }

    #[test]
    fn the_connections_tools_are_a_settings_read_and_direct_narrowing_writes() {
        for (wire, tool, level) in [
            ("connections.list", Tool::ConnectionsList, ApiLevel::Read),
            (
                "connections.disconnect",
                Tool::ConnectionsDisconnect,
                ApiLevel::Write(Level::L1),
            ),
            (
                "connections.composio.set",
                Tool::ConnectionsComposioSet,
                ApiLevel::Write(Level::L1),
            ),
            (
                "connections.gmail.set",
                Tool::ConnectionsGmailSet,
                ApiLevel::Write(Level::L1),
            ),
        ] {
            assert_eq!(Tool::from_wire(wire), Some(tool));
            assert_eq!(tool.wire_name(), wire);
            assert_eq!(tool_level(tool), level);
            assert_eq!(tool_group(tool), ToolGroup::Settings);
            assert!(!tool_write_is_pending(tool));
            assert!(!tool_requires_agent_execution(tool));
        }
        assert!(crate::visual_recall_api::is_structured_read(Tool::ConnectionsList));
    }

    #[test]
    fn replacing_capture_exclusions_is_l3() {
        assert_eq!(
            tool_level(Tool::CaptureExclusionsSet),
            ApiLevel::Write(Level::L3)
        );
    }

    #[test]
    fn mail_triage_tools_round_trip_their_wire_names_at_the_ui_levels() {
        let expected = [
            (Tool::MailTriageList, "mail.triage.list", ApiLevel::Read),
            (Tool::MailTriageGet, "mail.triage.get", ApiLevel::Read),
            (
                Tool::MailTriageSet,
                "mail.triage.set",
                ApiLevel::Write(Level::L1),
            ),
            (
                Tool::MailTriageArchive,
                "mail.triage.archive",
                ApiLevel::Write(Level::L2),
            ),
            (
                Tool::MailTriageUnarchive,
                "mail.triage.unarchive",
                ApiLevel::Write(Level::L2),
            ),
            (Tool::MailLabelsList, "mail.labels.list", ApiLevel::Read),
            (
                Tool::MailLabelsUpsert,
                "mail.labels.upsert",
                ApiLevel::Write(Level::L1),
            ),
            (
                Tool::MailLabelsDelete,
                "mail.labels.delete",
                ApiLevel::Write(Level::L1),
            ),
            (
                Tool::MailSenderRulesList,
                "mail.sender_rules.list",
                ApiLevel::Read,
            ),
            (
                Tool::MailSenderRulesAdd,
                "mail.sender_rules.add",
                ApiLevel::Write(Level::L1),
            ),
            (
                Tool::MailSenderRulesDelete,
                "mail.sender_rules.delete",
                ApiLevel::Write(Level::L1),
            ),
            (Tool::MailHistoryList, "mail.history.list", ApiLevel::Read),
        ];
        for (tool, wire, level) in expected {
            assert_eq!(tool.wire_name(), wire);
            assert_eq!(Tool::from_wire(wire), Some(tool), "{wire}");
            assert_eq!(tool_level(tool), level, "{wire}");
            assert!(ALL_TOOLS.contains(&tool), "{wire}");
        }
    }

    #[test]
    fn only_gmail_write_backs_and_skill_runs_need_agent_execution() {
        // FR-TG-31: reads and local corrections ride on the Memory API entitlement alone; the two
        // archive tools additionally need Pro agent execution. A plan that has the API but not
        // agent execution (not a shipped plan today, but the spec's split) sees exactly that.
        let mut api_only = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        api_only.agent_execution = false;
        assert!(plan_allows(Tool::MailTriageList, &api_only));
        assert!(plan_allows(Tool::MailTriageSet, &api_only));
        assert!(plan_allows(Tool::MailLabelsUpsert, &api_only));
        assert!(!plan_allows(Tool::MailTriageArchive, &api_only));
        assert!(!plan_allows(Tool::MailTriageUnarchive, &api_only));
        assert!(plan_allows(
            Tool::MailTriageArchive,
            &shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
        ));
        // No Memory API → nothing, the archive tools included.
        let mut locked = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        locked.memory_api = false;
        assert!(!plan_allows(Tool::MailTriageList, &locked));
        assert!(!plan_allows(Tool::MailTriageArchive, &locked));
        for &t in ALL_TOOLS {
            assert_eq!(
                tool_requires_agent_execution(t),
                matches!(
                    t,
                    Tool::MailTriageArchive
                        | Tool::MailTriageUnarchive
                        | Tool::SkillsRun
                        | Tool::SchedulesAdd
                        | Tool::WebVideoTranscript
                        | Tool::WebSocialPost
                        | Tool::WebPage
                        | Tool::TranslateText
                        | Tool::TranslateReplyDraft
                        | Tool::FormsPlan
                )
            );
        }
        // Issue #279: translating and drafting spend the user's Agent-lane access; reading a
        // parked answer and the pair / register settings do not.
        assert!(!plan_allows(Tool::TranslateText, &api_only));
        assert!(!plan_allows(Tool::TranslateReplyDraft, &api_only));
        assert!(plan_allows(Tool::TranslateStatus, &api_only));
        assert!(plan_allows(Tool::TranslatePairSet, &api_only));
        // A web read (issue #555) is Pro: it serves the Agent-lane chat, never a Standard surface.
        assert!(!plan_allows(Tool::WebPage, &api_only));
        // Listing skills and reading a run are plain reads; asking for a run spends Agent-lane
        // credits (issue #251).
        assert!(plan_allows(Tool::SkillsList, &api_only));
        assert!(!plan_allows(Tool::SkillsRun, &api_only));
    }

    #[test]
    fn standing_approval_text_says_only_the_user_grants_it_in_the_app() {
        assert!(STANDING_APPROVAL_TEXT.starts_with("Standing approval required:"));
        assert!(STANDING_APPROVAL_TEXT.contains("Only the user can grant it, in the ShogunAI app"));
        assert!(!STANDING_APPROVAL_TEXT.contains("confirm_standing=true"), "the flag no longer grants it");
        assert!(STANDING_APPROVAL_TEXT.contains("undone for 7 days"));
    }

    #[test]
    fn lessons_tools_are_symmetric_reads_and_l1_writes() {
        // Invariant 6: the Learned list and its toggle exist on the agent surface at the same
        // levels as the human UI; flipping a lesson can never become a send.
        assert_eq!(Tool::from_wire("lessons.list"), Some(Tool::LessonsList));
        assert_eq!(
            Tool::from_wire("lessons.set_active"),
            Some(Tool::LessonsSetActive)
        );
        assert_eq!(tool_level(Tool::LessonsList), ApiLevel::Read);
        assert_eq!(
            tool_level(Tool::LessonsSetActive),
            ApiLevel::Write(Level::L1)
        );
        assert!(ALL_TOOLS.contains(&Tool::LessonsList));
        assert!(ALL_TOOLS.contains(&Tool::LessonsSetActive));
    }

    #[test]
    fn dispute_tools_are_symmetric_read_and_l1_write() {
        assert_eq!(
            Tool::from_wire("state.disputes.list"),
            Some(Tool::StateDisputesList)
        );
        assert_eq!(
            Tool::from_wire("state.disputes.resolve"),
            Some(Tool::StateDisputesResolve)
        );
        assert_eq!(tool_level(Tool::StateDisputesList), ApiLevel::Read);
        assert_eq!(
            tool_level(Tool::StateDisputesResolve),
            ApiLevel::Write(Level::L1)
        );
        assert!(ALL_TOOLS.contains(&Tool::StateDisputesList));
        assert!(ALL_TOOLS.contains(&Tool::StateDisputesResolve));
        assert!(!tool_requires_agent_execution(Tool::StateDisputesResolve));
    }

    #[test]
    fn memory_import_tools_are_reads_and_l1_local_writes() {
        // Imports stay on the device, so no Pro agent-execution gate.
        for (tool, wire, level) in [
            (Tool::MemoryImportPreview, "memory.import.preview", ApiLevel::Read),
            (Tool::MemoryImport, "memory.import", ApiLevel::Write(Level::L1)),
            (Tool::MemoryImportList, "memory.import.list", ApiLevel::Read),
            (Tool::MemoryImportDelete, "memory.import.delete", ApiLevel::Write(Level::L1)),
        ] {
            assert_eq!(tool.wire_name(), wire);
            assert_eq!(Tool::from_wire(wire), Some(tool));
            assert_eq!(tool_level(tool), level);
            assert!(!tool_requires_agent_execution(tool));
            assert!(!tool_write_is_pending(tool));
            assert!(crate::memory_import_api::is_memory_import_tool(tool));
        }
    }

    #[test]
    fn world_model_correction_tools_are_l1_writes_and_export_is_a_read() {
        for (tool, wire, level) in [
            (Tool::StateUpdate, "state.update", ApiLevel::Write(Level::L1)),
            (Tool::StateDelete, "state.delete", ApiLevel::Write(Level::L1)),
            (Tool::StateCreate, "state.create", ApiLevel::Write(Level::L1)),
            (Tool::StateExport, "state.export", ApiLevel::Read),
            (Tool::StateSplit, "state.split", ApiLevel::Write(Level::L1)),
            (Tool::StateSplitEvidence, "state.split.evidence", ApiLevel::Read),
            (Tool::StateMergeCandidates, "state.merge.candidates", ApiLevel::Read),
            (Tool::StateMerge, "state.merge", ApiLevel::Write(Level::L1)),
            (Tool::StateKeepSeparate, "state.keep_separate", ApiLevel::Write(Level::L1)),
        ] {
            assert_eq!(tool.wire_name(), wire);
            assert_eq!(Tool::from_wire(wire), Some(tool));
            assert_eq!(tool_level(tool), level, "{wire}");
            assert!(ALL_TOOLS.contains(&tool));
            assert!(!tool_requires_agent_execution(tool), "{wire}");
        }
    }

    #[test]
    fn meeting_speaker_tools_are_symmetric_read_and_l1_write() {
        assert_eq!(
            Tool::from_wire("meeting.speakers.get"),
            Some(Tool::MeetingSpeakersGet)
        );
        assert_eq!(
            Tool::from_wire("meeting.speakers.set"),
            Some(Tool::MeetingSpeakersSet)
        );
        assert_eq!(tool_level(Tool::MeetingSpeakersGet), ApiLevel::Read);
        assert_eq!(
            tool_level(Tool::MeetingSpeakersSet),
            ApiLevel::Write(Level::L1)
        );
    }

    #[test]
    fn meeting_recap_and_transcript_reads_are_on_the_api_face() {
        assert_eq!(Tool::from_wire("meeting.recap.get"), Some(Tool::MeetingRecapGet));
        assert_eq!(
            Tool::from_wire("meeting.transcript.get"),
            Some(Tool::MeetingTranscriptGet)
        );
        assert_eq!(tool_level(Tool::MeetingRecapGet), ApiLevel::Read);
        assert_eq!(tool_level(Tool::MeetingTranscriptGet), ApiLevel::Read);
        assert!(ALL_TOOLS.contains(&Tool::MeetingRecapGet));
        assert!(ALL_TOOLS.contains(&Tool::MeetingTranscriptGet));
    }

    #[test]
    fn voice_dictionary_bulk_tools_are_an_l1_write_and_a_read() {
        assert_eq!(Tool::from_wire("voice_dictionary.import"), Some(Tool::VoiceDictionaryImport));
        assert_eq!(Tool::from_wire("voice_dictionary.export"), Some(Tool::VoiceDictionaryExport));
        assert_eq!(tool_level(Tool::VoiceDictionaryImport), ApiLevel::Write(Level::L1));
        assert_eq!(tool_level(Tool::VoiceDictionaryExport), ApiLevel::Read);
    }

    #[test]
    fn meeting_recap_edit_is_an_l1_write() {
        assert_eq!(Tool::from_wire("meeting.recap.edit"), Some(Tool::MeetingRecapEdit));
        assert_eq!(tool_level(Tool::MeetingRecapEdit), ApiLevel::Write(Level::L1));
    }

    #[test]
    fn meeting_search_is_a_read_and_delete_an_l1_write() {
        assert_eq!(Tool::from_wire("meeting.search"), Some(Tool::MeetingSearch));
        assert_eq!(Tool::from_wire("meeting.delete"), Some(Tool::MeetingDelete));
        assert_eq!(tool_level(Tool::MeetingSearch), ApiLevel::Read);
        assert_eq!(tool_level(Tool::MeetingDelete), ApiLevel::Write(Level::L1));
    }

    #[test]
    fn meeting_recap_prefs_are_a_read_and_an_l1_write() {
        assert_eq!(Tool::from_wire("meeting.recap_prefs.get"), Some(Tool::MeetingRecapPrefsGet));
        assert_eq!(Tool::from_wire("meeting.recap_prefs.set"), Some(Tool::MeetingRecapPrefsSet));
        assert_eq!(tool_level(Tool::MeetingRecapPrefsGet), ApiLevel::Read);
        assert_eq!(tool_level(Tool::MeetingRecapPrefsSet), ApiLevel::Write(Level::L1));
    }

    #[test]
    fn calendar_reminder_tools_keep_read_and_write_authorization() {
        assert_eq!(Tool::from_wire("meeting.reminder.get"), Some(Tool::MeetingReminderGet));
        assert_eq!(Tool::from_wire("meeting.reminder.set"), Some(Tool::MeetingReminderSet));
        assert_eq!(tool_level(Tool::MeetingReminderGet), ApiLevel::Read);
        assert_eq!(tool_level(Tool::MeetingReminderSet), ApiLevel::Write(Level::L1));
        assert!(scope_allows(TokenScope::Read, Tool::MeetingReminderGet));
        assert!(!scope_allows(TokenScope::Read, Tool::MeetingReminderSet));
        assert!(scope_allows(TokenScope::Full, Tool::MeetingReminderSet));
        assert!(!tool_write_is_pending(Tool::MeetingReminderSet));
    }

    #[test]
    fn meeting_microphone_tools_are_symmetric_read_and_l1_write() {
        assert_eq!(
            Tool::from_wire("meeting.microphone.get"),
            Some(Tool::MeetingMicrophoneGet)
        );
        assert_eq!(
            tool_level(Tool::MeetingMicrophoneSet),
            ApiLevel::Write(Level::L1)
        );
    }

    /// Issue #279 FR-TL-07 / invariant 6: the register the reply panel's chips set is readable
    /// and settable from the API at the same L1.
    #[test]
    fn translate_register_tools_are_symmetric_read_and_l1_write() {
        assert_eq!(Tool::from_wire("translate.register.get"), Some(Tool::TranslateRegisterGet));
        assert_eq!(Tool::from_wire("translate.register.set"), Some(Tool::TranslateRegisterSet));
        assert_eq!(tool_level(Tool::TranslateRegisterGet), ApiLevel::Read);
        assert_eq!(tool_level(Tool::TranslateRegisterSet), ApiLevel::Write(Level::L1));
    }

    #[test]
    fn meeting_start_is_l2_until_the_human_confirms() {
        assert_eq!(
            Tool::from_wire("meeting.start"),
            Some(Tool::MeetingRecordNow)
        );
        assert_eq!(
            tool_level(Tool::MeetingRecordNow),
            ApiLevel::Write(Level::L2)
        );
        assert!(ALL_TOOLS.contains(&Tool::MeetingRecordNow));
        assert_eq!(
            Tool::from_wire("meeting.start.status"),
            Some(Tool::MeetingStartStatus)
        );
        assert_eq!(tool_level(Tool::MeetingStartStatus), ApiLevel::Read);
        assert!(ALL_TOOLS.contains(&Tool::MeetingStartStatus));
    }

    #[test]
    fn voice_microphone_tools_are_symmetric_read_and_l1_write() {
        assert_eq!(
            Tool::from_wire("voice.microphone.get"),
            Some(Tool::VoiceMicrophoneGet)
        );
        assert_eq!(
            Tool::from_wire("voice.microphone.set"),
            Some(Tool::VoiceMicrophoneSet)
        );
        assert_eq!(tool_level(Tool::VoiceMicrophoneGet), ApiLevel::Read);
        assert_eq!(
            tool_level(Tool::VoiceMicrophoneSet),
            ApiLevel::Write(Level::L1)
        );
        // The dictation input matches the meeting input: same shape, same level (invariant 6).
        assert_eq!(
            tool_level(Tool::VoiceMicrophoneSet),
            tool_level(Tool::MeetingMicrophoneSet)
        );
    }

    #[test]
    fn no_read_tool_is_a_write_and_vice_versa() {
        for &t in ALL_TOOLS {
            match tool_level(t) {
                ApiLevel::Read => assert!(matches!(
                    t,
                    Tool::FormsStatus
                        | Tool::MemoryNotesList
                        | Tool::MemorySearch
                        | Tool::MemoryGetContext
                        | Tool::MemoryGetContextPack
                        | Tool::MemoryGetEvents
                        | Tool::MemoryReadSession
                        | Tool::MemoryRecall
                        | Tool::MemoryProcessingStatus
                        | Tool::HandoffCreate
                        | Tool::PaymentsMandatesList
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
                        | Tool::StateExport
                        | Tool::StateSplitEvidence
                        | Tool::StateMergeCandidates
                        | Tool::MemoryImportPreview
                        | Tool::MemoryImportList
                        | Tool::CaptureStatus
                    | Tool::DeviceOnboardingGet
                    | Tool::DeviceAccountGet
                    | Tool::DeviceReleaseNotesGet
                        | Tool::DreamStatus
                        | Tool::LessonsList
                        | Tool::VisualRecallStatus
                        | Tool::BackupStatus
                        | Tool::SearchIndexStatus
                        | Tool::StoragePreview
                        | Tool::VisualRecallSearchFrames
                        | Tool::VisualRecallGetFrame
                        | Tool::VisualRecallFrameTextLines
                        | Tool::CaptureExclusionsGet
                        | Tool::ProfileWhoami
                        | Tool::DictionaryAutoAddGet
                        | Tool::VoiceDictionaryList
                        | Tool::VoiceDictionaryExport
                        | Tool::MeetingReminderGet
                        | Tool::MeetingMicrophoneGet
                        | Tool::MeetingStartStatus
                        | Tool::MeetingSystemAudioStatus
                        | Tool::VoiceMicrophoneGet
                        | Tool::WritingVoiceGet
                        | Tool::AgentSourceGet
                        | Tool::ConnectionsList
                        | Tool::ApiTokensList
                        | Tool::TranslateRegisterGet
                        | Tool::UiLanguageGet
                        | Tool::DeviceUpdateGet
                        | Tool::MeetingSpeakersGet
                        | Tool::MeetingTemplatesGet
                        | Tool::MeetingRecapExport
                        | Tool::SkillsList
                        | Tool::SkillsRunStatus
                        | Tool::SchedulesList
                        | Tool::SupportReportsList
                        | Tool::MeetingList
                        | Tool::WebVideoTranscript
                        | Tool::WebSocialPost
                        | Tool::WebPage
                        | Tool::MeetingRecapGet
                        | Tool::MeetingTranscriptGet
                        | Tool::MeetingSearch
                        | Tool::MeetingRecapPrefsGet
                        | Tool::WikiEvidence
                        | Tool::WikiNeighbors
                        | Tool::WikiList
                        | Tool::PatternsList
                        | Tool::MailTriageList
                        | Tool::MailTriageGet
                        | Tool::MailLabelsList
                        | Tool::MailSenderRulesList
                        | Tool::MailHistoryList
                        | Tool::MailSettingsGet
                        | Tool::ActionsStatus
                        | Tool::ActionsQuestions
                        | Tool::ApprovalsList
                        | Tool::ApprovalsGet
                        | Tool::AuditList
                        | Tool::AclList
                        | Tool::ExportWritesList
                        | Tool::ExportFoldersList
                        | Tool::TranslateStatus
                        | Tool::TranslatePairGet
                )),
                ApiLevel::Write(_) => {
                    assert!(matches!(
                        t,
                        Tool::FormsInspect
                            | Tool::FormsPlan
                            | Tool::FormsApply
                            | Tool::FormsClose
                            | Tool::FormsUndo
                            | Tool::MemoryNotesChange
                            | Tool::MemoryAppendNote
                            | Tool::MemoryIngest
                            | Tool::MemoryIngestWebhook
                            | Tool::MemoryFs
                            | Tool::HandoffAccept
                            | Tool::PaymentsMandatesRequest
                            | Tool::PaymentsMandatesRevoke
                            | Tool::PaymentsSpend
                            | Tool::CaptureSetPaused
                            | Tool::LessonsSetActive
                            | Tool::StateDisputesResolve
                            | Tool::StateUpdate
                            | Tool::StateDelete
                            | Tool::StateCreate
                            | Tool::StateSplit
                            | Tool::StateMerge
                            | Tool::StateKeepSeparate
                            | Tool::MemoryImport
                            | Tool::MemoryImportDelete
                            | Tool::VisualRecallSetEnabled
                            | Tool::VisualRecallSetRetention
                            | Tool::BackupSetRetention
                            | Tool::SearchIndexCompactStart
                            | Tool::SearchIndexCompactStop
                            | Tool::DreamRetry
                            | Tool::VisualRecallDeleteFrame
                            | Tool::VisualRecallRescanFrame
                            | Tool::CaptureExclusionsSet
                            | Tool::ProfileSet
                            | Tool::DictionaryAutoAddSet
                            | Tool::VoiceDictionaryCreate
                            | Tool::VoiceDictionaryUpdate
                            | Tool::VoiceDictionaryDelete
                            | Tool::VoiceDictionaryImport
                            | Tool::MeetingReminderSet
                            | Tool::MeetingMicrophoneSet
                            | Tool::MeetingRecordNow
                            | Tool::MeetingSystemAudioRetry
                            | Tool::VoiceMicrophoneSet
                            | Tool::WritingVoiceSet
                            | Tool::AgentSourceSet
                            | Tool::ConnectionsDisconnect
                            | Tool::ConnectionsComposioSet
                            | Tool::ConnectionsGmailSet
                            | Tool::ApiConnectionsRestrict
                            | Tool::TranslateRegisterSet
                            | Tool::UiLanguageSet
                            | Tool::MeetingSpeakersSet
                            | Tool::MeetingTemplatesSet
                            | Tool::MeetingRecapTrack
                            | Tool::SkillsRun
                            | Tool::SkillsSave
                            | Tool::SchedulesAdd
                            | Tool::SchedulesRemove
                            | Tool::MeetingRecapPrefsSet
                            | Tool::MeetingRecapEdit
                            | Tool::MeetingDelete
                            | Tool::PatternsSetActive
                            | Tool::MailTriageSet
                            | Tool::MailTriageArchive
                            | Tool::MailTriageUnarchive
                            | Tool::MailLabelsUpsert
                            | Tool::MailLabelsDelete
                            | Tool::MailSenderRulesAdd
                            | Tool::MailSenderRulesDelete
                            | Tool::MailSettingsSet
                            | Tool::ExportWriteFile
                            | Tool::ExportFoldersRemove
                            | Tool::ActionsAnswer
                            | Tool::ActionsSteer
                            | Tool::TranslateText
                            | Tool::TranslateReplyDraft
                            | Tool::TranslatePairSet
                    ))
                }
                ApiLevel::PerAction => assert_eq!(t, Tool::ActionsExecute),
            }
        }
    }

    #[test]
    fn no_token_denies_everything_including_reads() {
        let reg = TokenRegistry::new();
        assert_eq!(reg.authenticate(None), AuthResult::DeniedNoToken);
    }

    #[test]
    fn issued_token_grants_and_revoked_token_denies() {
        let mut reg = TokenRegistry::new();
        reg.issue("client-abc");
        assert_eq!(reg.authenticate(Some("client-abc")), AuthResult::Granted(TokenScope::Full));
        assert_eq!(
            reg.authenticate(Some("unknown")),
            AuthResult::DeniedInvalidToken
        );
        reg.revoke("client-abc");
        assert_eq!(
            reg.authenticate(Some("client-abc")),
            AuthResult::DeniedInvalidToken
        );
    }

    #[test]
    fn read_confidence_rule_matches_ui() {
        // High plain, Medium possibly, Low excluded by default.
        assert_eq!(
            read_inclusion(0.9, false),
            ReadInclusion::Included { possibly: false }
        );
        assert_eq!(
            read_inclusion(0.6, false),
            ReadInclusion::Included { possibly: true }
        );
        assert_eq!(read_inclusion(0.3, false), ReadInclusion::Excluded);
        // low included only on explicit opt-in, flagged possibly.
        assert_eq!(
            read_inclusion(0.3, true),
            ReadInclusion::Included { possibly: true }
        );
    }
}
