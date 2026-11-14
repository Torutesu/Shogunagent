//! Official connector OAuth discovery and public-client registration. Metadata can never redirect
//! a credential-bearing request to another host. Tokens and client registration remain in Keychain.
use serde_json::{json, Value};
use shogun_integrations::AuthConfig;
use std::io::Read;

const ORIGIN: &str = "https://mcp.notion.com";
const MAX_METADATA_BYTES: u64 = 65_536;

fn valid_loopback_redirect(redirect_uri: &str) -> bool {
    let Ok(redirect) = reqwest::Url::parse(redirect_uri) else {
        return false;
    };
    redirect.scheme() == "http"
        && redirect.host_str() == Some("127.0.0.1")
        && redirect.port().is_some_and(|port| port != 0)
        && redirect.path() == "/callback"
        && redirect.username().is_empty()
        && redirect.password().is_none()
        && redirect.query().is_none()
        && redirect.fragment().is_none()
}

/// Accept a saved public client only for SHOGUN's exact loopback callback and the pinned
/// authorization server. This prevents a corrupted Keychain entry from changing OAuth endpoints.
fn validates_public_config(cfg: &AuthConfig, expected_redirect: &str, auth: &str, token: &str) -> bool {
    valid_loopback_redirect(expected_redirect)
        && cfg.redirect_uri == expected_redirect
        && cfg.auth_endpoint == auth
        && cfg.token_endpoint == token
        && !cfg.client_id.is_empty()
        && cfg.client_id.len() <= 8192
        && !cfg.client_id.chars().any(char::is_control)
        && cfg.client_secret.is_none()
}

pub fn validates_notion_config(cfg: &AuthConfig, expected_redirect: &str) -> bool {
    validates_public_config(cfg, expected_redirect, &format!("{ORIGIN}/authorize"), &format!("{ORIGIN}/token"))
}

pub fn validates_granola_config(cfg: &AuthConfig, expected_redirect: &str) -> bool {
    validates_public_config(
        cfg,
        expected_redirect,
        &format!("{GRANOLA_ISSUER}/oauth2/authorize"),
        &format!("{GRANOLA_ISSUER}/oauth2/token"),
    )
}

fn read_json(response: reqwest::blocking::Response) -> Result<Value, String> {
    if !response.status().is_success() {
        return Err("OAuth request was rejected".into());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "OAuth response could not be read")?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err("OAuth response exceeds limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "OAuth response is invalid".into())
}

fn validate_discovery(resource: &Value, metadata: &Value) -> Result<(), String> {
    if resource.get("resource").and_then(Value::as_str) != Some(ORIGIN)
        || resource
            .get("authorization_servers")
            .and_then(Value::as_array)
            .is_none_or(|servers| servers != &[json!(ORIGIN)])
        || metadata.get("issuer").and_then(Value::as_str) != Some(ORIGIN)
    {
        return Err("Notion OAuth issuer is unsupported".into());
    }
    for (key, path) in [
        ("authorization_endpoint", "/authorize"),
        ("token_endpoint", "/token"),
        ("registration_endpoint", "/register"),
    ] {
        if metadata.get(key).and_then(Value::as_str) != Some(format!("{ORIGIN}{path}").as_str()) {
            return Err("Notion OAuth endpoint is unsupported".into());
        }
    }
    if !metadata
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.contains(&json!("S256")))
        || !metadata
            .get("token_endpoint_auth_methods_supported")
            .and_then(Value::as_array)
            .is_some_and(|methods| methods.contains(&json!("none")))
    {
        return Err("Notion public-client PKCE is unavailable".into());
    }
    Ok(())
}

pub fn register_notion(redirect_uri: &str) -> Result<AuthConfig, String> {
    // Caller owns this loopback listener. No arbitrary redirect URL or remote metadata is accepted.
    if !valid_loopback_redirect(redirect_uri) {
        return Err("invalid OAuth redirect".into());
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| "OAuth client could not be initialized")?;
    let resource = read_json(
        client
            .get(format!("{ORIGIN}/.well-known/oauth-protected-resource"))
            .send()
            .map_err(|_| "Notion resource discovery failed")?,
    )?;
    let metadata = read_json(
        client
            .get(format!("{ORIGIN}/.well-known/oauth-authorization-server"))
            .send()
            .map_err(|_| "Notion OAuth discovery failed")?,
    )?;
    validate_discovery(&resource, &metadata)?;
    let registration = read_json(
        client
            .post(format!("{ORIGIN}/register"))
            .json(&json!({
                "client_name":"ShogunAI", "redirect_uris":[redirect_uri],
                "grant_types":["authorization_code","refresh_token"], "response_types":["code"],
                "token_endpoint_auth_method":"none", "scope":"default"
            }))
            .send()
            .map_err(|_| "Notion client registration failed")?,
    )?;
    let client_id = registration
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 8192 && !id.chars().any(char::is_control))
        .ok_or("Notion registration returned no client ID")?;
    Ok(AuthConfig {
        client_id: client_id.into(),
        client_secret: None,
        redirect_uri: redirect_uri.into(),
        auth_endpoint: format!("{ORIGIN}/authorize"),
        token_endpoint: format!("{ORIGIN}/token"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_rejects_foreign_issuers_and_credential_endpoints() {
        let resource = json!({"resource":ORIGIN,"authorization_servers":[ORIGIN]});
        let mut metadata = json!({"issuer":ORIGIN,"authorization_endpoint":format!("{ORIGIN}/authorize"),"token_endpoint":format!("{ORIGIN}/token"),"registration_endpoint":format!("{ORIGIN}/register"),"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]});
        assert!(validate_discovery(&resource, &metadata).is_ok());
        metadata["token_endpoint_auth_methods_supported"] = json!(["client_secret_post"]);
        assert!(validate_discovery(&resource, &metadata).is_err());
        metadata["token_endpoint_auth_methods_supported"] = json!(["none"]);
        metadata["token_endpoint"] = json!("https://attacker.invalid/token");
        assert!(validate_discovery(&resource, &metadata).is_err());
        assert!(validate_discovery(
            &json!({"resource":ORIGIN,"authorization_servers":["http://127.0.0.1"]}),
            &metadata
        )
        .is_err());
    }

    #[test]
    fn saved_notion_client_requires_exact_public_loopback_config() {
        let redirect = "http://127.0.0.1:47823/callback";
        let mut cfg = AuthConfig {
            client_id: "registered-client".into(),
            client_secret: None,
            redirect_uri: redirect.into(),
            auth_endpoint: format!("{ORIGIN}/authorize"),
            token_endpoint: format!("{ORIGIN}/token"),
        };
        assert!(validates_notion_config(&cfg, redirect));
        cfg.token_endpoint = "https://other.invalid/token".into();
        assert!(!validates_notion_config(&cfg, redirect));
        cfg.token_endpoint = format!("{ORIGIN}/token");
        cfg.client_id = "bad\nclient".into();
        assert!(!validates_notion_config(&cfg, redirect));
        cfg.client_id = "registered-client".into();
        for invalid in [
            "http://user@127.0.0.1:47823/callback",
            "http://127.0.0.1:47823/callback?next=https://other.invalid",
            "http://127.0.0.1:47823/callback#fragment",
            "http://127.0.0.1:0/callback",
            "http://localhost:47823/callback",
        ] {
            assert!(!validates_notion_config(&cfg, invalid));
        }
    }
}

const GRANOLA_RESOURCE: &str = "https://mcp.granola.ai/mcp";
const GRANOLA_ISSUER: &str = "https://mcp-auth.granola.ai";

fn validate_granola_discovery(resource: &Value, metadata: &Value) -> Result<(), String> {
    if resource.get("resource").and_then(Value::as_str) != Some(GRANOLA_RESOURCE)
        || resource
            .get("authorization_servers")
            .and_then(Value::as_array)
            .is_none_or(|servers| servers != &[json!(GRANOLA_ISSUER)])
        || metadata.get("issuer").and_then(Value::as_str) != Some(GRANOLA_ISSUER)
    {
        return Err("Granola OAuth issuer is unsupported".into());
    }
    for (key, path) in [
        ("authorization_endpoint", "/oauth2/authorize"),
        ("token_endpoint", "/oauth2/token"),
        ("registration_endpoint", "/oauth2/register"),
    ] {
        if metadata.get(key).and_then(Value::as_str)
            != Some(format!("{GRANOLA_ISSUER}{path}").as_str())
        {
            return Err("Granola OAuth endpoint is unsupported".into());
        }
    }
    if !metadata
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.contains(&json!("S256")))
        || !metadata
            .get("token_endpoint_auth_methods_supported")
            .and_then(Value::as_array)
            .is_some_and(|methods| methods.contains(&json!("none")))
    {
        return Err("Granola public-client PKCE is unavailable".into());
    }
    Ok(())
}

/// Register SHOGUN's loopback public client against Granola's pinned OAuth issuer.
pub fn register_granola(redirect_uri: &str) -> Result<AuthConfig, String> {
    if !valid_loopback_redirect(redirect_uri) {
        return Err("invalid OAuth redirect".into());
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| "OAuth client could not be initialized")?;
    let resource = read_json(
        client
            .get("https://mcp.granola.ai/.well-known/oauth-protected-resource")
            .send()
            .map_err(|_| "Granola resource discovery failed")?,
    )?;
    let metadata = read_json(
        client
            .get(format!(
                "{GRANOLA_ISSUER}/.well-known/oauth-authorization-server"
            ))
            .send()
            .map_err(|_| "Granola OAuth discovery failed")?,
    )?;
    validate_granola_discovery(&resource, &metadata)?;
    let registration = read_json(
        client
            .post(format!("{GRANOLA_ISSUER}/oauth2/register"))
            .json(
                &json!({"client_name":"ShogunAI", "redirect_uris":[redirect_uri],
            "grant_types":["authorization_code","refresh_token"], "response_types":["code"],
            "token_endpoint_auth_method":"none"}),
            )
            .send()
            .map_err(|_| "Granola client registration failed")?,
    )?;
    let client_id = registration
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 8192 && !id.chars().any(char::is_control))
        .ok_or("Granola registration returned no client ID")?;
    Ok(AuthConfig {
        client_id: client_id.into(),
        client_secret: None,
        redirect_uri: redirect_uri.into(),
        auth_endpoint: format!("{GRANOLA_ISSUER}/oauth2/authorize"),
        token_endpoint: format!("{GRANOLA_ISSUER}/oauth2/token"),
    })
}

#[cfg(test)]
mod granola_tests {
    use super::*;
    #[test]
    fn granola_metadata_cannot_redirect_credentials_or_disable_pkce() {
        let resource =
            json!({"resource":GRANOLA_RESOURCE,"authorization_servers":[GRANOLA_ISSUER]});
        let metadata = json!({"issuer":GRANOLA_ISSUER,
            "authorization_endpoint":format!("{GRANOLA_ISSUER}/oauth2/authorize"),
            "token_endpoint":format!("{GRANOLA_ISSUER}/oauth2/token"),
            "registration_endpoint":format!("{GRANOLA_ISSUER}/oauth2/register"),
            "code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]});
        assert!(validate_granola_discovery(&resource, &metadata).is_ok());
        for field in [
            "issuer",
            "token_endpoint",
            "registration_endpoint",
            "authorization_endpoint",
        ] {
            let mut malicious = metadata.clone();
            malicious[field] = json!("https://attacker.invalid/token");
            assert!(validate_granola_discovery(&resource, &malicious).is_err());
        }
        let mut no_pkce = metadata.clone();
        no_pkce["code_challenge_methods_supported"] = json!(["plain"]);
        assert!(validate_granola_discovery(&resource, &no_pkce).is_err());

        let redirect = "http://127.0.0.1:47824/callback";
        let mut saved = AuthConfig {
            client_id: "registered-client".into(),
            client_secret: None,
            redirect_uri: redirect.into(),
            auth_endpoint: format!("{GRANOLA_ISSUER}/oauth2/authorize"),
            token_endpoint: format!("{GRANOLA_ISSUER}/oauth2/token"),
        };
        assert!(validates_granola_config(&saved, redirect));
        assert!(!validates_notion_config(&saved, redirect));
        saved.client_id = "bad\nclient".into();
        assert!(!validates_granola_config(&saved, redirect));
    }
}
