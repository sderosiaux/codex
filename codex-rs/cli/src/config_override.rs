//! `-c key=value` overrides layered on top of `~/.codex/config.toml`.

use anyhow::Context;
use anyhow::bail;
use toml::Value;

/// Splits on the first `=` and parses the right-hand side as TOML, falling back to a bare
/// string so `-c model=gpt-5` works without quoting.
pub(crate) fn parse_overrides(raw: &[String]) -> anyhow::Result<Vec<(String, Value)>> {
    raw.iter().map(|entry| parse_override(entry)).collect()
}

fn parse_override(entry: &str) -> anyhow::Result<(String, Value)> {
    let Some((key, value)) = entry.split_once('=') else {
        bail!("invalid override (missing '='): {entry}");
    };
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key in override: {entry}");
    }
    let value = value.trim();
    let value = match parse_toml_value(value) {
        Ok(value) => value,
        Err(_) => Value::String(value.trim_matches(|c| c == '"' || c == '\'').to_string()),
    };
    Ok((key.to_string(), value))
}

fn parse_toml_value(raw: &str) -> anyhow::Result<Value> {
    let table: toml::Table = toml::from_str(&format!("_x_ = {raw}"))?;
    table.get("_x_").cloned().context("missing sentinel key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_toml_scalars_and_tables() {
        let parsed = parse_overrides(&[
            "model_reasoning_effort=\"high\"".to_string(),
            "agent_max_depth=3".to_string(),
            "features.web_search=true".to_string(),
            "mcp_servers.docs={ command = \"docs-mcp\", args = [\"--stdio\"] }".to_string(),
        ])
        .expect("parse overrides");

        assert_eq!(
            parsed,
            vec![
                (
                    "model_reasoning_effort".to_string(),
                    Value::String("high".to_string())
                ),
                ("agent_max_depth".to_string(), Value::Integer(3)),
                ("features.web_search".to_string(), Value::Boolean(true)),
                (
                    "mcp_servers.docs".to_string(),
                    toml::from_str::<Value>("command = \"docs-mcp\"\nargs = [\"--stdio\"]")
                        .expect("table literal"),
                ),
            ]
        );
    }

    #[test]
    fn falls_back_to_bare_strings() {
        assert_eq!(
            parse_overrides(&["model=gpt-5.5".to_string(), "cwd='/tmp/x=y'".to_string()])
                .expect("parse overrides"),
            vec![
                ("model".to_string(), Value::String("gpt-5.5".to_string())),
                ("cwd".to_string(), Value::String("/tmp/x=y".to_string())),
            ]
        );
    }

    #[test]
    fn rejects_missing_separator_or_key() {
        assert!(parse_overrides(&["model".to_string()]).is_err());
        assert!(parse_overrides(&["=gpt".to_string()]).is_err());
    }
}
