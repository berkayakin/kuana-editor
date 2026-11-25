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
//!
//! Resolve with AI asks the model chosen in Settings → AI about each
//! conflict that doesn't combine by itself (its sides, base and the lines
//! around it, `ai::tasks::resolve_conflict`), all at once, then puts every
//! answer in the result as one undoable edit, unless the result changed
//! meanwhile.

use std::{sync::Arc, time::Duration};

use futures::StreamExt as _;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render, SharedString, Subscription, Task, Window,
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
    /// Resolve with AI's requests while they run (dropped, they stop).
    ai: Option<Task<()>>,
    /// What Resolve with AI last said (resolved, failed), until the next edit.
    ai_note: Option<(u64, SharedString)>,
    _subscriptions: Vec<Subscription>,
}

/// Conflicts asked about at once at most; more wait for the next press.
const AI_CONFLICTS: usize = 24;
/// Lines around a conflict sent with it.
const AI_CONTEXT_LINES: usize = 30;

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
        Self { editors, sides: [yours, theirs], maps: None, tops: [None; 3], expected: [None; 3], ai: None, ai_note: None, _subscriptions: subscriptions }
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

    /// Resolve with AI (or Stop while it runs).
    fn resolve_with_ai(&mut self, cx: &mut Context<Self>) {
        if self.ai.take().is_some() {
            cx.notify();
            return;
        }
        let choice = match crate::settings::Settings::get(cx).ai.clone().unwrap_or_default().choice() {
            Ok(choice) => choice,
            Err(why) => {
                self.note(why, cx);
                cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Ai), cx));
                return;
            }
        };
        let (text, version, path, language) = {
            let doc = self.result().read(cx).doc.read(cx);
            let path = doc.path.as_ref().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).unwrap_or_default();
            (doc.rope().to_string(), doc.version(), path, doc.language.name())
        };
        let conflicts = conflict::parse(&text);
        let asked: Vec<usize> = (0..conflicts.len()).filter(|&i| conflicts[i].merged.is_none()).take(AI_CONFLICTS).collect();
        if asked.is_empty() {
            // Only simple ones (or none) left.
            self.resolve_simple(cx);
            return;
        }
        let lines = text.split('\n').count();
        let streams: Vec<_> = asked
            .iter()
            .map(|&i| {
                let c = &conflicts[i];
                let around = |rows: std::ops::Range<usize>| conflict::section(&text, rows);
                let base = c.base.clone().map(around);
                let sides = crate::ai::tasks::ConflictSides {
                    ours: &around(c.ours.clone()),
                    ours_label: &c.ours_label,
                    base: base.as_deref(),
                    theirs: &around(c.theirs.clone()),
                    theirs_label: &c.theirs_label,
                    before: &around(c.start.saturating_sub(AI_CONTEXT_LINES)..c.start),
                    after: &around(c.end + 1..(c.end + 1 + AI_CONTEXT_LINES).min(lines)),
                };
                choice.stream(crate::ai::tasks::resolve_conflict(&path, language, &sides))
            })
            .collect();
        self.ai = Some(cx.spawn(async move |this, cx| {
            let replies = futures::future::join_all(streams.into_iter().map(|mut stream| async move {
                let (mut reply, mut error, mut stop) = (String::new(), None, crate::ai::StopReason::EndTurn);
                while let Some(event) = stream.events.next().await {
                    match event {
                        crate::ai::StreamEvent::Text { text, .. } => reply.push_str(&text),
                        crate::ai::StreamEvent::Stop(reason) => stop = reason,
                        crate::ai::StreamEvent::Error(message) => error = Some(message),
                        _ => {}
                    }
                }
                // Only a whole answer resolves a conflict: an empty, refused or
                // cut-off one would drop or truncate its lines.
                match (error, stop) {
                    (Some(message), _) => Err(message),
                    (None, crate::ai::StopReason::Refusal) => Err("The model declined.".to_string()),
                    (None, crate::ai::StopReason::MaxTokens) => Err("The answer was cut off.".to_string()),
                    (None, _) if reply.trim().is_empty() => Err("The model sent nothing back.".to_string()),
                    (None, _) => crate::ai::tasks::conflict_resolution(&reply).ok_or_else(|| "The answer still had conflict markers.".to_string()),
                }
            }))
            .await;
            this.update(cx, |this, cx| {
                this.ai = None;
                let result = this.result().clone();
                if result.read(cx).doc.read(cx).version() != version {
                    this.note("The result changed while the AI worked; nothing was applied.".into(), cx);
                    return;
                }
                let failure = replies.iter().find_map(|r| r.as_ref().err()).cloned();
                let answers: std::collections::HashMap<usize, Result<String, String>> = asked.iter().copied().zip(replies).collect();
                let mut index = 0;
                let (text, resolved, left) = conflict::resolve_with(&text, &conflicts, |c| {
                    index += 1;
                    match answers.get(&(index - 1)) {
                        Some(answer) => answer.as_ref().ok().cloned(),
                        // Not asked: simple ones combine as Resolve Simple does.
                        None => c.merged.clone(),
                    }
                });
                if resolved > 0 {
                    result.update(cx, |e, cx| e.replace_text(&text, cx));
                }
                let mut note = match (resolved, left) {
                    (0, _) => "The AI resolved nothing.".to_string(),
                    (n, 0) => format!("The AI resolved {n} conflict{}: check the result, then Apply.", if n == 1 { "" } else { "s" }),
                    (n, l) => format!("The AI resolved {n}; {l} left."),
                };
                if let Some(why) = failure {
                    note.push_str(&format!(" ({why})"));
                }
                this.note(note, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// A line in the header from Resolve with AI, shown until the result changes.
    fn note(&mut self, note: String, cx: &mut Context<Self>) {
        let version = self.result().read(cx).doc.read(cx).version();
        self.ai_note = Some((version, note.into()));
        cx.notify();
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
        let version = self.result().read(cx).doc.read(cx).version();
        let ai_note = self.ai_note.as_ref().filter(|(v, _)| *v == version || self.ai.is_some()).map(|(_, n)| n.clone());
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
                    .when_some(ai_note, |el, note| el.child(div().min_w_0().truncate().text_size(px(12.)).text_color(theme.text_dim).child(note)))
                    .child(div().flex_1())
                    .when(left > 0 || self.ai.is_some(), |el| {
                        el.child(
                            button("merge-ai", if self.ai.is_some() { "Resolving… (Stop)" } else { "Resolve with AI" }, false, cx)
                                .tooltip(|_, cx| tooltip_view("Ask the AI model to resolve the conflicts; check the result before Apply", cx))
                                .on_click(cx.listener(|this, _, _, cx| this.resolve_with_ai(cx))),
                        )
                    })
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

    /// Resolve with AI asks about the conflict that doesn't combine, combines
    /// the one that does, and puts both in the result as one edit.
    #[gpui::test]
    fn conflicts_resolve_with_ai(cx: &mut gpui::TestAppContext) {
        use crate::ai::stream::test_server;
        use serde_json::json;

        let chunk = |v: serde_json::Value| format!("data: {v}\n\n");
        let answer = [chunk(json!({ "choices": [{ "delta": { "content": "```\nx = 3\n```" }, "finish_reason": "stop" }] })), "data: [DONE]\n\n".to_string()].concat();
        let server = test_server::start(vec![(200, "text/event-stream", answer)]);
        let settings = crate::settings::Settings::parse(&json!({ "ai": { "provider": "ollama", "model": "m", "providers": { "ollama": { "url": server.url } } } }).to_string()).unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::blank_for_tests());
            cx.set_global(settings);
        });
        let text = "a\n<<<<<<< HEAD\nx = 1\n||||||| base\nx = 0\n=======\nx = 2\n>>>>>>> feature\nb\n<<<<<<< HEAD\nlet one = 1;\n||||||| base\nlet one = 0;\n=======\nlet one = 0;\nlet two = 2;\n>>>>>>> feature\n";
        let file = cx.new(|cx| Editor::new(Document::from_text(text.to_string(), None), cx));
        let cx = cx.add_empty_window();
        let view = cx.update(|window, cx| {
            let version = |t: &str| Version { text: Some(t.to_string()), label: String::new() };
            cx.new(|cx| MergeView::new(&file, String::new(), version("x = 1\n"), version("x = 2\n"), window, cx))
        });
        view.update(cx, |v, cx| v.resolve_with_ai(cx));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while view.read_with(cx, |v, _| v.ai.is_some()) {
            assert!(std::time::Instant::now() < deadline, "no answer");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = file.read_with(cx, |e, cx| e.doc.read(cx).rope().to_string());
        assert_eq!(result, "a\nx = 3\nb\nlet one = 1;\nlet two = 2;\n");
        let request: serde_json::Value = serde_json::from_str(&server.requests.lock().unwrap()[0].1).unwrap();
        let asked = request["messages"][1]["content"].as_str().unwrap().to_string();
        assert!(asked.contains("<ours label=\"HEAD\">\nx = 1\n</ours>\n<base>\nx = 0\n</base>"), "{asked}");
        assert_eq!(server.requests.lock().unwrap().len(), 1, "the simple conflict wasn't asked about");
        // One undo step takes both back.
        file.update(cx, |e, cx| e.undo_step(cx));
        assert_eq!(file.read_with(cx, |e, cx| e.doc.read(cx).rope().to_string()), text);
    }

    #[test]
    fn lines_map_across_versions() {
        let map = line_map("a\nb\nc\nd\n", "a\nx\ny\nc\nd\n");
        // a stays, b's block starts at x, c and d move down one.
        assert_eq!(map, [0, 1, 3, 4]);
        assert_eq!(line_map("", "a\n"), Vec::<usize>::new());
    }
}
