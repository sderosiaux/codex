use std::collections::HashMap;
use std::future::Future;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

pub mod accessible;
mod app_info;
mod app_tool_policy;
mod connector_runtime;
mod directory_cache;
pub mod filter;
pub mod merge;
pub mod metadata;
mod metadata_store;
mod plugin_config;
mod runtime_projection;
mod snapshot;

pub use app_info::AppBranding;
pub use app_info::AppInfo;
pub use app_info::AppMetadata;
pub use app_info::AppReview;
pub use app_info::AppScreenshot;
pub use app_tool_policy::AppToolPolicyEvaluator;
pub use app_tool_policy::AppToolPolicyInput;
pub use app_tool_policy::app_is_enabled;
pub use app_tool_policy::apps_config_from_layer_stack;
pub use connector_runtime::ConnectorRuntimeContext;
pub use connector_runtime::ConnectorRuntimeContextKey;
pub use connector_runtime::ConnectorRuntimeFetchSource;
pub use connector_runtime::ConnectorRuntimeFetchTicket;
pub use connector_runtime::ConnectorRuntimeManager;
pub use connector_runtime::ConnectorRuntimePayload;
pub use connector_runtime::ConnectorRuntimeSnapshot;
pub use connector_runtime::connector_runtime_cache_path;
pub use connector_runtime::connector_runtime_context_key;
pub use directory_cache::ConnectorDirectoryCacheContext;
pub use metadata_store::ConnectorMetadata;
pub use metadata_store::ConnectorMetadataStore;
pub use metadata_store::ConnectorToolSummary;
pub use plugin_config::parse_plugin_app_config;
pub use plugin_config::parse_plugin_app_config_value;
pub use runtime_projection::ConnectorRuntimeTool;
pub use runtime_projection::InstalledConnectorRuntime;
pub use runtime_projection::connector_tool_is_synthetic;
pub use runtime_projection::installed_connector_runtime;
pub use snapshot::ConnectorSnapshot;
pub use snapshot::PluginConnectorSource;

pub const CONNECTORS_CACHE_TTL: Duration = Duration::from_secs(3600);
/// TTL for app/read metadata; it starts aligned with the connector directory cache.
pub const CONNECTOR_METADATA_CACHE_TTL: Duration = CONNECTORS_CACHE_TTL;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorDirectoryCacheKey {
    chatgpt_base_url: String,
    account_id: Option<String>,
    chatgpt_user_id: Option<String>,
    is_workspace_account: bool,
}

impl ConnectorDirectoryCacheKey {
    pub fn new(
        chatgpt_base_url: String,
        account_id: Option<String>,
        chatgpt_user_id: Option<String>,
        is_workspace_account: bool,
    ) -> Self {
        Self {
            chatgpt_base_url,
            account_id,
            chatgpt_user_id,
            is_workspace_account,
        }
    }
}

#[derive(Clone)]
struct CachedConnectorDirectory {
    key: ConnectorDirectoryCacheKey,
    expires_at: Instant,
    connectors: Vec<AppInfo>,
}

static CONNECTOR_DIRECTORY_CACHE: LazyLock<StdMutex<Option<CachedConnectorDirectory>>> =
    LazyLock::new(|| StdMutex::new(None));

#[derive(Debug, Deserialize)]
pub struct DirectoryListResponse {
    apps: Vec<DirectoryApp>,
    #[serde(alias = "nextToken")]
    next_token: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DirectoryApp {
    id: String,
    name: String,
    description: Option<String>,
    #[serde(alias = "appMetadata")]
    app_metadata: Option<AppMetadata>,
    branding: Option<AppBranding>,
    labels: Option<HashMap<String, String>>,
    #[serde(alias = "logoUrl")]
    logo_url: Option<String>,
    #[serde(alias = "logoUrlDark")]
    logo_url_dark: Option<String>,
    #[serde(alias = "iconAssets")]
    icon_assets: Option<HashMap<String, String>>,
    #[serde(alias = "iconDarkAssets")]
    icon_dark_assets: Option<HashMap<String, String>>,
    #[serde(alias = "distributionChannel")]
    distribution_channel: Option<String>,
    visibility: Option<String>,
}

pub fn cached_directory_connectors(
    cache_context: &ConnectorDirectoryCacheContext,
) -> Option<Vec<AppInfo>> {
    if let Some(cached_connectors) = cached_directory_connectors_in_memory(&cache_context.cache_key)
    {
        return Some(cached_connectors);
    }

    let directory_cache::CachedConnectorDirectoryDiskLoad::Hit { connectors } =
        directory_cache::load_cached_directory_connectors_from_disk(cache_context)
    else {
        return None;
    };
    write_cached_directory_connectors_in_memory(
        cache_context.cache_key.clone(),
        &connectors,
        Duration::ZERO,
    );
    Some(connectors)
}

fn cached_directory_connectors_in_memory(
    cache_key: &ConnectorDirectoryCacheKey,
) -> Option<Vec<AppInfo>> {
    let cache_guard = CONNECTOR_DIRECTORY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache_guard
        .as_ref()
        .filter(|cached| cached.key == *cache_key)
        .map(|cached| cached.connectors.clone())
}

fn unexpired_directory_connectors_in_memory(
    cache_key: &ConnectorDirectoryCacheKey,
) -> Option<Vec<AppInfo>> {
    let cache_guard = CONNECTOR_DIRECTORY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cached = cache_guard.as_ref()?;
    if cached.key == *cache_key && Instant::now() < cached.expires_at {
        return Some(cached.connectors.clone());
    }
    None
}

pub async fn list_all_connectors_with_options<F, Fut>(
    cache_context: ConnectorDirectoryCacheContext,
    is_workspace_account: bool,
    force_refetch: bool,
    mut fetch_page: F,
) -> anyhow::Result<Vec<AppInfo>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<DirectoryListResponse>>,
{
    if !force_refetch
        && let Some(cached_connectors) =
            unexpired_directory_connectors_in_memory(&cache_context.cache_key)
    {
        return Ok(cached_connectors);
    }

    let apps = if is_workspace_account {
        // The workspace directory is independent from the paginated public directory.
        // Start both before awaiting either so workspace accounts do not pay for the
        // two request chains back-to-back.
        let workspace_connectors =
            fetch_page("/connectors/directory/list_workspace?external_logos=true".to_string());
        let directory_connectors = list_directory_connectors(&mut fetch_page);
        let (directory_connectors, workspace_connectors) =
            tokio::join!(directory_connectors, workspace_connectors);
        let mut apps = directory_connectors?;
        if let Ok(response) = workspace_connectors {
            apps.extend(
                response
                    .apps
                    .into_iter()
                    .filter(|app| !is_hidden_directory_app(app)),
            );
        }
        apps
    } else {
        list_directory_connectors(&mut fetch_page).await?
    };

    let mut connectors = merge_directory_apps(apps)
        .into_iter()
        .map(directory_app_to_app_info)
        .collect::<Vec<_>>();
    for connector in &mut connectors {
        let install_url = match connector.install_url.take() {
            Some(install_url) => install_url,
            None => connector_install_url(&connector.name, &connector.id),
        };
        connector.name = normalize_connector_name(&connector.name, &connector.id);
        connector.description = normalize_connector_value(connector.description.as_deref());
        connector.install_url = Some(install_url);
        connector.is_accessible = false;
    }
    connectors.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    write_cached_directory_connectors(&cache_context, &connectors);
    Ok(connectors)
}

fn write_cached_directory_connectors(
    cache_context: &ConnectorDirectoryCacheContext,
    connectors: &[AppInfo],
) {
    write_cached_directory_connectors_in_memory(
        cache_context.cache_key.clone(),
        connectors,
        CONNECTORS_CACHE_TTL,
    );
    directory_cache::write_cached_directory_connectors_to_disk(cache_context, connectors);
}

fn write_cached_directory_connectors_in_memory(
    cache_key: ConnectorDirectoryCacheKey,
    connectors: &[AppInfo],
    ttl: Duration,
) {
    let mut cache_guard = CONNECTOR_DIRECTORY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *cache_guard = Some(CachedConnectorDirectory {
        key: cache_key,
        expires_at: Instant::now() + ttl,
        connectors: connectors.to_vec(),
    });
}

async fn list_directory_connectors<F, Fut>(fetch_page: &mut F) -> anyhow::Result<Vec<DirectoryApp>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<DirectoryListResponse>>,
{
    let mut apps = Vec::new();
    let mut next_token: Option<String> = None;
    loop {
        let path = match next_token.as_deref() {
            Some(token) => {
                let encoded_token = urlencoding::encode(token);
                format!("/connectors/directory/list?token={encoded_token}&external_logos=true")
            }
            None => "/connectors/directory/list?external_logos=true".to_string(),
        };
        let response = fetch_page(path).await?;
        apps.extend(
            response
                .apps
                .into_iter()
                .filter(|app| !is_hidden_directory_app(app)),
        );
        next_token = response
            .next_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        if next_token.is_none() {
            break;
        }
    }
    Ok(apps)
}

fn merge_directory_apps(apps: Vec<DirectoryApp>) -> Vec<DirectoryApp> {
    let mut merged: HashMap<String, DirectoryApp> = HashMap::new();
    for app in apps {
        if let Some(existing) = merged.get_mut(&app.id) {
            merge_directory_app(existing, app);
        } else {
            merged.insert(app.id.clone(), app);
        }
    }
    merged.into_values().collect()
}

fn merge_directory_app(existing: &mut DirectoryApp, incoming: DirectoryApp) {
    let DirectoryApp {
        id: _,
        name,
        description,
        app_metadata,
        branding,
        labels,
        logo_url,
        logo_url_dark,
        icon_assets,
        icon_dark_assets,
        distribution_channel,
        visibility: _,
    } = incoming;

    let incoming_name_is_empty = name.trim().is_empty();
    if existing.name.trim().is_empty() && !incoming_name_is_empty {
        existing.name = name;
    }

    let incoming_description_present = description
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if incoming_description_present {
        existing.description = description;
    }

    if existing.logo_url.is_none() && logo_url.is_some() {
        existing.logo_url = logo_url;
    }
    if existing.logo_url_dark.is_none() && logo_url_dark.is_some() {
        existing.logo_url_dark = logo_url_dark;
    }
    if existing.icon_assets.as_ref().is_none_or(HashMap::is_empty)
        && icon_assets
            .as_ref()
            .is_some_and(|assets| !assets.is_empty())
    {
        existing.icon_assets = icon_assets;
    }
    if existing
        .icon_dark_assets
        .as_ref()
        .is_none_or(HashMap::is_empty)
        && icon_dark_assets
            .as_ref()
            .is_some_and(|assets| !assets.is_empty())
    {
        existing.icon_dark_assets = icon_dark_assets;
    }
    if existing.distribution_channel.is_none() && distribution_channel.is_some() {
        existing.distribution_channel = distribution_channel;
    }

    if let Some(incoming_branding) = branding {
        if let Some(existing_branding) = existing.branding.as_mut() {
            if existing_branding.category.is_none() && incoming_branding.category.is_some() {
                existing_branding.category = incoming_branding.category;
            }
            if existing_branding.developer.is_none() && incoming_branding.developer.is_some() {
                existing_branding.developer = incoming_branding.developer;
            }
            if existing_branding.website.is_none() && incoming_branding.website.is_some() {
                existing_branding.website = incoming_branding.website;
            }
            if existing_branding.privacy_policy.is_none()
                && incoming_branding.privacy_policy.is_some()
            {
                existing_branding.privacy_policy = incoming_branding.privacy_policy;
            }
            if existing_branding.terms_of_service.is_none()
                && incoming_branding.terms_of_service.is_some()
            {
                existing_branding.terms_of_service = incoming_branding.terms_of_service;
            }
            if !existing_branding.is_discoverable_app && incoming_branding.is_discoverable_app {
                existing_branding.is_discoverable_app = true;
            }
        } else {
            existing.branding = Some(incoming_branding);
        }
    }

    if let Some(incoming_app_metadata) = app_metadata {
        if let Some(existing_app_metadata) = existing.app_metadata.as_mut() {
            if existing_app_metadata.review.is_none() && incoming_app_metadata.review.is_some() {
                existing_app_metadata.review = incoming_app_metadata.review;
            }
            if existing_app_metadata.categories.is_none()
                && incoming_app_metadata.categories.is_some()
            {
                existing_app_metadata.categories = incoming_app_metadata.categories;
            }
            if existing_app_metadata.sub_categories.is_none()
                && incoming_app_metadata.sub_categories.is_some()
            {
                existing_app_metadata.sub_categories = incoming_app_metadata.sub_categories;
            }
            if existing_app_metadata.seo_description.is_none()
                && incoming_app_metadata.seo_description.is_some()
            {
                existing_app_metadata.seo_description = incoming_app_metadata.seo_description;
            }
            if existing_app_metadata.screenshots.is_none()
                && incoming_app_metadata.screenshots.is_some()
            {
                existing_app_metadata.screenshots = incoming_app_metadata.screenshots;
            }
            if existing_app_metadata.developer.is_none()
                && incoming_app_metadata.developer.is_some()
            {
                existing_app_metadata.developer = incoming_app_metadata.developer;
            }
            if existing_app_metadata.version.is_none() && incoming_app_metadata.version.is_some() {
                existing_app_metadata.version = incoming_app_metadata.version;
            }
            if existing_app_metadata.version_id.is_none()
                && incoming_app_metadata.version_id.is_some()
            {
                existing_app_metadata.version_id = incoming_app_metadata.version_id;
            }
            if existing_app_metadata.version_notes.is_none()
                && incoming_app_metadata.version_notes.is_some()
            {
                existing_app_metadata.version_notes = incoming_app_metadata.version_notes;
            }
            if existing_app_metadata.first_party_requires_install.is_none()
                && incoming_app_metadata.first_party_requires_install.is_some()
            {
                existing_app_metadata.first_party_requires_install =
                    incoming_app_metadata.first_party_requires_install;
            }
            if existing_app_metadata
                .show_in_composer_when_unlinked
                .is_none()
                && incoming_app_metadata
                    .show_in_composer_when_unlinked
                    .is_some()
            {
                existing_app_metadata.show_in_composer_when_unlinked =
                    incoming_app_metadata.show_in_composer_when_unlinked;
            }
        } else {
            existing.app_metadata = Some(incoming_app_metadata);
        }
    }

    if existing.labels.is_none() && labels.is_some() {
        existing.labels = labels;
    }
}

fn is_hidden_directory_app(app: &DirectoryApp) -> bool {
    matches!(app.visibility.as_deref(), Some("HIDDEN"))
}

fn directory_app_to_app_info(app: DirectoryApp) -> AppInfo {
    AppInfo {
        id: app.id,
        name: app.name,
        description: app.description,
        logo_url: app.logo_url,
        logo_url_dark: app.logo_url_dark,
        icon_assets: app.icon_assets,
        icon_dark_assets: app.icon_dark_assets,
        distribution_channel: app.distribution_channel,
        branding: app.branding,
        app_metadata: app.app_metadata,
        labels: app.labels,
        install_url: None,
        is_accessible: false,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    }
}

fn connector_install_url(name: &str, connector_id: &str) -> String {
    let chatgpt_base_url = std::env::var("CODEX_APP_SERVER_CHATGPT_BASE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com".to_string());
    let chatgpt_origin = chatgpt_base_url
        .trim_end_matches('/')
        .trim_end_matches("/backend-api");
    let slug = connector_name_slug(name);
    format!("{chatgpt_origin}/apps/{slug}/{connector_id}")
}

fn connector_name_slug(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
        } else {
            normalized.push('-');
        }
    }
    let normalized = normalized.trim_matches('-');
    if normalized.is_empty() {
        "app".to_string()
    } else {
        normalized.to_string()
    }
}

fn normalize_connector_name(name: &str, connector_id: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        connector_id.to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_connector_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
