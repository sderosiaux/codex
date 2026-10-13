/*
Module: runtimes

Concrete tool runtimes for shell execution and patch application, plus the
environment helpers they share.
*/
use crate::exec_env::CODEX_SESSION_ID_ENV_VAR;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::exec_env::CODEX_TOOL_CALL_ID_ENV_VAR;
use crate::exec_env::CODEX_VERSION_ENV_VAR;
use crate::shell::Shell;
use codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR;
#[cfg(unix)]
use codex_install_context::InstallContext;
use codex_protocol::shell_environment::is_non_inheritable_env_var;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::path::Path;

pub(crate) mod apply_patch;
pub(crate) mod unified_exec;

/// Prepends `path_entry` to `PATH`, removing duplicate and empty existing
/// entries.
///
/// Returns the updated `PATH` value when `env` was changed. Returns `None` when
/// `path_entry` is empty, leaving `env` untouched so an empty entry does not add
/// the current working directory to command lookup.
#[cfg(unix)]
fn prepend_path_entry(env: &mut HashMap<String, String>, path_entry: &str) -> Option<String> {
    if path_entry.is_empty() {
        None
    } else {
        let updated_path = match env.get("PATH") {
            Some(path) if !path.is_empty() => std::iter::once(path_entry)
                .chain(
                    path.split(':')
                        .filter(|entry| !entry.is_empty() && *entry != path_entry),
                )
                .collect::<Vec<_>>()
                .join(":"),
            _ => path_entry.to_string(),
        };
        env.insert("PATH".to_string(), updated_path.clone());
        Some(updated_path)
    }
}

/// PATH entries owned by Codex runtime setup.
///
/// These are applied to the live exec environment immediately and replayed after
/// restoring a shell snapshot, unless the user explicitly overrides `PATH`.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimePathPrepends {
    entries: Vec<String>,
}

impl RuntimePathPrepends {
    #[cfg(unix)]
    pub(crate) fn prepend(&mut self, env: &mut HashMap<String, String>, path_entry: &Path) {
        let path_entry = path_entry.to_string_lossy().to_string();
        if prepend_path_entry(env, &path_entry).is_some() {
            self.entries.retain(|entry| entry != &path_entry);
            self.entries.push(path_entry);
        }
    }

    fn shell_exports_after_snapshot(
        &self,
        explicit_env_overrides: &HashMap<String, String>,
    ) -> String {
        if explicit_env_overrides.contains_key("PATH") {
            return String::new();
        }

        self.entries
            .iter()
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let entry = shell_single_quote(entry);
                format!(
                    "if [ -n \"${{PATH:-}}\" ]; then export PATH='{entry}':\"$PATH\"; else export PATH='{entry}'; fi"
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(unix)]
pub(crate) fn apply_package_path_prepend(
    env: &mut HashMap<String, String>,
    runtime_path_prepends: &mut RuntimePathPrepends,
) {
    let Some(path_dir) = InstallContext::current()
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())
    else {
        return;
    };

    runtime_path_prepends.prepend(env, path_dir.as_path());
}

/// POSIX-only helper: for commands produced by `Shell::derive_exec_args`
/// for Bash/Zsh/sh of the form `[shell_path, "-lc", "<script>"]`, and
/// when a snapshot is configured on the session shell, rewrite the argv
/// to a single non-login shell that sources the snapshot before running
/// the original script:
///
///   shell -lc "<script>"
///   => user_shell -c ". SNAPSHOT (best effort); exec shell -c <script>"
///
/// This wrapper script uses POSIX constructs (`if`, `.`, `exec`) so it can
/// be run by Bash/Zsh/sh. On non-matching commands, this is a no-op.
///
/// `explicit_env_overrides` and `env` are intentionally separate inputs.
/// `explicit_env_overrides` contains policy-driven shell env overrides that
/// should win after the snapshot is sourced, while `env` is the full live exec
/// environment. We need access to both so snapshot restore logic can preserve
/// runtime-only vars like `CODEX_THREAD_ID` without pretending they came from
/// the explicit override policy.
///
/// `runtime_path_prepends` contains Codex-owned PATH entries already applied to
/// the live `env`; snapshot wrapping replays them after restoring the snapshot
/// PATH unless the user explicitly overrides `PATH`.
pub(crate) fn maybe_wrap_shell_lc_with_snapshot(
    command: &[String],
    session_shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
    runtime_path_prepends: &RuntimePathPrepends,
) -> Vec<String> {
    if cfg!(windows) {
        return command.to_vec();
    }

    let Some(snapshot) = shell_snapshot else {
        return command.to_vec();
    };

    if !snapshot.exists() {
        return command.to_vec();
    }

    if command.len() < 3 || command[1] != "-lc" {
        return command.to_vec();
    }

    let snapshot_path = snapshot.to_string_lossy();
    let shell_path = session_shell.shell_path.to_string_lossy();
    let original_shell = shell_single_quote(&command[0]);
    let snapshot_path = shell_single_quote(snapshot_path.as_ref());
    let trailing_args = command[3..]
        .iter()
        .map(|arg| format!(" '{}'", shell_single_quote(arg)))
        .collect::<String>();
    let mut override_env = explicit_env_overrides.clone();
    for key in [
        CODEX_SESSION_ID_ENV_VAR,
        CODEX_THREAD_ID_ENV_VAR,
        CODEX_TOOL_CALL_ID_ENV_VAR,
        CODEX_VERSION_ENV_VAR,
        CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR,
    ] {
        if let Some(value) = env.get(key) {
            override_env.insert(key.to_string(), value.clone());
        }
    }
    // Do not let a snapshot resurrect stale runtime state when it is inactive.
    let (override_captures, override_exports) = build_override_exports(
        &override_env,
        &[
            CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR,
            CODEX_TOOL_CALL_ID_ENV_VAR,
        ],
    );
    let runtime_path_prepend_exports =
        runtime_path_prepends.shell_exports_after_snapshot(explicit_env_overrides);
    let override_exports = join_shell_blocks([override_exports, runtime_path_prepend_exports]);
    let original_script = shell_single_quote(&command[2]);
    let run_original = format!("exec '{original_shell}' -c '{original_script}'{trailing_args}");
    let rewritten_script = if override_exports.is_empty() {
        format!("if . '{snapshot_path}' >/dev/null 2>&1; then :; fi\n\n{run_original}")
    } else {
        format!(
            "{override_captures}\n\nif . '{snapshot_path}' >/dev/null 2>&1; then :; fi\n\n{override_exports}\n\n{run_original}"
        )
    };

    vec![shell_path.to_string(), "-c".to_string(), rewritten_script]
}

fn build_override_exports(
    explicit_env_overrides: &HashMap<String, String>,
    restore_even_when_absent: &[&str],
) -> (String, String) {
    let mut keys = explicit_env_overrides
        .keys()
        .map(String::as_str)
        .chain(restore_even_when_absent.iter().copied())
        .filter(|key| !is_non_inheritable_env_var(key))
        .filter(|key| is_valid_shell_variable_name(key))
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();

    build_override_exports_for_keys("__CODEX_SNAPSHOT_OVERRIDE", &keys)
}

fn build_override_exports_for_keys(variable_prefix: &str, keys: &[&str]) -> (String, String) {
    if keys.is_empty() {
        return (String::new(), String::new());
    }

    let captures = keys
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let set_var = format!("{variable_prefix}_SET_{idx}");
            let value_var = format!("{variable_prefix}_{idx}");
            format!("{set_var}=\"${{{key}+x}}\"\n{value_var}=\"${{{key}-}}\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let restores = keys
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let set_var = format!("{variable_prefix}_SET_{idx}");
            let value_var = format!("{variable_prefix}_{idx}");
            format!(
                "if [ -n \"${{{set_var}}}\" ]; then\n  if [ -z \"${{{key}+x}}\" ] || [ \"${{{key}-}}\" != \"${{{value_var}}}\" ]; then export {key}=\"${{{value_var}}}\"; else export {key}; fi\nelse builtin unset {key} 2>/dev/null || command unset {key}; fi\nbuiltin unset {set_var} {value_var} 2>/dev/null || command unset {set_var} {value_var}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    (captures, restores)
}

fn join_shell_blocks(blocks: impl IntoIterator<Item = String>) -> String {
    blocks
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_valid_shell_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn shell_single_quote(input: &str) -> String {
    input.replace('\'', r#"'"'"'"#)
}
