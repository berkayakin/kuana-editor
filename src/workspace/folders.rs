//! The project's folders (see `crate::project_folders` and
//! `plans/multi-root.md`): Add Folder to Project…, Remove Folder from
//! Project, and what follows a change of folders. Each folder brings its
//! own scan, watching, ignore rules, language servers and project settings;
//! its trust is asked for when it isn't trusted. The primary folder stays
//! the project's identity, so it can't be removed (another window opens a
//! project that starts elsewhere).

use super::*;
use crate::project_folders::AddError;

impl Workspace {
    /// Add Folder to Project…: asks for folders, then adds them.
    pub(super) fn add_folder_to_project(&mut self, _: &AddFolderToProject, window: &mut Window, cx: &mut Context<Self>) {
        if self.folders.is_none() {
            self.prompt_for_folder(OpenIn::Auto, window, cx);
            return;
        }
        // A remote project's folders are on its host: browsed there.
        if let Some(host) = self.primary_folder().and_then(kuana_core::remote::connection::host_of) {
            match kuana_core::remote::connection::connected(&host) {
                Some(connected) => self.ask_for_remote_folder(connected, OpenIn::AddToProject, window, cx),
                None => self.toast(ToastKind::Info, "Not connected", Some("Reconnect to add a folder from that machine.".into()), cx),
            }
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: true, prompt: Some("Add to Project".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |ws, window, cx| ws.add_folders(paths, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Adds folders to the project, saying why one can't be.
    pub(super) fn add_folders(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut project) = self.folders.clone() else {
            if let Some(first) = paths.into_iter().next() {
                self.set_root(first, window, cx);
            }
            return;
        };
        let mut added = Vec::new();
        for path in paths {
            let path = crate::paths::canonicalize(&path).unwrap_or(path);
            if !path.is_dir_on_host() {
                self.toast(ToastKind::Error, "Not a folder", Some(clone_dialog::display_path(&path)), cx);
                continue;
            }
            match project.add(path.clone()) {
                Ok(()) => added.push(path),
                Err(AddError::Duplicate) => self.toast(ToastKind::Info, format!("{} is in the project already", folder_name(&path)), None, cx),
                Err(AddError::Nested(other)) => {
                    let (inner, outer) = if path.starts_with(&other) { (&path, &other) } else { (&other, &path) };
                    let detail = format!("{} is inside {}. A project's folders can't hold one another.", folder_name(inner), folder_name(outer));
                    self.toast(ToastKind::Info, format!("Can't add {}", folder_name(&path)), Some(detail), cx);
                }
                Err(AddError::OtherHost) => {
                    let detail = "A project's folders are all on one machine. Open it in a new window instead.".to_string();
                    self.toast(ToastKind::Info, format!("{} is on another machine", folder_name(&path)), Some(detail), cx);
                }
            }
        }
        if added.is_empty() {
            return;
        }
        self.folders = Some(project);
        self.folders_changed(&added, &[], window, cx);
        // Show what was added.
        self.set_view_shown(SidebarMode::Files, true);
        if let Some(first) = added.first() {
            self.tree.update(cx, |t, cx| t.reveal_and_scroll(first, cx));
        }
    }

    /// Remove Folder from Project (a folder row's menu). Its open tabs stay,
    /// as files outside the project.
    pub(super) fn remove_folder(&mut self, folder: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.folders.as_mut() else { return };
        if folder == project.primary() {
            self.toast(ToastKind::Info, "The project's first folder stays", Some("It's the project's own: open the others in a window of their own instead.".into()), cx);
            return;
        }
        if !project.remove(folder) {
            return;
        }
        let removed = [folder.to_path_buf()];
        self.folders_changed(&[], &removed, window, cx);
    }

    /// Everything that follows the project's folders: the tree, the file
    /// index and watching, language servers, project settings, trust, lists
    /// kept up to date, extensions, the session.
    fn folders_changed(&mut self, added: &[PathBuf], removed: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.folders.clone() else { return };
        let folders = project.folders().to_vec();
        self.tree.update(cx, |t, cx| t.update_roots(folders.clone(), cx));
        self.problems.update(cx, |p, cx| p.set_project(Some(project.clone()), cx));
        // Files: the removed folders' go, the added ones are scanned.
        self.files = self.files.keeping(&folders);
        self.folders_gone.retain(|f| folders.contains(f));
        self.refresh_file_index(cx);
        self.start_watching(cx);
        // A server rooted in a folder that came or went starts again with its new root.
        self.lsp.update(cx, |s, cx| {
            s.set_folders(folders.clone());
            for folder in added.iter().chain(removed) {
                s.restart_under(folder, cx);
            }
        });
        for folder in added {
            crate::project_settings::layers(folder);
        }
        self.check_config(cx);
        self.tests_files_changed(added.to_vec(), cx);
        self.todo_files_changed(added.iter().chain(removed).cloned(), cx);
        self.refresh_extension_views(cx);
        self.extension_callers_changed(cx);
        self.update_recent_projects_for(&project, cx);
        self.schedule_session_save(cx);
        self.save_state(cx);
        // A folder that isn't trusted is asked about (its tools don't run until then).
        if added.iter().any(|f| !crate::trust::is_trusted(f)) && self.modals.current.is_none() {
            self.open_trust_dialog(window, cx);
        }
        cx.notify();
    }

    /// The tree's empty space, right-clicked with several folders.
    pub(super) fn open_tree_space_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let entries = vec![
            MenuEntry::item("Add Folder to Project…", Icon::FolderPlus).shortcut(binding_label(&AddFolderToProject, window)),
            MenuEntry::item("Collapse All", Icon::ChevronsDownUp),
        ];
        let actions = vec![Some(MenuAction::Run(Box::new(AddFolderToProject))), Some(MenuAction::Tree(TreeAction::CollapseAll))];
        self.show_context_menu(entries, actions, position, None, window, cx);
    }
}
