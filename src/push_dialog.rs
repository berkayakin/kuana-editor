//! The Push dialog (JetBrains ⌘⇧K): where the branch goes, the commits a
//! push would send (each with its changed files, which open as diffs), then
//! Push or Cancel.

use std::{ops::Range, path::PathBuf};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement, Render, ScrollStrategy, Task,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    diff_view::DiffSource,
    editor::{Cancel, MoveDown, MoveUp},
    git::{self, CommitFile, PushPreview},
    git_panel::change_color,
    theme::Theme,
    ui::{HoverFill, icon_el, modal_panel, text_button, tooltip_view},
};

actions!(push_dialog, [ConfirmPush]);

const ROW_HEIGHT: f32 = 30.;

pub enum PushDialogEvent {
    /// `remote`: where a branch without an upstream goes; `tags`: tags to
    /// push with it.
    Push { force: bool, remote: Option<String>, tags: Vec<String> },
    ManageRemotes,
    OpenDiff(DiffSource),
    Dismissed,
}

pub struct PushDialog {
    repo: PathBuf,
    preview: Option<Result<PushPreview, String>>,
    selected: usize,
    /// The selected commit's message and files.
    details: Option<(String, Result<(String, Vec<CommitFile>), String>)>,
    force: bool,
    /// The remote a new branch is pushed to (origin, else the first).
    remote: Option<String>,
    /// Local tags the remote doesn't have (None: still asking, or it couldn't be asked).
    tags: Option<Vec<String>>,
    /// Whether they go with the push (on when there are any).
    push_tags: bool,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    details_scroll: gpui::ScrollHandle,
    details_task: Option<Task<()>>,
}

impl EventEmitter<PushDialogEvent> for PushDialog {}

impl PushDialog {
    pub fn new(repo: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let root = repo.clone();
        cx.spawn(async move |this, cx| {
            let preview = cx.background_executor().spawn(async move { git::push_preview(&root) }).await;
            this.update(cx, |this, cx| {
                if let Ok(p) = &preview {
                    this.remote = p.remotes.iter().find(|r| *r == "origin").or(p.remotes.first()).cloned();
                }
                this.preview = Some(preview);
                this.select(0, cx);
                cx.notify();
            })
            .ok();
            // Then which tags are new to the remote: that asks it, so after the commits show.
            let Ok(remote) = this.update(cx, |this, _| this.remote.clone()) else { return };
            let root = this.update(cx, |this, _| this.repo.clone()).ok();
            let tags = cx
                .background_executor()
                .spawn(async move {
                    let root = root?;
                    let remote = git::push_remote(&root).or(remote)?;
                    git::unpushed_tags(&root, &remote)
                })
                .await;
            this.update(cx, |this, cx| {
                this.push_tags = tags.as_ref().is_some_and(|t| !t.is_empty());
                this.tags = tags;
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self {
            repo,
            preview: None,
            selected: 0,
            details: None,
            force: false,
            remote: None,
            tags: None,
            push_tags: false,
            focus_handle,
            scroll: UniformListScrollHandle::new(),
            details_scroll: gpui::ScrollHandle::new(),
            details_task: None,
        }
    }

    /// Focus returns here after a diff opened from the dialog closes.
    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    fn commits(&self) -> &[git::LogEntry] {
        match &self.preview {
            Some(Ok(p)) => &p.commits,
            _ => &[],
        }
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.commits().get(ix).cloned() else { return };
        self.selected = ix;
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        if self.details.as_ref().is_some_and(|(sha, _)| *sha == entry.sha) {
            cx.notify();
            return;
        }
        let repo = self.repo.clone();
        self.details_task = Some(cx.spawn(async move |this, cx| {
            let sha = entry.sha.clone();
            let details = cx.background_executor().spawn(async move { git::commit_details(&repo, &entry.sha) }).await;
            this.update(cx, |this, cx| {
                this.details = Some((sha, details));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The tags going with the push.
    fn tags_to_push(&self) -> Vec<String> {
        self.tags.clone().filter(|_| self.push_tags).unwrap_or_default()
    }

    /// A push would do something: there's a remote, and new commits, a new
    /// branch to create there or tags to send.
    fn can_push(&self) -> bool {
        let tags = !self.tags_to_push().is_empty();
        matches!(&self.preview, Some(Ok(p)) if p.has_remote && (!p.commits.is_empty() || p.upstream.is_none() || tags))
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if matches!(&self.preview, Some(Ok(p)) if !p.has_remote) {
            cx.emit(PushDialogEvent::ManageRemotes);
        } else if self.can_push() {
            cx.emit(PushDialogEvent::Push { force: self.force, remote: self.remote.clone(), tags: self.tags_to_push() });
        }
    }

    fn on_confirm(&mut self, _: &ConfirmPush, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PushDialogEvent::Dismissed);
    }

    fn render_details(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = div()
            .id("push-details")
            .size_full()
            .border_l_1()
            .border_color(theme.border_subtle)
            .overflow_y_scroll()
            .track_scroll(&self.details_scroll);
        let entry = self.commits().get(self.selected).cloned();
        let (Some(entry), Some((sha, details))) = (entry, self.details.as_ref()) else {
            return panel;
        };
        let details = match details {
            Ok(d) if *sha == entry.sha => d,
            Ok(_) => return panel.child(div().p_3().text_color(theme.text_dim).child("Loading…")),
            Err(e) => return panel.child(div().p_3().text_color(theme.error).child(e.clone())),
        };
        let body = details.0.split_once('\n').map(|(_, b)| b.trim().to_string()).unwrap_or_default();
        panel
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border_subtle)
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(entry.summary.clone()))
                    .when(!body.is_empty(), |el| el.child(div().text_size(px(12.5)).text_color(theme.text_muted).child(body)))
                    .child(div().text_size(px(12.)).text_color(theme.text_dim).child(format!(
                        "{} · {} · {}",
                        entry.author,
                        entry.date,
                        &entry.sha[..entry.sha.len().min(10)]
                    ))),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_dim)
                    .child(format!("{} FILE{}", details.1.len(), if details.1.len() == 1 { "" } else { "S" })),
            )
            .children(details.1.iter().enumerate().map(|(ix, file)| {
                let icon = crate::ui::file_icon(&PathBuf::from(&file.rel), px(14.), theme);
                let name = file.rel.rsplit('/').next().unwrap_or(&file.rel).to_string();
                let dir = file.rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
                let color = change_color(theme, file.change);
                let source = DiffSource::Commit { sha: entry.sha.clone(), summary: entry.summary.clone(), file: file.clone() };
                div().w_full().px_1p5().child(
                    div()
                        .id(("push-file", ix))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .h(px(24.))
                        .px_2()
                        .rounded(px(5.))
                        .cursor_pointer()
                        .row_hover(theme.hover)
                        .tooltip(|_, cx| tooltip_view("Show Diff", cx))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(PushDialogEvent::OpenDiff(source.clone()))))
                        .child(icon)
                        .child(div().flex_shrink_0().text_color(color).child(name))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_dim).child(dir))
                        .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(file.change.letter())),
                )
            }))
    }
}

impl Focusable for PushDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PushDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let preview = self.preview.clone();

        // main → origin/main (new); with several remotes, a new branch's is a choice.
        let route = match &preview {
            Some(Ok(p)) => {
                let remote = self.remote.clone().unwrap_or_else(|| "origin".into());
                let choose = p.upstream.is_none() && p.remotes.len() > 1;
                let target = match &p.upstream {
                    Some(upstream) => upstream.clone(),
                    None if choose => p.branch.clone(),
                    None => format!("{remote}/{}", p.branch),
                };
                let remotes = choose.then(|| {
                    crate::ui::dock(&theme).children(p.remotes.iter().enumerate().map(|(ix, name)| {
                        let on = *name == remote;
                        let (name, label) = (name.clone(), name.clone());
                        div()
                            .id(("push-remote", ix))
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded(px(7.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(if on { theme.text } else { theme.text_muted })
                            .when(on, |el| crate::ui::segment_on(el, &theme))
                            .tooltip(|_, cx| tooltip_view("Push the New Branch to This Remote", cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remote = Some(name.clone());
                                cx.notify();
                            }))
                            .child(label)
                            .hover_bg(theme.hover)
                    }))
                });
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_size(px(12.5))
                        .child(icon_el(Icon::GitBranch, px(13.), theme.text_muted.into()))
                        .child(div().text_color(theme.text).child(p.branch.clone()))
                        .child(div().text_color(theme.text_dim).child("→"))
                        .children(remotes)
                        .when(choose, |el| el.child(div().text_color(theme.text_dim).child("/")))
                        .child(div().text_color(theme.text).child(target))
                        .when(p.upstream.is_none(), |el| {
                            el.child(
                                div()
                                    .px_1p5()
                                    .rounded(px(4.))
                                    .bg(Hsla::from(theme.git_added).alpha(0.15))
                                    .text_size(px(11.))
                                    .text_color(theme.git_added)
                                    .child("new branch"),
                            )
                        }),
                )
            }
            _ => None,
        };
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(46.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(icon_el(Icon::ArrowUp, px(15.), theme.text_muted.into()))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Push Commits"))
            .child(div().flex_1())
            .children(route)
            .child(
                crate::ui::icon_button("push-remotes", Icon::Cloud, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Manage Remotes…", cx))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PushDialogEvent::ManageRemotes))),
            );

        let commits = self.commits().to_vec();
        let list = match &preview {
            None => div().flex_1().p_4().text_color(theme.text_dim).child("Looking for commits to push…").into_any_element(),
            Some(Err(e)) => div().flex_1().p_4().text_color(theme.error).child(e.clone()).into_any_element(),
            Some(Ok(p)) if commits.is_empty() => {
                let tags = self.tags.as_ref().is_some_and(|t| !t.is_empty());
                let text = if p.upstream.is_some() && tags {
                    "No new commits; the remote doesn't have some of your tags yet."
                } else if p.upstream.is_some() {
                    "Everything is pushed: there are no new commits."
                } else {
                    "No new commits; pushing creates the branch on the remote."
                };
                div().flex_1().p_4().text_color(theme.text_dim).child(text).into_any_element()
            }
            Some(Ok(_)) => {
                let selected = self.selected;
                uniform_list("push-commits", commits.len(), {
                    cx.processor(move |_this, range: Range<usize>, _window, cx| {
                        let theme = Theme::get(cx).clone();
                        range
                            .map(|ix| {
                                let e = &commits[ix];
                                let is_selected = ix == selected;
                                div().w_full().px_1p5().child(
                                    div()
                                        .id(("push-commit", ix))
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .h(px(ROW_HEIGHT))
                                        .px_2p5()
                                        .rounded(px(8.))
                                        .cursor_pointer()
                                        .relative()
                                        .when(is_selected, |el| el.bg(theme.accent_soft))
                                        .row_hover(theme.hover)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.focus(window);
                                            this.select(ix, cx);
                                        }))
                                        .child(icon_el(Icon::GitCommit, px(13.), theme.text_dim.into()))
                                        .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(e.summary.clone()))
                                        .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_muted).child(e.author.clone()))
                                        .child(div().flex_shrink_0().w(px(112.)).text_size(px(12.)).text_color(theme.text_dim).child(e.date.clone())),
                                )
                            })
                            .collect()
                    })
                })
                .track_scroll(self.scroll.clone())
                .flex_1()
                .h_full()
                .py_1()
                .into_any_element()
            }
        };

        // Why a push might not work, or what it's about to do.
        let note: Option<(String, Hsla)> = match &preview {
            Some(Ok(p)) if !p.has_remote => Some(("No remote is configured: add the repository to push to.".into(), theme.error.into())),
            Some(Ok(p)) if p.behind > 0 => Some((
                format!(
                    "{} has {} commit{} you don't have. Update the project first, or force push to replace them.",
                    p.upstream.clone().unwrap_or_default(),
                    p.behind,
                    if p.behind == 1 { "" } else { "s" }
                ),
                theme.warning.into(),
            )),
            Some(Ok(p)) if !p.commits.is_empty() => Some((
                format!("{} commit{} to push", p.commits.len(), if p.commits.len() == 1 { "" } else { "s" }),
                theme.text_dim.into(),
            )),
            Some(Ok(_)) if !self.tags_to_push().is_empty() => Some(("Only tags to push".into(), theme.text_dim.into())),
            _ => None,
        };
        // New tags: listed on a checkbox, on to start with.
        let tags_box = self.tags.clone().filter(|t| !t.is_empty()).map(|tags| {
            let on = self.push_tags;
            let shown: Vec<&str> = tags.iter().take(3).map(String::as_str).collect();
            let label = match tags.len() {
                n if n > 3 => format!("Push tags ({}, +{})", shown.join(", "), n - 3),
                _ => format!("Push tags ({})", shown.join(", ")),
            };
            let tip = format!("Tags the remote doesn't have yet: {}", tags.join(", "));
            div()
                .id("push-tags")
                .flex()
                .items_center()
                .gap_1()
                .min_w_0()
                .flex_shrink()
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.push_tags = !this.push_tags;
                    cx.notify();
                }))
                .child(icon_el(if on { Icon::SquareCheck } else { Icon::Square }, px(15.), if on { theme.accent.into() } else { theme.text_dim.into() }))
                .child(div().min_w_0().truncate().child(label))
        });
        let can_push = self.can_push();
        let no_remote = matches!(&preview, Some(Ok(p)) if !p.has_remote);
        let force = self.force;
        let footer = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(52.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    // Auto basis: a zero one (`flex_1`) can wrap the note a letter per line.
                    .flex_auto()
                    .min_w_0()
                    .text_size(px(12.))
                    .when_some(note, |el, (text, color)| el.text_color(color).child(text)),
            )
            .children(tags_box)
            .child(
                div()
                    .id("push-force")
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .tooltip(|_, cx| tooltip_view("Replace the remote branch (only if it hasn't moved since you last fetched)", cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.force = !this.force;
                        cx.notify();
                    }))
                    .child(icon_el(
                        if force { Icon::SquareCheck } else { Icon::Square },
                        px(15.),
                        if force { theme.warning.into() } else { theme.text_dim.into() },
                    ))
                    .child("Force push"),
            )
            .child(text_button("push-cancel", "Cancel", false, cx).on_click(cx.listener(|_, _, _, cx| cx.emit(PushDialogEvent::Dismissed))))
            .child(
                text_button("push-confirm", if no_remote { "Add Remote…" } else if force { "Force Push" } else { "Push" }, true, cx)
                    .when(!can_push && !no_remote, |el| el.opacity(0.45))
                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
            );

        modal_panel(cx)
            .key_context("PushDialog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .w(px(920.))
            .max_w(window.viewport_size().width - px(48.))
            .h(px(520.).min(crate::workspace::modal_max_height(window)))
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(list)
                            .child(crate::ui::list_scrollbar("push-commits-scrollbar", &self.scroll, cx)),
                    )
                    .child(
                        div()
                            .relative()
                            .w(px(340.))
                            .flex_shrink_0()
                            .h_full()
                            .child(self.render_details(&theme, cx))
                            .child(crate::ui::scrollbar("push-details-scrollbar", &self.details_scroll, cx)),
                    ),
            )
            .child(footer)
    }
}
