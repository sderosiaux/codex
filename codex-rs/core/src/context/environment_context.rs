use codex_utils_path_uri::PathUri;

/// Workspace roots visible to the model for the primary environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileSystemContext {
    workspace_roots: Vec<String>,
}

impl FileSystemContext {
    /// Returns `None` when there are no workspace roots to describe.
    pub(super) fn from_workspace_roots(workspace_roots: &[PathUri]) -> Option<Self> {
        (!workspace_roots.is_empty()).then(|| Self {
            workspace_roots: workspace_roots
                .iter()
                .map(PathUri::inferred_native_path_string)
                .collect(),
        })
    }

    pub(super) fn render(&self) -> String {
        let mut rendered = "<filesystem><workspace_roots>".to_string();
        for root in &self.workspace_roots {
            push_text_element(&mut rendered, "root", root);
        }
        rendered.push_str("</workspace_roots></filesystem>");
        rendered
    }
}

fn push_text_element(rendered: &mut String, name: &str, value: &str) {
    rendered.push_str(&format!("<{name}>"));
    push_xml_escaped_text(rendered, value);
    rendered.push_str(&format!("</{name}>"));
}

pub(crate) fn push_xml_escaped_text(rendered: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => rendered.push_str("&amp;"),
            '<' => rendered.push_str("&lt;"),
            '>' => rendered.push_str("&gt;"),
            '"' => rendered.push_str("&quot;"),
            '\'' => rendered.push_str("&apos;"),
            _ => rendered.push(ch),
        }
    }
}
