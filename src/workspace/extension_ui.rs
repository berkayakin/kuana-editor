//! Extensions' UI in the workspace (see `crate::extensions::contrib` and
//! `ui_host`): their views as activity bar tabs (`SidebarMode::Extension`),
//! their commands in Find Action, menus and toolbars, their panels in the
//! bottom area, and the requests their code makes while it runs (refresh a
//! view, show a panel, open a file, ask the user, report problems).

use std::{collections::HashMap, rc::Rc, sync::Arc};

use super::*;
use crate::{
    extension_panel::ExtensionPanelsEvent,
    extension_view::{ExtensionView, ExtensionViewEvent},
    extensions::{
        contrib::{Condition, Contributions, MenuPlace, ViewDef, When},
        ui_host::{self, Caller, Request, Tone},
    },
    lint::Diagnostic,
};

/// Runs one of an extension's commands (Find Action lists them as
/// "<extension>: <title>"; `keymap.json` can bind them).
#[derive(Clone, PartialEq, Debug, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workspace)]
#[serde(deny_unknown_fields)]
pub struct RunExtensionCommand {
    pub extension: String,
    pub command: String,
    /// What it runs on (a tree item's id, a file's path), as the code expects;
    /// for an editor-menu command, the current file when not given.
    #[serde(default)]
    pub target: Option<String>,
}

/// Shows or hides an extension's view (`view` = "<extension>/<view>").
#[derive(Clone, PartialEq, Debug, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workspace)]
#[serde(deny_unknown_fields)]
pub struct ToggleExtensionView {
    pub view: String,
}

// ---- View slots ------------------------------------------------------------------------

/// A contributed view's place in `SidebarMode::Extension`: views are
/// numbered once per run (a reload keeps the numbers), so the mode stays `Copy`.
struct Slot {
    key: String,
    title: SharedString,
    icon: SharedString,
}

static SLOTS: std::sync::Mutex<Vec<Slot>> = std::sync::Mutex::new(Vec::new());

fn slot_of(extension: &str, view: &ViewDef) -> u16 {
    let key = format!("{extension}/{}", view.id);
    let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ix) = slots.iter().position(|s| s.key == key) {
        slots[ix].title = view.title.clone().into();
        slots[ix].icon = view.icon.clone();
        return ix as u16;
    }
    slots.push(Slot { key, title: view.title.clone().into(), icon: view.icon.clone() });
    (slots.len() - 1) as u16
}

fn slot_key(slot: u16) -> Option<String> {
    SLOTS.lock().unwrap_or_else(|e| e.into_inner()).get(slot as usize).map(|s| s.key.clone())
}

impl SidebarMode {
    /// An extension view's title and icon (an asset path).
    pub(super) fn extension_look(self) -> Option<(SharedString, SharedString)> {
        let SidebarMode::Extension(slot) = self else { return None };
        SLOTS.lock().unwrap_or_else(|e| e.into_inner()).get(slot as usize).map(|s| (s.title.clone(), s.icon.clone()))
    }

    /// The state.json name of an extension view's mode.
    pub(super) fn extension_key(self) -> Option<String> {
        let SidebarMode::Extension(slot) = self else { return None };
        slot_key(slot)
    }
}

/// The extension view a saved "extension:<extension>/<view>" names, if it's installed.
pub(super) fn restore_extension_mode(saved: &str) -> Option<SidebarMode> {
    let key = saved.strip_prefix("extension:")?;
    let (extension, view) = key.split_once('/')?;
    let contributions = crate::extensions::contributions();
    let c = contributions.iter().find(|c| c.extension == extension)?;
    let def = c.views.iter().find(|v| v.id == view)?;
    Some(SidebarMode::Extension(slot_of(extension, def)))
}

// ---- Requests from extensions' code -------------------------------------------------------

/// Hands a request from an extension's code to the window it's for (or to
/// every window, for a refresh or problems). One the user must answer gets
/// no answer when its window is gone, which the code sees as cancelled.
pub fn handle_extension_request(request: Request, cx: &mut App) {
    let workspaces: Vec<WindowHandle<Workspace>> = cx.windows().into_iter().filter_map(|w| w.downcast::<Workspace>()).collect();
    let target = match &request {
        Request::Refresh { .. } | Request::Diagnostics { .. } => None,
        Request::ShowPanel { window, .. }
        | Request::OpenFile { window, .. }
        | Request::Notify { window, .. }
        | Request::AskInput { window, .. }
        | Request::Confirm { window, .. }
        | Request::Permission { window, .. }
        | Request::RunInTerminal { window, .. } => Some(*window),
    };
    match target {
        None => {
            let request = Rc::new(std::cell::RefCell::new(Some(request)));
            for handle in workspaces {
                let request = request.clone();
                handle
                    .update(cx, |ws, window, cx| {
                        if let Some(r) = request.borrow().as_ref() {
                            ws.extension_broadcast(r, window, cx)
                        }
                    })
                    .ok();
            }
        }
        Some(id) => {
            // The window that made the call, else the frontmost workspace.
            let made_it = workspaces.iter().copied().find(|h| h.update(cx, |_, _, cx| cx.entity_id().as_u64() == id).unwrap_or(false));
            let handle = made_it
                .or_else(|| cx.active_window().and_then(|w| workspaces.iter().copied().find(|h| h.window_id() == w.window_id())))
                .or(workspaces.first().copied());
            if let Some(handle) = handle {
                handle.update(cx, |ws, window, cx| ws.extension_request(request, window, cx)).ok();
            }
        }
    }
}

impl Workspace {
    /// What the code asks of this window.
    fn extension_request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            Request::ShowPanel { extension, panel, title, content, .. } => {
                let Some(c) = contribution(&extension) else { return };
                let panel_title = c.panel(&panel).map_or_else(|| panel.clone(), |p| p.title.clone());
                self.ext_panels.update(cx, |p, cx| p.show(extension, panel, panel_title, title, content, cx));
                self.has_ext_panel = true;
                self.bottom = Some(BottomPanel::Extension);
                cx.notify();
            }
            Request::OpenFile { path, line, .. } => self.open_at(path, line, None, window, cx),
            Request::Notify { extension, tone, message, detail, .. } => {
                let kind = match tone {
                    Tone::Success => ToastKind::Success,
                    Tone::Error | Tone::Warning => ToastKind::Error,
                    Tone::Normal | Tone::Muted => ToastKind::Info,
                };
                let name = contribution(&extension).map_or(extension, |c| c.name.clone());
                self.toast(kind, message, Some(detail.unwrap_or_else(|| format!("From the {name} extension"))), cx);
            }
            Request::AskInput { extension, prompt, initial, secret, answer, .. } => {
                let name = contribution(&extension).map_or(extension, |c| c.name.clone());
                let dialog = cx.new(|cx| {
                    let dialog = InputDialog::new(prompt, Icon::Puzzle, format!("Asked by the {name} extension"), &initial, None, window, cx).allow_empty();
                    if secret { dialog.secret(cx) } else { dialog }
                });
                let answer = Rc::new(std::cell::Cell::new(Some(answer)));
                let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
                    let reply = match event {
                        InputDialogEvent::Confirm(text) => Some(text.clone()),
                        InputDialogEvent::Dismissed => None,
                    };
                    if let Some(answer) = answer.take() {
                        answer.send(reply).ok();
                    }
                    ws.close_modal(window, cx);
                });
                self.modals.open(Modal::Input(dialog), subscription);
                cx.notify();
            }
            Request::Confirm { message, detail, action, answer, .. } => {
                let reply = window.prompt(PromptLevel::Warning, &message, Some(&detail), &[action.as_str(), "Cancel"], cx);
                cx.spawn(async move |_, _| {
                    answer.send(reply.await == Ok(0)).ok();
                })
                .detach();
            }
            Request::Permission { name, permission, answer, .. } => {
                let message = format!("Allow the {name} extension to {}?", permission.describe());
                let detail = "Kuana remembers your answer. You can revoke it from the extension's row in Settings → Extensions.";
                let reply = window.prompt(PromptLevel::Info, &message, Some(detail), &["Allow", "Don't Allow"], cx);
                cx.spawn(async move |_, _| {
                    answer.send(reply.await == Ok(0)).ok();
                })
                .detach();
            }
            Request::RunInTerminal { extension, title, command, .. } => {
                let cwd = self.root.clone().unwrap_or_else(|| self.terminal_cwd(cx));
                let name = contribution(&extension).map_or(extension, |c| c.name.clone());
                let target = crate::run::RunTarget { name: format!("{name}: {title}"), command, cwd, row: None, rows: 0..0, debug: None, test: None };
                self.run_target(target, window, cx);
            }
            Request::Refresh { .. } | Request::Diagnostics { .. } => self.extension_broadcast(&request, window, cx),
        }
    }

    /// A request every window hears.
    fn extension_broadcast(&mut self, request: &Request, _: &mut Window, cx: &mut Context<Self>) {
        match request {
            Request::Refresh { extension, view, item } => {
                for entity in self.ext_views.values() {
                    let matches = entity.read(cx).extension().extension == *extension && entity.read(cx).view().id == *view;
                    if matches {
                        entity.update(cx, |v, cx| v.refresh(item.clone(), cx));
                    }
                }
            }
            Request::Diagnostics { extension, path, problems } => {
                let list = self.ext_problems.entry(path.clone()).or_default();
                list.retain(|(e, _)| e != extension);
                if !problems.is_empty() {
                    list.push((extension.clone(), problems.clone()));
                }
                if let Some(editor) = self.tab_for(path, cx) {
                    apply_problems(&editor, extension, problems, cx);
                }
            }
            _ => {}
        }
    }

    /// A file opened: the problems extensions reported for it.
    pub(super) fn apply_extension_problems(&self, editor: &Entity<Editor>, cx: &mut App) {
        let Some(path) = editor.read(cx).doc.read(cx).path.clone() else { return };
        for (extension, problems) in self.ext_problems.get(&path).into_iter().flatten() {
            apply_problems(editor, extension, problems, cx);
        }
    }

    // ---- Views ---------------------------------------------------------------------------

    /// The extension views the activity bar shows now, in the extensions'
    /// order (worked out when the project, the open files or the extensions
    /// change: `refresh_extension_views`).
    pub(super) fn extension_modes(&self) -> &[SidebarMode] {
        &self.ext_modes
    }

    /// Works out which extension views apply (their `when`), and gives the
    /// open ones their extension's latest definition.
    pub(super) fn refresh_extension_views(&mut self, cx: &mut Context<Self>) {
        let languages: Vec<&'static str> = self.editors.iter().map(|e| e.read(cx).doc.read(cx).language.name()).collect();
        let mut modes = Vec::new();
        for c in crate::extensions::contributions() {
            for view in &c.views {
                let shown = match &view.when {
                    When::Always => true,
                    When::File(name) => self.root.as_ref().is_some_and(|r| r.join(name).exists()),
                    When::Language(name) => languages.iter().any(|l| l.eq_ignore_ascii_case(name)),
                };
                let slot = slot_of(&c.extension, view);
                if shown {
                    modes.push(SidebarMode::Extension(slot));
                }
                if let Some(entity) = self.ext_views.get(&slot).cloned() {
                    let changed = !Arc::ptr_eq(entity.read(cx).extension(), &c);
                    if changed {
                        let (c, view) = (c.clone(), view.clone());
                        entity.update(cx, |v, cx| v.set_extension(c, view, cx));
                    }
                }
            }
        }
        // Views whose extension went.
        let keep: Vec<u16> = crate::extensions::contributions()
            .iter()
            .flat_map(|c| c.views.iter().map(|v| slot_of(&c.extension, v)).collect::<Vec<_>>())
            .collect();
        self.ext_views.retain(|slot, _| keep.contains(slot));
        let installed: Vec<String> = crate::extensions::contributions().iter().map(|c| c.extension.clone()).collect();
        self.ext_panels.update(cx, |p, cx| p.retain_extensions(|e| installed.iter().any(|i| i == e), cx));
        if let SidebarMode::Extension(slot) = self.sidebar_mode
            && !keep.contains(&slot)
        {
            self.sidebar_mode = SidebarMode::Files;
        }
        if modes != self.ext_modes {
            self.ext_modes = modes;
            cx.notify();
        }
    }

    /// What extensions' code is told about this window.
    fn extension_caller(&self, extension: &str, cx: &Context<Self>) -> Caller {
        let settings = Settings::get(cx).extensions.as_ref().and_then(|map| map.get(extension)).map(|v| v.to_string()).unwrap_or_default();
        Caller { window: cx.entity_id().as_u64(), root: self.root.clone(), settings }
    }

    /// The view entity for `slot`, made the first time it shows.
    pub(super) fn extension_view(&mut self, slot: u16, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<ExtensionView>> {
        if let Some(entity) = self.ext_views.get(&slot) {
            return Some(entity.clone());
        }
        let key = slot_key(slot)?;
        let (extension, view_id) = key.split_once('/')?;
        let c = contribution(extension)?;
        let view = c.views.iter().find(|v| v.id == view_id)?.clone();
        let caller = self.extension_caller(extension, cx);
        let entity = cx.new(|cx| ExtensionView::new(c, view, caller, cx));
        let subscription = cx.subscribe_in(&entity, window, |ws, view, event: &ExtensionViewEvent, window, cx| ws.on_extension_view_event(view.clone(), event, window, cx));
        self.ext_view_subscriptions.push(subscription);
        self.ext_views.insert(slot, entity.clone());
        Some(entity)
    }

    /// The project moved or settings changed: the views' code is told.
    pub(super) fn extension_callers_changed(&mut self, cx: &mut Context<Self>) {
        for entity in self.ext_views.values().cloned().collect::<Vec<_>>() {
            let caller = self.extension_caller(&entity.read(cx).extension().extension.clone(), cx);
            entity.update(cx, |v, cx| v.set_caller(caller, cx));
        }
    }

    /// Shows an extension's view, or hides the sidebar when it's showing.
    pub(super) fn toggle_extension_mode(&mut self, mode: SidebarMode, window: &mut Window, cx: &mut Context<Self>) {
        let SidebarMode::Extension(slot) = mode else { return };
        if self.show_sidebar && self.sidebar_mode == mode {
            self.show_sidebar = false;
            self.focus_active(window, cx);
        } else if let Some(view) = self.extension_view(slot, window, cx) {
            self.show_sidebar = true;
            self.sidebar_mode = mode;
            window.focus(&view.focus_handle(cx));
        }
        self.save_state(cx);
        cx.notify();
    }

    pub(super) fn toggle_extension_view_action(&mut self, action: &ToggleExtensionView, window: &mut Window, cx: &mut Context<Self>) {
        let Some((extension, view)) = action.view.split_once('/') else { return };
        let Some(c) = contribution(extension) else {
            self.toast(ToastKind::Error, format!("No extension {extension} is installed"), None, cx);
            return;
        };
        let Some(def) = c.views.iter().find(|v| v.id == view) else { return };
        self.toggle_extension_mode(SidebarMode::Extension(slot_of(extension, def)), window, cx);
    }

    /// The sidebar's body for an extension view.
    pub(super) fn render_extension_sidebar(&self, slot: u16) -> Option<AnyElement> {
        let entity = self.ext_views.get(&slot)?;
        Some(div().flex_1().min_h_0().child(cached(entity)).into_any_element())
    }

    fn on_extension_view_event(&mut self, view: Entity<ExtensionView>, event: &ExtensionViewEvent, window: &mut Window, cx: &mut Context<Self>) {
        let c = view.read(cx).extension().clone();
        match event {
            ExtensionViewEvent::Run { command, target } => self.run_extension_command(c, command.clone(), target.clone(), cx),
            ExtensionViewEvent::Menu { item, position } => {
                let place = MenuPlace::ViewItem(view.read(cx).view().id.clone());
                let kind = item.kind.clone();
                let commands: Vec<(String, String, Option<SharedString>)> = c
                    .menu(&place, |cond| matches!(cond, Condition::Kind(k) if kind.as_deref() == Some(k.as_str())))
                    .map(|cmd| (cmd.id.clone(), cmd.title.clone(), cmd.icon.clone()))
                    .collect();
                if commands.is_empty() {
                    return;
                }
                let mut entries = Vec::new();
                let mut actions = Vec::new();
                for (command, title, _) in commands {
                    entries.push(MenuEntry::text(title));
                    let (c, target) = (c.clone(), item.id.clone());
                    actions.push(Some(MenuAction::Call(Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
                        ws.run_extension_command(c.clone(), command.clone(), Some(target.clone()), cx)
                    }))));
                }
                self.show_context_menu(entries, actions, *position, None, window, cx);
            }
            ExtensionViewEvent::Leave => {
                if self.active_item(cx).is_some() {
                    self.focus_active(window, cx);
                }
            }
        }
    }

    // ---- Commands -------------------------------------------------------------------------

    /// Runs a command in the extension's code; its failure becomes a toast.
    pub(super) fn run_extension_command(&mut self, extension: Arc<Contributions>, command: String, target: Option<String>, cx: &mut Context<Self>) {
        let caller = self.extension_caller(&extension.extension, cx);
        let title = extension.command(&command).map_or_else(|| command.clone(), |c| c.title.clone());
        let name = extension.name.clone();
        let job = ui_host::run_command(extension, caller, command, target);
        cx.spawn(async move |ws, cx| {
            if let Err(err) = job.await {
                ws.update(cx, |ws, cx| ws.toast(ToastKind::Error, format!("{name}: {title} failed"), Some(err), cx)).ok();
            }
        })
        .detach();
    }

    /// Find Action's (and key bindings') way in: on the current file when
    /// the command is in the editor's menu, else on nothing.
    pub(super) fn run_extension_command_action(&mut self, action: &RunExtensionCommand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = contribution(&action.extension) else {
            self.toast(ToastKind::Error, format!("No extension {} is installed", action.extension), None, cx);
            return;
        };
        if c.command(&action.command).is_none() {
            self.toast(ToastKind::Error, format!("{} has no command {}", c.name, action.command), None, cx);
            return;
        }
        let in_editor = c.menus.iter().any(|m| m.place == MenuPlace::Editor && m.command == action.command);
        let target = action.target.clone().or_else(|| {
            in_editor
                .then(|| self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.clone()))
                .flatten()
                .map(|p| p.to_string_lossy().into_owned())
        });
        self.run_extension_command(c, action.command.clone(), target, cx);
    }

    /// Find Action's entries for every installed extension's commands.
    pub(super) fn extension_action_entries() -> Vec<ActionEntry> {
        static NAMES: std::sync::Mutex<Vec<(String, &'static str)>> = std::sync::Mutex::new(Vec::new());
        let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for c in crate::extensions::contributions() {
            for command in &c.commands {
                let label = format!("{}: {}", c.name, command.title);
                // Leaked once per label: ActionEntry names are static.
                let name = match names.iter().find(|(l, _)| *l == label) {
                    Some((_, name)) => *name,
                    None => {
                        let name: &'static str = Box::leak(label.clone().into_boxed_str());
                        names.push((label, name));
                        name
                    }
                };
                out.push(ActionEntry {
                    name,
                    action: Box::new(RunExtensionCommand { extension: c.extension.clone(), command: command.id.clone(), target: None }),
                });
            }
            for view in &c.views {
                let label = format!("{}: Show {}", c.name, view.title);
                let name = match names.iter().find(|(l, _)| *l == label) {
                    Some((_, name)) => *name,
                    None => {
                        let name: &'static str = Box::leak(label.clone().into_boxed_str());
                        names.push((label, name));
                        name
                    }
                };
                out.push(ActionEntry { name, action: Box::new(ToggleExtensionView { view: format!("{}/{}", c.extension, view.id) }) });
            }
        }
        out
    }

    /// The editor menu's entries from extensions, for a file in `language`.
    pub(super) fn extension_editor_entries(&self, path: Option<&Path>, language: &str) -> Vec<(MenuEntry, Option<MenuAction>)> {
        let Some(path) = path else { return Vec::new() };
        let mut out = Vec::new();
        for c in crate::extensions::contributions() {
            let commands: Vec<(String, String)> = c
                .menu(&MenuPlace::Editor, |cond| matches!(cond, Condition::Language(l) if l.eq_ignore_ascii_case(language)))
                .map(|cmd| (cmd.id.clone(), cmd.title.clone()))
                .collect();
            for (command, title) in commands {
                let (c, target) = (c.clone(), path.to_string_lossy().into_owned());
                out.push((
                    MenuEntry::item(title, Icon::Puzzle),
                    Some(MenuAction::Call(Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
                        ws.run_extension_command(c.clone(), command.clone(), Some(target.clone()), cx)
                    }))),
                ));
            }
        }
        out
    }

    /// After an explicit Save: a file in an extension's work folder goes to
    /// its code (`file-saved`).
    pub(super) fn extension_file_saved(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(path) = editor.read(cx).doc.read(cx).path.clone() else { return };
        for c in crate::extensions::contributions() {
            if c.code.is_none() || !ui_host::in_work_folder(&c.extension, &path) {
                continue;
            }
            let caller = self.extension_caller(&c.extension, cx);
            let name = c.name.clone();
            let job = ui_host::file_saved(c, caller, path.clone());
            cx.spawn(async move |ws, cx| {
                if let Err(err) = job.await {
                    ws.update(cx, |ws, cx| ws.toast(ToastKind::Error, format!("{name} couldn't handle the save"), Some(err), cx)).ok();
                }
            })
            .detach();
        }
    }

    // ---- Panels ---------------------------------------------------------------------------

    pub(super) fn render_extension_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        area(&theme).h(self.shown_panel_height()).flex_shrink_0().child(cached(&self.ext_panels)).child(self.resize_handle(Edge::Problems, cx))
    }

    pub(super) fn on_extension_panels_event(&mut self, event: &ExtensionPanelsEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            ExtensionPanelsEvent::Close => {
                self.hide_bottom(BottomPanel::Extension);
                self.focus_active(window, cx);
                cx.notify();
            }
        }
    }

    pub(super) fn toggle_extension_output(&mut self, _: &ToggleExtensionOutput, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_extension_panel(window, cx);
    }

    pub(super) fn toggle_extension_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows(BottomPanel::Extension) {
            self.hide_bottom(BottomPanel::Extension);
            self.focus_active(window, cx);
        } else {
            self.bottom = Some(BottomPanel::Extension);
        }
        cx.notify();
    }
}

fn contribution(extension: &str) -> Option<Arc<Contributions>> {
    crate::extensions::contributions().into_iter().find(|c| c.extension == extension)
}

/// An extension's problems as a document's diagnostics (its own list, so
/// they don't mix with a language server's).
fn apply_problems(editor: &Entity<Editor>, extension: &str, problems: &[ui_host::Problem], cx: &mut App) {
    let diagnostics: Vec<Diagnostic> = {
        let doc = editor.read(cx).doc.read(cx);
        let lines = doc.rope().len_lines();
        problems
            .iter()
            .map(|p| {
                let row = (p.line.max(1) as usize - 1).min(lines.saturating_sub(1));
                let start = doc.line_start(row);
                let text = doc.line_text(row);
                let byte = |col: u32| text.char_indices().nth(col.max(1) as usize - 1).map_or(text.len(), |(i, _)| i);
                let from = byte(p.column);
                let to = match p.end_column {
                    Some(end) if end > p.column => byte(end),
                    _ => text[from..].find(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(text.len(), |i| from + i.max(1)),
                };
                Diagnostic {
                    range: start + from..start + to.max(from),
                    severity: p.severity,
                    message: Arc::from(p.message.as_str()),
                    rule: std::borrow::Cow::Owned(extension.to_string()),
                    unnecessary: false,
                }
            })
            .collect()
    };
    let server = format!("extension:{extension}");
    editor.update(cx, |e, cx| {
        e.doc.as_mut(cx).set_server_diagnostics(&server, diagnostics);
        cx.notify();
    });
}

/// Problems extensions reported, by file, then by extension.
pub(super) type ProblemMap = HashMap<PathBuf, Vec<(String, Vec<ui_host::Problem>)>>;
