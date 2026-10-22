//! Edit predictions: the code a provider (GitHub Copilot, `lsp::copilot`)
//! suggests after the caret, shown as ghost text until Tab accepts it or
//! anything else dismisses it. The suggestion's first line sits inline at
//! the caret, as an inlay (`display_part`), and the rest as block rows under
//! the caret's line (`build_blocks`). Where a suggestion retypes what's after
//! the caret (the `)` typing `(` closed), it goes after that text instead.
//!
//! It's asked for a moment after each edit, with one caret and nothing
//! selected; typing what it suggests keeps it (shorter), and so does stepping
//! over the text it goes after; typing anything else or moving the caret
//! elsewhere drops it.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use gpui::{App, Context, Task};
use serde_json::{Value, json};

use super::{EditKind, Editor, EditorMode, Selection};
use crate::lsp::{
    Server,
    protocol::{self, Encoding, offset_to_position, path_to_uri},
};

/// How long after an edit a suggestion is asked for: typing doesn't send a
/// request per key (Zed waits as long).
const DEBOUNCE: Duration = Duration::from_millis(75);

#[derive(Default)]
pub(super) struct Predictions {
    shown: Option<Shown>,
    /// The request waiting or out; a newer one replaces it, which cancels it.
    task: Option<Task<()>>,
    /// Bumped when what shows changes (the block rows key on it).
    pub(super) generation: u64,
}

struct Shown {
    /// The caret it's for, in document `version`, then `len` bytes long.
    at: usize,
    version: u64,
    len: usize,
    /// The text after the caret it goes after (usually none; a closing
    /// bracket it starts by writing again).
    skip: String,
    /// What goes in there.
    text: String,
    /// The server's item, for its telemetry (`didShowCompletion`) and the
    /// command accepting it runs, with the server (if it's still there).
    item: Value,
    server: Weak<Server>,
}

impl Editor {
    /// Whether this editor suggests anything now: a code editor with one
    /// caret and no selection, outside Vim's Normal and Visual modes and
    /// a snippet's stops.
    fn predicts(&self, cx: &App) -> bool {
        self.mode == EditorMode::Full
            && self.selections.len() == 1
            && self.primary_selection().is_empty()
            && self.snippet.is_none()
            && !self.vim_block_cursor()
            && !self.doc.read(cx).large
    }

    /// The Copilot server to ask for this document, signed in.
    fn prediction_server(&self, cx: &App) -> Option<Arc<Server>> {
        let path = self.doc.read(cx).path.as_deref()?;
        self.lsp.store.as_ref()?.upgrade()?.read(cx).copilot_for(path)
    }

    /// After an edit through the editor: a suggestion typed into stays (what's
    /// left of it), any other goes; a new one is asked for.
    pub(super) fn prediction_edited(&mut self, cx: &mut Context<Self>) {
        if let Some(shown) = self.prediction.shown.take() {
            let doc = self.doc.read(cx);
            let head = self.primary_selection().head;
            // Only text was added at the caret (the document grew by just
            // that), and it's how the suggestion starts.
            let grown = doc.len() == shown.len + head.saturating_sub(shown.at);
            let typed = (head > shown.at && self.predicts(cx) && grown && shown.skip.is_empty()).then(|| doc.slice(shown.at..head));
            let (version, len) = (doc.version(), doc.len());
            if let Some(typed) = typed.filter(|t| shown.text.starts_with(t.as_str()) && t.len() < shown.text.len()) {
                self.prediction.shown = Some(Shown { at: head, version, len, text: shown.text[typed.len()..].to_string(), ..shown });
                self.keep_prediction();
                return;
            }
            // Typing over what it goes after (a closing bracket typing steps
            // over): it's at the caret now.
            if !shown.skip.is_empty() && head == shown.at + shown.skip.len() && doc.len() == shown.len && doc.slice(shown.at..head) == shown.skip {
                self.prediction.shown = Some(Shown { at: head, version, len, skip: String::new(), ..shown });
                self.keep_prediction();
                return;
            }
            self.prediction.generation += 1;
        }
        self.schedule_prediction(DEBOUNCE, 2, cx);
    }

    /// The suggestion stays as it was moved to: it's still right, so no new
    /// request (Copilot would answer the same).
    fn keep_prediction(&mut self) {
        self.prediction.generation += 1;
        self.prediction.task = None;
    }

    /// The caret moved without an edit: a suggestion for where it was goes,
    /// unless the caret stepped over the text it goes after.
    pub(super) fn prediction_selection_changed(&mut self, cx: &mut Context<Self>) {
        let version = self.doc.read(cx).version();
        let (head, one) = (self.primary_selection().head, self.selections.len() == 1 && self.primary_selection().is_empty());
        let Some(shown) = self.prediction.shown.as_mut().filter(|s| s.version == version) else { return };
        if one && !shown.skip.is_empty() && head == shown.at + shown.skip.len() {
            shown.at = head;
            shown.skip.clear();
            self.keep_prediction();
        } else if shown.at != head || !one {
            self.dismiss_prediction(cx);
        }
    }

    /// Asks for a suggestion at the caret now (Alt+\), even right after one was dismissed.
    pub(super) fn show_prediction(&mut self, cx: &mut Context<Self>) {
        self.schedule_prediction(Duration::ZERO, 1, cx);
    }

    /// Asks the server for a suggestion at the caret, `delay` from now;
    /// `trigger` is LSP's: 1 asked for, 2 while typing.
    fn schedule_prediction(&mut self, delay: Duration, trigger: u8, cx: &mut Context<Self>) {
        self.prediction.task = None;
        if !self.predicts(cx) {
            return;
        }
        let Some(server) = self.prediction_server(cx) else { return };
        let Some(path) = self.doc.read(cx).path.clone() else { return };
        let insert_spaces = self.indent_unit != "\t";
        let tab_size = self.tab_size;
        self.prediction.task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            // The document and caret as they are now that the pause is over.
            let Ok(Some((params, head, version))) = this.update(cx, |e, cx| {
                let doc = e.doc.read(cx);
                let head = e.primary_selection().head;
                let position = offset_to_position(doc.rope(), head, server.encoding).to_json();
                let params = json!({
                    "textDocument": { "uri": path_to_uri(&path), "version": doc.version() as i64 },
                    "position": position,
                    "context": { "triggerKind": trigger },
                    "formattingOptions": { "tabSize": tab_size, "insertSpaces": insert_spaces },
                });
                e.predicts(cx).then_some((params, head, doc.version()))
            }) else {
                return;
            };
            let Ok(result) = server.request("textDocument/inlineCompletion", params).await else { return };
            this.update(cx, |e, cx| {
                let doc = e.doc.read(cx);
                if doc.version() != version || e.primary_selection().head != head || !e.predicts(cx) {
                    return;
                }
                let items = result.get("items").unwrap_or(&result);
                let found = items.as_array().into_iter().flatten().find_map(|item| Some((ghost_text(doc.rope(), head, item, server.encoding)?, item.clone())));
                let Some(((skip, text), item)) = found else { return };
                let len = doc.len();
                server.client.notify("textDocument/didShowCompletion", json!({ "item": item }));
                e.prediction.shown = Some(Shown { at: head, version, len, skip, text, item, server: Arc::downgrade(&server) });
                e.prediction.generation += 1;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Escape, or anything that makes the suggestion wrong: it goes.
    pub(super) fn dismiss_prediction(&mut self, cx: &mut Context<Self>) -> bool {
        self.prediction.task = None;
        if self.prediction.shown.take().is_none() {
            return false;
        }
        self.prediction.generation += 1;
        cx.notify();
        true
    }

    /// Tab: puts the suggestion in (after the text it skips), as one undo
    /// step, the caret after it.
    pub(super) fn accept_prediction(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(shown) = self.prediction.shown.take_if(|s| s.version == self.doc.read(cx).version()) else { return false };
        self.prediction.generation += 1;
        if self.primary_selection().head != shown.at || !self.predicts(cx) {
            cx.notify();
            return false;
        }
        let at = shown.at + shown.skip.len();
        let end = at + shown.text.len();
        self.edit_and_remap(vec![(at..at, shown.text.clone())], EditKind::Other, cx);
        self.set_selections(vec![Selection::cursor(end)], 0, cx);
        // Copilot counts what's accepted through the item's command.
        if let Some(command) = shown.item.get("command").filter(|c| c.get("command").is_some()).cloned()
            && let Some(server) = shown.server.upgrade()
        {
            let params = json!({
                "command": command["command"],
                "arguments": command.get("arguments").cloned().unwrap_or_else(|| json!([])),
            });
            // Kept until it's answered: a dropped request is cancelled.
            cx.background_executor().spawn(async move { server.request("workspace/executeCommand", params).await.ok() }).detach();
        }
        cx.notify();
        true
    }

    /// The suggestion's first line, inline at the caret (or after the text
    /// it skips): its buffer offset and text. None while the completion menu
    /// covers it.
    pub(super) fn prediction_inline(&self, cx: &App) -> Option<(usize, &str)> {
        let shown = self.shown_prediction(cx)?;
        let first = shown.text.split('\n').next().unwrap_or_default();
        (!first.is_empty()).then_some((shown.at + shown.skip.len(), first))
    }

    /// The suggestion's other lines, as rows under the caret's line: that
    /// line and the rows.
    pub(super) fn prediction_rows(&self, cx: &App) -> Option<(usize, Vec<String>)> {
        let shown = self.shown_prediction(cx)?;
        let rows: Vec<String> = shown.text.split('\n').skip(1).map(str::to_string).collect();
        (!rows.is_empty()).then(|| (self.doc.read(cx).row_of(shown.at), rows))
    }

    fn shown_prediction(&self, cx: &App) -> Option<&Shown> {
        self.prediction.shown.as_ref().filter(|s| s.version == self.doc.read(cx).version() && self.lsp.completion.is_none())
    }

    /// Shows `text` at the caret, after `skip` (tests).
    #[cfg(test)]
    pub(super) fn suggest_for_tests(&mut self, skip: &str, text: &str, cx: &App) {
        let doc = self.doc.read(cx);
        let (at, version, len) = (self.primary_selection().head, doc.version(), doc.len());
        self.prediction.shown = Some(Shown { at, version, len, skip: skip.into(), text: text.into(), item: json!({}), server: Weak::new() });
        self.prediction.generation += 1;
    }

    /// What the block rows key on for the suggestion (see `BlocksKey`).
    pub(super) fn prediction_key(&self) -> (u64, bool) {
        (self.prediction.generation, self.lsp.completion.is_some())
    }
}

/// What an inline completion `item` for the caret `head` shows as ghost text:
/// the text after the caret it goes after, and the text, or None when it
/// can't show as one insertion there. Its range starts at or before the
/// caret (Copilot's at the line's start, its text repeating what's before the
/// caret) and ends on the caret's line. It may replace text after the caret,
/// which is kept where the suggestion writes it again: before the ghost text
/// (a closing bracket it starts with) or after it (one it ends with). The rest
/// of the line can't follow a suggestion of several lines, since it would
/// show after its first.
fn ghost_text(rope: &ropey::Rope, head: usize, item: &Value, encoding: Encoding) -> Option<(String, String)> {
    let insert = match item.get("insertText")? {
        Value::String(text) => text.clone(),
        snippet => snippet.get("value")?.as_str()?.to_string(),
    };
    let insert = insert.replace("\r\n", "\n");
    let (start, end) = match item.get("range").and_then(protocol::LspRange::from_json) {
        Some(range) => {
            let range = protocol::range_to_offsets(rope, range, encoding);
            (range.start, range.end)
        }
        None => (head, head),
    };
    if start > head || end < head || end > rope.len_bytes() {
        return None;
    }
    let line = rope.byte_to_line(head);
    let line_end = if line + 1 < rope.len_lines() { rope.line_to_byte(line + 1) - 1 } else { rope.len_bytes() };
    if end > line_end {
        return None;
    }
    let before = rope.byte_slice(start..head).to_string();
    let text = insert.strip_prefix(before.as_str())?;
    // The line after the caret now, and as the suggestion leaves it: the
    // ghost text is what the second has where the first is cut in two.
    let old = rope.byte_slice(head..line_end).to_string();
    let new = format!("{text}{}", rope.byte_slice(end..line_end));
    let cuts = old.char_indices().map(|(i, _)| i).chain([old.len()]);
    cuts.filter(|&cut| new.len() >= old.len() && new.starts_with(&old[..cut]) && new.ends_with(&old[cut..]))
        .map(|cut| (cut, &new[cut..new.len() - (old.len() - cut)]))
        .find(|(cut, ghost)| !ghost.trim().is_empty() && (*cut == old.len() || !ghost.contains('\n')))
        .map(|(cut, ghost)| (old[..cut].to_string(), ghost.to_string()))
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;
    use crate::document::Document;

    #[gpui::test]
    fn a_suggestion_shows_follows_typing_and_goes_in_with_tab(cx: &mut TestAppContext) {
        let editor = cx.new(|cx| Editor::new(Document::from_text("fn main() {\n    \n}\n".into(), None), cx));
        let suggest = |e: &mut Editor, text: &str, cx: &mut Context<Editor>| {
            let doc = e.doc.read(cx);
            let (at, version, len) = (e.primary_selection().head, doc.version(), doc.len());
            e.prediction.shown = Some(Shown { at, version, len, skip: String::new(), text: text.into(), item: json!({}), server: Weak::new() });
        };
        editor.update(cx, |e, cx| {
            let at = "fn main() {\n    ".len();
            e.set_selections(vec![Selection::cursor(at)], 0, cx);
            suggest(e, "let x = 1;\n    x", cx);
            // The first line at the caret, the rest under its line.
            assert_eq!(e.prediction_inline(cx), Some((at, "let x = 1;")));
            assert_eq!(e.display_part(1, 0..=0, cx).1.text, "    let x = 1;");
            assert_eq!(*e.block_rows(cx), [(2, 1)]);

            // Typing what it says keeps the rest; anything else drops it.
            e.insert("l", cx);
            assert_eq!(e.prediction_inline(cx), Some((at + 1, "et x = 1;")));
            e.insert("o", cx);
            assert_eq!(e.prediction_inline(cx), None);
            assert!(e.block_rows(cx).is_empty());

            // Moving the caret drops it too.
            suggest(e, "ng = 2;", cx);
            e.set_selections(vec![Selection::cursor(0)], 0, cx);
            assert_eq!(e.prediction_inline(cx), None);

            // Tab puts it in, the caret after it.
            let end = e.doc.read(cx).line_end(1);
            e.set_selections(vec![Selection::cursor(end)], 0, cx);
            suggest(e, "ng = 2;\n    long", cx);
            assert!(e.accept_prediction(cx));
            assert_eq!(e.doc.read(cx).rope().to_string(), "fn main() {\n    long = 2;\n    long\n}\n");
            assert_eq!(e.primary_selection().head, e.doc.read(cx).line_end(2));
            assert!(!e.accept_prediction(cx));

            // Escape dismisses it.
            suggest(e, ";", cx);
            assert!(e.dismiss_prediction(cx));
            assert_eq!(e.prediction_inline(cx), None);
        });
    }

    #[gpui::test]
    fn a_suggestion_after_a_closing_bracket_goes_after_it(cx: &mut TestAppContext) {
        let new = |cx: &mut TestAppContext| cx.new(|cx| Editor::new(Document::from_text("def f(n)\n".into(), None), cx));
        let at = "def f(n".len();
        let suggest = |e: &mut Editor, cx: &mut Context<Editor>| {
            let doc = e.doc.read(cx);
            let (at, version, len) = (e.primary_selection().head, doc.version(), doc.len());
            e.prediction.shown = Some(Shown { at, version, len, skip: ")".into(), text: ":\n    pass".into(), item: json!({}), server: Weak::new() });
        };
        new(cx).update(cx, |e, cx| {
            e.set_selections(vec![Selection::cursor(at)], 0, cx);
            suggest(e, cx);
            // Shown after the bracket, the caret still before it.
            assert_eq!(e.prediction_inline(cx), Some((at + 1, ":")));
            assert_eq!(e.display_part(0, 0..=0, cx).1.text, "def f(n):");
            // Tab puts it after the bracket.
            assert!(e.accept_prediction(cx));
            assert_eq!(e.doc.read(cx).rope().to_string(), "def f(n):\n    pass\n");
            assert_eq!(e.primary_selection().head, e.doc.read(cx).line_end(1));
        });
        new(cx).update(cx, |e, cx| {
            // Stepping over the bracket keeps it, now at the caret.
            e.set_selections(vec![Selection::cursor(at)], 0, cx);
            suggest(e, cx);
            e.set_selections(vec![Selection::cursor(at + 1)], 0, cx);
            assert_eq!(e.prediction_inline(cx), Some((at + 1, ":")));
            // Going back drops it.
            e.set_selections(vec![Selection::cursor(at)], 0, cx);
            assert_eq!(e.prediction_inline(cx), None);
        });
    }

    const UTF16: Encoding = Encoding::Utf16;

    #[test]
    fn ghost_text_is_what_goes_after_the_caret() {
        let rope = ropey::Rope::from_str("fn main() {\n    let x = \n}\n");
        let head = "fn main() {\n    let x = ".len();
        let line = |text: &str, start: u32, end: u32| {
            json!({ "insertText": text, "range": { "start": { "line": 1, "character": start }, "end": { "line": 1, "character": end } } })
        };
        let at_caret = |text: &str| Some((String::new(), text.to_string()));
        // Copilot repeats the line up to the caret.
        assert_eq!(ghost_text(&rope, head, &line("    let x = 42;", 0, 12), UTF16), at_caret("42;"));
        // A suggestion going on below.
        assert_eq!(ghost_text(&rope, head, &line("    let x = 1;\n    x", 0, 12), UTF16), at_caret("1;\n    x"));
        // Text that isn't what's before the caret doesn't show.
        assert_eq!(ghost_text(&rope, head, &line("    let y = 1;", 0, 12), UTF16), None);
        // Nothing but spaces doesn't either.
        assert_eq!(ghost_text(&rope, head, &line("    let x =   ", 0, 12), UTF16), None);
    }

    #[test]
    fn a_closing_bracket_after_the_caret_stays() {
        let rope = ropey::Rope::from_str("print()\n");
        let head = "print(".len();
        let whole_line = |text: &str| json!({ "insertText": text, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } } });
        // One it ends with: the ghost text goes before it.
        assert_eq!(ghost_text(&rope, head, &whole_line("print(\"hi\")"), UTF16), Some((String::new(), "\"hi\"".into())));
        // One that changes the text after the caret can't show as ghost text.
        assert_eq!(ghost_text(&rope, head, &whole_line("print(\"hi\"]"), UTF16), None);

        // One it starts with (typing `(n` closed it): the ghost text goes after it,
        // even over several lines.
        let rope = ropey::Rope::from_str("def is_prime(n)\n");
        let head = "def is_prime(n".len();
        let item = json!({ "insertText": "def is_prime(n):\n    return n > 1", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 15 } } });
        assert_eq!(ghost_text(&rope, head, &item, UTF16), Some((")".into(), ":\n    return n > 1".into())));
        // Lines below the caret's line with the bracket before the caret's
        // text left over: it would show after the first, so it doesn't show.
        let item = json!({ "insertText": "def is_prime(n:\n    x):", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 14 } } });
        assert_eq!(ghost_text(&rope, head, &item, UTF16), None);
    }
}

