//! The `shogun` argument parser — a pure `parse(&[String]) -> Result<Invocation, CliError>`, so the
//! whole command grammar is unit-testable without spawning a process. Dependency-free (no clap):
//! the grammar is small and the tests are the spec.

use crate::command::{AgentSourceCommand, CaptureExclusionsCommand, Command, ConnectionsCommand, ConfigAction, ImportSource, ListOrGet, MicrophoneCommand, PrincipalCommand, RecapEditBody, TranslateCommand, TranslateRegisterCommand, VoiceDictionaryCommand};
use shogun_mcp::backend::SearchDepthV1;
use shogun_mcp::forms_api::FormOperation;

/// A fully parsed invocation: the command plus global flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    /// `--token <t>` (else the daemon reads it from the Keychain).
    pub token: Option<String>,
    /// `--json` machine-readable output.
    pub json: bool,
    /// `--include-low` — include <0.5 confidence read results (FR-API-06 opt-in).
    pub include_low: bool,
}

/// A parse failure, with a message suitable for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// A flag that needs a value was given none (e.g. trailing `--token`).
    MissingFlagValue(&'static str),
    /// A subcommand needs an argument it didn't get.
    MissingArgument(&'static str),
    /// An id argument wasn't an integer.
    BadId(String),
    /// Visual Recall retention is finite and bounded.
    BadRetention(String),
    /// Backup snapshot retention is 1–30 days (issue #647).
    BadBackupRetention(String),
    /// The subcommand isn't recognised.
    UnknownCommand(String),
    /// A recognised subcommand got a sub-argument it doesn't understand.
    UnknownSubcommand { command: &'static str, got: String },
    /// Search query exceeded shared API policy.
    SearchQueryTooLong,
    /// A file the user named could not be used (fixed text; never the file's content).
    BadInput(String),
}

impl CliError {
    pub fn message(&self) -> String {
        match self {
            CliError::MissingFlagValue(f) => format!("flag `{f}` needs a value"),
            CliError::MissingArgument(a) => format!("missing argument: {a}"),
            CliError::BadId(s) => format!("invalid id: `{s}` (expected a positive integer)"),
            CliError::BadRetention(s) => {
                format!("invalid retention: `{s}` (expected 1–3650 days)")
            }
            CliError::BadBackupRetention(s) => format!(
                "invalid backup retention: `{s}` (expected {}–{} days)",
                shogun_core::backup_retention::MIN_DAYS,
                shogun_core::backup_retention::MAX_DAYS
            ),
            CliError::UnknownCommand(c) => format!("unknown command: `{c}` (try `shogun help`)"),
            CliError::UnknownSubcommand { command, got } => match expected_for(command) {
                Some(expected) => {
                    format!("unknown `{command}` subcommand: `{got}` (expected {expected})")
                }
                None => format!("unknown `{command}` subcommand: `{got}` (see `shogun help`)"),
            },
            CliError::SearchQueryTooLong => "search_query_too_long".to_string(),
            CliError::BadInput(why) => why.clone(),
        }
    }
}

/// The valid choices for a command whose subcommand was not recognised (issue #779). The ones
/// without an entry here point at `shogun help` rather than guess a generic "(expected
/// list|get)".
fn expected_for(command: &str) -> Option<&'static str> {
    Some(match command {
        "forms" => "inspect | plan <session> <revision> <instruction...> | status <session> | apply <session> <revision> | close <session>",
        "capture" => "status | pause [15m|30m|1h] | resume",
        "search depth" => "warm | auto | all",
        "search source kind" => "meeting | state | brief | person | project | thread | period",
        "config" => "path | show | validate",
        "principal" => "import --from <memory.db> --root <dir> --principal <id>",
        "api" => "status | tokens",
        "actions" => "questions | answer | steer",
        "mail" => "triage | labels | rules | history | status | on | off",
        "backup" => "status | retention <days>",
        "calendar" => "upcoming",
        "update" => "status (installing and restarting are in the app)",
        "profile" => "set <json>",
        "wiki" => "list | evidence <key> | neighbors <key>",
        "approvals" => "list | get <id> | decide <id> approve|reject [--as <kind:id>]",
        "acl" => "list | grant <json> | revoke <json> | teams | team <json>",
        "audit" => "[--since-ms N] [--limit N]",
        "voice-dictionary" => "list | create | update | delete | import | export | autoadd | autoadd-manage",
        _ => return None,
    })
}

/// Parse argv (excluding the program name).
pub fn parse(args: &[String]) -> Result<Invocation, CliError> {
    let mut token = None;
    let mut json = false;
    let mut include_low = false;
    let mut positionals: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--token" => {
                let v = args
                    .get(i + 1)
                    .ok_or(CliError::MissingFlagValue("--token"))?;
                token = Some(v.clone());
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--include-low" => {
                include_low = true;
                i += 1;
            }
            _ => {
                positionals.push(args[i].clone());
                i += 1;
            }
        }
    }

    let command = parse_command(&positionals)?;
    Ok(Invocation {
        command,
        token,
        json,
        include_low,
    })
}

/// A `list` / `get <id>` sub-grammar shared by the state nouns.
fn parse_list_or_get(command: &'static str, rest: &[String]) -> Result<ListOrGet, CliError> {
    match rest.first().map(String::as_str) {
        None | Some("list") => Ok(ListOrGet::List),
        Some("get") => {
            let id_str = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
            let id = id_str
                .parse::<i64>()
                .map_err(|_| CliError::BadId(id_str.clone()))?;
            Ok(ListOrGet::Get { id })
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command,
            got: other.to_string(),
        }),
    }
}

/// `meeting speakers [show]|name <key> <name…>|unname <key>`. The key is what the transcript
/// stores (`me`, `other`, `other:N`); the server validates it, so parsing stays permissive.
fn parse_speakers(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::SpeakersCommand;
    match rest.first().map(String::as_str) {
        None | Some("show") => Ok(Command::MeetingSpeakers(SpeakersCommand::Show)),
        Some("name") => {
            let key = rest
                .get(1)
                .cloned()
                .ok_or(CliError::MissingArgument("<key>"))?;
            let name = join(&rest[2..]).ok_or(CliError::MissingArgument("<name>"))?;
            Ok(Command::MeetingSpeakers(SpeakersCommand::Name { key, name }))
        }
        Some("unname") => {
            let key = rest
                .get(1)
                .cloned()
                .ok_or(CliError::MissingArgument("<key>"))?;
            Ok(Command::MeetingSpeakers(SpeakersCommand::Unname { key }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "meeting speakers",
            got: other.to_string(),
        }),
    }
}

/// `meeting track [--session <id>] [--mine|--theirs] [--counterparty <id>] <text…>`.
fn parse_track(rest: &[String]) -> Result<Command, CliError> {
    let positive_id = |raw: Option<&String>| -> Result<i64, CliError> {
        let raw = raw.ok_or(CliError::MissingArgument("<id>"))?;
        raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.clone()))
    };
    let (mut session_id, mut direction, mut counterparty_id) = (None, None, None);
    let mut words = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--session" => {
                session_id = Some(positive_id(rest.get(i + 1))?);
                i += 1;
            }
            "--counterparty" => {
                counterparty_id = Some(positive_id(rest.get(i + 1))?);
                i += 1;
            }
            "--mine" => direction = Some("mine".to_string()),
            "--theirs" => direction = Some("theirs".to_string()),
            word => words.push(word.to_string()),
        }
        i += 1;
    }
    let text = join(&words).ok_or(CliError::MissingArgument("<text>"))?;
    Ok(Command::MeetingTrack { session_id, text, direction, counterparty_id })
}

/// `meeting templates [show]|set <id>`.
fn parse_templates(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::TemplatesCommand;
    match rest.first().map(String::as_str) {
        None | Some("show") => Ok(Command::MeetingTemplates(TemplatesCommand::Show)),
        Some("set") => {
            let id = rest
                .get(1)
                .cloned()
                .ok_or(CliError::MissingArgument("<id>"))?;
            Ok(Command::MeetingTemplates(TemplatesCommand::Set { id }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "meeting templates",
            got: other.to_string(),
        }),
    }
}

/// `meeting prefs [show]` / `meeting prefs set [--language X] [--instruction <text…>|--clear-instruction]`.
/// `--instruction` takes the rest of the line, so a multi-word instruction needs no quoting.
fn parse_recap_prefs(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::RecapPrefsCommand;
    match rest.first().map(String::as_str) {
        None | Some("show") => Ok(Command::MeetingRecapPrefs(RecapPrefsCommand::Show)),
        Some("set") => {
            let mut language: Option<String> = None;
            let mut instruction: Option<Option<String>> = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--language" => {
                        let value = rest.get(i + 1).ok_or(CliError::MissingArgument("<language>"))?;
                        language = Some(value.clone());
                        i += 2;
                    }
                    "--clear-instruction" => {
                        instruction = Some(None);
                        i += 1;
                    }
                    "--instruction" => {
                        let text = join(&rest[i + 1..]).ok_or(CliError::MissingArgument("<text>"))?;
                        instruction = Some(Some(text));
                        i = rest.len();
                    }
                    other => {
                        return Err(CliError::UnknownSubcommand {
                            command: "meeting prefs set",
                            got: other.to_string(),
                        })
                    }
                }
            }
            if language.is_none() && instruction.is_none() {
                return Err(CliError::MissingArgument("--language|--instruction|--clear-instruction"));
            }
            Ok(Command::MeetingRecapPrefs(RecapPrefsCommand::Set { language, instruction }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "meeting prefs",
            got: other.to_string(),
        }),
    }
}

/// `<noun> microphone show|set <name>|default` — the capture-input sub-grammar shared by the
/// meeting and dictation lanes, so the two cannot drift apart (invariant 6). `default` is the
/// only way to ask for the macOS default input; `set` with no name is a missing argument, never a
/// silent reset.
fn parse_microphone(
    noun: &'static str,
    nested: &'static str,
    rest: &[String],
) -> Result<MicrophoneCommand, CliError> {
    match (
        rest.first().map(String::as_str),
        rest.get(1).map(String::as_str),
        rest.get(2),
    ) {
        (Some("microphone"), None | Some("show"), None) => Ok(MicrophoneCommand::Show),
        (Some("microphone"), Some("default"), None) => {
            Ok(MicrophoneCommand::Set { microphone: None })
        }
        (Some("microphone"), Some("set"), _) => Ok(MicrophoneCommand::Set {
            microphone: Some(join(&rest[2..]).ok_or(CliError::MissingArgument("<name>"))?),
        }),
        (Some("microphone"), Some(other), _) => Err(CliError::UnknownSubcommand {
            command: nested,
            got: other.to_string(),
        }),
        (Some(other), _, _) => Err(CliError::UnknownSubcommand {
            command: noun,
            got: other.to_string(),
        }),
        (None, _, _) => Err(CliError::MissingArgument("microphone")),
    }
}

fn parse_command(positionals: &[String]) -> Result<Command, CliError> {
    let Some(head) = positionals.first().map(String::as_str) else {
        return Ok(Command::Help);
    };
    let rest = &positionals[1..];
    match head {
        "help" | "--help" | "-h" => Ok(Command::Help),
        "search" => {
            let mut depth = None;
            let mut from_ms = None;
            let mut to_ms = None;
            let mut source_kind = None;
            let mut ai = false;
            let mut limit = None;
            let mut query_parts = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    // #940 B2b: widen and rerank on the user's own model (the app runs it).
                    "--ai" => {
                        ai = true;
                        i += 1;
                    }
                    "--limit" => {
                        let value = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--limit"))?;
                        limit = Some(value.parse().map_err(|_| CliError::BadId(value.clone()))?);
                        i += 2;
                    }
                    "--depth" => {
                        let value = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--depth"))?;
                        depth = Some(match value.as_str() {
                            "warm" => SearchDepthV1::Warm,
                            "auto" => SearchDepthV1::Auto,
                            "all" => SearchDepthV1::All,
                            _ => {
                                return Err(CliError::UnknownSubcommand {
                                    command: "search depth",
                                    got: value.clone(),
                                })
                            }
                        });
                        i += 2;
                    }
                    "--from-ms" => {
                        let value = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--from-ms"))?;
                        from_ms = Some(value.parse().map_err(|_| CliError::BadId(value.clone()))?);
                        i += 2;
                    }
                    "--to-ms" => {
                        let value = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--to-ms"))?;
                        to_ms = Some(value.parse().map_err(|_| CliError::BadId(value.clone()))?);
                        i += 2;
                    }
                    "--source-kind" => {
                        let value = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--source-kind"))?;
                        source_kind = Some(match value.as_str() {
                            "meeting" => shogun_mcp::backend::MemorySourceFamilyV1::Meeting,
                            "state" => shogun_mcp::backend::MemorySourceFamilyV1::State,
                            "brief" => shogun_mcp::backend::MemorySourceFamilyV1::Brief,
                            "person" => shogun_mcp::backend::MemorySourceFamilyV1::Person,
                            "project" => shogun_mcp::backend::MemorySourceFamilyV1::Project,
                            "thread" => shogun_mcp::backend::MemorySourceFamilyV1::Thread,
                            "period" => shogun_mcp::backend::MemorySourceFamilyV1::Period,
                            _ => {
                                return Err(CliError::UnknownSubcommand {
                                    command: "search source kind",
                                    got: value.clone(),
                                })
                            }
                        });
                        i += 2;
                    }
                    _ => {
                        query_parts.push(rest[i].clone());
                        i += 1;
                    }
                }
            }
            let query = join(&query_parts).ok_or(CliError::MissingArgument("<query>"))?;
            shogun_mcp::backend::validate_search_query(&query)
                .map_err(|_| CliError::SearchQueryTooLong)?;
            Ok(Command::Search {
                query,
                depth,
                from_ms,
                to_ms,
                source_kind,
                limit,
                ai,
            })
        }
        "periods" => {
            let (mut from_ms, mut to_ms, mut limit) = (None, None, None);
            let mut i = 0;
            while i < rest.len() {
                let flag = rest[i].as_str();
                let name: &'static str = match flag {
                    "--from-ms" => "--from-ms",
                    "--to-ms" => "--to-ms",
                    // `--page-size` is the REST/MCP name; `--limit` stays as an alias.
                    "--page-size" => "--page-size",
                    "--limit" => "--limit",
                    other => {
                        return Err(CliError::UnknownSubcommand { command: "periods", got: other.to_string() })
                    }
                };
                let value = rest.get(i + 1).ok_or(CliError::MissingFlagValue(name))?;
                let bad = || CliError::BadId(value.clone());
                match name {
                    "--from-ms" => from_ms = Some(value.parse().map_err(|_| bad())?),
                    "--to-ms" => to_ms = Some(value.parse().map_err(|_| bad())?),
                    _ => limit = Some(value.parse().map_err(|_| bad())?),
                }
                i += 2;
            }
            Ok(Command::Periods { from_ms, to_ms, limit })
        }
        cmd @ ("events" | "session") => {
            use shogun_mcp::memory_api::Tool;
            let tool = if cmd == "events" { Tool::MemoryGetEvents } else { Tool::MemoryReadSession };
            let mut args = serde_json::Map::new();
            let mut ids = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                let flag = rest[i].as_str();
                let key = match (cmd, flag) {
                    ("events", "--max-chars") => "max_chars",
                    ("session", "--from-ms") => "from_ms",
                    ("session", "--to-ms") => "to_ms",
                    ("session", "--app") => "app",
                    ("session", "--limit") => "limit",
                    ("events", id) if !id.starts_with("--") => {
                        ids.push(serde_json::Value::from(id.parse::<i64>().map_err(|_| CliError::BadId(id.to_string()))?));
                        i += 1;
                        continue;
                    }
                    (_, other) => return Err(CliError::BadInput(format!("unknown {cmd} option `{other}`"))),
                };
                let value = rest.get(i + 1).ok_or_else(|| CliError::BadInput(format!("{flag} needs a value")))?;
                args.insert(
                    key.into(),
                    if key == "app" {
                        value.clone().into()
                    } else {
                        value.parse::<i64>().map_err(|_| CliError::BadId(value.clone()))?.into()
                    },
                );
                i += 2;
            }
            if cmd == "events" {
                args.insert("ids".into(), ids.into());
            }
            let body = serde_json::Value::Object(args);
            shogun_mcp::activity_api::request(tool, &body).map_err(CliError::BadInput)?;
            let body = body.to_string();
            Ok(if cmd == "events" { Command::Events { body } } else { Command::Session { body } })
        }
        // The context cache has no partial form: an extra word (e.g. `--no-screen`, which the
        // server never honours) is refused rather than silently ignored.
        "context" => match rest.first() {
            None => Ok(Command::Context),
            Some(other) => Err(CliError::UnknownSubcommand {
                command: "context",
                got: other.clone(),
            }),
        },
        "pack" => {
            let query = join(rest).ok_or(CliError::MissingArgument("<query>"))?;
            Ok(Command::Pack { query })
        }
        // E4-T3: the request JSON as `memory.ingest` takes it, validated by the shared parser so a
        // bad body is refused here with the same code the server would answer.
        "ingest" => {
            let raw = join(rest).ok_or(CliError::MissingArgument("<json>"))?;
            let body: serde_json::Value =
                serde_json::from_str(&raw).map_err(|_| CliError::BadInput("ingest takes one JSON object".into()))?;
            shogun_mcp::ingest_api::ingest_request(&body).map_err(CliError::BadInput)?;
            Ok(Command::Ingest { body: body.to_string() })
        }
        // E44: one provider webhook delivery as `memory.ingest_webhook` takes it (a saved delivery
        // replayed), validated by the shared parser.
        "ingest-webhook" => {
            let raw = join(rest).ok_or(CliError::MissingArgument("<json>"))?;
            let body: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|_| CliError::BadInput("ingest-webhook takes one JSON object".into()))?;
            shogun_mcp::webhook_ingest::webhook_request(&body).map_err(CliError::BadInput)?;
            Ok(Command::IngestWebhook { body: body.to_string() })
        }
        // E15: approvals. Deciding is the operator's (the CLI sends SHOGUN_ADMIN_TOKEN for it).
        "approvals" => {
            use crate::command::ApprovalsCommand;
            let id_at = |i: usize| -> Result<u64, CliError> {
                let raw = rest.get(i).ok_or(CliError::MissingArgument("<id>"))?;
                raw.parse::<u64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.clone()))
            };
            match rest.first().map(String::as_str) {
                None | Some("list") => Ok(Command::Approvals(ApprovalsCommand::List)),
                Some("get") => Ok(Command::Approvals(ApprovalsCommand::Get { id: id_at(1)? })),
                Some("decide") => {
                    let id = id_at(1)?;
                    let decision = match rest.get(2).map(String::as_str) {
                        Some(d @ ("approve" | "reject")) => d,
                        Some(other) => return Err(CliError::BadInput(format!("decision must be approve or reject, not `{other}`"))),
                        None => return Err(CliError::MissingArgument("approve|reject")),
                    };
                    let decided_by = match rest.get(3).map(String::as_str) {
                        Some("--as") => rest.get(4).cloned().ok_or(CliError::MissingFlagValue("--as"))?,
                        Some(other) => return Err(CliError::BadInput(format!("unexpected `{other}` (use --as <kind:id>)"))),
                        None => "human:operator".to_string(),
                    };
                    let valid = decided_by.split_once(':').is_some_and(|(kind, id)| !kind.is_empty() && !id.is_empty())
                        && decided_by.chars().count() <= 128
                        && !decided_by.chars().any(|c| c.is_whitespace() || c.is_control());
                    if !valid {
                        return Err(CliError::BadInput("--as takes <kind>:<id>, for example human:aiko".into()));
                    }
                    let body = serde_json::json!({ "decision": decision, "decided_by": decided_by }).to_string();
                    Ok(Command::Approvals(ApprovalsCommand::Decide { id, body }))
                }
                Some(other) => Err(CliError::UnknownSubcommand { command: "approvals", got: other.to_string() }),
            }
        }
        // E18: the audit log.
        // E26: the hash-chained export (operator) and its offline verifier.
        "audit" if rest.first().map(String::as_str) == Some("verify") => {
            let file = rest.get(1).cloned().ok_or(CliError::MissingArgument("<file>"))?;
            Ok(Command::AuditVerify { file })
        }
        "audit" if rest.first().map(String::as_str) == Some("export") => {
            let (mut since_ms, mut until_ms, mut format, mut principal, mut out) = (None, None, "jsonl".to_string(), None, None);
            let mut i = 1;
            while i < rest.len() {
                let flag = rest[i].as_str();
                let value = |name: &'static str| rest.get(i + 1).cloned().ok_or(CliError::MissingFlagValue(name));
                match flag {
                    "--since-ms" => {
                        since_ms = Some(value("--since-ms")?.parse::<i64>().map_err(|_| CliError::BadInput("--since-ms takes milliseconds".into()))?);
                    }
                    "--until-ms" => {
                        until_ms = Some(value("--until-ms")?.parse::<i64>().map_err(|_| CliError::BadInput("--until-ms takes milliseconds".into()))?);
                    }
                    "--format" => {
                        format = value("--format")?;
                        if shogun_mcp::audit_export::Format::parse(&format).is_none() {
                            return Err(CliError::BadInput("--format takes jsonl or csv".into()));
                        }
                    }
                    "--principal" => principal = Some(value("--principal")?),
                    "--out" => out = Some(value("--out")?),
                    other => return Err(CliError::UnknownSubcommand { command: "audit export", got: other.to_string() }),
                }
                i += 2;
            }
            if let (Some(since), Some(until)) = (since_ms, until_ms) {
                if until < since {
                    return Err(CliError::BadInput("--until-ms is before --since-ms".into()));
                }
            }
            if principal.as_deref().is_some_and(|p| !principal_slug_ok(p)) {
                return Err(CliError::BadInput("--principal takes a principal id ([a-z0-9][a-z0-9_-]*)".into()));
            }
            Ok(Command::AuditExport { since_ms, until_ms, format, principal, out })
        }
        "audit" => {
            let (mut since_ms, mut limit) = (None, None);
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--since-ms" => {
                        let raw = rest.get(i + 1).ok_or(CliError::MissingFlagValue("--since-ms"))?;
                        since_ms = Some(raw.parse::<i64>().map_err(|_| CliError::BadInput("--since-ms takes milliseconds".into()))?);
                        i += 2;
                    }
                    "--limit" => {
                        let raw = rest.get(i + 1).ok_or(CliError::MissingFlagValue("--limit"))?;
                        limit = Some(
                            raw.parse::<u32>().ok().filter(|n| (1..=500).contains(n)).ok_or_else(|| CliError::BadInput("--limit takes 1 to 500".into()))?,
                        );
                        i += 2;
                    }
                    other => return Err(CliError::UnknownSubcommand { command: "audit", got: other.to_string() }),
                }
            }
            Ok(Command::Audit { since_ms, limit })
        }
        // E18: access grants and teams. Changes are the operator's (SHOGUN_ADMIN_TOKEN).
        "acl" => {
            use crate::command::AclCommand;
            let json_body = |what: &'static str| -> Result<String, CliError> {
                let raw = join(&rest[1..]).ok_or(CliError::MissingArgument("<json>"))?;
                let value: serde_json::Value =
                    serde_json::from_str(&raw).map_err(|_| CliError::BadInput(format!("acl {what} takes one JSON object")))?;
                if !value.is_object() {
                    return Err(CliError::BadInput(format!("acl {what} takes one JSON object")));
                }
                Ok(value.to_string())
            };
            match rest.first().map(String::as_str) {
                None | Some("list") => Ok(Command::Acl(AclCommand::List)),
                Some(op @ ("grant" | "revoke")) => {
                    let body = json_body(if op == "grant" { "grant" } else { "revoke" })?;
                    let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| CliError::BadInput("bad JSON".into()))?;
                    shogun_mcp::governance_api::acl_change(&value, op == "grant").map_err(CliError::BadInput)?;
                    Ok(Command::Acl(if op == "grant" { AclCommand::Grant { body } } else { AclCommand::Revoke { body } }))
                }
                Some("teams") => Ok(Command::Acl(AclCommand::Teams)),
                Some("team") => Ok(Command::Acl(AclCommand::SetTeam { body: json_body("team")? })),
                Some(other) => Err(CliError::UnknownSubcommand { command: "acl", got: other.to_string() }),
            }
        }
        // E6-T2: one `memory.fs` command as JSON, validated by the shared parser.
        "fs" => {
            let raw = join(rest).ok_or(CliError::MissingArgument("<json>"))?;
            let body: serde_json::Value =
                serde_json::from_str(&raw).map_err(|_| CliError::BadInput("fs takes one JSON object".into()))?;
            shogun_mcp::memfs_tool::parse(&body).map_err(CliError::BadInput)?;
            Ok(Command::Fs { body: body.to_string() })
        }
        // E46: payment mandates and payments, the tool's arguments as one JSON object (the server
        // validates the terms; the CLI only refuses what is not an object).
        "payments" => {
            use shogun_mcp::memory_api::Tool;
            let words: Vec<&str> = rest.iter().map(String::as_str).collect();
            let (tool, json_at) = match words.as_slice() {
                ["mandates", "list", ..] => (Tool::PaymentsMandatesList, 2),
                ["mandates", "request", ..] => (Tool::PaymentsMandatesRequest, 2),
                ["mandates", "revoke", ..] => (Tool::PaymentsMandatesRevoke, 2),
                ["spend", ..] => (Tool::PaymentsSpend, 1),
                ["mandates", other, ..] => {
                    return Err(CliError::UnknownSubcommand { command: "payments mandates", got: (*other).to_string() })
                }
                ["mandates"] => return Err(CliError::MissingArgument("list|request|revoke")),
                [other, ..] => return Err(CliError::UnknownSubcommand { command: "payments", got: (*other).to_string() }),
                [] => return Err(CliError::MissingArgument("mandates|spend")),
            };
            let body = match rest.get(json_at..).and_then(join) {
                None if tool == Tool::PaymentsMandatesList => None,
                None => return Err(CliError::MissingArgument("<json>")),
                Some(raw) => {
                    let value: serde_json::Value = serde_json::from_str(&raw)
                        .ok()
                        .filter(serde_json::Value::is_object)
                        .ok_or_else(|| CliError::BadInput("payments takes one JSON object: the tool's arguments".into()))?;
                    Some(value.to_string())
                }
            };
            Ok(Command::Payments { tool, body })
        }
        // E35: the A2A handoff, one JSON object each, validated by the shared parser.
        "handoff" => {
            let sub = rest.first().map(String::as_str);
            let raw = rest.get(1..).and_then(join).ok_or(CliError::MissingArgument("<json>"));
            match sub {
                Some("create") => {
                    let body: serde_json::Value = serde_json::from_str(&raw?)
                        .map_err(|_| CliError::BadInput("handoff create takes one JSON object".into()))?;
                    shogun_mcp::handoff_api::create_request(&body).map_err(CliError::BadInput)?;
                    Ok(Command::HandoffCreate { body: body.to_string() })
                }
                Some("accept") => {
                    let body: serde_json::Value = serde_json::from_str(&raw?)
                        .map_err(|_| CliError::BadInput("handoff accept takes one JSON object".into()))?;
                    shogun_mcp::handoff_api::accept_request(&body, i64::MIN).map_err(CliError::BadInput)?;
                    Ok(Command::HandoffAccept { body: body.to_string() })
                }
                Some(other) => Err(CliError::UnknownSubcommand { command: "handoff", got: other.to_string() }),
                None => Err(CliError::MissingArgument("create|accept")),
            }
        }
        "recall" => {
            let mut args = serde_json::Map::new();
            let mut words = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--budget-tokens" => {
                        let value = rest.get(i + 1).ok_or_else(|| CliError::BadInput("--budget-tokens needs a value".into()))?;
                        args.insert("budget_tokens".into(), value.parse::<u64>().map_err(|_| CliError::BadId(value.clone()))?.into());
                        i += 2;
                    }
                    "--include-scratch" => {
                        args.insert("include_scratch".into(), true.into());
                        i += 1;
                    }
                    other if other.starts_with("--") => {
                        return Err(CliError::BadInput(format!("unknown recall option `{other}`")))
                    }
                    word => {
                        words.push(word.to_string());
                        i += 1;
                    }
                }
            }
            let query = words.join(" ");
            if query.trim().is_empty() {
                return Err(CliError::MissingArgument("<query>"));
            }
            args.insert("query".into(), query.into());
            let body = serde_json::Value::Object(args);
            shogun_mcp::ingest_api::recall_request(&body).map_err(CliError::BadInput)?;
            Ok(Command::Recall { body: body.to_string() })
        }
        "people" => Ok(Command::People(parse_list_or_get("people", rest)?)),
        "projects" => Ok(Command::Projects(parse_list_or_get("projects", rest)?)),
        "commitments" => Ok(Command::Commitments(parse_list_or_get(
            "commitments",
            rest,
        )?)),
        "open-loops" => Ok(Command::OpenLoops(parse_list_or_get("open-loops", rest)?)),
        "memory-notes" => match rest.first().map(String::as_str) {
            None | Some("list") => Ok(Command::MemoryNotes { id: None }),
            Some("show") => {
                let id=rest.get(1).and_then(|s| s.parse::<i64>().ok()).filter(|id| *id>0).ok_or(CliError::MissingArgument("<id>"))?;
                Ok(Command::MemoryNotes { id: Some(id) })
            },
            Some("change") => {
                let body=join(&rest[1..]).ok_or(CliError::MissingArgument("<change-json>"))?;
                serde_json::from_str::<shogun_mcp::memory_notes_api::NoteChange>(&body).map_err(|_| CliError::BadInput("invalid memory note change".into()))?;
                Ok(Command::MemoryNoteChange { body })
            },
            _ => Err(CliError::BadInput("use memory-notes list|show <id>|change <change-json>".into())),
        },
        "note" => {
            let text = join(rest).ok_or(CliError::MissingArgument("<text>"))?;
            Ok(Command::Note { text })
        }

        "run" => {
            // The remaining args form the action JSON spec (e.g. '{"kind":"local_search",...}').
            let agent = join(rest).ok_or(CliError::MissingArgument("<action-json>"))?;
            Ok(Command::Run { agent })
        }
        "actions" => match rest.first().map(String::as_str) {
            Some("poll") => {
                let id = rest
                    .get(1)
                    .ok_or(CliError::MissingArgument("<approval_id>"))?;
                Ok(Command::ActionsPoll {
                    approval_id: id.parse().map_err(|_| CliError::BadId(id.clone()))?,
                })
            }
            // Issue #394: the Clarify questions and their answer.
            Some("questions") => match rest.get(1) {
                None => Ok(Command::ActionsQuestions { id: None }),
                Some(raw) => match raw.parse::<i64>() {
                    Ok(id) if id > 0 => Ok(Command::ActionsQuestions { id: Some(id) }),
                    _ => Err(CliError::BadId(raw.clone())),
                },
            },
            Some("answer") => {
                let raw = rest.get(1).ok_or(CliError::MissingArgument("<question_id>"))?;
                let id = match raw.parse::<i64>() {
                    Ok(id) if id > 0 => id,
                    _ => return Err(CliError::BadId(raw.clone())),
                };
                let answer = join(rest.get(2..).unwrap_or_default())
                    .ok_or(CliError::MissingArgument("<answer>"))?;
                Ok(Command::ActionsAnswer { id, answer })
            }
            Some("steer") => {
                let kind = rest.get(1).ok_or(CliError::MissingArgument("<chat|notch>"))?;
                if !matches!(kind.as_str(), "chat" | "notch") {
                    return Err(CliError::UnknownSubcommand {
                        command: "actions steer",
                        got: kind.clone(),
                    });
                }
                let raw = rest.get(2).ok_or(CliError::MissingArgument("<run_id>"))?;
                let id = match raw.parse::<i64>() {
                    Ok(id) if id > 0 => id,
                    _ => return Err(CliError::BadId(raw.clone())),
                };
                let text = join(rest.get(3..).unwrap_or_default())
                    .ok_or(CliError::MissingArgument("<text>"))?;
                Ok(Command::ActionsSteer { kind: kind.clone(), id, text })
            }
            other => Err(CliError::UnknownSubcommand {
                command: "actions",
                got: other.unwrap_or("").to_string(),
            }),
        },
        "api" => match rest.first().map(String::as_str) {
            Some("status") => Ok(Command::ApiStatus),
            Some("tokens") => Ok(Command::ApiTokens),
            Some("restrict") => Ok(Command::ApiRestrict {
                client: rest.get(1).cloned().ok_or(CliError::MissingArgument("<client>"))?,
            }),
            other => Err(CliError::UnknownSubcommand {
                command: "api",
                got: other.unwrap_or("").to_string(),
            }),
        },
        "metrics" => Ok(Command::Metrics),
        "whoami" => Ok(Command::Whoami),
        "profile" => match rest.first().map(String::as_str) {
            Some("set") => Ok(Command::ProfileSet {
                body: join(&rest[1..]).ok_or(CliError::MissingArgument("<json>"))?,
            }),
            other => Err(CliError::UnknownSubcommand {
                command: "profile",
                got: other.unwrap_or("").to_string(),
            }),
        },
        "language" => match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] | ["show"] => Ok(Command::Language { set: None }),
            ["set", code] => Ok(Command::Language { set: Some((*code).to_string()) }),
            _ => Err(CliError::MissingArgument("language [show | set en|ja|es|de]")),
        },
        "forms" => parse_forms(rest),
        "translate" => match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["register"] | ["register", "show"] => {
                Ok(Command::TranslateRegister(TranslateRegisterCommand::Show))
            }
            ["register", "set", formality] => Ok(Command::TranslateRegister(
                TranslateRegisterCommand::Set { formality: (*formality).to_string(), app: None },
            )),
            ["register", "set", formality, "--app", app] if !app.trim().is_empty() => {
                Ok(Command::TranslateRegister(TranslateRegisterCommand::Set {
                    formality: (*formality).to_string(),
                    app: Some((*app).to_string()),
                }))
            }
            ["register", "set", ..] => Err(CliError::MissingArgument(
                "translate register set match|casual|polite|formal [--app <bundle-id>]",
            )),
            ["text", ..] | ["reply", ..] => parse_translate_request(rest),
            ["status", id] => Ok(Command::Translate(TranslateCommand::Status {
                id: id.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId((*id).to_string()))?,
            })),
            ["status"] => Err(CliError::MissingArgument("translate status <id>")),
            ["pair"] | ["pair", "show"] => Ok(Command::Translate(TranslateCommand::PairShow)),
            ["pair", "reset"] => Ok(Command::Translate(TranslateCommand::PairReset)),
            ["pair", "set", mine] => Ok(Command::Translate(TranslateCommand::PairSet {
                mine: (*mine).to_string(),
                theirs: None,
            })),
            ["pair", "set", mine, theirs] => Ok(Command::Translate(TranslateCommand::PairSet {
                mine: (*mine).to_string(),
                theirs: Some((*theirs).to_string()),
            })),
            ["pair", ..] => Err(CliError::MissingArgument("translate pair [show] | set en|ja [en|ja] | reset")),
            [other, ..] => Err(CliError::UnknownSubcommand { command: "translate", got: (*other).to_string() }),
            [] => Err(CliError::MissingArgument("text|reply|status|pair|register")),
        },
        "writing-style" => match rest {
            [action] if matches!(action.as_str(), "show" | "enable" | "disable" | "clear") => Ok(Command::WritingVoice { action: action.clone(), id: None }),
            [action, id] if matches!(action.as_str(), "note-delete" | "note-confirm" | "note-reject") => {
                let id = id.parse::<i64>().ok().filter(|id| *id > 0).ok_or(CliError::MissingArgument("writing-style note-delete|note-confirm|note-reject <id>"))?;
                Ok(Command::WritingVoice { action: action.replace('-', "_"), id: Some(id) })
            }
            _ => Err(CliError::MissingArgument("writing-style show|enable|disable|clear|note-delete <id>|note-confirm <id>|note-reject <id>")),
        },
        "agent-source" => match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] | ["show"] => Ok(Command::AgentSource(AgentSourceCommand::Show)),
            ["revoke"] => Ok(Command::AgentSource(AgentSourceCommand::Revoke)),
            ["use", source] => Ok(Command::AgentSource(AgentSourceCommand::Use {
                source: (*source).to_string(),
                model: None,
            })),
            ["use", source, "--model", model] => Ok(Command::AgentSource(AgentSourceCommand::Use {
                source: (*source).to_string(),
                model: Some((*model).to_string()),
            })),
            ["use", ..] => Err(CliError::MissingArgument("agent-source use <source> [--model <id>]")),
            ["openai-store", "on"] => Ok(Command::AgentSource(AgentSourceCommand::OpenAiStore(true))),
            ["openai-store", "off"] => Ok(Command::AgentSource(AgentSourceCommand::OpenAiStore(false))),
            ["openai-store", ..] => Err(CliError::MissingArgument("agent-source openai-store on|off")),
            [other, ..] => Err(CliError::UnknownSubcommand { command: "agent-source", got: (*other).to_string() }),
        },
        "connections" => match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] | ["list"] => Ok(Command::Connections(ConnectionsCommand::List)),
            ["disconnect", service] => Ok(Command::Connections(ConnectionsCommand::Disconnect {
                service: (*service).to_string(),
            })),
            ["disconnect", ..] => Err(CliError::MissingArgument(
                "connections disconnect <service>",
            )),
            ["gmail", "draft-only", "on"] => Ok(Command::Connections(
                ConnectionsCommand::GmailDraftOnly(true),
            )),
            ["gmail", "draft-only", "off"] => Ok(Command::Connections(
                ConnectionsCommand::GmailDraftOnly(false),
            )),
            ["gmail", ..] => Err(CliError::MissingArgument(
                "connections gmail draft-only on|off",
            )),
            ["composio", "revoke"] => Ok(Command::Connections(ConnectionsCommand::ComposioRevoke)),
            ["composio", "draft-only", "on"] => {
                Ok(Command::Connections(ConnectionsCommand::ComposioDraftOnly(true)))
            }
            ["composio", "draft-only", "off"] => {
                Ok(Command::Connections(ConnectionsCommand::ComposioDraftOnly(false)))
            }
            ["composio", ..] => Err(CliError::MissingArgument(
                "connections composio revoke | draft-only on|off",
            )),
            [other, ..] => Err(CliError::UnknownSubcommand { command: "connections", got: (*other).to_string() }),
        },
        "wrap" => Ok(Command::Wrap),
        "calendar" => match rest.first().map(String::as_str) {
            None | Some("upcoming") | Some("--page-size") => {
                let flags = if rest.first().is_some_and(|v| v == "upcoming") { &rest[1..] } else { rest };
                let page_size = match flags {
                    [] => None,
                    [flag, value] if flag == "--page-size" => Some(value.parse::<usize>()
                        .ok().filter(|n| (1..=100).contains(n))
                        .ok_or(CliError::MissingArgument("--page-size 1..100"))?),
                    _ => return Err(CliError::MissingArgument("calendar upcoming [--page-size 1..100]")),
                };
                Ok(Command::Calendar { page_size })
            },
            Some(other) => Err(CliError::UnknownSubcommand { command: "calendar", got: other.to_string() }),
        },
        "onboarding" => Ok(Command::Onboarding),
        "account" => Ok(Command::Account),
        "release-notes" => Ok(Command::ReleaseNotes),
        "update" => match rest.first().map(String::as_str) {
            None | Some("status") => Ok(Command::UpdateStatus),
            Some(other) => Err(CliError::UnknownSubcommand { command: "update", got: other.to_string() }),
        },
        "dream" => match rest.first().map(String::as_str) {
            None | Some("status") => Ok(Command::DreamStatus),
            Some("retry") => Ok(Command::DreamRetry),
            Some(other) => Err(CliError::UnknownSubcommand { command: "dream", got: other.to_string() }),
        },
        "processing" => match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["status", raw] => {
                let id = raw.parse::<i64>().ok().filter(|id| *id > 0)
                    .ok_or_else(|| CliError::BadId((*raw).to_string()))?;
                Ok(Command::ProcessingStatus { id })
            }
            _ => Err(CliError::MissingArgument("processing status <event-id>")),
        },
        "capture" => match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] | ["status"] => Ok(Command::Capture { paused: None, minutes: None }),
            ["pause"] => Ok(Command::Capture { paused: Some(true), minutes: None }),
            ["pause", "15m"] => Ok(Command::Capture { paused: Some(true), minutes: Some(15) }),
            ["pause", "30m"] => Ok(Command::Capture { paused: Some(true), minutes: Some(30) }),
            ["pause", "1h"] => Ok(Command::Capture { paused: Some(true), minutes: Some(60) }),
            ["resume"] => Ok(Command::Capture { paused: Some(false), minutes: None }),
            _ => Err(CliError::UnknownSubcommand { command: "capture", got: rest.join(" ") }),
        },
        "lessons" => parse_lessons(rest),
        "patterns" => {
            use crate::command::PatternsCommand;
            match rest.first().map(String::as_str) {
                None | Some("list") => Ok(Command::Patterns(PatternsCommand::List)),
                Some(cmd @ ("enable" | "disable")) => {
                    let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
                    let id = raw.parse().map_err(|_| CliError::BadId(raw.clone()))?;
                    Ok(Command::Patterns(if cmd == "enable" {
                        PatternsCommand::Enable { id }
                    } else {
                        PatternsCommand::Disable { id }
                    }))
                }
                Some(other) => Err(CliError::UnknownSubcommand {
                    command: "patterns",
                    got: other.to_string(),
                }),
            }
        }
        "disputes" => parse_disputes(rest),
        "state" => parse_state(rest),
        "import" => parse_import(rest),
        "skills" | "skill" => parse_skills(rest),
        "export" => parse_export(rest),
        "schedule" | "schedules" => parse_schedule(rest),
        "web" => parse_web(rest),
        "support" => match rest.first().map(String::as_str) {
            None | Some("reports") => Ok(Command::SupportReports),
            Some(other) => Err(CliError::UnknownSubcommand { command: "support", got: other.to_string() }),
        },
        "visual-recall" => parse_visual_recall(rest),
        "backup" | "backups" => parse_backup(rest),
        "search-index" => {
            use crate::command::SearchIndexCommand;
            let command = match rest.first().map(String::as_str) {
                None | Some("status") => SearchIndexCommand::Status,
                Some("compact") | Some("start") => SearchIndexCommand::Compact,
                Some("stop") => SearchIndexCommand::Stop,
                Some(other) => {
                    return Err(CliError::UnknownSubcommand {
                        command: "search-index",
                        got: other.to_string(),
                    })
                }
            };
            Ok(Command::SearchIndex(command))
        }
        "storage" => match rest.first().map(String::as_str) {
            Some("preview") => Ok(Command::StoragePreview),
            _ => Err(CliError::MissingArgument("preview")),
        },
        "capture-exclusions" => match rest.first().map(String::as_str) {
            None | Some("list") => Ok(Command::CaptureExclusions(CaptureExclusionsCommand::List)),
            Some("set") => Ok(Command::CaptureExclusions(CaptureExclusionsCommand::Set {
                body: join(&rest[1..]).ok_or(CliError::MissingArgument("<json>"))?,
            })),
            Some(other) => Err(CliError::UnknownSubcommand { command: "capture-exclusions", got: other.to_string() }),
        },
        "voice-dictionary" => match rest.first().map(String::as_str) {
            Some("autoadd") => Ok(Command::VoiceDictionary(VoiceDictionaryCommand::AutoAdd)),
            Some("autoadd-manage") => Ok(Command::VoiceDictionary(VoiceDictionaryCommand::ManageAutoAdd {
                action: join(&rest[1..]).ok_or(CliError::MissingArgument("<action-json>"))?,
            })),
            Some("list") | None => Ok(Command::VoiceDictionary(VoiceDictionaryCommand::List)),
            Some("create") => Ok(Command::VoiceDictionary(VoiceDictionaryCommand::Create {
                term: join(&rest[1..]).ok_or(CliError::MissingArgument("<term-json>"))?,
            })),
            Some("update") => {
                let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
                Ok(Command::VoiceDictionary(VoiceDictionaryCommand::Update {
                    id: raw.parse().map_err(|_| CliError::BadId(raw.clone()))?,
                    term: join(&rest[2..]).ok_or(CliError::MissingArgument("<term-json>"))?,
                }))
            }
            Some("delete") => {
                let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
                Ok(Command::VoiceDictionary(VoiceDictionaryCommand::Delete {
                    id: raw.parse().map_err(|_| CliError::BadId(raw.clone()))?,
                }))
            }
            Some("import") => {
                let raw = rest.get(1).ok_or(CliError::MissingArgument("<file|->"))?;
                let source = if raw == "-" {
                    ImportSource::Stdin
                } else {
                    ImportSource::Path(raw.clone())
                };
                Ok(Command::VoiceDictionary(VoiceDictionaryCommand::Import { source }))
            }
            Some("export") => Ok(Command::VoiceDictionary(VoiceDictionaryCommand::Export)),
            Some(other) => Err(CliError::UnknownSubcommand {
                command: "voice-dictionary",
                got: other.to_string(),
            }),
        },
        "wiki" => {
            if rest == ["list"] { return Ok(Command::WikiList); }
            let action = rest.first().map(String::as_str).unwrap_or("");
            if rest.len() != 2 || !matches!(action, "evidence" | "neighbors") {
                return Err(CliError::UnknownSubcommand { command: "wiki", got: rest.join(" ") });
            }
            let key = rest[1].clone();
            if shogun_mcp::backend::parse_knowledge_key(&key).is_none() {
                return Err(CliError::BadInput(format!(
                    "invalid wiki key: `{key}` (expected person-N | project-N | knowledge-N)"
                )));
            }
            Ok(if action == "evidence" { Command::WikiEvidence { key } } else { Command::WikiNeighbors { key } })
        },
        "meeting" => match rest.first().map(String::as_str) {
            Some("list") => {
                if rest.len() > 2 {
                    return Err(CliError::UnknownSubcommand { command: "meeting list", got: rest[2..].join(" ") });
                }
                let before_id = rest.get(1).map(|raw| raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.clone()))).transpose()?;
                Ok(Command::MeetingList { before_id })
            },
            Some("audio") if rest.len() == 1 || (rest.len() == 2 && rest[1] == "status") => Ok(Command::MeetingSystemAudioStatus),
            Some("audio") if rest.len() == 2 && rest[1] == "retry" => Ok(Command::MeetingSystemAudioRetry),
            Some("audio") => Err(CliError::UnknownSubcommand { command: "meeting audio", got: rest[1..].join(" ") }),
            Some("start") if rest.len() == 1 => Ok(Command::MeetingRecordNow { in_person: false }),
            Some("start") if rest.len() == 2 && rest[1] == "--in-person" => {
                Ok(Command::MeetingRecordNow { in_person: true })
            }
            Some("start") => Err(CliError::UnknownSubcommand {
                command: "meeting start",
                got: rest[1..].join(" "),
            }),
            Some("poll") => {
                let raw = rest
                    .get(1)
                    .ok_or(CliError::MissingArgument("<approval_id>"))?;
                if rest.len() != 2 {
                    return Err(CliError::UnknownSubcommand {
                        command: "meeting poll",
                        got: rest[2..].join(" "),
                    });
                }
                let id: u64 = raw.parse().map_err(|_| CliError::BadId(raw.clone()))?;
                if id == 0 {
                    return Err(CliError::BadId(raw.clone()));
                }
                Ok(Command::MeetingStartStatus { id })
            }
            Some("speakers") => parse_speakers(&rest[1..]),
            Some("templates") => parse_templates(&rest[1..]),
            Some("track") => parse_track(&rest[1..]),
            Some("prefs") => parse_recap_prefs(&rest[1..]),
            Some("search") => Ok(Command::MeetingSearch {
                query: join(&rest[1..]).ok_or(CliError::MissingArgument("<words>"))?,
            }),
            Some("delete") => {
                let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
                if rest.len() > 2 {
                    return Err(CliError::UnknownSubcommand { command: "meeting delete", got: rest[2..].join(" ") });
                }
                let session_id = raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.clone()))?;
                Ok(Command::MeetingDelete { session_id })
            }
            Some("recap") if matches!(rest.get(1).map(String::as_str), Some("edit" | "reset")) => {
                let reset = rest[1] == "reset";
                // `[id]` is optional: a positional that parses as a positive integer is the
                // session, anything else (for `edit`) is the start of the JSON body.
                let mut rest_after = &rest[2..];
                let session_id = match rest_after.first() {
                    Some(raw) => match raw.parse::<i64>() {
                        Ok(id) if id > 0 => {
                            rest_after = &rest_after[1..];
                            Some(id)
                        }
                        Ok(_) => return Err(CliError::BadId(raw.clone())),
                        Err(_) => None,
                    },
                    None => None,
                };
                let body = if reset {
                    if !rest_after.is_empty() {
                        return Err(CliError::UnknownSubcommand {
                            command: "meeting recap reset",
                            got: rest_after.join(" "),
                        });
                    }
                    RecapEditBody::Reset
                } else {
                    RecapEditBody::Json(join(rest_after).ok_or(CliError::MissingArgument("<json>"))?)
                };
                Ok(Command::MeetingRecapEdit { session_id, body })
            }
            Some(read @ ("recap" | "transcript")) => {
                if rest.len() > 2 {
                    return Err(CliError::UnknownSubcommand {
                        command: if read == "recap" { "meeting recap" } else { "meeting transcript" },
                        got: rest[2..].join(" "),
                    });
                }
                let session_id = rest
                    .get(1)
                    .map(|raw| raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.clone())))
                    .transpose()?;
                Ok(if read == "recap" {
                    Command::MeetingRecap { session_id }
                } else {
                    Command::MeetingTranscript { session_id }
                })
            }
            Some("export") => {
                let ids: Vec<_> = rest[1..].iter().filter(|arg| *arg != "--transcript").collect();
                if ids.len() > 1 {
                    return Err(CliError::UnknownSubcommand { command: "meeting export", got: rest[1..].join(" ") });
                }
                let session_id = ids.first().map(|raw| raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.to_string()))).transpose()?;
                Ok(Command::MeetingExport {
                    transcript: rest[1..].iter().any(|a| a == "--transcript"), session_id,
                })
            }
            Some("reminder") => match rest[1..]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                [] | ["show"] => Ok(Command::MeetingReminder { minutes: None }),
                ["off"] | ["set", "off"] => Ok(Command::MeetingReminder { minutes: Some(0) }),
                ["set", raw @ ("0" | "1" | "5" | "10")] => Ok(Command::MeetingReminder {
                    minutes: raw.parse().ok(),
                }),
                _ => Err(CliError::UnknownSubcommand {
                    command: "meeting reminder",
                    got: rest[1..].join(" "),
                }),
            },
            _ => parse_microphone("meeting", "meeting microphone", rest)
                .map(Command::MeetingMicrophone),
        },
        "voice" => parse_microphone("voice", "voice microphone", rest).map(Command::VoiceMicrophone),
        "mail" => parse_mail(rest),
        "config" => {
            let action = match rest.first().map(String::as_str) {
                Some("path") => ConfigAction::Path,
                Some("show") => ConfigAction::Show,
                Some("validate") => ConfigAction::Validate,
                Some(other) => {
                    return Err(CliError::UnknownSubcommand {
                        command: "config",
                        got: other.to_string(),
                    })
                }
                None => return Err(CliError::MissingArgument("path|show|validate")),
            };
            Ok(Command::Config { action })
        }
        "principal" => parse_principal(rest),
        other => Err(CliError::UnknownCommand(other.to_string())),
    }
}

/// `disputes [list]` | `disputes resolve <id> <keep_earlier|keep_later|dismiss_both|keep_both>`
/// (issue #253). The resolution is checked here so a typo never reaches the daemon.
/// `schedule [list]|add <skill> <days> <HH:MM> [input…]|remove <id>`.
fn parse_schedule(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::ScheduleCommand;
    let arg = |i: usize, what: &'static str| rest.get(i).cloned().ok_or(CliError::MissingArgument(what));
    match rest.first().map(String::as_str) {
        None | Some("list") => Ok(Command::Schedule(ScheduleCommand::List)),
        Some("add") => Ok(Command::Schedule(ScheduleCommand::Add {
            skill: arg(1, "<skill>")?,
            days: arg(2, "<days>")?,
            at: arg(3, "<HH:MM>")?,
            input: rest.get(4..).map(|w| w.join(" ")).unwrap_or_default(),
        })),
        Some("remove") | Some("rm") => {
            let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
            let id = raw
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| CliError::BadId(raw.clone()))?;
            Ok(Command::Schedule(ScheduleCommand::Remove { id }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "schedule",
            got: other.to_string(),
        }),
    }
}

/// `export [meeting] [id] --to <folder> [--name <n>] [--csv] [--transcript]` |
/// `export status <id>` | `export history` | `export folders [remove <folder>]` (issue #397).
fn parse_export(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::ExportCommand;
    let positive_id = |raw: &String| {
        raw.parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| CliError::BadId(raw.clone()))
    };
    match rest.first().map(String::as_str) {
        Some("status") => {
            let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
            return Ok(Command::Export(ExportCommand::Status { id: positive_id(raw)? }));
        }
        Some("history") => return Ok(Command::Export(ExportCommand::History)),
        Some("folders") => {
            return match rest.get(1).map(String::as_str) {
                None | Some("list") => Ok(Command::Export(ExportCommand::Folders)),
                Some("remove") => Ok(Command::Export(ExportCommand::FolderRemove {
                    folder: absolute(rest.get(2).ok_or(CliError::MissingArgument("<folder>"))?),
                })),
                Some(other) => Err(CliError::UnknownSubcommand {
                    command: "export folders",
                    got: other.to_string(),
                }),
            };
        }
        _ => {}
    }
    let args = match rest.first().map(String::as_str) {
        Some("meeting") => &rest[1..],
        _ => rest,
    };
    let (mut session_id, mut folder, mut name, mut csv, mut transcript) = (None, None, None, false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                folder = Some(absolute(args.get(i + 1).ok_or(CliError::MissingFlagValue("--to"))?));
                i += 2;
            }
            "--name" => {
                name = Some(args.get(i + 1).ok_or(CliError::MissingFlagValue("--name"))?.clone());
                i += 2;
            }
            "--csv" => {
                csv = true;
                i += 1;
            }
            "--transcript" => {
                transcript = true;
                i += 1;
            }
            raw if session_id.is_none() && !raw.starts_with("--") => {
                session_id = Some(positive_id(&args[i])?);
                i += 1;
            }
            other => {
                return Err(CliError::UnknownSubcommand {
                    command: "export",
                    got: other.to_string(),
                })
            }
        }
    }
    Ok(Command::Export(ExportCommand::Meeting {
        session_id,
        folder: folder.ok_or(CliError::MissingArgument("--to <folder>"))?,
        name,
        csv,
        transcript,
    }))
}

/// A folder as the user typed it, made absolute against the working directory (the server only
/// accepts absolute paths; `.` in a terminal means "here").
fn absolute(raw: &str) -> String {
    let path = std::path::Path::new(raw);
    if path.is_absolute() {
        return raw.to_string();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path).display().to_string())
        .unwrap_or_else(|_| raw.to_string())
}

/// `skills [list]|show <name>|run <name> [input…]|status <id>`.
fn parse_web(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::WebCommand;
    let url = |rest: &[String]| rest.get(1).cloned().ok_or(CliError::MissingArgument("<url>"));
    match rest.first().map(String::as_str) {
        None => Err(CliError::MissingArgument("web subcommand")),
        Some("transcript") => Ok(Command::Web(WebCommand::Transcript { url: url(rest)? })),
        Some("post") => Ok(Command::Web(WebCommand::Post { url: url(rest)? })),
        Some("page") => Ok(Command::Web(WebCommand::Page { url: url(rest)? })),
        Some(other) => Err(CliError::UnknownSubcommand { command: "web", got: other.to_string() }),
    }
}

/// `import preview <package.json> | --computer-history <path>…` | `import commit <package.json> |
/// --computer-history <path>… (--select <id>… | --defaults)` | `import list` | `import show <id>` |
/// `import delete <id>` (issues #384 / #391). Only the paths named here are read — a folder
/// contributes the Markdown files directly inside it — and what reaches the daemon is their
/// content, never a path.
fn parse_import(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::ImportCommand;
    let id = |rest: &[String]| -> Result<i64, CliError> {
        let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
        raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.clone()))
    };
    // The request body from the named package file or Computer History paths, plus `select`.
    let body = |rest: &[String], commit: bool| -> Result<String, CliError> {
        let mut history: Vec<std::path::PathBuf> = Vec::new();
        let mut package: Option<String> = None;
        let mut select: Vec<String> = Vec::new();
        let mut defaults = false;
        let mut people = false;
        let mut in_history = false;
        let mut i = 1;
        while i < rest.len() {
            match rest[i].as_str() {
                "--computer-history" => in_history = true,
                "--select" if commit => {
                    in_history = false;
                    select.push(rest.get(i + 1).cloned().ok_or(CliError::MissingFlagValue("--select"))?);
                    i += 1;
                }
                "--people-to-state" if commit => {
                    in_history = false;
                    people = true;
                }
                "--defaults" if commit => {
                    in_history = false;
                    defaults = true;
                }
                flag if flag.starts_with("--") => {
                    return Err(CliError::UnknownSubcommand { command: "import", got: flag.to_string() })
                }
                path if in_history => history.push(std::path::PathBuf::from(path)),
                path if package.is_none() => package = Some(path.to_string()),
                extra => return Err(CliError::UnknownSubcommand { command: "import", got: extra.to_string() }),
            }
            i += 1;
        }
        let mut body = match (package, history.is_empty()) {
            (Some(file), true) => {
                let text = std::fs::read_to_string(&file)
                    .map_err(|_| CliError::BadInput("invalid path: could not read the package file".into()))?;
                serde_json::json!({ "package": text })
            }
            (None, false) => {
                let files = shogun_mcp::memory_import_api::read_explicit_paths(&history)
                    .map_err(|e| CliError::BadInput(e.to_string()))?;
                serde_json::json!({ "computer_history": files
                    .iter()
                    .map(|f| serde_json::json!({ "name": f.name, "markdown": f.markdown }))
                    .collect::<Vec<_>>() })
            }
            (Some(_), false) => {
                return Err(CliError::BadInput("give a package file or --computer-history paths, not both".into()))
            }
            (None, true) => return Err(CliError::MissingArgument("<package.json> | --computer-history <path>…")),
        };
        if commit {
            body["select"] = match (defaults, select.is_empty()) {
                (true, true) => serde_json::json!("default"),
                (false, false) => serde_json::json!(select),
                (true, false) => return Err(CliError::BadInput("use --select or --defaults, not both".into())),
                (false, true) => return Err(CliError::MissingArgument("--select <id>… | --defaults")),
            };
            if people {
                body["people_to_state"] = serde_json::json!(true);
            }
        }
        Ok(body.to_string())
    };
    match rest.first().map(String::as_str) {
        Some("preview") => Ok(Command::Import(ImportCommand::Preview { body: body(rest, false)? })),
        Some("commit") => Ok(Command::Import(ImportCommand::Commit { body: body(rest, true)? })),
        None | Some("list") => Ok(Command::Import(ImportCommand::List)),
        Some("show") => Ok(Command::Import(ImportCommand::Show { id: id(rest)? })),
        Some("delete") => Ok(Command::Import(ImportCommand::Delete { id: id(rest)? })),
        Some(other) => Err(CliError::UnknownSubcommand { command: "import", got: other.to_string() }),
    }
}

fn parse_skills(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::SkillsCommand;
    let name = |rest: &[String]| rest.get(1).cloned().ok_or(CliError::MissingArgument("<name>"));
    match rest.first().map(String::as_str) {
        None | Some("list") => Ok(Command::Skills(SkillsCommand::List)),
        Some("show") => Ok(Command::Skills(SkillsCommand::Show { name: name(rest)? })),
        Some("run") => Ok(Command::Skills(SkillsCommand::Run {
            name: name(rest)?,
            input: rest.get(2..).map(|w| w.join(" ")).unwrap_or_default(),
        })),
        Some("status") => {
            let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
            let id = raw
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| CliError::BadId(raw.clone()))?;
            Ok(Command::Skills(SkillsCommand::Status { id }))
        }
        Some("save") => {
            let file = rest.get(1).ok_or(CliError::MissingArgument("<file.md>"))?;
            let markdown = std::fs::read_to_string(file)
                .map_err(|_| CliError::BadInput(format!("could not read the skill file `{file}`")))?;
            Ok(Command::Skills(SkillsCommand::Save { markdown }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "skills",
            got: other.to_string(),
        }),
    }
}

/// `state edit <table> <id> [--name T] [--summary T|--clear-summary] [--description T]
/// [--status S] [--due <ms>|--no-due]` | `state delete <table> <id>` | `state export` |
/// `state evidence <people|projects> <id>` | `state split <people|projects> <id> --name T
/// --event <id>… [--alias A…]` (FR-ST-10)
/// (issue #290). Which field fits which table is checked by the daemon's one parser.
fn parse_state(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::StateCommand;
    const TABLE: &str = "<people|projects|commitments|open-loops>";
    let target = |rest: &[String]| -> Result<(String, i64), CliError> {
        let table = match rest.get(1).map(String::as_str) {
            Some(t @ ("people" | "projects" | "commitments")) => t.to_string(),
            Some("open-loops" | "open_loops") => "open_loops".to_string(),
            _ => return Err(CliError::MissingArgument(TABLE)),
        };
        let raw = rest.get(2).ok_or(CliError::MissingArgument("<id>"))?;
        let id = raw
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| CliError::BadId(raw.clone()))?;
        Ok((table, id))
    };
    // A split (and the evidence it chooses from) applies to people and projects only.
    let splittable = |rest: &[String]| -> Result<(String, i64), CliError> {
        match rest.get(1).map(String::as_str) {
            Some("people" | "projects") => target(rest),
            _ => Err(CliError::MissingArgument("<people|projects>")),
        }
    };
    match rest.first().map(String::as_str) {
        Some("edit") => {
            let (table, id) = target(rest)?;
            let mut body = serde_json::Map::new();
            body.insert("table".into(), serde_json::json!(table));
            body.insert("id".into(), serde_json::json!(id));
            let mut i = 3;
            while i < rest.len() {
                let flag = rest[i].as_str();
                let value = |name: &'static str| {
                    rest.get(i + 1).cloned().ok_or(CliError::MissingFlagValue(name))
                };
                match flag {
                    "--name" => {
                        body.insert("name".into(), serde_json::json!(value("--name")?));
                        i += 2;
                    }
                    "--summary" => {
                        body.insert("summary".into(), serde_json::json!(value("--summary")?));
                        i += 2;
                    }
                    "--channel" => {
                        body.insert("channel".into(), serde_json::json!(value("--channel")?));
                        i += 2;
                    }
                    "--aliases" | "--emails" | "--handles" => {
                        // Comma-separated; replaces the stored list.
                        let list: Vec<String> = value("--list")?
                            .split(',')
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .collect();
                        body.insert(flag.trim_start_matches("--").into(), serde_json::json!(list));
                        i += 2;
                    }
                    "--clear-summary" => {
                        body.insert("summary".into(), serde_json::Value::Null);
                        i += 1;
                    }
                    "--description" => {
                        body.insert("description".into(), serde_json::json!(value("--description")?));
                        i += 2;
                    }
                    "--status" => {
                        body.insert("status".into(), serde_json::json!(value("--status")?));
                        i += 2;
                    }
                    "--due" => {
                        let raw = value("--due")?;
                        let ms = raw.parse::<i64>().map_err(|_| CliError::BadId(raw.clone()))?;
                        body.insert("due_at".into(), serde_json::json!(ms));
                        i += 2;
                    }
                    "--no-due" => {
                        body.insert("due_at".into(), serde_json::Value::Null);
                        i += 1;
                    }
                    "--snooze" => {
                        let raw = value("--snooze")?;
                        let ms = raw.parse::<i64>().map_err(|_| CliError::BadId(raw.clone()))?;
                        body.insert("deferred_until".into(), serde_json::json!(ms));
                        i += 2;
                    }
                    "--no-snooze" => {
                        body.insert("deferred_until".into(), serde_json::Value::Null);
                        i += 1;
                    }
                    other => {
                        return Err(CliError::UnknownSubcommand {
                            command: "state edit",
                            got: other.to_string(),
                        })
                    }
                }
            }
            if body.len() == 2 {
                return Err(CliError::MissingArgument(
                    "--name|--summary|--description|--status|--due|--snooze",
                ));
            }
            Ok(Command::State(StateCommand::Edit {
                body: serde_json::Value::Object(body).to_string(),
            }))
        }
        Some("delete") => {
            let (table, id) = target(rest)?;
            if let Some(extra) = rest.get(3) {
                return Err(CliError::UnknownSubcommand {
                    command: "state delete",
                    got: extra.clone(),
                });
            }
            Ok(Command::State(StateCommand::Delete {
                body: serde_json::json!({ "table": table, "id": id }).to_string(),
            }))
        }
        Some("create") => {
            let table = match rest.get(1).map(String::as_str) {
                Some("commitments") => "commitments",
                Some("open-loops" | "open_loops") => "open_loops",
                _ => return Err(CliError::MissingArgument("<commitments|open-loops>")),
            };
            let mut body = serde_json::json!({ "table": table });
            let mut i = 2;
            while i < rest.len() {
                let value = |name: &'static str| rest.get(i + 1).cloned().ok_or(CliError::MissingFlagValue(name));
                match rest[i].as_str() {
                    "--description" => body["description"] = serde_json::json!(value("--description")?),
                    "--due" => {
                        let raw = value("--due")?;
                        body["due_at"] = serde_json::json!(raw.parse::<i64>().map_err(|_| CliError::BadId(raw))?);
                    }
                    "--person" => {
                        let raw = value("--person")?;
                        let id = raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or(CliError::BadId(raw))?;
                        body["person_id"] = serde_json::json!(id);
                    }
                    other => {
                        return Err(CliError::UnknownSubcommand { command: "state create", got: other.to_string() })
                    }
                }
                i += 2;
            }
            if body.get("description").is_none() {
                return Err(CliError::MissingArgument("--description"));
            }
            Ok(Command::State(StateCommand::Create { body: body.to_string() }))
        }
        // `--format export-v1|export-v2` selects the portable JSON Lines export (E7, E27); without
        // it, Markdown.
        Some("export") if rest.iter().any(|arg| arg == "--format") => {
            let mut body = serde_json::Map::new();
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--format" => {
                        let value = rest.get(i + 1).ok_or(CliError::MissingArgument("--format <markdown|export-v1|export-v2>"))?;
                        body.insert("format".into(), value.clone().into());
                        i += 2;
                    }
                    "--differential" => {
                        body.insert("differential".into(), true.into());
                        i += 1;
                    }
                    "--since-cursor" => {
                        let value = rest.get(i + 1).ok_or(CliError::MissingArgument("--since-cursor <ms>"))?;
                        body.insert("since_cursor".into(), value.parse::<i64>().map_err(|_| CliError::BadId(value.clone()))?.into());
                        i += 2;
                    }
                    other => return Err(CliError::UnknownSubcommand { command: "state export", got: other.to_string() }),
                }
            }
            if body.get("format").and_then(serde_json::Value::as_str) == Some("markdown") {
                return Ok(Command::State(StateCommand::Export));
            }
            let body = serde_json::Value::Object(body);
            shogun_mcp::export_api::parse(&body).map_err(CliError::BadInput)?;
            Ok(Command::State(StateCommand::ExportV1 { body: body.to_string() }))
        }
        Some("export") => Ok(Command::State(StateCommand::Export)),
        Some("evidence") => {
            let (table, id) = splittable(rest)?;
            if let Some(extra) = rest.get(3) {
                return Err(CliError::UnknownSubcommand { command: "state evidence", got: extra.clone() });
            }
            Ok(Command::State(StateCommand::Evidence { table, id }))
        }
        Some("split") => {
            let (table, id) = splittable(rest)?;
            let mut name = None;
            let mut events: Vec<i64> = Vec::new();
            let mut aliases: Vec<String> = Vec::new();
            let mut i = 3;
            while i < rest.len() {
                let value = |name: &'static str| rest.get(i + 1).cloned().ok_or(CliError::MissingFlagValue(name));
                match rest[i].as_str() {
                    "--name" => name = Some(value("--name")?),
                    "--event" => {
                        let raw = value("--event")?;
                        let id = raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or(CliError::BadId(raw))?;
                        events.push(id);
                    }
                    "--alias" => aliases.push(value("--alias")?),
                    other => {
                        return Err(CliError::UnknownSubcommand { command: "state split", got: other.to_string() })
                    }
                }
                i += 2;
            }
            let name = name.ok_or(CliError::MissingArgument("--name"))?;
            if events.is_empty() {
                return Err(CliError::MissingArgument("--event"));
            }
            let mut body = serde_json::json!({ "table": table, "id": id, "name": name, "event_ids": events });
            if !aliases.is_empty() {
                body["aliases"] = serde_json::json!(aliases);
            }
            Ok(Command::State(StateCommand::Split { body: body.to_string() }))
        }
        // Merge review (issue #1362): the same people / projects rule as split.
        Some("duplicates") => {
            let table = match rest.get(1).map(String::as_str) {
                Some(t @ ("people" | "projects")) => t.to_string(),
                _ => return Err(CliError::MissingArgument("<people|projects>")),
            };
            if let Some(extra) = rest.get(2) {
                return Err(CliError::UnknownSubcommand { command: "state duplicates", got: extra.clone() });
            }
            Ok(Command::State(StateCommand::Duplicates { table }))
        }
        Some("merge") => {
            let (table, first) = splittable(rest)?;
            let mut ids = vec![first];
            for raw in &rest[3..] {
                let id = raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(raw.clone()))?;
                ids.push(id);
            }
            if ids.len() < 2 {
                return Err(CliError::MissingArgument("<id> <id>"));
            }
            let body = serde_json::json!({ "table": table, "ids": ids });
            Ok(Command::State(StateCommand::Merge { body: body.to_string() }))
        }
        Some("keep-separate") => {
            let (table, id) = splittable(rest)?;
            let other = rest.get(3).ok_or(CliError::MissingArgument("<id> <id>"))?;
            let other_id = other.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| CliError::BadId(other.clone()))?;
            if let Some(extra) = rest.get(4) {
                return Err(CliError::UnknownSubcommand { command: "state keep-separate", got: extra.clone() });
            }
            let body = serde_json::json!({ "table": table, "id": id, "other_id": other_id });
            Ok(Command::State(StateCommand::KeepSeparate { body: body.to_string() }))
        }
        other => Err(CliError::UnknownSubcommand {
            command: "state",
            got: other.unwrap_or_default().to_string(),
        }),
    }
}

fn parse_disputes(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::{DisputesCommand, DISPUTE_RESOLUTIONS};
    const RESOLUTION: &str = "<keep_earlier|keep_later|dismiss_both|keep_both>";
    match rest.first().map(String::as_str) {
        None | Some("list") => Ok(Command::Disputes(DisputesCommand::List)),
        Some("resolve") => {
            let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
            let id = raw.parse::<i64>().map_err(|_| CliError::BadId(raw.clone()))?;
            let resolution = rest
                .get(2)
                .filter(|value| DISPUTE_RESOLUTIONS.contains(&value.as_str()))
                .ok_or(CliError::MissingArgument(RESOLUTION))?;
            Ok(Command::Disputes(DisputesCommand::Resolve {
                id,
                resolution: resolution.clone(),
            }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "disputes",
            got: other.to_string(),
        }),
    }
}

fn parse_lessons(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::{LessonsCommand, LessonsList};
    let id_arg = |rest: &[String]| -> Result<i64, CliError> {
        let id_str = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
        id_str
            .parse::<i64>()
            .map_err(|_| CliError::BadId(id_str.clone()))
    };
    match rest.first().map(String::as_str) {
        None => Ok(Command::Lessons(LessonsCommand::List(
            LessonsList::default(),
        ))),
        Some("list") => parse_lessons_list(&rest[1..]),
        Some("enable") => Ok(Command::Lessons(LessonsCommand::Enable {
            id: id_arg(rest)?,
        })),
        Some("disable") => Ok(Command::Lessons(LessonsCommand::Disable {
            id: id_arg(rest)?,
        })),
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "lessons",
            got: other.to_string(),
        }),
    }
}

fn parse_lessons_list(flags: &[String]) -> Result<Command, CliError> {
    use crate::command::{LessonsCommand, LessonsList};
    let mut opts = LessonsList::default();
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--for-generation" => {
                opts.for_generation = true;
                i += 1;
            }
            "--app" => {
                let v = flags
                    .get(i + 1)
                    .ok_or(CliError::MissingFlagValue("--app"))?;
                opts.app_bundle_id = Some(v.clone());
                i += 2;
            }
            "--person" => {
                let v = flags
                    .get(i + 1)
                    .ok_or(CliError::MissingFlagValue("--person"))?;
                opts.person_id = Some(v.clone());
                i += 2;
            }
            "--project" => {
                let v = flags
                    .get(i + 1)
                    .ok_or(CliError::MissingFlagValue("--project"))?;
                opts.project_id = Some(v.clone());
                i += 2;
            }
            other => {
                return Err(CliError::UnknownSubcommand {
                    command: "lessons list",
                    got: other.to_string(),
                })
            }
        }
    }
    Ok(Command::Lessons(LessonsCommand::List(opts)))
}

fn parse_backup(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::BackupCommand;
    match rest.first().map(String::as_str) {
        None | Some("status") => Ok(Command::Backup(BackupCommand::Status)),
        Some("retention") => {
            let raw = rest.get(1).ok_or(CliError::MissingArgument("<days>"))?;
            let days = raw
                .parse::<u32>()
                .ok()
                .and_then(|days| shogun_core::backup_retention::BackupRetention::try_days(days).ok())
                .ok_or_else(|| CliError::BadBackupRetention(raw.clone()))?;
            Ok(Command::Backup(BackupCommand::Retention { days: days.days() }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "backup",
            got: other.to_string(),
        }),
    }
}

fn parse_visual_recall(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::VisualRecallCommand;
    match rest.first().map(String::as_str) {
        None => Err(CliError::MissingArgument("visual-recall subcommand")),
        Some("status") => Ok(Command::VisualRecall(VisualRecallCommand::Status)),
        Some("delete-hidden-self-frames") if rest.len() == 1 => Ok(Command::VisualRecall(VisualRecallCommand::DeleteHiddenSelfFrames)),
        Some("enable") => Ok(Command::VisualRecall(VisualRecallCommand::Enable)),
        Some("disable") => Ok(Command::VisualRecall(VisualRecallCommand::Disable)),
        Some("retention") => {
            let raw = rest.get(1).ok_or(CliError::MissingArgument("<days>"))?;
            let days = raw
                .parse::<u32>()
                .ok()
                .filter(|days| (1..=3_650).contains(days))
                .ok_or_else(|| CliError::BadRetention(raw.clone()))?;
            Ok(Command::VisualRecall(VisualRecallCommand::Retention {
                days,
            }))
        }
        Some("search") => {
            let mut from_ms = None;
            let mut to_ms = None;
            let mut app_bundle_id = None;
            let mut query_parts: Vec<String> = Vec::new();
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--from-ms" => {
                        let v = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--from-ms"))?;
                        from_ms = Some(v.parse().map_err(|_| CliError::BadId(v.clone()))?);
                        i += 2;
                    }
                    "--to-ms" => {
                        let v = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--to-ms"))?;
                        to_ms = Some(v.parse().map_err(|_| CliError::BadId(v.clone()))?);
                        i += 2;
                    }
                    "--app-bundle-id" => {
                        let v = rest
                            .get(i + 1)
                            .ok_or(CliError::MissingFlagValue("--app-bundle-id"))?;
                        app_bundle_id = Some(v.clone());
                        i += 2;
                    }
                    _ => {
                        query_parts.push(rest[i].clone());
                        i += 1;
                    }
                }
            }
            let query = join(&query_parts).ok_or(CliError::MissingArgument("<query>"))?;
            Ok(Command::VisualRecall(VisualRecallCommand::Search {
                query,
                from_ms,
                to_ms,
                app_bundle_id,
            }))
        }
        Some("frame") => match rest.get(1).map(String::as_str) {
            Some("get") => {
                let id_str = rest.get(2).ok_or(CliError::MissingArgument("<id>"))?;
                let id = id_str
                    .parse()
                    .map_err(|_| CliError::BadId(id_str.clone()))?;
                Ok(Command::VisualRecall(VisualRecallCommand::FrameGet { id }))
            }
            Some("text-lines") => {
                let id_str = rest.get(2).ok_or(CliError::MissingArgument("<id>"))?;
                let id = id_str
                    .parse()
                    .map_err(|_| CliError::BadId(id_str.clone()))?;
                Ok(Command::VisualRecall(VisualRecallCommand::FrameTextLines { id }))
            }
            Some("rescan") => {
                let id_str = rest.get(2).ok_or(CliError::MissingArgument("<id>"))?;
                let id = id_str
                    .parse()
                    .map_err(|_| CliError::BadId(id_str.clone()))?;
                Ok(Command::VisualRecall(VisualRecallCommand::FrameRescan {
                    id,
                }))
            }
            Some("delete") => {
                let id_str = rest.get(2).ok_or(CliError::MissingArgument("<id>"))?;
                let id = id_str
                    .parse()
                    .map_err(|_| CliError::BadId(id_str.clone()))?;
                Ok(Command::VisualRecall(VisualRecallCommand::FrameDelete {
                    id,
                }))
            }
            other => Err(CliError::UnknownSubcommand {
                command: "visual-recall frame",
                got: other.unwrap_or("").to_string(),
            }),
        },
        other => Err(CliError::UnknownSubcommand {
            command: "visual-recall",
            got: other.unwrap_or("").to_string(),
        }),
    }
}

/// A `--flag <value>` pair inside a mail sub-grammar.
fn flag_value(flags: &[String], i: usize, flag: &'static str) -> Result<String, CliError> {
    flags
        .get(i + 1)
        .cloned()
        .ok_or(CliError::MissingFlagValue(flag))
}

/// `important` | `other` — refused up front so a typo never reaches the daemon as a filter.
fn mail_bucket(value: String) -> Result<String, CliError> {
    match value.as_str() {
        "important" | "other" => Ok(value),
        _ => Err(CliError::UnknownSubcommand {
            command: "mail bucket",
            got: value,
        }),
    }
}

fn mail_attention(value: String) -> Result<String, CliError> {
    match value.as_str() {
        "needs_reply" | "waiting_on_them" | "fyi" | "none" => Ok(value),
        _ => Err(CliError::UnknownSubcommand {
            command: "mail attention",
            got: value,
        }),
    }
}

/// `on` | `off` for a label flag.
fn on_off(command: &'static str, value: String) -> Result<bool, CliError> {
    match value.as_str() {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        _ => Err(CliError::UnknownSubcommand {
            command,
            got: value,
        }),
    }
}

/// `mail triage|labels|rules|history …` (issue #319, FR-TG-30).
fn parse_mail(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::MailCommand;
    match rest.first().map(String::as_str) {
        None => Err(CliError::MissingArgument(
            "triage|labels|rules|history|status|on|off",
        )),
        Some("triage") => parse_mail_triage(&rest[1..]),
        Some("labels") => parse_mail_labels(&rest[1..]),
        Some("rules") => parse_mail_rules(&rest[1..]),
        Some("history") => Ok(Command::Mail(MailCommand::History)),
        Some("status") => Ok(Command::Mail(MailCommand::Status)),
        Some("on") => Ok(Command::Mail(MailCommand::SetEnabled { enabled: true })),
        Some("off") => Ok(Command::Mail(MailCommand::SetEnabled { enabled: false })),
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "mail",
            got: other.to_string(),
        }),
    }
}

/// `mail triage [filters]` | `show <key>` | `set <key> …` | `archive <key>` | `unarchive <key>`.
fn parse_mail_triage(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::{MailCommand, MailTriageList};
    let thread_key = |rest: &[String]| -> Result<String, CliError> {
        rest.get(1)
            .cloned()
            .filter(|k| !k.is_empty())
            .ok_or(CliError::MissingArgument("<thread_key>"))
    };
    match rest.first().map(String::as_str) {
        Some("show") => Ok(Command::Mail(MailCommand::TriageShow {
            thread_key: thread_key(rest)?,
        })),
        Some("archive") => Ok(Command::Mail(MailCommand::TriageArchive {
            thread_key: thread_key(rest)?,
        })),
        Some("unarchive") => Ok(Command::Mail(MailCommand::TriageUnarchive {
            thread_key: thread_key(rest)?,
        })),
        Some("set") => {
            let thread_key = thread_key(rest)?;
            let flags = &rest[2..];
            let (mut label, mut bucket, mut handled) = (None, None, None);
            let mut i = 0;
            while i < flags.len() {
                match flags[i].as_str() {
                    "--label" => {
                        label = Some(flag_value(flags, i, "--label")?);
                        i += 2;
                    }
                    "--bucket" => {
                        bucket = Some(mail_bucket(flag_value(flags, i, "--bucket")?)?);
                        i += 2;
                    }
                    "--handled" => {
                        handled = Some(true);
                        i += 1;
                    }
                    "--unhandled" => {
                        handled = Some(false);
                        i += 1;
                    }
                    other => {
                        return Err(CliError::UnknownSubcommand {
                            command: "mail triage set",
                            got: other.to_string(),
                        })
                    }
                }
            }
            if label.is_none() && bucket.is_none() && handled.is_none() {
                return Err(CliError::MissingArgument(
                    "--label KEY | --bucket B | --handled | --unhandled",
                ));
            }
            Ok(Command::Mail(MailCommand::TriageSet {
                thread_key,
                label,
                bucket,
                handled,
            }))
        }
        // No verb → the list, with optional filters (`list` is accepted as an alias).
        first => {
            let flags = if first == Some("list") { &rest[1..] } else { rest };
            let mut opts = MailTriageList::default();
            let mut i = 0;
            while i < flags.len() {
                match flags[i].as_str() {
                    "--bucket" => {
                        opts.bucket = Some(mail_bucket(flag_value(flags, i, "--bucket")?)?);
                        i += 2;
                    }
                    "--attention" => {
                        opts.attention =
                            Some(mail_attention(flag_value(flags, i, "--attention")?)?);
                        i += 2;
                    }
                    "--label" => {
                        opts.label = Some(flag_value(flags, i, "--label")?);
                        i += 2;
                    }
                    "--handled" => {
                        opts.handled = true;
                        i += 1;
                    }
                    other => {
                        return Err(CliError::UnknownSubcommand {
                            command: "mail triage",
                            got: other.to_string(),
                        })
                    }
                }
            }
            Ok(Command::Mail(MailCommand::TriageList(opts)))
        }
    }
}

/// `mail labels` | `set <key-or-name> …` | `rm <key>`.
fn parse_mail_labels(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::{MailCommand, MailLabelSet};
    match rest.first().map(String::as_str) {
        None | Some("list") => Ok(Command::Mail(MailCommand::LabelsList)),
        Some("rm") => Ok(Command::Mail(MailCommand::LabelsRemove {
            key: rest
                .get(1)
                .cloned()
                .filter(|k| !k.is_empty())
                .ok_or(CliError::MissingArgument("<key>"))?,
        })),
        Some("set") => {
            let mut opts = MailLabelSet {
                key_or_name: rest
                    .get(1)
                    .cloned()
                    .filter(|k| !k.is_empty() && !k.starts_with("--"))
                    .ok_or(CliError::MissingArgument("<key-or-name>"))?,
                ..MailLabelSet::default()
            };
            let flags = &rest[2..];
            let mut i = 0;
            while i < flags.len() {
                match flags[i].as_str() {
                    "--name" => {
                        opts.name = Some(flag_value(flags, i, "--name")?);
                        i += 2;
                    }
                    "--definition" => {
                        opts.definition = Some(flag_value(flags, i, "--definition")?);
                        i += 2;
                    }
                    "--bucket" => {
                        opts.bucket = Some(mail_bucket(flag_value(flags, i, "--bucket")?)?);
                        i += 2;
                    }
                    "--enabled" => {
                        opts.enabled = Some(on_off(
                            "mail labels set --enabled",
                            flag_value(flags, i, "--enabled")?,
                        )?);
                        i += 2;
                    }
                    "--auto-archive" => {
                        opts.auto_archive = Some(on_off(
                            "mail labels set --auto-archive",
                            flag_value(flags, i, "--auto-archive")?,
                        )?);
                        i += 2;
                    }
                    "--sync" => {
                        opts.sync_to_gmail = Some(on_off(
                            "mail labels set --sync",
                            flag_value(flags, i, "--sync")?,
                        )?);
                        i += 2;
                    }
                    "--confirm-standing" => {
                        opts.confirm_standing = true;
                        i += 1;
                    }
                    other => {
                        return Err(CliError::UnknownSubcommand {
                            command: "mail labels set",
                            got: other.to_string(),
                        })
                    }
                }
            }
            Ok(Command::Mail(MailCommand::LabelsSet(opts)))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "mail labels",
            got: other.to_string(),
        }),
    }
}

/// `mail rules` | `add --addr X|--domain X --label L [--bucket B]` | `rm <id>`.
fn parse_mail_rules(rest: &[String]) -> Result<Command, CliError> {
    use crate::command::MailCommand;
    match rest.first().map(String::as_str) {
        None | Some("list") => Ok(Command::Mail(MailCommand::RulesList)),
        Some("rm") => {
            let raw = rest.get(1).ok_or(CliError::MissingArgument("<id>"))?;
            Ok(Command::Mail(MailCommand::RulesRemove {
                id: raw.parse().map_err(|_| CliError::BadId(raw.clone()))?,
            }))
        }
        Some("add") => {
            let flags = &rest[1..];
            let (mut matcher, mut label, mut bucket) = (None, None, None);
            let mut i = 0;
            while i < flags.len() {
                match flags[i].as_str() {
                    "--addr" => {
                        matcher = Some(("addr", flag_value(flags, i, "--addr")?));
                        i += 2;
                    }
                    "--domain" => {
                        matcher = Some(("domain", flag_value(flags, i, "--domain")?));
                        i += 2;
                    }
                    "--label" => {
                        label = Some(flag_value(flags, i, "--label")?);
                        i += 2;
                    }
                    "--bucket" => {
                        bucket = Some(mail_bucket(flag_value(flags, i, "--bucket")?)?);
                        i += 2;
                    }
                    other => {
                        return Err(CliError::UnknownSubcommand {
                            command: "mail rules add",
                            got: other.to_string(),
                        })
                    }
                }
            }
            let (match_kind, match_value) =
                matcher.ok_or(CliError::MissingArgument("--addr X | --domain X"))?;
            Ok(Command::Mail(MailCommand::RulesAdd {
                match_kind: match_kind.to_string(),
                match_value,
                label: label.ok_or(CliError::MissingArgument("--label KEY"))?,
                bucket,
            }))
        }
        Some(other) => Err(CliError::UnknownSubcommand {
            command: "mail rules",
            got: other.to_string(),
        }),
    }
}

/// `translate text <words…> [--to L] [--register R] [--app ID]` and
/// `translate reply <words…> [--in L] [--tone T] [--register R] [--app ID]` (issue #279). Flags may
/// sit anywhere; every other word is the text. Values are checked by the backend, which answers
/// the same codes on every face.
fn parse_translate_request(rest: &[String]) -> Result<Command, CliError> {
    let reply = rest.first().map(String::as_str) == Some("reply");
    let mut words: Vec<String> = Vec::new();
    let (mut language, mut tone, mut formality, mut app) = (None, None, None, None);
    let mut i = 1;
    while i < rest.len() {
        match rest[i].as_str() {
            "--to" if !reply => {
                language = Some(flag_value(rest, i, "--to")?);
                i += 2;
            }
            "--in" if reply => {
                language = Some(flag_value(rest, i, "--in")?);
                i += 2;
            }
            "--tone" if reply => {
                tone = Some(flag_value(rest, i, "--tone")?);
                i += 2;
            }
            "--register" => {
                formality = Some(flag_value(rest, i, "--register")?);
                i += 2;
            }
            "--app" => {
                app = Some(flag_value(rest, i, "--app")?);
                i += 2;
            }
            _ => {
                words.push(rest[i].clone());
                i += 1;
            }
        }
    }
    let text = join(&words).ok_or(if reply {
        CliError::MissingArgument("translate reply <message>")
    } else {
        CliError::MissingArgument("translate text <text>")
    })?;
    Ok(Command::Translate(if reply {
        TranslateCommand::Reply { incoming: text, reply_in: language, adjustment: tone, formality, app }
    } else {
        TranslateCommand::Text { text, target: language, formality, app }
    }))
}

/// Join trailing words into one string (for free-text args); `None` if empty.
/// The remaining words as one argument. Blank words (`set ""`) count as missing, the same as
/// giving none, so a blank name or text never reaches the daemon.
fn join(words: &[String]) -> Option<String> {
    let joined = words.join(" ");
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}


fn parse_forms(rest: &[String]) -> Result<Command, CliError> {
    let positive_session = |raw: &str| {
        raw.parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| CliError::BadId(raw.to_owned()))
    };
    let revision = |raw: &str| {
        raw.parse::<u64>()
            .map_err(|_| CliError::BadInput("revision must be a nonnegative integer".into()))
    };
    if rest.iter().any(|arg| arg.starts_with("--")) {
        return Err(CliError::BadInput(
            "forms accepts no approval, answers or command-specific flags".into(),
        ));
    }
    let pieces: Vec<&str> = rest.iter().map(String::as_str).collect();
    let operation = match pieces.as_slice() {
        ["inspect"] => FormOperation::Inspect {},
        ["status", id] => FormOperation::Status {
            session_id: positive_session(id)?,
        },
        ["undo", id] => FormOperation::Undo {
            session_id: positive_session(id)?,
        },
        ["close", id] => FormOperation::Close {
            session_id: positive_session(id)?,
        },
        ["apply", id, rev] => FormOperation::Apply {
            session_id: positive_session(id)?,
            revision: revision(rev)?,
        },
        ["plan", id, rev, instruction @ ..] if !instruction.is_empty() => FormOperation::Plan {
            session_id: positive_session(id)?,
            revision: revision(rev)?,
            instruction: instruction.join(" "),
        },
        ["plan", ..] => {
            return Err(CliError::MissingArgument(
                "forms plan <session> <revision> <instruction...>",
            ))
        }
        ["apply", ..] => {
            return Err(CliError::MissingArgument(
                "forms apply <session> <revision>",
            ))
        }
        ["status", ..] | ["close", ..] => {
            return Err(CliError::MissingArgument("forms status|close <session>"))
        }
        [] => {
            return Err(CliError::MissingArgument(
                "forms inspect|plan|status|apply|close",
            ))
        }
        [other, ..] => {
            return Err(CliError::UnknownSubcommand {
                command: "forms",
                got: (*other).to_owned(),
            })
        }
    };
    shogun_mcp::forms_api::validate_operation(&operation)
        .map_err(|code| CliError::BadInput(code.to_owned()))?;
    Ok(Command::Forms(operation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_and_session_post_the_mcp_arguments() {
        let call = |args: &[&str]| {
            let inv = parse(&v(args)).unwrap();
            let call = crate::wire::to_call(&inv.command, false).unwrap();
            (call.method, call.path, serde_json::from_str::<serde_json::Value>(&call.body.unwrap()).unwrap())
        };
        assert_eq!(
            call(&["events", "12", "7", "--max-chars", "500"]),
            ("POST", "/v1/memory/events".into(), serde_json::json!({"ids": [12, 7], "max_chars": 500}))
        );
        assert_eq!(
            call(&["session", "--from-ms", "0", "--to-ms", "60000", "--app", "chrome"]),
            ("POST", "/v1/memory/session".into(), serde_json::json!({"from_ms": 0, "to_ms": 60000, "app": "chrome"}))
        );
        for bad in [&["events"][..], &["events", "0"], &["events", "1", "--max-chars", "9"], &["session", "--from-ms", "5"]] {
            assert!(parse(&v(bad)).is_err(), "{bad:?}");
        }
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_is_help() {
        assert_eq!(parse(&[]).unwrap().command, Command::Help);
    }

    /// E15 / E18: the governance commands map to the shared tools; deciding and granting are the
    /// operator's routes (no tool) and go to the admin paths.
    #[test]
    fn governance_commands_map_to_tools_and_operator_routes() {
        use shogun_mcp::memory_api::Tool;
        let call = |args: &[&str]| {
            let inv = parse(&v(args)).unwrap();
            (inv.command.tool(), crate::wire::to_call(&inv.command, false).unwrap())
        };
        let (tool, c) = call(&["approvals"]);
        assert_eq!((tool, c.method, c.path.as_str()), (Some(Tool::ApprovalsList), "GET", "/v1/approvals"));
        let (tool, c) = call(&["approvals", "get", "7"]);
        assert_eq!((tool, c.path.as_str()), (Some(Tool::ApprovalsGet), "/v1/approvals/7"));
        let (tool, c) = call(&["approvals", "decide", "7", "reject", "--as", "human:aiko"]);
        assert_eq!((tool, c.method, c.path.as_str()), (None, "POST", "/v1/approvals/7/decide"));
        let body: serde_json::Value = serde_json::from_str(c.body.as_deref().unwrap()).unwrap();
        assert_eq!((body["decision"].as_str(), body["decided_by"].as_str()), (Some("reject"), Some("human:aiko")));
        assert!(crate::port::needs_admin(&c.path));
        assert!(parse(&v(&["approvals", "decide", "7", "maybe"])).is_err());
        let (tool, c) = call(&["audit", "--since-ms", "5", "--limit", "10"]);
        assert_eq!((tool, c.path.as_str()), (Some(Tool::AuditList), "/v1/audit?since_ms=5&limit=10"));
        assert!(parse(&v(&["audit", "--limit", "0"])).is_err());
        // E26: the export is the operator's route; verify runs locally and sends nothing.
        let (tool, c) = call(&["audit", "export", "--since-ms", "5", "--until-ms", "9", "--format", "csv", "--principal", "alice", "--out", "/tmp/x.csv"]);
        assert_eq!((tool, c.method, c.path.as_str()), (None, "GET", "/v1/audit/export?format=csv&since_ms=5&until_ms=9&principal=alice"));
        assert!(crate::port::needs_admin(&c.path));
        assert_eq!(call(&["audit", "export"]).1.path, "/v1/audit/export?format=jsonl");
        assert!(parse(&v(&["audit", "export", "--format", "xml"])).is_err());
        assert!(parse(&v(&["audit", "export", "--since-ms", "9", "--until-ms", "5"])).is_err());
        assert!(parse(&v(&["audit", "export", "--principal", "Alice Smith"])).is_err());
        let verify = parse(&v(&["audit", "verify", "export.jsonl"])).unwrap();
        assert_eq!(verify.command, Command::AuditVerify { file: "export.jsonl".into() });
        assert!(crate::wire::to_call(&verify.command, false).is_none());
        assert!(parse(&v(&["audit", "verify"])).is_err());
        let (tool, c) = call(&["acl"]);
        assert_eq!((tool, c.path.as_str()), (Some(Tool::AclList), "/v1/acl"));
        assert!(!crate::port::needs_admin(&c.path));
        let (tool, c) = call(&["acl", "grant", r#"{"grantee":"agent:b","agent":"a"}"#]);
        assert_eq!((tool, c.path.as_str()), (None, "/v1/acl/grant"));
        assert!(crate::port::needs_admin(&c.path));
        assert!(parse(&v(&["acl", "grant", r#"{"grantee":"agent:a","agent":"a"}"#])).is_err());
        let (_, c) = call(&["acl", "team", r#"{"tenant":"acme","team":"ops","members":["alice"]}"#]);
        assert_eq!((c.method, c.path.as_str()), ("POST", "/v1/teams"));
    }

    /// E46: `shogun payments ...` are the four `payments.*` tools on their REST routes.
    #[test]
    fn payments_commands_map_to_their_routes() {
        use shogun_mcp::memory_api::Tool;
        let cases: [(&[&str], Tool, &str, &str); 5] = [
            (&["payments", "mandates", "list"], Tool::PaymentsMandatesList, "GET", "/v1/payments/mandates"),
            (&["payments", "mandates", "list", r#"{"mandate_id":"pmd_0123456789ab"}"#], Tool::PaymentsMandatesList, "POST", "/v1/payments/mandates/list"),
            (&["payments", "mandates", "request", r#"{"payee":"x","currency":"USD"}"#], Tool::PaymentsMandatesRequest, "POST", "/v1/payments/mandates/request"),
            (&["payments", "mandates", "revoke", r#"{"mandate_id":"pmd_0123456789ab"}"#], Tool::PaymentsMandatesRevoke, "POST", "/v1/payments/mandates/revoke"),
            (&["payments", "spend", r#"{"payee":"x","currency":"USD","amount_minor":1,"idempotency_key":"k"}"#], Tool::PaymentsSpend, "POST", "/v1/payments/spend"),
        ];
        for (args, tool, method, path) in cases {
            let inv = parse(&v(args)).unwrap();
            assert_eq!(inv.command.tool(), Some(tool));
            let call = crate::wire::to_call(&inv.command, false).unwrap();
            assert_eq!((call.method, call.path.as_str()), (method, path));
        }
        assert!(parse(&v(&["payments", "spend"])).is_err());
        assert!(parse(&v(&["payments", "spend", "[1]"])).is_err());
        assert!(parse(&v(&["payments", "mandates", "other", "{}"])).is_err());
        assert!(parse(&v(&["payments"])).is_err());
    }

    /// E35: `shogun handoff create|accept <json>` are `handoff.create` / `handoff.accept` on the
    /// same REST routes, validated by the shared parser before anything is sent.
    #[test]
    fn handoff_commands_post_the_shared_body() {
        let inv = parse(&v(&["handoff", "create", r#"{"task":"Draft the note","to_agent":"agent:writer"}"#])).unwrap();
        assert_eq!(inv.command.tool(), Some(shogun_mcp::memory_api::Tool::HandoffCreate));
        let call = crate::wire::to_call(&inv.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/handoff"));
        let handoff = r#"{"format":"shogunagent-handoff/v1","handoff_id":"hof_12345678-abcd","created_at":1,"expires_at":2,"from_agent":"agent:p","to_agent":"agent:w","task":{"summary":"x","query":"x","thread_key":null},"entities":[],"facts":[],"truncated":false}"#;
        let accept = format!(r#"{{"handoff":{handoff},"actor":"agent:w"}}"#);
        let inv = parse(&v(&["handoff", "accept", &accept])).unwrap();
        assert_eq!(inv.command.tool(), Some(shogun_mcp::memory_api::Tool::HandoffAccept));
        let call = crate::wire::to_call(&inv.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/handoff/accept"));
        assert!(parse(&v(&["handoff", "create", r#"{"to_agent":"agent:w"}"#])).is_err());
        assert!(parse(&v(&["handoff", "accept", r#"{"handoff":{},"actor":"agent:w"}"#])).is_err());
        assert!(parse(&v(&["handoff", "send", "{}"])).is_err());
        assert!(parse(&v(&["handoff"])).is_err());
    }

    /// E44: `shogun ingest-webhook <json>` is `memory.ingest_webhook` on its REST route, validated
    /// by the shared parser before anything is sent.
    #[test]
    fn ingest_webhook_posts_the_shared_body() {
        let body = r#"{"provider":"gmail","delivery_id":"m-1","idempotency_key":"hook:gmail:m-1","changed":{"history_id":"7"}}"#;
        let inv = parse(&v(&["ingest-webhook", body])).unwrap();
        assert_eq!(inv.command.tool(), Some(shogun_mcp::memory_api::Tool::MemoryIngestWebhook));
        let call = crate::wire::to_call(&inv.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/memory/ingest-webhook"));
        assert!(parse(&v(&["ingest-webhook", r#"{"provider":"jira"}"#])).is_err());
        assert!(parse(&v(&["ingest-webhook"])).is_err());
    }

    /// E6-T2: `shogun fs <json>` is `memory.fs`, validated by the shared parser before it is sent.
    #[test]
    fn fs_takes_one_memory_tool_command() {
        let inv = parse(&v(&["fs", r#"{"command":"view","path":"/memories"}"#])).unwrap();
        assert_eq!(inv.command.tool(), Some(shogun_mcp::memory_api::Tool::MemoryFs));
        let call = crate::wire::to_call(&inv.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/memory/fs"));
        assert!(parse(&v(&["fs", r#"{"command":"view","path":"/etc/passwd"}"#])).is_err());
        assert!(parse(&v(&["fs", "not json"])).is_err());
        assert!(parse(&v(&["fs"])).is_err());
    }

    /// E7: `state export --format export-v1 [--differential | --since-cursor <ms>]` posts the
    /// `state.export` arguments to `/v1/export`; without `--format` it stays the Markdown export.
    #[test]
    fn state_export_takes_the_v1_format_and_its_cursor_options() {
        use crate::command::StateCommand;
        assert_eq!(parse(&v(&["state", "export"])).unwrap().command, Command::State(StateCommand::Export));
        assert_eq!(
            parse(&v(&["state", "export", "--format", "markdown"])).unwrap().command,
            Command::State(StateCommand::Export)
        );
        let inv = parse(&v(&["state", "export", "--format", "export-v1", "--differential"])).unwrap();
        assert_eq!(inv.command.tool(), Some(shogun_mcp::memory_api::Tool::StateExport));
        let call = crate::wire::to_call(&inv.command, false).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("POST", "/v1/export"));
        let body: serde_json::Value = serde_json::from_str(&call.body.unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({ "format": "export-v1", "differential": true }));
        assert!(parse(&v(&["state", "export", "--format", "export-v1", "--since-cursor", "12"])).is_ok());
        assert!(parse(&v(&["state", "export", "--format", "export-v2", "--differential"])).is_ok());
        assert!(parse(&v(&["state", "export", "--format", "export-v1", "--differential", "--since-cursor", "12"])).is_err());
        assert!(parse(&v(&["state", "export", "--format", "xml"])).is_err());
        assert!(parse(&v(&["state", "export", "--format", "export-v1", "--bogus"])).is_err());
    }

    #[test]
    fn search_joins_multi_word_query() {
        let inv = parse(&v(&["search", "budget", "review"])).unwrap();
        assert_eq!(
            inv.command,
            Command::Search {
                query: "budget review".into(),
                depth: None,
                from_ms: None,
                to_ms: None,
                source_kind: None,
                limit: None,
                ai: false,
            }
        );
    }

    #[test]
    fn search_parses_archive_controls_without_changing_default_shape() {
        let inv = parse(&v(&[
            "search",
            "status",
            "--depth",
            "all",
            "--from-ms",
            "10",
            "--to-ms",
            "20",
        ]))
        .unwrap();
        assert_eq!(
            inv.command,
            Command::Search {
                query: "status".into(),
                depth: Some(SearchDepthV1::All),
                from_ms: Some(10),
                to_ms: Some(20),
                source_kind: None,
                limit: None,
                ai: false,
            }
        );
    }

    #[test]
    fn search_parses_explicit_source_kind() {
        let inv = parse(&v(&["search", "Phoenix", "--source-kind", "project"])).unwrap();

        assert_eq!(
            inv.command,
            Command::Search {
                query: "Phoenix".into(),
                depth: None,
                from_ms: None,
                to_ms: None,
                source_kind: Some(shogun_mcp::backend::MemorySourceFamilyV1::Project),
                limit: None,
                ai: false,
            }
        );
    }

    #[test]
    fn search_without_query_errors() {
        assert_eq!(
            parse(&v(&["search"])),
            Err(CliError::MissingArgument("<query>"))
        );
    }

    #[test]
    fn search_rejects_overlong_unicode_query_stably() {
        let error = parse(&v(&["search", &"界".repeat(4_097)])).unwrap_err();

        assert_eq!(error.message(), "search_query_too_long");
    }

    #[test]
    fn global_flags_are_extracted_anywhere() {
        let inv = parse(&v(&["--token", "abc", "search", "x", "--json"])).unwrap();
        assert_eq!(inv.token.as_deref(), Some("abc"));
        assert!(inv.json);
        assert_eq!(
            inv.command,
            Command::Search {
                query: "x".into(),
                depth: None,
                from_ms: None,
                to_ms: None,
                source_kind: None,
                limit: None,
                ai: false,
            }
        );
    }

    #[test]
    fn token_without_value_errors() {
        assert_eq!(
            parse(&v(&["search", "x", "--token"])),
            Err(CliError::MissingFlagValue("--token"))
        );
    }

    #[test]
    fn a_blank_argument_counts_as_missing() {
        for args in [
            &["meeting", "microphone", "set", ""][..],
            &["voice", "microphone", "set", "  "][..],
            &["note", ""][..],
            &["pack", " "][..],
        ] {
            assert_eq!(
                parse(&v(args)).map(|i| i.command),
                parse(&v(&args[..args.len() - 1])).map(|i| i.command),
                "{args:?}"
            );
            assert!(matches!(parse(&v(args)), Err(CliError::MissingArgument(_))), "{args:?}");
        }
    }

    #[test]
    fn context_takes_no_arguments() {
        assert_eq!(parse(&v(&["context"])).unwrap().command, Command::Context);
        // `--no-screen` is refused rather than ignored.
        assert!(parse(&v(&["context", "--no-screen"])).is_err());
        assert!(parse(&v(&["context", "extra"])).is_err());
    }

    #[test]
    fn state_noun_list_and_get() {
        assert_eq!(
            parse(&v(&["people"])).unwrap().command,
            Command::People(ListOrGet::List)
        );
        assert_eq!(
            parse(&v(&["people", "list"])).unwrap().command,
            Command::People(ListOrGet::List)
        );
        assert_eq!(
            parse(&v(&["people", "get", "42"])).unwrap().command,
            Command::People(ListOrGet::Get { id: 42 })
        );
    }

    #[test]
    fn get_without_id_errors_and_bad_id_errors() {
        assert_eq!(
            parse(&v(&["projects", "get"])),
            Err(CliError::MissingArgument("<id>"))
        );
        assert_eq!(
            parse(&v(&["projects", "get", "x"])),
            Err(CliError::BadId("x".into()))
        );
    }

    #[test]
    fn unknown_subcommand_of_state_noun() {
        assert_eq!(
            parse(&v(&["commitments", "delete"])),
            Err(CliError::UnknownSubcommand {
                command: "commitments",
                got: "delete".into()
            })
        );
    }

    #[test]
    fn note_joins_text() {
        assert_eq!(
            parse(&v(&["note", "call", "Alice", "tomorrow"]))
                .unwrap()
                .command,
            Command::Note {
                text: "call Alice tomorrow".into()
            }
        );
        assert!(parse(&v(&["propose", "add", "commitment"])).is_err());
    }

    #[test]
    fn include_low_flag_sets_the_field() {
        let inv = parse(&v(&["people", "list", "--include-low"])).unwrap();
        assert!(inv.include_low);
    }

    #[test]
    fn state_grammar_builds_the_shared_request_bodies() {
        use crate::command::StateCommand;
        let edit = |args: &[&str]| parse(&v(args)).map(|inv| inv.command);
        // Key order is not part of the contract; compare the JSON values.
        let body = |args: &[&str]| -> serde_json::Value {
            match edit(args) {
                Ok(Command::State(StateCommand::Edit { body } | StateCommand::Delete { body })) => {
                    serde_json::from_str(&body).unwrap()
                }
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            body(&["state", "edit", "commitments", "7", "--description", "send the Q3 deck", "--due", "1000"]),
            serde_json::json!({"table":"commitments","id":7,"description":"send the Q3 deck","due_at":1000})
        );
        assert_eq!(
            body(&["state", "edit", "open-loops", "2", "--status", "closed"]),
            serde_json::json!({"table":"open_loops","id":2,"status":"closed"})
        );
        assert_eq!(
            body(&["state", "edit", "people", "3", "--name", "Dana", "--clear-summary", "--no-due"]),
            serde_json::json!({"table":"people","id":3,"name":"Dana","summary":null,"due_at":null})
        );
        assert!(matches!(edit(&["state", "delete", "projects", "4"]), Ok(Command::State(StateCommand::Delete { .. }))));
        assert_eq!(
            body(&["state", "delete", "projects", "4"]),
            serde_json::json!({"table":"projects","id":4})
        );
        assert_eq!(edit(&["state", "export"]), Ok(Command::State(StateCommand::Export)));
        let inv = parse(&v(&["state", "export", "--json", "--include-low"])).unwrap();
        assert!(inv.json && inv.include_low);

        // refusals happen here, before the daemon
        assert_eq!(
            edit(&["state", "edit", "events", "1", "--name", "x"]),
            Err(CliError::MissingArgument("<people|projects|commitments|open-loops>"))
        );
        assert_eq!(edit(&["state", "edit", "people", "x"]), Err(CliError::BadId("x".into())));
        assert_eq!(edit(&["state", "delete", "people", "0"]), Err(CliError::BadId("0".into())));
        assert_eq!(
            edit(&["state", "edit", "people", "1"]),
            Err(CliError::MissingArgument("--name|--summary|--description|--status|--due|--snooze"))
        );
        assert_eq!(edit(&["state", "edit", "people", "1", "--name"]), Err(CliError::MissingFlagValue("--name")));
        assert_eq!(edit(&["state", "edit", "commitments", "1", "--due", "soon"]), Err(CliError::BadId("soon".into())));
        assert!(matches!(
            edit(&["state", "edit", "people", "1", "--confidence", "1"]),
            Err(CliError::UnknownSubcommand { command: "state edit", .. })
        ));
        assert!(matches!(
            edit(&["state", "delete", "people", "1", "now"]),
            Err(CliError::UnknownSubcommand { command: "state delete", .. })
        ));
        assert!(matches!(edit(&["state"]), Err(CliError::UnknownSubcommand { command: "state", .. })));
    }

    #[test]
    fn import_grammar_reads_only_the_named_files_and_sends_contents() {
        use crate::command::ImportCommand;
        let cmd = |args: &[&str]| parse(&v(args)).map(|inv| inv.command);
        let dir = std::env::temp_dir().join(format!("shogun_cli_import_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("history/nested")).unwrap();
        let pkg = dir.join("pkg.json");
        std::fs::write(&pkg, r#"{"format":"shogun-memory-import/v1"}"#).unwrap();
        std::fs::write(dir.join("history/a.md"), "## Goals\n- a\n").unwrap();
        std::fs::write(dir.join("history/nested/deep.md"), "## Goals\n- deep\n").unwrap();
        let p = |path: &std::path::Path| path.to_str().unwrap().to_string();
        let body = |c: Result<Command, CliError>| -> serde_json::Value {
            match c {
                Ok(Command::Import(ImportCommand::Preview { body } | ImportCommand::Commit { body })) => {
                    serde_json::from_str(&body).unwrap()
                }
                other => panic!("{other:?}"),
            }
        };

        let preview = body(cmd(&["import", "preview", &p(&pkg)]));
        assert_eq!(preview, serde_json::json!({ "package": r#"{"format":"shogun-memory-import/v1"}"# }));
        let history = body(cmd(&["import", "preview", "--computer-history", &p(&dir.join("history"))]));
        assert_eq!(history["computer_history"][0]["name"], "a.md");
        assert_eq!(history["computer_history"].as_array().map(Vec::len), Some(1), "no subfolders");
        assert!(!history.to_string().contains(&p(&dir)), "the daemon gets contents, never a path");
        let commit = body(cmd(&["import", "commit", &p(&pkg), "--select", "entities/0", "--select", "goals/1"]));
        assert_eq!(commit["select"], serde_json::json!(["entities/0", "goals/1"]));
        let defaults = body(cmd(&["import", "commit", "--computer-history", &p(&dir.join("history/a.md")), "--defaults"]));
        assert_eq!(defaults["select"], "default");
        let people = body(cmd(&["import", "commit", &p(&pkg), "--defaults", "--people-to-state"]));
        assert_eq!(people["people_to_state"], true);
        assert!(defaults.get("people_to_state").is_none());

        assert_eq!(cmd(&["import"]), Ok(Command::Import(ImportCommand::List)));
        assert_eq!(cmd(&["import", "show", "3"]), Ok(Command::Import(ImportCommand::Show { id: 3 })));
        assert_eq!(cmd(&["import", "delete", "3"]), Ok(Command::Import(ImportCommand::Delete { id: 3 })));
        assert_eq!(cmd(&["import", "delete", "x"]), Err(CliError::BadId("x".into())));
        // No default path: naming nothing is an error, not a search.
        assert!(matches!(cmd(&["import", "preview"]), Err(CliError::MissingArgument(_))));
        assert!(matches!(cmd(&["import", "preview", "--computer-history"]), Err(CliError::MissingArgument(_))));
        assert!(matches!(cmd(&["import", "commit", &p(&pkg)]), Err(CliError::MissingArgument("--select <id>… | --defaults"))));
        assert!(matches!(cmd(&["import", "preview", &p(&pkg), "--select", "x"]), Err(CliError::UnknownSubcommand { .. })));
        assert!(matches!(
            cmd(&["import", "preview", &p(&pkg), "--computer-history", &p(&dir.join("history"))]),
            Err(CliError::BadInput(_))
        ));
        let missing = cmd(&["import", "preview", &p(&dir.join("nope.json"))]).unwrap_err();
        assert!(!missing.message().contains("nope"), "errors never echo the path");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_merge_review_grammar() {
        use crate::command::StateCommand;
        let cmd = |args: &[&str]| parse(&v(args)).map(|inv| inv.command);
        let body = |args: &[&str]| -> serde_json::Value {
            match cmd(args) {
                Ok(Command::State(StateCommand::Merge { body } | StateCommand::KeepSeparate { body })) => {
                    serde_json::from_str(&body).unwrap()
                }
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(body(&["state", "merge", "people", "3", "9", "4"]), serde_json::json!({"table":"people","ids":[3,9,4]}));
        assert_eq!(
            body(&["state", "keep-separate", "projects", "2", "7"]),
            serde_json::json!({"table":"projects","id":2,"other_id":7})
        );
        assert_eq!(
            cmd(&["state", "duplicates", "projects"]),
            Ok(Command::State(StateCommand::Duplicates { table: "projects".into() }))
        );
        // refusals happen here, before the daemon
        assert_eq!(cmd(&["state", "merge", "commitments", "1", "2"]), Err(CliError::MissingArgument("<people|projects>")));
        assert_eq!(cmd(&["state", "merge", "people", "1"]), Err(CliError::MissingArgument("<id> <id>")));
        assert_eq!(cmd(&["state", "merge", "people", "1", "x"]), Err(CliError::BadId("x".into())));
        assert_eq!(cmd(&["state", "keep-separate", "people", "1"]), Err(CliError::MissingArgument("<id> <id>")));
        assert_eq!(cmd(&["state", "keep-separate", "people", "1", "0"]), Err(CliError::BadId("0".into())));
        assert_eq!(cmd(&["state", "duplicates"]), Err(CliError::MissingArgument("<people|projects>")));
        assert!(matches!(
            cmd(&["state", "duplicates", "people", "more"]),
            Err(CliError::UnknownSubcommand { command: "state duplicates", .. })
        ));
        assert!(matches!(
            cmd(&["state", "keep-separate", "people", "1", "2", "3"]),
            Err(CliError::UnknownSubcommand { command: "state keep-separate", .. })
        ));
    }

    #[test]
    fn state_split_and_evidence_grammar() {
        use crate::command::StateCommand;
        let cmd = |args: &[&str]| parse(&v(args)).map(|inv| inv.command);
        let body = |args: &[&str]| -> serde_json::Value {
            match cmd(args) {
                Ok(Command::State(StateCommand::Split { body })) => serde_json::from_str(&body).unwrap(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            body(&["state", "split", "people", "3", "--name", "Alex Morgan", "--event", "9", "--event", "4", "--alias", "alex.m@acme.com"]),
            serde_json::json!({"table":"people","id":3,"name":"Alex Morgan","event_ids":[9,4],"aliases":["alex.m@acme.com"]})
        );
        assert_eq!(
            body(&["state", "split", "projects", "2", "--event", "7", "--name", "Launch EU"]),
            serde_json::json!({"table":"projects","id":2,"name":"Launch EU","event_ids":[7]})
        );
        assert_eq!(
            cmd(&["state", "evidence", "projects", "2"]),
            Ok(Command::State(StateCommand::Evidence { table: "projects".into(), id: 2 }))
        );
        // refusals happen here, before the daemon
        assert_eq!(cmd(&["state", "split", "commitments", "1", "--name", "x", "--event", "1"]), Err(CliError::MissingArgument("<people|projects>")));
        assert_eq!(cmd(&["state", "evidence", "open-loops", "1"]), Err(CliError::MissingArgument("<people|projects>")));
        assert_eq!(cmd(&["state", "split", "people", "1", "--event", "2"]), Err(CliError::MissingArgument("--name")));
        assert_eq!(cmd(&["state", "split", "people", "1", "--name", "x"]), Err(CliError::MissingArgument("--event")));
        assert_eq!(cmd(&["state", "split", "people", "1", "--name", "x", "--event", "0"]), Err(CliError::BadId("0".into())));
        assert_eq!(cmd(&["state", "split", "people", "1", "--name"]), Err(CliError::MissingFlagValue("--name")));
        assert!(matches!(
            cmd(&["state", "split", "people", "1", "--name", "x", "--event", "2", "--keep", "3"]),
            Err(CliError::UnknownSubcommand { command: "state split", .. })
        ));
        assert!(matches!(
            cmd(&["state", "evidence", "people", "1", "more"]),
            Err(CliError::UnknownSubcommand { command: "state evidence", .. })
        ));
    }

    #[test]
    fn disputes_grammar_parses_list_and_resolve() {
        use crate::command::DisputesCommand;
        assert_eq!(
            parse(&v(&["disputes"])).unwrap().command,
            Command::Disputes(DisputesCommand::List)
        );
        assert_eq!(
            parse(&v(&["disputes", "list"])).unwrap().command,
            Command::Disputes(DisputesCommand::List)
        );
        for resolution in ["keep_earlier", "keep_later", "dismiss_both", "keep_both"] {
            assert_eq!(
                parse(&v(&["disputes", "resolve", "3", resolution]))
                    .unwrap()
                    .command,
                Command::Disputes(DisputesCommand::Resolve {
                    id: 3,
                    resolution: resolution.into()
                })
            );
        }
        assert_eq!(
            parse(&v(&["disputes", "resolve"])),
            Err(CliError::MissingArgument("<id>"))
        );
        assert_eq!(
            parse(&v(&["disputes", "resolve", "x", "keep_later"])),
            Err(CliError::BadId("x".into()))
        );
        assert_eq!(
            parse(&v(&["disputes", "resolve", "3"])),
            Err(CliError::MissingArgument(
                "<keep_earlier|keep_later|dismiss_both|keep_both>"
            ))
        );
        assert_eq!(
            parse(&v(&["disputes", "resolve", "3", "newest"])),
            Err(CliError::MissingArgument(
                "<keep_earlier|keep_later|dismiss_both|keep_both>"
            ))
        );
        assert_eq!(
            parse(&v(&["disputes", "delete"])),
            Err(CliError::UnknownSubcommand {
                command: "disputes",
                got: "delete".into()
            })
        );
    }

    #[test]
    fn lessons_grammar_parses_list_enable_disable() {
        use crate::command::{LessonsCommand, LessonsList};
        assert_eq!(
            parse(&v(&["lessons"])).unwrap().command,
            Command::Lessons(LessonsCommand::List(LessonsList::default()))
        );
        assert_eq!(
            parse(&v(&["lessons", "list"])).unwrap().command,
            Command::Lessons(LessonsCommand::List(LessonsList::default()))
        );
        assert_eq!(
            parse(&v(&["lessons", "enable", "7"])).unwrap().command,
            Command::Lessons(LessonsCommand::Enable { id: 7 })
        );
        assert_eq!(
            parse(&v(&["lessons", "disable", "7"])).unwrap().command,
            Command::Lessons(LessonsCommand::Disable { id: 7 })
        );
        assert_eq!(
            parse(&v(&[
                "lessons",
                "list",
                "--for-generation",
                "--person",
                "alice@example.com",
            ]))
            .unwrap()
            .command,
            Command::Lessons(LessonsCommand::List(LessonsList {
                for_generation: true,
                person_id: Some("alice@example.com".into()),
                ..LessonsList::default()
            }))
        );
        assert_eq!(
            parse(&v(&["lessons", "enable"])),
            Err(CliError::MissingArgument("<id>"))
        );
        assert_eq!(
            parse(&v(&["lessons", "disable", "x"])),
            Err(CliError::BadId("x".into()))
        );
        assert_eq!(
            parse(&v(&["lessons", "delete"])),
            Err(CliError::UnknownSubcommand {
                command: "lessons",
                got: "delete".into()
            })
        );
    }

    #[test]
    fn search_index_subcommands_parse() {
        use crate::command::SearchIndexCommand;
        for (args, expected) in [
            (vec!["search-index"], SearchIndexCommand::Status),
            (vec!["search-index", "status"], SearchIndexCommand::Status),
            (vec!["search-index", "compact"], SearchIndexCommand::Compact),
            (vec!["search-index", "start"], SearchIndexCommand::Compact),
            (vec!["search-index", "stop"], SearchIndexCommand::Stop),
        ] {
            assert_eq!(parse(&v(&args)).unwrap().command, Command::SearchIndex(expected));
        }
        assert_eq!(
            parse(&v(&["search-index", "optimize"])),
            Err(CliError::UnknownSubcommand { command: "search-index", got: "optimize".into() })
        );
    }

    #[test]
    fn storage_preview_is_explicit() {
        assert_eq!(parse(&v(&["storage", "preview"])).unwrap().command, Command::StoragePreview);
    }

    #[test]
    fn backup_retention_is_bounded_one_to_thirty_days() {
        use crate::command::BackupCommand;
        for (args, expected) in [
            (vec!["backup"], BackupCommand::Status),
            (vec!["backup", "status"], BackupCommand::Status),
            (vec!["backup", "retention", "1"], BackupCommand::Retention { days: 1 }),
            (vec!["backup", "retention", "30"], BackupCommand::Retention { days: 30 }),
        ] {
            assert_eq!(parse(&v(&args)).unwrap().command, Command::Backup(expected));
        }
        for invalid in ["0", "31", "forever", "-1"] {
            assert_eq!(
                parse(&v(&["backup", "retention", invalid])),
                Err(CliError::BadBackupRetention(invalid.into()))
            );
        }
        assert!(parse(&v(&["backup", "retention"])).is_err());
        assert!(parse(&v(&["backup", "wipe"])).is_err(), "no delete face here");
    }

    #[test]
    fn hidden_self_cleanup_is_explicit_and_rejects_extra_arguments() {
        assert_eq!(parse(&v(&["visual-recall", "delete-hidden-self-frames"])).unwrap().command,
            Command::VisualRecall(crate::command::VisualRecallCommand::DeleteHiddenSelfFrames));
        assert!(parse(&v(&["visual-recall", "delete-hidden-self-frames", "all"])).is_err());
    }

    #[test]
    fn visual_recall_retention_is_finite_and_bounded() {
        use crate::command::VisualRecallCommand;
        assert_eq!(
            parse(&v(&["visual-recall", "retention", "14"]))
                .unwrap()
                .command,
            Command::VisualRecall(VisualRecallCommand::Retention { days: 14 })
        );
        for invalid in ["0", "3651", "forever", "-1"] {
            assert_eq!(
                parse(&v(&["visual-recall", "retention", invalid])),
                Err(CliError::BadRetention(invalid.into()))
            );
        }
    }

    #[test]
    fn visual_recall_search_accepts_app_and_time_filters() {
        use crate::command::VisualRecallCommand;
        assert_eq!(
            parse(&v(&[
                "visual-recall",
                "search",
                "project comet",
                "--from-ms",
                "10",
                "--to-ms",
                "20",
                "--app-bundle-id",
                "com.apple.Safari",
            ]))
            .unwrap()
            .command,
            Command::VisualRecall(VisualRecallCommand::Search {
                query: "project comet".into(),
                from_ms: Some(10),
                to_ms: Some(20),
                app_bundle_id: Some("com.apple.Safari".into()),
            })
        );
    }

    #[test]
    fn api_status_and_unknown() {
        assert_eq!(
            parse(&v(&["api", "status"])).unwrap().command,
            Command::ApiStatus
        );
        assert!(matches!(
            parse(&v(&["api", "whoami"])),
            Err(CliError::UnknownSubcommand { .. })
        ));
        assert_eq!(parse(&v(&["api", "tokens"])).unwrap().command, Command::ApiTokens);
        assert_eq!(
            parse(&v(&["api", "restrict", "cursor"])).unwrap().command,
            Command::ApiRestrict { client: "cursor".into() }
        );
        assert!(matches!(parse(&v(&["api", "restrict"])), Err(CliError::MissingArgument(_))));
        assert_eq!(parse(&v(&["whoami"])).unwrap().command, Command::Whoami);
        assert_eq!(
            parse(&v(&["web", "page", "https://example.com/a?b=c"])).unwrap().command,
            Command::Web(crate::command::WebCommand::Page { url: "https://example.com/a?b=c".into() })
        );
        assert_eq!(
            parse(&v(&["web", "transcript", "https://youtu.be/abc"])).unwrap().command,
            Command::Web(crate::command::WebCommand::Transcript { url: "https://youtu.be/abc".into() })
        );
        assert!(matches!(parse(&v(&["web", "post"])), Err(CliError::MissingArgument("<url>"))));
        assert!(matches!(parse(&v(&["web", "search", "x"])), Err(CliError::UnknownSubcommand { .. })));
        assert_eq!(
            parse(&v(&["profile", "set", r#"{"role":"PM"}"#]))
                .unwrap()
                .command,
            Command::ProfileSet {
                body: r#"{"role":"PM"}"#.into()
            }
        );
        assert_eq!(
            parse(&v(&["frobnicate"])),
            Err(CliError::UnknownCommand("frobnicate".into()))
        );
    }

    #[test]
    fn capture_pause_commands_and_invalid_duration() {
        assert_eq!(parse(&v(&["capture", "pause", "15m"])).unwrap().command,
            Command::Capture { paused: Some(true), minutes: Some(15) });
        assert_eq!(parse(&v(&["capture", "pause", "30m"])).unwrap().command,
            Command::Capture { paused: Some(true), minutes: Some(30) });
        assert_eq!(parse(&v(&["capture", "resume"])).unwrap().command,
            Command::Capture { paused: Some(false), minutes: None });
        assert_eq!(parse(&v(&["capture", "status"])).unwrap().command,
            Command::Capture { paused: None, minutes: None });
        assert!(parse(&v(&["capture", "pause", "forever"])).is_err());
        assert!(parse(&v(&["capture", "resume", "1h"])).is_err());
        assert!(parse(&v(&["propose", "ignored"])).is_err());
    }

    #[test]
    fn run_takes_the_action_json() {
        assert_eq!(
            parse(&v(&["run", r#"{"kind":"local_search","query":"x"}"#]))
                .unwrap()
                .command,
            Command::Run {
                agent: r#"{"kind":"local_search","query":"x"}"#.into()
            }
        );
        assert_eq!(
            parse(&v(&["run"])),
            Err(CliError::MissingArgument("<action-json>"))
        );
    }

    #[test]
    fn parses_config_actions() {
        assert_eq!(
            parse(&v(&["config", "validate"])).unwrap().command,
            Command::Config {
                action: ConfigAction::Validate
            }
        );
        assert_eq!(
            parse(&v(&["config", "path"])).unwrap().command,
            Command::Config {
                action: ConfigAction::Path
            }
        );
        assert_eq!(
            parse(&v(&["config", "show"])).unwrap().command,
            Command::Config {
                action: ConfigAction::Show
            }
        );
        assert_eq!(
            parse(&v(&["config"])),
            Err(CliError::MissingArgument("path|show|validate"))
        );
        assert_eq!(
            parse(&v(&["config", "frobnicate"])),
            Err(CliError::UnknownSubcommand {
                command: "config",
                got: "frobnicate".into()
            })
        );
    }

    #[test]
    fn parses_meeting_audio_status_and_retry() {
        assert_eq!(parse(&v(&["meeting", "audio", "status"])).unwrap().command, Command::MeetingSystemAudioStatus);
        assert_eq!(parse(&v(&["meeting", "audio", "retry"])).unwrap().command, Command::MeetingSystemAudioRetry);
        assert!(parse(&v(&["meeting", "audio", "allow"])).is_err());
    }

    #[test]
    fn parses_meeting_start() {
        assert_eq!(
            parse(&v(&["meeting", "start"])).unwrap().command,
            Command::MeetingRecordNow { in_person: false }
        );
        assert_eq!(
            parse(&v(&["meeting", "start", "--in-person"])).unwrap().command,
            Command::MeetingRecordNow { in_person: true }
        );
        assert!(parse(&v(&["meeting", "start", "now"])).is_err());
        assert_eq!(
            parse(&v(&["meeting", "poll", "12"])).unwrap().command,
            Command::MeetingStartStatus { id: 12 }
        );
        assert!(parse(&v(&["meeting", "poll"])).is_err());
        assert!(parse(&v(&["meeting", "poll", "0"])).is_err());
        assert!(parse(&v(&["meeting", "poll", "12", "extra"])).is_err());
    }

    #[test]
    fn parses_calendar_reminder_commands() {
        assert_eq!(
            parse(&v(&["meeting", "reminder", "show"])).unwrap().command,
            Command::MeetingReminder { minutes: None }
        );
        for minutes in ["0", "1", "5", "10"] {
            assert_eq!(
                parse(&v(&["meeting", "reminder", "set", minutes]))
                    .unwrap()
                    .command,
                Command::MeetingReminder {
                    minutes: minutes.parse().ok()
                }
            );
        }
        for off in [&["meeting", "reminder", "off"][..], &["meeting", "reminder", "set", "off"]] {
            assert_eq!(
                parse(&v(off)).unwrap().command,
                Command::MeetingReminder { minutes: Some(0) }
            );
        }
        assert!(parse(&v(&["meeting", "reminder", "set", "2"])).is_err());
        assert!(parse(&v(&["meeting", "reminder", "show", "extra"])).is_err());
    }

    #[test]
    fn parses_meeting_microphone_commands() {
        assert_eq!(
            parse(&v(&["meeting", "microphone", "show"]))
                .unwrap()
                .command,
            Command::MeetingMicrophone(MicrophoneCommand::Show)
        );
        assert_eq!(
            parse(&v(&["meeting", "microphone", "set", "Studio", "Mic"]))
                .unwrap()
                .command,
            Command::MeetingMicrophone(MicrophoneCommand::Set {
                microphone: Some("Studio Mic".into()),
            })
        );
        assert!(parse(&v(&["meeting", "microphone", "default", "typo"])).is_err());
        assert!(parse(&v(&["meeting", "microphone", "show", "typo"])).is_err());
    }

    #[test]
    fn parses_mail_triage_commands() {
        use crate::command::{MailCommand, MailTriageList};
        assert_eq!(
            parse(&v(&["mail", "triage"])).unwrap().command,
            Command::Mail(MailCommand::TriageList(MailTriageList::default()))
        );
        assert_eq!(
            parse(&v(&[
                "mail",
                "triage",
                "--bucket",
                "other",
                "--attention",
                "fyi",
                "--label",
                "marketing",
                "--handled",
            ]))
            .unwrap()
            .command,
            Command::Mail(MailCommand::TriageList(MailTriageList {
                bucket: Some("other".into()),
                attention: Some("fyi".into()),
                label: Some("marketing".into()),
                handled: true,
            }))
        );
        assert!(matches!(
            parse(&v(&["mail", "triage", "--bucket", "sideways"])),
            Err(CliError::UnknownSubcommand {
                command: "mail bucket",
                ..
            })
        ));
        assert_eq!(
            parse(&v(&["mail", "triage", "show", "gmail:18f3"]))
                .unwrap()
                .command,
            Command::Mail(MailCommand::TriageShow {
                thread_key: "gmail:18f3".into()
            })
        );
        assert_eq!(
            parse(&v(&["mail", "triage", "show"])),
            Err(CliError::MissingArgument("<thread_key>"))
        );
        assert_eq!(
            parse(&v(&[
                "mail",
                "triage",
                "set",
                "gmail:18f3",
                "--label",
                "news",
                "--bucket",
                "other",
                "--unhandled",
            ]))
            .unwrap()
            .command,
            Command::Mail(MailCommand::TriageSet {
                thread_key: "gmail:18f3".into(),
                label: Some("news".into()),
                bucket: Some("other".into()),
                handled: Some(false),
            })
        );
        assert_eq!(
            parse(&v(&["mail", "triage", "set", "gmail:18f3", "--handled"]))
                .unwrap()
                .command,
            Command::Mail(MailCommand::TriageSet {
                thread_key: "gmail:18f3".into(),
                label: None,
                bucket: None,
                handled: Some(true),
            })
        );
        // An empty correction is a parse error, not a request the daemon refuses later.
        assert!(matches!(
            parse(&v(&["mail", "triage", "set", "gmail:18f3"])),
            Err(CliError::MissingArgument(_))
        ));
        assert_eq!(
            parse(&v(&["mail", "triage", "archive", "gmail:18f3"]))
                .unwrap()
                .command,
            Command::Mail(MailCommand::TriageArchive {
                thread_key: "gmail:18f3".into()
            })
        );
        assert_eq!(
            parse(&v(&["mail", "triage", "unarchive", "gmail:18f3"]))
                .unwrap()
                .command,
            Command::Mail(MailCommand::TriageUnarchive {
                thread_key: "gmail:18f3".into()
            })
        );
        assert_eq!(
            parse(&v(&["mail", "history"])).unwrap().command,
            Command::Mail(MailCommand::History)
        );
        assert_eq!(
            parse(&v(&["mail"])),
            Err(CliError::MissingArgument("triage|labels|rules|history|status|on|off"))
        );
        // The master switch, on every face (invariant 6): the Settings toggle, `mail.settings.*`
        // over MCP/REST, and these two words on the CLI.
        assert_eq!(
            parse(&v(&["mail", "status"])).unwrap().command,
            Command::Mail(MailCommand::Status)
        );
        assert_eq!(
            parse(&v(&["mail", "on"])).unwrap().command,
            Command::Mail(MailCommand::SetEnabled { enabled: true })
        );
        assert_eq!(
            parse(&v(&["mail", "off"])).unwrap().command,
            Command::Mail(MailCommand::SetEnabled { enabled: false })
        );
        assert!(matches!(
            parse(&v(&["mail", "frobnicate"])),
            Err(CliError::UnknownSubcommand { command: "mail", .. })
        ));
    }

    #[test]
    fn parses_mail_label_and_rule_commands() {
        use crate::command::{MailCommand, MailLabelSet};
        assert_eq!(
            parse(&v(&["mail", "labels"])).unwrap().command,
            Command::Mail(MailCommand::LabelsList)
        );
        assert_eq!(
            parse(&v(&[
                "mail",
                "labels",
                "set",
                "marketing",
                "--auto-archive",
                "on",
                "--confirm-standing",
            ]))
            .unwrap()
            .command,
            Command::Mail(MailCommand::LabelsSet(MailLabelSet {
                key_or_name: "marketing".into(),
                auto_archive: Some(true),
                confirm_standing: true,
                ..MailLabelSet::default()
            }))
        );
        assert_eq!(
            parse(&v(&[
                "mail",
                "labels",
                "set",
                "Vendors",
                "--definition",
                "Invoices from suppliers",
                "--bucket",
                "other",
                "--sync",
                "off",
                "--enabled",
                "on",
            ]))
            .unwrap()
            .command,
            Command::Mail(MailCommand::LabelsSet(MailLabelSet {
                key_or_name: "Vendors".into(),
                definition: Some("Invoices from suppliers".into()),
                bucket: Some("other".into()),
                sync_to_gmail: Some(false),
                enabled: Some(true),
                ..MailLabelSet::default()
            }))
        );
        assert!(matches!(
            parse(&v(&["mail", "labels", "set", "marketing", "--auto-archive", "maybe"])),
            Err(CliError::UnknownSubcommand {
                command: "mail labels set --auto-archive",
                ..
            })
        ));
        assert_eq!(
            parse(&v(&["mail", "labels", "set"])),
            Err(CliError::MissingArgument("<key-or-name>"))
        );
        assert_eq!(
            parse(&v(&["mail", "labels", "rm", "custom:vendors"]))
                .unwrap()
                .command,
            Command::Mail(MailCommand::LabelsRemove {
                key: "custom:vendors".into()
            })
        );
        assert_eq!(
            parse(&v(&["mail", "rules"])).unwrap().command,
            Command::Mail(MailCommand::RulesList)
        );
        assert_eq!(
            parse(&v(&[
                "mail",
                "rules",
                "add",
                "--domain",
                "news.example.com",
                "--label",
                "news",
                "--bucket",
                "other",
            ]))
            .unwrap()
            .command,
            Command::Mail(MailCommand::RulesAdd {
                match_kind: "domain".into(),
                match_value: "news.example.com".into(),
                label: "news".into(),
                bucket: Some("other".into()),
            })
        );
        assert_eq!(
            parse(&v(&["mail", "rules", "add", "--addr", "a@b.c", "--label", "people"]))
                .unwrap()
                .command,
            Command::Mail(MailCommand::RulesAdd {
                match_kind: "addr".into(),
                match_value: "a@b.c".into(),
                label: "people".into(),
                bucket: None,
            })
        );
        assert_eq!(
            parse(&v(&["mail", "rules", "add", "--label", "people"])),
            Err(CliError::MissingArgument("--addr X | --domain X"))
        );
        assert_eq!(
            parse(&v(&["mail", "rules", "add", "--addr", "a@b.c"])),
            Err(CliError::MissingArgument("--label KEY"))
        );
        assert_eq!(
            parse(&v(&["mail", "rules", "rm", "7"])).unwrap().command,
            Command::Mail(MailCommand::RulesRemove { id: 7 })
        );
        assert_eq!(
            parse(&v(&["mail", "rules", "rm", "x"])),
            Err(CliError::BadId("x".into()))
        );
    }

    #[test]
    fn parses_voice_microphone_commands() {
        assert_eq!(
            parse(&v(&["voice", "microphone"])).unwrap().command,
            Command::VoiceMicrophone(MicrophoneCommand::Show)
        );
        assert_eq!(
            parse(&v(&["voice", "microphone", "show"])).unwrap().command,
            Command::VoiceMicrophone(MicrophoneCommand::Show)
        );
        assert_eq!(
            parse(&v(&["voice", "microphone", "set", "Studio", "Mic"]))
                .unwrap()
                .command,
            Command::VoiceMicrophone(MicrophoneCommand::Set {
                microphone: Some("Studio Mic".into()),
            })
        );
        // `default` is the only reset; `set` with no name is a hard error, not a quiet default.
        assert_eq!(
            parse(&v(&["voice", "microphone", "default"]))
                .unwrap()
                .command,
            Command::VoiceMicrophone(MicrophoneCommand::Set { microphone: None })
        );
        assert_eq!(
            parse(&v(&["voice", "microphone", "set"])),
            Err(CliError::MissingArgument("<name>"))
        );
        assert!(parse(&v(&["voice", "microphone", "frobnicate"])).is_err());
        assert!(parse(&v(&["voice", "typo"])).is_err());
    }

    #[test]
    fn parses_agent_source_show_use_and_revoke() {
        assert_eq!(parse(&v(&["agent-source"])).unwrap().command, Command::AgentSource(AgentSourceCommand::Show));
        assert_eq!(parse(&v(&["agent-source", "show"])).unwrap().command, Command::AgentSource(AgentSourceCommand::Show));
        assert_eq!(parse(&v(&["agent-source", "revoke"])).unwrap().command, Command::AgentSource(AgentSourceCommand::Revoke));
        assert_eq!(
            parse(&v(&["agent-source", "use", "claude-code"])).unwrap().command,
            Command::AgentSource(AgentSourceCommand::Use { source: "claude-code".into(), model: None })
        );
        assert_eq!(
            parse(&v(&["agent-source", "use", "openai", "--model", "gpt-4o"])).unwrap().command,
            Command::AgentSource(AgentSourceCommand::Use { source: "openai".into(), model: Some("gpt-4o".into()) })
        );
        assert!(parse(&v(&["agent-source", "use"])).is_err());
        assert!(parse(&v(&["agent-source", "grant"])).is_err());
        assert_eq!(
            parse(&v(&["agent-source", "openai-store", "off"])).unwrap().command,
            Command::AgentSource(AgentSourceCommand::OpenAiStore(false))
        );
        assert_eq!(
            parse(&v(&["agent-source", "openai-store", "on"])).unwrap().command,
            Command::AgentSource(AgentSourceCommand::OpenAiStore(true))
        );
        assert!(parse(&v(&["agent-source", "openai-store"])).is_err());
        assert!(parse(&v(&["agent-source", "openai-store", "yes"])).is_err());
    }

    #[test]
    fn parses_connections_list_disconnect_and_composio() {
        let connections = |args: &[&str]| parse(&v(args)).map(|p| p.command);
        assert_eq!(connections(&["connections"]).unwrap(), Command::Connections(ConnectionsCommand::List));
        assert_eq!(connections(&["connections", "list"]).unwrap(), Command::Connections(ConnectionsCommand::List));
        assert_eq!(
            connections(&["connections", "disconnect", "gcal"]).unwrap(),
            Command::Connections(ConnectionsCommand::Disconnect { service: "gcal".into() })
        );
        assert_eq!(
            connections(&["connections", "composio", "revoke"]).unwrap(),
            Command::Connections(ConnectionsCommand::ComposioRevoke)
        );
        assert_eq!(
            connections(&["connections", "composio", "draft-only", "on"]).unwrap(),
            Command::Connections(ConnectionsCommand::ComposioDraftOnly(true))
        );
        assert_eq!(
            connections(&["connections", "composio", "draft-only", "off"]).unwrap(),
            Command::Connections(ConnectionsCommand::ComposioDraftOnly(false))
        );
        assert_eq!(
            connections(&["connections", "gmail", "draft-only", "on"]).unwrap(),
            Command::Connections(ConnectionsCommand::GmailDraftOnly(true))
        );
        assert_eq!(
            connections(&["connections", "gmail", "draft-only", "off"]).unwrap(),
            Command::Connections(ConnectionsCommand::GmailDraftOnly(false))
        );
        // Connecting and granting consent are not CLI verbs: they are in the app.
        for args in [
            &["connections", "disconnect"][..],
            &["connections", "connect", "gcal"],
            &["connections", "composio", "grant"],
            &["connections", "composio"],
            &["connections", "composio", "draft-only", "maybe"],
            &["connections", "gmail", "draft-only", "maybe"],
            &["connections", "gmail"],
        ] {
            assert!(parse(&v(args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn parses_capture_exclusions_list_and_set() {
        assert_eq!(parse(&v(&["capture-exclusions", "list"])).unwrap().command, Command::CaptureExclusions(CaptureExclusionsCommand::List));
        assert_eq!(parse(&v(&["capture-exclusions", "set", r#"{"apps":["com.example.Secret"],"websites":["private.example"]}"#])).unwrap().command, Command::CaptureExclusions(CaptureExclusionsCommand::Set { body: r#"{"apps":["com.example.Secret"],"websites":["private.example"]}"#.into() }));
    }
}

#[cfg(test)]
mod error_message_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn err(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        parse(&args).err().map(|e| e.message()).unwrap_or_default()
    }

    /// Issue #779: an unknown subcommand names the command's real choices, never a
    /// generic "list|get", and an id that is not one is not called an id.
    #[test]
    fn errors_name_the_real_choices() {
        let capture = err(&["capture", "foo"]);
        assert!(capture.contains("status | pause"), "{capture}");
        assert!(!capture.contains("list|get"), "{capture}");
        let config = err(&["config", "foo"]);
        assert!(config.contains("path | show | validate"), "{config}");
        let wiki = err(&["wiki", "evidence", "12"]);
        assert!(wiki.contains("person-N"), "{wiki}");
        assert!(!wiki.contains("invalid id"), "{wiki}");
        let skill = err(&["skills", "save", "/nonexistent/file.md"]);
        assert!(skill.contains("could not read the skill file"), "{skill}");
        let export = err(&["export", "status", "0"]);
        assert!(export.contains("positive integer"), "{export}");
    }
}

#[cfg(test)]
mod memory_note_tests {
    #[test]
    fn memory_notes_have_read_and_revision_checked_write_commands() {
        use crate::{parse,wire};
        for (args,path) in [
            (vec!["memory-notes","list"],"/v1/memory/user-notes"),
            (vec!["memory-notes","show","2"],"/v1/memory/user-notes/2"),
            (vec!["memory-notes","change",r#"{"action":"forget","id":2,"revision":1}"#],"/v1/memory/user-notes/change"),
        ] {
            let invocation=parse::parse(&args.iter().map(|s|s.to_string()).collect::<Vec<_>>()).unwrap();
            assert_eq!(wire::to_call(&invocation.command,false).unwrap().path,path);
        }
        assert!(parse::parse(&["memory-notes".into(),"change".into(),r#"{"action":"forget","id":2}"#.into()]).is_err());
    }
}

/// `principal import --from <memory.db> --root <dir> --principal <id>` (E2-T6). Flags may come in
/// any order; each is required once.
fn parse_principal(rest: &[String]) -> Result<Command, CliError> {
    match rest.first().map(String::as_str) {
        Some("import") => {}
        Some(other) => {
            return Err(CliError::UnknownSubcommand { command: "principal", got: other.to_string() })
        }
        None => return Err(CliError::MissingArgument("import")),
    }
    let mut from = None;
    let mut root = None;
    let mut principal = None;
    let mut i = 1;
    while i < rest.len() {
        let value = rest.get(i + 1);
        match rest[i].as_str() {
            "--from" => from = Some(value.ok_or(CliError::MissingFlagValue("--from"))?.clone()),
            "--root" => root = Some(value.ok_or(CliError::MissingFlagValue("--root"))?.clone()),
            "--principal" => {
                principal = Some(value.ok_or(CliError::MissingFlagValue("--principal"))?.clone())
            }
            other => {
                return Err(CliError::UnknownSubcommand { command: "principal import", got: other.to_string() })
            }
        }
        i += 2;
    }
    Ok(Command::Principal(PrincipalCommand::Import {
        from: from.ok_or(CliError::MissingArgument("--from <memory.db>"))?,
        root: root.ok_or(CliError::MissingArgument("--root <dir>"))?,
        principal: principal.ok_or(CliError::MissingArgument("--principal <id>"))?,
    }))
}

#[cfg(test)]
mod principal_tests {
    use super::*;

    #[test]
    fn principal_import_takes_its_three_flags_in_any_order() {
        let args: Vec<String> = ["principal", "import", "--root", "/tmp/p", "--principal", "alice", "--from", "/tmp/memory.db"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse(&args).unwrap().command,
            Command::Principal(PrincipalCommand::Import {
                from: "/tmp/memory.db".into(),
                root: "/tmp/p".into(),
                principal: "alice".into(),
            })
        );
        for bad in [
            vec!["principal"],
            vec!["principal", "export"],
            vec!["principal", "import", "--from", "/tmp/memory.db", "--root", "/tmp/p"],
            vec!["principal", "import", "--from"],
            vec!["principal", "import", "--bogus", "x", "--from", "a", "--root", "b", "--principal", "c"],
        ] {
            let args: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(parse(&args).is_err(), "{bad:?}");
        }
        // No wire call: it runs locally.
        let local = Command::Principal(PrincipalCommand::Import {
            from: "a".into(),
            root: "b".into(),
            principal: "c".into(),
        });
        assert!(crate::wire::to_call(&local, false).is_none());
        assert!(local.tool().is_none());
    }
}

/// A principal id as the server accepts it: `[a-z0-9][a-z0-9_-]{0,63}`.
fn principal_slug_ok(principal: &str) -> bool {
    let mut chars = principal.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && principal.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
