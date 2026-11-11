//! The Commit tool window (JetBrains ⌘K / ⌘0): changed files with
//! stage/unstage/discard, stashes, a commit message box and push/pull controls.
//! Operations are emitted as events; the workspace runs them.

use std::{collections::HashSet, ops::Range, path::PathBuf, sync::Arc};

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, MouseButton, MouseDownEvent, Pixels, Point, Render, SharedString, Window, div,
    prelude::*, px, uniform_list, UniformListScrollHandle,
};

use crate::{
    assets::Icon,
    busy::Busy,
    editor::{Editor, Submit},
    git::{self, Branch, Change, FileStatus, RepoState, RepoStatus, Stash},
    theme::Theme,
    ui::{Button, HoverFill, focus_ring, icon_el, kbd, tooltip_view},
};

#[derive(Clone, Debug)]
pub enum GitOp {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    /// Stage the changes of `text` (the file's current text) touching `rows`.
    StageLines { rel: String, text: String, rows: Range<usize> },
    /// Unstage the staged changes touching `rows`: of `text` when given (an
    /// editor's rows), else of the index version.
    UnstageLines { rel: String, text: Option<String>, rows: Range<usize> },
    Discard(Vec<FileStatus>),
    /// Commits what's staged, or with `all` every change to tracked files,
    /// plus the `untracked` files the user checked (never others).
    Commit { message: String, amend: bool, all: bool, untracked: Vec<String>, push: bool },
    /// Opens the Push dialog (the commits to push, then Push or Cancel).
    Push,
    /// Pushes the current branch (from the Push dialog); one without an
    /// upstream goes to `remote` (else the default remote).
    /// `tags`: tags to push after the branch.
    PushNow { force: bool, remote: Option<String>, tags: Vec<String> },
    /// Pulls with this strategy, else the `pull_strategy` setting's.
    Pull(Option<git::PullStrategy>),
    /// Pulls (as `Pull`), then opens the Push dialog when there are commits
    /// to push and the pull left nothing to resolve.
    Sync(Option<git::PullStrategy>),
    Fetch,
    Checkout(Branch),
    CreateBranch(String),
    Init,
    /// Merge a branch into the current one.
    Merge(String),
    /// Rebase the current branch onto another.
    Rebase(String),
    Abort(RepoState),
    Continue(RepoState),
    /// Combines the conflicts whose sides change different words, in every
    /// conflicted file (`git::resolve_simple`).
    ResolveSimple,
    /// Resolves conflicted files with the user's own version (`yours`) or the
    /// other one, whole (`git::accept_side`).
    AcceptSide { rels: Vec<String>, yours: bool },
    /// Undoes the last commit, its changes staged again (its message back in the box).
    UndoCommit,
    /// Deletes a branch on its remote ("origin/feature").
    DeleteRemoteBranch(String),
    /// Rewrites one commit of the current branch (reword, squash, drop).
    EditHistory { sha: String, edit: git::HistoryEdit },
    /// Drops the stash with this commit: the one a pull or Pop kept when
    /// putting changes back conflicted, once they're resolved.
    DropKeptStash(String),
    AddRemote { name: String, url: String },
    RemoveRemote(String),
    /// Changes a remote's URL and name.
    EditRemote { name: String, new_name: String, url: String },
    DeleteBranch { name: String, force: bool },
    RenameBranch { from: String, to: String },
    /// Stashes the changes to tracked files (and untracked files when
    /// `untracked`), with an optional message.
    Stash { message: Option<String>, untracked: bool },
    /// Applies `stash@{index}`; `pop` drops it afterwards.
    Unstash { index: usize, pop: bool },
    DropStash(usize),
    /// Checks out a commit (detached HEAD).
    CheckoutCommit(String),
    BranchAt { name: String, sha: String },
    TagAt { name: String, sha: String },
    CherryPick(String),
    Revert(String),
    Reset { sha: String, mode: git::ResetMode },
}

impl GitOp {
    pub fn changes_remotes(&self) -> bool {
        matches!(self, GitOp::AddRemote { .. } | GitOp::RemoveRemote(_) | GitOp::EditRemote { .. })
    }

    /// Progress text shown while the operation runs.
    pub fn busy_label(&self) -> &'static str {
        match self {
            GitOp::Stage(_) => "Staging…",
            GitOp::Unstage(_) => "Unstaging…",
            GitOp::StageLines { .. } => "Staging lines…",
            GitOp::UnstageLines { .. } => "Unstaging lines…",
            GitOp::Merge(_) => "Merging…",
            GitOp::Rebase(_) => "Rebasing…",
            GitOp::Abort(_) => "Aborting…",
            GitOp::Continue(_) => "Continuing…",
            GitOp::ResolveSimple | GitOp::AcceptSide { .. } => "Resolving…",
            GitOp::Discard(_) => "Discarding…",
            GitOp::Commit { push: true, .. } => "Committing and pushing…",
            GitOp::Commit { .. } => "Committing…",
            GitOp::Push | GitOp::PushNow { .. } => "Pushing…",
            GitOp::Pull(_) => "Pulling…",
            GitOp::Sync(_) => "Syncing…",
            GitOp::Fetch => "Fetching…",
            GitOp::Checkout(_) => "Checking out…",
            GitOp::CreateBranch(_) => "Creating branch…",
            GitOp::Init => "Initializing…",
            GitOp::AddRemote { .. } | GitOp::RemoveRemote(_) | GitOp::EditRemote { .. } => "Updating remotes…",
            GitOp::DeleteBranch { .. } | GitOp::DeleteRemoteBranch(_) => "Deleting branch…",
            GitOp::UndoCommit => "Undoing commit…",
            GitOp::EditHistory { .. } => "Rewriting history…",
            GitOp::RenameBranch { .. } => "Renaming branch…",
            GitOp::Stash { .. } => "Stashing…",
            GitOp::Unstash { .. } => "Applying stash…",
            GitOp::DropStash(_) | GitOp::DropKeptStash(_) => "Dropping stash…",
            GitOp::CheckoutCommit(_) => "Checking out…",
            GitOp::BranchAt { .. } => "Creating branch…",
            GitOp::TagAt { .. } => "Creating tag…",
            GitOp::CherryPick(_) => "Cherry-picking…",
            GitOp::Revert(_) => "Reverting…",
            GitOp::Reset { .. } => "Resetting…",
        }
    }
}

#[derive(Clone)]
pub enum GitPanelEvent {
    Run(GitOp),
    /// Stash the changes (the workspace asks for a message).
    Stash,
    OpenDiff { file: FileStatus, staged: bool },
    OpenFile(PathBuf),
    /// A conflicted file: open the Merge view.
    OpenMerge(PathBuf),
    /// A file's row was right-clicked (`staged`: in the Staged section).
    Menu { file: FileStatus, staged: bool, position: Point<Pixels> },
}

pub struct GitPanel {
    message: Entity<Editor>,
    amend: bool,
    /// Working-tree root of the repository, if the project is in one.
    repo: Option<PathBuf>,
    /// The open folder (to offer `git init` when it isn't a repository).
    project: Option<PathBuf>,
    status: Arc<RepoStatus>,
    stashes: Arc<Vec<Stash>>,
    /// The running Git operation; its progress pill shows only if it takes a
    /// while (staging a file doesn't), then long enough to read.
    busy: Busy,
    selected: Option<(String, bool)>,
    /// The row whose context menu is open (outlined while it is).
    menu_target: Option<(String, bool)>,
    /// Unversioned files checked to go into the next commit. Like JetBrains,
    /// they start unchecked: a commit never takes a new file unasked.
    include: HashSet<String>,
    scroll: UniformListScrollHandle,
    /// The stash (its commit) a pull or Pop kept because putting the changes
    /// back conflicted: once they're resolved, the banner offers to drop it.
    kept_stash: Option<String>,
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

/// The groups of changed files the panel lists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Staged,
    /// Tracked files with unstaged changes, and conflicts.
    Changes,
    Unversioned,
}

pub fn change_color(theme: &Theme, change: Change) -> Hsla {
    match change {
        Change::Added | Change::Renamed => theme.git_added,
        Change::Modified => theme.git_modified,
        Change::Deleted => theme.git_deleted,
        Change::Untracked => theme.git_untracked,
        Change::Conflicted => theme.git_conflict,
    }
    .into()
}

/// Whether a commit takes every change to tracked files (Commit All): only
/// when nothing is chosen, neither staged nor checked. Checking new files
/// chooses what goes in, so the tracked changes left unstaged stay out. An
/// amend takes only what's chosen: with nothing, it rewrites the message.
fn commits_all(status: &RepoStatus, include: &HashSet<String>, amend: bool) -> bool {
    !amend && status.staged().next().is_none() && include.is_empty()
}

impl GitPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| Editor::multi_line("Commit message", cx));
        cx.observe(&message, |_, _, cx| cx.notify()).detach();
        Self {
            message,
            amend: false,
            repo: None,
            project: None,
            status: Arc::new(RepoStatus::default()),
            stashes: Arc::new(Vec::new()),
            busy: Busy::default(),
            selected: None,
            menu_target: None,
            include: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            kept_stash: None,
        }
    }

    pub fn set_kept_stash(&mut self, sha: Option<String>, cx: &mut Context<Self>) {
        self.kept_stash = sha;
        cx.notify();
    }

    pub fn set_repo(&mut self, repo: Option<PathBuf>, project: Option<PathBuf>, cx: &mut Context<Self>) {
        if repo != self.repo {
            self.kept_stash = None;
        }
        self.repo = repo;
        self.project = project;
        cx.notify();
    }

    pub fn set_status(&mut self, status: Arc<RepoStatus>, cx: &mut Context<Self>) {
        // A merge that just stopped: start from git's merge message, as JetBrains does.
        let merge_started = status.state == Some(RepoState::Merging) && self.status.state != Some(RepoState::Merging);
        if merge_started
            && self.message_text(cx).is_empty()
            && let Some(repo) = self.repo.clone()
        {
            let message = self.message.clone();
            cx.spawn(async move |_, cx| {
                let text = cx.background_executor().spawn(async move { git::merge_message(&repo) }).await;
                if let Some(text) = text {
                    message.update(cx, |m, cx| m.set_text(&text, cx)).ok();
                }
            })
            .detach();
        }
        // Checked files that were committed, staged or deleted are done with.
        self.include.retain(|rel| status.untracked().any(|f| &f.rel == rel));
        self.status = status;
        cx.notify();
    }

    pub fn set_stashes(&mut self, stashes: Vec<Stash>, cx: &mut Context<Self>) {
        if *self.stashes != stashes {
            self.stashes = Arc::new(stashes);
            cx.notify();
        }
    }

    pub fn set_busy(&mut self, busy: Option<SharedString>, cx: &mut Context<Self>) {
        self.busy.set(busy, |panel: &mut Self| &mut panel.busy, cx);
    }

    pub fn set_menu_target(&mut self, target: Option<(String, bool)>, cx: &mut Context<Self>) {
        if self.menu_target != target {
            self.menu_target = target;
            cx.notify();
        }
    }

    pub fn committed(&mut self, cx: &mut Context<Self>) {
        self.amend = false;
        self.include.clear();
        self.message.update(cx, |m, cx| m.set_text("", cx));
        cx.notify();
    }

    /// Puts an undone commit's message back in the box, to commit again with.
    pub fn restore_message(&mut self, message: &str, cx: &mut Context<Self>) {
        if self.message_text(cx).is_empty() {
            self.message.update(cx, |m, cx| m.set_text(message, cx));
        }
        self.amend = false;
        cx.notify();
    }

    pub fn focus_message(&self, window: &mut Window, cx: &App) {
        window.focus(&self.message.focus_handle(cx));
    }

    fn message_text(&self, cx: &App) -> String {
        self.message.read(cx).text(cx).trim().to_string()
    }

    fn can_commit(&self, cx: &App) -> bool {
        // Unversioned files count only when checked.
        let something = self.status.staged().next().is_some() || self.status.unstaged().next().is_some() || !self.include.is_empty();
        !self.busy.is_running()
            && !self.message_text(cx).is_empty()
            && (self.amend || something)
            // Unresolved conflicts would be committed with their markers.
            && self.status.conflicted().next().is_none()
    }

    fn commit(&mut self, push: bool, cx: &mut Context<Self>) {
        if !self.can_commit(cx) {
            return;
        }
        let all = commits_all(&self.status, &self.include, self.amend);
        let mut untracked: Vec<String> = self.include.iter().cloned().collect();
        untracked.sort();
        cx.emit(GitPanelEvent::Run(GitOp::Commit {
            message: self.message_text(cx),
            amend: self.amend,
            all,
            untracked,
            push,
        }));
    }

    /// A checkbox for unversioned files: checked when all of `rels` go into
    /// the commit, partly when some do; clicking checks or clears them all.
    fn include_checkbox(&self, id: impl Into<gpui::ElementId>, rels: Vec<String>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        let (icon, color) = match rels.iter().filter(|rel| self.include.contains(*rel)).count() {
            0 => (Icon::Square, theme.text_dim),
            n if n == rels.len() => (Icon::SquareCheck, theme.accent),
            _ => (Icon::SquareMinus, theme.accent),
        };
        div()
            .id(id)
            .flex_shrink_0()
            .cursor_pointer()
            .tooltip(|_, cx| tooltip_view("Include in the commit", cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let all = rels.iter().all(|rel| this.include.contains(rel));
                for rel in &rels {
                    if all {
                        this.include.remove(rel);
                    } else {
                        this.include.insert(rel.clone());
                    }
                }
                cx.notify();
            }))
            .child(icon_el(icon, px(14.), color.into()))
    }

    fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        self.amend = !self.amend;
        // Like JetBrains, amending starts from the previous message.
        if self.amend
            && self.message_text(cx).is_empty()
            && let Some(repo) = self.repo.clone()
        {
            let message = self.message.clone();
            cx.spawn(async move |_, cx| {
                let text = cx
                    .background_executor()
                    .spawn(async move { git::last_commit_message(&repo) })
                    .await;
                if let Some(text) = text {
                    message.update(cx, |m, cx| m.set_text(&text, cx)).ok();
                }
            })
            .detach();
        }
        cx.notify();
    }

    fn on_submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        self.commit(false, cx);
    }

    fn small_button(&self, id: impl Into<gpui::ElementId>, icon: Icon, tip: &'static str, cx: &App) -> Button {
        let theme = Theme::get(cx);
        div()
            .id(id)
            .size(px(22.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .cursor_pointer()
            .hover_bg(theme.active)
            .tooltip(move |_, cx| tooltip_view(tip, cx))
            .child(icon_el(icon, px(14.), theme.text_muted.into()))
    }

    fn render_file(&self, file: &FileStatus, section: Section, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let staged = section == Section::Staged;
        let change = if staged {
            file.staged.unwrap_or(Change::Modified)
        } else if file.staged == Some(Change::Conflicted) {
            Change::Conflicted
        } else {
            file.unstaged.unwrap_or(Change::Modified)
        };
        let color = change_color(&theme, change);
        let icon = crate::ui::file_icon(&file.path, px(15.), &theme);
        let name = file.rel.rsplit('/').next().unwrap_or(&file.rel).to_string();
        let dir = file.rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let is_selected = self.selected.as_ref().is_some_and(|(r, s)| *r == file.rel && *s == staged);
        let is_menu_target = self.menu_target.as_ref().is_some_and(|(r, s)| *r == file.rel && *s == staged);
        let f = file.clone();
        let f_open = file.clone();
        let f_menu = file.clone();
        let rel = file.rel.clone();
        let deleted = change == Change::Deleted;
        let conflicted = change == Change::Conflicted;

        let row_id = match section {
            Section::Staged => "staged",
            Section::Changes => "unstaged",
            Section::Unversioned => "unversioned",
        };
        let group = SharedString::from(format!("git-row-{row_id}-{ix}"));
        div().w_full().px_1p5().child(
            div()
                .id((row_id, ix))
                .group(group.clone())
                .relative()
                .w_full()
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(26.))
                .pl_2()
                .pr_1()
                .rounded(px(6.))
                .cursor_pointer()
                .when(is_selected, |el| el.bg(theme.accent_soft))
                .when(is_menu_target, |el| el.border_1().border_color(Hsla::from(theme.accent).alpha(0.6)))
                .row_hover(theme.hover)
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.selected = Some((f_menu.rel.clone(), staged));
                        cx.emit(GitPanelEvent::Menu { file: f_menu.clone(), staged, position: e.position });
                        cx.notify();
                    }),
                )
                .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                    this.selected = Some((f.rel.clone(), staged));
                    // A conflict is resolved in the Merge view, not read in a diff.
                    if conflicted {
                        cx.emit(GitPanelEvent::OpenMerge(f.path.clone()));
                    } else if e.click_count() >= 2 && !deleted {
                        cx.emit(GitPanelEvent::OpenFile(f.path.clone()));
                    } else {
                        cx.emit(GitPanelEvent::OpenDiff { file: f.clone(), staged });
                    }
                    cx.notify();
                }))
                .when(section == Section::Unversioned, |el| el.child(self.include_checkbox(("include", ix), vec![file.rel.clone()], cx)))
                .child(icon)
                // The name gives way only when the sidebar is too narrow for it
                // (the folder goes first); the full path is in its tooltip.
                .child({
                    let path: SharedString = file.rel.clone().into();
                    div()
                        .id(("name", (staged as usize) * 100_000 + ix))
                        .min_w_0()
                        .truncate()
                        .text_color(color)
                        .when(deleted, |el| el.line_through())
                        // Not while its menu is open: GPUI still shows a tooltip that was
                        // waiting when the row was right-clicked, over the menu.
                        .when(path.contains('/') && !is_menu_target, |el| el.tooltip(move |_, cx| tooltip_view(path.clone(), cx)))
                        .child(name)
                })
                .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_dim).child(dir))
                .child(
                    div()
                        .w(px(14.))
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(change.letter()),
                )
                // Row actions float over the row's right end on hover, so they
                // never take width from the file name.
                .child(
                    div()
                        .absolute()
                        .top(px(1.))
                        .right(px(2.))
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .shadow(crate::ui::soft_shadow(&theme, 1))
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .when(!staged && !conflicted, |el| {
                            let discard = f_open.clone();
                            el.child(
                                self.small_button(("discard", ix), Icon::Undo, "Rollback Changes", cx)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(GitPanelEvent::Run(GitOp::Discard(vec![discard.clone()])));
                                    })),
                            )
                        })
                        .when(!deleted, |el| {
                            let path = f_open.path.clone();
                            el.child(
                                self.small_button(("open", (staged as usize) * 100_000 + ix), Icon::File, "Open File", cx)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(GitPanelEvent::OpenFile(path.clone()));
                                    })),
                            )
                        })
                        .child({
                            let rel = rel.clone();
                            let (icon, tip) = if staged {
                                (Icon::Minus, "Unstage")
                            } else if conflicted {
                                (Icon::Check, "Mark as Resolved")
                            } else {
                                (Icon::Plus, "Stage")
                            };
                            self.small_button(("stage", (staged as usize) * 100_000 + ix), icon, tip, cx)
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.stop_propagation();
                                    let op = if staged {
                                        GitOp::Unstage(vec![rel.clone()])
                                    } else {
                                        GitOp::Stage(vec![rel.clone()])
                                    };
                                    cx.emit(GitPanelEvent::Run(op));
                                }))
                        }),
                ),
        )
    }

    /// A file section's heading: Stash on the first one, Stage All /
    /// Unstage All, and a checkbox for all unversioned files.
    fn file_section_header(&self, section: Section, files: &[&FileStatus], first: bool, cx: &mut Context<Self>) -> AnyElement {
        let rels: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
        let mut actions = Vec::new();
        if first {
            actions.push(("stash", Icon::Archive, "Stash Changes…", GitPanelEvent::Stash));
        }
        let (title, lead) = match section {
            Section::Staged => {
                actions.push(("unstage-all", Icon::Minus, "Unstage All", GitPanelEvent::Run(GitOp::Unstage(rels))));
                ("Staged", None)
            }
            Section::Changes => {
                actions.push(("stage-all", Icon::Plus, "Stage All", GitPanelEvent::Run(GitOp::Stage(rels))));
                ("Changes", None)
            }
            Section::Unversioned => ("Unversioned Files", Some(self.include_checkbox("include-all", rels, cx).into_any_element())),
        };
        self.section_header(title, files.len(), lead, actions, cx).into_any_element()
    }

    fn section_header(
        &self,
        title: &str,
        count: usize,
        lead: Option<AnyElement>,
        actions: Vec<(&'static str, Icon, &'static str, GitPanelEvent)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .flex()
            .items_center()
            .gap_1()
            // As tall as a file row: the list draws rows of one height.
            .h(px(26.))
            .px_3()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_dim)
            // Level with the rows' checkboxes.
            .when_some(lead, |el, lead| el.child(div().mx(px(2.)).child(lead)))
            .child(title.to_uppercase())
            .child(div().px_1p5().rounded_full().bg(theme.active).text_color(theme.text_muted).child(count.to_string()))
            .child(div().flex_1())
            .children(actions.into_iter().map(|(id, icon, tip, event)| {
                self.small_button(id, icon, tip, cx).on_click(cx.listener(move |_, _, _, cx| cx.emit(event.clone())))
            }))
    }

    /// A stash: its message, with Pop / Apply / Drop on hover.
    fn render_stash(&self, stash: &Stash, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let index = stash.index;
        let group = SharedString::from(format!("git-stash-{index}"));
        // "On main: message", or without one "WIP on main: abc123 subject":
        // the message (else "WIP: subject"), then the branch.
        let (text, branch) = match stash.message.split_once(": ") {
            Some((on, text)) => match on.strip_prefix("WIP on ") {
                Some(branch) => (format!("WIP: {}", text.split_once(' ').map_or(text, |(_, subject)| subject)), branch),
                None => (text.to_string(), on.trim_start_matches("On ")),
            },
            None => (stash.message.clone(), ""),
        };
        let branch = branch.to_string();
        let full: SharedString = stash.message.clone().into();
        let button = |id: &'static str, icon: Icon, tip: &'static str, op: GitOp, cx: &mut Context<Self>| {
            self.small_button((id, index), icon, tip, cx).on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(GitPanelEvent::Run(op.clone()));
            }))
        };
        div().w_full().px_1p5().child(
            div()
                .id(("stash", index))
                .group(group.clone())
                .relative()
                .w_full()
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(26.))
                .pl_2()
                .pr_1()
                .rounded(px(6.))
                .row_hover(theme.hover)
                .tooltip(move |_, cx| tooltip_view(full.clone(), cx))
                .child(icon_el(Icon::Archive, px(14.), theme.text_muted.into()))
                .child(div().min_w_0().truncate().text_color(theme.text).child(text))
                .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_dim).child(branch))
                .child(
                    div()
                        .absolute()
                        .top(px(1.))
                        .right(px(2.))
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .shadow(crate::ui::soft_shadow(&theme, 1))
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .child(button("stash-pop", Icon::ArrowUp, "Pop (Apply and Drop)", GitOp::Unstash { index, pop: true }, cx))
                        .child(button("stash-apply", Icon::Check, "Apply (Keep the Stash)", GitOp::Unstash { index, pop: false }, cx))
                        .child(button("stash-drop", Icon::Trash, "Drop…", GitOp::DropStash(index), cx)),
                ),
        )
    }

    /// The kept stash, while it's still there.
    fn shown_kept_stash(&self) -> Option<String> {
        self.kept_stash.clone().filter(|sha| self.stashes.iter().any(|s| &s.sha == sha))
    }

    /// A box above the changes: an icon, a title with a detail, a hint and buttons.
    fn banner(&self, title: &'static str, detail: String, hint: Option<&'static str>, buttons: Vec<AnyElement>, cx: &App) -> Div {
        let theme = Theme::get(cx);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .m_2()
            .p_2()
            .rounded(px(7.))
            .bg(Hsla::from(theme.git_conflict).alpha(0.1))
            .border_1()
            .border_color(Hsla::from(theme.git_conflict).alpha(0.35))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(icon_el(Icon::GitBranch, px(14.), theme.git_conflict.into()))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title))
                    .when(!detail.is_empty(), |el| el.child(div().text_color(theme.text_muted).child(format!("· {detail}")))),
            )
            .when_some(hint, |el, hint| el.child(div().text_size(px(12.)).text_color(theme.text_muted).child(hint)))
            .child(div().flex().flex_wrap().gap_1p5().children(buttons))
    }

    fn conflict_count(&self) -> String {
        match self.status.conflicted().count() {
            0 => "conflicts resolved".to_string(),
            1 => "1 conflicted file".to_string(),
            n => format!("{n} conflicted files"),
        }
    }

    /// Resolve Simple: combines the conflicts whose sides change different words.
    fn resolve_simple_button(&self, cx: &mut Context<Self>) -> AnyElement {
        crate::ui::text_button("resolve-simple", "Resolve Simple", false, cx)
            .tooltip(|_, cx| tooltip_view("Combine the conflicts where both sides changed different words", cx))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(GitPanelEvent::Run(GitOp::ResolveSimple))))
            .into_any_element()
    }

    /// A stopped merge or rebase: what's left, and Abort / Continue.
    fn render_state_banner(&self, state: RepoState, cx: &mut Context<Self>) -> impl IntoElement {
        let conflicts = self.status.conflicted().count();
        let busy = self.busy.is_running();
        let mut buttons = vec![crate::ui::text_button("state-abort", "Abort", false, cx)
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(GitPanelEvent::Run(GitOp::Abort(state)));
            }))
            .into_any_element()];
        if conflicts > 0 {
            buttons.push(self.resolve_simple_button(cx));
        } else if !busy {
            buttons.push(
                crate::ui::text_button("state-continue", if state == RepoState::Merging { "Commit Merge" } else { "Continue" }, true, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // A merge commit takes the message box's text when there is one.
                        let message = this.message_text(cx);
                        let op = if state == RepoState::Merging && !message.is_empty() {
                            GitOp::Commit { message, amend: false, all: false, untracked: Vec::new(), push: false }
                        } else {
                            GitOp::Continue(state)
                        };
                        cx.emit(GitPanelEvent::Run(op));
                    }))
                    .into_any_element(),
            );
        }
        let hint = (conflicts > 0).then_some(
            "Click a conflicted file to merge it: yours, the result and theirs side by side. Right-click a file to take one version whole.",
        );
        self.banner(state.label(), self.conflict_count(), hint, buttons, cx)
    }

    /// Changes put back over new commits (after a pull, or a stash applied)
    /// that conflicted: no operation stopped, so there's nothing to continue,
    /// only the files to resolve and then the stash that kept them to drop.
    fn render_changes_banner(&self, kept: Option<String>, cx: &mut Context<Self>) -> impl IntoElement {
        let conflicts = self.status.conflicted().count();
        if conflicts > 0 {
            let hint = if kept.is_some() {
                "Putting your changes back stopped on conflicts; they're also kept in a stash. Click a conflicted file to merge it."
            } else {
                "Click a conflicted file to merge it. Right-click a file to take one version whole."
            };
            let buttons = vec![self.resolve_simple_button(cx)];
            return self.banner("Your changes conflict", self.conflict_count(), Some(hint), buttons, cx);
        }
        let sha = kept.unwrap_or_default();
        let buttons = vec![
            crate::ui::text_button("kept-stash-keep", "Keep It", false, cx)
                .on_click(cx.listener(|this, _, _, cx| this.set_kept_stash(None, cx)))
                .into_any_element(),
            crate::ui::text_button("kept-stash-drop", "Drop Stash", true, cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.emit(GitPanelEvent::Run(GitOp::DropKeptStash(sha.clone())));
                    this.set_kept_stash(None, cx);
                }))
                .into_any_element(),
        ];
        let hint = "Your changes are back in place. The stash that kept them while they conflicted isn't needed anymore.";
        self.banner("Conflicts resolved", String::new(), Some(hint), buttons, cx)
    }

    fn render_commit_box(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let focused = self.message.read(cx).focus_handle(cx).is_focused(window);
        let can_commit = self.can_commit(cx);
        // All changes to tracked files; unversioned ones only when checked.
        let label = if self.amend {
            "Amend Commit"
        } else if commits_all(&self.status, &self.include, false) && self.status.unstaged().next().is_some() {
            "Commit All"
        } else {
            "Commit"
        };
        let commit_button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                // Sized by their labels, sharing what's left.
                .flex_auto()
                .min_w_0()
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                // Pills, like `ui::text_button`.
                .rounded(px(13.))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .when(!can_commit, |el| el.opacity(0.45))
                .when(can_commit, |el| el.cursor_pointer())
                .when(primary, |el| el.text_color(theme.accent).border_1().border_color(Hsla::from(theme.accent).alpha(0.3)))
                .when(!primary, |el| el.text_color(theme.text))
                .child(div().truncate().child(label))
                .map(|el| {
                    let (rest, hover) = if primary {
                        (Hsla::from(theme.accent_soft), Hsla::from(theme.accent).alpha(0.24))
                    } else {
                        (Hsla::from(theme.hover), Hsla::from(theme.active))
                    };
                    el.hover_fill(rest, if can_commit { hover } else { rest })
                })
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .pb_1p5()
            .border_t_1()
            .border_color(theme.border_subtle)
            .child(focus_ring(
                div()
                    .h(px(96.))
                    .px_2p5()
                    .py_1p5()
                    .rounded(px(10.))
                    .bg(theme.background)
                    .overflow_hidden()
                    .child(self.message.clone()),
                "commit-message",
                focused,
                &theme,
            ))
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .child(
                        commit_button("commit", label, true)
                            .tooltip(|_, cx| tooltip_view(if cfg!(target_os = "macos") { "Commit (⌘↩)" } else { "Commit (Ctrl+Enter)" }, cx))
                            .on_click(cx.listener(|this, _, _, cx| this.commit(false, cx))),
                    )
                    .child(
                        commit_button("commit-push", "Commit & Push", false)
                            .on_click(cx.listener(|this, _, _, cx| this.commit(true, cx))),
                    ),
            )
            // Amend is rarely used, so it shares the hint's row instead of the buttons'.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .text_size(px(11.))
                    .text_color(theme.text_dim)
                    .child(kbd(if cfg!(target_os = "macos") { "⌘↩" } else { "Ctrl+Enter" }, cx))
                    .child("to commit")
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("amend")
                            .flex()
                            .items_center()
                            .gap_1()
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.text_muted))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_amend(cx)))
                            .child(icon_el(
                                if self.amend { Icon::SquareCheck } else { Icon::Square },
                                px(13.),
                                if self.amend { theme.accent.into() } else { theme.text_dim.into() },
                            ))
                            .child("Amend"),
                    ),
            )
    }
}

impl Focusable for GitPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.message.focus_handle(cx)
    }
}

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let root = div().key_context("GitPanel").on_action(cx.listener(Self::on_submit)).flex().flex_col().size_full();

        if self.repo.is_none() {
            let can_init = self.project.is_some();
            return root
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .text_color(theme.text_muted)
                        .child(if can_init {
                            "This folder isn't a Git repository."
                        } else {
                            "Open a folder to use Git."
                        })
                        .when(can_init, |el| {
                            el.child(
                                crate::ui::text_button("git-init", "Initialize Repository", true, cx)
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(GitPanelEvent::Run(GitOp::Init)))),
                            )
                        }),
                )
                .into_any_element();
        }

        let file_counts = [self.status.staged().count(), self.status.unstaged().count(), self.status.untracked().count()];
        let stash_count = self.stashes.len();
        let list = if file_counts == [0; 3] && stash_count == 0 {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .p_3()
                        .text_color(theme.text_dim)
                        .child(icon_el(Icon::CircleCheck, px(15.), theme.success.into()))
                        .child("No changes"),
                )
                .into_any_element()
        } else {
            // A heading and the rows of each non-empty section (a "No changes"
            // row when only stashes are left); only the rows on screen are built.
            let section = |n: usize| if n == 0 { 0 } else { 1 + n };
            let no_changes = usize::from(file_counts == [0; 3]);
            uniform_list(
                "git-changes",
                no_changes + file_counts.into_iter().map(section).sum::<usize>() + section(stash_count),
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let theme = Theme::get(cx).clone();
                    let status = this.status.clone();
                    let sections: [(Section, Vec<&FileStatus>); 3] = [
                        (Section::Staged, status.staged().collect()),
                        (Section::Changes, status.unstaged().collect()),
                        (Section::Unversioned, status.untracked().collect()),
                    ];
                    // The first heading offers Stash.
                    let first = sections.iter().position(|(_, files)| !files.is_empty());
                    range
                        .map(|ix| {
                            if no_changes == 1 && ix == 0 {
                                return div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .h(px(26.))
                                    .px_3()
                                    .text_color(theme.text_dim)
                                    .child(icon_el(Icon::CircleCheck, px(15.), theme.success.into()))
                                    .child("No changes")
                                    .into_any_element();
                            }
                            let mut ix = ix - no_changes;
                            for (n, (section, files)) in sections.iter().enumerate().filter(|(_, (_, files))| !files.is_empty()) {
                                if ix == 0 {
                                    return this.file_section_header(*section, files, first == Some(n), cx);
                                }
                                if let Some(file) = files.get(ix - 1) {
                                    return this.render_file(file, *section, ix - 1, cx).into_any_element();
                                }
                                ix -= 1 + files.len();
                            }
                            match ix {
                                0 => this.section_header("Stashes", this.stashes.len(), None, Vec::new(), cx).into_any_element(),
                                ix => match this.stashes.clone().get(ix - 1) {
                                    Some(stash) => this.render_stash(stash, cx).into_any_element(),
                                    None => div().into_any_element(),
                                },
                            }
                        })
                        .collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .flex_1()
            .min_h_0()
            .pb_2()
            .into_any_element()
        };

        root
            .map(|el| match self.status.state {
                Some(state) => el.child(self.render_state_banner(state, cx).into_any_element()),
                None => {
                    let kept = self.shown_kept_stash();
                    if self.status.conflicted().next().is_some() || kept.is_some() {
                        el.child(self.render_changes_banner(kept, cx).into_any_element())
                    } else {
                        el
                    }
                }
            })
            .child(
                // Progress floats over the list so the rows don't move.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(list)
                    .child(crate::ui::list_scrollbar("git-changes-scrollbar", &self.scroll, cx))
                    .when_some(self.busy.label().cloned(), |el, busy| {
                        el.child(
                            div().absolute().left_0().right_0().bottom(px(8.)).flex().justify_center().child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .h(px(26.))
                                    .px_2p5()
                                    .rounded_full()
                                    .bg(theme.elevated)
                                    .border_1()
                                    .border_color(theme.border)
                                    .shadow(crate::ui::soft_shadow(&theme, 2))
                                    .text_size(px(12.))
                                    .text_color(theme.text_muted)
                                    .child(icon_el(Icon::Refresh, px(12.), theme.accent.into()))
                                    .child(busy),
                            ),
                        )
                    }),
            )
            .child(self.render_commit_box(window, cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rel: &str, staged: Option<Change>, unstaged: Option<Change>) -> FileStatus {
        FileStatus { path: PathBuf::from(rel), rel: rel.into(), staged, unstaged, blobs: None }
    }

    #[test]
    fn checked_new_files_leave_unstaged_changes_out() {
        let mut status = RepoStatus {
            files: vec![
                file("changed.rs", None, Some(Change::Modified)),
                file("new.rs", None, Some(Change::Untracked)),
            ],
            ..RepoStatus::default()
        };
        let mut include = HashSet::new();
        // Nothing chosen: Commit All, but an amend only rewrites the message.
        assert!(commits_all(&status, &include, false));
        assert!(!commits_all(&status, &include, true));
        // Only the new file checked: just it.
        include.insert("new.rs".to_string());
        assert!(!commits_all(&status, &include, false));
        // Something staged: what's staged (and checked), never the rest.
        include.clear();
        status.files.push(file("staged.rs", Some(Change::Modified), None));
        assert!(!commits_all(&status, &include, false));
    }
}
