use codex_features::Feature;
use codex_features::Features;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::TUI_VISIBLE_COLLABORATION_MODES;

pub fn request_user_input_available_modes(features: &Features) -> Vec<ModeKind> {
    TUI_VISIBLE_COLLABORATION_MODES
        .into_iter()
        .filter(|mode| {
            mode.allows_request_user_input()
                || (features.enabled(Feature::DefaultModeRequestUserInput)
                    && *mode == ModeKind::Default)
        })
        .collect()
}
