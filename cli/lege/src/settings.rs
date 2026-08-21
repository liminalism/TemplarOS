//! `lege settings` — the typed settings schema (`crates/lege-session-core/src/settings.rs`):
//! every key has a description, a default, and its changes are recorded in history.
//!
//! Every D-Bus call goes through [`SettingsClient`] so the rendering below is testable with a
//! fixture, never a live `org.legeos.Settings1`.

use crate::humanize::timestamp_ms;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

/// What `lege settings` needs from `org.legeos.Settings1`.
#[async_trait]
pub trait SettingsClient {
    /// `(key, description, default_json, privileged)`
    async fn schema(&self) -> Result<Vec<(String, String, String, bool)>>;
    async fn get(&self, key: &str) -> Result<String>;
    async fn set(&self, key: &str, value_json: &str, origin: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<(String, String)>>;
    async fn changes(&self, limit: u32) -> Result<Vec<String>>;
    async fn reset(&self, key: &str, origin: &str) -> Result<()>;
}

#[async_trait]
impl SettingsClient for lege_client::proxies::SettingsProxy<'_> {
    async fn schema(&self) -> Result<Vec<(String, String, String, bool)>> {
        self.schema().await.context("Settings1.Schema")
    }
    async fn get(&self, key: &str) -> Result<String> {
        self.get(key).await.context("Settings1.Get")
    }
    async fn set(&self, key: &str, value_json: &str, origin: &str) -> Result<()> {
        self.set(key, value_json, origin)
            .await
            .context("Settings1.Set")
    }
    async fn list(&self) -> Result<Vec<(String, String)>> {
        self.list().await.context("Settings1.List")
    }
    async fn changes(&self, limit: u32) -> Result<Vec<String>> {
        self.changes(limit).await.context("Settings1.Changes")
    }
    async fn reset(&self, key: &str, origin: &str) -> Result<()> {
        self.reset(key, origin).await.context("Settings1.Reset")
    }
}

/// One row of `Schema`.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaEntry {
    pub key: String,
    pub description: String,
    pub default_json: String,
    pub privileged: bool,
}

pub fn render_schema(schema: &[(String, String, String, bool)]) -> String {
    if schema.is_empty() {
        return "no known settings".to_owned();
    }
    let rows: Vec<Vec<String>> = schema
        .iter()
        .map(|(key, description, default_json, privileged)| {
            vec![
                key.clone(),
                render_value(default_json),
                if *privileged {
                    "privileged".to_owned()
                } else {
                    String::new()
                },
                description.clone(),
            ]
        })
        .collect();
    crate::format::table(&rows)
}

/// One `(key, value_json)` from `List`, or the answer to `Get`.
pub fn render_value(value_json: &str) -> String {
    serde_json::from_str::<Value>(value_json)
        .map_or_else(|_| value_json.to_owned(), |value| render_json_value(&value))
}

fn render_json_value(value: &Value) -> String {
    match value {
        // A `SettingValue` on the wire is `{"type":"...","value":...}`; show just the payload.
        Value::Object(map) if map.contains_key("type") && map.contains_key("value") => {
            render_json_value(&map["value"])
        }
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items
            .iter()
            .map(render_json_value)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

pub fn render_list(values: &[(String, String)]) -> String {
    if values.is_empty() {
        return "no known settings".to_owned();
    }
    let rows: Vec<Vec<String>> = values
        .iter()
        .map(|(key, value_json)| vec![key.clone(), render_value(value_json)])
        .collect();
    crate::format::table(&rows)
}

/// Mirrors the JSON object shape `Settings1.Changes` documents:
/// `{"key","old_json","new_json","origin","at_ms"}`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Change {
    pub key: String,
    #[serde(default)]
    pub old_json: Option<String>,
    pub new_json: String,
    pub origin: String,
    pub at_ms: i64,
}

pub fn parse_changes(changes_json: &[String]) -> Vec<Change> {
    changes_json
        .iter()
        .filter_map(|json| serde_json::from_str(json).ok())
        .collect()
}

pub fn render_changes(changes: &[Change]) -> String {
    if changes.is_empty() {
        return "no recorded settings changes".to_owned();
    }
    changes
        .iter()
        .map(|change| {
            format!(
                "{}  {} = {} (was {}) [{}]",
                timestamp_ms(change.at_ms),
                change.key,
                render_value(&change.new_json),
                change
                    .old_json
                    .as_deref()
                    .map_or_else(|| "the schema default".to_owned(), render_value),
                change.origin,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_rows_show_default_and_privilege() {
        let schema = vec![(
            "resources.mode".to_owned(),
            "Resource governor mode".to_owned(),
            r#"{"type":"choice","value":"observe-only"}"#.to_owned(),
            false,
        )];
        let rendered = render_schema(&schema);
        assert!(rendered.contains("resources.mode"));
        assert!(rendered.contains("observe-only"));
        assert!(rendered.contains("Resource governor mode"));
    }

    #[test]
    fn setting_values_render_their_payload_not_the_wire_wrapper() {
        assert_eq!(render_value(r#"{"type":"int","value":3}"#), "3");
        assert_eq!(render_value(r#"{"type":"bool","value":true}"#), "true");
        assert_eq!(
            render_value(r#"{"type":"str-list","value":["a","b"]}"#),
            "a, b"
        );
    }

    #[test]
    fn changes_render_old_and_new_with_origin() {
        let changes = parse_changes(&[r#"{"key":"resources.mode","old_json":null,
                "new_json":"{\"type\":\"choice\",\"value\":\"enforce\"}",
                "origin":"user","at_ms":1787401920000}"#
            .to_owned()]);
        assert_eq!(changes.len(), 1);
        let rendered = render_changes(&changes);
        assert!(rendered.contains("resources.mode = enforce"));
        assert!(rendered.contains("schema default"));
        assert!(rendered.contains("[user]"));
    }
}
