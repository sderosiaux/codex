//! Orders model-visible entries and plans aliases from the full visible inventory.

use super::AvailableSkillsRender;
use super::CatalogLines;
use super::SkillCatalogRenderPolicy;
use super::SkillMetadataBudget;
use super::aliased_render_is_better;
use super::build_alias_plan;
use super::build_aliased_catalog;
use super::render_catalog;
use crate::aliases::AliasPlan;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog_prompt::SkillPromptKind;

pub(crate) struct PreparedSkillCatalog<'a> {
    pub(super) entries: Vec<&'a SkillCatalogEntry>,
    alias_plan: Option<AliasPlan>,
    policy: SkillCatalogRenderPolicy,
}

impl<'a> PreparedSkillCatalog<'a> {
    pub(crate) fn new(catalog: &'a SkillCatalog, policy: SkillCatalogRenderPolicy) -> Self {
        let mut entries = catalog
            .entries
            .iter()
            .filter(|entry| entry.is_model_visible())
            .collect::<Vec<_>>();
        policy.order_entries(&mut entries);
        let alias_plan = build_alias_plan(&entries);
        Self {
            entries,
            alias_plan,
            policy,
        }
    }

    pub(super) fn unaliased(&self) -> CatalogLines<'a> {
        CatalogLines::unaliased(&self.entries, self.policy)
    }

    pub(super) fn aliased(&self) -> CatalogLines<'a> {
        CatalogLines::aliased(&self.entries, self.policy, self.alias_plan.as_ref())
    }

    pub(crate) fn render(&self, budget: SkillMetadataBudget) -> Option<AvailableSkillsRender> {
        if self.entries.is_empty() {
            return None;
        }
        let absolute = render_catalog(
            self.unaliased().skills,
            budget,
            Vec::new(),
            SkillPromptKind::Unaliased,
            self.policy,
        );
        let selected = if let Some(aliased) =
            build_aliased_catalog(self.aliased(), self.policy, budget)
            && aliased_render_is_better(&aliased, &absolute, budget)
        {
            aliased
        } else {
            absolute
        };

        Some(AvailableSkillsRender {
            prompt_kind: selected.prompt_kind,
            skill_root_lines: selected.skill_root_lines,
            skill_lines: selected.skill_lines,
            preserve_empty_fragment: self.policy == SkillCatalogRenderPolicy::CoreCompatible,
            report: selected.report,
        })
    }
}
