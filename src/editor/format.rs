//! Reformat Code and format on save, through the language server or a
//! command-line formatter (see `crate::format`).

use std::sync::Arc;

use gpui::{App, Context, Task, Window};
use serde_json::{Value, json};

use super::{Editor, EditorLspEvent};
use crate::{
    document::EditKind,
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
            this.update(cx, |e, cx| {
                if e.doc.read(cx).version() != version {
                    return Err("The file changed while formatting".to_string());
                }
                let edits = format::line_edits(&text, &formatted);
                if edits.is_empty() {
                    return Ok(false);
                }
                e.edit_and_remap(edits, EditKind::Other, cx);
                Ok(true)
            })
            .map_err(|_| "The editor closed".to_string())?
        })
    }
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
