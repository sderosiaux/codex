use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_extension_api::ExtensionMetrics;

use crate::SkillsExtensionConfig;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;

pub(crate) struct SkillsSessionState {
    pub(crate) extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
}

/// Thread-owned skill configuration and caches; consumers can only read catalog snapshots.
pub struct SkillsThreadState {
    config: Mutex<SkillsExtensionConfig>,
}

impl SkillsThreadState {
    pub(crate) fn new(config: SkillsExtensionConfig) -> Self {
        Self {
            config: Mutex::new(config),
        }
    }

    pub(crate) fn config(&self) -> SkillsExtensionConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_config(&self, config: SkillsExtensionConfig) {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }
}

#[derive(Default)]
pub(crate) struct EmittedCatalogBudgetWarnings(Mutex<HashSet<String>>);

impl EmittedCatalogBudgetWarnings {
    pub(crate) fn insert(&self, warning: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(warning.to_string())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SkillsTurnState {
    pub(crate) catalog: SkillCatalog,
    pub(crate) selected_entries: Vec<SkillCatalogEntry>,
    pub(crate) warnings: Vec<String>,
    pub(crate) main_prompts_injected: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostSkillsCatalogInWorldState;

/// Marks that this step already listed host skills for world state.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HostSkillsStepState;
