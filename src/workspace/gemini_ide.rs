//! Gemini CLI's IDE connection for this window (`crate::gemini_ide`): the
//! window's server runs while a local project is open, the setting is on and
//! Gemini CLI is installed; it's told which files are open (the most recently
//! active first), the active one's caret and selection and whether the
//! project is trusted, and the window answers its two tools.
//!
//! `openDiff` opens the change as a diff tab (`agent_ide.rs`, shared with
//! Claude Code) and answers at once; the user's Accept or Reject goes to
//! Gemini as `ide/diffAccepted` (with the text, which Gemini writes too) or
//! `ide/diffRejected`. `closeDiff` (the user answered in the terminal) closes
//! the tab without either and answers the text it proposed.

use super::agent_ide::Proposal;
use super::*;
use crate::gemini_ide::{self as ide, OpenFile, Request, Server, ToolCall};
use serde_json::Value;

#[derive(Default)]
pub(super) struct GeminiBridge {
    pub(super) server: Option<Server>,
    /// Takes the server's requests while it runs.
    requests: Option<Task<()>>,
    /// How many Gemini CLI sessions are connected (the status bar's item).
    pub(super) clients: usize,
}

impl Workspace {
    /// Starts, updates or stops the window's server.
    pub(super) fn sync_gemini_ide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = Settings::get(cx).gemini_cli_integration.unwrap_or(true) && ide::gemini_installed();
        let Some(folders) = self.agent_folders(wanted) else {
            self.agents.gemini = GeminiBridge::default();
            return;
        };
        if let Some(server) = &self.agents.gemini.server {
            server.set_folders(folders);
            return;
        }
        match Server::start(folders, &ide::discovery_dir()) {
            Ok((server, requests)) => {
                self.agents.gemini.server = Some(server);
                self.agents.gemini.requests = Some(Self::take_agent_requests(requests, Self::gemini_request, window, cx));
            }
            Err(err) => eprintln!("couldn't start Gemini CLI's IDE server: {err}"),
        }
    }

    fn gemini_request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            Request::Connected => {
                self.agents.gemini.clients += 1;
                self.agent_connected(cx);
            }
            Request::Disconnected => {
                self.agents.gemini.clients = self.agents.gemini.clients.saturating_sub(1);
                cx.notify();
            }
            Request::Tool(call) if call.name == "openDiff" => self.gemini_open_diff(call, window, cx),
            Request::Tool(call) if call.name == "closeDiff" => {
                let given = call.arguments.get("filePath").and_then(Value::as_str).unwrap_or_default().to_string();
                let path = self.agent_path(&given);
                let taken = self.take_proposals(|p, file| matches!(p, Proposal::Gemini { file_path } if *file_path == given || file == path), window, cx);
                let text = taken.into_iter().find_map(|(_, text)| text);
                call.answer(Ok(vec![ide::closed_diff(text.as_deref())]));
            }
            Request::Tool(call) => {
                let answer = Err(format!("Unknown tool: {}", call.name));
                call.answer(answer);
            }
        }
    }

    /// Shows a proposed change in a diff tab and answers at once whether it
    /// opened; the user's decision follows as a notification.
    fn gemini_open_diff(&mut self, call: ToolCall, window: &mut Window, cx: &mut Context<Self>) {
        let string = |key: &str| call.arguments.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let (file_path, text) = (string("filePath"), string("newContent"));
        let path = self.agent_path(&file_path);
        let proposal = Proposal::Gemini { file_path: file_path.clone() };
        let shown = self.show_proposal(path, file_path, text, proposal, window, cx);
        call.answer(shown.map(|()| Vec::new()).map_err(|_| "Kuana couldn't show the change".to_string()));
    }

    /// Tells Gemini what's open and selected (it hears of it when it changed).
    pub(super) fn send_gemini_context(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.agents.gemini.server.as_ref().filter(|s| s.has_clients()) else { return };
        let active = self.active_editor(cx);
        let mut files: Vec<OpenFile> = Vec::new();
        for editor in &self.editors {
            let Some(path) = editor.read(cx).doc.read(cx).path.clone() else { continue };
            if files.iter().any(|f| f.path == path) {
                continue;
            }
            let timestamp = self.agents.focused.get(&path).copied().unwrap_or(0);
            files.push(OpenFile { path, timestamp, active: None });
        }
        // The active file's caret, from its active editor (a split shows
        // one file in two).
        if let Some(editor) = active {
            let e = editor.read(cx);
            let doc = e.doc.read(cx);
            if let (true, Some(path)) = (e.is_loaded(), &doc.path)
                && let Some(file) = files.iter_mut().find(|f| f.path == *path)
            {
                let selection = e.primary_selection();
                file.active = Some(ide::caret(doc.rope(), selection.anchor, selection.head));
            }
        }
        // Only files on disk (a deleted file's tab stays open).
        files.retain(|f| f.path.is_file());
        server.set_context(ide::context(files, !self.restricted(cx)));
    }
}
