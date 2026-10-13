use codex_plugin::PluginHookSource;
use codex_protocol::protocol::HookEventName;

/// Minimal declaration metadata for one bundled plugin hook handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHookDeclaration {
    pub key: String,
    pub event_name: HookEventName,
}

/// Return the hook handlers declared by plugin bundles without projecting live runtime state.
pub fn plugin_hook_declarations(hook_sources: &[PluginHookSource]) -> Vec<PluginHookDeclaration> {
    let mut declarations = Vec::new();

    for source in hook_sources {
        let key_source = plugin_hook_key_source(
            source.plugin_id.as_key().as_str(),
            source.source_relative_path.as_str(),
        );
        for (event_name, groups) in source.hooks.clone().into_matcher_groups() {
            for (group_index, group) in groups.iter().enumerate() {
                for (handler_index, _) in group.hooks.iter().enumerate() {
                    declarations.push(PluginHookDeclaration {
                        key: crate::hook_key(&key_source, event_name, group_index, handler_index),
                        event_name,
                    });
                }
            }
        }
    }

    declarations
}

pub(crate) fn plugin_hook_key_source(plugin_id: &str, source_relative_path: &str) -> String {
    format!("{plugin_id}:{source_relative_path}")
}
