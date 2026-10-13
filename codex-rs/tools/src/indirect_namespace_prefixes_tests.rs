//! Checks prefix normalization and rendering without exercising tool execution.

use super::*;
use codex_protocol::openai_models::IndirectDescriptionPrefixes;
use pretty_assertions::assert_eq;

#[test]
fn conflicting_namespace_and_server_prefixes_report_keys_without_values() {
    for (namespace_prefix, server_prefix) in [
        ("Namespace guidance.", "Server guidance."),
        ("", "Server guidance."),
        ("Namespace guidance.", " \n"),
    ] {
        let configured = IndirectDescriptionPrefixes {
            namespaces: Some(BTreeMap::from([(
                "mcp__reports_one".to_string(),
                namespace_prefix.to_string(),
            )])),
            mcp_servers: Some(BTreeMap::from([(
                "reports.one".to_string(),
                server_prefix.to_string(),
            )])),
        };
        let error = IndirectNamespacePrefixes::new(
            Some(&configured),
            [("reports.one", "mcp__reports_one")],
        )
        .err()
        .expect("conflicting prefixes")
        .to_string();
        assert_eq!(
            error,
            "Conflicting indirect description prefixes for namespace `mcp__reports_one`: `namespaces.mcp__reports_one` and `mcp_servers.reports.one`",
        );
    }
}

#[test]
fn conflicting_mcp_servers_report_the_shared_namespace() {
    let configured = IndirectDescriptionPrefixes {
        mcp_servers: Some(BTreeMap::from([
            ("reports.one".to_string(), "First guidance.".to_string()),
            ("reports.two".to_string(), "Second guidance.".to_string()),
        ])),
        ..Default::default()
    };
    let error = IndirectNamespacePrefixes::new(
        Some(&configured),
        [("reports.one", "shared"), ("reports.two", "shared")],
    )
    .err()
    .expect("conflicting prefixes")
    .to_string();
    assert_eq!(
        error,
        "Conflicting indirect description prefixes for namespace `shared`: `mcp_servers.reports.one` and `mcp_servers.reports.two`",
    );
}
