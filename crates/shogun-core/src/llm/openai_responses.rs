//! OpenAI **Responses API** Agent-lane client (`POST {base}/responses`, issue #390).
//!
//! [`super::openai_compat`] speaks only `chat/completions`. The current OpenAI models expose
//! function calling on the Responses API only, and three things the Agent lane wants live there:
//! function tools on an OpenAI BYOK key, `reasoning.effort` that can change between turns, and
//! `previous_response_id` continuation (the earlier turns are not re-sent, so the cached prefix
//! survives). This module is that client. `chat/completions` stays for OpenRouter / Gemini /
//! Groq.
//!
//! Scope and invariants — the same as the other Agent-lane clients:
//! - **Live lane only (invariant 5).** Direct API constructors take [`ByokKey`]; hosted
//!   constructors take `LicenseBearer` and pin requests to the operator gateway. Company
//!   provider keys never reach this process. Neither constructor accepts a Batch key.
//! - **Function tools only.** [`function_tools`] emits `type: "function"` entries built from
//!   SHOGUN's own catalog; hosted OpenAI tools (web search, file search, computer use, shell) have
//!   no constructor here, so every tool the model sees is a catalog entry gated by
//!   `service_gate::authorize_op`.
//! - **Traceability at the true egress point (invariant 3, AR-11).** One digest-only row per
//!   request, written *before* the socket, on `Route::MessagesApi` — a request that left and came
//!   back 401 is still traced.
//! - **Usage (FR-HN-01).** One `UsageRecord` per call, from the response's own `usage` (cached
//!   and cache-write tokens included), or a byte estimate when the provider sent none.
//! - **Secrets.** The key reaches the wire only through `openai_compat::bearer_headers` (the
//!   allow-listed exposure site); provider error text passes [`redact_secrets`] or is dropped.
//! - **`store` is explicit.** The API stores responses when `store` is omitted, so the builder
//!   always sends it. [`OpenAiResponsesConfig::store`] defaults to `false` (no provider-side
//!   retention); `previous_response_id` continuation needs a stored response, so it is only used
//!   when the caller opted in with [`OpenAiResponsesConfig::with_store`]. Without it, each turn
//!   replays its history instead (stateless). The Agent lane takes the answer from the user's
//!   "Let OpenAI store responses" setting (`llm.json` `openai_store`, default off; owner decision
//!   2026-09-25 on #664) through [`OpenAiResponsesConfig::for_agent`].
//!
//! ## Wire reference
//!
//! Field and event names were checked against the vendor's official Python SDK types, which are
//! generated from its OpenAPI spec (`openai-python`, `src/openai/types/responses/`, fetched
//! 2026-09-25; the docs site itself refused the fetch). Verified there:
//! - request: `model`, `input`, `instructions` (not carried over by `previous_response_id`),
//!   `max_output_tokens`, `previous_response_id`, `reasoning.effort`, `store` (defaults to true
//!   when omitted), `stream`, `tools`, `include` (`reasoning.encrypted_content`);
//! - input items: `{role, content}` messages, `function_call_output {call_id, output}`;
//! - function tool: `{type: "function", name, description, parameters, strict}`;
//! - output items: `message` with `output_text` / `refusal` parts, `function_call {call_id, name,
//!   arguments}` (arguments is a JSON string);
//! - response: `id`, `status`, `error.code` (incl. `rate_limit_exceeded`, `server_error`,
//!   `bio_policy`, `misalignment_policy_violation`), `incomplete_details.reason`
//!   (`max_output_tokens`, `content_filter`), `usage {input_tokens, output_tokens,
//!   input_tokens_details {cached_tokens, cache_write_tokens}, output_tokens_details
//!   {reasoning_tokens}}`;
//! - stream events: `response.created`, `response.output_item.added` / `.done` (`output_index`,
//!   `item`), `response.output_text.delta` (`delta`), `response.function_call_arguments.delta` /
//!   `.done` (`delta` / `arguments`), `response.refusal.delta`, `response.completed` /
//!   `response.incomplete` / `response.failed` (`response`), `error` (`code`, `message`).
//!
//! NOT verified against a vendor source (flagged where used):
//! - that `strict` defaults to `true` on this endpoint — [`function_tools`] sends `false`
//!   explicitly either way, because the catalog's schemas are not written for strict mode;
//! - that output items can be replayed verbatim as `input` on a `store: false` turn (the
//!   stateless tool path does exactly that, with `reasoning.encrypted_content` included);
//! - that `response.completed` always carries the full `output` array — [`ResponsesStream`]
//!   rebuilds the output from the item events when it does not.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::openai_compat::{bearer_headers, destination_host, OpenAiCompatConfig};
use super::traceability::{Route, TraceRecord, TraceabilitySink};
use super::transport::{HttpRequest, HttpTransport, Method};
use super::usage::{Usage, UsageHook, UsageOutcome, UsageRecord};
use super::{redact_secrets, ByokKey, LlmError, Secret, StreamEnd};

/// Connection + model settings for the Responses endpoint. The model id is the caller's —
/// never chosen here (default models are issue #389's decision, not this client's).
#[derive(Debug, Clone)]
pub struct OpenAiResponsesConfig {
    /// Base URL up to (not including) `/responses`, no trailing slash
    /// (`https://api.openai.com/v1`).
    pub base_url: String,
    pub model: String,
    /// `max_output_tokens`: visible output **and** reasoning tokens.
    pub max_output_tokens: u32,
    /// Default `reasoning.effort`; a turn can override it (see [`ResponsesTurn`]).
    pub reasoning_effort: Option<String>,
    /// Whether the provider stores the response. Always sent (the API default is `true`).
    /// `previous_response_id` continuation requires `true`.
    pub store: bool,
}

impl OpenAiResponsesConfig {
    /// Config for `model` against `base_url`: 1,024 output tokens, no reasoning override, and
    /// `store: false`.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let mut base_url: String = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            base_url,
            model: model.into(),
            max_output_tokens: 1024,
            reasoning_effort: None,
            store: false,
        }
    }

    /// The Responses config equivalent to a chat/completions config: same endpoint base, model,
    /// token cap and `reasoning_effort` (issue #390: effort comes from
    /// `OpenAiCompatConfig.reasoning_effort`). `include_reasoning` has no Responses equivalent
    /// and is dropped. `store` stays `false`.
    pub fn from_compat(cfg: &OpenAiCompatConfig) -> Self {
        let mut out = Self::new(cfg.base_url.clone(), cfg.model.clone());
        out.max_output_tokens = cfg.max_tokens;
        out.reasoning_effort = cfg.reasoning_effort.clone();
        out
    }

    /// The Agent lane's config: [`Self::from_compat`] with `store` taken from the user's settings
    /// (`llm.json` `openai_store`). Off unless the user turned it on in Settings and OpenAI is the
    /// selected provider ([`LlmSettings::stores_openai_responses`]); the API faces can turn it off
    /// but never on (`agent_source::plan`).
    ///
    /// [`LlmSettings::stores_openai_responses`]: crate::agent_source::LlmSettings::stores_openai_responses
    pub fn for_agent(cfg: &OpenAiCompatConfig, settings: &crate::agent_source::LlmSettings) -> Self {
        Self::from_compat(cfg).with_store(settings.stores_openai_responses())
    }

    /// The chat tool lane's config (issue #390): [`Self::from_compat`] with `store` pinned to
    /// [`crate::agent_source::CHAT_TOOL_LANE_STORES_RESPONSES`] (off, owner decision). Each tool
    /// turn replays the history it needs; nothing is kept on OpenAI's side and no response id
    /// outlives the chat turn.
    pub fn for_chat_tools(cfg: &OpenAiCompatConfig) -> Self {
        Self::from_compat(cfg).with_store(crate::agent_source::CHAT_TOOL_LANE_STORES_RESPONSES)
    }

    pub fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = max;
        self
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Opt in to provider-side storage, which `previous_response_id` continuation needs.
    pub fn with_store(mut self, store: bool) -> Self {
        self.store = store;
        self
    }

    fn destination(&self) -> String {
        destination_host(&self.base_url)
    }

    fn url(&self) -> String {
        format!("{}/responses", self.base_url)
    }
}

/// The selection rule from issue #390: the Responses client serves provider `openai` when the
/// turn carries tools; everything else (OpenRouter, Gemini, Groq, and OpenAI text-only turns)
/// stays on chat/completions. Pure so the shell's `build_agent` and its tests share one answer.
pub fn prefers_responses_api(provider: &str, tools_requested: bool) -> bool {
    provider == "openai" && tools_requested
}

/// Text and function-call lanes served by the company gateway. Model selection stays server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedLane { Chat, Agent, Draft, Scribe, Rewrite, SummaryLive, TranslateLive, QuickTranslate }

impl HostedLane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat", Self::Agent => "agent", Self::Draft => "draft",
            Self::Scribe => "scribe", Self::Rewrite => "rewrite", Self::SummaryLive => "summary_live",
            Self::TranslateLive => "translate_live", Self::QuickTranslate => "quick_translate",
        }
    }

    /// Only live product intents: Batch tags can never acquire this credential path.
    pub fn for_tag(tag: super::prompts::PromptTag) -> Option<Self> {
        use super::prompts::*;
        Some(match tag {
            INLINE_CHAT | MEETING_CHAT => Self::Chat,
            INLINE_DRAFT => Self::Rewrite,
            REPLY_DRAFT => Self::Draft,
            SCRIBE_EDIT => Self::Scribe,
            MEETING_LIVE_SUMMARY => Self::SummaryLive,
            MEETING_LIVE_TRANSLATE => Self::TranslateLive,
            AGENT_TRANSLATE => Self::QuickTranslate,
            // STYLE_NOTE_MINE is left out on purpose: the hourly style-note pass runs with no user
            // action, and background work does not spend the company live budget (invariant 5).
            MEETING_MINUTES | MAIL_TRIAGE | WEB_SUMMARY | USER_SKILL | SKILL_DRAFT
            | MEMORY_SEARCH_EXPAND | MEMORY_SEARCH_RERANK | MEMORY_SEARCH_SUMMARY | CHAT_MEMORY => Self::Agent,
            _ => return None,
        })
    }

    pub fn config(self) -> OpenAiResponsesConfig {
        // This model label is local metadata only. request() never sends it to the gateway.
        OpenAiResponsesConfig::new(super::relay::DEFAULT_RELAY_BASE_URL, format!("hosted/{}", self.as_str()))
    }
}

/// The readiness check for the company live gateway: `GET /v1/ai/usage` with this Mac's licence
/// or trial token. 200 means the gateway serves live AI to this Mac now; anything else (switched
/// off, no plan, unreachable) means it does not. Carries no user text. The caller traces it.
pub fn hosted_status_request(bearer: &super::LicenseBearer) -> Result<HttpRequest, LlmError> {
    let base = super::relay::relay_override_url()
        .unwrap_or_else(|| super::relay::DEFAULT_RELAY_BASE_URL.to_string());
    Ok(HttpRequest::new(Method::Get, format!("{base}/v1/ai/usage"), bearer_headers(bearer.secret()), None)?
        .without_redirects()
        .with_max_body_bytes(64 * 1024)
        .with_timeout(std::time::Duration::from_secs(10)))
}

/// The provider a gateway row's chunk went on to, by the row's purpose (its lane): the egress
/// screen names both hops. The readiness check stops at the gateway. Matches the server's lane
/// map (apps/api/src/live-config.ts): dictation cleanup runs on Groq, every other lane on OpenAI.
pub fn hosted_downstream(purpose: &str) -> Option<&'static str> {
    match purpose {
        "live_status" => None,
        "dictation_cleanup" => Some("api.groq.com"),
        _ => Some("api.openai.com"),
    }
}

/// The host a hosted request goes to, for its traceability row.
pub fn hosted_destination(req: &HttpRequest) -> String {
    destination_host(&req.url)
}

enum ResponsesCredential {
    Byok(ByokKey),
    Hosted { bearer: super::LicenseBearer, lane: HostedLane, base: String },
}

impl ResponsesCredential {
    fn hosted(bearer: super::LicenseBearer, lane: HostedLane) -> Self {
        Self::Hosted { bearer, lane, base: super::relay::relay_override_url()
            .unwrap_or_else(|| super::relay::DEFAULT_RELAY_BASE_URL.to_string()) }
    }
    fn is_hosted(&self) -> bool { matches!(self, Self::Hosted { .. }) }
    fn route(&self) -> Route { if self.is_hosted() { Route::LiveRelay } else { Route::MessagesApi } }
    fn purpose<'a>(&self, fallback: &'a str) -> &'a str {
        match self { Self::Hosted { lane, .. } => lane.as_str(), Self::Byok(_) => fallback }
    }
    fn destination(&self, cfg: &OpenAiResponsesConfig) -> String {
        match self { Self::Byok(_) => cfg.destination(), Self::Hosted { base, .. } => destination_host(base) }
    }
    fn request(&self, cfg: &OpenAiResponsesConfig, payload: String) -> Result<HttpRequest, LlmError> {
        match self {
            Self::Byok(key) => build_responses_request(cfg, key.secret(), payload),
            Self::Hosted { bearer, lane, base } => {
                let raw: Value = serde_json::from_str(&payload)
                    .map_err(|_| LlmError::Parse("invalid live request".into()))?;
                let mut wire = json!({"input": raw["input"]});
                for field in ["instructions", "tools"] {
                    if let Some(value) = raw.get(field) { wire[field] = value.clone(); }
                }
                Ok(HttpRequest::new(Method::Post, format!("{base}/v1/ai/{}", lane.as_str()),
                    bearer_headers(bearer.secret()), Some(wire.to_string()))?
                    .without_redirects()
                    .with_max_body_bytes(4 * 1024 * 1024)
                    .with_timeout(std::time::Duration::from_secs(260)))
            }
        }
    }
    /// A non-2xx answer: the gateway's refusals get SHOGUN's own wording (plan allowance,
    /// unavailable); a provider's goes through the shared mapping.
    fn status_error(&self, step: &str, status: u16, body: &str) -> LlmError {
        if self.is_hosted() {
            super::hosted_status_error(status, body)
        } else {
            super::status_error(step, status, body)
        }
    }
    fn response(&self, body: &str) -> Result<Value, LlmError> {
        if !self.is_hosted() {
            return serde_json::from_str(body).map_err(|_| LlmError::Parse("responses: body is not JSON".into()));
        }
        // The gateway always streams, including calls whose UI needs the complete text.
        let mut decoder = super::sse::SseDecoder::openai_responses();
        let mut stream = ResponsesStream::new();
        for event in decoder.push_events(body) { stream.observe(&event); }
        if let Some(error) = stream.failure() { return Err(error); }
        if !decoder.finished() { return Err(super::sse::closed_early("live gateway")); }
        Ok(stream.into_body())
    }
}

/// A `{role, content}` input message (`user` / `assistant` / `system` / `developer`).
pub fn message_item(role: &str, text: &str) -> Value {
    json!({ "role": role, "content": text })
}

/// A `function_call_output` input item answering the model's `call_id`.
pub fn function_call_output_item(call_id: &str, output: &str) -> Value {
    json!({ "type": "function_call_output", "call_id": call_id, "output": output })
}

/// Prefix on a failed or refused tool result. The Responses item has no `is_error` flag, and a
/// refusal must not reach the model looking like data it can build on.
pub const TOOL_ERROR_PREFIX: &str = "[tool error] ";

/// Map catalog tool definitions (`shogun_mcp::tool_catalog::tool_definitions`, Messages-API
/// shape: `name` / `description` / `input_schema`) onto Responses **function** tools.
///
/// Only `type: "function"` is ever produced — a hosted tool cannot be expressed through this
/// function. `strict` is sent as `false` explicitly: the catalog's schemas are not written for
/// strict mode, and whether the endpoint defaults to strict is not verified here. Entries with
/// no name cannot be called and are skipped.
pub fn function_tools(catalog: &[Value]) -> Vec<Value> {
    catalog
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?;
            let parameters = t
                .get("input_schema")
                .or_else(|| t.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            let mut tool = json!({
                "type": "function",
                "name": name,
                "parameters": parameters,
                "strict": false,
            });
            if let Some(d) = t.get("description").and_then(Value::as_str) {
                tool["description"] = json!(d);
            }
            Some(tool)
        })
        .collect()
}

/// One Responses request's content. `tools` must already be in function-tool shape
/// ([`function_tools`]).
#[derive(Debug, Clone, Default)]
pub struct ResponsesTurn<'a> {
    /// Ours (#123): the system/developer message. Not carried over by `previous_response_id`,
    /// so it is sent on every turn.
    pub instructions: Option<&'a str>,
    pub input: Vec<Value>,
    pub tools: &'a [Value],
    /// Continue from this stored response instead of re-sending the earlier turns.
    pub previous_response_id: Option<&'a str>,
    /// Overrides [`OpenAiResponsesConfig::reasoning_effort`] for this turn only. Changing it
    /// between turns does not drop `previous_response_id`.
    pub reasoning_effort: Option<&'a str>,
}

/// Render the `POST /responses` body for `turn`. `store` is always present (see module docs);
/// `tools`, `instructions`, `reasoning`, `previous_response_id` only when set; `stream` only
/// when streaming.
pub fn build_responses_body(cfg: &OpenAiResponsesConfig, turn: &ResponsesTurn<'_>, stream: bool) -> Value {
    let mut body = json!({
        "model": cfg.model,
        "input": turn.input,
        "max_output_tokens": cfg.max_output_tokens,
        "store": cfg.store,
    });
    if let Some(instructions) = turn.instructions.map(str::trim).filter(|s| !s.is_empty()) {
        body["instructions"] = json!(instructions);
    }
    if !turn.tools.is_empty() {
        body["tools"] = json!(turn.tools);
    }
    if let Some(prev) = turn.previous_response_id {
        body["previous_response_id"] = json!(prev);
    }
    let effort = turn.reasoning_effort.or(cfg.reasoning_effort.as_deref());
    if let Some(effort) = effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    // A stateless tool conversation replays the model's output items, reasoning items included;
    // unstored reasoning is only replayable in its encrypted form.
    if !cfg.store && effort.is_some() && !turn.tools.is_empty() {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if stream {
        body["stream"] = json!(true);
    }
    body
}

/// The HTTPS request carrying `payload` (a rendered body) with the BYOK bearer header.
pub fn build_responses_request(
    cfg: &OpenAiResponsesConfig,
    key: &Secret,
    payload: String,
) -> Result<HttpRequest, LlmError> {
    Ok(HttpRequest::new(Method::Post, cfg.url(), bearer_headers(key), Some(payload))?)
}

/// A `function_call` output item, arguments already parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionCall {
    /// The id the `function_call_output` must carry back.
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

/// A parsed `response` object.
#[derive(Debug, Clone)]
pub struct ParsedResponse {
    pub id: Option<String>,
    /// Concatenated `output_text` parts of the `message` items.
    pub text: String,
    pub function_calls: Vec<FunctionCall>,
    /// The `output` items verbatim (what a stateless next turn replays).
    pub output: Vec<Value>,
    /// The response stopped early (`status: incomplete`, e.g. `max_output_tokens`).
    pub truncated: bool,
    pub usage: Option<Usage>,
    pub reasoning_tokens: Option<u64>,
}

/// Documented Responses `error.code`s that mean "stopped by provider safety monitoring" (#399).
const RESPONSES_POLICY_CODES: &[&str] = &["bio_policy", "misalignment_policy_violation", "content_filter"];

/// The error for a failed response or an `error` stream event. Built from the `code` only —
/// the free-text `message` may quote input, so it never travels into an error string.
fn failure_error(code: Option<&str>) -> LlmError {
    match code {
        Some("rate_limit_exceeded") => LlmError::RateLimited(429, String::new()),
        Some(c) if RESPONSES_POLICY_CODES.contains(&c) => LlmError::PolicyStopped,
        Some(c) => {
            let short: String = c.chars().take(64).collect();
            LlmError::Provider(format!("responses: {}", redact_secrets(&short)))
        }
        None => LlmError::Provider("responses: failed".to_string()),
    }
}

/// `Some` when the response object reports a failure (an `error` object, or `status: failed` /
/// `cancelled`).
pub fn response_failure(response: &Value) -> Option<LlmError> {
    let error = response.get("error").filter(|e| e.is_object());
    let status = response.get("status").and_then(Value::as_str);
    if error.is_none() && !matches!(status, Some("failed") | Some("cancelled")) {
        return None;
    }
    if status == Some("cancelled") && error.is_none() {
        return Some(LlmError::Provider("responses: cancelled".to_string()));
    }
    Some(failure_error(error.and_then(|e| e.get("code")).and_then(Value::as_str)))
}

/// Parse a `response` object (the non-streaming body, or [`ResponsesStream::into_body`]).
///
/// Errors never echo body text: the body is model output over captured data, and error strings
/// reach logs and UI.
pub fn parse_response(body: &Value) -> Result<ParsedResponse, LlmError> {
    if let Some(e) = response_failure(body) {
        return Err(e);
    }
    let mut truncated = false;
    if body.get("status").and_then(Value::as_str) == Some("incomplete") {
        let reason = body
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str);
        if reason == Some("content_filter") {
            return Err(LlmError::PolicyStopped);
        }
        truncated = true;
    }
    let output = body
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Parse("responses: output is not an array".into()))?;

    let mut text = String::new();
    let mut function_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                        Some("refusal") => return Err(LlmError::PolicyStopped),
                        _ => {}
                    }
                }
            }
            Some("function_call") => function_calls.push(parse_function_call(item)?),
            // reasoning and anything newer: history only, never text.
            _ => {}
        }
    }
    if truncated && !function_calls.is_empty() {
        // A call cut off mid-arguments must not run on a guess.
        return Err(LlmError::Parse("responses: incomplete during a function call".into()));
    }
    Ok(ParsedResponse {
        id: body.get("id").and_then(Value::as_str).map(str::to_string),
        text,
        function_calls,
        output: output.clone(),
        truncated,
        usage: super::usage::responses_usage(body),
        reasoning_tokens: super::usage::responses_reasoning_tokens(body),
    })
}

fn parse_function_call(item: &Value) -> Result<FunctionCall, LlmError> {
    let field = |k: &str, missing: &'static str| {
        item.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| LlmError::Parse(missing.into()))
    };
    let call_id = field("call_id", "function_call missing call_id")?;
    let name = field("name", "function_call missing name")?;
    let raw = field("arguments", "function_call missing arguments")?;
    let raw = if raw.trim().is_empty() { "{}" } else { raw.as_str() };
    let arguments = serde_json::from_str::<Value>(raw)
        .map_err(|_| LlmError::Parse("function_call arguments are not JSON".into()))?;
    Ok(FunctionCall { call_id, name, arguments })
}

/// Rebuilds one `response` object from its streamed events, so a streamed turn is parsed by the
/// same [`parse_response`] as a non-streaming one — the Responses counterpart of
/// `tool_turns::StreamedTurn`.
///
/// - `response.output_text.delta` is forwarded to the caller **until the first `function_call`
///   item begins**; text after that is history only (same rule as the Anthropic tool lane).
/// - `function_call` items are rebuilt from `output_item.added` + `function_call_arguments.delta`
///   fragments; `.done` / `output_item.done` replace them with the final values. Fragments that
///   never add up to JSON make [`parse_response`] refuse the turn — never an invented call.
/// - The terminal event's `response` is authoritative (id, status, usage, output). When it
///   carries no `output`, the rebuilt items stand in.
/// - An `error` event or `response.failed` is surfaced by [`Self::failure`].
#[derive(Debug, Default)]
pub struct ResponsesStream {
    response_id: Option<String>,
    items: BTreeMap<u64, Value>,
    arguments: BTreeMap<u64, String>,
    texts: BTreeMap<u64, String>,
    terminal: Option<Value>,
    /// `Some(code)` once an `error` event arrived.
    error_event: Option<Option<String>>,
    saw_tool_call: bool,
}

impl ResponsesStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// The response id, once the stream announced it.
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Whether a `function_call` item has begun (after which text is no longer forwarded).
    pub fn saw_tool_call(&self) -> bool {
        self.saw_tool_call
    }

    fn note_response(&mut self, response: Option<&Value>) {
        if let Some(id) = response.and_then(|r| r.get("id")).and_then(Value::as_str) {
            self.response_id = Some(id.to_string());
        }
    }

    /// Feed one SSE event. Returns the text the caller should paint now, if any.
    pub fn observe(&mut self, event: &Value) -> Option<String> {
        let kind = event.get("type").and_then(Value::as_str)?;
        let index = event.get("output_index").and_then(Value::as_u64);
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {
                self.note_response(event.get("response"));
                None
            }
            "response.output_item.added" | "response.output_item.done" => {
                let item = event.get("item")?.clone();
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    self.saw_tool_call = true;
                }
                self.items.insert(index?, item);
                None
            }
            "response.output_text.delta" => {
                let delta = event.get("delta")?.as_str()?;
                if delta.is_empty() {
                    return None;
                }
                self.texts.entry(index.unwrap_or(0)).or_default().push_str(delta);
                (!self.saw_tool_call).then(|| delta.to_string())
            }
            "response.function_call_arguments.delta" => {
                let delta = event.get("delta")?.as_str()?;
                self.arguments.entry(index?).or_default().push_str(delta);
                None
            }
            "response.function_call_arguments.done" => {
                let args = event.get("arguments")?.as_str()?;
                self.arguments.insert(index?, args.to_string());
                None
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.note_response(event.get("response"));
                self.terminal = event.get("response").cloned();
                None
            }
            "error" => {
                let code = event.get("code").and_then(Value::as_str).map(str::to_string);
                self.error_event = Some(code);
                None
            }
            _ => None,
        }
    }

    /// The failure the stream reported, if any (an `error` event, or `response.failed`).
    pub fn failure(&self) -> Option<LlmError> {
        if let Some(code) = &self.error_event {
            return Some(failure_error(code.as_deref()));
        }
        self.terminal.as_ref().and_then(response_failure)
    }

    /// The output items rebuilt from the item events, in `output_index` order.
    fn rebuilt_output(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let mut indices: Vec<u64> = self.items.keys().chain(self.texts.keys()).copied().collect();
        indices.sort_unstable();
        indices.dedup();
        for idx in indices {
            let mut item = self
                .items
                .get(&idx)
                .cloned()
                .unwrap_or_else(|| json!({ "type": "message", "role": "assistant", "content": [] }));
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    let has_args = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .is_some_and(|a| !a.is_empty());
                    if !has_args {
                        if let Some(args) = self.arguments.get(&idx) {
                            item["arguments"] = json!(args);
                        }
                    }
                }
                Some("message") => {
                    let empty = item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_none_or(|c| c.is_empty());
                    if empty {
                        if let Some(text) = self.texts.get(&idx) {
                            item["content"] = json!([{ "type": "output_text", "text": text }]);
                        }
                    }
                }
                _ => {}
            }
            out.push(item);
        }
        out
    }

    /// The response in the non-streaming shape, ready for [`parse_response`]. A stream that ended
    /// without a terminal event is reported `status: incomplete`, so a partial answer is marked
    /// truncated and a half-streamed function call is refused.
    pub fn into_body(self) -> Value {
        if let Some(code) = &self.error_event {
            return json!({
                "id": self.response_id,
                "status": "failed",
                "error": { "code": code },
                "output": [],
            });
        }
        let rebuilt = self.rebuilt_output();
        match self.terminal {
            Some(mut response) => {
                let has_output = response
                    .get("output")
                    .and_then(Value::as_array)
                    .is_some_and(|o| !o.is_empty());
                if !has_output {
                    response["output"] = json!(rebuilt);
                }
                response
            }
            None => json!({
                "id": self.response_id,
                "status": "incomplete",
                "incomplete_details": { "reason": "stream_ended" },
                "output": rebuilt,
            }),
        }
    }
}

/// How a streamed Responses turn ended: the usual [`StreamEnd`] plus the response id the next
/// turn can continue from (when the caller stores responses).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResponsesStreamEnd {
    pub end: StreamEnd,
    pub response_id: Option<String>,
}

/// Agent-lane client over the Responses endpoint. Constructed with a [`ByokKey`] (invariant 5).
/// Same shape as [`super::openai_compat::OpenAiCompatAgentClient`]: `complete*` needs
/// `T: HttpTransport`, the streaming methods `T: StreamingTransport`.
pub struct OpenAiResponsesAgentClient<T, S> {
    transport: T,
    sink: S,
    key: ResponsesCredential,
    cfg: OpenAiResponsesConfig,
    usage: Option<UsageHook>,
}

impl<T, S: TraceabilitySink> OpenAiResponsesAgentClient<T, S> {
    pub fn new(transport: T, sink: S, key: ByokKey, cfg: OpenAiResponsesConfig) -> Self {
        Self { transport, sink, key: ResponsesCredential::Byok(key), cfg, usage: None }
    }

    /// A license bearer can only reach the operator gateway, never a caller-selected API host.
    pub fn new_hosted(transport: T, sink: S, bearer: super::LicenseBearer, lane: HostedLane) -> Self {
        Self { transport, sink, key: ResponsesCredential::hosted(bearer, lane), cfg: lane.config(), usage: None }
    }

    /// Attach usage accounting (FR-HN-01 / #313).
    pub fn with_usage(mut self, hook: UsageHook) -> Self {
        self.usage = Some(hook);
        self
    }

    pub fn config(&self) -> &OpenAiResponsesConfig {
        &self.cfg
    }

    fn record_usage(
        &self,
        started: std::time::Instant,
        first_token_ms: Option<u64>,
        usage: Usage,
        outcome: UsageOutcome,
    ) {
        if let Some(hook) = &self.usage {
            hook.sink.record(UsageRecord::new(
                self.key.route(),
                hook.tag,
                &self.cfg.model,
                usage,
                first_token_ms,
                started.elapsed().as_millis() as u64,
                outcome,
            ));
        }
    }

    /// Traceability before the socket (invariant 3): digest + length of the untrusted content
    /// that is actually leaving on this request. `Err` when the ledger would not take the row;
    /// the caller then does not send (fail-closed).
    fn trace(&self, purpose: &str, traced: &str) -> Result<(), LlmError> {
        self.sink.try_record(TraceRecord::for_chunk(
            self.key.route(),
            self.key.purpose(purpose),
            self.key.destination(&self.cfg),
            traced,
            false,
        ))?;
        Ok(())
    }
}

impl<T: HttpTransport, S: TraceabilitySink> OpenAiResponsesAgentClient<T, S> {
    /// Send `prompt` as one user message and return the answer text.
    pub async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
        self.complete_once(None, prompt, "agent").await
    }

    /// Role-separated draft call (#123): `system` as `instructions`, the untrusted half as the
    /// user message. The trace digests the untrusted half.
    pub async fn complete_split(&self, system: &str, user: &str) -> Result<String, LlmError> {
        self.complete_once(Some(system), user, "agent").await
    }

    /// [`Self::complete_split`] recording the caller's product purpose in the egress ledger.
    pub async fn complete_split_with_purpose(
        &self,
        system: &str,
        user: &str,
        purpose: &str,
    ) -> Result<String, LlmError> {
        self.complete_once(Some(system), user, purpose).await
    }

    async fn complete_once(
        &self,
        system: Option<&str>,
        user: &str,
        purpose: &str,
    ) -> Result<String, LlmError> {
        let started = std::time::Instant::now();
        let turn = ResponsesTurn {
            instructions: system,
            input: vec![message_item("user", user)],
            ..ResponsesTurn::default()
        };
        let payload = build_responses_body(&self.cfg, &turn, false).to_string();
        let req = self.key.request(&self.cfg, payload)?;
        self.trace(purpose, user)?;
        let result = async {
            let resp = self.transport.send(req).await?;
            if !resp.is_success() {
                return Err(self.key.status_error("responses", resp.status, &resp.body));
            }
            let value = self.key.response(&resp.body)?;
            let parsed = parse_response(&value)?;
            if parsed.truncated {
                return Err(LlmError::Parse("responses: truncated at token limit".into()));
            }
            let in_bytes = system.map_or(0, str::len) + user.len();
            let usage = parsed
                .usage
                .unwrap_or_else(|| Usage::estimated_from_bytes(in_bytes, parsed.text.len()));
            Ok((parsed.text, usage))
        }
        .await;
        match result {
            Ok((text, usage)) => {
                self.record_usage(started, None, usage, UsageOutcome::Ok);
                Ok(text)
            }
            Err(e) => {
                self.record_usage(started, None, Usage::default(), UsageOutcome::from_error(&e));
                Err(e)
            }
        }
    }
}

impl<T: crate::llm::transport::StreamingTransport, S: TraceabilitySink> OpenAiResponsesAgentClient<T, S> {
    /// Stream `prompt`'s answer to `out` as it arrives (SLO-03).
    pub async fn complete_streaming(
        &self,
        prompt: &str,
        out: std::sync::mpsc::Sender<String>,
    ) -> Result<StreamEnd, LlmError> {
        let turn = ResponsesTurn { input: vec![message_item("user", prompt)], ..ResponsesTurn::default() };
        self.stream_turn(&turn, prompt, out).await.map(|e| e.end)
    }

    /// A role-structured chat turn. With `previous_response_id` **and** `store` enabled, only the
    /// new user message goes out and the provider continues from its stored response (the cached
    /// prefix survives, earlier turns are not re-sent). Otherwise the thread is replayed as real
    /// turns, as on chat/completions — a continuation id is never sent for an unstored response.
    ///
    /// `instructions` (the system half) are sent every turn: the API does not carry them over.
    pub async fn complete_streaming_conversation(
        &self,
        req: &crate::llm::conversation::ChatRequest,
        previous_response_id: Option<&str>,
        out: std::sync::mpsc::Sender<String>,
    ) -> Result<ResponsesStreamEnd, LlmError> {
        let continuation = previous_response_id.filter(|_| self.cfg.store);
        let system = req.system.trim();
        // Remembered facts ride as fenced user evidence ahead of the turn, as on chat/completions
        // (FR-HN-05 layer 2). They are re-sent on a continuation too: the context cache can move
        // between turns, and a stored response only holds the facts of the turn that made it.
        let stable = req.stable_context.trim();
        let facts = (!stable.is_empty())
            .then(|| message_item("user", &super::prompt_cache::untrusted_stable_context(stable)));
        let (input, traced) = match continuation {
            Some(_) => {
                let user = req.user_content();
                let mut traced = format!("{}{user}", req.stable_context);
                let mut item = message_item("user", &user);
                let images: Vec<_> = req.images.iter().filter(|image| image.index == req.turns.len()).collect();
                for image in &images { traced.push_str(&image.image.data_url); }
                if !images.is_empty() {
                    let mut content = vec![json!({"type":"input_text", "text":user})];
                    content.extend(images.iter().map(|image| image.image.responses_block()));
                    item["content"] = json!(content);
                }
                (facts.into_iter().chain([item]).collect(), traced)
            }
            None => {
                let input = facts
                    .into_iter()
                    .chain(req.normalized_turns().iter().map(|t| {
                        t.responses_item()
                    }))
                    .collect();
                (input, req.untrusted_text())
            }
        };
        let turn = ResponsesTurn {
            instructions: (!system.is_empty()).then_some(system),
            input,
            previous_response_id: continuation,
            ..ResponsesTurn::default()
        };
        self.stream_turn(&turn, &traced, out).await
    }

    /// Stream one text turn built by the caller — the seam for per-turn `reasoning_effort` and
    /// explicit continuation. `traced` is the untrusted content this request carries (digested
    /// into the ledger). Tools are stripped: a function call cannot be answered on this path,
    /// so tool turns go through the `tool_turns` submodule.
    ///
    /// One retry before the first delta for transient failures (the other clients' policy).
    pub async fn stream_turn(
        &self,
        turn: &ResponsesTurn<'_>,
        traced: &str,
        out: std::sync::mpsc::Sender<String>,
    ) -> Result<ResponsesStreamEnd, LlmError> {
        let turn = ResponsesTurn { tools: &[], ..turn.clone() };
        let payload = build_responses_body(&self.cfg, &turn, true).to_string();
        let started = std::time::Instant::now();
        let mut attempt = 0u8;
        loop {
            let req = self.key.request(&self.cfg, payload.clone())?;
            self.trace("agent", traced)?;

            let mut decoder = crate::llm::sse::SseDecoder::openai_responses();
            let mut stream = ResponsesStream::new();
            let mut first_token: Option<std::time::Instant> = None;
            let mut out_bytes = 0usize;
            let mut hung_up = false;
            let outcome = self
                .transport
                .send_streaming(req, |chunk| {
                    for event in decoder.push_events(chunk) {
                        let Some(delta) = stream.observe(&event) else { continue };
                        first_token.get_or_insert_with(std::time::Instant::now);
                        out_bytes += delta.len();
                        // The receiver is gone: the panel closed. End quietly.
                        if out.send(delta).is_err() {
                            hung_up = true;
                            return false;
                        }
                    }
                    true
                })
                .await;

            let first_token_ms = first_token.map(|t| t.duration_since(started).as_millis() as u64);
            let result = match outcome {
                Ok(crate::llm::transport::StreamOutcome::Streamed { .. }) => match stream.failure() {
                    Some(e) => Err(e),
                    // No terminal event: the stream was cut, so this is not the whole answer
                    // (#895). Our own hang-up is a normal end.
                    None if !hung_up && !decoder.finished() => {
                        Err(crate::llm::sse::closed_early("responses"))
                    }
                    None => Ok(ResponsesStreamEnd {
                        end: StreamEnd {
                            truncated: decoder.truncated(),
                            policy_stopped: decoder.policy_stopped(),
                        },
                        response_id: stream.response_id().map(str::to_string),
                    }),
                },
                Ok(crate::llm::transport::StreamOutcome::Failed { status, body }) => {
                    Err(self.key.status_error("responses", status, &body))
                }
                Err(e) => Err(e.into()),
            };
            if let Err(e) = &result {
                if !self.key.is_hosted() && attempt == 0
                    && first_token.is_none()
                    && crate::llm::conversation::retryable_before_first_token(e)
                {
                    attempt += 1;
                    tokio::time::sleep(crate::llm::conversation::STREAM_RETRY_BACKOFF).await;
                    continue;
                }
            }
            match &result {
                Ok(end) => {
                    let usage = decoder
                        .usage()
                        .unwrap_or_else(|| Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(traced), out_bytes));
                    let outcome =
                        if end.end.policy_stopped { UsageOutcome::Error } else { UsageOutcome::Ok };
                    self.record_usage(started, first_token_ms, usage, outcome);
                }
                Err(e) => self.record_usage(
                    started,
                    first_token_ms,
                    decoder.usage().unwrap_or_default(),
                    UsageOutcome::from_error(e),
                ),
            }
            return result;
        }
    }
}

// tool turns (issue #81 seam, Responses shape)

/// The Responses half of the tool-loop seam: a [`ModelTurnSource`] the loop drives exactly as it
/// drives `tool_turns::AnthropicToolTurnSource`, over the same body-in/body-out
/// [`MessagesHttp`] transport seam. Gated like `tool_turns` (the loop types live in
/// `shogun-mcp`).
///
/// [`ModelTurnSource`]: shogun_mcp::tool_loop::ModelTurnSource
/// [`MessagesHttp`]: crate::llm::tool_turns::MessagesHttp
#[cfg(any(feature = "net", feature = "db"))]
pub mod tool_turns {
    use serde_json::Value;
    use shogun_mcp::tool_loop::{ModelTurn, ModelTurnSource, ToolResult, ToolUse};

    use super::{
        build_responses_body, function_call_output_item, function_tools, message_item,
        parse_response, OpenAiResponsesConfig, ResponsesTurn, TOOL_ERROR_PREFIX,
    };
    use crate::llm::tool_turns::{system_with_connected_services, MessagesHttp, LENGTH_LIMIT_NOTE};

    /// A tool result as a `function_call_output` item. `is_error` has no field on this item, so
    /// a failure is marked in the output text ([`TOOL_ERROR_PREFIX`]).
    pub fn tool_result_item(r: &ToolResult) -> Value {
        let content = crate::llm::tool_turns::egress_tool_result_content(r);
        if r.is_error {
            function_call_output_item(&r.id, &format!("{TOOL_ERROR_PREFIX}{content}"))
        } else {
            function_call_output_item(&r.id, &content)
        }
    }

    /// What one response parses to for the loop.
    #[derive(Debug, Clone)]
    pub struct ParsedResponsesTurn {
        pub turn: ModelTurn,
        /// The id a stored next turn continues from.
        pub id: Option<String>,
        /// The output items verbatim, for a stateless next turn's replay.
        pub output: Vec<Value>,
    }

    /// Parse one Responses body into the loop's [`ModelTurn`]: any `function_call` items become
    /// [`ModelTurn::ToolUses`] in output order (`call_id` is the id the result must carry back);
    /// otherwise [`ModelTurn::Final`] with the text. Refusals and policy stops are
    /// `"provider_policy_stop"`, as on the Anthropic tool lane. `Err` never echoes body text.
    ///
    /// A final answer follows the Anthropic tool lane's rules (`tool_turns::parse_tool_turn`):
    /// an empty answer is an error, not a blank reply; an answer cut at the output-token limit
    /// carries [`LENGTH_LIMIT_NOTE`]; an answer whose stream ended without the provider's terminal
    /// event is refused rather than presented as finished.
    pub fn parse_responses_tool_turn(body: &Value) -> Result<ParsedResponsesTurn, String> {
        let parsed = parse_response(body).map_err(|e| e.to_string())?;
        let turn = if parsed.function_calls.is_empty() {
            let mut text = parsed.text;
            if text.trim().is_empty() {
                return Err("The model returned no answer.".into());
            }
            if parsed.truncated {
                let reason = body
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str);
                if reason != Some("max_output_tokens") {
                    return Err("the answer ended before the provider finished it".into());
                }
                text.push_str(LENGTH_LIMIT_NOTE);
            }
            ModelTurn::Final(text)
        } else {
            ModelTurn::ToolUses(
                parsed
                    .function_calls
                    .into_iter()
                    .map(|c| ToolUse { id: c.call_id, name: c.name, input: c.arguments })
                    .collect(),
            )
        };
        Ok(ParsedResponsesTurn { turn, id: parsed.id, output: parsed.output })
    }

    /// The Agent-lane [`ModelTurnSource`] over the Responses API.
    ///
    /// Two continuation modes, chosen by [`OpenAiResponsesConfig::store`]:
    /// - **stored** — each turn after the first sends only its `function_call_output` items plus
    ///   `previous_response_id`; the provider holds the rest, and the cached prefix survives a
    ///   change of `reasoning.effort` between turns;
    /// - **stateless** (`store: false`, the default) — each turn replays the whole item list: the
    ///   opening message, every earlier output item verbatim, every result.
    ///
    /// `instructions` and `tools` are sent on every turn (neither is carried over) and are fixed
    /// for the conversation (spec §4-1). State advances only after a reply parses, so a failed
    /// turn leaves nothing half-applied for a retry to build on.
    ///
    /// Tool results go back exactly as the loop produced them — already fenced as untrusted data
    /// by `shogun_mcp::tool_loop`, refusals marked with [`TOOL_ERROR_PREFIX`] — and a mid-run
    /// user note (issue #395) rides as a user message after them, never as instructions.
    pub struct ResponsesToolTurnSource<H: MessagesHttp> {
        http: H,
        cfg: OpenAiResponsesConfig,
        instructions: String,
        tools: Vec<Value>,
        /// The opening input items (the question, or the chat thread), until the first turn
        /// succeeds.
        opening: Vec<Value>,
        previous_response_id: Option<String>,
        /// Stateless mode: every item sent or received so far.
        history: Vec<Value>,
        reasoning_effort: Option<String>,
        /// Mid-run user notes (issue #395) waiting for the next request.
        pending_note: Option<String>,
    }

    impl<H: MessagesHttp> ResponsesToolTurnSource<H> {
        /// `catalog_tools` is `tool_definitions` output (Messages shape); it is mapped to
        /// function tools here.
        pub fn new(
            http: H,
            cfg: OpenAiResponsesConfig,
            instructions: impl Into<String>,
            catalog_tools: &[Value],
            user_prompt: &str,
        ) -> Self {
            Self {
                http,
                cfg,
                instructions: instructions.into(),
                tools: function_tools(catalog_tools),
                opening: vec![message_item("user", user_prompt)],
                previous_response_id: None,
                history: Vec::new(),
                reasoning_effort: None,
                pending_note: None,
            }
        }

        /// The chat path's source (issue #390): the same inputs the Anthropic tool lane's
        /// `AnthropicToolTurnSource::from_chat` takes. `instructions` are the chat system prompt
        /// plus the Connected-services block; the opening input is the remembered facts (fenced
        /// as untrusted user evidence, FR-HN-05 layer 2, never instructions) followed by the
        /// normalized thread and this turn — the same shape the tool-free Responses chat sends.
        pub fn from_chat(
            http: H,
            cfg: OpenAiResponsesConfig,
            request: &crate::llm::conversation::ChatRequest,
            connected_block: Option<&str>,
            catalog_tools: &[Value],
        ) -> Self {
            let stable = request.stable_context.trim();
            let facts = (!stable.is_empty()).then(|| {
                message_item("user", &crate::llm::prompt_cache::untrusted_stable_context(stable))
            });
            let opening = facts
                .into_iter()
                .chain(request.normalized_turns().iter().map(|t| {
                    t.responses_item()
                }))
                .collect();
            Self {
                http,
                cfg,
                instructions: system_with_connected_services(&request.system, connected_block),
                tools: function_tools(catalog_tools),
                opening,
                previous_response_id: None,
                history: Vec::new(),
                reasoning_effort: None,
                pending_note: None,
            }
        }

        /// Continue a stored thread (a chat thread's last response). Used only in stored mode:
        /// an unstored response cannot be continued from.
        pub fn with_previous_response_id(mut self, id: impl Into<String>) -> Self {
            if self.cfg.store {
                self.previous_response_id = Some(id.into());
            }
            self
        }

        /// Change `reasoning.effort` from the next turn on. The continuation id is kept.
        pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
            self.reasoning_effort = effort;
        }

        /// The id of the last successful response (what a chat thread stores to continue).
        pub fn last_response_id(&self) -> Option<&str> {
            self.previous_response_id.as_deref()
        }

        pub fn http(&self) -> &H {
            &self.http
        }
    }

    impl<H: MessagesHttp> ModelTurnSource for ResponsesToolTurnSource<H> {
        fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
            let mut fresh: Vec<Value> = self.opening.clone();
            fresh.extend(results.iter().map(tool_result_item));
            // A note rides after this turn's results as the user's own message. It is consumed
            // only once the reply parses, so a failed turn resends it.
            if let Some(note) = &self.pending_note {
                fresh.push(message_item("user", note));
            }
            let (input, previous) = if self.cfg.store {
                (fresh.clone(), self.previous_response_id.as_deref())
            } else {
                let mut all = self.history.clone();
                all.extend(fresh.iter().cloned());
                (all, None)
            };
            let body = build_responses_body(
                &self.cfg,
                &ResponsesTurn {
                    instructions: Some(&self.instructions),
                    input,
                    tools: &self.tools,
                    previous_response_id: previous,
                    reasoning_effort: self.reasoning_effort.as_deref(),
                },
                false,
            );
            let resp = self.http.post(&body)?;
            let parsed = parse_responses_tool_turn(&resp)?;
            if self.cfg.store {
                let id = parsed
                    .id
                    .ok_or_else(|| "response carried no id to continue from".to_string())?;
                self.previous_response_id = Some(id);
            } else {
                self.history.extend(fresh);
                self.history.extend(parsed.output);
            }
            self.opening.clear();
            self.pending_note = None;
            Ok(parsed.turn)
        }

        fn accepts_user_notes(&self) -> bool {
            true
        }

        fn push_user_note(&mut self, note: String) {
            match &mut self.pending_note {
                Some(waiting) => {
                    waiting.push_str("\n\n");
                    waiting.push_str(&note);
                }
                None => self.pending_note = Some(note),
            }
        }
    }

    /// [`MessagesHttp`] over a real transport for the Responses endpoint, carrying the BYOK key —
    /// the counterpart of `tool_turns::ToolTurnHttp`, with the same guarantees: the key is a
    /// [`ByokKey`](crate::llm::ByokKey) (invariant 5), the row is traced before the socket, one
    /// usage row per turn, a per-turn deadline, and `Err` strings that carry no body text.
    #[cfg(feature = "net")]
    pub struct ResponsesToolTurnHttp<T, S> {
        pub(super) transport: T,
        pub(super) sink: S,
        key: super::ResponsesCredential,
        cfg: OpenAiResponsesConfig,
        turn_timeout: std::time::Duration,
        usage: Option<crate::llm::usage::UsageHook>,
        /// Chat tool lane (issue #390): streamed answer text as provisional
        /// [`ToolTextEvent`](crate::llm::tool_turns::ToolTextEvent)s — the same events the
        /// Anthropic tool lane's `ToolTurnHttp::with_text_out` sends. Function-call arguments
        /// never go here.
        text_out: Option<std::sync::mpsc::Sender<crate::llm::tool_turns::ToolTextEvent>>,
        model_turn: std::sync::atomic::AtomicU32,
        cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    #[cfg(feature = "net")]
    type TurnErr = (String, crate::llm::usage::UsageOutcome);

    #[cfg(feature = "net")]
    impl<T, S: crate::llm::traceability::TraceabilitySink> ResponsesToolTurnHttp<T, S> {
        pub fn new(
            transport: T,
            sink: S,
            key: crate::llm::ByokKey,
            cfg: OpenAiResponsesConfig,
        ) -> Self {
            Self {
                transport,
                sink,
                key: super::ResponsesCredential::Byok(key),
                cfg,
                turn_timeout: std::time::Duration::from_millis(
                    crate::llm::tool_turns::ToolTurnHttp::<T, S>::DEFAULT_TURN_TIMEOUT_MS,
                ),
                usage: None,
                text_out: None,
                model_turn: std::sync::atomic::AtomicU32::new(0),
                cancelled: None,
            }
        }

        /// Hosted chat uses the existing local tool gate, with stateless gateway requests.
        pub fn new_hosted(transport: T, sink: S, bearer: crate::llm::LicenseBearer) -> Self {
            Self {
                transport, sink,
                key: super::ResponsesCredential::hosted(bearer, super::HostedLane::Chat),
                cfg: super::HostedLane::Chat.config(),
                turn_timeout: std::time::Duration::from_secs(260), usage: None, text_out: None,
                model_turn: std::sync::atomic::AtomicU32::new(0), cancelled: None,
            }
        }

        /// Paint streamed answer text as [`ToolTextEvent`](crate::llm::tool_turns::ToolTextEvent)s
        /// (used by [`Self::post_streaming_events`]).
        pub fn with_text_out(
            mut self,
            tx: std::sync::mpsc::Sender<crate::llm::tool_turns::ToolTextEvent>,
        ) -> Self {
            self.text_out = Some(tx);
            self
        }

        /// Abandon an in-flight turn as soon as `cancelled` is set (the user pressed Stop).
        pub fn with_cancellation(
            mut self,
            cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> Self {
            self.cancelled = Some(cancelled);
            self
        }

        async fn wait_for_cancel(&self) {
            let Some(cancelled) = &self.cancelled else {
                return std::future::pending().await;
            };
            while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        pub fn with_turn_timeout(mut self, timeout: std::time::Duration) -> Self {
            self.turn_timeout = timeout;
            self
        }

        pub fn with_usage(mut self, hook: crate::llm::usage::UsageHook) -> Self {
            self.usage = Some(hook);
            self
        }

        fn record_usage(
            &self,
            started: std::time::Instant,
            first_token_ms: Option<u64>,
            usage: crate::llm::usage::Usage,
            outcome: crate::llm::usage::UsageOutcome,
        ) {
            if let Some(hook) = &self.usage {
                hook.sink.record(crate::llm::usage::UsageRecord::new(
                    self.key.route(),
                    hook.tag,
                    &self.cfg.model,
                    usage,
                    first_token_ms,
                    started.elapsed().as_millis() as u64,
                    outcome,
                ));
            }
        }

        fn request(&self, payload: &str) -> Result<crate::llm::transport::HttpRequest, TurnErr> {
            self.key.request(&self.cfg, payload.to_string())
                .map_err(|e| (format!("request build failed: {e}"), crate::llm::usage::UsageOutcome::Error))
        }

        /// Traceability before the socket. `Err` when the ledger would not take the row; the
        /// caller then does not send (fail-closed).
        fn trace(&self, payload: &str) -> Result<(), crate::llm::LlmError> {
            self.sink.try_record(crate::llm::traceability::TraceRecord::for_chunk(
                self.key.route(),
                self.key.purpose("agent_tools"),
                self.key.destination(&self.cfg),
                payload,
                false,
            ))?;
            Ok(())
        }

        fn failed(e: &crate::llm::LlmError) -> TurnErr {
            (e.to_string(), crate::llm::usage::UsageOutcome::from_error(e))
        }
    }

    #[cfg(feature = "net")]
    impl<T: crate::llm::transport::HttpTransport, S: crate::llm::traceability::TraceabilitySink>
        ResponsesToolTurnHttp<T, S>
    {
        /// One model turn, non-streaming. Records egress, sends, returns the `response` body.
        pub async fn post_async(&self, body: &Value) -> Result<Value, String> {
            use crate::llm::usage::{Usage, UsageOutcome};
            let started = std::time::Instant::now();
            match tokio::time::timeout(self.turn_timeout, self.send_turn(body)).await {
                Ok(Ok((value, usage))) => {
                    self.record_usage(started, None, usage, UsageOutcome::Ok);
                    Ok(value)
                }
                Ok(Err((reason, outcome))) => {
                    self.record_usage(started, None, Usage::default(), outcome);
                    Err(reason)
                }
                Err(_) => {
                    self.record_usage(started, None, Usage::default(), UsageOutcome::Error);
                    Err("the model did not answer in time".to_string())
                }
            }
        }

        async fn send_turn(&self, body: &Value) -> Result<(Value, crate::llm::usage::Usage), TurnErr> {
            use crate::llm::usage::{Usage, UsageOutcome};
            let payload = body.to_string();
            let req = self.request(&payload)?;
            self.trace(&payload).map_err(|e| Self::failed(&e))?;
            let resp = self
                .transport
                .send(req)
                .await
                .map_err(|e| (format!("transport error: {e}"), UsageOutcome::Error))?;
            if !resp.is_success() {
                return Err(Self::failed(&self.key.status_error("tool turn", resp.status, &resp.body)));
            }
            let value = self.key.response(&resp.body).map_err(|e| Self::failed(&e))?;
            if let Some(e) = super::response_failure(&value) {
                return Err(Self::failed(&e));
            }
            let usage = crate::llm::usage::responses_usage(&value)
                .unwrap_or_else(|| Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(&payload), resp.body.len()));
            Ok((value, usage))
        }

        pub fn post_blocking(&self, rt: &tokio::runtime::Runtime, body: &Value) -> Result<Value, String> {
            rt.block_on(self.post_async(body))
        }
    }

    #[cfg(feature = "net")]
    impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink>
        ResponsesToolTurnHttp<T, S>
    {
        /// One model turn with `stream: true`: text deltas go to `out` until a function call
        /// begins (see [`super::ResponsesStream`]), and the body is rebuilt into the same shape
        /// [`Self::post_async`] returns. `out` closing abandons the turn with an `Err`.
        pub async fn post_streaming_async(
            &self,
            body: &Value,
            out: &std::sync::mpsc::Sender<String>,
        ) -> Result<(Value, crate::llm::tool_turns::TurnStreamStats), String> {
            use crate::llm::usage::{Usage, UsageOutcome};
            let started = std::time::Instant::now();
            let streamed = tokio::time::timeout(self.turn_timeout, self.stream_turn(body, out, started));
            match streamed.await {
                Ok(Ok((value, usage, stats))) => {
                    self.record_usage(started, stats.first_token_ms, usage, UsageOutcome::Ok);
                    Ok((value, stats))
                }
                Ok(Err(((reason, outcome), stats))) => {
                    self.record_usage(started, stats.first_token_ms, Usage::default(), outcome);
                    Err(reason)
                }
                Err(_) => {
                    self.record_usage(started, None, Usage::default(), UsageOutcome::Error);
                    Err("the model did not answer in time".to_string())
                }
            }
        }

        #[allow(clippy::type_complexity)] // one private seam; the tuple is read once, right above
        async fn stream_turn(
            &self,
            body: &Value,
            out: &std::sync::mpsc::Sender<String>,
            started: std::time::Instant,
        ) -> Result<
            (Value, crate::llm::usage::Usage, crate::llm::tool_turns::TurnStreamStats),
            (TurnErr, crate::llm::tool_turns::TurnStreamStats),
        > {
            use crate::llm::tool_turns::TurnStreamStats;
            use crate::llm::transport::StreamOutcome;
            use crate::llm::usage::{Usage, UsageOutcome};

            let mut streamed = body.clone();
            streamed["stream"] = serde_json::json!(true);
            let payload = streamed.to_string();
            let req = self.request(&payload).map_err(|e| (e, TurnStreamStats::default()))?;
            self.trace(&payload).map_err(|e| (Self::failed(&e), TurnStreamStats::default()))?;

            let mut decoder = crate::llm::sse::SseDecoder::openai_responses();
            let mut stream = super::ResponsesStream::new();
            let mut first_token: Option<std::time::Instant> = None;
            let mut out_bytes = 0usize;
            let mut abandoned = false;
            let outcome = self
                .transport
                .send_streaming(req, |chunk| {
                    for event in decoder.push_events(chunk) {
                        let Some(text) = stream.observe(&event) else { continue };
                        first_token.get_or_insert_with(std::time::Instant::now);
                        out_bytes += text.len();
                        if out.send(text).is_err() {
                            abandoned = true;
                            return false;
                        }
                    }
                    true
                })
                .await;
            let stats = TurnStreamStats {
                first_token_ms: first_token.map(|t| t.duration_since(started).as_millis() as u64),
            };
            match outcome {
                Ok(StreamOutcome::Streamed { .. }) if abandoned => {
                    Err((("the answer was abandoned".to_string(), UsageOutcome::Error), stats))
                }
                Ok(StreamOutcome::Streamed { .. }) => {
                    if let Some(e) = stream.failure() {
                        return Err((Self::failed(&e), stats));
                    }
                    let usage = decoder
                        .usage()
                        .unwrap_or_else(|| Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(&payload), out_bytes));
                    Ok((stream.into_body(), usage, stats))
                }
                Ok(StreamOutcome::Failed { status, body }) => {
                    Err((Self::failed(&self.key.status_error("tool turn", status, &body)), stats))
                }
                Err(e) => Err(((format!("transport error: {e}"), UsageOutcome::Error), stats)),
            }
        }

        pub fn post_streaming_blocking(
            &self,
            rt: &tokio::runtime::Runtime,
            body: &Value,
            out: &std::sync::mpsc::Sender<String>,
        ) -> Result<(Value, crate::llm::tool_turns::TurnStreamStats), String> {
            rt.block_on(self.post_streaming_async(body, out))
        }
    }

    #[cfg(feature = "net")]
    impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink>
        ResponsesToolTurnHttp<T, S>
    {
        /// One chat tool-lane turn (issue #390), the Responses twin of the Anthropic lane's
        /// `ToolTurnHttp::post_streaming`, with the same guarantees:
        /// - answer text streams to [`Self::with_text_out`] as `Delta`s while it arrives (SLO-03:
        ///   the first visible character does not wait for the turn), then one `Commit` when the
        ///   turn is a final answer or one `Discard` when it turns into function calls or fails;
        /// - [`Self::with_cancellation`] ends the turn at once with `Err("cancelled")`;
        /// - one retry, only before any text was painted, for a transient failure;
        /// - every HTTP attempt is traced before the socket and writes one usage row;
        /// - `Err` is a short reason that never carries body text or the key.
        pub async fn post_streaming_events(&self, body: &Value) -> Result<Value, String> {
            use crate::llm::tool_turns::ToolTextEvent;
            use crate::llm::usage::{Usage, UsageOutcome};
            let started = std::time::Instant::now();
            let model_turn = self.model_turn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let result = tokio::select! {
                biased;
                () = self.wait_for_cancel() => Err("cancelled".to_string()),
                result = tokio::time::timeout(self.turn_timeout, self.send_events(body, model_turn)) => {
                    match result {
                        Ok(result) => result,
                        Err(_) => {
                            self.record_usage(started, None, Usage::default(), UsageOutcome::Error);
                            Err("the model did not answer in time".to_string())
                        }
                    }
                }
            };
            let parsed = result.and_then(|body| {
                let turn = super::tool_turns::parse_responses_tool_turn(&body)?;
                Ok((body, matches!(turn.turn, ModelTurn::Final(_))))
            });
            if let Some(tx) = &self.text_out {
                let event = if matches!(parsed, Ok((_, true))) {
                    ToolTextEvent::Commit { model_turn }
                } else {
                    ToolTextEvent::Discard { model_turn }
                };
                let _ = tx.send(event);
            }
            parsed.map(|(body, _)| body)
        }

        async fn send_events(&self, body: &Value, model_turn: u32) -> Result<Value, String> {
            use crate::llm::conversation::{retryable_before_first_token, STREAM_RETRY_BACKOFF};
            let mut saw_text = false;
            let mut result = self.send_events_attempt(body, model_turn, &mut saw_text).await;
            if !self.key.is_hosted() && !saw_text && result.as_ref().is_err_and(retryable_before_first_token) {
                // Retry only this model request: the loop has received and run nothing from it.
                tokio::time::sleep(STREAM_RETRY_BACKOFF).await;
                result = self.send_events_attempt(body, model_turn, &mut saw_text).await;
            }
            result.map_err(|e| match e {
                crate::llm::LlmError::Parse(_) => "malformed response body".to_string(),
                other => other.to_string(),
            })
        }

        async fn send_events_attempt(
            &self,
            body: &Value,
            model_turn: u32,
            saw_text: &mut bool,
        ) -> Result<Value, crate::llm::LlmError> {
            use crate::llm::tool_turns::ToolTextEvent;
            use crate::llm::transport::StreamOutcome;
            use crate::llm::usage::{Usage, UsageOutcome};
            use crate::llm::LlmError;

            let started = std::time::Instant::now();
            let mut wire = body.clone();
            wire["stream"] = serde_json::json!(true);
            let payload = wire.to_string();
            let req = self.key.request(&self.cfg, payload.clone())?;
            self.trace(&payload)?;

            let mut decoder = crate::llm::sse::SseDecoder::openai_responses();
            let mut stream = super::ResponsesStream::new();
            let text_out = self.text_out.clone();
            let mut first_token: Option<std::time::Instant> = None;
            let mut out_bytes = 0usize;
            let mut abandoned = false;
            let outcome = self
                .transport
                .send_streaming(req, |chunk| {
                    for event in decoder.push_events(chunk) {
                        let Some(text) = stream.observe(&event) else { continue };
                        *saw_text = true;
                        first_token.get_or_insert_with(std::time::Instant::now);
                        out_bytes += text.len();
                        if let Some(tx) = &text_out {
                            if tx.send(ToolTextEvent::Delta { model_turn, text }).is_err() {
                                abandoned = true;
                                return false;
                            }
                        }
                    }
                    true
                })
                .await;
            let first_token_ms = first_token.map(|t| t.duration_since(started).as_millis() as u64);

            let result = match outcome {
                Ok(StreamOutcome::Streamed { .. }) if abandoned => {
                    Err(LlmError::Provider("the answer was abandoned".to_string()))
                }
                Ok(StreamOutcome::Streamed { .. }) => match stream.failure() {
                    Some(e) => Err(e),
                    None if !decoder.finished() => Err(crate::llm::sse::closed_early("responses")),
                    None => Ok(stream.into_body()),
                },
                Ok(StreamOutcome::Failed { status, body }) => {
                    Err(self.key.status_error("tool turn", status, &body))
                }
                Err(e) => Err(e.into()),
            };
            match &result {
                Ok(_) => {
                    let usage = decoder
                        .usage()
                        .unwrap_or_else(|| Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(&payload), out_bytes));
                    self.record_usage(started, first_token_ms, usage, UsageOutcome::Ok);
                }
                Err(e) => {
                    self.record_usage(started, first_token_ms, Usage::default(), UsageOutcome::from_error(e));
                }
            }
            result
        }

        pub fn post_streaming_events_blocking(
            &self,
            rt: &tokio::runtime::Runtime,
            body: &Value,
        ) -> Result<Value, String> {
            rt.block_on(self.post_streaming_events(body))
        }
    }

    /// The chat tool lane's transport (issue #390): [`ResponsesToolTurnHttp`] streaming
    /// [`ToolTextEvent`](crate::llm::tool_turns::ToolTextEvent)s through
    /// [`ResponsesToolTurnHttp::post_streaming_events`], bound to the shell's runtime — the
    /// Responses counterpart of `tool_turns::RuntimeBoundToolTurnHttp`, which the desktop's
    /// Anthropic tool lane uses.
    #[cfg(feature = "net")]
    pub struct RuntimeBoundResponsesChatTurnHttp<'a, T, S> {
        inner: ResponsesToolTurnHttp<T, S>,
        rt: &'a tokio::runtime::Runtime,
    }

    #[cfg(feature = "net")]
    impl<'a, T, S> RuntimeBoundResponsesChatTurnHttp<'a, T, S> {
        pub fn new(inner: ResponsesToolTurnHttp<T, S>, rt: &'a tokio::runtime::Runtime) -> Self {
            Self { inner, rt }
        }

        pub fn inner(&self) -> &ResponsesToolTurnHttp<T, S> {
            &self.inner
        }
    }

    #[cfg(feature = "net")]
    impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink>
        MessagesHttp for RuntimeBoundResponsesChatTurnHttp<'_, T, S>
    {
        fn post(&mut self, body: &Value) -> Result<Value, String> {
            self.inner.post_streaming_events_blocking(self.rt, body)
        }
    }

    /// [`ResponsesToolTurnHttp`] bound to a runtime, so it satisfies the loop's synchronous seam
    /// (same reasoning as `tool_turns::RuntimeBoundToolTurnHttp`).
    #[cfg(feature = "net")]
    pub struct RuntimeBoundResponsesToolTurnHttp<'a, T, S> {
        inner: ResponsesToolTurnHttp<T, S>,
        rt: &'a tokio::runtime::Runtime,
    }

    #[cfg(feature = "net")]
    impl<'a, T, S> RuntimeBoundResponsesToolTurnHttp<'a, T, S> {
        pub fn new(inner: ResponsesToolTurnHttp<T, S>, rt: &'a tokio::runtime::Runtime) -> Self {
            Self { inner, rt }
        }
    }

    #[cfg(feature = "net")]
    impl<T: crate::llm::transport::HttpTransport, S: crate::llm::traceability::TraceabilitySink>
        MessagesHttp for RuntimeBoundResponsesToolTurnHttp<'_, T, S>
    {
        fn post(&mut self, body: &Value) -> Result<Value, String> {
            self.inner.post_blocking(self.rt, body)
        }
    }

    /// The streaming twin of [`RuntimeBoundResponsesToolTurnHttp`] (issue #485 shape).
    #[cfg(feature = "net")]
    pub struct RuntimeBoundStreamingResponsesToolTurnHttp<'a, T, S> {
        inner: ResponsesToolTurnHttp<T, S>,
        rt: &'a tokio::runtime::Runtime,
        out: std::sync::mpsc::Sender<String>,
        last: crate::llm::tool_turns::TurnStreamStats,
    }

    #[cfg(feature = "net")]
    impl<'a, T, S> RuntimeBoundStreamingResponsesToolTurnHttp<'a, T, S> {
        pub fn new(
            inner: ResponsesToolTurnHttp<T, S>,
            rt: &'a tokio::runtime::Runtime,
            out: std::sync::mpsc::Sender<String>,
        ) -> Self {
            Self { inner, rt, out, last: Default::default() }
        }

        pub fn last_stats(&self) -> crate::llm::tool_turns::TurnStreamStats {
            self.last
        }
    }

    #[cfg(feature = "net")]
    impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink>
        MessagesHttp for RuntimeBoundStreamingResponsesToolTurnHttp<'_, T, S>
    {
        fn post(&mut self, body: &Value) -> Result<Value, String> {
            match self.inner.post_streaming_blocking(self.rt, body, &self.out) {
                Ok((value, stats)) => {
                    self.last = stats;
                    Ok(value)
                }
                Err(e) => {
                    self.last = Default::default();
                    Err(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
