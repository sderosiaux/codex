use codex_extension_api::ExtensionData;
use codex_protocol::protocol::SkillScope;
use codex_skills::detect_implicit_skill_invocation_for_command;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

use crate::HostSkillsSnapshot;

/// A skill a command implicitly used, attributed for telemetry.
#[derive(Clone, Debug)]
pub struct SkillInvocation {
    pub skill_name: String,
    pub path: PathUri,
    pub scope: SkillScope,
}

/// Identifies the host-owned skill referenced by a command.
pub fn detect_implicit_skill_invocation(
    turn_store: &ExtensionData,
    command: &str,
    workdir: &AbsolutePathBuf,
) -> Option<SkillInvocation> {
    let host_snapshot = turn_store.get::<HostSkillsSnapshot>()?;
    let skill =
        detect_implicit_skill_invocation_for_command(host_snapshot.outcome(), command, workdir)?;
    Some(SkillInvocation {
        skill_name: skill.name,
        path: skill.path_to_skills_md,
        scope: skill.scope,
    })
}
