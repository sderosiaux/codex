use std::collections::BTreeMap;

use codex_protocol::protocol::HookEventName;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HooksFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub hooks: HookEventsToml,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HooksToml {
    #[serde(flatten)]
    pub events: HookEventsToml,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state: BTreeMap<String, HookStateToml>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookStateToml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookEventsToml {
    #[serde(rename = "PreToolUse", default)]
    pub pre_tool_use: Vec<MatcherGroup>,
    #[serde(rename = "PostToolUse", default)]
    pub post_tool_use: Vec<MatcherGroup>,
    #[serde(rename = "PreCompact", default)]
    pub pre_compact: Vec<MatcherGroup>,
    #[serde(rename = "PostCompact", default)]
    pub post_compact: Vec<MatcherGroup>,
    #[serde(rename = "SessionStart", default)]
    pub session_start: Vec<MatcherGroup>,
    #[serde(rename = "SessionEnd", default)]
    pub session_end: Vec<MatcherGroup>,
    #[serde(rename = "UserPromptSubmit", default)]
    pub user_prompt_submit: Vec<MatcherGroup>,
    #[serde(rename = "SubagentStart", default)]
    pub subagent_start: Vec<MatcherGroup>,
    #[serde(rename = "SubagentStop", default)]
    pub subagent_stop: Vec<MatcherGroup>,
    #[serde(rename = "Stop", default)]
    pub stop: Vec<MatcherGroup>,
    #[serde(rename = "Interrupt", default)]
    pub interrupt: Vec<MatcherGroup>,
}

impl HookEventsToml {
    pub fn is_empty(&self) -> bool {
        let Self {
            pre_tool_use,
            post_tool_use,
            pre_compact,
            post_compact,
            session_start,
            session_end,
            user_prompt_submit,
            subagent_start,
            subagent_stop,
            stop,
            interrupt,
        } = self;
        pre_tool_use.is_empty()
            && post_tool_use.is_empty()
            && pre_compact.is_empty()
            && post_compact.is_empty()
            && session_start.is_empty()
            && session_end.is_empty()
            && user_prompt_submit.is_empty()
            && subagent_start.is_empty()
            && subagent_stop.is_empty()
            && stop.is_empty()
            && interrupt.is_empty()
    }

    pub fn handler_count(&self) -> usize {
        let Self {
            pre_tool_use,
            post_tool_use,
            pre_compact,
            post_compact,
            session_start,
            session_end,
            user_prompt_submit,
            subagent_start,
            subagent_stop,
            stop,
            interrupt,
        } = self;
        [
            pre_tool_use,
            post_tool_use,
            pre_compact,
            post_compact,
            session_start,
            session_end,
            user_prompt_submit,
            subagent_start,
            subagent_stop,
            stop,
            interrupt,
        ]
        .into_iter()
        .flatten()
        .map(|group| group.hooks.len())
        .sum()
    }

    pub fn into_matcher_groups(mut self) -> [(HookEventName, Vec<MatcherGroup>); 11] {
        self.matcher_groups_mut()
            .map(|(event, groups)| (event, std::mem::take(groups)))
    }

    pub fn matcher_groups_mut(&mut self) -> [(HookEventName, &mut Vec<MatcherGroup>); 11] {
        use HookEventName as Event;

        [
            (Event::PreToolUse, &mut self.pre_tool_use),
            (Event::PostToolUse, &mut self.post_tool_use),
            (Event::PreCompact, &mut self.pre_compact),
            (Event::PostCompact, &mut self.post_compact),
            (Event::SessionStart, &mut self.session_start),
            (Event::SessionEnd, &mut self.session_end),
            (Event::UserPromptSubmit, &mut self.user_prompt_submit),
            (Event::SubagentStart, &mut self.subagent_start),
            (Event::SubagentStop, &mut self.subagent_stop),
            (Event::Stop, &mut self.stop),
            (Event::Interrupt, &mut self.interrupt),
        ]
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MatcherGroup {
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookHandlerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum HookHandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default, rename = "commandWindows", alias = "command_windows")]
        command_windows: Option<String>,
        #[serde(default, rename = "timeout")]
        timeout_sec: Option<u64>,
        #[serde(default)]
        r#async: bool,
        #[serde(default, rename = "statusMessage")]
        status_message: Option<String>,
        /// Approximate token threshold for spilling this hook's `additionalContext` to disk.
        /// Unset uses 2,500 tokens; `0` disables spilling for this hook. The threshold is
        /// evaluated against the original context; a spilled preview also includes recovery
        /// metadata.
        #[serde(
            default,
            rename = "additionalContextLimit",
            skip_serializing_if = "Option::is_none"
        )]
        additional_context_limit: Option<usize>,
    },
    #[serde(rename = "mcp_tool")]
    McpTool {
        server: String,
        tool: String,
        #[serde(default)]
        input: serde_json::Map<String, serde_json::Value>,
        #[serde(default, rename = "timeout")]
        timeout_sec: Option<u64>,
        #[serde(default, rename = "statusMessage")]
        status_message: Option<String>,
    },
    #[serde(rename = "prompt")]
    Prompt {},
    #[serde(rename = "agent")]
    Agent {},
}
