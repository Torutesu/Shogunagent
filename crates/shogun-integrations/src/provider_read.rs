//! Provider-specific read contracts. Connect checks the advertised schema and executes a bounded
//! read before persisting a grant. A renamed or incompatible tool never becomes a false success.
use crate::{result::payload, rpc::McpRpc};
use serde_json::{json, Value};
use shogun_mcp::{scope::Service, sync::SyncCursor};

/// `list_events` fields that only narrow or order the read (the schedule window): dropped, not
/// refused, when the server does not advertise them.
const OPTIONAL_LIST_EVENTS_FIELDS: &[&str] = &["start", "end", "single_events", "order_by"];

/// Bind the hub's stable Google read fields to names actually advertised by that server. Unknown
/// fields and missing required parameters fail closed; provider defaults are never guessed.
pub fn bind_google_arguments(
    tool: &str,
    arguments: &Value,
    tools: &[Value],
) -> Result<Value, String> {
    let schema = tools
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(tool))
        .and_then(|entry| entry.get("inputSchema"))
        .ok_or("Google tool is unavailable")?;
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or("Google tool schema is unavailable")?;
    let mut out = serde_json::Map::new();
    let mut order_by: Option<(String, Value)> = None;
    let mut single_events_bound = false;
    for (key, value) in arguments
        .as_object()
        .ok_or("tool arguments must be an object")?
    {
        let aliases: &[&str] = match key.as_str() {
            "max_results" | "limit" => &[
                "max_results",
                "maxResults",
                "pageSize",
                "page_size",
                "limit",
            ],
            "page_token" => &["page_token", "pageToken"],
            "single_events" => &["single_events", "singleEvents"],
            "order_by" => &["order_by", "orderBy"],
            "start" => &["time_min", "timeMin", "start_time", "startTime", "start"],
            "end" => &["time_max", "timeMax", "end_time", "endTime", "end"],
            "file_id" => &["file_id", "fileId"],
            "calendar_id" => &["calendar_id", "calendarId"],
            "duration_minutes" => &["duration_minutes", "durationMinutes"],
            _ => &[],
        };
        let advertised = if properties.contains_key(key) {
            Some(key.as_str())
        } else {
            aliases
                .iter()
                .copied()
                .find(|alias| properties.contains_key(*alias))
        };
        let optional = tool == "list_events" && OPTIONAL_LIST_EVENTS_FIELDS.contains(&key.as_str());
        let bound = match advertised {
            // The calendar read's window narrows what comes back; a server that does not
            // advertise it (or types it differently) gets the unwindowed read it always had,
            // rather than a failed sync.
            Some(bound) if optional && !type_matches(properties, bound, value) => continue,
            Some(bound) => bound,
            None if optional => continue,
            None => return Err("Google tool argument is unsupported".into()),
        };
        if key == "order_by" && tool == "list_events" {
            order_by = Some((bound.to_string(), value.clone()));
            continue;
        }
        if key == "single_events" {
            single_events_bound = true;
        }
        out.insert(bound.into(), value.clone());
    }
    // Google accepts `orderBy=startTime` only with `singleEvents=true`: never send it alone.
    if let (Some((bound, value)), true) = (order_by, single_events_bound) {
        out.insert(bound, value);
    }
    if tool == "list_events" {
        if let Some(id) = ["calendar_id", "calendarId"]
            .iter()
            .find(|key| properties.contains_key(**key))
        {
            out.entry((*id).to_string())
                .or_insert_with(|| json!("primary"));
        }
    }
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required
                .iter()
                .any(|key| key.as_str().is_none_or(|key| !out.contains_key(key)))
        })
    {
        return Err("Google tool requires unsupported arguments".into());
    }
    for (key, value) in &out {
        if !type_matches(properties, key, value) {
            return Err("Google tool argument type is incompatible".into());
        }
    }
    Ok(Value::Object(out))
}

/// Whether `value` fits the JSON type the schema declares for `key` (an untyped field fits).
fn type_matches(properties: &serde_json::Map<String, Value>, key: &str, value: &Value) -> bool {
    match properties
        .get(key)
        .and_then(|property| property.get("type"))
        .and_then(Value::as_str)
    {
        None => true,
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("array") => value.is_array(),
        Some("object") => value.is_object(),
        Some(_) => false,
    }
}

/// Granola publishes tool names but not wire schemas. Verify the authenticated server's
/// advertised contract before a read or before marking a connection usable.
pub fn validate_granola_tools(tools: &[Value]) -> Result<(), String> {
    let schema_for = |name| {
        tools.iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|tool| tool.get("inputSchema"))
            .filter(|schema| schema.get("type").and_then(Value::as_str) == Some("object"))
            .ok_or("Granola read tool is unavailable")
    };
    let list = schema_for("list_meetings")?;
    if list.get("required").and_then(Value::as_array).is_some_and(|required| !required.is_empty()) {
        return Err("Granola list requires unsupported arguments".into());
    }
    let get = schema_for("get_meetings")?;
    let ids = get.get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get("meeting_ids"))
        .ok_or("Granola meeting IDs are unsupported")?;
    if ids.get("type").and_then(Value::as_str) != Some("array")
        || ids.get("items").and_then(|items| items.get("type")).and_then(Value::as_str) != Some("string")
    {
        return Err("Granola meeting ID schema is incompatible".into());
    }
    let required = get.get("required").and_then(Value::as_array)
        .ok_or("Granola meeting ID requirement is unavailable")?;
    if required.len() != 1 || required[0].as_str() != Some("meeting_ids") {
        return Err("Granola meeting read requires unsupported arguments".into());
    }
    Ok(())
}

/// Bind stable hub input to the current Granola schema. One meeting per call keeps reads bounded.
pub fn granola_arguments(tool: &str, arguments: &Value, tools: &[Value]) -> Result<Value, String> {
    validate_granola_tools(tools)?;
    let args = arguments.as_object().ok_or("Granola arguments must be an object")?;
    match tool {
        "list_meetings" if args.is_empty() => Ok(json!({})),
        "get_meetings" if args.len() == 1 => {
            let id = args.get("meeting_id").and_then(Value::as_str)
                .or_else(|| args.get("meeting_ids")
                    .and_then(Value::as_array)
                    .filter(|ids| ids.len() == 1)
                    .and_then(|ids| ids[0].as_str()))
                .filter(|id| !id.trim().is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))
                .ok_or("Granola meeting ID is invalid")?;
            Ok(json!({"meeting_ids": [id]}))
        }
        _ => Err("Granola read arguments are unsupported".into()),
    }
}

pub fn validate_tools(service: Service, tools: &[Value]) -> Result<(), String> {
    let contracts: &[(&str, &[(&str, &str)])] = match service {
        Service::GitHub => &[
            ("get_me", &[]),
            (
                "search_issues",
                &[
                    ("query", "string"),
                    ("perPage", "number"),
                    ("page", "number"),
                    ("sort", "string"),
                    ("order", "string"),
                ],
            ),
        ],
        Service::Linear => &[(
            "list_issues",
            &[
                ("limit", "number"),
                ("assignee", "string"),
                ("cursor", "string"),
                ("query", "string"),
                // Sync sends the watermark here; fail Connect loudly if the server drops it.
                ("updatedAt", "string"),
            ],
        )],
        _ => return Err("provider read contract is not implemented".into()),
    };
    for (name, arguments) in contracts {
        let schema = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|tool| tool.get("inputSchema"))
            .ok_or("required read tool is unavailable")?;
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, kind) in *arguments {
            let actual = properties
                .and_then(|props| props.get(*key))
                .and_then(|prop| prop.get("type"))
                .and_then(Value::as_str);
            if actual != Some(kind) && !(*kind == "number" && actual == Some("integer")) {
                return Err("provider read tool schema is incompatible".into());
            }
        }
        if schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| {
                required
                    .iter()
                    .any(|key| !arguments.iter().any(|(arg, _)| key.as_str() == Some(arg)))
            })
        {
            return Err("provider read tool requires unsupported arguments".into());
        }
    }
    Ok(())
}

/// Notion keys omit the `notion-` prefix. Missing entries never imply permission.
pub fn notion_search_tool(access: &Value) -> Result<&'static str, String> {
    let map = access
        .get("current_tool_access")
        .ok_or("Notion tool access is unavailable")?;
    let status = |name: &str| {
        map.get(name)
            .and_then(|entry| entry.get("status"))
            .and_then(Value::as_str)
    };
    if status("ai_search") == Some("available") {
        return Ok("notion-ai-search");
    }
    if matches!(status("search"), Some("available" | "available_with_limit")) {
        return Ok("notion-search");
    }
    if matches!(
        status("ai_search"),
        Some("upgrade_required" | "plan_required")
    ) {
        return Ok("notion-ai-search");
    }
    Err("Notion workspace must enable a supported search tool".into())
}

pub fn validate_notion_search(tools: &[Value], tool: &str) -> Result<(), String> {
    let schema = tools
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(tool))
        .and_then(|entry| entry.get("inputSchema"))
        .ok_or("Notion search tool is unavailable")?;
    if schema
        .pointer("/properties/query/type")
        .and_then(Value::as_str)
        != Some("string")
        || schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.iter().any(|name| name.as_str() != Some("query")))
    {
        return Err("Notion search tool schema is incompatible".into());
    }
    Ok(())
}

pub fn validate_slack_search(tools: &[Value]) -> Result<(), String> {
    let schema = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("slack_search_public"))
        .and_then(|tool| tool.get("inputSchema"))
        .ok_or("Slack public search is unavailable; check the app's MCP feature and user scopes")?;
    for (key, kind) in [
        ("query", "string"),
        ("limit", "number"),
        ("only_my_channels", "boolean"),
        ("content_types", "string"),
    ] {
        let actual = schema
            .get("properties")
            .and_then(|properties| properties.get(key))
            .and_then(|property| property.get("type"))
            .and_then(Value::as_str);
        if actual != Some(kind) && !(kind == "number" && actual == Some("integer")) {
            return Err("Slack public search schema is incompatible".into());
        }
    }
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required.iter().any(|name| {
                !matches!(
                    name.as_str(),
                    Some("query" | "limit" | "only_my_channels" | "content_types")
                )
            })
        })
    {
        return Err("Slack public search requires unsupported arguments".into());
    }
    Ok(())
}

/// `YYYY-MM-DD` (UTC) of the sync watermark; day granularity, the watermark day is re-fetched.
fn watermark_date(cursor: &SyncCursor) -> String {
    let z = cursor.watermark_ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn valid_github_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// GitHub page token is `<page>:<login>`; the login rides the cursor so no process-wide cache is needed.
fn parse_github_token(token: &str) -> Option<(u32, &str)> {
    let (page, login) = token.split_once(':')?;
    valid_github_login(login).then_some(())?;
    Some((page.parse().ok()?, login))
}

pub fn arguments<R: McpRpc>(
    rpc: &R,
    service: Service,
    limit: u32,
    cursor: &SyncCursor,
) -> Result<(Value, Option<String>), String> {
    match service {
        Service::GitHub => {
            let (page, login) = match cursor.page_token.as_deref() {
                Some(t) => {
                    let (p, l) = parse_github_token(t).ok_or("invalid GitHub page cursor")?;
                    (p, l.to_string())
                }
                None => {
                    // Resolve the authenticated login rather than interpreting @me through a search engine.
                    let me = payload(&rpc.call_tool(Service::GitHub, "get_me", json!({}))?)?;
                    let login = me
                        .get("login")
                        .and_then(Value::as_str)
                        .filter(|l| valid_github_login(l))
                        .ok_or("GitHub identity response is incompatible")?
                        .to_string();
                    (1, login)
                }
            };
            if !(1..=40).contains(&page) {
                return Err("GitHub page cursor exceeds search limit".into());
            }
            Ok((
                json!({"query":format!("involves:{login} updated:>={}", watermark_date(cursor)), "sort":"updated", "order":"desc", "perPage":limit.clamp(1,100), "page":page}),
                Some(login),
            ))
        }
        Service::Linear => {
            let mut args = json!({"limit":limit.clamp(1,100), "assignee":"me", "updatedAt":format!("{}T00:00:00Z", watermark_date(cursor))});
            if let Some(cursor) = &cursor.page_token {
                args["cursor"] = json!(cursor);
            }
            Ok((args, None))
        }
        _ => Err("provider read contract is not implemented".into()),
    }
}

pub fn next_page(
    service: Service,
    result: &Value,
    cursor: &SyncCursor,
    login: Option<&str>,
    _normalized_count: usize,
    limit: u32,
) -> Result<Option<String>, String> {
    let limit = limit.clamp(1, 100);
    let data = payload(result)?;
    match service {
        Service::GitHub => {
            let page = match cursor.page_token.as_deref() {
                Some(t) => parse_github_token(t).ok_or("invalid GitHub page cursor")?.0,
                None => 1,
            };
            let login = login.ok_or("GitHub page cursor lacks identity")?;
            let count = data
                .get("items")
                .and_then(Value::as_array)
                .ok_or("GitHub search result is incompatible")?
                .len();
            let total = data
                .get("total_count")
                .and_then(Value::as_u64)
                .ok_or("GitHub search result is incompatible")?;
            Ok(
                (count == limit as usize && u64::from(page) * u64::from(limit) < total.min(1000))
                    .then(|| format!("{}:{login}", page + 1)),
            )
        }
        Service::Linear => {
            if data.get("issues").and_then(Value::as_array).is_none() {
                return Err("Linear issue result is incompatible".into());
            }
            let has_next = data
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .ok_or("Linear pagination response is incompatible")?;
            if has_next {
                return data
                    .get("cursor")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| Some(s.into()))
                    .ok_or_else(|| "Linear pagination response is incompatible".into());
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Approved writes must still match the current provider contract before crossing the boundary.
pub fn validate_arguments(tool: &str, arguments: &Value, tools: &[Value]) -> Result<(), String> {
    let schema = tools
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(tool))
        .and_then(|entry| entry.get("inputSchema"))
        .ok_or("approved write tool is unavailable")?;
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or("approved write schema is unavailable")?;
    let arguments = arguments
        .as_object()
        .ok_or("approved write arguments must be an object")?;
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required.iter().any(|key| {
                key.as_str()
                    .is_none_or(|key| !arguments.contains_key(key))
            })
        })
    {
        return Err("approved write requires additional arguments".into());
    }
    for (key, value) in arguments {
        let property = properties
            .get(key)
            .ok_or("approved write argument is unsupported")?;
        if let Some(kind) = property.get("type").and_then(Value::as_str) {
            let valid = match kind {
                "string" => value.is_string(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                "array" => value.is_array(),
                "object" => value.is_object(),
                _ => false,
            };
            if !valid {
                return Err("approved write argument type is incompatible".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn granola_reads_require_advertised_schema_and_one_meeting_id() {
        use serde_json::json;
        let tools = vec![
            json!({"name":"list_meetings","inputSchema":{"type":"object","properties":{"time_range":{"type":"string"}}}}),
            json!({"name":"get_meetings","inputSchema":{"type":"object","properties":{"meeting_ids":{"type":"array","items":{"type":"string"}}},"required":["meeting_ids"]}}),
        ];
        assert!(super::validate_granola_tools(&tools).is_ok());
        assert_eq!(super::granola_arguments("list_meetings", &json!({}), &tools), Ok(json!({})));
        assert_eq!(super::granola_arguments("get_meetings", &json!({"meeting_id":"abc"}), &tools), Ok(json!({"meeting_ids":["abc"]})));
        assert!(super::granola_arguments("get_meetings", &json!({"meeting_ids":["a","b"]}), &tools).is_err());
        assert!(super::granola_arguments("get_meetings", &json!({"meeting_id":"abc","write":true}), &tools).is_err());
        assert!(super::granola_arguments("delete_meeting", &json!({}), &tools).is_err());
        let drift = vec![tools[0].clone(), json!({"name":"get_meetings","inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}})];
        assert!(super::validate_granola_tools(&drift).is_err());
        let drift = vec![json!({"name":"list_meetings","inputSchema":{"type":"object","properties":{"limit":{"type":"integer"}},"required":["limit"]}}), tools[1].clone()];
        assert!(super::validate_granola_tools(&drift).is_err());
    }

    #[test]
    fn advertised_write_schema_rejects_missing_address_and_unknown_fields() {
        let tools = vec![
            serde_json::json!({"name":"add_issue_comment","inputSchema":{"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"},"issue_number":{"type":"number"},"body":{"type":"string"}},"required":["owner","repo","issue_number","body"]}}),
        ];
        assert!(super::validate_arguments(
            "add_issue_comment",
            &serde_json::json!({"body":"comment"}),
            &tools
        )
        .is_err());
        assert!(super::validate_arguments(
            "add_issue_comment",
            &serde_json::json!({"owner":"org","repo":"repo","issue_number":1,"body":"comment"}),
            &tools
        )
        .is_ok());
        assert!(super::validate_arguments("add_issue_comment", &serde_json::json!({"owner":"org","repo":"repo","issue_number":1,"body":"comment","target":"wrong"}), &tools).is_err());
    }

    use super::*;

    #[test]
    fn google_binding_uses_advertised_fields_and_refuses_missing_required_arguments() {
        let tools = vec![
            json!({"name":"list_events","inputSchema":{"properties":{"calendarId":{"type":"string"},"maxResults":{"type":"integer"},"timeMin":{"type":"string"}},"required":["calendarId"]}}),
        ];
        assert_eq!(
            bind_google_arguments(
                "list_events",
                &json!({"max_results":1,"start":"2026-09-27T00:00:00Z"}),
                &tools
            )
            .unwrap(),
            json!({"calendarId":"primary","maxResults":1,"timeMin":"2026-09-27T00:00:00Z"})
        );
        assert!(
            bind_google_arguments("list_events", &json!({"unrecognized":true}), &tools).is_err()
        );
        // The schedule window is optional on list_events: unadvertised fields are dropped, and
        // orderBy never goes without singleEvents (Google rejects that pair).
        assert_eq!(
            bind_google_arguments(
                "list_events",
                &json!({"max_results":5,"start":"a","end":"b","single_events":true,"order_by":"startTime"}),
                &tools
            )
            .unwrap(),
            json!({"calendarId":"primary","maxResults":5,"timeMin":"a"})
        );
        // A window field typed differently (an object `start`) is dropped too, not sent.
        let typed = vec![json!({"name":"list_events","inputSchema":{"properties":{"start":{"type":"object"}}}})];
        assert_eq!(bind_google_arguments("list_events", &json!({"start":"a"}), &typed).unwrap(), json!({}));
        let ordered = vec![json!({"name":"list_events","inputSchema":{"properties":{"orderBy":{"type":"string"}}}})];
        assert_eq!(
            bind_google_arguments("list_events", &json!({"single_events":true,"order_by":"startTime"}), &ordered)
                .unwrap(),
            json!({})
        );
        // Elsewhere a missing field still fails closed.
        let busy = vec![json!({"name":"suggest_time","inputSchema":{"properties":{}}})];
        assert!(bind_google_arguments("suggest_time", &json!({"start":"a"}), &busy).is_err());
        let writes = vec![
            json!({"name":"create_event","inputSchema":{"properties":{"summary":{"type":"string"},"start":{"type":"object"}},"required":["summary","start"]}}),
        ];
        assert!(
            bind_google_arguments("create_event", &json!({"summary":"test"}), &writes).is_err()
        );
    }

    #[test]
    fn notion_access_uses_only_advertised_and_documented_fallbacks() {
        assert_eq!(
            notion_search_tool(
                &json!({"current_tool_access":{"ai_search":{"status":"available"}}})
            )
            .unwrap(),
            "notion-ai-search"
        );
        assert_eq!(notion_search_tool(&json!({"current_tool_access":{"search":{"status":"available"},"ai_search":{"status":"upgrade_required"}}})).unwrap(), "notion-search");
        assert_eq!(
            notion_search_tool(
                &json!({"current_tool_access":{"ai_search":{"status":"plan_required"}}})
            )
            .unwrap(),
            "notion-ai-search"
        );
        assert!(notion_search_tool(
            &json!({"current_tool_access":{"ai_search":{"status":"not_enabled"}}})
        )
        .is_err());
        assert!(notion_search_tool(&json!({"current_tool_access":{}})).is_err());
    }

    #[test]
    fn watermark_bounds_query_and_clamped_page_size_paginates() {
        assert_eq!(
            watermark_date(&SyncCursor { watermark_ms: 86_400_000 * 365, ..SyncCursor::default() }),
            "1971-01-01"
        );
        let full = json!({"content":[{"type":"text","text":serde_json::to_string(&json!({"items":vec![json!({}); 100],"total_count":500})).unwrap()}]});
        assert_eq!(
            next_page(Service::GitHub, &full, &SyncCursor::default(), Some("octocat"), 100, 250).unwrap(),
            Some("2:octocat".into())
        );
    }

    #[test]
    fn github_background_query_uses_authenticated_identity_and_limits_pages() {
        struct Identity;
        impl McpRpc for Identity {
            fn call_tool(
                &self,
                service: Service,
                tool: &str,
                args: Value,
            ) -> Result<Value, String> {
                assert_eq!(service, Service::GitHub);
                assert_eq!(tool, "get_me");
                assert_eq!(args, json!({}));
                Ok(json!({"content":[{"type":"text","text":"{\"login\":\"octocat\"}"}]}))
            }
        }
        assert_eq!(
            arguments(&Identity, Service::GitHub, 25, &SyncCursor::default()).unwrap(),
            (
                json!({"query":"involves:octocat updated:>=1970-01-01","sort":"updated","order":"desc","perPage":25,"page":1}),
                Some("octocat".to_string())
            )
        );
        assert!(arguments(
            &Identity,
            Service::GitHub,
            25,
            &SyncCursor {
                page_token: Some("9999:octocat".into()),
                ..SyncCursor::default()
            }
        )
        .is_err());
        assert_eq!(
            arguments(
                &Identity,
                Service::Linear,
                25,
                &SyncCursor {
                    page_token: Some("cursor-1".into()),
                    ..SyncCursor::default()
                }
            )
            .unwrap(),
            (json!({"assignee":"me","limit":25,"cursor":"cursor-1","updatedAt":"1970-01-01T00:00:00Z"}), None)
        );
    }
    #[test]
    fn schema_rejects_drift_and_additional_required_parameters() {
        let mut tools = vec![
            json!({"name":"get_me", "inputSchema":{"type":"object"}}),
            json!({"name":"search_issues", "inputSchema":{"properties":{"query":{"type":"string"},"perPage":{"type":"integer"},"page":{"type":"integer"},"sort":{"type":"string"},"order":{"type":"string"}},"required":["query"]}}),
        ];
        assert!(validate_tools(Service::GitHub, &tools).is_ok());
        tools[1]["inputSchema"]["required"] = json!(["query", "owner"]);
        assert!(validate_tools(Service::GitHub, &tools).is_err());
        tools[1]["inputSchema"]["required"] = json!(["query"]);
        tools[1]["inputSchema"]["properties"]["page"]["type"] = json!("string");
        assert!(validate_tools(Service::GitHub, &tools).is_err());
    }
    #[test]
    fn github_token_carries_login_without_rpc() {
        struct NoRpc;
        impl McpRpc for NoRpc {
            fn call_tool(&self, _: Service, _: &str, _: Value) -> Result<Value, String> {
                Err("unexpected rpc".into())
            }
        }
        let cursor = SyncCursor { page_token: Some("3:octocat".into()), ..SyncCursor::default() };
        let (args, login) = arguments(&NoRpc, Service::GitHub, 25, &cursor).unwrap();
        assert!(args["query"].as_str().unwrap().contains("involves:octocat"));
        assert_eq!(args["page"], 3);
        assert_eq!(login.as_deref(), Some("octocat"));
        let full = json!({"structuredContent":{"total_count":500,"items":vec![json!({}); 25]}});
        assert_eq!(
            next_page(Service::GitHub, &full, &cursor, login.as_deref(), 25, 25).unwrap(),
            Some("4:octocat".into())
        );
    }
    #[test]
    fn pages_are_provider_specific_and_missing_cursors_fail() {
        let cursor = SyncCursor::default();
        assert_eq!(
            next_page(
                Service::GitHub,
                &json!({"structuredContent":{"total_count":27,"items":vec![json!({"id":"record-without-content"});25]}}),
                &cursor,
                Some("octocat"),
                0,
                25
            )
            .unwrap(),
            Some("2:octocat".into())
        );
        assert_eq!(
            next_page(
                Service::GitHub,
                &json!({"structuredContent":{"total_count":0,"items":[]}}),
                &cursor,
                Some("octocat"),
                0,
                25
            )
            .unwrap(),
            None
        );
        assert!(next_page(
            Service::Linear,
            &json!({"structuredContent":{"hasNextPage":true,"issues":[]}}),
            &cursor,
            None,
            25,
            25
        )
        .is_err());
    }
}
