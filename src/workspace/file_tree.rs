//! The file tree: listing, expanding, revealing, and its right-click menu.

use super::*;

impl Workspace {
    // ---- File tree --------------------------------------------------------------

    pub fn set_root(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.update_recent_projects(
            |list| {
                list.retain(|p| p != &root);
                list.insert(0, root.clone());
            },
            cx,
        );
        self.breakpoints.clear();
        self.problems.update(cx, |p, cx| {
            p.set_run(None, cx);
            p.set_root(Some(root.clone()), cx);
        });
        let mut session = ProjectSession::load(&root);
        // The folders that were open last time, if they still exist.
        let expanded: Vec<PathBuf> = session.iter().flat_map(|s| s.expanded.iter().map(|p| root.join(p))).filter(|p| p.is_dir()).collect();
        if let Some(session) = &mut session {
            self.breakpoints = session.breakpoints.iter().map(|b| (root.join(&b.path), b.breakpoints.clone())).collect();
            // The project's own terminal panel, unless this window has shells already.
            if let Some(terminals) = session.terminals.take()
                && self.terminals.read(cx).is_empty()
            {
                if terminals.open {
                    self.bottom = Some(BottomPanel::Terminal);
                } else {
                    self.hide_bottom(BottomPanel::Terminal);
                }
                self.terminals.update(cx, |p, _| p.set_saved(Some(terminals)));
            }
        }
        // The run widget shows what this project ran last.
        self.last_run = session.as_mut().and_then(|s| s.last_run.take());
        // Before any tab's language server starts.
        let venv = session.and_then(|s| s.python_venv).map(|saved| crate::python::Choice::from_saved(&saved));
        crate::python::set_choice(&root, venv);
        self.venv_shown.take();
        self.root = Some(root.clone());
        self.show_sidebar = SessionState::load().show_sidebar.unwrap_or(true);
        self.tree.update(cx, |t, cx| t.set_root(Some(root), expanded, cx));
        self.refresh_file_index(cx);
        self.start_watching(cx);
        if let Some(root) = self.root.clone() {
            self.detect_repo(root, cx);
        }
        self.save_state(cx);
        // Problems in the project's settings file show when it opens.
        if let Some(root) = &self.root {
            crate::project_settings::layers(root);
            self.check_config(cx);
        }
        let root = self.root.clone();
        self.lsp.update(cx, |s, _| s.set_root(root));
        if self.items(cx).is_empty() {
            self.restore_session(window, cx);
        }
        self.start_terminal_panel(window, cx);
        if self.restricted(cx) {
            if self.onboarding.is_some() {
                self.trust_waits = true;
            } else if self.modals.current.is_none() {
                self.open_trust_dialog(window, cx);
            }
        }
        cx.notify();
    }

    /// Reopens the folder from the previous session, if it still exists.
    pub fn restore_last_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = SessionState::load().last_root.filter(|r| r.is_dir()) else {
            return;
        };
        self.set_root(root, window, cx);
    }
}

impl Workspace {
    // ---- File tree context menu --------------------------------------------------

    pub(super) fn open_tree_menu(&mut self, path: PathBuf, is_dir: bool, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let mut entries: Vec<MenuEntry> = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<TreeAction>| {
            entries.push(entry);
            actions.push(action.map(MenuAction::Tree));
        };
        let dir = if is_dir { path.clone() } else { path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone()) };
        let is_root = self.root.as_deref() == Some(path.as_path());

        let undo = self.file_history.undo_label().map(|l| format!("Undo {l}"));
        let redo = self.file_history.redo_label().map(|l| format!("Redo {l}"));
        let binding = |action: &dyn gpui::Action| crate::ui::binding_label(action, window);
        if let Some(label) = undo {
            add(MenuEntry::item(label, Icon::Undo).shortcut(binding(&UndoFileChange)), Some(TreeAction::UndoFileChange));
        }
        if let Some(label) = redo {
            add(MenuEntry::item(label, Icon::Redo).shortcut(binding(&RedoFileChange)), Some(TreeAction::RedoFileChange));
        }
        if self.file_history.undo_label().is_some() || self.file_history.redo_label().is_some() {
            add(MenuEntry::Separator, None);
        }
        add(MenuEntry::item("New File…", Icon::FilePlus), Some(TreeAction::NewFile(dir.clone())));
        add(MenuEntry::item("New Folder…", Icon::FolderPlus), Some(TreeAction::NewFolder(dir.clone())));
        add(MenuEntry::Separator, None);
        if !is_dir {
            add(MenuEntry::item("Open", Icon::File), Some(TreeAction::Open(path.clone())));
        }
        if !is_root {
            add(MenuEntry::item("Rename…", Icon::Pencil), Some(TreeAction::Rename(path.clone())));
        }
        if !is_dir {
            add(MenuEntry::item("Duplicate", Icon::Copy), Some(TreeAction::Duplicate(path.clone())));
        }
        add(MenuEntry::item("Copy Path", Icon::Clipboard), Some(TreeAction::CopyPath(path.clone())));
        if !is_root {
            add(MenuEntry::item("Copy Relative Path", Icon::Clipboard), Some(TreeAction::CopyRelativePath(path.clone())));
        }
        add(MenuEntry::item(reveal_label(), Icon::FolderOpen), Some(TreeAction::Reveal(path.clone())));
        if !is_dir {
            add(MenuEntry::item("Local History", Icon::History), Some(TreeAction::LocalHistory(path.clone())));
        }
        if is_dir {
            add(MenuEntry::item("Find in Folder…", Icon::TextSearch), Some(TreeAction::FindInFolder(path.clone())));
            add(MenuEntry::item("Collapse All", Icon::ChevronsDownUp), Some(TreeAction::CollapseAll));
        }

        // Git actions for this file, or every changed file inside this folder.
        if let Some(repo) = self.repo.clone().filter(|r| path.starts_with(r)) {
            let files: Vec<FileStatus> = self
                .git_status
                .files
                .iter()
                .filter(|f| if is_dir { f.path.starts_with(&path) } else { f.path == path })
                .cloned()
                .collect();
            let unstaged: Vec<FileStatus> = files.iter().filter(|f| f.unstaged.is_some()).cloned().collect();
            let staged: Vec<FileStatus> = files.iter().filter(|f| f.staged.is_some()).cloned().collect();
            let rel = path.strip_prefix(&repo).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            let suffix = if is_dir { " All in Folder" } else { "" };
            let mut git_entries: Vec<(MenuEntry, TreeAction)> = Vec::new();
            if !unstaged.is_empty() {
                git_entries.push((
                    MenuEntry::item(format!("Stage{suffix}"), Icon::Plus),
                    TreeAction::Git(GitOp::Stage(unstaged.iter().map(|f| f.rel.clone()).collect())),
                ));
            }
            if !staged.is_empty() {
                git_entries.push((
                    MenuEntry::item(format!("Unstage{suffix}"), Icon::Minus),
                    TreeAction::Git(GitOp::Unstage(staged.iter().map(|f| f.rel.clone()).collect())),
                ));
            }
            if !is_dir && let Some(f) = files.first() {
                git_entries.push((MenuEntry::item("Show Diff", Icon::GitCompare), TreeAction::ShowDiff(f.clone(), f.unstaged.is_none())));
            }
            if !unstaged.is_empty() {
                git_entries.push((
                    MenuEntry::item(format!("Rollback{suffix}…"), Icon::Undo).danger(),
                    TreeAction::Git(GitOp::Discard(unstaged)),
                ));
            }
            if !rel.is_empty() && !rel.starts_with(".git") {
                git_entries.push((MenuEntry::item("Add to .gitignore", Icon::EyeOff), TreeAction::AddToGitignore(rel)));
            }
            if !git_entries.is_empty() {
                add(MenuEntry::Separator, None);
                add(MenuEntry::Header("Git".into()), None);
                for (entry, action) in git_entries {
                    add(entry, Some(action));
                }
            }
        }

        if !is_root {
            add(MenuEntry::Separator, None);
            add(MenuEntry::item(if cfg!(windows) { "Move to Recycle Bin" } else { "Move to Trash" }, Icon::Trash).danger(), Some(TreeAction::Delete(path.clone())));
        }

        self.show_context_menu(entries, actions, position, Some(path), window, cx);
    }

    pub(super) fn show_context_menu(
        &mut self,
        entries: Vec<MenuEntry>,
        actions: Vec<Option<MenuAction>>,
        position: gpui::Point<Pixels>,
        target: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = cx.new(|cx| ContextMenu::new(entries, window, cx));
        let subscription = cx.subscribe_in(&menu, window, |ws, _, event: &ContextMenuEvent, window, cx| match event {
            ContextMenuEvent::Chose(ix) => {
                let action = ws.context_menu.as_ref().and_then(|m| m.actions.get(*ix).cloned().flatten());
                // Closing refocuses the editor, so editor actions reach it.
                ws.close_context_menu(window, cx);
                match action {
                    Some(MenuAction::Tree(action)) => ws.run_tree_action(action, window, cx),
                    Some(MenuAction::Run(action)) => window.dispatch_action(action, cx),
                    Some(MenuAction::CodeAction(server, action)) => ws.apply_code_action(server, action.raw, window, cx),
                    Some(MenuAction::Tab(action, item)) => ws.run_tab_action(action, &item, window, cx),
                    Some(MenuAction::LineEndings(crlf, editor)) => editor.update(cx, |e, cx| {
                        e.doc.as_mut(cx).set_crlf(crlf);
                        cx.emit(EditorEvent::Edited);
                        cx.notify();
                    }),
                    Some(MenuAction::Encoding(encoding, editor)) => ws.choose_encoding(encoding, editor, window, cx),
                    Some(MenuAction::Launch(target, true)) => ws.debug_target(target, window, cx),
                    Some(MenuAction::Launch(target, false)) => ws.run_target(target, window, cx),
                    Some(MenuAction::Breakpoint(BreakpointEdit::Toggle, editor, row)) => editor.update(cx, |e, cx| e.toggle_breakpoint_at(row, cx)),
                    Some(MenuAction::Breakpoint(edit, editor, row)) => ws.edit_breakpoint(edit, editor, row, window, cx),
                    Some(MenuAction::Venv(choice)) => ws.set_venv(choice, cx),
                    Some(MenuAction::ChooseVenv) => ws.choose_venv_folder(window, cx),
                    Some(MenuAction::Terminal(terminal)) => {
                        if let Some((g, ix)) = ws.terminal_position(&terminal, cx) {
                            ws.activate_terminal(g, ix, window, cx);
                        }
                    }
                    Some(MenuAction::TerminalTab(action, terminal)) => ws.run_terminal_tab_action(action, terminal, window, cx),
                    Some(MenuAction::OpenFile(path)) => ws.open_path(path, window, cx),
                    Some(MenuAction::SaveBackground) => ws.save_background(false, false, cx),
                    None => {}
                }
            }
            ContextMenuEvent::Dismissed => ws.close_context_menu(window, cx),
            ContextMenuEvent::Neighbor(forward) => ws.step_app_menu(*forward, window, cx),
        });
        self.context_menu = Some(OpenContextMenu { menu, position, actions, target, git_target: None, app_menu: None, _subscription: subscription });
        cx.notify();
    }
}



/// Copies a file or folder into `dir`, as "name copy" when the name is
/// taken. Returns where it went.
pub(super) fn copy_into(source: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let name = source.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    let mut target = dir.join(name);
    if target.exists() {
        let path = Path::new(name);
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = path.extension().filter(|_| !source.is_dir()).map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let stem = if source.is_dir() { name.to_string_lossy().into_owned() } else { stem };
        target = (1..1000)
            .map(|n| dir.join(if n == 1 { format!("{stem} copy{ext}") } else { format!("{stem} copy {n}{ext}") }))
            .find(|p| !p.exists())
            .ok_or_else(|| std::io::Error::other("no free name"))?;
    }
    copy_recursive(source, &target)?;
    Ok(target)
}


pub(super) fn copy_recursive(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &target.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(source, target).map(|_| ())
    }
}
