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
