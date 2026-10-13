use std::collections::HashSet;

use crate::AppInfo;

pub fn filter_tool_suggest_discoverable_connectors(
    directory_connectors: Vec<AppInfo>,
    accessible_connectors: &[AppInfo],
    discoverable_connector_ids: &HashSet<String>,
) -> Vec<AppInfo> {
    let accessible_connector_ids: HashSet<&str> = accessible_connectors
        .iter()
        .filter(|connector| connector.is_accessible)
        .map(|connector| connector.id.as_str())
        .collect();

    let mut connectors = directory_connectors
        .into_iter()
        .filter(|connector| !accessible_connector_ids.contains(connector.id.as_str()))
        .filter(|connector| discoverable_connector_ids.contains(connector.id.as_str()))
        .collect::<Vec<_>>();
    connectors.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    connectors
}
