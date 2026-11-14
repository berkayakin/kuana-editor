//! Importing another editor's setup (Import Settings…, Welcome Setup): VS
//! Code and its forks (settings, keybindings, snippets, extensions) and
//! JetBrains IDEs (font, theme, soft wrap, keymap, IdeaVim). `find_sources`
//! looks for their settings folders, `plan` reads one into what Kuana would
//! change, shown before anything is written; `workspace/import.rs` applies
//! it. Only what maps onto Kuana's own settings is taken; what doesn't is
//! counted in the plan's notes.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// An editor whose settings can be imported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    VsCode,
    JetBrains,
}

/// One editor's settings found on this computer.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    /// As people know it: "Visual Studio Code", "PyCharm 2025.2".
    pub name: String,
    pub kind: Kind,
    /// VS Code's `User` folder, a JetBrains IDE's configuration folder.
    pub dir: PathBuf,
    /// VS Code's extensions folder (`~/.vscode/extensions`).
    pub extensions: Option<PathBuf>,
}

/// VS Code and its forks: their name, folder under the config root, and
/// extensions folder under the home folder.
const VSCODES: &[(&str, &str, &str)] = &[
    ("Visual Studio Code", "Code", ".vscode"),
    ("Visual Studio Code - Insiders", "Code - Insiders", ".vscode-insiders"),
    ("VSCodium", "VSCodium", ".vscode-oss"),
    ("Cursor", "Cursor", ".cursor"),
    ("Windsurf", "Windsurf", ".windsurf"),
];

/// JetBrains IDEs, by their configuration folders' prefix.
const JETBRAINS: &[(&str, &str)] = &[
    ("IntelliJIdea", "IntelliJ IDEA"),
    ("IdeaIC", "IntelliJ IDEA Community"),
    ("PyCharm", "PyCharm"),
    ("PyCharmCE", "PyCharm Community"),
    ("WebStorm", "WebStorm"),
    ("GoLand", "GoLand"),
    ("RustRover", "RustRover"),
    ("CLion", "CLion"),
    ("PhpStorm", "PhpStorm"),
    ("Rider", "Rider"),
    ("RubyMine", "RubyMine"),
    ("DataGrip", "DataGrip"),
];

/// Where editors keep their settings: macOS's Application Support, the XDG
/// config folder, Windows' roaming AppData.
pub fn config_root(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Roaming"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"))
    }
}

/// The editors whose settings are on this computer: each VS Code with a
/// `settings.json`, keybindings or snippets, and each JetBrains IDE's newest
/// version.
pub fn find_sources(root: &Path, home: &Path) -> Vec<Source> {
    let mut sources = Vec::new();
    for (name, folder, extensions) in VSCODES {
        let dir = root.join(folder).join("User");
        if ["settings.json", "keybindings.json", "snippets"].iter().any(|f| dir.join(f).exists()) {
            let extensions = home.join(extensions).join("extensions");
            sources.push(Source { name: name.to_string(), kind: Kind::VsCode, dir, extensions: extensions.is_dir().then_some(extensions) });
        }
    }
    let Ok(read) = std::fs::read_dir(root.join("JetBrains")) else { return sources };
    let mut found: Vec<(String, &str, String, PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let folder = entry.file_name().to_string_lossy().into_owned();
        // The longest prefix: "PyCharmCE2025.1" is PyCharm Community's.
        let Some((prefix, name)) = JETBRAINS.iter().filter(|(p, _)| folder.starts_with(p)).max_by_key(|(p, _)| p.len()) else { continue };
        let version = folder[prefix.len()..].to_string();
        if version.chars().next().is_some_and(|c| c.is_ascii_digit()) && entry.path().join("options").is_dir() {
            found.push((prefix.to_string(), name, version, entry.path()));
        }
    }
    // The newest version of each IDE.
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| version_key(&b.2).cmp(&version_key(&a.2))));
    found.dedup_by(|later, first| later.0 == first.0);
    sources.extend(found.into_iter().map(|(_, name, version, dir)| Source { name: format!("{name} {version}"), kind: Kind::JetBrains, dir, extensions: None }));
    sources
}

fn version_key(version: &str) -> Vec<u32> {
    version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

/// What importing a source would change.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Settings (`settings.json` keys) with how the preview names them.
    pub settings: Vec<(String, Value, String)>,
    /// Per-language settings: Kuana's language name, the key, the value.
    pub languages: Vec<(String, String, Value)>,
    /// Shortcuts for `keymap.json`.
    pub bindings: Vec<Binding>,
    /// Snippet files to copy into Kuana's snippets folder.
    pub snippets: Vec<PathBuf>,
    /// Extensions to install from Kuana's registry: its id, and the
    /// extension it stands in for.
    pub extensions: Vec<(&'static str, String)>,
    /// What wasn't taken, for the preview.
    pub notes: Vec<String>,
}

/// A shortcut to add: keys as `keymap.json` writes them, a Find Action
/// label, and where it applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub keys: String,
    pub action: &'static str,
    pub context: Option<&'static str>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty() && self.languages.is_empty() && self.bindings.is_empty() && self.snippets.is_empty() && self.extensions.is_empty()
    }

    /// The preview's lines, one per change or group of changes.
    pub fn preview(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.settings.iter().map(|(_, _, label)| label.clone()).collect();
        if !self.languages.is_empty() {
            let mut names: Vec<&str> = self.languages.iter().map(|(language, ..)| language.as_str()).collect();
            names.dedup();
            lines.push(format!("Settings for {}", names.join(", ")));
        }
        if !self.bindings.is_empty() {
            lines.push(format!("{} shortcut{} of your own", self.bindings.len(), plural(self.bindings.len())));
        }
        if !self.snippets.is_empty() {
            lines.push(format!("{} snippet file{}", self.snippets.len(), plural(self.snippets.len())));
        }
        if !self.extensions.is_empty() {
            let names: Vec<&str> = self.extensions.iter().map(|(id, _)| *id).collect();
            lines.push(format!("Extensions from Kuana's registry: {}", names.join(", ")));
        }
        lines
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// What importing `source` would change. `themes` are Kuana's theme names
/// (a VS Code theme with one of them is taken); `snippets_dir` is Kuana's
/// snippets folder (files already there aren't copied over).
pub fn plan(source: &Source, themes: &[String], snippets_dir: Option<&Path>) -> Plan {
    match source.kind {
        Kind::VsCode => plan_vscode(source, themes, snippets_dir),
        Kind::JetBrains => plan_jetbrains(source, themes),
    }
}

// ---- VS Code ----------------------------------------------------------------------

fn read_jsonc(path: &Path) -> Option<Value> {
    serde_json::from_str(&crate::settings::strip_jsonc(&std::fs::read_to_string(path).ok()?)).ok()
}

fn plan_vscode(source: &Source, themes: &[String], snippets_dir: Option<&Path>) -> Plan {
    let mut plan = Plan::default();
    let settings = read_jsonc(&source.dir.join("settings.json")).and_then(|v| v.as_object().cloned()).unwrap_or_default();
    plan.settings.push(("base_keymap".into(), json!("VSCode"), "VS Code's shortcuts as the base keymap".into()));
    let font_size = settings.get("editor.fontSize").and_then(Value::as_f64).unwrap_or(if cfg!(target_os = "macos") { 12. } else { 14. });
    let mut unknown_theme = None;
    for (key, value) in &settings {
        if let Some(language) = key.strip_prefix('[').and_then(|k| k.strip_suffix(']')) {
            let Some(overrides) = value.as_object() else { continue };
            for name in language.split("][") {
                let Some(kuana) = vscode_language(name) else { continue };
                for (key, value) in overrides {
                    if let Some((key, value, _)) = vscode_editor_setting(key, value) {
                        plan.languages.push((kuana.to_string(), key.to_string(), value));
                    }
                }
            }
            continue;
        }
        match key.as_str() {
            "editor.fontFamily" => {
                if let Some(family) = value.as_str().and_then(first_font) {
                    plan.settings.push(("editor_font_family".into(), json!(family), format!("Font: {family}")));
                }
            }
            "editor.fontSize" => {
                if let Some(size) = value.as_f64().filter(|s| *s > 0.) {
                    plan.settings.push(("editor_font_size".into(), json!(size), format!("Font size: {size}")));
                }
            }
            // VS Code's is in pixels, or a multiple of the font size under 8.
            "editor.lineHeight" => {
                if let Some(height) = value.as_f64().filter(|h| *h > 0.) {
                    let multiple = if height < 8. { height } else { height / font_size };
                    let multiple = (multiple * 100.).round() / 100.;
                    plan.settings.push(("editor_line_height".into(), json!(multiple), format!("Line height: {multiple}× the font size")));
                }
            }
            "files.autoSave" => {
                let mode = match value.as_str() {
                    Some("off") => "off",
                    Some("afterDelay") => "after_delay",
                    Some("onFocusChange") => "on_focus_change",
                    Some("onWindowChange") => "on_window_change",
                    _ => continue,
                };
                plan.settings.push(("auto_save".into(), json!(mode), format!("Auto-save: {}", mode.replace('_', " "))));
            }
            "files.autoSaveDelay" => {
                if let Some(delay) = value.as_u64() {
                    plan.settings.push(("auto_save_delay".into(), json!(delay), format!("Auto-save delay: {delay} ms")));
                }
            }
            "workbench.colorTheme" => {
                match value.as_str().and_then(|name| matching_theme(name, themes)) {
                    Some(theme) => plan.settings.push(("theme".into(), json!(theme), format!("Theme: {theme}"))),
                    None => unknown_theme = value.as_str().map(str::to_string),
                }
            }
            "workbench.sideBar.location" => {
                if let Some(side) = value.as_str().filter(|s| matches!(*s, "left" | "right")) {
                    plan.settings.push(("sidebar_dock".into(), json!(side), format!("Sidebar on the {side}")));
                }
            }
            _ => {
                if let Some((key, value, label)) = vscode_editor_setting(key, value) {
                    plan.settings.push((key.to_string(), value, label));
                }
            }
        }
    }
    if let Some(theme) = unknown_theme {
        plan.notes.push(format!("Kuana has no theme called “{theme}”; pick one with Select Theme…"));
    }

    // Keybindings.
    let mut skipped = 0;
    for entry in read_jsonc(&source.dir.join("keybindings.json")).and_then(|v| v.as_array().cloned()).unwrap_or_default() {
        let (Some(key), Some(command)) = (entry["key"].as_str(), entry["command"].as_str()) else { continue };
        // A removal ("-command") or a command Kuana doesn't have: left out.
        let binding = vscode_action(command).and_then(|action| {
            let keys = vscode_keys(key)?;
            let context = vscode_context(entry["when"].as_str().unwrap_or(""))?;
            Some(Binding { keys, action, context })
        });
        match binding {
            Some(binding) => plan.bindings.push(binding),
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        plan.notes.push(format!("{skipped} shortcut{} for commands Kuana doesn't have, or under conditions it can't follow, left out", plural(skipped)));
    }

    // Snippets: the same format, copied as they are.
    if let Ok(read) = std::fs::read_dir(source.dir.join("snippets")) {
        let mut files: Vec<PathBuf> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json" || e == "code-snippets"))
            .collect();
        files.sort();
        let (new, existing): (Vec<PathBuf>, Vec<PathBuf>) =
            files.into_iter().partition(|p| !snippets_dir.zip(p.file_name()).is_some_and(|(dir, name)| dir.join(name).exists()));
        plan.snippets = new;
        if !existing.is_empty() {
            plan.notes.push(format!("{} snippet file{} with a name Kuana's snippets already use, left as they are", existing.len(), plural(existing.len())));
        }
    }

    // Extensions: Vim mode, and the registry's for the languages they bring.
    let installed = source.extensions.as_deref().map(vscode_extensions).unwrap_or_default();
    if installed.iter().any(|id| id == "vscodevim.vim") && !plan.settings.iter().any(|(k, ..)| k == "vim_mode") {
        plan.settings.push(("vim_mode".into(), json!(true), "Vim mode (you use VSCodeVim)".into()));
    }
    for id in &installed {
        if let Some(kuana) = registry_extension(id)
            && !plan.extensions.iter().any(|(k, _)| *k == kuana)
        {
            plan.extensions.push((kuana, id.clone()));
        }
    }
    if installed.iter().any(|id| id.starts_with("github.copilot")) {
        plan.notes.push("GitHub Copilot: Kuana shows its suggestions once you turn them on in Settings → Editor".into());
    }
    plan
}

/// An editor setting VS Code and Kuana share (top level or per language):
/// Kuana's key, its value, and how the preview names it.
fn vscode_editor_setting(key: &str, value: &Value) -> Option<(&'static str, Value, String)> {
    let on = |v: bool| if v { "on" } else { "off" };
    Some(match key {
        "editor.tabSize" => {
            let size = value.as_u64().filter(|s| (1..=16).contains(s))?;
            ("tab_size", json!(size), format!("Tab size: {size}"))
        }
        "editor.insertSpaces" => {
            let spaces = value.as_bool()?;
            ("hard_tabs", json!(!spaces), format!("Indent with {}", if spaces { "spaces" } else { "tabs" }))
        }
        "editor.wordWrap" => {
            let wrap = value.as_str()? != "off";
            ("soft_wrap", json!(wrap), format!("Soft wrap: {}", on(wrap)))
        }
        "editor.rulers" => {
            let columns: Vec<u64> = value.as_array()?.iter().filter_map(|r| r.as_u64().or_else(|| r["column"].as_u64())).collect();
            let shown = columns.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
            ("rulers", json!(columns), format!("Rulers: {shown}"))
        }
        "editor.renderWhitespace" => {
            let mode = value.as_str().filter(|m| matches!(*m, "none" | "boundary" | "selection" | "trailing" | "all"))?;
            ("show_whitespace", json!(mode), format!("Show whitespace: {mode}"))
        }
        "editor.formatOnSave" => ("format_on_save", json!(value.as_bool()?), format!("Format on save: {}", on(value.as_bool()?))),
        "files.trimTrailingWhitespace" => {
            ("trim_trailing_whitespace", json!(value.as_bool()?), format!("Trim trailing whitespace: {}", on(value.as_bool()?)))
        }
        "files.insertFinalNewline" => ("insert_final_newline", json!(value.as_bool()?), format!("Final newline: {}", on(value.as_bool()?))),
        "editor.minimap.enabled" => ("minimap", json!(value.as_bool()?), format!("Minimap: {}", on(value.as_bool()?))),
        "editor.codeLens" => ("code_lens", json!(value.as_bool()?), format!("Code lenses: {}", on(value.as_bool()?))),
        "editor.linkedEditing" => ("linked_editing", json!(value.as_bool()?), format!("Linked editing: {}", on(value.as_bool()?))),
        "editor.colorDecorators" => ("document_colors", json!(value.as_bool()?), format!("Color swatches: {}", on(value.as_bool()?))),
        "editor.semanticHighlighting.enabled" => {
            ("semantic_tokens", json!(value.as_bool()?), format!("Semantic highlighting: {}", on(value.as_bool()?)))
        }
        "editor.inlayHints.enabled" => {
            let hints = matches!(value.as_str()?, "on" | "onUnlessPressed");
            ("inlay_hints", json!(hints), format!("Inlay hints: {}", on(hints)))
        }
        _ => return None,
    })
}

/// The first font of a CSS-like list, unless it's a generic family.
fn first_font(list: &str) -> Option<String> {
    let first = list.split(',').next()?.trim().trim_matches(['\'', '"']).trim();
    (!first.is_empty() && !matches!(first, "monospace" | "Menlo" | "Consolas" | "Courier New")).then(|| first.to_string())
}

/// Kuana's theme for a VS Code theme name: the same name, a few aliases.
fn matching_theme(name: &str, themes: &[String]) -> Option<String> {
    let simple = |s: &str| s.to_lowercase().replace('é', "e").chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    let wanted = match simple(name).as_str() {
        "onedarkpro" | "onedarkprodarker" | "atomonedark" => "onedark".to_string(),
        "atomonelight" => "onelight".to_string(),
        "githubdark" | "githubdarkdefault" => "onyx".to_string(),
        other => other.trim_end_matches("theme").to_string(),
    };
    themes.iter().find(|t| simple(t) == wanted).cloned()
}

/// VS Code's language ids, as Kuana names the built-in languages.
fn vscode_language(id: &str) -> Option<&'static str> {
    Some(match id {
        "rust" => "Rust",
        "go" => "Go",
        "python" => "Python",
        "javascript" | "javascriptreact" => "JavaScript",
        "typescript" => "TypeScript",
        "typescriptreact" => "TSX",
        "html" => "HTML",
        "css" => "CSS",
        "markdown" => "Markdown",
        "json" | "jsonc" => "JSON",
        "yaml" => "YAML",
        "toml" => "TOML",
        "shellscript" => "Shell Script",
        "php" => "PHP",
        _ => return None,
    })
}

/// A VS Code command as the Find Action label of Kuana's action for it.
fn vscode_action(command: &str) -> Option<&'static str> {
    Some(match command {
        "workbench.action.quickOpen" => "Go to File",
        "workbench.action.showCommands" => "Find Action",
        "workbench.action.findInFiles" => "Find in Files",
        "workbench.action.replaceInFiles" => "Replace in Files",
        "actions.find" => "Find",
        "editor.action.startFindReplaceAction" => "Replace",
        "editor.action.nextMatchFindAction" => "Find Next Occurrence",
        "editor.action.previousMatchFindAction" => "Find Previous Occurrence",
        "workbench.action.gotoLine" => "Go to Line:Column",
        "workbench.action.gotoSymbol" => "File Structure",
        "workbench.action.showAllSymbols" => "Go to Symbol",
        "workbench.action.navigateBack" => "Navigate Back",
        "workbench.action.navigateForward" => "Navigate Forward",
        "editor.action.revealDefinition" | "editor.action.goToDeclaration" => "Go to Declaration",
        "editor.action.goToImplementation" => "Go to Implementation",
        "editor.action.goToTypeDefinition" => "Go to Type Definition",
        "editor.action.goToReferences" | "editor.action.referenceSearch.trigger" | "references-view.findReferences" => "Find Usages",
        "editor.showCallHierarchy" => "Call Hierarchy",
        "editor.action.rename" => "Rename…",
        "editor.action.quickFix" => "Show Context Actions",
        "editor.action.formatDocument" => "Reformat Code",
        "editor.fold" => "Collapse",
        "editor.unfold" => "Expand",
        "editor.foldAll" => "Collapse All",
        "editor.unfoldAll" => "Expand All",
        "editor.action.triggerSuggest" => "Basic Completion",
        "editor.action.showHover" => "Quick Documentation",
        "editor.action.inlineSuggest.trigger" => "Show Edit Prediction",
        "editor.action.marker.next" | "editor.action.marker.nextInFiles" => "Next Problem",
        "editor.action.marker.prev" | "editor.action.marker.prevInFiles" => "Previous Problem",
        "workbench.action.editor.nextChange" => "Next Change",
        "workbench.action.editor.previousChange" => "Previous Change",
        "editor.action.toggleWordWrap" => "Soft-Wrap",
        "markdown.showPreviewToSide" | "markdown.showPreview" => "Toggle Markdown Preview",
        "workbench.action.terminal.toggleTerminal" => "Terminal",
        "workbench.action.terminal.new" => "New Terminal",
        "workbench.action.terminal.split" => "Split Terminal",
        "workbench.action.splitEditor" | "workbench.action.splitEditorRight" => "Split Right",
        "workbench.action.splitEditorDown" => "Split Down",
        "workbench.action.closeActiveEditor" => "Close Tab",
        "workbench.action.closeOtherEditors" => "Close Other Tabs",
        "workbench.action.closeAllEditors" => "Close All Tabs",
        "workbench.action.nextEditor" => "Next Tab",
        "workbench.action.previousEditor" => "Previous Tab",
        "workbench.action.showAllEditorsByMostRecentlyUsed" => "Recent Files",
        "workbench.action.toggleSidebarVisibility" => "Toggle Project Sidebar",
        "workbench.actions.view.problems" => "Toggle Problems",
        "workbench.action.toggleZenMode" => "Toggle Zen Mode",
        "workbench.view.scm" => "Toggle Commit Panel",
        "workbench.action.files.save" => "Save",
        "workbench.action.files.saveAs" => "Save As…",
        "workbench.action.files.newUntitledFile" => "New File",
        "workbench.action.openRecent" => "Recent Projects…",
        "workbench.action.newWindow" => "New Window",
        "workbench.action.closeWindow" => "Close Window",
        "workbench.action.selectTheme" => "Select Theme…",
        "workbench.action.openSettings" => "Settings…",
        "workbench.action.zoomIn" | "editor.action.fontZoomIn" => "Increase Font Size",
        "workbench.action.zoomOut" | "editor.action.fontZoomOut" => "Decrease Font Size",
        "workbench.action.zoomReset" | "editor.action.fontZoomReset" => "Reset Font Size",
        "editor.action.copyLinesDownAction" => "Duplicate Line or Selection",
        "editor.action.deleteLines" => "Delete Line",
        "editor.action.joinLines" => "Join Lines",
        "editor.action.moveLinesUpAction" => "Move Line Up",
        "editor.action.moveLinesDownAction" => "Move Line Down",
        "editor.action.insertLineAfter" => "Start New Line",
        "editor.action.insertLineBefore" => "Start New Line Before Current",
        "editor.action.commentLine" => "Comment with Line Comment",
        "editor.action.blockComment" => "Comment with Block Comment",
        "editor.action.smartSelect.expand" => "Extend Selection",
        "editor.action.smartSelect.shrink" => "Shrink Selection",
        "editor.action.addSelectionToNextFindMatch" => "Add Selection for Next Occurrence",
        "editor.action.selectHighlights" => "Select All Occurrences",
        "editor.action.insertCursorAbove" => "Add Caret Above",
        "editor.action.insertCursorBelow" => "Add Caret Below",
        "editor.action.transformToUppercase" => "Convert to Upper Case",
        "editor.action.transformToLowercase" => "Convert to Lower Case",
        "editor.action.transformToTitlecase" => "Convert to Title Case",
        "editor.action.transformToSnakecase" => "Convert to snake_case",
        "editor.action.transformToKebabcase" => "Convert to kebab-case",
        "editor.action.sortLinesAscending" => "Sort Lines",
        "editor.action.sortLinesDescending" => "Sort Lines Descending",
        "editor.action.selectAll" => "Select All",
        "expandLineSelection" => "Select Line",
        "undo" => "Undo",
        "redo" => "Redo",
        "workbench.action.debug.start" | "workbench.action.debug.continue" => "Resume Program",
        "workbench.action.debug.run" => "Run",
        "workbench.action.debug.stop" => "Stop",
        "workbench.action.debug.pause" => "Pause Program",
        "workbench.action.debug.stepOver" => "Step Over",
        "workbench.action.debug.stepInto" => "Step Into",
        "workbench.action.debug.stepOut" => "Step Out",
        "editor.debug.action.toggleBreakpoint" => "Toggle Line Breakpoint",
        _ => return None,
    })
}

/// VS Code's keys (`ctrl+shift+p`, `cmd+k cmd+s`) as `keymap.json` writes them.
fn vscode_keys(keys: &str) -> Option<String> {
    let strokes: Option<Vec<String>> = keys
        .split_whitespace()
        .map(|stroke| {
            let stroke = stroke.to_lowercase();
            // A `+` key is written as a trailing "++".
            let (modifiers, key) = match stroke.strip_suffix("++") {
                Some(rest) => (rest, "+".to_string()),
                None => match stroke.rsplit_once('+') {
                    Some((modifiers, key)) => (modifiers, key.to_string()),
                    None => ("", stroke.clone()),
                },
            };
            let mut parts: Vec<String> = Vec::new();
            for modifier in modifiers.split('+').filter(|m| !m.is_empty()) {
                parts.push(
                    match modifier {
                        "ctrl" | "shift" | "alt" | "cmd" => modifier,
                        "meta" | "win" => "cmd",
                        _ => return None,
                    }
                    .to_string(),
                );
            }
            let key = match key.as_str() {
                k if k.starts_with("numpad") || k.starts_with("oem_") || k.starts_with("[key") => return None,
                k => k.to_string(),
            };
            parts.push(key);
            Some(parts.join("-"))
        })
        .collect();
    let strokes = strokes?;
    // Each stroke must be one GPUI knows.
    strokes.iter().all(|s| gpui::Keystroke::parse(s).is_ok()).then(|| strokes.join(" "))
}

/// The keymap context for a binding's `when`: none for an empty one, the
/// editor's or the terminal's for those; None (left out) for conditions
/// Kuana can't follow.
fn vscode_context(when: &str) -> Option<Option<&'static str>> {
    let when = when.trim();
    if when.is_empty() {
        return Some(None);
    }
    let terms: Vec<&str> = when.split("&&").map(str::trim).collect();
    const EDITOR: &[&str] = &["editorTextFocus", "editorFocus", "textInputFocus", "!editorReadonly", "editorHasSelection"];
    if terms.iter().all(|t| EDITOR.contains(t)) {
        Some(Some("Editor"))
    } else if terms.iter().all(|t| matches!(*t, "terminalFocus" | "terminalIsOpen")) {
        Some(Some("Terminal"))
    } else {
        None
    }
}

/// The ids of the extensions installed in a VS Code extensions folder, from
/// its `extensions.json`, else its folders' names.
fn vscode_extensions(dir: &Path) -> Vec<String> {
    if let Some(Value::Array(list)) = read_jsonc(&dir.join("extensions.json")) {
        return list.iter().filter_map(|e| e["identifier"]["id"].as_str()).map(str::to_lowercase).collect();
    }
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    read.flatten()
        .filter(|e| e.path().is_dir())
        // `publisher.name-1.2.3` (and maybe a platform after the version).
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            let cut = name.char_indices().find(|&(i, c)| c == '-' && name[i + 1..].starts_with(|c: char| c.is_ascii_digit())).map(|(i, _)| i)?;
            Some(name[..cut].to_string())
        })
        .collect()
}

/// Kuana's registry extension for a VS Code extension that brings a language
/// Kuana doesn't have built in.
fn registry_extension(id: &str) -> Option<&'static str> {
    Some(match id {
        "redhat.java" | "vscjava.vscode-java-pack" => "java",
        "ms-dotnettools.csharp" | "ms-dotnettools.csdevkit" => "csharp",
        "dart-code.dart-code" | "dart-code.flutter" => "dart",
        "shopify.ruby-lsp" | "rebornix.ruby" => "ruby",
        "ziglang.vscode-zig" => "zig",
        "vue.volar" => "vue",
        "svelte.svelte-vscode" => "svelte",
        "astro-build.astro-vscode" => "astro",
        "ms-vscode.cpptools" | "llvm-vs-code-extensions.vscode-clangd" => "c-cpp",
        "jakebecker.elixir-ls" | "elixir-lsp.elixir-ls" => "elixir",
        "haskell.haskell" => "haskell",
        "sumneko.lua" => "lua",
        "fwcd.kotlin" | "mathiasfrohlich.kotlin" => "kotlin",
        "ms-azuretools.vscode-docker" | "ms-azuretools.vscode-containers" => "dockerfile",
        "jnoortheen.nix-ide" | "bbenoist.nix" => "nix",
        "graphql.vscode-graphql" => "graphql",
        "julialang.language-julia" => "julia",
        "scala-lang.scala" | "scalameta.metals" => "scala",
        "hashicorp.terraform" => "terraform",
        "prisma.prisma" => "prisma",
        "zxh404.vscode-proto3" => "proto",
        "ms-vscode.powershell" => "powershell",
        "swiftlang.swift-vscode" | "sswg.swift-lang" => "swift",
        "myriad-dreamin.tinymist" => "typst",
        "ocamllabs.ocaml-platform" => "ocaml",
        "juanblanco.solidity" => "solidity",
        "mechatroner.rainbow-csv" => "csv",
        "redhat.vscode-xml" => "xml",
        _ => return None,
    })
}

// ---- JetBrains --------------------------------------------------------------------

/// An `<option name="…" value="…" />`'s value in a JetBrains options file.
fn option(xml: &str, name: &str) -> Option<String> {
    let at = xml.find(&format!("name=\"{name}\""))?;
    let rest = &xml[at..xml[at..].find('>').map_or(xml.len(), |end| at + end)];
    let value = rest.split("value=\"").nth(1)?;
    Some(value[..value.find('"')?].to_string())
}

fn plan_jetbrains(source: &Source, themes: &[String]) -> Plan {
    let mut plan = Plan::default();
    let read = |file: &str| std::fs::read_to_string(source.dir.join("options").join(file)).unwrap_or_default();
    let font = read("editor-font.xml");
    if let Some(family) = option(&font, "FONT_FAMILY").filter(|f| !f.is_empty()) {
        plan.settings.push(("editor_font_family".into(), json!(family), format!("Font: {family}")));
    }
    if let Some(size) = option(&font, "FONT_SIZE").and_then(|s| s.parse::<f64>().ok()).filter(|s| *s > 0.) {
        plan.settings.push(("editor_font_size".into(), json!(size), format!("Font size: {size}")));
    }
    // A light IDE theme: Kuana's light base theme (dark ones keep Kuana's default).
    let laf = read("laf.xml");
    let light = laf.split("themeId=\"").nth(1).and_then(|rest| rest.split('"').next()).is_some_and(|id| id.to_lowercase().contains("light"));
    if light && let Some(theme) = themes.iter().find(|t| t.as_str() == "Clay Light") {
        plan.settings.push(("theme".into(), json!(theme), format!("Theme: {theme} (a light theme, as your IDE's)")));
    }
    if option(&read("editor.xml"), "USE_SOFT_WRAPS").is_some() {
        plan.settings.push(("soft_wrap".into(), json!(true), "Soft wrap: on".into()));
    }
    let keymap = read("keymap.xml");
    let active = keymap.split("active_keymap name=\"").nth(1).and_then(|rest| rest.split('"').next()).unwrap_or("").to_lowercase();
    if active.starts_with("vscode") {
        plan.settings.push(("base_keymap".into(), json!("VSCode"), "VS Code's shortcuts as the base keymap, as in your IDE".into()));
    } else if active.starts_with("sublime") {
        plan.settings.push(("base_keymap".into(), json!("SublimeText"), "Sublime Text's shortcuts as the base keymap, as in your IDE".into()));
    }
    if source.dir.join("plugins").join("IdeaVim").is_dir() || source.dir.join("options").join("vim_settings.xml").is_file() {
        plan.settings.push(("vim_mode".into(), json!(true), "Vim mode (you use IdeaVim)".into()));
    }
    plan.notes.push("Kuana's shortcuts are JetBrains' already, unless you chose another keymap".into());
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn themes() -> Vec<String> {
        ["Ember", "One Dark", "Tokyo Night", "Rosé Pine", "Clay Light"].map(String::from).to_vec()
    }

    #[test]
    fn editors_are_found_where_they_keep_their_settings() {
        let home = temp("find");
        let root = home.join("config");
        std::fs::create_dir_all(root.join("Code/User")).unwrap();
        std::fs::write(root.join("Code/User/settings.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("Cursor/User/snippets")).unwrap();
        // No settings of its own: not offered.
        std::fs::create_dir_all(root.join("VSCodium/User")).unwrap();
        std::fs::create_dir_all(home.join(".vscode/extensions")).unwrap();
        for ide in ["PyCharm2024.3", "PyCharm2025.2", "PyCharmCE2025.1", "GoLand2025.1"] {
            std::fs::create_dir_all(root.join("JetBrains").join(ide).join("options")).unwrap();
        }
        // Not a configuration folder.
        std::fs::create_dir_all(root.join("JetBrains/consentOptions")).unwrap();
        let found = find_sources(&root, &home);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Visual Studio Code", "Cursor", "GoLand 2025.1", "PyCharm 2025.2", "PyCharm Community 2025.1"]);
        assert_eq!(found[0].extensions.as_deref(), Some(home.join(".vscode/extensions").as_path()));
        assert_eq!(found[1].extensions, None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn vscode_settings_become_kuanas() {
        let dir = temp("vscode");
        let user = dir.join("User");
        std::fs::create_dir_all(user.join("snippets")).unwrap();
        std::fs::write(
            user.join("settings.json"),
            r##"{
              // VS Code writes comments too
              "editor.fontFamily": "'Fira Code', Menlo, monospace",
              "editor.fontSize": 14,
              "editor.lineHeight": 21,
              "editor.tabSize": 2,
              "editor.insertSpaces": false,
              "editor.wordWrap": "on",
              "editor.rulers": [80, {"column": 120, "color": "#ff0000"}],
              "editor.minimap.enabled": false,
              "editor.inlayHints.enabled": "offUnlessPressed",
              "files.autoSave": "onFocusChange",
              "workbench.colorTheme": "One Dark Pro",
              "workbench.sideBar.location": "right",
              "telemetry.telemetryLevel": "off",
              "[python]": {"editor.tabSize": 4, "editor.formatOnSave": true},
              "[javascript][typescript]": {"editor.tabSize": 2},
            }"##,
        )
        .unwrap();
        std::fs::write(
            user.join("keybindings.json"),
            r#"[
              {"key": "cmd+shift+d", "command": "editor.action.copyLinesDownAction", "when": "editorTextFocus"},
              {"key": "ctrl+k ctrl+z", "command": "workbench.action.toggleZenMode"},
              {"key": "ctrl+`", "command": "workbench.action.terminal.toggleTerminal", "when": "terminalFocus"},
              {"key": "cmd+i", "command": "some.extension.command"},
              {"key": "cmd+j", "command": "-workbench.action.togglePanel"},
              {"key": "cmd+l", "command": "editor.action.formatDocument", "when": "sideBarFocus"}
            ]"#,
        )
        .unwrap();
        std::fs::write(user.join("snippets/rust.json"), "{}").unwrap();
        std::fs::write(user.join("snippets/mine.code-snippets"), "{}").unwrap();
        std::fs::write(user.join("snippets/notes.txt"), "").unwrap();
        let kuana_snippets = dir.join("kuana-snippets");
        std::fs::create_dir_all(&kuana_snippets).unwrap();
        std::fs::write(kuana_snippets.join("rust.json"), "{}").unwrap();
        let extensions = dir.join("extensions");
        std::fs::create_dir_all(extensions.join("vscodevim.vim-1.29.0")).unwrap();
        std::fs::create_dir_all(extensions.join("redhat.java-1.40.0-darwin-arm64")).unwrap();
        std::fs::create_dir_all(extensions.join("rust-lang.rust-analyzer-0.3.2")).unwrap();
        std::fs::create_dir_all(extensions.join("github.copilot-1.300.0")).unwrap();
        let source = Source { name: "Visual Studio Code".into(), kind: Kind::VsCode, dir: user, extensions: Some(extensions) };
        let plan = plan(&source, &themes(), Some(&kuana_snippets));
        let setting = |key: &str| plan.settings.iter().find(|(k, ..)| k == key).map(|(_, v, _)| v.clone());
        assert_eq!(setting("base_keymap"), Some(json!("VSCode")));
        assert_eq!(setting("editor_font_family"), Some(json!("Fira Code")));
        assert_eq!(setting("editor_font_size"), Some(json!(14.0)));
        assert_eq!(setting("editor_line_height"), Some(json!(1.5)));
        assert_eq!(setting("tab_size"), Some(json!(2)));
        assert_eq!(setting("hard_tabs"), Some(json!(true)));
        assert_eq!(setting("soft_wrap"), Some(json!(true)));
        assert_eq!(setting("rulers"), Some(json!([80, 120])));
        assert_eq!(setting("minimap"), Some(json!(false)));
        assert_eq!(setting("inlay_hints"), Some(json!(false)));
        assert_eq!(setting("auto_save"), Some(json!("on_focus_change")));
        assert_eq!(setting("theme"), Some(json!("One Dark")));
        assert_eq!(setting("sidebar_dock"), Some(json!("right")));
        assert_eq!(setting("vim_mode"), Some(json!(true)));
        assert!(plan.settings.iter().all(|(k, ..)| !k.contains("telemetry")));
        assert_eq!(
            plan.languages,
            [
                ("Python".to_string(), "tab_size".to_string(), json!(4)),
                ("Python".to_string(), "format_on_save".to_string(), json!(true)),
                ("JavaScript".to_string(), "tab_size".to_string(), json!(2)),
                ("TypeScript".to_string(), "tab_size".to_string(), json!(2)),
            ]
        );
        assert_eq!(
            plan.bindings,
            [
                Binding { keys: "cmd-shift-d".into(), action: "Duplicate Line or Selection", context: Some("Editor") },
                Binding { keys: "ctrl-k ctrl-z".into(), action: "Toggle Zen Mode", context: None },
                Binding { keys: "ctrl-`".into(), action: "Terminal", context: Some("Terminal") },
            ]
        );
        assert!(plan.notes.iter().any(|n| n.starts_with("3 shortcuts")), "{:?}", plan.notes);
        assert_eq!(plan.snippets, [source.dir.join("snippets/mine.code-snippets")]);
        assert!(plan.notes.iter().any(|n| n.starts_with("1 snippet file")), "{:?}", plan.notes);
        assert_eq!(plan.extensions, [("java", "redhat.java".to_string())]);
        assert!(plan.notes.iter().any(|n| n.contains("Copilot")));
        let preview = plan.preview();
        assert!(preview.contains(&"Font: Fira Code".to_string()), "{preview:?}");
        assert!(preview.contains(&"Settings for Python, JavaScript, TypeScript".to_string()), "{preview:?}");
        assert!(preview.contains(&"3 shortcuts of your own".to_string()), "{preview:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_and_themes_translate() {
        assert_eq!(vscode_keys("ctrl+shift+alt+k").as_deref(), Some("ctrl-shift-alt-k"));
        assert_eq!(vscode_keys("cmd+k cmd+s").as_deref(), Some("cmd-k cmd-s"));
        assert_eq!(vscode_keys("ctrl++").as_deref(), Some("ctrl-+"));
        assert_eq!(vscode_keys("meta+p").as_deref(), Some("cmd-p"));
        assert_eq!(vscode_keys("ctrl+numpad_add"), None);
        assert_eq!(matching_theme("Rosé Pine", &themes()).as_deref(), Some("Rosé Pine"));
        assert_eq!(matching_theme("tokyo night", &themes()).as_deref(), Some("Tokyo Night"));
        assert_eq!(matching_theme("Default Dark Modern", &themes()), None);
        assert_eq!(first_font("monospace"), None);
        assert_eq!(first_font("\"JetBrains Mono\", monospace").as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn jetbrains_options_become_kuanas() {
        let dir = temp("jetbrains");
        let options = dir.join("options");
        std::fs::create_dir_all(&options).unwrap();
        std::fs::create_dir_all(dir.join("plugins/IdeaVim")).unwrap();
        std::fs::write(
            options.join("editor-font.xml"),
            r#"<application><component name="DefaultFont"><option name="VERSION" value="1" /><option name="FONT_SIZE" value="15" /><option name="FONT_FAMILY" value="JetBrains Mono" /></component></application>"#,
        )
        .unwrap();
        std::fs::write(options.join("laf.xml"), r#"<application><component name="LafManager"><laf themeId="JetBrainsLightTheme" /></component></application>"#).unwrap();
        std::fs::write(options.join("editor.xml"), r#"<application><component name="EditorSettings"><option name="USE_SOFT_WRAPS" value="MAIN_EDITOR" /></component></application>"#).unwrap();
        std::fs::write(options.join("keymap.xml"), r#"<application><component name="KeymapManager"><active_keymap name="VSCode (macOS)" /></component></application>"#).unwrap();
        let source = Source { name: "PyCharm 2025.2".into(), kind: Kind::JetBrains, dir: dir.clone(), extensions: None };
        let plan = plan(&source, &themes(), None);
        let keys: Vec<(&str, Value)> = plan.settings.iter().map(|(k, v, _)| (k.as_str(), v.clone())).collect();
        assert_eq!(
            keys,
            [
                ("editor_font_family", json!("JetBrains Mono")),
                ("editor_font_size", json!(15.0)),
                ("theme", json!("Clay Light")),
                ("soft_wrap", json!(true)),
                ("base_keymap", json!("VSCode")),
                ("vim_mode", json!(true)),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
