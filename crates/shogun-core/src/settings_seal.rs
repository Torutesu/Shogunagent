//! Sealed privacy settings (audit 2026-10-20 M6 / F2).
//!
//! Four plain JSON files in app-data decide how much SHOGUN reads from the screen and who may
//! read the memory: `visual_recall.json` (on/off, how long screens are kept), `exclusions.json`
//! (apps and websites never captured), `capture_control.json` (the user's pause) and
//! `memory_api.json` (whether other apps may use the memory). The desktop re-reads them on every
//! capture tick. Any program running as the user could rewrite them and so turn Visual recall on,
//! drop an exclusion, end a pause or switch the Memory API on, skipping the L3 approval the API
//! faces require for exactly those changes.
//!
//! Four more files hold switches only the app may widen, and are sealed the same way (same day):
//! `web_fetch.json` (the Web fetch consent), `export_folders.json` (the folders exports may be
//! written into), `llm.json` (which credential the Agent lane runs on, the subscription opt-in and
//! OpenAI response storage) and `gmail-policy.json` (draft-only). What narrows each is in its
//! [`SealedSetting::narrow`] below.
//!
//! So every writer that holds the memory DB key also writes a **seal** next to the file
//! (`.<file>.seal`): the document it just wrote, with an HMAC-SHA256 under a key derived from the
//! memory DB key (never stored; the same Keychain-held secret the L3 approval store derives its
//! key from). Every reader then compares the file with the sealed document ([`trusted`]):
//!
//! - the same: the file is in force;
//! - different: only what **narrows** privacy is taken from the file (Visual recall off, a
//!   shorter retention, more exclusions, a longer pause, the Memory API off). What would widen it
//!   is ignored, logged once per setting ([`ignored_widenings`]), and stays ignored until the user
//!   makes the change in the app, which writes a new seal. API callers keep asking through their
//!   L3 approval, which the desktop applies through the same sealed write;
//! - a seal that does not verify, or one removed after sealing began on this Mac (recorded in
//!   the Keychain, [`SealMarker`]): the most private reading of the file
//!   ([`SealedSetting::without_baseline`]);
//! - no seal yet, never sealed on this Mac: the first run after the upgrade. The file is adopted
//!   as it is and sealed (backward compatibility), by a process allowed to adopt (the desktop).
//!
//! A process without the key (Linux dev, tests, a face whose Keychain read failed) reads and
//! writes the files as before. The seal is integrity, not secrecy: the settings stay readable.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Domain separation for the derived key: the same memory key never yields these bytes for
/// anything else (the approval store uses its own label).
const KEY_LABEL: &[u8] = b"shogun-settings-seal-v1";
const SEAL_VERSION: u64 = 1;
/// A seal file larger than this is not one of ours.
const MAX_SEAL_BYTES: u64 = 512 * 1024;

/// The sealing key: HMAC-SHA256(memory DB key, [`KEY_LABEL`]). Never persisted or logged;
/// zeroed on drop.
pub struct SealKey([u8; 32]);

impl SealKey {
    /// Derive the key from the memory DB key's raw bytes.
    pub fn derive(db_key: &[u8; 32]) -> Self {
        let mut okm = [0u8; 32];
        if let Ok(mut mac) = HmacSha256::new_from_slice(db_key) {
            mac.update(KEY_LABEL);
            okm.copy_from_slice(&mac.finalize().into_bytes());
        }
        Self(okm)
    }

    /// Derive from the memory DB key without copying it out ([`shogun_memory::DbKey::with_bytes`]).
    #[cfg(feature = "db")]
    pub fn from_db_key(key: &shogun_memory::DbKey) -> Self {
        key.with_bytes(Self::derive)
    }

    fn mac(&self, kind: &str, file_name: &str, doc: &str) -> Option<HmacSha256> {
        let mut mac = HmacSha256::new_from_slice(&self.0).ok()?;
        mac.update(format!("v{SEAL_VERSION}\0{kind}\0{file_name}\0").as_bytes());
        mac.update(doc.as_bytes());
        Some(mac)
    }
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealKey(***redacted***)")
    }
}

impl Drop for SealKey {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            // Volatile so the wipe of a value about to be dropped is not elided.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// Which settings this Mac has sealed at least once, kept where rewriting app-data cannot reach
/// it (the Keychain on macOS). It is what tells "first run after the upgrade" (adopt the file)
/// from "the seal was deleted" (trust nothing the file widens).
#[derive(Clone, Copy)]
pub struct SealMarker {
    /// The kinds sealed so far, or `None` when the store cannot say (read as "none").
    pub sealed_kinds: fn() -> Option<String>,
    /// Record the kinds sealed so far (comma-separated). `None` for a process that may not write
    /// the marker; such a process does not adopt files either.
    pub record: Option<fn(&str)>,
}

/// What a process seals and verifies with.
pub struct SealState {
    key: SealKey,
    marker: Option<SealMarker>,
}

impl SealState {
    pub fn new(key: SealKey, marker: Option<SealMarker>) -> Self {
        Self { key, marker }
    }

    fn may_adopt(&self) -> bool {
        match self.marker {
            None => true,
            Some(marker) => marker.record.is_some(),
        }
    }

    fn sealed_before(&self, kind: &str) -> bool {
        self.marker
            .and_then(|marker| (marker.sealed_kinds)())
            .is_some_and(|kinds| kinds.split(',').any(|k| k.trim() == kind))
    }

    fn record_sealed(&self, kind: &str) {
        let Some(marker) = self.marker else { return };
        let Some(record) = marker.record else { return };
        let mut kinds: BTreeSet<String> = (marker.sealed_kinds)()
            .unwrap_or_default()
            .split(',')
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        if kinds.insert(kind.to_string()) {
            record(&kinds.into_iter().collect::<Vec<_>>().join(","));
        }
    }
}

static STATE: OnceLock<SealState> = OnceLock::new();
static IGNORED: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

/// Install this process's sealing state. `false` when one was already installed.
pub fn install(state: SealState) -> bool {
    STATE.set(state).is_ok()
}

/// Install from the memory DB key, with the Keychain marker on macOS. `may_adopt`: whether this
/// process seals files it finds unsealed on the first run (the desktop); the standalone faces
/// pass `false` and leave that to the app.
#[cfg(feature = "db")]
pub fn install_from_db_key(key: &shogun_memory::DbKey, may_adopt: bool) -> bool {
    let installed = install(SealState::new(SealKey::from_db_key(key), keychain_marker(may_adopt)));
    if installed {
        install_mcp_hooks();
    }
    installed
}

/// Whether a sealing state is installed in this process.
pub fn installed() -> bool {
    STATE.get().is_some()
}

/// The settings whose file asked for something wider than its seal since this process started
/// (ids such as `visual_recall`). The widening was not applied.
pub fn ignored_widenings() -> Vec<&'static str> {
    match IGNORED.lock() {
        Ok(set) => set.iter().copied().collect(),
        Err(poisoned) => poisoned.into_inner().iter().copied().collect(),
    }
}

fn note_ignored(kind: &'static str) {
    let mut set = match IGNORED.lock() {
        Ok(set) => set,
        Err(poisoned) => poisoned.into_inner(),
    };
    if set.insert(kind) {
        // The setting's id only; never a value from the file.
        eprintln!("[settings-seal] ignored a {kind} change made outside ShogunAI; change it in the app to apply it");
    }
}

/// A settings document that can be sealed.
pub trait SealedSetting: Sized + Clone + PartialEq {
    /// The setting's id (also the marker entry and the HMAC domain).
    const KIND: &'static str;
    /// The canonical document the seal covers.
    fn to_doc(&self) -> Option<String>;
    /// Read a sealed document back.
    fn from_doc(doc: &str) -> Option<Self>;
    /// The value in force when `trusted` is sealed and the file says `file`: everything that
    /// narrows privacy is taken from `file`, nothing that widens it.
    fn narrow(trusted: &Self, file: &Self, now_ms: i64) -> Self;
    /// The value in force when the file cannot be checked against a seal that should exist.
    fn without_baseline(file: &Self, now_ms: i64) -> Self;
}

/// Where the seal of `path` lives.
pub fn seal_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("settings.json");
    path.with_file_name(format!(".{name}.seal"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SealFile {
    v: u64,
    kind: String,
    doc: String,
    mac: String,
}

enum Baseline {
    Missing,
    Invalid,
    Verified(String),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

fn read_baseline(state: &SealState, path: &Path, kind: &str) -> Baseline {
    let seal = seal_path(path);
    match std::fs::symlink_metadata(&seal) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Baseline::Missing,
        Err(_) => return Baseline::Invalid,
        Ok(meta) if !meta.is_file() || meta.len() > MAX_SEAL_BYTES => return Baseline::Invalid,
        Ok(_) => {}
    }
    let Ok(text) = std::fs::read_to_string(&seal) else {
        return Baseline::Invalid;
    };
    let Ok(file) = serde_json::from_str::<SealFile>(&text) else {
        return Baseline::Invalid;
    };
    if file.v != SEAL_VERSION || file.kind != kind {
        return Baseline::Invalid;
    }
    let (Some(tag), Some(mac)) = (unhex(&file.mac), state.key.mac(kind, file_name(path), &file.doc))
    else {
        return Baseline::Invalid;
    };
    if mac.verify_slice(&tag).is_err() {
        return Baseline::Invalid;
    }
    Baseline::Verified(file.doc)
}

fn write_seal(state: &SealState, path: &Path, kind: &str, doc: &str) -> Result<(), String> {
    let mac = state
        .key
        .mac(kind, file_name(path), doc)
        .ok_or_else(|| "settings seal unavailable".to_string())?;
    let body = serde_json::to_string(&SealFile {
        v: SEAL_VERSION,
        kind: kind.to_string(),
        doc: doc.to_string(),
        mac: hex(&mac.finalize().into_bytes()),
    })
    .map_err(|_| "settings seal encode".to_string())?;
    let seal = seal_path(path);
    let tmp = seal.with_file_name(format!(
        "{}.{}.tmp",
        file_name(&seal),
        std::process::id()
    ));
    std::fs::write(&tmp, body).map_err(|_| "settings seal write".to_string())?;
    std::fs::rename(&tmp, &seal).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        "settings seal replace".to_string()
    })?;
    state.record_sealed(kind);
    Ok(())
}

/// The value in force for `file` read from `path`, under `state` (see the module docs).
pub fn trusted_with<T: SealedSetting>(state: &SealState, path: &Path, file: T, now_ms: i64) -> T {
    match read_baseline(state, path, T::KIND) {
        Baseline::Verified(doc) => match T::from_doc(&doc) {
            Some(sealed) if sealed == file => file,
            Some(sealed) => {
                let in_force = T::narrow(&sealed, &file, now_ms);
                if in_force != file {
                    note_ignored(T::KIND);
                }
                in_force
            }
            None => {
                note_ignored(T::KIND);
                T::without_baseline(&file, now_ms)
            }
        },
        Baseline::Invalid => {
            let in_force = T::without_baseline(&file, now_ms);
            if in_force != file {
                note_ignored(T::KIND);
            }
            in_force
        }
        Baseline::Missing if state.sealed_before(T::KIND) => {
            let in_force = T::without_baseline(&file, now_ms);
            if in_force != file {
                note_ignored(T::KIND);
            }
            in_force
        }
        Baseline::Missing => {
            // First run after the upgrade: adopt the file as it is.
            if state.may_adopt() {
                if let Some(doc) = file.to_doc() {
                    let _ = write_seal(state, path, T::KIND, &doc);
                }
            }
            file
        }
    }
}

/// Seal `value`, just written to `path`, under `state`.
pub fn seal_with<T: SealedSetting>(state: &SealState, path: &Path, value: &T) -> Result<(), String> {
    let doc = value.to_doc().ok_or_else(|| "settings seal encode".to_string())?;
    write_seal(state, path, T::KIND, &doc)
}

/// [`trusted_with`] under the installed state; the file as it is when none is installed.
pub fn trusted<T: SealedSetting>(path: &Path, file: T, now_ms: i64) -> T {
    match STATE.get() {
        Some(state) => trusted_with(state, path, file, now_ms),
        None => file,
    }
}

/// [`seal_with`] under the installed state; nothing when none is installed. A failed seal is
/// logged, not returned: the setting itself was saved, and the next read narrows to the old seal,
/// which only errs towards privacy.
pub fn seal<T: SealedSetting>(path: &Path, value: &T) {
    if let Some(state) = STATE.get() {
        if seal_with(state, path, value).is_err() {
            eprintln!("[settings-seal] could not seal {}", T::KIND);
        }
    }
}

/// Wall-clock ms for the time-dependent rules (a timed pause).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// [`trusted`] at the current time, in the shape `shogun_mcp::settings_seal_hook` calls.
#[cfg(any(feature = "db", feature = "net"))]
fn trusted_now<T: SealedSetting>(path: &Path, file: T) -> T {
    trusted(path, file, now_ms())
}

// ── The sealed settings ─────────────────────────────────────────────────────────────────────

impl SealedSetting for crate::capture::visual_recall::Settings {
    const KIND: &'static str = "visual_recall";

    fn to_doc(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    fn from_doc(doc: &str) -> Option<Self> {
        serde_json::from_str(doc).ok()
    }

    /// Off wins; the shorter retention wins.
    fn narrow(trusted: &Self, file: &Self, _now_ms: i64) -> Self {
        Self {
            enabled: trusted.enabled && file.enabled,
            retention: if file.retention.days() <= trusted.retention.days() {
                file.retention
            } else {
                trusted.retention
            },
        }
    }

    /// Off, and no longer than the default retention.
    fn without_baseline(file: &Self, _now_ms: i64) -> Self {
        let default = crate::capture::visual_recall::RetentionPolicy::default();
        Self {
            enabled: false,
            retention: if file.retention.days() <= default.days() { file.retention } else { default },
        }
    }
}

impl SealedSetting for crate::capture::exclusion::ExclusionPolicy {
    const KIND: &'static str = "exclusions";

    fn to_doc(&self) -> Option<String> {
        Some(crate::capture::exclusion::policy_document(self))
    }

    fn from_doc(doc: &str) -> Option<Self> {
        crate::capture::exclusion::parse_policy_document(doc)
    }

    /// Every exclusion either side has: removing one from the file does not resume capture.
    fn narrow(trusted: &Self, file: &Self, _now_ms: i64) -> Self {
        trusted.union(file)
    }

    /// What was removed cannot be known without the seal: the file's exclusions stay in force
    /// (and are noted), the non-removable defaults always apply.
    fn without_baseline(file: &Self, _now_ms: i64) -> Self {
        file.clone()
    }
}

impl SealedSetting for crate::capture::control::Status {
    const KIND: &'static str = "capture_control";

    fn to_doc(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    fn from_doc(doc: &str) -> Option<Self> {
        serde_json::from_str(doc).ok()
    }

    /// The pause that runs longer wins: a file that resumes capture sooner is not followed.
    fn narrow(trusted: &Self, file: &Self, now_ms: i64) -> Self {
        if trusted.resumes_sooner(*file, now_ms) {
            *trusted
        } else {
            *file
        }
    }

    /// Paused until the user resumes in the app.
    fn without_baseline(_file: &Self, _now_ms: i64) -> Self {
        Self { paused: true, resume_at_ms: None }
    }
}

/// `llm.json`: which credential the Agent lane runs on (`crate::agent_source`).
impl SealedSetting for crate::agent_source::LlmSettings {
    const KIND: &'static str = "agent_source";

    fn to_doc(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    fn from_doc(doc: &str) -> Option<Self> {
        serde_json::from_str(doc).ok()
    }

    /// Which provider and model receive prompts has no "more private" direction (one third party
    /// for another), so a source chosen outside the app is not followed, except going back to the
    /// default (the default provider with its recommended model). The subscription opt-in and
    /// OpenAI response storage can only be withdrawn: each is on only when both sides say so.
    fn narrow(trusted: &Self, file: &Self, _now_ms: i64) -> Self {
        let default = Self::default();
        let unchanged = file.provider == trusted.provider && file.model == trusted.model;
        let to_default = file.provider == default.provider && file.model.trim().is_empty();
        let (provider, model) = if unchanged || to_default {
            (file.provider.clone(), file.model.clone())
        } else {
            (trusted.provider.clone(), trusted.model.clone())
        };
        Self {
            provider,
            model,
            subscription_consent: trusted.subscription_consent && file.subscription_consent,
            openai_store: trusted.openai_store && file.openai_store,
        }
    }

    /// The default source, no subscription opt-in, no response storage.
    fn without_baseline(_file: &Self, _now_ms: i64) -> Self {
        Self::default()
    }
}

/// `memory_api.json` lives in `shogun-mcp`, which cannot depend on this crate: its loader and
/// writer call the hook installed here ([`install_mcp_hooks`]).
#[cfg(any(feature = "db", feature = "net"))]
mod memory_api {
    use super::SealedSetting;
    use shogun_mcp::memory_api_settings::Settings;

    impl SealedSetting for Settings {
        const KIND: &'static str = "memory_api";

        fn to_doc(&self) -> Option<String> {
            serde_json::to_string(self).ok()
        }

        fn from_doc(doc: &str) -> Option<Self> {
            serde_json::from_str(doc).ok()
        }

        /// Off wins for the API and for auto-connect; every declined app stays declined. The
        /// profile and the bookkeeping lists come from the file.
        fn narrow(trusted: &Self, file: &Self, _now_ms: i64) -> Self {
            let mut declined = trusted.auto_connect_declined.clone();
            for id in &file.auto_connect_declined {
                if !declined.contains(id) {
                    declined.push(id.clone());
                }
            }
            Self {
                enabled: trusted.enabled && file.enabled,
                auto_connect: trusted.auto_connect && file.auto_connect,
                auto_connect_declined: declined,
                ..file.clone()
            }
        }

        fn without_baseline(file: &Self, _now_ms: i64) -> Self {
            Self { enabled: false, auto_connect: false, ..file.clone() }
        }
    }

    pub(super) struct Hook;

    impl shogun_mcp::memory_api_settings::SettingsSealHook for Hook {
        fn trusted(&self, path: &std::path::Path, file: Settings) -> Settings {
            super::trusted(path, file, super::now_ms())
        }

        fn seal(&self, path: &std::path::Path, settings: &Settings) {
            super::seal(path, settings);
        }
    }
}

/// `web_fetch.json`, `export_folders.json` and `gmail-policy.json` also live in `shogun-mcp`;
/// their loaders and writers call the slots installed here ([`install_mcp_hooks`]).
#[cfg(any(feature = "db", feature = "net"))]
mod mcp_files {
    use super::SealedSetting;
    use shogun_mcp::export_folders::FolderList;
    use shogun_mcp::gmail_policy::GmailPolicy;
    use shogun_mcp::web_consent::WebFetchPolicy;

    /// The Web fetch consent: the one-time acknowledgement of three disclosures.
    impl SealedSetting for WebFetchPolicy {
        const KIND: &'static str = "web_fetch";

        fn to_doc(&self) -> Option<String> {
            serde_json::to_string(self).ok()
        }

        fn from_doc(doc: &str) -> Option<Self> {
            serde_json::from_str::<Self>(doc).ok().map(|policy| {
                if policy.disclosures.all_acknowledged() {
                    policy
                } else {
                    Self::default()
                }
            })
        }

        /// Revoking applies; a consent the seal does not hold is not followed.
        fn narrow(trusted: &Self, file: &Self, _now_ms: i64) -> Self {
            if file.consent().is_none() || trusted.consent().is_some() {
                file.clone()
            } else {
                trusted.clone()
            }
        }

        /// Not consented: the user acknowledges again in the app.
        fn without_baseline(_file: &Self, _now_ms: i64) -> Self {
            Self::default()
        }
    }

    /// The folders exports may be written into.
    impl SealedSetting for FolderList {
        const KIND: &'static str = "export_folders";

        fn to_doc(&self) -> Option<String> {
            let folders: Vec<&str> =
                self.0.iter().map(|p| p.to_str()).collect::<Option<Vec<_>>>()?;
            serde_json::to_string(&folders).ok()
        }

        fn from_doc(doc: &str) -> Option<Self> {
            let folders: Vec<String> = serde_json::from_str(doc).ok()?;
            Some(Self(folders.into_iter().map(std::path::PathBuf::from).collect()))
        }

        /// Only folders both sides list: a removal applies, an addition does not.
        fn narrow(trusted: &Self, file: &Self, _now_ms: i64) -> Self {
            Self(file.0.iter().filter(|f| trusted.0.contains(f)).cloned().collect())
        }

        /// No folder: exports stay off until the user picks one in the app.
        fn without_baseline(_file: &Self, _now_ms: i64) -> Self {
            Self::default()
        }
    }

    /// Direct Gmail's draft-only switch.
    impl SealedSetting for GmailPolicy {
        const KIND: &'static str = "gmail_policy";

        fn to_doc(&self) -> Option<String> {
            serde_json::to_string(self).ok()
        }

        fn from_doc(doc: &str) -> Option<Self> {
            serde_json::from_str(doc).ok()
        }

        /// Draft-only on wins: turning it on applies, turning it off does not.
        fn narrow(trusted: &Self, file: &Self, _now_ms: i64) -> Self {
            Self { draft_stop: trusted.draft_stop || file.draft_stop }
        }

        fn without_baseline(_file: &Self, _now_ms: i64) -> Self {
            Self { draft_stop: true }
        }
    }
}

/// Route the reads and writes of the settings files `shogun-mcp` owns (`memory_api.json`,
/// `web_fetch.json`, `export_folders.json`, `gmail-policy.json`) through this process's seal.
#[cfg(any(feature = "db", feature = "net"))]
pub fn install_mcp_hooks() -> bool {
    use shogun_mcp::settings_seal_hook::SealFns;
    let memory_api = shogun_mcp::memory_api_settings::install_seal_hook(Box::new(memory_api::Hook));
    let web_fetch = shogun_mcp::web_consent::SEAL
        .install(SealFns { trusted: trusted_now, seal });
    let export_folders = shogun_mcp::export_folders::SEAL
        .install(SealFns { trusted: trusted_now, seal });
    let gmail = shogun_mcp::gmail_policy::SEAL
        .install(SealFns { trusted: trusted_now, seal });
    memory_api && web_fetch && export_folders && gmail
}

/// The Keychain marker (the Mac app's build, `desktop::keychain::seal_marker`). `may_adopt`
/// lets this process record new kinds.
#[cfg(all(feature = "desktop", feature = "db"))]
fn keychain_marker(may_adopt: bool) -> Option<SealMarker> {
    crate::desktop::keychain::seal_marker(may_adopt)
}

#[cfg(all(not(feature = "desktop"), feature = "db"))]
fn keychain_marker(may_adopt: bool) -> Option<SealMarker> {
    // Without the Mac app there is no Keychain: a process that may adopt runs without a marker
    // (every missing seal is a first run); one that may not gets a marker that says nothing was
    // sealed and records nothing, so it never adopts.
    if may_adopt {
        None
    } else {
        Some(SealMarker { sealed_kinds: || Some(String::new()), record: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::control::Status;
    use crate::capture::exclusion::ExclusionPolicy;
    use crate::capture::visual_recall::{RetentionPolicy, Settings as Recall};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("shogun-seal-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state() -> SealState {
        SealState::new(SealKey::derive(&[7u8; 32]), None)
    }

    fn recall(enabled: bool, days: u32) -> Recall {
        Recall { enabled, retention: RetentionPolicy::try_days(days).unwrap() }
    }

    #[test]
    fn the_first_run_adopts_the_file_as_it_is_and_seals_it() {
        let dir = TempDir::new("adopt");
        let path = dir.file("visual_recall.json");
        let state = state();
        assert_eq!(trusted_with(&state, &path, recall(true, 7), 0), recall(true, 7));
        assert!(seal_path(&path).exists(), "sealed on the first read");
        // The adopted value now holds: the same file reads as itself.
        assert_eq!(trusted_with(&state, &path, recall(true, 7), 0), recall(true, 7));
    }

    #[test]
    fn a_widening_edit_is_ignored_and_a_narrowing_one_applies() {
        let dir = TempDir::new("visual");
        let path = dir.file("visual_recall.json");
        let state = state();
        seal_with(&state, &path, &recall(false, 3)).unwrap();
        // Turned on and lengthened behind the app's back: neither applies.
        assert_eq!(trusted_with(&state, &path, recall(true, 30), 0), recall(false, 3));
        // Shortened: applies.
        assert_eq!(trusted_with(&state, &path, recall(false, 1), 0), recall(false, 1));
        // Sealed on, edited off: off applies.
        seal_with(&state, &path, &recall(true, 7)).unwrap();
        assert_eq!(trusted_with(&state, &path, recall(false, 7), 0), recall(false, 7));
        // Sealed on, edited to keep longer: the sealed retention holds.
        assert_eq!(trusted_with(&state, &path, recall(true, 365), 0), recall(true, 7));
        assert!(ignored_widenings().contains(&"visual_recall"));
    }

    #[test]
    fn a_change_the_app_seals_applies_in_full() {
        let dir = TempDir::new("app-write");
        let path = dir.file("visual_recall.json");
        let state = state();
        seal_with(&state, &path, &recall(false, 3)).unwrap();
        seal_with(&state, &path, &recall(true, 30)).unwrap();
        assert_eq!(trusted_with(&state, &path, recall(true, 30), 0), recall(true, 30));
    }

    #[test]
    fn a_forged_or_foreign_seal_falls_back_to_the_most_private_reading() {
        let dir = TempDir::new("forged");
        let path = dir.file("visual_recall.json");
        // Sealed under another key: does not verify.
        let other = SealState::new(SealKey::derive(&[8u8; 32]), None);
        seal_with(&other, &path, &recall(true, 30)).unwrap();
        let state = state();
        assert_eq!(trusted_with(&state, &path, recall(true, 30), 0), recall(false, 3));
        // A hand-written seal with a made-up tag.
        std::fs::write(
            seal_path(&path),
            r#"{"v":1,"kind":"visual_recall","doc":"{\"enabled\":true,\"retention\":{\"days\":30}}","mac":"00"}"#,
        )
        .unwrap();
        assert_eq!(trusted_with(&state, &path, recall(true, 30), 0), recall(false, 3));
        // Another setting's seal moved over this one.
        let elsewhere = dir.file("capture_control.json");
        seal_with(&state, &elsewhere, &Status { paused: false, resume_at_ms: None }).unwrap();
        std::fs::copy(seal_path(&elsewhere), seal_path(&path)).unwrap();
        assert_eq!(trusted_with(&state, &path, recall(true, 30), 0), recall(false, 3));
    }

    #[test]
    fn a_seal_deleted_after_sealing_began_is_not_a_first_run() {
        static SEALED: Mutex<String> = Mutex::new(String::new());
        fn read() -> Option<String> {
            Some(SEALED.lock().unwrap().clone())
        }
        fn record(kinds: &str) {
            *SEALED.lock().unwrap() = kinds.to_string();
        }
        let dir = TempDir::new("deleted");
        let path = dir.file("visual_recall.json");
        let state = SealState::new(
            SealKey::derive(&[7u8; 32]),
            Some(SealMarker { sealed_kinds: read, record: Some(record) }),
        );
        seal_with(&state, &path, &recall(false, 3)).unwrap();
        assert_eq!(read().as_deref(), Some("visual_recall"));
        std::fs::remove_file(seal_path(&path)).unwrap();
        assert_eq!(trusted_with(&state, &path, recall(true, 30), 0), recall(false, 3));
        assert!(!seal_path(&path).exists(), "not re-adopted");
    }

    #[test]
    fn a_process_that_may_not_adopt_reads_an_unsealed_file_as_it_is_without_sealing() {
        let dir = TempDir::new("no-adopt");
        let path = dir.file("visual_recall.json");
        let state = SealState::new(
            SealKey::derive(&[7u8; 32]),
            Some(SealMarker { sealed_kinds: || Some(String::new()), record: None }),
        );
        assert_eq!(trusted_with(&state, &path, recall(true, 7), 0), recall(true, 7));
        assert!(!seal_path(&path).exists());
    }

    #[test]
    fn removing_an_exclusion_behind_the_apps_back_does_not_resume_capture() {
        let dir = TempDir::new("exclusions");
        let path = dir.file("exclusions.json");
        let state = state();
        let mut sealed = ExclusionPolicy::new();
        sealed.add_app("com.example.secret");
        sealed.add_website("bank.example");
        seal_with(&state, &path, &sealed).unwrap();
        // The file dropped both and added another.
        let mut file = ExclusionPolicy::new();
        file.add_app("com.example.more");
        let in_force = trusted_with(&state, &path, file, 0);
        assert_eq!(in_force.user_apps(), vec!["com.example.more", "com.example.secret"]);
        assert_eq!(in_force.user_websites(), vec!["bank.example"]);
        // Adding only: applies as is.
        let mut more = sealed.clone();
        more.add_app("com.example.third");
        assert_eq!(trusted_with(&state, &path, more.clone(), 0), more);
    }

    #[test]
    fn ending_a_pause_behind_the_apps_back_does_not_resume_capture() {
        let dir = TempDir::new("pause");
        let path = dir.file("capture_control.json");
        let state = state();
        let now = 1_000_000;
        let paused = Status { paused: true, resume_at_ms: None };
        seal_with(&state, &path, &paused).unwrap();
        let resumed = Status { paused: false, resume_at_ms: None };
        assert_eq!(trusted_with(&state, &path, resumed, now), paused);
        // A timed pause brought forward is not followed either.
        let timed = Status { paused: true, resume_at_ms: Some(now + 3_600_000) };
        seal_with(&state, &path, &timed).unwrap();
        let sooner = Status { paused: true, resume_at_ms: Some(now + 60_000) };
        assert_eq!(trusted_with(&state, &path, sooner, now), timed);
        // Pausing from the file applies.
        seal_with(&state, &path, &resumed).unwrap();
        assert_eq!(trusted_with(&state, &path, paused, now), paused);
        // No seal that verifies: paused until the user resumes in the app.
        std::fs::write(seal_path(&path), "{}").unwrap();
        assert_eq!(trusted_with(&state, &path, resumed, now), paused);
    }

    #[cfg(any(feature = "db", feature = "net"))]
    #[test]
    fn switching_the_memory_api_on_behind_the_apps_back_is_not_followed() {
        use shogun_mcp::memory_api_settings::Settings as Api;
        let dir = TempDir::new("memory-api");
        let path = dir.file("memory_api.json");
        let state = state();
        let off = Api {
            enabled: false,
            auto_connect: false,
            auto_connect_declined: vec!["cursor".into()],
            ..Api::default()
        };
        seal_with(&state, &path, &off).unwrap();
        let widened = Api {
            enabled: true,
            auto_connect: true,
            auto_connect_declined: Vec::new(),
            ..Api::default()
        };
        let in_force = trusted_with(&state, &path, widened, 0);
        assert!(!in_force.enabled && !in_force.auto_connect);
        assert_eq!(in_force.auto_connect_declined, vec!["cursor".to_string()]);
        // The profile is not a privacy switch: it comes from the file.
        let mut profiled = off.clone();
        profiled.profile.display_name = "Aki".into();
        assert_eq!(trusted_with(&state, &path, profiled.clone(), 0), profiled);
        // Sealed on, edited off: off applies.
        let on = Api { enabled: true, ..off.clone() };
        seal_with(&state, &path, &on).unwrap();
        assert!(!trusted_with(&state, &path, off.clone(), 0).enabled);
        // A forged seal: off.
        std::fs::write(seal_path(&path), "garbage").unwrap();
        assert!(!trusted_with(&state, &path, on, 0).enabled);
    }

    /// The first read of an unsealed file adopts it; a value the app seals reads back as itself.
    fn adopts_and_round_trips<T: SealedSetting + std::fmt::Debug>(file_name: &str, value: T) {
        let dir = TempDir::new("adopt-each");
        let path = dir.file(file_name);
        let state = state();
        assert_eq!(T::from_doc(&value.to_doc().unwrap()), Some(value.clone()));
        assert_eq!(trusted_with(&state, &path, value.clone(), 0), value);
        assert!(seal_path(&path).exists(), "{} sealed on the first read", T::KIND);
        assert_eq!(trusted_with(&state, &path, value.clone(), 0), value);
        let fresh = dir.file(&format!("fresh-{file_name}"));
        seal_with(&state, &fresh, &value).unwrap();
        assert_eq!(trusted_with(&state, &fresh, value.clone(), 0), value);
    }

    fn llm(provider: &str, model: &str, consent: bool, store: bool) -> crate::agent_source::LlmSettings {
        crate::agent_source::LlmSettings {
            provider: provider.into(),
            model: model.into(),
            subscription_consent: consent,
            openai_store: store,
        }
    }

    #[test]
    fn an_agent_source_chosen_outside_the_app_is_not_followed_unless_it_is_the_default() {
        adopts_and_round_trips("llm.json", llm("openai", "gpt-6-sol", false, true));
        let dir = TempDir::new("agent-source");
        let path = dir.file("llm.json");
        let state = state();
        let sealed = llm("openai", "gpt-6-sol", false, false);
        seal_with(&state, &path, &sealed).unwrap();
        // A subscription with the opt-in, written behind the app's back: not followed.
        assert_eq!(trusted_with(&state, &path, llm("codex", "", true, false), 0), sealed);
        // Another API-key provider or model: not followed either.
        assert_eq!(trusted_with(&state, &path, llm("openrouter", "x/y", false, false), 0), sealed);
        assert_eq!(trusted_with(&state, &path, llm("openai", "gpt-6-luna", false, false), 0), sealed);
        // OpenAI response storage turned on: not followed.
        assert_eq!(trusted_with(&state, &path, llm("openai", "gpt-6-sol", false, true), 0), sealed);
        // Back to the default source: applies.
        let default = crate::agent_source::LlmSettings::default();
        assert_eq!(trusted_with(&state, &path, default.clone(), 0), default);
        // Sealed on a consented subscription with storage on: withdrawing either applies.
        let delegate = llm("claude-code", "", true, true);
        seal_with(&state, &path, &delegate).unwrap();
        assert_eq!(
            trusted_with(&state, &path, llm("claude-code", "", false, false), 0),
            llm("claude-code", "", false, false)
        );
        // Moving to the default keeps neither switch on that the file turned off.
        assert_eq!(trusted_with(&state, &path, llm("shogun", "", true, false), 0), llm("shogun", "", true, false));
        // A seal that does not verify: the default, opted out.
        std::fs::write(seal_path(&path), "{}").unwrap();
        assert_eq!(trusted_with(&state, &path, delegate, 0), default);
        assert!(ignored_widenings().contains(&"agent_source"));
    }

    #[cfg(any(feature = "db", feature = "net"))]
    #[test]
    fn a_web_fetch_consent_written_outside_the_app_is_not_followed() {
        use shogun_mcp::web_consent::{Disclosures, WebFetchPolicy};
        let consented = WebFetchPolicy {
            disclosures: Disclosures { urls_leave_device: true, text_to_model: true, no_media_stored: true },
            acknowledged_at_ms: Some(5),
        };
        adopts_and_round_trips("web_fetch.json", consented.clone());
        adopts_and_round_trips("web_fetch.json", WebFetchPolicy::default());
        let dir = TempDir::new("web-fetch");
        let path = dir.file("web_fetch.json");
        let state = state();
        seal_with(&state, &path, &WebFetchPolicy::default()).unwrap();
        // Consent granted behind the app's back: not followed.
        assert!(trusted_with(&state, &path, consented.clone(), 0).consent().is_none());
        // Sealed consented, revoked in the file: the revocation applies.
        seal_with(&state, &path, &consented).unwrap();
        assert_eq!(trusted_with(&state, &path, WebFetchPolicy::default(), 0), WebFetchPolicy::default());
        // A forged seal: not consented.
        std::fs::write(seal_path(&path), "garbage").unwrap();
        assert!(trusted_with(&state, &path, consented, 0).consent().is_none());
        assert!(ignored_widenings().contains(&"web_fetch"));
    }

    #[cfg(any(feature = "db", feature = "net"))]
    #[test]
    fn an_export_folder_added_outside_the_app_is_not_an_export_folder() {
        use shogun_mcp::export_folders::FolderList;
        let list = |paths: &[&str]| FolderList(paths.iter().map(PathBuf::from).collect());
        adopts_and_round_trips("export_folders.json", list(&["/Users/a/Notes", "/Users/a/Out"]));
        let dir = TempDir::new("export-folders");
        let path = dir.file("export_folders.json");
        let state = state();
        seal_with(&state, &path, &list(&["/Users/a/Notes", "/Users/a/Out"])).unwrap();
        // One folder dropped and a broad one added: the removal applies, the addition does not.
        assert_eq!(
            trusted_with(&state, &path, list(&["/Users/a", "/Users/a/Out"]), 0),
            list(&["/Users/a/Out"])
        );
        // Removing every folder applies.
        assert_eq!(trusted_with(&state, &path, list(&[]), 0), list(&[]));
        // A forged seal: no export folder.
        std::fs::write(seal_path(&path), "[]").unwrap();
        assert_eq!(trusted_with(&state, &path, list(&["/Users/a/Notes"]), 0), list(&[]));
        assert!(ignored_widenings().contains(&"export_folders"));
    }

    #[cfg(any(feature = "db", feature = "net"))]
    #[test]
    fn draft_only_turned_off_outside_the_app_is_not_followed() {
        use shogun_mcp::gmail_policy::GmailPolicy;
        adopts_and_round_trips("gmail-policy.json", GmailPolicy { draft_stop: false });
        let dir = TempDir::new("gmail-policy");
        let path = dir.file("gmail-policy.json");
        let state = state();
        seal_with(&state, &path, &GmailPolicy { draft_stop: true }).unwrap();
        // Off behind the app's back: stays on.
        assert!(trusted_with(&state, &path, GmailPolicy { draft_stop: false }, 0).draft_stop);
        // Sealed off (the user's choice in Settings), turned on in the file: on applies.
        seal_with(&state, &path, &GmailPolicy { draft_stop: false }).unwrap();
        assert!(trusted_with(&state, &path, GmailPolicy { draft_stop: true }, 0).draft_stop);
        assert!(!trusted_with(&state, &path, GmailPolicy { draft_stop: false }, 0).draft_stop);
        // A forged seal: on.
        std::fs::write(seal_path(&path), "garbage").unwrap();
        assert!(trusted_with(&state, &path, GmailPolicy { draft_stop: false }, 0).draft_stop);
        assert!(ignored_widenings().contains(&"gmail_policy"));
    }

    #[test]
    fn an_exclusion_document_round_trips() {
        let mut policy = ExclusionPolicy::new();
        policy.add_app("com.example.a");
        policy.add_website("https://Example.org/path");
        let doc = crate::capture::exclusion::policy_document(&policy);
        assert_eq!(crate::capture::exclusion::parse_policy_document(&doc), Some(policy));
    }

    #[test]
    fn the_seal_holds_no_secret_and_the_key_is_redacted() {
        let key = SealKey::derive(&[9u8; 32]);
        assert_eq!(format!("{key:?}"), "SealKey(***redacted***)");
        assert_ne!(SealKey::derive(&[9u8; 32]).0, [9u8; 32]);
        assert_ne!(SealKey::derive(&[9u8; 32]).0, SealKey::derive(&[10u8; 32]).0);
    }
}
