mod config;
mod discovery;
mod signing;
mod transport;

use std::sync::Arc;
use std::time::SystemTime;

use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use bytes::Bytes;
use http::HeaderMap;
use http::Method;
use thiserror::Error;

pub use discovery::AwsProfile;
pub use discovery::discover_aws_profiles;
pub use discovery::validate_aws_profile;

/// AWS auth configuration used to resolve credentials and sign requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsAuthConfig {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub service: String,
}

/// Static AWS access keys supplied by a caller instead of the default SDK chain.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsAccessKeys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsAccessKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsAccessKeys")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Supplies AWS access keys on demand without exposing AWS SDK credential types to callers.
///
/// Implementations should return current credentials for every call so request signing can
/// observe credential refreshes. Errors must not contain credentials or command output.
pub trait AwsCredentialsProvider: std::fmt::Debug + Send + Sync {
    fn credentials(
        &self,
    ) -> impl std::future::Future<Output = std::io::Result<AwsAccessKeys>> + Send;
}

#[derive(Debug)]
struct AwsCredentialsProviderAdapter<P>(Arc<P>);

#[derive(Debug, Error)]
#[error("{0}")]
struct ProvidedCredentialsError(std::io::Error);

impl<P: AwsCredentialsProvider> ProvideCredentials for AwsCredentialsProviderAdapter<P> {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::new(async move {
            let access_keys = self.0.credentials().await.map_err(|error| {
                let error = ProvidedCredentialsError(error);
                match error.0.kind() {
                    std::io::ErrorKind::InvalidData
                    | std::io::ErrorKind::InvalidInput
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied => {
                        aws_credential_types::provider::error::CredentialsError::invalid_configuration(error)
                    }
                    _ => aws_credential_types::provider::error::CredentialsError::provider_error(error),
                }
            })?;

            Ok(aws_credential_types::Credentials::new(
                access_keys.access_key_id,
                access_keys.secret_access_key,
                access_keys.session_token,
                /*expires_after*/ None,
                "codex-bedrock-credential-export",
            ))
        })
    }
}

/// Generic HTTP request shape consumed by SigV4 signing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsRequestToSign {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Signed request parts returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsSignedRequest {
    pub url: String,
    pub headers: HeaderMap,
}

/// Errors returned by credential loading or SigV4 signing.
#[derive(Debug, Error)]
pub enum AwsAuthError {
    #[error("AWS service name must not be empty")]
    EmptyService,
    #[error("AWS profile must be configured")]
    MissingProfile,
    #[error("AWS SDK config did not resolve a credentials provider")]
    MissingCredentialsProvider,
    #[error("AWS SDK config did not resolve a region")]
    MissingRegion,
    #[error("failed to load AWS profiles: {0}")]
    ProfileLoad(#[from] aws_config::profile::ProfileFileLoadError),
    #[error("failed to load AWS credentials: {0}")]
    Credentials(#[from] aws_credential_types::provider::error::CredentialsError),
    #[error("request URL is not a valid URI: {0}")]
    InvalidUri(#[source] http::uri::InvalidUri),
    #[error("failed to construct HTTP request for signing: {0}")]
    BuildHttpRequest(#[source] http::Error),
    #[error("request contains a non-UTF8 header value: {0}")]
    InvalidHeaderValue(#[source] http::header::ToStrError),
    #[error("failed to build signable request: {0}")]
    SigningRequest(#[source] aws_sigv4::http_request::SigningError),
    #[error("failed to build SigV4 signing params: {0}")]
    SigningParams(String),
    #[error("SigV4 signing failed: {0}")]
    SigningFailure(#[source] aws_sigv4::http_request::SigningError),
}

/// Loaded AWS auth context that can sign outbound HTTP requests.
#[derive(Clone)]
pub struct AwsAuthContext {
    credentials_provider: SharedCredentialsProvider,
    region: String,
    service: String,
}

impl std::fmt::Debug for AwsAuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsAuthContext")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl AwsAuthContext {
    pub async fn load(
        config: AwsAuthConfig,
        factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, AwsAuthError> {
        let sdk_config = config::load_sdk_config(&config, &factory).await?;
        Ok(Self {
            credentials_provider: config::credentials_provider(&sdk_config)?,
            region: config::resolved_region(&sdk_config)?,
            service: config.service.trim().to_string(),
        })
    }

    pub async fn load_with_access_keys(
        config: AwsAuthConfig,
        access_keys: AwsAccessKeys,
        factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, AwsAuthError> {
        if config.service.trim().is_empty() {
            return Err(AwsAuthError::EmptyService);
        }
        let region = match config.region.as_ref() {
            Some(region) => region.clone(),
            None => config::resolved_region(&config::load_sdk_config(&config, &factory).await?)?,
        };
        Ok(Self {
            credentials_provider: SharedCredentialsProvider::new(
                aws_credential_types::Credentials::new(
                    access_keys.access_key_id,
                    access_keys.secret_access_key,
                    access_keys.session_token,
                    /*expires_after*/ None,
                    "codex-managed-bedrock-access-keys",
                ),
            ),
            region,
            service: config.service.trim().to_string(),
        })
    }

    pub async fn load_with_credentials_provider(
        config: AwsAuthConfig,
        provider: Arc<impl AwsCredentialsProvider + 'static>,
        factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, AwsAuthError> {
        let mut context = Self::load(config, factory).await?;
        context.credentials_provider =
            SharedCredentialsProvider::new(AwsCredentialsProviderAdapter(provider));
        Ok(context)
    }

    pub async fn load_profile(
        config: AwsAuthConfig,
        factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, AwsAuthError> {
        let profile = config.profile.clone().ok_or(AwsAuthError::MissingProfile)?;
        let mut context = Self::load(config, factory.clone()).await?;
        context.credentials_provider = SharedCredentialsProvider::new(
            discovery::profile_credentials_provider(&profile, &context.region, factory),
        );
        Ok(context)
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub async fn sign(&self, request: AwsRequestToSign) -> Result<AwsSignedRequest, AwsAuthError> {
        self.sign_at(request, SystemTime::now()).await
    }

    async fn sign_at(
        &self,
        request: AwsRequestToSign,
        time: SystemTime,
    ) -> Result<AwsSignedRequest, AwsAuthError> {
        let credentials = self.credentials_provider.provide_credentials().await?;
        signing::sign_request(&credentials, &self.region, &self.service, request, time)
    }
}

impl AwsAuthError {
    /// Returns the caller-supplied credential error without exposing SDK error sources.
    pub fn credentials_provider_error(&self) -> Option<&std::io::Error> {
        let Self::Credentials(error) = self else {
            return None;
        };
        std::error::Error::source(error)?
            .downcast_ref::<ProvidedCredentialsError>()
            .map(|error| &error.0)
    }

    /// Returns whether retrying the outbound request can reasonably recover from this auth error.
    pub fn is_retryable(&self) -> bool {
        match self {
            AwsAuthError::Credentials(error) => matches!(
                error,
                aws_credential_types::provider::error::CredentialsError::ProviderTimedOut(_)
                    | aws_credential_types::provider::error::CredentialsError::ProviderError(_)
            ),
            AwsAuthError::EmptyService
            | AwsAuthError::MissingProfile
            | AwsAuthError::MissingCredentialsProvider
            | AwsAuthError::MissingRegion
            | AwsAuthError::ProfileLoad(_)
            | AwsAuthError::InvalidUri(_)
            | AwsAuthError::BuildHttpRequest(_)
            | AwsAuthError::InvalidHeaderValue(_)
            | AwsAuthError::SigningRequest(_)
            | AwsAuthError::SigningParams(_)
            | AwsAuthError::SigningFailure(_) => false,
        }
    }
}
