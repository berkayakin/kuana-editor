//! The project session: tabs, panes and panels saved per project and brought back.

use super::*;

impl Workspace {
    // ---- Project session --------------------------------------------------------

    /// Writes the project's tabs, carets and unsaved text for next time.
    pub(super) fn save_session(&self, cx: &App) -> std::io::Result<()> {
        self.session_snapshot(cx).map_or(Ok(()), SessionSnapshot::write)
    }

    /// The session to save, with unsaved texts as rope clones: making them
    /// strings, serializing and writing can happen off the UI thread.
    pub(super) fn session_snapshot(&self, cx: &App) -> Option<SessionSnapshot> {
        let root = self.root.clone()?;
        let mut tabs = Vec::new();
        let mut unsaved = Vec::new();
        let mut active = 0;
        let mut pane_active = Vec::new();
        // Panes are numbered in tree order, as `PaneNode` reads them.
        let panes = self.center.panes();
        for (pane_ix, entity) in panes.iter().enumerate() {
            let pane = entity.read(cx);
            let mut active_in_pane = 0;
            let mut count = 0;
            for (ix, item) in pane.tabs().iter().enumerate() {
                let Some((mut session, text)) = item.read(cx).session(cx) else { continue };
                session.pane = pane_ix;
                session.pinned = pane.is_pinned(item.entity_id());
                if ix == pane.active_ix() {
                    active_in_pane = count;
                    if *entity == self.active_pane {
                        active = tabs.len();
                    }
                }
                count += 1;
                if let Some(text) = text {
                    unsaved.push((tabs.len(), text));
                }
                tabs.push(session);
            }
            pane_active.push(active_in_pane);
        }
        // Changed without a tab and not saved: they come back as tabs.
        let last_pane = panes.len() - 1;
        for doc in self.documents.read(cx).kept(cx) {
            let d = doc.read(cx);
            let encoding = (d.encoding != TextEncoding::UTF8).then(|| d.encoding.key());
            unsaved.push((tabs.len(), d.rope().clone()));
            tabs.push(TabSession { path: d.path.clone(), pane: last_pane, encoding, ..TabSession::default() });
        }
        let mut pane_active = pane_active.into_iter();
        let pane_layout = (panes.len() > 1).then(|| self.center.save(&mut |_| pane_active.next().unwrap_or(0)));
        let mut expanded: Vec<PathBuf> =
            self.tree.read(cx).expanded().iter().filter_map(|p| p.strip_prefix(&root).ok()).filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).collect();
        expanded.sort();
        let mut breakpoints: Vec<FileBreakpoints> = self
            .all_breakpoints(cx)
            .into_iter()
            .filter_map(|(path, breakpoints)| Some(FileBreakpoints { path: path.strip_prefix(&root).ok()?.to_path_buf(), breakpoints }))
            .collect();
        breakpoints.sort_by(|a, b| a.path.cmp(&b.path));
        let python_venv = crate::python::choice(&root).map(|c| c.to_saved());
        let terminals = Some(self.terminal_layout(cx));
        let last_run = self.last_run.clone();
        let session = ProjectSession {
            root,
            tabs,
            active,
            layout: None,
            panes: pane_layout,
            expanded,
            breakpoints,
            python_venv,
            terminals,
            last_run,
            other: Default::default(),
        };
        Some(SessionSnapshot { session, unsaved })
    }

    /// The terminal panel as the session keeps it.
    pub(super) fn terminal_layout(&self, cx: &App) -> TerminalLayout {
        self.terminals.read(cx).layout(self.shows(BottomPanel::Terminal), cx)
    }

    pub(super) fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        if self.root.is_none() {
            return;
        }
        self.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let Ok(Some(snapshot)) = this.update(cx, |ws, cx| ws.session_snapshot(cx)) else { return };
            if let Err(err) = cx.background_executor().spawn(async move { snapshot.write() }).await {
                eprintln!("couldn't save the session: {err}");
            }
        }));
    }

    /// Reopens the tabs the project had last time, with their unsaved edits.
    pub(super) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.root.as_deref().and_then(ProjectSession::load);
        // A session or state file that didn't read was kept aside: say where.
        for problem in crate::state::take_problems() {
            self.toast(ToastKind::Error, "Couldn't read a saved session", Some(problem), cx);
        }
        let Some(session) = session else {
            return;
        };
        // Tabs read their files when first shown (see `Editor::load`): the
        // current tab of each pane right away, the others when activated.
        let mut restored: Vec<(PathBuf, Entity<Editor>)> = Vec::new();
        let mut items = Vec::new();
        for tab in &session.tabs {
            items.push(match tab.kind {
                // Sessions from before images had their own tabs list them as text.
                TabKind::Image => self.restore_image(tab, cx),
                TabKind::Editor if tab.path.as_deref().is_some_and(crate::preview::is_image) => self.restore_image(tab, cx),
                TabKind::Editor => self.restore_editor(tab, &mut restored, cx),
                TabKind::MarkdownPreview => None,
            });
        }
        // Previews once their documents are there (a preview may be in a pane
        // before its file's).
        for (tab, item) in session.tabs.iter().zip(&mut items) {
            if tab.kind == TabKind::MarkdownPreview {
                *item = self.restore_preview(tab, &restored, cx);
            }
        }
        // The panes as saved (an older session's row or column reads as a
        // tree too). Without a tree, the panes the tabs name make a row.
        let (node, pane_of) = match session.pane_layout().filter(|node| node.pane_count() > 0) {
            // A tab numbered past the tree's panes goes to the last one.
            Some(node) => {
                let last = node.pane_count() - 1;
                let pane_of: Vec<usize> = session.tabs.iter().map(|t| t.pane.min(last)).collect();
                (node, pane_of)
            }
            None => {
                let mut numbers: Vec<usize> = session.tabs.iter().map(|t| t.pane).collect();
                numbers.sort_unstable();
                numbers.dedup();
                let pane_of = session.tabs.iter().map(|t| numbers.binary_search(&t.pane).unwrap_or(0)).collect();
                // Each pane's last tab is its current one, as they were opened.
                let pane = |_| PaneShare { share: 1. / numbers.len() as f32, node: PaneNode::Pane { active: usize::MAX } };
                let node = match numbers.len() {
                    0 | 1 => PaneNode::Pane { active: usize::MAX },
                    n => PaneNode::Row((0..n).map(pane).collect()),
                };
                (node, pane_of)
            }
        };
        let focused = items.get(session.active).cloned().flatten();
        let mut by_pane: Vec<Vec<(Option<PaneItem>, bool)>> = vec![Vec::new(); node.pane_count()];
        for ((tab, item), pane) in session.tabs.into_iter().zip(items).zip(pane_of) {
            if let Some(path) = tab.path.filter(|_| item.is_some() && tab.kind != TabKind::MarkdownPreview) {
                self.recent.retain(|p| p != &path);
                self.recent.insert(0, path);
            }
            by_pane[pane].push((item, tab.pinned));
        }
        self.recent.truncate(50);
        // Panes whose tabs didn't come back (their files are gone) are left
        // out, as closing them would.
        let before = self.center.panes();
        let tree = PaneTree::restore(&node, &mut |number, active| {
            let tabs = std::mem::take(&mut by_pane[number]);
            if tabs.iter().all(|(item, _)| item.is_none()) {
                return None;
            }
            let pane = self.new_pane(window, cx);
            self.active_pane = pane.clone();
            // The current tab, counted among the saved ones; else the closest
            // one before it that came back.
            let mut current = None;
            for (ix, (item, pinned)) in tabs.into_iter().enumerate() {
                let Some(item) = item else { continue };
                if ix <= active || current.is_none() {
                    current = Some(item.entity_id());
                }
                let at = self.insert_tab(item, pinned, window, cx);
                pane.update(cx, |p, cx| p.set_active(at, cx));
            }
            pane.update(cx, |p, cx| {
                if let Some(ix) = current.and_then(|id| p.position(id)) {
                    p.set_active(ix, cx);
                }
            });
            Some(pane)
        });
        if let Some(tree) = tree {
            for pane in before {
                self.pane_subscriptions.remove(&pane.entity_id());
            }
            self.center = tree;
            self.active_pane = self.center.panes()[0].clone();
        }
        if let Some(item) = focused {
            self.activate_item(&item, window, cx);
        }
        self.sync_previewed(cx);
        self.load_shown_tabs(cx);
    }

    /// A text tab from the session, not read yet; `restored` lists the files
    /// restored so far, so a file shown twice is one document.
    fn restore_editor(&mut self, tab: &TabSession, restored: &mut Vec<(PathBuf, Entity<Editor>)>, cx: &mut Context<Self>) -> Option<PaneItem> {
        // The file is gone and there's nothing unsaved to keep.
        if tab.unsaved.is_none() && tab.path.as_deref().is_none_or(|p| !p.is_file()) {
            return None;
        }
        let encoding = tab.encoding.as_deref().and_then(TextEncoding::from_key);
        // Text the file has by now isn't unsaved (auto-save wrote it after
        // the session was written).
        let unsaved = tab.unsaved.as_ref().filter(|text| tab.path.as_deref().is_none_or(|p| !Document::file_has(p, text, encoding)));
        let unloaded = Unloaded {
            cursor: tab.cursor,
            top_line: tab.top_line,
            unsaved: unsaved.map(|text| ropey::Rope::from_str(text)),
            folds: tab.folds.clone(),
            encoding,
        };
        // A file shown in two panes is one document, in two editors.
        let shown = tab.path.as_ref().and_then(|p| restored.iter().find(|(q, _)| q == p)).map(|(_, e)| e.clone());
        let editor = match shown {
            Some(first) => cx.new(|cx| {
                let mut split = Editor::view_of(&first, cx);
                split.set_waiting_view(tab.cursor, tab.top_line);
                split
            }),
            None => cx.new(|cx| Editor::new_unloaded(tab.path.clone(), unloaded, cx)),
        };
        if let Some(path) = &tab.path {
            restored.push((path.clone(), editor.clone()));
        }
        Some(editor.into())
    }

    fn restore_image(&mut self, tab: &TabSession, cx: &mut Context<Self>) -> Option<PaneItem> {
        let path = tab.path.clone().filter(|p| p.is_file())?;
        Some(cx.new(|cx| ImageView::new(path, cx)).into())
    }

    /// A preview follows a document a text tab has open (one restored before
    /// it); without one, it isn't brought back.
    fn restore_preview(&mut self, tab: &TabSession, restored: &[(PathBuf, Entity<Editor>)], cx: &mut Context<Self>) -> Option<PaneItem> {
        let path = tab.path.as_ref()?;
        let doc = restored.iter().find(|(p, _)| p == path)?.1.read(cx).doc.clone();
        Some(cx.new(|cx| MarkdownPreview::new(doc, cx)).into())
    }

    /// Takes a pane out (never the last one): the pane beside it takes its
    /// space (see `PaneTree::remove`), and the keyboard if it had it.
    pub(super) fn remove_pane(&mut self, pane: &Entity<Pane>) {
        let Some(heir) = self.center.remove(pane) else { return };
        self.pane_subscriptions.remove(&pane.entity_id());
        if self.active_pane == *pane {
            self.active_pane = heir;
        }
    }

    /// Changes the recent projects list shared by all windows.
    pub(super) fn update_recent_projects(&mut self, f: impl FnOnce(&mut Vec<PathBuf>), cx: &mut App) {
        self.recent_projects = update_recent_projects(f, cx);
    }

    /// Remembers layout and the open folder for the next launch: at once
    /// for what reads it (`SessionState::load`), on disk a moment later, in
    /// the background (panel toggles, pane changes and resizes come in
    /// bursts, and a write waits for the disk), and when Kuana quits.
    pub(super) fn save_state(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let (sidebar_width, problems_height) = (f32::from(self.sidebar_width), f32::from(self.problems_height));
        let show_sidebar = self.show_sidebar;
        let (show_problems, show_terminal, show_git_log) =
            (self.shows(BottomPanel::Problems), self.shows(BottomPanel::Terminal), self.shows(BottomPanel::GitLog));
        let sidebar_mode = match self.sidebar_mode {
            SidebarMode::Files => "files",
            SidebarMode::Commit => "commit",
        };
        // What this window shows; what's shared with other windows (zoom,
        // recent projects, placement) stays as the others left it.
        SessionState::change_soon("layout", cx.entity_id().as_u64(), move |state| {
            // Opening a lone file shouldn't forget the remembered project,
            // and the sidebar's visibility is remembered for projects only
            // (it's always closed without one).
            if let Some(root) = &root {
                state.last_root = Some(root.clone());
                state.show_sidebar = Some(show_sidebar);
            }
            state.sidebar_width = Some(sidebar_width);
            state.problems_height = Some(problems_height);
            state.show_problems = Some(show_problems);
            state.show_terminal = Some(show_terminal);
            state.sidebar_mode = Some(sidebar_mode.into());
            state.show_git_log = Some(show_git_log);
        });
        self.state_task = Some(crate::state::write_soon(cx));
    }

    /// Saves where the window is (after moves and resizes settle), so the
    /// next launch opens it there.
    pub(super) fn remember_placement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placement = placement_of(window, cx);
        SessionState::change_soon("window", 0, move |state| state.window = Some(placement.clone()));
        self.state_task = Some(crate::state::write_soon(cx));
    }

    /// Shows Welcome Setup (theme, keymap, editor basics) over the window.
    pub fn show_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // At startup the restored project has just asked.
        if matches!(self.modals.current, Some(Modal::Trust(_))) {
            self.trust_waits = true;
        }
        self.close_modal(window, cx);
        let onboarding = cx.new(Onboarding::new);
        let subscription = cx.subscribe_in(&onboarding, window, |ws, _, event: &OnboardingEvent, window, cx| match event {
            OnboardingEvent::Done => ws.finish_onboarding(window, cx),
            OnboardingEvent::OpenSettings => {
                ws.finish_onboarding(window, cx);
                ws.open_settings(&OpenSettings, window, cx);
            }
            OnboardingEvent::BrowseThemes => ws.select_theme(&SelectTheme, window, cx),
            OnboardingEvent::Problems(problems) => ws.report_config_problems(problems.clone(), cx),
        });
        window.focus(&onboarding.focus_handle(cx));
        self.onboarding = Some((onboarding, subscription));
        cx.notify();
    }

    pub(super) fn welcome_setup(&mut self, _: &WelcomeSetup, window: &mut Window, cx: &mut Context<Self>) {
        self.show_onboarding(window, cx);
    }

    pub(super) fn finish_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.onboarding = None;
        SessionState::set_onboarded();
        self.focus_active(window, cx);
        self.ask_waiting_trust(window, cx);
        cx.notify();
    }
}

/// Changes the recent projects list shared by all windows and returns it.
/// It's re-read from disk first so one window doesn't drop another's
/// entries, and shown where the system lists recent items (the Dock, the
/// jump list: `cli::show_recent_projects`).
pub fn update_recent_projects(f: impl FnOnce(&mut Vec<PathBuf>), cx: &mut App) -> Vec<PathBuf> {
    let before = SessionState::load().recent_projects;
    let mut projects = before.clone();
    f(&mut projects);
    projects.truncate(MAX_RECENT_PROJECTS);
    // What the user removed from the jump list goes from here too.
    let removed = crate::cli::show_recent_projects(&projects, cx);
    projects.retain(|p| !removed.contains(p));
    if projects != before {
        let list = projects.clone();
        SessionState::change_soon("recent_projects", 0, move |state| state.recent_projects = list.clone());
        crate::state::write_soon(cx).detach();
    }
    projects
}


/// A session to write, its unsaved texts still ropes (see `session_snapshot`).
pub(super) struct SessionSnapshot {
    pub(super) session: ProjectSession,
    /// Tabs with unsaved edits: index in `session.tabs` and the text.
    pub(super) unsaved: Vec<(usize, ropey::Rope)>,
}


impl SessionSnapshot {
    pub(super) fn write(mut self) -> std::io::Result<()> {
        for (ix, text) in self.unsaved {
            self.session.tabs[ix].unsaved = Some(text.to_string());
        }
        self.session.save()
    }
}
