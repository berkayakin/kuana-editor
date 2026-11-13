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
    let mut schemas = SCHEMAS.get().cloned().unwrap_or(Value::Null);
    // Extensions' own settings (`[kuana.settings]`), as installed now: asked
    // when the JSON server starts, not while Kuana does.
    let extensions: Vec<(String, Value)> = crate::extensions::contributions()
        .iter()
        .filter(|c| !c.settings.is_empty())
        .map(|c| (c.extension.clone(), extension_settings_schema(&c.name, &c.settings)))
        .collect();
    if let Some(settings) = schemas.get_mut(0).and_then(|entry| entry.get_mut("schema"))
        && !extensions.is_empty()
    {
        add_extension_settings(settings, extensions);
    }
    schemas
}

/// Puts each extension's settings under `extensions.<id>` of `settings.json`'s schema.
fn add_extension_settings(settings: &mut Value, extensions: Vec<(String, Value)>) {
    let properties: serde_json::Map<String, Value> = extensions.into_iter().collect();
    settings["properties"]["extensions"] = json!({
        "description": "Extensions' own settings, by extension id.",
        "type": ["object", "null"],
        "properties": properties,
        "additionalProperties": true,
    });
}

/// One extension's settings as a schema: its keys with their types, defaults
/// and descriptions (other keys are let through: the code may read more).
fn extension_settings_schema(name: &str, settings: &[crate::extensions::contrib::SettingDef]) -> Value {
    use crate::extensions::contrib::SettingKind;
    let mut properties = serde_json::Map::new();
    for setting in settings {
        let mut schema = match &setting.kind {
            SettingKind::Text => json!({ "type": "string" }),
            SettingKind::Boolean => json!({ "type": "boolean" }),
            SettingKind::Number => json!({ "type": "number" }),
            SettingKind::Choice(values) => json!({ "type": "string", "enum": values }),
        };
        schema["title"] = json!(setting.title);
        if let Some(description) = &setting.description {
            schema["description"] = json!(description);
        }
        if let Some(default) = &setting.default {
            schema["default"] = default.clone();
        }
        properties.insert(setting.key.clone(), schema);
    }
    json!({ "description": format!("Settings of the {name} extension."), "type": "object", "properties": properties })
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
    fn extensions_settings_join_the_settings_schema() {
        use crate::extensions::contrib::{SettingDef, SettingKind};
        let defs = [
            SettingDef { key: "folder".into(), title: "Notes folder".into(), description: Some("Where notes are kept.".into()), kind: SettingKind::Text, default: Some(json!("~/Notes")) },
            SettingDef { key: "order".into(), title: "Sort by".into(), description: None, kind: SettingKind::Choice(vec!["name".into(), "date".into()]), default: None },
        ];
        let mut settings = settings_schema();
        add_extension_settings(&mut settings, vec![("notes".into(), extension_settings_schema("Notes", &defs))]);
        let notes = &settings["properties"]["extensions"]["properties"]["notes"]["properties"];
        assert_eq!(notes["folder"]["default"], json!("~/Notes"));
        assert_eq!(notes["folder"]["description"], json!("Where notes are kept."));
        assert_eq!(notes["order"]["enum"], json!(["name", "date"]));
        // The rest of the settings are still described.
        assert!(settings["properties"]["theme"].is_object());
    }

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
