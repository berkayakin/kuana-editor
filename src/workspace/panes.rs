//! Panes and tabs: splitting, moving, pinning, closing.

use super::*;

impl Workspace {
    // ---- Panes and tab management ---------------------------------------------------------

    pub(super) fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(Side::Right, window, cx);
    }

    pub(super) fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split(Side::Down, window, cx);
    }

    /// A new pane beside the current one, on `side`, showing the current tab
    /// too (for a file, a second editor of the same document, with its own
    /// carets and scroll; see `PaneItem::split`). Splitting the other way
    /// than the pane's neighbors divides the pane itself, so panes make
    /// grids.
    pub(super) fn split(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.active_pane.clone();
        if !can_split(source.read(cx).bounds(), side) {
            self.toast(ToastKind::Info, "No room to split this pane", None, cx);
            return;
        }
        let shown = self.active_item(cx);
        let pane = self.new_pane(window, cx);
        self.center.split(&source, pane.clone(), side);
        self.active_pane = pane;
        match shown {
            Some(item) => {
                self.load_item(&item, cx);
                let split = item.split(cx);
                self.add_tab(split, window, cx);
            }
            None => window.focus(&self.focus_handle),
        }
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Moves a tab to `to` (before index `before`, or at the end); its pin
    /// goes with it.
    pub(super) fn move_tab(&mut self, item: &PaneItem, to: &Entity<Pane>, before: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((from, ix)) = self.locate(item.entity_id(), cx) else { return };
        let before = before.map(|b| if from == *to && b > ix { b - 1 } else { b });
        let Some(pinned) = from.update(cx, |p, cx| p.remove(item.entity_id(), cx)) else { return };
        to.update(cx, |p, cx| {
            let at = before.unwrap_or(p.tabs().len());
            p.insert(item.clone(), at, pinned, cx);
        });
        // The source pane closes if that was its last tab.
        if from != *to && from.read(cx).is_empty() {
            self.remove_pane(&from);
        }
        self.activate_item(item, window, cx);
        self.schedule_session_save(cx);
    }

    /// Moves a tab to a new pane beside `beside`, on `side` (a tab dropped
    /// near a pane's edge).
    pub(super) fn move_to_new_pane(&mut self, item: &PaneItem, beside: &Entity<Pane>, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.new_pane(window, cx);
        if !self.center.split(beside, pane.clone(), side) {
            self.pane_subscriptions.remove(&pane.entity_id());
            return;
        }
        self.move_tab(item, &pane, None, window, cx);
        cx.notify();
    }

    pub(super) fn next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        self.step_pane(true, window, cx);
    }

    /// The pane after the current one in tree order (left to right, top to
    /// bottom), or before it; around at the ends.
    fn step_pane(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let panes = self.center.panes();
        if panes.len() < 2 {
            return;
        }
        let at = panes.iter().position(|p| *p == self.active_pane).unwrap_or(0);
        let to = if forward { (at + 1) % panes.len() } else { (at + panes.len() - 1) % panes.len() };
        self.focus_pane(&panes[to], window, cx);
    }

    /// Makes `pane` the current one, with the keyboard in its current tab
    /// (in the workspace when it has none).
    fn focus_pane(&mut self, pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        self.active_pane = pane.clone();
        match pane.read(cx).active_item().cloned() {
            Some(item) => self.activate_item(&item, window, cx),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    pub(super) fn toggle_pin_tab(&mut self, _: &TogglePinTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.active_item(cx) {
            self.run_tab_action(TabAction::TogglePin, &item, window, cx);
        }
    }

    pub(super) fn close_other_tabs(&mut self, _: &CloseOtherTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.active_item(cx) {
            self.run_tab_action(TabAction::CloseOthers, &item, window, cx);
        }
    }

    pub(super) fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.active_item(cx) {
            self.run_tab_action(TabAction::CloseAll, &item, window, cx);
        }
    }

    pub(super) fn run_tab_action(&mut self, action: TabAction, item: &PaneItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane, ix)) = self.locate(item.entity_id(), cx) else { return };
        let others = |keep: &dyn Fn(usize, &PaneItem) -> bool| -> Vec<PaneItem> {
            let pane = pane.read(cx);
            pane.tabs()
                .iter()
                .enumerate()
                .filter(|(i, t)| !keep(*i, t) && !pane.is_pinned(t.entity_id()))
                .map(|(_, t)| t.clone())
                .collect()
        };
        let to_close = match action {
            TabAction::Close => vec![item.clone()],
            TabAction::CloseOthers => others(&|_, t| t == item),
            TabAction::CloseAll => others(&|_, _| false),
            TabAction::CloseToTheRight => others(&|i, _| i <= ix),
            TabAction::TogglePin => {
                pane.update(cx, |p, cx| p.toggle_pin(item.entity_id(), cx));
                self.schedule_session_save(cx);
                return;
            }
            TabAction::SelectInTree => {
                self.activate_item(item, window, cx);
                self.select_opened_file(&SelectOpenedFile, window, cx);
                return;
            }
            TabAction::Activate => {
                self.activate_item(item, window, cx);
                return;
            }
            TabAction::SplitRight | TabAction::SplitDown => {
                self.activate_item(item, window, cx);
                self.split(if action == TabAction::SplitRight { Side::Right } else { Side::Down }, window, cx);
                return;
            }
        };
        for item in to_close {
            self.close_item(&item, window, cx);
        }
    }

    pub(super) fn open_tab_menu(&mut self, item: PaneItem, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let pinned = self.locate(item.entity_id(), cx).is_some_and(|(pane, _)| pane.read(cx).is_pinned(item.entity_id()));
        let path = item.path(cx);
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        let tab = |action: TabAction| Some(MenuAction::Tab(action, item.clone()));
        add(MenuEntry::item("Close", Icon::X).shortcut(binding_label(&CloseTab, window)), tab(TabAction::Close));
        add(MenuEntry::text("Close Others"), tab(TabAction::CloseOthers));
        add(MenuEntry::text("Close All"), tab(TabAction::CloseAll));
        add(MenuEntry::text("Close Tabs to the Right"), tab(TabAction::CloseToTheRight));
        add(MenuEntry::Separator, None);
        add(MenuEntry::item(if pinned { "Unpin Tab" } else { "Pin Tab" }, Icon::Pin), tab(TabAction::TogglePin));
        add(MenuEntry::Separator, None);
        add(
            MenuEntry::item("Split Right", Icon::SplitRight).shortcut(binding_label(&SplitRight, window)),
            tab(TabAction::SplitRight),
        );
        add(
            MenuEntry::item("Split Down", Icon::SplitDown).shortcut(binding_label(&SplitDown, window)),
            tab(TabAction::SplitDown),
        );
        if let Some(path) = path {
            add(MenuEntry::Separator, None);
            if self.root.as_deref().is_some_and(|r| path.starts_with(r)) {
                add(
                    MenuEntry::item("Select in File Tree", Icon::FolderTree).shortcut(binding_label(&SelectOpenedFile, window)),
                    tab(TabAction::SelectInTree),
                );
            }
            add(MenuEntry::item("Copy Path", Icon::Clipboard), Some(MenuAction::Tree(TreeAction::CopyPath(path.clone()))));
            add(MenuEntry::item(reveal_label(), Icon::FolderOpen), Some(MenuAction::Tree(TreeAction::Reveal(path))));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// ⌘K V: a Markdown file's preview, beside it (see `toggle_preview_of`);
    /// in a preview, closes it.
    pub(super) fn toggle_markdown_preview(&mut self, _: &ToggleMarkdownPreview, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_item(cx) {
            Some(PaneItem::Editor(editor)) => self.toggle_preview_of(&editor, window, cx),
            Some(preview @ PaneItem::Preview(_)) => self.close_item(&preview, window, cx),
            _ => {}
        }
    }

    /// Shows the preview of `editor`'s Markdown document in the pane after
    /// the editor's (a new split beside it if there's none), leaving the
    /// keyboard in the editor; one showing already closes instead.
    pub(super) fn toggle_preview_of(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if editor.read(cx).doc.read(cx).language != Language::Markdown {
            return;
        }
        let Some((editor_pane, _)) = self.locate(editor.entity_id(), cx) else { return };
        let previews = self.previews_of(&editor.read(cx).doc.clone(), cx);
        let shown = |ws: &Self, item: &PaneItem, cx: &App| ws.center.panes().iter().any(|p| p.read(cx).active_item() == Some(item));
        if previews.iter().any(|p| shown(self, p, cx)) {
            for preview in previews {
                self.remove_item(&preview, window, cx);
            }
            if let Some((pane, ix)) = self.locate(editor.entity_id(), cx) {
                self.activate_in(&pane, ix, window, cx);
            }
            return;
        }
        // Open in another pane, behind another tab: brought to the front (one
        // behind the editor itself couldn't show beside it).
        let found = previews.iter().filter_map(|p| self.locate(p.entity_id(), cx)).find(|(pane, _)| *pane != editor_pane);
        let (pane, ix) = match found {
            Some(found) => found,
            None => {
                // The pane after the editor's, else a new one to its right,
                // else the one before (else the editor's own: no room).
                let panes = self.center.panes();
                let at = panes.iter().position(|p| *p == editor_pane).unwrap_or(0);
                let pane = if let Some(next) = panes.get(at + 1) {
                    next.clone()
                } else if can_split(editor_pane.read(cx).bounds(), Side::Right) {
                    let pane = self.new_pane(window, cx);
                    self.center.split(&editor_pane, pane.clone(), Side::Right);
                    pane
                } else {
                    panes[at.saturating_sub(1)].clone()
                };
                let doc = editor.read(cx).doc.clone();
                let preview = cx.new(|cx| MarkdownPreview::new(doc, cx));
                self.active_pane = pane.clone();
                let ix = self.insert_tab(preview.into(), false, window, cx);
                (pane, ix)
            }
        };
        pane.update(cx, |p, cx| p.set_active(ix, cx));
        self.sync_previewed(cx);
        // The keyboard stays where the typing is.
        self.activate_editor(editor, window, cx);
    }

    /// The Markdown previews of a document, in pane order.
    pub(super) fn previews_of(&self, doc: &Entity<Document>, cx: &App) -> Vec<PaneItem> {
        self.items(cx).into_iter().filter(|t| t.preview().is_some_and(|p| p.read(cx).document() == doc)).collect()
    }

    /// Tells Markdown editors whether a preview of their document is open,
    /// which their preview toggle shows.
    pub(super) fn sync_previewed(&self, cx: &mut App) {
        let docs: Vec<Entity<Document>> = self.items(cx).iter().filter_map(|t| Some(t.preview()?.read(cx).document().clone())).collect();
        for editor in &self.editors {
            let previewed = docs.contains(&editor.read(cx).doc);
            editor.update(cx, |e, cx| e.set_previewed(previewed, cx));
        }
    }

    pub(super) fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        // Like JetBrains tool windows: show Files, or hide if Files is showing.
        if self.show_sidebar && self.sidebar_mode == SidebarMode::Files {
            self.show_sidebar = false;
        } else {
            self.show_sidebar = true;
            self.sidebar_mode = SidebarMode::Files;
        }
        // Leaving the Commit view: its message box must not keep focus while hidden, or
        // key presses (⌘0 included) go nowhere.
        if self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        self.save_state(cx);
        cx.notify();
    }

    /// Shows or hides the sidebar, keeping the view it has.
    pub(super) fn set_sidebar_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = visible;
        // A hidden Commit view's message box must not keep focus (see `toggle_sidebar`).
        if !visible && self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        if visible && self.sidebar_mode == SidebarMode::Commit {
            self.refresh_commit_view(cx);
        }
        self.save_state(cx);
        cx.notify();
    }

    /// The sidebar switch's Files segment.
    pub(super) fn show_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_mode == SidebarMode::Files {
            return;
        }
        self.sidebar_mode = SidebarMode::Files;
        if self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        self.save_state(cx);
        cx.notify();
    }

    pub(super) fn show_commit(&mut self, _: &ShowCommit, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = true;
        self.sidebar_mode = SidebarMode::Commit;
        self.refresh_commit_view(cx);
        self.git_panel.read(cx).focus_message(window, cx);
        self.save_state(cx);
        cx.notify();
    }

    pub(super) fn toggle_commit(&mut self, _: &ToggleCommit, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_sidebar && self.sidebar_mode == SidebarMode::Commit {
            self.show_sidebar = false;
            self.focus_active(window, cx);
            self.save_state(cx);
            cx.notify();
        } else {
            self.show_commit(&ShowCommit, window, cx);
        }
    }
}
