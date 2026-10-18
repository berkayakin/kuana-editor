//! In-file Find / Replace bar shown above the active editor (Ctrl+F / Ctrl+R).

use std::time::Duration;

use gpui::{
    Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Hsla, IntoElement, Render, Subscription,
    Task, Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::{
        Cancel, Editor, EditorEvent, Indent, MoveDown, MoveUp, Newline, NewlineBelow, Outdent,
    },
    search::{SearchMatches, SearchOptions, SearchQuery},
    theme::Theme,
    ui::{icon_button, input_box, text_button, toggle, tooltip_view},
};

pub enum FindBarEvent {
    Dismissed,
}

pub struct FindBar {
    query: Entity<Editor>,
    replacement: Entity<Editor>,
    replace_mode: bool,
    options: SearchOptions,
    target: Option<Entity<Editor>>,
    target_subscription: Option<Subscription>,
    /// Shared with the editor, never copied: there can be hundreds of thousands.
    matches: SearchMatches,
    /// What `matches` were found for: editor, document version, query, options.
    searched: Option<(EntityId, u64, String, SearchOptions)>,
    /// A search running in the background.
    search_task: Option<Task<()>>,
    active: Option<usize>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FindBarEvent> for FindBar {}

impl FindBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| Editor::single_line("Search", cx));
        let replacement = cx.new(|cx| Editor::single_line("Replace", cx));
        let subscription = cx.subscribe(&query, |this, _, _: &EditorEvent, cx| this.update_matches(true, cx));
        Self {
            query,
            replacement,
            replace_mode: false,
            options: SearchOptions::default(),
            target: None,
            target_subscription: None,
            matches: SearchMatches::default(),
            searched: None,
            search_task: None,
            active: None,
            error: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn has_query(&self, cx: &gpui::App) -> bool {
        self.query.read(cx).doc.read(cx).len() > 0
    }

    /// Opens the bar for `target`, prefilled with the editor's selection.
    pub fn show(&mut self, target: Entity<Editor>, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_mode = replace;
        self.set_target(Some(target.clone()), cx);
        let selected = {
            let editor = target.read(cx);
            let s = editor.primary_selection();
            let text = editor.doc.read(cx).slice(s.range());
            (!s.is_empty() && !text.contains('\n')).then_some(text)
        };
        if let Some(text) = selected {
            self.query.update(cx, |q, cx| q.set_text(&text, cx));
        }
        self.query.update(cx, |q, cx| q.select_all_text(cx));
        window.focus(&self.query.focus_handle(cx));
        self.update_matches(false, cx);
    }

    /// Follows the active tab; highlights move with it.
    pub fn set_target(&mut self, target: Option<Entity<Editor>>, cx: &mut Context<Self>) {
        if self.target == target {
            return;
        }
        if let Some(old) = self.target.take() {
            old.update(cx, |e, cx| e.set_search_highlights(SearchMatches::default(), None, cx));
        }
        self.target_subscription = target
            .as_ref()
            .map(|t| cx.subscribe(t, |this, _, _: &EditorEvent, cx| this.update_matches(false, cx)));
        self.target = target;
        self.update_matches(false, cx);
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.target.as_ref() {
            target.update(cx, |e, cx| e.set_search_highlights(SearchMatches::default(), None, cx));
        }
        self.target = None;
        self.target_subscription = None;
        cx.emit(FindBarEvent::Dismissed);
    }

    fn query(&self, cx: &gpui::App) -> Result<Option<SearchQuery>, String> {
        SearchQuery::new(&self.query.read(cx).text(cx), self.options)
    }

    /// Recomputes matches, in the background (the whole text is scanned),
    /// shortly after typing stops. When the query changed, jumps to the
    /// nearest match.
    fn update_matches(&mut self, jump: bool, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        let text = self.query.read(cx).text(cx);
        let key = (target.entity_id(), target.read(cx).doc.read(cx).version(), text, self.options);
        if self.searched.as_ref() == Some(&key) {
            self.search_task = None;
            self.show_matches(jump, cx);
            return;
        }
        let query = match SearchQuery::new(&key.2, self.options) {
            Ok(Some(query)) => query,
            result => {
                self.error = result.err();
                self.search_task = None;
                self.matches = SearchMatches::default();
                self.searched = Some(key);
                self.show_matches(jump, cx);
                return;
            }
        };
        self.error = None;
        let rope = target.read(cx).doc.read(cx).rope().clone();
        // Typing a query waits less than editing the text with the bar open.
        let delay = Duration::from_millis(if jump { 30 } else { 100 });
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let matches = cx
                .background_executor()
                .spawn(async move { SearchMatches::new(query.find_all(&rope.to_string()), &rope) })
                .await;
            this.update(cx, |this, cx| {
                let current = this.target.as_ref().map(|t| (t.entity_id(), t.read(cx).doc.read(cx).version()));
                if current != Some((key.0, key.1)) {
                    return;
                }
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = this.search_task.take() {
                    task.detach();
                }
                this.matches = matches;
                this.searched = Some(key);
                this.show_matches(jump, cx);
            })
            .ok();
        }));
    }

    /// Brings the matches up to date here and now, for an action on them
    /// (next, replace), which mustn't wait for the search or use old ones.
    fn search_now(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        let text = self.query.read(cx).text(cx);
        let key = (target.entity_id(), target.read(cx).doc.read(cx).version(), text, self.options);
        if self.searched.as_ref() == Some(&key) {
            return;
        }
        self.search_task = None;
        self.matches = match SearchQuery::new(&key.2, self.options) {
            Ok(Some(query)) => {
                let rope = target.read(cx).doc.read(cx).rope();
                SearchMatches::new(query.find_all(&rope.to_string()), rope)
            }
            _ => SearchMatches::default(),
        };
        self.searched = Some(key);
        self.show_matches(false, cx);
    }

    /// Highlights the matches in the editor, the one at or after the caret
    /// as the current one (selected when `jump`).
    fn show_matches(&mut self, jump: bool, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        let cursor = target.read(cx).primary_selection().range().start;
        self.active = if self.matches.is_empty() {
            None
        } else {
            Some(self.matches.ranges.partition_point(|m| m.end <= cursor) % self.matches.len())
        };
        let (matches, active) = (self.matches.clone(), self.active);
        target.update(cx, |e, cx| {
            let selected = active.map(|a| matches.ranges[a].clone());
            e.set_search_highlights(matches, active, cx);
            if let Some(range) = selected.filter(|_| jump) {
                e.select_range(range, cx);
            }
        });
        cx.notify();
    }

    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    pub fn select_prev(&mut self, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.search_now(cx);
        let Some(target) = self.target.clone() else { return };
        if self.matches.is_empty() {
            return;
        }
        let sel = target.read(cx).primary_selection().range();
        let n = self.matches.len();
        // Sorted by start: the next or previous one by binary search.
        let ranges = &self.matches.ranges;
        let ix = if forward {
            Some(ranges.partition_point(|m| m.start <= sel.start)).filter(|&ix| ix < n).unwrap_or(0)
        } else {
            ranges.partition_point(|m| m.start < sel.start).checked_sub(1).unwrap_or(n - 1)
        };
        self.active = Some(ix);
        let (matches, range) = (self.matches.clone(), self.matches.ranges[ix].clone());
        target.update(cx, |e, cx| {
            e.set_search_highlights(matches, Some(ix), cx);
            e.select_range(range, cx);
        });
        cx.notify();
    }

    fn replace_one(&mut self, cx: &mut Context<Self>) {
        self.search_now(cx);
        let (Some(target), Some(ix), Ok(Some(query))) = (self.target.clone(), self.active, self.query(cx)) else {
            return;
        };
        let range = self.matches.ranges[ix].clone();
        if target.read(cx).primary_selection().range() != range {
            self.select_next(cx);
            return;
        }
        let replacement = self.replacement.read(cx).text(cx);
        let replacement = query.replacements_in(target.read(cx).doc.read(cx).rope(), std::slice::from_ref(&range), &replacement).remove(0);
        let after = range.start + replacement.len();
        target.update(cx, |e, cx| {
            e.replace_ranges(vec![(range, replacement)], cx);
            e.select_range(after..after, cx);
        });
        // Go to the next match (`step` searches the edited text first).
        self.select_next(cx);
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        self.search_now(cx);
        let (Some(target), Ok(Some(query))) = (self.target.clone(), self.query(cx)) else {
            return;
        };
        let replacement = self.replacement.read(cx).text(cx);
        let ranges = &self.matches.ranges;
        let replacements = query.replacements_in(target.read(cx).doc.read(cx).rope(), ranges, &replacement);
        let edits: Vec<_> = ranges.iter().cloned().zip(replacements).collect();
        target.update(cx, |e, cx| e.replace_ranges(edits, cx));
    }

    fn toggle_option(&mut self, f: impl FnOnce(&mut SearchOptions), cx: &mut Context<Self>) {
        f(&mut self.options);
        self.update_matches(true, cx);
    }

    // Keys typed in the inputs bubble up here as editor actions.

    fn on_enter(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.replacement.focus_handle(cx).is_focused(window) {
            self.replace_one(cx);
        } else {
            self.select_next(cx);
        }
    }

    fn on_shift_enter(&mut self, _: &NewlineBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.select_prev(cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select_next(cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select_prev(cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
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
}

impl Focusable for FindBar {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for FindBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let has_query = self.has_query(cx);
        let (status, status_color): (String, Hsla) = match (&self.error, self.active) {
            (Some(e), _) => (e.clone(), theme.error.into()),
            (None, Some(a)) => (format!("{}/{}", a + 1, self.matches.len()), theme.text_muted.into()),
            (None, None) if has_query => ("0 results".into(), theme.error.into()),
            _ => (String::new(), theme.text_muted.into()),
        };
        let opts = self.options;

        let search_row = div()
            .flex()
            .items_center()
            .gap_1()
            .child(input_box(&self.query, Some(Icon::Search), window, cx))
            .child(
                div()
                    .min_w(px(78.))
                    .px_2()
                    .text_size(px(12.))
                    .text_color(status_color)
                    .truncate()
                    .child(status),
            )
            .child(
                icon_button("find-prev", Icon::ChevronUp, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Previous Occurrence (⇧↩)", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.select_prev(cx))),
            )
            .child(
                icon_button("find-next", Icon::ChevronDown, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Next Occurrence (↩)", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.select_next(cx))),
            )
            .child(div().w(px(1.)).h(px(16.)).mx_1().bg(theme.border))
            .child(
                toggle("opt-case", "Cc", opts.case_sensitive, "Match Case", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.case_sensitive = !o.case_sensitive, cx))),
            )
            .child(
                toggle("opt-word", "W", opts.whole_word, "Words", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.whole_word = !o.whole_word, cx))),
            )
            .child(
                toggle("opt-regex", ".*", opts.regex, "Regex", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.regex = !o.regex, cx))),
            )
            .child(
                icon_button("find-close", Icon::X, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Close (Esc)", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
            );

        div()
            .key_context("FindBar")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_shift_enter))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_1p5()
            .mx_1()
            .mb_1()
            .px_2()
            .py_1p5()
            .rounded(px(10.))
            .bg(theme.panel)
            .border_1()
            .border_color(gpui::Hsla::from(theme.border).alpha(0.75))
            .child(search_row)
            .when(self.replace_mode, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(input_box(&self.replacement, Some(Icon::Replace), window, cx))
                        .child(
                            text_button("replace-one", "Replace", false, cx)
                                .on_click(cx.listener(|this, _, _, cx| this.replace_one(cx))),
                        )
                        .child(
                            text_button("replace-all", "Replace All", true, cx)
                                .on_click(cx.listener(|this, _, _, cx| this.replace_all(cx))),
                        ),
                )
            })
    }
}
