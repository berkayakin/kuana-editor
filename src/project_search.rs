//! "Find in Files" / "Replace in Files" popup (Ctrl+Shift+F / Ctrl+Shift+R).

use std::{
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    IntoElement, Render, ScrollStrategy, SharedString, StyledText, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, Indent, MoveDown, MoveUp, Newline, Outdent, PageDown, PageUp, Submit},
    project::ProjectSnapshot,
    search::{ProjectMatch, SearchOptions, SearchQuery, context_lines, project_files, search_project},
    theme::Theme,
    ui::{HoverFill, icon_el, input_box, kbd, modal_panel, text_button, toggle},
};

pub enum ProjectSearchEvent {
    Open { path: PathBuf, range: Range<usize> },
    ReplaceAll { query: SearchQuery, replacement: String, files: Vec<PathBuf> },
    /// The results as a multibuffer (`query` finds the matches again in a
    /// file whose text changed since it was searched).
    OpenInEditor { title: SharedString, query: SearchQuery, matches: Arc<Vec<ProjectMatch>> },
    Dismissed,
}

pub struct ProjectSearch {
    root: PathBuf,
    /// The project's files, when they're scanned and `root` is in the project.
    project: Option<Arc<ProjectSnapshot>>,
    query: Entity<Editor>,
    replacement: Entity<Editor>,
    replace_mode: bool,
    options: SearchOptions,
    overrides: HashMap<PathBuf, Arc<str>>,
    matches: Arc<Vec<ProjectMatch>>,
    files: usize,
    truncated: bool,
    searching: bool,
    error: Option<String>,
    selected: usize,
    scroll: UniformListScrollHandle,
    task: Option<Task<()>>,
    /// Stops the running search's walk (a newer query took over).
    cancel: Arc<AtomicBool>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ProjectSearchEvent> for ProjectSearch {}

impl ProjectSearch {
    pub fn new(
        root: PathBuf,
        project: Option<Arc<ProjectSnapshot>>,
        initial: Option<String>,
        replace_mode: bool,
        overrides: HashMap<PathBuf, Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| Editor::single_line("Text to find", cx));
        let replacement = cx.new(|cx| Editor::single_line("Replace with", cx));
        if let Some(text) = initial {
            query.update(cx, |q, cx| q.set_text(&text, cx));
        }
        window.focus(&query.focus_handle(cx));
        let subscription = cx.subscribe(&query, |this, _, _: &EditorEvent, cx| this.search(cx));
        let mut this = Self {
            root,
            project,
            query,
            replacement,
            replace_mode,
            options: SearchOptions::default(),
            overrides,
            matches: Arc::new(Vec::new()),
            files: 0,
            truncated: false,
            searching: false,
            error: None,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            task: None,
            cancel: Arc::new(AtomicBool::new(false)),
            _subscriptions: vec![subscription],
        };
        this.search(cx);
        this
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        // Whatever ran before stops at its next file.
        self.cancel.store(true, Ordering::Relaxed);
        let query = match SearchQuery::new(&self.query.read(cx).text(cx), self.options) {
            Ok(Some(q)) => q,
            Ok(None) => {
                self.matches = Arc::new(Vec::new());
                self.files = 0;
                self.error = None;
                self.searching = false;
                self.task = None;
                cx.notify();
                return;
            }
            Err(e) => {
                self.error = Some(e);
                self.matches = Arc::new(Vec::new());
                self.task = None;
                cx.notify();
                return;
            }
        };
        self.error = None;
        self.searching = true;
        let (root, project) = (self.root.clone(), self.project.clone());
        let overrides = self.overrides.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            // Debounce typing.
            cx.background_executor().timer(Duration::from_millis(120)).await;
            // Matches stream in as they're found, in batches.
            let (found_tx, mut found) = futures::channel::mpsc::unbounded();
            let search = cx.background_executor().spawn(async move {
                // The project's files under the folder searched, else a walk of it.
                let files = match project {
                    Some(project) => project.searchable().into_iter().filter(|p| p.starts_with(&root)).collect(),
                    None => project_files(&root),
                };
                search_project(&root, files, &query, &overrides, &cancel, |batch, files| {
                    let _ = found_tx.unbounded_send((batch, files));
                })
            });
            let mut first = true;
            while let Some((mut batch, mut files)) = found.next().await {
                while let Ok((more, count)) = found.try_recv() {
                    batch.extend(more);
                    files = count;
                }
                let start = std::mem::take(&mut first);
                this.update(cx, |this, cx| {
                    // The old results stay until the first new ones are in.
                    if start {
                        this.matches = Arc::new(Vec::new());
                        this.selected = 0;
                        this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                    }
                    Arc::make_mut(&mut this.matches).extend(batch);
                    this.files = files;
                    cx.notify();
                })
                .ok();
            }
            let end = search.await;
            this.update(cx, |this, cx| {
                if first {
                    this.matches = Arc::new(Vec::new());
                    this.files = 0;
                }
                this.truncated = end.truncated;
                this.searching = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = ix.min(self.matches.len() - 1);
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = self.matches.get(self.selected) {
            cx.emit(ProjectSearchEvent::Open {
                path: m.path.clone(),
                range: m.range.clone(),
            });
        }
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let Ok(Some(query)) = SearchQuery::new(&self.query.read(cx).text(cx), self.options) else {
            return;
        };
        let mut files: Vec<PathBuf> = self.matches.iter().map(|m| m.path.clone()).collect();
        files.dedup();
        cx.emit(ProjectSearchEvent::ReplaceAll {
            query,
            replacement: self.replacement.read(cx).text(cx),
            files,
        });
    }

    fn open_in_editor(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text(cx);
        let Ok(Some(query)) = SearchQuery::new(&text, self.options) else {
            return;
        };
        if self.matches.is_empty() {
            return;
        }
        cx.emit(ProjectSearchEvent::OpenInEditor { title: format!("“{text}”").into(), query, matches: self.matches.clone() });
    }

    fn on_submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        self.open_in_editor(cx);
    }

    /// Re-runs the search after a replace, with fresh buffer contents.
    pub fn refresh(&mut self, overrides: HashMap<PathBuf, Arc<str>>, cx: &mut Context<Self>) {
        self.overrides = overrides;
        self.search(cx);
    }

    fn toggle_option(&mut self, f: impl FnOnce(&mut SearchOptions), cx: &mut Context<Self>) {
        f(&mut self.options);
        self.search(cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(10), cx);
    }

    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 10, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.open_selected(cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ProjectSearchEvent::Dismissed);
    }

    fn on_tab(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn on_shift_tab(&mut self, _: &Outdent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn switch_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replace_mode {
            return;
        }
        let to = if self.query.focus_handle(cx).is_focused(window) {
            &self.replacement
        } else {
            &self.query
        };
        to.update(cx, |e, cx| e.select_all_text(cx));
        window.focus(&to.focus_handle(cx));
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let Some(m) = self.matches.get(self.selected) else {
            return div().flex_1().into_any_element();
        };
        const CONTEXT: usize = 6;
        // Only the lines around the match, each cut short: the file may be
        // huge, or one minified line.
        let lines = context_lines(&m.contents, m.line_start, m.line, CONTEXT, 1000);
        let highlight = m.range.start - m.line_start..m.range.end - m.line_start;
        let mono = theme.mono_font.clone();
        div()
            .id("preview")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .py_2()
            .overflow_hidden()
            .font_family(mono)
            .text_size(px(12.5))
            .children(lines.into_iter().map(|(row, line)| {
                let text: SharedString = line.replace('\t', "    ").into();
                let is_match = row == m.line;
                let styled = if is_match && highlight.end <= line.len() && !line.contains('\t') {
                    StyledText::new(text).with_highlights([(
                        highlight.clone(),
                        HighlightStyle {
                            background_color: Some(theme.search_active.into()),
                            color: Some(theme.text.into()),
                            ..Default::default()
                        },
                    )])
                } else {
                    StyledText::new(text)
                };
                div()
                    .flex()
                    .h(px(20.))
                    .when(is_match, |el| el.bg(theme.current_line))
                    .child(
                        div()
                            .w(px(52.))
                            .pr_3()
                            .flex_shrink_0()
                            .flex()
                            .justify_end()
                            .text_color(if is_match { theme.line_number_active } else { theme.line_number })
                            .child((row + 1).to_string()),
                    )
                    .child(div().text_color(theme.text_muted).whitespace_nowrap().child(styled))
            }))
            .into_any_element()
    }
}

impl Focusable for ProjectSearch {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for ProjectSearch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let opts = self.options;
        let summary = if let Some(e) = &self.error {
            e.clone()
        } else if self.searching {
            "Searching…".into()
        } else if self.matches.is_empty() {
            if self.query.read(cx).doc.read(cx).len() > 0 { "No matches".into() } else { String::new() }
        } else {
            format!(
                "{}{} matches in {} file{}",
                self.matches.len(),
                if self.truncated { "+" } else { "" },
                self.files,
                if self.files == 1 { "" } else { "s" }
            )
        };
        let title = if self.replace_mode { "Replace in Files" } else { "Find in Files" };
        let scope = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .pt_3()
            .pb_2()
            .child(icon_el(Icon::TextSearch, px(15.), theme.text_muted.into()))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .rounded(px(5.))
                    .bg(theme.hover)
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(icon_el(Icon::Folder, px(12.), theme.text_muted.into()))
                    .child(scope),
            )
            .child(div().flex_1().text_size(px(12.)).text_color(if self.error.is_some() { theme.error } else { theme.text_dim }).child(summary))
            .child(toggle("ps-case", "Cc", opts.case_sensitive, "Match Case", cx).on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.case_sensitive = !o.case_sensitive, cx))))
            .child(toggle("ps-word", "W", opts.whole_word, "Words", cx).on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.whole_word = !o.whole_word, cx))))
            .child(toggle("ps-regex", ".*", opts.regex, "Regex", cx).on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.regex = !o.regex, cx))));

        let inputs = div()
            .flex()
            .flex_col()
            .gap_1p5()
            .px_3()
            .pb_3()
            .child(input_box(&self.query, Some(Icon::Search), window, cx))
            .when(self.replace_mode, |el| {
                el.child(
                    div()
                        .flex()
                        .gap_1p5()
                        .child(input_box(&self.replacement, Some(Icon::Replace), window, cx))
                        .child(text_button("ps-replace-all", "Replace All", true, cx).on_click(cx.listener(|this, _, _, cx| this.replace_all(cx)))),
                )
            });

        let matches = self.matches.clone();
        let selected = self.selected;
        let list = uniform_list("project-results", matches.len(), {
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                let theme = Theme::get(cx).clone();
                range
                    .map(|ix| {
                        let m = &matches[ix];
                        // Trim leading whitespace and keep the match visible.
                        let raw = m.line_text.replace('\t', " ");
                        let indent = raw.len() - raw.trim_start().len();
                        let mut start = indent.min(m.line_range.start);
                        if m.line_range.start > start + 60 {
                            start = raw.floor_char_boundary(m.line_range.start - 30);
                        }
                        let shown = &raw[start..];
                        let shown_text: SharedString = if shown.len() > 200 {
                            shown[..shown.floor_char_boundary(200)].to_string().into()
                        } else {
                            shown.to_string().into()
                        };
                        let hl = (m.line_range.start - start).min(shown_text.len())..(m.line_range.end - start).min(shown_text.len());
                        let file_name = m.relative.rsplit(['/', '\\']).next().unwrap_or(&m.relative).to_string();
                        let icon = crate::ui::file_icon(&m.path, px(13.), &theme);
                        let is_selected = ix == selected;
                        div()
                            .id(ix)
                            .relative()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(26.))
                            .px_3()
                            .rounded(px(7.))
                            .cursor_pointer()
                            .when(is_selected, |el| el.bg(theme.accent_soft))
                            .row_hover(theme.hover)
                            .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| {
                                this.select(ix, cx);
                                if e.click_count() >= 2 {
                                    this.open_selected(cx);
                                }
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(theme.mono_font.clone())
                                    .text_size(px(12.5))
                                    .text_color(theme.text_muted)
                                    .child(StyledText::new(shown_text).with_highlights([(
                                        hl,
                                        HighlightStyle {
                                            color: Some(theme.text.into()),
                                            background_color: Some(theme.search_match.into()),
                                            font_weight: Some(FontWeight::SEMIBOLD),
                                            ..Default::default()
                                        },
                                    )])),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .flex_shrink_0()
                                    .text_size(px(12.))
                                    .text_color(theme.text_dim)
                                    .child(icon)
                                    .child(format!("{file_name} {}", m.line + 1)),
                            )
                    })
                    .collect()
            })
        })
        .track_scroll(self.scroll.clone())
        .h(px(260.))
        .w_full();
        let list = div().relative().child(list).child(crate::ui::list_scrollbar("project-results-scrollbar", &self.scroll, cx));

        let path_line = self
            .matches
            .get(self.selected)
            .map(|m| m.relative.clone())
            .unwrap_or_default();

        modal_panel(cx)
            .key_context("ProjectSearch")
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .w(px(860.))
            .max_w(window.viewport_size().width - px(48.))
            .h(px(640.))
            .max_h(window.viewport_size().height - px(120.))
            .child(header)
            .child(inputs)
            .child(crate::ui::island(&theme).mx(px(crate::ui::GAP)).child(list))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_3p5()
                    .text_size(px(12.))
                    .text_color(theme.text_dim)
                    .child(div().flex_1().truncate().child(path_line))
                    .child(kbd("↩", cx))
                    .child("open")
                    .when(!self.matches.is_empty(), |el| {
                        let key = crate::ui::binding_label_in(&Submit, "ProjectSearch Editor", window);
                        el.when_some(key, |el, key| el.child(div().w(px(6.))).child(kbd(key, cx)))
                            .child(
                                div()
                                    .id("ps-open-in-editor")
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme.text))
                                    .child("open all in editor")
                                    .on_click(cx.listener(|this, _, _, cx| this.open_in_editor(cx))),
                            )
                    }),
            )
            .child(
                crate::ui::island(&theme)
                    .flex_1()
                    .min_h_0()
                    .mx(px(crate::ui::GAP))
                    .mb(px(crate::ui::GAP))
                    .child(self.render_preview(cx)),
            )
    }
}
