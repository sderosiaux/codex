//! Resolves the configured MCP OAuth store and pins that concrete source for one client lifecycle.

use anyhow::Context;
use anyhow::Result;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_keyring_store::KeyringStore;

use super::OAuthKeyringLoadError;
use super::OAuthStore;
use super::OAuthStoreLock;
use super::OAuthStoreLockFailure;
use super::StoredOAuthTokens;
use super::compute_store_key;
use super::delete_oauth_tokens_from_direct_keyring;
use super::delete_oauth_tokens_from_file;
use super::delete_oauth_tokens_from_secrets_keyring;
use super::load_oauth_tokens_from_file;
use super::load_oauth_tokens_from_file_with_lock_held;
use super::load_oauth_tokens_from_keyring;
use super::load_oauth_tokens_from_secrets_keyring_with_lock_held;
use super::save_oauth_tokens_to_file;
use super::save_oauth_tokens_with_keyring;

/// Concrete credential store resolved for one MCP OAuth client lifecycle.
///
/// This is intentionally not durable. `Auto` may resolve differently in a later process, but a
/// client that loaded credentials from one store must reread, refresh, persist, and remove only
/// through that store. A mid-lifecycle backend failure is unexpected and must return an error
/// rather than falling back to another possibly stale refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedOAuthCredentialStore {
    pub(super) backend: Backend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Backend {
    File,
    Keyring(AuthKeyringBackendKind),
}

impl ResolvedOAuthCredentialStore {
    pub(crate) fn file() -> Self {
        Self {
            backend: Backend::File,
        }
    }

    pub(crate) fn keyring(kind: AuthKeyringBackendKind) -> Self {
        Self {
            backend: Backend::Keyring(kind),
        }
    }

    /// Loads credentials only from this already-resolved authority.
    ///
    /// Unlike `resolve_oauth_tokens_from_store_policy`, this never evaluates configured
    /// `Auto` fallback policy.
    pub(crate) fn load<K: KeyringStore + Clone + 'static>(
        self,
        keyring_store: &K,
        server_name: &str,
        url: &str,
    ) -> Result<Option<StoredOAuthTokens>> {
        match self.backend {
            Backend::File => load_oauth_tokens_from_file(server_name, url)
                .context("failed to reread OAuth tokens from resolved file storage"),
            Backend::Keyring(keyring_backend_kind) => load_oauth_tokens_from_keyring(
                keyring_store,
                keyring_backend_kind,
                server_name,
                url,
            )
            .map_err(anyhow::Error::from)
            .context(
                "failed to reread OAuth tokens from resolved keyring storage; refusing file fallback",
            ),
        }
    }

    /// Reads the selected authority without waiting for its aggregate-store lock.
    pub(crate) fn try_load<K: KeyringStore + Clone + 'static>(
        self,
        keyring_store: &K,
        server_name: &str,
        url: &str,
    ) -> Result<Option<StoredOAuthTokens>> {
        match self.backend {
            Backend::File => {
                let _store_lock = OAuthStoreLock::try_acquire_for_read(OAuthStore::File)?;
                load_oauth_tokens_from_file_with_lock_held(server_name, url)
                    .context("failed to probe OAuth tokens from resolved file storage")
            }
            Backend::Keyring(AuthKeyringBackendKind::Direct) => {
                load_oauth_tokens_from_keyring(keyring_store, AuthKeyringBackendKind::Direct, server_name, url)
                    .map_err(anyhow::Error::from)
                    .context("failed to reread OAuth tokens from resolved keyring storage; refusing file fallback")
            }
            Backend::Keyring(AuthKeyringBackendKind::Secrets) => {
                let _store_lock = OAuthStoreLock::try_acquire_for_read(OAuthStore::Secrets)?;
                load_oauth_tokens_from_secrets_keyring_with_lock_held(
                    keyring_store,
                    server_name,
                    url,
                )
                .map_err(anyhow::Error::from)
            }
        }
    }

    /// Saves credentials only to this already-resolved authority.
    pub(crate) fn save<K: KeyringStore + Clone + 'static>(
        self,
        keyring_store: &K,
        server_name: &str,
        tokens: &StoredOAuthTokens,
    ) -> Result<()> {
        match self.backend {
            Backend::File => save_oauth_tokens_to_file(tokens),
            Backend::Keyring(keyring_backend_kind) => save_oauth_tokens_with_keyring(
                keyring_store,
                keyring_backend_kind,
                server_name,
                tokens,
            ),
        }
    }

    /// Deletes credentials only from this already-resolved authority.
    pub(crate) fn delete<K: KeyringStore + Clone + 'static>(
        self,
        keyring_store: &K,
        server_name: &str,
        url: &str,
    ) -> Result<bool> {
        match self.backend {
            Backend::File => compute_store_key(server_name, url)
                .and_then(|key| delete_oauth_tokens_from_file(&key)),
            Backend::Keyring(AuthKeyringBackendKind::Direct) => {
                delete_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)
            }
            Backend::Keyring(AuthKeyringBackendKind::Secrets) => {
                delete_oauth_tokens_from_secrets_keyring(keyring_store, server_name, url)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedOAuthTokens {
    pub(crate) tokens: StoredOAuthTokens,
    pub(crate) store: ResolvedOAuthCredentialStore,
}

pub(crate) fn resolve_oauth_tokens_from_store_policy<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<ResolvedOAuthTokens>> {
    match store_mode {
        OAuthCredentialsStoreMode::Auto => {
            // Auto remains keyring-first at lifecycle startup. The returned source is then pinned
            // by the client transport recipe and OAuth persistor so retries, recovery, and
            // refresh work cannot hot-switch stores.
            // TODO(stevenlee): Different processes can still resolve Auto to different stores
            // when keyring availability differs. Solving that safely requires durable backend
            // selection or reconciliation of legacy entries and is intentionally outside this
            // stack.
            let result = load_oauth_tokens_from_keyring(
                keyring_store,
                keyring_backend_kind,
                server_name,
                url,
            );
            match result {
                Ok(Some(tokens)) => Ok(Some(ResolvedOAuthTokens {
                    tokens,
                    store: ResolvedOAuthCredentialStore::keyring(keyring_backend_kind),
                })),
                Ok(None) => {
                    let result = load_oauth_tokens_from_file(server_name, url);
                    Ok(result?.map(|tokens| ResolvedOAuthTokens {
                        tokens,
                        store: ResolvedOAuthCredentialStore::file(),
                    }))
                }
                // Auto may fall back when the keyring backend is unavailable, but a Secrets
                // aggregate-lock failure means authority may be changing. Consulting File in
                // that state could replay credentials hidden behind a newer Secrets entry.
                Err(OAuthKeyringLoadError::StoreLock(error)) => Err(error.into()),
                Err(error) => {
                    let result = load_oauth_tokens_from_file(server_name, url);
                    Ok(result
                        .with_context(|| {
                            format!("failed to read OAuth tokens from keyring: {error}")
                        })?
                        .map(|tokens| ResolvedOAuthTokens {
                            tokens,
                            store: ResolvedOAuthCredentialStore::file(),
                        }))
                }
            }
        }
        OAuthCredentialsStoreMode::File => {
            let result = load_oauth_tokens_from_file(server_name, url);
            Ok(result?.map(|tokens| ResolvedOAuthTokens {
                tokens,
                store: ResolvedOAuthCredentialStore::file(),
            }))
        }
        OAuthCredentialsStoreMode::Keyring => {
            let result = load_oauth_tokens_from_keyring(
                keyring_store,
                keyring_backend_kind,
                server_name,
                url,
            );
            Ok(result
                .map_err(anyhow::Error::from)
                .context("failed to read OAuth tokens from keyring")?
                .map(|tokens| ResolvedOAuthTokens {
                    tokens,
                    store: ResolvedOAuthCredentialStore::keyring(keyring_backend_kind),
                }))
        }
    }
}

pub(crate) fn try_resolve_oauth_tokens_from_store_policy<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<ResolvedOAuthTokens>> {
    let load = |store: ResolvedOAuthCredentialStore| {
        let result = store.try_load(keyring_store, server_name, url);
        result.map(|tokens| tokens.map(|tokens| ResolvedOAuthTokens { tokens, store }))
    };
    let keyring = ResolvedOAuthCredentialStore::keyring(keyring_backend_kind);
    match store_mode {
        OAuthCredentialsStoreMode::File => load(ResolvedOAuthCredentialStore::file()),
        OAuthCredentialsStoreMode::Keyring => load(keyring),
        OAuthCredentialsStoreMode::Auto => match load(keyring) {
            Ok(Some(tokens)) => Ok(Some(tokens)),
            Ok(None) => load(ResolvedOAuthCredentialStore::file()),
            Err(error) if error.downcast_ref::<OAuthStoreLockFailure>().is_some() => Err(error),
            Err(error) => load(ResolvedOAuthCredentialStore::file())
                .with_context(|| format!("failed to read OAuth tokens from keyring: {error}")),
        },
    }
}
