//! Snippets in the editor: inserting one (indented like its line, first stop
//! selected), Tab / Shift+Tab between its stops, and user snippets expanded by
//! typing their prefix and Tab.

use std::ops::Range;

use gpui::{App, Context};
use serde_json::json;

use super::{Editor, Selection};
use crate::{
    document::EditKind,
    lsp::protocol::{CompletionItem, expand_snippet_with},
    snippets::{self, SnippetStops},
};

impl Editor {
    /// What `body`'s variables are filled from, at the caret now; `expr`:
    /// a postfix template's expression. The clipboard is read only when the
    /// body uses it.
    pub(super) fn snippet_variables(&self, body: &str, expr: Option<String>, cx: &App) -> snippets::Variables {
        let doc = self.doc.read(cx);
        let sel = self.primary_selection();
        let row = doc.row_of(sel.head);
        snippets::Variables {
            file: doc.path.clone(),
            workspace: None,
            selected: doc.slice(sel.range()),
            line: doc.line_text(row),
            word: doc.slice(super::word_range(doc, sel.head)),
            line_number: row + 1,
            clipboard: body.contains("CLIPBOARD").then(|| cx.read_from_clipboard().and_then(|c| c.text())).flatten(),
            line_comment: doc.language.line_comment().map(|c| c.trim_end().to_string()),
            block_comment: doc.language.block_comment().map(|(start, end)| (start.to_string(), end.to_string())),
            expr,
            now: None,
        }
    }

    /// A snippet body expanded for insertion at `at`: indented like its
    /// line, its variables filled in.
    pub(super) fn expand_snippet_at(&self, body: &str, at: usize, expr: Option<String>, cx: &App) -> (String, Vec<Vec<Range<usize>>>) {
        let vars = self.snippet_variables(body, expr, cx);
        expand_snippet_with(&self.indented_snippet(body, at, cx), &|name| vars.get(name))
    }

    /// A snippet body indented for insertion at `at`: later lines start at
    /// that line's indentation, tabs become the file's indent unit.
    pub(super) fn indented_snippet(&self, body: &str, at: usize, cx: &App) -> String {
        let line = self.doc.read(cx).line_text(self.doc.read(cx).row_of(at));
        let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        snippets::indent_body(body, &indent, &self.indent_unit)
    }

    /// After a snippet's text went in at `base`: selects its first stop and,
    /// when there are more, remembers them for Tab.
    pub(super) fn select_snippet(&mut self, base: usize, len: usize, stops: Vec<Vec<Range<usize>>>, cx: &mut Context<Self>) {
        let stops: Vec<Vec<Range<usize>>> =
            stops.into_iter().map(|s| s.into_iter().map(|r| base + r.start..base + r.end).collect()).collect();
        let Some(first) = stops.first().cloned() else {
            self.set_selections(vec![Selection::cursor(base + len)], 0, cx);
            return;
        };
        self.snippet = (stops.len() > 1).then_some(SnippetStops { stops, current: 0 });
        self.select_stop(&first, cx);
    }

    fn select_stop(&mut self, ranges: &[Range<usize>], cx: &mut Context<Self>) {
        let selections = ranges.iter().map(|r| Selection { anchor: r.start, head: r.end }).collect();
        self.set_selections(selections, 0, cx);
    }

    /// Tab (`step` 1) or Shift+Tab (-1) in a snippet: moves between its stops;
    /// reaching the last ends it. False when no snippet is being filled in
    /// here, so the key does its usual thing.
    pub(super) fn snippet_tab(&mut self, step: isize, cx: &mut Context<Self>) -> bool {
        let Some(mut snippet) = self.snippet.take() else { return false };
        let head = self.primary_selection().head;
        // The caret left the snippet: it's done.
        if !snippet.stops[snippet.current].iter().any(|r| r.start <= head && head <= r.end) {
            return false;
        }
        let last = snippet.stops.len() - 1;
        snippet.current = (snippet.current as isize + step).clamp(0, last as isize) as usize;
        let ranges = snippet.stops[snippet.current].clone();
        if snippet.current < last {
            self.snippet = Some(snippet);
        }
        self.select_stop(&ranges, cx);
        true
    }

    /// Tab after a user snippet's prefix: replaces it with the snippet.
    pub(super) fn expand_user_snippet(&mut self, cx: &mut Context<Self>) -> bool {
        let sel = self.primary_selection();
        if self.selections.len() != 1 || !sel.is_empty() {
            return false;
        }
        let head = sel.head;
        let line_start = self.doc.read(cx).line_start(self.doc.read(cx).row_of(head));
        let before = self.doc.read(cx).slice(line_start..head);
        // The longest prefix that ends at the caret and starts a word.
        let Some(snippet) = snippets::user_snippets(self.doc.read(cx).language)
            .into_iter()
            .filter(|s| !s.prefix.is_empty() && before.ends_with(&s.prefix))
            .filter(|s| {
                let rest = &before[..before.len() - s.prefix.len()];
                !(rest.chars().last().is_some_and(super::lsp::is_word_char) && s.prefix.chars().next().is_some_and(super::lsp::is_word_char))
            })
            .max_by_key(|s| s.prefix.len())
        else {
            return false;
        };
        let start = head - snippet.prefix.len();
        let (text, stops) = self.expand_snippet_at(&snippet.body, start, None, cx);
        let len = text.len();
        self.edit_and_remap(vec![(start..head, text)], EditKind::Other, cx);
        self.select_snippet(start, len, stops, cx);
        true
    }
}

/// The user's snippets as completion items, listed with the server's.
pub(super) fn user_snippet_items(language: crate::syntax::Language) -> Vec<CompletionItem> {
    snippets::user_snippets(language)
        .into_iter()
        .filter_map(|s| {
            CompletionItem::from_json(json!({
                "label": s.prefix,
                "kind": 15,
                "detail": s.description,
                "insertText": s.body,
                "insertTextFormat": 2,
                "kuanaUserSnippet": true,
            }))
        })
        .collect()
}
