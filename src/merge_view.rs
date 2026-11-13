//! The Merge view (JetBrains' Merge Revisions): a conflicted file's three
//! versions side by side, yours on the left, theirs on the right and the
//! result between them. The sides are read-only editors of each version
//! (`Document::read_only`), their changes against the common base marked in
//! the gutter; the result is another view of the file's own document
//! (`Editor::view_of` of its tab), so its edits, undo and conflict labels
//! (Accept Current / Incoming / Both / Combination) are the editor's. The
//! three scroll together, line by line where the texts match.
//!
//! Apply saves the result and marks the file resolved; the workspace does
//! both (`MergeViewEvent::Apply`), as it does taking a side that deleted the
//! file (`AcceptDeletion`).

use std::{sync::Arc, time::Duration};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render, Subscription, Window,
    div, prelude::*, px,
};

use crate::{
    assets::Icon,
    conflict::{self, Conflict},
    document::Document,
    document_diff::DocumentDiff,
    editor::{Cancel, Editor},
    theme::Theme,
    ui::{icon_button, icon_el, text_button, tooltip_view},
};

pub enum MergeViewEvent {
    /// Save the result and mark the file resolved.
    Apply,
    /// Resolve by taking a side that deleted the file (`yours`: your side).
    AcceptDeletion { yours: bool },
    Close,
}

/// One side of the merge: its version of the file (None: that side deleted
/// it) and what the conflict markers call it ("HEAD", a branch, "Stashed changes").
pub struct Version {
    pub text: Option<String>,
    pub label: String,
}

/// Where each version's lines are in the others, for scrolling together:
/// for the result's text at `version`.
struct Maps {
    version: u64,
    result_to: [Vec<usize>; 2],
    to_result: [Vec<usize>; 2],
}

pub struct MergeView {
    /// Yours, the result, theirs.
    editors: [Entity<Editor>; 3],
    sides: [Version; 2],
    maps: Option<Maps>,
    /// Each editor's top line as last seen, and the one it was told to scroll to.
    tops: [Option<usize>; 3],
    expected: [Option<usize>; 3],
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MergeViewEvent> for MergeView {}

impl MergeView {
    /// `file` is the conflicted file's editor (its tab), `base` the version
    /// both sides started from.
    pub fn new(file: &Entity<Editor>, base: String, yours: Version, theirs: Version, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = file.read(cx).doc.read(cx).language;
        let marked = |editor: &Entity<Editor>, cx: &mut Context<Self>| {
            let doc = editor.read(cx).doc.clone();
            let diff = cx.new(|cx| DocumentDiff::new(&doc, cx));
            diff.update(cx, |d, cx| d.set_base(None, Some(base.clone()), Some(base.clone()), cx));
            editor.update(cx, |e, cx| e.set_git_diff(diff, cx));
        };
        let side = |version: &Version, cx: &mut Context<Self>| {
            let text = version.text.clone().unwrap_or_default();
            let editor = cx.new(|cx| Editor::new(Document::read_only(text, language), cx));
            marked(&editor, cx);
            editor
        };
        let left = side(&yours, cx);
        let right = side(&theirs, cx);
        let result = cx.new(|cx| Editor::view_of(file, cx));
        marked(&result, cx);
        window.focus(&result.focus_handle(cx));
        let editors = [left, result, right];
        let subscriptions = editors.iter().enumerate().map(|(i, e)| cx.observe(e, move |this, _, cx| this.scrolled(i, cx))).collect();
        Self { editors, sides: [yours, theirs], maps: None, tops: [None; 3], expected: [None; 3], _subscriptions: subscriptions }
    }

    fn result(&self) -> &Entity<Editor> {
        &self.editors[1]
    }

    /// The maps for the result's text now.
    fn maps(&mut self, cx: &App) -> &Maps {
        let doc = self.result().read(cx).doc.read(cx);
        let version = doc.version();
        if self.maps.as_ref().is_none_or(|m| m.version != version) {
            let result = doc.rope().to_string();
            let side = |i: usize| self.sides[i].text.as_deref().unwrap_or("");
            self.maps = Some(Maps {
                version,
                result_to: [line_map(&result, side(0)), line_map(&result, side(1))],
                to_result: [line_map(side(0), &result), line_map(side(1), &result)],
            });
        }
        self.maps.as_ref().expect("just made")
    }

    /// Editor `i` notified: if the user scrolled it, the others follow.
    fn scrolled(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(top) = self.editors[i].read(cx).top_line(cx) else { return };
        if self.tops[i] == Some(top) {
            return;
        }
        self.tops[i] = Some(top);
        // Where it was told to go: not a scroll of its own.
        if self.expected[i].take() == Some(top) {
            return;
        }
        let maps = self.maps(cx);
        let at = |map: &[usize], line: usize| map.get(line).or(map.last()).copied().unwrap_or(0);
        let result_line = match i {
            0 => at(&maps.to_result[0], top),
            2 => at(&maps.to_result[1], top),
            _ => top,
        };
        let lines = [at(&maps.result_to[0], result_line), result_line, at(&maps.result_to[1], result_line)];
        for (j, line) in lines.into_iter().enumerate() {
            if j != i && self.tops[j] != Some(line) {
                self.expected[j] = Some(line);
                self.editors[j].update(cx, |e, cx| e.scroll_line_to_top(line, cx));
            }
        }
    }

    /// The result's conflicts, as its editor knows them (it parses the text
    /// once per version, for its own labels too).
    fn conflicts(&self, cx: &mut App) -> Arc<Vec<Conflict>> {
        self.result().update(cx, |e, cx| e.conflicts(cx))
    }

    /// Moves the result's caret to the next (or previous) conflict, around the end.
    fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.result().clone();
        let caret = result.read(cx).doc.read(cx).row_of(result.read(cx).primary_selection().head);
        let rows: Vec<usize> = self.conflicts(cx).iter().map(|c| c.start).collect();
        let next = if forward {
            rows.iter().find(|&&r| r > caret).or(rows.first())
        } else {
            rows.iter().rev().find(|&&r| r < caret).or(rows.last())
        };
        if let Some(&row) = next {
            result.update(cx, |e, cx| {
                let offset = e.doc.read(cx).line_start(row);
                e.go_to(offset, window, cx);
            });
        }
    }

    /// Combines every conflict whose sides change different words, as one undoable edit.
    fn resolve_simple(&mut self, cx: &mut Context<Self>) {
        self.result().update(cx, |e, cx| {
            let (text, resolved, _) = conflict::resolve_simple(&e.doc.read(cx).rope().to_string());
            if resolved > 0 {
                e.replace_text(&text, cx);
            }
        });
    }

    /// Makes the result one side's whole version (undoable); a side that
    /// deleted the file is taken by the workspace.
    fn accept(&mut self, yours: bool, cx: &mut Context<Self>) {
        match self.sides[if yours { 0 } else { 1 }].text.clone() {
            Some(text) => self.result().update(cx, |e, cx| e.replace_text(&text, cx)),
            None => cx.emit(MergeViewEvent::AcceptDeletion { yours }),
        }
    }

    fn on_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(MergeViewEvent::Close);
    }

    fn column(&self, i: usize, title: &str, label: Option<&str>, deleted: bool, theme: &Theme) -> impl IntoElement {
        // Modern: a title over an island. Classic: flush squares side by
        // side, a separator line between them and under each title's strip.
        let line = crate::ui::separator(theme);
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .when(!theme.classic, |el| el.gap_1())
            .when(theme.classic && i > 0, |el| el.border_l_1().border_color(line))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1p5()
                    .map(|el| if theme.classic { el.h(px(28.)).px_3().border_b_1().border_color(line) } else { el.px_1() })
                    .text_size(px(12.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title.to_string()))
                    .when_some(label.filter(|l| !l.is_empty()), |el, label| {
                        el.child(div().min_w_0().truncate().text_color(theme.text_dim).child(label.to_string()))
                    })
                    .when(deleted, |el| el.child(div().text_color(theme.git_deleted).child("deleted on this side"))),
            )
            .child(crate::ui::area(theme).flex_1().min_h_0().overflow_hidden().child(self.editors[i].clone()))
    }
}

/// For each line of `from`, the line of `to` it matches, or where its
/// changed block starts there.
fn line_map(from: &str, to: &str) -> Vec<usize> {
    let diff = similar::TextDiff::configure().timeout(Duration::from_millis(50)).diff_lines(from, to);
    let mut map = Vec::new();
    for op in diff.ops() {
        let (old, new) = (op.old_range(), op.new_range());
        for (k, _) in old.enumerate() {
            map.push(new.start + k.min(new.len().saturating_sub(1)));
        }
    }
    map
}

impl Focusable for MergeView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.result().focus_handle(cx)
    }
}

impl Render for MergeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let left = self.conflicts(cx).len();
        let status = match left {
            0 => "No conflicts left".to_string(),
            1 => "1 conflict left".to_string(),
            n => format!("{n} conflicts left"),
        };
        let button = |id: &'static str, label: &'static str, primary: bool, cx: &mut Context<Self>| text_button(id, label, primary, cx);
        let step = |id: &'static str, icon: Icon, tip: &'static str, forward: bool, cx: &mut Context<Self>| {
            icon_button(id, icon, theme.text_muted.into(), cx)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .on_click(cx.listener(move |this, _, window, cx| this.step(forward, window, cx)))
        };
        // The whole of its window (`tool_window.rs`), under the title bar
        // that names the file.
        div()
            .key_context("MergeView")
            .on_action(cx.listener(Self::on_cancel))
            .size_full()
            .flex()
            .flex_col()
            .when(!theme.classic, |el| el.pb_3().gap_2())
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .when(theme.classic, |el| el.pb_2())
                    .child(icon_el(Icon::GitCompare, px(15.), theme.text_muted.into()))
                    .child(div().text_size(px(12.)).text_color(if left == 0 { theme.text_dim } else { theme.git_conflict }).child(status))
                    .child(step("merge-previous", Icon::ChevronUp, "Previous Conflict", false, cx))
                    .child(step("merge-next", Icon::ChevronDown, "Next Conflict", true, cx))
                    .child(div().flex_1())
                    .child(
                        button("merge-simple", "Resolve Simple", false, cx)
                            .tooltip(|_, cx| tooltip_view("Combine the conflicts where both sides changed different words", cx))
                            .on_click(cx.listener(|this, _, _, cx| this.resolve_simple(cx))),
                    )
                    .child(button("merge-yours", "Accept Yours", false, cx).on_click(cx.listener(|this, _, _, cx| this.accept(true, cx))))
                    .child(button("merge-theirs", "Accept Theirs", false, cx).on_click(cx.listener(|this, _, _, cx| this.accept(false, cx))))
                    .child(button("merge-cancel", "Cancel", false, cx).on_click(cx.listener(|_, _, _, cx| cx.emit(MergeViewEvent::Close))))
                    .child(button("merge-apply", "Apply", true, cx).on_click(cx.listener(|_, _, _, cx| cx.emit(MergeViewEvent::Apply)))),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .map(|el| if theme.classic { el.border_t_1().border_color(crate::ui::separator(&theme)) } else { el.px_3().gap_2() })
                    .child(self.column(0, "Yours", Some(&self.sides[0].label), self.sides[0].text.is_none(), &theme))
                    .child(self.column(1, "Result", None, false, &theme))
                    .child(self.column(2, "Theirs", Some(&self.sides[1].label), self.sides[1].text.is_none(), &theme)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_map_across_versions() {
        let map = line_map("a\nb\nc\nd\n", "a\nx\ny\nc\nd\n");
        // a stays, b's block starts at x, c and d move down one.
        assert_eq!(map, [0, 1, 3, 4]);
        assert_eq!(line_map("", "a\n"), Vec::<usize>::new());
    }
}
