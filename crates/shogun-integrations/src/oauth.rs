//! OAuth 2.1 Authorization Code + PKCE for first-layer connections (§6.9, FR-INT-02).
//!
//! Auth is **user → service directly** (no intermediary): the app opens the service's consent page
//! in the system browser, catches the redirect on a loopback URI, and exchanges the code for tokens
//! using a PKCE verifier — so no long-lived secret is required in the client and no code can be
//! replayed. Tokens land in the Keychain (invariant 7), never a file/DB/log.
//!
//! This module is the **pure** half: PKCE derivation, the authorize URL, the token-exchange /
//! refresh request forms, and token-response parsing with expiry. It has no network and no
//! randomness (entropy is passed in), so it is fully Linux-testable. The effectful half — generate
//! entropy, open the browser, run the loopback listener, POST the token endpoint, persist to
//! Keychain — is [`crate::oauth_flow`] behind the `live` feature.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Google's OAuth endpoints (the defaults for every first-layer Google Workspace service).
pub const GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Slack's user-token OAuth endpoints (Wave 2; Slack does not support Dynamic Client Registration,
/// so a fixed app client ID is used. PKCE-enabled desktop apps need no bundled secret).
pub const SLACK_AUTH_ENDPOINT: &str = "https://slack.com/oauth/v2_user/authorize";
pub const SLACK_TOKEN_ENDPOINT: &str = "https://slack.com/api/oauth.v2.user.access";
pub const LINEAR_AUTH_ENDPOINT: &str = "https://linear.app/oauth/authorize";
pub const LINEAR_TOKEN_ENDPOINT: &str = "https://api.linear.app/oauth/token";
pub const GITHUB_TOKEN_ENDPOINT: &str = "https://github.com/login/oauth/access_token";
/// Notion's public-integration OAuth endpoints (E51). The token endpoint takes the app's client id
/// and secret as HTTP Basic auth and a JSON body, not the form the other vendors take.
pub const NOTION_AUTH_ENDPOINT: &str = "https://api.notion.com/v1/oauth/authorize";
pub const NOTION_TOKEN_ENDPOINT: &str = "https://api.notion.com/v1/oauth/token";
const GRANOLA_AUTH_ORIGIN: &str = "https://mcp-auth.granola.ai/";
const GRANOLA_RESOURCE: &str = "https://mcp.granola.ai/mcp";

/// The OAuth client configuration for one connection. `client_secret` is `None` for pure-PKCE
/// clients; Google "Desktop app" clients still send a (non-confidential) secret, so it is optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    /// The loopback redirect, e.g. `http://127.0.0.1:47821/callback`.
    pub redirect_uri: String,
    pub auth_endpoint: String,
    pub token_endpoint: String,
}

impl AuthConfig {
    /// A Google-endpoints config for a loopback `redirect_uri`.
    pub fn google(
        client_id: impl Into<String>,
        client_secret: Option<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            redirect_uri: redirect_uri.into(),
            auth_endpoint: GOOGLE_AUTH_ENDPOINT.to_string(),
            token_endpoint: GOOGLE_TOKEN_ENDPOINT.to_string(),
        }
    }

    /// A Slack-endpoints config (Wave 2). Slack requires the client secret (confidential client).
    pub fn slack(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: Some(client_secret.into()),
            redirect_uri: redirect_uri.into(),
            auth_endpoint: SLACK_AUTH_ENDPOINT.to_string(),
            token_endpoint: SLACK_TOKEN_ENDPOINT.to_string(),
        }
    }

    /// Slack desktop PKCE uses a public product client ID. Keep the confidential constructor
    /// above for existing owner-managed apps; never bundle its secret in a desktop build.
    pub fn slack_public(client_id: impl Into<String>, redirect_uri: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            redirect_uri: redirect_uri.into(),
            auth_endpoint: SLACK_AUTH_ENDPOINT.into(),
            token_endpoint: SLACK_TOKEN_ENDPOINT.into(),
        }
    }

    /// Linear's user-actor PKCE grant. Its token endpoint accepts a public client ID without
    /// a client secret for both code exchange and refresh.
    pub fn linear(client_id: impl Into<String>, redirect_uri: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            redirect_uri: redirect_uri.into(),
            auth_endpoint: LINEAR_AUTH_ENDPOINT.into(),
            token_endpoint: LINEAR_TOKEN_ENDPOINT.into(),
        }
    }
}

/// Slack's public client IDs are two nonempty decimal components separated by a dot.
pub fn valid_slack_client_id(id: &str) -> bool {
    id.len() <= 64
        && id.split_once('.').is_some_and(|(first, second)| {
            !first.is_empty()
                && !second.is_empty()
                && first
                    .bytes()
                    .chain(second.bytes())
                    .all(|byte| byte.is_ascii_digit())
        })
}

/// Bounded public OAuth client ID (GitHub App, Linear) for Settings and the built-in value.
pub fn valid_public_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// A PKCE verifier/challenge pair (RFC 7636, method `S256`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// The secret verifier — sent only in the token exchange, never in the browser URL.
    pub verifier: String,
    /// `base64url(sha256(verifier))` — sent in the authorize URL.
    pub challenge: String,
}

impl Pkce {
    /// Derive a PKCE pair from raw entropy (≥ 32 bytes recommended). Deterministic and pure — the
    /// caller supplies OS randomness (see [`crate::oauth_flow`]). The verifier is the base64url of
    /// the entropy (unreserved chars only, RFC 7636 §4.1); the challenge is its SHA-256.
    pub fn from_entropy(entropy: &[u8]) -> Self {
        let verifier = URL_SAFE_NO_PAD.encode(entropy);
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }
}

/// Build the authorize URL to open in the system browser. `state` is an anti-CSRF nonce the caller
/// must verify on the redirect. `access_type=offline` + `prompt=consent` request a refresh token.
pub fn authorize_url(
    cfg: &AuthConfig,
    scopes: &[&str],
    pkce: &Pkce,
    state: &str,
) -> Result<String, String> {
    let mut url =
        url::Url::parse(&cfg.auth_endpoint).map_err(|e| format!("bad auth endpoint: {e}"))?;
    // Slack and Linear document comma-separated scopes; OAuth's default is a space.
    let separator = if cfg.auth_endpoint == SLACK_AUTH_ENDPOINT || cfg.auth_endpoint == LINEAR_AUTH_ENDPOINT { "," } else { " " };
    url.query_pairs_mut()
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &cfg.redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes.join(separator))
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    if cfg.auth_endpoint == GOOGLE_AUTH_ENDPOINT {
        url.query_pairs_mut().append_pair("access_type", "offline").append_pair("prompt", "consent");
    }
    if cfg.auth_endpoint == LINEAR_AUTH_ENDPOINT {
        url.query_pairs_mut().append_pair("actor", "user");
    }
    if cfg.auth_endpoint.starts_with(GRANOLA_AUTH_ORIGIN) {
        url.query_pairs_mut().append_pair("resource", GRANOLA_RESOURCE);
    }
    Ok(url.into())
}

/// The form body for the authorization-code → token exchange.
pub fn token_exchange_form(cfg: &AuthConfig, code: &str, pkce: &Pkce) -> Vec<(String, String)> {
    let mut form = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), cfg.redirect_uri.clone()),
        ("client_id".into(), cfg.client_id.clone()),
        ("code_verifier".into(), pkce.verifier.clone()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret".into(), secret.clone()));
    }
    if cfg.token_endpoint.starts_with(GRANOLA_AUTH_ORIGIN) {
        form.push(("resource".into(), GRANOLA_RESOURCE.into()));
    }
    form
}

/// The form body for a refresh-token → access-token exchange.
pub fn refresh_form(cfg: &AuthConfig, refresh_token: &str) -> Vec<(String, String)> {
    let mut form = vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), refresh_token.into()),
        ("client_id".into(), cfg.client_id.clone()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret".into(), secret.clone()));
    }
    if cfg.token_endpoint.starts_with(GRANOLA_AUTH_ORIGIN) {
        form.push(("resource".into(), GRANOLA_RESOURCE.into()));
    }
    form
}

/// Extract `(code, state)` from a loopback redirect's HTTP request line, e.g.
/// `GET /callback?code=abc&state=xyz HTTP/1.1`. Pure, so the loopback handler stays testable. An
/// `error=...` param (user denied consent) becomes an `Err`.
pub fn parse_redirect(request_line: &str) -> Result<(String, String), String> {
    let target = request_line.split_whitespace().nth(1).ok_or("malformed request line")?;
    // Give the relative target a base so `url` can parse just the path+query.
    let parsed = url::Url::parse("http://127.0.0.1").and_then(|b| b.join(target)).map_err(|e| format!("bad redirect target: {e}"))?;
    // Only the registered redirect path resolves the flow — any local process can hit the
    // listener, and a stray request carrying code+state params must not be mistaken for the
    // provider's redirect.
    if parsed.path() != "/callback" {
        return Err(format!("unexpected redirect path: {}", parsed.path()));
    }
    let mut code = None;
    let mut state = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "error" => return Err(format!("authorization denied: {v}")),
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => Err("redirect missing code or state".to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct RawTokenResponse {
    // Standard OAuth shape (Google): top-level fields.
    access_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    // Slack `oauth.v2.user.access` shape: the user token nests under `authed_user`.
    authed_user: Option<RawAuthedUser>,
}

#[derive(Debug, Deserialize)]
struct RawAuthedUser {
    access_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
}

/// The token-endpoint POST seam — a form POST returning the JSON body. The real impl is a `reqwest`
/// client ([`crate::oauth_flow::HttpTokenExchange`], feature `live`); tests and the token manager
/// inject a fake. Defined here (pure) so both the interactive flow and [`crate::token`] share it.
pub trait TokenExchange {
    /// POST `form` (application/x-www-form-urlencoded) to `token_endpoint`, returning the JSON body.
    fn post_form(&self, token_endpoint: &str, form: &[(String, String)]) -> Result<String, String>;

    /// One OAuth request that is not a plain form POST (E51): Notion's token endpoint (HTTP Basic
    /// client authentication and a JSON body), Linear's revocation (the access token as bearer) and
    /// GitHub's grant deletion (`DELETE` with the app's Basic auth). The errors follow
    /// [`Self::post_form`]'s words: `token exchange failed: …` when nothing came back,
    /// `token endpoint http <status>` for a non-2xx answer (the body is never surfaced). The
    /// default answers a plain form POST through [`Self::post_form`] and refuses anything else, so
    /// an exchange that cannot speak a request never sends a different one.
    fn send(&self, request: &ExchangeRequest<'_>) -> Result<String, String> {
        match (request.method, &request.auth, &request.body) {
            (ExchangeMethod::Post, ExchangeAuth::None, ExchangeBody::Form(form)) if request.headers.is_empty() => {
                self.post_form(request.endpoint, form)
            }
            _ => Err("token exchange unsupported request".to_string()),
        }
    }
}

/// The HTTP method of an [`ExchangeRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeMethod {
    Post,
    Delete,
}

/// How an [`ExchangeRequest`] authenticates. No `Debug`: it holds a secret or a token.
pub enum ExchangeAuth<'a> {
    /// Client credentials, if any, are in the form.
    None,
    /// `Authorization: Basic base64(user:password)` (RFC 7617): an app's client id and secret.
    Basic { user: &'a str, password: &'a str },
    /// `Authorization: Bearer <token>`.
    Bearer(&'a str),
}

/// The body of an [`ExchangeRequest`]. No `Debug`: it holds codes and tokens.
pub enum ExchangeBody<'a> {
    Form(&'a [(String, String)]),
    Json(&'a serde_json::Value),
    Empty,
}

/// One request to a vendor's OAuth API through the core's single egress.
pub struct ExchangeRequest<'a> {
    pub method: ExchangeMethod,
    pub endpoint: &'a str,
    pub auth: ExchangeAuth<'a>,
    pub body: ExchangeBody<'a>,
    /// Extra fixed headers (an API version); never a credential.
    pub headers: &'a [(&'static str, &'static str)],
}

/// Notion's authorize URL (E51): `owner=user`, the hosted callback and the gateway's state. Notion
/// has no scopes (an integration's capabilities are set in its settings) and no PKCE.
pub fn notion_authorize_url(cfg: &AuthConfig, state: &str) -> Result<String, String> {
    let mut url = url::Url::parse(&cfg.auth_endpoint).map_err(|e| format!("bad auth endpoint: {e}"))?;
    url.query_pairs_mut()
        .append_pair("client_id", &cfg.client_id)
        .append_pair("response_type", "code")
        .append_pair("owner", "user")
        .append_pair("redirect_uri", &cfg.redirect_uri)
        .append_pair("state", state);
    Ok(url.into())
}

/// Notion's token request body: the code exchange with the hosted callback, or a refresh.
pub fn notion_code_body(cfg: &AuthConfig, code: &str) -> serde_json::Value {
    serde_json::json!({ "grant_type": "authorization_code", "code": code, "redirect_uri": cfg.redirect_uri })
}

/// Notion's refresh request body.
pub fn notion_refresh_body(refresh_token: &str) -> serde_json::Value {
    serde_json::json!({ "grant_type": "refresh_token", "refresh_token": refresh_token })
}

/// Parse Notion's token response: an `access_token` of visible ASCII, `token_type: bearer` and a
/// `bot_id`, with the workspace's name as the account label. A Notion integration token has not
/// expired historically: a response without `expires_in` stores a grant that does not expire
/// (`i64::MAX`, like a Slack token without rotation); one with `expires_in` (and a
/// `refresh_token`) is refreshed like any other grant.
pub fn parse_notion_token_response(body: &str, now_ms: i64, prior_refresh: Option<String>) -> Result<TokenSet, String> {
    const INCOMPATIBLE: &str = "Notion OAuth token response is incompatible";
    let response: serde_json::Value = serde_json::from_str(body).map_err(|_| "token response was not valid json".to_string())?;
    let text = |name: &str| {
        response
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty() && v.len() <= 4_096 && v.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_string)
    };
    let Some(access_token) = text("access_token") else {
        return Err(INCOMPATIBLE.into());
    };
    if response.get("error").is_some()
        || response.get("token_type").and_then(serde_json::Value::as_str).is_some_and(|t| !t.eq_ignore_ascii_case("bearer"))
        || response.get("bot_id").and_then(serde_json::Value::as_str).is_none_or(str::is_empty)
    {
        return Err(INCOMPATIBLE.into());
    }
    let expires_at_ms = match response.get("expires_in") {
        None | Some(serde_json::Value::Null) => i64::MAX,
        Some(value) => value
            .as_i64()
            .filter(|seconds| *seconds > 0)
            .and_then(|seconds| seconds.checked_mul(1_000))
            .and_then(|ms| now_ms.checked_add(ms))
            .ok_or(INCOMPATIBLE)?,
    };
    Ok(TokenSet { access_token, refresh_token: text("refresh_token").or(prior_refresh), expires_at_ms, account_label: account_label_from_response(body) })
}

/// A parsed token set with an absolute expiry (unix ms). `refresh_token` may be absent on a refresh
/// response (the existing one stays valid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: i64,
    /// Non-secret identity returned by the existing sign-in response. Never log it.
    pub account_label: Option<String>,
}

impl TokenSet {
    /// Whether the access token is expired (or within `skew_ms` of expiring) at `now_ms`.
    pub fn is_expired(&self, now_ms: i64, skew_ms: i64) -> bool {
        now_ms.saturating_add(skew_ms) >= self.expires_at_ms
    }
}

/// Verify explicit provider scope grants. OAuth permits an omitted scope only when the grant
/// is identical to the requested scope (RFC 6749 section 5.1).
pub fn verify_granted_scopes(body: &str, requested: &[&str]) -> Result<(), String> {
    let response: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid token response")?;
    let Some(scope) = response.get("scope") else {
        return Ok(());
    };
    let scope = scope.as_str().ok_or("Google granted scopes were invalid")?;
    if requested.iter().any(|required| {
        !scope
            .split_ascii_whitespace()
            .any(|granted| granted == *required)
    }) {
        return Err("Google did not grant every requested permission".into());
    }
    Ok(())
}

/// Parse a token-endpoint JSON response into a [`TokenSet`], computing the absolute expiry from
/// `now_ms + expires_in`. Accepts both the standard flat OAuth shape (Google) and Slack's
/// `authed_user`-nested user-token shape. `prior_refresh` carries forward a refresh token the
/// response omitted.
pub fn parse_token_response(
    body: &str,
    now_ms: i64,
    prior_refresh: Option<String>,
) -> Result<TokenSet, String> {
    let raw: RawTokenResponse =
        serde_json::from_str(body).map_err(|_| "token response was not valid json".to_string())?;
    // Prefer the flat shape; fall back to the nested Slack user token.
    let nested = raw.authed_user;
    let (access, expires_in, refresh) = match (raw.access_token, nested) {
        (Some(at), _) => (at, raw.expires_in, raw.refresh_token),
        (None, Some(u)) => match u.access_token {
            Some(at) => (at, u.expires_in, u.refresh_token),
            None => return Err("token response had no access token".to_string()),
        },
        (None, None) => return Err("token response had no access token".to_string()),
    };
    // Default to a conservative 60 min if the server omitted expires_in.
    let ttl_ms = expires_in.unwrap_or(3600).saturating_mul(1000);
    Ok(TokenSet {
        access_token: access,
        refresh_token: refresh.or(prior_refresh),
        expires_at_ms: now_ms.saturating_add(ttl_ms),
        account_label: account_label_from_response(body),
    })
}

/// Keep only explicit identity fields; never retain the OAuth response or decode tokens.
pub fn account_label_from_response(body: &str) -> Option<String> {
    let response: serde_json::Value = serde_json::from_str(body).ok()?;
    ["/email", "/workspace_name", "/team/name", "/login", "/organization/name"]
        .iter()
        .find_map(|path| response.pointer(path).and_then(serde_json::Value::as_str)
            .map(str::trim).filter(|label| !label.is_empty()).map(str::to_owned))
}

/// Slack public-client grants can be non-expiring when rotation is disabled. No OAuth secret is
/// embedded in the app, and Slack's `ok:false` never becomes a token success.
pub fn parse_token_response_for(
    cfg: &AuthConfig,
    body: &str,
    now_ms: i64,
    prior_refresh: Option<String>,
) -> Result<TokenSet, String> {
    let mut tokens = parse_token_response(body, now_ms, prior_refresh)?;
    if cfg.token_endpoint == LINEAR_TOKEN_ENDPOINT {
        let response: serde_json::Value =
            serde_json::from_str(body).map_err(|_| "invalid token response")?;
        let token = |name: &str| {
            response
                .get(name)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| {
                    !value.is_empty()
                        && value.len() <= 4_096
                        && value.bytes().all(|byte| byte.is_ascii_graphic())
                })
        };
        let ttl = response
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .filter(|seconds| {
                *seconds > 0
                    && seconds
                        .checked_mul(1_000)
                        .and_then(|ms| now_ms.checked_add(ms))
                        .is_some()
            });
        let read_granted = match response.get("scope") {
            None => true,
            Some(serde_json::Value::String(scope)) => scope
                .split(|c: char| c == ',' || c.is_ascii_whitespace())
                .any(|scope| scope == "read"),
            Some(serde_json::Value::Array(scopes)) => {
                scopes.iter().any(|scope| scope.as_str() == Some("read"))
            }
            _ => false,
        };
        if response.get("error").is_some()
            || response
                .get("token_type")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|kind| !kind.eq_ignore_ascii_case("bearer"))
            || ttl.is_none()
            || !token("access_token")
            || !token("refresh_token")
            || !read_granted
        {
            return Err("Linear OAuth token response is incompatible".into());
        }
    }
    if cfg.token_endpoint == GITHUB_TOKEN_ENDPOINT {
        let response: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid token response")?;
        if response.get("error").is_some()
            || response.get("token_type").and_then(serde_json::Value::as_str) != Some("bearer")
            || response.get("expires_in").and_then(serde_json::Value::as_i64).is_none_or(|ttl| ttl <= 0)
            || response.get("refresh_token").and_then(serde_json::Value::as_str).is_none_or(str::is_empty)
        {
            return Err("GitHub OAuth token response is incompatible".into());
        }
    }
    if cfg.token_endpoint == SLACK_TOKEN_ENDPOINT {
        let response: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid token response")?;
        if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err("Slack OAuth unauthorized".into());
        }
        if tokens.access_token.len() > 8192
            || !tokens
                .access_token
                .bytes()
                .all(|byte| byte.is_ascii_graphic())
            || response.get("token_type").is_some_and(|kind| kind.as_str() != Some("user"))
        {
            return Err("Slack OAuth user token response is incompatible".into());
        }
        // Validate the same payload that supplied the access token; a rotating response must
        // include the replacement refresh token rather than retaining a consumed one.
        let payload = if response.get("access_token").is_some() {
            &response
        } else {
            response.get("authed_user").unwrap_or(&response)
        };
        if payload.get("token_type").is_some_and(|kind| kind.as_str() != Some("user")) {
            return Err("Slack OAuth user token response is incompatible".into());
        }
        if tokens.refresh_token.is_some() && payload.get("expires_in").is_none() {
            return Err("Slack rotating token response has no expiry".into());
        }
        if let Some(expiry) = payload.get("expires_in") {
            if expiry.as_i64().is_none_or(|seconds| {
                seconds <= 0
                    || seconds
                        .checked_mul(1000)
                        .and_then(|ms| now_ms.checked_add(ms))
                        .is_none()
            }) || payload
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|value| {
                    value.is_empty()
                        || value.len() > 8192
                        || !value.bytes().all(|byte| byte.is_ascii_graphic())
                })
            {
                return Err("Slack rotating token response is incompatible".into());
            }
        }
        if tokens.refresh_token.is_none()
            && response.get("expires_in").is_none()
            && response.pointer("/authed_user/expires_in").is_none()
        {
            tokens.expires_at_ms = i64::MAX;
        }
    }
    if tokens.access_token.trim().is_empty() { return Err("token response had no access token".into()); }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    #[test]
    fn linear_public_pkce_requests_user_read_and_secretless_refresh() {
        use super::{
            authorize_url, refresh_form, token_exchange_form, valid_public_client_id, AuthConfig,
            Pkce, LINEAR_AUTH_ENDPOINT, LINEAR_TOKEN_ENDPOINT,
        };
        assert!(valid_public_client_id(
            "c625b425-c950-43b9-9a3e-2dab88071375"
        ));
        assert!(!valid_public_client_id("bad\nclient"));
        let cfg = AuthConfig::linear(
            "c625b425-c950-43b9-9a3e-2dab88071375",
            "http://127.0.0.1:47825/callback",
        );
        assert_eq!(cfg.auth_endpoint, LINEAR_AUTH_ENDPOINT);
        assert_eq!(cfg.token_endpoint, LINEAR_TOKEN_ENDPOINT);
        assert!(cfg.client_secret.is_none());
        let pkce = Pkce::from_entropy(&[7; 32]);
        let authorize =
            url::Url::parse(&authorize_url(&cfg, &["read"], &pkce, "anti-csrf").unwrap()).unwrap();
        let params: std::collections::HashMap<_, _> =
            authorize.query_pairs().into_owned().collect();
        assert_eq!(params.get("scope").map(String::as_str), Some("read"));
        assert_eq!(params.get("actor").map(String::as_str), Some("user"));
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("anti-csrf"));
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(cfg.redirect_uri.as_str())
        );
        assert!(!authorize.as_str().contains(&pkce.verifier));
        let code_form = token_exchange_form(&cfg, "code", &pkce);
        assert!(code_form.contains(&("code_verifier".into(), pkce.verifier)));
        assert!(!code_form.iter().any(|(key, _)| key == "client_secret"));
        let refresh = refresh_form(&cfg, "old-refresh");
        assert!(refresh.contains(&("client_id".into(), cfg.client_id)));
        assert!(!refresh.iter().any(|(key, _)| key == "client_secret"));
    }

    #[test]
    fn linear_requires_bearer_read_expiry_and_fresh_rotating_tokens() {
        use super::{parse_token_response_for, AuthConfig};
        let cfg = AuthConfig::linear("client", "http://127.0.0.1:47825/callback");
        let valid = r#"{"access_token":"access","token_type":"Bearer","expires_in":86399,"scope":"read","refresh_token":"refresh"}"#;
        let tokens = parse_token_response_for(&cfg, valid, 1_000, None).unwrap();
        assert_eq!(tokens.expires_at_ms, 86_400_000);
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));
        let legacy_scope = r#"{"access_token":"next","token_type":"Bearer","expires_in":86399,"scope":["read"],"refresh_token":"next-refresh"}"#;
        assert_eq!(
            parse_token_response_for(&cfg, legacy_scope, 0, Some("old".into()))
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("next-refresh")
        );
        for bad in [
            r#"{"error":"invalid_grant","access_token":"x","token_type":"Bearer","expires_in":1,"scope":"read","refresh_token":"r"}"#,
            r#"{"access_token":"x","token_type":"mac","expires_in":1,"scope":"read","refresh_token":"r"}"#,
            r#"{"access_token":"x","token_type":"Bearer","expires_in":0,"scope":"read","refresh_token":"r"}"#,
            r#"{"access_token":"x","token_type":"Bearer","expires_in":1,"scope":"write","refresh_token":"r"}"#,
            r#"{"access_token":"x","token_type":"Bearer","expires_in":1,"scope":"read"}"#,
        ] {
            assert!(parse_token_response_for(&cfg, bad, 0, Some("spent".into())).is_err());
        }
    }

    #[test]
    fn granola_requests_the_protected_mcp_resource() {
        use super::{
            authorize_url, refresh_form, token_exchange_form, AuthConfig, Pkce, GRANOLA_RESOURCE,
        };
        let cfg = AuthConfig {
            client_id: "client".into(), client_secret: None,
            redirect_uri: "http://127.0.0.1:1234/callback".into(),
            auth_endpoint: "https://mcp-auth.granola.ai/oauth2/authorize".into(),
            token_endpoint: "https://mcp-auth.granola.ai/oauth2/token".into(),
        };
        let pkce = Pkce::from_entropy(&[7; 32]);
        let url = authorize_url(&cfg, &["mcp"], &pkce, "state").unwrap();
        assert!(url.contains("resource=https%3A%2F%2Fmcp.granola.ai%2Fmcp"));
        assert!(token_exchange_form(&cfg, "code", &pkce).contains(&("resource".into(), GRANOLA_RESOURCE.into())));
        assert!(refresh_form(&cfg, "refresh").contains(&("resource".into(), GRANOLA_RESOURCE.into())));
    }

    #[test]
    fn github_device_tokens_require_bearer_expiry_and_refresh() {
        use super::{parse_token_response_for, AuthConfig, GITHUB_TOKEN_ENDPOINT};
        let cfg = AuthConfig {
            client_id: "id".into(),
            client_secret: None,
            redirect_uri: String::new(),
            auth_endpoint: String::new(),
            token_endpoint: GITHUB_TOKEN_ENDPOINT.into(),
        };
        let good = r#"{"access_token":"access","token_type":"bearer","expires_in":28800,"refresh_token":"refresh"}"#;
        assert_eq!(parse_token_response_for(&cfg, good, 1_000, None).unwrap().expires_at_ms, 28_801_000);
        for bad in [
            r#"{"error":"expired_token","access_token":"fake","token_type":"bearer","expires_in":1,"refresh_token":"r"}"#,
            r#"{"access_token":"x","token_type":"mac","expires_in":1,"refresh_token":"r"}"#,
            r#"{"access_token":"x","token_type":"bearer","expires_in":0,"refresh_token":"r"}"#,
            r#"{"access_token":"x","token_type":"bearer","expires_in":1}"#,
        ] {
            assert!(parse_token_response_for(&cfg, bad, 0, None).is_err());
        }
        let rotated = r#"{"access_token":"new","token_type":"bearer","expires_in":28800}"#;
        // GitHub rotates refresh tokens: carrying the spent prior grant must fail.
        assert!(parse_token_response_for(&cfg, rotated, 0, Some("old".into())).is_err());
        let rotated = r#"{"access_token":"new","token_type":"bearer","expires_in":28800,"refresh_token":"new-refresh"}"#;
        assert_eq!(parse_token_response_for(&cfg, rotated, 0, Some("old".into())).unwrap().refresh_token.as_deref(), Some("new-refresh"));
    }

    #[test]
    fn granted_scope_refuses_partial_permission_and_accepts_exact_or_omitted() {
        let requested = ["https://www.googleapis.com/auth/gmail.modify"];
        assert!(super::verify_granted_scopes(
            r#"{"scope":"https://www.googleapis.com/auth/gmail.readonly"}"#,
            &requested
        )
        .is_err());
        assert!(super::verify_granted_scopes(
            r#"{"scope":"https://www.googleapis.com/auth/gmail.modify openid"}"#,
            &requested
        )
        .is_ok());
        assert!(
            super::verify_granted_scopes(r#"{"access_token":"not-logged"}"#, &requested).is_ok()
        );
        assert!(super::verify_granted_scopes(r#"{"scope":null}"#, &requested).is_err());
    }

    use super::*;

    #[test]
    fn identity_uses_only_explicit_response_metadata() {
        for (body, expected) in [
            (r#"{"email":"you@example.com"}"#, "you@example.com"),
            (r#"{"team":{"name":"Acme"}}"#, "Acme"),
            (r#"{"workspace_name":"Notes"}"#, "Notes"),
            (r#"{"login":"octocat"}"#, "octocat"),
            (r#"{"organization":{"name":"Engineering"}}"#, "Engineering"),
        ] {
            assert_eq!(account_label_from_response(body).as_deref(), Some(expected));
        }
        for body in [r#"{"access_token":"secret","workspace_id":"id"}"#, r#"{"email":"  "}"#, "invalid"] {
            assert_eq!(account_label_from_response(body), None);
        }
    }

    #[test]
    fn slack_public_client_omits_google_options_and_handles_nonrotating_user_tokens() {
        let config = AuthConfig::slack_public("123.456", "http://localhost:47822/callback");
        let pkce = Pkce::from_entropy(&[7; 32]);
        let url = authorize_url(&config, &["search:read.public"], &pkce, "nonce").unwrap();
        assert!(!url.contains("access_type"));
        assert!(!url.contains("prompt="));
        assert!(!token_exchange_form(&config, "code", &pkce)
            .iter()
            .any(|(key, _)| key == "client_secret"));
        assert_eq!(
            parse_token_response_for(
                &config,
                r#"{"ok":true,"access_token":"test-user"}"#,
                2000,
                None
            )
            .unwrap()
            .expires_at_ms,
            i64::MAX
        );
        assert!(parse_token_response_for(
            &config,
            r#"{"ok":false,"access_token":"invalid"}"#,
            2000,
            None
        )
        .is_err());
    }

    #[test]
    fn slack_public_client_id_and_rotating_grants_are_validated() {
        assert!(valid_slack_client_id("123.456"));
        for invalid in ["", ".456", "123.", "123.456.7", "123.a", "123.456\n"] {
            assert!(!valid_slack_client_id(invalid));
        }
        let cfg = AuthConfig::slack_public("123.456", "http://localhost:47822/callback");
        let form = refresh_form(&cfg, "old-refresh");
        assert!(form.contains(&("client_id".into(), "123.456".into())));
        assert!(!form.iter().any(|(key, _)| key == "client_secret"));
        let valid = r#"{"ok":true,"token_type":"user","access_token":"new-access","expires_in":43200,"refresh_token":"new-refresh"}"#;
        let grant = parse_token_response_for(&cfg, valid, 1000, Some("old".into())).unwrap();
        assert_eq!(grant.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(grant.expires_at_ms, 43_201_000);
        let nested = r#"{"ok":true,"authed_user":{"access_token":"user-token","token_type":"user","expires_in":43200,"refresh_token":"replacement"}}"#;
        assert!(parse_token_response_for(&cfg, nested, 0, None).is_ok());
        for wrong_type in [
            r#"{"ok":true,"token_type":"bot","access_token":"bot-token"}"#,
            r#"{"ok":true,"token_type":null,"access_token":"user-token"}"#,
            r#"{"ok":true,"authed_user":{"access_token":"bot-token","token_type":"bot"}}"#,
        ] {
            assert!(parse_token_response_for(&cfg, wrong_type, 0, None).is_err());
        }
        for invalid in [
            r#"{"ok":true,"token_type":"bot","access_token":"bot-token"}"#,
            r#"{"ok":true,"access_token":"bad token"}"#,
            r#"{"ok":true,"access_token":"new","expires_in":0,"refresh_token":"r"}"#,
            r#"{"ok":true,"access_token":"new","expires_in":43200}"#,
            r#"{"ok":true,"access_token":"new","expires_in":43200,"refresh_token":""}"#,
            r#"{"ok":true,"access_token":"new","expires_in":9223372036854775807,"refresh_token":"r"}"#,
            r#"{"ok":true,"access_token":"new","refresh_token":"r"}"#,
        ] {
            assert!(parse_token_response_for(&cfg, invalid, 0, Some("spent".into())).is_err());
        }
    }

    fn cfg() -> AuthConfig {
        AuthConfig::google(
            "client-123.apps.googleusercontent.com",
            Some("secret".into()),
            "http://127.0.0.1:47821/callback",
        )
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier_and_deterministic() {
        let entropy = [7u8; 32];
        let a = Pkce::from_entropy(&entropy);
        let b = Pkce::from_entropy(&entropy);
        assert_eq!(a, b, "same entropy → same pair");
        // challenge == base64url(sha256(verifier))
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(a.verifier.as_bytes()));
        assert_eq!(a.challenge, expected);
        // 32-byte SHA-256 → 43-char base64url (no padding).
        assert_eq!(a.challenge.len(), 43);
        // verifier is URL-safe (no '+', '/', '=').
        assert!(!a.verifier.contains(['+', '/', '=']));
    }

    #[test]
    fn authorize_url_has_pkce_and_offline_and_state() {
        let pkce = Pkce::from_entropy(&[1u8; 32]);
        let scopes = [
            "https://www.googleapis.com/auth/gmail.readonly",
            "https://www.googleapis.com/auth/gmail.compose",
        ];
        let u = authorize_url(&cfg(), &scopes, &pkce, "nonce-xyz").unwrap();
        assert!(u.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains(&format!("code_challenge={}", pkce.challenge)));
        assert!(u.contains("access_type=offline"));
        assert!(u.contains("state=nonce-xyz"));
        // scopes are space-joined then percent-encoded (space → %20 or +).
        assert!(u.contains("gmail.readonly"));
        // the verifier must NEVER appear in the browser URL.
        assert!(!u.contains(&pkce.verifier));
    }

    #[test]
    fn slack_scopes_are_comma_separated() {
        let cfg = AuthConfig::slack("1.2", "s", "http://localhost:47822/callback");
        let u = authorize_url(
            &cfg,
            &["search:read.public", "chat:write"],
            &Pkce::from_entropy(&[3u8; 32]),
            "st",
        )
        .unwrap();
        assert!(
            u.contains("scope=search%3Aread.public%2Cchat%3Awrite"),
            "{u}"
        );
    }

    #[test]
    fn exchange_form_includes_verifier_and_secret() {
        let pkce = Pkce::from_entropy(&[2u8; 32]);
        let form = token_exchange_form(&cfg(), "auth-code", &pkce);
        assert!(form.contains(&("grant_type".into(), "authorization_code".into())));
        assert!(form.contains(&("code_verifier".into(), pkce.verifier.clone())));
        assert!(form.contains(&("client_secret".into(), "secret".into())));
    }

    #[test]
    fn refresh_form_omits_secret_when_none() {
        let mut c = cfg();
        c.client_secret = None;
        let form = refresh_form(&c, "refresh-abc");
        assert!(form.contains(&("grant_type".into(), "refresh_token".into())));
        assert!(!form.iter().any(|(k, _)| k == "client_secret"));
    }

    #[test]
    fn parse_redirect_extracts_code_and_state() {
        let (code, state) = parse_redirect("GET /callback?code=abc123&state=xyz HTTP/1.1").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_redirect_surfaces_user_denial_and_missing_params() {
        assert!(parse_redirect("GET /callback?error=access_denied HTTP/1.1").unwrap_err().contains("denied"));
        assert!(parse_redirect("GET /callback?code=only HTTP/1.1").is_err());
        assert!(parse_redirect("garbage").is_err());
    }

    #[test]
    fn parse_token_response_computes_absolute_expiry() {
        let body = r#"{"access_token":"at","expires_in":3600,"refresh_token":"rt","token_type":"Bearer"}"#;
        let ts = parse_token_response(body, 1_000, None).unwrap();
        assert_eq!(ts.access_token, "at");
        assert_eq!(ts.refresh_token.as_deref(), Some("rt"));
        assert_eq!(ts.expires_at_ms, 1_000 + 3_600_000); // = 3_601_000
        assert!(!ts.is_expired(1_000, 0));
        assert!(!ts.is_expired(3_600_999, 0));
        assert!(ts.is_expired(3_601_000, 0));
        // skew makes it "expired" early
        assert!(ts.is_expired(3_000_000, 700_000));
    }

    #[test]
    fn refresh_response_without_refresh_token_carries_prior_forward() {
        let body = r#"{"access_token":"new","expires_in":3600}"#;
        let ts = parse_token_response(body, 0, Some("old-refresh".into())).unwrap();
        assert_eq!(ts.refresh_token.as_deref(), Some("old-refresh"));
    }

    #[test]
    fn slack_nested_authed_user_token_is_parsed() {
        // Slack oauth.v2.user.access nests the user token under authed_user.
        let body = r#"{"ok":true,"app_id":"A1","authed_user":{"id":"U1","access_token":"xoxp-1","refresh_token":"xoxe-1","expires_in":43200}}"#;
        let ts = parse_token_response(body, 1_000, None).unwrap();
        assert_eq!(ts.access_token, "xoxp-1");
        assert_eq!(ts.refresh_token.as_deref(), Some("xoxe-1"));
        assert_eq!(ts.expires_at_ms, 1_000 + 43_200_000);
    }

    #[test]
    fn response_with_no_token_anywhere_is_an_error() {
        assert!(parse_token_response(r#"{"ok":false,"error":"invalid_code"}"#, 0, None).is_err());
        assert!(parse_token_response(r#"{"authed_user":{"id":"U1"}}"#, 0, None).is_err());
    }

    #[test]
    fn slack_config_uses_slack_endpoints_and_requires_secret() {
        let c = AuthConfig::slack("cid", "csec", "http://127.0.0.1:1/callback");
        assert_eq!(c.auth_endpoint, SLACK_AUTH_ENDPOINT);
        assert_eq!(c.token_endpoint, SLACK_TOKEN_ENDPOINT);
        assert_eq!(c.client_secret.as_deref(), Some("csec"));
    }
}
