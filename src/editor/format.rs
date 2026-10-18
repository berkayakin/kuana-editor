//! Reformat Code and format on save, through the language server or a
//! command-line formatter (see `crate::format`).

use std::{ops::Range, sync::Arc};

use gpui::{App, Context, Task, Window};
use serde_json::{Value, json};

use super::{Editor, EditorLspEvent};
use crate::{
    document::{EditKind, Patch},
    format,
    lsp::{Server, protocol},
    settings::Settings,
};

impl Editor {
    pub(super) fn reformat_code(&mut self, _: &super::ReformatCode, _: &mut Window, cx: &mut Context<Self>) {
        let task = self.format(true, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |_, cx| match result {
                Ok(true) => {}
                Ok(false) => cx.emit(EditorLspEvent::Message("Already formatted".into())),
                Err(err) => cx.emit(EditorLspEvent::Message(err.into())),
            })
            .ok();
        })
        .detach();
    }

    /// Not in Restricted Mode, where formatters don't run (saving shouldn't
    /// complain about it every time).
    pub fn format_on_save(&self, cx: &App) -> bool {
        self.file_settings.format_on_save && self.doc.read(cx).path.as_deref().is_some_and(crate::trust::is_trusted)
    }

    /// Formats the document, or just the selection when `use_selection` and
    /// the language server can. Prefers a formatter installed in the project,
    /// then the language server, then one on PATH. Resolves to whether the
    /// text changed.
    pub fn format(&mut self, use_selection: bool, cx: &mut Context<Self>) -> Task<Result<bool, String>> {
        let Some(path) = self.doc.read(cx).path.clone() else {
            return Task::ready(Err("Save the file to format it".into()));
        };
        // Formatters and servers may run the project's code (Prettier plugins, say).
        if !crate::trust::is_trusted(&path) {
            return Task::ready(Err("Formatting is off in Restricted Mode. Trust the project to use it.".into()));
        }
        let language = self.doc.read(cx).language;
        let configured = Settings::get(cx).at(&path).formatters.get(language.name()).cloned();
        let command: Option<Vec<String>> = match &configured {
            Some(Value::Bool(false)) => return Task::ready(Err(format!("Formatting is off for {}", language.name()))),
            Some(Value::Array(c)) => Some(c.iter().filter_map(Value::as_str).map(str::to_string).collect()),
            _ => None,
        };
        let server_only = matches!(&configured, Some(Value::String(s)) if s == "language_server");
        let selection = self.primary_selection().range();
        // The first of the document's servers that formats.
        let server = self.lsp_servers(cx).into_iter().find(|s| {
            if use_selection && !selection.is_empty() {
                s.capability("documentRangeFormattingProvider") || s.capability("documentFormattingProvider")
            } else {
                s.capability("documentFormattingProvider")
            }
        });
        let version = self.doc.read(cx).version();
        let text = self.doc.read(cx).rope().to_string();
        let options = json!({
            "tabSize": self.tab_size,
            "insertSpaces": self.indent_unit != "\t",
            "trimTrailingWhitespace": self.file_settings.trim_trailing_whitespace,
            "insertFinalNewline": self.file_settings.insert_final_newline,
        });
        let range = (use_selection && !selection.is_empty())
            .then(|| protocol::offsets_to_range(self.doc.read(cx).rope(), selection, server.as_ref().map_or(protocol::Encoding::Utf16, |s| s.encoding)));
        cx.spawn(async move |this, cx| {
            // Finding a formatter checks the disk (and the login shell's PATH).
            let cli = if server_only {
                None
            } else {
                let path = path.clone();
                cx.background_executor().spawn(async move { format::find(language, &path, command.as_deref()) }).await
            };
            let use_cli = match (&cli, &server) {
                (Some(f), _) if f.project_local => true,
                (Some(_), None) => true,
                _ => false,
            };
            let formatted = if use_cli {
                let formatter = cli.expect("checked above");
                let (input, file) = (text.clone(), path.clone());
                cx.background_executor().spawn(async move { formatter.run(&input, &file) }).await?
            } else if let Some(server) = server {
                lsp_format(&server, &path, &text, range, options).await?
            } else {
                return Err(format!(
                    "No formatter for {}. Install one, or a language server that formats.",
                    language.name()
                ));
            };
            let edits = cx.background_executor().spawn(async move { format::line_edits(&text, &formatted) }).await;
            this.update(cx, |e, cx| {
                if edits.is_empty() {
                    return Ok(false);
                }
                // Typing went on meanwhile: the formatter's edits move past it.
                let changed = "The file changed while formatting".to_string();
                let patches = e.doc.read(cx).patches_since(version).ok_or_else(|| changed.clone())?;
                let edits = rebase_edits(edits, patches);
                if edits.is_empty() {
                    return Err(changed);
                }
                e.edit_and_remap(edits, EditKind::Other, cx);
                Ok(true)
            })
            .map_err(|_| "The editor closed".to_string())?
        })
    }
}

/// A formatter's edits to the text it was given, moved to today's text
/// through the edits made since (`patches`). One that an edit since touched
/// is left out, so what was typed meanwhile stays (as Zed's
/// `Buffer::apply_diff` does); the others still apply.
fn rebase_edits<'a>(edits: Vec<(Range<usize>, String)>, patches: impl Iterator<Item = &'a Patch>) -> Vec<(Range<usize>, String)> {
    let patches: Vec<&Patch> = patches.collect();
    edits
        .into_iter()
        .filter_map(|(mut range, text)| {
            for patch in &patches {
                let (delta, touching) = patch.around(range.clone());
                if !touching.is_empty() {
                    return None;
                }
                range = range.start.checked_add_signed(delta)?..range.end.checked_add_signed(delta)?;
            }
            Some((range, text))
        })
        .collect()
}

/// Asks the server to format, and applies its edits to a copy of `text`.
async fn lsp_format(
    server: &Arc<Server>,
    path: &std::path::Path,
    text: &str,
    range: Option<protocol::LspRange>,
    options: Value,
) -> Result<String, String> {
    let document = json!({ "uri": protocol::path_to_uri(path) });
    let ranged = range.filter(|_| server.capability("documentRangeFormattingProvider"));
    let result = match ranged {
        Some(range) => {
            let params = json!({ "textDocument": document, "range": range.to_json(), "options": options });
            server.request("textDocument/rangeFormatting", params).await
        }
        None => {
            let params = json!({ "textDocument": document, "options": options });
            server.request("textDocument/formatting", params).await
        }
    };
    let value = result.map_err(|e| format!("{}: {e}", server.name))?;
    let rope = ropey::Rope::from_str(text);
    let edits = protocol::resolve_edits(&rope, &protocol::text_edits(Some(&value)), server.encoding);
    let mut out = text.to_string();
    for (range, new_text) in edits.iter().rev() {
        out.replace_range(range.clone(), new_text);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut out = text.to_string();
        for (range, new_text) in edits.iter().rev() {
            out.replace_range(range.clone(), new_text);
        }
        out
    }

    #[test]
    fn typing_while_formatting_keeps_the_edits_it_did_not_touch() {
        let (text, formatted) = ("a  =1\nb=2\nc  =3\nd=4\ne  =5\n", "a = 1\nb=2\nc = 3\nd=4\ne = 5\n");
        assert_eq!(format::line_edits(text, formatted).len(), 3);
        // Meanwhile: "x" typed on line 2 (which the formatter leaves), then
        // "y" on line 3 (which it reformats).
        let typed = [Patch::new(vec![(7..7, 1)]), Patch::new(vec![(12..12, 1)])];
        let now = "a  =1\nbx=2\ncy  =3\nd=4\ne  =5\n";
        let rebased = rebase_edits(format::line_edits(text, formatted), typed.iter());
        // Lines 1 and 5 are formatted; line 3 keeps what was typed.
        assert_eq!(apply(now, &rebased), "a = 1\nbx=2\ncy  =3\nd=4\ne = 5\n");
        // Nothing since: every edit applies as it was.
        assert_eq!(apply(text, &rebase_edits(format::line_edits(text, formatted), std::iter::empty())), formatted);
    }
}
