//! Agent sessions in the window (`crate::agents`): the Agents view, session
//! tabs, and what sessions ask of the workspace. Agents read files as the
//! editor has them (unsaved edits included) and write through open documents
//! (one undo step, saved, the text before kept in Local History and in the
//! session's change log for Review / Reject). Kuana's own agent gets the
//! problems Kuana shows through `_kuana/problems`.

use super::*;
use crate::acp::RpcError;
use crate::agents::{
    session::{Before, ContextItem, Session, SessionEvent},
    sidebar::AgentsViewEvent,
    store::{self, KUANA_AGENT},
    view::{MenuKind, SessionView, SessionViewEvent},
};
use crate::diff_view::{Compared, DiffSource};
use serde_json::{Value, json};

impl Workspace {
    pub(super) fn toggle_agents(&mut self, _: &ToggleAgents, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_agents_view(cx);
        if self.view_shown(SidebarMode::Agents) {
            self.set_view_shown(SidebarMode::Agents, false);
            self.focus_active(window, cx);
        } else {
            self.set_view_shown(SidebarMode::Agents, true);
        }
        self.save_state(cx);
        cx.notify();
    }

    /// Tells the Agents view which project's sessions to list.
    pub(super) fn sync_agents_view(&self, cx: &mut Context<Self>) {
        let folders = self.folders.as_ref().map(|p| p.folders().to_vec()).unwrap_or_default();
        self.agents_view.update(cx, |v, cx| v.set_folders(folders, cx));
    }

    /// The project or the window closes: its sessions stop (saved), so no
    /// agent goes on working, or asking a window that's gone.
    pub(super) fn close_agent_sessions(&mut self, cx: &mut App) {
        self.agent_tabs.clear();
        let ids: Vec<String> = self.agent_sessions.drain().map(|(id, _)| id).collect();
        if let Some(store) = store::store(cx) {
            store.update(cx, |s, cx| {
                for id in &ids {
                    s.close(id, cx);
                }
            });
        }
    }

    pub(super) fn new_agent_session(&mut self, _: &NewAgentSession, window: &mut Window, cx: &mut Context<Self>) {
        let agent = self.last_agent.clone();
        self.start_agent_session(agent, window, cx);
    }

    /// Where a session works: the current file's folder, else the project's.
    fn agent_cwd(&mut self, cx: &mut Context<Self>) -> Option<PathBuf> {
        let Some(cwd) = self.active_folder(cx) else {
            self.toast(ToastKind::Info, "Open a folder first", Some("Agents work in a project.".into()), cx);
            return None;
        };
        if crate::fs::is_remote(&cwd) {
            self.toast(ToastKind::Info, "Agents work in local projects for now", None, cx);
            return None;
        }
        Some(cwd)
    }

    /// Starts a session with `agent` in a tab (installing the agent first
    /// when it isn't yet).
    pub(super) fn start_agent_session(&mut self, agent: String, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<SessionView>> {
        let store = store::store(cx)?;
        let cwd = self.agent_cwd(cx)?;
        if agent == KUANA_AGENT
            && let Err(why) = Settings::get(cx).ai.clone().unwrap_or_default().choice()
        {
            self.toast(ToastKind::Info, "Choose an AI model for Kuana Agent", Some(why), cx);
            cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Ai), cx));
            return None;
        }
        let choice = store.read(cx).choices().into_iter().find(|c| c.id == agent);
        if choice.as_ref().is_some_and(|c| !c.installed) {
            self.install_then_start(agent, window, cx);
            return None;
        }
        match store.update(cx, |s, cx| s.new_session(&agent, &cwd, cx)) {
            Ok(session) => {
                // Started now: its modes, models and commands show while
                // the first prompt is written.
                session.update(cx, |s, cx| s.start(cx));
                // The system's permission to notify, asked now that the user
                // is here (once; macOS remembers the answer).
                static ASKED: std::sync::Once = std::sync::Once::new();
                if Settings::get(cx).agent_notifications() {
                    ASKED.call_once(kuana_os::notify::prepare);
                }
                if self.last_agent != agent {
                    let remembered = agent.clone();
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(err) = SessionState::change(|state| state.last_agent = Some(remembered)) {
                                eprintln!("couldn't save the agent used last: {err}");
                            }
                        })
                        .detach();
                }
                self.last_agent = agent;
                Some(self.open_session_view(session, window, cx))
            }
            Err(error) => {
                self.toast(ToastKind::Error, "Couldn't start the session", Some(error), cx);
                None
            }
        }
    }

    fn install_then_start(&mut self, agent: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = store::store(cx) else { return };
        let name = store.read(cx).agent_name(&agent);
        store.update(cx, |s, cx| s.install(&agent, cx));
        self.toast(ToastKind::Info, format!("Installing {name}…"), Some("The session starts when it's done.".into()), cx);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(400)).await;
                let Ok(done) = store.read_with(cx, |s, _| s.choices().into_iter().find(|c| c.id == agent).map(|c| (c.installing, c.installed, c.error))) else { return };
                match done {
                    Some((true, _, _)) => continue,
                    Some((false, true, _)) => {
                        this.update_in(cx, |ws, window, cx| ws.start_agent_session(agent.clone(), window, cx)).ok();
                    }
                    Some((false, false, error)) => {
                        this.update(cx, |ws, cx| ws.toast(ToastKind::Error, format!("Couldn't install {name}"), error, cx)).ok();
                    }
                    None => {}
                }
                return;
            }
        })
        .detach();
    }

    /// The session in a tab: the one showing it, else a new tab, beside the
    /// code when the window has one pane.
    pub(super) fn open_session_view(&mut self, session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Entity<SessionView> {
        let shown = self.items(cx).into_iter().find_map(|item| item.agent().filter(|v| v.read(cx).session == session).cloned());
        if let Some(view) = shown {
            self.activate_item(&PaneItem::Agent(view.clone()), window, cx);
            window.focus(&view.read(cx).composer().focus_handle(cx));
            return view;
        }
        let view = cx.new(|cx| SessionView::new(session, cx));
        let source = self.active_pane.clone();
        let beside = self.center.panes().len() == 1
            && !source.read(cx).is_empty()
            && crate::workspace::pane_tree::can_split(source.read(cx).bounds(), Side::Right, layout_gap(Theme::get(cx)));
        if beside {
            let pane = self.new_pane(window, cx);
            self.center.split(&source, pane.clone(), Side::Right);
            self.active_pane = pane;
        }
        self.add_tab(PaneItem::Agent(view.clone()), window, cx);
        window.focus(&view.read(cx).composer().focus_handle(cx));
        self.schedule_session_save(cx);
        view
    }

    /// A session tab: the workspace hears it, and its session once.
    pub(super) fn add_agent_tab(&mut self, view: &Entity<SessionView>, window: &mut Window, cx: &mut Context<Self>) {
        let files = self.files.openable();
        view.update(cx, |v, cx| v.set_files(files, cx));
        self.sync_agent_place(view, cx);
        let sub = cx.subscribe_in(view, window, |ws, view, event: &SessionViewEvent, window, cx| ws.on_session_view(view, event, window, cx));
        self.agent_tabs.insert(view.entity_id(), vec![sub]);
        let session = view.read(cx).session.clone();
        let id = session.read(cx).id.clone();
        if let std::collections::hash_map::Entry::Vacant(slot) = self.agent_sessions.entry(id) {
            slot.insert(cx.subscribe_in(&session, window, |ws, session, event: &SessionEvent, window, cx| ws.on_session_event(session, event, window, cx)));
        }
    }

    /// Tells a session's view about its folder: whether it's in a Git
    /// repository (a worktree may be made) and how many folders the project
    /// has (another may be chosen).
    pub(super) fn sync_agent_place(&self, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        let folder = view.read(cx).session.read(cx).folder().to_path_buf();
        let in_repo = self.store_for(&folder, cx).is_some_and(|store| store.read(cx).repo().is_some());
        let folders = self.folders.as_ref().map_or(1, |p| p.folders().len());
        view.update(cx, |v, cx| {
            if v.in_repo != in_repo || v.project_folders != folders {
                v.in_repo = in_repo;
                v.project_folders = folders;
                cx.notify();
            }
        });
    }

    /// A restored session tab (`TabKind::AgentSession`).
    pub(super) fn restore_agent_session(&mut self, tab: &crate::state::TabSession, cx: &mut Context<Self>) -> Option<PaneItem> {
        let id = tab.agent_session.as_deref()?;
        let store = store::store(cx)?;
        let session = store.update(cx, |s, cx| s.open(id, cx))?;
        Some(PaneItem::Agent(cx.new(|cx| SessionView::new(session, cx))))
    }

    /// Closing a session's tab: the last one closes the session (its agent
    /// stops; it stays saved), asking first while it works.
    pub(super) fn close_agent_tab(&mut self, view: &Entity<SessionView>, window: &mut Window, cx: &mut Context<Self>) {
        let session = view.read(cx).session.clone();
        let others = self.items(cx).into_iter().filter_map(|i| i.agent().cloned()).filter(|v| v != view && v.read(cx).session == session).count();
        let busy = session.read(cx).working();
        if others == 0 && busy {
            let answer = ask(window, "Stop the agent?", "It's still working; closing the tab stops it. The session stays in the Agents view.", "Stop and Close", true, cx);
            let view = view.clone();
            cx.spawn_in(window, async move |this, cx| {
                if answer.await {
                    this.update_in(cx, |ws, window, cx| ws.remove_agent_tab(&view, true, window, cx)).ok();
                }
            })
            .detach();
        } else {
            self.remove_agent_tab(view, others == 0, window, cx);
        }
    }

    /// Takes a session's tab away; `last`: the session closes too.
    fn remove_agent_tab(&mut self, view: &Entity<SessionView>, last: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_item(&PaneItem::Agent(view.clone()), window, cx);
        self.agent_tabs.remove(&view.entity_id());
        if last {
            let id = view.read(cx).session.read(cx).id.clone();
            self.agent_sessions.remove(&id);
            if let Some(store) = store::store(cx) {
                store.update(cx, |s, cx| s.close(&id, cx));
            }
        }
    }

    pub(super) fn on_agents_view(&mut self, event: &AgentsViewEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            AgentsViewEvent::NewSession(position) => self.open_new_session_menu(*position, window, cx),
            AgentsViewEvent::Start(agent) => {
                self.start_agent_session(agent.clone(), window, cx);
            }
            AgentsViewEvent::Open(id) => {
                let Some(store) = store::store(cx) else { return };
                match store.update(cx, |s, cx| s.open(id, cx)) {
                    Some(session) => {
                        self.open_session_view(session, window, cx);
                    }
                    None => self.toast(ToastKind::Error, "Couldn't open the session", Some("Its saved file is gone or unreadable.".into()), cx),
                }
            }
            AgentsViewEvent::Delete(id) => {
                let id = id.clone();
                let worktree = store::store(cx).and_then(|s| s.read(cx).summary_of(&id, cx)).and_then(|s| s.worktree).filter(|w| !w.removed);
                let detail = match &worktree {
                    Some(wt) => format!("Its conversation goes for good, and its worktree too (changes not committed in it are lost); its branch {} stays.", wt.branch),
                    None => "Its conversation goes for good; files it changed stay as they are.".to_string(),
                };
                let answer = ask(window, "Delete this session?", &detail, "Delete", false, cx);
                cx.spawn_in(window, async move |this, cx| {
                    if !answer.await {
                        return;
                    }
                    this.update_in(cx, |ws, window, cx| {
                        let views: Vec<Entity<SessionView>> = ws.items(cx).into_iter().filter_map(|i| i.agent().cloned()).filter(|v| v.read(cx).session.read(cx).id == id).collect();
                        for view in views {
                            ws.remove_item(&PaneItem::Agent(view.clone()), window, cx);
                            ws.agent_tabs.remove(&view.entity_id());
                        }
                        ws.agent_sessions.remove(&id);
                        if let Some(store) = store::store(cx) {
                            store.update(cx, |s, cx| s.delete(&id, cx));
                        }
                        if let Some(wt) = worktree.clone() {
                            cx.background_executor()
                                .spawn(async move {
                                    if let Err(err) = crate::agents::worktree::remove(&wt, false) {
                                        eprintln!("couldn't remove the worktree {}: {err}", wt.checkout.display());
                                    }
                                })
                                .detach();
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    /// New Session: Kuana's agent, the installed ones, then ones to install.
    pub(super) fn open_new_session_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = store::store(cx) else { return };
        let choices = store.read(cx).choices();
        let mut entries = vec![MenuEntry::Header("New Session With".into())];
        let mut actions: Vec<Option<MenuAction>> = vec![None];
        let start = |id: String| Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
            ws.start_agent_session(id.clone(), window, cx);
        })));
        // The agent New Agent Session starts (the one used last) shows its shortcut.
        let shortcut = binding_label(&NewAgentSession, window);
        for choice in choices.iter().filter(|c| c.installed) {
            let entry = MenuEntry::item(choice.name.clone(), Icon::Sparkle);
            entries.push(if choice.id == self.last_agent { entry.shortcut(shortcut.clone()) } else { entry });
            actions.push(start(choice.id.clone()));
        }
        // Agents being installed; the rest (the registry lists dozens) in a
        // picker that filters.
        for choice in choices.iter().filter(|c| c.installing) {
            entries.push(MenuEntry::item(format!("{} (installing…)", choice.name), Icon::Download));
            actions.push(None);
        }
        if choices.iter().any(|c| !c.installed && !c.installing) {
            entries.push(MenuEntry::item("Install an Agent…", Icon::Download));
            actions.push(Some(MenuAction::Call(std::rc::Rc::new(|ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                ws.pick_agent_to_install(window, cx);
            }))));
        }
        entries.push(MenuEntry::Separator);
        actions.push(None);
        entries.push(MenuEntry::item("AI Settings…", Icon::Settings));
        actions.push(Some(MenuAction::Call(std::rc::Rc::new(|_, _, cx: &mut Context<Workspace>| {
            cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Ai), cx));
        }))));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// The registry's agents not installed yet, to filter; the one chosen
    /// installs, then a session with it starts.
    fn pick_agent_to_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = store::store(cx) else { return };
        let choices: Vec<crate::picker::Choice> = store
            .read(cx)
            .choices()
            .into_iter()
            .filter(|c| !c.installed && !c.installing)
            .map(|c| crate::picker::Choice {
                id: c.id,
                label: c.name.into(),
                detail: Some(c.error.map_or(c.description, |error| format!("Last install failed: {error}")).into()).filter(|d: &SharedString| !d.is_empty()),
                icon: Some(Icon::Download.path().into()),
                file: None,
            })
            .collect();
        let picker = cx.new(|cx| {
            let mut picker = crate::picker::Picker::new(crate::picker::PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
            picker.set_choices("Install an Agent".into(), "Agent", choices, cx);
            picker.set_title_icon(Icon::Sparkle, cx);
            picker
        });
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &crate::picker::PickerEvent, window, cx| match event {
            crate::picker::PickerEvent::Chosen(id) => {
                let id = id.clone();
                ws.close_modal(window, cx);
                ws.start_agent_session(id, window, cx);
            }
            crate::picker::PickerEvent::Dismissed => ws.close_modal(window, cx),
            _ => {}
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    fn on_session_view(&mut self, view: &Entity<SessionView>, event: &SessionViewEvent, window: &mut Window, cx: &mut Context<Self>) {
        let session = view.read(cx).session.clone();
        match event {
            SessionViewEvent::OpenFile { path, line } => self.open_at(path.clone(), *line, None, window, cx),
            SessionViewEvent::OpenDiff { path, old } => {
                let text: Arc<str> = old.clone().unwrap_or_default().into();
                self.show_diff(DiffSource::Compare { path: path.clone(), with: Compared::Text { label: "Before the agent's edit".into(), text } }, window, cx);
            }
            SessionViewEvent::Menu { kind, position } => self.open_session_menu(view, kind.clone(), *position, window, cx),
            SessionViewEvent::Review => self.review_agent_changes(&session, window, cx),
            SessionViewEvent::KeepAll => session.update(cx, |s, cx| {
                s.checkpoints.clear();
                cx.notify();
            }),
            SessionViewEvent::RejectAll => self.reject_agent_changes(&session, window, cx),
            SessionViewEvent::RestoreTo(turn) => self.restore_agent_files(&session, *turn, window, cx),
            SessionViewEvent::Resend { turn, text, context } => self.resend_agent_message(view, &session, *turn, text.clone(), context.clone(), window, cx),
            SessionViewEvent::RunCommand(command) => {
                let cwd = session.read(cx).cwd.clone();
                let first: String = command.lines().next().unwrap_or_default().chars().take(40).collect();
                let target = RunTarget { name: format!("Run '{first}'"), command: command.clone(), cwd, row: None, rows: 0..0, debug: None, test: None };
                self.run_target(target, window, cx);
            }
        }
    }

    fn open_session_menu(&mut self, view: &Entity<SessionView>, kind: MenuKind, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let session = view.read(cx).session.clone();
        let mut entries = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        match kind {
            MenuKind::Mode => {
                let s = session.read(cx);
                // The mode option where the agent has one (ACP's successor
                // to modes), else its modes.
                let choices: Vec<(String, String, Option<String>, bool)> = match s.mode_option() {
                    Some(option) => {
                        let current = option.current.as_str().unwrap_or_default();
                        option.choices.iter().map(|c| (c.value.clone(), c.name.clone(), c.description.clone(), c.value == current)).collect()
                    }
                    None => s.modes.iter().map(|m| (m.id.clone(), m.name.clone(), m.description.clone(), s.mode.as_deref() == Some(m.id.as_str()))).collect(),
                };
                let option = s.mode_option().map(|o| o.id.clone());
                for (value, name, description, current) in choices {
                    let entry = if current { MenuEntry::item(name, Icon::Check) } else { MenuEntry::text(name) };
                    entries.push(entry.detail(description));
                    let (session, option) = (session.clone(), option.clone());
                    actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |_, _, cx: &mut Context<Workspace>| {
                        session.update(cx, |s, cx| match &option {
                            Some(option) => s.set_option(option, Value::String(value.clone()), cx),
                            None => s.set_mode(&value, cx),
                        })
                    }))));
                }
            }
            MenuKind::Model => {
                let s = session.read(cx);
                let mut options: Vec<crate::agents::session::ConfigOption> =
                    s.options.iter().filter(|o| o.category.as_deref() != Some("mode") && !o.choices.is_empty()).cloned().collect();
                // The model first, then its effort, then the rest as the agent lists them.
                options.sort_by_key(|o| match o.category.as_deref() {
                    Some("model") => 0,
                    Some("thought_level") => 1,
                    _ => 2,
                });
                let set = |session: &Entity<Session>, option: &str, value: String| -> Option<MenuAction> {
                    let (session, option) = (session.clone(), option.to_string());
                    Some(MenuAction::Call(std::rc::Rc::new(move |_, _, cx: &mut Context<Workspace>| {
                        session.update(cx, |s, cx| s.set_option(&option, Value::String(value.clone()), cx));
                    })))
                };
                let mut switches = Vec::new();
                for o in &options {
                    if let Some(on) = o.switch() {
                        switches.push((o.clone(), on));
                        continue;
                    }
                    let current = o.current.as_str().unwrap_or_default();
                    entries.push(MenuEntry::Header(o.name.clone().into()));
                    actions.push(None);
                    for choice in &o.choices {
                        let entry = if choice.value == current { MenuEntry::item(choice.name.clone(), Icon::Check) } else { MenuEntry::text(choice.name.clone()) };
                        // "Opus 5.5 · Best for…" under "Opus 5.5": the name once.
                        let detail = choice.description.as_deref().map(|d| d.strip_prefix(choice.name.as_str()).map_or(d, |rest| rest.trim_start_matches([' ', '·']))).filter(|d| !d.is_empty());
                        entries.push(entry.detail(detail.map(str::to_string)));
                        actions.push(set(&session, &o.id, choice.value.clone()));
                    }
                }
                if !switches.is_empty() && !entries.is_empty() {
                    entries.push(MenuEntry::Separator);
                    actions.push(None);
                }
                for (o, on) in switches {
                    let entry = if on { MenuEntry::item(o.name.clone(), Icon::Check) } else { MenuEntry::text(o.name.clone()) };
                    entries.push(entry.detail(o.description.clone()));
                    actions.push(o.switch_value(!on).and_then(|value| set(&session, &o.id, value)));
                }
            }
            MenuKind::AddContext => {
                let add = |what: fn(&mut Workspace, &Entity<SessionView>, &mut Context<Workspace>), view: Entity<SessionView>| {
                    Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| what(ws, &view, cx))))
                };
                entries.push(MenuEntry::item("Current File", Icon::FileText));
                actions.push(add(|ws, view, cx| ws.attach_current_file(view, cx), view.clone()));
                entries.push(MenuEntry::item("Selection", Icon::Type));
                actions.push(add(|ws, view, cx| ws.attach_selection(view, cx), view.clone()));
                entries.push(MenuEntry::item("Open Files", Icon::File));
                actions.push(add(|ws, view, cx| ws.attach_open_files(view, cx), view.clone()));
                entries.push(MenuEntry::item("Problems", Icon::CircleAlert));
                actions.push(add(|ws, view, cx| ws.attach_problems(view, cx), view.clone()));
                entries.push(MenuEntry::item("Uncommitted Changes (Git Diff)", Icon::GitCompare));
                actions.push(add(|ws, view, cx| ws.attach_git_diff(view, cx), view.clone()));
                entries.push(MenuEntry::item("Picture File…", Icon::Palette));
                actions.push(add(|ws, view, cx| ws.attach_pictures(view, cx), view.clone()));
            }
            MenuKind::Place => {
                let s = session.read(cx);
                let fresh = s.transcript.entries.is_empty();
                match (s.worktree.clone(), fresh) {
                    (worktree, true) => {
                        // The project's folders (it works in one; with several,
                        // another may be picked), then a worktree of its own.
                        let current = s.folder().to_path_buf();
                        let folders: Vec<PathBuf> = self.folders.as_ref().map(|p| p.folders().to_vec()).unwrap_or_else(|| vec![current.clone()]);
                        if folders.len() > 1 {
                            entries.push(MenuEntry::Header("Works in".into()));
                            actions.push(None);
                        }
                        for folder in folders {
                            let here = worktree.is_none() && folder == current;
                            let name = folder.file_name().map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned());
                            let entry = if here { MenuEntry::item(name, Icon::Check) } else { MenuEntry::text(name) };
                            let detail = if folder == current { "Its edits land in your working copy, as you'd make them.".to_string() } else { crate::agents::view::tilde(&folder) };
                            entries.push(entry.detail(Some(detail)));
                            let (session_a, view_a) = (session.clone(), view.clone());
                            actions.push((!here).then(|| MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| ws.agent_to_folder(&session_a, &view_a, folder.clone(), cx)))));
                        }
                        if self.store_for(&current, cx).is_some_and(|store| store.read(cx).repo().is_some()) {
                            let in_worktree = worktree.is_some();
                            let entry = if in_worktree { MenuEntry::item("New Worktree", Icon::Check) } else { MenuEntry::text("New Worktree") };
                            entries.push(entry.detail(Some("A checkout of its own on a new branch, from the last commit: its edits don't touch your files or other sessions'.".to_string())));
                            let (session_b, view_b) = (session.clone(), view.clone());
                            actions.push((!in_worktree).then(|| MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| ws.agent_to_worktree(&session_b, &view_b, cx)))));
                        }
                    }
                    (Some(wt), false) if !wt.removed => {
                        // The project's branch as Git last told (read when the status was).
                        let current = self.store_for(&wt.project, cx).and_then(|store| store.read(cx).status().branch.clone());
                        let call = |f: fn(&mut Workspace, &Entity<Session>, &mut Window, &mut Context<Workspace>), session: Entity<Session>| {
                            Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| f(ws, &session, window, cx))))
                        };
                        entries.push(MenuEntry::item("Open Worktree in New Window", Icon::ExternalLink).detail(Some("To run, test or look around in it.".to_string())));
                        actions.push(call(Workspace::open_agent_worktree, session.clone()));
                        entries.push(MenuEntry::item("Commit Changes on Its Branch", Icon::GitCommit).detail(Some(format!("Everything it changed, as one commit on {}.", wt.branch))));
                        actions.push(call(Workspace::commit_agent_worktree, session.clone()));
                        let into = current.clone().unwrap_or_else(|| "the current branch".into());
                        entries.push(MenuEntry::item(format!("Merge into {into}"), Icon::GitBranch).detail(Some("Commits what's left, then merges the branch into your project's.".to_string())));
                        actions.push(current.is_some().then(|| call(Workspace::merge_agent_worktree, session.clone())).flatten());
                        entries.push(MenuEntry::Separator);
                        actions.push(None);
                        entries.push(MenuEntry::item("Remove Worktree…", Icon::Trash));
                        actions.push(call(Workspace::remove_agent_worktree, session.clone()));
                    }
                    _ => {}
                }
            }
            MenuKind::More => {
                entries.push(MenuEntry::item("Copy as Markdown", Icon::Copy));
                let s = session.clone();
                actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |_, _, cx: &mut Context<Workspace>| {
                    let markdown = s.read(cx).transcript.markdown();
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(markdown));
                }))));
                if !session.read(cx).checkpoints.is_empty() {
                    entries.push(MenuEntry::item("Review Changes", Icon::GitCompare));
                    let s = session.clone();
                    actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| ws.review_agent_changes(&s, window, cx)))));
                }
                entries.push(MenuEntry::item("Show Agents", Icon::Sparkle).shortcut(binding_label(&ToggleAgents, window)));
                actions.push(Some(MenuAction::Run(Box::new(ToggleAgents))));
                entries.push(MenuEntry::Separator);
                actions.push(None);
                entries.push(MenuEntry::item("Delete Session", Icon::Trash));
                let id = session.read(cx).id.clone();
                actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                    ws.on_agents_view(&AgentsViewEvent::Delete(id.clone()), window, cx);
                }))));
            }
        }
        if !entries.is_empty() {
            self.show_context_menu(entries, actions, position, None, window, cx);
        }
    }

    // ---- What sessions ask -------------------------------------------------------------

    fn on_session_event(&mut self, session: &Entity<Session>, event: &SessionEvent, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(event, SessionEvent::Finished) {
            self.drop_unchanged_files(session, cx);
        }
        match event {
            SessionEvent::NeedsYou | SessionEvent::Finished => {
                let shown = self.active_item(cx).and_then(|i| i.agent().cloned()).is_some_and(|v| v.read(cx).session == *session) && window.is_window_active();
                if !shown {
                    let s = session.read(cx);
                    let failed = matches!(s.state, crate::agents::session::State::Failed(_));
                    let message = match event {
                        SessionEvent::NeedsYou => format!("{} needs your answer", s.agent_name),
                        _ if failed => format!("{} stopped", s.agent_name),
                        _ => format!("{} finished: {}", s.agent_name, s.title),
                    };
                    // Elsewhere (another window, another app): the system tells
                    // too, and a click comes back here (`show_notified_session`).
                    if !window.is_window_active() && Settings::get(cx).agent_notifications() {
                        let (title, body) = match event {
                            SessionEvent::NeedsYou => (format!("{} needs your answer", s.agent_name), s.open_question().unwrap_or_else(|| s.title.clone())),
                            _ if failed => (format!("{} stopped", s.agent_name), s.title.clone()),
                            _ => (format!("{} finished", s.agent_name), s.title.clone()),
                        };
                        kuana_os::notify::show(&title, &body, &s.id);
                        kuana_os::notify::request_attention(native_window(window));
                    }
                    self.toast_with_button(ToastKind::Info, message, None, Some(ToastButton::ShowAgents), cx);
                }
            }
            SessionEvent::TitleChanged => {}
            SessionEvent::ReadFile { id, path, line, limit } => {
                let result = self.agent_read(&resolve_agent_path(path), *line, *limit, cx);
                session.update(cx, |s, _| s.answer(*id, result));
            }
            // Where the write really lands decides whether it's in the project.
            SessionEvent::WriteFile { id, path, content } => self.agent_write(session, *id, resolve_agent_path(path), content.clone(), window, cx),
            SessionEvent::WillEdit { path, old, new } => self.agent_will_edit(session, resolve_agent_path(path), old.as_deref(), new.as_deref(), cx),
            SessionEvent::Ext { id, method, params } => {
                let result = match method.as_str() {
                    "_kuana/problems" => Ok(self.agent_problems(params.get("path").and_then(Value::as_str).map(PathBuf::from), cx)),
                    _ => Err(RpcError::new(RpcError::METHOD_NOT_FOUND, format!("Kuana doesn't serve {method}"))),
                };
                session.update(cx, |s, _| s.answer(*id, result));
            }
            SessionEvent::RunSignIn { program, args, env, cwd } => {
                let mut command = String::new();
                for (name, value) in env {
                    if cfg!(windows) {
                        command.push_str(&format!("$env:{name}={}; ", crate::run::shell_quote(value)));
                    } else {
                        command.push_str(&format!("{name}={} ", crate::run::shell_quote(value)));
                    }
                }
                // `chain` puts PowerShell's call operator before a quoted program.
                let argv: Vec<String> = std::iter::once(program.display().to_string()).chain(args.iter().cloned()).collect();
                command.push_str(&crate::run::chain(&[], &argv));
                let name = format!("Sign in to {}", session.read(cx).agent_name);
                let target = RunTarget { name, command, cwd: cwd.clone(), row: None, rows: 0..0, debug: None, test: None };
                self.run_target(target, window, cx);
                self.toast(ToastKind::Info, "Sign in in the terminal", Some("Then choose \"I've signed in\" in the session.".into()), cx);
            }
        }
    }

    /// A file as the editor has it (its open document, read from disk first
    /// for a restored tab, else the disk), lines `line` (1-based) on, at most
    /// `limit` of them.
    fn agent_read(&mut self, path: &Path, line: Option<u32>, limit: Option<u32>, cx: &mut Context<Self>) -> Result<Value, RpcError> {
        let text = match self.agent_document(path, cx) {
            Some(editor) => editor.read(cx).doc.read(cx).rope().to_string(),
            None => {
                let bytes = crate::fs::read(path).map_err(|e| RpcError::new(RpcError::NOT_FOUND, format!("{}: {e}", path.display())))?;
                if crate::encoding::looks_binary(&bytes) {
                    return Err(RpcError::internal(format!("{} isn't a text file", path.display())));
                }
                crate::encoding::decode(&bytes, None).0
            }
        };
        let text = match (line, limit) {
            (None, None) => text,
            _ => {
                let start = line.unwrap_or(1).max(1) as usize - 1;
                let count = limit.map_or(usize::MAX, |l| l as usize);
                text.split_inclusive('\n').skip(start).take(count).collect()
            }
        };
        Ok(json!({ "content": text }))
    }

    /// The tab showing `path`, read from its file if it was a restored tab
    /// that hadn't yet (so its unsaved text is what the agent sees and edits).
    fn agent_document(&mut self, path: &Path, cx: &mut Context<Self>) -> Option<Entity<Editor>> {
        let editor = self.tab_for(path, cx)?;
        self.load_tab_now(&editor, cx);
        editor.read(cx).is_loaded().then_some(editor)
    }

    /// What a file is before an agent writes it: its open document's text,
    /// else its bytes, else missing. Err when it's there but can't be read.
    fn agent_before(&mut self, path: &Path, cx: &mut Context<Self>) -> Result<Before, String> {
        if let Some(editor) = self.agent_document(path, cx) {
            return Ok(Before::Text(editor.read(cx).doc.read(cx).rope().to_string()));
        }
        match crate::fs::read(path) {
            Ok(bytes) => Ok(Before::Bytes(bytes.into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Before::Missing),
            Err(e) => Err(format!("Couldn't read {} before writing it: {e}", path.display())),
        }
    }

    /// Whether `path` was there before an agent wrote it, as far as the
    /// project's file index tells: it hears of a write only after the watcher
    /// does, so it lists a file that was there and not one just made. A path
    /// it doesn't cover counts as there (nothing says otherwise); a worktree
    /// session's file goes by the repository's copy.
    fn listed_before_agent(&self, session: &Entity<Session>, path: &Path, cx: &App) -> bool {
        let path = match &session.read(cx).worktree {
            Some(worktree) => worktree.unmap(path),
            None => path.to_path_buf(),
        };
        self.files.snapshot_of(&path).is_none_or(|folder| folder.lists(&path))
    }

    /// A turn ended: files noted for it that are as they were before it (an
    /// edit refused, or put back) aren't changes.
    fn drop_unchanged_files(&mut self, session: &Entity<Session>, cx: &mut Context<Self>) {
        let noted = session.read(cx).noted_this_turn();
        let same: Vec<PathBuf> = noted
            .into_iter()
            .filter(|(path, before)| self.agent_before(path, cx).is_ok_and(|now| now.text() == before.text()))
            .map(|(path, _)| path)
            .collect();
        if !same.is_empty() {
            session.update(cx, |s, cx| {
                for path in &same {
                    s.unchanged(path);
                }
                s.save(cx);
                cx.notify();
            });
        }
    }

    /// A file an agent is about to change (`SessionEvent::WillEdit`): what it
    /// is now is kept for Review, Reject All and Restore, and in Local
    /// History. Agents that write the disk themselves (Claude's) never ask
    /// Kuana to write, so this is the only time to keep it. Should the change
    /// have landed already, what the file was is worked out where that's
    /// certain (`landed_before`); else it's kept as it is.
    fn agent_will_edit(&mut self, session: &Entity<Session>, path: PathBuf, old: Option<&str>, new: Option<&str>, cx: &mut Context<Self>) {
        let in_worktree = session.read(cx).worktree.as_ref().is_some_and(|w| path.starts_with(&w.checkout));
        if !(self.in_project(&path) || in_worktree) || session.read(cx).noted(&path) {
            return;
        }
        let Ok(mut before) = self.agent_before(&path, cx) else { return };
        if let (Some(new), Some(text)) = (new, before.text())
            && let Some(was) = landed_before(&text, old, new, self.listed_before_agent(session, &path, cx))
        {
            before = was;
        }
        if let Some(text) = before.text() {
            crate::local_history::record(&path, &text, crate::local_history::Kind::BeforeAgent);
        }
        session.update(cx, |s, cx| {
            s.note_change(&path, before);
            cx.notify();
        });
    }

    /// Writes for an agent: through the file's tab when it has one (one undo
    /// step, then saved), else to the disk; outside the project only after
    /// asking. The text before goes to Local History and the session's log.
    fn agent_write(&mut self, session: &Entity<Session>, id: u64, path: PathBuf, content: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.agent_may_write(session, &path, cx) {
            let place = if session.read(cx).worktree.is_some() { "its worktree" } else { "the project" };
            let detail = format!("{} wants to write {}, outside {place}.", session.read(cx).agent_name, path.display());
            let answer = ask(window, &format!("Allow writing outside {place}?"), &detail, "Allow", true, cx);
            let session = session.clone();
            cx.spawn_in(window, async move |this, cx| {
                if answer.await {
                    this.update_in(cx, |ws, window, cx| ws.write_for_agent(&session, id, path, content, window, cx)).ok();
                } else {
                    session.update(cx, |s, _| s.answer(id, Err(RpcError::internal("The user didn't allow writing outside the project")))).ok();
                }
            })
            .detach();
            return;
        }
        self.write_for_agent(session, id, path, content, window, cx);
    }

    /// Where an agent writes without asking: the project, or for a session in
    /// a worktree of its own, that worktree (the project folder is outside it).
    fn agent_may_write(&self, session: &Entity<Session>, path: &Path, cx: &App) -> bool {
        match &session.read(cx).worktree {
            Some(wt) if !wt.removed => path.starts_with(&wt.checkout),
            _ => self.in_project(path),
        }
    }

    fn write_for_agent(&mut self, session: &Entity<Session>, id: u64, path: PathBuf, content: String, window: &mut Window, cx: &mut Context<Self>) {
        let before = match self.agent_before(&path, cx) {
            Ok(before) => before,
            Err(error) => {
                session.update(cx, |s, _| s.answer(id, Err(RpcError::internal(error))));
                return;
            }
        };
        if let Some(text) = before.text() {
            crate::local_history::record(&path, &text, crate::local_history::Kind::BeforeAgent);
        }
        session.update(cx, |s, cx| {
            s.note_change(&path, before);
            cx.notify();
        });
        if let Some(editor) = self.agent_document(&path, cx) {
            editor.update(cx, |e, cx| e.replace_text(&content, cx));
            let saving = self.save_editor_task(&editor, window, cx);
            let session = session.clone();
            cx.spawn(async move |_, cx| {
                let saved = saving.await;
                session.update(cx, |s, _| s.answer(id, saved.map(|()| json!({})).map_err(RpcError::internal))).ok();
            })
            .detach();
            return;
        }
        let written = path.parent().map_or(Ok(()), crate::fs::create_dir_all).and_then(|()| crate::fs::write(&path, content.as_bytes()));
        match written {
            Ok(()) => {
                self.files_written([path.clone()], cx);
                session.update(cx, |s, _| s.answer(id, Ok(json!({}))));
            }
            Err(e) => session.update(cx, |s, _| s.answer(id, Err(RpcError::internal(format!("Couldn't write {}: {e}", path.display()))))),
        }
    }

    /// The problems Kuana shows, for `_kuana/problems`.
    fn agent_problems(&self, only: Option<PathBuf>, cx: &App) -> Value {
        let files = self.claude_diagnostics(only, cx);
        let mut problems = Vec::new();
        for file in files.as_array().into_iter().flatten() {
            let path = file["uri"].as_str().map(crate::agent_ide::path_of).unwrap_or_default();
            for d in file["diagnostics"].as_array().into_iter().flatten() {
                problems.push(json!({
                    "path": path,
                    "line": d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
                    "severity": d["severity"].as_str().unwrap_or("Error").to_lowercase(),
                    "message": d["message"],
                }));
            }
        }
        json!({ "problems": problems })
    }

    // ---- Review --------------------------------------------------------------------------

    /// Opens each file the session changed as a diff against what it was.
    fn review_agent_changes(&mut self, session: &Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        let changes = session.read(cx).changes();
        if changes.is_empty() {
            self.toast(ToastKind::Info, "No changes to review", None, cx);
            return;
        }
        for (path, before) in changes.iter().rev() {
            let text: Arc<str> = before.text().unwrap_or_default().into();
            let label = if matches!(before, Before::Missing) { "New file" } else { "Before the agent" };
            self.show_diff(DiffSource::Compare { path: path.clone(), with: Compared::Text { label: label.into(), text } }, window, cx);
        }
    }

    /// Reject All: every changed file back as it was before the session.
    fn reject_agent_changes(&mut self, session: &Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_agent_files(session, 0, window, cx);
    }

    /// Puts the files the agent changed in `turn` and after back as they were
    /// before it (a file it created goes to the Trash), after asking; turn 0
    /// is the whole session (Reject All).
    pub(super) fn restore_agent_files(&mut self, session: &Entity<Session>, turn: usize, window: &mut Window, cx: &mut Context<Self>) {
        let files = session.read(cx).checkpoints.restore_to(turn);
        if files.is_empty() {
            return;
        }
        let detail = files.iter().map(|(path, _)| format!("• {}", self.display_rel(path))).collect::<Vec<_>>().join("\n");
        let count = format!("{} file{}", files.len(), if files.len() == 1 { "" } else { "s" });
        let title = if turn == 0 { format!("Undo the agent's changes to {count}?") } else { format!("Put {count} back as they were before this message?") };
        let answer = ask(window, &title, &detail, "Undo Changes", false, cx);
        let session = session.clone();
        cx.spawn_in(window, async move |this, cx| {
            if !answer.await {
                return;
            }
            this.update_in(cx, |ws, window, cx| {
                let failed = ws.put_back_files(&files, window, cx);
                session.update(cx, |s, cx| {
                    s.checkpoints.forget_from(turn);
                    let text = if turn > 0 { format!("Files put back as they were before message {turn}.") } else { format!("The agent's changes to {count} were undone.") };
                    s.transcript.notice(crate::agents::transcript::Level::Info, text);
                    s.save(cx);
                    cx.notify();
                });
                if failed.is_empty() {
                    ws.toast(ToastKind::Success, format!("Put back {count}"), Some("Local History keeps the agent's version.".into()), cx);
                } else {
                    ws.toast(ToastKind::Error, "Some files couldn't be put back", Some(failed.join("\n")), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// An earlier message, edited and sent: the conversation goes back to
    /// before it. When the agent changed files since, the user chooses
    /// whether they go back too (Cancel keeps editing).
    #[allow(clippy::too_many_arguments)]
    fn resend_agent_message(&mut self, view: &Entity<SessionView>, session: &Entity<Session>, turn: usize, text: String, context: Vec<ContextItem>, window: &mut Window, cx: &mut Context<Self>) {
        let files = session.read(cx).checkpoints.restore_to(turn);
        if files.is_empty() {
            session.update(cx, |s, cx| s.edit_and_send(turn, text, context, false, cx));
            return;
        }
        let detail = files.iter().map(|(path, _)| format!("• {}", self.display_rel(path))).collect::<Vec<_>>().join("\n");
        let count = format!("{} file{}", files.len(), if files.len() == 1 { "" } else { "s" });
        let message = format!("The agent changed {count} after that message. Put them back as they were?");
        let answer = window.prompt(gpui::PromptLevel::Info, &message, Some(&detail), &["Put Back and Send", "Keep Files and Send", "Cancel"], cx);
        let (view, session) = (view.clone(), session.clone());
        cx.spawn_in(window, async move |this, cx| {
            let choice = answer.await.ok();
            this.update_in(cx, |ws, window, cx| match choice {
                Some(0) => {
                    let failed = ws.put_back_files(&files, window, cx);
                    if !failed.is_empty() {
                        ws.toast(ToastKind::Error, "Some files couldn't be put back", Some(failed.join("\n")), cx);
                    }
                    session.update(cx, |s, cx| s.edit_and_send(turn, text, context, true, cx));
                }
                Some(1) => session.update(cx, |s, cx| s.edit_and_send(turn, text, context, false, cx)),
                _ => view.update(cx, |v, cx| v.resume_editing(turn, &text, context, cx)),
            })
            .ok();
        })
        .detach();
    }

    /// Files back as they were (Local History keeps what they are now); the
    /// ones that couldn't be, with why.
    fn put_back_files(&mut self, files: &[(PathBuf, Before)], window: &mut Window, cx: &mut Context<Self>) -> Vec<String> {
        let mut failed = Vec::new();
        for (path, before) in files {
            if let Ok(now) = self.agent_before(path, cx)
                && let Some(text) = now.text()
            {
                crate::local_history::record(path, &text, crate::local_history::Kind::BeforeRestore);
            }
            if let Err(e) = self.put_back(path, before, window, cx) {
                failed.push(format!("{}: {e}", self.display_rel(path)));
            }
            self.files_written([path.clone()], cx);
        }
        failed
    }

    /// One file back as it was: through its tab when it has one (one undo
    /// step, saved), else its bytes written as they were; a file that didn't
    /// exist goes to the Trash.
    fn put_back(&mut self, path: &Path, before: &Before, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if matches!(before, Before::Missing) {
            if !path.exists() {
                return Ok(());
            }
            // Every tab of it closes for good (document released, the
            // language server told), so nothing holds the old text.
            while let Some(editor) = self.tab_for(path, cx) {
                self.remove_tab(&editor, window, cx);
            }
            return crate::file_history::move_to_trash(path).map(|_| ()).map_err(|e| e.to_string());
        }
        if let Some(editor) = self.agent_document(path, cx) {
            let text = before.text().unwrap_or_default();
            editor.update(cx, |e, cx| e.replace_text(&text, cx));
            self.save_editor(&editor, window, cx);
            return Ok(());
        }
        let bytes: Vec<u8> = match before {
            Before::Bytes(bytes) => bytes.to_vec(),
            Before::Text(text) => text.clone().into_bytes(),
            Before::Missing => unreachable!("handled above"),
        };
        path.parent().map_or(Ok(()), crate::fs::create_dir_all).and_then(|()| crate::fs::write(path, &bytes)).map_err(|e| e.to_string())
    }

    // ---- Context and asking ----------------------------------------------------------------

    fn attach_current_file(&mut self, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        let Some(editor) = self.last_text_editor(cx) else {
            self.toast(ToastKind::Info, "No file is open", None, cx);
            return;
        };
        let e = editor.read(cx);
        let doc = e.doc.read(cx);
        let Some(path) = doc.path.clone() else { return };
        let item = ContextItem::File { path, text: doc.rope().to_string() };
        view.update(cx, |v, cx| v.add_context(item, cx));
    }

    fn attach_selection(&mut self, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        match self.last_text_editor(cx).and_then(|e| selection_context(&e, cx)) {
            Some(item) => view.update(cx, |v, cx| v.add_context(item, cx)),
            None => self.toast(ToastKind::Info, "Nothing is selected", None, cx),
        }
    }

    fn attach_open_files(&mut self, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        let items: Vec<ContextItem> = self
            .editors
            .iter()
            .filter_map(|e| {
                let doc = e.read(cx).doc.read(cx);
                let path = doc.path.clone().filter(|_| e.read(cx).is_loaded() && !doc.large)?;
                Some(ContextItem::File { path, text: doc.rope().to_string() })
            })
            .take(20)
            .collect();
        view.update(cx, |v, cx| {
            for item in items {
                v.add_context(item, cx);
            }
        });
    }

    fn attach_problems(&mut self, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        let problems = self.agent_problems(None, cx);
        let lines: Vec<String> = problems["problems"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| format!("{}:{}: {}: {}", self.display_rel(Path::new(p["path"].as_str().unwrap_or_default())), p["line"], p["severity"].as_str().unwrap_or_default(), p["message"].as_str().unwrap_or_default()))
            .collect();
        if lines.is_empty() {
            self.toast(ToastKind::Info, "No problems", None, cx);
            return;
        }
        let item = ContextItem::Text { label: format!("Problems ({})", lines.len()), text: lines.join("\n") };
        view.update(cx, |v, cx| v.add_context(item, cx));
    }

    // ---- Worktrees (`agents/worktree.rs`) ----------------------------------------------------

    /// New Worktree, before the first message: a checkout of its own on a new
    /// branch, made in the background; then its agent starts over there.
    fn agent_to_worktree(&mut self, session: &Entity<Session>, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        let (folder, name) = {
            let s = session.read(cx);
            (s.folder().to_path_buf(), crate::agents::worktree::name_for(&s.id))
        };
        view.update(cx, |v, cx| {
            v.place_busy = Some("Making a worktree…".into());
            cx.notify();
        });
        let made = cx.background_executor().spawn(async move { crate::agents::worktree::create(&folder, &name) });
        let (session, view) = (session.clone(), view.clone());
        cx.spawn(async move |this, cx| {
            let made = made.await;
            this.update(cx, |ws, cx| {
                view.update(cx, |v, cx| {
                    v.place_busy = None;
                    cx.notify();
                });
                let moved = made.and_then(|wt| {
                    let store = store::store(cx).ok_or("No agent store")?;
                    // Something was sent meanwhile: it stays where it is.
                    if !session.read(cx).transcript.entries.is_empty() {
                        let _ = crate::agents::worktree::remove(&wt, true);
                        return Ok(());
                    }
                    store.update(cx, |s, cx| s.move_session(&session, wt.path.clone(), Some(wt), cx))
                });
                if let Err(why) = moved {
                    ws.toast(ToastKind::Error, "Couldn't make a worktree", Some(why), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// A project folder picked, before the first message: an unused worktree
    /// goes (with its branch) and the agent starts over in that folder.
    fn agent_to_folder(&mut self, session: &Entity<Session>, view: &Entity<SessionView>, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(store) = store::store(cx) else { return };
        let worktree = session.read(cx).worktree.clone();
        if let Err(why) = store.update(cx, |s, cx| s.move_session(session, folder, None, cx)) {
            self.toast(ToastKind::Error, "Couldn't move the session", Some(why), cx);
            return;
        }
        view.update(cx, |v, cx| {
            v.place_busy = None;
            cx.notify();
        });
        self.sync_agent_place(view, cx);
        if let Some(wt) = worktree {
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = crate::agents::worktree::remove(&wt, true) {
                        eprintln!("couldn't remove the worktree {}: {err}", wt.checkout.display());
                    }
                })
                .detach();
        }
    }

    fn open_agent_worktree(&mut self, session: &Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(wt) = session.read(cx).worktree.clone() else { return };
        if !wt.path.exists() {
            self.toast(ToastKind::Info, "The worktree is gone", Some(wt.path.display().to_string()), cx);
            return;
        }
        self.open_project(wt.path, OpenIn::NewWindow, window, cx);
    }

    /// Commits everything the session changed in its worktree on its branch,
    /// the session's title as the message.
    fn commit_agent_worktree(&mut self, session: &Entity<Session>, _: &mut Window, cx: &mut Context<Self>) {
        let Some(wt) = session.read(cx).worktree.clone() else { return };
        let message = session.read(cx).title.clone();
        let committed = cx.background_executor().spawn(async move { crate::agents::worktree::commit_all(&wt, &message).map(|done| (done, wt.branch)) });
        cx.spawn(async move |this, cx| {
            let committed = committed.await;
            this.update(cx, |ws, cx| match committed {
                Ok((true, branch)) => ws.toast(ToastKind::Info, format!("Committed on {branch}"), None, cx),
                Ok((false, _)) => ws.toast(ToastKind::Info, "Nothing to commit", Some("The worktree has no changes.".into()), cx),
                Err(why) => ws.toast(ToastKind::Error, "Couldn't commit", Some(why), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Commits what's left in the worktree, then merges its branch into the
    /// project's current branch with Kuana's own merge (conflicts show in
    /// the Commit view as for any merge).
    fn merge_agent_worktree(&mut self, session: &Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(wt) = session.read(cx).worktree.clone() else { return };
        let message = session.read(cx).title.clone();
        let committed = cx.background_executor().spawn({
            let wt = wt.clone();
            async move { crate::agents::worktree::commit_all(&wt, &message) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let committed = committed.await;
            this.update_in(cx, |ws, window, cx| {
                if let Err(why) = committed {
                    ws.toast(ToastKind::Error, "Couldn't commit the worktree's changes", Some(why), cx);
                    return;
                }
                let Some(store) = ws.store_for(&wt.project, cx) else {
                    ws.toast(ToastKind::Error, "The project's repository isn't open", None, cx);
                    return;
                };
                ws.show_repo(store, cx);
                ws.run_git(GitOp::Merge(wt.branch.clone()), window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Removes the session's worktree, asking first; its branch goes too
    /// when it holds nothing the project's branch lacks.
    fn remove_agent_worktree(&mut self, session: &Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(wt) = session.read(cx).worktree.clone() else { return };
        let checked = cx.background_executor().spawn({
            let wt = wt.clone();
            async move { (crate::agents::worktree::has_changes(&wt), crate::agents::worktree::unmerged(&wt)) }
        });
        let session = session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (changes, unmerged) = checked.await;
            let detail = match (changes, unmerged) {
                (true, _) => format!("It has changes that aren't committed; they're lost. Its branch {} stays.", wt.branch),
                (false, true) => format!("Its branch {} has commits your branch doesn't, so it stays.", wt.branch),
                (false, false) => format!("Its branch {} has nothing new and goes too.", wt.branch),
            };
            let Ok(answer) = this.update_in(cx, |_, window, cx| ask(window, "Remove the session's worktree?", &detail, "Remove", false, cx)) else { return };
            if !answer.await {
                return;
            }
            let removed = cx.background_executor().spawn({
                let wt = wt.clone();
                async move { crate::agents::worktree::remove(&wt, !changes && !unmerged) }
            });
            let removed = removed.await;
            this.update(cx, |ws, cx| match removed {
                Ok(()) => {
                    session.update(cx, |s, cx| {
                        s.worktree_removed(cx);
                    });
                    ws.toast(ToastKind::Info, "Worktree removed", None, cx);
                }
                Err(why) => ws.toast(ToastKind::Error, "Couldn't remove the worktree", Some(why), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Files and folders dropped on a session (from Finder or the file tree):
    /// a file open in a tab as the tab has it (unsaved edits included), the
    /// rest read off the UI thread (`view::attachment`).
    pub(super) fn attach_paths(&mut self, view: &Entity<SessionView>, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let mut open = Vec::new();
        let mut rest = Vec::new();
        for path in paths {
            let tab = self.tab_for(&path, cx).filter(|e| e.read(cx).is_loaded() && crate::agents::session::image_mime(&path).is_none());
            match tab {
                Some(editor) => open.push(crate::agents::view::Attachment::Context(ContextItem::File { path, text: editor.read(cx).doc.read(cx).rope().to_string() })),
                None => rest.push(path),
            }
        }
        let read = cx.background_executor().spawn(async move { rest.iter().map(|path| crate::agents::view::attachment(path)).collect::<Vec<_>>() });
        let view = view.clone();
        cx.spawn_in(window, async move |_, cx| {
            open.extend(read.await);
            view.update_in(cx, |v, window, cx| v.add_attachments(open, window, cx)).ok();
        })
        .detach();
    }

    /// Picture File…: PNG, JPEG, GIF or WebP files chosen in the system's
    /// dialog, read off the UI thread.
    fn attach_pictures(&mut self, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        let view = view.clone();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else { return };
            let read = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| {
                            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            let mime = crate::agents::session::image_mime(&path).ok_or_else(|| format!("{name} isn't a PNG, JPEG, GIF or WebP picture"))?;
                            let data = std::fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
                            if data.len() > crate::agents::session::MAX_IMAGE_BYTES {
                                return Err(format!("{name} is over 5 MB"));
                            }
                            Ok(ContextItem::Image { name, mime: mime.into(), data: data.into() })
                        })
                        .collect::<Vec<Result<ContextItem, String>>>()
                })
                .await;
            view.update(cx, |v, cx| {
                v.notice = None;
                for item in read {
                    match item {
                        Ok(item) => v.add_context(item, cx),
                        Err(why) => v.notice = Some(why.into()),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn attach_git_diff(&mut self, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo(cx) else {
            self.toast(ToastKind::Info, "The project isn't in a Git repository", None, cx);
            return;
        };
        let view = view.clone();
        let diff = cx.background_executor().spawn(async move { crate::git::run(&repo, &["diff", "HEAD", "--no-color"]) });
        cx.spawn(async move |this, cx| {
            let diff = diff.await;
            this.update(cx, |ws, cx| match diff {
                Ok(text) if !text.trim().is_empty() => {
                    let text: String = text.chars().take(200_000).collect();
                    view.update(cx, |v, cx| v.add_context(ContextItem::Text { label: "Uncommitted changes".into(), text }, cx));
                }
                Ok(_) => ws.toast(ToastKind::Info, "No uncommitted changes", None, cx),
                Err(e) => ws.toast(ToastKind::Error, "Couldn't read the Git diff", Some(e), cx),
            })
            .ok();
        })
        .detach();
    }

    /// The text editor used last: the active tab, else the active pane's
    /// last editor (an agent tab may be in front).
    fn last_text_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.active_editor(cx).or_else(|| self.center.panes().iter().find_map(|p| p.read(cx).tabs().iter().rev().find_map(|t| t.editor().cloned())))
    }

    /// The agent tab to send to: the one in front, else one shown, else a
    /// new session with the agent used last.
    fn target_session_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<SessionView>> {
        if let Some(view) = self.active_item(cx).and_then(|i| i.agent().cloned()) {
            return Some(view);
        }
        let shown = self.center.panes().iter().filter_map(|p| p.read(cx).active_item().and_then(|i| i.agent().cloned())).next();
        match shown {
            Some(view) => Some(view),
            None => {
                let agent = self.last_agent.clone();
                self.start_agent_session(agent, window, cx)
            }
        }
    }

    /// Ask Agent: the selection (else the current file) goes to a session's
    /// composer, which gets the keyboard.
    pub(super) fn ask_agent(&mut self, _: &AskAgent, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.active_editor(cx);
        let item = editor.as_ref().and_then(|e| selection_context(e, cx)).or_else(|| {
            let e = editor.as_ref()?.read(cx);
            let doc = e.doc.read(cx);
            Some(ContextItem::File { path: doc.path.clone()?, text: doc.rope().to_string() })
        });
        let Some(view) = self.target_session_view(window, cx) else { return };
        if let Some(item) = item {
            view.update(cx, |v, cx| v.add_context(item, cx));
        }
        window.focus(&view.read(cx).composer().focus_handle(cx));
    }

    /// Fix with Agent: the problem at the caret, with the lines around it, as
    /// a prompt sent right away.
    pub(super) fn fix_with_agent(&mut self, _: &FixWithAgent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        let (prompt, context) = {
            let e = editor.read(cx);
            let doc = e.doc.read(cx);
            let Some(path) = doc.path.clone() else { return };
            let caret = e.primary_selection().head;
            let rope = doc.rope();
            let row = rope.byte_to_line(caret.min(rope.len_bytes()));
            let problem = doc
                .diagnostics
                .iter()
                .filter(|d| d.severity.is_problem())
                .min_by_key(|d| (rope.byte_to_line(d.range.start.min(rope.len_bytes())) as isize - row as isize).abs())
                .cloned();
            let Some(problem) = problem else {
                self.toast(ToastKind::Info, "No problem here to fix", None, cx);
                return;
            };
            let line = rope.byte_to_line(problem.range.start.min(rope.len_bytes()));
            let (first, last) = (line.saturating_sub(10), (line + 10).min(rope.len_lines().saturating_sub(1)));
            let text: String = (first..=last).map(|l| rope.line(l).to_string()).collect();
            let rel = self.display_rel(&path);
            let prompt = format!("Fix this problem in {rel} at line {}: {}", line + 1, problem.message);
            (prompt, ContextItem::Selection { path, lines: (first + 1, last + 1), text })
        };
        let Some(view) = self.target_session_view(window, cx) else { return };
        let session = view.read(cx).session.clone();
        session.update(cx, |s, cx| s.send(prompt, vec![context], cx));
        self.activate_item(&PaneItem::Agent(view.clone()), window, cx);
    }
}

/// The selection of an editor as context; None without one.
impl Workspace {
    /// Explain with Agent: the selection, else the definition around the
    /// caret, else the file, sent with a request to explain it.
    pub(super) fn explain_with_agent(&mut self, _: &ExplainWithAgent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        let item = selection_context(&editor, cx).or_else(|| {
            let (_, range) = editor.update(cx, |e, cx| e.symbol_at_caret(cx))?;
            let doc = editor.read(cx).doc.read(cx);
            let rope = doc.rope();
            // On character boundaries, whatever the outline says.
            let snap = |at: usize| rope.char_to_byte(rope.byte_to_char(at.min(rope.len_bytes())));
            let (start, end) = (snap(range.start), snap(range.end));
            Some(ContextItem::Selection {
                path: doc.path.clone()?,
                lines: (rope.byte_to_line(start) + 1, rope.byte_to_line(end) + 1),
                text: rope.byte_slice(start..end).to_string(),
            })
        });
        let item = item.or_else(|| {
            let doc = editor.read(cx).doc.read(cx);
            Some(ContextItem::File { path: doc.path.clone()?, text: doc.rope().to_string() })
        });
        let Some(item) = item else { return };
        let prompt = format!("Explain {}: what it does, how it works, and anything surprising or worth knowing about it.", item.label());
        let Some(view) = self.target_session_view(window, cx) else { return };
        let session = view.read(cx).session.clone();
        session.update(cx, |s, cx| s.send(prompt, vec![item], cx));
        self.activate_item(&PaneItem::Agent(view.clone()), window, cx);
    }
}

/// Where an agent's path really points: `.` and `..` resolved, and links
/// followed as far as the path exists, so `/proj/../../.ssh/x` or a link out
/// of the project isn't taken for a file in it.
fn resolve_agent_path(path: &Path) -> PathBuf {
    resolve_within(path, 0)
}

fn resolve_within(path: &Path, depth: usize) -> PathBuf {
    let mut plain = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                plain.pop();
            }
            std::path::Component::CurDir => {}
            other => plain.push(other),
        }
    }
    // The deepest part that's there (a link counts, even to nothing), with
    // its links resolved, then the rest.
    let mut existing = plain.as_path();
    let mut rest = Vec::new();
    while std::fs::symlink_metadata(existing).is_err() {
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else { return plain };
        rest.push(name.to_os_string());
        existing = parent;
    }
    // A link to something that isn't there: where it points decides (a write
    // through it lands there).
    if !existing.exists()
        && depth < 16
        && let Ok(target) = std::fs::read_link(existing)
    {
        let mut target = match existing.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
        target.extend(rest.iter().rev());
        return resolve_within(&target, depth + 1);
    }
    let mut resolved = crate::paths::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    resolved.extend(rest.iter().rev());
    resolved
}

/// What a file was before an agent's change that has landed already, where
/// that's certain: `text` is the file now, `old` and `new` the change's diff
/// (`old` None: the whole file written), `listed` whether the file was there
/// before (`listed_before_agent`). None: keep it as it is (the change hasn't
/// landed, or what it was can't be told).
fn landed_before(text: &str, old: Option<&str>, new: &str, listed: bool) -> Option<Before> {
    match old {
        // The old text gone and the new one there once: it took the old one's
        // place. A new text that's empty (a deletion) or there more than once
        // could have gone anywhere.
        Some(old) if !old.is_empty() && !text.contains(old) => {
            only_place(text, new).map(|at| Before::Text(format!("{}{old}{}", &text[..at], &text[at + new.len()..])))
        }
        // Written whole, and the project didn't have it: it's new.
        None if text == new && !listed => Some(Before::Missing),
        _ => None,
    }
}

/// Where `needle` is in `text` when it's there exactly once, overlapping
/// matches counted (`aa` is twice in `aaa`); never for an empty one.
fn only_place(text: &str, needle: &str) -> Option<usize> {
    let at = text.find(needle).filter(|_| !needle.is_empty())?;
    let next = at + text[at..].chars().next().map_or(1, char::len_utf8);
    text[next..].find(needle).is_none().then_some(at)
}

fn selection_context(editor: &Entity<Editor>, cx: &App) -> Option<ContextItem> {
    let e = editor.read(cx);
    let doc = e.doc.read(cx);
    let path = doc.path.clone()?;
    let range = e.primary_selection().range();
    if range.is_empty() {
        return None;
    }
    let rope = doc.rope();
    let (start, end) = (range.start.min(rope.len_bytes()), range.end.min(rope.len_bytes()));
    let text = rope.byte_slice(start..end).to_string();
    let first = rope.byte_to_line(start) + 1;
    let mut last = rope.byte_to_line(end) + 1;
    if last > first && rope.line_to_byte(last - 1) == end {
        last -= 1;
    }
    Some(ContextItem::Selection { path, lines: (first, last), text })
}

/// A click on an agent session's system notification: Kuana comes forward
/// with the session's window and the session in front.
pub fn show_notified_session(id: &str, cx: &mut App) {
    cx.activate(true);
    let session = store::store(cx).and_then(|store| store.read(cx).live().iter().find(|s| s.read(cx).id == id).cloned());
    let Some(session) = session else { return };
    for handle in cx.windows() {
        let Some(workspace) = handle.downcast::<Workspace>() else { continue };
        let shown = workspace
            .update(cx, |ws, window, cx| {
                if !ws.agent_sessions.contains_key(id) {
                    return false;
                }
                window.activate_window();
                ws.open_session_view(session.clone(), window, cx);
                true
            })
            .unwrap_or(false);
        if shown {
            return;
        }
    }
}

/// A dictation couldn't start or go on: why, in a toast in the window in front.
pub fn show_dictation_error(why: String, cx: &mut App) {
    let window = cx.active_window().or_else(|| cx.windows().into_iter().next());
    if let Some(workspace) = window.and_then(|w| w.downcast::<Workspace>()) {
        workspace.update(cx, |ws, _, cx| ws.toast(ToastKind::Error, "Dictation stopped", Some(why), cx)).ok();
    }
}

/// The window's native handle where the system wants one for attention
/// (Windows' `HWND`, whose taskbar button flashes).
fn native_window(window: &Window) -> Option<isize> {
    #[cfg(target_os = "windows")]
    {
        // Window's own `window_handle()` (a GPUI `AnyWindowHandle`) shadows the trait's.
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = HasWindowHandle::window_handle(window)
            && let RawWindowHandle::Win32(win32) = handle.as_raw()
        {
            return Some(win32.hwnd.get());
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        None
    }
}

#[cfg(test)]
mod tests {
    /// `..` and links out of a folder are resolved before anything checks
    /// whether a path is in the project.
    #[test]
    fn agent_paths_resolve_dots_and_links() {
        let root = crate::paths::canonicalize(&std::env::temp_dir()).unwrap().join(format!("kuana-agent-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj/src")).unwrap();
        let resolve = super::resolve_agent_path;
        assert_eq!(resolve(&root.join("proj/src/../../outside.txt")), root.join("outside.txt"));
        assert_eq!(resolve(&root.join("proj/./src/new/file.rs")), root.join("proj/src/new/file.rs"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&root, root.join("proj/link")).unwrap();
            assert_eq!(resolve(&root.join("proj/link/secret")), root.join("secret"));
            // A link to a file that isn't there yet: where a write would land.
            std::os::unix::fs::symlink(root.join("elsewhere/new.txt"), root.join("proj/dangling")).unwrap();
            assert_eq!(resolve(&root.join("proj/dangling")), root.join("elsewhere/new.txt"));
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// An edit noticed after it landed: what the file was is worked out only
    /// where that's certain, never guessed into the wrong place.
    #[test]
    fn a_landed_edit_is_undone_only_where_certain() {
        use super::{Before, landed_before};
        // The new text once: the old one goes back in its place.
        assert_eq!(landed_before("é fn two() {}\n", Some("one"), "two", true), Some(Before::Text("é fn one() {}\n".into())));
        // Not landed yet (the old text is there): kept as it is.
        assert_eq!(landed_before("let one = 1;\n", Some("one"), "two", true), None);
        // A deletion: where the text was can't be told (it used to go first).
        assert_eq!(landed_before("a\nc\n", Some("b\n"), "", true), None);
        // The new text more than once, overlapping too: which one is unknown.
        assert_eq!(landed_before("}\n}\n", Some("x }"), "}", true), None);
        assert_eq!(landed_before("aaa", Some("b"), "aa", true), None);
        // A whole file written: new only when the project didn't list it.
        assert_eq!(landed_before("hello\n", None, "hello\n", false), Some(Before::Missing));
        assert_eq!(landed_before("hello\n", None, "hello\n", true), None);
        assert_eq!(landed_before("before\n", None, "after\n", false), None);
    }
}

