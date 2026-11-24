//! Claude Code's `/ide` connection for this window (`crate::claude_ide`):
//! the window's server runs while a local project is open, the setting is
//! on and Claude Code is installed; it's told the project's folders and the
//! active editor's selection, and the window answers its tool calls: open
//! editors, problems, opening files, unsaved changes and saving.
//!
//! Edits Claude proposes (`openDiff`) open as diff tabs
//! (`DiffSource::Proposed`) whose call waits in `ClaudeBridge::proposals`
//! until the user decides. Accept puts the text in the file's document as
//! one undoable edit (its text before kept in Local History) and saves it,
//! then answers `FILE_SAVED` with the text, which Claude writes too (the same
//! bytes); Reject, or closing the tab, answers `DIFF_REJECTED`.

use super::*;
use crate::claude_ide::{self as ide, OpenSelection, Request, Server, ToolCall};
use crate::diff_view::DiffSource;
use futures::StreamExt as _;
use serde_json::{Value, json};

/// How long the selection stays put before Claude hears of it.
const SELECTION_DELAY: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(super) struct ClaudeBridge {
    server: Option<Server>,
    /// Takes the server's requests while it runs.
    requests: Option<Task<()>>,
    /// The pane item last seen active (`note_claude_editor`), and the
    /// observation of its editor.
    seen_item: Option<EntityId>,
    watching: Option<Subscription>,
    /// What was last sent: the editor, its document's version, its
    /// selections' generation.
    sent: Option<(EntityId, u64, u64)>,
    sending: Option<Task<()>>,
    /// Proposed changes waiting for the user, by their diff tab.
    proposals: HashMap<EntityId, Proposal>,
    /// How many Claude Code sessions are connected (the status bar's item).
    clients: usize,
}

/// An `openDiff` call waiting for Accept or Reject.
struct Proposal {
    call: ToolCall,
    tab_name: String,
}

impl Proposal {
    fn reject(self) {
        let answer = vec!["DIFF_REJECTED".to_string(), self.tab_name];
        self.call.answer(Ok(answer));
    }
}

impl Workspace {
    /// Starts, updates or stops the window's server (after the project or
    /// its folders change, and when settings do).
    pub(super) fn sync_claude_ide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = !cfg!(test) && Settings::get(cx).claude_code_integration.unwrap_or(true) && ide::claude_installed();
        // A remote project's paths exist only through its connection: a
        // `claude` here couldn't read them, one there can't reach this port.
        let folders = self
            .folders
            .as_ref()
            .filter(|p| wanted && kuana_core::remote::connection::host_of(p.primary()).is_none())
            .map(|p| p.folders().to_vec());
        let Some(folders) = folders else {
            self.claude = ClaudeBridge::default();
            return;
        };
        if let Some(server) = &self.claude.server {
            server.set_folders(folders);
            return;
        }
        let Some(dir) = ide::lock_dir() else { return };
        match Server::start(folders, &dir) {
            Ok((server, mut requests)) => {
                self.claude.server = Some(server);
                self.claude.requests = Some(cx.spawn_in(window, async move |ws, cx| {
                    while let Some(request) = requests.next().await {
                        if ws.update_in(cx, |ws, window, cx| ws.claude_request(request, window, cx)).is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(err) => eprintln!("couldn't start Claude Code's IDE server: {err}"),
        }
    }

    /// Stops the window's server (the project closed).
    pub(super) fn stop_claude_ide(&mut self) {
        self.claude = ClaudeBridge::default();
    }

    fn claude_request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            Request::Connected => {
                self.claude.clients += 1;
                self.claude.sent = None;
                self.watch_claude_editor(cx);
                cx.notify();
            }
            Request::Disconnected => {
                self.claude.clients = self.claude.clients.saturating_sub(1);
                cx.notify();
            }
            Request::Tool(call) if call.name == "openDiff" => self.claude_open_diff(call, window, cx),
            Request::Tool(call) => {
                let answer = match call.name.as_str() {
                    "getOpenEditors" => Ok(vec![self.claude_open_editors(cx).to_string()]),
                    "getDiagnostics" => {
                        let only = call.arguments.get("uri").and_then(Value::as_str).map(|uri| self.claude_path(uri));
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

    /// A path Claude names (a `file://` URL or a path, relative to the
    /// project's first folder), as the tabs have it.
    fn claude_path(&self, given: &str) -> PathBuf {
        let path = ide::path_of(given);
        let path = match &self.folders {
            Some(project) if path.is_relative() => project.primary().join(path),
            _ => path,
        };
        crate::paths::canonicalize(&path).unwrap_or(path)
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
        let path = self.claude_path(given);
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
        let path = self.claude_path(given);
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
        let arguments = &call.arguments;
        let string = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let path = self.claude_path(&string("new_file_path"));
        let tab_name = string("tab_name");
        let source = DiffSource::Proposed {
            new_file: !path.is_file_on_host(),
            path,
            tab_name: tab_name.clone(),
            text: string("new_file_contents").into(),
        };
        let replaced: Vec<EntityId> = self.claude.proposals.iter().filter(|(_, p)| p.tab_name == tab_name).map(|(id, _)| *id).collect();
        for id in replaced {
            if let Some(old) = self.claude.proposals.remove(&id) {
                old.reject();
            }
        }
        match self.show_diff(source, window, cx) {
            Some(diff) => {
                self.claude.proposals.insert(diff.entity_id(), Proposal { call, tab_name });
            }
            None => call.answer(Err("Kuana couldn't show the change".into())),
        }
    }

    /// Accept or Reject in a proposed change's tab. Accept applies it even
    /// when nobody waits any more (Claude ended meanwhile).
    pub(super) fn claude_proposal_decided(&mut self, diff: &Entity<DiffView>, accepted: bool, window: &mut Window, cx: &mut Context<Self>) {
        let proposal = self.claude.proposals.remove(&diff.entity_id());
        let DiffSource::Proposed { path, text, new_file, .. } = diff.read(cx).source().clone() else { return };
        self.remove_item(&PaneItem::Diff(diff.clone()), window, cx);
        if !accepted {
            if let Some(proposal) = proposal {
                proposal.reject();
            }
            return;
        }
        // Claude hears "saved" only once the file is: a failed save is
        // its error (the save's own prompt tells the user).
        let applying = self.apply_claude_change(&path, &text, new_file, window, cx);
        cx.spawn(async move |_, _| {
            let applied = applying.await;
            if let Some(proposal) = proposal {
                proposal.call.answer(applied.map(|()| vec!["FILE_SAVED".to_string(), text.to_string()]));
            }
        })
        .detach();
    }

    /// Puts a proposed text in its file: a new file is written; an existing
    /// one's document gets it as one undoable edit and is saved (without
    /// format on save, which would change what Claude then writes). The
    /// task ends when the file is written.
    fn apply_claude_change(&mut self, path: &Path, text: &Arc<str>, new_file: bool, window: &mut Window, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        if new_file && !path.is_file_on_host() {
            let written = path.parent().map_or(Ok(()), crate::fs::create_dir_all).and_then(|()| crate::fs::write(path, text.as_bytes()));
            if let Err(err) = written {
                self.toast(ToastKind::Error, "Couldn't create the file Claude Code proposed", Some(err.to_string()), cx);
                return Task::ready(Err(err.to_string()));
            }
            self.files_written([path.to_path_buf()], cx);
            self.open_path(path.to_path_buf(), window, cx);
            return Task::ready(Ok(()));
        }
        self.open_path(path.to_path_buf(), window, cx);
        let Some(editor) = self.tab_for(path, cx) else {
            let message = format!("Couldn't open {}", path.display());
            self.toast(ToastKind::Error, "Couldn't apply Claude Code's change", Some(message.clone()), cx);
            return Task::ready(Err(message));
        };
        let current = editor.read(cx).doc.read(cx).rope().clone();
        let kept = path.to_path_buf();
        cx.background_executor().spawn(async move { crate::local_history::record_rope(&kept, &current, crate::local_history::Kind::BeforeAgent) }).detach();
        editor.update(cx, |e, cx| e.replace_text(text, cx));
        self.save_editor_task(&editor, window, cx)
    }

    /// Closes proposed changes' tabs whose names `which` takes, rejecting
    /// them; how many.
    fn close_claude_diffs(&mut self, which: impl Fn(&str) -> bool, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let ids: Vec<EntityId> = self.claude.proposals.iter().filter(|(_, p)| which(&p.tab_name)).map(|(id, _)| *id).collect();
        for id in &ids {
            if let Some(proposal) = self.claude.proposals.remove(id) {
                proposal.reject();
            }
            if let Some(diff) = self.diff_views(cx).into_iter().find(|d| d.entity_id() == *id) {
                self.remove_item(&PaneItem::Diff(diff), window, cx);
            }
        }
        ids.len()
    }

    /// A tab closed: a proposed change in it is rejected.
    pub(super) fn claude_tab_removed(&mut self, id: EntityId) {
        if let Some(proposal) = self.claude.proposals.remove(&id) {
            proposal.reject();
        }
    }

    /// Whether Claude Code is connected to this window.
    pub(super) fn claude_connected(&self) -> bool {
        self.claude.server.is_some() && self.claude.clients > 0
    }

    /// Add to Claude Context: the active file, or its selected lines, goes
    /// into Claude's prompt as an @-mention.
    pub(super) fn add_to_claude_context(&mut self, _: &AddToClaudeContext, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(server), true) = (self.claude.server.as_ref(), self.claude.clients > 0) else {
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

    /// The status bar's Claude Code item, while it's connected.
    pub(super) fn render_claude_status(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.claude_connected() {
            return None;
        }
        let light: Hsla = theme.success.into();
        let tip = match self.claude.clients {
            1 => "Claude Code is connected".to_string(),
            n => format!("{n} Claude Code sessions are connected"),
        };
        Some(
            super::status_bar::status_button("claude-status", theme)
                .gap_1p5()
                .hover(|s| s.text_color(theme.text_muted))
                .child(div().size(px(6.)).rounded_full().bg(light).shadow(crate::ui::glow(theme, light.alpha(0.6), 4., 0.)))
                .child("Claude")
                .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                .on_click(cx.listener(|ws, event: &ClickEvent, window, cx| ws.open_claude_menu(event.position(), window, cx)))
                .into_any_element(),
        )
    }

    fn open_claude_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let mut entries = vec![MenuEntry::Header("Claude Code".into())];
        let mut actions = vec![None];
        if self.active_editor(cx).is_some_and(|e| e.read(cx).doc.read(cx).path.is_some()) {
            entries.push(MenuEntry::text("Add to Claude Context").shortcut(binding_label(&AddToClaudeContext, window)));
            actions.push(Some(MenuAction::Run(Box::new(AddToClaudeContext))));
        }
        entries.push(MenuEntry::item("Settings…", Icon::Settings));
        actions.push(Some(MenuAction::Call(std::rc::Rc::new(|_, _, cx: &mut Context<Workspace>| {
            cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Editor), cx));
        }))));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// Called each render: when another tab became active, its editor's
    /// selection is watched (read after the frame: the root view mustn't
    /// come to depend on editors, which notify as one types).
    pub(super) fn note_claude_editor(&mut self, cx: &mut Context<Self>) {
        if self.claude.server.is_none() {
            return;
        }
        let item = self.active_item(cx).map(|item| item.entity_id());
        if item == self.claude.seen_item {
            return;
        }
        self.claude.seen_item = item;
        cx.spawn(async move |ws, cx| ws.update(cx, |ws, cx| ws.watch_claude_editor(cx)).ok()).detach();
    }

    fn watch_claude_editor(&mut self, cx: &mut Context<Self>) {
        let editor = self.active_editor(cx);
        self.claude.watching = editor.as_ref().map(|editor| cx.observe(editor, |ws, editor, cx| ws.claude_editor_changed(&editor, cx)));
        if let Some(editor) = editor {
            self.claude_editor_changed(&editor, cx);
        }
    }

    /// The watched editor redrew: when its selection or text changed and
    /// Claude is connected, the selection goes once it's been still a moment.
    fn claude_editor_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if !self.claude.server.as_ref().is_some_and(Server::has_clients) {
            return;
        }
        let e = editor.read(cx);
        if !e.is_loaded() || e.doc.read(cx).path.is_none() {
            return;
        }
        let key = (editor.entity_id(), e.doc.read(cx).version(), e.selections_generation());
        if self.claude.sent == Some(key) {
            return;
        }
        self.claude.sent = Some(key);
        let editor = editor.downgrade();
        self.claude.sending = Some(cx.spawn(async move |ws, cx| {
            cx.background_executor().timer(SELECTION_DELAY).await;
            ws.update(cx, |ws, cx| {
                let (Some(editor), Some(server)) = (editor.upgrade(), ws.claude.server.as_ref()) else { return };
                let e = editor.read(cx);
                let doc = e.doc.read(cx);
                if let Some(path) = &doc.path {
                    server.set_selection(ide::selection_payload(path, doc.rope(), e.primary_selection().range()));
                }
            })
            .ok();
        }));
    }
}
