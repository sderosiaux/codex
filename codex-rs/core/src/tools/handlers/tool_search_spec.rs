use codex_tools::JsonSchema;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use codex_utils_string::take_bytes_at_char_boundary;
use std::collections::BTreeMap;

const MAX_TOOL_SEARCH_SOURCE_DESCRIPTION_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolSearchSourceListing {
    Include,
    Omit,
}

pub(crate) fn create_tool_search_tool(
    searchable_sources: &[ToolSearchSourceInfo],
    default_limit: usize,
    source_listing: ToolSearchSourceListing,
) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            JsonSchema::string(Some("Search query for deferred tools.".to_string())),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some(format!(
                "Maximum number of tools to return. Defaults to {default_limit}."
            ))),
        ),
    ]);

    let source_section = match source_listing {
        ToolSearchSourceListing::Include => {
            let mut source_descriptions = BTreeMap::new();
            for source in searchable_sources {
                source_descriptions
                    .entry(source.name.clone())
                    .and_modify(|existing: &mut Option<String>| {
                        if existing.is_none() {
                            *existing = source.description.clone();
                        }
                    })
                    .or_insert(source.description.clone());
            }

            let source_descriptions = if source_descriptions.is_empty() {
                "None currently enabled.".to_string()
            } else {
                let reserved_name_bytes = source_descriptions.keys().fold(
                    source_descriptions.len().saturating_sub(1),
                    |reserved, name| reserved.saturating_add(2).saturating_add(name.len()),
                );
                let mut description_budget =
                    MAX_TOOL_SEARCH_SOURCE_DESCRIPTION_BYTES.saturating_sub(reserved_name_bytes);
                let mut rendered = String::new();
                for (name, description) in source_descriptions {
                    let separator_bytes = usize::from(!rendered.is_empty());
                    let required = separator_bytes.saturating_add(2).saturating_add(name.len());
                    if required
                        > MAX_TOOL_SEARCH_SOURCE_DESCRIPTION_BYTES.saturating_sub(rendered.len())
                    {
                        continue;
                    }

                    if !rendered.is_empty() {
                        rendered.push('\n');
                    }
                    rendered.push_str("- ");
                    rendered.push_str(&name);

                    if let Some(description) = description
                        && description_budget >= 2
                    {
                        rendered.push_str(": ");
                        description_budget -= 2;
                        let bounded_description =
                            take_bytes_at_char_boundary(&description, description_budget);
                        rendered.push_str(bounded_description);
                        description_budget -= bounded_description.len();
                    }
                }
                rendered
            };
            format!(
                "\n\nYou have access to tools from the following sources:\n{source_descriptions}\n"
            )
        }
        ToolSearchSourceListing::Omit => "\n\n".to_string(),
    };

    let description = format!(
        "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.{source_section}Some of the tools may not have been provided to you upfront, and you should use this tool (`{TOOL_SEARCH_TOOL_NAME}`) to search for the required tools. For MCP tool discovery, always use `{TOOL_SEARCH_TOOL_NAME}` instead of `list_mcp_resources` or `list_mcp_resource_templates`."
    );

    ToolSpec::ToolSearch {
        execution: "client".to_string(),
        description,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
    }
}
