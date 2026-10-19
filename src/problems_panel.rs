//! The Problems panel: the active file's diagnostics, then what the last Run
//! printed. The workspace tells it the active editor, the project and the
//! Run's problems, and opens what's clicked (`ProblemsEvent`); it draws the
//! island around it, sized and resized there.

use std::path::{Path, PathBuf};

use gpui::{Context, Div, Entity, EventEmitter, FontWeight, Hsla, Render, SharedString, Stateful, Subscription, Window, div, prelude::*, px, uniform_list};

use crate::{
    assets::Icon,
    document::Document,
    editor::Editor,
    lint::Severity,
    run::OutputProblem,
    theme::Theme,
    ui::{HoverFill, icon_el},
};

pub enum ProblemsEvent {
    /// A Run problem was clicked: open its file there (1-based).
    Open { path: PathBuf, line: u32, column: Option<u32> },
    Close,
}

impl EventEmitter<ProblemsEvent> for ProblemsPanel {}

#[derive(Default)]
pub struct ProblemsPanel {
    editor: Option<Entity<Editor>>,
    /// Redraws when the editor's document (its text, its diagnostics)
    /// changes; not the editor, which notifies at every caret blink.
    _editor_observer: Option<Subscription>,
    /// The last Run's name and problems.
    run: Option<(String, Vec<OutputProblem>)>,
    /// Counts `set_run` calls, for `run_counts`.
    run_generation: u64,
    /// `run_counts` for (document state, run generation): the status bar
    /// asks every frame.
    counts: std::cell::Cell<Option<((Option<(u64, u64, u64)>, u64), [usize; 4])>>,
    root: Option<PathBuf>,
    scroll: gpui::UniformListScrollHandle,
}

impl ProblemsPanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// The file whose diagnostics show (the active one).
    pub fn set_editor(&mut self, editor: Option<Entity<Editor>>, cx: &mut Context<Self>) {
        if self.editor == editor {
            return;
        }
        self._editor_observer = editor.as_ref().map(|e| {
            let doc = e.read(cx).doc.clone();
            cx.observe(&doc, |_, _, cx| cx.notify())
        });
        self.editor = editor;
        cx.notify();
    }

    /// What the last Run printed (None: nothing, or a run without problems).
    pub fn set_run(&mut self, run: Option<(String, Vec<OutputProblem>)>, cx: &mut Context<Self>) {
        self.run = run;
        self.run_generation += 1;
        cx.notify();
    }

    /// The project, for showing the Run's paths relative to it.
    pub fn set_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.root = root;
        cx.notify();
    }

    /// The last Run's problems, without the ones `doc` (the active file)
    /// shows already (its language server reports the same compiler errors).
    pub fn run_shown(&self, doc: Option<&Document>) -> Vec<&OutputProblem> {
        let Some((_, problems)) = &self.run else { return Vec::new() };
        let known = |p: &OutputProblem| {
            doc.is_some_and(|doc| {
                doc.path.as_deref() == Some(p.path.as_path()) && (1..=doc.line_count()).contains(&(p.line as usize)) && {
                    // Its line's diagnostics, by binary search (they're sorted).
                    let row = p.line as usize - 1;
                    let (start, end) = (doc.line_start(row), doc.line_end(row));
                    let first = doc.diagnostics.partition_point(|d| d.range.start < start);
                    doc.diagnostics[first..].iter().take_while(|d| d.range.start <= end).any(|d| d.severity == p.severity)
                }
            })
        };
        problems.iter().filter(|p| !known(p)).collect()
    }

    /// How many of `run_shown` there are of each severity (by `Severity as
    /// usize`), kept until the document or the Run's problems change.
    pub fn run_counts(&self, doc: Option<&Document>) -> [usize; 4] {
        let key = (doc.map(|d| (d.id(), d.version(), d.diagnostics_generation())), self.run_generation);
        if let Some((k, counts)) = self.counts.get()
            && k == key
        {
            return counts;
        }
        let mut counts = [0; 4];
        for p in self.run_shown(doc) {
            counts[p.severity as usize] += 1;
        }
        self.counts.set(Some((key, counts)));
        counts
    }

    fn display_rel(&self, path: &Path) -> String {
        match self.root.as_deref().and_then(|r| path.strip_prefix(r).ok()) {
            Some(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => path.display().to_string(),
        }
    }
}

impl Render for ProblemsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let doc = self.editor.as_ref().map(|e| e.read(cx).doc.read(cx));
        let run_count = self.run_counts(doc).iter().sum::<usize>();
        // Information and hints aren't problems.
        let diag_count = doc.map_or(0, |d| d.severity_count(Severity::Error) + d.severity_count(Severity::Warning));

        let header = crate::ui::panel_header(&theme)
            .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("PROBLEMS"))
            .child(
                div()
                    .px_1p5()
                    .rounded_full()
                    .bg(theme.active)
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child((diag_count + run_count).to_string()),
            )
            .child(div().flex_1())
            .child(crate::ui::panel_close_button("close-problems", cx).on_click(cx.listener(|_, _, _, cx| cx.emit(ProblemsEvent::Close))));

        // Rows, top to bottom: the file's title when the last Run reported
        // problems too, the file's problems, then the Run's title and its
        // problems. Only the rows on screen are built.
        let file_title = run_count > 0 && diag_count > 0;
        let count = file_title as usize + diag_count + if run_count > 0 { 1 + run_count } else { 0 };
        let list = if count == 0 {
            div()
                .flex()
                .items_center()
                .gap_2()
                .p_3()
                .text_color(theme.text_dim)
                .child(icon_el(Icon::CircleCheck, px(15.), theme.success.into()))
                .child("No problems detected")
                .into_any_element()
        } else {
            uniform_list(
                "problems-list",
                count,
                cx.processor(move |panel, range: std::ops::Range<usize>, _window, cx| {
                    let theme = Theme::get(cx).clone();
                    let editor = panel.editor.clone();
                    let doc = editor.as_ref().map(|e| e.read(cx).doc.read(cx));
                    let run_problems: Vec<OutputProblem> = panel.run_shown(doc).into_iter().cloned().collect();
                    let run_name = panel.run.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
                    let mut rows = Vec::with_capacity(range.len());
                    for ix in range {
                        let mut ix = ix;
                        if file_title {
                            if ix == 0 {
                                let name = doc.and_then(|d| d.path.as_deref().map(folder_name)).unwrap_or_default();
                                rows.push(problem_section("file-problems".into(), Icon::File, name, &theme).into_any_element());
                                continue;
                            }
                            ix -= 1;
                        }
                        let diagnostic = doc.and_then(|doc| {
                            let d = doc.diagnostics.iter().filter(|d| d.severity.is_problem()).nth(ix)?;
                            let row = doc.row_of(d.range.start);
                            let col = doc.slice(doc.line_start(row)..d.range.start).chars().count();
                            Some((d.clone(), row + 1, col + 1))
                        });
                        if let Some((d, line, col)) = diagnostic {
                            let (icon, color) = severity_icon(d.severity, &theme);
                            let target = d.range.start;
                            let editor = editor.clone();
                            // One row per problem: a multi-line message (Pyright's
                            // details) reads as one line, like the inline hint.
                            let message = d.message.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
                            rows.push(
                                problem_row(SharedString::from(format!("problem-{ix}")), icon, color, message, &theme)
                                    .on_click(move |_, window, cx| {
                                        if let Some(editor) = editor.as_ref() {
                                            editor.update(cx, |e, cx| e.go_to(target, window, cx));
                                        }
                                    })
                                    .child(div().text_color(theme.text_dim).flex_shrink_0().child(SharedString::from(d.rule.to_string())))
                                    .child(div().flex_1())
                                    .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(format!("Ln {line}, Col {col}")))
                                    .into_any_element(),
                            );
                            continue;
                        }
                        ix -= diag_count;
                        if ix == 0 {
                            rows.push(problem_section("run-problems".into(), Icon::Play, run_name.clone(), &theme).into_any_element());
                            continue;
                        }
                        let Some(p) = run_problems.get(ix - 1) else {
                            rows.push(div().into_any_element());
                            continue;
                        };
                        let (icon, color) = severity_icon(p.severity, &theme);
                        let place = match p.column {
                            Some(column) => format!("{}:{}:{column}", panel.display_rel(&p.path), p.line),
                            None => format!("{}:{}", panel.display_rel(&p.path), p.line),
                        };
                        let (path, line, column) = (p.path.clone(), p.line, p.column);
                        rows.push(
                            problem_row(SharedString::from(format!("run-problem-{ix}")), icon, color, p.message.clone(), &theme)
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(ProblemsEvent::Open { path: path.clone(), line, column })))
                                .child(div().flex_1())
                                .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(place))
                                .into_any_element(),
                        );
                    }
                    rows
                }),
            )
            .track_scroll(self.scroll.clone())
            .size_full()
            .py_1()
            .into_any_element()
        };
        let list = div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(list)
            .child(crate::ui::list_scrollbar("problems-scrollbar", &self.scroll, cx));
        div().flex().flex_col().size_full().child(header).child(list)
    }
}

fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The icon and color for a severity.
pub fn severity_icon(severity: Severity, theme: &Theme) -> (Icon, Hsla) {
    match severity {
        Severity::Error => (Icon::CircleX, theme.error.into()),
        Severity::Warning => (Icon::TriangleAlert, theme.warning.into()),
        Severity::Info => (Icon::CircleAlert, theme.info.into()),
        Severity::Hint => (Icon::CircleAlert, theme.text_dim.into()),
    }
}

/// A heading in the Problems panel (the file, the last Run).
fn problem_section(id: SharedString, icon: Icon, title: String, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1p5()
        .h(px(26.))
        .px_3()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_dim)
        .child(icon_el(icon, px(12.), theme.text_dim.into()))
        .child(title)
}

/// A problem in the Problems panel: severity icon and message; callers add
/// where it is.
fn problem_row(id: SharedString, icon: Icon, color: Hsla, message: String, theme: &Theme) -> crate::ui::Button {
    div()
        .id(id)
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .h(px(26.))
        .mx_1p5()
        .px_2()
        .rounded(px(6.))
        .cursor_pointer()
        .hover_bg(theme.hover)
        .child(icon_el(icon, px(14.), color))
        .child(div().min_w_0().text_color(theme.text).truncate().child(message))
}
