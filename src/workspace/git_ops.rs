//! Git: running operations, refreshing status, diffs and blame.

use super::*;

impl Workspace {
    // ---- Git -------------------------------------------------------------------------

    /// The repository's working-tree root, if the project is in one.
    pub(super) fn repo(&self, cx: &App) -> Option<PathBuf> {
        self.git.read(cx).repo().cloned()
    }

    /// Whether the project is in a Git repository.
    pub(super) fn has_repo(&self, cx: &App) -> bool {
        self.git.read(cx).repo().is_some()
    }

    /// The repository's status as last read.
    pub(super) fn git_status(&self, cx: &App) -> Arc<RepoStatus> {
        self.git.read(cx).status().clone()
    }

    pub(super) fn detect_repo(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let project = self.root.clone();
        self.git.update(cx, |g, cx| g.detect(dir, project, cx));
    }

    /// What the store reports: shown here, in the panels and in the editors.
    pub(super) fn on_git_event(&mut self, _: &Entity<GitStore>, event: &GitEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            GitEvent::RepoChanged => self.repo_changed(cx),
            GitEvent::StatusRead { stashes, changed } => {
                if let Some(stashes) = stashes {
                    self.git_panel.update(cx, |p, cx| p.set_stashes(stashes.clone(), cx));
                }
                if let Some(changed) = changed {
                    self.show_status(changed, cx);
                }
            }
            GitEvent::Started(label) => self.git_panel.update(cx, |p, cx| p.set_busy(Some(label.clone()), cx)),
            GitEvent::Finished { op, result } => self.git_finished(op, result, window, cx),
        }
    }

    /// Another repository (or none) is the project's now.
    fn repo_changed(&mut self, cx: &mut Context<Self>) {
        let (repo, project) = {
            let git = self.git.read(cx);
            (git.repo().cloned(), git.root().cloned())
        };
        // A repository above the project folder: watch its .git too.
        if let (Some(repo), Some(root), Some(watcher)) = (&repo, &self.root, self.watcher.as_mut())
            && !repo.starts_with(root)
            && let Err(err) = watcher.add(&repo.join(".git"))
        {
            eprintln!("couldn't watch {}: {err}", repo.display());
        }
        self.git_log.update(cx, |l, cx| l.set_repo(repo.clone(), cx));
        self.git_panel.update(cx, |p, cx| {
            p.set_repo(repo, project, cx);
            p.set_status(Arc::new(RepoStatus::default()), cx);
        });
        self.load_diff_bases(self.editors.clone(), cx);
        cx.notify();
    }

    /// Opens a conflicted file in the Merge view: its tab's editor in the
    /// middle, the versions from the index's conflict stages around it
    /// (base 1, ours 2, theirs 3; yours is the user's side, `Side::yours`).
    pub(super) fn show_merge(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo(cx) else { return };
        let Some(rel) = path.strip_prefix(&repo).ok().map(|r| r.to_string_lossy().replace('\\', "/")) else { return };
        let read_rel = rel.clone();
        cx.spawn_in(window, async move |this, cx| {
            let stages = cx
                .background_executor()
                .spawn(async move {
                    let stage = |n: u8| git::blob_bytes(&repo, &format!(":{n}:{read_rel}"));
                    let yours = git::Side::yours(git::repo_state(&repo));
                    (stage(1), stage(2), stage(3), yours)
                })
                .await;
            this.update_in(cx, |ws, window, cx| ws.open_merge(path, rel, stages, window, cx)).ok();
        })
        .detach();
    }

    #[allow(clippy::type_complexity)]
    fn open_merge(
        &mut self,
        path: PathBuf,
        rel: String,
        (base, ours, theirs, yours): (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>, git::Side),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ours.is_none() && theirs.is_none() {
            self.toast(ToastKind::Info, format!("{rel} isn't conflicted"), None, cx);
            return;
        }
        let binary = [&base, &ours, &theirs].into_iter().flatten().any(|b| crate::encoding::looks_binary(&b[..b.len().min(8000)]));
        if binary {
            let detail = "Right-click it in the Commit panel for Accept Yours or Accept Theirs.".to_string();
            self.toast(ToastKind::Info, format!("{rel} is a binary file"), Some(detail), cx);
            return;
        }
        let text = |b: Option<Vec<u8>>| b.map(|b| String::from_utf8_lossy(&b).into_owned());
        let (base, ours, theirs) = (text(base).unwrap_or_default(), text(ours), text(theirs));
        // Each side as the markers name it ("HEAD", a branch, "Stashed changes").
        if path.exists() {
            self.open_path(path.clone(), window, cx);
        }
        let Some(file) = self.tab_for(&path, cx) else {
            // Gone from the working tree (deleted on one side): only a whole side resolves it.
            self.toast(ToastKind::Info, format!("{rel} was deleted on one side"), Some("Right-click it in the Commit panel for Accept Yours or Accept Theirs.".into()), cx);
            return;
        };
        let labels = file
            .update(cx, |e, cx| e.conflicts(cx))
            .first()
            .map(|c| (c.ours_label.clone(), c.theirs_label.clone()))
            .unwrap_or_default();
        let (ours, theirs) = (
            crate::merge_view::Version { text: ours, label: labels.0 },
            crate::merge_view::Version { text: theirs, label: labels.1 },
        );
        let (mine, other) = match yours {
            git::Side::Ours => (ours, theirs),
            git::Side::Theirs => (theirs, ours),
        };
        // Already merging it: forward. Another file's merge gives way.
        if let Some((handle, merging, _)) = &self.merge_window
            && *merging == path
            && tool_window::activate(handle, cx)
        {
            return;
        }
        self.close_merge_window(cx);
        self.close_modal(window, cx);
        let title = format!("Merge {rel}");
        let result = file.clone();
        let Some(handle) = tool_window::open(tool_window::Kind::Merge, title, window, cx, move |window, cx| {
            cx.new(|cx| crate::merge_view::MergeView::new(&result, base, mine, other, window, cx))
        }) else {
            return;
        };
        let Some(view) = tool_window::view(&handle, cx) else { return };
        let subscription = cx.subscribe_in(&view, window, move |ws, _, event: &crate::merge_view::MergeViewEvent, window, cx| match event {
            crate::merge_view::MergeViewEvent::Close => {
                ws.close_merge_window(cx);
                window.activate_window();
            }
            crate::merge_view::MergeViewEvent::AcceptDeletion { yours } => {
                ws.close_merge_window(cx);
                window.activate_window();
                ws.run_git(GitOp::AcceptSide { rels: vec![rel.clone()], yours: *yours }, window, cx);
            }
            crate::merge_view::MergeViewEvent::Apply => {
                // Saved first: marking it resolved reads the file (and asks if markers remain).
                let saving = file.update(cx, |e, cx| e.save_in_background(cx));
                let rel = rel.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let saved = saving.await;
                    this.update_in(cx, |ws, window, cx| match saved {
                        Ok(()) => {
                            ws.close_merge_window(cx);
                            window.activate_window();
                            ws.run_git(GitOp::Stage(vec![rel]), window, cx);
                        }
                        Err(err) => {
                            let _ = window.prompt(PromptLevel::Critical, "Couldn't save file", Some(&err.to_string()), &["OK"], cx);
                        }
                    })
                    .ok();
                })
                .detach();
            }
        });
        self.merge_window = Some((handle, path, subscription));
    }

    /// Asks the user what Git or SSH asks (`askpass`): a password or
    /// passphrase in a one-line dialog (bullets for secrets), or whether to
    /// trust a host SSH hasn't seen. Closing the dialog cancels.
    pub fn answer_question(&mut self, question: crate::askpass::Question, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = question.prompt.trim().trim_end_matches(':').trim().to_string();
        if question.confirm || prompt.contains("(yes/no") {
            // SSH's "The authenticity of host … can't be established …".
            let detail = prompt.lines().filter(|l| !l.contains("(yes/no")).collect::<Vec<_>>().join("\n");
            let answer = window.prompt(PromptLevel::Warning, "Connect to a host SSH hasn't seen before?", Some(&detail), &["Connect", "Cancel"], cx);
            cx.spawn(async move |_, _| {
                let yes = matches!(answer.await, Ok(0));
                question.answer(Some(if yes { "yes" } else { "no" }.into()));
            })
            .detach();
            return;
        }
        let secret = question.secret();
        let lower = prompt.to_lowercase();
        let hint = if secret && lower.contains("github.com") {
            "GitHub takes a personal access token here, not your account password."
        } else if lower.contains("passphrase") {
            "The passphrase of your SSH key. Kuana doesn't keep it; an SSH agent can."
        } else if secret {
            "Git asks for it to reach the remote; your credential helper may keep it."
        } else {
            "Git asks for it to reach the remote."
        };
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| {
            let dialog = InputDialog::new(prompt, Icon::Key, hint, "", None, window, cx).allow_empty();
            if secret { dialog.secret(cx) } else { dialog }
        });
        // Answered once; dropped unanswered (another dialog replaced this one), it cancels.
        let question = std::cell::RefCell::new(Some(question));
        let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            let answer = match event {
                InputDialogEvent::Confirm(text) => Some(text.clone()),
                InputDialogEvent::Dismissed => None,
            };
            if let Some(question) = question.borrow_mut().take() {
                question.answer(answer);
            }
            ws.close_modal(window, cx);
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    /// Fetches in the background (`auto_fetch`), every `AUTO_FETCH_EVERY`
    /// while the window is in use, so commits waiting to be pulled show
    /// before you've edited the same lines. Quiet: a failure (offline, no
    /// credentials) shows nothing, and it waits while a Git operation runs.
    pub(super) fn start_auto_fetch(window: &mut Window, cx: &mut Context<Self>) {
        const AUTO_FETCH_EVERY: Duration = Duration::from_secs(180);
        cx.spawn_in(window, async move |this, cx| {
            let mut last: Option<(PathBuf, Instant)> = None;
            loop {
                cx.background_executor().timer(Duration::from_secs(20)).await;
                let Ok(target) = this.update_in(cx, |ws, window, cx| ws.auto_fetch_target(window, cx)) else { break };
                let Some(repo) = target else { continue };
                if last.as_ref().is_some_and(|(r, at)| *r == repo && at.elapsed() < AUTO_FETCH_EVERY) {
                    continue;
                }
                last = Some((repo.clone(), Instant::now()));
                let dir = repo.clone();
                let fetched = cx.background_executor().spawn(async move { git::fetch(&dir).is_ok() }).await;
                let shown = this.update(cx, |ws, cx| {
                    if fetched && ws.repo(cx).as_ref() == Some(&repo) {
                        ws.refresh_git(cx);
                    }
                });
                if shown.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// The repository to fetch in the background now, if any: the setting is
    /// on, the window is in use, the project is trusted, it has a remote, and
    /// no Git operation is running or stopped halfway.
    fn auto_fetch_target(&self, window: &Window, cx: &App) -> Option<PathBuf> {
        let git = self.git.read(cx);
        let repo = git.repo()?;
        let ready = Settings::get(cx).auto_fetch()
            && window.is_window_active()
            && git.remote().is_some()
            && !git.busy().is_running()
            && git.status().state.is_none()
            && crate::trust::is_trusted(repo);
        ready.then(|| repo.clone())
    }

    /// Reads all of `git status` again in the background: a project
    /// opened, `.git` changed (HEAD, the index, refs), a Git operation ran.
    pub(super) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        self.git.update(cx, |g, cx| g.refresh(cx));
    }

    /// The Commit view opens: it lists the whole repository's changes, and
    /// the watcher only sees the project folder's (and `.git`), so one
    /// above the project folder is read again, as is one not watched.
    pub(super) fn refresh_commit_view(&mut self, cx: &mut Context<Self>) {
        let outside = self.repo(cx).as_ref().zip(self.root.as_ref()).is_some_and(|(repo, root)| repo != root);
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
    /// (see `GitStore::read_status`).
    pub(super) fn read_status(&mut self, work: StatusWork, cx: &mut Context<Self>) {
        self.git.update(cx, |g, cx| g.read_status(work, cx));
    }

    /// Shows a status that differs from the one before: the tree's colors,
    /// diff bases, the Git panel, the diff view.
    fn show_status(&mut self, changed: &StatusChange, cx: &mut Context<Self>) {
        if changed.branch_moved && self.shows(BottomPanel::GitLog) {
            self.git_log.update(cx, |l, cx| l.reload(cx));
        }
        self.decorate_tree(cx);
        let new = self.git_status(cx);
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
        let Some(repo) = self.repo(cx) else { return };
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
        let user = self.git.read(cx).user().cloned();
        let head = self.git_status(cx).head_oid.clone();
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
        if let (GitOp::Stage(rels), Some(repo)) = (&op, &self.repo(cx)) {
            let unresolved: Vec<&String> = rels
                .iter()
                .filter(|rel| self.git_status(cx).conflicted().any(|f| &f.rel == *rel))
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
        // Rewriting a commit that's on a remote already means a force push.
        if let (GitOp::UndoCommit | GitOp::EditHistory { .. }, Some(repo)) = (&op, self.repo(cx)) {
            let sha = match &op {
                GitOp::EditHistory { sha, .. } => sha.clone(),
                _ => "HEAD".into(),
            };
            cx.spawn_in(window, async move |this, cx| {
                let pushed = cx.background_executor().spawn(async move { git::is_pushed(&repo, &sha) }).await;
                this.update_in(cx, |ws, window, cx| {
                    let drop = matches!(&op, GitOp::EditHistory { edit: git::HistoryEdit::Drop, .. });
                    if pushed {
                        let detail = "Changing it here makes your branch differ from the remote's: pushing then needs Force push, which replaces the commit for everyone using that branch.";
                        ws.confirm_git(op, "This commit is already pushed", detail, "Change It Anyway", false, window, cx);
                    } else if drop {
                        let detail = "The commit and its changes leave the branch. Git's reflog keeps it for a while, but Kuana can't undo this.";
                        ws.confirm_git(op, "Drop this commit?", detail, "Drop", false, window, cx);
                    } else {
                        ws.execute_git(op, window, cx);
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        if let Some((message, detail, button, undoable)) = confirmation(&op) {
            self.confirm_git(op, &message, detail, button, undoable, window, cx);
            return;
        }
        // Deleting a branch whose commits aren't merged loses them, so look first.
        if let (GitOp::DeleteBranch { name, force: false }, Some(repo)) = (&op, self.repo(cx)) {
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
        if let (&GitOp::Unstash { index, pop }, Some(repo)) = (&op, self.repo(cx)) {
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
        let Some(repo) = self.repo(cx) else { return };
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

    /// Runs `op` after the one running (see `GitStore::execute`); how it
    /// ended comes back in `git_finished`.
    pub(super) fn execute_git(&mut self, op: GitOp, _: &mut Window, cx: &mut Context<Self>) {
        self.git.update(cx, |g, cx| g.execute(op, cx));
    }

    /// Reports how a Git operation ended and shows what it changed.
    fn git_finished(&mut self, op: &GitOp, result: &Result<Outcome, String>, window: &mut Window, cx: &mut Context<Self>) {
        self.git_panel.update(cx, |p, cx| p.set_busy(None, cx));
        match result.clone() {
            Ok(Outcome::Done(message)) => {
                if let Some(message) = message {
                    // A merge or rebase that stopped on conflicts isn't done yet.
                    let kind = if message.contains("conflicts") { ToastKind::Info } else { ToastKind::Success };
                    self.toast(kind, message, None, cx);
                }
                if matches!(op, GitOp::Commit { .. }) {
                    self.git_panel.update(cx, |p, cx| p.committed(cx));
                }
            }
            Ok(Outcome::Nothing(message)) => self.toast(ToastKind::Info, message, None, cx),
            Ok(Outcome::UndoneCommit { message }) => {
                let summary = message.lines().next().unwrap_or_default().to_string();
                self.toast(ToastKind::Success, format!("Undid “{summary}”; its changes are staged"), None, cx);
                self.git_panel.update(cx, |p, cx| p.restore_message(&message, cx));
            }
            Ok(Outcome::PushNext(message)) => {
                if let Some(message) = message.filter(|m| m != "Already up to date") {
                    self.toast(ToastKind::Success, message, None, cx);
                }
                self.show_push(window, cx);
            }
            Ok(Outcome::KeptStash { message, sha }) => {
                self.toast(ToastKind::Info, message, None, cx);
                self.git_panel.update(cx, |p, cx| p.set_kept_stash(Some(sha), cx));
            }
            Ok(Outcome::Diverged { branch, upstream, ahead, behind }) => match op {
                // Nothing says how to combine them (the setting or the Git config): ask, once.
                GitOp::Pull(None) => self.choose_pull_strategy(&branch, &upstream, ahead, behind, false, window, cx),
                GitOp::Sync(None) => self.choose_pull_strategy(&branch, &upstream, ahead, behind, true, window, cx),
                _ => {
                    let detail = format!(
                        "{branch} has {} that {upstream} doesn't. Merge or rebase it from Branches…, or change the pull strategy in Settings → Git.",
                        commits(ahead),
                    );
                    self.toast(ToastKind::Error, format!("Can't fast-forward {branch} to {upstream}"), Some(detail), cx);
                }
            },
            Ok(Outcome::CommittedNotPushed { summary, error }) => {
                self.git_panel.update(cx, |p, cx| p.committed(cx));
                match error {
                    None => {
                        self.toast(ToastKind::Info, format!("Committed {summary}"), None, cx);
                        self.show_push(window, cx);
                    }
                    Some(error) => self.toast(ToastKind::Error, format!("Committed {summary}, but the push failed"), Some(error), cx),
                }
            }
            Ok(Outcome::PartlyApplied { not_restored, error }) => {
                // Git applies the changes to tracked files first, then restores
                // untracked ones, skipping those that exist (or stopping).
                let what = match not_restored.as_slice() {
                    [] => format!("Tracked files are updated; restoring untracked ones stopped ({error})."),
                    [one] => format!("{one} wasn't restored: it already exists. The rest is applied."),
                    many => format!("{} untracked files weren't restored: they already exist. The rest is applied.", many.len()),
                };
                let detail = format!("{what} The stash is kept: don't apply it again, drop it when you're done.");
                self.toast(ToastKind::Error, "The stash was only partly applied", Some(detail), cx);
            }
            Err(err) => {
                let title = match op {
                    GitOp::Push | GitOp::PushNow { .. } => "Push failed",
                    GitOp::Pull(_) | GitOp::Sync(_) => "Update failed",
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
                self.toast(ToastKind::Error, title, Some(err), cx);
            }
        }
        if matches!(
            op,
            GitOp::Discard(_)
                | GitOp::ResolveSimple
                | GitOp::AcceptSide { .. }
                | GitOp::UndoCommit
                | GitOp::EditHistory { .. }
                | GitOp::Pull(_)
                | GitOp::Sync(_)
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
            self.reload_changed_files(cx);
        }
        if self.shows(BottomPanel::GitLog)
            && matches!(
                op,
                GitOp::Commit { .. }
                    | GitOp::Fetch
                    | GitOp::Pull(_)
                    | GitOp::Sync(_)
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
                    | GitOp::DeleteRemoteBranch(_)
                    | GitOp::UndoCommit
                    | GitOp::EditHistory { .. }
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
            self.git_log.update(cx, |l, cx| l.reload(cx));
        }
        // Staging part of a file may leave its status as it was.
        if matches!(op, GitOp::StageLines { .. } | GitOp::UnstageLines { .. }) {
            self.load_diff_bases(self.editors.clone(), cx);
        }
        for diff in self.diff_views(cx) {
            diff.update(cx, |v, cx| v.reload(cx));
        }
        if let Some(Modal::Remotes(dialog)) = &self.modals.current {
            dialog.update(cx, |d, cx| d.reload(cx));
        }
        // A remote added or changed: where a push goes, and what it sends.
        if let Some(dialog) = self.push_window.as_ref().and_then(|(handle, _)| tool_window::view(handle, cx)) {
            dialog.update(cx, |d, cx| d.reload(cx));
        }
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

    pub(super) fn git_undo_commit(&mut self, _: &GitUndoCommit, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::UndoCommit, window, cx);
    }

    /// Update Project, then Push… when there are commits to push (one click
    /// for a branch that's both behind and ahead).
    pub(super) fn git_sync(&mut self, _: &GitSync, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Sync(None), window, cx);
    }

    /// A pull found commits on both sides and nothing that says how to
    /// combine them: asks (like JetBrains' first Update Project), saves the
    /// answer as the pull strategy and pulls with it.
    #[allow(clippy::too_many_arguments)]
    fn choose_pull_strategy(&mut self, branch: &str, upstream: &str, ahead: u32, behind: u32, then_push: bool, window: &mut Window, cx: &mut Context<Self>) {
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
                ws.run_git(if then_push { GitOp::Sync(Some(strategy)) } else { GitOp::Pull(Some(strategy)) }, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn git_fetch(&mut self, _: &GitFetch, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Fetch, window, cx);
    }

    pub(super) fn git_branches(&mut self, _: &GitBranches, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo(cx) else {
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
        if !self.has_repo(cx) {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
        } else if self.git_status(cx).files.is_empty() {
            self.toast(ToastKind::Info, "No changes to stash", None, cx);
        } else {
            self.ask_for_name(PendingInput::Git(GitPrompt::Stash), window, cx);
        }
    }

    pub(super) fn manage_remotes(&mut self, _: &ManageRemotes, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo(cx) else {
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
        match (&self.root, &self.repo(cx)) {
            (None, _) => self.toast(ToastKind::Error, "Open a folder first", None, cx),
            (Some(_), Some(_)) => self.toast(ToastKind::Error, "This folder is already in a Git repository", None, cx),
            (Some(_), None) => self.run_git(GitOp::Init, window, cx),
        }
    }

    pub(super) fn open_remote(&mut self, _: &OpenRemote, _: &mut Window, cx: &mut Context<Self>) {
        match self.git.read(cx).remote().and_then(git::web_url) {
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
        let Some(repo) = self.repo(cx) else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.diff_return = None;
        // Already open: forward, with what a push would send now.
        if let Some((handle, _)) = &self.push_window
            && let Some(dialog) = tool_window::view(handle, cx)
        {
            dialog.update(cx, |d, cx| d.reload(cx));
            tool_window::activate(handle, cx);
            return;
        }
        self.close_modal(window, cx);
        let Some(handle) = tool_window::open(tool_window::Kind::Push, "Push Commits", window, cx, |window, cx| {
            cx.new(|cx| PushDialog::new(repo, window, cx))
        }) else {
            return;
        };
        let Some(dialog) = tool_window::view(&handle, cx) else { return };
        let subscription = cx.subscribe_in(&dialog, window, |ws, _, event: &PushDialogEvent, window, cx| match event {
            PushDialogEvent::Push { force, remote, tags } => {
                ws.close_push_window(cx);
                window.activate_window();
                ws.run_git(GitOp::PushNow { force: *force, remote: remote.clone(), tags: tags.clone() }, window, cx);
            }
            // In this window, which comes forward; closing the diff's tab
            // brings the Push window back (`close_diff`).
            PushDialogEvent::OpenDiff(source) => {
                window.activate_window();
                if let Some(diff) = ws.show_diff(source.clone(), window, cx) {
                    ws.diff_return = Some(diff.entity_id());
                }
            }
            // Here too; the Push window reads the remotes again after a change (`git_finished`).
            PushDialogEvent::ManageRemotes => {
                window.activate_window();
                ws.manage_remotes(&ManageRemotes, window, cx);
            }
            PushDialogEvent::Dismissed => {
                ws.close_push_window(cx);
                window.activate_window();
            }
        });
        self.push_window = Some((handle, subscription));
    }

    fn close_push_window(&mut self, cx: &mut App) {
        self.diff_return = None;
        if let Some((handle, _)) = self.push_window.take() {
            tool_window::close(handle, cx);
        }
    }

    fn close_merge_window(&mut self, cx: &mut App) {
        if let Some((handle, _, _)) = self.merge_window.take() {
            tool_window::close(handle, cx);
        }
    }

    /// Closes the Push and Merge windows this window opened.
    pub(super) fn close_tool_windows(&mut self, cx: &mut App) {
        self.close_push_window(cx);
        self.close_merge_window(cx);
    }

    /// Shows a diff in a tab of the current pane, as opening a file does:
    /// the tab already showing it (a Local History version replaces the
    /// version its file's tab shows), else a new one.
    pub(super) fn show_diff(&mut self, source: DiffSource, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<DiffView>> {
        // A Local History version needs no repository: paths show from the project.
        let repo = match &source {
            DiffSource::History { path, .. } => self.root.clone().or_else(|| path.parent().map(Path::to_path_buf)),
            _ => self.repo(cx),
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

    /// Closes a diff's tab; one opened from the Push window brings it forward.
    pub(super) fn close_diff(&mut self, diff: &Entity<DiffView>, window: &mut Window, cx: &mut Context<Self>) {
        let back = self.diff_return.take_if(|id| *id == diff.entity_id()).is_some();
        self.remove_item(&PaneItem::Diff(diff.clone()), window, cx);
        if back && let Some((handle, _)) = &self.push_window {
            tool_window::activate(handle, cx);
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
        GitOp::DeleteRemoteBranch(name) => Some((
            format!("Delete {name} on the remote?"),
            "Everyone using the remote loses the branch; commits no other branch has go with it.",
            "Delete",
            false,
        )),
        GitOp::Reset { sha, mode: git::ResetMode::Hard } => Some((
            format!("Hard reset to {}?", &sha[..sha.len().min(7)]),
            "Commits after it leave the branch, and every uncommitted change is lost. This can't be undone.",
            "Reset",
            false,
        )),
        _ => None,
    }
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
