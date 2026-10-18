//! Context menus: editor, Git panel files, tabs, terminal tabs, breakpoints.

use super::*;

impl Workspace {
    // ---- Editor context menu -----------------------------------------------------

    pub(super) fn open_editor_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let e = editor.read(cx);
        let path = e.doc.read(cx).path.clone();
        let has_change = e.has_change_at_cursor(cx);
        let change_staged = e.change_at_cursor_staged(cx);
        let in_conflict = e.has_conflict_at_cursor(cx);
        let soft_wrap = e.soft_wrap();
        let mut entries: Vec<MenuEntry> = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        // An editor action with its shortcut hint.
        let run = |entry: MenuEntry, action: Box<dyn gpui::Action>| {
            (entry.shortcut(binding_label(&*action, window)), Some(MenuAction::Run(action)))
        };
        // Code intelligence first, as in VS Code and JetBrains.
        for (label, action) in [
            ("Go to Declaration", Box::new(editor::GoToDeclaration) as Box<dyn gpui::Action>),
            ("Go to Implementation", Box::new(editor::GoToImplementation)),
            ("Find Usages", Box::new(editor::FindUsages)),
            ("Call Hierarchy", Box::new(editor::CallHierarchy)),
            ("Rename…", Box::new(editor::RenameSymbol)),
            ("Show Context Actions", Box::new(editor::ShowContextActions)),
            ("Reformat Code", Box::new(editor::ReformatCode)),
        ] {
            let (entry, action) = run(MenuEntry::text(label), action);
            add(entry, action);
        }
        add(MenuEntry::Separator, None);
        let (entry, action) = run(MenuEntry::text("Cut"), Box::new(editor::Cut));
        add(entry, action);
        let (entry, action) = run(MenuEntry::item("Copy", Icon::Copy), Box::new(editor::Copy));
        add(entry, action);
        let (entry, action) = run(MenuEntry::item("Paste", Icon::Clipboard), Box::new(editor::Paste));
        add(entry, action);
        add(MenuEntry::Separator, None);
        for (label, action) in [
            ("Comment with Line Comment", Box::new(editor::ToggleComment) as Box<dyn gpui::Action>),
            ("Duplicate Line or Selection", Box::new(editor::DuplicateLine)),
            ("Select All Occurrences", Box::new(editor::SelectAllOccurrences)),
            ("Extend Selection", Box::new(editor::ExtendSelection)),
        ] {
            let (entry, action) = run(MenuEntry::text(label), action);
            add(entry, action);
        }

        // Git, when the file is in the repository.
        if let (Some(path), Some(repo)) = (path.as_ref(), self.repo.as_ref())
            && path.starts_with(repo)
        {
            add(MenuEntry::Separator, None);
            add(MenuEntry::Header("Git".into()), None);
            if in_conflict {
                for (label, action) in [
                    ("Accept Current Change", Box::new(editor::AcceptCurrent) as Box<dyn gpui::Action>),
                    ("Accept Incoming Change", Box::new(editor::AcceptIncoming)),
                    ("Accept Both Changes", Box::new(editor::AcceptBoth)),
                ] {
                    let (entry, action) = run(MenuEntry::text(label), action);
                    add(entry, action);
                }
            }
            if has_change {
                let (entry, action) = match change_staged {
                    Some(true) => run(MenuEntry::item("Unstage Lines", Icon::Minus), Box::new(editor::UnstageHunk)),
                    _ => run(MenuEntry::item("Stage Lines", Icon::Plus), Box::new(editor::StageHunk)),
                };
                add(entry, action);
                let (entry, action) = run(MenuEntry::item("Rollback Lines", Icon::Undo), Box::new(editor::RollbackHunk));
                add(entry, action);
            }
            if let Some(f) = self.git_status.files.iter().find(|f| &f.path == path) {
                add(
                    MenuEntry::item("Show Diff", Icon::GitCompare),
                    Some(MenuAction::Tree(TreeAction::ShowDiff(f.clone(), f.unstaged.is_none()))),
                );
            }
            let (entry, action) = run(MenuEntry::item("Show History", Icon::History), Box::new(ShowFileHistory));
            add(entry, action);
            let (entry, action) = run(MenuEntry::item("Toggle Inline Git Blame", Icon::GitCommit), Box::new(editor::ToggleInlineBlame));
            add(entry, action);
        }

        add(MenuEntry::Separator, None);
        let wrap_entry = if soft_wrap { MenuEntry::item("Soft-Wrap", Icon::Check) } else { MenuEntry::text("Soft-Wrap") };
        let (entry, action) = run(wrap_entry, Box::new(editor::ToggleSoftWrap));
        add(entry, action);
        if let Some(path) = path {
            add(MenuEntry::Separator, None);
            add(MenuEntry::item("Copy Path", Icon::Clipboard), Some(MenuAction::Tree(TreeAction::CopyPath(path.clone()))));
            if self.root.as_ref().is_some_and(|r| path.starts_with(r)) {
                add(
                    MenuEntry::item("Copy Relative Path", Icon::Clipboard),
                    Some(MenuAction::Tree(TreeAction::CopyRelativePath(path.clone()))),
                );
            }
            add(MenuEntry::item(reveal_label(), Icon::FolderOpen), Some(MenuAction::Tree(TreeAction::Reveal(path))));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    // ---- Git panel context menu --------------------------------------------------

    /// A changed file's menu in the Commit view; `staged`: its row is in the Staged section.
    pub(super) fn open_git_file_menu(&mut self, file: FileStatus, staged: bool, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let change = if staged { file.staged } else { file.unstaged.or(file.staged) };
        let conflicted = file.staged == Some(Change::Conflicted) || file.unstaged == Some(Change::Conflicted);
        let deleted = change == Some(Change::Deleted);
        let untracked = file.unstaged == Some(Change::Untracked);
        // Nothing to show for a file that isn't in any commit yet.
        let has_history = !untracked && file.staged != Some(Change::Added);
        let mut entries: Vec<MenuEntry> = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<TreeAction>| {
            entries.push(entry);
            actions.push(action.map(MenuAction::Tree));
        };

        add(MenuEntry::item("Show Diff", Icon::GitCompare), Some(TreeAction::ShowDiff(file.clone(), staged)));
        if !deleted {
            add(MenuEntry::item("Open File", Icon::File), Some(TreeAction::Open(file.path.clone())));
        }
        if has_history {
            add(MenuEntry::item("Show History", Icon::History), Some(TreeAction::ShowHistory(file.rel.clone())));
        }

        add(MenuEntry::Separator, None);
        let rels = vec![file.rel.clone()];
        if staged {
            add(MenuEntry::item("Unstage", Icon::Minus), Some(TreeAction::Git(GitOp::Unstage(rels))));
        } else if conflicted {
            add(MenuEntry::item("Mark as Resolved", Icon::Check), Some(TreeAction::Git(GitOp::Stage(rels))));
        } else {
            add(MenuEntry::item("Stage", Icon::Plus), Some(TreeAction::Git(GitOp::Stage(rels))));
        }
        if !staged && !conflicted {
            add(MenuEntry::item("Rollback…", Icon::Undo).danger(), Some(TreeAction::Git(GitOp::Discard(vec![file.clone()]))));
        }
        if untracked {
            add(MenuEntry::item("Add to .gitignore", Icon::EyeOff), Some(TreeAction::AddToGitignore(file.rel.clone())));
        }

        add(MenuEntry::Separator, None);
        add(MenuEntry::item("Copy Path", Icon::Clipboard), Some(TreeAction::CopyPath(file.path.clone())));
        if self.root.as_ref().is_some_and(|r| file.path.starts_with(r)) {
            add(MenuEntry::item("Copy Relative Path", Icon::Clipboard), Some(TreeAction::CopyRelativePath(file.path.clone())));
        }
        if !deleted {
            add(MenuEntry::item(reveal_label(), Icon::FolderOpen), Some(TreeAction::Reveal(file.path.clone())));
            add(MenuEntry::Separator, None);
            add(
                MenuEntry::item(if cfg!(windows) { "Move to Recycle Bin" } else { "Move to Trash" }, Icon::Trash).danger(),
                Some(TreeAction::Delete(file.path.clone())),
            );
        }

        let target = (file.rel.clone(), staged);
        self.show_context_menu(entries, actions, position, None, window, cx);
        if let Some(menu) = &mut self.context_menu {
            menu.git_target = Some(target);
        }
    }

    /// A commit's menu in the Git log.
    pub(super) fn open_commit_menu(&mut self, entry: git::LogEntry, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let sha = entry.sha.clone();
        let is_head = self.git_status.head_oid.as_deref() == Some(sha.as_str());
        let mut entries: Vec<MenuEntry> = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<TreeAction>| {
            entries.push(entry);
            actions.push(action.map(MenuAction::Tree));
        };
        add(MenuEntry::item("Copy Commit Hash", Icon::Copy), Some(TreeAction::CopyText(sha.clone())));
        add(MenuEntry::Separator, None);
        add(MenuEntry::item("Checkout Revision", Icon::GitCommit), Some(TreeAction::Git(GitOp::CheckoutCommit(sha.clone()))));
        add(MenuEntry::item("New Branch…", Icon::GitBranch), Some(TreeAction::AskGit(GitPrompt::BranchAt(sha.clone()))));
        add(MenuEntry::item("New Tag…", Icon::Tag), Some(TreeAction::AskGit(GitPrompt::TagAt(sha.clone()))));
        add(MenuEntry::Separator, None);
        if !is_head {
            add(MenuEntry::item("Cherry-Pick", Icon::Plus), Some(TreeAction::Git(GitOp::CherryPick(sha.clone()))));
        }
        add(MenuEntry::item("Revert Commit", Icon::Undo), Some(TreeAction::Git(GitOp::Revert(sha.clone()))));
        if !is_head {
            add(MenuEntry::Separator, None);
            let reset = |mode| Some(TreeAction::Git(GitOp::Reset { sha: sha.clone(), mode }));
            add(MenuEntry::item("Reset Here (Soft: Keep Changes Staged)", Icon::History), reset(git::ResetMode::Soft));
            add(MenuEntry::item("Reset Here (Mixed: Keep Changes)", Icon::History), reset(git::ResetMode::Mixed));
            add(MenuEntry::item("Reset Here (Hard: Discard Changes)…", Icon::History).danger(), reset(git::ResetMode::Hard));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    pub(super) fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    pub(super) fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.context_menu.as_ref()?;
        Some(
            div()
                .absolute()
                .inset_0()
                // While a menu is open, what's behind it doesn't react to the
                // pointer (no editor hovers); clicking anywhere else closes it.
                .occlude()
                // A title bar menu: another menu's name, under the pointer or
                // clicked, opens that one (clicking its own closes it).
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, event: &MouseDownEvent, window, cx| match ws.app_menu_at(event.position) {
                        Some(ix) if ws.context_menu.as_ref().and_then(|m| m.app_menu) != Some(ix) => ws.open_app_menu(ix, window, cx),
                        _ => ws.close_context_menu(window, cx),
                    }),
                )
                .on_mouse_down(MouseButton::Right, cx.listener(|ws, _, window, cx| ws.close_context_menu(window, cx)))
                .when(open.app_menu.is_some(), |el| {
                    el.on_mouse_move(cx.listener(|ws, event: &MouseMoveEvent, window, cx| {
                        if let Some(ix) = ws.app_menu_at(event.position)
                            && ws.context_menu.as_ref().and_then(|m| m.app_menu) != Some(ix)
                        {
                            ws.open_app_menu(ix, window, cx);
                        }
                    }))
                })
                // A title bar menu is drawn under its name (`render_app_menu`).
                .when(open.app_menu.is_none(), |el| {
                    el.child(gpui::deferred(
                        gpui::anchored()
                            .position(open.position)
                            .snap_to_window_with_margin(px(8.))
                            .child(
                                div()
                                    .occlude()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                                    .child(open.menu.clone()),
                            ),
                    ))
                })
                .into_any_element(),
        )
    }

    pub(super) fn run_tree_action(&mut self, action: TreeAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            TreeAction::NewFile(dir) => self.ask_for_name(PendingInput::NewFile(dir), window, cx),
            TreeAction::NewFolder(dir) => self.ask_for_name(PendingInput::NewFolder(dir), window, cx),
            TreeAction::Rename(path) => self.ask_for_name(PendingInput::Rename(path), window, cx),
            TreeAction::Open(path) => self.open_path(path, window, cx),
            TreeAction::Duplicate(path) => self.duplicate_file(path, window, cx),
            TreeAction::Delete(path) => self.confirm_trash(path, window, cx),
            TreeAction::CopyPath(path) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.display().to_string()));
            }
            TreeAction::CopyRelativePath(path) => {
                let base = self.root.clone().unwrap_or_default();
                let rel = path.strip_prefix(&base).unwrap_or(&path).to_string_lossy().into_owned();
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel));
            }
            TreeAction::Reveal(path) => cx.reveal_path(&path),
            TreeAction::FindInFolder(dir) => self.open_project_search_in(Some(dir), false, window, cx),
            TreeAction::CollapseAll => {
                self.tree.update(cx, |t, cx| t.collapse_all(cx));
                self.schedule_session_save(cx);
                cx.notify();
            }
            TreeAction::Git(op) => self.run_git(op, window, cx),
            TreeAction::ShowDiff(file, staged) => self.open_diff(file, staged, window, cx),
            TreeAction::AddToGitignore(rel) => self.add_to_gitignore(rel, cx),
            TreeAction::ShowHistory(rel) => self.show_history_of(rel, window, cx),
            TreeAction::AskGit(prompt) => self.ask_for_name(PendingInput::Git(prompt), window, cx),
            TreeAction::CopyText(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
            TreeAction::UndoFileChange => self.step_file_history(false, window, cx),
            TreeAction::RedoFileChange => self.step_file_history(true, window, cx),
        }
    }

    pub(super) fn ask_for_name(&mut self, pending: PendingInput, window: &mut Window, cx: &mut Context<Self>) {
        let (title, icon, hint, initial, select) = match &pending {
            PendingInput::NewFile(dir) => (
                "New File",
                Icon::FilePlus,
                format!("In {} · use / to create folders too", self.display_rel(dir)),
                String::new(),
                None,
            ),
            PendingInput::NewFolder(dir) => ("New Folder", Icon::FolderPlus, format!("In {}", self.display_rel(dir)), String::new(), None),
            PendingInput::Rename(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                // Preselect the name without its extension, like JetBrains.
                let stem = if path.is_dir() { name.len() } else { name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len()) };
                ("Rename", Icon::Pencil, format!("Rename {name}"), name.clone(), Some(0..stem))
            }
            PendingInput::RenameSymbol { current, .. } => {
                ("Rename", Icon::Pencil, format!("Rename {current} and its usages"), current.clone(), Some(0..current.len()))
            }
            PendingInput::VimCommand => (":", Icon::Terminal, "w, q, wq, x, or a line number".to_string(), String::new(), None),
            PendingInput::Git(GitPrompt::RenameBranch(name)) => {
                ("Rename Branch", Icon::GitBranch, format!("Rename {name}"), name.clone(), Some(0..name.len()))
            }
            PendingInput::Git(GitPrompt::BranchAt(sha)) => {
                ("New Branch", Icon::GitBranch, format!("From {} · checks it out", &sha[..sha.len().min(7)]), String::new(), None)
            }
            PendingInput::Git(GitPrompt::TagAt(sha)) => ("New Tag", Icon::Tag, format!("At {}", &sha[..sha.len().min(7)]), String::new(), None),
            PendingInput::Git(GitPrompt::Stash) => ("Stash Changes", Icon::Archive, "A message (optional)".into(), String::new(), None),
        };
        self.close_modal(window, cx);
        let stash = matches!(pending, PendingInput::Git(GitPrompt::Stash));
        // Untracked files stay out of a stash unless asked for (they're often
        // build output, which comes back and blocks the stash's own copy);
        // when they're all there is to stash, they're in from the start.
        let untracked = self.git_status.untracked().count();
        let only_untracked = untracked == self.git_status.files.len();
        let dialog = cx.new(|cx| {
            let dialog = InputDialog::new(title, icon, hint, &initial, select, window, cx);
            match stash {
                true if untracked > 0 => dialog.allow_empty().with_option(format!("Include untracked files ({untracked})"), only_untracked),
                true => dialog.allow_empty(),
                false => dialog,
            }
        });
        let subscription = cx.subscribe_in(&dialog, window, |ws, dialog, event: &InputDialogEvent, window, cx| match event {
            InputDialogEvent::Confirm(name) => match ws.apply_name(name, dialog.read(cx).option_checked(), window, cx) {
                Ok(()) => {
                    let in_tree = matches!(ws.pending_input, Some(PendingInput::Rename(_) | PendingInput::NewFolder(_)));
                    ws.pending_input = None;
                    ws.close_modal(window, cx);
                    // So ⌘Z undoes it.
                    if in_tree {
                        window.focus(&ws.tree.focus_handle(cx));
                    }
                }
                Err(err) => dialog.update(cx, |d, cx| d.set_error(err, cx)),
            },
            InputDialogEvent::Dismissed => {
                ws.pending_input = None;
                ws.close_modal(window, cx);
            }
        });
        self.pending_input = Some(pending);
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    /// `name` as a branch name (spaces become dashes), if Git accepts it.
    fn branch_name(&self, name: &str) -> Result<String, String> {
        let name = name.replace(char::is_whitespace, "-");
        match &self.repo {
            Some(repo) if !git::valid_branch_name(repo, &name) => Err(format!("“{name}” isn't a valid branch name")),
            _ => Ok(name),
        }
    }

    pub(super) fn display_rel(&self, path: &Path) -> String {
        match self.root.as_deref().and_then(|r| path.strip_prefix(r).ok()) {
            Some(rel) if rel.as_os_str().is_empty() => self.root.as_deref().map(folder_name).unwrap_or_default(),
            Some(rel) => rel.display().to_string(),
            None => path.display().to_string(),
        }
    }

    /// A location's file for usage and symbol lists (`location_label`).
    pub(super) fn location_label(&self, path: &Path) -> String {
        location_label(self.root.as_deref(), path)
    }

    pub(super) fn vim_command_line(&mut self, _: &VimCommandLine, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_for_name(PendingInput::VimCommand, window, cx);
    }

    /// Runs a Vim `:` command: write, quit, both, or go to a line.
    pub(super) fn run_vim_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let command = command.trim().trim_start_matches(':');
        if let Ok(line) = command.parse::<usize>() {
            let editor = self.active_editor().cloned().ok_or("No file is open")?;
            editor.update(cx, |e, cx| {
                let row = line.saturating_sub(1).min(e.doc.read(cx).line_count().saturating_sub(1));
                let text = e.doc.read(cx).line_text(row);
                let offset = e.doc.read(cx).line_start(row) + (text.len() - text.trim_start().len());
                e.go_to(offset, window, cx);
            });
            return Ok(());
        }
        match command {
            "w" | "write" => self.save(&Save, window, cx),
            "q" | "q!" | "quit" | "close" => self.close_tab(&CloseTab, window, cx),
            "wq" | "x" | "wq!" | "x!" => {
                self.save(&Save, window, cx);
                self.close_tab(&CloseTab, window, cx);
            }
            "wa" | "wall" => {
                for editor in self.editors.clone() {
                    if editor.read(cx).doc.read(cx).is_dirty() && editor.read(cx).doc.read(cx).path.is_some() {
                        editor.update(cx, |e, cx| e.save(cx)).ok();
                    }
                }
            }
            "qa" | "qa!" | "qall" => cx.defer(quit),
            "" => {}
            other => return Err(format!("Not a command Kuana knows: {other}")),
        }
        Ok(())
    }

    /// Creates or renames according to the open input dialog; `checked` is
    /// its checkbox (Include untracked files).
    pub(super) fn apply_name(&mut self, name: &str, checked: bool, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if matches!(self.pending_input, Some(PendingInput::VimCommand)) {
            let command = name.to_string();
            self.pending_input = None;
            self.close_modal(window, cx);
            if let Err(err) = self.run_vim_command(&command, window, cx) {
                self.toast(ToastKind::Error, err, None, cx);
            }
            return Ok(());
        }
        let git_prompt = match &self.pending_input {
            Some(PendingInput::Git(prompt)) => Some(prompt.clone()),
            _ => None,
        };
        if let Some(prompt) = git_prompt {
            let name = name.trim();
            let op = match prompt {
                GitPrompt::Stash => GitOp::Stash { message: Some(name.to_string()).filter(|n| !n.is_empty()), untracked: checked },
                _ if name.is_empty() => return Err("Enter a name".into()),
                GitPrompt::RenameBranch(from) if from == name => return Ok(()),
                GitPrompt::RenameBranch(from) => GitOp::RenameBranch { from, to: self.branch_name(name)? },
                GitPrompt::BranchAt(sha) => GitOp::BranchAt { name: self.branch_name(name)?, sha },
                GitPrompt::TagAt(sha) => GitOp::TagAt { name: name.to_string(), sha },
            };
            self.pending_input = None;
            self.close_modal(window, cx);
            self.run_git(op, window, cx);
            return Ok(());
        }
        if let Some(PendingInput::RenameSymbol { server, path, position, current }) = self.pending_input.as_ref() {
            let new_name = name.trim();
            if new_name.is_empty() {
                return Err("Enter a name".into());
            }
            if new_name != current {
                self.rename_symbol(server.clone(), path.clone(), *position, new_name.to_string(), window, cx);
            }
            return Ok(());
        }
        let name = name.trim_matches('/');
        if name.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err("That isn't a valid name".into());
        }
        match self.pending_input.as_ref().ok_or("Nothing to do")? {
            PendingInput::NewFile(dir) => {
                let path = dir.join(name);
                if path.exists() {
                    return Err(format!("{name} already exists"));
                }
                let created = crate::file_history::topmost_new(&path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| e.to_string())?;
                self.file_history.record(format!("New File “{name}”"), crate::file_history::Change::Created(created.clone()));
                self.reveal_in_tree(&path, cx);
                self.after_fs_change(&[created], cx);
                self.open_path(path, window, cx);
            }
            PendingInput::NewFolder(dir) => {
                let path = dir.join(name);
                if path.exists() {
                    return Err(format!("{name} already exists"));
                }
                let created = crate::file_history::topmost_new(&path);
                std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
                self.file_history.record(format!("New Folder “{name}”"), crate::file_history::Change::Created(created.clone()));
                self.reveal_in_tree(&path, cx);
                self.tree.update(cx, |t, _| t.expand(path));
                self.after_fs_change(&[created], cx);
            }
            PendingInput::Rename(old) => {
                let old = old.clone();
                let new = old.parent().map(|p| p.join(name)).ok_or("Can't rename this")?;
                if new == old {
                    return Ok(());
                }
                if new.exists() {
                    return Err(format!("{name} already exists"));
                }
                // Folders the new name adds go away again on undo.
                let new_dirs = new.parent().map(crate::file_history::topmost_new).filter(|d| !d.exists());
                self.rename_with_servers(old.clone(), new.clone(), cx, move |ws, cx| {
                    let old_name = old.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let renamed = crate::file_history::Change::Renamed { from: old.clone(), to: new.clone() };
                    let mut changed = vec![old.clone(), new.clone()];
                    let change = match new_dirs {
                        Some(dirs) => {
                            changed.push(dirs.clone());
                            crate::file_history::Change::Batch(vec![crate::file_history::Change::Created(dirs), renamed])
                        }
                        None => renamed,
                    };
                    ws.file_history.record(format!("Rename “{old_name}”"), change);
                    ws.moved(&old, &new, cx);
                    ws.reveal_in_tree(&new, cx);
                    ws.after_fs_change(&changed, cx);
                });
            }
            // Handled before the name checks above.
            PendingInput::RenameSymbol { .. } | PendingInput::VimCommand | PendingInput::Git(_) => {}
        }
        Ok(())
    }

    /// Undoes (or redoes) the last file operation. Moving files with unsaved
    /// edits to the Trash asks first.
    pub(super) fn step_file_history(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        let label = if redo { self.file_history.redo_label() } else { self.file_history.undo_label() };
        let Some(label) = label.map(str::to_string) else { return };
        let mut trashes = Vec::new();
        if let Some(change) = self.file_history.next(redo) {
            change.visit(&mut |c| {
                if let crate::file_history::Change::Created(p) | crate::file_history::Change::Restored(p) = c {
                    trashes.push(p.clone());
                }
            });
        }
        let dirty = self.editors.iter().any(|t| {
            let doc = t.read(cx).doc.read(cx);
            doc.is_dirty() && doc.path.as_deref().is_some_and(|p| trashes.iter().any(|t| p.starts_with(t)))
        });
        if !dirty {
            self.apply_file_history(redo, window, cx);
            return;
        }
        let verb = if redo { "Redo" } else { "Undo" };
        let detail = format!("This moves files with unsaved changes to the {}; the changes will be lost.", trash_name());
        // Unsaved changes don't come back: Cancel is the default.
        let confirmed = ask(window, &format!("{verb} {label}?"), &detail, verb, false, cx);
        cx.spawn_in(window, async move |this, cx| {
            if confirmed.await {
                this.update_in(cx, |ws, window, cx| ws.apply_file_history(redo, window, cx)).ok();
            }
        })
        .detach();
    }

    pub(super) fn apply_file_history(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::file_history::Change;
        let Some((label, result)) = self.file_history.step(redo) else { return };
        let mut changed = Vec::new();
        match result {
            Ok(done) => done.visit(&mut |change| match change {
                Change::Renamed { from, to } => {
                    self.moved(from, to, cx);
                    self.reveal_in_tree(to, cx);
                    changed.extend([from.clone(), to.clone()]);
                }
                Change::Trashed(trashed) => {
                    let path = &trashed.original;
                    self.close_tabs_under(path, window, cx);
                    self.tree.update(cx, |t, _| t.forget(path));
                    self.recent.retain(|p| !p.starts_with(path));
                    changed.push(path.clone());
                }
                Change::Created(path) | Change::Restored(path) => {
                    self.reveal_in_tree(path, cx);
                    changed.push(path.clone());
                }
                Change::Batch(_) => {}
            }),
            Err(err) => {
                let verb = if redo { "redo" } else { "undo" };
                self.toast(ToastKind::Error, format!("Couldn't {verb} {label}"), Some(err.to_string()), cx);
            }
        }
        window.focus(&self.tree.focus_handle(cx));
        self.after_fs_change(&changed, cx);
        self.schedule_session_save(cx);
    }

    /// Keeps open tabs, expanded folders and recents pointing at moved files.
    pub(super) fn moved(&mut self, old: &Path, new: &Path, cx: &mut Context<Self>) {
        self.lsp.read(cx).did_rename(&[(old.to_path_buf(), new.to_path_buf(), new.is_dir())]);
        let remap = |p: &Path| p.strip_prefix(old).ok().map(|rest| if rest.as_os_str().is_empty() { new.to_path_buf() } else { new.join(rest) });
        for editor in self.editors.clone() {
            let path = editor.read(cx).doc.read(cx).path.clone();
            if let Some((path, moved)) = path.as_deref().and_then(|p| Some((p, remap(p)?))) {
                editor.update(cx, |e, cx| e.set_path(moved, cx));
                let doc = editor.read(cx).doc.clone();
                self.documents.update(cx, |s, cx| s.add(&doc, cx));
                self.lsp.update(cx, |s, cx| s.moved(path, &editor, cx));
                self.load_diff_base(&editor, cx);
            }
        }
        // Changed without a tab: saved later, it goes to the new place.
        for doc in self.documents.read(cx).kept(cx) {
            let path = doc.read(cx).path.clone();
            if let Some((path, moved)) = path.as_deref().and_then(|p| Some((p, remap(p)?))) {
                doc.update(cx, |d, cx| {
                    d.set_path(moved);
                    cx.notify();
                });
                self.documents.update(cx, |s, cx| s.add(&doc, cx));
                self.lsp.update(cx, |s, cx| {
                    s.did_close(path);
                    s.register_document(&doc, None, cx);
                });
            }
        }
        for image in self.images() {
            if let Some(moved) = remap(image.read(cx).path()) {
                image.update(cx, |i, cx| i.set_path(moved, cx));
            }
        }
        self.tree.update(cx, |t, _| t.remap(remap));
        for p in &mut self.recent {
            if let Some(m) = remap(p) {
                *p = m;
            }
        }
    }

    /// Moves a dragged file or folder into `dir`, keeping open tabs on it.
    pub(super) fn move_entry(&mut self, from: PathBuf, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(name) = from.file_name() else { return };
        let to = dir.join(name);
        if from.parent() == Some(dir.as_path()) || to == from {
            return;
        }
        let problem = if dir.starts_with(&from) {
            Some("A folder can't go inside itself".to_string())
        } else if to.exists() {
            Some(format!("{} already has {}", folder_name(&dir), name.to_string_lossy()))
        } else {
            None
        };
        if let Some(problem) = problem {
            self.toast(ToastKind::Error, "Couldn't move", Some(problem), cx);
            return;
        }
        let label = format!("Move “{}”", name.to_string_lossy());
        self.rename_with_servers(from.clone(), to.clone(), cx, move |ws, cx| {
            ws.file_history.record(label, crate::file_history::Change::Renamed { from: from.clone(), to: to.clone() });
            ws.moved(&from, &to, cx);
            ws.tree.update(cx, |t, _| t.expand(dir));
            ws.reveal_in_tree(&to, cx);
            ws.after_fs_change(&[from, to], cx);
            ws.schedule_session_save(cx);
        });
    }

    /// Renames or moves `old` to `new` (making the folders it needs), then
    /// runs `done`. Language servers that ask (`workspace/willRenameFiles`)
    /// first say what else changes with it, as Rust's `mod` lines or
    /// TypeScript's imports, and those edits are made before the rename (they
    /// name the old paths), as in VS Code; the files they change are saved by
    /// auto-save or listed like a rename's. A server that takes too long is
    /// left out.
    fn rename_with_servers(&mut self, old: PathBuf, new: PathBuf, cx: &mut Context<Self>, done: impl FnOnce(&mut Self, &mut Context<Self>) + 'static) {
        let asking = self.lsp.read(cx).will_rename(&[(old.clone(), new.clone(), old.is_dir())], cx);
        let rename = move |ws: &mut Self, cx: &mut Context<Self>| {
            let result = new.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::rename(&old, &new));
            match result {
                Ok(()) => done(ws, cx),
                Err(err) => ws.toast(ToastKind::Error, "Couldn't rename", Some(err.to_string()), cx),
            }
        };
        let Some(asking) = asking else {
            rename(self, cx);
            return;
        };
        cx.spawn(async move |ws, cx| {
            let answers = asking.await;
            let Ok(applying) = ws.update(cx, |ws, cx| match answers {
                Ok(answers) => answers.into_iter().map(|a| ws.apply_workspace_edit(a.edits, a.encoding, cx)).collect::<Vec<_>>(),
                Err(()) => {
                    ws.toast(ToastKind::Info, "Renamed without updating references", Some("The language server took too long to answer".into()), cx);
                    Vec::new()
                }
            }) else {
                return;
            };
            for edit in applying {
                edit.await;
            }
            ws.update(cx, rename).ok();
        })
        .detach();
    }



    /// Finder files dropped on the tree: copied into `dir` (under a free
    /// name if taken); then, like JetBrains, an offer to add them to Git.
    pub(super) fn drop_into_tree(&mut self, paths: Vec<PathBuf>, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.tree.update(cx, |t, cx| t.set_drop_target(None, cx));
        let mut added = Vec::new();
        let mut problems = Vec::new();
        for source in paths {
            let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if dir.starts_with(&source) {
                problems.push(format!("{name} can't go inside itself"));
                continue;
            }
            match copy_into(&source, &dir) {
                Ok(path) => added.push(path),
                Err(err) => problems.push(format!("{name}: {err}")),
            }
        }
        if !problems.is_empty() {
            self.toast(ToastKind::Error, "Couldn't copy everything", Some(problems.join("\n")), cx);
        }
        let Some(first) = added.first().cloned() else { return };
        let label = match added.as_slice() {
            [one] => format!("Copy “{}”", one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            many => format!("Copy {} Items", many.len()),
        };
        let created = added.iter().cloned().map(crate::file_history::Change::Created).collect();
        self.file_history.record(label, crate::file_history::Change::Batch(created));
        window.focus(&self.tree.focus_handle(cx));
        self.tree.update(cx, |t, _| t.expand(dir.clone()));
        self.reveal_in_tree(&first, cx);
        self.after_fs_change(&added, cx);
        self.schedule_session_save(cx);
        let Some(repo) = self.repo.clone().filter(|r| added.iter().all(|p| p.starts_with(r))) else { return };
        let ignored: Vec<bool> = added.iter().map(|p| self.ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(p, p.is_dir()).is_ignore())).collect();
        let added: Vec<PathBuf> = added.into_iter().zip(ignored).filter(|(_, ignored)| !ignored).map(|(p, _)| p).collect();
        if added.is_empty() {
            return;
        }
        let names: Vec<String> = added.iter().filter_map(|p| p.strip_prefix(&repo).ok()).map(|p| p.display().to_string()).collect();
        let message = if names.len() == 1 { "Add the file to Git?".to_string() } else { format!("Add {} files to Git?", names.len()) };
        let detail = names.iter().take(6).cloned().collect::<Vec<_>>().join("\n");
        let answer = window.prompt(PromptLevel::Info, &message, Some(&detail), &["Add", "Don't Add"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let mut args = vec!["add".to_string(), "--".to_string()];
            args.extend(names);
            let result = cx
                .background_executor()
                .spawn({
                    let repo = repo.clone();
                    async move { git::run(&repo, &args.iter().map(String::as_str).collect::<Vec<_>>()) }
                })
                .await;
            this.update(cx, |ws, cx| {
                if let Err(err) = result {
                    ws.toast(ToastKind::Error, "Couldn't add to Git", Some(err), cx);
                }
                ws.refresh_git(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Files and folders dropped from Finder: folders open as projects,
    /// files in this window.
    pub(super) fn open_dropped(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        for path in paths {
            if path.is_dir() {
                self.open_project(path.clone(), OpenIn::Auto, window, cx);
            } else {
                self.open_path(path.clone(), window, cx);
            }
        }
    }

    /// Opens what another app asked Kuana to open (Finder, the `kuana` command).
    pub fn open_external(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        self.open_dropped(&paths, window, cx);
    }

    /// Installs the `kuana` command (it may ask for an administrator password).
    pub(super) fn install_shell_command(&mut self, _: &InstallShellCommand, _: &mut Window, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async { crate::cli::install() }).await;
            this.update(cx, |ws, cx| match result {
                Ok(()) => {
                    let detail = format!("{} opens files and folders here: kuana . or kuana file.rs", crate::cli::COMMAND_PATH);
                    ws.toast(ToastKind::Success, "Installed the kuana command", Some(detail), cx);
                }
                Err(err) if err == "Cancelled" => {}
                Err(err) => ws.toast(ToastKind::Error, "Couldn't install the kuana command", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Shows the active file in the tree: opens its folders and scrolls to it.
    pub(super) fn select_opened_file(&mut self, _: &SelectOpenedFile, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_item().and_then(|t| t.path(cx)) else { return };
        if !self.root.as_deref().is_some_and(|r| path.starts_with(r)) {
            return;
        }
        self.show_sidebar = true;
        self.sidebar_mode = SidebarMode::Files;
        self.tree.update(cx, |t, cx| t.reveal_and_scroll(&path, cx));
        self.save_state(cx);
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Expands the folders above `path` so it's visible in the tree.
    pub(super) fn reveal_in_tree(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.tree.update(cx, |t, _| t.reveal(path));
    }

    /// After a file operation on `changed` (created, removed or renamed):
    /// the tree reads again the folders they're in. Go to File and Git
    /// status hear of it from the watcher.
    pub(super) fn after_fs_change(&mut self, changed: &[PathBuf], cx: &mut Context<Self>) {
        self.tree.update(cx, |t, cx| t.paths_changed(changed.iter().map(PathBuf::as_path), cx));
        self.files_written(changed.iter().cloned(), cx);
        cx.notify();
    }

    pub(super) fn duplicate_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().map(|s| s.to_string_lossy().into_owned())) else {
            return;
        };
        let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let target = (1..100)
            .map(|n| dir.join(if n == 1 { format!("{stem} copy{ext}") } else { format!("{stem} copy {n}{ext}") }))
            .find(|p| !p.exists());
        let Some(target) = target else { return };
        match std::fs::copy(&path, &target) {
            Ok(_) => {
                self.file_history.record(format!("Duplicate “{stem}{ext}”"), crate::file_history::Change::Created(target.clone()));
                self.after_fs_change(std::slice::from_ref(&target), cx);
                // Offer to name the copy right away.
                self.ask_for_name(PendingInput::Rename(target), window, cx);
            }
            Err(err) => self.toast(ToastKind::Error, "Couldn't duplicate", Some(err.to_string()), cx),
        }
    }

    pub(super) fn confirm_trash(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dirty = self.editors.iter().any(|t| {
            let doc = t.read(cx).doc.read(cx);
            doc.is_dirty() && doc.path.as_deref().is_some_and(|p| p.starts_with(&path))
        });
        let mut detail = if path.is_dir() {
            format!("The folder and everything in it will be moved to the {}.", trash_name())
        } else {
            format!("You can restore it from the {}.", trash_name())
        };
        if dirty {
            detail.push_str(" Unsaved changes in open files will be lost.");
        }
        let message = format!("Move “{name}” to the {}?", trash_name());
        // The Trash gives the file back, but not unsaved changes.
        let confirmed = ask(window, &message, &detail, &format!("Move to {}", trash_name()), !dirty, cx);
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = confirmed.await;
            this.update_in(cx, |ws, window, cx| {
                if confirmed {
                    ws.move_to_trash(&path, window, cx);
                }
                // The alert took the keyboard: back to the tree, where ⌘Z undoes it.
                if ws.show_sidebar && ws.sidebar_mode == SidebarMode::Files {
                    window.focus(&ws.tree.focus_handle(cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Closes the tabs of a file, or of the files in a folder, that went to
    /// the Trash, unsaved edits and all (the user was asked).
    fn close_tabs_under(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        for editor in self.editors.clone() {
            if editor.read(cx).doc.read(cx).path.as_deref().is_some_and(|p| p.starts_with(path)) {
                self.remove_tab(&editor, window, cx);
            }
        }
        for image in self.images() {
            if image.read(cx).path().starts_with(path) {
                self.remove_item(&image.into(), window, cx);
            }
        }
    }

    /// Moves a file or folder to the Trash (undoable), closing its tabs.
    fn move_to_trash(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let trashed = match crate::file_history::move_to_trash(path) {
            Ok(trashed) => trashed,
            Err(err) => {
                self.toast(ToastKind::Error, format!("Couldn't move to the {}", trash_name()), Some(err.to_string()), cx);
                return;
            }
        };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let id = self.file_history.record(format!("Delete “{name}”"), crate::file_history::Change::Trashed(trashed));
        self.close_tabs_under(path, window, cx);
        self.tree.update(cx, |t, _| t.forget(path));
        self.recent.retain(|p| !p.starts_with(path));
        self.after_fs_change(&[path.to_path_buf()], cx);
        let message = format!("Moved “{name}” to the {}", trash_name());
        self.toast_with_button(ToastKind::Success, message, None, Some(ToastButton::UndoFile(id)), cx);
    }

    /// A toast's button, while what it does still applies (what it would
    /// undo is still next; there are files to save).
    pub(super) fn toast_button_applies(&self, button: ToastButton, cx: &App) -> bool {
        match button {
            ToastButton::UndoFile(id) => self.file_history.undo_id() == Some(id),
            ToastButton::SaveBackground => !self.documents.read(cx).kept(cx).is_empty(),
        }
    }

    pub(super) fn run_toast_button(&mut self, button: ToastButton, window: &mut Window, cx: &mut Context<Self>) {
        if !self.toast_button_applies(button, cx) {
            return;
        }
        match button {
            ToastButton::UndoFile(_) => self.step_file_history(false, window, cx),
            ToastButton::SaveBackground => self.save_background(false, false, cx),
        }
    }

    pub(super) fn add_to_gitignore(&mut self, rel: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let file = repo.join(".gitignore");
        let entry = if repo.join(&rel).is_dir() { format!("/{rel}/") } else { format!("/{rel}") };
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        if existing.lines().any(|l| l.trim() == entry) {
            self.toast(ToastKind::Success, format!("{entry} is already ignored"), None, cx);
            return;
        }
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&entry);
        text.push('\n');
        match std::fs::write(&file, text) {
            Ok(()) => {
                self.toast(ToastKind::Success, format!("Added {entry} to .gitignore"), None, cx);
                self.reload_changed_files(cx);
                self.after_fs_change(&[file], cx);
            }
            Err(err) => self.toast(ToastKind::Error, "Couldn't update .gitignore", Some(err.to_string()), cx),
        }
    }

    pub(super) fn render_modal(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let content: AnyElement = match self.modals.current.as_ref()? {
            Modal::Picker(p) => p.clone().into_any_element(),
            Modal::GoToLine(g) => g.clone().into_any_element(),
            Modal::ProjectSearch(s) => s.clone().into_any_element(),
            Modal::Theme(t) => t.clone().into_any_element(),
            // Drawn over the editor area by `render_panes`.
            Modal::Diff(_) => return None,
            Modal::Branches(b) => b.clone().into_any_element(),
            Modal::Input(i) => i.clone().into_any_element(),
            Modal::Clone(c) => c.clone().into_any_element(),
            Modal::Push(p) => p.clone().into_any_element(),
            Modal::Remotes(r) => r.clone().into_any_element(),
            Modal::Trust(t) => t.clone().into_any_element(),
        };
        let top = modal_top(window);
        // Deferred above the other deferred layers (resize handles), so they
        // neither show through a modal nor take its mouse events.
        Some(
            gpui::deferred(
                div()
                    .id("modal-layer")
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(top)
                    .bg(gpui::black().opacity(0.18))
                    // Wheel and hover don't reach the editor behind a modal.
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|ws, _, window, cx| {
                            // A running clone keeps its dialog open; it has a Stop button.
                            if let Some(Modal::Clone(c)) = &ws.modals.current
                                && c.read(cx).is_busy()
                            {
                                return;
                            }
                            ws.close_modal(window, cx)
                        }),
                    )
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(content),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
