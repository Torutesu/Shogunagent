//! Subscription-delegated Agent lane (Issue #110) — run agent inference on the plan the user
//! **already pays for**, instead of demanding a second, metered API key.
//!
//! ## Why this exists
//!
//! `FR-AG-06` makes BYOK mandatory for the Agent lane, and the requirements themselves flag the
//! consequence as a revenue risk ("BYOKハードルによるPro転換率低下"). SHOGUN's target user already
//! pays for Claude Pro/Max or a ChatGPT plan; asking them for a SHOGUN subscription *plus* an API
//! key *plus* per-token billing is charging three times for one job.
//!
//! ## The policy this depends on (check before changing anything here)
//!
//! Anthropic's position moved three times in 2026 and could move again, so it is recorded rather
//! than assumed (last verified 2026-08-24 against the Help Center article "Use the Claude Agent
//! SDK with your Claude plan"):
//!
//! - **Feb–Apr 2026**: third-party use of subscription OAuth tokens was banned outright and blocked
//!   server-side. Tools that had minted their own tokens against the consumer endpoints stopped
//!   working.
//! - **2026-05-13 announcement**: from June 15, Agent SDK / `claude -p` / third-party app usage
//!   was to move onto a separate **monthly Agent SDK credit** (~$20 Pro / $100 Max 5x / $200
//!   Max 20x, billed at standard API rates).
//! - **2026-06-15, the day it was due**: that change was **paused before taking effect**, and it
//!   has not shipped since. Current behavior: Agent SDK, `claude -p`, and **third-party apps
//!   authenticating with the user's subscription** draw from the plan's normal usage limits (the
//!   rolling ~5-hour window plus weekly caps) — the same pool as the user's own interactive
//!   Claude Code. Anthropic says it will give notice before any future change takes effect.
//!
//! Two load-bearing facts survive every swing: delegation through `claude -p` is a sanctioned,
//! *named* surface for third-party apps, and what stays permanently out of bounds is the thing
//! that got banned — minting or lifting subscription OAuth tokens and calling the API directly.
//! Because the billing regime has already flipped repeatedly, nothing in this module or in
//! user-facing copy may hard-code a recovery horizon ("resets monthly", "back in five hours"):
//! a limit is described as the plan's own, recovering on the plan's schedule, with the API key
//! offered as the way to keep working meanwhile.
//!
//! OpenAI has no equivalent published statement about third-party use of a ChatGPT-plan sign-in;
//! `codex exec` here is the user running their own installed CLI. If that becomes a problem it is a
//! per-delegate decision, not a reason to abandon the approach.
//!
//! ## What this is NOT
//!
//! Reimplementing a vendor's authorization flow, or lifting the token its CLI stored, is off the
//! table — that is precisely what was banned, it gets user accounts terminated, and it is not a base
//! a notarized paid product can stand on. So this module has hard non-goals, asserted by tests below:
//!
//! 1. It never reads another application's credential store (`~/.claude/.credentials.json`,
//!    `~/.codex/auth.json`, another app's Keychain item, …).
//! 2. It never performs an OAuth flow against a vendor's consumer endpoints.
//! 3. It never persists a subscription credential. SHOGUN holds none to persist.
//!
//! ## What it does instead
//!
//! It **delegates**. The user has already installed and signed into a vendor's own agent CLI; this
//! module runs that CLI as a local subprocess in its documented non-interactive mode and reads the
//! answer off stdout. Authentication stays entirely inside the delegate. SHOGUN observes exactly
//! two things about it: whether the binary exists, and whether a run succeeded.
//!
//! ## Invariants this upholds
//!
//! - **Invariant 5 (key separation).** [`SubscriptionAgentClient`] implements [`AgentClient`] and
//!   *deliberately does not implement* [`BatchClient`](super::BatchClient). Indexing / Dream Cycle /
//!   Morning Brief stay on the Select KK Batch API. Beyond the invariant this is a product
//!   requirement under **every** billing regime the vendor has tried: today a delegated run draws
//!   from the same usage window as the user's own interactive CLI, so batch volume would lock them
//!   out of their day job within minutes; under the announced-then-paused credit model it would
//!   drain a finite monthly credit in days. Either way, bulk work that Select KK already covers at
//!   batch rates would kill the Agent lane — the thing the user actually notices.
//! - **Invariant 3 (traceability).** A delegated run is still egress. Exactly one [`TraceRecord`]
//!   is written per completion on [`Route::LocalAgent`], carrying only a digest and byte length.
//! - **Invariant 7 (secrets).** The prompt travels on **stdin**, never argv: argv is world-readable
//!   via `ps` to every process on the machine, and captured text must not be. The child's
//!   environment is also scrubbed of SHOGUN's own API keys, so a delegate can never silently bill
//!   the user's metered API account when they asked to spend their subscription.

use std::collections::BTreeMap;
use std::time::Duration;

use super::prompts::{Effort, PromptTag};
use super::traceability::{Route, TraceRecord, TraceabilitySink};
use super::{redact_secrets, AgentClient, LlmError};

/// How long a `--version` probe may take. Generous for a cold binary on a spinning disk, short
/// enough that detecting three delegates never stalls onboarding.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one delegated completion may take before it is killed. Agent CLIs think for a while;
/// an unbounded wait would hang the caller's thread forever if a delegate blocked on an interactive
/// prompt we failed to suppress.
pub const COMPLETION_TIMEOUT: Duration = Duration::from_secs(120);

/// Environment variables scrubbed from every delegate process.
///
/// If SHOGUN's own BYOK key (or a stray shell export) reaches the child, the delegate authenticates
/// with *that* instead of the user's subscription — the user asked to spend a plan they already pay
/// for and would quietly be charged per token instead. Removing these makes the subscription the
/// only credential available to the child.
pub const SCRUBBED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    // `codex exec` authenticates with this instead of the ChatGPT sign-in when it is set
    // (documented on https://developers.openai.com/codex/noninteractive).
    "CODEX_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
];

/// Per-call knobs for a delegated run (issue #388). The default — both `None` — adds **no** argv:
/// the delegate runs on whatever model and effort the user's own CLI defaults to, which is the only
/// choice SHOGUN can always honour. A model name is never guessed.
///
/// Only verified headless flags are emitted (see [`Delegate::option_args`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegateOptions {
    /// Model id to request instead of the CLI's configured default.
    pub model: Option<String>,
    /// Reasoning effort to request instead of the CLI's configured default.
    pub reasoning_effort: Option<Effort>,
    /// Claude Code's verified `--effort` flag, used only for meeting chat.
    pub claude_code_effort: Option<Effort>,
    /// A bridged chat run carries SHOGUN's fixed chat contract in the CLI's own system role
    /// (owner decision 2026-10-14) instead of only inside the stdin user message:
    /// [`crate::llm::prompt_cache::delegated_system_text`], appended to the CLI's default
    /// prompt — never a replacement (`--system-prompt` stays forbidden). A flag rather than a
    /// string on purpose: the text is constant SHOGUN text, so nothing a user typed or SHOGUN
    /// captured can reach argv (Codex takes it as a `-c` value). See [`bridged_system_args`].
    pub shogun_system: bool,
}

impl DelegateOptions {
    /// The registry's static policy for `tag` ([`PromptTag::effort`]); never a model.
    pub fn for_tag(tag: PromptTag) -> Self {
        Self {
            model: None,
            reasoning_effort: tag.effort,
            claude_code_effort: (tag == crate::llm::prompts::MEETING_CHAT).then_some(Effort::Medium),
            shogun_system: false,
        }
    }

    /// The model id, if it is safe to put in argv. It becomes the value after `--model`, so a
    /// value starting with `-` could be read as a flag of its own, and anything outside the id
    /// alphabet is not a model id at all. Such a value is dropped (the CLI default wins) rather
    /// than passed through.
    fn safe_model(&self) -> Option<&str> {
        let m = self.model.as_deref()?.trim();
        let ok = !m.is_empty()
            && m.len() <= 128
            && !m.starts_with('-')
            && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/'));
        ok.then_some(m)
    }
}

/// File name of the Gemini CLI containment policy inside the scratch cwd. Relative on purpose: the
/// CLI resolves it against its cwd, which is the scratch directory the runner creates and writes
/// the file into (see [`Delegate::containment_files`]).
pub const GEMINI_CONTAINMENT_POLICY_FILE: &str = "shogun-containment-policy.toml";

/// The Gemini CLI policy that denies every tool call — built-in, MCP and subagent alike — so the
/// delegate can only answer in text. Schema per
/// <https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/policy-engine.md>
/// ("TOML rule schema": `toolName`, `decision` = allow | deny | ask_user, `priority` 0–999); the
/// `*` wildcard matches every tool name (`policy-engine.ts`, "Match all tools").
pub const GEMINI_CONTAINMENT_POLICY: &str = "\
# Written by SHOGUN for one delegated run and removed with it. Denies every tool call.
[[rule]]
toolName = \"*\"
decision = \"deny\"
priority = 999
";

/// A vendor agent CLI SHOGUN can delegate the Agent lane to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Delegate {
    /// Anthropic's Claude Code CLI, signed in with a Claude Pro/Max subscription.
    ClaudeCode,
    /// OpenAI's Codex CLI, signed in with a ChatGPT plan.
    Codex,
    /// Google's Gemini CLI, signed in with a Google account.
    GeminiCli,
}

impl Delegate {
    /// Every delegate, in the order the onboarding UI offers them.
    pub const ALL: [Delegate; 3] = [Delegate::ClaudeCode, Delegate::Codex, Delegate::GeminiCli];

    /// The stable id used in settings JSON and across the Tauri boundary.
    pub fn id(self) -> &'static str {
        match self {
            Delegate::ClaudeCode => "claude-code",
            Delegate::Codex => "codex",
            Delegate::GeminiCli => "gemini-cli",
        }
    }

    /// Parse a settings id back. `None` for anything unknown, so a hand-edited settings file can
    /// never select a delegate that does not exist.
    pub fn parse(id: &str) -> Option<Delegate> {
        Delegate::ALL.into_iter().find(|d| d.id() == id)
    }

    /// Product name, for UI.
    pub fn label(self) -> &'static str {
        match self {
            Delegate::ClaudeCode => "Claude Code",
            Delegate::Codex => "Codex",
            Delegate::GeminiCli => "Gemini CLI",
        }
    }

    /// Whose allowance a run is spent against. The UI must say this before the user opts in —
    /// "this runs on your own plan" is the entire proposition, and an exhausted-allowance message
    /// later is only intelligible if the user was told which plan it refers to.
    pub fn plan_label(self) -> &'static str {
        match self {
            Delegate::ClaudeCode => "Claude Pro / Max",
            Delegate::Codex => "ChatGPT Plus / Pro",
            Delegate::GeminiCli => "Google account",
        }
    }

    /// The executable looked up on `PATH`.
    pub fn binary(self) -> &'static str {
        match self {
            Delegate::ClaudeCode => "claude",
            Delegate::Codex => "codex",
            Delegate::GeminiCli => "gemini",
        }
    }

    /// The upstream host the delegate's own traffic reaches. Recorded as the traceability
    /// destination: what the user needs from that screen is *who saw this*, and the answer is the
    /// vendor, not the local binary. The local hop is carried by [`Route::LocalAgent`].
    pub fn upstream_host(self) -> &'static str {
        match self {
            Delegate::ClaudeCode => "api.anthropic.com",
            Delegate::Codex => "chatgpt.com",
            Delegate::GeminiCli => "generativelanguage.googleapis.com",
        }
    }

    /// Traceability `destination`: the upstream host plus the local hop that carried it, so the
    /// viewer can distinguish "SHOGUN sent this with your key" from "your Claude Code sent this on
    /// your plan" without consulting another column.
    pub fn destination(self) -> String {
        format!("{} (via local {} CLI)", self.upstream_host(), self.binary())
    }

    /// Arguments that ask for the version. The cheapest "is it installed" probe there is: no
    /// network, no quota, no auth.
    pub fn version_args(self) -> Vec<String> {
        vec!["--version".to_string()]
    }

    /// The oldest release of this CLI that accepts every flag in [`Self::containment_args`], as
    /// `major.minor.patch`, or `None` when no minimum is pinned.
    ///
    /// An older CLI rejects those flags (strict argv parsing) and the run fails closed — this
    /// minimum only turns that into an honest "update it" before the run instead of the CLI's own
    /// argument error after it. Compared by [`version_below`], which never calls an unreadable
    /// version too old.
    ///
    /// - **Codex 0.122.0**: `--ignore-user-config` / `--ignore-rules` (openai/codex#18646).
    /// - **Gemini CLI 0.35.0**: the policy flags, and the empty `--allowed-mcp-server-names`
    ///   meaning "no server" (see [`Self::containment_args`]).
    /// - **Claude Code**: not pinned — the release that added the last of its containment flags is
    ///   not recorded, and guessing would refuse a CLI that works. A too-old one still fails
    ///   closed, and [`classify`] turns its argument error into the same `needs_update`.
    pub fn min_version(self) -> Option<&'static str> {
        match self {
            Delegate::ClaudeCode => None,
            Delegate::Codex => Some("0.122.0"),
            Delegate::GeminiCli => Some("0.35.0"),
        }
    }

    /// Arguments for one non-interactive completion, with the prompt arriving on **stdin**.
    ///
    /// These invocation contracts are the single place a vendor CLI change has to be absorbed. They
    /// use each CLI's documented headless mode; the prompt is deliberately absent from argv (see
    /// the module docs on `ps` visibility). `opts` adds only [`Self::option_args`].
    pub fn completion_args(self, opts: &DelegateOptions) -> Vec<String> {
        let mut args = match self {
            // `claude -p` is print/headless mode; with stdin piped it reads the prompt from there.
            Delegate::ClaudeCode => {
                vec!["-p".to_string(), "--output-format".to_string(), "text".to_string()]
            }
            // `codex exec -` is the non-interactive subcommand reading the prompt from stdin.
            Delegate::Codex => vec!["exec".to_string(), "-".to_string()],
            // The Gemini CLI runs non-interactively when its stdin is a pipe.
            Delegate::GeminiCli => Vec::new(),
        };
        args.extend(self.containment_args());
        args.extend(self.option_args(opts));
        args
    }

    /// The argv that [`DelegateOptions`] adds — empty for the default options.
    ///
    /// Codex (`codex exec`, per <https://developers.openai.com/codex/cli/reference>):
    /// - `--model <string>` — "Override the configured model for this run."
    /// - `-c key=value` — "Inline configuration override for the non-interactive run
    ///   (repeatable)", value parsed as TOML; the reasoning key is `model_reasoning_effort`
    ///   (<https://developers.openai.com/codex/config-reference>). The value is TOML-quoted as
    ///   <https://developers.openai.com/codex/config-advanced> recommends for strings — no shell sits
    ///   in between, so the quotes reach the CLI verbatim.
    ///
    /// Claude Code's `--effort <level>` is documented at
    /// <https://code.claude.com/docs/en/cli-reference> and used only for meeting chat.
    /// TODO(#388): Gemini CLI flags and Claude Code's model flag remain unverified here.
    pub fn option_args(self, opts: &DelegateOptions) -> Vec<String> {
        match self {
            Delegate::Codex => {
                let mut args = Vec::new();
                if let Some(model) = opts.safe_model() {
                    args.extend(["--model".to_string(), model.to_string()]);
                }
                if let Some(effort) = opts.reasoning_effort {
                    args.extend([
                        "-c".to_string(),
                        format!("model_reasoning_effort=\"{}\"", effort.as_str()),
                    ]);
                }
                args
            }
            Delegate::ClaudeCode => opts.claude_code_effort.map_or_else(Vec::new, |effort| {
                vec!["--effort".to_string(), effort.as_str().to_string()]
            }),
            Delegate::GeminiCli => Vec::new(),
        }
    }

    /// Flags that keep the delegate a **text completion**, not an agent.
    ///
    /// The prompt we pipe in is captured screen text, mail bodies and transcripts — untrusted by
    /// definition, and only fenced (`fence_untrusted`), because a CLI has no system/user role
    /// split. Without these flags the vendor CLI would answer with *its* tool surface enabled: the
    /// user's own MCP servers, connectors, extensions, persisted "always allow" approvals and
    /// built-in shell/file tools. An instruction planted in a mail body could then call a
    /// user-configured MCP server (GitHub, Slack, a filesystem server…) entirely outside SHOGUN's
    /// L1/L2/L3 engine, the L3 queue and the traceability ledger (#782). Every delegate therefore
    /// runs with no MCP servers, no connectors/extensions, no built-in tools and no persisted
    /// session, in the empty scratch cwd the runner provides (see `ScratchCwd`).
    ///
    /// Only flags passed on the command line — SHOGUN never edits the user's own CLI config. Each
    /// flag is pinned to the vendor doc and source it comes from, because these CLIs change fast.
    /// A CLI too old to know one of these flags rejects the invocation (both CLIs parse argv
    /// strictly) and the run fails — it never runs uncontained.
    ///
    /// **Claude Code** (<https://code.claude.com/docs/en/cli-reference>): `--tools ""` (no built-in
    /// tools), `--strict-mcp-config` with an empty `--mcp-config` (the user's MCP servers do not
    /// load), `--no-session-persistence`.
    ///
    /// **Codex** — checked 2026-10-03 against <https://learn.chatgpt.com/docs/cli/reference>
    /// (`codex exec` options) and <https://learn.chatgpt.com/docs/config-file/config-reference>
    /// (where the `developers.openai.com/codex/…` pages now redirect), and the
    /// source at `openai/codex` main `b741e480` (`codex-rs/exec/src/cli.rs`,
    /// `codex-rs/config/src/loader/mod.rs`, `codex-rs/features/src/lib.rs`,
    /// `codex-rs/tools/src/tool_config.rs`, `codex-rs/codex-mcp/src/mcp/mod.rs`). Needs Codex
    /// **0.122.0+** (`--ignore-user-config` / `--ignore-rules` landed in openai/codex#18646).
    /// - `--sandbox read-only` — "Sandbox policy for model-generated shell commands"; nothing can
    ///   write or reach the network even if a tool slipped through.
    /// - `--skip-git-repo-check` — "Allow running outside a Git repository" (the scratch cwd).
    /// - `--ignore-user-config` — "Do not load `$CODEX_HOME/config.toml`. Authentication still uses
    ///   `CODEX_HOME`." That file is where the user's `[mcp_servers]`, plugin enablement, hooks
    ///   and approval/sandbox defaults live. It has to be this flag: a `-c mcp_servers={}`
    ///   override is a config *layer* that is deep-merged over config.toml
    ///   (`merge_toml_values`), so an empty table removes nothing.
    /// - `--ignore-rules` — "Do not load user or project execpolicy `.rules` files for this run"
    ///   (the user's command auto-approvals).
    /// - `--ephemeral` — "Run without persisting session rollout files to disk" (the captured
    ///   prompt does not land in `~/.codex/sessions`).
    /// - `-c features.apps=false` — "Enable app (connector) integrations (stable; on by default)".
    ///   Connectors are attached to the **ChatGPT account**, not config.toml, and are served as
    ///   the `codex_apps` MCP server whenever this is on and the sign-in is ChatGPT — so
    ///   `--ignore-user-config` alone leaves them reachable.
    /// - `-c features.plugins=false` — plugins contribute MCP servers of their own.
    /// - `-c features.shell_tool=false` — "Enable the default `shell` tool for running commands
    ///   (stable; on by default)". Off means no shell at all (`ConfigShellToolType::Disabled`);
    ///   the read-only sandbox still lets a shell *read* any file, so the tool itself goes.
    /// - `-c web_search="disabled"` — `web_search` is `disabled | cached | indexed | live`.
    ///
    /// `-c` values are TOML (strings quoted, as `option_args` does); an unknown feature key is
    /// only a warning, never an error, on every version checked.
    ///
    /// **Gemini CLI** — checked 2026-10-03 against
    /// <https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/cli-reference.md>,
    /// `docs/reference/configuration.md` ("Command-line arguments"),
    /// `docs/reference/policy-engine.md`, and the source at `google-gemini/gemini-cli` main
    /// `fb972b2f` (`packages/cli/src/config/config.ts`,
    /// `packages/core/src/tools/mcp-client-manager.ts`, `packages/core/src/policy/config.ts`).
    /// Needs Gemini CLI **0.35.0+**.
    /// - `--allowed-mcp-server-names ""` — "Allowed MCP server names". An empty value is kept as
    ///   `[""]` and allows **no** server (upstream test "should allow no MCP servers if the flag
    ///   is provided but empty"); servers from settings and from extensions both pass this check.
    ///   Before 0.35.0 the empty value collapsed to "no filter" — which is why the policy flags
    ///   below, unknown to those versions, must travel with it: strict argv parsing then refuses
    ///   the whole run instead of running it with every server enabled.
    /// - `--extensions none` — "Use the special term `gemini -e none` to disable all extensions"
    ///   (their MCP servers, context files and policy rules).
    /// - `--admin-policy <file>` and `--policy <file>` pointing at
    ///   [`GEMINI_CONTAINMENT_POLICY`], which the runner writes into the scratch cwd: one
    ///   `toolName = "*"`, `decision = "deny"` rule ("You can use wildcards like `*`"). Headless
    ///   mode already turns `ask_user` into deny, but read-only tools are allowed by default and
    ///   a user's own "always allow" approvals and `tools.allowed` settings would still let a
    ///   shell command or web fetch through. `--admin-policy` puts the deny-all in the Admin tier,
    ///   above every user rule; `--policy` "replace[s] the default user policies directory", so
    ///   the user's saved approvals are not loaded either, and the deny-all also outranks the
    ///   settings-derived user-tier allows (≤ 4.9) on a machine whose administrator already
    ///   installed system policies (then `--admin-policy` is ignored by design).
    ///
    /// Tested by `delegates_run_without_tools_or_mcp_servers`, which covers every entry of
    /// [`Delegate::ALL`]; every future flag change here must keep that test green rather than
    /// weaken it.
    ///
    /// A **bridged** chat run (issue #484, [`bridged_spec`]) keeps all of this and admits exactly
    /// one MCP server, SHOGUN's chat bridge — Claude Code and Codex only ([`BridgedDelegate`]);
    /// `a_bridged_claude_code_run_adds_only_shoguns_chat_bridge` and
    /// `a_bridged_codex_run_adds_only_shoguns_chat_bridge` hold that line.
    pub fn containment_args(self) -> Vec<String> {
        match self {
            Delegate::ClaudeCode => [
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--no-session-persistence",
            ]
            .map(str::to_string)
            .to_vec(),
            Delegate::Codex => [
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--ignore-user-config",
                "--ignore-rules",
                "--ephemeral",
                "-c",
                "features.apps=false",
                "-c",
                "features.plugins=false",
                "-c",
                "features.shell_tool=false",
                "-c",
                r#"web_search="disabled""#,
            ]
            .map(str::to_string)
            .to_vec(),
            Delegate::GeminiCli => [
                "--allowed-mcp-server-names",
                "",
                "--extensions",
                "none",
                "--admin-policy",
                GEMINI_CONTAINMENT_POLICY_FILE,
                "--policy",
                GEMINI_CONTAINMENT_POLICY_FILE,
            ]
            .map(str::to_string)
            .to_vec(),
        }
    }

    /// Files the runner must write into the scratch cwd before this delegate starts, as
    /// `(file name, contents)`. Only the Gemini CLI needs one: its deny-all policy can only be
    /// given as a file (see [`Self::containment_args`]). The file is SHOGUN's own, lives only as
    /// long as the run, and is never one of the user's CLI config files.
    pub fn containment_files(self) -> Vec<(String, String)> {
        match self {
            Delegate::ClaudeCode | Delegate::Codex => Vec::new(),
            Delegate::GeminiCli => vec![(
                GEMINI_CONTAINMENT_POLICY_FILE.to_string(),
                GEMINI_CONTAINMENT_POLICY.to_string(),
            )],
        }
    }

    /// Arguments for one **streaming** non-interactive completion (prompt on stdin), or `None`
    /// when the delegate has no documented incremental output surface — it then falls back to the
    /// buffered [`Self::completion_args`] contract. `opts` adds only [`Self::option_args`].
    pub fn streaming_args(self, opts: &DelegateOptions) -> Option<Vec<String>> {
        let mut args = match self {
            // The Agent SDK's own programmatic surface: line-delimited JSON events on stdout.
            // `--include-partial-messages` adds token-level `content_block_delta` events, and the
            // CLI requires `--verbose` for stream-json in print mode.
            Delegate::ClaudeCode => {
                ["-p", "--output-format", "stream-json", "--include-partial-messages", "--verbose"]
                    .map(str::to_string)
                    .to_vec()
            }
            // `codex exec --json` prints newline-delimited JSON events instead of formatted text
            // (see `StreamFormat::CodexJsonl` for the event shapes and sources). Prompt still on
            // stdin via `-`.
            Delegate::Codex => ["exec", "--json", "-"].map(str::to_string).to_vec(),
            // The Gemini CLI writes plain text as it generates; reading the pipe incrementally IS
            // the streaming.
            Delegate::GeminiCli => Vec::new(),
        };
        args.extend(self.containment_args());
        args.extend(self.option_args(opts));
        Some(args)
    }

    /// How to pull text deltas out of this delegate's streaming stdout. `None` would mirror
    /// [`Self::streaming_args`] returning `None`; every delegate has a streaming surface today.
    pub fn stream_format(self) -> Option<StreamFormat> {
        match self {
            Delegate::ClaudeCode => Some(StreamFormat::ClaudeStreamJson),
            Delegate::Codex => Some(StreamFormat::CodexJsonl),
            Delegate::GeminiCli => Some(StreamFormat::RawText),
        }
    }

    /// Whether a chat answer from this delegate is timed for NFR-SLO-03 (both the first visible
    /// character and the first character of the final answer). Codex is excluded by the
    /// 2026-09-25 definition: `codex exec --json` returns the answer in one piece, so its "first
    /// token" is its completion and says nothing about streaming.
    pub fn in_slo03(self) -> bool {
        match self {
            Delegate::ClaudeCode | Delegate::GeminiCli => true,
            Delegate::Codex => false,
        }
    }
}

/// The wire shape of a delegate's incremental stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// `claude -p --output-format stream-json`: one JSON event per line. Token deltas arrive as
    /// `stream_event` / `content_block_delta` / `text_delta`; the final `result` event carries the
    /// whole answer (the fallback when an older CLI emits no partial events).
    ClaudeStreamJson,
    /// `codex exec --json`: one JSON event per line.
    ///
    /// Sources (checked 2026-09-24):
    /// - <https://developers.openai.com/codex/noninteractive> — `--json` "converts stdout to a JSON
    ///   Lines stream"; event types `thread.started`, `turn.started`, `turn.completed`,
    ///   `turn.failed`, `item.*`, `error`; the answer is an `agent_message` item, e.g.
    ///   `{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"…"}}`.
    /// - <https://github.com/openai/codex/blob/main/codex-rs/exec/src/exec_events.rs> and
    ///   `…/event_processor_with_jsonl_output.rs` — item types `agent_message`, `reasoning`,
    ///   `command_execution`, `file_change`, `mcp_tool_call`, `collab_tool_call`, `web_search`,
    ///   `todo_list`, `error`; `agent_message` and `reasoning` are emitted **only** as
    ///   `item.completed` (never `item.started`/`item.updated`), and no event carries a partial
    ///   text delta. `turn.failed` carries `error.message`, `error` carries `message`.
    ///
    /// So Codex streams at **message** granularity, not token granularity: the first delta is the
    /// first completed `agent_message`. Reasoning and tool items are never user-visible text.
    CodexJsonl,
    /// Plain text, streamed as written.
    RawText,
}

/// Accumulating parser turning one stdout line into at most one text delta.
///
/// Lives here rather than in the runner because it is pure text-shape knowledge: the runner's job
/// ends at "a line arrived". Everything is Linux-testable through [`MockRunner`].
pub struct DeltaParser {
    format: StreamFormat,
    saw_delta: bool,
    whole: String,
}

impl DeltaParser {
    pub fn new(format: StreamFormat) -> Self {
        Self { format, saw_delta: false, whole: String::new() }
    }

    /// The text delta carried by `line`, if any. Also accumulates it into [`Self::into_whole`].
    pub fn push(&mut self, line: &str) -> Option<String> {
        match self.format {
            StreamFormat::RawText => {
                let delta = format!("{line}\n");
                self.whole.push_str(&delta);
                Some(delta)
            }
            StreamFormat::ClaudeStreamJson => {
                let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
                match v.get("type").and_then(|t| t.as_str())? {
                    // Token-level deltas (with --include-partial-messages). Only text: thinking
                    // deltas are the model's scratchpad, not the answer.
                    "stream_event" => {
                        if v.pointer("/event/delta/type").and_then(|t| t.as_str()) != Some("text_delta") {
                            return None;
                        }
                        let text = v.pointer("/event/delta/text")?.as_str()?.to_string();
                        self.saw_delta = true;
                        self.whole.push_str(&text);
                        Some(text)
                    }
                    // The final event carries the whole answer. It is used only when no partial
                    // events arrived (an older CLI) — otherwise it would duplicate every token.
                    "result" => {
                        if self.saw_delta {
                            return None;
                        }
                        let text = v.get("result")?.as_str()?.to_string();
                        self.whole.push_str(&text);
                        Some(text)
                    }
                    // `system` init, `assistant` message echoes (already covered by the deltas)…
                    _ => None,
                }
            }
            StreamFormat::CodexJsonl => {
                let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
                // Only a finished agent message is answer text. `reasoning` is the model's
                // scratchpad and command / tool / todo items are its actions — none of it may be
                // painted into the user's document.
                if v.get("type").and_then(|t| t.as_str()) != Some("item.completed")
                    || v.pointer("/item/type").and_then(|t| t.as_str()) != Some("agent_message")
                {
                    return None;
                }
                let text = v.pointer("/item/text")?.as_str()?;
                if text.is_empty() {
                    return None;
                }
                // A turn may hold several agent messages; keep them apart as paragraphs, the way
                // they read in the CLI, instead of gluing one's last word to the next one's first.
                let delta = if self.saw_delta { format!("\n\n{text}") } else { text.to_string() };
                self.saw_delta = true;
                self.whole.push_str(&delta);
                Some(delta)
            }
        }
    }

    /// Everything pushed so far, joined.
    pub fn into_whole(self) -> String {
        self.whole
    }
}

/// One subprocess invocation. A value rather than a call so tests can assert on exactly what
/// *would* have been run — which is how the "never touches a credential file" guarantee below is
/// checked rather than merely asserted in prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Fed to the child's stdin and closed. `None` closes stdin immediately.
    pub stdin: Option<String>,
    /// Environment names to remove from the child (see [`SCRUBBED_ENV`]).
    pub env_remove: Vec<String>,
    pub timeout: Duration,
    /// Files written into the child's fresh scratch cwd before it starts, as `(plain file name,
    /// contents)` — SHOGUN's own containment files (see [`Delegate::containment_files`]), never
    /// user data. If one cannot be written the child is **not** started: a delegate missing its
    /// containment must not run at all.
    pub cwd_files: Vec<(String, String)>,
}

/// A finished subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code, or `None` when the child was signalled (including our own timeout kill).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why a subprocess could not be run to completion. Distinct from a non-zero exit: "the binary is
/// not there" and "the binary ran and refused" need different UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The program is not on `PATH`.
    NotFound,
    /// The child outlived its timeout and was killed.
    Timeout,
    /// The caller withdrew the request and the child was killed.
    Cancelled,
    /// Anything else (spawn failure, pipe error), already redacted.
    Io(String),
}

/// The subprocess seam. The real implementation is [`ProcessRunner`]; tests use [`MockRunner`], so
/// every classification rule in this module is exercised on Linux CI with no vendor CLI installed.
pub trait CommandRunner: Send + Sync {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunError>;

    /// Run `spec`, handing each stdout **line** to `on_line` as it arrives (SLO-03). Returning
    /// `false` from `on_line` stops the run early — the real runner kills the child, so tokens the
    /// caller stopped listening for are tokens nobody pays for. The returned output's stdout is
    /// the full accumulated stream either way.
    ///
    /// The default implementation is honest but not incremental: it blocks on [`Self::run`] and
    /// replays the finished stdout line by line. That gives every implementor — [`MockRunner`]
    /// included — a working streaming contract for free; [`ProcessRunner`] overrides it with a
    /// real incremental read.
    fn run_streaming(
        &self,
        spec: &CommandSpec,
        on_line: &mut dyn FnMut(&str) -> bool,
    ) -> Result<CommandOutput, RunError> {
        let out = self.run(spec)?;
        if out.ok() {
            for line in out.stdout.lines() {
                if !on_line(line) {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Run with caller-owned cancellation. The real process runner kills its child; test runners
    /// retain existing behavior and still reject a request cancelled before it starts.
    fn run_cancellable(
        &self,
        spec: &CommandSpec,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<CommandOutput, RunError> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RunError::Cancelled);
        }
        self.run(spec)
    }

    /// [`Self::run_streaming`] with caller-owned cancellation. The flag is the *primary* stop
    /// signal: the real runner polls it while the child is silent and kills the child the moment
    /// it flips, so Stop takes effect during the delegate's thinking phase instead of waiting for
    /// the next stdout line (which is when the `on_line` → `false` path first gets a chance to
    /// run). The default checks the flag once up front, then falls back to the plain contract.
    fn run_streaming_cancellable(
        &self,
        spec: &CommandSpec,
        on_line: &mut dyn FnMut(&str) -> bool,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<CommandOutput, RunError> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RunError::Cancelled);
        }
        self.run_streaming(spec, on_line)
    }
}

/// What SHOGUN knows about a delegate. Deliberately shallow: presence and, when the user has asked
/// for a live check, whether a real completion went through. Nothing here is derived from reading
/// the delegate's stored credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateState {
    /// The binary is not on `PATH`.
    NotInstalled,
    /// Installed, but SHOGUN has not run a completion, so sign-in status is unknown. This is the
    /// honest resting state: claiming "ready" from the binary's existence alone would send the user
    /// out of onboarding into a first action that fails.
    Installed { version: String },
    /// A completion succeeded — the delegate is installed *and* signed in.
    Ready { version: String },
    /// Installed but the delegate says it is not signed in.
    NeedsLogin { version: String },
    /// Installed and signed in, but the plan's quota is currently exhausted.
    RateLimited { version: String },
    /// Installed, but too old to run with SHOGUN's containment flags: its version is below
    /// [`Delegate::min_version`], or it rejected one of those flags as unknown. Nothing was run
    /// (or the run was refused before it started). `minimum` is SHOGUN's own constant — `None`
    /// when no minimum is pinned for this CLI, so the copy asks for the latest release.
    NeedsUpdate { version: String, minimum: Option<&'static str> },
}

impl DelegateState {
    /// Whether the Agent lane can be driven through this delegate right now.
    pub fn is_usable(&self) -> bool {
        matches!(self, DelegateState::Ready { .. })
    }

    /// Whether the binary is present at all (any state but [`DelegateState::NotInstalled`]).
    pub fn is_installed(&self) -> bool {
        !matches!(self, DelegateState::NotInstalled)
    }

    /// A stable tag for the UI and for logs. Never carries the version or any provider text.
    pub fn tag(&self) -> &'static str {
        match self {
            DelegateState::NotInstalled => "not_installed",
            DelegateState::Installed { .. } => "installed",
            DelegateState::Ready { .. } => "ready",
            DelegateState::NeedsLogin { .. } => "needs_login",
            DelegateState::RateLimited { .. } => "rate_limited",
            DelegateState::NeedsUpdate { .. } => "needs_update",
        }
    }

    /// The CLI's own one-line version output (bounded and redacted by detection), or empty.
    pub fn version(&self) -> String {
        match self {
            DelegateState::NotInstalled => String::new(),
            DelegateState::Installed { version }
            | DelegateState::Ready { version }
            | DelegateState::NeedsLogin { version }
            | DelegateState::RateLimited { version }
            | DelegateState::NeedsUpdate { version, .. } => version.clone(),
        }
    }

    /// The version to update to, for [`DelegateState::NeedsUpdate`] with a pinned minimum.
    pub fn minimum(&self) -> Option<&'static str> {
        match self {
            DelegateState::NeedsUpdate { minimum, .. } => *minimum,
            _ => None,
        }
    }
}

/// Build the version-probe invocation for `delegate`.
pub fn detect_spec(delegate: Delegate) -> CommandSpec {
    CommandSpec {
        program: delegate.binary().to_string(),
        args: delegate.version_args(),
        stdin: None,
        env_remove: SCRUBBED_ENV.iter().map(|s| s.to_string()).collect(),
        timeout: DETECT_TIMEOUT,
        cwd_files: Vec::new(),
    }
}

/// Is `delegate` installed, and at what version? Runs `--version` only: no network, no quota, no
/// credential read. A version below [`Delegate::min_version`] is
/// [`DelegateState::NeedsUpdate`]; one that cannot be read stays `Installed`.
pub fn detect(runner: &dyn CommandRunner, delegate: Delegate) -> DelegateState {
    match runner.run(&detect_spec(delegate)) {
        Ok(out) if out.ok() => {
            let version = parse_version(&out.stdout, &out.stderr);
            match delegate.min_version() {
                Some(minimum) if version_below(&version, minimum) => {
                    DelegateState::NeedsUpdate { version, minimum: Some(minimum) }
                }
                _ => DelegateState::Installed { version },
            }
        }
        // It exists but `--version` failed. Still installed — a broken version flag is not a reason
        // to hide a delegate the user can see in their own terminal.
        Ok(_) => DelegateState::Installed { version: String::new() },
        Err(RunError::NotFound) => DelegateState::NotInstalled,
        Err(_) => DelegateState::NotInstalled,
    }
}

/// Detect every delegate, in [`Delegate::ALL`] order.
pub fn detect_all(runner: &dyn CommandRunner) -> BTreeMap<Delegate, DelegateState> {
    Delegate::ALL.into_iter().map(|d| (d, detect(runner, d))).collect()
}

/// The version string, trimmed to one short safe line.
///
/// Version output is vendor-controlled text that ends up in a log line, so it is bounded and
/// scrubbed of anything credential-shaped rather than trusted.
fn parse_version(stdout: &str, stderr: &str) -> String {
    let raw = if stdout.trim().is_empty() { stderr } else { stdout };
    let line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let line = redact_secrets(line);
    match line.char_indices().nth(64) {
        Some((i, _)) => line[..i].to_string(),
        None => line,
    }
}

/// The `major.minor.patch` number in a CLI's `--version` line, or `None` when there is none.
///
/// Reads what these CLIs actually print — `codex-cli 0.122.0`, `0.35.0`, `2.0.14 (Claude Code)`,
/// `v0.35.0` — taking the first whitespace/punctuation-separated word that starts with a digit
/// and has at least `major.minor`. A missing patch reads as `0`. A pre-release or nightly suffix
/// (`0.36.0-preview.1`, `0.36.0-nightly.20251001.abc`) and build metadata (`+build.5`) are
/// dropped, not compared — see [`version_below`] for why.
pub fn parse_cli_version(raw: &str) -> Option<(u64, u64, u64)> {
    raw.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '[' | ']'))
        .find_map(|word| {
            let word = word.strip_prefix(['v', 'V']).unwrap_or(word);
            if !word.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            // The numeric core ends at the first character that is neither a digit nor a dot.
            let core_end = word.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(word.len());
            let rest = &word[core_end..];
            // `1.2.3-preview.1` and `1.2.3+build` are versions; `1.2.3abc` or `2026/10/01` is not.
            if !(rest.is_empty() || rest.starts_with(['-', '+'])) {
                return None;
            }
            let parts: Vec<&str> = word[..core_end].trim_end_matches('.').split('.').collect();
            if !(2..=3).contains(&parts.len()) || parts.iter().any(|p| p.is_empty()) {
                return None;
            }
            let num = |p: &str| p.parse::<u64>().ok();
            Some((num(parts[0])?, num(parts[1])?, match parts.get(2) {
                Some(p) => num(p)?,
                None => 0,
            }))
        })
}

/// Whether the CLI version line `installed` is **known** to be older than `minimum`.
///
/// Unknown is not too old: when either side cannot be read this is `false`, and the run goes
/// ahead — strict argv parsing still refuses the flags of a CLI that really is too old, so the
/// run fails closed either way. Only `major.minor.patch` is compared; a pre-release of the
/// minimum itself (`0.35.0-preview.1` against `0.35.0`) passes. A gate that wrongly says "too
/// old" leaves the user with a working CLI they cannot use and nothing to do about it, while one
/// that wrongly passes only shows the CLI's own argument error.
pub fn version_below(installed: &str, minimum: &str) -> bool {
    match (parse_cli_version(installed), parse_cli_version(minimum)) {
        (Some(have), Some(need)) => have < need,
        _ => false,
    }
}

/// Run a real, minimal completion to learn whether the delegate is signed in.
///
/// This spends a token or two of the user's quota, so it is a **user-initiated** check ("Test
/// connection"), never something fired on every launch. It is also the only honest test available:
/// the alternative — inspecting the delegate's credential file — is the exact thing this module
/// refuses to do.
pub fn verify(runner: &dyn CommandRunner, delegate: Delegate) -> DelegateState {
    let state = detect(runner, delegate);
    // Not installed, or known to be too old: the completion would only fail on its flags, so it
    // is not run (and spends nothing).
    if matches!(state, DelegateState::NotInstalled | DelegateState::NeedsUpdate { .. }) {
        return state;
    }
    let version = state.version();
    match runner.run(&completion_spec(delegate, "Reply with the single word: ok")) {
        Ok(out) if out.ok() => DelegateState::Ready { version },
        Ok(out) => match classify(&out) {
            DelegateFailure::NeedsLogin => DelegateState::NeedsLogin { version },
            DelegateFailure::RateLimited => DelegateState::RateLimited { version },
            // The version was unreadable (or not pinned), but the CLI refused a flag as unknown.
            DelegateFailure::NeedsUpdate => {
                DelegateState::NeedsUpdate { version, minimum: delegate.min_version() }
            }
            // It ran, it was not an auth or quota refusal — the delegate itself is reachable, but we
            // have no proof of sign-in. Do not claim Ready.
            _ => DelegateState::Installed { version },
        },
        Err(RunError::NotFound) => DelegateState::NotInstalled,
        Err(_) => DelegateState::Installed { version },
    }
}

/// How long a delegate whose version passed [`Delegate::min_version`] goes without another
/// `--version` probe on the run path.
pub const VERSION_GATE_TTL: Duration = Duration::from_secs(10 * 60);

/// The run path's minimum-version check, remembered per delegate so a turn does not pay a
/// `--version` probe every time (a Node-based CLI takes a noticeable fraction of a second to
/// start, which would land on SLO-03's first token).
///
/// Only a **pass** is remembered, for [`VERSION_GATE_TTL`]. A too-old verdict is probed again on
/// the next run, so updating the CLI takes effect on the very next turn. A delegate with no pinned
/// minimum, or a version that cannot be read, always passes (see [`version_below`]).
pub struct VersionGate {
    passed: std::sync::Mutex<BTreeMap<Delegate, std::time::Instant>>,
}

impl VersionGate {
    pub const fn new() -> Self {
        Self { passed: std::sync::Mutex::new(BTreeMap::new()) }
    }

    /// `Err(NeedsUpdate)` when the installed `delegate` is known to be below its minimum.
    /// Runs `--version` only (no network, no quota) and only when no fresh pass is remembered.
    pub fn check(&self, runner: &dyn CommandRunner, delegate: Delegate) -> Result<(), DelegateFailure> {
        if delegate.min_version().is_none() {
            return Ok(());
        }
        let fresh = self
            .passed
            .lock()
            .ok()
            .and_then(|g| g.get(&delegate).copied())
            .is_some_and(|at| at.elapsed() < VERSION_GATE_TTL);
        if fresh {
            return Ok(());
        }
        let state = detect(runner, delegate);
        self.observe(delegate, &state);
        match state {
            DelegateState::NeedsUpdate { .. } => Err(DelegateFailure::NeedsUpdate),
            _ => Ok(()),
        }
    }

    /// Learn from a `--version` probe made elsewhere (the app's own presence check), so the
    /// first run after it pays no probe of its own. A too-old state forgets any earlier pass; a
    /// missing binary is not a pass (the run reports it as not installed).
    pub fn observe(&self, delegate: Delegate, state: &DelegateState) {
        let Ok(mut g) = self.passed.lock() else { return };
        match state {
            DelegateState::NeedsUpdate { .. } | DelegateState::NotInstalled => {
                g.remove(&delegate);
            }
            DelegateState::Installed { .. }
            | DelegateState::Ready { .. }
            | DelegateState::NeedsLogin { .. }
            | DelegateState::RateLimited { .. } => {
                g.insert(delegate, std::time::Instant::now());
            }
        }
    }
}

impl Default for VersionGate {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide [`VersionGate`] the app's Agent lane uses
/// ([`SubscriptionAgentClient::with_version_gate`]).
pub static VERSION_GATE: VersionGate = VersionGate::new();

/// Why a delegated run did not produce an answer. The distinctions exist because the user's next
/// step differs completely: install something, sign in somewhere, wait, or report a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateFailure {
    NotInstalled,
    NeedsLogin,
    /// The plan's allowance for delegated runs is used up. Never SHOGUN's fault, and must not be
    /// reported as SHOGUN failing. How fast it recovers is the vendor's billing policy, which has
    /// flipped more than once (see the module docs) — so the copy names the plan and its own
    /// schedule, and offers the API-key path, instead of promising when the limit comes back.
    RateLimited,
    /// The CLI is too old for SHOGUN's flags: its version is below [`Delegate::min_version`], or
    /// it rejected a flag as unknown. Carries no CLI text — the message is SHOGUN's own.
    NeedsUpdate,
    Timeout,
    /// Anything else. Already passed through [`redact_secrets`] and truncated.
    Failed(String),
}

/// Substrings that mean "the delegate is not signed in".
const LOGIN_MARKERS: &[&str] = &[
    "not logged in",
    "not authenticated",
    "please log in",
    "please login",
    "run /login",
    "login required",
    "authentication required",
    "unauthorized",
    "invalid api key",
    "no credentials",
    "credentials not found",
    "sign in to",
    "http 401",
    "status 401",
];

/// Substrings that mean "the plan's quota window is exhausted". Checked before the login markers:
/// limit messages often also invite the user to sign in somewhere to upgrade, and calling a quota
/// problem an auth problem sends them to re-authenticate an account that was never broken.
const RATE_LIMIT_MARKERS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "usage limit",
    "quota",
    "too many requests",
    "limit reached",
    "limit will reset",
    "http 429",
    "status 429",
];

/// Classify a finished-but-unsuccessful run from what the delegate printed.
///
/// An argument error on **stderr** comes first: it means the CLI never started the turn, and some
/// CLIs print their whole option help with it, whose descriptions could otherwise match a login
/// or quota marker. Stdout is not read for it — that is where a model's partial answer goes.
pub fn classify(out: &CommandOutput) -> DelegateFailure {
    if has_flag_error(&out.stderr) {
        return DelegateFailure::NeedsUpdate;
    }
    let hay = format!("{}\n{}", out.stderr, out.stdout).to_lowercase();
    if RATE_LIMIT_MARKERS.iter().any(|m| hay.contains(m)) {
        return DelegateFailure::RateLimited;
    }
    if LOGIN_MARKERS.iter().any(|m| hay.contains(m)) {
        return DelegateFailure::NeedsLogin;
    }
    DelegateFailure::Failed(short_reason(&out.stderr, &out.stdout))
}

/// The delegate's own explanation, redacted and bounded, for a one-line UI pill.
fn short_reason(stderr: &str, stdout: &str) -> String {
    let raw = if stderr.trim().is_empty() { stdout } else { stderr };
    let line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let line = redact_secrets(line);
    match line.char_indices().nth(180) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line,
    }
}

impl DelegateFailure {
    /// Turn a delegate failure into the lane's error type, keeping the distinctions the UI needs.
    pub fn into_llm_error(self, delegate: Delegate) -> LlmError {
        match self {
            DelegateFailure::NotInstalled => LlmError::NotConfigured,
            // Reuse Unauthorized so every existing 401 path — the sticky "credential rejected"
            // indicator, the settings prompt — lights up unchanged for a delegate that needs a login.
            DelegateFailure::NeedsLogin => LlmError::Unauthorized(
                401,
                format!("{} is not signed in — run `{}` once in a terminal to log in", delegate.label(), delegate.binary()),
            ),
            // Deliberately promises no recovery horizon (see the module docs): the message names
            // whose limit it is and offers the API key as the way to keep working meanwhile.
            DelegateFailure::RateLimited => LlmError::QuotaExhausted(format!(
                "{} has hit its usage limit on your {} — the limit recovers on that plan's own schedule. Add an API key in Settings to keep working in the meantime.",
                delegate.label(),
                delegate.plan_label()
            )),
            // SHOGUN's own words only: the CLI's argument error is not shown.
            DelegateFailure::NeedsUpdate => LlmError::NeedsUpdate(match delegate.min_version() {
                Some(minimum) => format!(
                    "{} needs an update to run here — update it to version {minimum} or later, then test the connection in Settings.",
                    delegate.label()
                ),
                None => format!(
                    "{} needs an update to run here — update it to its latest version, then test the connection in Settings.",
                    delegate.label()
                ),
            }),
            DelegateFailure::Timeout => {
                LlmError::Provider(format!("{} did not answer in time", delegate.label()))
            }
            DelegateFailure::Failed(reason) if reason.is_empty() => {
                LlmError::Provider(format!("{} failed", delegate.label()))
            }
            DelegateFailure::Failed(reason) => {
                LlmError::Provider(format!("{}: {reason}", delegate.label()))
            }
        }
    }
}

/// Build the completion invocation for `delegate` on the CLI's own defaults, with `prompt` on stdin.
pub fn completion_spec(delegate: Delegate, prompt: &str) -> CommandSpec {
    completion_spec_with(delegate, prompt, &DelegateOptions::default())
}

/// [`completion_spec`] with per-call [`DelegateOptions`].
pub fn completion_spec_with(delegate: Delegate, prompt: &str, opts: &DelegateOptions) -> CommandSpec {
    CommandSpec {
        program: delegate.binary().to_string(),
        args: delegate.completion_args(opts),
        stdin: Some(prompt.to_string()),
        env_remove: SCRUBBED_ENV.iter().map(|s| s.to_string()).collect(),
        timeout: COMPLETION_TIMEOUT,
        cwd_files: delegate.containment_files(),
    }
}

/// Build the **streaming** completion invocation for `delegate` (buffered args when it has no
/// streaming surface) on the CLI's own defaults, with `prompt` on stdin. Same privacy contract as
/// [`completion_spec`]: prompt never in argv, SHOGUN's own keys scrubbed from the child.
pub fn streaming_completion_spec(delegate: Delegate, prompt: &str) -> CommandSpec {
    streaming_completion_spec_with(delegate, prompt, &DelegateOptions::default())
}

/// [`streaming_completion_spec`] with per-call [`DelegateOptions`].
pub fn streaming_completion_spec_with(
    delegate: Delegate,
    prompt: &str,
    opts: &DelegateOptions,
) -> CommandSpec {
    CommandSpec {
        program: delegate.binary().to_string(),
        args: delegate.streaming_args(opts).unwrap_or_else(|| delegate.completion_args(opts)),
        stdin: Some(prompt.to_string()),
        env_remove: SCRUBBED_ENV.iter().map(|s| s.to_string()).collect(),
        timeout: COMPLETION_TIMEOUT,
        cwd_files: delegate.containment_files(),
    }
}

// chat bridge (#484)

/// The MCP server name of SHOGUN's chat bridge as Claude Code sees it (`mcp__shogun_chat__…`).
/// Mirrors `shogun_mcp::chat_bridge::SERVER_NAME` (this module builds without shogun-mcp; the
/// two are pinned equal by a test in `llm::delegate_tools`).
pub const CHAT_BRIDGE_SERVER: &str = "shogun_chat";
/// The `shogun-mcp` argument selecting the bridge mode (`shogun_mcp::chat_bridge::BRIDGE_ARG`).
pub const CHAT_BRIDGE_ARG: &str = "--chat-bridge";
/// Env names the bridge process reads (`shogun_mcp::chat_bridge::{SOCKET_ENV, NONCE_ENV}`).
pub const CHAT_BRIDGE_SOCKET_ENV: &str = "SHOGUN_CHAT_BRIDGE_SOCKET";
pub const CHAT_BRIDGE_NONCE_ENV: &str = "SHOGUN_CHAT_BRIDGE_NONCE";
/// File name of a bridged run's MCP config inside the scratch cwd. Relative on purpose, like
/// [`GEMINI_CONTAINMENT_POLICY_FILE`]: the CLI resolves it against the cwd the runner gives it.
/// The config carries the run's nonce, so it travels as a file (0600, in the 0700 scratch
/// directory, removed with it) and never on the command line, where process listings, crash
/// reports and command-line audit tools would record it (owner decision 2026-10-08, #484).
pub const CHAT_BRIDGE_MCP_CONFIG_FILE: &str = "shogun-chat-mcp.json";
/// The `shogun-mcp` argument naming the bridge file (`shogun_mcp::chat_bridge::CONFIG_ARG`).
pub const CHAT_BRIDGE_CONFIG_ARG: &str = "--chat-bridge-config";
/// File name of a bridged **Codex** run's bridge file inside the scratch cwd: the socket and the
/// nonce, which `shogun-mcp --chat-bridge --chat-bridge-config <this>` reads from its working
/// directory (`shogun_mcp::chat_bridge::read_config_file`). Codex starts a stdio MCP server in
/// the session's cwd when the server sets none, and passes it only a fixed set of environment
/// variables, so the file — not the env, not argv — is how the nonce reaches the bridge there.
pub const CHAT_BRIDGE_CONFIG_FILE: &str = "shogun-chat-bridge.json";

/// A delegate whose chat runs can carry SHOGUN's chat bridge (issue #484). The Gemini CLI is
/// deliberately not one: it can only learn of an MCP server from a settings file, and the only
/// settings file SHOGUN could write — the scratch cwd's `.gemini/settings.json` — is read only
/// for a trusted folder. Trusting the scratch folder (`--skip-trust` or
/// `GEMINI_CLI_TRUST_WORKSPACE=true`) trusts every path for the whole process and loads
/// `.gemini/.env` files from the cwd's ancestors and the home directory without the
/// auth-variable allow-list (checked 2026-10-08 against `google-gemini/gemini-cli` main
/// `44d764ee`: `packages/core/src/utils/trust.ts` `checkPathTrust`,
/// `packages/cli/src/config/settings.ts` `findEnvFile` / `loadEnvironment`,
/// `packages/cli/src/config/config.ts` `--skip-trust`). That is a wider surface than the
/// contained run, so the Gemini CLI stays tool-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgedDelegate {
    /// Claude Code: `--mcp-config` file + `--allowedTools mcp__shogun_chat`.
    ClaudeCode,
    /// Codex: `-c mcp_servers.shogun_chat.*` overrides + the bridge file.
    Codex,
}

impl BridgedDelegate {
    /// Every bridged delegate.
    pub const ALL: [BridgedDelegate; 2] = [BridgedDelegate::ClaudeCode, BridgedDelegate::Codex];

    /// The bridged form of `delegate`, or `None` for a delegate that runs tool-free.
    pub fn of(delegate: Delegate) -> Option<Self> {
        match delegate {
            Delegate::ClaudeCode => Some(BridgedDelegate::ClaudeCode),
            Delegate::Codex => Some(BridgedDelegate::Codex),
            Delegate::GeminiCli => None,
        }
    }

    /// The delegate this runs.
    pub fn delegate(self) -> Delegate {
        match self {
            BridgedDelegate::ClaudeCode => Delegate::ClaudeCode,
            BridgedDelegate::Codex => Delegate::Codex,
        }
    }
}

/// What a bridged run needs to reach SHOGUN's chat bridge for one chat turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatBridgeLaunch {
    /// The `shogun-mcp` executable shipped with the app.
    pub server: String,
    /// The run's socket (`ChatBridgeHost::socket_path`).
    pub socket: String,
    /// The run's nonce (`ChatBridgeHost::nonce`).
    pub nonce: String,
}

impl ChatBridgeLaunch {
    /// The contents of [`CHAT_BRIDGE_MCP_CONFIG_FILE`]: exactly one stdio server,
    /// [`CHAT_BRIDGE_SERVER`], launching `server --chat-bridge` with the socket and nonce in its
    /// environment.
    pub fn mcp_config(&self) -> String {
        let mut env = serde_json::Map::new();
        env.insert(CHAT_BRIDGE_SOCKET_ENV.into(), self.socket.clone().into());
        env.insert(CHAT_BRIDGE_NONCE_ENV.into(), self.nonce.clone().into());
        let mut servers = serde_json::Map::new();
        servers.insert(
            CHAT_BRIDGE_SERVER.into(),
            serde_json::json!({
                "type": "stdio",
                "command": self.server,
                "args": [CHAT_BRIDGE_ARG],
                "env": env,
            }),
        );
        serde_json::json!({ "mcpServers": servers }).to_string()
    }

    /// The contents of [`CHAT_BRIDGE_CONFIG_FILE`]: `{"socket":…,"nonce":…}`.
    pub fn bridge_config(&self) -> String {
        serde_json::json!({ "socket": self.socket, "nonce": self.nonce }).to_string()
    }
}

/// Containment for a **bridged** Claude Code chat run (issue #484): the same flags as
/// [`Delegate::containment_args`] for Claude Code, with exactly one change — the empty
/// `--mcp-config` is replaced by one carrying only SHOGUN's chat bridge — plus the permission
/// rule that lets that one server's tools run in print mode.
///
/// Checked 2026-10-08 against <https://code.claude.com/docs/en/cli-reference> and
/// <https://code.claude.com/docs/en/permissions>:
/// - `--tools ""` — "Restrict which built-in tools Claude can use. Use `""` to disable all … The
///   flag doesn't affect MCP tools". Built-in tools stay off; the bridge's tools are MCP tools.
/// - `--strict-mcp-config` with `--mcp-config <file>` — "Load MCP servers from JSON files or
///   strings"; "Only use MCP servers from `--mcp-config`, ignoring all other MCP
///   configurations": the user's own servers still do not load, and the file names one server.
///   The file is [`CHAT_BRIDGE_MCP_CONFIG_FILE`] in the scratch cwd, so the nonce stays off argv. With `-p`, the CLI "waits for still-pending servers to
///   connect before running the first turn, up to the `MCP_TIMEOUT` startup timeout".
/// - `--no-session-persistence` — unchanged.
/// - `--allowedTools mcp__shogun_chat` — "Tools that execute without prompting for permission";
///   per the permission rule syntax ("### MCP"): "`mcp__puppeteer` matches any tool provided by
///   the `puppeteer` server". So every tool of [`CHAT_BRIDGE_SERVER`] runs without a prompt (print
///   mode has nobody to answer one) and nothing else is allowed. No permission mode, no
///   `--dangerously-skip-permissions`, no `--add-dir`.
///
/// What the model can do with those tools is decided on the desktop, per call, by the same gate
/// the API-key lanes use; a send is only ever proposed to the L3 queue.
pub fn claude_code_bridged_containment_args() -> Vec<String> {
    vec![
        "--tools".to_string(),
        String::new(),
        "--strict-mcp-config".to_string(),
        "--mcp-config".to_string(),
        CHAT_BRIDGE_MCP_CONFIG_FILE.to_string(),
        "--no-session-persistence".to_string(),
        "--allowedTools".to_string(),
        format!("mcp__{CHAT_BRIDGE_SERVER}"),
    ]
}

/// The streaming Claude Code invocation for a chat turn that may use SHOGUN's tools through the
/// chat bridge (issue #484). Same privacy contract as [`streaming_completion_spec`]: the
/// prompt on stdin, SHOGUN's own keys scrubbed, an empty scratch cwd.
pub fn claude_code_bridged_spec(prompt: &str, launch: &ChatBridgeLaunch, opts: &DelegateOptions) -> CommandSpec {
    let delegate = Delegate::ClaudeCode;
    let mut args: Vec<String> =
        ["-p", "--output-format", "stream-json", "--include-partial-messages", "--verbose"]
            .map(str::to_string)
            .to_vec();
    args.extend(claude_code_bridged_containment_args());
    args.extend(delegate.option_args(opts));
    let mut cwd_files = delegate.containment_files();
    cwd_files.push((CHAT_BRIDGE_MCP_CONFIG_FILE.to_string(), launch.mcp_config()));
    let (system_args, system_file) = bridged_system_args(BridgedDelegate::ClaudeCode, opts);
    args.extend(system_args);
    cwd_files.extend(system_file);
    CommandSpec {
        program: delegate.binary().to_string(),
        args,
        stdin: Some(prompt.to_string()),
        env_remove: SCRUBBED_ENV.iter().map(|s| s.to_string()).collect(),
        timeout: COMPLETION_TIMEOUT,
        cwd_files,
    }
}

/// Containment for a **bridged** Codex chat run (issue #484): every flag of
/// [`Delegate::containment_args`] for Codex, unchanged, plus three `-c` overrides that add
/// exactly one MCP server — SHOGUN's chat bridge — and let its tools run without a prompt.
/// `server` is the `shogun-mcp` path (not a secret); the socket and nonce are only in
/// [`CHAT_BRIDGE_CONFIG_FILE`].
///
/// Checked 2026-10-08 against <https://learn.chatgpt.com/docs/config-file/config-reference>
/// (`mcp_servers.<id>.command` "Launcher command for an MCP stdio server", `.args`,
/// `.cwd` "Working directory for the MCP stdio server process", `.default_tools_approval_mode`
/// "Default approval behavior for MCP tools on this server unless a per-tool override exists"),
/// <https://learn.chatgpt.com/docs/extend/mcp> (values `auto`, `prompt`, `writes`, `approve`),
/// and the source at `openai/codex` main `d2a6dac2` (and tag `rust-v0.122.0`, the containment
/// minimum, which already has every key used here):
/// - `-c` overrides survive `--ignore-user-config`: `config/src/loader/mod.rs` replaces only the
///   user layer (`load_user_config_layer` returns an empty table) and still pushes the CLI
///   overrides as the `SessionFlags` layer, which is how the containment `-c features.*=false`
///   already apply. The user's `[mcp_servers]` stay out with their file, so the only server is
///   this one (the ChatGPT-account connectors stay off with `features.apps=false`, plugin
///   servers with `features.plugins=false`).
/// - `mcp_servers.shogun_chat.command` / `.args` — a dotted `-c` key becomes a nested table
///   (`utils/cli/src/config_override.rs`), deserialized as a stdio server. No `cwd` and no
///   `env`: a stdio server without `cwd` is started in the session's cwd
///   (`rmcp-client/src/stdio_server_launcher.rs` `LocalStdioServerLauncher` → `fallback_cwd`,
///   `core/src/session/mcp.rs` `local_process_cwd`), i.e. the scratch cwd holding
///   [`CHAT_BRIDGE_CONFIG_FILE`]; and it is not sandboxed (only Codex's own shell commands are).
///   Its environment is a fixed allow-list (`DEFAULT_ENV_VARS`), which is why the nonce travels
///   as a file rather than an env var.
/// - `mcp_servers.shogun_chat.default_tools_approval_mode="approve"` — `codex exec` runs with
///   `approval_policy = never` (`exec/src/lib.rs`), and under `never` with a read-only sandbox an
///   MCP tool that needs approval (any tool not annotated read-only) is **denied** ("MCP tool call
///   requires approval, but approval policy is never", `core/src/mcp_tool_call.rs`). `approve`
///   auto-approves that server's tools only (`mcp_permission_prompt_is_auto_approved`); no
///   approval or sandbox setting changes for anything else, and there is no
///   `--dangerously-bypass-approvals-and-sandbox` / `--full-auto`.
///
/// What the model can do with those tools is decided on the desktop, per call, by the same gate
/// the API-key lanes use; a send is only ever proposed to the L3 queue.
pub fn codex_bridged_containment_args(server: &str) -> Vec<String> {
    let mut args = Delegate::Codex.containment_args();
    // A TOML basic string: JSON's string escapes are a subset of TOML's.
    let command = serde_json::Value::from(server).to_string();
    let bridge_args = serde_json::json!([CHAT_BRIDGE_ARG, CHAT_BRIDGE_CONFIG_ARG, CHAT_BRIDGE_CONFIG_FILE]);
    for (key, value) in [
        ("command", command),
        ("args", bridge_args.to_string()),
        ("default_tools_approval_mode", r#""approve""#.to_string()),
    ] {
        args.push("-c".to_string());
        args.push(format!("mcp_servers.{CHAT_BRIDGE_SERVER}.{key}={value}"));
    }
    args
}

/// The `codex exec --json` invocation for a chat turn that may use SHOGUN's tools through the
/// chat bridge (issue #484). Same privacy contract as [`streaming_completion_spec`]; the
/// bridge file is written into the scratch cwd with the containment files. Codex answers in one
/// piece (`StreamFormat::CodexJsonl`, outside SLO-03), and its tool calls reach SHOGUN over the
/// bridge whatever the output format.
pub fn codex_bridged_spec(prompt: &str, launch: &ChatBridgeLaunch, opts: &DelegateOptions) -> CommandSpec {
    let delegate = Delegate::Codex;
    let mut args: Vec<String> = ["exec", "--json", "-"].map(str::to_string).to_vec();
    args.extend(codex_bridged_containment_args(&launch.server));
    args.extend(delegate.option_args(opts));
    let mut cwd_files = delegate.containment_files();
    cwd_files.push((CHAT_BRIDGE_CONFIG_FILE.to_string(), launch.bridge_config()));
    let (system_args, system_file) = bridged_system_args(BridgedDelegate::Codex, opts);
    args.extend(system_args);
    cwd_files.extend(system_file);
    CommandSpec {
        program: delegate.binary().to_string(),
        args,
        stdin: Some(prompt.to_string()),
        env_remove: SCRUBBED_ENV.iter().map(|s| s.to_string()).collect(),
        timeout: COMPLETION_TIMEOUT,
        cwd_files,
    }
}

/// File name, inside the scratch cwd, of SHOGUN's chat contract for a bridged Claude Code run.
pub const CHAT_SYSTEM_PROMPT_FILE: &str = "shogun-chat-system.md";

/// How a bridged run receives SHOGUN's chat contract in the CLI's own system role, when
/// [`DelegateOptions::shogun_system`] asks for it (owner decision 2026-10-14). Both forms
/// **append** to the CLI's default instructions; neither replaces them.
///
/// - **Claude Code** — `--append-system-prompt-file <file>`. Checked 2026-10-14 against
///   <https://code.claude.com/docs/en/cli-reference> ("System prompt flags"): "`--append-system-prompt`
///   Appends to the default prompt", "`--append-system-prompt-file` Appends file contents to the
///   default prompt", "All five work in both interactive and non-interactive modes". The file form,
///   so the text stays off argv like every prompt byte (the module's `ps` rule); the file is
///   [`CHAT_SYSTEM_PROMPT_FILE`] in the 0700 scratch cwd, written 0600 and removed with it.
///   `--system-prompt` / `--system-prompt-file` (replace) stay forbidden.
/// - **Codex** — `-c developer_instructions="…"`. Checked 2026-10-14 against
///   <https://learn.chatgpt.com/docs/config-file/config-reference>: "`developer_instructions`
///   (string): Additional developer instructions injected into the session". A `-c` override
///   survives `--ignore-user-config` (see [`codex_bridged_containment_args`]). There is no file
///   form, so the value is in argv — which is why the text is SHOGUN's constant contract and
///   never anything per user. `model_instructions_file` ("Replacement for built-in instructions")
///   is the replacing form and is not used.
///
/// The CLIs' own instruction files still load, and the contract sits beside them: Claude Code in
/// `-p` from the scratch cwd reads the user's `~/.claude/CLAUDE.md` and auto memory (the cwd has
/// no project file). Only `--safe-mode` / `--bare` skip those, and neither fits: safe mode also
/// drops MCP servers (the bridge), bare mode changes the tool surface and sign-in. Codex's
/// `--ignore-user-config` is documented for `config.toml` only, so a global
/// `$CODEX_HOME/AGENTS.md` presumably still loads (unverified). A user's coding-style global
/// instructions can therefore colour delegated chat answers; the appended contract is the
/// counterweight, and it says it is SHOGUN's chat, not a coding session.
pub fn bridged_system_args(
    delegate: BridgedDelegate,
    opts: &DelegateOptions,
) -> (Vec<String>, Option<(String, String)>) {
    if !opts.shogun_system {
        return (Vec::new(), None);
    }
    let text = crate::llm::prompt_cache::delegated_system_text();
    match delegate {
        BridgedDelegate::ClaudeCode => (
            vec!["--append-system-prompt-file".to_string(), CHAT_SYSTEM_PROMPT_FILE.to_string()],
            Some((CHAT_SYSTEM_PROMPT_FILE.to_string(), text)),
        ),
        // A TOML basic string: JSON's string escapes are a subset of TOML's.
        BridgedDelegate::Codex => (
            vec!["-c".to_string(), format!("developer_instructions={}", serde_json::Value::from(text))],
            None,
        ),
    }
}

/// The bridged chat invocation for `delegate` (issue #484). Only a [`BridgedDelegate`] has one,
/// so the Gemini CLI cannot be started with the bridge.
pub fn bridged_spec(
    delegate: BridgedDelegate,
    prompt: &str,
    launch: &ChatBridgeLaunch,
    opts: &DelegateOptions,
) -> CommandSpec {
    match delegate {
        BridgedDelegate::ClaudeCode => claude_code_bridged_spec(prompt, launch, opts),
        BridgedDelegate::Codex => codex_bridged_spec(prompt, launch, opts),
    }
}

/// The Agent-lane client that runs on the user's subscription via a local vendor CLI.
///
/// Implements [`AgentClient`] and **only** [`AgentClient`]. There is no [`BatchClient`](super::BatchClient)
/// implementation anywhere in this file, so indexing / Dream Cycle / Morning Brief cannot be routed
/// here even by a caller who wants to: invariant 5 stays a compile error, not a code review note.
pub struct SubscriptionAgentClient<R: CommandRunner, S: TraceabilitySink> {
    runner: R,
    sink: S,
    delegate: Delegate,
    purpose: &'static str,
    usage: Option<crate::llm::usage::UsageHook>,
    /// Explicit per-call options. `None` means the prompt's registry policy
    /// ([`DelegateOptions::for_tag`] on the usage hook's tag), or the CLI defaults without one.
    options: Option<DelegateOptions>,
    /// The minimum-version check run before a completion is spawned, if any.
    version_gate: Option<&'static VersionGate>,
}

impl<R: CommandRunner, S: TraceabilitySink> SubscriptionAgentClient<R, S> {
    pub fn new(runner: R, sink: S, delegate: Delegate) -> Self {
        Self { runner, sink, delegate, purpose: "agent", usage: None, options: None, version_gate: None }
    }

    /// Check the installed CLI against [`Delegate::min_version`] before each run (normally with
    /// [`VERSION_GATE`]). A CLI known to be too old is refused with [`LlmError::NeedsUpdate`]
    /// before anything is spawned: no egress, so no trace and no usage row.
    pub fn with_version_gate(mut self, gate: &'static VersionGate) -> Self {
        self.version_gate = Some(gate);
        self
    }

    /// The gate's verdict for this client's delegate (`Ok` without a gate).
    fn version_refusal(&self) -> Result<(), LlmError> {
        match self.version_gate {
            Some(gate) => gate
                .check(&self.runner, self.delegate)
                .map_err(|failure| failure.into_llm_error(self.delegate)),
            None => Ok(()),
        }
    }

    /// Override the per-call [`DelegateOptions`], replacing the prompt tag's registry policy.
    pub fn with_options(mut self, options: DelegateOptions) -> Self {
        self.options = Some(options);
        self
    }

    /// The options this client's runs use: the explicit ones, else the registry policy of the
    /// prompt it serves (issue #388 — effort is a per-`PromptTag` decision), else CLI defaults.
    pub fn options(&self) -> DelegateOptions {
        match (&self.options, &self.usage) {
            (Some(o), _) => o.clone(),
            (None, Some(hook)) => DelegateOptions::for_tag(hook.tag),
            (None, None) => DelegateOptions::default(),
        }
    }

    /// The `llm_usage.model` value: the delegate, plus the model it was asked for when one was
    /// (`codex/<model>`), so a per-model cost comparison is one `GROUP BY`. Effort has no column
    /// in `llm_usage` (V30) and is deliberately not smuggled into this one. `used` is what the
    /// recorded attempt actually ran with (the defaults, after a retry).
    fn usage_model(&self, used: &DelegateOptions) -> String {
        let requested = match self.delegate {
            // Only a delegate that actually passes the model on may claim it in the ledger.
            Delegate::Codex => used.safe_model(),
            Delegate::ClaudeCode | Delegate::GeminiCli => None,
        };
        match requested {
            Some(m) => format!("{}/{m}", self.delegate.binary()),
            None => self.delegate.binary().to_string(),
        }
    }

    /// Attach usage accounting (FR-HN-01 / #313). The CLI reports no token counts, so rows on
    /// this lane are byte-length **estimates**, marked as such — still enough for the monthly
    /// delegate-quota meter (FR-HN-08), where the resource being spent is the user's plan.
    pub fn with_usage(mut self, hook: crate::llm::usage::UsageHook) -> Self {
        self.usage = Some(hook);
        self
    }

    /// Override the traceability `purpose` (e.g. `"chat"` vs the default `"agent"`).
    pub fn with_purpose(mut self, purpose: &'static str) -> Self {
        self.purpose = purpose;
        self
    }

    pub fn delegate(&self) -> Delegate {
        self.delegate
    }

    /// One egress trace, written **before** a run: the trace must exist for a send that then fails
    /// mid-flight, because the content left the process either way (AR-11). `Err` when the ledger
    /// would not take the row; the delegate is then not started (fail-closed).
    fn trace(&self, prompt: &str) -> Result<(), LlmError> {
        self.sink.try_record(TraceRecord::for_chunk(
            Route::LocalAgent,
            self.purpose,
            self.delegate.destination(),
            prompt,
            false,
        ))?;
        Ok(())
    }

    /// Interactive completion whose delegate process dies when its chat turn is cancelled.
    pub fn complete_cancellable(
        &self,
        prompt: &str,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<String, LlmError> {
        self.complete_recorded(prompt, Some(cancelled))
    }

    /// One buffered attempt with `opts`. Writes its own egress trace (every spawn is an egress);
    /// the usage row is the caller's, once per call.
    fn complete_with(
        &self,
        prompt: &str,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
        opts: &DelegateOptions,
    ) -> Attempt {
        if let Err(e) = self.trace(prompt) {
            return Attempt::final_err(e);
        }

        let spec = completion_spec_with(self.delegate, prompt, opts);
        let run = match cancelled {
            Some(flag) => self.runner.run_cancellable(&spec, flag),
            None => self.runner.run(&spec),
        };
        let out = match run {
            Ok(out) => out,
            Err(RunError::NotFound) => {
                return Attempt::final_err(DelegateFailure::NotInstalled.into_llm_error(self.delegate))
            }
            Err(RunError::Timeout) => {
                return Attempt::final_err(DelegateFailure::Timeout.into_llm_error(self.delegate))
            }
            Err(RunError::Cancelled) => {
                return Attempt::final_err(
                    DelegateFailure::Failed("cancelled".into()).into_llm_error(self.delegate),
                )
            }
            Err(RunError::Io(e)) => {
                return Attempt::final_err(DelegateFailure::Failed(e).into_llm_error(self.delegate))
            }
        };

        if !out.ok() {
            return Attempt::refused(classify(&out), self.delegate);
        }

        let text = out.stdout.trim();
        if text.is_empty() {
            return Attempt::final_err(
                DelegateFailure::Failed("returned an empty response".into())
                    .into_llm_error(self.delegate),
            );
        }
        Attempt::done(Ok(text.to_string()))
    }

    /// Whether a refused attempt run with `used` may be rerun once on the CLI's own defaults
    /// (issue #388): only when `used` actually put option argv on the command line — a run on
    /// the defaults has nothing left to drop — and the caller has not withdrawn the turn.
    fn may_retry_on_defaults(
        &self,
        used: &DelegateOptions,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> bool {
        !self.delegate.option_args(used).is_empty()
            && !cancelled.is_some_and(|f| f.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Like [`AgentClient::complete`], but pushes each text delta into `out` as the delegate
    /// produces it (SLO-03 / FR-AG-07), and returns the whole answer. A closed receiver stops the
    /// run early — the child is killed instead of generating tokens nobody is reading.
    ///
    /// Falls back to the buffered [`AgentClient::complete`] contract (whole answer as one delta)
    /// when the delegate has no streaming surface, or when the installed CLI rejects the streaming
    /// flags — an older version predating them. The fallback run is a second spawn and writes its
    /// own trace, because it is a second egress.
    pub fn complete_streaming(
        &self,
        prompt: &str,
        out: std::sync::mpsc::Sender<String>,
    ) -> Result<String, LlmError> {
        self.stream_inner(prompt, out, None)
    }

    /// [`Self::complete_streaming`] with a caller-owned cancel flag. Flipping the flag kills the
    /// delegate child immediately — including while it is silently thinking, when no delta is
    /// due and the receiver-dropped signal of the plain variant could not reach it — and returns
    /// whatever text had streamed so far as a deliberate stop, not an error. This is the variant
    /// the chat Stop button needs: with the plain one, Stop on the subscription lane only took
    /// effect at the *next* token, routinely 5–30 s into a `claude -p` turn.
    pub fn complete_streaming_cancellable(
        &self,
        prompt: &str,
        out: std::sync::mpsc::Sender<String>,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<String, LlmError> {
        self.stream_inner(prompt, out, Some(cancelled))
    }

    /// The streamed run plus its usage row (FR-HN-01 / #313). The row carries `first_token_ms` —
    /// the moment the first delta was handed to the caller's channel — so the subscription lane's
    /// first-token latency reaches the ledger.
    ///
    /// A run with non-default options that the delegate refuses **before any delta reached the
    /// caller** is rerun once on the CLI's defaults (see [`Self::complete_recorded_with`]); after
    /// a delta, the caller has already seen text and the failure stands. `first_token_ms` is
    /// measured from the start of the call, so a retried call's row shows the wait the user had.
    fn stream_inner(
        &self,
        prompt: &str,
        out: std::sync::mpsc::Sender<String>,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<String, LlmError> {
        // A turn withdrawn before it spawned sent nothing and costs nothing: no row.
        if cancelled.is_some_and(|f| f.load(std::sync::atomic::Ordering::Acquire)) {
            return Ok(String::new());
        }
        // Neither does a turn refused because the CLI is too old.
        self.version_refusal()?;
        let started = std::time::Instant::now();
        let mut first_token_ms = None;
        let mut used = self.options();
        let mut attempt = self.stream_run(prompt, &out, cancelled, started, &mut first_token_ms, &used);
        if attempt.refused && first_token_ms.is_none() && self.may_retry_on_defaults(&used, cancelled) {
            used = DelegateOptions::default();
            attempt = self.stream_run(prompt, &out, cancelled, started, &mut first_token_ms, &used);
        }
        if attempt.owes_row {
            self.record_usage(prompt, &attempt.result, first_token_ms, started, &used);
        }
        attempt.result
    }

    /// One streamed attempt with `opts`. [`Attempt::owes_row`] is `false` when the attempt fell
    /// back to the buffered contract, which records its own row.
    fn stream_run(
        &self,
        prompt: &str,
        out: &std::sync::mpsc::Sender<String>,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
        started: std::time::Instant,
        first_token_ms: &mut Option<u64>,
        opts: &DelegateOptions,
    ) -> Attempt {
        let Some(format) = self.delegate.stream_format() else {
            return self.buffered_fallback(prompt, out, cancelled, opts);
        };

        if let Err(e) = self.trace(prompt) {
            return Attempt::final_err(e);
        }

        let mut parser = DeltaParser::new(format);
        let mut receiver_gone = false;
        let spec = streaming_completion_spec_with(self.delegate, prompt, opts);
        let mut on_line = |line: &str| match parser.push(line) {
            Some(delta) if !delta.is_empty() => {
                if out.send(delta).is_err() {
                    receiver_gone = true;
                    return false;
                }
                first_token_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                true
            }
            _ => true,
        };
        let run = match cancelled {
            Some(flag) => self.runner.run_streaming_cancellable(&spec, &mut on_line, flag),
            None => self.runner.run_streaming(&spec, &mut on_line),
        };

        let finished = match run {
            Ok(o) => o,
            Err(RunError::NotFound) => {
                return Attempt::final_err(DelegateFailure::NotInstalled.into_llm_error(self.delegate))
            }
            Err(RunError::Timeout) => {
                return Attempt::final_err(DelegateFailure::Timeout.into_llm_error(self.delegate))
            }
            // The caller withdrew the turn (Stop): what streamed is theirs — a deliberate stop,
            // not a failure.
            Err(RunError::Cancelled) => {
                return Attempt::done(Ok(parser.into_whole().trim().to_string()));
            }
            Err(RunError::Io(e)) => {
                return Attempt::final_err(DelegateFailure::Failed(e).into_llm_error(self.delegate))
            }
        };

        // The caller hung up (panel closed, stop pressed): what streamed is theirs, and the run
        // was cut short on purpose — not a failure.
        if receiver_gone {
            return Attempt::done(Ok(parser.into_whole().trim().to_string()));
        }

        if !finished.ok() {
            // The streaming *flags* being refused is an old CLI, not a real failure — rerun on
            // the buffered contract the same binary does support.
            if rejects_streaming_flags(&finished) {
                return self.buffered_fallback(prompt, out, cancelled, opts);
            }
            let finished = match format {
                StreamFormat::CodexJsonl => with_codex_error(finished),
                StreamFormat::ClaudeStreamJson | StreamFormat::RawText => finished,
            };
            return Attempt::refused(classify(&finished), self.delegate);
        }

        let mut whole = parser.into_whole().trim().to_string();
        if whole.is_empty() {
            // A zero exit that still reported a failed turn (`turn.failed` / `error`) is a
            // refusal, in the delegate's own words — not an empty answer.
            if format == StreamFormat::CodexJsonl && codex_error_message(&finished.stdout).is_some() {
                return Attempt::refused(classify(&with_codex_error(finished)), self.delegate);
            }
            // A zero exit whose stdout parsed to nothing: if the CLI ignored the streaming flags
            // and printed plain text, that text IS the answer — pass it through rather than
            // erroring on a run that succeeded. JSON-looking output stays excluded: raw events
            // are not an answer.
            let raw = finished.stdout.trim();
            if !raw.is_empty() && !raw.starts_with('{') {
                let _ = out.send(raw.to_string());
                first_token_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                whole = raw.to_string();
            } else {
                return Attempt::final_err(
                    DelegateFailure::Failed("returned an empty response".into())
                        .into_llm_error(self.delegate),
                );
            }
        }
        Attempt::done(Ok(whole))
    }

    /// The buffered contract standing in for a stream (whole answer as one delta). It is a call
    /// of its own — own spawn, own trace, own usage row, own retry on defaults — so the stream
    /// owes no row for it.
    fn buffered_fallback(
        &self,
        prompt: &str,
        out: &std::sync::mpsc::Sender<String>,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
        opts: &DelegateOptions,
    ) -> Attempt {
        let text = self.complete_recorded_with(prompt, cancelled, opts.clone());
        if let Ok(text) = &text {
            let _ = out.send(text.clone());
        }
        Attempt { result: text, owes_row: false, refused: false }
    }
}

/// The outcome of one delegate spawn, before the call decides on a retry and its usage row.
struct Attempt {
    result: Result<String, LlmError>,
    /// Whether the call still owes a usage row for this attempt (`false` when a nested buffered
    /// call already wrote it).
    owes_row: bool,
    /// The delegate ran and refused the turn for a reason that is not the account's: not the
    /// login, not the plan limit (no model or effort choice changes those), not a timeout, a
    /// missing binary or a cancel. Only such a refusal is worth one rerun on the defaults.
    refused: bool,
}

impl Attempt {
    fn done(result: Result<String, LlmError>) -> Self {
        Self { result, owes_row: true, refused: false }
    }

    fn final_err(e: LlmError) -> Self {
        Self::done(Err(e))
    }

    fn refused(failure: DelegateFailure, delegate: Delegate) -> Self {
        // An argument error is retried on the defaults too: the refused flag may be one of the
        // per-call options (`--effort`, `--model`, `-c …`) rather than a containment flag. If the
        // defaults are refused as well, that run's `NeedsUpdate` is the answer.
        let refused = matches!(failure, DelegateFailure::Failed(_) | DelegateFailure::NeedsUpdate);
        Self { result: Err(failure.into_llm_error(delegate)), owes_row: true, refused }
    }
}

/// The argument-parser errors of these CLIs, lowercased: `error: unknown option '--x'`
/// (Claude Code), `error: unexpected argument '--x' found` (Codex), `Unknown arguments: x`
/// (Gemini CLI).
const FLAG_MARKERS: &[&str] =
    &["unknown option", "unknown argument", "unrecognized option", "unexpected argument", "invalid option"];

/// Whether `text` carries one of the argument-parser errors in [`FLAG_MARKERS`]. Fixed patterns
/// only — the CLI's own text never travels further than this check.
fn has_flag_error(text: &str) -> bool {
    let hay = text.to_lowercase();
    FLAG_MARKERS.iter().any(|m| hay.contains(m))
}

/// Did a failed streaming run refuse the streaming flags themselves (a CLI predating them)?
/// Checked before [`classify`]: rerunning on the buffered contract is strictly better than
/// surfacing an argument error, and if the buffered run is refused too, *that* run's
/// classification ([`DelegateFailure::NeedsUpdate`]) is the answer.
fn rejects_streaming_flags(out: &CommandOutput) -> bool {
    has_flag_error(&out.stderr) || has_flag_error(&out.stdout)
}

/// A failed `codex exec --json` run reports why on **stdout**, inside an `error` event
/// (`message`) or a `turn.failed` event (`error.message`) — see [`StreamFormat::CodexJsonl`].
/// Left alone, [`classify`] would still find quota/login words in the raw JSON, but the one-line
/// reason shown to the user would be the first JSON event (`{"type":"thread.started",…}`). This
/// puts the delegate's own last error message first on stderr, where [`classify`] reads it.
pub(crate) fn with_codex_error(mut out: CommandOutput) -> CommandOutput {
    if let Some(message) = codex_error_message(&out.stdout) {
        out.stderr = if out.stderr.trim().is_empty() {
            message
        } else {
            format!("{message}\n{}", out.stderr)
        };
    }
    out
}

/// The last failure message a `codex exec --json` stdout reports (`turn.failed` → `error.message`,
/// `error` → `message`), if any.
pub(crate) fn codex_error_message(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        let m = match v.get("type")?.as_str()? {
            "turn.failed" => v.pointer("/error/message")?,
            "error" => v.get("message")?,
            _ => return None,
        };
        m.as_str().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string)
    })
}

impl<R: CommandRunner, S: TraceabilitySink> SubscriptionAgentClient<R, S> {
    /// [`Self::complete_recorded_with`] on this client's [`Self::options`].
    fn complete_recorded(
        &self,
        prompt: &str,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<String, LlmError> {
        self.complete_recorded_with(prompt, cancelled, self.options())
    }

    /// A buffered call: one attempt with `opts`, plus the usage row (FR-HN-01 / #313). Wrapping
    /// here rather than inside `complete_with` keeps the cancellable path accounted for too.
    ///
    /// Safety net for issue #388: model and effort come from SHOGUN's registry, not from the
    /// user's own CLI config, so a model that rejects the requested effort (or a retired model id)
    /// would otherwise turn every inline draft into an error. When an attempt that carried option
    /// argv is refused, the call is rerun **once** on the CLI's own defaults — the configuration
    /// the user already runs. Login and plan-limit refusals are not retried: no option choice
    /// changes them. Either way the call writes exactly one usage row, for the attempt whose
    /// result it returns, attributed to the options that attempt ran with.
    fn complete_recorded_with(
        &self,
        prompt: &str,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
        opts: DelegateOptions,
    ) -> Result<String, LlmError> {
        // A CLI known to be too old is refused before anything is spawned: no trace, no row.
        self.version_refusal()?;
        let started = std::time::Instant::now();
        let mut used = opts;
        let mut attempt = self.complete_with(prompt, cancelled, &used);
        if attempt.refused && self.may_retry_on_defaults(&used, cancelled) {
            used = DelegateOptions::default();
            attempt = self.complete_with(prompt, cancelled, &used);
        }
        // A buffered body arrives whole; there is no first token to time.
        self.record_usage(prompt, &attempt.result, None, started, &used);
        attempt.result
    }

    /// One usage row for a delegated run. The CLI reports no token counts on these paths, so the
    /// row is marked estimated; the outcome and the latency are real either way.
    fn record_usage(
        &self,
        prompt: &str,
        result: &Result<String, LlmError>,
        first_token_ms: Option<u64>,
        started: std::time::Instant,
        used: &DelegateOptions,
    ) {
        use crate::llm::usage::{Usage, UsageOutcome, UsageRecord};
        let Some(hook) = &self.usage else { return };
        let (usage, outcome) = match result {
            Ok(text) => (Usage::estimated_from_bytes(prompt.len(), text.len()), UsageOutcome::Ok),
            Err(e) => (Usage::default(), UsageOutcome::from_error(e)),
        };
        hook.sink.record(UsageRecord::new(
            Route::LocalAgent,
            hook.tag,
            &self.usage_model(used),
            usage,
            first_token_ms,
            started.elapsed().as_millis() as u64,
            outcome,
        ));
    }
}

impl<R: CommandRunner, S: TraceabilitySink> AgentClient for SubscriptionAgentClient<R, S> {
    fn complete(&self, prompt: &str) -> Result<String, LlmError> {
        self.complete_recorded(prompt, None)
    }
}

/// Runs delegates as real subprocesses.
///
/// Not compiled into the Linux test path's assertions — every rule in this module is tested through
/// [`MockRunner`] — but it is ordinary `std::process` code and builds everywhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunError> {
        self.run_inner(spec, None)
    }

    fn run_cancellable(
        &self,
        spec: &CommandSpec,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<CommandOutput, RunError> {
        self.run_inner(spec, Some(cancelled))
    }

    /// Real incremental streaming: stdout is read line-by-line on a drain thread and pumped to
    /// `on_line` as it arrives, instead of after the child exits.
    ///
    /// The timeout here is an **inactivity** deadline rather than the absolute one [`Self::run`]
    /// uses: a generation still producing output is alive however long it takes, while one that
    /// has gone quiet for the whole window is stuck on something interactive we failed to
    /// suppress.
    fn run_streaming(
        &self,
        spec: &CommandSpec,
        on_line: &mut dyn FnMut(&str) -> bool,
    ) -> Result<CommandOutput, RunError> {
        self.run_streaming_inner(spec, on_line, None)
    }

    fn run_streaming_cancellable(
        &self,
        spec: &CommandSpec,
        on_line: &mut dyn FnMut(&str) -> bool,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<CommandOutput, RunError> {
        self.run_streaming_inner(spec, on_line, Some(cancelled))
    }
}

impl ProcessRunner {
    /// The shared body of both streaming entry points. `cancel_flag` is polled on every turn of
    /// the pump loop (every 20 ms while the child is quiet), so a Stop pressed mid-thinking kills
    /// the child within one tick rather than at the next stdout line.
    fn run_streaming_inner(
        &self,
        spec: &CommandSpec,
        on_line: &mut dyn FnMut(&str) -> bool,
        cancel_flag: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<CommandOutput, RunError> {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::process::{Command, Stdio};

        if cancel_flag.is_some_and(|f| f.load(std::sync::atomic::Ordering::Acquire)) {
            return Err(RunError::Cancelled);
        }

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in &spec.env_remove {
            cmd.env_remove(name);
        }
        isolate_process_group(&mut cmd);
        let scratch = ScratchCwd::create(&spec.cwd_files)?;
        cmd.current_dir(scratch.path());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunError::NotFound),
            Err(e) => return Err(RunError::Io(redact_secrets(&e.to_string()))),
        };

        let writer = spec.stdin.clone().and_then(|body| {
            child.stdin.take().map(|mut pipe| {
                std::thread::spawn(move || {
                    let _ = pipe.write_all(body.as_bytes());
                })
            })
        });
        if spec.stdin.is_none() {
            drop(child.stdin.take());
        }

        // Lines cross to this thread over a channel; the drain thread also keeps the joined copy,
        // so the returned CommandOutput carries the full stream even if the callback stops early.
        let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
        let out_reader = child.stdout.take().map(|p| {
            std::thread::spawn(move || {
                let mut whole = String::new();
                for line in BufReader::new(p).lines() {
                    let Ok(line) = line else { break };
                    whole.push_str(&line);
                    whole.push('\n');
                    // A dropped receiver just means the pump stopped — keep draining so the
                    // child can never block on a full pipe while being killed.
                    let _ = line_tx.send(line);
                }
                whole
            })
        });
        let err_reader = child.stderr.take().map(|mut p| {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = p.read_to_string(&mut s);
                s
            })
        });

        let pgid = child.id();
        let pipes = PipeThreads { writer, stdout: out_reader, stderr: err_reader };
        // Every line handed to `on_line`, kept so a stdout drain thread abandoned to a straggler
        // (see `PipeThreads::collect`) still leaves the caller the stream it was shown.
        let mut seen = String::new();
        let mut cancelled = false;
        let mut withdrawn = false;
        let mut timed_out = false;
        let mut last_activity = std::time::Instant::now();
        let mut drain_until = None;
        let status = loop {
            // The caller's flag wins over everything else: a delegate that is still thinking
            // produces no lines, so this is the only place Stop can take effect promptly.
            if cancel_flag.is_some_and(|f| f.load(std::sync::atomic::Ordering::Acquire)) {
                withdrawn = true;
                kill_tree(&mut child);
                break None;
            }
            // Hand over everything that has arrived.
            while let Ok(line) = line_rx.try_recv() {
                last_activity = std::time::Instant::now();
                seen.push_str(&line);
                seen.push('\n');
                if !on_line(&line) {
                    cancelled = true;
                    break;
                }
            }
            if cancelled {
                kill_tree(&mut child);
                break None;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    // The child exited; the drain thread may still hold its tail — for Claude
                    // Code the last line is the `result` event that says whether the turn
                    // failed. Hand over lines until the drain thread finishes (the channel
                    // closes), not until the first quiet moment, which on a loaded machine loses
                    // that line. Bounded: something the delegate spawned can hold the pipe open
                    // for as long as it lives.
                    let deadline = std::time::Instant::now() + POST_EXIT_DRAIN;
                    drain_until = Some(deadline);
                    loop {
                        if cancel_flag.is_some_and(|f| f.load(std::sync::atomic::Ordering::Acquire)) {
                            // Stop pressed after the delegate itself exited: whatever it
                            // spawned and left holding the pipe goes too.
                            withdrawn = true;
                            kill_group(pgid);
                            break;
                        }
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        match line_rx.recv_timeout(left.min(Duration::from_millis(20))) {
                            Ok(line) => {
                                seen.push_str(&line);
                                seen.push('\n');
                                if !on_line(&line) {
                                    break;
                                }
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    break Some(status);
                }
                Ok(None) => {
                    if last_activity.elapsed() >= spec.timeout {
                        kill_tree(&mut child);
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(RunError::Io(redact_secrets(&e.to_string()))),
            }
        };

        // On the kill paths nothing downstream reads stdout/stderr, so do not wait for the
        // drain threads: a pipe still held by a straggler would otherwise stall Stop for as
        // long as that straggler lives. The threads end on their own when the pipe closes.
        if withdrawn {
            return Err(RunError::Cancelled);
        }
        if timed_out {
            return Err(RunError::Timeout);
        }
        let deadline = drain_until.unwrap_or_else(|| std::time::Instant::now() + POST_EXIT_DRAIN);
        let (stdout, stderr) = pipes.collect(pgid, deadline);
        Ok(CommandOutput {
            code: status.and_then(|s| s.code()),
            stdout: stdout.unwrap_or(seen),
            stderr: stderr.unwrap_or_default(),
        })
    }
}

/// How long a delegate's output may keep arriving after the delegate itself exited. Its own
/// buffered tail arrives at once; anything still holding the pipes past this is something it
/// spawned (an MCP server, a helper) that outlived it, and is killed rather than waited for.
const POST_EXIT_DRAIN: Duration = Duration::from_secs(2);

/// How long the pipe threads get to finish once the delegate's process group was killed. Past
/// it they are abandoned: they end on their own when the last holder of the pipe exits.
const KILL_GRACE: Duration = Duration::from_millis(500);

/// The threads feeding a delegate its prompt and draining its output.
struct PipeThreads {
    writer: Option<std::thread::JoinHandle<()>>,
    stdout: Option<std::thread::JoinHandle<String>>,
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl PipeThreads {
    fn finished(&self) -> bool {
        self.writer.as_ref().is_none_or(std::thread::JoinHandle::is_finished)
            && self.stdout.as_ref().is_none_or(std::thread::JoinHandle::is_finished)
            && self.stderr.as_ref().is_none_or(std::thread::JoinHandle::is_finished)
    }

    fn wait_until(&self, deadline: std::time::Instant) -> bool {
        while !self.finished() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// The output of a delegate that has exited (or was killed), without letting a straggler
    /// hold the caller. A process the delegate spawned inherits its pipes; joining the drain
    /// threads unconditionally would block until that process happened to exit, so the run would
    /// never finish, Stop would spin, and the thread would leak. The threads get until
    /// `deadline`; then the delegate's process group (see [`isolate_process_group`]) is killed,
    /// which closes the pipes it holds, and they get [`KILL_GRACE`] more. A thread still running
    /// after that is abandoned, and its stream is `None`.
    fn collect(self, pgid: u32, deadline: std::time::Instant) -> (Option<String>, Option<String>) {
        if !self.wait_until(deadline) {
            // Something still holds a pipe, so the group most likely still has a live member and
            // its id has not been handed to a new group.
            kill_group(pgid);
            self.wait_until(std::time::Instant::now() + KILL_GRACE);
        }
        fn take<T>(h: Option<std::thread::JoinHandle<T>>) -> Option<T> {
            h.filter(std::thread::JoinHandle::is_finished).and_then(|h| h.join().ok())
        }
        let _ = take(self.writer);
        (take(self.stdout), take(self.stderr))
    }
}

/// An empty, per-invocation working directory for the delegate.
///
/// A GUI app's cwd is `/` (or whatever launched it), and every vendor CLI treats its cwd as the
/// project: Claude Code auto-approves reads inside it and loads `.claude/` settings from it,
/// Codex sandboxes writes to it. Giving the child an empty directory makes "inside the project"
/// mean nothing. Removed when the run ends; if the directory cannot be created the system temp
/// dir is used, which still contains no user project.
///
/// The only things ever placed in it are the spec's `cwd_files` (SHOGUN's own containment
/// files). A spec that has some is refused rather than run when they cannot be written into a
/// directory of our own — including the shared-temp-dir fallback, which is not ours to litter.
struct ScratchCwd {
    path: std::path::PathBuf,
    owned: bool,
}

impl ScratchCwd {
    fn create(files: &[(String, String)]) -> Result<Self, RunError> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("shogun-delegate-{}-{n}", std::process::id()));
        let scratch = match std::fs::create_dir_all(&path) {
            Ok(()) => Self { path, owned: true },
            Err(_) => Self { path: std::env::temp_dir(), owned: false },
        };
        // Only this user may list or open what goes in (a bridged run's config holds its nonce).
        #[cfg(unix)]
        if scratch.owned {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&scratch.path, std::fs::Permissions::from_mode(0o700));
        }
        if files.is_empty() {
            return Ok(scratch);
        }
        let refuse = || RunError::Io("could not prepare the delegate's containment files".to_string());
        if !scratch.owned {
            return Err(refuse());
        }
        for (name, contents) in files {
            // A plain file name only: nothing may land outside the scratch directory.
            let plain = std::path::Path::new(name).file_name().is_some_and(|f| f == name.as_str());
            if !plain || name == "." || name == ".." {
                return Err(refuse());
            }
            write_private(&scratch.path.join(name), contents).map_err(|_| refuse())?;
        }
        Ok(scratch)
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Write a containment file readable by this user only (0600 on Unix), refusing to follow or
/// reuse anything already at the path.
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

impl Drop for ScratchCwd {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Put the delegate in its own process group so a kill reaches its grandchildren too. `claude`
/// spawns MCP servers and `codex`/`gemini` spawn helpers; killing only the direct child leaves
/// those holding our stdout pipe open, and the drain thread — and therefore Stop — then blocks
/// until they happen to exit. No-op off Unix.
fn isolate_process_group(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

/// Kill the delegate and everything it spawned, then reap it. Best effort: a group that is
/// already gone is not an error.
fn kill_tree(child: &mut std::process::Child) {
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGKILL the delegate's process group: what it spawned, and the delegate itself while it is
/// still running. Best effort; a group that is already gone is not an error. No-op off Unix.
fn kill_group(pgid: u32) {
    #[cfg(unix)]
    {
        // The child is its own group leader (see `isolate_process_group`), so -pid addresses
        // the whole group. Sign-flip of a u32 pid always fits: pids are < 2^31.
        let pgid = pgid as i32;
        if pgid > 0 {
            // SAFETY: plain syscall with a value we own; no memory is shared.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

impl ProcessRunner {
    fn run_inner(
        &self,
        spec: &CommandSpec,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<CommandOutput, RunError> {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in &spec.env_remove {
            cmd.env_remove(name);
        }
        isolate_process_group(&mut cmd);
        let scratch = ScratchCwd::create(&spec.cwd_files)?;
        cmd.current_dir(scratch.path());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunError::NotFound),
            Err(e) => return Err(RunError::Io(redact_secrets(&e.to_string()))),
        };

        // Write the prompt and close stdin, on a thread: a delegate that starts emitting output
        // before it has drained a large prompt would deadlock a same-thread write.
        let writer = spec.stdin.clone().and_then(|body| {
            child.stdin.take().map(|mut pipe| {
                std::thread::spawn(move || {
                    let _ = pipe.write_all(body.as_bytes());
                    // Dropping `pipe` closes stdin, which is how the delegate knows the prompt ended.
                })
            })
        });
        if spec.stdin.is_none() {
            drop(child.stdin.take());
        }

        // Drain both pipes concurrently. A single-pipe read would block forever the moment the
        // other filled its buffer.
        let out_reader = child.stdout.take().map(|mut p| {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = p.read_to_string(&mut s);
                s
            })
        });
        let err_reader = child.stderr.take().map(|mut p| {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = p.read_to_string(&mut s);
                s
            })
        });

        let deadline = std::time::Instant::now() + spec.timeout;
        let mut timed_out = false;
        let mut was_cancelled = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if cancelled.is_some_and(|flag| {
                        flag.load(std::sync::atomic::Ordering::Acquire)
                    }) {
                        kill_tree(&mut child);
                        was_cancelled = true;
                        break None;
                    }
                    if std::time::Instant::now() >= deadline {
                        kill_tree(&mut child);
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(RunError::Io(redact_secrets(&e.to_string()))),
            }
        };

        // Bounded, like the streaming runner: something the delegate spawned may still hold a
        // pipe after it exited (or after the kill, if it left the group).
        let pipes = PipeThreads { writer, stdout: out_reader, stderr: err_reader };
        let (stdout, stderr) = pipes.collect(child.id(), std::time::Instant::now() + POST_EXIT_DRAIN);
        let (stdout, stderr) = (stdout.unwrap_or_default(), stderr.unwrap_or_default());

        if timed_out {
            return Err(RunError::Timeout);
        }
        if was_cancelled {
            return Err(RunError::Cancelled);
        }
        Ok(CommandOutput { code: status.and_then(|s| s.code()), stdout, stderr })
    }

}

/// A scripted runner for tests: records every [`CommandSpec`] it is handed and replies from a queue
/// of canned outcomes. Public so the desktop crate can test its wiring without a vendor CLI.
pub struct MockRunner {
    seen: std::sync::Mutex<Vec<CommandSpec>>,
    replies: std::sync::Mutex<std::collections::VecDeque<Result<CommandOutput, RunError>>>,
    fallback: Result<CommandOutput, RunError>,
}

impl MockRunner {
    /// A runner that replies with `replies` in order, then repeats `fallback` forever.
    pub fn new(
        replies: Vec<Result<CommandOutput, RunError>>,
        fallback: Result<CommandOutput, RunError>,
    ) -> Self {
        Self {
            seen: std::sync::Mutex::new(Vec::new()),
            replies: std::sync::Mutex::new(replies.into()),
            fallback,
        }
    }

    /// A runner that always answers `out`.
    pub fn always(out: Result<CommandOutput, RunError>) -> Self {
        Self::new(Vec::new(), out)
    }

    /// Every spec this runner was asked to execute, in order.
    pub fn seen(&self) -> Vec<CommandSpec> {
        self.seen.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunError> {
        if let Ok(mut g) = self.seen.lock() {
            g.push(spec.clone());
        }
        self.replies
            .lock()
            .ok()
            .and_then(|mut g| g.pop_front())
            .unwrap_or_else(|| self.fallback.clone())
    }
}

/// Shorthand for a successful run.
pub fn out_ok(stdout: &str) -> Result<CommandOutput, RunError> {
    Ok(CommandOutput { code: Some(0), stdout: stdout.to_string(), stderr: String::new() })
}

/// Shorthand for a failed run carrying `stderr`.
pub fn out_err(code: i32, stderr: &str) -> Result<CommandOutput, RunError> {
    Ok(CommandOutput { code: Some(code), stdout: String::new(), stderr: stderr.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::RecordingSink;

    #[test]
    fn codex_is_outside_slo03_and_the_streaming_delegates_are_in() {
        assert!(Delegate::ClaudeCode.in_slo03());
        assert!(Delegate::GeminiCli.in_slo03());
        assert!(!Delegate::Codex.in_slo03());
    }

    // non-goals: the guarantees that make this approach legitimate

    #[test]
    fn no_invocation_ever_touches_a_credential_store() {
        // The whole design rests on SHOGUN never lifting a vendor's stored token. Every command
        // this module can construct is checked against the paths a credential lift would need.
        const FORBIDDEN: &[&str] = &[
            ".credentials",
            "auth.json",
            "keychain",
            "security find-generic-password",
            "oauth",
            "access_token",
            "refresh_token",
        ];
        let mut specs = Vec::new();
        let opts = DelegateOptions { model: Some("some-model".into()), reasoning_effort: Some(Effort::High), ..Default::default() };
        for d in Delegate::ALL {
            specs.push(detect_spec(d));
            specs.push(completion_spec(d, "hello"));
            specs.push(streaming_completion_spec(d, "hello"));
            specs.push(completion_spec_with(d, "hello", &opts));
            specs.push(streaming_completion_spec_with(d, "hello", &opts));
        }
        for d in BridgedDelegate::ALL {
            specs.push(bridged_spec(d, "hello", &bridge_launch(), &opts));
        }
        for spec in specs {
            let line = format!("{} {}", spec.program, spec.args.join(" ")).to_lowercase();
            for bad in FORBIDDEN {
                assert!(!line.contains(bad), "invocation reaches for a credential: {line}");
            }
        }
    }

    #[test]
    fn the_prompt_travels_on_stdin_never_in_argv() {
        // argv is readable by every process on the machine via `ps`. Captured user text in there
        // would be a privacy breach that no amount of care at the call sites could fix.
        let secret = "quarterly numbers are down 40%";
        for d in Delegate::ALL {
            let bridged = BridgedDelegate::of(d).map(|b| bridged_spec(b, secret, &bridge_launch(), &DelegateOptions::default()));
            for spec in [completion_spec(d, secret), streaming_completion_spec(d, secret)].into_iter().chain(bridged) {
                assert_eq!(spec.stdin.as_deref(), Some(secret), "{} must take the prompt on stdin", d.id());
                for arg in &spec.args {
                    assert!(!arg.contains("quarterly"), "{} leaked the prompt into argv", d.id());
                }
            }
        }
    }

    /// The value following `flag` in `args`, for every occurrence of `flag`.
    fn values_after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == flag)
            .filter_map(|(i, _)| args.get(i + 1).map(String::as_str))
            .collect()
    }

    #[test]
    fn delegates_run_without_tools_or_mcp_servers() {
        // The prompt is untrusted captured text. A delegate with its tool surface enabled would
        // let an instruction in a mail body or on screen call the user's own MCP servers (or read
        // files, or run a shell) outside SHOGUN's L1/L2/L3 engine — so EVERY delegate, in both
        // invocation shapes and with or without options, must carry its containment (#782).
        // The `match` is exhaustive on purpose: a new delegate does not compile until it has an
        // arm here saying how its MCP servers and tools are switched off.
        let with_opts = DelegateOptions { model: Some("m".into()), reasoning_effort: Some(Effort::Low), ..Default::default() };
        for d in Delegate::ALL {
            let contain = d.containment_args();
            assert!(!contain.is_empty(), "{} has no containment flags", d.id());
            let specs = [
                completion_spec(d, "x"),
                streaming_completion_spec(d, "x"),
                completion_spec_with(d, "x", &with_opts),
                streaming_completion_spec_with(d, "x", &with_opts),
            ];
            for spec in specs {
                let a = &spec.args;
                assert!(
                    a.windows(contain.len()).any(|w| w == contain.as_slice()),
                    "{}: containment flags missing from {a:?}",
                    d.id()
                );
                assert_eq!(spec.cwd_files, d.containment_files(), "{}: containment files missing", d.id());
                match d {
                    Delegate::ClaudeCode => {
                        assert_eq!(values_after(a, "--tools"), [""], "built-in tools must be disabled");
                        assert!(a.iter().any(|s| s == "--strict-mcp-config"), "user MCP config must not load");
                        let cfg = values_after(a, "--mcp-config");
                        assert_eq!(cfg.len(), 1);
                        let cfg: serde_json::Value = serde_json::from_str(cfg[0]).expect("inline JSON");
                        assert_eq!(cfg["mcpServers"], serde_json::json!({}), "MCP server set must be empty");
                        assert!(a.iter().any(|s| s == "--no-session-persistence"));
                    }
                    Delegate::Codex => {
                        assert_eq!(values_after(a, "--sandbox"), ["read-only"]);
                        for flag in ["--skip-git-repo-check", "--ignore-user-config", "--ignore-rules", "--ephemeral"] {
                            assert!(a.iter().any(|s| s == flag), "codex must pass {flag}");
                        }
                        let overrides = values_after(a, "-c");
                        for o in [
                            "features.apps=false",
                            "features.plugins=false",
                            "features.shell_tool=false",
                            r#"web_search="disabled""#,
                        ] {
                            assert!(overrides.contains(&o), "codex must pass -c {o}: {overrides:?}");
                        }
                        // `-c mcp_servers={}` would be deep-merged over config.toml and remove
                        // nothing; the user's servers go only with the whole user config.
                        assert!(!overrides.iter().any(|o| o.starts_with("mcp_servers")), "{overrides:?}");
                        // The flags belong to `codex exec`: they must follow the subcommand.
                        assert_eq!(a[0], "exec");
                    }
                    Delegate::GeminiCli => {
                        assert_eq!(values_after(a, "--allowed-mcp-server-names"), [""], "no MCP server may be allowed");
                        assert_eq!(values_after(a, "--extensions"), ["none"], "extensions must be off");
                        assert_eq!(values_after(a, "--admin-policy"), [GEMINI_CONTAINMENT_POLICY_FILE]);
                        assert_eq!(values_after(a, "--policy"), [GEMINI_CONTAINMENT_POLICY_FILE]);
                        assert_eq!(
                            spec.cwd_files,
                            vec![(GEMINI_CONTAINMENT_POLICY_FILE.to_string(), GEMINI_CONTAINMENT_POLICY.to_string())]
                        );
                        let rule: Vec<&str> = GEMINI_CONTAINMENT_POLICY
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                            .collect();
                        assert_eq!(rule, ["[[rule]]", r#"toolName = "*""#, r#"decision = "deny""#, "priority = 999"]);
                    }
                }
            }
        }
        // The version probe needs nothing written anywhere.
        for d in Delegate::ALL {
            assert!(detect_spec(d).cwd_files.is_empty());
        }
    }

    fn bridge_launch() -> ChatBridgeLaunch {
        ChatBridgeLaunch {
            server: "/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp".into(),
            socket: "/tmp/b/abc.sock".into(),
            nonce: "n0nce".into(),
        }
    }

    /// Owner decision 2026-10-14: SHOGUN's chat contract reaches the delegate's own system role,
    /// appended to the CLI's default prompt — never replacing it, never per-user text in argv.
    #[test]
    fn a_bridged_run_appends_shoguns_chat_contract_to_the_system_role() {
        let launch = bridge_launch();
        let contract = crate::llm::prompt_cache::delegated_system_text();
        let opts = DelegateOptions { shogun_system: true, ..Default::default() };
        // Claude Code: the append-file form, the file in the scratch cwd, no text in argv.
        let spec = claude_code_bridged_spec("secret prompt", &launch, &opts);
        assert_eq!(values_after(&spec.args, "--append-system-prompt-file"), [CHAT_SYSTEM_PROMPT_FILE]);
        for bad in ["--system-prompt", "--system-prompt-file", "--append-system-prompt"] {
            assert!(!spec.args.iter().any(|s| s == bad), "{bad} in {:?}", spec.args);
        }
        assert!(spec.args.iter().all(|s| !s.contains("SHOGUN")), "{:?}", spec.args);
        let file: Vec<_> = spec.cwd_files.iter().filter(|(n, _)| n == CHAT_SYSTEM_PROMPT_FILE).collect();
        assert_eq!(file.len(), 1);
        assert_eq!(file[0].1, contract);
        // The rest of the run is the contained bridged run, flag for flag.
        let without = claude_code_bridged_spec("secret prompt", &launch, &DelegateOptions::default());
        let mut expected = without.args.clone();
        expected.extend(["--append-system-prompt-file".to_string(), CHAT_SYSTEM_PROMPT_FILE.to_string()]);
        assert_eq!(spec.args, expected);
        assert_eq!(spec.stdin, without.stdin);
        // Codex: one `-c developer_instructions` override carrying the constant contract as TOML.
        let spec = codex_bridged_spec("secret prompt", &launch, &opts);
        let overrides = values_after(&spec.args, "-c");
        let dev: Vec<&str> = overrides.iter().copied().filter(|v| v.starts_with("developer_instructions=")).collect();
        assert_eq!(dev.len(), 1, "{overrides:?}");
        let value: String = serde_json::from_str(dev[0].trim_start_matches("developer_instructions=")).expect("quoted");
        assert_eq!(value, contract);
        assert!(!spec.args.iter().any(|s| s.contains("model_instructions_file") || s == "--profile"));
        assert!(!spec.args.iter().any(|s| s.contains("secret prompt")));
        let without = codex_bridged_spec("secret prompt", &launch, &DelegateOptions::default());
        assert!(!without.args.iter().any(|s| s.starts_with("developer_instructions=")));
        assert!(!without.cwd_files.iter().any(|(n, _)| n == CHAT_SYSTEM_PROMPT_FILE));
        // The contract is SHOGUN's own text: no clock, nothing a user could have typed.
        assert!(contract.contains(crate::llm::prompt_cache::CHAT_SYSTEM_PREAMBLE));
        assert!(contract.contains("shogun_chat"));
    }

    #[test]
    fn a_bridged_claude_code_run_adds_only_shoguns_chat_bridge() {
        // Issue #484: the one change from the contained run is SHOGUN's own MCP server. Built-in
        // tools stay off, the user's MCP config stays out, the session stays unsaved, and the
        // permission rule allows that one server's tools and nothing else.
        let launch = bridge_launch();
        for opts in [DelegateOptions::default(), DelegateOptions { claude_code_effort: Some(Effort::Medium), ..Default::default() }] {
            let spec = claude_code_bridged_spec("secret prompt", &launch, &opts);
            let a = &spec.args;
            assert_eq!(spec.program, "claude");
            assert_eq!(spec.stdin.as_deref(), Some("secret prompt"));
            assert!(a.iter().all(|s| !s.contains("secret")), "prompt stays off argv");
            assert_eq!(values_after(a, "--tools"), [""], "built-in tools must stay disabled");
            assert!(a.iter().any(|s| s == "--strict-mcp-config"), "user MCP config must not load");
            assert!(a.iter().any(|s| s == "--no-session-persistence"));
            assert_eq!(values_after(a, "--allowedTools"), ["mcp__shogun_chat"]);
            assert_eq!(a.iter().filter(|s| s.starts_with("--allowed")).count(), 1);
            // The config travels as a file in the scratch cwd: no nonce or socket on argv.
            assert_eq!(values_after(a, "--mcp-config"), [CHAT_BRIDGE_MCP_CONFIG_FILE]);
            assert!(a.iter().all(|s| !s.contains(&launch.nonce) && !s.contains(&launch.socket)), "{a:?}");
            let files: Vec<&(String, String)> =
                spec.cwd_files.iter().filter(|(n, _)| n == CHAT_BRIDGE_MCP_CONFIG_FILE).collect();
            assert_eq!(files.len(), 1);
            let cfg: serde_json::Value = serde_json::from_str(&files[0].1).expect("config JSON");
            let servers = cfg["mcpServers"].as_object().expect("server map");
            assert_eq!(servers.len(), 1, "exactly one MCP server");
            let server = &servers["shogun_chat"];
            assert_eq!(server["type"], "stdio");
            assert_eq!(server["command"], launch.server.as_str());
            assert_eq!(server["args"], serde_json::json!(["--chat-bridge"]));
            assert_eq!(server["env"]["SHOGUN_CHAT_BRIDGE_SOCKET"], launch.socket.as_str());
            assert_eq!(server["env"]["SHOGUN_CHAT_BRIDGE_NONCE"], launch.nonce.as_str());
            assert_eq!(server.as_object().map(|o| o.len()), Some(4), "{server}");
            // No flag that widens permissions or the file surface.
            for bad in [
                "--dangerously-skip-permissions",
                "--allow-dangerously-skip-permissions",
                "--permission-mode",
                "--add-dir",
                "--disallowedTools",
                "--settings",
                "--plugin-dir",
                "--agents",
                "--chrome",
                "--ide",
                "--resume",
                "--continue",
                "--system-prompt",
                "--system-prompt-file",
            ] {
                assert!(!a.iter().any(|s| s == bad || s.starts_with(&format!("{bad}="))), "{bad} in {a:?}");
            }
            // Everything else is the contained streaming run, flag for flag.
            let plain = streaming_completion_spec_with(Delegate::ClaudeCode, "secret prompt", &opts);
            let strip = |v: &[String]| -> Vec<String> {
                let mut out = Vec::new();
                let mut skip = false;
                for s in v {
                    if skip { skip = false; continue; }
                    if s == "--mcp-config" || s == "--allowedTools" { skip = true; continue; }
                    out.push(s.clone());
                }
                out
            };
            assert_eq!(strip(a), strip(&plain.args));
            assert_eq!(spec.env_remove, plain.env_remove);
            let others: Vec<_> =
                spec.cwd_files.iter().filter(|(n, _)| n != CHAT_BRIDGE_MCP_CONFIG_FILE).cloned().collect();
            assert_eq!(others, plain.cwd_files);
            assert_eq!(spec.timeout, plain.timeout);
        }
        // The plain contained run is untouched by the bridge's existence.
        let plain = streaming_completion_spec(Delegate::ClaudeCode, "x");
        assert!(!plain.args.iter().any(|s| s == "--allowedTools"));
    }

    /// The `-c` overrides of a Codex invocation that touch `mcp_servers`.
    fn codex_mcp_overrides(args: &[String]) -> Vec<&str> {
        values_after(args, "-c").into_iter().filter(|o| o.starts_with("mcp_servers")).collect()
    }

    #[test]
    fn a_bridged_codex_run_adds_only_shoguns_chat_bridge() {
        // Issue #484: every containment flag of the plain Codex run stays; the only
        // addition is SHOGUN's one stdio server and the approval mode for its tools.
        let launch = bridge_launch();
        let with_opts = DelegateOptions { model: Some("m".into()), reasoning_effort: Some(Effort::Low), ..Default::default() };
        for opts in [DelegateOptions::default(), with_opts] {
            let spec = codex_bridged_spec("secret prompt", &launch, &opts);
            let a = &spec.args;
            assert_eq!(spec.program, "codex");
            assert_eq!(&a[..3], ["exec", "--json", "-"]);
            assert_eq!(spec.stdin.as_deref(), Some("secret prompt"));
            assert!(a.iter().all(|s| !s.contains("secret")), "prompt stays off argv");
            // The contained run's flags, contiguous and unchanged.
            let contain = Delegate::Codex.containment_args();
            assert!(a.windows(contain.len()).any(|w| w == contain.as_slice()), "{a:?}");
            assert_eq!(values_after(a, "--sandbox"), ["read-only"]);
            // Exactly one server, three keys, nothing else under `mcp_servers`.
            let server_toml = serde_json::Value::from(launch.server.as_str()).to_string();
            assert_eq!(
                codex_mcp_overrides(a),
                [
                    format!("mcp_servers.shogun_chat.command={server_toml}").as_str(),
                    r#"mcp_servers.shogun_chat.args=["--chat-bridge","--chat-bridge-config","shogun-chat-bridge.json"]"#,
                    r#"mcp_servers.shogun_chat.default_tools_approval_mode="approve""#,
                ]
            );
            // No socket or nonce on argv: they are only in the bridge file.
            assert!(a.iter().all(|s| !s.contains(&launch.nonce) && !s.contains(&launch.socket)), "{a:?}");
            let files: Vec<&(String, String)> =
                spec.cwd_files.iter().filter(|(n, _)| n == CHAT_BRIDGE_CONFIG_FILE).collect();
            assert_eq!(files.len(), 1);
            let cfg: serde_json::Value = serde_json::from_str(&files[0].1).expect("bridge file JSON");
            assert_eq!(cfg, serde_json::json!({ "socket": launch.socket, "nonce": launch.nonce }));
            // No flag or override that widens approvals, the sandbox, the config or the tools.
            for bad in [
                "--dangerously-bypass-approvals-and-sandbox",
                "--full-auto",
                "--yolo",
                "--ask-for-approval",
                "-a",
                "--add-dir",
                "--profile",
                "-p",
                "--cd",
                "-C",
                "--enable",
                "--search",
                "--image",
                "--oss",
            ] {
                assert!(!a.iter().any(|s| s == bad || s.starts_with(&format!("{bad}="))), "{bad} in {a:?}");
            }
            for o in values_after(a, "-c") {
                for widen in ["approval_policy", "sandbox", "features.", "web_search", "tools.", "profile", "projects", "mcp_servers.shogun_chat.env", "mcp_servers.shogun_chat.cwd"] {
                    let contained = Delegate::Codex.containment_args();
                    let ours = contained.iter().any(|c| c == o);
                    assert!(ours || !o.starts_with(widen), "-c {o} widens the run");
                }
            }
            // Everything but the three overrides is the contained `--json` run, flag for flag.
            let plain = streaming_completion_spec_with(Delegate::Codex, "secret prompt", &opts);
            let strip = |v: &[String]| -> Vec<String> {
                let mut out = Vec::new();
                let mut i = 0;
                while i < v.len() {
                    if v[i] == "-c" && v.get(i + 1).is_some_and(|o| o.starts_with("mcp_servers.")) {
                        i += 2;
                        continue;
                    }
                    out.push(v[i].clone());
                    i += 1;
                }
                out
            };
            assert_eq!(strip(a), plain.args);
            assert_eq!(spec.env_remove, plain.env_remove);
            let others: Vec<_> = spec.cwd_files.iter().filter(|(n, _)| n != CHAT_BRIDGE_CONFIG_FILE).cloned().collect();
            assert_eq!(others, plain.cwd_files);
            assert_eq!(spec.timeout, plain.timeout);
        }
        // A server path that needs escaping still parses as one TOML string.
        let odd = ChatBridgeLaunch { server: r#"/Apps/My "Shogun"\x.app/shogun-mcp"#.into(), ..bridge_launch() };
        let spec = codex_bridged_spec("p", &odd, &DelegateOptions::default());
        let command = codex_mcp_overrides(&spec.args)[0].strip_prefix("mcp_servers.shogun_chat.command=").unwrap_or_default();
        assert_eq!(serde_json::from_str::<String>(command).ok().as_deref(), Some(odd.server.as_str()));
        // The plain contained run is untouched by the bridge's existence.
        assert!(codex_mcp_overrides(&streaming_completion_spec(Delegate::Codex, "x").args).is_empty());
    }

    #[test]
    fn every_bridged_spec_admits_one_server_and_only_bridged_delegates_have_one() {
        // The Gemini CLI has no bridged form (see `BridgedDelegate`); the others map both ways.
        assert_eq!(BridgedDelegate::of(Delegate::GeminiCli), None);
        for d in BridgedDelegate::ALL {
            assert_eq!(BridgedDelegate::of(d.delegate()), Some(d));
            let launch = bridge_launch();
            let spec = bridged_spec(d, "p", &launch, &DelegateOptions::default());
            assert_eq!(spec.program, d.delegate().binary());
            // The nonce and socket are in exactly one SHOGUN file, never on argv.
            let carrying: Vec<&String> =
                spec.cwd_files.iter().filter(|(_, c)| c.contains(&launch.nonce)).map(|(n, _)| n).collect();
            assert_eq!(carrying.len(), 1, "{}", d.delegate().id());
            assert!(spec.args.iter().all(|s| !s.contains(&launch.nonce) && !s.contains(&launch.socket)));
            // The contained run's containment files are still there.
            for f in d.delegate().containment_files() {
                assert!(spec.cwd_files.contains(&f));
            }
            match d {
                BridgedDelegate::ClaudeCode => {
                    assert_eq!(spec, claude_code_bridged_spec("p", &launch, &DelegateOptions::default()));
                }
                BridgedDelegate::Codex => {
                    assert_eq!(spec, codex_bridged_spec("p", &launch, &DelegateOptions::default()));
                    assert_eq!(codex_mcp_overrides(&spec.args).len(), 3);
                    assert!(codex_mcp_overrides(&spec.args).iter().all(|o| o.starts_with("mcp_servers.shogun_chat.")));
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_real_runner_writes_containment_files_into_the_scratch_cwd() {
        // The Gemini policy is relative to the child's cwd, so it must be there when the child
        // starts — and gone with the scratch directory afterwards.
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), format!("pwd; ls -A | wc -l; cat {GEMINI_CONTAINMENT_POLICY_FILE}")],
            stdin: None,
            env_remove: vec![],
            timeout: Duration::from_secs(10),
            cwd_files: Delegate::GeminiCli.containment_files(),
        };
        let out = ProcessRunner.run(&spec).unwrap();
        assert!(out.ok(), "{out:?}");
        let mut lines = out.stdout.lines();
        let cwd = lines.next().unwrap_or_default().to_string();
        assert!(cwd.contains("shogun-delegate-"), "unexpected cwd {cwd}");
        assert_eq!(lines.next().unwrap_or_default().trim(), "1", "only the containment file");
        assert_eq!(lines.collect::<Vec<_>>().join("\n") + "\n", GEMINI_CONTAINMENT_POLICY);
        assert!(!std::path::Path::new(&cwd).exists(), "scratch cwd must be removed after the run");
    }

    #[cfg(unix)]
    #[test]
    fn a_bridged_runs_config_is_a_private_file_in_a_private_cwd() {
        // Issue #484: the file carries the run's nonce, so only this user may read it or list
        // the directory, and both are gone after the run — for every bridged delegate.
        let launch = bridge_launch();
        let opts = DelegateOptions::default();
        for (bridged, file, contents) in [
            (claude_code_bridged_spec("p", &launch, &opts), CHAT_BRIDGE_MCP_CONFIG_FILE, launch.mcp_config()),
            (codex_bridged_spec("p", &launch, &opts), CHAT_BRIDGE_CONFIG_FILE, launch.bridge_config()),
        ] {
            let spec = CommandSpec {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    format!(
                        "pwd; stat -c %a . 2>/dev/null || stat -f %Lp .; \
                         stat -c %a {file} 2>/dev/null || stat -f %Lp {file}; cat {file}"
                    ),
                ],
                stdin: None,
                env_remove: vec![],
                timeout: Duration::from_secs(10),
                cwd_files: bridged.cwd_files.clone(),
            };
            let out = ProcessRunner.run(&spec).unwrap();
            assert!(out.ok(), "{out:?}");
            let mut lines = out.stdout.lines();
            let cwd = lines.next().unwrap_or_default().to_string();
            assert_eq!(lines.next().unwrap_or_default().trim(), "700", "scratch cwd mode");
            assert_eq!(lines.next().unwrap_or_default().trim(), "600", "{file} mode");
            assert_eq!(lines.collect::<Vec<_>>().join("\n"), contents);
            assert!(!std::path::Path::new(&cwd).exists(), "scratch cwd must be removed after the run");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_containment_file_that_cannot_be_placed_stops_the_run() {
        // Anything but a plain file name could land outside the scratch directory. The child
        // must then not start at all — a delegate without its containment does not run.
        let marker = std::env::temp_dir().join(format!("shogun-contain-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        for name in ["../escape.toml", "sub/dir.toml", "..", ".", ""] {
            for streaming in [false, true] {
                let spec = CommandSpec {
                    program: "sh".into(),
                    args: vec!["-c".into(), format!("touch {}", marker.display())],
                    stdin: None,
                    env_remove: vec![],
                    timeout: Duration::from_secs(10),
                    cwd_files: vec![(name.to_string(), "x".to_string())],
                };
                let got = if streaming {
                    ProcessRunner.run_streaming(&spec, &mut |_: &str| true)
                } else {
                    ProcessRunner.run(&spec)
                };
                assert!(matches!(got, Err(RunError::Io(_))), "{name:?}: {got:?}");
                assert!(!marker.exists(), "{name:?}: the child ran without its containment");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_real_runner_gives_the_child_an_empty_scratch_cwd() {
        // Whatever launched SHOGUN, the delegate must not inherit a cwd that a CLI would treat
        // as a project it may read.
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "pwd; ls -A | wc -l".into()],
            stdin: None,
            env_remove: vec![],
            timeout: Duration::from_secs(10),
            cwd_files: Vec::new(),
        };
        let out = ProcessRunner.run(&spec).unwrap();
        let mut lines = out.stdout.lines();
        let cwd = lines.next().unwrap_or_default();
        let entries = lines.next().unwrap_or_default().trim().to_string();
        assert!(cwd.contains("shogun-delegate-"), "unexpected cwd {cwd}");
        assert_eq!(entries, "0", "scratch cwd must be empty");
        assert!(!std::path::Path::new(cwd).exists(), "scratch cwd must be removed after the run");
    }

    #[test]
    fn our_own_api_keys_are_scrubbed_from_every_child() {
        // Left in place, a delegate would authenticate with SHOGUN's metered key and quietly bill
        // the user per token — the exact outcome they chose a subscription to avoid.
        for d in Delegate::ALL {
            let bridged = BridgedDelegate::of(d).map(|b| bridged_spec(b, "x", &bridge_launch(), &DelegateOptions::default()));
            for spec in [detect_spec(d), completion_spec(d, "x"), streaming_completion_spec(d, "x")].into_iter().chain(bridged) {
                assert!(spec.env_remove.iter().any(|e| e == "ANTHROPIC_API_KEY"));
                assert!(spec.env_remove.iter().any(|e| e == "OPENAI_API_KEY"));
                assert!(spec.env_remove.iter().any(|e| e == "CLAUDE_CODE_OAUTH_TOKEN"));
                assert!(spec.env_remove.iter().any(|e| e == "CODEX_API_KEY"));
            }
        }
    }

    #[test]
    fn delegate_ids_roundtrip_and_are_distinct() {
        let mut ids: Vec<&str> = Delegate::ALL.iter().map(|d| d.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Delegate::ALL.len(), "delegate ids must be unique");
        for d in Delegate::ALL {
            assert_eq!(Delegate::parse(d.id()), Some(d));
        }
        assert_eq!(Delegate::parse("gpt-5-cli"), None);
    }

    #[test]
    fn destination_names_the_vendor_and_the_local_hop() {
        // The traceability screen answers "who saw this". Naming only the binary would hide the
        // vendor; naming only the vendor would hide that it left through another process.
        let d = Delegate::ClaudeCode.destination();
        assert!(d.contains("api.anthropic.com"), "{d}");
        assert!(d.contains("claude"), "{d}");
    }

    #[test]
    fn a_missing_binary_is_not_installed() {
        let runner = MockRunner::always(Err(RunError::NotFound));
        assert_eq!(detect(&runner, Delegate::Codex), DelegateState::NotInstalled);
    }

    #[test]
    fn detection_reads_the_version_but_never_claims_ready() {
        // Presence is not sign-in. Promoting "installed" to "ready" would end onboarding on a
        // success screen and fail on the user's first real action.
        let runner = MockRunner::always(out_ok("1.2.3 (Claude Code)\n"));
        let state = detect(&runner, Delegate::ClaudeCode);
        assert_eq!(state, DelegateState::Installed { version: "1.2.3 (Claude Code)".into() });
        assert!(!state.is_usable());
        assert!(state.is_installed());
    }

    #[test]
    fn detect_all_covers_every_delegate() {
        let runner = MockRunner::always(Err(RunError::NotFound));
        let all = detect_all(&runner);
        assert_eq!(all.len(), Delegate::ALL.len());
        assert!(all.values().all(|s| !s.is_installed()));
    }

    #[test]
    fn a_credential_shaped_version_string_is_redacted() {
        // Version output is vendor text that lands in a log line; it must not carry anything
        // credential-shaped through (invariant 7).
        let fake = format!("sk-{}", "a".repeat(40));
        let runner = MockRunner::always(out_ok(&format!("1.0.0 {fake}")));
        let state = detect(&runner, Delegate::ClaudeCode);
        assert!(!format!("{state:?}").contains(&fake), "{state:?}");
    }

    #[test]
    fn verify_promotes_to_ready_only_after_a_real_completion() {
        let runner = MockRunner::new(vec![out_ok("2.0.0"), out_ok("ok")], out_ok(""));
        assert_eq!(verify(&runner, Delegate::ClaudeCode), DelegateState::Ready { version: "2.0.0".into() });
    }

    #[test]
    fn verify_reports_needs_login_without_reading_credentials() {
        let runner = MockRunner::new(
            vec![out_ok("2.0.0"), out_err(1, "Error: not logged in. Run `claude` to sign in.")],
            out_ok(""),
        );
        assert_eq!(verify(&runner, Delegate::ClaudeCode), DelegateState::NeedsLogin { version: "2.0.0".into() });
    }

    #[test]
    fn verify_reports_rate_limited_separately_from_needs_login() {
        let runner = MockRunner::new(
            vec![out_ok("2.0.0"), out_err(1, "usage limit reached; resets at 5pm")],
            out_ok(""),
        );
        assert_eq!(verify(&runner, Delegate::ClaudeCode), DelegateState::RateLimited { version: "2.0.0".into() });
    }

    #[test]
    fn verify_of_a_missing_binary_stays_not_installed() {
        let runner = MockRunner::always(Err(RunError::NotFound));
        assert_eq!(verify(&runner, Delegate::GeminiCli), DelegateState::NotInstalled);
    }

    #[test]
    fn quota_messages_beat_login_messages() {
        // Vendors word limit errors as "usage limit reached — sign in to upgrade". Reading that as
        // an auth failure sends the user to re-authenticate an account that works fine.
        let out = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "Usage limit reached. Sign in to your account to upgrade.".into(),
        };
        assert_eq!(classify(&out), DelegateFailure::RateLimited);
    }

    #[test]
    fn a_401_is_a_login_problem() {
        let out = CommandOutput { code: Some(1), stdout: String::new(), stderr: "request failed: HTTP 401".into() };
        assert_eq!(classify(&out), DelegateFailure::NeedsLogin);
    }

    #[test]
    fn an_unrecognised_failure_keeps_the_delegates_own_words() {
        let out = out_err(2, "model overloaded, try again").unwrap();
        match classify(&out) {
            DelegateFailure::Failed(r) => assert!(r.contains("overloaded"), "{r}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_quoting_a_credential_is_redacted() {
        let fake = format!("sk-ant-{}", "z".repeat(40));
        let out = out_err(1, &format!("bad request with {fake}")).unwrap();
        let shown = format!("{:?}", classify(&out));
        assert!(!shown.contains(&fake), "credential survived classification: {shown}");
    }

    #[test]
    fn rate_limit_error_names_the_plan_and_does_not_blame_shogun() {
        let e = DelegateFailure::RateLimited.into_llm_error(Delegate::ClaudeCode);
        assert!(matches!(e, LlmError::QuotaExhausted(_)));
        let msg = e.to_string();
        assert!(msg.contains("Claude Pro / Max"), "{msg}");
        assert!(!msg.to_lowercase().contains("shogun"), "{msg}");
        // The message must offer a way forward the user controls…
        assert!(msg.contains("API key"), "{msg}");
        // …and must not promise a recovery horizon (see the module docs).
        for banned in ["try again", "monthly", "month", "5 hours", "five hours"] {
            assert!(!msg.to_lowercase().contains(banned), "{msg}");
        }
    }

    #[test]
    fn needs_login_maps_onto_the_existing_401_path() {
        // So the sticky "credential rejected" indicator and the settings prompt light up unchanged.
        let e = DelegateFailure::NeedsLogin.into_llm_error(Delegate::Codex);
        assert!(matches!(e, LlmError::Unauthorized(401, _)), "{e:?}");
    }

    #[test]
    fn a_successful_completion_returns_the_trimmed_answer() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("  Sounds good — shipping Friday.\n")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        assert_eq!(client.complete("draft a reply").unwrap(), "Sounds good — shipping Friday.");
    }

    #[test]
    fn every_completion_writes_exactly_one_trace_with_no_text() {
        let prompt = "reply about the acquisition of Northwind";
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("done")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        client.complete(prompt).unwrap();
        let recs = client.sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].route, Route::LocalAgent);
        assert_eq!(recs[0].chunk_bytes, prompt.len());
        assert!(!recs[0].third_party, "the user's own plan is not a third-party relay");
        assert!(!format!("{:?}", recs[0]).contains("Northwind"), "prompt text reached the trace");
    }

    /// FR-HN-01 / #313: CLI は usage を返さないので、行はバイト長推定（estimated 印付き）。
    /// model 列には委譲先バイナリ名が入る — 月次委譲メータ（FR-HN-08）の集計キーになる。
    #[test]
    fn a_completion_records_one_estimated_usage_row() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        let usage_sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("done")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        )
        .with_usage(UsageHook::new(usage_sink.clone(), prompts::INLINE_DRAFT));
        client.complete("draft a reply").unwrap();

        let recs = usage_sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].route, Route::LocalAgent);
        assert_eq!(recs[0].model, "claude");
        assert!(recs[0].usage.estimated);
        assert!(recs[0].usage.input_tokens > 0);
        assert_eq!(recs[0].outcome, UsageOutcome::Ok);
    }

    #[test]
    fn quota_exhaustion_records_a_quota_outcome() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        let usage_sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_err(1, "you have hit your usage limit")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        )
        .with_usage(UsageHook::new(usage_sink.clone(), prompts::INLINE_DRAFT));
        assert!(client.complete("hello").is_err());
        assert_eq!(usage_sink.records()[0].outcome, UsageOutcome::Quota);
    }

    #[test]
    fn a_failed_run_is_still_traced() {
        // The content left this process before the delegate refused. AR-11 records egress, not
        // success.
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_err(1, "not logged in")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        assert!(client.complete("hello").is_err());
        assert_eq!(client.sink.records().len(), 1);
    }

    #[test]
    fn an_empty_answer_is_an_error_not_an_empty_draft() {
        // Exit 0 with nothing on stdout would otherwise insert an empty draft at the user's caret
        // and report success.
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("   \n")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        assert!(client.complete("hi").is_err());
    }

    #[test]
    fn a_missing_delegate_reports_not_configured() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(Err(RunError::NotFound)),
            RecordingSink::new(),
            Delegate::Codex,
        );
        assert!(matches!(client.complete("hi"), Err(LlmError::NotConfigured)));
    }

    #[test]
    fn a_timeout_is_reported_as_such() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(Err(RunError::Timeout)),
            RecordingSink::new(),
            Delegate::GeminiCli,
        );
        let msg = client.complete("hi").unwrap_err().to_string();
        assert!(msg.contains("in time"), "{msg}");
    }

    #[test]
    fn a_pre_cancelled_completion_never_starts_the_delegate() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("must not run")),
            RecordingSink::new(),
            Delegate::Codex,
        );
        let cancelled = std::sync::atomic::AtomicBool::new(true);

        assert!(client.complete_cancellable("hi", &cancelled).is_err());
        assert!(client.runner.seen().is_empty());
    }

    #[test]
    fn the_client_runs_the_delegate_it_was_built_with() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("x")),
            RecordingSink::new(),
            Delegate::Codex,
        );
        client.complete("hi").unwrap();
        let seen = client.runner.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].program, "codex");
    }

    // streaming (SLO-03 / FR-AG-07)

    /// The stream-json a current `claude -p --output-format stream-json --include-partial-messages`
    /// run emits: init, token deltas, the assistant echo, the final result.
    const CLAUDE_STREAM: &str = concat!(
        r#"{"type":"system","subtype":"init","session_id":"abc"}"#,
        "\n",
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sounds "}}}"#,
        "\n",
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"good."}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Sounds good."}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","result":"Sounds good."}"#,
        "\n",
    );

    fn streamed(client: &SubscriptionAgentClient<MockRunner, RecordingSink>, prompt: &str) -> (Result<String, LlmError>, Vec<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let got = client.complete_streaming(prompt, tx);
        (got, rx.into_iter().collect())
    }

    #[test]
    fn claude_stream_json_yields_token_deltas_without_duplicating_the_result() {
        // The assistant echo and the result both repeat the deltas' text; using them too would
        // paint every token twice.
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok(CLAUDE_STREAM)),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (got, deltas) = streamed(&client, "draft a reply");
        assert_eq!(got.unwrap(), "Sounds good.");
        assert_eq!(deltas, vec!["Sounds ".to_string(), "good.".to_string()]);
        // Streaming asked for the stream-json surface, not text mode.
        let seen = client.runner.seen();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].args.iter().any(|a| a == "stream-json"), "{:?}", seen[0].args);
    }

    #[test]
    fn an_older_cli_with_only_a_result_event_still_answers_once() {
        let only_result = r#"{"type":"result","subtype":"success","result":"Shipping Friday."}"#;
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok(only_result)),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (got, deltas) = streamed(&client, "hi");
        assert_eq!(got.unwrap(), "Shipping Friday.");
        assert_eq!(deltas, vec!["Shipping Friday.".to_string()]);
    }

    #[test]
    fn a_cli_that_rejects_the_streaming_flags_falls_back_to_the_buffered_contract() {
        // First run: the installed CLI predates --include-partial-messages. Second run: the
        // buffered text-mode contract it does support.
        let client = SubscriptionAgentClient::new(
            MockRunner::new(
                vec![out_err(2, "error: unknown option '--include-partial-messages'"), out_ok("Plain answer.")],
                out_ok(""),
            ),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (got, deltas) = streamed(&client, "hi");
        assert_eq!(got.unwrap(), "Plain answer.");
        assert_eq!(deltas, vec!["Plain answer.".to_string()]);
        let seen = client.runner.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].args.iter().any(|a| a == "text"), "{:?}", seen[1].args);
        // Two spawns, two egresses, two traces (AR-11 records sends, not successes).
        assert_eq!(client.sink.records().len(), 2);
    }

    #[test]
    fn a_codex_that_rejects_json_falls_back_to_the_buffered_contract() {
        // A Codex predating `--json`: clap refuses the flag, the buffered `codex exec -` contract
        // answers, and the whole answer goes out as one delta.
        let client = SubscriptionAgentClient::new(
            MockRunner::new(
                vec![out_err(2, "error: unexpected argument '--json' found"), out_ok("Whole answer.\n")],
                out_ok(""),
            ),
            RecordingSink::new(),
            Delegate::Codex,
        );
        let (got, deltas) = streamed(&client, "hi");
        assert_eq!(got.unwrap(), "Whole answer.");
        assert_eq!(deltas, vec!["Whole answer.".to_string()]);
        let seen = client.runner.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].args.iter().any(|a| a == "--json"), "{:?}", seen[0].args);
        assert_eq!(&seen[1].args[..2], ["exec".to_string(), "-".to_string()]);
        assert_eq!(seen[1].args, completion_spec(Delegate::Codex, "hi").args);
        // Two spawns, two egresses, two traces.
        assert_eq!(client.sink.records().len(), 2);
    }

    #[test]
    fn raw_text_streaming_passes_lines_through() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("First line.\nSecond line.\n")),
            RecordingSink::new(),
            Delegate::GeminiCli,
        );
        let (got, deltas) = streamed(&client, "hi");
        assert_eq!(got.unwrap(), "First line.\nSecond line.");
        assert_eq!(deltas, vec!["First line.\n".to_string(), "Second line.\n".to_string()]);
    }

    #[test]
    fn a_streamed_quota_failure_classifies_like_the_buffered_one() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_err(1, "usage limit reached; resets at 5pm")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (got, _) = streamed(&client, "hi");
        assert!(matches!(got, Err(LlmError::QuotaExhausted(_))), "{got:?}");
        // The failed send still left the process — it is still traced.
        assert_eq!(client.sink.records().len(), 1);
    }

    #[test]
    fn a_caller_that_hung_up_gets_what_streamed_and_no_error() {
        // The receiver is dropped before the run: the first delta's send fails, the run stops,
        // and what made it through is returned as a deliberate stop rather than a failure.
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok(CLAUDE_STREAM)),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let got = client.complete_streaming("hi", tx).unwrap();
        assert_eq!(got, "Sounds");
    }

    #[test]
    fn a_flag_already_set_stops_a_streamed_turn_before_it_spawns() {
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok(CLAUDE_STREAM)),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let flag = std::sync::atomic::AtomicBool::new(true);
        let (tx, rx) = std::sync::mpsc::channel();
        let got = client.complete_streaming_cancellable("hi", tx, &flag).unwrap();
        assert_eq!(got, "");
        assert!(rx.into_iter().next().is_none(), "no delta may leak from a withdrawn turn");
        assert!(client.runner.seen().is_empty(), "the delegate must not be spawned");
        assert!(client.sink.records().is_empty(), "nothing was sent, so nothing is traced");
    }

    /// The real runner, with a child that prints one line and then goes quiet for far longer
    /// than the test tolerates. Flipping the flag while it is silent must kill it promptly —
    /// this is the exact shape of Stop pressed while `claude -p` is thinking.
    #[cfg(unix)]
    #[test]
    fn the_real_runner_kills_a_silent_child_when_the_flag_flips() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "echo first; sleep 30; echo late".into()],
            stdin: None,
            env_remove: vec![],
            timeout: Duration::from_secs(60),
            cwd_files: Vec::new(),
        };
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let started = std::time::Instant::now();
        let (seen_cb, flag_cb) = (seen.clone(), flag.clone());
        let run = ProcessRunner.run_streaming_cancellable(
            &spec,
            &mut |line| {
                seen_cb.lock().map(|mut v| v.push(line.to_string())).ok();
                // Simulate Stop arriving right after the first token painted.
                flag_cb.store(true, Ordering::Release);
                true
            },
            &flag,
        );
        assert_eq!(run, Err(RunError::Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancel took {:?}; the child was not killed on the flag",
            started.elapsed()
        );
        let seen = seen.lock().map(|v| v.clone()).unwrap_or_default();
        assert_eq!(seen, vec!["first".to_string()]);
    }

    #[cfg(unix)]
    fn sh(script: &str) -> CommandSpec {
        CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            stdin: None,
            env_remove: vec![],
            timeout: Duration::from_secs(60),
            cwd_files: Vec::new(),
        }
    }

    /// A delegate that exits normally while something it spawned (an MCP server) still holds
    /// its stderr. The run must finish anyway: a runner that joined the drain thread with no
    /// deadline would never send `Finished`, and Stop would spin until the straggler died.
    #[cfg(unix)]
    #[test]
    fn a_straggler_holding_stderr_does_not_hold_the_streamed_run() {
        let started = std::time::Instant::now();
        let mut lines = Vec::new();
        let out = ProcessRunner
            .run_streaming(&sh("echo answer; sleep 30 >/dev/null & exit 0"), &mut |line| {
                lines.push(line.to_string());
                true
            })
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
        assert!(out.ok(), "{out:?}");
        assert_eq!(out.stdout, "answer\n");
        assert_eq!(lines, vec!["answer".to_string()]);
    }

    /// The same with the straggler holding stdout: the run ends, and the caller still gets every
    /// line it was shown.
    #[cfg(unix)]
    #[test]
    fn a_straggler_holding_stdout_does_not_hold_either_runner() {
        let spec = sh("echo answer; sleep 30 & exit 0");
        let started = std::time::Instant::now();
        let out = ProcessRunner.run_streaming(&spec, &mut |_| true).unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
        assert!(out.ok(), "{out:?}");
        assert_eq!(out.stdout, "answer\n");

        let started = std::time::Instant::now();
        let out = ProcessRunner.run(&sh("echo answer; sleep 30 >/dev/null & exit 0")).unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
        assert!(out.ok(), "{out:?}");
        assert_eq!(out.stdout, "answer\n");
    }

    /// Lines that reach the drain thread after a quiet gap past the delegate's exit are still
    /// handed over. For Claude Code that last line is the `result` event carrying `is_error`; a
    /// tail drain that stops at the first 200 ms gap loses it.
    #[cfg(unix)]
    #[test]
    fn the_tail_after_exit_is_handed_over_even_after_a_quiet_gap() {
        let mut lines = Vec::new();
        let out = ProcessRunner
            .run_streaming(&sh("echo first; (sleep 0.6; echo result) & exit 0"), &mut |line| {
                lines.push(line.to_string());
                true
            })
            .unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(lines, vec!["first".to_string(), "result".to_string()]);
    }

    #[test]
    fn every_streamed_completion_writes_exactly_one_trace_with_no_text() {
        let prompt = "reply about the acquisition of Northwind";
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok(CLAUDE_STREAM)),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (got, _) = streamed(&client, prompt);
        got.unwrap();
        let recs = client.sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].route, Route::LocalAgent);
        assert_eq!(recs[0].chunk_bytes, prompt.len());
        assert!(!format!("{:?}", recs[0]).contains("Northwind"), "prompt text reached the trace");
    }

    #[test]
    fn plain_text_from_a_cli_that_ignored_the_streaming_flags_still_answers() {
        // Zero exit, no JSON events — the CLI ignored the flags and printed its answer as text.
        // The run succeeded; erroring on it would turn a working delegate into a dead one.
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("Just plain text.\n")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (got, deltas) = streamed(&client, "hi");
        assert_eq!(got.unwrap(), "Just plain text.");
        assert_eq!(deltas, vec!["Just plain text.".to_string()]);
    }

    #[test]
    fn thinking_deltas_never_reach_the_caller() {
        // The model's scratchpad is not the answer; painting it would leak reasoning into the
        // user's document.
        let stream = concat!(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"the user wants…"}}}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}}"#,
            "\n",
        );
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok(stream)),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        );
        let (got, deltas) = streamed(&client, "hi");
        assert_eq!(got.unwrap(), "Done.");
        assert_eq!(deltas, vec!["Done.".to_string()]);
    }

    // Codex `exec --json` streaming (issue #388)

    /// The documented sample from <https://developers.openai.com/codex/noninteractive>, with a
    /// `reasoning` item added (the source emits one per non-empty reasoning summary).
    const CODEX_STREAM: &str = concat!(
        r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}"#,
        "\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**Planning** the user wants a short reply"}}"#,
        "\n",
        r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","status":"in_progress"}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Repo contains docs, sdk, and examples directories."}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"output_tokens":122,"reasoning_output_tokens":0}}"#,
        "\n",
    );

    fn codex_client(runner: MockRunner) -> SubscriptionAgentClient<MockRunner, RecordingSink> {
        SubscriptionAgentClient::new(runner, RecordingSink::new(), Delegate::Codex)
    }

    fn codex_parse(stream: &str) -> (Vec<String>, String) {
        let mut p = DeltaParser::new(StreamFormat::CodexJsonl);
        let deltas = stream.lines().filter_map(|l| p.push(l)).collect();
        (deltas, p.into_whole())
    }

    #[test]
    fn codex_streams_through_exec_json_with_its_containment() {
        let args = streaming_completion_spec(Delegate::Codex, "x").args;
        assert_eq!(&args[..3], ["exec", "--json", "-"].map(String::from));
        assert_eq!(&args[3..], codex_containment().as_slice());
        assert_eq!(Delegate::Codex.stream_format(), Some(StreamFormat::CodexJsonl));
    }

    #[test]
    fn codex_jsonl_yields_the_agent_message_and_nothing_else() {
        let client = codex_client(MockRunner::always(out_ok(CODEX_STREAM)));
        let (got, deltas) = streamed(&client, "summarize");
        assert_eq!(got.unwrap(), "Repo contains docs, sdk, and examples directories.");
        assert_eq!(deltas, vec!["Repo contains docs, sdk, and examples directories.".to_string()]);
        assert_eq!(client.runner.seen().len(), 1, "one spawn");
        assert_eq!(client.sink.records().len(), 1, "one egress, one trace");
    }

    #[test]
    fn codex_reasoning_and_tool_items_never_reach_the_caller() {
        // Every non-answer item the source can emit, each carrying text that must stay private.
        let stream = concat!(
            r#"{"type":"item.completed","item":{"id":"a","type":"reasoning","text":"LEAK-reasoning"}}"#,
            "\n",
            r#"{"type":"item.started","item":{"id":"b","type":"command_execution","command":"cat LEAK-cmd","status":"in_progress"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"b","type":"command_execution","command":"cat x","aggregated_output":"LEAK-output","exit_code":0,"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"c","type":"file_change","changes":[{"path":"LEAK-path","kind":"add"}],"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"d","type":"mcp_tool_call","server":"s","tool":"LEAK-tool","status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"e","type":"web_search","query":"LEAK-query"}}"#,
            "\n",
            r#"{"type":"item.updated","item":{"id":"f","type":"todo_list","items":[{"text":"LEAK-todo","completed":false}]}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"g","type":"error","message":"LEAK-item-error"}}"#,
            "\n",
            r#"{"type":"item.updated","item":{"id":"h","type":"agent_message","text":"LEAK-undocumented-partial"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"Done."}}"#,
            "\n",
        );
        let (deltas, whole) = codex_parse(stream);
        assert_eq!(deltas, vec!["Done.".to_string()]);
        assert!(!whole.contains("LEAK"), "non-answer text reached the caller: {whole}");
    }

    #[test]
    fn codex_agent_messages_in_one_turn_stay_separate_paragraphs() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"id":"1","type":"agent_message","text":"First."}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"2","type":"agent_message","text":""}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"3","type":"agent_message","text":"Second."}}"#,
            "\n",
        );
        let (deltas, whole) = codex_parse(stream);
        assert_eq!(deltas, vec!["First.".to_string(), "\n\nSecond.".to_string()]);
        assert_eq!(whole, "First.\n\nSecond.");
    }

    #[test]
    fn codex_parser_survives_a_partial_line_and_noise() {
        // Half a JSON object (a read cut mid-line), a blank line, CRLF, and non-JSON chatter must
        // neither emit nor poison the parser for the next whole line.
        let mut p = DeltaParser::new(StreamFormat::CodexJsonl);
        assert_eq!(p.push(r#"{"type":"item.completed","item":{"id":"1","type":"agent_mes"#), None);
        assert_eq!(p.push(""), None);
        assert_eq!(p.push("Reading prompt from stdin..."), None);
        assert_eq!(
            p.push("{\"type\":\"item.completed\",\"item\":{\"id\":\"1\",\"type\":\"agent_message\",\"text\":\"Hi.\"}}\r"),
            Some("Hi.".to_string())
        );
        assert_eq!(p.into_whole(), "Hi.");
    }

    /// The real runner with a child that writes one event in two pieces with a pause between them
    /// — the shape of a pipe read landing mid-line. The parser must see one whole line, once.
    #[cfg(unix)]
    #[test]
    fn codex_a_json_line_split_across_reads_arrives_whole() {
        let first = r#"{"type":"item.completed","item":{"id":"1","type":"agent_mes"#;
        let second = r#"sage","text":"Split but whole."}}"#;
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!("printf '%s' '{first}'; sleep 0.3; printf '%s\\n' '{second}'"),
            ],
            stdin: None,
            env_remove: vec![],
            timeout: Duration::from_secs(10),
            cwd_files: Vec::new(),
        };
        let mut parser = DeltaParser::new(StreamFormat::CodexJsonl);
        let mut deltas = Vec::new();
        let out = ProcessRunner
            .run_streaming(&spec, &mut |line| {
                deltas.extend(parser.push(line));
                true
            })
            .unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(deltas, vec!["Split but whole.".to_string()]);
    }

    #[test]
    fn a_codex_stream_without_an_agent_message_is_an_error_not_raw_json() {
        // Zero exit, events only — raw JSON must never be painted as the answer.
        let stream = concat!(
            r#"{"type":"thread.started","thread_id":"t"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":0}}"#,
            "\n",
        );
        let client = codex_client(MockRunner::always(out_ok(stream)));
        let (got, deltas) = streamed(&client, "hi");
        assert!(matches!(got, Err(LlmError::Provider(_))), "{got:?}");
        assert!(deltas.is_empty(), "{deltas:?}");
    }

    #[test]
    fn a_failed_codex_turn_reports_the_delegates_own_message() {
        let failed = |message: &str| {
            Ok(CommandOutput {
                code: Some(1),
                stdout: format!(
                    "{}\n{}\n",
                    r#"{"type":"thread.started","thread_id":"t"}"#,
                    serde_json::json!({"type":"turn.failed","error":{"message":message}})
                ),
                stderr: String::new(),
            })
        };
        // Quota words inside the event still classify as quota.
        let client = codex_client(MockRunner::always(failed("You've hit your usage limit.")));
        let (got, _) = streamed(&client, "hi");
        assert!(matches!(got, Err(LlmError::QuotaExhausted(_))), "{got:?}");
        // Anything else surfaces the delegate's words — not the first raw JSON event.
        let client = codex_client(MockRunner::always(failed("model is not supported")));
        let (got, _) = streamed(&client, "hi");
        match got {
            Err(LlmError::Provider(m)) => {
                assert!(m.contains("model is not supported"), "{m}");
                assert!(!m.contains("thread.started"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        // A bare `error` event is read the same way.
        let err_event = Ok(CommandOutput {
            code: Some(1),
            stdout: r#"{"type":"error","message":"stream disconnected"}"#.to_string(),
            stderr: String::new(),
        });
        let (got, _) = streamed(&codex_client(MockRunner::always(err_event)), "hi");
        assert!(matches!(&got, Err(LlmError::Provider(m)) if m.contains("stream disconnected")), "{got:?}");
    }

    // DelegateOptions (issue #388)

    fn claude_containment() -> Vec<String> {
        ["--tools", "", "--strict-mcp-config", "--mcp-config", r#"{"mcpServers":{}}"#, "--no-session-persistence"]
            .map(String::from)
            .to_vec()
    }

    fn codex_containment() -> Vec<String> {
        [
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--ephemeral",
            "-c",
            "features.apps=false",
            "-c",
            "features.plugins=false",
            "-c",
            "features.shell_tool=false",
            "-c",
            r#"web_search="disabled""#,
        ]
        .map(String::from)
        .to_vec()
    }

    fn gemini_containment() -> Vec<String> {
        [
            "--allowed-mcp-server-names",
            "",
            "--extensions",
            "none",
            "--admin-policy",
            "shogun-containment-policy.toml",
            "--policy",
            "shogun-containment-policy.toml",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn default_options_add_no_argv() {
        // The exact argv every delegate ran before options existed. `None` must change nothing.
        let none = DelegateOptions::default();
        let expect_buffered = |d: Delegate| -> Vec<String> {
            match d {
                Delegate::ClaudeCode => [["-p", "--output-format", "text"].map(String::from).to_vec(), claude_containment()].concat(),
                Delegate::Codex => [["exec", "-"].map(String::from).to_vec(), codex_containment()].concat(),
                Delegate::GeminiCli => gemini_containment(),
            }
        };
        let expect_streaming = |d: Delegate| -> Vec<String> {
            match d {
                Delegate::ClaudeCode => [
                    ["-p", "--output-format", "stream-json", "--include-partial-messages", "--verbose"].map(String::from).to_vec(),
                    claude_containment(),
                ]
                .concat(),
                Delegate::Codex => [["exec", "--json", "-"].map(String::from).to_vec(), codex_containment()].concat(),
                Delegate::GeminiCli => gemini_containment(),
            }
        };
        for d in Delegate::ALL {
            assert!(d.option_args(&none).is_empty(), "{}", d.id());
            assert_eq!(d.completion_args(&none), expect_buffered(d), "{}", d.id());
            assert_eq!(d.streaming_args(&none), Some(expect_streaming(d)), "{}", d.id());
            assert_eq!(completion_spec(d, "x").args, expect_buffered(d));
            assert_eq!(streaming_completion_spec(d, "x").args, expect_streaming(d));
        }
    }

    #[test]
    fn codex_options_emit_only_the_documented_flags() {
        let both = DelegateOptions { model: Some("gpt-test-1".into()), reasoning_effort: Some(Effort::Low), ..Default::default() };
        let flags = ["--model", "gpt-test-1", "-c", "model_reasoning_effort=\"low\""].map(String::from).to_vec();
        assert_eq!(Delegate::Codex.option_args(&both), flags);
        assert_eq!(
            Delegate::Codex.completion_args(&both),
            [["exec", "-"].map(String::from).to_vec(), codex_containment(), flags.clone()].concat()
        );
        assert_eq!(
            Delegate::Codex.streaming_args(&both),
            Some([["exec", "--json", "-"].map(String::from).to_vec(), codex_containment(), flags].concat())
        );

        let model_only = DelegateOptions { model: Some("gpt-test-1".into()), reasoning_effort: None, ..Default::default() };
        assert_eq!(Delegate::Codex.option_args(&model_only), ["--model", "gpt-test-1"].map(String::from));
        for (effort, word) in [(Effort::Low, "low"), (Effort::Medium, "medium"), (Effort::High, "high")] {
            let o = DelegateOptions { model: None, reasoning_effort: Some(effort), ..Default::default() };
            assert_eq!(
                Delegate::Codex.option_args(&o),
                ["-c".to_string(), format!("model_reasoning_effort=\"{word}\"")]
            );
        }
    }

    #[test]
    fn unverified_delegates_ignore_options() {
        // Generic model/reasoning options do not change these delegates' argv.
        let both = DelegateOptions { model: Some("m".into()), reasoning_effort: Some(Effort::High), ..Default::default() };
        for d in [Delegate::ClaudeCode, Delegate::GeminiCli] {
            assert!(d.option_args(&both).is_empty());
            assert_eq!(d.completion_args(&both), d.completion_args(&DelegateOptions::default()));
            assert_eq!(d.streaming_args(&both), d.streaming_args(&DelegateOptions::default()));
        }
    }

    #[test]
    fn claude_meeting_chat_uses_medium_effort_only_for_that_tag() {
        use crate::llm::prompts::{INLINE_CHAT, INLINE_DRAFT, MEETING_CHAT};

        let meeting = DelegateOptions::for_tag(MEETING_CHAT);
        assert_eq!(Delegate::ClaudeCode.option_args(&meeting), ["--effort", "medium"]);
        assert!(Delegate::ClaudeCode.completion_args(&meeting).ends_with(&["--effort".into(), "medium".into()]));
        assert!(Delegate::ClaudeCode.streaming_args(&meeting).unwrap().ends_with(&["--effort".into(), "medium".into()]));
        for tag in [INLINE_CHAT, INLINE_DRAFT] {
            assert!(Delegate::ClaudeCode.option_args(&DelegateOptions::for_tag(tag)).is_empty());
        }
    }

    #[test]
    fn an_unsafe_model_name_never_reaches_argv() {
        let long = "x".repeat(129);
        for bad in ["", "   ", "-c", "--config=x", "a b", "gpt;rm -rf", "m\nx", long.as_str()] {
            let o = DelegateOptions { model: Some(bad.to_string()), reasoning_effort: None, ..Default::default() };
            assert!(Delegate::Codex.option_args(&o).is_empty(), "{bad:?} reached argv");
        }
        let ok = DelegateOptions { model: Some(" gpt-6.1/codex:latest ".into()), reasoning_effort: None, ..Default::default() };
        assert_eq!(Delegate::Codex.option_args(&ok), ["--model", "gpt-6.1/codex:latest"].map(String::from));
    }

    #[test]
    fn the_prompt_tags_effort_policy_reaches_the_codex_argv() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook};
        // The `-c` override carrying the effort; containment adds `-c` overrides of its own.
        let effort_arg = |args: &[String]| -> Option<String> {
            values_after(args, "-c").into_iter().find(|v| v.starts_with("model_reasoning_effort=")).map(str::to_string)
        };
        let sink = std::sync::Arc::new(RecordingUsageSink::new());

        // Inline draft (buffered) runs low.
        let client = codex_client(MockRunner::always(out_ok("ok")))
            .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_DRAFT));
        client.complete("x").unwrap();
        assert_eq!(effort_arg(&client.runner.seen()[0].args).as_deref(), Some("model_reasoning_effort=\"low\""));

        // Chat (streamed) runs medium.
        let client = codex_client(MockRunner::always(out_ok(CODEX_STREAM)))
            .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_CHAT));
        streamed(&client, "x").0.unwrap();
        assert_eq!(effort_arg(&client.runner.seen()[0].args).as_deref(), Some("model_reasoning_effort=\"medium\""));

        // A tag without a policy, and a client without a tag, stay on the CLI default.
        let client = codex_client(MockRunner::always(out_ok("ok")))
            .with_usage(UsageHook::new(sink.clone(), prompts::REPLY_DRAFT));
        client.complete("x").unwrap();
        assert_eq!(effort_arg(&client.runner.seen()[0].args), None);
        let client = codex_client(MockRunner::always(out_ok("ok")));
        client.complete("x").unwrap();
        assert_eq!(client.runner.seen()[0].args, completion_spec(Delegate::Codex, "x").args);

        // Explicit options replace the policy.
        let client = codex_client(MockRunner::always(out_ok("ok")))
            .with_usage(UsageHook::new(sink, prompts::INLINE_DRAFT))
            .with_options(DelegateOptions { model: None, reasoning_effort: Some(Effort::High), ..Default::default() });
        client.complete("x").unwrap();
        assert_eq!(effort_arg(&client.runner.seen()[0].args).as_deref(), Some("model_reasoning_effort=\"high\""));
    }

    #[test]
    fn a_streamed_completion_records_one_usage_row_with_its_first_token() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        let sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = codex_client(MockRunner::always(out_ok(CODEX_STREAM)))
            .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_CHAT))
            .with_options(DelegateOptions { model: Some("gpt-test-1".into()), reasoning_effort: None, ..Default::default() });
        streamed(&client, "summarize").0.unwrap();
        let recs = sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].route, Route::LocalAgent);
        assert_eq!(recs[0].prompt_id, "inline_chat");
        assert_eq!(recs[0].model, "codex/gpt-test-1", "the requested model is in the ledger");
        assert!(recs[0].first_token_ms.is_some(), "SLO-03 needs the first-token time");
        assert!(recs[0].usage.estimated);
        assert_eq!(recs[0].outcome, UsageOutcome::Ok);

        // A delegate that does not pass the model on does not claim it either.
        let sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = SubscriptionAgentClient::new(MockRunner::always(out_ok(CLAUDE_STREAM)), RecordingSink::new(), Delegate::ClaudeCode)
            .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_CHAT))
            .with_options(DelegateOptions { model: Some("gpt-test-1".into()), reasoning_effort: None, ..Default::default() });
        streamed(&client, "x").0.unwrap();
        assert_eq!(sink.records()[0].model, "claude");
    }

    #[test]
    fn a_streamed_failure_and_a_fallback_each_record_exactly_one_row() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        let sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = codex_client(MockRunner::always(out_err(1, "usage limit reached")))
            .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_CHAT));
        let _ = streamed(&client, "x");
        let recs = sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].outcome, UsageOutcome::Quota);
        assert_eq!(recs[0].first_token_ms, None);

        let sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = codex_client(MockRunner::new(
            vec![out_err(2, "error: unexpected argument '--json' found"), out_ok("Whole.")],
            out_ok(""),
        ))
        .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_CHAT));
        assert_eq!(streamed(&client, "x").0.unwrap(), "Whole.");
        assert_eq!(sink.records().len(), 1, "the buffered fallback's own row, not two");

        // A turn withdrawn before it spawned sent nothing and costs nothing: no row.
        let sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = codex_client(MockRunner::always(out_ok(CODEX_STREAM)))
            .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_CHAT));
        let flag = std::sync::atomic::AtomicBool::new(true);
        let (tx, _rx) = std::sync::mpsc::channel();
        client.complete_streaming_cancellable("x", tx, &flag).unwrap();
        assert!(sink.records().is_empty());
    }

    // retry on the CLI defaults (issue #388 safety net)

    /// A `codex exec --json` stdout for a turn the delegate refused, exiting with `code`.
    fn codex_refusal(code: i32, message: &str) -> Result<CommandOutput, RunError> {
        Ok(CommandOutput {
            code: Some(code),
            stdout: format!(
                "{}\n{}\n",
                r#"{"type":"thread.started","thread_id":"t"}"#,
                serde_json::json!({"type":"turn.failed","error":{"message":message}})
            ),
            stderr: String::new(),
        })
    }

    fn with_options_and_usage(
        runner: MockRunner,
        tag: crate::llm::prompts::PromptTag,
    ) -> (SubscriptionAgentClient<MockRunner, RecordingSink>, std::sync::Arc<crate::llm::usage::RecordingUsageSink>) {
        use crate::llm::usage::{RecordingUsageSink, UsageHook};
        let sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = codex_client(runner)
            .with_usage(UsageHook::new(sink.clone(), tag))
            .with_options(DelegateOptions { model: Some("gpt-test-1".into()), reasoning_effort: Some(Effort::Low), ..Default::default() });
        (client, sink)
    }

    #[test]
    fn a_refused_buffered_options_run_is_retried_once_on_the_cli_defaults() {
        use crate::llm::prompts;
        use crate::llm::usage::UsageOutcome;
        let (client, sink) = with_options_and_usage(
            MockRunner::new(vec![out_err(1, "reasoning effort 'low' is not supported for this model")], out_ok("Draft.")),
            prompts::INLINE_DRAFT,
        );
        assert_eq!(client.complete("x").unwrap(), "Draft.");
        let seen = client.runner.seen();
        assert_eq!(seen.len(), 2, "exactly one retry");
        assert!(seen[0].args.iter().any(|a| a == "--model"), "{:?}", seen[0].args);
        assert_eq!(seen[1].args, completion_spec(Delegate::Codex, "x").args, "the retry carries no option argv");
        assert_eq!(client.sink.records().len(), 2, "two spawns, two egress traces");
        let recs = sink.records();
        assert_eq!(recs.len(), 1, "one usage row for the call");
        assert_eq!(recs[0].outcome, UsageOutcome::Ok);
        assert_eq!(recs[0].model, "codex", "the row names what the answering run used");
        assert_eq!(recs[0].first_token_ms, None);
    }

    #[test]
    fn a_refused_streamed_options_run_is_retried_once_on_the_cli_defaults() {
        use crate::llm::prompts;
        use crate::llm::usage::UsageOutcome;
        // Non-zero exit with `turn.failed`, and a zero exit that still reports `turn.failed` /
        // `error`: all three are refusals before any delta.
        let bare_error = Ok(CommandOutput {
            code: Some(0),
            stdout: format!("{}\n", serde_json::json!({"type":"error","message":"unknown model gpt-test-1"})),
            stderr: String::new(),
        });
        for refusal in [codex_refusal(1, "effort not supported"), codex_refusal(0, "effort not supported"), bare_error] {
            let (client, sink) =
                with_options_and_usage(MockRunner::new(vec![refusal], out_ok(CODEX_STREAM)), prompts::INLINE_CHAT);
            let (got, deltas) = streamed(&client, "x");
            assert_eq!(got.unwrap(), "Repo contains docs, sdk, and examples directories.");
            assert_eq!(deltas.len(), 1, "{deltas:?}");
            let seen = client.runner.seen();
            assert_eq!(seen.len(), 2, "exactly one retry");
            assert_eq!(seen[1].args, streaming_completion_spec(Delegate::Codex, "x").args);
            let recs = sink.records();
            assert_eq!(recs.len(), 1, "one usage row for the call");
            assert_eq!(recs[0].outcome, UsageOutcome::Ok);
            assert_eq!(recs[0].model, "codex");
            assert!(recs[0].first_token_ms.is_some(), "the answering attempt's first token");
        }

        // The retry failing too surfaces that failure, still with one row and no third spawn.
        let (client, sink) =
            with_options_and_usage(MockRunner::always(codex_refusal(1, "still broken")), prompts::INLINE_CHAT);
        match streamed(&client, "x").0 {
            Err(LlmError::Provider(m)) => assert!(m.contains("still broken"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(client.runner.seen().len(), 2);
        let recs = sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].outcome, UsageOutcome::Error);
        assert_eq!(recs[0].first_token_ms, None);
    }

    /// Stands in for the delegate binary: each spawn runs the next `sh -c` script through the
    /// real [`ProcessRunner`] (real pipe, lines delivered while the child runs), and records the
    /// spec it replaced. The mock's buffered `run_streaming` cannot deliver a line before a
    /// non-zero exit; a real CLI does.
    #[cfg(unix)]
    struct ShScripts {
        scripts: std::sync::Mutex<std::collections::VecDeque<String>>,
        seen: std::sync::Mutex<Vec<CommandSpec>>,
    }

    #[cfg(unix)]
    impl ShScripts {
        fn new(scripts: Vec<String>) -> Self {
            Self { scripts: std::sync::Mutex::new(scripts.into()), seen: Default::default() }
        }

        fn stand_in(&self, spec: &CommandSpec) -> CommandSpec {
            self.seen.lock().unwrap().push(spec.clone());
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| "exit 97".into());
            CommandSpec {
                program: "sh".into(),
                args: vec!["-c".into(), script],
                stdin: None,
                env_remove: vec![],
                timeout: Duration::from_secs(10),
                cwd_files: Vec::new(),
            }
        }
    }

    #[cfg(unix)]
    impl CommandRunner for ShScripts {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunError> {
            ProcessRunner.run(&self.stand_in(spec))
        }

        fn run_streaming(
            &self,
            spec: &CommandSpec,
            on_line: &mut dyn FnMut(&str) -> bool,
        ) -> Result<CommandOutput, RunError> {
            ProcessRunner.run_streaming(&self.stand_in(spec), on_line)
        }
    }

    /// An `sh` script printing each of `lines` (no single quotes inside) and exiting with `code`.
    #[cfg(unix)]
    fn sh_emit(lines: &[&str], code: i32) -> String {
        let mut script: String = lines.iter().map(|l| format!("printf '%s\\n' '{l}'; ")).collect();
        script.push_str(&format!("exit {code}"));
        script
    }

    #[cfg(unix)]
    #[test]
    fn a_codex_failure_after_a_delta_is_not_retried() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        let failed = serde_json::json!({"type":"turn.failed","error":{"message":"stream disconnected"}}).to_string();
        let partial_then_failed = sh_emit(
            &[r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"Half an answer."}}"#, &failed],
            1,
        );
        // What a retry would have answered: it must never run.
        let would_answer = sh_emit(
            &[r#"{"type":"item.completed","item":{"id":"j","type":"agent_message","text":"Second answer."}}"#],
            0,
        );
        let sink = std::sync::Arc::new(RecordingUsageSink::new());
        let client = SubscriptionAgentClient::new(
            ShScripts::new(vec![partial_then_failed, would_answer]),
            RecordingSink::new(),
            Delegate::Codex,
        )
        .with_usage(UsageHook::new(sink.clone(), prompts::INLINE_CHAT))
        .with_options(DelegateOptions { model: Some("gpt-test-1".into()), reasoning_effort: Some(Effort::Low), ..Default::default() });

        let (tx, rx) = std::sync::mpsc::channel();
        let got = client.complete_streaming("x", tx);
        let deltas: Vec<String> = rx.into_iter().collect();
        match got {
            Err(LlmError::Provider(m)) => assert!(m.contains("stream disconnected"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(deltas, vec!["Half an answer.".to_string()], "the caller saw text: no rerun");
        assert_eq!(client.runner.seen.lock().unwrap().len(), 1, "one spawn");
        let recs = sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].outcome, UsageOutcome::Error);
        assert!(recs[0].first_token_ms.is_some());
        assert_eq!(recs[0].model, "codex/gpt-test-1", "the row names the run that failed");
    }

    #[test]
    fn a_failure_with_nothing_to_drop_or_an_account_cause_is_not_retried() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        // Default options (a tag with no effort policy, no explicit options): nothing to drop.
        for streaming in [false, true] {
            let sink = std::sync::Arc::new(RecordingUsageSink::new());
            let client = codex_client(MockRunner::new(vec![codex_refusal(1, "boom")], out_ok(CODEX_STREAM)))
                .with_usage(UsageHook::new(sink.clone(), prompts::REPLY_DRAFT));
            let got = if streaming { streamed(&client, "x").0 } else { client.complete("x") };
            assert!(got.is_err(), "streaming={streaming}: {got:?}");
            assert_eq!(client.runner.seen().len(), 1, "streaming={streaming}: no retry");
            let recs = sink.records();
            assert_eq!(recs.len(), 1);
            assert_eq!(recs[0].outcome, UsageOutcome::Error);
            assert_eq!(recs[0].model, "codex");
        }

        // Options set, but the refusal is the plan limit or the login: defaults cannot fix those.
        for (stderr, outcome) in [("usage limit reached", UsageOutcome::Quota), ("not logged in", UsageOutcome::Unauthorized)] {
            let (client, sink) =
                with_options_and_usage(MockRunner::new(vec![out_err(1, stderr)], out_ok("never")), prompts::INLINE_DRAFT);
            assert!(client.complete("x").is_err());
            assert_eq!(client.runner.seen().len(), 1, "{stderr}");
            assert_eq!(sink.records().len(), 1);
            assert_eq!(sink.records()[0].outcome, outcome);
        }
    }

    /// Compile-time trait probe: `Probe::<T>::IS_BATCH` resolves to the inherent `true` only when
    /// `T: BatchClient`; otherwise the blanket trait's `false` is the only candidate.
    struct Probe<T: ?Sized>(std::marker::PhantomData<T>);
    trait NotBatchClient {
        const IS_BATCH: bool = false;
    }
    impl<T: ?Sized> NotBatchClient for Probe<T> {}
    impl<T: ?Sized + crate::llm::BatchClient> Probe<T> {
        #[allow(dead_code)]
        const IS_BATCH: bool = true;
    }

    /// Positive control for the probe.
    struct SomeBatch;
    impl crate::llm::BatchClient for SomeBatch {
        fn classify(&self, _: &crate::llm::BatchRequest) -> Result<String, LlmError> {
            Ok(String::new())
        }
    }

    /// Evaluated at compile time: [a real BatchClient, the delegate client on either runner].
    const BATCH_PROBES: [bool; 3] = [
        Probe::<SomeBatch>::IS_BATCH,
        Probe::<SubscriptionAgentClient<MockRunner, RecordingSink>>::IS_BATCH,
        Probe::<SubscriptionAgentClient<ProcessRunner, RecordingSink>>::IS_BATCH,
    ];

    #[test]
    fn the_subscription_client_can_never_serve_the_batch_lane() {
        // The probe detects a BatchClient, and the delegate client is Agent-lane only
        // (invariant 5): subscription quota can never carry indexing / Dream Cycle / Brief.
        assert_eq!(BATCH_PROBES, [true, false, false]);
        let client = codex_client(MockRunner::always(out_ok("x")));
        assert_eq!(AgentClient::lane(&client), crate::llm::Lane::Agent);
    }

    // Invariant 5, stated where it is enforced: `SubscriptionAgentClient` implements `AgentClient`
    // and nothing else. There is no `impl BatchClient for SubscriptionAgentClient`, so the
    // following does not compile, and indexing / Dream Cycle / Morning Brief cannot be pushed
    // through a subscription quota:
    //     fn f(c: &dyn super::super::BatchClient) {}
    //     f(&SubscriptionAgentClient::new(..., Delegate::ClaudeCode));

    #[test]
    fn the_minimums_are_the_releases_the_containment_flags_need() {
        assert_eq!(Delegate::Codex.min_version(), Some("0.122.0"));
        assert_eq!(Delegate::GeminiCli.min_version(), Some("0.35.0"));
        // Not pinned: a guess could refuse a Claude Code that works.
        assert_eq!(Delegate::ClaudeCode.min_version(), None);
        for d in Delegate::ALL {
            if let Some(m) = d.min_version() {
                assert!(parse_cli_version(m).is_some(), "{m}");
            }
        }
    }

    #[test]
    fn versions_are_read_from_what_these_clis_actually_print() {
        type Case = (&'static str, Option<(u64, u64, u64)>);
        let cases: &[Case] = &[
            ("codex-cli 0.122.0", Some((0, 122, 0))),
            ("0.35.0", Some((0, 35, 0))),
            ("1.2.3 (Claude Code)", Some((1, 2, 3))),
            ("2.0.14 (Claude Code)", Some((2, 0, 14))),
            ("v0.35.0", Some((0, 35, 0))),
            ("0.36.0-preview.1", Some((0, 36, 0))),
            ("0.36.0-nightly.20251001.abc1234", Some((0, 36, 0))),
            ("codex-cli 0.123.0-alpha.3", Some((0, 123, 0))),
            ("0.122.0+build.5", Some((0, 122, 0))),
            ("gemini 0.35", Some((0, 35, 0))),
            ("codex-cli 0.122.0\n", Some((0, 122, 0))),
            ("(0.35.1)", Some((0, 35, 1))),
            ("", None),
            ("codex-cli", None),
            ("unknown", None),
            ("1", None),
            ("1.2.3.4", None),
            ("0.35.0abc", None),
            ("2026/10/01", None),
            ("99999999999999999999.0.0", None),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_cli_version(raw), *want, "{raw:?}");
        }
    }

    #[test]
    fn only_a_version_known_to_be_older_is_below_the_minimum() {
        assert!(version_below("codex-cli 0.121.9", "0.122.0"));
        assert!(version_below("codex-cli 0.99.0", "0.122.0"));
        assert!(version_below("0.34.2", "0.35.0"));
        assert!(version_below("0.34.9-preview.0", "0.35.0"));
        assert!(!version_below("codex-cli 0.122.0", "0.122.0"));
        assert!(!version_below("codex-cli 0.130.2", "0.122.0"));
        assert!(!version_below("1.0.0", "0.35.0"));
        // A pre-release or nightly of the minimum itself passes: a false "too old" would leave a
        // working CLI unusable, while a false pass still fails closed on the flags.
        assert!(!version_below("0.35.0-preview.1", "0.35.0"));
        assert!(!version_below("0.35.0-nightly.20250930.abc", "0.35.0"));
        // Unknown is not "too old".
        assert!(!version_below("", "0.35.0"));
        assert!(!version_below("gemini dev build", "0.35.0"));
        assert!(!version_below("0.1.0", "not a version"));
    }

    #[test]
    fn detection_reports_a_cli_older_than_the_minimum_as_needing_an_update() {
        let old = MockRunner::always(out_ok("codex-cli 0.121.0\n"));
        assert_eq!(
            detect(&old, Delegate::Codex),
            DelegateState::NeedsUpdate { version: "codex-cli 0.121.0".into(), minimum: Some("0.122.0") }
        );
        let old_gemini = MockRunner::always(out_ok("0.34.0\n"));
        let state = detect(&old_gemini, Delegate::GeminiCli);
        assert_eq!(state.tag(), "needs_update");
        assert_eq!(state.minimum(), Some("0.35.0"));
        assert_eq!(state.version(), "0.34.0");
        assert!(state.is_installed());
        assert!(!state.is_usable());
        // At or above the minimum, or unreadable: plain "installed".
        for (d, out) in [
            (Delegate::Codex, "codex-cli 0.122.0"),
            (Delegate::GeminiCli, "0.35.0-preview.2"),
            (Delegate::GeminiCli, "something unexpected"),
            (Delegate::ClaudeCode, "0.0.1 (Claude Code)"),
        ] {
            let runner = MockRunner::always(out_ok(out));
            assert_eq!(detect(&runner, d).tag(), "installed", "{d:?} {out}");
        }
    }

    #[test]
    fn verify_refuses_a_too_old_cli_without_running_a_completion() {
        let runner = MockRunner::new(vec![out_ok("codex-cli 0.100.0")], out_ok("ok"));
        let state = verify(&runner, Delegate::Codex);
        assert_eq!(state.tag(), "needs_update");
        // Only the version probe ran: nothing was sent, nothing of the plan was spent.
        let seen = runner.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].args, vec!["--version".to_string()]);
    }

    #[test]
    fn verify_reports_needs_update_when_an_unreadable_version_rejects_a_flag() {
        let runner = MockRunner::new(
            vec![out_ok("dev build"), out_err(2, "error: unexpected argument '--ignore-user-config' found")],
            out_ok(""),
        );
        assert_eq!(
            verify(&runner, Delegate::Codex),
            DelegateState::NeedsUpdate { version: "dev build".into(), minimum: Some("0.122.0") }
        );
        let claude = MockRunner::new(
            vec![out_ok("1.0.0 (Claude Code)"), out_err(1, "error: unknown option '--no-session-persistence'")],
            out_ok(""),
        );
        assert_eq!(
            verify(&claude, Delegate::ClaudeCode),
            DelegateState::NeedsUpdate { version: "1.0.0 (Claude Code)".into(), minimum: None }
        );
    }

    #[test]
    fn an_argument_error_is_classified_as_needs_update_from_fixed_patterns() {
        for stderr in [
            "error: unknown option '--tools'",
            "error: unexpected argument '--ignore-rules' found\n\nUsage: codex exec [OPTIONS] [PROMPT]",
            "Unknown arguments: admin-policy, adminPolicy",
            "Unknown argument: policy",
        ] {
            let out = CommandOutput { code: Some(2), stdout: String::new(), stderr: stderr.into() };
            assert_eq!(classify(&out), DelegateFailure::NeedsUpdate, "{stderr}");
        }
        // Option help printed with the error must not read as a login or quota problem.
        let yargs = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "Options:\n  --prompt  Not logged in? Run /login. Quota applies.\nUnknown arguments: admin-policy".into(),
        };
        assert_eq!(classify(&yargs), DelegateFailure::NeedsUpdate);
        // A model's partial answer on stdout that happens to mention an option is not one.
        let answer = CommandOutput {
            code: Some(1),
            stdout: "Pass --verbose; an unknown option is ignored.".into(),
            stderr: "connection reset".into(),
        };
        assert!(matches!(classify(&answer), DelegateFailure::Failed(_)));
    }

    #[test]
    fn needs_update_is_shogun_copy_naming_the_minimum_and_never_the_cli_text() {
        match DelegateFailure::NeedsUpdate.into_llm_error(Delegate::Codex) {
            LlmError::NeedsUpdate(m) => {
                assert!(m.contains("Codex") && m.contains("0.122.0"), "{m}");
                assert!(!m.contains("unexpected argument"), "{m}");
            }
            other => panic!("expected NeedsUpdate, got {other:?}"),
        }
        match DelegateFailure::NeedsUpdate.into_llm_error(Delegate::ClaudeCode) {
            LlmError::NeedsUpdate(m) => assert!(m.contains("latest version"), "{m}"),
            other => panic!("expected NeedsUpdate, got {other:?}"),
        }
        assert_eq!(
            crate::chat_failure::for_error(&DelegateFailure::NeedsUpdate.into_llm_error(Delegate::GeminiCli)),
            crate::chat_failure::NEEDS_UPDATE
        );
    }

    #[test]
    fn a_gated_client_refuses_a_too_old_cli_before_spawning_the_run() {
        static GATE: VersionGate = VersionGate::new();
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("codex-cli 0.110.0")),
            RecordingSink::new(),
            Delegate::Codex,
        )
        .with_version_gate(&GATE);
        assert!(matches!(client.complete("hi"), Err(LlmError::NeedsUpdate(_))));
        let (got, deltas) = streamed(&client, "hi");
        assert!(matches!(got, Err(LlmError::NeedsUpdate(_))), "{got:?}");
        assert!(deltas.is_empty());
        // Two version probes (a too-old verdict is not remembered), no completion spawned, and
        // so no egress trace: nothing left the device.
        let seen = client.runner.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|s| s.args == vec!["--version".to_string()]));
        assert!(client.sink.records().is_empty());
    }

    #[test]
    fn the_gate_remembers_a_pass_and_rechecks_a_too_old_cli_every_run() {
        static GATE: VersionGate = VersionGate::new();
        // Too old, then updated: the very next run goes through.
        let runner = MockRunner::new(
            vec![out_ok("0.30.0"), out_ok("0.35.0"), out_ok("answer one"), out_ok("answer two")],
            out_ok("unexpected"),
        );
        let client = SubscriptionAgentClient::new(runner, RecordingSink::new(), Delegate::GeminiCli)
            .with_version_gate(&GATE);
        assert!(matches!(client.complete("hi"), Err(LlmError::NeedsUpdate(_))));
        assert_eq!(client.complete("hi").unwrap(), "answer one");
        // The pass is remembered: no third probe.
        assert_eq!(client.complete("hi").unwrap(), "answer two");
        let probes = client.runner.seen().iter().filter(|s| s.args == vec!["--version".to_string()]).count();
        assert_eq!(probes, 2);
        assert_eq!(client.sink.records().len(), 2);
    }

    #[test]
    fn a_probe_made_elsewhere_warms_the_gate_and_a_too_old_one_clears_it() {
        static GATE: VersionGate = VersionGate::new();
        let probe = MockRunner::always(out_ok("0.40.0"));
        GATE.observe(Delegate::GeminiCli, &detect(&probe, Delegate::GeminiCli));
        // Warm: the run's own check spawns nothing.
        let silent = MockRunner::always(out_ok("0.1.0"));
        assert!(GATE.check(&silent, Delegate::GeminiCli).is_ok());
        assert!(silent.seen().is_empty());
        // A too-old observation (the user downgraded) forgets the pass.
        GATE.observe(
            Delegate::GeminiCli,
            &DelegateState::NeedsUpdate { version: "0.1.0".into(), minimum: Some("0.35.0") },
        );
        assert_eq!(GATE.check(&silent, Delegate::GeminiCli), Err(DelegateFailure::NeedsUpdate));
        assert_eq!(silent.seen().len(), 1);
    }

    #[test]
    fn the_gate_never_probes_a_cli_without_a_pinned_minimum() {
        static GATE: VersionGate = VersionGate::new();
        let client = SubscriptionAgentClient::new(
            MockRunner::always(out_ok("Fine.")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        )
        .with_version_gate(&GATE);
        assert_eq!(client.complete("hi").unwrap(), "Fine.");
        assert_eq!(client.runner.seen().len(), 1);
        assert!(GATE.check(&MockRunner::always(out_ok("0.0.1")), Delegate::ClaudeCode).is_ok());
    }

    #[test]
    fn an_unreadable_version_passes_the_gate_and_a_rejected_flag_still_says_needs_update() {
        static GATE: VersionGate = VersionGate::new();
        let flag_error = "error: unexpected argument '--ignore-user-config' found";
        let client = SubscriptionAgentClient::new(
            MockRunner::new(vec![out_ok("codex-cli dev")], out_err(2, flag_error)),
            RecordingSink::new(),
            Delegate::Codex,
        )
        .with_version_gate(&GATE);
        // Streaming refuses the flags, the buffered fallback refuses them too: an update, in
        // SHOGUN's words, not the CLI's argument error.
        let (got, _) = streamed(&client, "hi");
        match got {
            Err(LlmError::NeedsUpdate(m)) => assert!(!m.contains("unexpected argument"), "{m}"),
            other => panic!("expected NeedsUpdate, got {other:?}"),
        }
    }

    #[test]
    fn a_rejected_option_flag_is_still_retried_on_the_cli_defaults() {
        // An older Claude Code that knows the containment flags but not `--effort`: the run on
        // the defaults answers, so this is not reported as needing an update.
        let client = SubscriptionAgentClient::new(
            MockRunner::new(vec![out_err(1, "error: unknown option '--effort'")], out_ok("Answer.")),
            RecordingSink::new(),
            Delegate::ClaudeCode,
        )
        .with_options(DelegateOptions { claude_code_effort: Some(Effort::Medium), ..DelegateOptions::default() });
        assert_eq!(client.complete("hi").unwrap(), "Answer.");
        let seen = client.runner.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].args.iter().any(|a| a == "--effort"));
        assert!(!seen[1].args.iter().any(|a| a == "--effort"));
    }
}
