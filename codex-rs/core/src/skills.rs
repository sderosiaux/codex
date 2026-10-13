use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_extension_api::SkillInvocationInput;
use codex_extension_api::SkillInvocationKind;
use codex_protocol::protocol::SkillScope;
use codex_skills::SkillMetadata;
use codex_skills_extension::HostSkillsLoadInput;
use codex_skills_extension::InjectedHostSkillPrompts;
use codex_skills_extension::detect_implicit_skill_invocation;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashSet;
use tokio::sync::Mutex;

#[derive(Debug, Default)]
struct ImplicitSkillInvocations(Mutex<HashSet<String>>);

pub(crate) fn skills_load_input_from_config(config: &Config) -> HostSkillsLoadInput {
    HostSkillsLoadInput::new(config.cwd.clone(), config.config_layer_stack.clone())
}

pub(crate) async fn emit_explicit_skill_invocations(
    sess: &Session,
    turn_context: &TurnContext,
    injected_skills: &[SkillMetadata],
) {
    let injected_host_skill_prompts = turn_context
        .extension_data
        .get::<InjectedHostSkillPrompts>();
    for skill in injected_skills {
        let skill_resource = skill.path_to_skills_md.inferred_native_path_string();
        if injected_host_skill_prompts
            .as_ref()
            .is_some_and(|prompts| prompts.is_superseded_path(&skill_resource))
        {
            continue;
        }
        for contributor in sess.services.extensions.skill_invocation_contributors() {
            contributor
                .on_skill_invocation(SkillInvocationInput {
                    session_store: &sess.services.session_extension_data,
                    thread_store: &sess.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                    turn_id: turn_context.sub_id.as_str(),
                    skill_resource: skill_resource.as_ref(),
                    kind: SkillInvocationKind::Explicit,
                })
                .await;
        }
    }
}

pub(crate) async fn maybe_emit_implicit_skill_invocation(
    sess: &Session,
    turn_context: &TurnContext,
    command: &str,
    native_workdir: Option<&AbsolutePathBuf>,
) {
    let Some(native_workdir) = native_workdir else {
        return;
    };
    let Some(invocation) = detect_implicit_skill_invocation(
        turn_context.extension_data.as_ref(),
        command,
        native_workdir,
    ) else {
        return;
    };
    let skill_name = invocation.skill_name.clone();
    let skill_scope = match invocation.scope {
        SkillScope::User => "user",
        SkillScope::Repo => "repo",
        SkillScope::System => "system",
        SkillScope::Admin => "admin",
    };
    let skill_resource = invocation.path.inferred_native_path_string();
    let seen_key = format!("{skill_scope}:{skill_resource}:{skill_name}");
    let inserted = {
        let skill_invocations = turn_context
            .extension_data
            .get_or_init(ImplicitSkillInvocations::default);
        let mut seen_skills = skill_invocations.0.lock().await;
        seen_skills.insert(seen_key)
    };
    if !inserted {
        return;
    }

    for contributor in sess.services.extensions.skill_invocation_contributors() {
        contributor
            .on_skill_invocation(SkillInvocationInput {
                session_store: &sess.services.session_extension_data,
                thread_store: &sess.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
                turn_id: turn_context.sub_id.as_str(),
                skill_resource: skill_resource.as_str(),
                kind: SkillInvocationKind::Implicit,
            })
            .await;
    }
}
