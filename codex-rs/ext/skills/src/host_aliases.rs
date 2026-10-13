use crate::catalog::SkillCatalogEntry;

pub(crate) fn shared_host_alias_roots(entries: &[&SkillCatalogEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| entry.alias_root())
        .map(|root| root.replace('\\', "/"))
        .collect()
}
