//! A project's Git repository: which one it is, its status as last read and
//! the operation running on it (one at a time). It has no UI: the workspace
//! asks what needs asking before it hands an operation over, and shows what
//! `GitEvent`s report, as it does for language servers with `LspStore`.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{Context, EventEmitter, SharedString, Task};

use crate::{
    busy::Busy,
    git::{self, Change, RepoStatus},
    git_panel::GitOp,
    settings::Settings,
};

pub struct GitStore {
    /// The project folder, where `git init` runs.
    root: Option<PathBuf>,
    /// Working-tree root of the project's Git repository.
    repo: Option<PathBuf>,
    status: Arc<RepoStatus>,
    /// Change per file (and "contains changes" per folder) for the file tree.
    paths: Arc<HashMap<PathBuf, Change>>,
    /// What Git ignores (files, and folders as a whole), dimmed in the tree.
    ignored: Arc<HashSet<PathBuf>>,
    user: Option<String>,
    /// URL of the repository's `origin` (or first) remote.
    remote: Option<String>,
    last_commit: Option<git::CommitInfo>,
    /// The running Git operation (one at a time), shown in the status bar.
    busy: Busy,
    /// Operations started while another ran: they run next, in order (Git
    /// takes one at a time).
    queue: VecDeque<GitOp>,
    status_task: Option<Task<()>>,
    op_task: Option<Task<()>>,
    /// Git status asked for while a read ran (`read_status`).
    pending: StatusWork,
}

/// What the store tells its workspace.
pub enum GitEvent {
    /// The repository was looked for again (a project opened, `git init`):
    /// there may be another one now, or none.
    RepoChanged,
    /// A status read landed.
    StatusRead {
        /// With all of it read: the stashes.
        stashes: Option<Vec<git::Stash>>,
        /// When it differs from the status known before.
        changed: Option<StatusChange>,
    },
    /// An operation started; its label for progress.
    Started(SharedString),
    /// An operation ended. The status is being read again, and the next
    /// operation waiting has started.
    Finished { op: GitOp, result: Result<Outcome, String> },
}

/// How a status read differs from the one before, for what follows it.
pub struct StatusChange {
    /// The branch, its commit or its distance from its upstream changed.
    pub branch_moved: bool,
    /// HEAD moved: every file's diff base may have.
    pub head_moved: bool,
    /// The files whose HEAD or index blob moved: their diff bases did.
    pub blobs_moved: HashSet<PathBuf>,
}

impl EventEmitter<GitEvent> for GitStore {}

impl GitStore {
    pub fn new() -> Self {
        GitStore {
            root: None,
            repo: None,
            status: Arc::default(),
            paths: Arc::default(),
            ignored: Arc::default(),
            user: None,
            remote: None,
            last_commit: None,
            busy: Busy::default(),
            queue: VecDeque::new(),
            status_task: None,
            op_task: None,
            pending: StatusWork::default(),
        }
    }

    pub fn repo(&self) -> Option<&PathBuf> {
        self.repo.as_ref()
    }

    /// The project folder the repository was looked for (None for a lone file's).
    pub fn root(&self) -> Option<&PathBuf> {
        self.root.as_ref()
    }

    pub fn status(&self) -> &Arc<RepoStatus> {
        &self.status
    }

    pub fn paths(&self) -> &Arc<HashMap<PathBuf, Change>> {
        &self.paths
    }

    pub fn ignored(&self) -> &Arc<HashSet<PathBuf>> {
        &self.ignored
    }

    pub fn user(&self) -> Option<&String> {
        self.user.as_ref()
    }

    pub fn remote(&self) -> Option<&str> {
        self.remote.as_deref()
    }

    pub fn last_commit(&self) -> Option<&git::CommitInfo> {
        self.last_commit.as_ref()
    }

    /// The running operation's progress (see `Busy`).
    pub fn busy(&self) -> &Busy {
        &self.busy
    }

    /// Looks for the repository `dir` is in, in the background, and reads
    /// its status. `root` is the project folder, if `dir` is one's.
    pub fn detect(&mut self, dir: PathBuf, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.root = root;
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
            this.update(cx, |store, cx| {
                let (repo, user, remote) = found.map_or((None, None, None), |(r, u, o)| (Some(r), u, o));
                store.repo = repo;
                store.user = user;
                store.remote = remote;
                store.forget_status();
                cx.emit(GitEvent::RepoChanged);
                store.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The project closed: no repository until the next `detect`.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.root = None;
        self.repo = None;
        self.remote = None;
        self.forget_status();
        cx.notify();
    }

    /// What was read or asked for is the last repository's.
    fn forget_status(&mut self) {
        self.last_commit = None;
        self.status = Arc::default();
        self.paths = Arc::default();
        self.ignored = Arc::default();
        self.status_task = None;
        self.pending = StatusWork::default();
    }

    /// Reads all of `git status` again in the background: a project
    /// opened, `.git` changed (HEAD, the index, refs), a Git operation ran.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.read_status(StatusWork::all(), cx);
    }

    /// Reads Git status again in the background, of what `work` covers
    /// (paths are merged into what's known). One read at a time: what's
    /// asked for meanwhile is read together after it.
    pub fn read_status(&mut self, work: StatusWork, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        self.pending.add(work);
        if self.status_task.is_some() || self.pending.is_empty() {
            return;
        }
        let work = std::mem::take(&mut self.pending);
        let (known, ignored) = (self.status.clone(), self.ignored.clone());
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let repo2 = repo.clone();
            let read = cx.background_executor().spawn(async move { StatusRead::read(&repo2, work, &known, &ignored) }).await;
            this.update(cx, |store, cx| {
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = store.status_task.take() {
                    task.detach();
                }
                // Not for another repository opened meanwhile.
                if store.repo.as_ref() == Some(&repo) {
                    store.apply(read, cx);
                }
                // Once it's known: a read of some paths merges into it.
                store.read_status(StatusWork::default(), cx);
            })
            .ok();
        }));
    }

    /// Takes a status read in the background as the one known, and tells
    /// the workspace what showing it needs.
    fn apply(&mut self, read: StatusRead, cx: &mut Context<Self>) {
        let stashes = read.head.map(|(last_commit, stashes)| {
            if self.last_commit != last_commit {
                self.last_commit = last_commit;
                cx.notify();
            }
            stashes
        });
        let changed = read.changed.map(|changed| {
            let (old, status) = (&self.status, &changed.status);
            let branch_moved = (&old.head_oid, &old.branch, old.ahead, old.behind) != (&status.head_oid, &status.branch, status.ahead, status.behind);
            self.paths = changed.paths;
            if let Some(ignored) = changed.ignored {
                self.ignored = ignored;
            }
            self.status = Arc::new(changed.status);
            cx.notify();
            StatusChange { branch_moved, head_moved: changed.head_moved, blobs_moved: changed.blobs_moved }
        });
        if stashes.is_some() || changed.is_some() {
            cx.emit(GitEvent::StatusRead { stashes, changed });
        }
    }

    /// Runs `op` in the background, after the one running and those
    /// waiting, and reports how it ended with `GitEvent::Finished`. Nothing
    /// is asked here: what needs a confirmation gets it before.
    pub fn execute(&mut self, op: GitOp, cx: &mut Context<Self>) {
        let dir = match (&op, &self.repo, &self.root) {
            (GitOp::Init, _, Some(root)) => root.clone(),
            (_, Some(repo), _) => repo.clone(),
            _ => return,
        };
        if self.busy.is_running() {
            self.queue.push_back(op);
            return;
        }
        // Without a strategy of its own, a pull takes the setting's.
        let op = match op {
            GitOp::Pull(None) => GitOp::Pull(Settings::get(cx).pull_strategy),
            GitOp::Sync(None) => GitOp::Sync(Settings::get(cx).pull_strategy),
            op => op,
        };
        let label: SharedString = op.busy_label().into();
        self.busy.set(Some(label.clone()), |store: &mut Self| &mut store.busy, cx);
        cx.emit(GitEvent::Started(label));
        let status = self.status.clone();
        let auto_resolve = Settings::get(cx).auto_resolve_conflicts();
        let op2 = op.clone();
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { perform_git(&dir, op2, &status, auto_resolve) }).await;
            this.update(cx, |store, cx| {
                store.busy.set(None, |store: &mut Self| &mut store.busy, cx);
                if matches!((&op, &result), (GitOp::Init, Ok(Outcome::Done(_))))
                    && let Some(root) = store.root.clone()
                {
                    store.detect(root.clone(), Some(root), cx);
                }
                if op.changes_remotes()
                    && let Some(repo) = &store.repo
                {
                    store.remote = git::remote_url(repo);
                }
                cx.emit(GitEvent::Finished { op, result });
                store.status_task = None;
                store.refresh(cx);
                // Then what was started meanwhile (this task is running: let
                // it finish rather than drop it for the next one's).
                if let Some(next) = store.queue.pop_front() {
                    if let Some(task) = store.op_task.take() {
                        task.detach();
                    }
                    store.execute(next, cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// Git status to read: all of it, or what's under some paths (files Kuana
/// wrote, changes in the work tree).
#[derive(Debug, Default)]
pub(crate) struct StatusWork {
    pub(crate) all: bool,
    pub(crate) paths: BTreeSet<PathBuf>,
}

impl StatusWork {
    pub(crate) fn all() -> Self {
        StatusWork { all: true, paths: BTreeSet::new() }
    }

    pub(crate) fn paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        StatusWork { all: false, paths: paths.into_iter().collect() }
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.all && self.paths.is_empty()
    }

    pub(crate) fn add(&mut self, more: StatusWork) {
        self.all |= more.all;
        if self.all {
            self.paths.clear();
        } else {
            self.paths.extend(more.paths);
        }
    }

    pub(crate) fn add_path(&mut self, path: PathBuf) {
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

/// How a Git operation ended, for the workspace to report.
#[derive(Clone)]
pub(crate) enum Outcome {
    /// Finished; the message for a notification, if any.
    Done(Option<String>),
    /// Nothing to do, and why.
    Nothing(String),
    /// A fast-forward-only pull found commits on both sides; nothing changed.
    Diverged { branch: String, upstream: String, ahead: u32, behind: u32 },
    /// A stash applied in part and kept (see `git::Unstashed::Partly`).
    PartlyApplied { not_restored: Vec<String>, error: String },
    /// Commit & Push committed, then didn't push: it failed with `error`,
    /// or (None) the branch is behind its upstream.
    CommittedNotPushed { summary: String, error: Option<String> },
    /// Changes put back over new commits (a pull's, a stash popped)
    /// conflicted; the stash with commit `sha` keeps them until they're resolved.
    KeptStash { message: String, sha: String },
    /// Sync pulled (`message`, if any to show); there are commits to push next.
    PushNext(Option<String>),
    /// The last commit was undone; its message goes back in the commit box.
    UndoneCommit { message: String },
}

/// "1 commit", "3 commits".
pub(crate) fn commits(n: impl Into<u64>) -> String {
    let n = n.into();
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

/// Runs one Git operation; blocking. When it stops on conflicts and
/// `auto_resolve` is on, the simple ones (each side changing different words)
/// are combined right away, as JetBrains does.
pub(crate) fn perform_git(dir: &Path, op: GitOp, status: &RepoStatus, auto_resolve: bool) -> Result<Outcome, String> {
    let may_conflict = matches!(
        op,
        GitOp::Merge(_)
            | GitOp::Rebase(_)
            | GitOp::Pull(_)
            | GitOp::Sync(_)
            | GitOp::Continue(_)
            | GitOp::Unstash { .. }
            | GitOp::CherryPick(_)
            | GitOp::Revert(_)
            | GitOp::Checkout(_)
            | GitOp::CheckoutCommit(_)
            | GitOp::EditHistory { .. }
    );
    // What the user started may ask for credentials.
    let cancellations = crate::askpass::cancellations();
    let outcome = match git::asking(|| perform(dir, op, status)) {
        Err(_) if crate::askpass::cancellations() > cancellations => {
            return Ok(Outcome::Nothing("Cancelled: Git didn't get the credentials it asked for".into()));
        }
        outcome => outcome?,
    };
    if !auto_resolve || !may_conflict {
        return Ok(outcome);
    }
    let conflicted: Vec<String> = git::status(dir).map(|s| s.conflicted().map(|f| f.rel.clone()).collect()).unwrap_or_default();
    let done = match git::resolve_simple(dir, &conflicted) {
        Ok(done) if done.resolved > 0 => done,
        _ => return Ok(outcome),
    };
    let note = match done.left {
        0 if done.resolved == 1 => "the conflict was simple and is resolved".to_string(),
        0 => format!("all {} conflicts were simple and are resolved", done.resolved),
        left => format!("{} resolved automatically, {left} left", conflicts(done.resolved)),
    };
    Ok(match outcome {
        Outcome::Done(Some(message)) => Outcome::Done(Some(format!("{message}; {note}"))),
        // Changes put back over a pull's commits that all combined: say what that means.
        Outcome::KeptStash { message, sha } if done.left == 0 => Outcome::KeptStash {
            message: match message.split_once("; ") {
                Some((pulled, _)) => format!("{pulled}; your changes touched the same lines and were combined"),
                None => format!("{message}; {note}"),
            },
            sha,
        },
        Outcome::KeptStash { message, sha } => Outcome::KeptStash { message: format!("{message}; {note}"), sha },
        outcome => outcome,
    })
}

/// "1 conflict", "3 conflicts".
fn conflicts(n: usize) -> String {
    format!("{n} conflict{}", if n == 1 { "" } else { "s" })
}

fn perform(dir: &Path, op: GitOp, status: &RepoStatus) -> Result<Outcome, String> {
    // A switch whose local changes came along, through a stash when Git refused.
    let switched = |switched: git::Switched, done: String| match switched {
        git::Switched::Done => Outcome::Done(Some(done)),
        git::Switched::Carried => Outcome::Done(Some(format!("{done}; your changes came along"))),
        git::Switched::ChangesConflict { stash } => {
            let message = format!("{done}; putting your changes back stopped on conflicts");
            match stash {
                Some(sha) => Outcome::KeptStash { message, sha },
                None => Outcome::Done(Some(message)),
            }
        }
    };
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
            // A checked file deleted since it was listed would fail the
            // whole commit (and leave what `all` staged behind).
            let untracked: Vec<String> = untracked.into_iter().filter(|rel| std::fs::symlink_metadata(dir.join(rel)).is_ok()).collect();
            // Untracked files only when checked: a commit of "everything"
            // mustn't pick up build output or notes nobody chose.
            if all {
                git::stage_tracked(dir)?;
            }
            if !untracked.is_empty() {
                git::stage(dir, &untracked)?;
            }
            let summary = git::commit(dir, &message, amend)?;
            if !and_push {
                return Ok(Outcome::Done(Some(format!("Committed {summary}"))));
            }
            // Behind its upstream (an amended commit that was pushed, or
            // commits fetched since), a push would be refused: the Push
            // dialog says why and offers Force Push.
            if git::status(dir).is_ok_and(|s| s.upstream.is_some() && s.behind > 0) {
                return Ok(Outcome::CommittedNotPushed { summary, error: None });
            }
            return Ok(match push(dir, false, None) {
                Ok(_) => Outcome::Done(Some(format!("Committed and pushed {summary}"))),
                Err(error) => Outcome::CommittedNotPushed { summary, error: Some(error) },
            });
        }
        // `Push` opens the dialog before it gets here; treat it as a plain push.
        GitOp::Push => push(dir, false, None).map(Some),
        GitOp::PushNow { force, remote, tags } => {
            // The tags go where the branch went.
            let tag_remote = remote.clone().or_else(|| git::push_remote(dir));
            // Only tags: the branch is on its remote already.
            let only_tags = !tags.is_empty() && git::status(dir).is_ok_and(|s| s.upstream.is_some() && s.ahead == 0) && !force;
            let pushed = if only_tags { String::new() } else { push(dir, force, remote)? };
            match (tags.as_slice(), tag_remote) {
                ([], _) => Ok(Some(pushed)),
                (_, None) => Err("No remote is configured. Add one in Git → Manage Remotes….".into()),
                (tags, Some(remote)) => git::push_tags(dir, &remote, tags).map(|_| {
                    let what = match tags {
                        [one] => format!("tag {one}"),
                        many => format!("{} tags", many.len()),
                    };
                    Some(if only_tags { format!("Pushed {what} to {remote}") } else { format!("{pushed}, with {what}") })
                }),
            }
        }
        GitOp::Sync(strategy) => {
            let pulled = perform(dir, GitOp::Pull(strategy), status)?;
            // Pushes only after a pull that left nothing to sort out.
            let now = git::status(dir).ok();
            let clean = git::repo_state(dir).is_none() && now.as_ref().is_some_and(|s| s.conflicted().next().is_none());
            let ahead = now.as_ref().filter(|s| s.upstream.is_some()).map_or(0, |s| s.ahead);
            return Ok(match pulled {
                Outcome::Done(message) if clean && ahead > 0 => Outcome::PushNext(message),
                outcome => outcome,
            });
        }
        GitOp::Pull(strategy) => {
            return git::pull(dir, strategy).map(|pulled| match pulled {
                git::Pulled::UpToDate => Outcome::Done(Some("Already up to date".into())),
                git::Pulled::FastForwarded(n) => Outcome::Done(Some(format!("Pulled {}", commits(n as u64)))),
                git::Pulled::Merged(n) => Outcome::Done(Some(format!("Pulled {} and merged", commits(n as u64)))),
                git::Pulled::Rebased(n) => Outcome::Done(Some(format!("Pulled {}, yours rebased on top", commits(n as u64)))),
                git::Pulled::Conflicts => Outcome::Done(Some("Update stopped on conflicts".into())),
                git::Pulled::ChangesConflict { incoming, stash } => {
                    let message = format!("Pulled {}; putting your changes back stopped on conflicts", commits(incoming as u64));
                    match stash {
                        Some(sha) => Outcome::KeptStash { message, sha },
                        None => Outcome::Done(Some(message)),
                    }
                }
                git::Pulled::Diverged { branch, upstream, ahead, behind } => Outcome::Diverged { branch, upstream, ahead, behind },
            });
        }
        GitOp::Fetch => git::fetch(dir).map(|_| Some("Fetched all remotes".into())),
        GitOp::Checkout(branch) => return git::checkout(dir, &branch).map(|s| switched(s, format!("Switched to {}", branch.name))),
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
            let sha = git::stash_sha(dir, index);
            return git::unstash(dir, index, pop).map(|unstashed| match unstashed {
                git::Unstashed::Applied => Outcome::Done(Some(if pop { "Popped stash".into() } else { "Applied stash".into() })),
                // Git keeps a stash that didn't apply cleanly; a Pop's is
                // offered for dropping once the conflicts are resolved.
                git::Unstashed::Conflicts => match sha.filter(|_| pop) {
                    Some(sha) => Outcome::KeptStash { message: "Popping the stash stopped on conflicts".into(), sha },
                    None => Outcome::Done(Some("Applying the stash stopped on conflicts; it's kept".into())),
                },
                git::Unstashed::Partly { error, not_restored } => Outcome::PartlyApplied { not_restored, error },
            });
        }
        GitOp::DropStash(index) => git::drop_stash(dir, index).map(|_| Some("Dropped stash".into())),
        GitOp::UndoCommit => return git::undo_commit(dir).map(|message| Outcome::UndoneCommit { message }),
        GitOp::DeleteRemoteBranch(name) => git::delete_remote_branch(dir, &name).map(|_| Some(format!("Deleted {name} on the remote"))),
        GitOp::EditHistory { sha, edit } => {
            let done = match &edit {
                git::HistoryEdit::Reword(_) => format!("Reworded {}", short(&sha)),
                git::HistoryEdit::Squash => format!("Squashed {} into the commit before it", short(&sha)),
                git::HistoryEdit::Drop => format!("Dropped {}", short(&sha)),
            };
            git::edit_history(dir, &sha, &edit).map(|out| stopped(&out, "Rewriting history", done))
        }
        GitOp::DropKeptStash(sha) => git::drop_stash_sha(dir, &sha).map(|_| Some("Dropped the stash".into())),
        GitOp::ResolveSimple => {
            let conflicted: Vec<String> = git::status(dir)?.conflicted().map(|f| f.rel.clone()).collect();
            let done = git::resolve_simple(dir, &conflicted)?;
            return Ok(match (done.resolved, done.left) {
                (0, 0) => Outcome::Nothing("No conflict markers left; mark the files resolved".into()),
                (0, _) => Outcome::Nothing("No simple conflicts: the rest change the same words on both sides".into()),
                (n, 0) => Outcome::Done(Some(format!("Resolved {}", conflicts(n)))),
                (n, left) => Outcome::Done(Some(format!("Resolved {}; {} left", conflicts(n), conflicts(left)))),
            });
        }
        GitOp::AcceptSide { rels, yours } => {
            let mine = git::Side::yours(git::repo_state(dir));
            let side = match (yours, mine) {
                (true, side) => side,
                (false, git::Side::Ours) => git::Side::Theirs,
                (false, git::Side::Theirs) => git::Side::Ours,
            };
            let whose = if yours { "yours" } else { "theirs" };
            git::accept_side(dir, &rels, side).map(|_| match rels.as_slice() {
                [one] => Some(format!("Resolved {one}: kept {whose}")),
                many => Some(format!("Resolved {} files: kept {whose}", many.len())),
            })
        }
        GitOp::CheckoutCommit(sha) => {
            return git::checkout_commit(dir, &sha).map(|s| switched(s, format!("Checked out {} (detached HEAD)", short(&sha))));
        }
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
    use gpui::AppContext as _;

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

    /// The store finds the repository, reads its status and runs operations
    /// one at a time, telling its workspace about each step.
    #[gpui::test]
    fn finds_the_repository_and_runs_operations_in_order(cx: &mut gpui::TestAppContext) {
        if !git::is_available() {
            return;
        }
        let root = std::env::temp_dir().join(format!("kuana-git-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git::init(&root).unwrap();
        let root = git::discover(&root).unwrap();
        for args in [["config", "user.name", "Test"], ["config", "user.email", "test@example.com"]] {
            git::run(&root, &args).unwrap();
        }
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();

        let store = cx.new(|_| GitStore::new());
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|cx| {
            cx.subscribe(&store, move |_, event: &GitEvent, _| {
                seen.borrow_mut().push(match event {
                    GitEvent::RepoChanged => "repository".to_string(),
                    GitEvent::StatusRead { changed, .. } => format!("status{}", if changed.is_some() { " changed" } else { "" }),
                    GitEvent::Started(label) => format!("started {label}"),
                    GitEvent::Finished { op, result } => format!("finished {op:?} {}", if result.is_ok() { "ok" } else { "failed" }),
                });
            })
            .detach();
        });
        let staged = |store: &gpui::Entity<GitStore>, cx: &mut gpui::TestAppContext| -> Vec<String> {
            store.read_with(cx, |s, _| s.status().files.iter().filter(|f| f.staged.is_some()).map(|f| f.rel.clone()).collect())
        };

        store.update(cx, |s, cx| s.detect(root.join("a.txt").parent().unwrap().to_path_buf(), Some(root.clone()), cx));
        cx.run_until_parked();
        assert_eq!(store.read_with(cx, |s, _| s.repo().cloned()), Some(root.clone()));
        assert_eq!(store.read_with(cx, |s, _| s.status().files.len()), 2, "both files are untracked");
        assert_eq!(store.read_with(cx, |s, _| s.paths().get(&root.join("a.txt")).copied()), Some(Change::Untracked));
        assert_eq!(std::mem::take(&mut *events.borrow_mut()), ["repository", "status changed"]);

        // The second waits for the first, and each is reported as it ends.
        store.update(cx, |s, cx| {
            s.execute(GitOp::Stage(vec!["a.txt".into()]), cx);
            s.execute(GitOp::Stage(vec!["b.txt".into()]), cx);
            assert!(s.busy().is_running());
        });
        cx.run_until_parked();
        assert!(!store.read_with(cx, |s, _| s.busy().is_running()));
        assert_eq!(staged(&store, cx), ["a.txt", "b.txt"]);
        let events = std::mem::take(&mut *events.borrow_mut());
        let steps: Vec<&str> = events.iter().map(String::as_str).filter(|e| !e.starts_with("status")).collect();
        assert_eq!(
            steps,
            ["started Staging…", r#"finished Stage(["a.txt"]) ok"#, "started Staging…", r#"finished Stage(["b.txt"]) ok"#],
            "{events:?}"
        );
        assert!(events.iter().any(|e| e == "status changed"), "the status is read again after an operation");

        // A closed project has no repository, and nothing runs for it.
        store.update(cx, |s, cx| {
            s.close(cx);
            s.execute(GitOp::Fetch, cx);
            assert!(s.repo().is_none() && s.status().files.is_empty() && !s.busy().is_running());
        });
        std::fs::remove_dir_all(&root).ok();
    }

    /// Commit & Push: checked files that went away are skipped, a branch
    /// behind its upstream (an amended pushed commit) isn't pushed, and a
    /// push that fails still reports the commit.
    #[test]
    fn commit_and_push() {
        if !git::is_available() {
            return;
        }
        let base = std::env::temp_dir().join(format!("kuana-commit-push-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let remote = base.join("remote.git");
        let dir = base.join("work");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        git::run(&remote, &["init", "--bare", "--quiet"]).unwrap();
        git::init(&dir).unwrap();
        let root = git::discover(&dir).unwrap();
        for args in [["config", "user.name", "Test"], ["config", "user.email", "test@example.com"], ["symbolic-ref", "HEAD", "refs/heads/main"]] {
            git::run(&root, &args).unwrap();
        }
        git::add_remote(&root, "origin", remote.to_str().unwrap()).unwrap();
        let write = |rel: &str, text: &str| std::fs::write(root.join(rel), text).unwrap();
        let commit = |message: &str, amend: bool, untracked: &[&str], status: &RepoStatus| {
            let untracked = untracked.iter().map(|rel| rel.to_string()).collect();
            perform_git(&root, GitOp::Commit { message: message.into(), amend, all: !amend, untracked, push: true }, status, false)
        };

        // The first push sets the upstream; a checked file deleted since is left out.
        write("a.txt", "one\n");
        write("gone.txt", "");
        let status = git::status(&root).unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        assert!(matches!(commit("first", false, &["a.txt", "gone.txt"], &status), Ok(Outcome::Done(Some(m))) if m.starts_with("Committed and pushed")));
        let status = git::status(&root).unwrap();
        assert_eq!((status.upstream.as_deref(), status.ahead, status.files.len()), (Some("origin/main"), 0, 0));

        // Amending the pushed commit: committed, not pushed (the Push dialog offers to force).
        assert!(matches!(commit("first, amended", true, &[], &status), Ok(Outcome::CommittedNotPushed { error: None, .. })));
        assert_eq!(git::last_commit_message(&root).unwrap(), "first, amended");

        // A push that fails still reports the commit.
        git::run(&root, &["reset", "--quiet", "--hard", "origin/main"]).unwrap();
        git::checkout_commit(&root, &status.head_oid.clone().unwrap()).unwrap();
        write("a.txt", "two\n");
        let status = git::status(&root).unwrap();
        assert!(matches!(commit("detached", false, &[], &status), Ok(Outcome::CommittedNotPushed { error: Some(_), .. })));
        assert_eq!(git::last_commit_message(&root).unwrap(), "detached");
        let _ = std::fs::remove_dir_all(&base);
    }
}
