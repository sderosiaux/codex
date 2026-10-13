//! Serialize captured snapshot state and apply export-name policy without executing shell code.
//! Native declarations are replayed unchanged.

use super::capture::CapturedSnapshot;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::shell_environment::create_env_from_vars;

impl CapturedSnapshot<'_> {
    /// Render native functions, options, and aliases; the executor restores environment separately.
    pub fn render_state(&self) -> String {
        format!("{}{}", self.state, self.aliases)
    }

    /// Persist only exports admitted by the policy. Explicit overrides are supplied
    /// by the command environment; never retain their original captured values.
    pub fn render_script(&self, policy: &ShellEnvironmentPolicy) -> String {
        let allowed = create_env_from_vars(
            self.exports
                .iter()
                .map(|export| (export.key.to_string(), String::new())),
            policy,
            /*thread_id*/ None,
        );
        let mut script = self.render_state();
        script.push_str("# exports (native declarations)\n");
        for export in &self.exports {
            if allowed.contains_key(export.key) && !policy.r#set.contains_key(export.key) {
                script.push_str(&export.source);
            }
        }
        script
    }
}
