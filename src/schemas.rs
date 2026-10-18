//! JSON schemas for Kuana's own files: `settings.json` (from the `Settings`
//! structs, so it can't fall behind them), `keymap.json` (action names) and
//! themes (color keys). The JSON language server gets them (`json.schemas`,
//! see `lsp::servers`), so editing these files has completion, hover docs
//! and checks, with comments allowed.

use std::sync::OnceLock;

use gpui::App;
use serde_json::{Value, json};

static SCHEMAS: OnceLock<Value> = OnceLock::new();

/// Builds the schemas, once (action names need the app).
pub fn init(cx: &App) {
    SCHEMAS.get_or_init(|| build(cx));
}

/// The JSON server's `json.schemas` list (file patterns and schemas), or
/// null before `init`.
pub fn json_schemas() -> Value {
    SCHEMAS.get().cloned().unwrap_or(Value::Null)
}

fn build(cx: &App) -> Value {
    let Some(dir) = crate::theme::config_dir() else { return Value::Null };
    // Matched by the end of the path: "kuana/settings.json".
    let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut actions: Vec<String> = crate::keymap::action_entries().into_iter().map(|e| e.name.to_string()).collect();
    actions.extend(cx.all_action_names().iter().map(|name| name.to_string()));
    actions.sort();
    actions.dedup();
    json!([
        { "fileMatch": [format!("{folder}/settings.json")], "schema": settings_schema() },
        { "fileMatch": [format!("{folder}/keymap.json")], "schema": keymap_schema(&actions) },
        { "fileMatch": [format!("{folder}/themes/*.json")], "schema": theme_schema() },
        { "fileMatch": [crate::project_settings::FILE], "schema": project_settings_schema() },
    ])
}

/// Kuana's files are JSON with comments and trailing commas (`allowComments`
/// and `allowTrailingCommas` are the JSON server's own keywords).
fn jsonc(mut schema: Value) -> Value {
    schema["allowComments"] = json!(true);
    schema["allowTrailingCommas"] = json!(true);
    schema
}

fn settings_schema() -> Value {
    jsonc(serde_json::to_value(schemars::schema_for!(crate::settings::Settings)).unwrap_or(Value::Null))
}

/// `settings.json`'s schema with only what a project's `.kuana/settings.json` takes.
fn project_settings_schema() -> Value {
    let mut schema = settings_schema();
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        let allowed = |key: &str| crate::project_settings::PROJECT_KEYS.contains(&key) || crate::settings::EDITOR_KEYS.contains(&key);
        properties.retain(|key, _| allowed(key));
    }
    schema["additionalProperties"] = json!(false);
    schema
}

fn keymap_schema(actions: &[String]) -> Value {
    jsonc(json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "context": {
                    "type": "string",
                    "description": "Where the bindings apply, e.g. \"Editor\" or \"Terminal\"; everywhere when absent.",
                },
                "bindings": {
                    "type": "object",
                    "description": "Keys (\"cmd-shift-p\", \"ctrl-k ctrl-s\") to an action, or null to remove a built-in binding.",
                    "additionalProperties": { "anyOf": [{ "enum": actions }, { "type": "null" }] },
                },
            },
            "required": ["bindings"],
        },
    }))
}

fn theme_schema() -> Value {
    let color = json!({ "type": "string", "pattern": "^#([0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$" });
    let mut colors = serde_json::Map::new();
    let mut syntax = serde_json::Map::new();
    for key in crate::theme::COLOR_KEYS {
        match key.strip_prefix("syntax.") {
            Some(name) => syntax.insert(name.to_string(), color.clone()),
            None => colors.insert(key.to_string(), color.clone()),
        };
    }
    jsonc(json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "appearance": { "enum": ["dark", "light"] },
            "extends": { "type": "string", "description": "A theme to take the colors this one doesn't set from." },
            "italic_comments": { "type": "boolean" },
            "deep_chrome": { "type": "boolean" },
            "colors": { "type": "object", "properties": colors, "additionalProperties": false },
            // Any capture name ("keyword.control") besides the base kinds.
            "syntax": { "type": "object", "properties": syntax, "additionalProperties": color },
        },
        "required": ["name"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_describe_kins_files() {
        let settings = settings_schema();
        let properties = &settings["properties"];
        // Top-level settings, the editor ones flattened in, and lsp.
        for key in ["theme", "tab_size", "soft_wrap", "lsp", "language_servers"] {
            assert!(properties.get(key).is_some(), "{key} missing");
        }
        assert_eq!(settings["allowComments"], true);
        let project = project_settings_schema();
        assert!(project["properties"].get("tab_size").is_some() && project["properties"].get("formatters").is_some());
        assert!(project["properties"].get("theme").is_none());

        let keymap = keymap_schema(&["editor::MoveLeft".into()]);
        assert_eq!(keymap["items"]["properties"]["bindings"]["additionalProperties"]["anyOf"][0]["enum"][0], "editor::MoveLeft");

        let theme = theme_schema();
        assert!(theme["properties"]["colors"]["properties"].get("background").is_some());
        assert!(theme["properties"]["syntax"]["properties"].get("keyword").is_some());
        assert!(theme["properties"]["colors"]["properties"].get("syntax.keyword").is_none());
    }
}
