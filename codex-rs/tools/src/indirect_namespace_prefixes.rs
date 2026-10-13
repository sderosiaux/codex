//! Adds namespace guidance to indirect tool descriptions after selection.
//! Conflicting prefixes for the same final namespace are rejected before rendering.
//! Direct specifications, search indexes, and cached definitions remain unchanged.

use crate::LoadableToolSpec;
use codex_protocol::openai_models::IndirectDescriptionPrefixes;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// Conflicting catalog selectors for the same namespace, without including their prompt text.
#[derive(Debug, thiserror::Error)]
#[error(
    "Conflicting indirect description prefixes for namespace `{namespace}`: `{first_source}` and `{second_source}`"
)]
pub struct IndirectNamespacePrefixConflict {
    namespace: String,
    first_source: String,
    second_source: String,
}

/// Borrowed model text applied only to fresh indirect presentation data.
pub struct IndirectNamespacePrefixes<'a> {
    prefixes: BTreeMap<String, &'a str>,
}

impl<'a> IndirectNamespacePrefixes<'a> {
    /// Expands configured server names using registered tools' final namespaces.
    /// Overlapping selectors must agree after trimming, including explicit empty values.
    pub fn new<'s>(
        prefixes: Option<&'a IndirectDescriptionPrefixes>,
        namespaces: impl IntoIterator<Item = (&'s str, &'s str)>,
    ) -> Result<Self, IndirectNamespacePrefixConflict> {
        let mut resolved = BTreeMap::new();
        if let Some(namespaces) = prefixes.and_then(|prefixes| prefixes.namespaces.as_ref()) {
            for (namespace, prefix) in namespaces {
                resolved.insert(
                    namespace.clone(),
                    (prefix.trim(), format!("namespaces.{namespace}")),
                );
            }
        }
        if let Some(servers) = prefixes
            .and_then(|prefixes| prefixes.mcp_servers.as_ref())
            .filter(|servers| !servers.is_empty())
        {
            for (server, namespace) in namespaces {
                if let Some(prefix) = servers.get(server) {
                    let prefix = prefix.trim();
                    match resolved.entry(namespace.to_owned()) {
                        Entry::Vacant(entry) => {
                            entry.insert((prefix, format!("mcp_servers.{server}")));
                        }
                        Entry::Occupied(entry) if entry.get().0 != prefix => {
                            return Err(IndirectNamespacePrefixConflict {
                                namespace: namespace.to_owned(),
                                first_source: entry.get().1.clone(),
                                second_source: format!("mcp_servers.{server}"),
                            });
                        }
                        Entry::Occupied(_) => {}
                    }
                }
            }
        }
        Ok(Self {
            prefixes: resolved
                .into_iter()
                .filter(|(_, (prefix, _))| !prefix.is_empty())
                .map(|(namespace, (prefix, _))| (namespace, prefix))
                .collect(),
        })
    }

    /// Applies prefixes after search ranking and namespace coalescing.
    pub fn apply_search(&self, tools: &mut [LoadableToolSpec]) {
        if self.prefixes.is_empty() {
            return;
        }
        for spec in tools {
            if let LoadableToolSpec::Namespace(namespace) = spec
                && let Some(prefix) = self.prefix(&namespace.name)
            {
                prepend(prefix, &mut namespace.description);
            }
        }
    }

    fn prefix(&self, namespace: &str) -> Option<&'a str> {
        self.prefixes.get(namespace).copied()
    }
}

fn prepend(prefix: &str, description: &mut String) {
    *description = if description.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}\n\n{description}")
    };
}

#[cfg(test)]
#[path = "indirect_namespace_prefixes_tests.rs"]
mod tests;
