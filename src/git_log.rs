//! The Git log (JetBrains ⌘9 / Alt+9): commits with a branch graph, a text
//! filter, and the selected commit's message and changed files. With a file
//! set, it's that file's history (following renames).

use std::{ops::Range, path::{Path, PathBuf}, sync::Arc};

use gpui::{
    App, Bounds, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement,
    PathBuilder, Pixels, Render, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, Window,
    canvas, div, point, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    diff_view::DiffSource,
    editor::{Editor, EditorEvent, MoveDown, MoveUp},
    git::{self, CommitFile, LogEntry},
    git_panel::change_color,
    theme::Theme,
    ui::{HoverFill, icon_el, input_box, tooltip_view},
};

const ROW_HEIGHT: f32 = 24.0;
const LANE_WIDTH: f32 = 12.0;
const MAX_COMMITS: usize = 3000;

pub enum GitLogEvent {
    OpenDiff(DiffSource),
    /// A commit was right-clicked.
    Menu { entry: git::LogEntry, position: gpui::Point<gpui::Pixels> },
    Close,
}

/// One row of the commit graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRow {
    /// The commit's column.
    pub col: usize,
    /// Columns in use on this row.
    pub width: usize,
    /// Line segments: (from column, from y, to column, to y), with y 0 = top,
    /// 1 = the commit's center, 2 = bottom.
    pub lines: Vec<(usize, u8, usize, u8)>,
}

/// Lays out lanes for commits listed newest first: each commit continues the
/// lane that expected it and hands it on to its first parent; other parents
/// get lanes of their own.
pub fn graph(entries: &[LogEntry]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows = Vec::with_capacity(entries.len());
    for e in entries {
        let sha = e.sha.as_str();
        let col = lanes.iter().position(|l| *l == Some(sha)).unwrap_or_else(|| {
            let free = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
            if free == lanes.len() {
                lanes.push(None);
            }
            free
        });
        let mut row = GraphRow { col, ..Default::default() };
        // Top half: every lane comes down; the ones expecting this commit meet at it.
        for (i, lane) in lanes.iter().enumerate() {
            match lane {
                Some(s) if *s == sha => row.lines.push((i, 0, col, 1)),
                Some(_) => row.lines.push((i, 0, i, 1)),
                None => {}
            }
        }
        for lane in lanes.iter_mut().filter(|l| **l == Some(sha)) {
            *lane = None;
        }
        let passing: Vec<bool> = lanes.iter().map(Option::is_some).collect();
        // Bottom half: to each parent's lane (new lanes as needed).
        let mut targets = Vec::new();
        for (k, parent) in e.parents.iter().enumerate() {
            let target = match lanes.iter().position(|l| *l == Some(parent.as_str())) {
                Some(j) => j,
                None if k == 0 => {
                    lanes[col] = Some(parent);
                    col
                }
                None => {
                    let j = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
                    if j == lanes.len() {
                        lanes.push(None);
                    }
                    lanes[j] = Some(parent);
                    j
                }
            };
            targets.push(target);
        }
        for (i, lane) in lanes.iter().enumerate() {
            if lane.is_none() {
                continue;
            }
            if targets.contains(&i) {
                row.lines.push((col, 1, i, 2));
            }
            if passing.get(i).copied().unwrap_or(false) {
                row.lines.push((i, 1, i, 2));
            }
        }
        while lanes.last() == Some(&None) {
            lanes.pop();
        }
        row.width = row.lines.iter().map(|l| l.0.max(l.2) + 1).max().unwrap_or(1).max(col + 1);
        rows.push(row);
    }
    rows
}

struct Details {
    sha: String,
    message: String,
    files: Vec<CommitFile>,
}

pub struct GitLog {
    repo: Option<PathBuf>,
    /// Show only this file's history (repository-relative path).
    path: Option<String>,
    all_branches: bool,
    entries: Arc<Vec<LogEntry>>,
    graph: Arc<Vec<GraphRow>>,
    /// Indices into `entries` that match the filter.
    visible: Arc<Vec<usize>>,
    filter: Entity<Editor>,
    selected: Option<usize>,
    details: Option<Result<Details, String>>,
    loading: bool,
    error: Option<String>,
    scroll: UniformListScrollHandle,
    details_scroll: gpui::ScrollHandle,
    focus_handle: FocusHandle,
    load_task: Option<Task<()>>,
    details_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GitLogEvent> for GitLog {}

impl GitLog {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| Editor::single_line("Filter by message, author or hash", cx));
        let subscription = cx.subscribe(&filter, |this, _, _: &EditorEvent, cx| this.apply_filter(cx));
        Self {
            repo: None,
            path: None,
            all_branches: true,
            entries: Arc::new(Vec::new()),
            graph: Arc::new(Vec::new()),
            visible: Arc::new(Vec::new()),
            filter,
            selected: None,
            details: None,
            loading: false,
            error: None,
            scroll: UniformListScrollHandle::new(),
            details_scroll: gpui::ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            load_task: None,
            details_task: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn set_repo(&mut self, repo: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.repo != repo {
            self.repo = repo;
            self.path = None;
            // Everything indexing the old repository's commits goes with them:
            // `visible` left behind would index an empty `entries` when drawn.
            self.entries = Arc::new(Vec::new());
            self.graph = Arc::new(Vec::new());
            self.selected = None;
            self.details = None;
            self.details_task = None;
            self.apply_filter(cx);
            self.reload(cx);
        }
    }

    /// Shows one file's history, or the whole log for `None`.
    pub fn show_path(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.path != path {
            self.path = path;
            self.selected = None;
            self.reload(cx);
        }
    }

    pub fn focus_filter(&self, window: &mut Window, cx: &App) {
        window.focus(&self.filter.focus_handle(cx));
    }

    /// Re-reads the log (after commits, fetches and checkouts), keeping the
    /// selected commit when it's still there.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.entries = Arc::new(Vec::new());
            self.apply_filter(cx);
            return;
        };
        let (path, all) = (self.path.clone(), self.all_branches);
        self.loading = true;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    git::log(&repo, MAX_COMMITS, all, path.as_deref()).map(|entries| {
                        let graph = graph(&entries);
                        (entries, graph)
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                let selected_sha = this.selected.and_then(|i| this.entries.get(i)).map(|e| e.sha.clone());
                match result {
                    Ok((entries, graph)) => {
                        this.error = None;
                        this.entries = Arc::new(entries);
                        this.graph = Arc::new(graph);
                    }
                    Err(e) => this.error = Some(e),
                }
                this.selected = selected_sha.and_then(|sha| this.entries.iter().position(|e| e.sha == sha));
                this.apply_filter(cx);
                if this.selected.is_none() && !this.visible.is_empty() {
                    this.select(this.visible[0], cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let needle = self.filter.read(cx).text(cx).trim().to_lowercase();
        let visible: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                needle.is_empty()
                    || e.summary.to_lowercase().contains(&needle)
                    || e.author.to_lowercase().contains(&needle)
                    || e.sha.starts_with(&needle)
                    || e.refs.iter().any(|r| r.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        self.visible = Arc::new(visible);
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(entry), Some(repo)) = (self.entries.get(ix).cloned(), self.repo.clone()) else { return };
        if self.selected == Some(ix) && self.details.is_some() {
            return;
        }
        self.selected = Some(ix);
        if let Some(pos) = self.visible.iter().position(|&v| v == ix) {
            self.scroll.scroll_to_item(pos, ScrollStrategy::Center);
        }
        let sha = entry.sha.clone();
        self.details_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { git::commit_details(&repo, &entry.sha) }).await;
            this.update(cx, |this, cx| {
                this.details = Some(result.map(|(message, files)| Details { sha, message, files }));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let pos = self.selected.and_then(|s| self.visible.iter().position(|&v| v == s)).unwrap_or(0) as isize;
        let next = (pos + delta).clamp(0, self.visible.len() as isize - 1) as usize;
        self.select(self.visible[next], cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn render_graph(&self, row: &GraphRow, width: usize, palette: &Arc<Vec<Hsla>>) -> impl IntoElement {
        let row = row.clone();
        let palette = palette.clone();
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, _, window, _| {
                let x = |col: usize| bounds.left() + px(LANE_WIDTH * col as f32 + LANE_WIDTH / 2. + 4.);
                let y = |at: u8| bounds.top() + bounds.size.height * (at as f32 / 2.);
                for &(c0, y0, c1, y1) in &row.lines {
                    // A lane's color follows the column it runs in (the lower end).
                    let color = palette[if y1 == 2 { c1 } else { c0 } % palette.len()];
                    let mut path = PathBuilder::stroke(px(1.5));
                    let (a, b) = (point(x(c0), y(y0)), point(x(c1), y(y1)));
                    path.move_to(a);
                    if c0 == c1 {
                        path.line_to(b);
                    } else {
                        // Bend smoothly between lanes.
                        let mid = (a.y + b.y) / 2.;
                        path.cubic_bezier_to(b, point(a.x, mid), point(b.x, mid));
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, color);
                    }
                }
                let center = point(x(row.col), y(1));
                let dot = px(4.);
                window.paint_quad(
                    gpui::fill(Bounds::new(point(center.x - dot, center.y - dot), gpui::size(dot * 2., dot * 2.)), palette[row.col % palette.len()])
                        .corner_radii(dot),
                );
            },
        )
        .w(px(LANE_WIDTH * width as f32 + 8.))
        .h_full()
        .flex_shrink_0()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let panel = div()
            .id("log-details")
            .size_full()
            .border_l_1()
            .border_color(theme.border_subtle)
            .overflow_y_scroll()
            .track_scroll(&self.details_scroll);
        let entry = self.selected.and_then(|i| self.entries.get(i)).cloned();
        let (Some(entry), Some(details)) = (entry, self.details.as_ref()) else {
            return panel.child(div().p_3().text_color(theme.text_dim).child("Select a commit"));
        };
        let details = match details {
            Ok(d) if d.sha == entry.sha => d,
            Ok(_) => return panel.child(div().p_3().text_color(theme.text_dim).child("Loading…")),
            Err(e) => return panel.child(div().p_3().text_color(theme.error).child(e.clone())),
        };
        let body = details.message.split_once('\n').map(|(_, b)| b.trim().to_string()).unwrap_or_default();
        let sha = entry.sha.clone();
        let email: SharedString = entry.email.clone().into();
        let files = details.files.clone();
        let summary = entry.summary.clone();
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
                    .when(!body.is_empty(), |el| el.child(div().text_size(px(12.5)).text_color(theme.text_muted).whitespace_normal().child(body)))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_x_1p5()
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .child(
                                div()
                                    .id("log-author")
                                    .text_color(theme.text)
                                    .tooltip(move |_, cx| tooltip_view(email.clone(), cx))
                                    .child(entry.author.clone()),
                            )
                            .child(format!("· {} · {}", git::relative_time(entry.time), entry.date))
                            .child("·")
                            .child(div().font_family(theme.mono_font.clone()).child(entry.sha[..entry.sha.len().min(10)].to_string()))
                            .child(
                                div()
                                    .id("copy-sha")
                                    .size(px(18.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .hover_bg(theme.hover)
                                    .tooltip(|_, cx| tooltip_view("Copy Commit Hash", cx))
                                    .on_click(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(sha.clone())))
                                    .child(icon_el(Icon::Copy, px(12.), theme.text_muted.into())),
                            )
                            .when(entry.parents.len() > 1, |el| el.child(div().text_color(theme.text_dim).child("· merge"))),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_dim)
                    .child(format!("{} FILE{}", files.len(), if files.len() == 1 { "" } else { "S" })),
            )
            .children(files.into_iter().enumerate().map(|(ix, file)| {
                let icon = crate::ui::file_icon(Path::new(&file.rel), px(15.), &theme);
                let name = file.rel.rsplit('/').next().unwrap_or(&file.rel).to_string();
                let dir = file.rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
                let color = change_color(&theme, file.change);
                let source = DiffSource::Commit { sha: entry.sha.clone(), summary: summary.clone(), file: file.clone() };
                div().w_full().px_1p5().child(
                    div()
                        .id(("commit-file", ix))
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
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(GitLogEvent::OpenDiff(source.clone()))))
                        .child(icon)
                        .child(div().flex_shrink_0().text_color(color).child(name))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_dim).child(dir))
                        .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(file.change.letter())),
                )
            }))
    }
}

impl Focusable for GitLog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitLog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let toggle = |id: &'static str, icon: Icon, on: bool, tip: &'static str| {
            div()
                .id(id)
                .size(px(24.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .when(on, |el| crate::ui::segment_on(el, &theme))
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .child(icon_el(icon, px(14.), if on { theme.text.into() } else { theme.text_muted.into() }))
        };
        let title: SharedString = match &self.path {
            Some(path) => format!("HISTORY · {}", path.rsplit('/').next().unwrap_or(path)).into(),
            None => "GIT LOG".into(),
        };
        let header = crate::ui::panel_header(&theme)
            .gap_1()
            .child(div().mr_1().flex_shrink_0().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child(title))
            .when(self.path.is_some(), |el| {
                el.child(toggle("log-whole", Icon::X, false, "Show the Whole Log").on_click(cx.listener(|this, _, _, cx| this.show_path(None, cx))))
            })
            // Compact, like the header's buttons, so the header keeps some room around it.
            .child(div().w(px(280.)).child(input_box(&self.filter, Some(Icon::Search), window, cx).h(px(24.)).rounded(px(6.)).text_size(px(12.5))))
            .when(self.path.is_none(), |el| {
                el.child(toggle("log-all", Icon::GitBranch, self.all_branches, "Show All Branches").on_click(cx.listener(|this, _, _, cx| {
                    this.all_branches = !this.all_branches;
                    this.reload(cx);
                })))
            })
            .child(toggle("log-refresh", Icon::Refresh, false, "Refresh").on_click(cx.listener(|this, _, _, cx| this.reload(cx))))
            .when(self.loading, |el| el.child(div().text_size(px(11.5)).text_color(theme.text_dim).child("Loading…")))
            .child(div().flex_1())
            .child(crate::ui::panel_close_button("log-close", cx).on_click(cx.listener(|_, _, _, cx| cx.emit(GitLogEvent::Close))));

        let list = if let Some(err) = &self.error {
            div().flex_1().p_3().text_color(theme.error).child(err.clone()).into_any_element()
        } else if self.repo.is_none() {
            div().flex_1().p_3().text_color(theme.text_dim).child("Not a Git repository").into_any_element()
        } else if self.visible.is_empty() {
            let text = if self.loading { "" } else if self.entries.is_empty() { "No commits yet" } else { "No matching commits" };
            div().flex_1().p_3().text_color(theme.text_dim).child(text).into_any_element()
        } else {
            let (entries, graph, visible) = (self.entries.clone(), self.graph.clone(), self.visible.clone());
            // History of one file, or a filtered list, has no meaningful graph.
            let show_graph = self.path.is_none() && visible.len() == entries.len();
            let graph_width = if show_graph { graph.iter().map(|g| g.width).max().unwrap_or(1).min(12) } else { 0 };
            let palette: Arc<Vec<Hsla>> = Arc::new(
                [theme.git_modified, theme.git_added, theme.warning, theme.terminal_magenta, theme.terminal_cyan, theme.git_deleted, theme.terminal_yellow]
                    .into_iter()
                    .map(Hsla::from)
                    .collect(),
            );
            let selected = self.selected;
            uniform_list("log-rows", visible.len(), {
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    let theme = Theme::get(cx).clone();
                    range
                        .map(|pos| {
                            let ix = visible[pos];
                            let e = &entries[ix];
                            let is_selected = selected == Some(ix);
                            let refs = e.refs.iter().map(|r| {
                                let (label, color): (String, Hsla) = if let Some(tag) = r.strip_prefix("tag: ") {
                                    (tag.to_string(), theme.warning.into())
                                } else if let Some(head) = r.strip_prefix("HEAD -> ") {
                                    (head.to_string(), theme.git_added.into())
                                } else if r.contains('/') {
                                    (r.clone(), theme.git_modified.into())
                                } else {
                                    (r.clone(), theme.git_added.into())
                                };
                                div()
                                    .flex_shrink_0()
                                    .px_1p5()
                                    .rounded(px(4.))
                                    .bg(color.alpha(0.14))
                                    .text_color(color)
                                    .text_size(px(11.))
                                    .child(label)
                            });
                            div().w_full().px_1().child(
                                div()
                                    .id(("log-row", ix))
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .h(px(ROW_HEIGHT))
                                    .pr_2()
                                    .rounded(px(7.))
                                    .cursor_pointer()
                                    .when(is_selected, |el| el.bg(theme.accent_soft))
                                    .row_hover(theme.hover)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                        window.focus(&this.focus_handle);
                                        this.select(ix, cx);
                                    }))
                                    .on_mouse_down(
                                        gpui::MouseButton::Right,
                                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                            window.focus(&this.focus_handle);
                                            this.select(ix, cx);
                                            let Some(entry) = this.entries.get(ix).cloned() else { return };
                                            cx.emit(GitLogEvent::Menu { entry, position: event.position });
                                        }),
                                    )
                                    .when(show_graph, |el| el.child(this.render_graph(&graph[ix], graph_width, &palette)))
                                    .when(!show_graph, |el| el.pl_2())
                                    .children(refs)
                                    .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(e.summary.clone()))
                                    .child(div().w(px(150.)).flex_shrink_0().truncate().text_size(px(12.)).text_color(theme.text_muted).child(e.author.clone()))
                                    .child(div().w(px(118.)).flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(e.date.clone())),
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
        };

        div()
            .key_context("GitLog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .flex()
            .flex_col()
            .size_full()
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
                            .child(crate::ui::list_scrollbar("log-commits-scrollbar", &self.scroll, cx)),
                    )
                    .child(
                        div()
                            .relative()
                            .w(px(380.))
                            .flex_shrink_0()
                            .h_full()
                            .child(self.render_details(cx))
                            .child(crate::ui::scrollbar("log-details-scrollbar", &self.details_scroll, cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sha: &str, parents: &[&str]) -> LogEntry {
        LogEntry {
            sha: sha.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: String::new(),
            email: String::new(),
            time: 0,
            date: String::new(),
            summary: String::new(),
            refs: Vec::new(),
        }
    }

    #[gpui::test]
    fn another_project_leaves_no_rows_of_the_last(cx: &mut gpui::TestAppContext) {
        let log = cx.new(GitLog::new);
        log.update(cx, |log, cx| {
            log.repo = Some("/a".into());
            log.entries = Arc::new(vec![entry("b", &["a"]), entry("a", &[])]);
            log.graph = Arc::new(graph(&log.entries));
            log.apply_filter(cx);
            log.selected = Some(1);
            log.set_repo(Some("/b".into()), cx);
            // Drawing reads `entries[visible[..]]`, so these must agree.
            assert!(log.visible.iter().all(|&ix| ix < log.entries.len()));
            assert!(log.visible.is_empty() && log.graph.is_empty() && log.selected.is_none());
        });
    }

    #[test]
    fn linear_history_is_one_lane() {
        let rows = graph(&[entry("c", &["b"]), entry("b", &["a"]), entry("a", &[])]);
        assert!(rows.iter().all(|r| r.col == 0 && r.width == 1));
        assert_eq!(rows[1].lines, [(0, 0, 0, 1), (0, 1, 0, 2)]);
        // The root commit has nothing below it.
        assert_eq!(rows[2].lines, [(0, 0, 0, 1)]);
    }

    #[test]
    fn merges_branch_out_and_back() {
        // m merges b into a's line: m → a (first parent), m → b; b → a.
        let rows = graph(&[entry("m", &["a", "b"]), entry("b", &["a"]), entry("a", &[])]);
        assert_eq!(rows[0].lines, [(0, 1, 0, 2), (0, 1, 1, 2)]);
        assert_eq!(rows[1].col, 1);
        // b's lane bends into a's, which keeps going straight down.
        assert_eq!(rows[1].lines, [(0, 0, 0, 1), (1, 0, 1, 1), (1, 1, 0, 2), (0, 1, 0, 2)]);
        assert_eq!((rows[2].col, rows[2].width), (0, 1));
    }

    #[test]
    fn branch_tips_get_their_own_lanes() {
        // Two branches (x, y) off a: both tips start lanes, then meet at a.
        let rows = graph(&[entry("x", &["a"]), entry("y", &["a"]), entry("a", &[])]);
        assert_eq!((rows[0].col, rows[1].col), (0, 1));
        assert_eq!(rows[1].lines, [(0, 0, 0, 1), (1, 1, 0, 2), (0, 1, 0, 2)]);
        assert_eq!(rows[2].lines, [(0, 0, 0, 1)]);
    }
}
