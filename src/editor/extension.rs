//! What extensions' code sees and changes of an open file (the `editor`,
//! `apply-edits` and `set-selections` calls of `extensions::ui_host`). All
//! of it runs when the code asks, never while drawing or typing.

use std::ops::Range;

use super::*;
use crate::extensions::ui_host::EditorState;

impl Editor {
    /// The file as extensions' code gets it: a text file read into a full
    /// editor, not too large to hand over.
    pub fn extension_state(&self, with_text: bool, cx: &App) -> Option<EditorState> {
        let doc = self.doc.read(cx);
        if !self.is_loaded() || self.is_chromeless() || doc.large {
            return None;
        }
        let mut selections: Vec<Range<usize>> = Vec::with_capacity(self.selections.len());
        selections.push(self.primary_selection().range());
        selections.extend(self.selections.iter().enumerate().filter(|(ix, _)| *ix != self.primary).map(|(_, s)| s.range()));
        Some(EditorState {
            path: doc.path.clone()?,
            language: doc.language.name().to_string(),
            version: doc.version(),
            dirty: doc.is_dirty(),
            selections,
            // A rope's clone shares its text: the string is made off this thread.
            text: with_text.then(|| doc.rope().clone()),
        })
    }

    /// That `spans` are places in the text at `version`: the text hasn't
    /// changed since, and each is in it, between characters.
    fn check_extension_spans<'a>(&self, version: u64, spans: impl Iterator<Item = &'a Range<usize>>, cx: &App) -> Result<(), String> {
        let doc = self.doc.read(cx);
        if !self.is_loaded() || self.is_chromeless() || doc.large {
            return Err("the file isn't one extensions can edit".into());
        }
        if doc.version() != version {
            return Err(format!("the text has changed since version {version} (it's {} now)", doc.version()));
        }
        for span in spans {
            if span.start > span.end || span.end > doc.len() {
                return Err(format!("{}..{} isn't in the text ({} bytes)", span.start, span.end, doc.len()));
            }
            if doc.clip(span.start) != span.start || doc.clip(span.end) != span.end {
                return Err(format!("{}..{} cuts a character in two", span.start, span.end));
            }
        }
        Ok(())
    }

    /// An extension's edits, as one undo step: the new version, or why they
    /// weren't made (the text moved on, a span outside it, two that overlap).
    pub fn apply_extension_edits(&mut self, version: u64, mut edits: Vec<(Range<usize>, String)>, cx: &mut Context<Self>) -> Result<u64, String> {
        self.check_extension_spans(version, edits.iter().map(|(span, _)| span), cx)?;
        if self.doc.read(cx).is_read_only() {
            return Err("the file is read-only".into());
        }
        edits.sort_by_key(|(span, _)| (span.start, span.end));
        if let Some(pair) = edits.windows(2).find(|pair| pair[1].0.start < pair[0].0.end || pair[1].0 == pair[0].0) {
            return Err(format!("the edits at {}..{} and {}..{} overlap", pair[0].0.start, pair[0].0.end, pair[1].0.start, pair[1].0.end));
        }
        edits.retain(|(span, text)| !span.is_empty() || !text.is_empty());
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
            // The carets follow the edits; the view stays where the user has it.
            self.autoscroll = false;
        }
        Ok(self.doc.read(cx).version())
    }

    /// Carets and selections an extension sets, the main one first, which
    /// is scrolled to.
    pub fn set_extension_selections(&mut self, version: u64, spans: Vec<Range<usize>>, cx: &mut Context<Self>) -> Result<(), String> {
        if spans.is_empty() {
            return Err("no selection was given".into());
        }
        self.check_extension_spans(version, spans.iter(), cx)?;
        let selections = spans.into_iter().map(|span| Selection { anchor: span.start, head: span.end }).collect();
        self.set_selections(selections, 0, cx);
        self.reveal_primary = true;
        cx.notify();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn editor(text: &str, cx: &mut TestAppContext) -> Entity<Editor> {
        let dir = std::env::temp_dir().join(format!("kuana-ext-editor-{}-{}", std::process::id(), text.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, text).unwrap();
        cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx))
    }

    #[gpui::test]
    fn edits_apply_as_one_undo_step_at_their_version(cx: &mut TestAppContext) {
        let editor = editor("héllo wörld\n", cx);
        cx.update(|cx| {
            let state = editor.read(cx).extension_state(true, cx).unwrap();
            assert_eq!(state.text.as_ref().map(|r| r.to_string()).as_deref(), Some("héllo wörld\n"));
            assert_eq!(state.selections, [0..0]);
            let version = state.version;

            editor.update(cx, |e, cx| {
                // Inside "é" (2 bytes at 1..3), past the end, overlapping: refused, nothing changes.
                assert!(e.apply_extension_edits(version, vec![(2..3, "x".into())], cx).unwrap_err().contains("character"));
                assert!(e.apply_extension_edits(version, vec![(0..99, "x".into())], cx).unwrap_err().contains("isn't in the text"));
                assert!(e.apply_extension_edits(version, vec![(0..4, "x".into()), (3..6, "y".into())], cx).unwrap_err().contains("overlap"));
                assert_eq!(e.text(cx), "héllo wörld\n");

                // Given out of order; both land, in one step.
                let after = e.apply_extension_edits(version, vec![(7..13, "WORLD".into()), (0..6, "HELLO".into())], cx).unwrap();
                assert_eq!(e.text(cx), "HELLO WORLD\n");
                assert!(after != version);

                // The version they were made for is gone.
                assert!(e.apply_extension_edits(version, vec![(0..1, "x".into())], cx).unwrap_err().contains("changed"));

                e.set_extension_selections(after, vec![6..11, 0..0], cx).unwrap();
                let state = e.extension_state(false, cx).unwrap();
                assert_eq!(state.selections, [6..11, 0..0]);
                assert!(state.text.is_none() && state.dirty);

                e.undo_step(cx);
                assert_eq!(e.text(cx), "héllo wörld\n");
            });
        });
    }
}
