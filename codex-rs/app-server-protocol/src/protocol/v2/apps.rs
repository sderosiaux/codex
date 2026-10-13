use super::shared::default_enabled;
use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL - app metadata returned by app-list APIs.
pub struct AppBranding {
    pub category: Option<String>,
    pub developer: Option<String>,
    pub website: Option<String>,
    pub privacy_policy: Option<String>,
    pub terms_of_service: Option<String>,
    pub is_discoverable_app: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AppReview {
    pub status: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AppScreenshot {
    pub url: Option<String>,
    #[serde(alias = "file_id")]
    pub file_id: Option<String>,
    #[serde(alias = "user_prompt")]
    pub user_prompt: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AppMetadata {
    pub review: Option<AppReview>,
    pub categories: Option<Vec<String>>,
    pub sub_categories: Option<Vec<String>>,
    pub seo_description: Option<String>,
    pub screenshots: Option<Vec<AppScreenshot>>,
    pub developer: Option<String>,
    pub version: Option<String>,
    pub version_id: Option<String>,
    pub version_notes: Option<String>,
    pub first_party_requires_install: Option<bool>,
    pub show_in_composer_when_unlinked: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL - app metadata returned by app-list APIs.
pub struct AppInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub logo_url: Option<String>,
    pub logo_url_dark: Option<String>,
    pub icon_assets: Option<HashMap<String, String>>,
    pub icon_dark_assets: Option<HashMap<String, String>>,
    pub distribution_channel: Option<String>,
    pub branding: Option<AppBranding>,
    pub app_metadata: Option<AppMetadata>,
    pub labels: Option<HashMap<String, String>>,
    pub install_url: Option<String>,
    #[serde(default)]
    pub is_accessible: bool,
    /// Whether this app is enabled in config.toml.
    /// Example:
    /// ```toml
    /// [apps.bad_app]
    /// enabled = false
    /// ```
    #[serde(default = "default_enabled")]
    pub is_enabled: bool,
    #[serde(default)]
    pub plugin_display_names: Vec<String>,
}

impl AppInfo {
    pub fn category(&self) -> Option<String> {
        self.branding
            .as_ref()
            .and_then(|branding| non_empty_category(branding.category.as_deref()))
            .or_else(|| {
                self.app_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.categories.as_ref())
                    .and_then(|categories| {
                        categories
                            .iter()
                            .find_map(|category| non_empty_category(Some(category.as_str())))
                    })
            })
    }
}

fn non_empty_category(category: Option<&str>) -> Option<String> {
    let category = category?.trim();
    if category.is_empty() {
        None
    } else {
        Some(category.to_string())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL - metadata returned by app/read.
pub struct AppToolSummary {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    #[serde(default = "default_enabled")]
    pub is_enabled: bool,
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub is_read_only: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL - metadata returned by app/read.
pub struct ConnectorMetadata {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    pub icon_url_dark: Option<String>,
    pub distribution_channel: Option<String>,
    pub install_url: Option<String>,
    #[serde(default)]
    pub plugin_display_names: Vec<String>,
    pub tool_summaries: Option<Vec<AppToolSummary>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL - app metadata summary for plugin responses.
pub struct AppSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub install_url: Option<String>,
    pub category: Option<String>,
}

impl From<AppInfo> for AppSummary {
    fn from(value: AppInfo) -> Self {
        let category = value.category();
        Self {
            id: value.id,
            name: value.name,
            description: value.description,
            install_url: value.install_url,
            category,
        }
    }
}
