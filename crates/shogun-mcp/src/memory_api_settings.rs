//! Memory API opt-in, profile, and bearer verifier storage.
//!
//! `memory_api.json` contains no secrets. Bearers live only in the macOS Keychain; this module
//! serializes SHA-256 verifiers so a process can authenticate without persisting plaintext tokens.
//!
//! Each Keychain record also carries the token's scope (issue #400): `read` or `full`. The scope
//! is not secret, but it sits on the record it governs, so there is no second file to drift from
//! the verifier. A record written before scopes existed has no `scope` and reads as `full`, which
//! is what it could do; a newly issued token defaults to `read`.
//!
//! A record may also name the app it was issued for (`client`, an [`McpClient::id`]) when it was
//! issued from the "Add to …" flow, so "Remove from …" can revoke it with the entry. It is
//! metadata only and optional: a record without it is an unlinked token, listed for revoking by
//! hand. Older builds ignore the field.
//!
//! The same Keychain item holds the per-app connections (issue #963, [`ClientGrant`]): which apps
//! the user added SHOGUN to, and with what scope. A connection is not a secret — the app's entry
//! names it (`shogun-mcp --client <id>`) and holds nothing else — but it lives next to the token
//! verifiers so that adding one, or widening one, takes the same Keychain write a token does, not
//! an edit to a plain file. Older builds ignore the field (and drop it when they rewrite the item,
//! which fails closed: the app is refused until it is added again).
//!
//! Each connection also says whether it may read meeting notes and screen recall
//! ([`ClientGrant::sensitive_sources`], owner decision 2026-10-20, audit F4): one the app made on
//! its own (automatic connection) starts without them, and only the user lets them in, in
//! Settings ([`set_sensitive_sources`]) or by widening the connection to `full`. An API face can
//! only take them away ([`withhold_sensitive_sources`]). The rule itself is
//! [`crate::source_policy`].
//!
//! [`McpClient::id`]: crate::client_config::McpClient::id

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub use crate::memory_api::TokenScope;
pub use crate::source_policy::SensitiveSources;

pub const TOKEN_VERIFIER_PREFIX: &str = "sha256:";
pub const TOKENS_KEYCHAIN_ACCOUNT: &str = "memory-api-tokens";
pub const SETTINGS_FILE: &str = "memory_api.json";
pub const SETTINGS_ENV: &str = "SHOGUN_MEMORY_API_SETTINGS";
/// Per-field cap, in characters rather than bytes: a byte cap gave Japanese a third of the room
/// English gets, while the settings form's `maxLength` (UTF-16 units) let the user type past it.
pub const MAX_PROFILE_FIELD_CHARS: usize = 4_096;
pub const MAX_TOKEN_NAME_BYTES: usize = 120;
pub const MAX_TOKENS: usize = 64;
/// Far more than the apps SHOGUN knows; a bound, so a hand-edited item cannot grow without limit.
pub const MAX_GRANTS: usize = 32;

/// Hash a bearer before persistence. The plaintext bearer must never be serialized.
pub fn token_verifier(token: &str) -> String {
    format!(
        "{TOKEN_VERIFIER_PREFIX}{:x}",
        Sha256::digest(token.as_bytes())
    )
}

fn valid_verifier(verifier: &str) -> bool {
    verifier
        .strip_prefix(TOKEN_VERIFIER_PREFIX)
        .is_some_and(|hex| hex.len() == 64 && hex.as_bytes().iter().all(u8::is_ascii_hexdigit))
}

/// Decode a persisted SHA-256 verifier. Never accepts a raw bearer.
pub fn persisted_verifier_bytes(verifier: &str) -> Result<[u8; 32], String> {
    let Some(hex) = verifier.strip_prefix(TOKEN_VERIFIER_PREFIX) else {
        return Err("invalid token verifier".into());
    };
    if hex.len() != 64 || !hex.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err("invalid token verifier".into());
    }
    let mut out = [0u8; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| "invalid token verifier".to_string())?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| "invalid token verifier".to_string())?;
        out[index] = ((high << 4) | low) as u8;
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub prefs: String,
}

/// Non-secret receipt for an automatic config edit; original bytes stay in the writer's backup.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AutoConnectNotice {
    pub client: String,
    pub before_digest: String,
    pub after_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// Missing or unreadable reads as off (the standalone faces fail closed). A new install gets
    /// `true` because the app writes the file on first run (owner decision 2026-10-14), not
    /// because of this default.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub profile: Profile,
    /// Connect newly found AI apps with `read` access, no click (owner decision 2026-10-14).
    #[serde(default = "default_true")]
    pub auto_connect: bool,
    /// Connection ids ([`grant_id`]) the user removed or disconnected: never re-added
    /// automatically. An explicit Add / Connect takes the id off again.
    #[serde(default)]
    pub auto_connect_declined: Vec<String>,
    /// Connection ids the app connected on its own, so Settings can say so.
    #[serde(default)]
    pub auto_connected: Vec<String>,
    #[serde(default)]
    pub auto_connect_notices: Vec<AutoConnectNotice>,
    #[serde(default)]
    pub auto_connect_conflicts: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: Profile::default(),
            auto_connect: true,
            auto_connect_declined: Vec::new(),
            auto_connected: Vec::new(),
            auto_connect_notices: Vec::new(),
            auto_connect_conflicts: Vec::new(),
        }
    }
}

/// The user removed or disconnected `client`: never connect it on its own again.
pub fn record_decline(settings: &mut Settings, client: &str) {
    let Some(id) = grant_id(client) else { return };
    settings.auto_connected.retain(|c| c != id);
    settings.auto_connect_notices.retain(|n| n.client != id);
    settings.auto_connect_conflicts.retain(|c| c != id);
    if !settings.auto_connect_declined.iter().any(|c| c == id) {
        settings.auto_connect_declined.push(id.to_string());
    }
}

/// The user added or connected `client` themselves: it is theirs, not declined or automatic.
pub fn clear_decline(settings: &mut Settings, client: &str) {
    let Some(id) = grant_id(client) else { return };
    settings.auto_connect_declined.retain(|c| c != id);
    settings.auto_connected.retain(|c| c != id);
    settings.auto_connect_notices.retain(|n| n.client != id);
    settings.auto_connect_conflicts.retain(|c| c != id);
}

/// A Keychain token record. `secret` exists only while migrating a legacy blob or immediately
/// after minting; custom serialization guarantees it never reaches Keychain/disk output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub secret: String,
    pub verifier: String,
    /// What the token may do. Missing on a pre-scope record, which reads as
    /// [`TokenScope::LEGACY`] (`full`).
    pub scope: TokenScope,
    /// The app it was issued for (see the module docs). `None`: an unlinked token.
    pub client: Option<String>,
}

/// Longest client id a record may carry. The known ids are far shorter; a newer build's id that
/// this build does not know is kept as it is (it just never matches a "Remove from …").
pub const MAX_CLIENT_ID_BYTES: usize = 64;

fn valid_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CLIENT_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl serde::Serialize for IssuedToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("IssuedToken", 6)?;
        value.serialize_field("id", &self.id)?;
        value.serialize_field("name", &self.name)?;
        value.serialize_field("created_at", &self.created_at)?;
        value.serialize_field("verifier", &self.verifier)?;
        value.serialize_field("scope", &self.scope)?;
        // Written only when set, so an unlinked record serializes without it.
        if let Some(client) = &self.client {
            value.serialize_field("client", client)?;
        } else {
            value.skip_field("client")?;
        }
        value.end()
    }
}

impl<'de> serde::Deserialize<'de> for IssuedToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Wire {
            id: String,
            name: String,
            created_at: i64,
            verifier: Option<String>,
            secret: Option<String>,
            // Absent on records written before issue #400; an unknown name fails the parse, so
            // the whole blob fails closed rather than guessing a scope.
            scope: Option<TokenScope>,
            // Absent on an unlinked record. A malformed id fails closed like the rest of the
            // metadata; only this app's Settings writes the field.
            client: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.id.is_empty()
            || wire.id.len() > 128
            || wire.name.trim().is_empty()
            || wire.name.len() > MAX_TOKEN_NAME_BYTES
            || wire.client.as_deref().is_some_and(|client| !valid_client_id(client))
        {
            return Err(serde::de::Error::custom("invalid token metadata"));
        }
        let (secret, verifier) = match (wire.verifier, wire.secret) {
            (Some(verifier), None) if valid_verifier(&verifier) => (String::new(), verifier),
            (None, Some(secret)) if !secret.is_empty() && secret.len() <= 512 => {
                let verifier = token_verifier(&secret);
                (secret, verifier)
            }
            _ => return Err(serde::de::Error::custom("invalid token verifier")),
        };
        Ok(Self {
            id: wire.id,
            name: wire.name,
            created_at: wire.created_at,
            secret,
            verifier,
            scope: wire.scope.unwrap_or(TokenScope::LEGACY),
            client: wire.client,
        })
    }
}

/// One app the user connected SHOGUN to (issue #963): the app's stdio entry runs
/// `shogun-mcp --client <client>`, and the session runs under `scope`. No secret: whether the app
/// may connect is this record being on the Keychain list, re-checked on every call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ClientGrant {
    /// The connection id, [`grant_id`] of the app: apps that read one config file share one.
    pub client: String,
    pub scope: TokenScope,
    pub created_at: i64,
    /// Whether the app may read meeting notes and screen recall (owner decision 2026-10-20):
    /// `withheld` for a connection the app made on its own, until the user allows them.
    pub sensitive_sources: SensitiveSources,
}

/// When automatic connection shipped (2026-10-14T00:00:00Z, in ms; the change landed that day).
/// A connection stored before the marker existed and created from then on may have been made
/// without a click, so it reads as withheld unless it is `full` ([`legacy_sensitive_sources`]).
pub const AUTO_CONNECT_SINCE_MS: i64 = 1_791_936_000_000;

/// The marker for a connection stored without one (every connection written before 2026-10-20,
/// or rewritten by an older build, which drops fields it does not know). Safety first: nothing
/// stored says whether a `read` connection made since automatic connection shipped came from a
/// click, so it reads as withheld; the user allows it again with one switch. Before that day only
/// a click could make a connection, and `full` has only ever been the user's own choice, so those
/// keep every source.
pub fn legacy_sensitive_sources(scope: TokenScope, created_at: i64) -> SensitiveSources {
    if scope == TokenScope::Read && created_at >= AUTO_CONNECT_SINCE_MS {
        SensitiveSources::Withheld
    } else {
        SensitiveSources::Allowed
    }
}

impl<'de> serde::Deserialize<'de> for ClientGrant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Wire {
            client: String,
            // Required: a connection never existed without a scope, so a missing one is a broken
            // record, not an old one. An unknown name fails the blob closed like a token's.
            scope: TokenScope,
            created_at: i64,
            // Absent on a connection stored before 2026-10-20 (or by an older build): read by
            // [`legacy_sensitive_sources`]. An unknown name fails the blob closed.
            sensitive_sources: Option<SensitiveSources>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if !valid_client_id(&wire.client) {
            return Err(serde::de::Error::custom("invalid connection"));
        }
        let sensitive_sources = wire
            .sensitive_sources
            .unwrap_or_else(|| legacy_sensitive_sources(wire.scope, wire.created_at));
        Ok(Self {
            client: wire.client,
            scope: wire.scope,
            created_at: wire.created_at,
            sensitive_sources,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenBlob {
    #[serde(default)]
    pub tokens: Vec<IssuedToken>,
    /// Per-app connections (issue #963). Written only when there is one, so a token-only item
    /// keeps the exact shape older builds wrote.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<ClientGrant>,
}

impl TokenBlob {
    /// Nothing issued and nothing connected: the Keychain item can go.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.grants.is_empty()
    }
}

pub fn resolve_settings_path(db_path: &str) -> PathBuf {
    // Debug-only, like every other env override in the tree (audit 2026-08-13 P0-3): in a
    // release binary an environment variable must not be able to point the REST/MCP faces at a
    // foreign settings file that enables the API and swaps the token verifiers.
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var(SETTINGS_ENV) {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    Path::new(db_path)
        .parent()
        .map(|parent| parent.join(SETTINGS_FILE))
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE))
}

/// How `memory_api.json` is checked against its seal (audit 2026-10-20 M6). The sealing code
/// lives in `shogun-core` (`settings_seal`), which depends on this crate, so it installs itself
/// here. Without a hook (tests, a process without the memory key) the file is read as it is.
pub trait SettingsSealHook: Send + Sync {
    /// The settings in force for what the file says: a widening the seal does not cover (the
    /// API or auto-connect switched on, a declined app dropped) is not followed.
    fn trusted(&self, path: &Path, file: Settings) -> Settings;
    /// Seal settings just written to `path`.
    fn seal(&self, path: &Path, settings: &Settings);
}

static SEAL_HOOK: std::sync::OnceLock<Box<dyn SettingsSealHook>> = std::sync::OnceLock::new();

/// Install the seal for this process. `false` when one is already installed.
pub fn install_seal_hook(hook: Box<dyn SettingsSealHook>) -> bool {
    SEAL_HOOK.set(hook).is_ok()
}

/// Unreadable/missing settings are disabled: external API remains fail-closed.
pub fn load_settings(path: &Path) -> Settings {
    try_load_settings(path).unwrap_or_default()
}

/// Missing settings are a first run; unreadable or malformed settings must not be overwritten
/// by an editor using empty defaults. What it returns is checked against the file's seal (M6).
pub fn try_load_settings(path: &Path) -> Result<Settings, String> {
    let file = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|_| "Couldn't read your memory settings. Try again.".to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(_) => return Err("Couldn't read your memory settings. Try again.".to_string()),
    };
    Ok(match SEAL_HOOK.get() {
        Some(hook) => hook.trusted(path, file),
        None => file,
    })
}

pub fn save_settings(path: &Path, settings: &Settings) -> Result<(), String> {
    validate_profile(&settings.profile)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create memory API settings directory: {error}"))?;
    }
    let json = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(SETTINGS_FILE);
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temp, json)
        .map_err(|error| format!("save memory API settings temp file: {error}"))?;
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("replace memory API settings: {error}")
    })?;
    // The app's write (or an API write the settings allow): seal it (M6).
    if let Some(hook) = SEAL_HOOK.get() {
        hook.seal(path, settings);
    }
    Ok(())
}

/// Owner decision 2026-10-14: a Mac with no `memory_api.json` gets the Memory API on, written
/// out so the standalone faces ([`require_enabled`]) agree. A file that exists, `enabled: false`
/// or unreadable, is left alone. Returns whether it wrote the file.
pub fn enable_on_first_run(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A missing file is a first run only when no seal says otherwise (audit 2026-10-20
            // M6): deleting a file the user had turned off must not turn the API on.
            let wanted = Settings { enabled: true, ..Settings::default() };
            let next = match SEAL_HOOK.get() {
                Some(hook) => hook.trusted(path, wanted),
                None => wanted,
            };
            save_settings(path, &next)?;
            Ok(next.enabled)
        }
        _ => Ok(false),
    }
}

pub fn require_enabled(path: &Path) -> Result<Settings, String> {
    if !path.exists() {
        return Err("Memory API is disabled. Enable it in ShogunAI Settings.".into());
    }
    let settings = load_settings(path);
    if !settings.enabled {
        return Err("Memory API is disabled. Enable it in ShogunAI Settings.".into());
    }
    Ok(settings)
}

pub fn validate_profile(profile: &Profile) -> Result<(), String> {
    let fields = [
        (&profile.display_name, false),
        (&profile.role, false),
        // Preferences are free text from a textarea: line breaks and tabs are content there.
        (&profile.prefs, true),
    ];
    for (value, multiline) in fields {
        if value.chars().count() > MAX_PROFILE_FIELD_CHARS {
            return Err("profile field exceeds 4096 characters".into());
        }
        if value.chars().any(|c| c.is_control() && !(multiline && (c == '\n' || c == '\t'))) {
            return Err("profile field contains control characters".into());
        }
    }
    Ok(())
}

/// The stored form of the preferences text: trimmed, with CR and CRLF line breaks folded to LF so
/// every surface (desktop form, MCP, REST) saves the same text for the same input.
pub fn normalize_prefs(prefs: &str) -> String {
    prefs.replace("\r\n", "\n").replace('\r', "\n").trim().to_string()
}

pub fn parse_token_blob_with_migration(bytes: &[u8]) -> Result<(TokenBlob, bool), String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "invalid token blob".to_string())?;
    let Some(tokens) = value.get("tokens").and_then(serde_json::Value::as_array) else {
        return Err("invalid token blob".into());
    };
    if tokens.len() > MAX_TOKENS {
        return Err("too many token records".into());
    }
    let migrated = tokens
        .iter()
        .any(|token| token.get("secret").is_some() && token.get("verifier").is_none());
    let blob: TokenBlob =
        serde_json::from_value(value).map_err(|_| "invalid token blob".to_string())?;
    check_grants(&blob.grants)?;
    Ok((blob, migrated))
}

/// A bounded list with one connection per id: two records for one app would leave which scope
/// applies to the order they were read in.
fn check_grants(grants: &[ClientGrant]) -> Result<(), String> {
    if grants.len() > MAX_GRANTS {
        return Err("too many connections".into());
    }
    for (index, grant) in grants.iter().enumerate() {
        if grants[..index].iter().any(|earlier| earlier.client == grant.client) {
            return Err("invalid token blob".into());
        }
    }
    Ok(())
}

pub fn serialize_token_blob(blob: &TokenBlob) -> Result<Vec<u8>, String> {
    if blob.tokens.len() > MAX_TOKENS {
        return Err("too many token records".into());
    }
    check_grants(&blob.grants)?;
    serde_json::to_vec(blob).map_err(|error| error.to_string())
}

/// The connection id an app's entry carries (`--client <id>`, issue #963): the app's own id,
/// except that apps reading one config file share the first one's (Codex CLI and the ChatGPT
/// desktop app share `~/.codex/config.toml`, so one entry, and one connection, serves both).
/// `None` for an id SHOGUN does not know.
pub fn grant_id(client: &str) -> Option<&'static str> {
    removal_clients(client).first().copied()
}

/// One connection as Settings and `api.tokens.list` show it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GrantListing {
    /// The connection id ([`grant_id`]).
    pub client: String,
    /// The apps it serves, by name ("Codex CLI, ChatGPT desktop").
    pub label: String,
    pub scope: TokenScope,
    pub created_at: i64,
    /// Who can use it (issue #979): any program running as the user, or only the app itself,
    /// checked by its code signature when `shogun-mcp` starts
    /// ([`crate::launcher_check::policy_for`]).
    pub launcher: crate::launcher_check::LauncherCheck,
    /// Whether it may read meeting notes and screen recall (owner decision 2026-10-20).
    pub sensitive_sources: SensitiveSources,
}

/// The name a connection is shown under: every known app that shares it, or the id itself for an
/// id a newer build wrote.
pub fn grant_label(client: &str) -> String {
    let labels: Vec<&str> = removal_clients(client)
        .into_iter()
        .filter_map(crate::client_install::client_by_id)
        .map(crate::client_config::McpClient::label)
        .collect();
    if labels.is_empty() {
        client.to_string()
    } else {
        labels.join(", ")
    }
}

/// Every connection, in the order they were made.
pub fn grant_listing(blob: &TokenBlob) -> Vec<GrantListing> {
    blob.grants
        .iter()
        .map(|grant| GrantListing {
            client: grant.client.clone(),
            label: grant_label(&grant.client),
            scope: grant.scope,
            created_at: grant.created_at,
            launcher: crate::launcher_check::policy_for(&grant.client).check(),
            sensitive_sources: grant.sensitive_sources,
        })
        .collect()
}

/// Who is making a connection (owner decision 2026-10-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantOrigin {
    /// The user, in Settings: "Add to …", Connect, or changing a connection's access.
    User,
    /// The app on its own (automatic connection, owner decision 2026-10-14): `read`, without
    /// meeting notes and screen recall.
    Automatic,
}

fn listing_for(blob: &TokenBlob, id: &str) -> Result<GrantListing, String> {
    grant_listing(blob)
        .into_iter()
        .find(|listing| listing.client == id)
        .ok_or_else(|| "unknown app".to_string())
}

/// "Add to <app>" (issue #963): connect `client`'s connection with `scope`, or change the scope
/// of the one already there (the entry is the same either way). Only Settings and the app's own
/// automatic connection call this: no API face can make a connection or widen one.
///
/// Meeting notes and screen recall (owner decision 2026-10-20): a new connection the user makes
/// has them; one the app makes on its own does not (and is `read` whatever `scope` says). Changing
/// an existing connection keeps its setting, except that the user widening it to `full` allows
/// them too. The app never changes a connection that is already there.
pub fn grant_into(
    blob: &mut TokenBlob,
    client: &str,
    scope: TokenScope,
    created_at: i64,
    origin: GrantOrigin,
) -> Result<GrantListing, String> {
    let id = grant_id(client).ok_or("unknown app")?;
    if let Some(grant) = blob.grants.iter_mut().find(|grant| grant.client == id) {
        if origin == GrantOrigin::User {
            grant.scope = scope;
            if scope == TokenScope::Full {
                grant.sensitive_sources = SensitiveSources::Allowed;
            }
        }
    } else {
        if blob.grants.len() >= MAX_GRANTS {
            return Err("too many connections".into());
        }
        let (scope, sensitive_sources) = match origin {
            GrantOrigin::User => (scope, SensitiveSources::Allowed),
            GrantOrigin::Automatic => (TokenScope::Read, SensitiveSources::Withheld),
        };
        blob.grants.push(ClientGrant {
            client: id.to_string(),
            scope,
            created_at,
            sensitive_sources,
        });
    }
    listing_for(blob, id)
}

/// Settings' "Include meetings and screen recall" switch for one connection (owner decision
/// 2026-10-20): either way, in the app only, like widening a connection to `full`.
pub fn set_sensitive_sources(
    blob: &mut TokenBlob,
    client: &str,
    access: SensitiveSources,
) -> Result<GrantListing, String> {
    let id = grant_id(client).map_or_else(|| client.to_string(), str::to_string);
    let grant = blob
        .grants
        .iter_mut()
        .find(|grant| grant.client == id)
        .ok_or("connection not found")?;
    grant.sensitive_sources = access;
    listing_for(blob, &id)
}

/// `api.connections.restrict`: withhold meeting notes and screen recall from one connection. A
/// tightening, so every face may do it; there is no API way back (the user's switch in Settings).
/// Returns whether anything changed.
pub fn withhold_sensitive_sources(blob: &mut TokenBlob, client: &str) -> Result<bool, String> {
    let id = grant_id(client).map_or_else(|| client.to_string(), str::to_string);
    let grant = blob
        .grants
        .iter_mut()
        .find(|grant| grant.client == id)
        .ok_or("connection not found")?;
    let changed = !grant.sensitive_sources.is_withheld();
    grant.sensitive_sources = SensitiveSources::Withheld;
    Ok(changed)
}

/// `api.connections.restrict` against the Keychain item (`read` / `write` are its accessors, as
/// in [`load_token_blob_with_migration`]): read the list, withhold, write it back only when
/// something changed. The caller bumps the revision marker so running faces see it.
pub fn withhold_in_store<Read, Write>(read: Read, write: Write, client: &str) -> Result<bool, String>
where
    Read: FnOnce() -> Result<Option<Vec<u8>>, String>,
    Write: FnOnce(&[u8]) -> Result<(), String>,
{
    let Some(bytes) = read()? else {
        return Err("connection not found".into());
    };
    let (mut blob, _) = parse_token_blob_with_migration(&bytes)?;
    let changed = withhold_sensitive_sources(&mut blob, client)?;
    if changed {
        write(&serialize_token_blob(&blob)?)?;
    }
    Ok(changed)
}

/// Disconnect `client` (its connection id, or any app sharing it): the app's entry stops working
/// from its next call. A tightening, so it is direct everywhere. Returns whether one was there.
pub fn revoke_grant(blob: &mut TokenBlob, client: &str) -> bool {
    let id = grant_id(client).map_or_else(|| client.to_string(), str::to_string);
    let before = blob.grants.len();
    blob.grants.retain(|grant| grant.client != id);
    before != blob.grants.len()
}

/// Settings' Issue: validate the client name and the token limit, then add a record holding only
/// the new token's verifier and `scope`. The caller mints `secret`, shows it to the user once,
/// and saves the blob to the Keychain. Scope is required here, so no issuer can forget it; the
/// Settings default is [`TokenScope::DEFAULT_FOR_NEW`] (`read`).
pub fn issue_into(
    blob: &mut TokenBlob,
    id: String,
    name: &str,
    created_at: i64,
    secret: &str,
    scope: TokenScope,
) -> Result<crate::memory_api::TokenListing, String> {
    issue_for_client(blob, id, name, created_at, secret, scope, None)
}

/// [`issue_into`] for a token issued from the "Add to <client>" flow: the record also names the
/// client (a known [`crate::client_config::McpClient`] id), so removing SHOGUN from that app can
/// revoke it ([`linked_to_removal`] / [`revoke_linked`]). Only Settings calls this.
pub fn issue_for_client(
    blob: &mut TokenBlob,
    id: String,
    name: &str,
    created_at: i64,
    secret: &str,
    scope: TokenScope,
    client: Option<&str>,
) -> Result<crate::memory_api::TokenListing, String> {
    if let Some(client) = client {
        if crate::client_install::client_by_id(client).is_none() {
            return Err("unknown app".into());
        }
    }
    let client = client.map(str::to_string);
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_TOKEN_NAME_BYTES || name.chars().any(char::is_control)
    {
        return Err("token name must be 1-120 printable bytes".into());
    }
    if blob.tokens.len() >= MAX_TOKENS {
        return Err("token limit reached".into());
    }
    if id.is_empty() || id.len() > 128 || blob.tokens.iter().any(|token| token.id == id) {
        return Err("invalid token id".into());
    }
    if secret.is_empty() || secret.len() > 512 {
        return Err("invalid token".into());
    }
    let listing = crate::memory_api::TokenListing {
        id: id.clone(),
        name: name.to_string(),
        created_at,
        scope,
        client: client.clone(),
    };
    blob.tokens.push(IssuedToken {
        id,
        name: name.to_string(),
        created_at,
        secret: String::new(),
        verifier: token_verifier(secret),
        scope,
        client,
    });
    Ok(listing)
}

/// Settings' Revoke: drop one record by id. A tightening, so it is direct everywhere.
pub fn revoke_from(blob: &mut TokenBlob, id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 {
        return Err("invalid token id".into());
    }
    let before = blob.tokens.len();
    blob.tokens.retain(|token| token.id != id);
    if before == blob.tokens.len() {
        return Err("token not found".into());
    }
    Ok(())
}

/// What Settings lists for each token: metadata and scope, never the verifier.
pub fn listing(blob: &TokenBlob) -> Vec<crate::memory_api::TokenListing> {
    blob.tokens
        .iter()
        .map(|token| crate::memory_api::TokenListing {
            id: token.id.clone(),
            name: token.name.clone(),
            created_at: token.created_at,
            scope: token.scope,
            client: token.client.clone(),
        })
        .collect()
}

/// The apps whose SHOGUN entry "Remove from `client`" takes away: `client` itself, plus any app
/// that reads the same config file (Codex CLI and the ChatGPT desktop app share
/// `~/.codex/config.toml`, so removing one removes the other's entry too). Ids, in
/// [`crate::client_config::ALL_CLIENTS`] order; empty for an unknown id.
pub fn removal_clients(client: &str) -> Vec<&'static str> {
    use crate::client_config::ALL_CLIENTS;
    use crate::client_install::{client_by_id, config_file};
    let Some(chosen) = client_by_id(client) else {
        return Vec::new();
    };
    // Any fixed home works: only whether two clients share a file matters.
    let home = Path::new("/");
    let file = config_file(chosen, home);
    ALL_CLIENTS
        .iter()
        .copied()
        .filter(|&other| other == chosen || (file.is_some() && config_file(other, home) == file))
        .map(|other| other.id())
        .collect()
}

/// The tokens "Remove from `client`" offers to revoke: those issued for an app in
/// [`removal_clients`]. Metadata only.
pub fn linked_to_removal(blob: &TokenBlob, client: &str) -> Vec<crate::memory_api::TokenListing> {
    let clients = removal_clients(client);
    listing(blob)
        .into_iter()
        .filter(|token| token.client.as_deref().is_some_and(|id| clients.contains(&id)))
        .collect()
}

/// The tokens no app is recorded for, which the Remove dialog lists for revoking by hand.
pub fn unlinked(blob: &TokenBlob) -> Vec<crate::memory_api::TokenListing> {
    listing(blob).into_iter().filter(|token| token.client.is_none()).collect()
}

/// "Remove from `client`" with "also revoke" checked: drop every token [`linked_to_removal`]
/// names. Returns the ids dropped (empty when none was linked). A tightening, like Revoke.
pub fn revoke_linked(blob: &mut TokenBlob, client: &str) -> Vec<String> {
    let ids: Vec<String> = linked_to_removal(blob, client).into_iter().map(|t| t.id).collect();
    blob.tokens.retain(|token| !ids.contains(&token.id));
    ids
}

/// `errSecItemNotFound`: the Keychain has no token-list item at all.
pub const KEYCHAIN_ITEM_NOT_FOUND: i32 = -25300;

/// What a standalone face makes of its Keychain read of the token list (issue #744), given the
/// bytes or the `OSStatus`. Only "no such item" means no token has been issued (an empty list,
/// which admits no stdio process either since audit 2026-10-20 M5);
/// every other failure (the user clicked Deny, the Keychain is locked, access was refused) makes
/// the list unavailable, which refuses every caller. The reason carries the status, never data.
pub fn token_blob_read(result: Result<Vec<u8>, i32>) -> Result<Option<Vec<u8>>, String> {
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(KEYCHAIN_ITEM_NOT_FOUND) => Ok(None),
        Err(status) => Err(format!("Keychain status {status}")),
    }
}

pub fn load_token_blob_with_migration<Read, Write>(
    read: Read,
    write: Write,
) -> Result<TokenBlob, String>
where
    Read: FnOnce() -> Result<Option<Vec<u8>>, String>,
    Write: FnOnce(&[u8]) -> Result<(), String>,
{
    let Some(bytes) = read()? else {
        return Ok(TokenBlob::default());
    };
    let (blob, migrated) = parse_token_blob_with_migration(&bytes)?;
    if migrated {
        let rewritten = serialize_token_blob(&blob)?;
        write(&rewritten)?;
    }
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_item_means_no_token_was_issued() {
        // Issue #744: a denied, locked or refused read must never read as "nothing issued yet",
        // which the faces once answered with process trust (full scope; removed by M5).
        assert_eq!(token_blob_read(Ok(b"{}".to_vec())), Ok(Some(b"{}".to_vec())));
        assert_eq!(token_blob_read(Err(KEYCHAIN_ITEM_NOT_FOUND)), Ok(None));
        // errSecUserCanceled, errSecInteractionNotAllowed, errSecAuthFailed, anything else.
        for status in [-128, -25308, -25293, -34018, 1] {
            let got = token_blob_read(Err(status));
            assert_eq!(got, Err(format!("Keychain status {status}")), "{status}");
        }
    }

    #[test]
    fn settings_editor_distinguishes_missing_from_failed_reads() -> Result<(), Box<dyn std::error::Error>> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("shogun-settings-{}-{stamp}.json", std::process::id()));
        assert_eq!(try_load_settings(&path)?, Settings::default());
        std::fs::write(&path, b"not valid JSON")?;
        let result = try_load_settings(&path);
        let disabled = !load_settings(&path).enabled;
        std::fs::remove_file(&path)?;
        assert!(result.is_err());
        assert!(disabled);
        assert!(try_load_settings(&std::env::temp_dir()).is_err());
        Ok(())
    }

    #[test]
    fn first_run_turns_the_api_on_but_never_overrides_a_saved_choice() -> Result<(), Box<dyn std::error::Error>> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
        let dir = std::env::temp_dir().join(format!("shogun-first-run-{}-{stamp}", std::process::id()));
        let path = dir.join(SETTINGS_FILE);
        assert!(enable_on_first_run(&path)?);
        assert!(require_enabled(&path)?.auto_connect);
        let mut off = load_settings(&path);
        off.enabled = false;
        save_settings(&path, &off)?;
        assert!(!enable_on_first_run(&path)?);
        assert!(!load_settings(&path).enabled);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn default_is_disabled_and_bearer_never_serializes() {
        assert!(!Settings::default().enabled);
        let raw = "shogun_secret";
        let bytes = serialize_token_blob(&TokenBlob {
            tokens: vec![IssuedToken {
                id: "id".into(),
                name: "client".into(),
                created_at: 1,
                secret: raw.into(),
                verifier: token_verifier(raw),
                scope: TokenScope::Read,
                client: None,
            }],
            grants: Vec::new(),
        })
        .unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains(raw));
    }

    #[test]
    fn legacy_plaintext_migrates_and_invalid_data_fails_closed() {
        let raw =
            br#"{"tokens":[{"id":"id","name":"client","created_at":1,"secret":"old-secret"}]}"#;
        let (blob, migrated) = parse_token_blob_with_migration(raw).unwrap();
        assert!(migrated);
        assert_eq!(blob.tokens[0].verifier, token_verifier("old-secret"));
        assert!(parse_token_blob_with_migration(b"{}").is_err());
    }

    #[test]
    fn pre_scope_records_read_as_full_and_stay_full_on_rewrite() {
        // Issue #400: a record issued before scopes existed has no `scope`. It could call every
        // tool, so it keeps doing so, and re-saving the blob writes that down.
        let raw = format!(
            r#"{{"tokens":[{{"id":"old","name":"client","created_at":1,"verifier":"{}"}}]}}"#,
            token_verifier("s")
        );
        let (blob, migrated) = parse_token_blob_with_migration(raw.as_bytes()).unwrap();
        assert!(!migrated);
        assert_eq!(blob.tokens[0].scope, TokenScope::Full);
        let rewritten = String::from_utf8(serialize_token_blob(&blob).unwrap()).unwrap();
        assert!(rewritten.contains(r#""scope":"full""#), "{rewritten}");
        // A legacy plaintext record migrates to a verifier and to `full` in the same pass.
        let legacy = br#"{"tokens":[{"id":"id","name":"client","created_at":1,"secret":"old-secret"}]}"#;
        let (blob, migrated) = parse_token_blob_with_migration(legacy).unwrap();
        assert!(migrated);
        assert_eq!(blob.tokens[0].scope, TokenScope::Full);
    }

    #[test]
    fn scope_round_trips_and_an_unknown_scope_fails_closed() {
        let mut blob = TokenBlob::default();
        issue_into(&mut blob, "a".into(), "Claude Code", 5, "shogun_a", TokenScope::Read).unwrap();
        issue_into(&mut blob, "b".into(), "Cursor", 6, "shogun_b", TokenScope::Full).unwrap();
        let bytes = serialize_token_blob(&blob).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("shogun_a") && !text.contains("shogun_b"), "{text}");
        let (back, _) = parse_token_blob_with_migration(&bytes).unwrap();
        assert_eq!(
            back.tokens.iter().map(|t| t.scope).collect::<Vec<_>>(),
            vec![TokenScope::Read, TokenScope::Full]
        );
        // A scope this build does not know (a newer build's finer scope, or a hand edit) fails
        // the whole blob closed instead of being read as either scope.
        let unknown = text.replacen(r#""scope":"read""#, r#""scope":"admin""#, 1);
        assert!(parse_token_blob_with_migration(unknown.as_bytes()).is_err());
    }

    #[test]
    fn issue_and_revoke_validate_and_listing_never_carries_a_verifier() {
        let mut blob = TokenBlob::default();
        let listed = issue_into(&mut blob, "a".into(), "  Claude Code ", 5, "shogun_a", TokenScope::Read).unwrap();
        assert_eq!(listed.name, "Claude Code");
        assert!(issue_into(&mut blob, "a".into(), "dup", 5, "shogun_c", TokenScope::Read).is_err());
        assert!(issue_into(&mut blob, "c".into(), " ", 5, "shogun_c", TokenScope::Read).is_err());
        assert!(issue_into(&mut blob, "c".into(), "x", 5, "", TokenScope::Read).is_err());
        let json = serde_json::to_string(&listing(&blob)).unwrap();
        assert!(!json.contains("sha256:") && !json.contains("shogun_a"), "{json}");
        assert!(json.contains(r#""scope":"read""#), "{json}");
        assert!(revoke_from(&mut blob, "missing").is_err());
        revoke_from(&mut blob, "a").unwrap();
        assert!(blob.tokens.is_empty());
    }

    #[test]
    fn a_token_issued_for_an_app_records_it_and_old_records_stay_unlinked() {
        let mut blob = TokenBlob::default();
        let listed = issue_for_client(&mut blob, "a".into(), "Cursor", 1, "shogun_a", TokenScope::Read, Some("cursor")).unwrap();
        assert_eq!(listed.client.as_deref(), Some("cursor"));
        issue_into(&mut blob, "b".into(), "by hand", 2, "shogun_b", TokenScope::Full).unwrap();
        // Only a known app can be recorded.
        assert!(issue_for_client(&mut blob, "c".into(), "x", 3, "shogun_c", TokenScope::Read, Some("other-app")).is_err());
        let bytes = serialize_token_blob(&blob).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains(r#""client":"cursor""#), "{text}");
        assert_eq!(text.matches(r#""client""#).count(), 1, "an unlinked record writes no client: {text}");
        assert!(!text.contains("shogun_a") && !text.contains("shogun_b"), "{text}");
        let (back, _) = parse_token_blob_with_migration(&bytes).unwrap();
        assert_eq!(back, blob);
        // A record from before this field (and from before scopes) reads as unlinked.
        let legacy = format!(
            r#"{{"tokens":[{{"id":"old","name":"client","created_at":1,"verifier":"{}","scope":"read"}}]}}"#,
            token_verifier("s")
        );
        let (old, _) = parse_token_blob_with_migration(legacy.as_bytes()).unwrap();
        assert_eq!(old.tokens[0].client, None);
        // A newer build's app id is kept as it is; a malformed one fails the blob closed.
        let newer = text.replacen(r#""client":"cursor""#, r#""client":"some-new-app""#, 1);
        let (newer, _) = parse_token_blob_with_migration(newer.as_bytes()).unwrap();
        assert_eq!(newer.tokens[0].client.as_deref(), Some("some-new-app"));
        for bad in [r#""client":"""#, r#""client":"Cursor App""#, r#""client":7"#] {
            let broken = text.replacen(r#""client":"cursor""#, bad, 1);
            assert!(parse_token_blob_with_migration(broken.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn remove_revokes_the_apps_tokens_including_an_app_sharing_its_file() {
        assert_eq!(removal_clients("cursor"), vec!["cursor"]);
        assert_eq!(removal_clients("claude-code"), vec!["claude-code"]);
        // Codex CLI and the ChatGPT desktop app read one file: removing one removes both entries.
        assert_eq!(removal_clients("codex-cli"), vec!["codex-cli", "chatgpt-desktop"]);
        assert_eq!(removal_clients("chatgpt-desktop"), vec!["codex-cli", "chatgpt-desktop"]);
        assert!(removal_clients("nope").is_empty());
        let mut blob = TokenBlob::default();
        issue_for_client(&mut blob, "cur".into(), "Cursor", 1, "t1", TokenScope::Read, Some("cursor")).unwrap();
        issue_for_client(&mut blob, "cdx".into(), "Codex CLI", 2, "t2", TokenScope::Read, Some("codex-cli")).unwrap();
        issue_for_client(&mut blob, "gpt".into(), "ChatGPT desktop", 3, "t3", TokenScope::Full, Some("chatgpt-desktop")).unwrap();
        issue_into(&mut blob, "hand".into(), "by hand", 4, "t4", TokenScope::Read).unwrap();
        let ids = |tokens: Vec<crate::memory_api::TokenListing>| tokens.into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(linked_to_removal(&blob, "codex-cli")), ["cdx", "gpt"]);
        assert_eq!(ids(unlinked(&blob)), ["hand"]);
        assert_eq!(revoke_linked(&mut blob, "codex-cli"), ["cdx", "gpt"]);
        assert_eq!(ids(listing(&blob)), ["cur", "hand"]);
        // Nothing linked: nothing revoked, the unlinked token is left for the user.
        assert!(revoke_linked(&mut blob, "claude-code").is_empty());
        assert_eq!(revoke_linked(&mut blob, "cursor"), ["cur"]);
        assert_eq!(ids(listing(&blob)), ["hand"]);
    }

    #[test]
    fn app_connections_round_trip_beside_tokens_and_hold_nothing_secret() {
        let mut blob = TokenBlob::default();
        issue_into(&mut blob, "a".into(), "script", 1, "shogun_a", TokenScope::Full).unwrap();
        let listed = grant_into(&mut blob, "cursor", TokenScope::Read, 5, GrantOrigin::User).unwrap();
        assert_eq!((listed.client.as_str(), listed.label.as_str()), ("cursor", "Cursor"));
        // The ChatGPT desktop app's connection is the one Codex CLI's entry names.
        let listed = grant_into(&mut blob, "chatgpt-desktop", TokenScope::Full, 6, GrantOrigin::User).unwrap();
        assert_eq!((listed.client.as_str(), listed.label.as_str()), ("codex-cli", "Codex CLI, ChatGPT desktop"));
        // Issue #979: the listing says who can use each connection. No app's launcher is checked
        // yet, so every one is usable by any program, and says so on every face.
        assert_eq!(listed.launcher, crate::launcher_check::LauncherCheck::AnyProgram);
        assert_eq!(
            serde_json::to_value(&listed).unwrap()["launcher"],
            serde_json::json!("any_program")
        );
        // Adding again changes the scope in place; it never makes a second record.
        grant_into(&mut blob, "cursor", TokenScope::Full, 9, GrantOrigin::User).unwrap();
        assert_eq!(blob.grants.len(), 2);
        assert_eq!((blob.grants[0].scope, blob.grants[0].created_at), (TokenScope::Full, 5));
        assert!(grant_into(&mut blob, "other-app", TokenScope::Read, 1, GrantOrigin::User).is_err());

        let bytes = serialize_token_blob(&blob).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("shogun_a"), "{text}");
        let (back, _) = parse_token_blob_with_migration(&bytes).unwrap();
        assert_eq!(back, blob);
        // A token-only item keeps the exact shape older builds wrote.
        let only_tokens = TokenBlob { tokens: blob.tokens.clone(), grants: Vec::new() };
        let older = String::from_utf8(serialize_token_blob(&only_tokens).unwrap()).unwrap();
        assert!(!older.contains("grants"), "{older}");
        // Older items (no `grants`) read as no connections.
        let (old, _) = parse_token_blob_with_migration(older.as_bytes()).unwrap();
        assert!(old.grants.is_empty() && !old.is_empty());

        // Broken records fail the whole list closed: an unknown scope, a missing scope, a bad
        // id, the same app twice.
        for bad in [
            text.replacen(r#""scope":"full","created_at":5"#, r#""scope":"admin","created_at":5"#, 1),
            text.replacen(r#""scope":"full","created_at":5"#, r#""created_at":5"#, 1),
            text.replacen(r#""client":"cursor""#, r#""client":"Cursor App""#, 1),
            text.replacen(r#""client":"codex-cli""#, r#""client":"cursor""#, 1),
        ] {
            assert_ne!(bad, text);
            assert!(parse_token_blob_with_migration(bad.as_bytes()).is_err(), "{bad}");
        }

        // Disconnecting by either app sharing the connection removes it.
        assert!(revoke_grant(&mut blob, "codex-cli"));
        assert!(!revoke_grant(&mut blob, "chatgpt-desktop"));
        assert!(revoke_grant(&mut blob, "cursor"));
        assert!(blob.grants.is_empty());
        assert_eq!(grant_listing(&blob), Vec::new());
        // Tokens are untouched by a disconnect.
        assert_eq!(blob.tokens.len(), 1);
    }

    #[test]
    fn connection_ids_follow_the_shared_config_files() {
        assert_eq!(grant_id("cursor"), Some("cursor"));
        assert_eq!(grant_id("claude-code"), Some("claude-code"));
        assert_eq!(grant_id("claude-desktop"), Some("claude-desktop"));
        assert_eq!(grant_id("codex-cli"), Some("codex-cli"));
        assert_eq!(grant_id("chatgpt-desktop"), Some("codex-cli"));
        assert_eq!(grant_id("nope"), None);
        // A connection a newer build wrote is shown by its id.
        assert_eq!(grant_label("some-new-app"), "some-new-app");
    }

    #[test]
    fn multi_line_japanese_preferences_save() {
        // The report: two lines of Japanese were refused as "control characters" because the
        // line break is one.
        let prefs = normalize_prefs("ShogunAIの創業者です。\r\n株式会社Selectで事業を行っています。\n");
        assert_eq!(prefs, "ShogunAIの創業者です。\n株式会社Selectで事業を行っています。");
        let profile = Profile {
            display_name: "Toru".into(),
            role: "Founder".into(),
            prefs: format!("{prefs}\n\t- 箇条書き"),
        };
        assert_eq!(validate_profile(&profile), Ok(()));
        assert_eq!(normalize_prefs("a\rb"), "a\nb");
    }

    #[test]
    fn single_line_fields_and_other_controls_stay_refused() {
        for profile in [
            Profile { display_name: "To\nru".into(), ..Profile::default() },
            Profile { role: "Found\ter".into(), ..Profile::default() },
            Profile { prefs: "bell\u{7}".into(), ..Profile::default() },
            Profile { prefs: "escape\u{1b}[31m".into(), ..Profile::default() },
            Profile { prefs: "nul\0".into(), ..Profile::default() },
        ] {
            assert_eq!(
                validate_profile(&profile),
                Err("profile field contains control characters".into()),
                "{profile:?}"
            );
        }
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        // 4,096 Japanese characters are 12,288 bytes: a byte cap would refuse anything past 1,365.
        let at_cap = "記".repeat(MAX_PROFILE_FIELD_CHARS);
        assert!(at_cap.len() > MAX_PROFILE_FIELD_CHARS);
        let with = |field: usize, value: String| {
            let mut profile = Profile::default();
            *[&mut profile.display_name, &mut profile.role, &mut profile.prefs][field] = value;
            profile
        };
        for field in 0..3 {
            assert_eq!(validate_profile(&with(field, at_cap.clone())), Ok(()), "field {field} at the cap");
            assert_eq!(
                validate_profile(&with(field, format!("{at_cap}記"))),
                Err("profile field exceeds 4096 characters".into()),
                "field {field} past the cap"
            );
        }
    }

    #[test]
    fn profile_and_token_metadata_have_bounds() {
        assert!(validate_profile(&Profile {
            prefs: "x".repeat(MAX_PROFILE_FIELD_CHARS + 1),
            ..Profile::default()
        })
        .is_err());
        let invalid = format!(
            r#"{{\"tokens\":[{{\"id\":\"id\",\"name\":\"{}\",\"created_at\":1,\"verifier\":\"{}\"}}]}}"#,
            "x".repeat(MAX_TOKEN_NAME_BYTES + 1),
            token_verifier("s")
        );
        assert!(parse_token_blob_with_migration(invalid.as_bytes()).is_err());
    }

    /// Owner decision 2026-10-20 (audit F4): a connection the app makes on its own withholds
    /// meeting notes and screen recall; one the user makes does not; only the user (the switch,
    /// or widening to `full`) lets them back in; an API face can only withhold.
    #[test]
    fn automatic_connections_withhold_sensitive_sources_until_the_user_allows_them() {
        let mut blob = TokenBlob::default();
        let auto = grant_into(&mut blob, "cursor", TokenScope::Full, 5, GrantOrigin::Automatic).unwrap();
        // Automatic is always `read`, whatever was asked.
        assert_eq!((auto.scope, auto.sensitive_sources), (TokenScope::Read, SensitiveSources::Withheld));
        assert_eq!(serde_json::to_value(&auto).unwrap()["sensitive_sources"], serde_json::json!("withheld"));
        let manual = grant_into(&mut blob, "claude-code", TokenScope::Read, 6, GrantOrigin::User).unwrap();
        assert_eq!(manual.sensitive_sources, SensitiveSources::Allowed);

        // The app never changes a connection that is already there.
        grant_into(&mut blob, "claude-code", TokenScope::Read, 7, GrantOrigin::Automatic).unwrap();
        assert_eq!(blob.grants[1].sensitive_sources, SensitiveSources::Allowed);
        // The user changing the access to `read` keeps the setting; widening to `full` lifts it.
        grant_into(&mut blob, "cursor", TokenScope::Read, 8, GrantOrigin::User).unwrap();
        assert_eq!(blob.grants[0].sensitive_sources, SensitiveSources::Withheld);
        let widened = grant_into(&mut blob, "cursor", TokenScope::Full, 9, GrantOrigin::User).unwrap();
        assert_eq!((widened.scope, widened.sensitive_sources), (TokenScope::Full, SensitiveSources::Allowed));
        // Back to `read` by the user: still allowed (narrowing scope is not withholding).
        grant_into(&mut blob, "cursor", TokenScope::Read, 10, GrantOrigin::User).unwrap();
        assert_eq!(blob.grants[0].sensitive_sources, SensitiveSources::Allowed);

        // The API can withhold, and has nothing that allows.
        assert_eq!(withhold_sensitive_sources(&mut blob, "cursor"), Ok(true));
        assert_eq!(withhold_sensitive_sources(&mut blob, "cursor"), Ok(false));
        assert_eq!(blob.grants[0].sensitive_sources, SensitiveSources::Withheld);
        assert!(withhold_sensitive_sources(&mut blob, "codex-cli").is_err(), "not connected");
        // The Settings switch goes either way.
        let on = set_sensitive_sources(&mut blob, "cursor", SensitiveSources::Allowed).unwrap();
        assert_eq!(on.sensitive_sources, SensitiveSources::Allowed);
        let off = set_sensitive_sources(&mut blob, "cursor", SensitiveSources::Withheld).unwrap();
        assert_eq!(off.sensitive_sources, SensitiveSources::Withheld);
        assert!(set_sensitive_sources(&mut blob, "codex-cli", SensitiveSources::Allowed).is_err());

        // The marker survives the Keychain round trip, and an unknown value fails closed.
        let text = String::from_utf8(serialize_token_blob(&blob).unwrap()).unwrap();
        assert!(text.contains(r#""sensitive_sources":"withheld""#), "{text}");
        let (back, _) = parse_token_blob_with_migration(text.as_bytes()).unwrap();
        assert_eq!(back, blob);
        let bad = text.replacen(r#""sensitive_sources":"withheld""#, r#""sensitive_sources":"some""#, 1);
        assert!(parse_token_blob_with_migration(bad.as_bytes()).is_err());

        // The registry runs a stdio session under the connection's setting.
        let mut registry = crate::memory_api::TokenRegistry::new();
        registry.register_blob(&blob).unwrap();
        assert_eq!(registry.client_access("cursor"), Some((TokenScope::Read, SensitiveSources::Withheld)));
        assert_eq!(registry.client_access("claude-code"), Some((TokenScope::Read, SensitiveSources::Allowed)));
        assert_eq!(registry.authenticate_client("cursor"), Some(TokenScope::Read));
        let listed = registry.connections();
        assert_eq!(listed[0].sensitive_sources, SensitiveSources::Withheld);
    }

    /// Connections stored before the marker existed: a `read` one made since automatic
    /// connection shipped reads as withheld (nothing says the user clicked); an earlier one, and
    /// any `full` one, keep every source.
    #[test]
    fn connections_stored_without_the_marker_read_by_the_safety_rule() {
        let raw = |scope: &str, created_at: i64| {
            format!(r#"{{"tokens":[],"grants":[{{"client":"cursor","scope":"{scope}","created_at":{created_at}}}]}}"#)
        };
        let read = |scope: &str, created_at: i64| {
            parse_token_blob_with_migration(raw(scope, created_at).as_bytes()).unwrap().0.grants[0].sensitive_sources
        };
        assert_eq!(read("read", AUTO_CONNECT_SINCE_MS), SensitiveSources::Withheld);
        assert_eq!(read("read", AUTO_CONNECT_SINCE_MS + 86_400_000), SensitiveSources::Withheld);
        assert_eq!(read("read", AUTO_CONNECT_SINCE_MS - 1), SensitiveSources::Allowed);
        assert_eq!(read("full", AUTO_CONNECT_SINCE_MS + 1), SensitiveSources::Allowed);
        assert_eq!(read("full", 1), SensitiveSources::Allowed);
        // 2026-10-14T00:00:00Z.
        assert_eq!(AUTO_CONNECT_SINCE_MS, 1_791_936_000_000);
    }

    #[test]
    fn api_withholding_against_the_store_writes_only_a_narrowing() {
        let mut blob = TokenBlob::default();
        grant_into(&mut blob, "cursor", TokenScope::Read, 1, GrantOrigin::User).unwrap();
        let stored = serialize_token_blob(&blob).unwrap();
        let written = std::cell::RefCell::new(None::<Vec<u8>>);
        let changed = withhold_in_store(
            || Ok(Some(stored.clone())),
            |bytes| {
                *written.borrow_mut() = Some(bytes.to_vec());
                Ok(())
            },
            "cursor",
        );
        assert_eq!(changed, Ok(true));
        let after = written.borrow().clone().unwrap();
        let (after, _) = parse_token_blob_with_migration(&after).unwrap();
        assert_eq!(after.grants[0].sensitive_sources, SensitiveSources::Withheld);
        // Already withheld: nothing is written.
        let again = withhold_in_store(
            || Ok(Some(serialize_token_blob(&after).unwrap())),
            |_| Err("must not write".to_string()),
            "cursor",
        );
        assert_eq!(again, Ok(false));
        // No list at all: nothing connected.
        assert!(withhold_in_store(|| Ok(None), |_| Ok(()), "cursor").is_err());
    }
}
