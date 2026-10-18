//! Project settings: `.kuana/settings.json` in a project's folder (or any
//! folder above a file), committed with the code so everyone working on it
//! gets the same editor options, formatters, language server settings and
//! run configurations, as Zed's `.zed/settings.json`. For files under that
//! folder they go over the user's `settings.json` (`Settings::at`,
//! `Settings::for_file`); a nested folder's file goes over an outer one's.
//!
//! Only project-level keys are read (`PROJECT_KEYS` and the editor options);
//! the theme, fonts, keymap and the like stay the user's. In a folder that
//! isn't trusted only editor options apply, since the rest names programs to
//! run (language servers, formatters, run commands).
//!
//! Files are read once and kept (`LAYERS`, usable from any thread); changes
//! apply when `refresh` finds them, which `settings::reload_if_changed` calls
//! (on save, window focus and watcher changes), like `settings.json`'s own.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::SystemTime,
};

use serde_json::Value;

use crate::settings::{EDITOR_KEYS, Settings};

/// The folder in a project that holds its settings.
pub const DIR: &str = ".kuana";
/// The settings file, relative to the project folder.
pub const FILE: &str = ".kuana/settings.json";

/// Top-level keys a project file may set besides the editor options.
pub const PROJECT_KEYS: &[&str] = &["languages", "language_servers", "lsp", "formatters", "run_configurations"];

/// What a folder's settings file held when it was last read.
struct Layer {
    /// Modified time and length; `None` when there's no file.
    stamp: Option<(SystemTime, u64)>,
    /// `None` when there's no file or it isn't valid JSON.
    settings: Option<Arc<Settings>>,
}

/// Every folder looked at so far, with or without a settings file, so
/// `refresh` also notices one that appears.
static LAYERS: LazyLock<Mutex<HashMap<PathBuf, Layer>>> = LazyLock::new(Default::default);
/// Problems found reading files, until `take_problems` reports them.
static PROBLEMS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn stamp(dir: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(dir.join(FILE)).ok()?;
    meta.is_file().then(|| (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()))
}

/// Reads `dir`'s settings file, noting its problems.
fn read(dir: &Path) -> Layer {
    let stamp = stamp(dir);
    if stamp.is_none() {
        return Layer { stamp, settings: None };
    }
    // "app/.kuana/settings.json": the folder's name tells which project.
    let folder = dir.file_name().map(|n| format!("{}/", n.to_string_lossy())).unwrap_or_default();
    let label = format!("{folder}{FILE}: ");
    let text = std::fs::read_to_string(dir.join(FILE)).unwrap_or_default();
    let (settings, problems) = parse(&text);
    PROBLEMS.lock().unwrap_or_else(|e| e.into_inner()).extend(problems.into_iter().map(|p| format!("{label}{p}")));
    Layer { stamp, settings: settings.map(Arc::new) }
}

/// A project settings file's settings (`None` when it isn't JSON) and its
/// problems: keys only the user's settings.json takes are left out.
pub fn parse(text: &str) -> (Option<Settings>, Vec<String>) {
    let mut problems = Vec::new();
    if text.trim().is_empty() {
        return (Some(Settings::default()), problems);
    }
    let mut value: Value = match serde_json::from_str(&crate::settings::strip_jsonc(text)) {
        Ok(value) => value,
        Err(err) => return (None, vec![err.to_string()]),
    };
    let Value::Object(map) = &mut value else { return (None, vec!["expected an object of settings".into()]) };
    let known = Settings::unknown_keys(text);
    map.retain(|key, _| {
        let allowed = PROJECT_KEYS.contains(&key.as_str()) || EDITOR_KEYS.contains(&key.as_str());
        if !allowed && !known.iter().any(|p| p.contains(&format!("\"{key}\""))) {
            problems.push(format!("\"{key}\" can only be set in your own settings.json"));
        }
        allowed
    });
    problems.extend(known);
    match Settings::from_value_reporting(value) {
        Ok((settings, invalid)) => {
            problems.extend(invalid);
            (Some(settings), problems)
        }
        Err(err) => {
            problems.push(err);
            (None, problems)
        }
    }
}

/// The project settings that apply at `path` (a file or a folder), the
/// outermost folder's first. In folders that aren't trusted, editor options
/// only.
pub fn layers(path: &Path) -> Vec<Arc<Settings>> {
    let mut found = Vec::new();
    let mut layers = LAYERS.lock().unwrap_or_else(|e| e.into_inner());
    for dir in path.ancestors() {
        let layer = layers.entry(dir.to_path_buf()).or_insert_with(|| read(dir));
        if let Some(settings) = &layer.settings {
            found.push(if crate::trust::is_trusted(dir) { settings.clone() } else { Arc::new(restricted(settings)) });
        }
    }
    found.reverse();
    found
}

/// What applies before a folder is trusted: editor options.
fn restricted(settings: &Settings) -> Settings {
    Settings { editor: settings.editor.clone(), languages: settings.languages.clone(), ..Settings::default() }
}

/// Reads again the files that changed (or appeared, or went) since they were
/// read. Whether any did.
pub fn refresh() -> bool {
    let mut layers = LAYERS.lock().unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    for (dir, layer) in layers.iter_mut() {
        if stamp(dir) != layer.stamp {
            *layer = read(dir);
            changed = true;
        }
    }
    changed
}

/// Problems found in project settings files since the last call.
pub fn take_problems() -> Vec<String> {
    std::mem::take(&mut *PROBLEMS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Whether `path` is a project settings file.
pub fn is_settings_file(path: &Path) -> bool {
    path.ends_with(FILE)
}

/// Written by Open Project Settings when the project has none yet.
pub const TEMPLATE: &str = r#"// Kuana settings for this project, shared by everyone who opens it: commit
// this file. They go over each person's own settings.json for files in this
// folder. Changes apply when you save. Comments and trailing commas are allowed.
//
// Editor options ("tab_size", "hard_tabs", "soft_wrap", "format_on_save",
// "rulers", "trim_trailing_whitespace", "insert_final_newline", …),
// "languages", "language_servers", "lsp", "formatters" and
// "run_configurations" work here, written as in settings.json. Until the
// project is trusted, only editor options and "languages" apply.
{
  // "tab_size": 2,
  // "format_on_save": true,

  // "languages": {
  //   "Python": { "tab_size": 4, "rulers": [88] }
  // },

  // "formatters": {
  //   "Python": ["ruff", "format", "-"]
  // },

  // "lsp": {
  //   "rust-analyzer": { "initialization_options": { "check": { "command": "clippy" } } }
  // },

  // Folders are relative to this project.
  // "run_configurations": [
  //   { "name": "Dev server", "command": "npm run dev" }
  // ]
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_only_keys_are_reported_and_dropped() {
        let (settings, problems) = parse(r#"{ "theme": "Ember", "tab_size": 2, "tab_sise": 3, "lsp": { "gopls": {} } }"#);
        let settings = settings.unwrap();
        assert_eq!((settings.theme, settings.editor.tab_size), (None, Some(2)));
        assert!(settings.lsp.contains_key("gopls"));
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("\"theme\" can only be set")));
        assert!(problems.iter().any(|p| p.contains("did you mean \"tab_size\"")));
        assert_eq!(parse("{ nope").0, None);
        assert_eq!(parse("").0, Some(Settings::default()));
    }

    #[test]
    fn nested_folders_layer_and_changes_show_after_refresh() {
        let root = std::env::temp_dir().join(format!("kuana-project-settings-{}", std::process::id()));
        let sub = root.join("packages/app");
        std::fs::create_dir_all(sub.join(DIR)).unwrap();
        std::fs::create_dir_all(root.join(DIR)).unwrap();
        crate::trust::set_trust_all(true);
        std::fs::write(root.join(FILE), r#"{ "tab_size": 2, "formatters": { "Go": false } }"#).unwrap();
        std::fs::write(sub.join(FILE), r#"{ "tab_size": 8 }"#).unwrap();
        let file = sub.join("src/main.go");
        let tabs = |path: &Path| layers(path).iter().map(|l| l.editor.tab_size).collect::<Vec<_>>();
        assert_eq!(tabs(&file), [Some(2), Some(8)]);
        assert_eq!(tabs(&root.join("a.go")), [Some(2)]);

        // A settings file that appears (or changes) applies after refresh.
        std::fs::remove_file(sub.join(FILE)).unwrap();
        assert!(refresh());
        assert_eq!(tabs(&file), [Some(2)]);
        assert!(!refresh());
        std::fs::write(sub.join(FILE), "{ \"tab_size\": 3, \"theme\": \"x\" }").unwrap();
        assert!(refresh());
        assert_eq!(tabs(&file), [Some(2), Some(3)]);
        assert!(take_problems().iter().any(|p| p.contains("\"theme\"")));

        let user = Settings::parse(r#"{ "tab_size": 4, "languages": { "Go": { "tab_size": 6 } }, "formatters": { "Go": ["gofmt"] } }"#).unwrap();
        // The project's top-level option beats the user's per-language one.
        assert_eq!(user.for_file(crate::syntax::Language::Go, Some(&file)).tab_size, 3);
        assert_eq!(user.at(&file).formatters["Go"], Value::Bool(false));
        std::fs::remove_dir_all(&root).ok();
    }
}
