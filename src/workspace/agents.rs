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
        let sub = cx.subscribe_in(view, window, |ws, view, event: &SessionViewEvent, window, cx| ws.on_session_view(view, event, window, cx));
        self.agent_tabs.insert(view.entity_id(), vec![sub]);
        let session = view.read(cx).session.clone();
        let id = session.read(cx).id.clone();
        if let std::collections::hash_map::Entry::Vacant(slot) = self.agent_sessions.entry(id) {
            slot.insert(cx.subscribe_in(&session, window, |ws, session, event: &SessionEvent, window, cx| ws.on_session_event(session, event, window, cx)));
        }
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
        let busy = session.read(cx).state.busy();
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
                let answer = ask(window, "Delete this session?", "Its conversation goes for good; files it changed stay as they are.", "Delete", false, cx);
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
        for choice in choices.iter().filter(|c| c.installed) {
            let icon = if choice.id == self.last_agent { Icon::Check } else { Icon::Sparkle };
            entries.push(MenuEntry::item(choice.name.clone(), icon));
            actions.push(start(choice.id.clone()));
        }
        let to_install: Vec<_> = choices.iter().filter(|c| !c.installed).collect();
        if !to_install.is_empty() {
            entries.push(MenuEntry::Separator);
            actions.push(None);
            entries.push(MenuEntry::Header("Install".into()));
            actions.push(None);
            for choice in to_install {
                let label = if choice.installing { format!("{} (installing…)", choice.name) } else { choice.name.clone() };
                entries.push(MenuEntry::item(label, Icon::Download));
                actions.push(start(choice.id.clone()));
            }
        }
        entries.push(MenuEntry::Separator);
        actions.push(None);
        entries.push(MenuEntry::item("AI Settings…", Icon::Settings));
        actions.push(Some(MenuAction::Call(std::rc::Rc::new(|_, _, cx: &mut Context<Workspace>| {
            cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Ai), cx));
        }))));
        self.show_context_menu(entries, actions, position, None, window, cx);
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
                for mode in s.modes.clone() {
                    let current = s.mode.as_deref() == Some(mode.id.as_str());
                    entries.push(if current { MenuEntry::item(mode.name.clone(), Icon::Check) } else { MenuEntry::text(mode.name.clone()) });
                    let (session, id) = (session.clone(), mode.id.clone());
                    actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |_, _, cx: &mut Context<Workspace>| session.update(cx, |s, cx| s.set_mode(&id, cx))))));
                }
            }
            MenuKind::Option(option) => {
                let s = session.read(cx);
                let Some(o) = s.options.iter().find(|o| o.id == option).cloned() else { return };
                entries.push(MenuEntry::Header(o.name.clone().into()));
                actions.push(None);
                for (value, name) in o.choices {
                    let current = o.current.as_str() == Some(value.as_str());
                    entries.push(if current { MenuEntry::item(name, Icon::Check) } else { MenuEntry::text(name) });
                    let (session, id) = (session.clone(), option.clone());
                    actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |_, _, cx: &mut Context<Workspace>| {
                        session.update(cx, |s, cx| s.set_option(&id, Value::String(value.clone()), cx));
                    }))));
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
        match event {
            SessionEvent::NeedsYou | SessionEvent::Finished => {
                let shown = self.active_item(cx).and_then(|i| i.agent().cloned()).is_some_and(|v| v.read(cx).session == *session) && window.is_window_active();
                if !shown {
                    let s = session.read(cx);
                    let message = match event {
                        SessionEvent::NeedsYou => format!("{} needs your answer", s.agent_name),
                        _ if matches!(s.state, crate::agents::session::State::Failed(_)) => format!("{} stopped", s.agent_name),
                        _ => format!("{} finished: {}", s.agent_name, s.title),
                    };
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
                let quoted: Vec<String> = std::iter::once(program.display().to_string()).chain(args.iter().cloned()).map(|a| crate::run::shell_quote(&a)).collect();
                command.push_str(&quoted.join(" "));
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
        self.load_tab(&editor, cx);
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

    /// Writes for an agent: through the file's tab when it has one (one undo
    /// step, then saved), else to the disk; outside the project only after
    /// asking. The text before goes to Local History and the session's log.
    fn agent_write(&mut self, session: &Entity<Session>, id: u64, path: PathBuf, content: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_project(&path) {
            let detail = format!("{} wants to write {}, outside the project.", session.read(cx).agent_name, path.display());
            let answer = ask(window, "Allow writing outside the project?", &detail, "Allow", true, cx);
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
                let mut failed = Vec::new();
                for (path, before) in &files {
                    if let Ok(now) = ws.agent_before(path, cx)
                        && let Some(text) = now.text()
                    {
                        crate::local_history::record(path, &text, crate::local_history::Kind::BeforeRestore);
                    }
                    if let Err(e) = ws.put_back(path, before, window, cx) {
                        failed.push(format!("{}: {e}", ws.display_rel(path)));
                    }
                    ws.files_written([path.clone()], cx);
                }
                session.update(cx, |s, cx| {
                    s.checkpoints.forget_from(turn);
                    if turn > 0 {
                        s.transcript.notice(crate::agents::transcript::Level::Info, format!("Files put back as they were before message {turn}."));
                    }
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

    /// One file back as it was: through its tab when it has one (one undo
    /// step, saved), else its bytes written as they were; a file that didn't
    /// exist goes to the Trash.
    fn put_back(&mut self, path: &Path, before: &Before, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if matches!(before, Before::Missing) {
            if !path.exists() {
                return Ok(());
            }
            if let Some(editor) = self.tab_for(path, cx) {
                self.remove_item(&PaneItem::Editor(editor), window, cx);
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
            let (start, end) = (range.start.min(rope.len_bytes()), range.end.min(rope.len_bytes()));
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
    // The deepest part that exists, with its links resolved, then the rest.
    let mut existing = plain.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else { return plain };
        rest.push(name.to_os_string());
        existing = parent;
    }
    let mut resolved = crate::paths::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    resolved.extend(rest.iter().rev());
    resolved
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
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
