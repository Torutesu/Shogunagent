//! Direct Gmail REST transport. OAuth credentials are supplied by the Keychain token provider.
//! Internal tool aliases keep sync, chat, triage and confirmed writes on the same transport.

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{json, Value};
use shogun_integrations::rpc::{McpRpc, TokenProvider};
use shogun_mcp::scope::Service;

use crate::gmail_shape::{envelope, record_from_thread};
use crate::llm::traceability::{RoundTrip, Route, TraceRecord, TraceabilitySink};

const API_ROOT: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const MAX_RESPONSE: u64 = 2 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
#[derive(Clone, Copy)]
struct RequestContext {
    deadline: Instant,
    authorization_revision: u64,
}

/// A transient refresh fault (Wi-Fi drop, token endpoint down) leaves the stored grant valid, so
/// it must not read as a credential failure: the runtime turns Gmail amber ("Needs reauth") only
/// for "credentials are unavailable", and this message deliberately matches none of its markers.
pub const GMAIL_REFRESH_UNAVAILABLE: &str = "Gmail sign-in couldn't be refreshed; try again later.";

/// Content-free copy for a token-provider failure: the provider's reason never leaves here.
fn token_failure_message(reason: &str) -> String {
    if reason == "keychain_locked" {
        "Keychain locked".into()
    } else if shogun_integrations::token::is_transient_token_failure(reason) {
        GMAIL_REFRESH_UNAVAILABLE.into()
    } else {
        "Gmail credentials are unavailable; reconnect Gmail.".into()
    }
}

/// Explicit read, draft, label and send scopes; Gmail deletion remains unavailable.
pub const GMAIL_SCOPES: &[&str] = shogun_integrations::hosted_connect::GMAIL_SCOPES;

/// Gmail uses REST directly; it does not require Google's MCP developer preview.
pub struct GmailDirectRpc<P> {
    client: reqwest::blocking::Client,
    tokens: P,
    sink: Arc<dyn TraceabilitySink>,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    send_allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    base_url: String,
    auth_revision: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl<P: TokenProvider> GmailDirectRpc<P> {
    pub fn new(
        tokens: P,
        sink: Arc<dyn TraceabilitySink>,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("shogun/1.0")
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(DEADLINE)
            .build()
            .map_err(|_| "Gmail client is unavailable.")?;
        Ok(Self {
            client,
            tokens,
            sink,
            allowed,
            send_allowed: Arc::new(|| true),
            base_url: API_ROOT.into(),
            auth_revision: Arc::new(|| 0),
        })
    }

    /// Re-check the live human-controlled draft-only setting and current send entitlement.
    pub fn with_send_allowed(mut self, probe: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.send_allowed = probe;
        self
    }

    /// Cancel an entire logical call if credentials were disconnected or replaced between
    /// requests, even after the durable disconnect marker has already been removed.
    pub fn with_auth_revision(mut self, revision: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.auth_revision = revision;
        self
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        context: RequestContext,
    ) -> Result<Value, String> {
        if (self.auth_revision)() != context.authorization_revision {
            return Err("Gmail connection changed; retry the operation.".into());
        }
        if !(self.allowed)() {
            return Err("Gmail disconnect is pending.".into());
        }
        if path == "/messages/send" && !(self.send_allowed)() {
            return Err("Gmail sending is disabled; save a draft instead.".into());
        }
        let token = self
            .tokens
            .access_token(Service::Gmail)
            .map_err(|reason| token_failure_message(&reason))?;
        if (self.auth_revision)() != context.authorization_revision {
            return Err("Gmail connection changed; retry the operation.".into());
        }
        if !(self.allowed)() {
            return Err("Gmail disconnect is pending.".into());
        }
        let remaining = context
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or("Gmail request deadline expired before send.")?;
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(token)
            .query(query)
            .timeout(remaining);
        if let Some(value) = body {
            request = request.json(&value);
        }
        self.sink
            .try_record(TraceRecord::for_chunk(
                Route::Mcp,
                "gmail_provider_request",
                "gmail",
                "",
                false,
            ))
            .map_err(|_| "Gmail request could not be recorded.")?;
        // Re-check immediately before egress, including after token refresh and trace persistence.
        if (self.auth_revision)() != context.authorization_revision {
            return Err("Gmail connection changed; retry the operation.".into());
        }
        if !(self.allowed)() {
            return Err("Gmail disconnect is pending.".into());
        }
        if path == "/messages/send" && !(self.send_allowed)() {
            return Err("Gmail sending is disabled; save a draft instead.".into());
        }
        // Persisting a trace can block on storage. It consumes this same operation deadline.
        let request = request.timeout(context.deadline.checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero()).ok_or("Gmail request deadline expired before send.")?);
        // Mark ambiguity only once the actual send can have reached Gmail.
        let uncertain = |reason: &str| -> String {
            if path == "/messages/send" {
                format!("delivery_uncertain: {reason}")
            } else {
                reason.into()
            }
        };
        // No automatic retries: a failed send may already have been delivered.
        let started = Instant::now();
        let result = request.send();
        self.sink.round_trip(RoundTrip {
            route: Route::Mcp,
            ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            status: result
                .as_ref()
                .map_or(0, |response| response.status().as_u16()),
        });
        let response = result.map_err(|error| {
            if error.is_connect() || error.is_builder() {
                "Gmail connection failed before send.".into()
            } else if error.is_timeout() {
                uncertain("Gmail request timed out.")
            } else {
                uncertain("Gmail request failed.")
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 403 {
                use crate::mcp_http::Forbidden;
                return Err(match crate::mcp_http::forbidden_kind(response) {
                    Forbidden::Auth => "Gmail token expired; reconnect Gmail.",
                    Forbidden::RateLimit => "Gmail rate limit reached; try again later.",
                    Forbidden::Permission => {
                        "Gmail permission was denied; check scopes and reconnect."
                    }
                }
                .into());
            }
            return Err(match status.as_u16() {
                401 => "Gmail token expired; reconnect Gmail.".into(),
                429 => "Gmail rate limit reached; try again later.".into(),
                code if status.is_server_error() => {
                    uncertain(&format!("Gmail request failed (HTTP {code})."))
                }
                code => format!("Gmail request refused (HTTP {code})."),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE)
        {
            return Err(uncertain("Gmail response exceeded the limit."));
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_RESPONSE + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| uncertain("Gmail response could not be read."))?;
        if bytes.len() as u64 > MAX_RESPONSE {
            return Err(uncertain("Gmail response exceeded the limit."));
        }
        if (self.auth_revision)() != context.authorization_revision {
            return Err(uncertain("Gmail connection changed; retry the operation."));
        }
        if !(self.allowed)() {
            return Err(uncertain("Gmail disconnect is pending."));
        }
        serde_json::from_slice(&bytes).map_err(|_| uncertain("Gmail response was invalid."))
    }

    fn thread(&self, id: &str, context: RequestContext) -> Result<Value, String> {
        validate_id(id)?;
        self.request(
            Method::GET,
            &format!("/threads/{id}"),
            &[("format", "full".into())],
            None,
            context,
        )
    }

    fn search(&self, arguments: &Value, context: RequestContext) -> Result<Value, String> {
        let count = match arguments.get("max_results") {
            Some(value) => value
                .as_u64()
                .filter(|count| (1..=25).contains(count))
                .ok_or("Gmail result limit is invalid.")?,
            None => 15,
        };
        let mut query = vec![("maxResults", count.to_string())];
        for (input, output, limit) in [("query", "q", 4096), ("page_token", "pageToken", 2048)] {
            if let Some(value) = arguments.get(input) {
                let value = value
                    .as_str()
                    .filter(|value| value.len() <= limit && !value.chars().any(char::is_control))
                    .ok_or("Gmail search arguments are invalid.")?;
                if !value.is_empty() {
                    query.push((output, value.into()));
                }
            }
        }
        let list = self.request(Method::GET, "/threads", &query, None, context)?;
        let mut records = Vec::new();
        if let Some(threads) = list.get("threads") {
            let threads = threads
                .as_array()
                .filter(|threads| threads.len() <= count as usize)
                .ok_or("Gmail thread list was invalid.")?;
            for thread in threads {
                let id = thread
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("Gmail thread id is missing.")?;
                records.push(record_from_thread(&self.thread(id, context)?));
            }
        }
        let mut result = envelope(records);
        if let Some(token) = list
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
        {
            result["nextPageToken"] = json!(token);
        }
        Ok(result)
    }
}

impl<P: TokenProvider> McpRpc for GmailDirectRpc<P> {
    fn call_tool(&self, service: Service, tool: &str, arguments: Value) -> Result<Value, String> {
        if service != Service::Gmail {
            return Err("Gmail transport only serves Gmail.".into());
        }
        let context = RequestContext {
            deadline: Instant::now() + DEADLINE,
            authorization_revision: (self.auth_revision)(),
        };
        match tool {
            "search_threads" => self.search(&arguments, context),
            "get_thread" => {
                let id = arguments
                    .get("thread_id")
                    .or_else(|| arguments.get("id"))
                    .and_then(Value::as_str)
                    .ok_or("Gmail thread id is missing.")?;
                if arguments.get("marker_only").and_then(Value::as_bool) == Some(true) {
                    validate_id(id)?;
                    let thread = self.request(Method::GET, &format!("/threads/{id}"),
                        &[("format", "minimal".into()), ("fields", "id,messages(id)".into())], None, context)?;
                    let latest = thread.get("messages").and_then(Value::as_array)
                        .and_then(|m| m.last()).and_then(|m| m.get("id"))
                        .and_then(Value::as_str).ok_or("Gmail thread marker unavailable.")?;
                    return Ok(json!({"latest_message_id": latest}));
                }
                Ok(envelope(vec![record_from_thread(
                    &self.thread(id, context)?,
                )]))
            }
            "create_draft" | "send_email" => {
                let message = message_body(&arguments)?;
                let (path, body) = if tool == "create_draft" {
                    ("/drafts", json!({"message":message}))
                } else {
                    ("/messages/send", message)
                };
                let result = self.request(Method::POST, path, &[], Some(body), context)?;
                if result
                    .get("id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(
                        if tool == "send_email" {
                            "delivery_uncertain: Gmail write response carries no id."
                        } else {
                            "Gmail write response carries no id."
                        }.into()
                    );
                }
                Ok(result)
            }
            "list_labels" => self.request(Method::GET, "/labels", &[], None, context),
            // The connected account's own address, and nothing else from the profile response.
            "get_profile" => {
                let profile = self.request(Method::GET, "/profile", &[], None, context)?;
                let address = profile
                    .get("emailAddress")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|address| {
                        address.contains('@')
                            && address.len() <= 320
                            && !address.chars().any(|c| c.is_control() || c.is_whitespace())
                    })
                    .ok_or("Gmail profile carries no address.")?;
                Ok(json!({ "email_address": address }))
            }
            "create_label" => {
                let name = arguments
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| {
                        !name.trim().is_empty()
                            && name.len() <= 225
                            && !name.chars().any(char::is_control)
                    })
                    .ok_or("Gmail label name is invalid.")?;
                self.request(
                    Method::POST,
                    "/labels",
                    &[],
                    Some(json!({"name":name})),
                    context,
                )
            }
            "modify_labels" | "archive_thread" | "unarchive_thread" => {
                let id = arguments
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .ok_or("Gmail thread id is missing.")?;
                validate_id(id)?;
                let body = match tool {
                    "archive_thread" => json!({"removeLabelIds":["INBOX"]}),
                    "unarchive_thread" => json!({"addLabelIds":["INBOX"]}),
                    _ => {
                        json!({"addLabelIds":label_ids(&arguments,"add_label_ids")?, "removeLabelIds":label_ids(&arguments,"remove_label_ids")?})
                    }
                };
                self.request(
                    Method::POST,
                    &format!("/threads/{id}/modify"),
                    &[],
                    Some(body),
                    context,
                )
            }
            _ => Err("That Gmail operation is unavailable.".into()),
        }
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("Gmail identifier is invalid.".into());
    }
    Ok(())
}

fn label_ids(arguments: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = arguments.get(key) else {
        return Ok(Vec::new());
    };
    let ids = value
        .as_array()
        .filter(|ids| ids.len() <= 100)
        .ok_or("Gmail labels are invalid.")?;
    ids.iter()
        .map(|id| {
            let id = id.as_str().ok_or("Gmail label id is invalid.")?;
            validate_id(id)?;
            Ok(id.into())
        })
        .collect()
}

fn message_body(arguments: &Value) -> Result<Value, String> {
    let to = arguments
        .get("to")
        .and_then(Value::as_str)
        .ok_or("Gmail recipient is missing.")?;
    validate_recipient(to)?;
    let subject = arguments
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("");
    if subject.len() > 998 || subject.chars().any(char::is_control) {
        return Err("Gmail subject is invalid.".into());
    }
    let body = arguments
        .get("body")
        .and_then(Value::as_str)
        .ok_or("Gmail body is missing.")?;
    if body.len() > 256 * 1024 {
        return Err("Gmail message exceeded the limit.".into());
    }
    // Encoding both subject and body avoids Unicode/header injection and preserves plain text.
    use base64::Engine;
    let subject = folded_subject(subject);
    let body = base64::engine::general_purpose::STANDARD.encode(body.as_bytes());
    let wrapped = body
        .as_bytes()
        .chunks(76)
        .map(|line| String::from_utf8_lossy(line))
        .collect::<Vec<_>>()
        .join("\r\n");
    let mime = format!("To: {to}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{wrapped}\r\n");
    Ok(json!({"raw":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mime.as_bytes())}))
}

fn validate_recipient(to: &str) -> Result<(), String> {
    let valid = to.len() <= 320
        && to.is_ascii()
        && to.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && local.len() <= 64
                && !local.starts_with('.')
                && !local.ends_with('.')
                && !local.contains("..")
                && local
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b))
                && domain.len() <= 253
                && domain.contains('.')
                && domain.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && label
                            .as_bytes()
                            .first()
                            .is_some_and(u8::is_ascii_alphanumeric)
                        && label
                            .as_bytes()
                            .last()
                            .is_some_and(u8::is_ascii_alphanumeric)
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        });
    if valid {
        Ok(())
    } else {
        Err("Gmail recipient is invalid.".into())
    }
}

// RFC 2047 encoded words are at most 75 characters. Chunk on UTF-8 boundaries so each
// word is independently decodable and fold the header using continuation whitespace.
fn folded_subject(subject: &str) -> String {
    use base64::Engine;
    let mut words = Vec::new();
    let mut chunk = String::new();
    for ch in subject.chars() {
        if chunk.len() + ch.len_utf8() > 42 {
            words.push(format!(
                "=?UTF-8?B?{}?=",
                base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes())
            ));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes())
        ));
    }
    words.join("\r\n ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::{RecordingSink, TraceUnrecorded};
    use base64::Engine;
    use shogun_integrations::rpc::StaticTokenProvider;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Fixture {
        root: String,
        server: std::thread::JoinHandle<Vec<String>>,
    }
    fn fixture(responses: Vec<String>) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        listener.set_nonblocking(true).expect("test listener mode");
        let root = format!(
            "http://{}/gmail/v1/users/me",
            listener.local_addr().expect("test address")
        );
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream: TcpStream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "provider request did not arrive");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => panic!("test accept failed"),
                    }
                };
                // macOS can inherit the listener's nonblocking flag. A large fixture response
                // must stream with blocking I/O rather than fail halfway through with WouldBlock.
                stream.set_nonblocking(false).expect("test stream mode");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("test timeout");
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = stream.read(&mut buf).expect("test request read");
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(str::trim)
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(request).expect("test UTF8"));
                stream
                    .write_all(response.as_bytes())
                    .expect("test response write");
            }
            // Any automatic retry of a failed write would arrive here.
            std::thread::sleep(Duration::from_millis(60));
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "unexpected retry"
            );
            requests
        });
        Fixture { root, server }
    }
    fn response(status: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    }
    fn rpc(root: &str, sink: Arc<dyn TraceabilitySink>) -> GmailDirectRpc<StaticTokenProvider> {
        let mut rpc = GmailDirectRpc::new(
            StaticTokenProvider::new("fixture-access"),
            sink,
            Arc::new(|| true),
        )
        .expect("test client");
        rpc.base_url = root.into();
        rpc
    }
    #[test]
    fn reply_marker_uses_one_minimal_thread_get() {
        let fixture = fixture(vec![response("200 OK",
            r#"{"id":"abc12","messages":[{"id":"m1"},{"id":"m2"}]}"#)]);
        let result = rpc(&fixture.root, Arc::new(RecordingSink::new()))
            .call_tool(Service::Gmail, "get_thread", json!({"thread_id":"abc12", "marker_only":true})).expect("test");
        assert_eq!(result, json!({"latest_message_id":"m2"}));
        let requests = fixture.server.join().expect("test");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /gmail/v1/users/me/threads/abc12?format=minimal&fields=id%2Cmessages%28id%29 "));
    }

    #[test]
    fn search_encodes_query_and_page_token_and_normalizes_thread() {
        let fixture = fixture(vec![
            response(
                "200 OK",
                r#"{"threads":[{"id":"abc12"}],"nextPageToken":"next-page"}"#,
            ),
            response(
                "200 OK",
                r#"{"id":"abc12","messages":[{"id":"msg1","internalDate":"1234","snippet":"hello","payload":{"headers":[{"name":"Subject","value":"Sync"}],"mimeType":"text/plain","body":{"data":"aGVsbG8"}}}]}"#,
            ),
        ]);
        let sink = Arc::new(RecordingSink::new());
        let result = rpc(&fixture.root, sink.clone()).call_tool(Service::Gmail,"search_threads",json!({"query":"from:a+b@example.com &label:work","page_token":"page&+/?","max_results":1})).expect("search");
        let items = shogun_integrations::result::parse_items(&result).expect("normalize");
        assert_eq!(items[0].external_id, "abc12");
        assert_eq!(items[0].body, "hello");
        assert_eq!(result["nextPageToken"], "next-page");
        let requests = fixture.server.join().expect("fixture");
        assert!(requests[0].starts_with("GET /gmail/v1/users/me/threads?maxResults=1&q=from%3Aa%2Bb%40example.com+%26label%3Awork&pageToken=page%26%2B%2F%3F "));
        assert!(requests[1].starts_with("GET /gmail/v1/users/me/threads/abc12?format=full "));
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-access"));
        let traces = sink.records();
        assert_eq!(traces.len(), 2);
        assert!(traces.iter().all(|record| record.route == Route::Mcp
            && !record.third_party
            && record.chunk_bytes == 0));
        assert!(!format!("{traces:?}").contains("example.com"));
    }
    #[test]
    fn profile_returns_only_the_accounts_address() {
        let fixture = fixture(vec![response(
            "200 OK",
            r#"{"emailAddress":"ada@example.com","messagesTotal":12,"threadsTotal":9,"historyId":"77"}"#,
        )]);
        let sink = Arc::new(RecordingSink::new());
        let result = rpc(&fixture.root, sink.clone())
            .call_tool(Service::Gmail, "get_profile", json!({}))
            .expect("profile");
        assert_eq!(result, json!({ "email_address": "ada@example.com" }));
        let requests = fixture.server.join().expect("fixture");
        assert!(requests[0].starts_with("GET /gmail/v1/users/me/profile "));
        assert_eq!(sink.records().len(), 1, "traced like every Google call");
    }
    #[test]
    fn empty_mailbox_is_success() {
        let fixture = fixture(vec![response("200 OK", r#"{"resultSizeEstimate":0}"#)]);
        let result = rpc(&fixture.root, Arc::new(RecordingSink::new()))
            .call_tool(Service::Gmail, "search_threads", json!({}))
            .expect("empty mailbox");
        assert!(shogun_integrations::result::parse_items(&result)
            .expect("normalize")
            .is_empty());
        assert_eq!(fixture.server.join().expect("fixture").len(), 1);
    }
    #[test]
    fn send_uses_encoded_mime_and_failed_send_is_not_retried() {
        let fixture = fixture(vec![response(
            "500 Internal Server Error",
            "secret provider body",
        )]);
        let sink = Arc::new(RecordingSink::new());
        let subject = "日曜日の予定 ".repeat(20);
        let body = "Hello\n日本語\n";
        let error = rpc(&fixture.root, sink.clone())
            .call_tool(
                Service::Gmail,
                "send_email",
                json!({"to":"person+work@example.com","subject":subject,"body":body}),
            )
            .expect_err("failed send");
        assert_eq!(error, "delivery_uncertain: Gmail request failed (HTTP 500).");
        let requests = fixture.server.join().expect("fixture");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /gmail/v1/users/me/messages/send "));
        let payload: Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").expect("body").1)
                .expect("JSON");
        let mime = String::from_utf8(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload["raw"].as_str().expect("raw"))
                .expect("decode"),
        )
        .expect("UTF8");
        let (headers, encoded_body) = mime.split_once("\r\n\r\n").expect("MIME split");
        assert!(headers.starts_with("To: person+work@example.com\r\nSubject: "));
        let subject_lines = headers
            .split("Subject: ")
            .nth(1)
            .expect("subject")
            .split("\r\nMIME-Version")
            .next()
            .expect("subject header");
        let decoded_subject = subject_lines
            .split_ascii_whitespace()
            .map(|word| {
                assert!(word.len() <= 75);
                let encoded = word
                    .strip_prefix("=?UTF-8?B?")
                    .and_then(|word| word.strip_suffix("?="))
                    .expect("encoded word");
                String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .expect("decode word"),
                )
                .expect("word UTF8")
            })
            .collect::<String>();
        assert_eq!(decoded_subject, subject);
        let decoded_body = base64::engine::general_purpose::STANDARD
            .decode(encoded_body.split_ascii_whitespace().collect::<String>())
            .expect("body decode");
        assert_eq!(decoded_body, body.as_bytes());
        assert_eq!(sink.records().len(), 1);
    }
    #[test]
    fn send_rejection_is_definite_but_lost_reply_is_uncertain() {
        for (reply, uncertain) in [
            (response("400 Bad Request", "private rejection"), false),
            (response("401 Unauthorized", "private rejection"), false),
            (response("403 Forbidden", "private rejection"), false),
            (response("429 Too Many Requests", "private rejection"), false),
            (String::new(), true),
            (response("200 OK", "not json"), true),
            (response("200 OK", "{}"), true),
        ] {
            let fixture = fixture(vec![reply]);
            let error = rpc(&fixture.root, Arc::new(RecordingSink::new()))
                .call_tool(Service::Gmail, "send_email",
                    json!({"to":"person@example.com", "body":"hello"}))
                .expect_err("failed send");
            assert_eq!(error.starts_with("delivery_uncertain:"), uncertain, "{error}");
            assert!(!error.contains("private rejection"));
            assert_eq!(fixture.server.join().expect("fixture").len(), 1);
        }
    }

    #[test]
    fn refused_connection_does_not_imply_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let root = format!("http://{}/gmail/v1/users/me", listener.local_addr().expect("address"));
        drop(listener);
        let error = rpc(&root, Arc::new(RecordingSink::new()))
            .call_tool(Service::Gmail, "send_email",
                json!({"to":"person@example.com", "body":"hello"}))
            .expect_err("connection refused");
        assert_eq!(error, "Gmail connection failed before send.");
    }

    #[test]
    fn draft_payload_is_wrapped_and_write_id_is_required() {
        let fixture = fixture(vec![
            response("200 OK", r#"{"id":"draft-1"}"#),
            response("200 OK", r#"{}"#),
        ]);
        let client = rpc(&fixture.root, Arc::new(RecordingSink::new()));
        let args = json!({"to":"person@example.com","subject":"","body":"hello"});
        assert_eq!(
            client
                .call_tool(Service::Gmail, "create_draft", args.clone())
                .expect("draft")["id"],
            "draft-1"
        );
        assert!(client
            .call_tool(Service::Gmail, "send_email", args)
            .expect_err("missing id")
            .contains("no id"));
        let requests = fixture.server.join().expect("fixture");
        let payload: Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").expect("body").1)
                .expect("JSON");
        assert!(payload["message"]["raw"].is_string());
    }
    #[test]
    fn malformed_ids_headers_and_delete_are_refused_before_token_access() {
        struct NeverToken;
        impl TokenProvider for NeverToken {
            fn access_token(&self, _: Service) -> Result<String, String> {
                panic!("must refuse before token access")
            }
        }
        let client = GmailDirectRpc::new(
            NeverToken,
            Arc::new(RecordingSink::new()),
            Arc::new(|| true),
        )
        .expect("test client");
        for (tool, args) in [
            ("get_thread", json!({"thread_id":"../other-user"})),
            ("get_thread", json!({"thread_id":"id?alt=media"})),
            (
                "send_email",
                json!({"to":"person@example.com\r\nBcc: victim@example.com","body":"hello"}),
            ),
            (
                "send_email",
                json!({"to":"person@example.com","subject":"Hi\nBcc: victim@example.com","body":"hello"}),
            ),
            ("delete_thread", json!({"thread_id":"abc"})),
            ("search_threads", json!({"max_results":100})),
        ] {
            assert!(client.call_tool(Service::Gmail, tool, args).is_err());
        }
        for address in [
            ".a@example.com",
            "a..b@example.com",
            "a@example..com",
            "a@-example.com",
            "a@example.com.",
            "a@example.com,b@example.com",
        ] {
            assert!(validate_recipient(address).is_err());
        }
        assert!(validate_recipient("person+work@example.com").is_ok());
    }
    #[test]
    fn redirect_is_not_followed_and_provider_errors_are_content_free() {
        let fixture = fixture(vec!["HTTP/1.1 302 Found\r\nLocation: https://unexpected.example/mail\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),response("403 Forbidden","sensitive-email-content")]);
        let client = rpc(&fixture.root, Arc::new(RecordingSink::new()));
        assert_eq!(
            client
                .call_tool(Service::Gmail, "list_labels", json!({}))
                .expect_err("redirect"),
            "Gmail request refused (HTTP 302)."
        );
        assert_eq!(
            client
                .call_tool(Service::Gmail, "list_labels", json!({}))
                .expect_err("403"),
            "Gmail permission was denied; check scopes and reconnect."
        );
        assert_eq!(fixture.server.join().expect("fixture").len(), 2);
    }
    #[test]
    fn rate_limit_forbidden_is_transient_not_a_scope_problem() {
        let body = |reason: &str| {
            format!(r#"{{"error":{{"code":403,"errors":[{{"reason":"{reason}"}}]}}}}"#)
        };
        let fixture = fixture(vec![
            response("403 Forbidden", &body("userRateLimitExceeded")),
            response("403 Forbidden", &body("dailyLimitExceeded")),
            response("403 Forbidden", &body("insufficientPermissions")),
        ]);
        let client = rpc(&fixture.root, Arc::new(RecordingSink::new()));
        for expected in [
            "Gmail rate limit reached; try again later.",
            "Gmail rate limit reached; try again later.",
            "Gmail permission was denied; check scopes and reconnect.",
        ] {
            assert_eq!(
                client
                    .call_tool(Service::Gmail, "list_labels", json!({}))
                    .expect_err("403"),
                expected
            );
        }
        assert_eq!(fixture.server.join().expect("fixture").len(), 3);
    }
    #[test]
    fn oversized_response_is_refused() {
        let fixture = fixture(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE + 1
        )]);
        assert!(rpc(&fixture.root, Arc::new(RecordingSink::new()))
            .call_tool(Service::Gmail, "list_labels", json!({}))
            .expect_err("size")
            .contains("exceeded"));
        fixture.server.join().expect("fixture");
    }
    #[test]
    fn revocation_after_refresh_or_trace_blocks_egress() {
        struct RevokingToken(Arc<AtomicU64>);
        impl TokenProvider for RevokingToken {
            fn access_token(&self, _: Service) -> Result<String, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok("fixture-token".into())
            }
        }
        let revision = Arc::new(AtomicU64::new(1));
        let rev = revision.clone();
        let sink = Arc::new(RecordingSink::new());
        let client = GmailDirectRpc::new(RevokingToken(revision), sink.clone(), Arc::new(|| true))
            .expect("test client")
            .with_auth_revision(Arc::new(move || rev.load(Ordering::SeqCst)));
        assert!(client
            .call_tool(Service::Gmail, "list_labels", json!({}))
            .expect_err("revision")
            .contains("changed"));
        assert!(sink.records().is_empty());
        struct RevokingSink(Arc<AtomicBool>);
        impl TraceabilitySink for RevokingSink {
            fn record(&self, _: TraceRecord) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let allowed = Arc::new(AtomicBool::new(true));
        let probe = allowed.clone();
        let client = GmailDirectRpc::new(
            StaticTokenProvider::new("fixture-token"),
            Arc::new(RevokingSink(allowed)),
            Arc::new(move || probe.load(Ordering::SeqCst)),
        )
        .expect("client");
        assert!(client
            .call_tool(Service::Gmail, "list_labels", json!({}))
            .expect_err("cancel")
            .contains("disconnect"));
    }
    #[test]
    fn credentials_failure_is_content_free_and_cancels_before_trace() {
        struct BadToken;
        impl TokenProvider for BadToken {
            fn access_token(&self, _: Service) -> Result<String, String> {
                Err("private-token@example.com".into())
            }
        }
        let sink = Arc::new(RecordingSink::new());
        let client =
            GmailDirectRpc::new(BadToken, sink.clone(), Arc::new(|| true)).expect("test client");
        assert_eq!(
            client
                .call_tool(Service::Gmail, "list_labels", json!({}))
                .expect_err("credentials"),
            "Gmail credentials are unavailable; reconnect Gmail."
        );
        assert!(sink.records().is_empty());
    }

    #[test]
    fn transient_refresh_failure_does_not_ask_for_reauth() {
        struct FailingRefresh(&'static str);
        impl TokenProvider for FailingRefresh {
            fn access_token(&self, _: Service) -> Result<String, String> {
                Err(self.0.into())
            }
        }
        let call = |reason: &'static str| {
            let sink = Arc::new(RecordingSink::new());
            let client = GmailDirectRpc::new(FailingRefresh(reason), sink.clone(), Arc::new(|| true))
                .expect("test client");
            let error = client
                .call_tool(Service::Gmail, "list_labels", json!({}))
                .expect_err("token failure");
            assert!(sink.records().is_empty());
            error
        };
        let locked = call("keychain_locked");
        assert_eq!(locked, "Keychain locked");
        assert!(!shogun_integrations::runtime::is_auth_failure(&locked));
        // Wi-Fi dropped during the refresh: the stored grant is still valid.
        let offline = call(r#"Refresh("token exchange failed: error sending request for url")"#);
        assert_eq!(offline, GMAIL_REFRESH_UNAVAILABLE);
        assert!(!shogun_integrations::runtime::is_auth_failure(&offline));
        let outage = call(r#"Refresh("token endpoint http 503 Service Unavailable")"#);
        assert!(!shogun_integrations::runtime::is_auth_failure(&outage));
        // The grant itself was rejected or is gone: amber with the reauth affordance.
        for reason in [r#"Refresh("token endpoint http 400 Bad Request")"#, "NeedsReauth", "NotConnected"] {
            let revoked = call(reason);
            assert_eq!(revoked, "Gmail credentials are unavailable; reconnect Gmail.");
            assert!(shogun_integrations::runtime::is_auth_failure(&revoked));
        }
    }

    #[test]
    fn revocation_between_list_and_thread_fetch_discards_page_without_more_http() {
        struct RotatingToken {
            revision: Arc<AtomicU64>,
            calls: AtomicU64,
        }
        impl TokenProvider for RotatingToken {
            fn access_token(&self, _: Service) -> Result<String, String> {
                if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                    self.revision.fetch_add(1, Ordering::SeqCst);
                }
                Ok("fixture-token".into())
            }
        }
        let fixture = fixture(vec![response("200 OK", r#"{"threads":[{"id":"abc"}]}"#)]);
        let revision = Arc::new(AtomicU64::new(1));
        let probe = revision.clone();
        let sink = Arc::new(RecordingSink::new());
        let mut client = GmailDirectRpc::new(
            RotatingToken {
                revision,
                calls: AtomicU64::new(0),
            },
            sink.clone(),
            Arc::new(|| true),
        )
        .expect("test client")
        .with_auth_revision(Arc::new(move || probe.load(Ordering::SeqCst)));
        client.base_url = fixture.root;
        assert!(client
            .call_tool(Service::Gmail, "search_threads", json!({}))
            .expect_err("changed grant")
            .contains("changed"));
        assert_eq!(fixture.server.join().expect("fixture").len(), 1);
        assert_eq!(sink.records().len(), 1);
    }

    #[test]
    fn response_without_content_length_is_still_bounded() {
        let body = "x".repeat(MAX_RESPONSE as usize + 1);
        let fixture = fixture(vec![format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}"
        )]);
        assert!(rpc(&fixture.root, Arc::new(RecordingSink::new()))
            .call_tool(Service::Gmail, "list_labels", json!({}))
            .expect_err("limit")
            .contains("exceeded"));
        fixture.server.join().expect("fixture");
    }

    #[test]
    fn slow_trace_cannot_dispatch_after_the_operation_deadline() {
        struct SlowTrace;
        impl TraceabilitySink for SlowTrace {
            fn record(&self, _: TraceRecord) {
                std::thread::sleep(Duration::from_millis(60));
            }
        }
        let client = rpc("http://127.0.0.1:0", Arc::new(SlowTrace));
        let context = RequestContext {
            deadline: Instant::now() + Duration::from_millis(30),
            authorization_revision: 0,
        };
        assert_eq!(client.request(Method::GET, "/labels", &[], None, context)
            .expect_err("expired before dispatch"), "Gmail request deadline expired before send.");
    }

    #[test]
    fn missing_trace_and_disabled_sending_stop_before_provider_dispatch() {
        struct FailedSink;
        impl TraceabilitySink for FailedSink {
            fn record(&self, _: TraceRecord) {}
            fn try_record(&self, _: TraceRecord) -> Result<(), TraceUnrecorded> {
                Err(TraceUnrecorded)
            }
        }
        let client = rpc("http://127.0.0.1:1/gmail/v1/users/me", Arc::new(FailedSink));
        assert!(client
            .call_tool(Service::Gmail, "list_labels", json!({}))
            .expect_err("trace")
            .contains("recorded"));
        let sink = Arc::new(RecordingSink::new());
        let client = rpc("http://127.0.0.1:1/gmail/v1/users/me", sink.clone())
            .with_send_allowed(Arc::new(|| false));
        assert!(client
            .call_tool(
                Service::Gmail,
                "send_email",
                json!({"to":"person@example.com","body":"hi"})
            )
            .expect_err("disabled")
            .contains("disabled"));
        assert!(sink.records().is_empty());
    }
}
