//! Git: running operations, refreshing status, diffs and blame.

use std::collections::BTreeSet;

use super::*;

impl Workspace {
    // ---- Git -------------------------------------------------------------------------

    pub(super) fn detect_repo(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let project = self.root.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    if !git::is_available() {
                        return None;
                    }
                    let repo = git::discover(&dir)?;
                    let user = git::user_name(&repo);
                    let remote = git::remote_url(&repo);
                    Some((repo, user, remote))
                })
                .await;
            this.update(cx, |ws, cx| {
                let (repo, user, remote) = found.map_or((None, None, None), |(r, u, o)| (Some(r), u, o));
                ws.repo = repo.clone();
                ws.git_user = user;
                ws.git_remote = remote;
                ws.last_commit = None;
                ws.git_status = Arc::new(RepoStatus::default());
                ws.git_paths = Arc::new(HashMap::new());
                ws.git_ignored = Arc::default();
                // A repository above the project folder: watch its .git too.
                if let (Some(repo), Some(root), Some(watcher)) = (&repo, &ws.root, ws.watcher.as_mut())
                    && !repo.starts_with(root)
                    && let Err(err) = watcher.add(&repo.join(".git"))
                {
                    eprintln!("couldn't watch {}: {err}", repo.display());
                }
                ws.git_log.update(cx, |l, cx| l.set_repo(repo.clone(), cx));
                ws.git_panel.update(cx, |p, cx| {
                    p.set_repo(repo, project, cx);
                    p.set_status(Arc::new(RepoStatus::default()), cx);
                });
                // What was read or asked for is the last repository's.
                ws.status_task = None;
                ws.git_pending = StatusWork::default();
                ws.refresh_git(cx);
                ws.load_diff_bases(ws.editors.clone(), cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads all of `git status` again in the background: a project
    /// opened, `.git` changed (HEAD, the index, refs), a Git operation ran.
    pub(super) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        self.read_status(StatusWork::all(), cx);
    }

    /// The Commit view opens: it lists the whole repository's changes, and
    /// the watcher only sees the project folder's (and `.git`), so one
    /// above the project folder is read again, as is one not watched.
    pub(super) fn refresh_commit_view(&mut self, cx: &mut Context<Self>) {
        let outside = self.repo.as_ref().zip(self.root.as_ref()).is_some_and(|(repo, root)| repo != root);
        if outside || self.watcher.is_none() {
            self.refresh_git(cx);
        }
    }

    /// Kuana wrote these files (saves, file operations): the watcher
    /// reports them like any change, unless there's none for the project
    /// or they're outside it; those get their status read here.
    pub(super) fn files_written(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        let watched = |path: &Path| self.watcher.is_some() && self.root.as_ref().is_some_and(|root| path.starts_with(root));
        let paths: Vec<PathBuf> = paths.into_iter().filter(|path| !watched(path)).collect();
        if !paths.is_empty() {
            self.read_status(StatusWork::paths(paths), cx);
        }
    }

    /// Reads Git status again in the background, of what `work` covers
    /// (paths are merged into what's known). One read at a time: what's
    /// asked for meanwhile is read together after it.
    pub(super) fn read_status(&mut self, work: StatusWork, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        self.git_pending.add(work);
        if self.status_task.is_some() || self.git_pending.is_empty() {
            return;
        }
        let work = std::mem::take(&mut self.git_pending);
        let (known, ignored) = (self.git_status.clone(), self.git_ignored.clone());
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let repo2 = repo.clone();
            let read = cx.background_executor().spawn(async move { StatusRead::read(&repo2, work, &known, &ignored) }).await;
            this.update(cx, |ws, cx| {
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = ws.status_task.take() {
                    task.detach();
                }
                // Not for another repository opened meanwhile.
                if ws.repo.as_ref() == Some(&repo) {
                    ws.show_status(read, cx);
                }
                // Once it's shown: a read of some paths merges into it.
                ws.read_status(StatusWork::default(), cx);
            })
            .ok();
        }));
    }

    /// Shows a status read in the background: the tree's colors, diff
    /// bases, the Git panel, the diff view.
    fn show_status(&mut self, read: StatusRead, cx: &mut Context<Self>) {
        if let Some((last_commit, stashes)) = read.head {
            self.git_panel.update(cx, |p, cx| p.set_stashes(stashes, cx));
            if self.last_commit != last_commit {
                self.last_commit = last_commit;
                cx.notify();
            }
        }
        let Some(changed) = read.changed else { return };
        let (old, status) = (&self.git_status, &changed.status);
        let moved = (&old.head_oid, &old.branch, old.ahead, old.behind) != (&status.head_oid, &status.branch, status.ahead, status.behind);
        if moved && self.shows(BottomPanel::GitLog) {
            self.git_log.update(cx, |l, cx| l.reload(cx));
        }
        self.git_paths = changed.paths;
        if let Some(ignored) = changed.ignored {
            self.git_ignored = ignored;
        }
        self.decorate_tree(cx);
        self.git_status = Arc::new(changed.status);
        let new = self.git_status.clone();
        // Diff bases are read again only where HEAD or a file's index blob
        // moved (commits, staging, checkouts), not for every change in the
        // working tree.
        let stale: Vec<Entity<Editor>> = if changed.head_moved {
            self.editors.clone()
        } else {
            self.editors
                .iter()
                .filter(|t| t.read(cx).doc.read(cx).path.as_ref().is_some_and(|p| changed.blobs_moved.contains(p)))
                .cloned()
                .collect()
        };
        self.load_diff_bases(stale, cx);
        for diff in self.diff_views(cx) {
            diff.update(cx, |v, cx| v.status_changed(&new, cx));
        }
        self.git_panel.update(cx, |p, cx| p.set_status(new, cx));
        cx.notify();
    }

    /// Loads the file's HEAD and index text so the editor can show changed lines.
    pub(super) fn load_diff_base(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        self.load_diff_bases(vec![editor.clone()], cx);
    }

    /// Loads several files' HEAD and index text with one `git cat-file`,
    /// once per document however many tabs show it (they share its diff).
    pub(super) fn load_diff_bases(&mut self, editors: Vec<Entity<Editor>>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        for editor in editors {
            // A tab not read yet gets it when it is (`file_opened`).
            let doc = editor.read(cx).doc.clone();
            let d = doc.read(cx);
            let Some(path) = d.path.clone().filter(|_| editor.read(cx).is_loaded()) else { continue };
            let encoding = d.encoding;
            let diff = self.documents.update(cx, |s, cx| s.diff_for(&doc, cx));
            editor.update(cx, |e, cx| e.set_git_diff(diff.clone(), cx));
            if !seen.insert(doc.entity_id()) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&repo) else {
                diff.update(cx, |d, cx| d.set_base(None, None, None, cx));
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            // Compared as the editor holds the file: decoded like it, LF line ends.
            wanted.push((diff.downgrade(), rel, encoding));
        }
        if wanted.is_empty() {
            return;
        }
        let user = self.git_user.clone();
        let head = self.git_status.head_oid.clone();
        cx.spawn(async move |_, cx| {
            let specs: Vec<String> = wanted.iter().flat_map(|(_, rel, _)| [format!("HEAD:{rel}"), format!(":{rel}")]).collect();
            let repo2 = repo.clone();
            let blobs = cx.background_executor().spawn(async move { git::read_blobs(&repo2, &specs) }).await.unwrap_or_default();
            let mut blobs = blobs.into_iter();
            for (diff, rel, encoding) in wanted {
                let text = |bytes: Option<Option<Vec<u8>>>| bytes.flatten().map(|b| git::BlobText::new(&b, Some(encoding)).text);
                let (base, index) = (text(blobs.next()), text(blobs.next()));
                let git_file = GitFile { root: repo.clone(), rel, user_name: user.clone(), head: head.clone() };
                diff.update(cx, |d, cx| d.set_base(Some(git_file), base, index, cx)).ok();
            }
        })
        .detach();
    }

    /// Picks up on-disk changes (checkout, pull, rollback) in unmodified tabs.
    pub(super) fn reload_changed_files(&mut self, cx: &mut Context<Self>) {
        self.reload_documents(|_| true, cx);
        for image in self.images(cx) {
            image.update(cx, |i, cx| i.reload_if_changed(cx));
        }
        // The watcher tells the tree what changed, if there's one.
        if self.watcher.is_none() {
            self.tree.update(cx, |t, cx| t.relist_all(cx));
        }
    }

    pub(super) fn run_git(&mut self, op: GitOp, window: &mut Window, cx: &mut Context<Self>) {
        // Push asks first, like JetBrains: the commits to push, then Push or Cancel.
        if matches!(op, GitOp::Push) {
            self.show_push(window, cx);
            return;
        }
        // Marking a file resolved while it still has conflict markers is
        // usually a mistake, so ask.
        if let (GitOp::Stage(rels), Some(repo)) = (&op, &self.repo) {
            let unresolved: Vec<&String> = rels
                .iter()
                .filter(|rel| self.git_status.conflicted().any(|f| &f.rel == *rel))
                .filter(|rel| std::fs::read_to_string(repo.join(rel)).is_ok_and(|t| !crate::conflict::parse(&t).is_empty()))
                .collect();
            if !unresolved.is_empty() {
                let message = match unresolved.as_slice() {
                    [one] => format!("{one} still has conflict markers"),
                    many => format!("{} files still have conflict markers", many.len()),
                };
                let confirmed = ask(window, &message, "Mark as resolved anyway?", "Mark as Resolved", true, cx);
                cx.spawn_in(window, async move |this, cx| {
                    if confirmed.await {
                        this.update_in(cx, |ws, window, cx| ws.execute_git(op, window, cx)).ok();
                    }
                })
                .detach();
                return;
            }
        }
        if let Some((message, detail, button, undoable)) = confirmation(&op) {
            self.confirm_git(op, &message, detail, button, undoable, window, cx);
            return;
        }
        // Deleting a branch whose commits aren't merged loses them, so look first.
        if let (GitOp::DeleteBranch { name, force: false }, Some(repo)) = (&op, self.repo.clone()) {
            let name = name.clone();
            cx.spawn_in(window, async move |this, cx| {
                let check = name.clone();
                let merged = cx.background_executor().spawn(async move { git::is_merged(&repo, &check) }).await;
                this.update_in(cx, |ws, window, cx| {
                    let (detail, force) = if merged {
                        ("Its commits are all in the current branch.", false)
                    } else {
                        ("It has commits that aren't in the current branch; they'll be lost unless another branch has them.", true)
                    };
                    let op = GitOp::DeleteBranch { name: name.clone(), force };
                    ws.confirm_git(op, &format!("Delete branch {name}?"), detail, "Delete", !force, window, cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        if let GitOp::Discard(files) = &op {
            // Rolling back loses changes, so ask first.
            let message = match files.as_slice() {
                [one] => format!("Roll back changes to {}?", one.rel),
                many => format!("Roll back changes to {} files?", many.len()),
            };
            let untracked = files.iter().filter(|f| f.unstaged == Some(Change::Untracked)).count();
            let detail = if untracked == files.len() {
                "Untracked files will be moved to the Trash."
            } else if untracked > 0 {
                "Untracked files will be moved to the Trash; other local changes will be lost. This can't be undone."
            } else {
                "Local changes will be lost. This can't be undone."
            };
            self.confirm_git(op, &message, detail, "Roll Back", false, window, cx);
            return;
        }
        // Untracked files a stash holds that exist again would leave it half
        // applied (Git restores the rest and keeps the stash), so look first.
        if let (&GitOp::Unstash { index, pop }, Some(repo)) = (&op, self.repo.clone()) {
            cx.spawn_in(window, async move |this, cx| {
                let blockers = cx.background_executor().spawn(async move { git::stash_blockers(&repo, index) }).await;
                this.update_in(cx, |ws, window, cx| {
                    if blockers.is_empty() {
                        ws.execute_git(op, window, cx);
                    } else {
                        ws.clear_stash_blockers(blockers, index, pop, window, cx);
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        self.execute_git(op, window, cx);
    }

    /// Asks before `op`, then runs it.
    #[allow(clippy::too_many_arguments)]
    fn confirm_git(&mut self, op: GitOp, message: &str, detail: &str, button: &str, undoable: bool, window: &mut Window, cx: &mut Context<Self>) {
        let confirmed = ask(window, message, detail, button, undoable, cx);
        cx.spawn_in(window, async move |this, cx| {
            if confirmed.await {
                this.update_in(cx, |ws, window, cx| ws.execute_git(op, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Untracked files of a stash exist in the working tree again: offers to
    /// move them to the Trash so the whole stash applies, instead of Git
    /// applying the rest and keeping the stash.
    fn clear_stash_blockers(&mut self, blockers: Vec<String>, index: usize, pop: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let verb = if pop { "Pop" } else { "Apply" };
        let (message, detail) = match blockers.as_slice() {
            [one] => (
                format!("“{one}” already exists"),
                format!(
                    "The stash has its own copy, and Git won't replace the file: it would apply the rest of the stash and keep it. Move the existing file to the {}, then {} the stash?",
                    trash_name(),
                    verb.to_lowercase(),
                ),
            ),
            many => (
                format!("{} files from the stash already exist", many.len()),
                format!(
                    "{}\n\nThe stash has its own copies, and Git won't replace the files: it would apply the rest of the stash and keep it. Move the existing files to the {}, then {} the stash?",
                    list_some(many),
                    trash_name(),
                    verb.to_lowercase(),
                ),
            ),
        };
        let confirmed = ask(window, &message, &detail, &format!("Move to {} and {verb}", trash_name()), true, cx);
        cx.spawn_in(window, async move |this, cx| {
            if !confirmed.await {
                return;
            }
            this.update_in(cx, |ws, window, cx| {
                for rel in &blockers {
                    if let Err(err) = crate::file_history::move_to_trash(&repo.join(rel)) {
                        ws.toast(ToastKind::Error, format!("Couldn't move {rel} to the {}", trash_name()), Some(err.to_string()), cx);
                        return;
                    }
                }
                ws.execute_git(GitOp::Unstash { index, pop }, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn execute_git(&mut self, op: GitOp, window: &mut Window, cx: &mut Context<Self>) {
        let dir = match (&op, &self.repo, &self.root) {
            (GitOp::Init, _, Some(root)) => root.clone(),
            (_, Some(repo), _) => repo.clone(),
            _ => return,
        };
        if self.git_busy.is_running() {
            self.git_queue.push_back(op);
            return;
        }
        // Without a strategy of its own, a pull takes the setting's.
        let op = match op {
            GitOp::Pull(None) => GitOp::Pull(Settings::get(cx).pull_strategy),
            op => op,
        };
        let label = op.busy_label();
        self.git_busy.set(Some(label.into()), |ws: &mut Self| &mut ws.git_busy, cx);
        self.git_panel.update(cx, |p, cx| p.set_busy(Some(label.into()), cx));
        let status = self.git_status.clone();
        let op2 = op.clone();
        self.git_op_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { perform_git(&dir, op2, &status) }).await;
            this.update_in(cx, |ws, window, cx| {
                ws.git_busy.set(None, |ws: &mut Self| &mut ws.git_busy, cx);
                ws.git_panel.update(cx, |p, cx| p.set_busy(None, cx));
                match result {
                    Ok(Outcome::Done(message)) => {
                        if let Some(message) = message {
                            // A merge or rebase that stopped on conflicts isn't done yet.
                            let kind = if message.contains("conflicts") { ToastKind::Info } else { ToastKind::Success };
                            ws.toast(kind, message, None, cx);
                        }
                        match &op {
                            GitOp::Commit { .. } => ws.git_panel.update(cx, |p, cx| p.committed(cx)),
                            GitOp::Init => {
                                if let Some(root) = ws.root.clone() {
                                    ws.detect_repo(root, cx);
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(Outcome::Nothing(message)) => ws.toast(ToastKind::Info, message, None, cx),
                    Ok(Outcome::Diverged { branch, upstream, ahead, behind }) => match &op {
                        // Nothing says how to combine them (the setting or the Git config): ask, once.
                        GitOp::Pull(None) => ws.choose_pull_strategy(&branch, &upstream, ahead, behind, window, cx),
                        _ => {
                            let detail = format!(
                                "{branch} has {} that {upstream} doesn't. Merge or rebase it from Branches…, or change the pull strategy in Settings → Git.",
                                commits(ahead),
                            );
                            ws.toast(ToastKind::Error, format!("Can't fast-forward {branch} to {upstream}"), Some(detail), cx);
                        }
                    },
                    Ok(Outcome::PartlyApplied { not_restored, error }) => {
                        // Git applies the changes to tracked files first, then restores
                        // untracked ones, skipping those that exist (or stopping).
                        let what = match not_restored.as_slice() {
                            [] => format!("Tracked files are updated; restoring untracked ones stopped ({error})."),
                            [one] => format!("{one} wasn't restored: it already exists. The rest is applied."),
                            many => format!("{} untracked files weren't restored: they already exist. The rest is applied.", many.len()),
                        };
                        let detail = format!("{what} The stash is kept: don't apply it again, drop it when you're done.");
                        ws.toast(ToastKind::Error, "The stash was only partly applied", Some(detail), cx);
                    }
                    Err(err) => {
                        let title = match &op {
                            GitOp::Push | GitOp::PushNow { .. } | GitOp::Commit { push: true, .. } => "Push failed",
                            GitOp::Pull(_) => "Update failed",
                            GitOp::Fetch => "Fetch failed",
                            GitOp::Commit { .. } => "Commit failed",
                            GitOp::Checkout(_) | GitOp::CreateBranch(_) | GitOp::CheckoutCommit(_) | GitOp::BranchAt { .. } => {
                                "Checkout failed"
                            }
                            GitOp::CherryPick(_) => "Cherry-pick failed",
                            GitOp::Revert(_) => "Revert failed",
                            GitOp::Reset { .. } => "Reset failed",
                            GitOp::Stash { .. } => "Stash failed",
                            GitOp::Unstash { .. } => "Couldn't apply the stash",
                            _ => "Git operation failed",
                        };
                        ws.toast(ToastKind::Error, title, Some(err), cx);
                    }
                }
                if matches!(
                    op,
                    GitOp::Discard(_)
                        | GitOp::Pull(_)
                        | GitOp::Checkout(_)
                        | GitOp::Merge(_)
                        | GitOp::Rebase(_)
                        | GitOp::Abort(_)
                        | GitOp::Continue(_)
                        | GitOp::Stash { .. }
                        | GitOp::Unstash { .. }
                        | GitOp::CheckoutCommit(_)
                        | GitOp::BranchAt { .. }
                        | GitOp::CherryPick(_)
                        | GitOp::Revert(_)
                        | GitOp::Reset { .. }
                ) {
                    ws.reload_changed_files(cx);
                }
                if ws.shows(BottomPanel::GitLog)
                    && matches!(
                        op,
                        GitOp::Commit { .. }
                            | GitOp::Fetch
                            | GitOp::Pull(_)
                            | GitOp::Push
                            | GitOp::PushNow { .. }
                            | GitOp::Checkout(_)
                            | GitOp::CreateBranch(_)
                            | GitOp::Merge(_)
                            | GitOp::Rebase(_)
                            | GitOp::Abort(_)
                            | GitOp::Continue(_)
                            | GitOp::RemoveRemote(_)
                            | GitOp::EditRemote { .. }
                            | GitOp::DeleteBranch { .. }
                            | GitOp::RenameBranch { .. }
                            | GitOp::Stash { .. }
                            | GitOp::CheckoutCommit(_)
                            | GitOp::BranchAt { .. }
                            | GitOp::TagAt { .. }
                            | GitOp::CherryPick(_)
                            | GitOp::Revert(_)
                            | GitOp::Reset { .. }
                    )
                {
                    ws.git_log.update(cx, |l, cx| l.reload(cx));
                }
                // Staging part of a file may leave its status as it was.
                if matches!(op, GitOp::StageLines { .. } | GitOp::UnstageLines { .. }) {
                    ws.load_diff_bases(ws.editors.clone(), cx);
                }
                for diff in ws.diff_views(cx) {
                    diff.update(cx, |v, cx| v.reload(cx));
                }
                if let Some(Modal::Remotes(dialog)) = &ws.modals.current {
                    dialog.update(cx, |d, cx| d.reload(cx));
                }
                if op.changes_remotes()
                    && let Some(repo) = &ws.repo
                {
                    ws.git_remote = git::remote_url(repo);
                }
                ws.status_task = None;
                ws.refresh_git(cx);
                // Then what was started meanwhile (this task is running: let
                // it finish rather than drop it for the next one's).
                if let Some(next) = ws.git_queue.pop_front() {
                    if let Some(task) = ws.git_op_task.take() {
                        task.detach();
                    }
                    ws.execute_git(next, window, cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn toast(&mut self, kind: ToastKind, message: impl Into<SharedString>, detail: Option<String>, cx: &mut Context<Self>) {
        self.toast_with_button(kind, message, detail, None, cx);
    }

    /// A toast with a button for what it reports.
    pub(super) fn toast_with_button(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        detail: Option<String>,
        button: Option<ToastButton>,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, kind, message: message.into(), detail: detail.map(Into::into), button, leaving: false });
        let shown: Vec<usize> = self.toasts.iter().filter(|t| !t.leaving).map(|t| t.id).collect();
        if let Some(&oldest) = shown.first().filter(|_| shown.len() > 4) {
            self.dismiss_toast(oldest, cx);
        }
        // Long enough to reach for its button.
        let seconds = if kind == ToastKind::Error || button.is_some() { 9 } else { 4 };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(seconds)).await;
            this.update(cx, |ws, cx| ws.dismiss_toast(id, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    /// Fades a toast out, then removes it.
    pub(super) fn dismiss_toast(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id && !t.leaving) else { return };
        if Theme::get(cx).reduce_effects {
            self.toasts.retain(|t| t.id != id);
        } else {
            toast.leaving = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(transition::TOAST).await;
                this.update(cx, |ws, cx| {
                    ws.toasts.retain(|t| t.id != id);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    pub(super) fn git_push(&mut self, _: &GitPush, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Push, window, cx);
    }

    pub(super) fn git_pull(&mut self, _: &GitPull, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Pull(None), window, cx);
    }

    /// A pull found commits on both sides and nothing that says how to
    /// combine them: asks (like JetBrains' first Update Project), saves the
    /// answer as the pull strategy and pulls with it.
    fn choose_pull_strategy(&mut self, branch: &str, upstream: &str, ahead: u32, behind: u32, window: &mut Window, cx: &mut Context<Self>) {
        let message = format!("{branch} and {upstream} both have new commits");
        let detail = format!(
            "{upstream} has {} that {branch} doesn't, and {branch} has {} of its own. Merge them with a merge commit, or rebase yours on top of the incoming ones?\n\nThe answer is kept as the pull strategy (Settings → Git).",
            commits(behind),
            commits(ahead),
        );
        let answer = window.prompt(PromptLevel::Info, &message, Some(&detail), &["Merge", "Rebase", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let strategy = match answer.await {
                Ok(0) => git::PullStrategy::Merge,
                Ok(1) => git::PullStrategy::Rebase,
                _ => return,
            };
            this.update_in(cx, |ws, window, cx| {
                let mut problems: Vec<String> = Settings::set("pull_strategy", strategy.key().into())
                    .err()
                    .map(|e| format!("settings.json: {e}"))
                    .into_iter()
                    .collect();
                problems.extend(settings::reload_now(cx));
                if !problems.is_empty() {
                    ws.report_config_problems(problems, cx);
                }
                ws.run_git(GitOp::Pull(Some(strategy)), window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn git_fetch(&mut self, _: &GitFetch, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Fetch, window, cx);
    }

    pub(super) fn git_branches(&mut self, _: &GitBranches, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        let picker = cx.new(|cx| BranchPicker::new(repo, window, cx));
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &BranchPickerEvent, window, cx| match event {
            BranchPickerEvent::Run(op) => {
                ws.close_modal(window, cx);
                ws.run_git(op.clone(), window, cx);
            }
            BranchPickerEvent::Rename(name) => ws.ask_for_name(PendingInput::Git(GitPrompt::RenameBranch(name.clone())), window, cx),
            BranchPickerEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Branches(picker), subscription);
        cx.notify();
    }

    pub(super) fn git_stash(&mut self, _: &GitStash, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo.is_none() {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
        } else if self.git_status.files.is_empty() {
            self.toast(ToastKind::Info, "No changes to stash", None, cx);
        } else {
            self.ask_for_name(PendingInput::Git(GitPrompt::Stash), window, cx);
        }
    }

    pub(super) fn manage_remotes(&mut self, _: &ManageRemotes, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| RemotesDialog::new(repo, window, cx));
        let subscription = cx.subscribe_in(&dialog, window, |ws, _, event: &RemotesEvent, window, cx| match event {
            // The dialog stays open; `execute_git` reloads it.
            RemotesEvent::Run(op) => ws.run_git(op.clone(), window, cx),
            RemotesEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Remotes(dialog), subscription);
        cx.notify();
    }

    pub(super) fn git_clone(&mut self, _: &GitClone, window: &mut Window, cx: &mut Context<Self>) {
        if !git::is_available() {
            self.toast(ToastKind::Error, "Git isn't installed", Some("Install Git and make sure it's on your PATH.".into()), cx);
            return;
        }
        self.close_modal(window, cx);
        let parent = clone_dialog::default_parent(self.root.as_deref());
        let dialog = cx.new(|cx| CloneDialog::new(parent, window, cx));
        let subscription = cx.subscribe_in(&dialog, window, |ws, _, event: &CloneDialogEvent, window, cx| match event {
            CloneDialogEvent::Cloned(path) => {
                ws.close_modal(window, cx);
                ws.toast(ToastKind::Success, format!("Cloned {}", folder_name(path)), Some(clone_dialog::display_path(path)), cx);
                ws.open_project(path.clone(), OpenIn::Auto, window, cx);
            }
            CloneDialogEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Clone(dialog), subscription);
        cx.notify();
    }

    pub(super) fn git_init(&mut self, _: &GitInit, window: &mut Window, cx: &mut Context<Self>) {
        match (&self.root, &self.repo) {
            (None, _) => self.toast(ToastKind::Error, "Open a folder first", None, cx),
            (Some(_), Some(_)) => self.toast(ToastKind::Error, "This folder is already in a Git repository", None, cx),
            (Some(_), None) => self.run_git(GitOp::Init, window, cx),
        }
    }

    pub(super) fn open_remote(&mut self, _: &OpenRemote, _: &mut Window, cx: &mut Context<Self>) {
        match self.git_remote.as_deref().and_then(git::web_url) {
            Some(url) => cx.open_url(&url),
            None => self.toast(ToastKind::Error, "No browsable remote", Some("This repository has no HTTPS or SSH remote.".into()), cx),
        }
    }

    pub(super) fn remove_recent_project(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.update_recent_projects(|list| list.retain(|p| p != path), cx);
        cx.notify();
    }

    /// Opens a recent project here, or in a new window when `new_window`
    /// (⌘/Ctrl held).
    pub(super) fn open_recent_project(&mut self, path: PathBuf, new_window: bool, window: &mut Window, cx: &mut Context<Self>) {
        let target = if new_window { OpenIn::NewWindow } else { OpenIn::ThisWindow };
        self.open_project(path, target, window, cx);
    }

    pub(super) fn recent_projects(&mut self, _: &RecentProjects, window: &mut Window, cx: &mut Context<Self>) {
        self.recent_projects = SessionState::load().recent_projects;
        self.open_picker(PickerMode::Projects, window, cx);
    }

    pub(super) fn open_diff(&mut self, file: FileStatus, staged: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.show_diff(DiffSource::Working { file, staged }, window, cx);
    }

    pub(super) fn show_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        self.diff_return = None;
        let dialog = cx.new(|cx| PushDialog::new(repo, window, cx));
        self.open_push_modal(dialog, window, cx);
    }

    pub(super) fn open_push_modal(&mut self, dialog: Entity<PushDialog>, window: &mut Window, cx: &mut Context<Self>) {
        let subscription = cx.subscribe_in(&dialog, window, |ws, dialog, event: &PushDialogEvent, window, cx| match event {
            PushDialogEvent::Push { force, remote } => {
                ws.close_modal(window, cx);
                ws.run_git(GitOp::PushNow { force: *force, remote: remote.clone() }, window, cx);
            }
            PushDialogEvent::OpenDiff(source) => {
                ws.close_modal(window, cx);
                if let Some(diff) = ws.show_diff(source.clone(), window, cx) {
                    ws.diff_return = Some((diff.entity_id(), dialog.clone()));
                }
            }
            PushDialogEvent::ManageRemotes => ws.manage_remotes(&ManageRemotes, window, cx),
            PushDialogEvent::Dismissed => ws.close_modal(window, cx),
        });
        dialog.read(cx).focus(window);
        self.modals.open(Modal::Push(dialog), subscription);
        cx.notify();
    }

    /// Shows a diff in a tab of the current pane, as opening a file does:
    /// the tab already showing it (a Local History version replaces the
    /// version its file's tab shows), else a new one.
    pub(super) fn show_diff(&mut self, source: DiffSource, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<DiffView>> {
        // A Local History version needs no repository: paths show from the project.
        let repo = match &source {
            DiffSource::History { path, .. } => self.root.clone().or_else(|| path.parent().map(Path::to_path_buf)),
            _ => self.repo.clone(),
        };
        let repo = repo?;
        let document = source.live_file().and_then(|path| self.documents.read(cx).get(path, cx));
        if let Some(diff) = self.diff_views(cx).into_iter().find(|d| d.read(cx).source().same_tab(&source)) {
            diff.update(cx, |d, cx| d.set_source(source, document, cx));
            self.activate_item(&PaneItem::Diff(diff.clone()), window, cx);
            return Some(diff);
        }
        let side_by_side = SessionState::load().diff_side_by_side.unwrap_or(true);
        let diff = cx.new(|cx| DiffView::new(repo, source, document, side_by_side, cx));
        self.add_tab(PaneItem::Diff(diff.clone()), window, cx);
        Some(diff)
    }

    /// Every diff tab, in pane order.
    pub(super) fn diff_views(&self, cx: &App) -> Vec<Entity<DiffView>> {
        self.items(cx).into_iter().filter_map(|t| t.diff().cloned()).collect()
    }

    /// A new diff tab: the workspace runs what it asks for.
    pub(super) fn add_diff(&mut self, diff: &Entity<DiffView>, window: &mut Window, cx: &mut Context<Self>) {
        let subscription = cx.subscribe_in(diff, window, |ws, diff, event: &DiffViewEvent, window, cx| match event {
            DiffViewEvent::Close => ws.close_diff(diff, window, cx),
            DiffViewEvent::OpenFile(path) => ws.open_path(path.clone(), window, cx),
            DiffViewEvent::Restore { path, text } => {
                // The version becomes the file: nothing left to compare.
                ws.remove_item(&PaneItem::Diff(diff.clone()), window, cx);
                ws.restore_version(path.clone(), text.clone(), window, cx);
            }
            // A change staged or discarded reloads the diff, or closes it
            // once nothing is left (`DiffView::status_changed`).
            DiffViewEvent::Run(op) => ws.run_git(op.clone(), window, cx),
            DiffViewEvent::Layout { side_by_side } => {
                // App-wide, like the font zoom.
                let side_by_side = *side_by_side;
                SessionState::change_soon("diff_side_by_side", 0, move |state| state.diff_side_by_side = Some(side_by_side));
                ws.state_task = Some(crate::state::write_soon(cx));
            }
        });
        // Dropped with the tab (see `remove_item`).
        self.tab_subscriptions.insert(diff.entity_id(), vec![subscription]);
    }

    /// Closes a diff's tab; one opened from the Push dialog goes back to it.
    pub(super) fn close_diff(&mut self, diff: &Entity<DiffView>, window: &mut Window, cx: &mut Context<Self>) {
        let back = self.diff_return.take_if(|(id, _)| *id == diff.entity_id());
        self.remove_item(&PaneItem::Diff(diff.clone()), window, cx);
        if let Some((_, dialog)) = back {
            self.open_push_modal(dialog, window, cx);
        }
    }

    pub(super) fn toggle_problems(&mut self, _: &ToggleProblems, window: &mut Window, cx: &mut Context<Self>) {
        // The terminal it replaces had the keyboard; the file gets it back.
        let had_terminal = self.shows(BottomPanel::Terminal);
        self.toggle_bottom(BottomPanel::Problems);
        if had_terminal && self.shows(BottomPanel::Problems) {
            self.focus_active(window, cx);
        }
        self.save_state(cx);
        cx.notify();
    }
}



/// Git status to read: all of it, or what's under some paths (files Kuana
/// wrote, changes in the work tree).
#[derive(Debug, Default)]
pub(super) struct StatusWork {
    pub(super) all: bool,
    pub(super) paths: BTreeSet<PathBuf>,
}

impl StatusWork {
    pub(super) fn all() -> Self {
        StatusWork { all: true, paths: BTreeSet::new() }
    }

    pub(super) fn paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        StatusWork { all: false, paths: paths.into_iter().collect() }
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.all && self.paths.is_empty()
    }

    pub(super) fn add(&mut self, more: StatusWork) {
        self.all |= more.all;
        if self.all {
            self.paths.clear();
        } else {
            self.paths.extend(more.paths);
        }
    }

    pub(super) fn add_path(&mut self, path: PathBuf) {
        if !self.all {
            self.paths.insert(path);
        }
    }
}

/// A Git status read in the background, with what showing it needs.
struct StatusRead {
    /// With all of it read: the last commit and the stashes.
    head: Option<(Option<git::CommitInfo>, Vec<git::Stash>)>,
    /// When it differs from the status shown.
    changed: Option<ChangedStatus>,
}

struct ChangedStatus {
    status: RepoStatus,
    /// Its colors for the tree (`RepoStatus::path_map`).
    paths: Arc<HashMap<PathBuf, Change>>,
    /// What Git ignores, when that changed.
    ignored: Option<Arc<HashSet<PathBuf>>>,
    /// HEAD moved: every file's diff base may have.
    head_moved: bool,
    /// The files whose HEAD or index blob moved: their diff bases did.
    blobs_moved: HashSet<PathBuf>,
}

impl StatusRead {
    /// Reads the status (blocking): all of it, or `work`'s paths', merged
    /// into `known` (the status shown, `ignored` its ignored set).
    fn read(repo: &Path, work: StatusWork, known: &RepoStatus, ignored: &HashSet<PathBuf>) -> Self {
        let pathspecs = if work.all { None } else { git::pathspecs(repo, work.paths.iter().map(PathBuf::as_path)) };
        let (status, head) = match pathspecs {
            // All outside the repository.
            Some(rels) if rels.is_empty() => return StatusRead { head: None, changed: None },
            Some(rels) => {
                let status = git::status_of(repo, &rels).map(|scoped| {
                    let mut status = known.clone();
                    status.merge(repo, &rels, scoped);
                    status
                });
                (status, None)
            }
            None => (git::status(repo), Some((git::last_commit(repo), git::stashes(repo)))),
        };
        let changed = status.ok().filter(|status| status != known).map(|status| {
            let blobs = |status: &RepoStatus| -> HashMap<PathBuf, String> {
                status.files.iter().filter_map(|f| Some((f.path.clone(), f.blobs.clone()?))).collect()
            };
            let (before, after) = (blobs(known), blobs(&status));
            let blobs_moved = before.keys().chain(after.keys()).filter(|path| before.get(*path) != after.get(*path)).cloned().collect();
            let now_ignored: HashSet<PathBuf> = status.ignored.iter().cloned().collect();
            ChangedStatus {
                paths: Arc::new(status.path_map(repo)),
                ignored: (now_ignored != *ignored).then(|| Arc::new(now_ignored)),
                head_moved: status.head_oid != known.head_oid,
                blobs_moved,
                status,
            }
        });
        StatusRead { head, changed }
    }
}

/// What to ask before an operation that loses something: message, detail,
/// button, and whether it can be undone (else Cancel is the default).
fn confirmation(op: &GitOp) -> Option<(String, &'static str, &'static str, bool)> {
    match op {
        GitOp::RemoveRemote(name) => Some((
            format!("Remove remote {name}?"),
            "Its remote-tracking branches go too; the remote repository itself isn't touched.",
            "Remove",
            true,
        )),
        GitOp::DropStash(index) => Some((format!("Drop stash@{{{index}}}?"), "Its changes will be lost.", "Drop", false)),
        GitOp::Reset { sha, mode: git::ResetMode::Hard } => Some((
            format!("Hard reset to {}?", &sha[..sha.len().min(7)]),
            "Commits after it leave the branch, and every uncommitted change is lost. This can't be undone.",
            "Reset",
            false,
        )),
        _ => None,
    }
}

/// How a Git operation ended, for the workspace to report.
pub(super) enum Outcome {
    /// Finished; the message for a notification, if any.
    Done(Option<String>),
    /// Nothing to do, and why.
    Nothing(String),
    /// A fast-forward-only pull found commits on both sides; nothing changed.
    Diverged { branch: String, upstream: String, ahead: u32, behind: u32 },
    /// A stash applied in part and kept (see `git::Unstashed::Partly`).
    PartlyApplied { not_restored: Vec<String>, error: String },
}

/// "1 commit", "3 commits".
fn commits(n: impl Into<u64>) -> String {
    let n = n.into();
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

/// Up to a few names, one per line, and how many more there are.
fn list_some(names: &[String]) -> String {
    const SHOWN: usize = 6;
    let mut list = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join("\n");
    if names.len() > SHOWN {
        list.push_str(&format!("\nand {} more", names.len() - SHOWN));
    }
    list
}

/// Runs one Git operation; blocking.
pub(super) fn perform_git(dir: &Path, op: GitOp, status: &RepoStatus) -> Result<Outcome, String> {
    let push = |dir: &Path, force: bool, remote: Option<String>| -> Result<String, String> {
        let Some(remote) = remote.or_else(|| git::default_remote(dir)) else {
            return Err("No remote is configured. Add one in Git → Manage Remotes….".into());
        };
        let current = git::status(dir).ok();
        let branch = current.as_ref().and_then(|s| s.branch.clone()).or_else(|| status.branch.clone());
        let has_upstream = current.and_then(|s| s.upstream).is_some();
        let verb = if force { "Force-pushed" } else { "Pushed" };
        match (has_upstream, branch) {
            (false, Some(branch)) => git::push(dir, Some((&remote, &branch)), force).map(|_| format!("{verb} {branch} to {remote}")),
            _ => git::push(dir, None, force).map(|_| verb.to_string()),
        }
    };
    let short = |sha: &str| sha[..sha.len().min(7)].to_string();
    let stopped = |out: &str, what: &str, done: String| Some(if out == "CONFLICT" { format!("{what} stopped on conflicts") } else { done });
    let message = match op {
        GitOp::Stage(rels) => git::stage(dir, &rels).map(|_| None),
        GitOp::Unstage(rels) => git::unstage(dir, &rels, status.head_oid.is_some()).map(|_| None),
        GitOp::StageLines { rel, text, rows } => git::stage_lines(dir, &rel, &text, rows).map(|_| None),
        GitOp::UnstageLines { rel, text, rows } => git::unstage_lines(dir, &rel, text.as_deref(), rows).map(|_| None),
        GitOp::Merge(branch) => git::merge(dir, &branch).map(|out| {
            Some(if out == "CONFLICT" {
                "Merge stopped on conflicts".into()
            } else if out.contains("Already up to date") {
                "Already up to date".into()
            } else {
                format!("Merged {branch}")
            })
        }),
        GitOp::Rebase(onto) => git::rebase(dir, &onto).map(|out| {
            Some(if out == "CONFLICT" { "Rebase stopped on conflicts".into() } else { format!("Rebased onto {onto}") })
        }),
        GitOp::Abort(state) => git::abort(dir, state).map(|_| Some(format!("{} aborted", state.noun()))),
        GitOp::Continue(state) => git::continue_op(dir, state).map(|_| {
            Some(match git::repo_state(dir) {
                // A rebase can stop again on the next commit.
                Some(_) => "Stopped on more conflicts".into(),
                None => format!("{} finished", state.noun()),
            })
        }),
        GitOp::Discard(files) => {
            // Local History keeps what a rollback throws away.
            for file in &files {
                crate::local_history::record_disk(&file.path, crate::local_history::Kind::BeforeRollback);
            }
            let n = files.len();
            git::discard(dir, &files).map(|_| Some(format!("Rolled back {n} file{}", if n == 1 { "" } else { "s" })))
        }
        GitOp::Commit { message, amend, all, untracked, push: and_push } => {
            // Untracked files only when checked: a commit of "everything"
            // mustn't pick up build output or notes nobody chose.
            if all {
                git::stage_tracked(dir)?;
            }
            if !untracked.is_empty() {
                git::stage(dir, &untracked)?;
            }
            let summary = git::commit(dir, &message, amend)?;
            if and_push {
                push(dir, false, None)?;
                Ok(Some(format!("Committed and pushed {summary}")))
            } else {
                Ok(Some(format!("Committed {summary}")))
            }
        }
        // `Push` opens the dialog before it gets here; treat it as a plain push.
        GitOp::Push => push(dir, false, None).map(Some),
        GitOp::PushNow { force, remote } => push(dir, force, remote).map(Some),
        GitOp::Pull(strategy) => {
            return git::pull(dir, strategy).map(|pulled| match pulled {
                git::Pulled::UpToDate => Outcome::Done(Some("Already up to date".into())),
                git::Pulled::FastForwarded(n) => Outcome::Done(Some(format!("Pulled {}", commits(n as u64)))),
                git::Pulled::Merged(n) => Outcome::Done(Some(format!("Pulled {} and merged", commits(n as u64)))),
                git::Pulled::Rebased(n) => Outcome::Done(Some(format!("Pulled {}, yours rebased on top", commits(n as u64)))),
                git::Pulled::Conflicts => Outcome::Done(Some("Update stopped on conflicts".into())),
                git::Pulled::Diverged { branch, upstream, ahead, behind } => Outcome::Diverged { branch, upstream, ahead, behind },
            });
        }
        GitOp::Fetch => git::fetch(dir).map(|_| Some("Fetched all remotes".into())),
        GitOp::Checkout(branch) => git::checkout(dir, &branch).map(|_| Some(format!("Switched to {}", branch.name))),
        GitOp::CreateBranch(name) => git::create_branch(dir, &name).map(|_| Some(format!("Created and switched to {name}"))),
        GitOp::Init => git::init(dir).map(|_| Some("Initialized a Git repository".into())),
        GitOp::AddRemote { name, url } => git::add_remote(dir, &name, &url).map(|_| Some(format!("Added remote {name}"))),
        GitOp::RemoveRemote(name) => git::remove_remote(dir, &name).map(|_| Some(format!("Removed remote {name}"))),
        GitOp::EditRemote { name, new_name, url } => {
            git::set_remote_url(dir, &name, &url)?;
            if new_name != name {
                git::rename_remote(dir, &name, &new_name)?;
            }
            Ok(Some(format!("Updated remote {new_name}")))
        }
        GitOp::DeleteBranch { name, force } => git::delete_branch(dir, &name, force).map(|_| Some(format!("Deleted branch {name}"))),
        GitOp::RenameBranch { from, to } => git::rename_branch(dir, &from, &to).map(|_| Some(format!("Renamed {from} to {to}"))),
        GitOp::Stash { message, untracked } => {
            return git::stash(dir, message.as_deref(), untracked).map(|stashed| match stashed {
                true => Outcome::Done(Some("Stashed changes".into())),
                false if untracked => Outcome::Nothing("No changes to stash".into()),
                false => Outcome::Nothing("No changes to stash besides untracked files".into()),
            });
        }
        GitOp::Unstash { index, pop } => {
            return git::unstash(dir, index, pop).map(|unstashed| match unstashed {
                git::Unstashed::Applied => Outcome::Done(Some(if pop { "Popped stash".into() } else { "Applied stash".into() })),
                // Git keeps a stash that didn't apply cleanly.
                git::Unstashed::Conflicts => Outcome::Done(Some("Applying the stash stopped on conflicts; it's kept".into())),
                git::Unstashed::Partly { error, not_restored } => Outcome::PartlyApplied { not_restored, error },
            });
        }
        GitOp::DropStash(index) => git::drop_stash(dir, index).map(|_| Some("Dropped stash".into())),
        GitOp::CheckoutCommit(sha) => git::checkout_commit(dir, &sha).map(|_| Some(format!("Checked out {} (detached HEAD)", short(&sha)))),
        GitOp::BranchAt { name, sha } => git::create_branch_at(dir, &name, &sha).map(|_| Some(format!("Created and switched to {name}"))),
        GitOp::TagAt { name, sha } => git::create_tag(dir, &name, &sha).map(|_| Some(format!("Tagged {} as {name}", short(&sha)))),
        GitOp::CherryPick(sha) => git::cherry_pick(dir, &sha).map(|out| stopped(&out, "Cherry-pick", format!("Cherry-picked {}", short(&sha)))),
        GitOp::Revert(sha) => git::revert(dir, &sha).map(|out| stopped(&out, "Revert", format!("Reverted {}", short(&sha)))),
        GitOp::Reset { sha, mode } => git::reset(dir, &sha, mode).map(|_| Some(format!("Reset to {}", short(&sha)))),
    };
    message.map(Outcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read of some paths merges into the status shown, and tells what
    /// showing it needs to know.
    #[test]
    fn a_read_of_some_paths_merges_into_the_status_shown() {
        if !git::is_available() {
            return;
        }
        let root = std::env::temp_dir().join(format!("kuana-status-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git::init(&root).unwrap();
        let root = git::discover(&root).unwrap();
        for args in [["config", "user.name", "Test"], ["config", "user.email", "test@example.com"]] {
            git::run(&root, &args).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git::stage(&root, &[".gitignore".into(), "a.txt".into()]).unwrap();
        git::commit(&root, "first", false).unwrap();

        // All of it: with the last commit, and HEAD moved from nothing.
        let read = StatusRead::read(&root, StatusWork::all(), &RepoStatus::default(), &HashSet::new());
        assert!(read.head.as_ref().is_some_and(|(commit, _)| commit.is_some()));
        let shown = read.changed.expect("HEAD is known now");
        assert!(shown.head_moved && shown.status.files.is_empty());

        // A file written and a log: only they are read, and merge in.
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("new.log"), "").unwrap();
        let ignored: HashSet<PathBuf> = shown.status.ignored.iter().cloned().collect();
        let read = StatusRead::read(&root, StatusWork::paths([root.join("a.txt"), root.join("new.log")]), &shown.status, &ignored);
        assert!(read.head.is_none(), "no need for the last commit");
        let changed = read.changed.expect("a.txt changed");
        assert_eq!(changed.status.head_oid, shown.status.head_oid);
        assert_eq!(changed.status.files.iter().map(|f| (f.rel.as_str(), f.unstaged)).collect::<Vec<_>>(), [("a.txt", Some(Change::Modified))]);
        assert!(changed.ignored.is_some_and(|i| i.contains(&root.join("new.log"))), "the tree dims it");
        // Its blobs are known now: the diff base is read again.
        assert!(!changed.head_moved && changed.blobs_moved.iter().eq([&root.join("a.txt")]));
        assert_eq!(changed.paths.get(&root.join("a.txt")), Some(&Change::Modified));

        // Read again unchanged: nothing to show. Outside the repository: nothing read.
        let ignored: HashSet<PathBuf> = changed.status.ignored.iter().cloned().collect();
        assert!(StatusRead::read(&root, StatusWork::paths([root.join("a.txt")]), &changed.status, &ignored).changed.is_none());
        let outside = StatusWork::paths([std::env::temp_dir().join("elsewhere.txt")]);
        assert!(StatusRead::read(&root, outside, &changed.status, &ignored).changed.is_none());

        // Asked for together: all of it covers any paths.
        let mut work = StatusWork::paths([root.join("a.txt")]);
        work.add(StatusWork::all());
        work.add(StatusWork::paths([root.join("b.txt")]));
        assert!(work.all && work.paths.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
