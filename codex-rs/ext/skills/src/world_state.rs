use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;
use serde::Serialize;
use serde_json::json;

use crate::render::SkillRenderReport;

pub(crate) const HOST_SKILLS_WORLD_STATE_ID: &str = "host_skills";
const NO_HOST_SKILLS_BODY: &str =
    "\n## Host skills update\nNo host skills are currently available.\n";
const HIDDEN_HOST_SKILLS_BODY: &str = "\n## Host skills update\nHost skills are not listed automatically. Explicit skill mentions can still be resolved when available.\n";
const OMITTED_HOST_SKILLS_BODY: &str = "\n## Host skills update\nHost skills are available but omitted from the model-visible skills list because the skills context budget was exceeded.\n";

pub(crate) type CatalogRenderCallback = Box<dyn Fn() + Send + Sync>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSnapshot {
    body: Option<String>,
    include_instructions: bool,
}

fn skills_world_state_section(
    id: &'static str,
    state: CatalogSnapshot,
    no_skills_body: &'static str,
    hidden_skills_body: &'static str,
    on_render: CatalogRenderCallback,
) -> WorldStateSectionContribution {
    let snapshot = json!(&state);
    let CatalogSnapshot {
        body,
        include_instructions,
    } = state;
    let retained_body = body.clone();

    let contribution = WorldStateSectionContribution::new(id, move |previous| {
        if let PreviousWorldStateSection::Known(previous) = &previous {
            let previous_body = previous.get("body").and_then(serde_json::Value::as_str);
            let previous_include_instructions = previous
                .get("includeInstructions")
                .and_then(serde_json::Value::as_bool);
            if previous_body == body.as_deref()
                && previous_include_instructions == Some(include_instructions)
            {
                return (Some(snapshot.clone()), None);
            }
        }

        let body = match body.as_deref() {
            Some(body) => body,
            None if matches!(previous, PreviousWorldStateSection::Absent) => {
                return (Some(snapshot.clone()), None);
            }
            None if !include_instructions => hidden_skills_body,
            None => no_skills_body,
        };
        on_render();

        (
            Some(snapshot.clone()),
            Some(RenderedWorldStateFragment::new(
                "developer",
                (SKILLS_INSTRUCTIONS_OPEN_TAG, SKILLS_INSTRUCTIONS_CLOSE_TAG),
                body,
            )),
        )
    });
    match retained_body {
        Some(body) => contribution.with_retained_fragment_matcher(move |role, text| {
            role == "developer" && text.contains(&body)
        }),
        None => contribution,
    }
}

pub(crate) fn host_skills_world_state_section(
    body: Option<String>,
    include_instructions: bool,
    report: &SkillRenderReport,
    on_render: CatalogRenderCallback,
) -> WorldStateSectionContribution {
    let body = body.or_else(|| {
        (report.included_count == 0 && report.omitted_count > 0)
            .then(|| OMITTED_HOST_SKILLS_BODY.to_string())
    });
    let retained_fragment = body
        .as_ref()
        .map(|body| format!("{SKILLS_INSTRUCTIONS_OPEN_TAG}{body}{SKILLS_INSTRUCTIONS_CLOSE_TAG}"));

    let contribution = skills_world_state_section(
        HOST_SKILLS_WORLD_STATE_ID,
        CatalogSnapshot {
            body,
            include_instructions,
        },
        NO_HOST_SKILLS_BODY,
        HIDDEN_HOST_SKILLS_BODY,
        on_render,
    );
    match retained_fragment {
        Some(fragment) => contribution.with_retained_fragment_matcher(move |role, text| {
            role == "developer" && text.contains(&fragment)
        }),
        None => contribution,
    }
}
