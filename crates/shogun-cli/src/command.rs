//! The CLI command model and its mapping to Memory API tools (§6.11, FR-API-02). The `shogun`
//! command is symmetric with the UI: every subcommand resolves to the same [`Tool`] the MCP/REST
//! faces call, at the same permission level — so there is no CLI-only capability (invariant 6).

use shogun_mcp::backend::{MemorySourceFamilyV1, SearchDepthV1};
use shogun_mcp::memory_api::Tool;
pub use shogun_mcp::forms_api::FormOperation;

/// A `list` or `get <id>` on a state noun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListOrGet {
    List,
    Get { id: i64 },
}

/// `shogun approvals …` (E15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalsCommand {
    /// `approvals [list]` → `approvals.list`.
    List,
    /// `approvals get <id>` → `approvals.get`.
    Get { id: u64 },
    /// `approvals decide <id> approve|reject [--as <kind:id>]`: the operator's decision, sent with
    /// `SHOGUN_ADMIN_TOKEN` (never an agent's token). `body` is the decide request JSON.
    Decide { id: u64, body: String },
}

/// `shogun acl …` (E18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclCommand {
    /// `acl [list]` → `acl.list`.
    List,
    /// `acl grant <json>` (operator, `SHOGUN_ADMIN_TOKEN`).
    Grant { body: String },
    /// `acl revoke <json>` (operator).
    Revoke { body: String },
    /// `acl teams` (operator, multi-principal).
    Teams,
    /// `acl team <json>` define a team (operator, multi-principal).
    SetTeam { body: String },
}

/// The action for `shogun config path|show|validate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    Path,
    Show,
    Validate,
}

/// A parsed CLI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Live form review; API apply only asks for the human confirmation window.
    Forms(FormOperation),
    /// `shogun search <query>` → hybrid memory search.
    Search {
        query: String,
        depth: Option<SearchDepthV1>,
        from_ms: Option<i64>,
        to_ms: Option<i64>,
        source_kind: Option<MemorySourceFamilyV1>,
        /// `--limit N`: most hits to return (server default 20, max 50).
        limit: Option<usize>,
        /// `--ai`: the user's own model widens the question and reranks (issue #940 B2b).
        ai: bool,
    },
    /// `shogun periods [--from-ms N] [--to-ms N] [--page-size N]` → stored hour/day/week summaries
    /// for a window (default: the last 24 hours). Issue #1066.
    Periods { from_ms: Option<i64>, to_ms: Option<i64>, limit: Option<usize> },
    /// `shogun context` → what is happening now, sessionized from the capture log.
    Context,
    /// `shogun events <id>... [--max-chars N]` → open source events by id. `body` is the
    /// `memory.get_events` JSON (validated at parse).
    Events { body: String },
    /// `shogun session --from-ms N --to-ms N [--app A] [--limit N]` → capture rows over a span as
    /// app/window sessions. `body` is the `memory.read_session` JSON (validated at parse).
    Session { body: String },
    /// `shogun pack <query>` → the grounded context pack for a task/question (FR-API-08).
    Pack {
        query: String,
    },
    /// `shogun ingest <json>` → `memory.ingest` (E4-T3): `body` is the request JSON (events or
    /// turns, source, actor, thread_key, idempotency_key), validated at parse.
    Ingest { body: String },
    /// `shogun ingest-webhook <json>` → `memory.ingest_webhook` (E44): one provider webhook
    /// delivery (`provider`, `delivery_id`, `idempotency_key`, `payload_b64` or `changed`),
    /// validated at parse with the server's parser.
    IngestWebhook { body: String },
    /// `shogun recall <query> [--budget-tokens N] [--include-scratch]` → `memory.recall`
    /// (E4-T3): `body` is the request JSON, validated at parse.
    Recall { body: String },
    /// `shogun fs <json>` → `memory.fs` (E6-T2): one memory-tool command (`view`, `create`,
    /// `str_replace`, `insert`, `delete`, `rename`) over `/memories`; `body` is validated at parse.
    Fs { body: String },
    /// `shogun handoff create <json>` → `handoff.create` (E35): the A2A state-of-work handoff for
    /// a task; `body` is the request JSON (task, to_agent, …), validated at parse.
    HandoffCreate { body: String },
    /// `shogun handoff accept <json>` → `handoff.accept` (E35): `{"handoff": …, "actor": …}` into
    /// the caller's own scratch; `body` is validated at parse (expiry is the server's to judge).
    HandoffAccept { body: String },
    /// `shogun payments <sub> [<json>]` (E46): `mandates list` → `payments.mandates.list`
    /// (`body` `None` lists them all), `mandates request` / `mandates revoke` / `spend` → the
    /// L3 / L1 / L3 tools. `body` is the tool's arguments as one JSON object.
    Payments { tool: Tool, body: Option<String> },
    /// `shogun approvals list|get <id>|decide <id> approve|reject` (E15).
    Approvals(ApprovalsCommand),
    /// `shogun audit [--since-ms N] [--limit N]` → `audit.list` (E18).
    Audit { since_ms: Option<i64>, limit: Option<u32> },
    /// `shogun audit export [--since-ms N] [--until-ms N] [--format jsonl|csv] [--principal P]
    /// [--out FILE]` (E26): the hash-chained export, the operator's (`SHOGUN_ADMIN_TOKEN`).
    AuditExport { since_ms: Option<i64>, until_ms: Option<i64>, format: String, principal: Option<String>, out: Option<String> },
    /// `shogun audit verify <file>` (E26): recompute an export's chain offline. No server.
    AuditVerify { file: String },
    /// `shogun acl list|grant|revoke|teams|team` (E18).
    Acl(AclCommand),
    People(ListOrGet),
    Projects(ListOrGet),
    Commitments(ListOrGet),
    OpenLoops(ListOrGet),
    /// `shogun note <text>` → append a user note (L1).
    MemoryNotes { id: Option<i64> },
    MemoryNoteChange { body: String },
    Note {
        text: String,
    },
    /// `shogun run <agent>` → launch a preset agent (level follows the action).
    Run {
        agent: String,
    },
    /// `shogun actions poll <approval_id>` returns a durable, body-free L3 outcome.
    ActionsPoll {
        approval_id: u64,
    },
    /// `shogun actions questions [<id>]` — the questions agent runs are waiting on, or one
    /// (issue #394). Question ids are not approval ids.
    ActionsQuestions {
        id: Option<i64>,
    },
    /// `shogun actions answer <id> <answer…>` — answer one question (L1; it approves nothing).
    ActionsAnswer {
        id: i64,
        answer: String,
    },
    /// `shogun actions steer <chat|notch> <id> <text…>` — one more instruction for a run that is
    /// still going (issue #395; L1, read at its next turn, it approves nothing).
    ActionsSteer {
        kind: String,
        id: i64,
        text: String,
    },
    /// `shogun wrap` → today's Evening Wrap (issue #10, invariant 6 — the notch card as a read).
    Wrap,
    /// `shogun calendar [upcoming]` → the synced calendar's next events, the Today page's
    /// "Coming up" as a read (invariant 6).
    Calendar { page_size: Option<usize> },
    /// `shogun onboarding` → this device's onboarding / first-run setup state (issue #6).
    Onboarding,
    /// `shogun account` → the account on this device, read-only: signed in or out, the address,
    /// sign-in methods, devices and the plan (#148 P2). Signing in is in the app only.
    Account,
    /// `shogun release-notes` → what changed in the running build, newest first (issue #67).
    ReleaseNotes,
    /// `shogun update [status]` → automatic updates on/off, the staged or available version and
    /// the next check (issue #1296). Read-only: installing and restarting are in the app only.
    UpdateStatus,
    /// `shogun dream [status]` → the nightly review's status, why last night has no fresh
    /// result, if it has none, and the actions available for it with a code when one is off
    /// (issue #285 — the Settings "Nightly review" row as a read).
    DreamStatus,
    ProcessingStatus { id: i64 },
    /// `shogun dream retry` → resume the pending Batch review now (`dream.retry`).
    DreamRetry,
    Capture { paused: Option<bool>, minutes: Option<u32> },
    /// `shogun api status` → report the running REST port (FR-API-01).
    ApiStatus,
    /// `shogun api tokens` → the issued Memory API tokens and their scopes, plus this caller's
    /// own scope (issue #400). Read-only: a token is issued or widened only in Settings.
    ApiTokens,
    /// `shogun api restrict <client>` → withhold meeting notes and screen recall from one
    /// connected app (`api.connections.restrict`, owner decision 2026-10-20). Only narrows;
    /// allowing them again is only in the app.
    ApiRestrict { client: String },
    /// `shogun metrics` → the in-product SLO snapshot (NFR-SLO-00).
    Metrics,
    /// `shogun lessons list|enable <id>|disable <id>` (L5 — Memory API symmetry).
    Lessons(LessonsCommand),
    /// `shogun patterns [list]|enable <id>|disable <id>` (Work Patterns, FR-PAT-06 symmetry).
    Patterns(PatternsCommand),
    /// `shogun disputes [list]|resolve <id> <resolution>` (issue #253 — the Context Health
    /// pane's contradiction flags and their decision).
    Disputes(DisputesCommand),
    /// `shogun state edit|delete|export` (issue #290 — the World model pane's correction, delete
    /// and export, at the same L1 / read levels).
    State(StateCommand),
    /// `shogun import preview|commit|list|show|delete` (issues #384 / #391 — the Import memory
    /// screen's preview, import, history and undo, at the same read / L1 levels).
    Import(ImportCommand),
    /// `shogun skills [list]|show <name>|run <name> [input…]|status <id>` (issue #251 — the
    /// user's skill files, the same folder and runs the desktop Skills card shows).
    Skills(SkillsCommand),
    /// `shogun web transcript|post|page <url>` (issue #555 — the public reads chat can make,
    /// as reads: needs the Web fetch consent and Pro, fetched by the daemon itself).
    Web(WebCommand),
    /// `shogun schedule [list]|add <skill> <days> <HH:MM> [input…]|remove <id>` (issue #252).
    Schedule(ScheduleCommand),
    /// `shogun support reports` — this Mac's support reports and their status (issue #108).
    SupportReports,
    /// `shogun export [meeting] [id] --to <folder> …|status <id>|history|folders …` (issue #397 —
    /// save a Recap into an export folder; L2, the user confirms it in the desktop).
    Export(ExportCommand),
    VisualRecall(VisualRecallCommand),
    /// `shogun backup status|retention <days>` — how long encrypted database snapshots are kept
    /// (issue #647; the same setting as Settings › Privacy & Security).
    Backup(BackupCommand),
    /// `shogun search-index status|compact|stop` — the Settings › Memory "Compact search index"
    /// job (#714 step 3). The app runs it; these ask and report.
    SearchIndex(SearchIndexCommand),
    /// `shogun storage preview` — bounded, explicit canonical-text inventory (#709).
    StoragePreview,
    CaptureExclusions(CaptureExclusionsCommand),
    /// `shogun whoami` → profile and compact work summary.
    Whoami,
    /// `shogun profile set <json>` → local L1 profile update.
    ProfileSet {
        body: String,
    },
    VoiceDictionary(VoiceDictionaryCommand),
    /// `shogun writing-style show|enable|disable|clear` (same local store as Settings; enable waits
    /// for in-app approval, issue #654).
    WritingVoice { action: String, id: Option<i64> },
    /// `shogun agent-source [show]|use <source> [--model <id>]|revoke|openai-store on|off` — which
    /// credential chat and drafts run on (issue #110). Using a subscription CLI waits for in-app
    /// approval (it is the opt-in); an API-key provider, `revoke` and `openai-store off` apply at
    /// once; `openai-store on` is refused (Settings only, #390).
    AgentSource(AgentSourceCommand),
    /// `shogun connections [list] | disconnect <service> | gmail draft-only on|off`
    /// and legacy `composio` policy commands — the Connections screen (issue #82). Narrowing applies at once
    /// (L1); turning draft-only mode off is refused (Settings > Connections only), and connecting is only
    /// in the app.
    Connections(ConnectionsCommand),
    /// `shogun meeting reminder show|set 0|1|5|10|off` — upcoming calendar reminder lead time (L1).
    MeetingReminder {
        minutes: Option<u8>,
    },
    MeetingMicrophone(MicrophoneCommand),
    /// `shogun translate register show` / `set <register> [--app <bundle-id>]` (#279 FR-TL-07).
    TranslateRegister(TranslateRegisterCommand),
    /// `shogun translate text|reply|status|pair …` (#279, invariant 6): translate text or draft a
    /// reply on the user's own model access (the app runs it; L1, nothing is sent), poll a
    /// request, and read or choose the language pair.
    Translate(TranslateCommand),
    /// The app's display language: show, or set `en|ja|es|de`.
    Language { set: Option<String> },
    /// `shogun meeting start [--in-person]` — queue an L2 Record now offer (the desktop consumes it
    /// within a second); `--in-person` for a room (#262).
    MeetingRecordNow { in_person: bool },
    MeetingSystemAudioStatus,
    MeetingSystemAudioRetry,
    /// `shogun meeting poll <id>` — poll a `meeting.start` L2 approval (FR-API-04).
    MeetingStartStatus {
        id: u64,
    },
    /// `shogun meeting speakers [show]|name <key> <name…>|unname <key>` (FR-MT-15, invariant 6).
    MeetingSpeakers(SpeakersCommand),
    /// `shogun meeting templates [show]|set <id>` — the Recap-shaping template.
    MeetingTemplates(TemplatesCommand),
    /// `shogun meeting export [--transcript]` — the Recap as share-ready Markdown.
    MeetingExport {
        transcript: bool,
        session_id: Option<i64>,
    },
    MeetingList { before_id: Option<i64> },
    /// `shogun meeting track [--session <id>] [--mine|--theirs] [--counterparty <id>] <text…>` —
    /// the Recap's `[Track]` (FR-MT-17): confirm one suggested next action as a commitment.
    MeetingTrack {
        session_id: Option<i64>,
        text: String,
        direction: Option<String>,
        counterparty_id: Option<i64>,
    },
    /// `shogun meeting recap [id]` — the Recap as structured JSON (FR-MT-22).
    MeetingRecap { session_id: Option<i64> },
    /// `shogun meeting transcript [id]` — the transcript lines with speaker labels (FR-MT-22).
    MeetingTranscript { session_id: Option<i64> },
    /// `shogun meeting recap edit [id] <json>` — rewrite part of the minutes by hand (D1);
    /// `shogun meeting recap reset [id]` hands edited minutes back to the model.
    MeetingRecapEdit { session_id: Option<i64>, body: RecapEditBody },
    /// `shogun meeting search <words…>` — saved meetings whose minutes or transcript match (D6).
    MeetingSearch { query: String },
    /// `shogun meeting delete <id>` — delete one saved meeting for good (D6, L1).
    MeetingDelete { session_id: i64 },
    /// `shogun meeting prefs [show]|set …` — minutes language and custom instruction.
    MeetingRecapPrefs(RecapPrefsCommand),
    WikiEvidence { key: String },
    WikiNeighbors { key: String },
    WikiList,
    VoiceMicrophone(MicrophoneCommand),
    /// `shogun mail triage|labels|rules|history …` (issue #319, FR-TG-30 — the Mail pane's
    /// operations on the CLI face, at the same levels).
    Mail(MailCommand),
    /// `shogun help` / no args.
    Help,
    Config {
        action: ConfigAction,
    },
    /// `shogun principal import --from <memory.db> --root <dir> --principal <id>` (ShogunAgent
    /// E2-T6): copy a single-user store into the principal layout and apply the pending
    /// migrations. Runs locally, no server call.
    Principal(PrincipalCommand),
}

/// The `principal` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalCommand {
    Import {
        from: String,
        root: String,
        principal: String,
    },
}

/// Filters for `lessons list`. Default is the Settings/management list (every row).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LessonsList {
    /// Standing-prompt lookup: active Global lessons, plus any ids below.
    pub for_generation: bool,
    pub app_bundle_id: Option<String>,
    pub person_id: Option<String>,
    pub project_id: Option<String>,
}

/// Work Patterns subcommands (invariant 6: the same list + switch the Patterns card offers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternsCommand {
    List,
    Enable { id: i64 },
    Disable { id: i64 },
}

/// Contradiction-flag subcommands (issue #253, invariant 6: the same list + decision the Context
/// Health pane offers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisputesCommand {
    List,
    /// `resolution` is one of [`DISPUTE_RESOLUTIONS`], validated at parse time.
    Resolve { id: i64, resolution: String },
}

/// World model subcommands (issue #290). `body` is the shared `state.update` / `state.delete`
/// request JSON, built at parse time so a malformed edit never reaches the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateCommand {
    Edit { body: String },
    Delete { body: String },
    /// `state create <commitments|open-loops> --description T …` — the shared `state.create` body.
    Create { body: String },
    Export,
    /// `state export --format export-v1|export-v2 [--differential | --since-cursor <ms>]` — the
    /// portable JSON Lines export (E7, E27); `body` is the `state.export` arguments, validated at
    /// parse.
    ExportV1 { body: String },
    /// `state split <table> <id> --name T --event N…` — the shared `state.split` body.
    Split { body: String },
    /// `state evidence <table> <id>` — what a split chooses from.
    Evidence { table: String, id: i64 },
    /// `state merge <table> <id> <id>…` — the shared `state.merge` body (issue #1362).
    Merge { body: String },
    /// `state keep-separate <table> <id> <id>` — the shared `state.keep_separate` body.
    KeepSeparate { body: String },
    /// `state duplicates <table>` — the pairs nothing merged (`state.merge.candidates`).
    Duplicates { table: String },
}

/// Memory import subcommands (issues #384 / #391). `body` is the shared request JSON, built at
/// parse time from files the user named on the command line — a package file, or Computer History
/// Markdown files / a folder. Only those paths are read; the daemon never receives a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportCommand {
    Preview { body: String },
    Commit { body: String },
    List,
    Show { id: i64 },
    Delete { id: i64 },
}

/// Skill subcommands (issue #251).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsCommand {
    List,
    Show { name: String },
    Run { name: String, input: String },
    Status { id: i64 },
    /// Save a new skill; `markdown` is the file's contents, read by the parser from a path.
    Save { markdown: String },
}

/// Web read subcommands (issue #555). The link is validated by the daemon, where the one
/// classifier lives; the CLI only carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebCommand {
    Transcript { url: String },
    Post { url: String },
    Page { url: String },
}

/// File export subcommands (issue #397).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportCommand {
    /// Ask to save a meeting's Recap into `folder` (absolute). `session_id: None` → latest.
    Meeting {
        session_id: Option<i64>,
        folder: String,
        name: Option<String>,
        csv: bool,
        transcript: bool,
    },
    /// One export request and where it stands.
    Status { id: i64 },
    /// Every export, newest first.
    History,
    /// The export folders.
    Folders,
    /// Remove an export folder.
    FolderRemove { folder: String },
}

/// Schedule subcommands (issue #252). `days` / `at` are validated server-side, where the one
/// parser lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleCommand {
    List,
    Add { skill: String, days: String, at: String, input: String },
    Remove { id: i64 },
}

/// The wire values `state.disputes.resolve` accepts, in the order the help text lists them.
pub const DISPUTE_RESOLUTIONS: &[&str] = &["keep_earlier", "keep_later", "dismiss_both", "keep_both"];

/// Meeting speaker subcommands (invariant 6: the same chips + naming the Recap offers). The
/// session is always the most recent meeting — the CLI mirrors the Recap's "name who just spoke".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeakersCommand {
    Show,
    Name { key: String, name: String },
    Unname { key: String },
}

/// Meeting template subcommands (invariant 6: the same picker the Recap renders).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatesCommand {
    Show,
    Set { id: String },
}

/// Recap preferences (invariant 6: the Settings card's minutes language and custom instruction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecapPrefsCommand {
    Show,
    /// `language`: auto / english / japanese. `instruction`: `Some(None)` clears,
    /// `Some(Some(text))` sets, `None` leaves it alone.
    Set {
        language: Option<String>,
        instruction: Option<Option<String>>,
    },
}

/// Lessons subcommands (invariant 6: same list + toggle the Learned UI offers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LessonsCommand {
    List(LessonsList),
    Enable { id: i64 },
    Disable { id: i64 },
}

/// Backup snapshot subcommands (issue #647, Memory API symmetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupCommand {
    Status,
    Retention { days: u32 },
}

/// Search-index compaction subcommands (#714 step 3, Memory API symmetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchIndexCommand {
    Status,
    Compact,
    Stop,
}

/// Visual recall subcommands (Memory API symmetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisualRecallCommand {
    Status,
    DeleteHiddenSelfFrames,
    Enable,
    Disable,
    Retention {
        days: u32,
    },
    Search {
        query: String,
        from_ms: Option<i64>,
        to_ms: Option<i64>,
        app_bundle_id: Option<String>,
    },
    FrameGet {
        id: i64,
    },
    FrameTextLines {
        id: i64,
    },
    FrameRescan {
        id: i64,
    },
    FrameDelete {
        id: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSourceCommand {
    Show,
    Use { source: String, model: Option<String> },
    /// Withdraw the subscription opt-in.
    Revoke,
    /// `openai-store on|off`: whether OpenAI may store responses (#390). `off` applies at once;
    /// the server refuses `on` (Settings only).
    OpenAiStore(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionsCommand {
    List,
    Disconnect { service: String },
    /// Revoke the Mail (Composio) consent.
    ComposioRevoke,
    /// Draft-only mode on (`true`) or off (`false`; the server refuses turning it off).
    ComposioDraftOnly(bool),
    /// Direct Gmail draft-only mode; APIs can enable it but cannot turn it off.
    GmailDraftOnly(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureExclusionsCommand {
    List,
    Set { body: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceDictionaryCommand {
    AutoAdd,
    ManageAutoAdd { action: String },
    List,
    Create { term: String },
    Update { id: i64, term: String },
    Delete { id: i64 },
    /// Bulk upsert from a CSV / TSV / Markdown-table file (`-` = stdin). The binary reads the
    /// file into `text` before the call is built; the wire layer only ever sees text.
    Import { source: ImportSource },
    /// The whole dictionary as CSV, printed to stdout.
    Export,
}

/// What `meeting recap edit` / `reset` sends: a JSON object of the parts to change, or a reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecapEditBody {
    /// The `{summary?, decisions?, next_actions?}` object, as the user typed it.
    Json(String),
    Reset,
}

/// Where `voice-dictionary import` takes its rows from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSource {
    Path(String),
    Stdin,
    /// Already-read text (what the binary substitutes before building the call).
    Text(String),
}

/// `show` / `set <name>` / `default` — the grammar every microphone selection shares. `None` is
/// the explicit "follow the macOS default input" reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrophoneCommand {
    Show,
    Set { microphone: Option<String> },
}

/// The translation register (politeness) remembered per app — the reply panel's chip row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateRegisterCommand {
    Show,
    /// `formality` is one of match|casual|polite|formal; `app` is a bundle id, else the default.
    Set { formality: String, app: Option<String> },
}

/// Translation and reply drafts on the CLI (issue #279).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateCommand {
    /// `translate text <words…> [--to en|ja] [--register R] [--app ID]`.
    Text { text: String, target: Option<String>, formality: Option<String>, app: Option<String> },
    /// `translate reply <message…> [--in en|ja] [--tone <instruction>] [--register R] [--app ID]`.
    Reply {
        incoming: String,
        reply_in: Option<String>,
        adjustment: Option<String>,
        formality: Option<String>,
        app: Option<String>,
    },
    /// `translate status <id>`.
    Status { id: i64 },
    /// `translate pair [show]`.
    PairShow,
    /// `translate pair set <mine> [<theirs>]`.
    PairSet { mine: String, theirs: Option<String> },
    /// `translate pair reset` — follow the system language again.
    PairReset,
}

/// The built-in mail label keys (V33 seed). `mail labels set <key-or-name>` treats one of these,
/// or any `custom:` key, as a key; anything else is the name of a new custom label.
pub const BUILTIN_MAIL_LABEL_KEYS: &[&str] = &[
    "people",
    "team",
    "calendar",
    "finance",
    "notification",
    "news",
    "marketing",
    "social",
    "pitch",
];

/// Whether a `mail labels set` positional is a label key (built-in or `custom:…`) rather than
/// the display name of a new custom label.
pub fn is_mail_label_key(value: &str) -> bool {
    value.starts_with("custom:") || BUILTIN_MAIL_LABEL_KEYS.contains(&value)
}

/// Filters for `mail triage` (the list). Default is the Mail pane's default split: every
/// unhandled, un-archived thread, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailTriageList {
    /// `important` | `other`.
    pub bucket: Option<String>,
    /// `needs_reply` | `waiting_on_them` | `fyi` | `none`.
    pub attention: Option<String>,
    /// A label key.
    pub label: Option<String>,
    /// `--handled`: include threads already marked handled.
    pub handled: bool,
}

/// `mail labels set <key-or-name> …` — a custom label's name / definition / bucket, or any
/// label's flags. `None` leaves a field untouched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailLabelSet {
    /// A label key (`marketing`, `custom:vendors`) or, for a new custom label, its name.
    pub key_or_name: String,
    pub name: Option<String>,
    pub definition: Option<String>,
    /// `important` | `other`.
    pub bucket: Option<String>,
    pub enabled: Option<bool>,
    pub auto_archive: Option<bool>,
    pub sync_to_gmail: Option<bool>,
    /// `--confirm-standing`: still accepted and sent, but it grants nothing (issue #779) — a
    /// standing approval is granted only in the app, and the API refuses to turn either on.
    pub confirm_standing: bool,
}

/// Mail triage subcommands (invariant 6: the Mail pane's operations, at the same levels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailCommand {
    /// `mail triage [--bucket B] [--attention A] [--label L] [--handled]`
    TriageList(MailTriageList),
    /// `mail triage show <thread_key>`
    TriageShow { thread_key: String },
    /// `mail triage set <thread_key> [--label L] [--bucket B] [--handled|--unhandled]` (L1)
    TriageSet {
        thread_key: String,
        label: Option<String>,
        bucket: Option<String>,
        handled: Option<bool>,
    },
    /// `mail triage archive <thread_key>` (L2 — queued for the user's approval in the app, or
    /// recorded for the desktop write-back under the label's standing approval; issue #779)
    TriageArchive { thread_key: String },
    /// `mail triage unarchive <thread_key>` (L2, same path)
    TriageUnarchive { thread_key: String },
    /// `mail labels`
    LabelsList,
    /// `mail labels set …` (L1)
    LabelsSet(MailLabelSet),
    /// `mail labels rm <key>` (L1, custom only)
    LabelsRemove { key: String },
    /// `mail rules`
    RulesList,
    /// `mail rules add --addr X|--domain X --label L [--bucket B]` (L1)
    RulesAdd {
        /// `addr` | `domain`.
        match_kind: String,
        match_value: String,
        label: String,
        bucket: Option<String>,
    },
    /// `mail rules rm <id>` (L1)
    RulesRemove { id: i64 },
    /// `mail history`
    History,
    /// `mail status` — is triage running?
    Status,
    /// `mail on` / `mail off` (L1). Stops classification and every Gmail-side write; Gmail stays
    /// connected and what is already classified is kept.
    SetEnabled { enabled: bool },
}

impl Command {
    /// The Memory API tool this command invokes, if any. `api status` and `help` are local CLI
    /// concerns and map to no tool.
    pub fn tool(&self) -> Option<Tool> {
        Some(match self {
            Command::Forms(operation) => match operation {
                FormOperation::Inspect {} => Tool::FormsInspect,
                FormOperation::Plan { .. } => Tool::FormsPlan,
                FormOperation::Status { .. } => Tool::FormsStatus,
                FormOperation::Apply { .. } => Tool::FormsApply,
                FormOperation::Close { .. } => Tool::FormsClose,
                FormOperation::Undo { .. } => Tool::FormsUndo,
                // Slack proposals arrive over MCP/REST only; the CLI has no command for them.
                FormOperation::ProposeSlackPost { .. } => return None,
            },
            Command::Search { .. } => Tool::MemorySearch,
            Command::Periods { .. } => Tool::MemoryGetPeriods,
            Command::Context => Tool::MemoryGetContext,
            Command::Events { .. } => Tool::MemoryGetEvents,
            Command::Session { .. } => Tool::MemoryReadSession,
            Command::Pack { .. } => Tool::MemoryGetContextPack,
            Command::Ingest { .. } => Tool::MemoryIngest,
            Command::IngestWebhook { .. } => Tool::MemoryIngestWebhook,
            Command::Recall { .. } => Tool::MemoryRecall,
            Command::Fs { .. } => Tool::MemoryFs,
            Command::HandoffCreate { .. } => Tool::HandoffCreate,
            Command::HandoffAccept { .. } => Tool::HandoffAccept,
            Command::Payments { tool, .. } => *tool,
            Command::Approvals(ApprovalsCommand::List) => Tool::ApprovalsList,
            Command::Approvals(ApprovalsCommand::Get { .. }) => Tool::ApprovalsGet,
            Command::Audit { .. } => Tool::AuditList,
            Command::Acl(AclCommand::List) => Tool::AclList,
            // The operator's routes: not tools, never an agent's (E15 / E18).
            Command::Approvals(ApprovalsCommand::Decide { .. })
            | Command::AuditExport { .. }
            | Command::AuditVerify { .. }
            | Command::Acl(AclCommand::Grant { .. } | AclCommand::Revoke { .. } | AclCommand::Teams | AclCommand::SetTeam { .. }) => {
                return None
            }
            Command::People(ListOrGet::List) => Tool::StatePeopleList,
            Command::People(ListOrGet::Get { .. }) => Tool::StatePeopleGet,
            Command::Projects(ListOrGet::List) => Tool::StateProjectsList,
            Command::Projects(ListOrGet::Get { .. }) => Tool::StateProjectsGet,
            Command::Commitments(ListOrGet::List) => Tool::StateCommitmentsList,
            Command::Commitments(ListOrGet::Get { .. }) => Tool::StateCommitmentsGet,
            Command::OpenLoops(ListOrGet::List) => Tool::StateOpenLoopsList,
            Command::OpenLoops(ListOrGet::Get { .. }) => Tool::StateOpenLoopsGet,
            Command::MemoryNotes { .. } => Tool::MemoryNotesList,
            Command::MemoryNoteChange { .. } => Tool::MemoryNotesChange,
            Command::Note { .. } => Tool::MemoryAppendNote,
            Command::Run { .. } => Tool::ActionsExecute,
            Command::ActionsPoll { .. } => return None,
            Command::ActionsQuestions { .. } => Tool::ActionsQuestions,
            Command::ActionsAnswer { .. } => Tool::ActionsAnswer,
            Command::ActionsSteer { .. } => Tool::ActionsSteer,
            Command::Wrap => Tool::MemoryGetWrap,
            Command::Calendar { .. } => Tool::CalendarUpcoming,
            Command::Onboarding => Tool::DeviceOnboardingGet,
            Command::Account => Tool::DeviceAccountGet,
            Command::ReleaseNotes => Tool::DeviceReleaseNotesGet,
            Command::UpdateStatus => Tool::DeviceUpdateGet,
            Command::DreamStatus => Tool::DreamStatus,
            Command::ProcessingStatus { .. } => Tool::MemoryProcessingStatus,
            Command::DreamRetry => Tool::DreamRetry,
            Command::Capture { paused: None, .. } => Tool::CaptureStatus,
            Command::Capture { .. } => Tool::CaptureSetPaused,
            Command::Lessons(LessonsCommand::List(_)) => Tool::LessonsList,
            Command::Lessons(LessonsCommand::Enable { .. })
            | Command::Lessons(LessonsCommand::Disable { .. }) => Tool::LessonsSetActive,
            Command::Patterns(PatternsCommand::List) => Tool::PatternsList,
            Command::Patterns(PatternsCommand::Enable { .. })
            | Command::Patterns(PatternsCommand::Disable { .. }) => Tool::PatternsSetActive,
            Command::Disputes(DisputesCommand::List) => Tool::StateDisputesList,
            Command::Disputes(DisputesCommand::Resolve { .. }) => Tool::StateDisputesResolve,
            Command::State(StateCommand::Edit { .. }) => Tool::StateUpdate,
            Command::State(StateCommand::Delete { .. }) => Tool::StateDelete,
            Command::State(StateCommand::Create { .. }) => Tool::StateCreate,
            Command::State(StateCommand::Export) => Tool::StateExport,
            Command::State(StateCommand::ExportV1 { .. }) => Tool::StateExport,
            Command::State(StateCommand::Split { .. }) => Tool::StateSplit,
            Command::State(StateCommand::Evidence { .. }) => Tool::StateSplitEvidence,
            Command::State(StateCommand::Merge { .. }) => Tool::StateMerge,
            Command::State(StateCommand::KeepSeparate { .. }) => Tool::StateKeepSeparate,
            Command::State(StateCommand::Duplicates { .. }) => Tool::StateMergeCandidates,
            Command::Import(ImportCommand::Preview { .. }) => Tool::MemoryImportPreview,
            Command::Import(ImportCommand::Commit { .. }) => Tool::MemoryImport,
            Command::Import(ImportCommand::List | ImportCommand::Show { .. }) => Tool::MemoryImportList,
            Command::Import(ImportCommand::Delete { .. }) => Tool::MemoryImportDelete,
            Command::Skills(SkillsCommand::List | SkillsCommand::Show { .. }) => Tool::SkillsList,
            Command::Skills(SkillsCommand::Run { .. }) => Tool::SkillsRun,
            Command::Skills(SkillsCommand::Status { .. }) => Tool::SkillsRunStatus,
            Command::Web(WebCommand::Transcript { .. }) => Tool::WebVideoTranscript,
            Command::Web(WebCommand::Post { .. }) => Tool::WebSocialPost,
            Command::Web(WebCommand::Page { .. }) => Tool::WebPage,
            Command::Skills(SkillsCommand::Save { .. }) => Tool::SkillsSave,
            Command::Schedule(ScheduleCommand::List) => Tool::SchedulesList,
            Command::Schedule(ScheduleCommand::Add { .. }) => Tool::SchedulesAdd,
            Command::Schedule(ScheduleCommand::Remove { .. }) => Tool::SchedulesRemove,
            Command::SupportReports => Tool::SupportReportsList,
            Command::Export(cmd) => match cmd {
                ExportCommand::Meeting { .. } => Tool::ExportWriteFile,
                ExportCommand::Status { .. } | ExportCommand::History => Tool::ExportWritesList,
                ExportCommand::Folders => Tool::ExportFoldersList,
                ExportCommand::FolderRemove { .. } => Tool::ExportFoldersRemove,
            },
            Command::Backup(BackupCommand::Status) => Tool::BackupStatus,
            Command::Backup(BackupCommand::Retention { .. }) => Tool::BackupSetRetention,
            Command::SearchIndex(SearchIndexCommand::Status) => Tool::SearchIndexStatus,
            Command::SearchIndex(SearchIndexCommand::Compact) => Tool::SearchIndexCompactStart,
            Command::SearchIndex(SearchIndexCommand::Stop) => Tool::SearchIndexCompactStop,
            Command::StoragePreview => Tool::StoragePreview,
            Command::VisualRecall(VisualRecallCommand::Status) => Tool::VisualRecallStatus,
            Command::VisualRecall(VisualRecallCommand::Enable) => Tool::VisualRecallSetEnabled,
            Command::VisualRecall(VisualRecallCommand::Disable) => Tool::VisualRecallSetEnabled,
            Command::VisualRecall(VisualRecallCommand::Retention { .. }) => {
                Tool::VisualRecallSetRetention
            }
            Command::VisualRecall(VisualRecallCommand::Search { .. }) => {
                Tool::VisualRecallSearchFrames
            }
            Command::VisualRecall(VisualRecallCommand::FrameGet { .. }) => {
                Tool::VisualRecallGetFrame
            }
            Command::VisualRecall(VisualRecallCommand::FrameTextLines { .. }) => {
                Tool::VisualRecallFrameTextLines
            }
            Command::VisualRecall(VisualRecallCommand::FrameRescan { .. }) => {
                Tool::VisualRecallRescanFrame
            }
            Command::VisualRecall(VisualRecallCommand::FrameDelete { .. } | VisualRecallCommand::DeleteHiddenSelfFrames) => {
                Tool::VisualRecallDeleteFrame
            }
            Command::CaptureExclusions(CaptureExclusionsCommand::List) => {
                Tool::CaptureExclusionsGet
            }
            Command::CaptureExclusions(CaptureExclusionsCommand::Set { .. }) => {
                Tool::CaptureExclusionsSet
            }
            Command::Whoami => Tool::ProfileWhoami,
            Command::ProfileSet { .. } => Tool::ProfileSet,
            Command::VoiceDictionary(VoiceDictionaryCommand::List) => Tool::VoiceDictionaryList,
            Command::VoiceDictionary(VoiceDictionaryCommand::AutoAdd) => Tool::DictionaryAutoAddGet,
            Command::VoiceDictionary(VoiceDictionaryCommand::ManageAutoAdd { .. }) => Tool::DictionaryAutoAddSet,
            Command::WritingVoice { action, .. } if action == "show" => Tool::WritingVoiceGet,
            Command::WritingVoice { .. } => Tool::WritingVoiceSet,
            Command::AgentSource(AgentSourceCommand::Show) => Tool::AgentSourceGet,
            Command::AgentSource(_) => Tool::AgentSourceSet,
            Command::Connections(ConnectionsCommand::List) => Tool::ConnectionsList,
            Command::Connections(ConnectionsCommand::Disconnect { .. }) => {
                Tool::ConnectionsDisconnect
            }
            Command::Connections(ConnectionsCommand::GmailDraftOnly(_)) => {
                Tool::ConnectionsGmailSet
            }
            Command::Connections(_) => Tool::ConnectionsComposioSet,
            Command::Language { set: None } => Tool::UiLanguageGet,
            Command::Language { set: Some(_) } => Tool::UiLanguageSet,
            Command::TranslateRegister(TranslateRegisterCommand::Show) => Tool::TranslateRegisterGet,
            Command::TranslateRegister(TranslateRegisterCommand::Set { .. }) => {
                Tool::TranslateRegisterSet
            }
            Command::Translate(TranslateCommand::Text { .. }) => Tool::TranslateText,
            Command::Translate(TranslateCommand::Reply { .. }) => Tool::TranslateReplyDraft,
            Command::Translate(TranslateCommand::Status { .. }) => Tool::TranslateStatus,
            Command::Translate(TranslateCommand::PairShow) => Tool::TranslatePairGet,
            Command::Translate(TranslateCommand::PairSet { .. } | TranslateCommand::PairReset) => {
                Tool::TranslatePairSet
            }
            Command::VoiceDictionary(VoiceDictionaryCommand::Create { .. }) => {
                Tool::VoiceDictionaryCreate
            }
            Command::VoiceDictionary(VoiceDictionaryCommand::Update { .. }) => {
                Tool::VoiceDictionaryUpdate
            }
            Command::VoiceDictionary(VoiceDictionaryCommand::Delete { .. }) => {
                Tool::VoiceDictionaryDelete
            }
            Command::VoiceDictionary(VoiceDictionaryCommand::Import { .. }) => {
                Tool::VoiceDictionaryImport
            }
            Command::VoiceDictionary(VoiceDictionaryCommand::Export) => {
                Tool::VoiceDictionaryExport
            }
            Command::MeetingReminder { minutes: None } => Tool::MeetingReminderGet,
            Command::MeetingReminder { minutes: Some(_) } => Tool::MeetingReminderSet,
            Command::MeetingMicrophone(MicrophoneCommand::Show) => Tool::MeetingMicrophoneGet,
            Command::MeetingMicrophone(MicrophoneCommand::Set { .. }) => Tool::MeetingMicrophoneSet,
            Command::MeetingRecordNow { .. } => Tool::MeetingRecordNow,
            Command::MeetingSystemAudioStatus => Tool::MeetingSystemAudioStatus,
            Command::MeetingSystemAudioRetry => Tool::MeetingSystemAudioRetry,
            Command::MeetingStartStatus { .. } => Tool::MeetingStartStatus,
            Command::MeetingSpeakers(SpeakersCommand::Show) => Tool::MeetingSpeakersGet,
            Command::MeetingSpeakers(SpeakersCommand::Name { .. })
            | Command::MeetingSpeakers(SpeakersCommand::Unname { .. }) => Tool::MeetingSpeakersSet,
            Command::MeetingTemplates(TemplatesCommand::Show) => Tool::MeetingTemplatesGet,
            Command::MeetingTemplates(TemplatesCommand::Set { .. }) => Tool::MeetingTemplatesSet,
            Command::MeetingExport { .. } => Tool::MeetingRecapExport,
            Command::MeetingTrack { .. } => Tool::MeetingRecapTrack,
            Command::MeetingList { .. } => Tool::MeetingList,
            Command::MeetingRecap { .. } => Tool::MeetingRecapGet,
            Command::MeetingTranscript { .. } => Tool::MeetingTranscriptGet,
            Command::MeetingRecapEdit { .. } => Tool::MeetingRecapEdit,
            Command::MeetingSearch { .. } => Tool::MeetingSearch,
            Command::MeetingDelete { .. } => Tool::MeetingDelete,
            Command::MeetingRecapPrefs(RecapPrefsCommand::Show) => Tool::MeetingRecapPrefsGet,
            Command::MeetingRecapPrefs(RecapPrefsCommand::Set { .. }) => Tool::MeetingRecapPrefsSet,
            Command::WikiEvidence { .. } => Tool::WikiEvidence,
            Command::WikiNeighbors { .. } => Tool::WikiNeighbors,
            Command::WikiList => Tool::WikiList,
            Command::VoiceMicrophone(MicrophoneCommand::Show) => Tool::VoiceMicrophoneGet,
            Command::VoiceMicrophone(MicrophoneCommand::Set { .. }) => Tool::VoiceMicrophoneSet,
            Command::Mail(mail) => match mail {
                MailCommand::TriageList(_) => Tool::MailTriageList,
                MailCommand::TriageShow { .. } => Tool::MailTriageGet,
                MailCommand::TriageSet { .. } => Tool::MailTriageSet,
                MailCommand::TriageArchive { .. } => Tool::MailTriageArchive,
                MailCommand::TriageUnarchive { .. } => Tool::MailTriageUnarchive,
                MailCommand::LabelsList => Tool::MailLabelsList,
                MailCommand::LabelsSet(_) => Tool::MailLabelsUpsert,
                MailCommand::LabelsRemove { .. } => Tool::MailLabelsDelete,
                MailCommand::RulesList => Tool::MailSenderRulesList,
                MailCommand::RulesAdd { .. } => Tool::MailSenderRulesAdd,
                MailCommand::RulesRemove { .. } => Tool::MailSenderRulesDelete,
                MailCommand::History => Tool::MailHistoryList,
                MailCommand::Status => Tool::MailSettingsGet,
                MailCommand::SetEnabled { .. } => Tool::MailSettingsSet,
            },
            Command::ApiTokens => Tool::ApiTokensList,
            Command::ApiRestrict { .. } => Tool::ApiConnectionsRestrict,
            Command::ApiStatus
            | Command::Metrics
            | Command::Help
            | Command::Config { .. }
            | Command::Principal(_) => return None,
        })
    }
}

/// The usage text (also printed on a parse error).
pub const USAGE: &str = "\
shogun — ShogunAI Memory API (CLI face)

USAGE:
    shogun [--token <t>] <command>

COMMANDS:
    search <query> [--depth warm|auto|all] [--from-ms N --to-ms N] [--source-kind KIND]
                              Federated memory search; deep ranges are half-open [from,to)
    search --ai <query>       Your own model widens the question and puts the matches that
                              answer it first (needs the app running; else keyword results)
    context                   Current context cache
    pack <query>              Grounded context pack (facts + evidence with provenance)
    recall <query> [--budget-tokens N] [--include-scratch]
                              What memory holds on a question, within a token budget
    ingest <json>             Append events[] or turns[] under an idempotency_key (L1)
    ingest-webhook <json>     Store one provider webhook delivery as events with provenance (L1)
    handoff create <json>     The state of work for a task as an A2A handoff (task, to_agent, ...)
    handoff accept <json>     Accept a handoff into your own scratch (handoff, actor; L1)
    payments mandates list [<json>]
                              Payment mandates: bounds, spent, remaining, status (free)
    payments mandates request <json>
                              Ask a human for a payment mandate (L3: approval URL; narrowing is direct)
    payments mandates revoke <json>
                              Revoke a mandate at once (mandate_id; L1)
    payments spend <json>     Pay: inside a mandate it settles, otherwise it waits for approval (L3)
    approvals [list]          Queued L2/L3 approvals, redacted (kind, tier, recipient class)
    approvals get <id>        One approval's redacted summary
    approvals decide <id> approve|reject [--as <kind:id>]
                              Decide as the operator (sends SHOGUN_ADMIN_TOKEN, never an agent key)
    audit [--since-ms N] [--limit N]
                              The audit log: who wrote or decided what, when (no bodies)
    audit export [--since-ms N] [--until-ms N] [--format jsonl|csv] [--principal P] [--out FILE]
                              Hash-chained export of the audit log (operator, SHOGUN_ADMIN_TOKEN):
                              every record carries prev_hash and hash, a trailer the head hash
    audit verify <file>       Recompute an export's chain offline: altered, dropped, reordered
                              or truncated records fail (exit 1)
    acl [list]                Scratch and Team access grants
    acl grant|revoke <json>   Change a grant as the operator (SHOGUN_ADMIN_TOKEN), e.g.
                              {\"grantee\":\"agent:b\",\"agent\":\"a\",\"access\":\"read\"} or
                              {\"kind\":\"principal\",\"grantee_principal\":\"bob\",\"principal\":\"alice\"}
    acl teams | acl team <json>
                              List or define teams (operator, multi-principal):
                              {\"tenant\":\"acme\",\"team\":\"ops\",\"members\":[\"alice\",\"bob\"]}
    people list|get <id>      People state
    projects list|get <id>    Projects state
    commitments list|get <id> Commitments state
    open-loops list|get <id>  Open loops state
    disputes [list]           Contradiction flags (same promise, different dates)
    disputes resolve <id> keep_earlier|keep_later|dismiss_both|keep_both
                              Settle one flag; the other reading is cancelled (L1)
    state edit <table> <id> [--name T] [--summary T|--clear-summary] [--description T]
               [--status S] [--due <unix-ms>|--no-due] [--snooze <unix-ms>|--no-snooze]
                              Correct a record (people|projects|commitments|open-loops); it is
                              pinned so automatic passes never overwrite it
    state delete <table> <id> Delete a record; its source events stay
    state create <commitments|open-loops> --description T [--due <unix-ms>] [--person <id>]
                              Add a commitment or open loop of your own (pinned, confidence 1.0)
    state export [--json]     The world model as Markdown (--json: the rows too)
    state evidence <people|projects> <id>
                              What a record rests on: aliases and source events (no content)
    state split <people|projects> <id> --name T --event <id> [--event <id>…] [--alias A…]
                              Split a wrong merge: move those events (and aliases) to a new
                              record named T; the two are not merged again
    state duplicates <people|projects>
                              Pairs whose names relate that nothing merged; you decide
    state merge <people|projects> <id> <id> [<id>…]
                              Merge those records into one; sources and links move, nothing is lost
    state keep-separate <people|projects> <id> <id>
                              Say two records are different; they are not suggested or merged again
    import preview <package.json> | --computer-history <file.md|folder>…
                              Check a memory package, or Computer History files you name, and
                              show what importing would do (new / merge / duplicate); saves nothing
    import commit <package.json> | --computer-history <path>… --select <id>… | --defaults [--people-to-state]
                              Import the chosen items (local; never stored as High confidence)
    import list               Past imports
    import show <id>          One import and its items
    import delete <id>        Undo an import: remove it and everything it stored
    skills [list]             Your skill files (<app-data>/skills/*.md) and any that failed to load
    skills show <name>        One skill with its instructions
    skills run <name> [input] Ask the desktop to run a skill (Pro; above L1 you confirm it there)
    skills status <id>        A skill run's status and, once done, its text
    skills save <file.md>     Add a skill from a Markdown file (never overwrites a name)
    schedule [list]           Scheduled skills and what their last slot did
    schedule add <skill> <days> <HH:MM> [input]
                              Run a skill on mon,wed,fri | weekdays | daily at a local time (Pro)
    schedule remove <id>      Delete a schedule
    web transcript <url>      A public YouTube video's captions (Pro; Web fetch consent in Settings)
    web post <url>            One public X post from its link
    web page <url>            The text of a public https page
    support reports           Your support reports and where each stands
    export [meeting] [id] --to <folder> [--name <n>] [--csv] [--transcript]
                              Save the latest (or selected) Recap into an export folder
                              (L2: confirm it in the app; never overwrites)
    export status <id>        One export request and where it stands
    export history            Every export: status, path, size, digest
    export folders            Your export folders (added in the app)
    export folders remove <folder>
                              Remove an export folder (files already saved stay)
    note <text>               Append a user note            (L1)
    memory-notes list|show <id>
                              Saved chat memories and revision history
    memory-notes change <change-json>
                              Save/update/forget/undo a chat memory (L1; changes need revision)
    run <action-json>         Execute one action, e.g. '{\"kind\":\"local_search\",\"query\":\"x\"}'.
                              local_search answers at once (L1); open_app, reveal_file,
                              show_notification and copy_to_clipboard run only in the app and
                              are refused here (501); sends are L3: pending until you approve
                              them in the app (poll with `actions poll <id>`)
    actions poll <id>         Poll L3 approval status
    actions questions [id]    Questions agent runs are waiting on (or one, with its status)
    actions answer <id> <answer>
                              Answer one question (L1: the run resumes; it approves nothing)
    actions steer <chat|notch> <id> <text>
                              Add one more instruction to a run that is still going (L1: it
                              reads it at its next turn; it approves nothing)
    wrap                      Today's Evening Wrap (outcome, still open, tomorrow)
    calendar [upcoming] [--page-size 1..100]    Upcoming events in the next 7 days
    periods [--from-ms N] [--to-ms N] [--page-size N]
                              Stored hour/day/week summaries for a window (default last 24h)
    events <id>... [--max-chars N]
                              Open the source events behind a search hit or pack line
    session --from-ms N --to-ms N [--app A] [--limit N]
                              What happened over a span (max 4h), as app/window sessions
    onboarding                This device's first-run setup state
    release-notes             What changed in the running build, newest first
    update [status]           Automatic updates on/off, staged or available version, next check
                              (install and restart are in the app only)
    dream retry               Resume the pending Batch review now (safe to repeat)
    processing status <event-id>  Read progress and freshness for a saved event; never starts model work
    dream [status]            Nightly review: last night's result, why it failed or didn't run,
                              whether the result shown is from an older night, and which
                              actions are available (with a code when one is off)
    capture status|pause [15m|30m|1h]|resume  Screen capture control (sync continues)
      status also returns capture_events_10m: screen-text capture events in the last 10 minutes
      (a count only, never text). pause applies at once (L1); a pause shorter than the one in
      force is refused (change it in the app). resume while paused is L3: pending until you
      approve it in the app (poll with `actions poll <id>`).
    api status                Show the running REST port
    metrics                   In-product SLO snapshot + lesson counters
    writing-style show|enable|disable|clear
      enable asks to send up to four same-app examples to your selected provider (L3: pending until
      you approve it in the app; poll with `actions poll <id>`). disable and clear apply at once (L1).
    agent-source [show]       Which credential chat and drafts run on: the selected source, your
                              subscription opt-in, installed CLIs, which providers have a key,
                              and the models each API-key provider offers (--model takes one;
                              openrouter also takes any well-formed vendor/model id), and the
                              most one deep-context run can cost on the model that runs
    agent-source use <source> [--model <id>]
      Run chat and drafts on claude-code|codex|gemini-cli (your subscription: L3, pending until you
      approve it in the app), shogun (company-funded live AI via the SHOGUN gateway to OpenAI, L1),
      or anthropic|openrouter|openai|gemini (your API key: applies at once, L1)
    agent-source revoke       Stop running on your subscription (L1)
    agent-source openai-store on|off
                              off stops OpenAI storing responses (L1); on is refused (turn it on in
                              the app, where its retention disclosure is)
    connections [list]        Each connection's state, access, third party, pending disconnect,
                              Gmail draft-only mode and legacy consent (states only, never an account)
    connections disconnect <service>
                              Disconnect gmail|gcal|gdrive|slack|notion|github|linear|granola (L1; the app
                              applies it, now or when it next starts). Connecting is only in the app
    connections gmail draft-only on|off
                              on applies at once (L1); off is refused: turn it off in Settings > Connections
    connections composio revoke
                              Revoke legacy Composio consent (L1); direct Gmail stays connected
    connections composio draft-only on|off
                              on applies at once (L1); off is refused: turn it off in Settings > Connections
    lessons list [--for-generation] [--app ID] [--person ID] [--project ID]
                              All lessons (Settings). --for-generation = lookup
    lessons enable <id>       Switch a learned lesson on  (L1)
    lessons disable <id>      Switch a learned lesson off (L1)
    visual-recall status      Visual recall status + frame stats
    visual-recall enable      Turn visual recall on. While it is off this is L3: pending until you
                              approve it in the app (poll with `actions poll <id>`)
    visual-recall disable     Turn visual recall off and delete all automatically captured frames and their OCR text
                              now (L1). To stop without deleting anything, use `capture pause`
    visual-recall retention <days>  Keep frames 1–3650 days. Shorter or the same applies at once
                              (L1); longer is L3, pending until you approve it in the app
    backup status             Snapshot retention (days), snapshot count and ages
    backup retention <days>   Keep encrypted DB snapshots at most 1–30 days (L1). The newest
                              verified snapshot is kept until a newer one exists
    search-index status       Compaction job state and progress, index shape, database and
                              free-disk numbers (counts only)
    search-index compact      Start or resume compacting the search index (L1). The app runs it
                              in small paced steps; refused when the disk is too full
    search-index stop         Stop compaction between two steps (L1); compact resumes it
    storage preview           Bounded event-history duplicate text preview (counts only)
    visual-recall search <q> [--from-ms N] [--to-ms N] [--app-bundle-id ID]
                              Search stored screen frames
    visual-recall frame get <id>           Frame metadata + OCR text
    visual-recall frame text-lines <id>    Line boxes from a fresh on-device OCR; stores nothing
    visual-recall frame rescan <id>        Re-OCR stored JPEG, replacing its OCR text (L1)
    visual-recall frame delete <id>        Delete one stored frame
    visual-recall delete-hidden-self-frames Delete only hidden legacy ShogunAI frames
    capture-exclusions list|set <json>     List or replace capture exclusions
      set that only adds exclusions applies at once (L1). Any removal resumes capture, so the whole
      request is L3: pending until you approve it in the app (poll with `actions poll <id>`).
    whoami                    Profile + compact work summary
    api tokens                Issued tokens and their scope (read / full), and yours; never a token.
      A read token is refused every change (403 scope_insufficient). Issue or widen one in the app.
    api restrict <client>     Withhold meeting notes and screen recall from one connected app (L1).
      Allowing them again is only in the app (Settings > Memory API).
    profile set <json>        Update profile preferences   (L1)
    voice-dictionary list     Your local vocabulary terms
    voice-dictionary create <term-json>
    voice-dictionary update <id> <term-json>
    voice-dictionary delete <id>
                              Add, replace or remove a term (L1). <term-json> is
                              {\"canonical\":\"ShogunAI\",\"aliases\":[\"shogun ai\"]} plus optional
                              locale, scope (global|bundle|surface), scope_ref, priority, enabled
    voice-dictionary autoadd|autoadd-manage <json>  View/addition Undo; enable needs in-app approval
    voice-dictionary import <file|->       Bulk-add terms from CSV / TSV / Markdown table (L1)
    voice-dictionary export                Every term as CSV (the columns import reads back)
    patterns [list]           Work Patterns and bounded captured-activity usage
    patterns enable|disable <id>         Switch one pattern on/off (L1)
    meeting start              Offer to start meeting notes now (L2; desktop must be running)
    meeting start --in-person  Same, for a meeting in the room (no call audio; voices labelled by speaker)
    meeting poll <id>          Poll a meeting.start L2 approval
    meeting reminder show|set 0|1|5|10|off      Upcoming calendar notifications (L1 set)
    meeting microphone show|set <name>|default  Manage meeting capture input (L1 set)
    language [show]            App display language (en default; ja, es, de)
    language set en|ja|es|de   Change the app's display language (L1)
    forms inspect            Inspect focused form and open human review (L2; app must run)
    forms plan <session> <revision> <instruction...>   Match facts with Jev using your TypeSafe key (L1; human review)
    forms status <session>   Read captured fields, proposals, evidence and results
    forms apply <session> <revision>   Open pending human review (L3; cannot confirm or submit)
    forms undo <session>     Ask for human Undo confirmation (L3)
    forms close <session>    Cancel and close own form session (L1)
    translate register [show]  Reply/translation register: default + per-app entries
    translate register set match|casual|polite|formal [--app <bundle-id>]  Remember a register (L1)
    translate text <text> [--to en|ja] [--register R] [--app <bundle-id>]
                               Translate text on your own model access (L1; Pro; the app must be running)
    translate reply <message> [--in en|ja] [--tone <instruction>] [--register R] [--app <bundle-id>]
                               Draft a reply in their language + a back-translation (L1; Pro; never sent)
    translate status <id>      A translate request that was still running (kept 10 minutes)
    translate pair [show]      Translation language pair (null = follows the system language)
    translate pair set en|ja [en|ja]     Choose your language (L1)
    translate pair reset       Follow the system language again (L1)
    meeting speakers [show]   Latest meeting's speakers + confirmed names
    meeting speakers name <key> <name>   Confirm who a speaker is (L1)
    meeting speakers unname <key>        Clear a confirmed name (L1)
    meeting templates [show]  Meeting templates + current selection
    meeting templates set <id>           Choose the Recap-shaping template (L1)
    meeting export [id] [--transcript]   Selected or latest recap as Markdown
    meeting track [--session <id>] [--mine|--theirs] [--counterparty <id>] <text>
                                         Track a Recap next action as a commitment (L1)
    meeting recap [id]                   Selected or latest recap as structured JSON
    meeting recap edit [id] <json>       Rewrite summary / decisions / next_actions by hand (L1)
    meeting recap reset [id]             Hand edited minutes back to the model (L1)
    meeting transcript [id]              Selected or latest transcript lines (text only)
    meeting prefs [show]                 Minutes language + custom minutes instruction
    meeting prefs set [--language auto|english|japanese] [--instruction <text>|--clear-instruction]
                                         Set Recap preferences (L1)
    meeting list [before_id]            Saved meetings, 50 at a time
    meeting search <words…>             Saved meetings whose minutes or transcript match
    meeting delete <id>                 Delete one saved meeting for good (L1)
    wiki list [--include-low]          Source-backed entities and explicit self notes
    wiki evidence <key>               Retained source excerpts; use a key from wiki list
    wiki neighbors <key>              Shared-source neighbors, not semantic links
    voice microphone show|set <name>|default    Manage dictation input (L1 set)
    mail status                          Whether mail triage is on (the master switch)
    mail on|off                          Turn mail triage on or off (L1)
    mail triage [--bucket important|other] [--attention needs_reply|waiting_on_them|fyi|none]
                [--label KEY] [--handled]  Triaged threads (estimates only, no bodies)
    mail triage show <thread_key>        One thread's triage row (thread_key = gmail:<id>)
    mail triage set <thread_key> [--label KEY] [--bucket B] [--handled|--unhandled]
                                         Correct a thread's triage (L1)
    mail triage archive <thread_key>     Ask to archive on Gmail (L2): pending until you approve
                                         it in the app (poll with `actions poll <id>`), unless
                                         the label has your standing Auto Archive approval
    mail triage unarchive <thread_key>   Put an archived thread back (L2): pending until you
                                         approve it in the app
    mail labels                          Labels (built-in + custom) and their flags
    mail labels set <key-or-name> [--name N] [--definition D] [--bucket B] [--enabled on|off]
                [--auto-archive on|off] [--sync on|off]
                                         Create/edit a custom label or set flags (L1).
                                         A new label needs --definition and --bucket.
                                         Turning auto-archive/sync on is a standing approval
                                         only you grant, in the app (Settings > Connections >
                                         Triage); here it is refused. Turning them off works
    mail labels rm <key>                 Delete a custom label (L1)
    mail rules                           Sender rules (hand-made and learned)
    mail rules add --addr X|--domain X --label KEY [--bucket B]   Add a sender rule (L1)
    mail rules rm <id>                   Delete a sender rule (L1)
    mail history                         Gmail write-back history (the undo list)
    config path|show|validate Show the Shougun.md path, the parsed config (JSON), or
                              validation (ok, or one error per line; exit 1)
    principal import --from <memory.db> --root <dir> --principal <id>
                              Copy a single-user store into <dir>/<id>/memory.db and apply the
                              pending migrations (local; the key comes from SHOGUN_DB_KEY or
                              SHOGUN_DB_KEY_<ID>)
    models list|path          Local embedding models (local; no server)
    models fetch [--model M]  Download the pinned embedding model (the only command that
                              downloads one; needs a CLI built with --features embed-local)
    models verify [--model M] Check the installed model files against their pinned SHA-256
    help                      This help

GLOBAL FLAGS:
    --token <t>   API token (else the SHOGUN_API_TOKEN environment variable)
    --include-low Include <0.5 confidence results (reads; default excludes them)

Output is the server's JSON, as received.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_commands_map_to_read_tools() {
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        for (cmd, _) in [
            (
                Command::Search {
                    query: "x".into(),
                    depth: None,
                    from_ms: None,
                    to_ms: None,
                    source_kind: None,
                    limit: None,
                    ai: false,
                },
                (),
            ),
            (Command::Context, ()),
            (Command::People(ListOrGet::List), ()),
            (Command::Commitments(ListOrGet::Get { id: 1 }), ()),
        ] {
            let tool = cmd.tool().unwrap();
            assert_eq!(tool_level(tool), ApiLevel::Read, "{cmd:?} must be a read");
        }
    }

    #[test]
    fn note_is_l1_propose_is_l2_run_is_per_action() {
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        assert_eq!(
            tool_level(Command::Note { text: "n".into() }.tool().unwrap()),
            ApiLevel::Write(Level::L1)
        );
        assert_eq!(
            tool_level(Command::MeetingRecordNow { in_person: false }.tool().unwrap()),
            ApiLevel::Write(Level::L2)
        );
        assert_eq!(
            tool_level(Command::MeetingStartStatus { id: 1 }.tool().unwrap()),
            ApiLevel::Read
        );
        assert_eq!(
            tool_level(Command::Run { agent: "a".into() }.tool().unwrap()),
            ApiLevel::PerAction
        );
        // Issue #394: listing questions is a read, answering one is L1 — never an approval.
        assert_eq!(
            tool_level(Command::ActionsQuestions { id: None }.tool().unwrap()),
            ApiLevel::Read
        );
        assert_eq!(
            tool_level(Command::ActionsAnswer { id: 1, answer: "a".into() }.tool().unwrap()),
            ApiLevel::Write(Level::L1)
        );
        // Issue #395: steering a running run is L1 too — it approves nothing.
        assert_eq!(
            tool_level(
                Command::ActionsSteer { kind: "chat".into(), id: 1, text: "a".into() }.tool().unwrap()
            ),
            ApiLevel::Write(Level::L1)
        );
    }

    #[test]
    fn local_commands_have_no_tool() {
        assert!(Command::ApiStatus.tool().is_none());
        assert!(Command::Help.tool().is_none());
        assert!(Command::Config {
            action: ConfigAction::Path
        }
        .tool()
        .is_none());
    }

    #[test]
    fn lessons_commands_map_to_the_lessons_tools_at_ui_levels() {
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        assert_eq!(
            Command::Lessons(LessonsCommand::List(LessonsList::default())).tool(),
            Some(Tool::LessonsList)
        );
        assert_eq!(tool_level(Tool::LessonsList), ApiLevel::Read);
        for cmd in [
            Command::Lessons(LessonsCommand::Enable { id: 1 }),
            Command::Lessons(LessonsCommand::Disable { id: 1 }),
        ] {
            assert_eq!(cmd.tool(), Some(Tool::LessonsSetActive));
            assert_eq!(tool_level(cmd.tool().unwrap()), ApiLevel::Write(Level::L1));
        }
    }

    #[test]
    fn disputes_commands_map_to_the_dispute_tools_at_ui_levels() {
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        assert_eq!(
            Command::Disputes(DisputesCommand::List).tool(),
            Some(Tool::StateDisputesList)
        );
        assert_eq!(tool_level(Tool::StateDisputesList), ApiLevel::Read);
        let resolve = Command::Disputes(DisputesCommand::Resolve {
            id: 3,
            resolution: "keep_later".into(),
        });
        assert_eq!(resolve.tool(), Some(Tool::StateDisputesResolve));
        assert_eq!(
            tool_level(Tool::StateDisputesResolve),
            ApiLevel::Write(Level::L1)
        );
    }

    #[test]
    fn get_variants_map_to_get_tools() {
        assert_eq!(
            Command::People(ListOrGet::Get { id: 3 }).tool(),
            Some(Tool::StatePeopleGet)
        );
        assert_eq!(
            Command::Projects(ListOrGet::List).tool(),
            Some(Tool::StateProjectsList)
        );
    }

    #[test]
    fn backup_commands_map_to_the_symmetric_tools() {
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        assert_eq!(Command::Backup(BackupCommand::Status).tool(), Some(Tool::BackupStatus));
        assert_eq!(tool_level(Tool::BackupStatus), ApiLevel::Read);
        let set = Command::Backup(BackupCommand::Retention { days: 7 });
        assert_eq!(set.tool(), Some(Tool::BackupSetRetention));
        assert_eq!(tool_level(Tool::BackupSetRetention), ApiLevel::Write(Level::L1));
    }

    #[test]
    fn search_index_commands_map_to_the_symmetric_tools() {
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        for (command, tool, level) in [
            (SearchIndexCommand::Status, Tool::SearchIndexStatus, ApiLevel::Read),
            (SearchIndexCommand::Compact, Tool::SearchIndexCompactStart, ApiLevel::Write(Level::L1)),
            (SearchIndexCommand::Stop, Tool::SearchIndexCompactStop, ApiLevel::Write(Level::L1)),
        ] {
            assert_eq!(Command::SearchIndex(command).tool(), Some(tool));
            assert_eq!(tool_level(tool), level);
        }
    }

    #[test]
    fn retention_maps_to_an_l3_ceiling_tool() {
        // Issue #782: a shorter retention runs at L1, but a longer one waits for the user, so the
        // tool's ceiling is L3 (the CLI reports the ceiling).
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        let command = Command::VisualRecall(VisualRecallCommand::Retention { days: 30 });
        assert_eq!(command.tool(), Some(Tool::VisualRecallSetRetention));
        assert_eq!(
            tool_level(command.tool().unwrap()),
            ApiLevel::Write(Level::L3)
        );
    }

    #[test]
    fn frame_rescan_maps_to_an_l1_write_over_the_same_post() {
        // Issue #410: the rescan overwrites stored OCR text, so the CLI reports it as L1, and
        // it still POSTs to the same path the REST face gates as an L1 write.
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        use shogun_mcp::rest::{route, Method, Routed};
        let command = Command::VisualRecall(VisualRecallCommand::FrameRescan { id: 12 });
        assert_eq!(command.tool(), Some(Tool::VisualRecallRescanFrame));
        assert_eq!(
            tool_level(command.tool().unwrap()),
            ApiLevel::Write(Level::L1)
        );
        let call = crate::wire::to_call(&command, false).unwrap();
        assert_eq!(
            (call.method, call.path.as_str()),
            ("POST", "/v1/visual_recall/frames/12/rescan")
        );
        let mut tokens = shogun_mcp::memory_api::TokenRegistry::new();
        tokens.issue("t");
        let rest_request = shogun_mcp::rest::RestRequest {
            method: Method::Post,
            path: call.path,
            token: Some("t".into()),
            include_low: false,
            query: None,
            search_depth: None,
            search_source_kind: None,
            search_mode: None,
            search_bound_invalid: false,
            body: call.body,
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
            route(
                &rest_request,
                &tokens,
                &shogun_mcp::entitlement::entitlements(shogun_mcp::entitlement::Plan::Pro, 0)
            ),
            Routed::Write {
                tool: Tool::VisualRecallRescanFrame,
                level: Level::L1
            }
        );
    }

    #[test]
    fn mail_commands_map_to_the_mail_tools_at_the_mail_pane_levels() {
        use shogun_agents::permission::Level;
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        let key = || "gmail:18f3".to_string();
        let cases = [
            (
                Command::Mail(MailCommand::TriageList(MailTriageList::default())),
                Tool::MailTriageList,
                ApiLevel::Read,
            ),
            (
                Command::Mail(MailCommand::TriageShow { thread_key: key() }),
                Tool::MailTriageGet,
                ApiLevel::Read,
            ),
            (
                Command::Mail(MailCommand::TriageSet {
                    thread_key: key(),
                    label: None,
                    bucket: None,
                    handled: Some(true),
                }),
                Tool::MailTriageSet,
                ApiLevel::Write(Level::L1),
            ),
            (
                Command::Mail(MailCommand::TriageArchive { thread_key: key() }),
                Tool::MailTriageArchive,
                ApiLevel::Write(Level::L2),
            ),
            (
                Command::Mail(MailCommand::TriageUnarchive { thread_key: key() }),
                Tool::MailTriageUnarchive,
                ApiLevel::Write(Level::L2),
            ),
            (
                Command::Mail(MailCommand::LabelsList),
                Tool::MailLabelsList,
                ApiLevel::Read,
            ),
            (
                Command::Mail(MailCommand::LabelsSet(MailLabelSet::default())),
                Tool::MailLabelsUpsert,
                ApiLevel::Write(Level::L1),
            ),
            (
                Command::Mail(MailCommand::LabelsRemove {
                    key: "custom:vendors".into(),
                }),
                Tool::MailLabelsDelete,
                ApiLevel::Write(Level::L1),
            ),
            (
                Command::Mail(MailCommand::RulesList),
                Tool::MailSenderRulesList,
                ApiLevel::Read,
            ),
            (
                Command::Mail(MailCommand::RulesAdd {
                    match_kind: "domain".into(),
                    match_value: "x.com".into(),
                    label: "news".into(),
                    bucket: None,
                }),
                Tool::MailSenderRulesAdd,
                ApiLevel::Write(Level::L1),
            ),
            (
                Command::Mail(MailCommand::RulesRemove { id: 7 }),
                Tool::MailSenderRulesDelete,
                ApiLevel::Write(Level::L1),
            ),
            (
                Command::Mail(MailCommand::History),
                Tool::MailHistoryList,
                ApiLevel::Read,
            ),
        ];
        for (cmd, tool, level) in cases {
            assert_eq!(cmd.tool(), Some(tool), "{cmd:?}");
            assert_eq!(tool_level(tool), level, "{cmd:?}");
        }
        assert!(is_mail_label_key("marketing"));
        assert!(is_mail_label_key("custom:vendors"));
        assert!(!is_mail_label_key("Vendors"));
        for line in [
            "mail triage show <thread_key>",
            "mail triage archive <thread_key>",
            "mail labels set <key-or-name>",
            "Settings > Connections >",
            "mail rules add --addr X|--domain X --label KEY",
            "mail history",
            "mail status",
            "mail on|off",
            "A new label needs --definition and --bucket",
        ] {
            assert!(USAGE.contains(line), "usage lacks {line}");
        }
    }

    #[test]
    fn usage_matches_what_the_commands_do() {
        // The disputes line describes `disputes resolve`, not the export folder above it.
        let resolve = USAGE.find("disputes resolve <id>").unwrap();
        let settle = USAGE.find("Settle one flag").unwrap();
        let next = USAGE.find("state edit <table> <id>").unwrap();
        assert!(resolve < settle && settle < next);
        assert_eq!(USAGE.matches("Settle one flag").count(), 1);
        // `run` sends an action spec; it does not launch a preset agent.
        assert!(USAGE.contains("run <action-json>"));
        assert!(!USAGE.contains("preset agent"));
        // The voice term shape is spelled out, and no flag is advertised that does nothing.
        assert!(USAGE.contains("voice-dictionary update <id> <term-json>"));
        assert!(USAGE.contains("\"canonical\""));
        assert!(!USAGE.contains("--no-screen"));
        assert!(USAGE.contains("the parsed config (JSON)"));
    }

    #[test]
    fn voice_dictionary_bulk_commands_map_to_the_shared_tools_and_are_documented() {
        assert_eq!(
            Command::VoiceDictionary(VoiceDictionaryCommand::Import { source: ImportSource::Stdin }).tool(),
            Some(Tool::VoiceDictionaryImport)
        );
        assert_eq!(
            Command::VoiceDictionary(VoiceDictionaryCommand::Export).tool(),
            Some(Tool::VoiceDictionaryExport)
        );
        assert!(USAGE.contains("voice-dictionary import <file|->"));
        assert!(USAGE.contains("voice-dictionary export"));
    }

    #[test]
    fn visual_recall_search_help_lists_every_supported_filter() {
        assert!(USAGE
            .contains("visual-recall search <q> [--from-ms N] [--to-ms N] [--app-bundle-id ID]"));
    }

    #[test]
    fn state_commands_map_to_the_world_model_tools_at_ui_levels() {
        use shogun_mcp::memory_api::{tool_level, ApiLevel};
        use shogun_agents::permission::Level;
        let edit = Command::State(StateCommand::Edit { body: String::new() });
        let delete = Command::State(StateCommand::Delete { body: String::new() });
        assert_eq!(edit.tool(), Some(Tool::StateUpdate));
        assert_eq!(delete.tool(), Some(Tool::StateDelete));
        assert_eq!(Command::State(StateCommand::Create { body: String::new() }).tool(), Some(Tool::StateCreate));
        assert_eq!(tool_level(Tool::StateCreate), ApiLevel::Write(Level::L1));
        assert_eq!(Command::State(StateCommand::Export).tool(), Some(Tool::StateExport));
        let split = Command::State(StateCommand::Split { body: String::new() });
        assert_eq!(split.tool(), Some(Tool::StateSplit));
        assert_eq!(tool_level(Tool::StateSplit), ApiLevel::Write(Level::L1));
        let evidence = Command::State(StateCommand::Evidence { table: "people".into(), id: 1 });
        assert_eq!(evidence.tool(), Some(Tool::StateSplitEvidence));
        assert_eq!(tool_level(Tool::StateSplitEvidence), ApiLevel::Read);
        assert!(USAGE.contains("state split <people|projects> <id> --name T --event <id>"));
        for (cmd, tool, level) in [
            (ImportCommand::Preview { body: String::new() }, Tool::MemoryImportPreview, ApiLevel::Read),
            (ImportCommand::Commit { body: String::new() }, Tool::MemoryImport, ApiLevel::Write(Level::L1)),
            (ImportCommand::List, Tool::MemoryImportList, ApiLevel::Read),
            (ImportCommand::Show { id: 1 }, Tool::MemoryImportList, ApiLevel::Read),
            (ImportCommand::Delete { id: 1 }, Tool::MemoryImportDelete, ApiLevel::Write(Level::L1)),
        ] {
            assert_eq!(Command::Import(cmd).tool(), Some(tool));
            assert_eq!(tool_level(tool), level);
        }
        assert!(USAGE.contains("import preview <package.json> | --computer-history"));
        assert_eq!(tool_level(Tool::StateUpdate), ApiLevel::Write(Level::L1));
        assert_eq!(tool_level(Tool::StateDelete), ApiLevel::Write(Level::L1));
        assert_eq!(tool_level(Tool::StateExport), ApiLevel::Read);
        assert!(USAGE.contains("state edit <table> <id>"));
    }
}
