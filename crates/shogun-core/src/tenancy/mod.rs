//! Tenancy (ShogunAgent E2): one principal = one SQLCipher file (CLAUDE.md invariant 6).
//!
//! A `tenant` (organisation) holds `principal`s (people, one world model each), and `agent`s hold
//! the keys that act for a principal. Nothing here puts a tenant column in the schema: the file is
//! the boundary, and a cross-principal read can only happen through an explicit ACL (Team, later),
//! never through a query.
//!
//! - [`PrincipalId`]: a validated slug, which is also the directory name under the store root.
//! - [`KeyProvider`]: where a principal's SQLCipher key comes from — the environment
//!   ([`EnvKeyProvider`]: `SHOGUN_DB_KEY`, or `SHOGUN_DB_KEY_<PRINCIPAL>` per principal) or a KMS
//!   ([`KmsKeyProvider`], E26: envelope encryption under the customer's AWS KMS, GCP Cloud KMS or
//!   Vault Transit key, the wrapped data key stored next to the principal's file). Keys are held
//!   as [`shogun_memory::DbKey`] and never rendered.
//! - [`PrincipalStore`]: `principal_id → <root>/<id>/memory.db`, opened with the daemon's own
//!   [`crate::daemon::Db`] / [`crate::daemon::DaemonDb`] machinery, with an LRU of open handles
//!   and a hard cap on how many stay open.
//! - [`PrincipalResolver`]: bearer token → principal + actor. [`TokenPrincipalMap`] is the
//!   sidecar's own in-memory map (`SHOGUN_PRINCIPAL_TOKENS`); `apps/api` replaces it with real
//!   agent keys in E3.
//! - [`derived_bearer`]: the gateway's derived `sgw1.<principal>.<key_id>.<mac>` bearers, verified
//!   against `SHOGUN_SIDECAR_BEARER_SECRET` (E34, threat model R4); with it no principal token is
//!   listed in the sidecar's environment.
//! - [`import`]: a ShogunAI single-user `memory.db` becomes a principal's file (E2-T6).
//! - [`dream_tick`]: the hourly Dream Cycle per principal, only when new material exists (E5-T4;
//!   `shogun-api --dream-tick`).
//! - [`credentials`]: the principal's token store on a local server (sealed files keyed by the
//!   principal's own key) and the relay-credential lookup that picks the tick's lane (E5-T5).
//! - [`multi`] (feature `daemon-server`): the `shogun-api --multi` composition.
//! - [`team`]: the tenant's teams (E18). A principal grants a teammate read access to its
//!   `world` state in its own file (`principal_grants`, V101); the sidecar checks both before it
//!   opens that file for the teammate.

pub mod connector_sync;
pub mod credentials;
pub mod derived_bearer;
pub mod dream_tick;
pub mod import;
pub mod key_provider;
#[cfg(feature = "daemon-server")]
pub mod multi;
#[cfg(feature = "daemon-server")]
pub mod processing_runtime;
pub mod principal;
pub mod resolver;
pub mod store;
pub mod team;

pub use derived_bearer::{ChainedResolver, DerivedBearers};
pub use import::{import_single_user_store, ImportReport};
pub use key_provider::{provider_from_env, EnvKeyProvider, KeyProvider, KeyProviderKind, KmsKeyProvider};
pub use principal::PrincipalId;
pub use resolver::{PrincipalResolver, ResolvedPrincipal, TokenPrincipalMap};
pub use store::{OpenPolicy, OpenPrincipal, PrincipalStore, PurgeReport};
pub use team::{Team, TeamRegistry};

/// What can go wrong around a principal's store. Never carries a key or a row.
#[derive(Debug, thiserror::Error)]
pub enum TenancyError {
    #[error("principal id must be 1-64 characters of [a-z0-9_-] and start with a letter or digit")]
    InvalidPrincipalId,
    #[error("no key for this principal: set SHOGUN_DB_KEY, or SHOGUN_DB_KEY_<PRINCIPAL> for it")]
    NoKey,
    /// Names the variable (suffix), never its value.
    #[error("{0} is not a 64-character hex key")]
    InvalidKey(String),
    #[error("the KMS key provider is not available in this build (it needs the `net` feature)")]
    KmsUnavailable,
    /// The KMS refused, failed or could not be reached. The store stays closed; nothing falls
    /// back to another key or to plaintext. Content-free (`KmsError` carries a status at most).
    #[error("the principal's key could not be unwrapped by the KMS: {0}")]
    Kms(shogun_integrations::token_store::KmsError),
    /// The wrapped-key file next to the principal's store is missing, unreadable or not this
    /// principal's.
    #[error("the principal's wrapped key: {0}")]
    WrappedKey(&'static str),
    /// A KMS provider setting is missing or malformed; names the variable, never its value.
    #[error("key provider configuration: {0} is missing or invalid")]
    KmsConfig(&'static str),
    #[error("principal store i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not open the principal's store: {0}")]
    Open(String),
    #[error("the principal's store is unavailable: {0}")]
    Unavailable(String),
    #[error("a store for this principal already exists")]
    AlreadyExists,
    #[error("the source store does not exist or is not a file")]
    SourceMissing,
    #[error("the source store has a live write-ahead log; close the app that holds it first")]
    SourceInUse,
    #[error("invalid principal token map: {0}")]
    InvalidTokenMap(&'static str),
    /// A request still holds the principal's store open; a purge waits for it (E39).
    #[error("the principal's store is in use by a request in flight; retry shortly")]
    Busy,
    /// The principal was purged (E39): a tombstone under the root keeps a still-mapped token from
    /// silently recreating an empty store. The operator lifts it on purpose
    /// (`shogun-api --reinstate-principal <id>`).
    #[error("this principal was deleted; an operator must reinstate it before it can be used again")]
    Purged,
}
