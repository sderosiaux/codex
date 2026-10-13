use super::AppSummary;
use super::HookEventName;
use crate::JsonSchema;
use crate::TS;
use codex_protocol::protocol::SkillDependencies as CoreSkillDependencies;
use codex_protocol::protocol::SkillInterface as CoreSkillInterface;
use codex_protocol::protocol::SkillMetadata as CoreSkillMetadata;
use codex_protocol::protocol::SkillScope as CoreSkillScope;
use codex_protocol::protocol::SkillToolDependency as CoreSkillToolDependency;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum PluginShareDiscoverability {
    #[serde(rename = "LISTED")]
    #[ts(rename = "LISTED")]
    Listed,
    #[serde(rename = "UNLISTED")]
    #[ts(rename = "UNLISTED")]
    Unlisted,
    #[serde(rename = "PRIVATE")]
    #[ts(rename = "PRIVATE")]
    Private,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum PluginSharePrincipalType {
    #[serde(rename = "user")]
    #[ts(rename = "user")]
    User,
    #[serde(rename = "group")]
    #[ts(rename = "group")]
    Group,
    #[serde(rename = "workspace")]
    #[ts(rename = "workspace")]
    Workspace,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginSharePrincipal {
    pub principal_type: PluginSharePrincipalType,
    pub principal_id: String,
    pub role: PluginSharePrincipalRole,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
#[ts(export_to = "v2/")]
pub enum PluginSharePrincipalRole {
    Reader,
    Editor,
    Owner,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum SkillScope {
    User,
    Repo,
    System,
    Admin,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    /// Legacy short_description from SKILL.md. Prefer SKILL.json interface.short_description.
    pub short_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub interface: Option<SkillInterface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dependencies: Option<SkillDependencies>,
    pub path: LegacyAppPathString,
    pub scope: SkillScope,
    pub enabled: bool,
    /// Owning plugin ID, matching `PluginSummary.id`, when known.
    pub plugin_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillInterface {
    #[ts(optional)]
    pub display_name: Option<String>,
    #[ts(optional)]
    pub short_description: Option<String>,
    #[ts(optional)]
    pub icon_small: Option<AbsolutePathBuf>,
    #[ts(optional)]
    pub icon_large: Option<AbsolutePathBuf>,
    /// Remote small icon URL from the plugin catalog.
    pub icon_small_url: Option<String>,
    /// Remote large icon URL from the plugin catalog.
    pub icon_large_url: Option<String>,
    #[ts(optional)]
    pub brand_color: Option<String>,
    #[ts(optional)]
    pub default_prompt: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillDependencies {
    pub tools: Vec<SkillToolDependency>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillToolDependency {
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub r#type: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MarketplaceInterface {
    pub display_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum PluginInstallPolicy {
    #[serde(rename = "NOT_AVAILABLE")]
    #[ts(rename = "NOT_AVAILABLE")]
    NotAvailable,
    #[serde(rename = "AVAILABLE")]
    #[ts(rename = "AVAILABLE")]
    Available,
    #[serde(rename = "INSTALLED_BY_DEFAULT")]
    #[ts(rename = "INSTALLED_BY_DEFAULT")]
    InstalledByDefault,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum PluginInstallPolicySource {
    #[serde(rename = "WORKSPACE_SETTING")]
    #[ts(rename = "WORKSPACE_SETTING")]
    WorkspaceSetting,
    #[serde(rename = "IMPLICIT_CANONICAL_APP")]
    #[ts(rename = "IMPLICIT_CANONICAL_APP")]
    ImplicitCanonicalApp,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum PluginAuthPolicy {
    #[serde(rename = "ON_INSTALL")]
    #[ts(rename = "ON_INSTALL")]
    OnInstall,
    #[serde(rename = "ON_USE")]
    #[ts(rename = "ON_USE")]
    OnUse,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum PluginAvailability {
    /// Plugin-service currently sends `"ENABLED"` for available remote plugins.
    /// Codex app-server exposes `"AVAILABLE"` in its API; the alias keeps
    /// decoding compatible with that upstream response.
    #[serde(rename = "AVAILABLE", alias = "ENABLED")]
    #[ts(rename = "AVAILABLE")]
    #[default]
    Available,
    #[serde(rename = "DISABLED_BY_ADMIN")]
    #[ts(rename = "DISABLED_BY_ADMIN")]
    DisabledByAdmin,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/", rename_all = "snake_case")]
pub enum PluginDisabledReason {
    DisabledByAdmin,
    PlanNotEligible,
    RequiredAppUnavailable,
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginSummary {
    pub id: String,
    /// Backend remote plugin identifier when available.
    pub remote_plugin_id: Option<String>,
    /// Version advertised by the remote marketplace backend when available.
    #[serde(default)]
    pub version: Option<String>,
    /// Version of the locally materialized plugin package when available.
    #[serde(default)]
    pub local_version: Option<String>,
    pub name: String,
    /// Remote sharing context associated with this plugin when available.
    pub share_context: Option<PluginShareContext>,
    pub source: PluginSource,
    pub installed: bool,
    /// Unix timestamp in seconds when the remote plugin was installed, when available.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub installed_at: Option<i64>,
    pub enabled: bool,
    pub install_policy: PluginInstallPolicy,
    pub install_policy_source: Option<PluginInstallPolicySource>,
    #[serde(default)]
    pub must_show_installation_interstitial: Option<bool>,
    pub auth_policy: PluginAuthPolicy,
    /// Availability state for installing and using the plugin.
    #[serde(default)]
    pub availability: PluginAvailability,
    /// Why the remote plugin is unavailable, when provided by plugin-service.
    #[serde(default)]
    pub disabled_reason: Option<PluginDisabledReason>,
    /// Raw plugin-service plan identifiers eligible to install the plugin.
    #[serde(default)]
    pub eligible_plan_types: Option<Vec<String>>,
    pub interface: Option<PluginInterface>,
    #[serde(default)]
    pub keywords: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginShareContext {
    pub remote_plugin_id: String,
    /// Version of the remote shared plugin release when available.
    #[serde(default)]
    pub remote_version: Option<String>,
    pub discoverability: Option<PluginShareDiscoverability>,
    pub share_url: Option<String>,
    pub creator_account_user_id: Option<String>,
    pub creator_name: Option<String>,
    pub share_principals: Option<Vec<PluginSharePrincipal>>,
    #[serde(default)]
    pub can_publish_to_workspace: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginDetail {
    pub marketplace_name: String,
    pub marketplace_path: Option<AbsolutePathBuf>,
    pub summary: PluginSummary,
    pub share_url: Option<String>,
    pub description: Option<String>,
    pub skills: Vec<SkillSummary>,
    /// The declared onboarding skill, when the plugin and visible skill are enabled.
    #[serde(default)]
    pub onboarding_skill: Option<SkillSummary>,
    pub hooks: Vec<PluginHookSummary>,
    pub apps: Vec<AppSummary>,
    pub app_templates: Vec<AppTemplateSummary>,
    pub mcp_servers: Vec<String>,
    pub scheduled_tasks: Option<Vec<ScheduledTaskSummary>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ScheduledTaskSummary {
    pub key: String,
    pub name: String,
    pub prompt: String,
    pub schedule: ScheduledTaskSchedule,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum ScheduledTaskSchedule {
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Hourly {
        interval_hours: u32,
        days: Option<Vec<ScheduledTaskWeekday>>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Daily { time: String },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Weekdays { time: String },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Weekly {
        days: Vec<ScheduledTaskWeekday>,
        time: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export_to = "v2/")]
pub enum ScheduledTaskWeekday {
    Mo,
    Tu,
    We,
    Th,
    Fr,
    Sa,
    Su,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export_to = "v2/")]
pub enum AppTemplateUnavailableReason {
    NotConfiguredForWorkspace,
    NoActiveWorkspace,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AppTemplateSummary {
    pub template_id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub canonical_connector_id: Option<String>,
    pub logo_url: Option<String>,
    pub logo_url_dark: Option<String>,
    pub materialized_app_ids: Vec<String>,
    pub reason: Option<AppTemplateUnavailableReason>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginHookSummary {
    pub key: String,
    pub event_name: HookEventName,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub interface: Option<SkillInterface>,
    pub path: Option<LegacyAppPathString>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginInterface {
    pub display_name: Option<String>,
    pub short_description: Option<String>,
    pub long_description: Option<String>,
    pub developer_name: Option<String>,
    pub category: Option<String>,
    pub capabilities: Vec<String>,
    pub website_url: Option<String>,
    pub privacy_policy_url: Option<String>,
    pub terms_of_service_url: Option<String>,
    /// Starter prompts for the plugin. Capped at 3 entries with a maximum of
    /// 128 characters per entry.
    pub default_prompt: Option<Vec<String>>,
    pub brand_color: Option<String>,
    /// Local composer icon path, resolved from the installed plugin package.
    pub composer_icon: Option<AbsolutePathBuf>,
    /// Remote composer icon URL from the plugin catalog.
    pub composer_icon_url: Option<String>,
    /// Local logo path, resolved from the installed plugin package.
    pub logo: Option<AbsolutePathBuf>,
    /// Local dark-mode logo path, resolved from the installed plugin package.
    pub logo_dark: Option<AbsolutePathBuf>,
    /// Remote logo URL from the plugin catalog.
    pub logo_url: Option<String>,
    /// Remote dark-mode logo URL from the plugin catalog.
    pub logo_url_dark: Option<String>,
    /// Local screenshot paths, resolved from the installed plugin package.
    pub screenshots: Vec<AbsolutePathBuf>,
    /// Remote screenshot URLs from the plugin catalog.
    pub screenshot_urls: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum PluginSource {
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Local { path: AbsolutePathBuf },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Git {
        url: String,
        path: Option<String>,
        ref_name: Option<String>,
        sha: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Npm {
        package: String,
        /// Optional npm version or version range.
        version: Option<String>,
        /// Optional HTTPS registry URL. Authentication stays in the user's npm config.
        registry: Option<String>,
    },
    /// The plugin is available in the remote catalog. Download metadata is
    /// kept server-side and is not exposed through the app-server API.
    Remote,
}

impl From<CoreSkillMetadata> for SkillMetadata {
    fn from(value: CoreSkillMetadata) -> Self {
        Self {
            name: value.name,
            description: value.description,
            short_description: value.short_description,
            interface: value.interface.map(SkillInterface::from),
            dependencies: value.dependencies.map(SkillDependencies::from),
            path: value.path.into(),
            scope: value.scope.into(),
            enabled: true,
            plugin_id: None,
        }
    }
}

impl From<CoreSkillInterface> for SkillInterface {
    fn from(value: CoreSkillInterface) -> Self {
        Self {
            display_name: value.display_name,
            short_description: value.short_description,
            brand_color: value.brand_color,
            default_prompt: value.default_prompt,
            icon_small: value.icon_small,
            icon_large: value.icon_large,
            icon_small_url: None,
            icon_large_url: None,
        }
    }
}

impl From<CoreSkillDependencies> for SkillDependencies {
    fn from(value: CoreSkillDependencies) -> Self {
        Self {
            tools: value
                .tools
                .into_iter()
                .map(SkillToolDependency::from)
                .collect(),
        }
    }
}

impl From<CoreSkillToolDependency> for SkillToolDependency {
    fn from(value: CoreSkillToolDependency) -> Self {
        Self {
            r#type: value.r#type,
            value: value.value,
            description: value.description,
            transport: value.transport,
            command: value.command,
            url: value.url,
        }
    }
}

impl From<CoreSkillScope> for SkillScope {
    fn from(value: CoreSkillScope) -> Self {
        match value {
            CoreSkillScope::User => Self::User,
            CoreSkillScope::Repo => Self::Repo,
            CoreSkillScope::System => Self::System,
            CoreSkillScope::Admin => Self::Admin,
        }
    }
}
