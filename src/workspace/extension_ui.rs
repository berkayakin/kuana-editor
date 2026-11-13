//! Extensions' UI in the workspace (see `crate::extensions::contrib` and
//! `ui_host`): their views as activity bar tabs (`SidebarMode::Extension`),
//! their commands in Find Action, menus and toolbars, their panels in the
//! bottom area, and the requests their code makes while it runs (refresh a
//! view, show a panel, open a file, ask the user, report problems, read and
//! edit the open file, set a status item or a timer). The other way, what
//! the code is told: `extension_event` (a file opened, saved, closed, the
//! project, the tab the user is in, settings, timers).
//!
//! Extensions' code costs nothing until it's needed, and nothing at startup:
//! an event goes only to the extensions that list it (none: one read of the
//! registry), and the first ones wait `STARTUP_QUIET` so no code is compiled
//! or loaded while the window comes up.

use std::{
    collections::HashMap,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::Instant,
};

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};

use super::*;
use crate::{
    extension_panel::ExtensionPanelsEvent,
    extension_view::{ExtensionView, ExtensionViewEvent},
    extensions::{
        contrib::{Api, Condition, Contributions, FileSubject, MenuPlace, Permission, ViewDef, When},
        ui_host::{self, Caller, Event, Request, StatusItem, Tone},
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

/// How long after the first event extensions' code is left alone: starting
/// Kuana opens a project and its tabs, and none of that should wait for (or
/// share the processor with) code being loaded.
const STARTUP_QUIET: Duration = Duration::from_secs(2);
/// How long the user stays in a tab before `editor-changed` says so.
const EDITOR_CHANGED_DELAY: Duration = Duration::from_millis(200);

/// When extensions' code may first hear events: `STARTUP_QUIET` after the
/// first one (which comes as the first window opens its project).
fn awake_at() -> Instant {
    static AT: OnceLock<Instant> = OnceLock::new();
    *AT.get_or_init(|| Instant::now() + STARTUP_QUIET)
}

/// An event on its way to one extension's code.
struct Delivery {
    extension: Arc<Contributions>,
    event: Event,
    needs: Option<Permission>,
    /// A timer's: told how it went, and nobody else is.
    done: Option<oneshot::Sender<Result<(), String>>>,
}

/// What a status bar item of an extension shows in this window.
pub(super) struct ShownStatus {
    pub extension: Arc<Contributions>,
    pub item: String,
    pub status: StatusItem,
    /// Its icon as an asset path, found when the status was set (an
    /// extension's own SVG is looked for on disk).
    pub icon: Option<SharedString>,
    /// Whether its `when` holds, worked out when the project, the open files
    /// or the extensions change: the status bar only reads it.
    pub visible: bool,
}

/// What extensions' code keeps in a window while it runs.
#[derive(Default)]
pub(super) struct ExtensionRuntime {
    /// The window (its workspace's entity id): the programs extensions
    /// started for it end with it.
    window: Option<u64>,
    /// Status bar items the code has filled, in the order they first showed.
    pub status: Vec<ShownStatus>,
    /// Timers by (extension, name): dropping one ends it.
    timers: HashMap<(String, String), Task<()>>,
    /// Timers that came due while the window wasn't the active one.
    waiting_for_window: Vec<oneshot::Sender<()>>,
    /// Events in the order they happened, to the task that hands them over.
    events: Option<mpsc::UnboundedSender<Delivery>>,
    /// Whether any extension listens for `editor-changed` (checked each
    /// render, so it's worked out when the extensions change).
    hears_editor_changed: bool,
    /// The tab the user was in at the last render, so a render in the same
    /// one reads nothing more.
    seen_item: Option<EntityId>,
    /// The file `editor-changed` was last told (or is about to be).
    told_editor: Option<PathBuf>,
    editor_changed: Option<Task<()>>,
    /// The project each listening extension was told is open.
    told_project: Vec<(String, PathBuf)>,
    /// The settings each listening extension has, as it last heard.
    told_settings: HashMap<String, String>,
}

impl ExtensionRuntime {
    pub(super) fn for_window(window: u64) -> Self {
        let mut runtime = Self::default();
        runtime.window = Some(window);
        runtime
    }
}

impl Drop for ExtensionRuntime {
    fn drop(&mut self) {
        if let Some(window) = self.window {
            ui_host::window_closed(window);
        }
    }
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
        | Request::Editor { window, .. }
        | Request::ApplyEdits { window, .. }
        | Request::SetSelections { window, .. }
        | Request::Pick { window, .. }
        | Request::Status { window, .. }
        | Request::AppendPanel { window, .. }
        | Request::ProcessEvent { window, .. }
        | Request::Timer { window, .. }
        | Request::ShowResults { window, .. }
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
            Request::Permission { name, permission, scope, answer, .. } => {
                let message = format!("Allow the {name} extension to {}?", permission.describe(scope.as_deref()));
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
            Request::Editor { path, with_text, answer, .. } => {
                let editor = match &path {
                    Some(path) => self.tab_for(path, cx),
                    None => self.active_editor(cx),
                };
                // A restored tab that wasn't shown yet reads its file now.
                if let Some(editor) = &editor {
                    self.load_tab(editor, cx);
                }
                answer.send(editor.and_then(|e| e.read(cx).extension_state(with_text, cx))).ok();
            }
            Request::ApplyEdits { path, version, edits, answer, .. } => {
                let result = match self.tab_for(&path, cx) {
                    Some(editor) => editor.update(cx, |e, cx| e.apply_extension_edits(version, edits, cx)),
                    None => Err(format!("{} isn't open in a tab", path.display())),
                };
                answer.send(result).ok();
            }
            Request::SetSelections { path, version, selections, answer, .. } => {
                let result = match self.tab_for(&path, cx) {
                    Some(editor) => editor.update(cx, |e, cx| e.set_extension_selections(version, selections, cx)),
                    None => Err(format!("{} isn't open in a tab", path.display())),
                };
                answer.send(result).ok();
            }
            Request::Pick { extension, placeholder, items, answer, .. } => self.extension_pick(extension, placeholder, items, answer, window, cx),
            Request::ShowResults { extension, title, items, .. } => {
                let Some(c) = contribution(&extension) else { return };
                let nodes = crate::extensions::results::result_nodes(items, &c);
                // Its tab is found again by extension and title, so showing them
                // again (a refresh, maybe from a timer) leaves the keyboard be.
                let kind = format!("extension:{extension}:{title}");
                let new = !self.results.read(cx).has_kind(&kind);
                self.results.update(cx, |p, cx| p.show_kind(kind, title.into(), nodes, Vec::new(), cx));
                self.has_results = true;
                self.bottom = Some(BottomPanel::Results);
                if new {
                    window.focus(&self.results.focus_handle(cx));
                }
                cx.notify();
            }
            Request::Status { extension, item, status, .. } => {
                let at = self.ext_runtime.status.iter().position(|s| s.extension.extension == extension && s.item == item);
                let Some(c) = contribution(&extension) else { return };
                match (at, status) {
                    (Some(at), Some(status)) if self.ext_runtime.status[at].status == status => return,
                    (Some(at), None) => {
                        self.ext_runtime.status.remove(at);
                    }
                    (at, Some(status)) => {
                        let icon = status.icon.as_deref().and_then(|icon| crate::extension_view::icon_path(&c, icon));
                        let visible = c.status.iter().find(|s| s.id == item).is_some_and(|s| self.extension_when_holds(&s.when, cx));
                        let shown = ShownStatus { extension: c, item, status, icon, visible };
                        match at {
                            Some(at) => self.ext_runtime.status[at] = shown,
                            None => self.ext_runtime.status.push(shown),
                        }
                    }
                    (None, None) => return,
                }
                cx.notify();
            }
            Request::AppendPanel { extension, panel, lines, .. } => {
                let Some(c) = contribution(&extension) else { return };
                let title = c.panel(&panel).map_or_else(|| panel.clone(), |p| p.title.clone());
                // Only the panel is told (it redraws alone); the window hears of a new one.
                if self.ext_panels.update(cx, |p, cx| p.append(extension, panel, title, lines, cx)) {
                    self.has_ext_panel = true;
                    self.bottom = Some(BottomPanel::Extension);
                    cx.notify();
                }
            }
            Request::ProcessEvent { extension, event, .. } => {
                if let Some(c) = contribution(&extension).filter(|c| c.api >= Api::V2 && c.code.is_some()) {
                    self.deliver_extension_event(Delivery { extension: c, event, needs: None, done: None }, cx);
                }
            }
            Request::Timer { extension, name, every, .. } => self.set_extension_timer(extension, name, every, window, cx),
            Request::Refresh { .. } | Request::Diagnostics { .. } => self.extension_broadcast(&request, window, cx),
        }
    }

    // ---- Events and timers ---------------------------------------------------------------

    /// Tells extensions' code that `name` happened (`contrib::EVENTS`), to
    /// `path` when it's about a file or folder. It reaches the extensions
    /// that list the event, and for a file only those that can reach it: one
    /// in their own folder, or in the project when they declare
    /// `workspace-files` (which the user is asked for before the first one).
    /// A save of an extension's own file always reaches it (0.1's `file-saved`).
    pub(super) fn extension_event(&mut self, name: &'static str, path: Option<&Path>, language: Option<&str>, cx: &mut Context<Self>) {
        let about_file = !matches!(name, "project-opened" | "project-closed" | "settings-changed");
        for c in crate::extensions::contributions() {
            if c.code.is_none() {
                continue;
            }
            let own = about_file && path.is_some_and(|p| ui_host::in_work_folder(&c.extension, p));
            let needs = match (own, about_file) {
                (true, _) if name == "file-saved" || (c.api >= Api::V2 && c.hears(name)) => None,
                (false, false) if c.api >= Api::V2 && c.hears(name) => None,
                (false, true) if c.api >= Api::V2 && c.hears(name) => {
                    let in_project = path.zip(self.root.as_ref()).is_some_and(|(p, root)| p.starts_with(root));
                    if !in_project || !c.permissions.contains(&Permission::WorkspaceFiles) {
                        continue;
                    }
                    Some(Permission::WorkspaceFiles)
                }
                _ => continue,
            };
            let event = Event { name, path: path.map(Path::to_path_buf), language: language.map(str::to_string), data: None };
            self.deliver_extension_event(Delivery { extension: c, event, needs, done: None }, cx);
        }
    }

    /// Queues an event for an extension's code. One task per window hands
    /// them over in order, the first ones once Kuana has started.
    fn deliver_extension_event(&mut self, delivery: Delivery, cx: &mut Context<Self>) {
        let events = self.ext_runtime.events.get_or_insert_with(|| {
            let (send, mut queued) = mpsc::unbounded::<Delivery>();
            cx.spawn(async move |ws, cx| {
                while let Some(Delivery { extension, event, needs, done }) = queued.next().await {
                    let wait = awake_at().saturating_duration_since(Instant::now());
                    if !wait.is_zero() {
                        cx.background_executor().timer(wait).await;
                    }
                    let name = extension.name.clone();
                    // The window as it is now (its project may have changed while this waited).
                    let Ok(job) = ws.update(cx, |ws, cx| ui_host::event(extension.clone(), ws.extension_caller(&extension.extension, cx), event, needs)) else {
                        break;
                    };
                    // Not waited for here: the code may take its time, and other extensions' events go on.
                    let ws = ws.clone();
                    cx.spawn(async move |cx| {
                        let result = job.await;
                        match done {
                            Some(done) => {
                                done.send(result).ok();
                            }
                            None => {
                                if let Err(err) = result {
                                    ws.update(cx, |ws, cx| ws.toast(ToastKind::Error, format!("The {name} extension failed"), Some(err), cx)).ok();
                                }
                            }
                        }
                    })
                    .detach();
                }
            })
            .detach();
            send
        });
        events.unbounded_send(delivery).ok();
    }

    /// A file's events: `name` for the file in `editor`.
    pub(super) fn extension_file_event(&mut self, name: &'static str, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let (path, language) = {
            let doc = editor.read(cx).doc.read(cx);
            (doc.path.clone(), doc.language.name())
        };
        if let Some(path) = path {
            self.extension_event(name, Some(&path), Some(language), cx);
        }
    }

    /// Called each render: when the user has moved to another file's tab and
    /// stayed, the extensions listening are told. One comparison when none is.
    pub(super) fn note_extension_editor(&mut self, cx: &mut Context<Self>) {
        if !self.ext_runtime.hears_editor_changed {
            return;
        }
        // The same tab as last time: nothing of the editor is read (the
        // root view mustn't come to depend on what changes as one types).
        let item = self.active_item(cx).map(|item| item.entity_id());
        if item == self.ext_runtime.seen_item {
            return;
        }
        let unread = self.active_editor(cx).is_some_and(|e| !e.read(cx).is_loaded());
        if !unread {
            // A restored tab still being read is looked at again once it's there.
            self.ext_runtime.seen_item = item;
        }
        let path = self.active_editor(cx).filter(|e| e.read(cx).is_loaded()).and_then(|e| e.read(cx).doc.read(cx).path.clone());
        if path.is_none() || path == self.ext_runtime.told_editor {
            return;
        }
        self.ext_runtime.told_editor = path.clone();
        self.ext_runtime.editor_changed = Some(cx.spawn(async move |ws, cx| {
            cx.background_executor().timer(EDITOR_CHANGED_DELAY).await;
            ws.update(cx, |ws, cx| {
                // Still there?
                if let Some(editor) = ws.active_editor(cx).filter(|e| e.read(cx).doc.read(cx).path == path) {
                    ws.extension_file_event("editor-changed", &editor, cx);
                }
            })
            .ok();
        }));
    }

    /// Sets or clears the window's timer `name` of an extension. When it's
    /// due the code's `on-event` is told, once the window is the active one;
    /// the next wait starts when the code has answered, so ticks never pile
    /// up. A timer whose code fails stops.
    fn set_extension_timer(&mut self, extension: String, name: String, every: Option<(u32, bool)>, window: &mut Window, cx: &mut Context<Self>) {
        let key = (extension.clone(), name.clone());
        self.ext_runtime.timers.remove(&key);
        let Some((seconds, repeat)) = every else { return };
        let task = cx.spawn_in(window, async move |ws, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(seconds.max(ui_host::MIN_TIMER_SECONDS) as u64)).await;
                let Ok(inactive) = ws.update_in(cx, |ws, window, _| {
                    (!window.is_window_active()).then(|| {
                        let (wake, woken) = oneshot::channel();
                        ws.ext_runtime.waiting_for_window.push(wake);
                        woken
                    })
                }) else {
                    return;
                };
                if let Some(woken) = inactive
                    && woken.await.is_err()
                {
                    return;
                }
                let (done, answered) = oneshot::channel();
                let told = ws.update(cx, |ws, cx| {
                    let Some(c) = contribution(&extension).filter(|c| c.api >= Api::V2 && c.code.is_some()) else { return false };
                    let event = Event { name: "timer", path: None, language: None, data: Some(name.clone()) };
                    ws.deliver_extension_event(Delivery { extension: c, event, needs: None, done: Some(done) }, cx);
                    true
                });
                let result = match told {
                    Ok(true) => answered.await.unwrap_or(Ok(())),
                    _ => Err(String::new()),
                };
                if result.is_ok() && repeat {
                    continue;
                }
                ws.update(cx, |ws, cx| {
                    if let Err(err) = result.as_ref()
                        && !err.is_empty()
                    {
                        let title = contribution(&extension).map_or_else(|| extension.clone(), |c| c.name.clone());
                        ws.toast(ToastKind::Error, format!("The {title} extension's timer {name} failed"), Some(err.clone()), cx);
                    }
                    // This task itself: it ends here either way.
                    ws.ext_runtime.timers.remove(&(extension.clone(), name.clone()));
                })
                .ok();
                return;
            }
        });
        self.ext_runtime.timers.insert(key, task);
    }

    /// A list the code asks the user to pick from (`pick`): the picked row's
    /// id, or none when they close it (or something else takes its place).
    fn extension_pick(
        &mut self,
        extension: String,
        placeholder: String,
        items: Vec<ui_host::PickItem>,
        answer: oneshot::Sender<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let c = contribution(&extension);
        let title: SharedString = c.as_ref().map_or(extension, |c| c.name.clone()).into();
        let choices = items
            .into_iter()
            .map(|item| crate::picker::Choice {
                id: item.id,
                label: item.label.into(),
                detail: item.detail.map(SharedString::from),
                icon: item.icon.and_then(|icon| c.as_ref().and_then(|c| crate::extension_view::icon_path(c, &icon))),
                file: None,
            })
            .collect();
        let picker = cx.new(|cx| {
            let mut picker = Picker::new(PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
            picker.set_choices(title, &placeholder, choices, cx);
            picker
        });
        let answer = Rc::new(std::cell::Cell::new(Some(answer)));
        let subscription = cx.subscribe_in(&picker, window, move |ws, _, event: &PickerEvent, window, cx| {
            let picked = match event {
                PickerEvent::Chosen(id) => Some(id.clone()),
                PickerEvent::Dismissed => None,
                _ => return,
            };
            if let Some(answer) = answer.take() {
                answer.send(picked).ok();
            }
            ws.close_modal(window, cx);
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    /// The window became the active one: timers that came due meanwhile go on.
    pub(super) fn extension_window_active(&mut self) {
        for wake in self.ext_runtime.waiting_for_window.drain(..) {
            wake.send(()).ok();
        }
    }

    /// Whether a view's or a status item's `when` holds in this window now.
    /// It looks at the disk (`file:`), so it's asked when something changes,
    /// never while drawing.
    fn extension_when_holds(&self, when: &When, cx: &App) -> bool {
        match when {
            When::Always => true,
            When::File(name) => self.root.as_ref().is_some_and(|r| r.join(name).exists()),
            When::Language(name) => self.editors.iter().any(|e| e.read(cx).doc.read(cx).language.name().eq_ignore_ascii_case(name)),
        }
    }

    /// The project, the open files or the extensions changed (installed,
    /// removed, reloaded): what's left of the ones that went goes, status
    /// items show by their `when`, and who listens for the current tab is
    /// worked out again.
    fn sync_extension_runtime(&mut self, installed: &[Arc<Contributions>], cx: &mut Context<Self>) {
        let current = |extension: &str| installed.iter().find(|c| c.extension == extension);
        self.ext_runtime.timers.retain(|(extension, _), _| current(extension).is_some_and(|c| c.api >= Api::V2));
        if !self.ext_runtime.status.is_empty() {
            let mut shown = std::mem::take(&mut self.ext_runtime.status);
            let before: Vec<bool> = shown.iter().map(|s| s.visible).collect();
            shown.retain_mut(|shown| {
                let Some(c) = current(&shown.extension.extension) else { return false };
                let Some(def) = c.status.iter().find(|s| s.id == shown.item) else { return false };
                shown.visible = self.extension_when_holds(&def.when, cx);
                shown.extension = c.clone();
                true
            });
            if shown.iter().map(|s| s.visible).ne(before.iter().copied()) {
                cx.notify();
            }
            self.ext_runtime.status = shown;
        }
        let runtime = &mut self.ext_runtime;
        runtime.hears_editor_changed = installed.iter().any(|c| c.api >= Api::V2 && c.hears("editor-changed"));

        // The project: each extension hears of the one that's open once, and
        // of its going (one installed later hears of the one already open).
        let root = self.root.clone();
        let mut events = Vec::new();
        runtime.told_project.retain(|(extension, _)| current(extension).is_some());
        for c in installed.iter().filter(|c| c.api >= Api::V2 && c.code.is_some()) {
            let told = runtime.told_project.iter().position(|(extension, _)| *extension == c.extension);
            if told.map(|ix| &runtime.told_project[ix].1) == root.as_ref() {
                continue;
            }
            if let Some(ix) = told {
                let (_, old) = runtime.told_project.remove(ix);
                events.push((c.clone(), "project-closed", old));
            }
            if let Some(root) = &root {
                runtime.told_project.push((c.extension.clone(), root.clone()));
                events.push((c.clone(), "project-opened", root.clone()));
            }
        }
        for (c, name, path) in events {
            if c.hears(name) {
                let event = Event { name, path: Some(path), language: None, data: None };
                self.deliver_extension_event(Delivery { extension: c, event, needs: None, done: None }, cx);
            }
        }

        // The settings each listener starts from, to tell a change by.
        let told = &mut self.ext_runtime.told_settings;
        told.retain(|extension, _| current(extension).is_some());
        for c in installed.iter().filter(|c| c.api >= Api::V2 && c.hears("settings-changed")) {
            if !told.contains_key(&c.extension) {
                let user = Settings::get(cx).extensions.as_ref().and_then(|map| map.get(&c.extension));
                told.insert(c.extension.clone(), c.settings_json(user));
            }
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
        let contributions = crate::extensions::contributions();
        self.sync_extension_runtime(&contributions, cx);
        let installed: Vec<String> = contributions.iter().map(|c| c.extension.clone()).collect();
        self.ext_panels.update(cx, |p, cx| p.retain_extensions(|e| installed.iter().any(|i| i == e), cx));
        if let SidebarMode::Extension(slot) = self.sidebar_mode
            && !keep.contains(&slot)
        {
            self.sidebar_mode = self.first_sidebar_view();
        }
        // A moved view whose extension went leaves the second sidebar.
        self.moved_views.retain(|mode| !matches!(mode, SidebarMode::Extension(slot) if !keep.contains(slot)));
        if modes != self.ext_modes {
            self.ext_modes = modes;
            cx.notify();
        }
    }

    /// What extensions' code is told about this window.
    fn extension_caller(&self, extension: &str, cx: &Context<Self>) -> Caller {
        // What the user set, with the manifest's defaults for the rest.
        let user = Settings::get(cx).extensions.as_ref().and_then(|map| map.get(extension));
        let settings = match contribution(extension) {
            Some(c) => c.settings_json(user),
            None => user.map(|v| v.to_string()).unwrap_or_default(),
        };
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

    /// Makes the views of the extension views that show, in either sidebar:
    /// one restored from the last session has none until now.
    pub(super) fn make_shown_extension_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = [self.show_sidebar.then_some(self.sidebar_mode), self.second_open()];
        for mode in shown.into_iter().flatten() {
            if let SidebarMode::Extension(slot) = mode
                && !self.ext_views.contains_key(&slot)
            {
                self.extension_view(slot, window, cx);
            }
        }
    }

    /// The project moved or settings changed: the views' code is told.
    pub(super) fn extension_callers_changed(&mut self, cx: &mut Context<Self>) {
        for entity in self.ext_views.values().cloned().collect::<Vec<_>>() {
            let caller = self.extension_caller(&entity.read(cx).extension().extension.clone(), cx);
            entity.update(cx, |v, cx| v.set_caller(caller, cx));
        }
        // `settings-changed`, for the extensions whose own settings did.
        if self.ext_runtime.told_settings.is_empty() {
            return;
        }
        let user = Settings::get(cx).extensions.clone().unwrap_or_default();
        for c in crate::extensions::contributions() {
            let Some(told) = self.ext_runtime.told_settings.get_mut(&c.extension) else { continue };
            let now = c.settings_json(user.get(&c.extension));
            if *told != now {
                *told = now;
                let event = Event { name: "settings-changed", path: None, language: None, data: None };
                self.deliver_extension_event(Delivery { extension: c, event, needs: None, done: None }, cx);
            }
        }
    }

    /// Shows an extension's view, or hides the sidebar when it's showing.
    pub(super) fn toggle_extension_mode(&mut self, mode: SidebarMode, window: &mut Window, cx: &mut Context<Self>) {
        let SidebarMode::Extension(slot) = mode else { return };
        if self.view_shown(mode) {
            self.set_view_shown(mode, false);
            self.focus_active(window, cx);
        } else if let Some(view) = self.extension_view(slot, window, cx) {
            self.set_view_shown(mode, true);
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
        self.extension_menu_entries(&MenuPlace::Editor, &FileSubject { path, folder: false, language: Some(language) })
    }

    /// Extensions' entries for a menu on a file (the editor's, a tab's, the
    /// file tree's): the commands whose `when` holds for it, run on its path.
    pub(super) fn extension_menu_entries(&self, place: &MenuPlace, file: &FileSubject) -> Vec<(MenuEntry, Option<MenuAction>)> {
        let mut out = Vec::new();
        for c in crate::extensions::contributions() {
            let commands: Vec<(String, String)> = c.menu(place, |cond| cond.holds_for(file)).map(|cmd| (cmd.id.clone(), cmd.title.clone())).collect();
            for (command, title) in commands {
                let (c, target) = (c.clone(), file.path.to_string_lossy().into_owned());
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

    /// After an explicit Save: the extensions listening for saves are told,
    /// and the one whose own file it is (0.1's `file-saved`).
    pub(super) fn extension_file_saved(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        self.extension_file_event("file-saved", editor, cx);
    }

    // ---- Status bar -----------------------------------------------------------------------

    /// The status bar items extensions' code has filled (`set-status`), in
    /// this window. Only what `ExtensionRuntime` holds is read: nothing is
    /// asked of the code or the disk while drawing.
    pub(super) fn render_extension_status(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        for (ix, shown) in self.ext_runtime.status.iter().enumerate().filter(|(_, s)| s.visible) {
            let color: Hsla = match shown.status.tone {
                Tone::Normal => theme.text_muted.into(),
                Tone::Muted => theme.text_dim.into(),
                Tone::Success => theme.success.into(),
                Tone::Warning => theme.warning.into(),
                Tone::Error => theme.error.into(),
            };
            let command = shown.extension.status.iter().find(|s| s.id == shown.item).and_then(|s| s.command.clone());
            let tip: SharedString = shown.status.tooltip.clone().unwrap_or_else(|| shown.extension.name.clone()).into();
            let extension = shown.extension.clone();
            let item = div()
                .id(("extension-status", ix))
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                // Same size and rounding as the status bar's menus.
                .h(px(24.))
                .px_2()
                .rounded(px(9.))
                .text_color(color)
                .children(shown.icon.clone().map(|icon| gpui::svg().path(icon).size(px(13.)).flex_shrink_0().text_color(color)))
                .when(!shown.status.text.is_empty(), |el| el.child(SharedString::from(shown.status.text.clone())))
                .tooltip(move |_, cx| tooltip_view(tip.clone(), cx));
            out.push(match command {
                Some(command) => item
                    .cursor_pointer()
                    .on_click(cx.listener(move |ws, _, _, cx| ws.run_extension_command(extension.clone(), command.clone(), None, cx)))
                    .hover_bg(theme.hover)
                    .into_any_element(),
                None => item.into_any_element(),
            });
        }
        out
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
