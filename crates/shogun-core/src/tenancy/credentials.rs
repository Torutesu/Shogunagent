//! A principal's credentials on the multi-principal server (plan E5-T5).
//!
//! The values live in a [`shogun_integrations::TokenStore`]; this module gives the server its
//! local implementation and the lookups the server needs:
//!
//! - [`KeyProviderSeal`]: the file-sealed store's key source over the principal's own
//!   [`KeyProvider`] key (HKDF-derived, never the SQLCipher key itself), so the token files of a
//!   principal open only with that principal's key.
//! - [`local_store`]: `<SHOGUN_TOKEN_STORE_DIR | <root>/.tokens>` sealed with that key. The
//!   leading dot keeps the directory out of [`super::dream_tick::principals_on_disk`] (it is not a
//!   valid principal id).
//! - [`try_relay_bearer`]: the Batch relay bearer a principal's hourly Dream Cycle presents,
//!   distinguishing an absent credential from a storage error so processing fails closed.
//!   [`relay_bearer`] remains a compatibility lookup that collapses either case to `None`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use shogun_integrations::token_store::{FileSealedTokenStore, SealKey, SealKeySource};
use shogun_integrations::{CredentialSlot, TokenStore, TokenStoreError};

use super::{KeyProvider, PrincipalId};
use crate::llm::LicenseBearer;

/// The environment variable that overrides where the local token store keeps its files.
pub const TOKEN_STORE_DIR_VAR: &str = "SHOGUN_TOKEN_STORE_DIR";

/// The store directory under the multi-principal root when [`TOKEN_STORE_DIR_VAR`] is unset.
pub const DEFAULT_TOKEN_DIR: &str = ".tokens";

/// Seal keys derived from each principal's [`KeyProvider`] key.
#[derive(Clone)]
pub struct KeyProviderSeal {
    keys: Arc<dyn KeyProvider>,
}

impl KeyProviderSeal {
    pub fn new(keys: Arc<dyn KeyProvider>) -> Self {
        Self { keys }
    }
}

impl SealKeySource for KeyProviderSeal {
    fn seal_key(&self, principal: &str) -> Result<SealKey, TokenStoreError> {
        let id = PrincipalId::parse(principal).map_err(|_| TokenStoreError::InvalidPrincipal)?;
        let key = self.keys.key_for(&id).map_err(|_| TokenStoreError::Locked)?;
        key.with_bytes(|bytes| SealKey::derive(bytes, principal))
    }
}

/// Where the local token store lives for the principals under `root`.
pub fn token_dir(root: &Path, override_dir: Option<PathBuf>) -> PathBuf {
    override_dir.unwrap_or_else(|| root.join(DEFAULT_TOKEN_DIR))
}

/// The local Linux server's token store: sealed files keyed by each principal's store key.
pub fn local_store(dir: PathBuf, keys: Arc<dyn KeyProvider>) -> FileSealedTokenStore<KeyProviderSeal> {
    FileSealedTokenStore::new(dir, KeyProviderSeal::new(keys))
}

/// The relay bearer configured for `principal`, if any. Absent, empty, locked or unreadable all
/// answer `None`. Processing uses [`try_relay_bearer`] to distinguish storage failure from absence.
pub fn relay_bearer(store: &dyn TokenStore, principal: &PrincipalId) -> Option<LicenseBearer> {
    try_relay_bearer(store, principal).ok().flatten()
}

/// Processing must distinguish an absent credential from a locked or corrupt store. A storage
/// failure must never consume pending model work using local rules instead.
pub fn try_relay_bearer(store: &dyn TokenStore, principal: &PrincipalId) -> Result<Option<LicenseBearer>, TokenStoreError> {
    Ok(store.get(principal.as_str(), CredentialSlot::BatchRelay)?
        .filter(|secret| !secret.is_empty()).map(LicenseBearer::new))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::llm::Secret;
    use crate::tenancy::EnvKeyProvider;
    use shogun_memory::DbKey;

    #[test]
    fn the_bearer_is_sealed_per_principal_key_and_read_back() {
        let dir = std::env::temp_dir().join(format!("shogun_credentials_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let alice = PrincipalId::parse("alice").unwrap();
        let bob = PrincipalId::parse("bob").unwrap();
        let keys: Arc<dyn KeyProvider> = Arc::new(
            EnvKeyProvider::with_default(DbKey::new([1; 32])).with_principal_key(&alice, DbKey::new([2; 32])),
        );
        let store = local_store(dir.clone(), keys);
        assert!(relay_bearer(&store, &alice).is_none());
        store.put("alice", CredentialSlot::BatchRelay, &Secret::new("v1.alice-token")).unwrap();
        let bearer = relay_bearer(&store, &alice).expect("configured");
        assert_eq!(bearer.secret(), &Secret::new("v1.alice-token"));
        assert!(relay_bearer(&store, &bob).is_none(), "bob has no bearer of his own");

        // Sealed under alice's key: a store that resolves another key for her cannot open it,
        // and that reads as "no bearer" (local fallback), not as an error that sends anything.
        let other: Arc<dyn KeyProvider> = Arc::new(EnvKeyProvider::with_default(DbKey::new([3; 32])));
        assert!(try_relay_bearer(&local_store(dir.clone(), other.clone()), &alice).is_err());
        assert!(relay_bearer(&local_store(dir.clone(), other), &alice).is_none());

        // An empty value is not a credential.
        store.put("bob", CredentialSlot::BatchRelay, &Secret::new("")).unwrap();
        assert!(relay_bearer(&store, &bob).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_directory_is_not_a_principal() {
        let dir = token_dir(Path::new("/srv/principals"), None);
        assert_eq!(dir, Path::new("/srv/principals/.tokens"));
        assert!(PrincipalId::parse(DEFAULT_TOKEN_DIR).is_err());
        assert_eq!(token_dir(Path::new("/x"), Some(PathBuf::from("/y"))), Path::new("/y"));
    }
}
