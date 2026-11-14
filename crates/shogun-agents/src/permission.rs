//! The L1/L2/L3 permission model (spec §6.6.1) and the invariant-4 guarantee.
//!
//! CLAUDE.md invariant 4: an L1 (auto-executed) action must never send anything off the device
//! — sends, posts, and calendar creation are always L3 (explicit confirmation). This is
//! enforced structurally: [`Action`] splits into [`LocalAction`] (on-device only) and
//! [`SendAction`] (leaves the device), and the level assignment maps every `SendAction` to
//! [`Level::L3`]. There is no `SendAction` variant reachable from L1, so an auto-run can never
//! be a send — a mislabel is impossible, not merely discouraged.

/// The three permission levels (spec §6.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Auto-execute (no confirmation). Local, reversible, on-device only.
    L1,
    /// One-tap confirmation.
    L2,
    /// Explicit confirmation — required for everything that leaves the device (invariant 4).
    L3,
}

/// On-device actions — none of these leave the device, so all are L1/L2 eligible (never a send).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAction {
    /// Bring an app to the foreground.
    OpenApp { bundle_id: String },
    /// Reveal a file in Finder.
    RevealFile { path: String },
    /// Run a local memory search.
    LocalSearch { query: String },
    /// Show a local notification / Notch indicator.
    ShowNotification { text: String },
    /// Put text on the clipboard (e.g. a generated draft the user will paste — not a send).
    CopyToClipboard { text: String },
    /// Update a local state record (people/projects/…). Local mutation, not an external write.
    UpdateState { table: &'static str, state_id: i64 },
    /// Save a draft locally (the "draft-stop mode" default for email — never sends). Persists a
    /// row, so it is L2 like [`LocalAction::UpdateState`] (and like the preset tables'
    /// `DraftGenerate`).
    SaveDraft { target: &'static str },
    /// Open the reply drafter on the message on screen, in the language it was written in
    /// (issue #279 FR-TL-10 (1)). Generates on the Agent lane and stops at Insert — never a send
    /// (FR-TL-14). `language` is the detected incoming language tag (`ja`, `en`).
    ReplyDraft { language: String },
    /// Write a new file into a folder the user chose for exports (issue #397): a meeting Recap
    /// as Markdown / CSV. `path_in_allowed_root` is the intended file path; the executor refuses
    /// anything that does not resolve inside an allowed folder, and never overwrites (a taken
    /// name gets ` (2)`, ` (3)`, …) nor deletes. It persists a file the user did not have
    /// before, so it is L2 like [`LocalAction::SaveDraft`]. Never a send.
    WriteFile { path_in_allowed_root: String, format: ExportFormat, body: String },
}

/// The file formats [`LocalAction::WriteFile`] writes (issue #397). Plain text only — no
/// document engines, no binary formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Csv,
}

impl ExportFormat {
    /// The file extension (without the dot), which is also the wire name.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            ExportFormat::Csv => "csv",
        }
    }

    /// Parse the wire name (`md` / `markdown` / `csv`).
    pub fn parse(s: &str) -> Option<ExportFormat> {
        match s.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Some(ExportFormat::Markdown),
            "csv" => Some(ExportFormat::Csv),
            _ => None,
        }
    }
}

/// Actions that leave the device. By CLAUDE.md invariant 4 these are **always L3**.
///
/// The set is deliberately total over the scope table's `ExternalSend` rows
/// (`shogun_mcp::scope`): every operation that can leave the device has a variant here, so the
/// routing from a service operation to an action is a derivation rather than a match with a
/// silent fallback. An operation with no representation would have to be special-cased at the
/// call site, and a special case in this particular map is how invariant 4 gets lost.
///
/// The variants describe *what the user is about to have happen*, not which vendor does it —
/// the confirmation UI reads these, and "post a message to #eng" is the sentence a person can
/// approve, while "slack.post_message" is not.
///
/// Variants that only change a local setting on confirm (`ShareWritingStyle`, `ResumeCapture`,
/// `UseSubscriptionAgent`, `WidenVisualRecall`, `UnpauseCapture`) send nothing at confirm time,
/// but each widens what SHOGUN reads, keeps or sends afterwards, so an API caller can only
/// request one and it waits in the same L3 queue as a send. These and `ArchiveMailThread` map to
/// no scope-table op: `send_bridge::route_send` gives them no transport, and the desktop applies
/// them on confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendAction {
    /// Send an email (via Composio, opt-in — §6.10).
    SendEmail { to: String },
    /// Post a message to a chat service (Slack, …).
    PostMessage { channel: String },
    /// React to someone's message (an emoji is still visible to everyone in the channel).
    AddReaction { target: String },
    /// Create a calendar event.
    CreateCalendarEvent { title: String },
    /// Change or cancel an existing calendar event — visible to every attendee.
    UpdateCalendarEvent { title: String },
    /// Post a comment / review on an issue/PR (GitHub, Linear, …).
    PostComment { target: String },
    /// Create a document, page or file in a service (Drive, Notion, …).
    CreateDocument { title: String },
    /// Change an existing document or page in a service.
    UpdateDocument { title: String },
    /// Move an issue to another state (Linear, …) — visible to the whole team.
    ChangeIssueStatus { target: String },
    /// Start sharing the user's learned writing style with their model provider (issue #654):
    /// from then on, up to four of the user's own same-app examples go out with each rewrite.
    /// Confirming turns a local setting on; it opens a standing egress of the user's own text.
    ShareWritingStyle,
    /// Enable local automatic dictionary additions after an in-app approval.
    EnableDictionaryAutoAdd,
    /// Stop excluding apps or websites from capture (issue #657): an MCP / REST / CLI
    /// `capture_exclusions.set` that removes at least one exclusion. Confirming replaces the
    /// local exclusion lists, which widens what SHOGUN reads from the screen. Adding exclusions
    /// only narrows capture and stays a direct write. The desktop re-checks against the lists as
    /// they are at confirm time.
    ResumeCapture { change: CaptureExclusionChange },
    /// Run the Agent lane on the user's own subscription through a vendor CLI (issue #110,
    /// invariant 6): an MCP / REST / CLI `agent.source.set` that selects a subscription delegate
    /// (`delegate` is its stable id, e.g. `claude-code`). From then on chat and draft prompts
    /// leave through that vendor's consumer plan, whose data handling differs from the metered API
    /// path; the user's confirm is the opt-in consent the desktop's disclosure asks for. Switching
    /// to an API key or revoking the consent only narrows egress and stays direct.
    UseSubscriptionAgent { delegate: String },
    /// Widen Visual recall (issue #782): an MCP / REST / CLI `visual_recall.set_enabled` that
    /// turns it on while it is Off, or a `visual_recall.set_retention` that keeps saved screens
    /// longer than the retention in force. Confirming changes the local `visual_recall.json` and
    /// starts (or lengthens) keeping encrypted screen frames. Turning it off or shortening
    /// retention only narrows what is kept and stays a direct write. The desktop never applies
    /// more than the card showed.
    WidenVisualRecall { change: VisualRecallWidening },
    /// Archive a Gmail thread, or put it back in the Inbox (`unarchive`), because an MCP / REST /
    /// CLI caller asked (`mail.triage.archive` / `mail.triage.unarchive`, issue #779). The Gmail
    /// change itself is a [`ServiceStateAction`] (L2: reversible, the user's own mailbox, nobody
    /// else sees it), and the desktop's Mail pane runs it on the user's own tap. An API caller is
    /// not the user, though — a client steered by text in an email it read could empty the Inbox
    /// — so its request waits in the shared approval queue for the user's one-tap confirm, the
    /// same queue every other API request that needs the user goes through (FR-AG-04 /
    /// invariant 6). The only exception is a thread whose label already carries the user's
    /// standing approval (`auto_archive`, FR-TG-21): that archive is recorded directly.
    /// `thread_key` is the triage key (`gmail:<thread id>`). Confirming records the request for
    /// the desktop's mail write-back tick, which executes it (traced, logged, and undoable for 7
    /// days) after re-checking the thread.
    ArchiveMailThread { thread_key: String, unarchive: bool },
    /// End the user's capture pause (issue #897): an MCP / REST / CLI `capture.set_paused` with
    /// `paused: false` while capture is paused. Confirming resumes the local capture loop, so
    /// SHOGUN reads the screen again after the user stopped it. Pausing, or keeping a pause at
    /// least as long, stays a direct write. `pause` is the pause in force when the request was
    /// made: confirming ends only that pause, and fails closed when the pause has changed since
    /// (the user resumed and paused again, or a timed pause ran out).
    UnpauseCapture { pause: PausedCapture },
    /// Pay a payee through the payment rail (E46). Money leaves the principal: always L3, like
    /// every send. A payment inside a mandate a human granted carries that grant as its L3
    /// confirmation ([`crate::payment::confirmed_under_mandate`]); any other payment waits in the
    /// approval queue for a human. No connector transport accepts it: only the payment rail does.
    Pay { order: Box<crate::payment::PaymentOrder> },
    /// Grant, widen or extend a payment mandate (E46). Confirming writes the mandate into the
    /// principal's store (nothing leaves the device then), but it lets an agent pay later
    /// without asking again, so it is an L3 human decision like the settings that widen egress.
    GrantPaymentMandate { grant: Box<crate::payment::MandateGrant> },
}

/// The capture pause an [`SendAction::UnpauseCapture`] request was made against (issue #897).
/// `resume_at_ms` is when that pause ends on its own; `None` for a pause that waits to be
/// resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PausedCapture {
    pub resume_at_ms: Option<i64>,
}

/// The longest Gmail triage thread key an archive approval carries. Real keys are `gmail:` plus
/// a 16-hex-digit id; the bound only keeps a tampered queue row from growing without limit.
pub const MAIL_THREAD_KEY_MAX_BYTES: usize = 256;

/// Whether `key` is a Gmail triage thread key an archive approval may carry (issue #779):
/// `gmail:` followed by a non-empty id of visible ASCII characters, within
/// [`MAIL_THREAD_KEY_MAX_BYTES`]. Anything else is never queued and never loaded.
pub fn plausible_mail_thread_key(key: &str) -> bool {
    key.len() <= MAIL_THREAD_KEY_MAX_BYTES
        && key
            .strip_prefix("gmail:")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_graphic()))
}

/// The issue or pull request a GitHub comment approval names: the exact
/// `https://github.com/{owner}/{repo}/issues/{n}` (or `/pull/{n}`) URL, split into the fields the
/// provider takes. A trailing slash, query or fragment is fine; `acme/repo#7` or another host is not.
pub fn github_issue_address(target: &str) -> Option<(&str, &str, u64)> {
    let path = target.trim().strip_prefix("https://github.com/")?;
    let path = path.split(['?', '#']).next()?.trim_end_matches('/');
    let mut parts = path.split('/');
    let (owner, repo, kind) = (parts.next()?, parts.next()?, parts.next()?);
    let number = parts.next()?.parse::<u64>().ok().filter(|n| *n > 0)?;
    let name = |s: &str| {
        !s.bytes().all(|b| b == b'.')
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    (parts.next().is_none() && name(owner) && name(repo) && matches!(kind, "issues" | "pull"))
        .then_some((owner, repo, number))
}

/// Whether `id` is a Slack conversation or user ID a post can be addressed to (`C…`, `G…`, `D…`,
/// `U…`, or an Enterprise Grid `W…`). A `#channel` or a display name is not: the post tool takes
/// the ID, and a card addressed to a name could never send.
pub fn is_slack_conversation_id(id: &str) -> bool {
    matches!(id.as_bytes().first(), Some(b'C' | b'G' | b'D' | b'U' | b'W'))
        && (9..=32).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Why a send could never run as addressed with this build's connectors, or `None` when it can.
/// Checked when a send is queued (so a card that cannot send is never offered for approval) and
/// again before it runs (so a card queued by an older build never leaves a traceability row).
pub fn send_target_refusal(action: &SendAction) -> Option<&'static str> {
    match action {
        SendAction::PostMessage { channel } if !is_slack_conversation_id(channel) => {
            Some("slack_target_unresolved")
        }
        SendAction::PostComment { target } if github_issue_address(target).is_none() => {
            Some("GitHub comments need the full https://github.com/owner/repo/issues/N or /pull/N link.")
        }
        // The approval carries a title only: no time, attendees, page or value to apply.
        SendAction::CreateCalendarEvent { .. } | SendAction::UpdateCalendarEvent { .. } => {
            Some("Calendar changes aren't available yet.")
        }
        SendAction::UpdateDocument { .. } => Some("Page changes aren't available yet."),
        SendAction::AddReaction { .. } => Some("Reactions aren't available yet."),
        SendAction::ChangeIssueStatus { .. } => Some("Issue status changes aren't available yet."),
        _ => None,
    }
}

/// A Visual recall setting change that keeps more of the screen (issue #782), as queued for the
/// user's approval. Exactly what the card shows and the desktop applies: `enable` turns Visual
/// recall on; `retention_days` keeps saved screens that many days. `previous_retention_days` is
/// the retention in force when the request was made, so the card can say what changes. Applying
/// never sets a retention longer than `retention_days`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VisualRecallWidening {
    pub enable: bool,
    pub retention_days: Option<u32>,
    pub previous_retention_days: u32,
}

/// The longest Visual recall retention any face accepts, in days (ten years). Mirrors
/// `shogun_core::capture::visual_recall::MAX_CUSTOM_RETENTION_DAYS`; a core test pins the two.
pub const VISUAL_RECALL_MAX_RETENTION_DAYS: u32 = 3_650;

impl VisualRecallWidening {
    /// Whether the request keeps more of the screen than the settings it was classified
    /// against. One that widens nothing is a direct write and is never queued.
    pub fn widens(&self) -> bool {
        self.enable
            || self
                .retention_days
                .is_some_and(|days| days > self.previous_retention_days)
    }

    /// A request the approval card can show and the desktop can apply: it widens something and
    /// every day count is within the bounds the settings accept.
    pub fn is_valid(&self) -> bool {
        let in_bounds = |days: u32| (1..=VISUAL_RECALL_MAX_RETENTION_DAYS).contains(&days);
        self.widens()
            && in_bounds(self.previous_retention_days)
            && self.retention_days.is_none_or(in_bounds)
    }
}

/// A `capture_exclusions.set` request that removes exclusions (issue #657), as queued for the
/// user's approval. The lists are the complete requested lists (normalized: bundle ids as given,
/// website rules as exact lower-case hosts); `resumed_*` are the entries excluded when the request
/// was made that confirming would start capturing again — what the approval card shows. Applying
/// refuses if, by then, the replacement would stop excluding anything outside `resumed_*`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureExclusionChange {
    pub apps: Vec<String>,
    pub websites: Vec<String>,
    pub resumed_apps: Vec<String>,
    pub resumed_websites: Vec<String>,
}

impl CaptureExclusionChange {
    /// Whether the request stops excluding anything. A change that resumes nothing is a plain
    /// tightening and is never queued.
    pub fn resumes_capture(&self) -> bool {
        !self.resumed_apps.is_empty() || !self.resumed_websites.is_empty()
    }
}

/// Reversible changes to the user's **own** state on a service — nothing a third person sees
/// (issue #319, §6.9.2 `ServiceStateChange`). Not local (they cross the device boundary, so
/// never L1), not a send (nothing is delivered to anyone, so never L3): always **L2**. They may
/// run under a per-label *standing* approval (FR-TG-21) — the level is unchanged; only when the
/// one-tap was given moves earlier, and every execution is logged and undoable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStateAction {
    /// Remove a Gmail thread from the inbox (`INBOX` label removal). Reversible by
    /// [`ServiceStateAction::UnarchiveThread`].
    ArchiveThread { thread_id: String },
    /// Put a Gmail thread back in the inbox.
    UnarchiveThread { thread_id: String },
    /// Add / remove SHOGUN-owned labels on a Gmail thread.
    ModifyLabels { thread_id: String, add: Vec<String>, remove: Vec<String> },
}

impl ServiceStateAction {
    pub fn kind(&self) -> &'static str {
        match self {
            ServiceStateAction::ArchiveThread { .. } => "archive_thread",
            ServiceStateAction::UnarchiveThread { .. } => "unarchive_thread",
            ServiceStateAction::ModifyLabels { .. } => "modify_labels",
        }
    }

    /// The scope-table op this action maps to (`shogun_mcp::scope` Gmail rows).
    pub fn scope_op(&self) -> &'static str {
        match self {
            ServiceStateAction::ArchiveThread { .. } => "archive",
            ServiceStateAction::UnarchiveThread { .. } => "unarchive",
            ServiceStateAction::ModifyLabels { .. } => "label_sync",
        }
    }

    /// The thread the action targets.
    pub fn thread_id(&self) -> &str {
        match self {
            ServiceStateAction::ArchiveThread { thread_id }
            | ServiceStateAction::UnarchiveThread { thread_id }
            | ServiceStateAction::ModifyLabels { thread_id, .. } => thread_id,
        }
    }

    /// The action that undoes this one (label modification swaps add/remove).
    pub fn inverse(&self) -> ServiceStateAction {
        match self {
            ServiceStateAction::ArchiveThread { thread_id } => {
                ServiceStateAction::UnarchiveThread { thread_id: thread_id.clone() }
            }
            ServiceStateAction::UnarchiveThread { thread_id } => {
                ServiceStateAction::ArchiveThread { thread_id: thread_id.clone() }
            }
            ServiceStateAction::ModifyLabels { thread_id, add, remove } => ServiceStateAction::ModifyLabels {
                thread_id: thread_id.clone(),
                add: remove.clone(),
                remove: add.clone(),
            },
        }
    }
}

/// An agent action: on-device, a reversible own-state change on a service, or an external send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Local(LocalAction),
    ServiceState(ServiceStateAction),
    Send(SendAction),
}

impl LocalAction {
    pub fn kind(&self) -> &'static str {
        match self {
            LocalAction::OpenApp { .. } => "open_app",
            LocalAction::RevealFile { .. } => "reveal_file",
            LocalAction::LocalSearch { .. } => "local_search",
            LocalAction::ShowNotification { .. } => "show_notification",
            LocalAction::CopyToClipboard { .. } => "copy_to_clipboard",
            LocalAction::UpdateState { .. } => "update_state",
            LocalAction::SaveDraft { .. } => "save_draft",
            LocalAction::ReplyDraft { .. } => "reply_draft",
            LocalAction::WriteFile { .. } => "write_file",
        }
    }
}

impl SendAction {
    pub fn kind(&self) -> &'static str {
        match self {
            SendAction::SendEmail { .. } => "send_email",
            SendAction::PostMessage { .. } => "post_message",
            SendAction::AddReaction { .. } => "add_reaction",
            SendAction::CreateCalendarEvent { .. } => "create_calendar_event",
            SendAction::UpdateCalendarEvent { .. } => "update_calendar_event",
            SendAction::PostComment { .. } => "post_comment",
            SendAction::CreateDocument { .. } => "create_document",
            SendAction::UpdateDocument { .. } => "update_document",
            SendAction::ChangeIssueStatus { .. } => "change_issue_status",
            SendAction::ShareWritingStyle => "share_writing_style",
            SendAction::EnableDictionaryAutoAdd => "enable_dictionary_autoadd",
            SendAction::ResumeCapture { .. } => "resume_capture",
            SendAction::UseSubscriptionAgent { .. } => "use_subscription_agent",
            SendAction::WidenVisualRecall { .. } => "widen_visual_recall",
            SendAction::ArchiveMailThread { .. } => "archive_mail_thread",
            SendAction::UnpauseCapture { .. } => "unpause_capture",
            SendAction::Pay { .. } => "pay",
            SendAction::GrantPaymentMandate { .. } => "grant_payment_mandate",
        }
    }
}

impl Action {
    /// Stable snake_case kind name, one per variant and never per payload (FR-PAT-04). It is the
    /// join key between "offered" (`offer_impressions`) and "decided"
    /// (`feedback_events.action_kind`): both sides must aggregate under one vocabulary.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Local(a) => a.kind(),
            Action::ServiceState(s) => s.kind(),
            Action::Send(s) => s.kind(),
        }
    }

    /// The minimum permission level required to run this action.
    ///
    /// Every [`SendAction`] is L3 (invariant 4). Local actions are L1 when they are trivially
    /// reversible and side-effect-light, else L2. No local action is L3, and — crucially — no
    /// send is ever below L3.
    pub fn required_level(&self) -> Level {
        match self {
            Action::Send(_) => Level::L3,
            // Crosses the device boundary (never L1); delivers nothing to anyone (never L3).
            Action::ServiceState(_) => Level::L2,
            Action::Local(a) => local_level(a),
        }
    }

    /// Whether this action may auto-execute at L1. True only for L1 local actions; a send can
    /// never be L1 (it is not even representable as a `LocalAction`).
    pub fn is_l1_eligible(&self) -> bool {
        self.required_level() == Level::L1
    }

    /// Whether this action leaves the device (a send). Sends always need L3 confirmation.
    pub fn is_external_send(&self) -> bool {
        matches!(self, Action::Send(_))
    }
}

fn local_level(a: &LocalAction) -> Level {
    match a {
        // Trivially reversible, no external effect → auto.
        LocalAction::OpenApp { .. }
        | LocalAction::RevealFile { .. }
        | LocalAction::LocalSearch { .. }
        | LocalAction::ShowNotification { .. }
        | LocalAction::CopyToClipboard { .. } => Level::L1,
        // Mutates persisted state → one-tap confirmation. SaveDraft writes a note row, and the
        // preset tables mandate DraftGenerate at L2 (OpKind::mandated_level) — keeping it L1 here
        // let fusion's draft candidates auto-run a persisted write with zero confirmation.
        LocalAction::UpdateState { .. } | LocalAction::SaveDraft { .. } => Level::L2,
        // Generates on the user's own Agent lane (their credentials — invariant 5's other half),
        // so one tap, like every other DraftGenerate. Never a send: the draft stops at Insert.
        LocalAction::ReplyDraft { .. } => Level::L2,
        // Creates a file on disk the user did not have — a persisted write, so one tap, like
        // SaveDraft. Local only: the folder is on this Mac and nothing is sent (issue #397).
        LocalAction::WriteFile { .. } => Level::L2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_targets_must_be_exact_before_queueing() {
        assert_eq!(
            github_issue_address("https://github.com/octo-cat/my.repo/pull/42/#issuecomment-1"),
            Some(("octo-cat", "my.repo", 42))
        );
        for bad in ["acme/repo#7", "https://attacker.invalid/a/b/issues/1", "https://github.com/a/b/issues/0",
            "https://github.com/../b/issues/1", "https://github.com/a/b/commits/1", "https://github.com/a/b/issues/1/x"] {
            assert_eq!(github_issue_address(bad), None, "{bad}");
        }
        for ok in ["C0123ABCD", "U0123ABCD", "W0123ABCD", "D0123ABCD", "G0123ABCD"] {
            assert!(is_slack_conversation_id(ok), "{ok}");
        }
        for bad in ["#general", "@dana", "c0123abcd", "C012", "X0123ABCD"] {
            assert!(!is_slack_conversation_id(bad), "{bad}");
        }
        assert!(send_target_refusal(&SendAction::PostMessage { channel: "#general".into() }).is_some());
        assert!(send_target_refusal(&SendAction::PostMessage { channel: "C0123ABCD".into() }).is_none());
        assert!(send_target_refusal(&SendAction::CreateCalendarEvent { title: "Sync".into() }).is_some());
        assert!(send_target_refusal(&SendAction::SendEmail { to: "a@b.co".into() }).is_none());
    }

    fn all_sends() -> Vec<Action> {
        vec![
            Action::Send(SendAction::SendEmail { to: "a@b.com".into() }),
            Action::Send(SendAction::PostMessage { channel: "#general".into() }),
            Action::Send(SendAction::CreateCalendarEvent { title: "Sync".into() }),
            Action::Send(SendAction::PostComment { target: "pr#12".into() }),
        ]
    }

    #[test]
    fn every_send_is_l3_and_never_l1() {
        for a in all_sends() {
            assert_eq!(a.required_level(), Level::L3, "{a:?} must be L3 (invariant 4)");
            assert!(!a.is_l1_eligible(), "{a:?} must never be L1-eligible");
            assert!(a.is_external_send());
        }
    }

    #[test]
    fn local_auto_actions_are_l1() {
        for a in [
            LocalAction::OpenApp { bundle_id: "com.apple.Safari".into() },
            LocalAction::RevealFile { path: "/x".into() },
            LocalAction::LocalSearch { query: "budget".into() },
            LocalAction::ShowNotification { text: "hi".into() },
            LocalAction::CopyToClipboard { text: "draft".into() },
        ] {
            let action = Action::Local(a);
            assert_eq!(action.required_level(), Level::L1);
            assert!(action.is_l1_eligible());
            assert!(!action.is_external_send());
        }
    }

    #[test]
    fn state_mutation_is_l2() {
        // Both persisted-write local actions require the one-tap confirm — matching the preset
        // tables, where DraftGenerate and StateWrite are mandated L2.
        for a in [
            LocalAction::UpdateState { table: "people", state_id: 1 },
            LocalAction::SaveDraft { target: "gmail" },
        ] {
            let a = Action::Local(a);
            assert_eq!(a.required_level(), Level::L2);
            assert!(!a.is_l1_eligible());
        }
    }

    #[test]
    fn writing_a_file_is_l2_local_and_never_a_send() {
        let a = Action::Local(LocalAction::WriteFile {
            path_in_allowed_root: "/Users/me/Notes/Recap.md".into(),
            format: ExportFormat::Markdown,
            body: "# Recap\n".into(),
        });
        assert_eq!(a.required_level(), Level::L2);
        assert!(!a.is_l1_eligible());
        assert!(!a.is_external_send());
        assert_eq!(a.kind(), "write_file");
    }

    #[test]
    fn export_formats_round_trip_their_wire_names() {
        for f in [ExportFormat::Markdown, ExportFormat::Csv] {
            assert_eq!(ExportFormat::parse(f.extension()), Some(f));
        }
        assert_eq!(ExportFormat::parse("Markdown"), Some(ExportFormat::Markdown));
        assert_eq!(ExportFormat::parse("pdf"), None);
    }

    #[test]
    fn no_local_action_is_ever_l3() {
        for a in [
            LocalAction::OpenApp { bundle_id: "x".into() },
            LocalAction::UpdateState { table: "projects", state_id: 9 },
            LocalAction::SaveDraft { target: "gmail" },
        ] {
            assert_ne!(Action::Local(a).required_level(), Level::L3);
        }
    }

    #[test]
    fn service_state_changes_are_l2_never_l1_never_a_send() {
        for a in [
            ServiceStateAction::ArchiveThread { thread_id: "t".into() },
            ServiceStateAction::UnarchiveThread { thread_id: "t".into() },
            ServiceStateAction::ModifyLabels { thread_id: "t".into(), add: vec!["L1".into()], remove: vec![] },
        ] {
            let inverse = a.inverse();
            assert_eq!(inverse.inverse(), a, "inverse is an involution");
            let action = Action::ServiceState(a);
            assert_eq!(action.required_level(), Level::L2);
            assert!(!action.is_l1_eligible());
            assert!(!action.is_external_send());
        }
    }

    #[test]
    fn an_archive_approval_carries_only_a_plausible_gmail_thread_key() {
        // Issue #779: the queue row is the whole request, so a key that could not name a Gmail
        // triage thread is never queued and never loaded.
        assert!(plausible_mail_thread_key("gmail:18f3a"));
        for bad in ["", "gmail:", "18f3a", "slack:C1", "gmail:a b", "gmail:a\n", "gmail:\u{3042}"] {
            assert!(!plausible_mail_thread_key(bad), "{bad:?}");
        }
        assert!(!plausible_mail_thread_key(&format!("gmail:{}", "a".repeat(MAIL_THREAD_KEY_MAX_BYTES))));
        let action = SendAction::ArchiveMailThread { thread_key: "gmail:1".into(), unarchive: false };
        assert_eq!(action.kind(), "archive_mail_thread");
        assert_eq!(Action::Send(action).required_level(), Level::L3);
    }
}
