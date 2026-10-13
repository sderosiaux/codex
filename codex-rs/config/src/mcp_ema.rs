//! Enterprise MCP configuration and account-scoped credential names.
//!
//! Only host, user, or managed configuration selects the IdP. Plugin declarations
//! and project settings may not redirect the enterprise credential source.

use std::io;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::ConfigLayerStack;
use crate::McpServerConfig;
use crate::McpServerTransportConfig;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerIdpOAuthConfig {
    /// Issuer used for enterprise OAuth discovery and identity validation.
    pub issuer: String,
    /// Public OAuth client registered with that enterprise IdP.
    pub client_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpEnterpriseManagedAuthConfig {
    /// Shared enterprise authorization, independent of Codex account credentials.
    pub idp: McpServerIdpOAuthConfig,
}

/// Immutable authorization selected after trusted policy and catalog resolution.
/// This cannot be deserialized from a server or plugin declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEmaRegistration {
    idp: McpServerIdpOAuthConfig,
    server_url: String,
    resource: Option<String>,
    client_id: Option<String>,
    authorization_server_issuer: Option<String>,
    scopes: Vec<String>,
}

impl McpEmaRegistration {
    pub fn idp(&self) -> &McpServerIdpOAuthConfig {
        &self.idp
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn resource(&self) -> Option<&str> {
        self.resource.as_deref()
    }

    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    pub fn authorization_server_issuer(&self) -> Option<&str> {
        self.authorization_server_issuer.as_deref()
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
}

/// Account identity that owns an enterprise OAuth credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpEmaAuthScope {
    user_id: String,
    workspace_id: String,
}

impl McpEmaAuthScope {
    pub fn new(user_id: String, workspace_id: String) -> Option<Self> {
        (!user_id.trim().is_empty() && !workspace_id.trim().is_empty()).then_some(Self {
            user_id,
            workspace_id,
        })
    }
}

impl McpServerIdpOAuthConfig {
    /// Shared by allowed MCP resources, but never by another account or IdP client.
    pub fn credential_name(&self, scope: &McpEmaAuthScope) -> String {
        let mut digest = Sha256::new();
        digest.update(b"codex-ema-idp-credential-v1\0");
        for part in [
            &scope.user_id,
            &scope.workspace_id,
            &self.issuer,
            &self.client_id,
        ] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        format!("ema-idp:{}", URL_SAFE_NO_PAD.encode(digest.finalize()))
    }
}

impl McpEnterpriseManagedAuthConfig {
    /// Selects one complete registration, from the highest-precedence layer that defines
    /// one, rather than merging issuer/client pairs across layers.
    pub fn resolve(stack: &ConfigLayerStack, fallback: Option<&Self>) -> io::Result<Option<Self>> {
        if stack.layers_high_to_low().next().is_none() {
            return Ok(fallback.cloned());
        }
        stack
            .layers_high_to_low()
            .find_map(|layer| layer.config.get("mcp_enterprise_managed_auth"))
            .map(|section| {
                section.clone().try_into().map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("enterprise IdP must be complete in one config layer: {error}"),
                    )
                })
            })
            .transpose()
    }
}

impl McpServerConfig {
    pub fn oauth_idp(&self) -> Option<&McpServerIdpOAuthConfig> {
        self.ema_registration().map(McpEmaRegistration::idp)
    }

    pub fn ema_registration(&self) -> Option<&McpEmaRegistration> {
        self.oauth.as_ref()?.ema_registration.as_ref()
    }

    /// Called once on a materialized catalog after configuration provenance is checked.
    pub fn resolve_ema_registration(
        &mut self,
        idp: &McpServerIdpOAuthConfig,
    ) -> Result<(), &'static str> {
        if let Some(oauth) = &mut self.oauth {
            oauth.ema_registration = None;
        }
        if !matches!(self.auth, crate::McpServerAuth::EmaAuth) {
            return Err("enterprise registration requires ema_auth");
        }
        self.validate_ema_auth_transport()?;
        let McpServerTransportConfig::StreamableHttp { url, .. } = &self.transport else {
            unreachable!("EMA transport was validated");
        };
        let oauth = self.oauth.get_or_insert_default();
        oauth.ema_registration = Some(McpEmaRegistration {
            idp: idp.clone(),
            server_url: url.clone(),
            resource: self.oauth_resource.clone(),
            client_id: oauth.client_id.clone(),
            authorization_server_issuer: oauth.authorization_server_issuer.clone(),
            scopes: self.scopes.clone().unwrap_or_default(),
        });
        Ok(())
    }

    /// EMA never falls back to an unrelated bearer or executor-owned credential.
    pub fn validate_ema_auth_transport(&self) -> Result<(), &'static str> {
        if !self.is_local_environment() {
            return Err("ema_auth requires a host-owned MCP connection");
        }
        if self.oauth_client_secret().is_some() {
            return Err("ema_auth cannot be combined with oauth.client_secret");
        }
        let McpServerTransportConfig::StreamableHttp {
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            http_headers_helper,
            ..
        } = &self.transport
        else {
            return Err("ema_auth requires streamable HTTP");
        };
        if bearer_token_env_var.is_some()
            || http_headers
                .as_ref()
                .is_some_and(|headers| !headers.is_empty())
            || env_http_headers
                .as_ref()
                .is_some_and(|headers| !headers.is_empty())
            || http_headers_helper.is_some()
        {
            return Err("ema_auth cannot be combined with alternate HTTP credentials");
        }
        Ok(())
    }
}
