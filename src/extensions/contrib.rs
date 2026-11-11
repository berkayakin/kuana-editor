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
//! The code that answers for views and commands is `kuana.wasm`
//! (`ui_host`), beside any Zed `extension.wasm`.

use std::path::{Path, PathBuf};

use gpui::SharedString;
use toml::Value;

/// The interface versions Kuana hosts.
pub const API_VERSIONS: &[&str] = &["0.1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Permission {
    Network,
    Process,
    WorkspaceFiles,
}

impl Permission {
    pub fn key(self) -> &'static str {
        match self {
            Permission::Network => "network",
            Permission::Process => "process",
            Permission::WorkspaceFiles => "workspace-files",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        [Permission::Network, Permission::Process, Permission::WorkspaceFiles].into_iter().find(|p| p.key() == key)
    }

    /// What it lets the extension do, as the permission prompt says it.
    pub fn describe(self) -> &'static str {
        match self {
            Permission::Network => "connect to the internet and your network",
            Permission::Process => "run programs on this computer",
            Permission::WorkspaceFiles => "read the files of the open project",
        }
    }
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
    /// The file's language (editor menus).
    Language(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum MenuPlace {
    /// A button in a view's header.
    ViewToolbar(String),
    /// A tree item's right-click menu.
    ViewItem(String),
    /// The editor's right-click menu.
    Editor,
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

/// One extension's contributions.
#[derive(Clone, Debug, Default)]
pub struct Contributions {
    pub extension: String,
    /// The extension's name (Find Action lists commands as "<name>: <title>").
    pub name: String,
    pub dir: PathBuf,
    /// Its `kuana.wasm`, if it has one.
    pub code: Option<PathBuf>,
    pub permissions: Vec<Permission>,
    pub views: Vec<ViewDef>,
    pub commands: Vec<CommandDef>,
    pub menus: Vec<MenuDef>,
    pub panels: Vec<PanelDef>,
}

impl Contributions {
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

/// `kind == 'container'` / `language == 'Go'`.
fn condition(text: &str) -> Option<Condition> {
    let (key, value) = text.split_once("==")?;
    let value = value.trim().trim_matches(['\'', '"']).to_string();
    match key.trim() {
        "kind" => Some(Condition::Kind(value)),
        "language" => Some(Condition::Language(value)),
        _ => None,
    }
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
    let code = dir.join("kuana.wasm");
    out.code = code.is_file().then_some(code);

    for value in table.get("permissions").and_then(Value::as_array).into_iter().flatten() {
        match value.as_str().and_then(Permission::from_key) {
            Some(p) if !out.permissions.contains(&p) => out.permissions.push(p),
            Some(_) => {}
            None => problems.push(format!("[kuana] permissions: {value} isn't one of network, process, workspace-files")),
        }
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
        let when = match str_of(view, "when").as_deref() {
            None | Some("always") => When::Always,
            Some(text) => match text.split_once(':') {
                Some(("file", name)) if !name.is_empty() => When::File(name.to_string()),
                Some(("language", name)) if !name.is_empty() => When::Language(name.to_string()),
                _ => {
                    problems.push(format!("{what}: when = \"{text}\" isn't always, file:<name> or language:<name>"));
                    When::Always
                }
            },
        };
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
        out.commands.push(CommandDef { id: id.clone(), title, icon });
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
            Some(text) => match text.split_once(':') {
                Some(("view-toolbar", view)) => MenuPlace::ViewToolbar(view.to_string()),
                Some(("view-item", view)) => MenuPlace::ViewItem(view.to_string()),
                _ => {
                    problems.push(format!("{what}: place = \"{text}\" isn't view-toolbar:<view>, view-item:<view> or editor"));
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
                    None => problems.push(format!("{what}: when = \"{text}\" isn't kind == '…' or language == '…' (alternatives joined by ||)")),
                }
            }
        }
        out.menus.push(MenuDef { place, command, when });
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
        let dir = std::env::temp_dir().join(format!("kuana-contrib-{}", std::process::id()));
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
    fn no_kuana_table_is_none() {
        let manifest: toml::Table = "id = \"x\"".parse().unwrap();
        assert!(parse(&manifest, "x", "X", Path::new("/tmp")).is_none());
    }
}
