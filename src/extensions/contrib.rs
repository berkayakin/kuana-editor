//! What an extension adds to Kuana's UI: the `[kuana]` part of its
//! `extension.toml` (Zed ignores it, so the extension still loads there).
//!
//! ```toml
//! [kuana]
//! api = "0.1"                      # the kuana:extension interface its kuana.wasm targets
//! permissions = ["process"]        # network, process, workspace-files
//!
//! [kuana.views.containers]
//! title = "Containers"
//! icon = "database"                # one of Kuana's icons, or an SVG in the extension
//! when = "file:compose.yaml"       # always (default), file:<name>, language:<name>
//!
//! [kuana.commands.refresh]
//! title = "Refresh"
//! icon = "refresh-cw"
//!
//! [[kuana.menus]]
//! place = "view-toolbar:containers" # view-toolbar:<view>, view-item:<view>, editor
//! command = "refresh"
//! when = "kind == 'running' || kind == 'stopped'"  # view-item: kind; editor: language
//!
//! [kuana.panels.logs]
//! title = "Container Logs"
//! ```
//!
//! With `api = "0.2"` (`kuana-wit/v0.2`), also `events = ["file-saved", …]`
//! (what the code's `on-event` hears about), the `workspace-write` permission
//! and `[kuana.status.<id>]` (a status bar item the code fills). For any
//! version: `key` on a command (a default shortcut), the `file-tree` and
//! `editor-tab` menu places, and `[kuana.settings.<key>]` (the extension's
//! settings, with their defaults).
//!
//! The code that answers for views and commands is `kuana.wasm`
//! (`ui_host`), beside any Zed `extension.wasm`.

use std::path::{Path, PathBuf};

use gpui::SharedString;
use toml::Value;

/// The interface versions Kuana hosts.
pub const API_VERSIONS: &[&str] = &["0.1", "0.2"];

/// The interface an extension's `kuana.wasm` targets (`[kuana] api`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Api {
    #[default]
    V1,
    V2,
}

/// What `events` may list (0.2). A timer and a save of the extension's own
/// file are told without being listed.
pub const EVENTS: &[&str] =
    &["project-opened", "project-closed", "file-opened", "file-closed", "file-saved", "editor-changed", "settings-changed", "process-output"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Permission {
    Network,
    Process,
    WorkspaceFiles,
    WorkspaceWrite,
}

const PERMISSIONS: [Permission; 4] = [Permission::Network, Permission::Process, Permission::WorkspaceFiles, Permission::WorkspaceWrite];

impl Permission {
    pub fn key(self) -> &'static str {
        match self {
            Permission::Network => "network",
            Permission::Process => "process",
            Permission::WorkspaceFiles => "workspace-files",
            Permission::WorkspaceWrite => "workspace-write",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        PERMISSIONS.into_iter().find(|p| p.key() == key)
    }

    /// Whether `[kuana.allow]` can narrow it to a list (of programs, of hosts).
    pub fn takes_list(self) -> bool {
        matches!(self, Permission::Process | Permission::Network)
    }

    /// What it lets the extension do, as the permission prompt says it;
    /// `scope` is its `[kuana.allow]` list (None: anything).
    pub fn describe(self, scope: Option<&[String]>) -> String {
        match (self, scope) {
            (Permission::Network, None) => "connect to any server on the internet and your network".into(),
            (Permission::Network, Some(hosts)) => format!("connect to {}", and_list(hosts)),
            (Permission::Process, None) => "run any program on this computer".into(),
            (Permission::Process, Some(programs)) => format!("run {} on this computer", and_list(programs)),
            (Permission::WorkspaceFiles, _) => "read the files of the open project".into(),
            (Permission::WorkspaceWrite, _) => "change the files of the open project".into(),
        }
    }

    /// Its scope in a few words (Settings, `kuana extension check`).
    pub fn scope_words(self, scope: Option<&[String]>) -> Option<String> {
        match (self, scope) {
            (_, Some(list)) => Some(list.join(", ")),
            (Permission::Process, None) => Some("any program".into()),
            (Permission::Network, None) => Some("any host".into()),
            _ => None,
        }
    }
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[String]) -> String {
    match items {
        [] => "nothing".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A program name `[kuana.allow] process` may list: a bare name, found on
/// PATH (a path would let the code start a copy it wrote).
pub fn valid_program(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')) && !name.starts_with('.')
}

/// A host `[kuana.allow] network` may list: a name or address, or `*.` and
/// a name for every name under it.
pub fn valid_host(host: &str) -> bool {
    let name = host.strip_prefix("*.").unwrap_or(host);
    !name.is_empty()
        && name.split('.').all(|label| !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && (name.contains('.') || !host.starts_with("*."))
}

/// When a view shows on the activity bar.
#[derive(Clone, Debug, PartialEq)]
pub enum When {
    Always,
    /// A file or folder with this name in the project's folder.
    File(String),
    /// An open file in this language (by name, as the status bar shows it).
    Language(String),
}

/// A menu entry's condition.
#[derive(Clone, Debug, PartialEq)]
pub enum Condition {
    /// The tree item's `kind`.
    Kind(String),
    /// The file's language (editor and tab menus).
    Language(String),
    /// The file's whole name.
    Name(String),
    /// Its extension, without the dot.
    Extension(String),
    /// A folder (the file tree's menu), or a file.
    Folder(bool),
}

/// What a menu on a file is opened for.
pub struct FileSubject<'a> {
    pub path: &'a Path,
    pub folder: bool,
    /// Its language's name, when it's known (an open file's).
    pub language: Option<&'a str>,
}

impl Condition {
    /// Whether it holds for a file (never, for a view item's `kind`).
    pub fn holds_for(&self, file: &FileSubject) -> bool {
        let same = |part: Option<&std::ffi::OsStr>, wanted: &str| part.and_then(|p| p.to_str()).is_some_and(|p| p.eq_ignore_ascii_case(wanted));
        match self {
            Condition::Kind(_) => false,
            Condition::Language(wanted) => file.language.is_some_and(|l| l.eq_ignore_ascii_case(wanted)),
            Condition::Name(wanted) => same(file.path.file_name(), wanted),
            Condition::Extension(wanted) => !file.folder && same(file.path.extension(), wanted),
            Condition::Folder(folder) => file.folder == *folder,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MenuPlace {
    /// A button in a view's header.
    ViewToolbar(String),
    /// A tree item's right-click menu.
    ViewItem(String),
    /// The editor's right-click menu.
    Editor,
    /// The file tree's right-click menu.
    FileTree,
    /// A tab's right-click menu.
    EditorTab,
}

#[derive(Clone, Debug)]
pub struct ViewDef {
    pub id: String,
    pub title: String,
    /// An asset path for `svg()`: Kuana's icon or the extension's file.
    pub icon: SharedString,
    pub when: When,
}

#[derive(Clone, Debug)]
pub struct CommandDef {
    pub id: String,
    pub title: String,
    pub icon: Option<SharedString>,
    /// Its default shortcut, as `keymap.rs` writes them (`secondary-alt-k`).
    pub key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MenuDef {
    pub place: MenuPlace,
    pub command: String,
    /// Shown when any of these holds (always, when empty).
    pub when: Vec<Condition>,
}

#[derive(Clone, Debug)]
pub struct PanelDef {
    pub id: String,
    pub title: String,
}

/// A status bar item: the code says what it shows (`set-status`).
#[derive(Clone, Debug)]
pub struct StatusDef {
    pub id: String,
    /// Run when it's clicked.
    pub command: Option<String>,
    pub when: When,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingKind {
    Text,
    Boolean,
    Number,
    /// One of these.
    Choice(Vec<String>),
}

/// One of the extension's settings (`extensions.<id>.<key>` in settings.json).
#[derive(Clone, Debug)]
pub struct SettingDef {
    pub key: String,
    pub title: String,
    pub description: Option<String>,
    pub kind: SettingKind,
    pub default: Option<serde_json::Value>,
}

/// One extension's contributions.
#[derive(Clone, Debug, Default)]
pub struct Contributions {
    pub extension: String,
    /// The extension's name (Find Action lists commands as "<name>: <title>").
    pub name: String,
    pub dir: PathBuf,
    /// Its `kuana.wasm`, if it has one.
    pub code: Option<PathBuf>,
    pub api: Api,
    pub permissions: Vec<Permission>,
    /// `[kuana.allow]`: permissions narrowed to a list (programs for
    /// `process`, hosts for `network`). One that isn't here is unrestricted.
    pub allow: Vec<(Permission, Vec<String>)>,
    /// The events its code hears about (0.2), from `EVENTS`.
    pub events: Vec<&'static str>,
    pub views: Vec<ViewDef>,
    pub commands: Vec<CommandDef>,
    pub menus: Vec<MenuDef>,
    pub panels: Vec<PanelDef>,
    pub status: Vec<StatusDef>,
    pub settings: Vec<SettingDef>,
}

impl Contributions {
    /// What `permission` is narrowed to (None: anything it covers).
    pub fn scope(&self, permission: Permission) -> Option<&[String]> {
        self.allow.iter().find(|(p, _)| *p == permission).map(|(_, list)| list.as_slice())
    }

    /// Whether its code is told about `event` (see `EVENTS`).
    pub fn hears(&self, event: &str) -> bool {
        self.code.is_some() && self.events.contains(&event)
    }

    /// The extension's settings as its code gets them: what the user set
    /// (`user`, the `extensions.<id>` object as JSON, or nothing), with the
    /// manifest's defaults for the keys that aren't.
    pub fn settings_json(&self, user: Option<&serde_json::Value>) -> String {
        let defaults = || self.settings.iter().filter_map(|s| Some((s.key.clone(), s.default.clone()?)));
        match user {
            None if self.settings.iter().all(|s| s.default.is_none()) => String::new(),
            Some(value) if !value.is_object() || defaults().next().is_none() => value.to_string(),
            user => {
                let mut map: serde_json::Map<String, serde_json::Value> = defaults().collect();
                for (key, value) in user.and_then(|v| v.as_object()).into_iter().flatten() {
                    map.insert(key.clone(), value.clone());
                }
                serde_json::Value::Object(map).to_string()
            }
        }
    }

    pub fn command(&self, id: &str) -> Option<&CommandDef> {
        self.commands.iter().find(|c| c.id == id)
    }

    pub fn panel(&self, id: &str) -> Option<&PanelDef> {
        self.panels.iter().find(|p| p.id == id)
    }

    /// The menu entries at `place` whose condition holds.
    pub fn menu<'a>(&'a self, place: &'a MenuPlace, holds: impl Fn(&Condition) -> bool + 'a) -> impl Iterator<Item = &'a CommandDef> + 'a {
        self.menus
            .iter()
            .filter(move |m| m.place == *place && (m.when.is_empty() || m.when.iter().any(&holds)))
            .filter_map(|m| self.command(&m.command))
    }
}

/// The asset path `svg()` loads an extension's own icon file from (see
/// `assets::Assets::load`, which reads it only inside the extensions folder).
pub const FILE_ICON_PREFIX: &str = "extension-file:";

/// An icon value: one of Kuana's icon names, else an SVG file in `dir`.
fn icon(value: &str, dir: &Path, problems: &mut Vec<String>, what: &str) -> Option<SharedString> {
    if crate::assets::has_icon(value) {
        return Some(format!("icons/{value}.svg").into());
    }
    let Ok(path) = crate::extensions::wasm::inside(dir, value) else {
        problems.push(format!("{what}: icon {value} is outside the extension"));
        return None;
    };
    if path.extension().is_none_or(|e| e != "svg") || !path.is_file() {
        problems.push(format!("{what}: icon {value} is neither one of Kuana's icons nor an SVG file in the extension"));
        return None;
    }
    Some(format!("{FILE_ICON_PREFIX}{}", path.display()).into())
}

/// `kind == 'container'` / `language == 'Go'` / `name == 'Makefile'` /
/// `extension == 'json'` / `type == 'folder'`.
fn condition(text: &str) -> Option<Condition> {
    let (key, value) = text.split_once("==")?;
    let value = value.trim().trim_matches(['\'', '"']).to_string();
    match key.trim() {
        "kind" => Some(Condition::Kind(value)),
        "language" => Some(Condition::Language(value)),
        "name" => Some(Condition::Name(value)),
        "extension" => Some(Condition::Extension(value.trim_start_matches('.').to_string())),
        "type" => match value.as_str() {
            "folder" => Some(Condition::Folder(true)),
            "file" => Some(Condition::Folder(false)),
            _ => None,
        },
        _ => None,
    }
}

/// `when` of a view or a status item: always, `file:<name>`, `language:<name>`.
fn when(text: Option<&str>, what: &str, problems: &mut Vec<String>) -> When {
    match text {
        None | Some("always") => When::Always,
        Some(text) => match text.split_once(':') {
            Some(("file", name)) if !name.is_empty() => When::File(name.to_string()),
            Some(("language", name)) if !name.is_empty() => When::Language(name.to_string()),
            _ => {
                problems.push(format!("{what}: when = \"{text}\" isn't always, file:<name> or language:<name>"));
                When::Always
            }
        },
    }
}

/// A TOML value as the JSON settings.json would hold.
fn json_of(value: &Value) -> Option<serde_json::Value> {
    match value {
        Value::String(s) => Some(s.as_str().into()),
        Value::Boolean(b) => Some((*b).into()),
        Value::Integer(n) => Some((*n).into()),
        Value::Float(n) => serde_json::Number::from_f64(*n).map(serde_json::Value::Number),
        _ => None,
    }
}

/// A shortcut as `keymap.rs` writes them: parts joined by `-`, modifiers
/// first, then one key. Kuana checks what it's bound over when it installs it.
fn valid_key(key: &str) -> bool {
    let mut parts: Vec<&str> = key.split('-').collect();
    let Some(last) = parts.pop().filter(|k| !k.is_empty()) else { return false };
    let modifier = |p: &&str| matches!(*p, "secondary" | "cmd" | "ctrl" | "alt" | "shift" | "fn");
    last.chars().all(|c| c.is_ascii_graphic()) && !modifier(&last) && parts.iter().all(modifier) && parts.iter().any(|p| *p != "shift")
}

/// An id usable in commands, views and panels: letters, digits, `-`, `_`, `.`.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Reads `[kuana]` (None when the manifest has none), with its problems.
pub fn parse(manifest: &toml::Table, extension: &str, name: &str, dir: &Path) -> Option<(Contributions, Vec<String>)> {
    let table = manifest.get("kuana")?.as_table();
    let mut problems = Vec::new();
    let Some(table) = table else {
        return Some((Contributions::default(), vec!["[kuana] must be a table".into()]));
    };
    let mut out = Contributions { extension: extension.to_string(), name: name.to_string(), dir: dir.to_path_buf(), ..Default::default() };
    let str_of = |t: &toml::Table, key: &str| t.get(key).and_then(Value::as_str).map(str::to_string);

    let api = str_of(table, "api").unwrap_or_else(|| "0.1".into());
    if !API_VERSIONS.contains(&api.as_str()) {
        problems.push(format!("[kuana] api {api}: this Kuana hosts {}", API_VERSIONS.join(", ")));
    }
    out.api = if api == "0.2" { Api::V2 } else { Api::V1 };
    let v2 = out.api >= Api::V2;
    let needs_v2 = |what: &str, problems: &mut Vec<String>| {
        if !v2 {
            problems.push(format!("{what} needs api = \"0.2\" in [kuana]"));
        }
    };
    let code = dir.join("kuana.wasm");
    out.code = code.is_file().then_some(code);

    for value in table.get("permissions").and_then(Value::as_array).into_iter().flatten() {
        match value.as_str().and_then(Permission::from_key) {
            Some(p) if !out.permissions.contains(&p) => out.permissions.push(p),
            Some(_) => {}
            None => problems.push(format!("[kuana] permissions: {value} isn't one of network, process, workspace-files, workspace-write")),
        }
    }
    // `[kuana.allow]` narrows a permission to a list; listing one declares it.
    match table.get("allow") {
        None => {}
        Some(Value::Table(allow)) => {
            for (key, value) in allow {
                let Some(permission) = Permission::from_key(key).filter(|p| p.takes_list()) else {
                    problems.push(format!("[kuana.allow] {key}: only process and network take a list"));
                    continue;
                };
                let Some(items) = value.as_array() else {
                    problems.push(format!("[kuana.allow] {key} must be a list of strings"));
                    continue;
                };
                let (what, valid): (&str, fn(&str) -> bool) = match permission {
                    Permission::Process => ("a program name (no path)", valid_program),
                    _ => ("a host name, or *. and one", valid_host),
                };
                let mut list = Vec::new();
                for item in items {
                    match item.as_str().map(|s| if permission == Permission::Network { s.to_ascii_lowercase() } else { s.to_string() }) {
                        Some(name) if valid(&name) => {
                            if !list.contains(&name) {
                                list.push(name);
                            }
                        }
                        _ => problems.push(format!("[kuana.allow] {key}: {item} isn't {what}")),
                    }
                }
                if list.is_empty() {
                    problems.push(format!("[kuana.allow] {key} allows nothing; list what the code needs or leave the key out"));
                }
                if !out.permissions.contains(&permission) {
                    out.permissions.push(permission);
                }
                out.allow.push((permission, list));
            }
        }
        Some(_) => problems.push("[kuana.allow] must be a table".into()),
    }
    if out.permissions.contains(&Permission::WorkspaceWrite) {
        needs_v2("the workspace-write permission", &mut problems);
    }

    for value in table.get("events").and_then(Value::as_array).into_iter().flatten() {
        match value.as_str().and_then(|name| EVENTS.iter().find(|e| **e == name)) {
            Some(event) if !out.events.contains(event) => out.events.push(event),
            Some(_) => {}
            None => problems.push(format!("[kuana] events: {value} isn't one of {}", EVENTS.join(", "))),
        }
    }
    if !out.events.is_empty() {
        needs_v2("[kuana] events", &mut problems);
    }

    for (id, view) in table.get("views").and_then(Value::as_table).into_iter().flatten() {
        let what = format!("view {id}");
        let Some(view) = view.as_table().filter(|_| valid_id(id)) else {
            problems.push(format!("{what}: needs a table and an id of letters, digits, -, _ or ."));
            continue;
        };
        let title = str_of(view, "title").unwrap_or_else(|| id.clone());
        let icon = match str_of(view, "icon") {
            Some(value) => icon(&value, dir, &mut problems, &what).unwrap_or_else(|| "icons/puzzle.svg".into()),
            None => "icons/puzzle.svg".into(),
        };
        let when = when(str_of(view, "when").as_deref(), &what, &mut problems);
        out.views.push(ViewDef { id: id.clone(), title, icon, when });
    }

    for (id, command) in table.get("commands").and_then(Value::as_table).into_iter().flatten() {
        let what = format!("command {id}");
        let Some(command) = command.as_table().filter(|_| valid_id(id)) else {
            problems.push(format!("{what}: needs a table and an id of letters, digits, -, _ or ."));
            continue;
        };
        let title = str_of(command, "title").unwrap_or_else(|| id.clone());
        let icon = str_of(command, "icon").and_then(|value| icon(&value, dir, &mut problems, &what));
        let key = str_of(command, "key").filter(|key| {
            let valid = valid_key(key);
            if !valid {
                problems.push(format!("{what}: key = \"{key}\" isn't a shortcut like secondary-alt-k (modifiers, then a key)"));
            }
            valid
        });
        out.commands.push(CommandDef { id: id.clone(), title, icon, key });
    }

    for (id, panel) in table.get("panels").and_then(Value::as_table).into_iter().flatten() {
        let title = panel.as_table().and_then(|p| str_of(p, "title")).unwrap_or_else(|| id.clone());
        if !valid_id(id) {
            problems.push(format!("panel {id}: needs an id of letters, digits, -, _ or ."));
            continue;
        }
        out.panels.push(PanelDef { id: id.clone(), title });
    }

    for (ix, menu) in table.get("menus").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let what = format!("menu {}", ix + 1);
        let Some(menu) = menu.as_table() else {
            problems.push(format!("{what}: [[kuana.menus]] entries are tables"));
            continue;
        };
        let place = match str_of(menu, "place").as_deref() {
            Some("editor") => MenuPlace::Editor,
            Some("file-tree") => MenuPlace::FileTree,
            Some("editor-tab") => MenuPlace::EditorTab,
            Some(text) => match text.split_once(':') {
                Some(("view-toolbar", view)) => MenuPlace::ViewToolbar(view.to_string()),
                Some(("view-item", view)) => MenuPlace::ViewItem(view.to_string()),
                _ => {
                    problems.push(format!("{what}: place = \"{text}\" isn't view-toolbar:<view>, view-item:<view>, editor, editor-tab or file-tree"));
                    continue;
                }
            },
            None => {
                problems.push(format!("{what}: it needs a place"));
                continue;
            }
        };
        if let MenuPlace::ViewToolbar(view) | MenuPlace::ViewItem(view) = &place
            && !out.views.iter().any(|v| v.id == *view)
        {
            problems.push(format!("{what}: there's no view {view}"));
        }
        let Some(command) = str_of(menu, "command") else {
            problems.push(format!("{what}: it needs a command"));
            continue;
        };
        if out.command(&command).is_none() {
            problems.push(format!("{what}: there's no command {command}"));
        }
        let mut when = Vec::new();
        if let Some(text) = str_of(menu, "when") {
            for part in text.split("||") {
                match condition(part) {
                    Some(c) => when.push(c),
                    None => problems.push(format!(
                        "{what}: when = \"{text}\" isn't kind, language, name or extension == '…', or type == 'file' / 'folder' (alternatives joined by ||)"
                    )),
                }
            }
        }
        out.menus.push(MenuDef { place, command, when });
    }

    for (id, item) in table.get("status").and_then(Value::as_table).into_iter().flatten() {
        let what = format!("status item {id}");
        let Some(item) = item.as_table().filter(|_| valid_id(id)) else {
            problems.push(format!("{what}: needs a table and an id of letters, digits, -, _ or ."));
            continue;
        };
        let command = str_of(item, "command");
        if let Some(command) = command.as_ref().filter(|c| out.command(c).is_none()) {
            problems.push(format!("{what}: there's no command {command}"));
        }
        let when = when(str_of(item, "when").as_deref(), &what, &mut problems);
        out.status.push(StatusDef { id: id.clone(), command, when });
    }
    if !out.status.is_empty() {
        needs_v2("[kuana.status]", &mut problems);
    }

    for (key, setting) in table.get("settings").and_then(Value::as_table).into_iter().flatten() {
        let what = format!("setting {key}");
        let Some(setting) = setting.as_table().filter(|_| valid_id(key)) else {
            problems.push(format!("{what}: needs a table and a key of letters, digits, -, _ or ."));
            continue;
        };
        let values = || setting.get("values").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>();
        let kind = match str_of(setting, "type").as_deref() {
            None | Some("string") => SettingKind::Text,
            Some("boolean") => SettingKind::Boolean,
            Some("number") => SettingKind::Number,
            Some("enum") if !values().is_empty() => SettingKind::Choice(values()),
            Some("enum") => {
                problems.push(format!("{what}: type = \"enum\" needs values = [\"…\", …]"));
                continue;
            }
            Some(other) => {
                problems.push(format!("{what}: type = \"{other}\" isn't string, boolean, number or enum"));
                continue;
            }
        };
        let default = setting.get("default").and_then(json_of);
        let fits = match (&kind, &default) {
            (_, None) => setting.get("default").is_none(),
            (SettingKind::Text, Some(v)) => v.is_string(),
            (SettingKind::Boolean, Some(v)) => v.is_boolean(),
            (SettingKind::Number, Some(v)) => v.is_number(),
            (SettingKind::Choice(values), Some(v)) => v.as_str().is_some_and(|v| values.iter().any(|x| x == v)),
        };
        if !fits {
            problems.push(format!("{what}: its default doesn't fit its type"));
        }
        out.settings.push(SettingDef {
            key: key.clone(),
            title: str_of(setting, "title").unwrap_or_else(|| key.clone()),
            description: str_of(setting, "description"),
            kind,
            default: default.filter(|_| fits),
        });
    }

    if out.code.is_none() && !out.events.is_empty() {
        problems.push("[kuana]: events need the extension's code (kuana.wasm)".into());
    }
    if out.code.is_none() && (!out.views.is_empty() || !out.commands.is_empty()) {
        problems.push("[kuana]: views and commands need the extension's code (kuana.wasm)".into());
    }
    Some((out, problems))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> (Contributions, Vec<String>) {
        // A folder per call: tests run side by side, and each removes its own.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kuana-contrib-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kuana.wasm"), b"").unwrap();
        std::fs::write(dir.join("logo.svg"), b"<svg/>").unwrap();
        let manifest: toml::Table = text.parse().unwrap();
        let out = parse(&manifest, "demo", "Demo", &dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        out
    }

    #[test]
    fn reads_views_commands_menus_and_panels() {
        let (c, problems) = parsed(
            r#"
            [kuana]
            permissions = ["process", "network"]
            [kuana.views.boxes]
            title = "Boxes"
            icon = "logo.svg"
            when = "file:compose.yaml"
            [kuana.commands.refresh]
            title = "Refresh"
            icon = "refresh-cw"
            [kuana.commands.open]
            title = "Open"
            [[kuana.menus]]
            place = "view-toolbar:boxes"
            command = "refresh"
            [[kuana.menus]]
            place = "view-item:boxes"
            command = "open"
            when = "kind == 'box' || kind == 'crate'"
            [kuana.panels.logs]
            title = "Logs"
            "#,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(c.permissions, [Permission::Process, Permission::Network]);
        assert_eq!(c.views[0].when, When::File("compose.yaml".into()));
        assert!(c.views[0].icon.starts_with(FILE_ICON_PREFIX));
        assert_eq!(c.commands.iter().find(|c| c.id == "refresh").unwrap().icon.as_ref().map(|i| i.as_ref()), Some("icons/refresh-cw.svg"));
        let item = MenuPlace::ViewItem("boxes".into());
        let box_kind = |c: &Condition| *c == Condition::Kind("box".into());
        assert_eq!(c.menu(&item, box_kind).map(|c| c.id.as_str()).collect::<Vec<_>>(), ["open"]);
        assert_eq!(c.menu(&item, |c| *c == Condition::Kind("crate".into())).count(), 1);
        assert_eq!(c.menu(&item, |_| false).count(), 0);
        assert_eq!(c.panel("logs").unwrap().title, "Logs");
    }

    #[test]
    fn narrows_process_and_network_to_lists() {
        let (c, problems) = parsed(
            r#"
            [kuana]
            api = "0.2"
            permissions = ["process", "workspace-files"]
            [kuana.allow]
            process = ["docker", "git", "docker"]
            network = ["API.GitHub.com", "*.example.com"]
            "#,
        );
        assert!(problems.is_empty(), "{problems:?}");
        // Listing network declares it.
        assert_eq!(c.permissions, [Permission::Process, Permission::WorkspaceFiles, Permission::Network]);
        assert_eq!(c.scope(Permission::Process), Some(&["docker".to_string(), "git".to_string()][..]));
        assert_eq!(c.scope(Permission::Network), Some(&["api.github.com".to_string(), "*.example.com".to_string()][..]));
        assert_eq!(c.scope(Permission::WorkspaceFiles), None);
        assert_eq!(Permission::Process.describe(c.scope(Permission::Process)), "run docker and git on this computer");
        assert_eq!(Permission::Process.describe(None), "run any program on this computer");
        assert_eq!(Permission::Network.describe(c.scope(Permission::Network)), "connect to api.github.com and *.example.com");
        assert_eq!(Permission::Network.scope_words(None).as_deref(), Some("any host"));
        assert_eq!(Permission::WorkspaceFiles.scope_words(None), None);

        // A bare permission stays unrestricted, as before lists.
        let (c, _) = parsed("[kuana]\npermissions = [\"process\", \"network\"]");
        assert!(c.allow.is_empty() && c.scope(Permission::Process).is_none());

        let (_, problems) = parsed(
            r#"
            [kuana]
            [kuana.allow]
            process = ["/usr/bin/docker", "sh -c"]
            network = ["*.com", "http://x.org", "ok.org"]
            workspace-files = ["src"]
            "#,
        );
        let expected = [
            "/usr/bin/docker\" isn't a program",
            "sh -c\" isn't a program",
            "process allows nothing",
            "\"*.com\" isn't a host",
            "\"http://x.org\" isn't a host",
            "workspace-files: only process and network take a list",
        ];
        for text in expected {
            assert!(problems.iter().any(|p| p.contains(text)), "{text} not in {problems:?}");
        }
        assert_eq!(problems.len(), expected.len(), "{problems:?}");
    }

    #[test]
    fn reports_mistakes() {
        let (_, problems) = parsed(
            r#"
            [kuana]
            api = "9.0"
            permissions = ["root"]
            [kuana.views.v]
            when = "sometimes"
            icon = "../../etc/x.svg"
            [[kuana.menus]]
            place = "view-item:nope"
            command = "missing"
            when = "color == 'red'"
            "#,
        );
        let text = problems.join("\n");
        for expected in ["api 9.0", "root", "sometimes", "outside the extension", "no view nope", "no command missing", "color == 'red'"] {
            assert!(text.contains(expected), "{expected} in {text}");
        }
    }

    #[test]
    fn reads_what_0_2_adds() {
        let (c, problems) = parsed(
            r#"
            [kuana]
            api = "0.2"
            permissions = ["workspace-files", "workspace-write"]
            events = ["file-saved", "editor-changed", "file-saved"]
            [kuana.commands.sort]
            title = "Sort Keys"
            key = "secondary-alt-k"
            [[kuana.menus]]
            place = "file-tree"
            command = "sort"
            when = "extension == '.json' || name == 'Makefile' || type == 'folder'"
            [[kuana.menus]]
            place = "editor-tab"
            command = "sort"
            when = "language == 'JSON'"
            [kuana.status.count]
            command = "sort"
            when = "language:JSON"
            [kuana.settings.folder]
            title = "Notes folder"
            default = "~/Notes"
            [kuana.settings.order]
            type = "enum"
            values = ["name", "date"]
            default = "name"
            [kuana.settings.limit]
            type = "number"
            "#,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(c.api, Api::V2);
        assert_eq!(c.events, ["file-saved", "editor-changed"]);
        assert!(c.hears("editor-changed") && !c.hears("file-opened"));
        assert_eq!(c.commands[0].key.as_deref(), Some("secondary-alt-k"));
        assert_eq!(c.status[0].when, When::Language("JSON".into()));
        assert_eq!(c.settings.iter().find(|s| s.key == "order").unwrap().kind, SettingKind::Choice(vec!["name".into(), "date".into()]));

        let file = |path: &'static str, folder, language| FileSubject { path: Path::new(path), folder, language };
        let shown = |place: MenuPlace, subject: FileSubject| c.menu(&place, |cond| cond.holds_for(&subject)).count();
        assert_eq!(shown(MenuPlace::FileTree, file("/p/a.JSON", false, None)), 1);
        assert_eq!(shown(MenuPlace::FileTree, file("/p/makefile", false, None)), 1);
        assert_eq!(shown(MenuPlace::FileTree, file("/p/src", true, None)), 1);
        assert_eq!(shown(MenuPlace::FileTree, file("/p/a.rs", false, None)), 0);
        assert_eq!(shown(MenuPlace::EditorTab, file("/p/a.json", false, Some("JSON"))), 1);
        assert_eq!(shown(MenuPlace::EditorTab, file("/p/a.json", false, Some("Rust"))), 0);

        // What the user set wins over the defaults; unset keys get theirs.
        let json = |text: &str| -> serde_json::Value { serde_json::from_str(text).unwrap() };
        assert_eq!(json(&c.settings_json(None)), json(r#"{"folder": "~/Notes", "order": "name"}"#));
        assert_eq!(json(&c.settings_json(Some(&json(r#"{"order": "date", "x": 1}"#)))), json(r#"{"folder": "~/Notes", "order": "date", "x": 1}"#));
    }

    #[test]
    fn what_0_2_adds_needs_it_declared() {
        let (_, problems) = parsed(
            r#"
            [kuana]
            permissions = ["workspace-write"]
            events = ["file-saved", "key-pressed"]
            [kuana.commands.c]
            key = "k"
            [kuana.status.s]
            command = "nope"
            [kuana.settings.a]
            type = "enum"
            [kuana.settings.b]
            type = "boolean"
            default = "yes"
            "#,
        );
        let text = problems.join("\n");
        for expected in [
            "workspace-write permission needs api",
            "events needs api",
            "key-pressed",
            "key = \"k\"",
            "[kuana.status] needs api",
            "no command nope",
            "needs values",
            "default doesn't fit",
        ] {
            assert!(text.contains(expected), "{expected} in {text}");
        }
    }

    #[test]
    fn no_kuana_table_is_none() {
        let manifest: toml::Table = "id = \"x\"".parse().unwrap();
        assert!(parse(&manifest, "x", "X", Path::new("/tmp")).is_none());
    }
}
