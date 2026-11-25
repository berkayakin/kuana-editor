//! Claude Code's `/ide` connection for this window (`crate::claude_ide`):
//! the window's server runs while a local project is open, the setting is
//! on and Claude Code is installed; it's told the project's folders and the
//! active editor's selection, and the window answers its tool calls: open
//! editors, problems, opening files, unsaved changes and saving.
//!
//! Edits Claude proposes (`openDiff`) open as diff tabs whose call waits
//! until the user decides (`agent_ide.rs`, shared with Gemini CLI): Accept
//! answers `FILE_SAVED` with the text, which Claude writes too (the same
//! bytes); Reject, or closing the tab, answers `DIFF_REJECTED`.

use super::agent_ide::Proposal;
use super::*;
use crate::claude_ide::{self as ide, OpenSelection, Request, Server, ToolCall};
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct ClaudeBridge {
    pub(super) server: Option<Server>,
    /// Takes the server's requests while it runs.
    requests: Option<Task<()>>,
    /// How many Claude Code sessions are connected (the status bar's item).
    pub(super) clients: usize,
}

impl Workspace {
    /// Starts, updates or stops the window's server (after the project or
    /// its folders change, and when settings do).
    pub(super) fn sync_claude_ide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = Settings::get(cx).claude_code_integration.unwrap_or(true) && ide::claude_installed();
        let Some(folders) = self.agent_folders(wanted) else {
            self.agents.claude = ClaudeBridge::default();
            return;
        };
        if let Some(server) = &self.agents.claude.server {
            server.set_folders(folders);
            return;
        }
        let Some(dir) = ide::lock_dir() else { return };
        match Server::start(folders, &dir) {
            Ok((server, requests)) => {
                self.agents.claude.server = Some(server);
                self.agents.claude.requests = Some(Self::take_agent_requests(requests, Self::claude_request, window, cx));
            }
            Err(err) => eprintln!("couldn't start Claude Code's IDE server: {err}"),
        }
    }

    fn claude_request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            Request::Connected => {
                self.agents.claude.clients += 1;
                self.agent_connected(cx);
            }
            Request::Disconnected => {
                self.agents.claude.clients = self.agents.claude.clients.saturating_sub(1);
                cx.notify();
            }
            Request::Tool(call) if call.name == "openDiff" => self.claude_open_diff(call, window, cx),
            Request::Tool(call) => {
                let answer = match call.name.as_str() {
                    "getOpenEditors" => Ok(vec![self.claude_open_editors(cx).to_string()]),
                    "getDiagnostics" => {
                        let only = call.arguments.get("uri").and_then(Value::as_str).map(|uri| self.agent_path(uri));
                        Ok(vec![self.claude_diagnostics(only, cx).to_string()])
                    }
                    "openFile" => self.claude_open_file(&call.arguments, window, cx),
                    "checkDocumentDirty" | "saveDocument" => {
                        let given = call.arguments.get("filePath").and_then(Value::as_str).unwrap_or_default().to_string();
                        let save = call.name == "saveDocument";
                        Ok(vec![self.claude_document(&given, save, window, cx).to_string()])
                    }
                    "close_tab" => {
                        let name = call.arguments.get("tab_name").and_then(Value::as_str).unwrap_or_default().to_string();
                        self.close_claude_diffs(|tab| tab == name, window, cx);
                        Ok(vec!["TAB_CLOSED".to_string()])
                    }
                    "closeAllDiffTabs" => {
                        let closed = self.close_claude_diffs(|_| true, window, cx);
                        Ok(vec![format!("CLOSED_{closed}_DIFF_TABS")])
                    }
                    other => Err(format!("Unknown tool: {other}")),
                };
                call.answer(answer);
            }
        }
    }

    fn claude_open_editors(&self, cx: &App) -> Value {
        let active = self.active_editor(cx).map(|e| e.entity_id());
        // A file split in two is one editor to Claude, active when either is.
        let mut tabs: Vec<(PathBuf, Value)> = Vec::new();
        for editor in &self.editors {
            let doc = editor.read(cx).doc.read(cx);
            let Some(path) = doc.path.as_ref() else { continue };
            let is_active = Some(editor.entity_id()) == active;
            if let Some((_, tab)) = tabs.iter_mut().find(|(p, _)| p == path) {
                if is_active {
                    tab["isActive"] = json!(true);
                }
                continue;
            }
            let tab = json!({
                "uri": ide::file_url(path),
                "isActive": is_active,
                "label": path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                "languageId": crate::lsp::servers::language_id(doc.language, path),
                "isDirty": doc.is_dirty(),
                "fileName": path,
                "isUntitled": false,
                "lineCount": doc.line_count(),
            });
            tabs.push((path.clone(), tab));
        }
        json!({ "tabs": tabs.into_iter().map(|(_, tab)| tab).collect::<Vec<_>>() })
    }

    /// The problems Kuana shows: open files' (built-in lint, linters and
    /// language servers, moved by edits since) and what servers published
    /// for files that aren't open. `only` one file, listed even without any.
    fn claude_diagnostics(&self, only: Option<PathBuf>, cx: &App) -> Value {
        let mut files = Vec::new();
        let mut open = HashSet::new();
        for editor in &self.editors {
            let editor = editor.read(cx);
            let doc = editor.doc.read(cx);
            // A restored tab not read yet has no problems of its own: the
            // servers' are listed for it below.
            let Some(path) = doc.path.clone().filter(|_| editor.is_loaded()) else { continue };
            if !open.insert(path.clone()) || only.as_ref().is_some_and(|o| *o != path) {
                continue;
            }
            let list: Vec<Value> = doc.diagnostics.iter().map(|d| ide::diagnostic_json(doc.rope(), d)).collect();
            if only.is_some() || !list.is_empty() {
                files.push(json!({ "uri": ide::file_url(&path), "diagnostics": list }));
            }
        }
        let mut elsewhere: HashMap<PathBuf, Vec<Value>> = HashMap::new();
        for (path, server, list) in self.lsp.read(cx).published_elsewhere(&open) {
            if only.as_ref().is_none_or(|o| *o == path) {
                elsewhere.entry(path).or_default().extend(list.iter().map(|d| ide::lsp_diagnostic_json(d, &server)));
            }
        }
        let mut elsewhere: Vec<_> = elsewhere.into_iter().collect();
        elsewhere.sort_by(|a, b| a.0.cmp(&b.0));
        files.extend(elsewhere.into_iter().map(|(path, list)| json!({ "uri": ide::file_url(&path), "diagnostics": list })));
        if files.is_empty()
            && let Some(path) = only
        {
            files.push(json!({ "uri": ide::file_url(&path), "diagnostics": [] }));
        }
        Value::Array(files)
    }

    fn claude_open_file(&mut self, arguments: &Value, window: &mut Window, cx: &mut Context<Self>) -> Result<Vec<String>, String> {
        let given = arguments.get("filePath").and_then(Value::as_str).unwrap_or_default();
        let path = self.agent_path(given);
        if !path.is_file_on_host() {
            return Err(format!("File not found: {given}"));
        }
        let select = OpenSelection::read(arguments);
        let focus = arguments.get("makeFrontmost").and_then(Value::as_bool).unwrap_or(true);
        self.navigate(window, cx, |ws, window, cx| {
            ws.open_path(path.clone(), window, cx);
            let Some(editor) = ws.tab_for(&path, cx) else { return };
            editor.update(cx, |e, cx| {
                let range = select.range(e.doc.read(cx).rope());
                if let Some(range) = range {
                    e.select_range(range, cx);
                }
            });
            if focus {
                window.focus(&editor.focus_handle(cx));
            }
        });
        Ok(vec![format!("Opened file: {}", path.display())])
    }

    /// `checkDocumentDirty`, or with `save`, `saveDocument`: an open file's
    /// unsaved changes, saved as Save would (format on save, the question
    /// for a file changed on disk).
    fn claude_document(&mut self, given: &str, save: bool, window: &mut Window, cx: &mut Context<Self>) -> Value {
        let path = self.agent_path(given);
        let Some(editor) = self.tab_for(&path, cx) else {
            return json!({ "success": false, "message": format!("Document not open: {given}") });
        };
        let dirty = editor.read(cx).doc.read(cx).is_dirty();
        if !save {
            return json!({ "success": true, "filePath": given, "isDirty": dirty, "isUntitled": false });
        }
        if dirty {
            self.save_one(editor, window, cx);
        }
        let message = if dirty { "Document saved" } else { "Document had no unsaved changes" };
        json!({ "success": true, "filePath": given, "saved": true, "message": message })
    }

    /// Shows a proposed change in a diff tab; the call is answered when the
    /// user decides. A proposal under the name of one shown replaces it
    /// (that one is rejected).
    fn claude_open_diff(&mut self, call: ToolCall, window: &mut Window, cx: &mut Context<Self>) {
        let string = |key: &str| call.arguments.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let (path, tab_name, text) = (self.agent_path(&string("new_file_path")), string("tab_name"), string("new_file_contents"));
        let proposal = Proposal::Claude { call, tab_name: tab_name.clone() };
        if let Err(proposal) = self.show_proposal(path, tab_name, text, proposal, window, cx)
            && let Proposal::Claude { call, .. } = *proposal
        {
            call.answer(Err("Kuana couldn't show the change".into()));
        }
    }

    /// Closes Claude's proposed changes' tabs whose names `which` takes,
    /// rejecting them; how many.
    fn close_claude_diffs(&mut self, which: impl Fn(&str) -> bool, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let taken = self.take_proposals(|p, _| matches!(p, Proposal::Claude { tab_name, .. } if which(tab_name)), window, cx);
        let count = taken.len();
        for (proposal, _) in taken {
            proposal.reject(&self.agents);
        }
        count
    }

    /// Whether Claude Code is connected to this window.
    pub(super) fn claude_connected(&self) -> bool {
        self.agents.claude.server.is_some() && self.agents.claude.clients > 0
    }

    /// Add to Claude Context: the active file, or its selected lines, goes
    /// into Claude's prompt as an @-mention.
    pub(super) fn add_to_claude_context(&mut self, _: &AddToClaudeContext, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(server), true) = (self.agents.claude.server.as_ref(), self.agents.claude.clients > 0) else {
            let detail = "Run claude in a terminal in this project, or /ide in one that's running.";
            self.toast(ToastKind::Info, "Claude Code isn't connected", Some(detail.into()), cx);
            return;
        };
        let Some(editor) = self.active_editor(cx) else { return };
        let e = editor.read(cx);
        let doc = e.doc.read(cx);
        let Some(path) = &doc.path else { return };
        server.broadcast("at_mentioned", ide::mention(path, doc.rope(), e.primary_selection().range()));
    }

    /// Tells Claude the active editor's selection (it hears of it when it
    /// changed).
    pub(super) fn send_claude_selection(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.agents.claude.server.as_ref().filter(|s| s.has_clients()) else { return };
        let Some(editor) = self.active_editor(cx) else { return };
        let e = editor.read(cx);
        let doc = e.doc.read(cx);
        if let (true, Some(path)) = (e.is_loaded(), &doc.path) {
            server.set_selection(ide::selection_payload(path, doc.rope(), e.primary_selection().range()));
        }
    }
}
