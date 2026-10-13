//! This file handles all logic related to managing MCP OAuth credentials.
//! All credentials are stored using the keyring crate which uses os-specific keyring services.
//! https://crates.io/crates/keyring
//! macOS: macOS keychain.
//! Windows: Windows Credential Manager
//! Linux: DBus-based Secret Service, the kernel keyutils, and a combo of the two
//! FreeBSD, OpenBSD: DBus-based Secret Service
//!
//! For Linux, we use linux-native-async-persistent which uses both keyutils and async-secret-service (see below) for storage.
//! See the docs for the keyutils_persistent module for a full explanation of why both are used. Because this store uses the
//! async-secret-service, you must specify the additional features required by that store
//!
//! async-secret-service provides access to the DBus-based Secret Service storage on Linux, FreeBSD, and OpenBSD. This is an asynchronous
//! keystore that always encrypts secrets when they are transferred across the bus. If DBus isn't installed the keystore will fall back to the json
//! file because we don't use the "vendored" feature.
//!
//! If the keyring is not available or fails, we fall back to CODEX_HOME/.credentials.json which is consistent with other coding CLI agents.

mod credential_store;
mod ema_identity;
mod enterprise_generation;
mod issuer_binding;
mod refresh_lock;
mod refresh_transaction;
mod resolved_store;
mod runtime;
mod store_lock;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use oauth2::AccessToken;
use oauth2::RefreshToken;
use oauth2::Scope;
use oauth2::TokenResponse;
use oauth2::basic::BasicTokenType;
use rmcp::transport::auth::OAuthTokenResponse;
use rmcp::transport::auth::VendorExtraTokenFields;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::map::Map as JsonMap;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tracing::warn;

use self::store_lock::OAuthStore;
use self::store_lock::OAuthStoreLock;
use self::store_lock::OAuthStoreLockFailure;

use codex_config::McpServerOAuthConfig;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use rmcp::transport::auth::AuthorizationManager;
use tokio::sync::Mutex;

use codex_utils_home_dir::find_codex_home;

pub(crate) use self::credential_store::OAuthCredentialStore;
pub use self::ema_identity::EmaCredentialLease;
pub use self::ema_identity::EmaCredentialSnapshot;
pub(crate) use self::ema_identity::stored_oidc_identity;
pub(crate) use self::enterprise_generation::EnterpriseOAuthGeneration;
pub(crate) use self::enterprise_generation::EnterpriseOAuthGenerationFile;
pub(crate) use self::enterprise_generation::EnterpriseOAuthGenerationKind;
use self::enterprise_generation::invalidate_enterprise_credential_version;
pub(crate) use self::issuer_binding::validate_authorization_server_endpoints;
pub(crate) use self::issuer_binding::validate_refresh_token_issuer;
pub(crate) use self::refresh_lock::RefreshCredentialLock;
pub(crate) use self::refresh_transaction::install_tokens_in_manager;
pub(crate) use self::resolved_store::ResolvedOAuthCredentialStore;
pub(crate) use self::resolved_store::ResolvedOAuthTokens;
pub(crate) use self::resolved_store::resolve_oauth_tokens_from_store_policy;
use self::resolved_store::try_resolve_oauth_tokens_from_store_policy;
pub(crate) use self::runtime::OAuthRuntime;

const KEYRING_SERVICE: &str = "Codex MCP Credentials";
const MCP_OAUTH_SECRET_PREFIX: &str = "MCP_OAUTH";
const REFRESH_SKEW_MILLIS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredOAuthTokens {
    pub server_name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    pub client_id: String,
    pub token_response: WrappedOAuthTokenResponse,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl StoredOAuthTokens {
    pub(crate) fn has_refresh_token(&self) -> bool {
        self.token_response
            .0
            .refresh_token()
            .is_some_and(|refresh_token| !refresh_token.secret().trim().is_empty())
    }

    pub(crate) fn bound_issuer(&self) -> Option<&str> {
        self.issuer
            .as_deref()
            .filter(|issuer| !issuer.trim().is_empty())
    }

    pub(crate) fn access_token_is_usable_without_refresh(&self) -> bool {
        !token_needs_refresh(self.expires_at)
            && !self
                .token_response
                .0
                .access_token()
                .secret()
                .trim()
                .is_empty()
    }
}

/// OAuth credentials paired with the concrete store selected for their client lifecycle.
#[derive(Debug, Clone)]
pub struct StoredOAuthCredentialSnapshot {
    credentials: StoredOAuthTokens,
    store: ResolvedOAuthCredentialStore,
    store_was_contended: bool,
}

impl PartialEq for StoredOAuthCredentialSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.credentials == other.credentials && self.store == other.store
    }
}

impl StoredOAuthCredentialSnapshot {
    pub(crate) fn new(
        mut credentials: StoredOAuthTokens,
        store: ResolvedOAuthCredentialStore,
    ) -> Self {
        credentials.token_response.0.set_expires_in(None);
        Self {
            credentials,
            store,
            store_was_contended: false,
        }
    }

    /// Returns the normalized credentials originally read from the selected store.
    pub fn credentials(&self) -> &StoredOAuthTokens {
        &self.credentials
    }

    /// Returns whether this snapshot was retained because its store could not be read.
    pub fn store_was_contended(&self) -> bool {
        self.store_was_contended
    }

    /// Refreshes a runtime snapshot without waiting or discarding its last known authority.
    pub fn for_runtime_refresh(
        previous: Option<&Self>,
        server_name: &str,
        url: &str,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Result<Option<Self>> {
        match try_resolve_oauth_tokens_from_store_policy(
            &DefaultKeyringStore,
            server_name,
            url,
            store_mode,
            keyring_backend_kind,
        ) {
            Ok(Some(mut resolved)) => {
                resolved.tokens.token_response.0.set_expires_in(None);
                Ok(Some(Self {
                    credentials: resolved.tokens,
                    store: resolved.store,
                    store_was_contended: false,
                }))
            }
            Ok(None) => Ok(None),
            Err(error) if oauth_store_is_contended(&error) => Ok(previous
                .filter(|previous| {
                    previous.credentials.server_name == server_name
                        && previous.credentials.url == url
                })
                .map(|previous| Self {
                    store_was_contended: true,
                    ..previous.clone()
                })),
            Err(error) => Err(error),
        }
    }

    /// Rereads the selected authority without waiting for a contended credential-store lock.
    pub fn reload(
        &self,
        server_name: &str,
        url: &str,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Result<Option<StoredOAuthTokens>> {
        if self.store == ResolvedOAuthCredentialStore::file()
            && store_mode == OAuthCredentialsStoreMode::Auto
        {
            return Self::for_runtime_refresh(
                /*previous*/ None,
                server_name,
                url,
                store_mode,
                keyring_backend_kind,
            )
            .map(|snapshot| snapshot.map(|snapshot| snapshot.credentials));
        }

        let result = self.store.try_load(&DefaultKeyringStore, server_name, url);
        let credentials = match result {
            Ok(credentials) => credentials,
            Err(error) if oauth_store_is_contended(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(normalized_oauth_credentials(credentials.as_ref()))
    }
}

/// Wrap OAuthTokenResponse to allow for partial equality comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrappedOAuthTokenResponse(pub OAuthTokenResponse);

impl PartialEq for WrappedOAuthTokenResponse {
    fn eq(&self, other: &Self) -> bool {
        match (serde_json::to_value(self), serde_json::to_value(other)) {
            (Ok(s1), Ok(s2)) => s1 == s2,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoredOAuthTokenStatus {
    Missing,
    Usable,
    AuthorizationRequired,
}

pub(crate) fn oauth_token_status(
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<StoredOAuthTokenStatus> {
    let resolved = resolve_oauth_tokens_from_store_policy(
        &DefaultKeyringStore,
        server_name,
        url,
        store_mode,
        keyring_backend_kind,
    )?;
    Ok(match resolved.as_ref().map(|resolved| &resolved.tokens) {
        None => StoredOAuthTokenStatus::Missing,
        Some(tokens) if oauth_tokens_are_usable(tokens) => StoredOAuthTokenStatus::Usable,
        Some(_) => StoredOAuthTokenStatus::AuthorizationRequired,
    })
}

/// Returns stored OAuth credentials without their derived expiration interval.
pub fn stored_oauth_credentials(
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<StoredOAuthTokens>> {
    Ok(
        stored_oauth_credential_snapshot(server_name, url, store_mode, keyring_backend_kind)?
            .map(|snapshot| snapshot.credentials),
    )
}

/// Loads OAuth credentials together with the concrete authority selected by store policy.
pub fn stored_oauth_credential_snapshot(
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<StoredOAuthCredentialSnapshot>> {
    let Some(resolved) = resolve_oauth_tokens_from_store_policy(
        &DefaultKeyringStore,
        server_name,
        url,
        store_mode,
        keyring_backend_kind,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(StoredOAuthCredentialSnapshot::new(
        resolved.tokens,
        resolved.store,
    )))
}

fn oauth_store_is_contended(error: &Error) -> bool {
    matches!(
        error.downcast_ref::<OAuthStoreLockFailure>(),
        Some(OAuthStoreLockFailure::Timeout { acquire_timeout, .. })
            if acquire_timeout.is_zero()
    )
}

fn normalized_oauth_credentials(tokens: Option<&StoredOAuthTokens>) -> Option<StoredOAuthTokens> {
    tokens.map(|tokens| {
        let mut tokens = tokens.clone();
        tokens.token_response.0.set_expires_in(None);
        tokens
    })
}

fn oauth_tokens_are_usable(tokens: &StoredOAuthTokens) -> bool {
    if tokens.client_id.trim().is_empty() {
        return false;
    }

    if token_needs_refresh(tokens.expires_at) {
        return tokens.bound_issuer().is_some() && tokens.has_refresh_token();
    }

    tokens.access_token_is_usable_without_refresh()
}

fn refresh_expires_in_from_timestamp(tokens: &mut StoredOAuthTokens) {
    let Some(expires_at) = tokens.expires_at else {
        return;
    };

    match expires_in_from_timestamp(expires_at) {
        Some(seconds) => {
            let duration = Duration::from_secs(seconds);
            tokens.token_response.0.set_expires_in(Some(&duration));
        }
        None => {
            // RMCP treats a missing expiry as unknown and uses the access token
            // as-is. Treat a known-expired timestamp as an explicit zero so
            // startup refreshes the token before the first request.
            tokens
                .token_response
                .0
                .set_expires_in(Some(&Duration::ZERO));
        }
    }
}

fn load_oauth_tokens_from_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> std::result::Result<Option<StoredOAuthTokens>, OAuthKeyringLoadError> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            load_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)
                .map_err(OAuthKeyringLoadError::Backend)
        }
        AuthKeyringBackendKind::Secrets => {
            load_oauth_tokens_from_secrets_keyring(keyring_store, server_name, url)
        }
    }
}

fn load_oauth_tokens_from_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    let key = compute_store_key(server_name, url)?;
    match keyring_store.load(KEYRING_SERVICE, &key) {
        Ok(Some(serialized)) => {
            let mut tokens: StoredOAuthTokens = serde_json::from_str(&serialized)
                .context("failed to deserialize OAuth tokens from keyring")?;
            refresh_expires_in_from_timestamp(&mut tokens);
            Ok(Some(tokens))
        }
        Ok(None) => Ok(None),
        Err(error) => Err(Error::new(std::io::Error::from(error))),
    }
}

fn load_oauth_tokens_from_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> std::result::Result<Option<StoredOAuthTokens>, OAuthKeyringLoadError> {
    let _store_lock = OAuthStoreLock::acquire_for_read(OAuthStore::Secrets)?;
    load_oauth_tokens_from_secrets_keyring_with_lock_held(keyring_store, server_name, url)
}

fn load_oauth_tokens_from_secrets_keyring_with_lock_held<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> std::result::Result<Option<StoredOAuthTokens>, OAuthKeyringLoadError> {
    let codex_home = find_codex_home().map_err(anyhow::Error::from)?;
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, url)?;
    match manager
        .get(&SecretScope::Global, &secret_name)
        .context("failed to load MCP OAuth tokens from encrypted storage")?
    {
        Some(serialized) => {
            let mut tokens: StoredOAuthTokens = serde_json::from_str(&serialized)
                .context("failed to deserialize OAuth tokens from encrypted storage")?;
            refresh_expires_in_from_timestamp(&mut tokens);
            Ok(Some(tokens))
        }
        None => Ok(None),
    }
}

/// Classifies keyring load failures that affect Auto fallback policy.
#[derive(Debug, thiserror::Error)]
enum OAuthKeyringLoadError {
    /// Store coordination failed, so consulting another authority would be unsafe.
    #[error(transparent)]
    StoreLock(#[from] OAuthStoreLockFailure),
    /// The selected keyring backend itself was unavailable or its data was invalid.
    #[error("{0}")]
    Backend(#[from] anyhow::Error),
}

pub async fn save_oauth_tokens(
    server_name: &str,
    tokens: &StoredOAuthTokens,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<()> {
    let lock = RefreshCredentialLock::acquire_for_server(server_name, &tokens.url).await?;
    save_oauth_tokens_with_lock_held(&lock, server_name, tokens, store_mode, keyring_backend_kind)
}

/// Save while retaining the matching credential lock acquired by the caller.
pub(crate) fn save_oauth_tokens_with_lock_held(
    lock: &RefreshCredentialLock,
    server_name: &str,
    tokens: &StoredOAuthTokens,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<()> {
    invalidate_enterprise_credential_version(server_name, &tokens.url, lock)?;
    let keyring_store = DefaultKeyringStore;
    match store_mode {
        OAuthCredentialsStoreMode::Auto => save_oauth_tokens_with_keyring_with_fallback_to_file(
            &keyring_store,
            keyring_backend_kind,
            server_name,
            tokens,
        ),
        OAuthCredentialsStoreMode::File => save_oauth_tokens_to_file(tokens),
        OAuthCredentialsStoreMode::Keyring => save_oauth_tokens_with_keyring_and_cleanup_file(
            &keyring_store,
            keyring_backend_kind,
            server_name,
            tokens,
        ),
    }
}

fn save_oauth_tokens_with_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    // This exact-store writer is used after a client resolves its authority. Only login-time
    // policy resolution may clean up or update the non-selected store.
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            save_oauth_tokens_to_direct_keyring(keyring_store, server_name, tokens)
        }
        AuthKeyringBackendKind::Secrets => {
            save_oauth_tokens_to_secrets_keyring(keyring_store, server_name, tokens)
        }
    }
}

fn save_oauth_tokens_to_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let serialized = serde_json::to_string(tokens).context("failed to serialize OAuth tokens")?;

    let key = compute_store_key(server_name, &tokens.url)?;
    keyring_store
        .save(KEYRING_SERVICE, &key, &serialized)
        .map_err(std::io::Error::from)
        .context("failed to write OAuth tokens to keyring")
}

/// Saves one credential while holding the Secrets aggregate-store lock across the mutation.
fn save_oauth_tokens_to_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let serialized = serde_json::to_string(tokens).context("failed to serialize OAuth tokens")?;
    let _store_lock = OAuthStoreLock::acquire_for_write(OAuthStore::Secrets)?;
    save_oauth_tokens_to_secrets_keyring_with_lock_held(
        keyring_store,
        server_name,
        tokens,
        &serialized,
    )
}

/// Writes one credential to Secrets. The caller must hold the Secrets aggregate-store lock.
fn save_oauth_tokens_to_secrets_keyring_with_lock_held<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    tokens: &StoredOAuthTokens,
    serialized: &str,
) -> Result<()> {
    let codex_home = find_codex_home()?;
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, &tokens.url)?;
    manager
        .set(&SecretScope::Global, &secret_name, serialized)
        .context("failed to write OAuth tokens to encrypted storage")
}

/// Saves to the selected keyring backend, then best-effort removes the fallback File entry.
fn save_oauth_tokens_with_keyring_and_cleanup_file<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    save_oauth_tokens_with_keyring(keyring_store, keyring_backend_kind, server_name, tokens)?;
    let key = compute_store_key(server_name, &tokens.url)?;
    if delete_oauth_tokens_from_file(&key).is_err() {
        warn!("failed to remove OAuth tokens from fallback storage");
    }
    Ok(())
}

fn save_oauth_tokens_with_keyring_with_fallback_to_file<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let result = save_oauth_tokens_with_keyring_and_cleanup_file(
        keyring_store,
        keyring_backend_kind,
        server_name,
        tokens,
    );
    match result {
        Ok(()) => Ok(()),
        // As on load, a store lock failure is a coordination failure rather than evidence that
        // the keyring backend is unavailable. Falling back could leave a newer File token hidden
        // behind a stale Secrets entry.
        Err(error) if error.downcast_ref::<OAuthStoreLockFailure>().is_some() => Err(error),
        Err(error) => {
            let message = error.to_string();
            let result = save_oauth_tokens_to_file(tokens);
            result.with_context(|| format!("failed to write OAuth tokens to keyring: {message}"))
        }
    }
}

pub async fn delete_oauth_tokens(
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<bool> {
    let lock = RefreshCredentialLock::acquire_for_server(server_name, url).await?;
    delete_oauth_tokens_with_lock_held(&lock, server_name, url, store_mode, keyring_backend_kind)
}

/// Delete while retaining the matching credential lock acquired by the caller.
pub(crate) fn delete_oauth_tokens_with_lock_held(
    lock: &RefreshCredentialLock,
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<bool> {
    invalidate_enterprise_credential_version(server_name, url, lock)?;
    let keyring_store = DefaultKeyringStore;
    delete_oauth_tokens_from_keyring_and_file(
        &keyring_store,
        store_mode,
        keyring_backend_kind,
        server_name,
        url,
    )
}

fn delete_oauth_tokens_from_keyring_and_file<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    compute_store_key(server_name, url).and_then(|key| {
        let keyring_result =
            delete_oauth_tokens_from_keyring(keyring_store, keyring_backend_kind, server_name, url);
        let keyring_removed = match keyring_result {
            Ok(removed) => removed,
            Err(error) => {
                warn!("failed to delete OAuth tokens from keyring");
                match store_mode {
                    OAuthCredentialsStoreMode::Auto | OAuthCredentialsStoreMode::Keyring => {
                        return Err(error).context("failed to delete OAuth tokens from keyring");
                    }
                    OAuthCredentialsStoreMode::File => false,
                }
            }
        };

        let file_removed = delete_oauth_tokens_from_file(&key)?;
        Ok(keyring_removed || file_removed)
    })
}

fn delete_oauth_tokens_from_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            delete_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)
        }
        AuthKeyringBackendKind::Secrets => {
            let direct_removed =
                delete_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)?;
            let secrets_removed =
                delete_oauth_tokens_from_secrets_keyring(keyring_store, server_name, url)?;
            Ok(direct_removed || secrets_removed)
        }
    }
}

fn delete_oauth_tokens_from_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    let key = compute_store_key(server_name, url)?;
    keyring_store
        .delete(KEYRING_SERVICE, &key)
        .map_err(|error| Error::new(std::io::Error::from(error)))
}

fn delete_oauth_tokens_from_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    let _store_lock = OAuthStoreLock::acquire_for_write(OAuthStore::Secrets)?;
    let codex_home = find_codex_home()?;
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, url)?;
    let secrets_removed = manager
        .delete(&SecretScope::Global, &secret_name)
        .context("failed to delete OAuth tokens from encrypted storage")?;
    Ok(secrets_removed)
}

#[derive(Clone)]
pub(crate) struct OAuthPersistor {
    inner: Arc<OAuthPersistorInner>,
}

struct OAuthPersistorInner {
    server_name: String,
    url: String,
    authorization_manager: Arc<Mutex<AuthorizationManager>>,
    credential_store: ResolvedOAuthCredentialStore,
    oauth_config: Option<McpServerOAuthConfig>,
    last_credentials: Mutex<Option<StoredOAuthTokens>>,
}

impl OAuthPersistor {
    pub(crate) fn new(
        server_name: String,
        url: String,
        authorization_manager: Arc<Mutex<AuthorizationManager>>,
        credential_store: ResolvedOAuthCredentialStore,
        initial_credentials: Option<StoredOAuthTokens>,
        oauth_config: Option<McpServerOAuthConfig>,
    ) -> Self {
        Self {
            inner: Arc::new(OAuthPersistorInner {
                server_name,
                url,
                authorization_manager,
                credential_store,
                last_credentials: Mutex::new(initial_credentials),
                oauth_config,
            }),
        }
    }

    pub(crate) async fn stored_credentials(&self) -> Option<StoredOAuthTokens> {
        let credentials = self.inner.last_credentials.lock().await;
        normalized_oauth_credentials(credentials.as_ref())
    }

    /// Persists RMCP-managed credential changes back to this client's resolved authority.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "AuthorizationManager async access must be serialized through its mutex"
    )]
    pub(crate) async fn persist_if_needed(&self) -> Result<()> {
        let (client_id, maybe_credentials) = {
            let manager = self.inner.authorization_manager.clone();
            let guard = manager.lock().await;
            guard.get_credentials().await
        }?;

        match maybe_credentials {
            Some(credentials) => {
                let mut last_credentials = self.inner.last_credentials.lock().await;
                let new_token_response = WrappedOAuthTokenResponse(credentials.clone());
                let same_token = last_credentials
                    .as_ref()
                    .map(|previous| previous.token_response == new_token_response)
                    .unwrap_or(false);
                let expires_at = if same_token {
                    last_credentials
                        .as_ref()
                        .and_then(|previous| previous.expires_at)
                } else {
                    compute_expires_at_millis(&credentials)
                };
                let stored = StoredOAuthTokens {
                    server_name: self.inner.server_name.clone(),
                    url: self.inner.url.clone(),
                    issuer: last_credentials
                        .as_ref()
                        .and_then(|previous| previous.issuer.clone()),
                    client_id,
                    token_response: new_token_response,
                    expires_at,
                };
                if last_credentials.as_ref() != Some(&stored) {
                    self.inner.credential_store.save(
                        &DefaultKeyringStore,
                        &self.inner.server_name,
                        &stored,
                    )?;
                    *last_credentials = Some(stored);
                }
            }
            None => {
                let mut last_credentials = self.inner.last_credentials.lock().await;
                if last_credentials.take().is_some()
                    && let Err(error) = self.inner.credential_store.delete(
                        &DefaultKeyringStore,
                        &self.inner.server_name,
                        &self.inner.url,
                    )
                {
                    warn!(
                        server_name = %self.inner.server_name,
                        error = %error,
                        "failed to remove MCP OAuth credentials from the resolved store"
                    );
                }
            }
        }

        Ok(())
    }
}

const FALLBACK_FILENAME: &str = ".credentials.json";
const MCP_SERVER_TYPE: &str = "http";

type FallbackFile = BTreeMap<String, FallbackTokenEntry>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FallbackTokenEntry {
    server_name: String,
    server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuer: Option<String>,
    client_id: String,
    access_token: String,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

fn load_oauth_tokens_from_file(server_name: &str, url: &str) -> Result<Option<StoredOAuthTokens>> {
    let _store_lock = OAuthStoreLock::acquire_for_read(OAuthStore::File)?;
    load_oauth_tokens_from_file_with_lock_held(server_name, url)
}

fn load_oauth_tokens_from_file_with_lock_held(
    server_name: &str,
    url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    let Some(store) = read_fallback_file_unlocked()? else {
        return Ok(None);
    };

    for entry in store.values() {
        if entry.server_url != url || entry.server_name != server_name {
            continue;
        }

        let mut token_response = OAuthTokenResponse::new(
            AccessToken::new(entry.access_token.clone()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );

        if let Some(refresh) = entry.refresh_token.clone() {
            token_response.set_refresh_token(Some(RefreshToken::new(refresh)));
        }

        let scopes = entry.scopes.clone();
        if !scopes.is_empty() {
            token_response.set_scopes(Some(scopes.into_iter().map(Scope::new).collect()));
        }

        let mut stored = StoredOAuthTokens {
            server_name: entry.server_name.clone(),
            url: entry.server_url.clone(),
            issuer: entry.issuer.clone(),
            client_id: entry.client_id.clone(),
            token_response: WrappedOAuthTokenResponse(token_response),
            expires_at: entry.expires_at,
        };
        refresh_expires_in_from_timestamp(&mut stored);

        return Ok(Some(stored));
    }

    Ok(None)
}

/// Saves one credential while holding the File aggregate-store lock across the full
/// read-modify-write operation.
fn save_oauth_tokens_to_file(tokens: &StoredOAuthTokens) -> Result<()> {
    let _store_lock = OAuthStoreLock::acquire_for_write(OAuthStore::File)?;
    save_oauth_tokens_to_file_with_lock_held(tokens)
}

/// Updates the fallback File. The caller must hold the File aggregate-store lock.
fn save_oauth_tokens_to_file_with_lock_held(tokens: &StoredOAuthTokens) -> Result<()> {
    let key = compute_store_key(&tokens.server_name, &tokens.url)?;
    let mut store = read_fallback_file_unlocked()?.unwrap_or_default();

    let token_response = &tokens.token_response.0;
    let expires_at = tokens
        .expires_at
        .or_else(|| compute_expires_at_millis(token_response));
    let refresh_token = token_response
        .refresh_token()
        .map(|token| token.secret().to_string());
    let scopes = token_response
        .scopes()
        .map(|s| s.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let entry = FallbackTokenEntry {
        server_name: tokens.server_name.clone(),
        server_url: tokens.url.clone(),
        issuer: tokens.issuer.clone(),
        client_id: tokens.client_id.clone(),
        access_token: token_response.access_token().secret().to_string(),
        expires_at,
        refresh_token,
        scopes,
    };

    store.insert(key, entry);
    write_fallback_file(&store)
}

fn delete_oauth_tokens_from_file(key: &str) -> Result<bool> {
    let _store_lock = OAuthStoreLock::acquire_for_write(OAuthStore::File)?;
    let mut store = match read_fallback_file_unlocked()? {
        Some(store) => store,
        None => return Ok(false),
    };

    let removed = store.remove(key).is_some();

    if removed {
        write_fallback_file(&store)?;
    }

    Ok(removed)
}

pub(crate) fn compute_expires_at_millis(response: &OAuthTokenResponse) -> Option<u64> {
    let expires_in = response.expires_in()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let expiry = now.checked_add(expires_in)?;
    let millis = expiry.as_millis();
    if millis > u128::from(u64::MAX) {
        Some(u64::MAX)
    } else {
        Some(millis as u64)
    }
}

fn expires_in_from_timestamp(expires_at: u64) -> Option<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let now_ms = now.as_millis() as u64;

    if expires_at <= now_ms {
        None
    } else {
        Some((expires_at - now_ms) / 1000)
    }
}

fn token_needs_refresh(expires_at: Option<u64>) -> bool {
    let Some(expires_at) = expires_at else {
        return false;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64;

    now.saturating_add(REFRESH_SKEW_MILLIS) >= expires_at
}

fn compute_store_key(server_name: &str, server_url: &str) -> Result<String> {
    let enterprise_owned = server_name.starts_with("ema-idp:");
    let mut payload = JsonMap::new();
    payload.insert(
        "type".to_string(),
        Value::String(MCP_SERVER_TYPE.to_string()),
    );
    payload.insert("url".to_string(), Value::String(server_url.to_string()));
    payload.insert("headers".to_string(), Value::Object(JsonMap::new()));
    let payload = if enterprise_owned {
        // The OS keyring is shared across homes. Keep enterprise sessions
        // isolated by Codex profile as well as authenticated user and workspace.
        let codex_home = find_codex_home()?;
        fs::create_dir_all(&codex_home)?;
        payload.insert(
            "codex_home".to_string(),
            serde_json::to_value(codex_home.as_path().canonicalize()?)?,
        );
        // Different binaries can enable different serde_json ordering features.
        serde_json::to_value(payload.into_iter().collect::<BTreeMap<_, _>>())?
    } else {
        Value::Object(payload)
    };
    let truncated = sha_256_prefix(&payload)?;
    Ok(format!("{server_name}|{truncated}"))
}

/// Derive a valid secret-store name from the MCP OAuth store key.
///
/// `compute_store_key` intentionally includes readable identity components and
/// punctuation, but `SecretName` only allows `A-Z`, `0-9`, and `_`.
/// Re-hashing keeps the secret key deterministic while satisfying that
/// restricted alphabet.
fn compute_secret_name(server_name: &str, server_url: &str) -> Result<SecretName> {
    let key = compute_store_key(server_name, server_url)?;
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:X}");
    SecretName::new(&format!("{MCP_OAUTH_SECRET_PREFIX}_{}", &hex[..32]))
}

fn fallback_file_path() -> Result<PathBuf> {
    Ok(find_codex_home()?.join(FALLBACK_FILENAME).to_path_buf())
}

fn read_fallback_file_unlocked() -> Result<Option<FallbackFile>> {
    let path = fallback_file_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).context(format!(
                "failed to read credentials file at {}",
                path.display()
            ));
        }
    };

    match serde_json::from_str::<FallbackFile>(&contents) {
        Ok(store) => Ok(Some(store)),
        Err(e) => Err(e).context(format!(
            "failed to parse credentials file at {}",
            path.display()
        )),
    }
}

fn open_fallback_file_for_write(path: &std::path::Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "credentials path is not a regular file"
    );
    Ok(file)
}

fn write_fallback_file(store: &FallbackFile) -> Result<()> {
    let path = fallback_file_path()?;

    if store.is_empty() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }

    let parent = path
        .parent()
        .context("credentials file path has no parent directory")?;
    fs::create_dir_all(parent)?;

    let serialized = serde_json::to_string(store)?;
    let mut file = open_fallback_file_for_write(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.set_len(/*size*/ 0)?;
    file.write_all(serialized.as_bytes())?;

    Ok(())
}

fn sha_256_prefix(value: &Value) -> Result<String> {
    let serialized =
        serde_json::to_string(&value).context("failed to serialize MCP OAuth key payload")?;
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let truncated = &hex[..16];
    Ok(truncated.to_string())
}
