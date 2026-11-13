//! A multibuffer: places from many files (usages, TODO items, search
//! results, test failures) as excerpts in one tab, each editable in place.
//!
//! Every excerpt is an ordinary editor on its file's own document showing a
//! few lines of it (`Editor::excerpt_of`), so what a file's tab has (syntax,
//! problems, Git marks, completion, rename, undo, Save) works in an excerpt
//! as it is, and an edit is at once in the file's tab too. The view is a
//! list (only excerpts on screen are laid out): a header above each file's
//! first excerpt, a "⋯ N lines" strip wherever lines are left out (a click
//! shows more there; excerpts that meet become one), ↑/↓ going from one
//! excerpt into the next, and Open in File (F4) jumping to the caret's place
//! in the file's tab.
//!
//! Zed's multibuffer is one editor over a combined buffer, with anchors and
//! two layers of coordinate maps; here the files stay apart and only their
//! lines are picked, which keeps every editor feature exact at the cost of
//! multiple carets spanning excerpts (each excerpt has its own).

use std::{
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ListAlignment, ListState, Render,
    SharedString, Subscription, Window, actions, div, list, prelude::*, px,
};

use crate::{
    assets::Icon,
    document::Document,
    editor::{Editor, ExcerptEvent},
    search::SearchMatches,
    theme::Theme,
    ui::{HoverFill, file_icon, icon_el},
};

actions!(
    multibuffer,
    [
        /// The file's tab, at the caret's place in the excerpt.
        OpenInFile,
    ]
);

/// Lines shown around each place.
pub const CONTEXT: usize = 2;
/// Lines a click on a "⋯" strip adds.
const EXPAND: usize = 5;

/// One file's places for a multibuffer: lines (0-based) and the byte ranges
/// to mark in them (a usage, a match).
#[derive(Clone, Debug, Default)]
pub struct FilePlaces {
    pub rows: Vec<usize>,
    pub marks: Vec<Range<usize>>,
}

/// What a multibuffer marks at its places: the word there (a usage, a
/// match), or nothing (problems, which their underlines show).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Marks {
    Words,
    None,
}

/// The excerpts for places on `rows` of a file with `line_count` lines:
/// `context` lines around each, merged when at most one line would be left
/// out between them, and taking in a lone line at the file's start or end
/// (a "⋯" strip would take as much room as the line).
pub fn excerpt_rows(rows: impl IntoIterator<Item = usize>, context: usize, line_count: usize) -> Vec<RangeInclusive<usize>> {
    let last = line_count.saturating_sub(1);
    let mut rows: Vec<usize> = rows.into_iter().map(|r| r.min(last)).collect();
    rows.sort_unstable();
    rows.dedup();
    let mut out: Vec<RangeInclusive<usize>> = Vec::new();
    for row in rows {
        let (start, end) = edges(row.saturating_sub(context), (row + context).min(last), last);
        match out.last_mut() {
            Some(prev) if start <= prev.end() + 2 => *prev = *prev.start()..=end.max(*prev.end()),
            _ => out.push(start..=end),
        }
    }
    out
}

/// `runs` (sorted, apart) with `extra` added: runs it overlaps or comes
/// within a line of become one, as in `excerpt_rows`.
fn merge_runs(runs: Vec<RangeInclusive<usize>>, extra: RangeInclusive<usize>) -> Vec<RangeInclusive<usize>> {
    let mut all = runs;
    let at = all.partition_point(|r| r.start() < extra.start());
    all.insert(at, extra);
    let mut out: Vec<RangeInclusive<usize>> = Vec::with_capacity(all.len());
    for run in all {
        match out.last_mut() {
            Some(prev) if *run.start() <= prev.end() + 2 => *prev = *prev.start()..=(*run.end()).max(*prev.end()),
            _ => out.push(run),
        }
    }
    out
}

/// An excerpt's rows with a lone line left at the file's start or end taken in.
fn edges(start: usize, end: usize, last: usize) -> (usize, usize) {
    (if start == 1 { 0 } else { start }, if end + 1 == last { last } else { end })
}

pub enum MultiBufferEvent {
    /// New excerpt editors: the workspace hears them as it does a tab's.
    Added(Vec<Entity<Editor>>),
    /// Excerpts merged into others: the workspace lets go of them.
    Removed(Vec<Entity<Editor>>),
    /// Open in File: the file's tab, at this offset.
    Open { path: PathBuf, offset: usize },
    /// Something its tab shows changed (a file's unsaved edits).
    Changed,
}

struct Item {
    path: PathBuf,
    editor: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

pub struct MultiBufferView {
    title: SharedString,
    /// The project folder, for the headers' folders.
    root: Option<PathBuf>,
    items: Vec<Item>,
    list: ListState,
    /// The excerpt the keyboard was in last (where the tab gives it back).
    active: usize,
    /// Files still being read.
    loading: usize,
    focus_handle: FocusHandle,
}

impl EventEmitter<MultiBufferEvent> for MultiBufferView {}

impl MultiBufferView {
    /// An empty view for `files` files about to be added.
    pub fn new(title: impl Into<SharedString>, root: Option<PathBuf>, files: usize, cx: &mut Context<Self>) -> Self {
        Self {
            title: title.into(),
            root,
            items: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(600.)),
            active: 0,
            loading: files,
            focus_handle: cx.focus_handle(),
        }
    }

    /// A second view of the same excerpts (a split): editors of the same
    /// documents and lines, with carets of their own.
    pub fn split_of(other: &Entity<Self>, cx: &mut Context<Self>) -> Self {
        let (title, root) = (other.read(cx).title.clone(), other.read(cx).root.clone());
        let mut this = Self::new(title, root, 0, cx);
        let shown: Vec<(PathBuf, Entity<Document>, RangeInclusive<usize>)> = other
            .read(cx)
            .items
            .iter()
            .map(|i| {
                let e = i.editor.read(cx);
                (i.path.clone(), e.doc.clone(), e.excerpt_rows(cx).unwrap_or(0..=0))
            })
            .collect();
        for (path, doc, rows) in shown {
            let editor = cx.new(|cx| Editor::excerpt_of(doc, rows, cx));
            this.items.push(Item { path, editor, _subscriptions: Vec::new() });
        }
        this.list.splice(0..0, this.items.len());
        this
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// The excerpt editors, in order.
    pub fn editors(&self) -> impl Iterator<Item = &Entity<Editor>> {
        self.items.iter().map(|i| &i.editor)
    }

    /// The excerpt the keyboard is in (or was last).
    pub fn active_editor(&self) -> Option<&Entity<Editor>> {
        self.items.get(self.active).map(|i| &i.editor)
    }

    /// Whether a file shown has unsaved edits.
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.items.iter().any(|i| i.editor.read(cx).doc.read(cx).is_dirty())
    }

    /// A file that couldn't be read: one fewer to wait for.
    pub fn file_failed(&mut self, cx: &mut Context<Self>) {
        self.loading = self.loading.saturating_sub(1);
        cx.notify();
    }

    /// Adds a file's excerpts (in path order among the files there).
    pub fn add_file(&mut self, path: PathBuf, doc: Entity<Document>, places: FilePlaces, cx: &mut Context<Self>) {
        self.loading = self.loading.saturating_sub(1);
        let line_count = doc.read(cx).line_count();
        let ranges = excerpt_rows(places.rows.iter().copied(), CONTEXT, line_count);
        let at = self.items.partition_point(|i| i.path <= path);
        let mut marks = places.marks;
        marks.sort_by_key(|r| r.start);
        let mut editors = Vec::with_capacity(ranges.len());
        for (n, rows) in ranges.into_iter().enumerate() {
            let editor = cx.new(|cx| {
                let mut editor = Editor::excerpt_of(doc.clone(), rows.clone(), cx);
                let (start, end) = {
                    let d = editor.doc.read(cx);
                    (d.line_start(*rows.start()), d.line_end(*rows.end()))
                };
                let mine: Vec<Range<usize>> = marks.iter().filter(|r| r.start >= start && r.end <= end).cloned().collect();
                // The caret at the first place: its mark, else its line.
                let place = mine.first().map(|r| r.start).or_else(|| {
                    let row = places.rows.iter().copied().filter(|r| rows.contains(r)).min()?;
                    Some(editor.doc.read(cx).line_start(row))
                });
                if let Some(at) = place {
                    editor.set_caret(at, cx);
                }
                if !mine.is_empty() {
                    let matches = SearchMatches::new(mine, editor.doc.read(cx).rope());
                    editor.set_search_highlights(matches, None, cx);
                }
                editor
            });
            self.items.insert(at + n, Item { path: path.clone(), editor: editor.clone(), _subscriptions: Vec::new() });
            editors.push(editor);
        }
        if at <= self.active && !self.items.is_empty() && self.items.len() > editors.len() {
            self.active += editors.len();
        }
        self.list.splice(at..at, editors.len());
        cx.emit(MultiBufferEvent::Added(editors));
        cx.notify();
    }

    /// The files it shows, in order.
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.items.iter().map(|i| i.path.clone()).collect();
        paths.dedup();
        paths
    }

    /// Whether the keyboard is in one of its excerpts.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx) || self.items.iter().any(|i| i.editor.focus_handle(cx).is_focused(window))
    }

    /// Shows a file's `places` as they are now (problems come and go): an
    /// excerpt overlapping one of the new runs of lines stays, with its
    /// editor (caret, scroll, undo) and the new run's lines; runs no excerpt
    /// overlaps get new ones; excerpts left over go. `hold`: the excerpt the
    /// keyboard is in stays as it is or grows, whatever the places say, so
    /// what's being typed in doesn't move or vanish. A file not shown yet
    /// goes in at its place in path order; one with no places and no held
    /// excerpt goes away.
    pub fn set_file_places(&mut self, path: PathBuf, doc: Entity<Document>, places: FilePlaces, hold: bool, cx: &mut Context<Self>) {
        // One of the files it was made for (later updates find none waiting).
        self.loading = self.loading.saturating_sub(1);
        let line_count = doc.read(cx).line_count();
        let from = self.items.partition_point(|i| i.path < path);
        let to = self.items.partition_point(|i| i.path <= path);
        let held = (hold && (from..to).contains(&self.active)).then(|| self.rows(self.active, cx).0);
        let mut runs = excerpt_rows(places.rows.iter().copied(), CONTEXT, line_count);
        if let Some(held) = &held {
            runs = merge_runs(runs, held.clone());
        }
        let mut old: Vec<Option<Item>> = self.items.drain(from..to).map(Some).collect();
        let old_rows: Vec<RangeInclusive<usize>> =
            old.iter().map(|i| i.as_ref().and_then(|i| i.editor.read(cx).excerpt_rows(cx)).unwrap_or(0..=0)).collect();
        let was_active = (from..to).contains(&self.active).then(|| self.active - from);
        let mut active_at = None;
        let (mut items, mut added) = (Vec::with_capacity(runs.len()), Vec::new());
        for run in runs {
            let reuse = (0..old.len()).find(|&n| old[n].is_some() && old_rows[n].start() <= run.end() && run.start() <= old_rows[n].end());
            let item = match reuse.and_then(|n| old[n].take().map(|item| (n, item))) {
                Some((n, item)) => {
                    if old_rows[n] != run {
                        item.editor.update(cx, |e, cx| e.set_excerpt_rows(run.clone(), cx));
                    }
                    if was_active == Some(n) {
                        active_at = Some(items.len());
                    }
                    item
                }
                None => {
                    let editor = cx.new(|cx| {
                        let mut editor = Editor::excerpt_of(doc.clone(), run.clone(), cx);
                        if let Some(row) = places.rows.iter().copied().filter(|r| run.contains(r)).min() {
                            let at = editor.doc.read(cx).line_start(row);
                            editor.set_caret(at, cx);
                        }
                        editor
                    });
                    added.push(editor.clone());
                    Item { path: path.clone(), editor, _subscriptions: Vec::new() }
                }
            };
            items.push(item);
        }
        let removed: Vec<Entity<Editor>> = old.into_iter().flatten().map(|i| i.editor).collect();
        let count = items.len();
        self.items.splice(from..from, items);
        self.list.splice(from..to, count);
        // The keyboard's excerpt: where its editor went, else the file's
        // first (or the next file's) excerpt.
        if was_active.is_some() {
            self.active = if count > 0 { from + active_at.unwrap_or(0) } else { from.min(self.items.len().saturating_sub(1)) };
        } else if self.active >= to {
            self.active = self.active - (to - from) + count;
        }
        if !added.is_empty() {
            cx.emit(MultiBufferEvent::Added(added));
        }
        if !removed.is_empty() {
            cx.emit(MultiBufferEvent::Removed(removed));
        }
        cx.notify();
    }

    fn watch(&self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) -> Vec<Subscription> {
        vec![
            cx.subscribe_in(editor, window, |this, editor, event: &ExcerptEvent, window, cx| {
                let Some(ix) = this.index_of(editor) else { return };
                match event {
                    ExcerptEvent::Leave { up, x } => this.step(ix, *up, *x, window, cx),
                    ExcerptEvent::CaretMoved => {
                        this.active = ix;
                        this.reveal(ix, cx);
                    }
                    ExcerptEvent::Resized => {
                        this.list.splice(ix..ix + 1, 1);
                        cx.notify();
                    }
                }
            }),
            // Edits: the tab's dirty dot, and a "⋯" strip's line count.
            cx.observe(&editor.read(cx).doc.clone(), |_, _, cx| {
                cx.emit(MultiBufferEvent::Changed);
                cx.notify();
            }),
            cx.on_focus(&editor.focus_handle(cx), window, {
                let editor = editor.downgrade();
                move |this, _, _| {
                    if let Some(ix) = editor.upgrade().and_then(|e| this.index_of(&e)) {
                        this.active = ix;
                    }
                }
            }),
        ]
    }

    fn index_of(&self, editor: &Entity<Editor>) -> Option<usize> {
        self.items.iter().position(|i| &i.editor == editor)
    }

    /// ↑/↓ past an excerpt's edge: into the one above or below.
    fn step(&mut self, ix: usize, up: bool, x: gpui::Pixels, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = (if up { ix.checked_sub(1) } else { Some(ix + 1) }).filter(|&n| n < self.items.len()) else { return };
        self.active = next;
        let editor = self.items[next].editor.clone();
        window.focus(&editor.focus_handle(cx));
        editor.update(cx, |e, cx| e.enter_excerpt(!up, x, window, cx));
        self.reveal(next, cx);
    }

    /// Scrolls the list so excerpt `ix`'s caret is in view.
    fn reveal(&mut self, ix: usize, cx: &mut Context<Self>) {
        let editor = self.items[ix].editor.read(cx);
        let (Some(_), Some(bounds)) = (self.list.bounds_for_item(ix), editor.shown_bounds()) else {
            self.list.scroll_to_reveal_item(ix);
            cx.notify();
            return;
        };
        let (top, bottom) = editor.caret_span(cx);
        let margin = editor.excerpt_line_height(cx) * 2.;
        let viewport = self.list.viewport_bounds();
        let (top, bottom) = (bounds.top() + top - margin, bounds.top() + bottom + margin);
        if top < viewport.top() {
            self.list.scroll_by(top - viewport.top());
        } else if bottom > viewport.bottom() {
            self.list.scroll_by(bottom - viewport.bottom());
        } else {
            return;
        }
        cx.notify();
    }

    /// Rows of excerpt `ix` and the file's line count.
    fn rows(&self, ix: usize, cx: &App) -> (RangeInclusive<usize>, usize) {
        let e = self.items[ix].editor.read(cx);
        (e.excerpt_rows(cx).unwrap_or(0..=0), e.doc.read(cx).line_count())
    }

    fn same_file(&self, a: usize, b: usize) -> bool {
        matches!((self.items.get(a), self.items.get(b)), (Some(x), Some(y)) if x.path == y.path)
    }

    /// Shows `EXPAND` more lines above excerpt `ix` (`up`) or below it,
    /// merging it with the next excerpt of the file when they meet.
    fn expand(&mut self, ix: usize, up: bool, cx: &mut Context<Self>) {
        let (rows, lines) = self.rows(ix, cx);
        let (mut start, mut end) = (*rows.start(), *rows.end());
        if up {
            start = start.saturating_sub(EXPAND);
        } else {
            end = (end + EXPAND).min(lines.saturating_sub(1));
        }
        (start, end) = edges(start, end, lines.saturating_sub(1));
        let neighbor = if up { ix.checked_sub(1) } else { Some(ix + 1) }.filter(|&n| self.same_file(ix, n));
        if let Some(n) = neighbor {
            let (other, _) = self.rows(n, cx);
            // As in `excerpt_rows`: one line between them isn't worth a strip.
            let meets = if up { start <= other.end() + 2 } else { end + 2 >= *other.start() };
            if meets {
                let (keep, gone) = if up { (n, ix) } else { (ix, n) };
                let merged = (*other.start()).min(start)..=(*other.end()).max(end);
                self.merge(keep, gone, merged, cx);
                return;
            }
        }
        self.items[ix].editor.update(cx, |e, cx| e.set_excerpt_rows(start..=end, cx));
        self.list.splice(ix..ix + 1, 1);
        cx.notify();
    }

    /// Excerpt `gone` joins `keep` (the one above it), which shows `rows`.
    fn merge(&mut self, keep: usize, gone: usize, rows: RangeInclusive<usize>, cx: &mut Context<Self>) {
        self.items[keep].editor.update(cx, |e, cx| e.set_excerpt_rows(rows, cx));
        let item = self.items.remove(gone);
        self.list.splice(gone..gone + 1, 0);
        self.list.splice(keep..keep + 1, 1);
        if self.active >= gone && self.active > 0 {
            self.active -= 1;
        }
        cx.emit(MultiBufferEvent::Removed(vec![item.editor]));
        cx.notify();
    }

    fn open_in_file(&mut self, _: &OpenInFile, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(self.active) else { return };
        let offset = item.editor.read(cx).primary_selection().head;
        cx.emit(MultiBufferEvent::Open { path: item.path.clone(), offset });
    }

    /// The header above a file's first excerpt.
    fn render_header(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = Theme::get(cx);
        let path = self.items[ix].path.clone();
        let doc = self.items[ix].editor.read(cx).doc.read(cx);
        let dirty = doc.is_dirty();
        let name = doc.file_name();
        let folder = folder_of(&path, self.root.as_deref());
        let editor = self.items[ix].editor.clone();
        div()
            .id(("excerpt-header", ix))
            .flex()
            .items_center()
            .gap_2()
            .h(px(32.))
            .px_3()
            .mt(if ix == 0 { px(0.) } else { px(10.) })
            .bg(theme.panel)
            .border_y_1()
            .border_color(theme.border_subtle)
            .cursor_pointer()
            .text_size(px(12.))
            .child(file_icon(&path, px(14.), theme))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(name))
            .when_some(folder, |el, folder| el.child(div().min_w_0().truncate().text_color(theme.text_dim).child(folder)))
            .when(dirty, |el| el.child(div().size(px(6.)).flex_shrink_0().rounded_full().bg(theme.accent)))
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(theme.text_muted)
                    .child(icon_el(Icon::FileCode, px(12.), theme.text_muted.into()))
                    .child("Open"),
            )
            .hover_bg(theme.hover)
            .tooltip(|_, cx| crate::ui::tooltip_view("Open the file at this place (F4)", cx))
            .on_click(cx.listener(move |_, _, _, cx| {
                let offset = editor.read(cx).primary_selection().head;
                cx.emit(MultiBufferEvent::Open { path: path.clone(), offset });
            }))
    }

    /// A strip for lines left out: above the file's first excerpt (`ix`,
    /// `up`), between two excerpts (above `ix`) or below its last (`ix`,
    /// down). A click shows more of them.
    fn render_gap(&self, id: (&'static str, usize), lines: usize, cx: &mut Context<Self>, on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static) -> gpui::AnyElement {
        let theme = Theme::get(cx);
        let label = format!("⋯  {lines} line{}", if lines == 1 { "" } else { "s" });
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .h(px(22.))
            .pl(px(52.))
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(theme.text_dim)
            .hover_bg(theme.hover)
            .child(label)
            .tooltip(move |_, cx| crate::ui::tooltip_view(format!("Show up to {EXPAND} more lines"), cx))
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .into_any_element()
    }

    fn render_item(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (rows, lines) = self.rows(ix, cx);
        let first = ix == 0 || !self.same_file(ix - 1, ix);
        let last = !self.same_file(ix, ix + 1);
        let mut el = div().w_full().flex().flex_col();
        if first {
            el = el.child(self.render_header(ix, cx));
            if *rows.start() > 0 {
                el = el.child(self.render_gap(("gap-above", ix), *rows.start(), cx, move |this, cx| this.expand(ix, true, cx)));
            }
        } else {
            let (prev, _) = self.rows(ix - 1, cx);
            let gap = rows.start().saturating_sub(*prev.end() + 1);
            // Both sides grow toward each other; they become one when they meet.
            el = el.child(self.render_gap(("gap-between", ix), gap, cx, move |this, cx| {
                let before = this.items.len();
                this.expand(ix, true, cx);
                // Not merged yet: the one above grows down to meet it.
                if this.items.len() == before {
                    this.expand(ix - 1, false, cx);
                }
            }));
        }
        el = el.child(self.items[ix].editor.clone());
        if last && rows.end() + 1 < lines {
            let below = lines - rows.end() - 1;
            el = el.child(self.render_gap(("gap-below", ix), below, cx, move |this, cx| this.expand(ix, false, cx)));
        }
        el.into_any_element()
    }
}

impl Focusable for MultiBufferView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active_editor() {
            Some(editor) => editor.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for MultiBufferView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The tab got the keyboard before its first excerpts came: they take it.
        if self.focus_handle.is_focused(window)
            && let Some(editor) = self.active_editor()
        {
            window.focus(&editor.focus_handle(cx));
        }
        // Excerpts added (or split) since the last frame: heard from now on.
        for ix in 0..self.items.len() {
            if self.items[ix]._subscriptions.is_empty() {
                let editor = self.items[ix].editor.clone();
                self.items[ix]._subscriptions = self.watch(&editor, window, cx);
            }
        }
        let theme = Theme::get(cx).clone();
        let root = div().key_context("MultiBuffer").track_focus(&self.focus_handle).on_action(cx.listener(Self::open_in_file)).size_full().bg(theme.background);
        if self.items.is_empty() {
            let text = if self.loading > 0 { "Loading…" } else { "Nothing to show" };
            return root.flex().items_center().justify_center().text_color(theme.text_dim).child(text).into_any_element();
        }
        let list = list(self.list.clone(), cx.processor(|this, ix: usize, _, cx| this.render_item(ix, cx))).size_full().pb(px(40.));
        root.relative().child(list).child(crate::ui::scrollbar("multibuffer-scrollbar", &self.list, cx)).into_any_element()
    }
}

/// `path`'s folder as the header shows it: relative to the project (None
/// for a file at its top), else the whole folder.
fn folder_of(path: &Path, root: Option<&Path>) -> Option<String> {
    let parent = path.parent()?;
    let shown = match root.and_then(|r| parent.strip_prefix(r).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => return None,
        Some(rel) => rel,
        None => parent,
    };
    Some(shown.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_become_merged_excerpts() {
        // Around 3 and 5 (touching), 20 apart, 0 at the top, past-the-end held.
        assert_eq!(excerpt_rows([5, 3, 20, 3], 2, 100), [0..=7, 18..=22]);
        assert_eq!(excerpt_rows([0, 99, 150], 2, 100), [0..=2, 97..=99]);
        // A lone line at the start or end is taken in.
        assert_eq!(excerpt_rows([3, 96], 2, 100), [0..=5, 94..=99]);
        // One line between them: merged (a "⋯ 1 line" strip saves nothing).
        assert_eq!(excerpt_rows([2, 8], 2, 100), [0..=10]);
        assert_eq!(excerpt_rows([2, 9], 2, 100), [0..=4, 7..=11]);
        assert!(excerpt_rows([], 2, 10).is_empty());
    }

    #[test]
    fn held_runs_merge_with_their_neighbors() {
        assert_eq!(merge_runs(vec![0..=4, 20..=24], 10..=12), [0..=4, 10..=12, 20..=24]);
        assert_eq!(merge_runs(vec![0..=4, 20..=24], 6..=18), [0..=24]);
        assert_eq!(merge_runs(vec![], 3..=5), [3..=5]);
    }

    #[gpui::test]
    fn a_files_places_are_replaced_keeping_editors(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        let text: String = (0..60).map(|n| format!("line {n}\n")).collect();
        let doc = cx.new(|_| Document::from_text(text, None));
        let other = cx.new(|_| Document::from_text("x\n".into(), None));
        let view = cx.new(|cx| MultiBufferView::new("Problems", None, 2, cx));
        let (a, b) = (PathBuf::from("/p/a.rs"), PathBuf::from("/p/b.rs"));
        let places = |rows: &[usize]| FilePlaces { rows: rows.to_vec(), marks: Vec::new() };
        view.update(cx, |v, cx| {
            v.add_file(a.clone(), doc.clone(), places(&[10, 40]), cx);
            v.add_file(b.clone(), other.clone(), places(&[0]), cx);
        });
        let rows = |cx: &mut gpui::TestAppContext| {
            view.read_with(cx, |v, cx| v.items.iter().map(|i| (i.path.clone(), i.editor.read(cx).excerpt_rows(cx).unwrap())).collect::<Vec<_>>())
        };
        assert_eq!(rows(cx), [(a.clone(), 8..=12), (a.clone(), 38..=42), (b.clone(), 0..=1)]);
        let first = view.read_with(cx, |v, _| v.items[0].editor.clone());

        // The problem at 10 moved to 11, the one at 40 went, one at 25 came.
        view.update(cx, |v, cx| v.set_file_places(a.clone(), doc.clone(), places(&[11, 25]), false, cx));
        assert_eq!(rows(cx), [(a.clone(), 9..=13), (a.clone(), 23..=27), (b.clone(), 0..=1)]);
        view.read_with(cx, |v, _| assert_eq!(v.items[0].editor, first, "the overlapping excerpt kept its editor"));

        // The keyboard's excerpt stays while held, even with nothing left in it.
        view.update(cx, |v, cx| {
            v.active = 1;
            v.set_file_places(a.clone(), doc.clone(), places(&[]), true, cx);
        });
        assert_eq!(rows(cx), [(a.clone(), 23..=27), (b.clone(), 0..=1)]);
        view.read_with(cx, |v, _| assert_eq!(v.active, 0));
        // Not held: the file goes; a new file goes in by path.
        view.update(cx, |v, cx| {
            v.set_file_places(a.clone(), doc.clone(), places(&[]), false, cx);
            v.set_file_places(PathBuf::from("/p/0.rs"), other.clone(), places(&[0]), false, cx);
        });
        assert_eq!(rows(cx), [(PathBuf::from("/p/0.rs"), 0..=1), (b.clone(), 0..=1)]);
        view.read_with(cx, |v, _| assert_eq!(v.paths().len(), 2));
    }
}
