//! The window's side of terminal agents' connections (`crate::agent_ide`):
//! what Claude Code's (`claude_ide.rs`) and Gemini CLI's (`gemini_ide.rs`)
//! share. Their servers run while a project is open, their setting is on
//! and the agent is installed (Gemini CLI's for local projects only; Claude
//! Code's for remote ones too, reached through the host, see
//! `workspace/claude_ide.rs`). Paths cross through [`Workspace::agent_map`].
//!
//! The active editor is watched (`note_agent_editor`): once its selection,
//! its text or the tabs have been still a moment, each connected agent is
//! told what it follows (Claude the selection, Gemini the open files).
//!
//! Edits an agent proposes open as diff tabs (`DiffSource::Proposed`) whose
//! [`Proposal`] waits in `AgentBridges::proposals` until the user decides.
//! Accept puts the text in the file's document as one undoable edit (its
//! text before kept in Local History) and saves it; then the agent hears it
//! was accepted, with the text, which it writes too (the same bytes).
//! Reject, or closing the tab, tells it the change was rejected.

use super::*;
use crate::agent_ide::{Agent, Request, ToolCall};
use crate::diff_view::DiffSource;
use futures::StreamExt as _;
use futures::channel::mpsc;
use serde_json::json;

/// How long the selection and the tabs stay put before agents hear of them.
const CONTEXT_DELAY: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(super) struct AgentBridges {
    pub(super) claude: super::claude_ide::ClaudeBridge,
    pub(super) gemini: super::gemini_ide::GeminiBridge,
    /// The pane item last seen active and how many tabs there were
    /// (`note_agent_editor`), and the observation of the active editor.
    seen: Option<(Option<EntityId>, usize)>,
    watching: Option<Subscription>,
    /// The editor, its document's version and its selections' generation
    /// last scheduled to be sent.
    sent: Option<(EntityId, u64, u64)>,
    sending: Option<Task<()>>,
    /// Proposed changes waiting for the user, by their diff tab.
    proposals: HashMap<EntityId, Proposal>,
    /// When each file was last the active tab (Unix milliseconds), which
    /// Gemini CLI orders its file list by.
    pub(super) focused: HashMap<PathBuf, u64>,
}

/// A proposed change waiting for Accept or Reject.
pub(super) enum Proposal {
    /// Claude Code's `openDiff` call, answered when the user decides.
    Claude { call: ToolCall, tab_name: String },
    /// Gemini CLI's `openDiff` (answered when the tab opened): the user's
    /// decision goes as a notification naming the file as Gemini did.
    Gemini { file_path: String },
}

impl Proposal {
    fn agent(&self) -> Agent {
        match self {
            Proposal::Claude { .. } => Agent::ClaudeCode,
            Proposal::Gemini { .. } => Agent::GeminiCli,
        }
    }

    /// The name its tab goes by: Claude's own, or the file Gemini named.
    fn tab_name(&self) -> &str {
        match self {
            Proposal::Claude { tab_name, .. } => tab_name,
            Proposal::Gemini { file_path } => file_path,
        }
    }

    /// The user rejected it (Reject, or closed its tab).
    pub(super) fn reject(self, bridges: &AgentBridges) {
        match self {
            Proposal::Claude { call, tab_name } => call.answer(Ok(vec!["DIFF_REJECTED".to_string(), tab_name])),
            Proposal::Gemini { file_path } => {
                if let Some(server) = &bridges.gemini.server {
                    server.broadcast("ide/diffRejected", json!({ "filePath": file_path }));
                }
            }
        }
    }

    /// The agent proposed another change under the same name. Claude's call
    /// is answered; Gemini's, whose answers go by file, is dropped: it waits
    /// for one change at a time, so a new one for the file means it's done
    /// with the old, and a rejection now would answer the new one.
    fn superseded(self, bridges: &AgentBridges) {
        if matches!(self, Proposal::Claude { .. }) {
            self.reject(bridges);
        }
    }

    /// The accepted change is in the file (`Ok`), or couldn't be put there.
    fn applied(self, bridges: &AgentBridges, applied: Result<(), String>, text: &str) {
        match self {
            Proposal::Claude { call, .. } => call.answer(applied.map(|()| vec!["FILE_SAVED".to_string(), text.to_string()])),
            Proposal::Gemini { file_path } => {
                let Some(server) = &bridges.gemini.server else { return };
                match applied {
                    Ok(()) => server.broadcast("ide/diffAccepted", json!({ "filePath": file_path, "content": text })),
                    Err(_) => server.broadcast("ide/diffRejected", json!({ "filePath": file_path })),
                }
            }
        }
    }
}

impl Workspace {
    /// Starts, updates or stops the agents' servers (after the project or its
    /// folders change, and when settings do).
    pub(super) fn sync_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_claude_ide(window, cx);
        self.sync_gemini_ide(window, cx);
    }

    /// Stops the agents' servers (the project closed).
    pub(super) fn stop_agents(&mut self) {
        self.agents = AgentBridges::default();
    }

    /// The project's folders when an agent's server should run here for a
    /// local project: a remote project's paths exist only through its
    /// connection, so an agent here couldn't read them and one there can't
    /// reach this port (Claude Code's is relayed by its host instead).
    pub(super) fn agent_folders(&self, wanted: bool) -> Option<Vec<PathBuf>> {
        self.folders
            .as_ref()
            .filter(|p| wanted && !cfg!(test) && kuana_core::remote::connection::host_of(p.primary()).is_none())
            .map(|p| p.folders().to_vec())
    }

    /// Hands a server's requests to `handle` while the window lives.
    pub(super) fn take_agent_requests(
        mut requests: mpsc::UnboundedReceiver<Request>,
        handle: fn(&mut Workspace, Request, &mut Window, &mut Context<Workspace>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |ws, cx| {
            while let Some(request) = requests.next().await {
                if ws.update_in(cx, |ws, window, cx| handle(ws, request, window, cx)).is_err() {
                    break;
                }
            }
        })
    }

    /// An agent connected: it hears what it follows.
    pub(super) fn agent_connected(&mut self, cx: &mut Context<Self>) {
        self.agents.sent = None;
        self.watch_agent_editor(cx);
        cx.notify();
    }

    /// Paths as the project's agents name them: a remote project's are its
    /// host's.
    pub(super) fn agent_map(&self) -> crate::agent_ide::PathMap {
        let mount = self.folders.as_ref().and_then(|p| kuana_core::remote::connection::host_of(p.primary())).map(|name| crate::fs::mount_root(&name));
        crate::agent_ide::PathMap::new(mount)
    }

    /// A path an agent names (a `file://` URL or a path, relative to the
    /// project's first folder; a remote project's host's), as the tabs have it.
    pub(super) fn agent_path(&self, given: &str) -> PathBuf {
        let path = self.agent_map().from_agent(given);
        let path = match &self.folders {
            Some(project) if path.is_relative() => project.primary().join(path),
            _ => path,
        };
        crate::paths::canonicalize(&path).unwrap_or(path)
    }

    /// Shows a proposed change in a diff tab, where it waits for the user. A
    /// proposal under the name of one shown replaces it. The proposal comes
    /// back when the tab couldn't open.
    pub(super) fn show_proposal(&mut self, path: PathBuf, tab_name: String, text: String, proposal: Proposal, window: &mut Window, cx: &mut Context<Self>) -> Result<(), Box<Proposal>> {
        let agent = proposal.agent();
        let source = DiffSource::Proposed { agent, new_file: !path.is_file_on_host(), path, tab_name: tab_name.clone(), text: text.into() };
        let replaced: Vec<EntityId> = self.agents.proposals.iter().filter(|(_, p)| p.agent() == agent && p.tab_name() == tab_name).map(|(id, _)| *id).collect();
        for id in replaced {
            if let Some(old) = self.agents.proposals.remove(&id) {
                old.superseded(&self.agents);
            }
        }
        match self.show_diff(source, window, cx) {
            Some(diff) => {
                self.agents.proposals.insert(diff.entity_id(), proposal);
                Ok(())
            }
            None => Err(Box::new(proposal)),
        }
    }

    /// Accept or Reject in a proposed change's tab. Accept applies it even
    /// when nobody waits any more (the agent ended meanwhile).
    pub(super) fn agent_proposal_decided(&mut self, diff: &Entity<DiffView>, accepted: bool, window: &mut Window, cx: &mut Context<Self>) {
        let proposal = self.agents.proposals.remove(&diff.entity_id());
        let DiffSource::Proposed { agent, path, text, new_file, .. } = diff.read(cx).source().clone() else { return };
        self.remove_item(&PaneItem::Diff(diff.clone()), window, cx);
        if !accepted {
            if let Some(proposal) = proposal {
                proposal.reject(&self.agents);
            }
            return;
        }
        // The agent hears "accepted" only once the file is saved: a failed
        // save is its error (the save's own prompt tells the user).
        let applying = self.apply_agent_change(agent, &path, &text, new_file, window, cx);
        cx.spawn(async move |ws, cx| {
            let applied = applying.await;
            if let Some(proposal) = proposal {
                ws.update(cx, |ws, _| proposal.applied(&ws.agents, applied, &text)).ok();
            }
        })
        .detach();
    }

    /// Puts a proposed text in its file: a new file is written; an existing
    /// one's document gets it as one undoable edit and is saved (without
    /// format on save, which would change what the agent then writes). The
    /// task ends when the file is written.
    fn apply_agent_change(&mut self, agent: Agent, path: &Path, text: &Arc<str>, new_file: bool, window: &mut Window, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        if new_file && !path.is_file_on_host() {
            let written = path.parent().map_or(Ok(()), crate::fs::create_dir_all).and_then(|()| crate::fs::write(path, text.as_bytes()));
            if let Err(err) = written {
                self.toast(ToastKind::Error, format!("Couldn't create the file {} proposed", agent.name()), Some(err.to_string()), cx);
                return Task::ready(Err(err.to_string()));
            }
            self.files_written([path.to_path_buf()], cx);
            self.open_path(path.to_path_buf(), window, cx);
            return Task::ready(Ok(()));
        }
        self.open_path(path.to_path_buf(), window, cx);
        let Some(editor) = self.tab_for(path, cx) else {
            let message = format!("Couldn't open {}", path.display());
            self.toast(ToastKind::Error, format!("Couldn't apply {}'s change", agent.name()), Some(message.clone()), cx);
            return Task::ready(Err(message));
        };
        let current = editor.read(cx).doc.read(cx).rope().clone();
        let kept = path.to_path_buf();
        cx.background_executor().spawn(async move { crate::local_history::record_rope(&kept, &current, crate::local_history::Kind::BeforeAgent) }).detach();
        editor.update(cx, |e, cx| e.replace_text(text, cx));
        self.save_editor_task(&editor, window, cx)
    }

    /// Closes the proposed changes' tabs `which` takes (given each one's
    /// file), without telling anyone; the proposals and the text each tab
    /// proposed.
    pub(super) fn take_proposals(&mut self, which: impl Fn(&Proposal, &Path) -> bool, window: &mut Window, cx: &mut Context<Self>) -> Vec<(Proposal, Option<Arc<str>>)> {
        let diffs = self.diff_views(cx);
        let source_of = |id: EntityId| diffs.iter().find(|d| d.entity_id() == id).map(|d| (d.clone(), d.read(cx).source().clone()));
        let chosen: Vec<(EntityId, Option<(Entity<DiffView>, DiffSource)>)> = self
            .agents
            .proposals
            .iter()
            .filter_map(|(id, proposal)| {
                let found = source_of(*id);
                let path = found.as_ref().and_then(|(_, source)| source.live_file()).map(Path::to_path_buf).unwrap_or_default();
                which(proposal, &path).then_some((*id, found))
            })
            .collect();
        let mut taken = Vec::new();
        for (id, found) in chosen {
            let Some(proposal) = self.agents.proposals.remove(&id) else { continue };
            let text = match found {
                Some((diff, DiffSource::Proposed { text, .. })) => {
                    self.remove_item(&PaneItem::Diff(diff), window, cx);
                    Some(text)
                }
                _ => None,
            };
            taken.push((proposal, text));
        }
        taken
    }

    /// A tab closed: a proposed change in it is rejected.
    pub(super) fn agent_tab_removed(&mut self, id: EntityId) {
        if let Some(proposal) = self.agents.proposals.remove(&id) {
            proposal.reject(&self.agents);
        }
    }

    /// How many sessions of an agent are connected to this window.
    fn agent_clients(&self, agent: Agent) -> usize {
        match agent {
            Agent::ClaudeCode if self.agents.claude.server.is_some() => self.agents.claude.clients,
            Agent::GeminiCli if self.agents.gemini.server.is_some() => self.agents.gemini.clients,
            _ => 0,
        }
    }

    /// The status bar's items for the agents connected now.
    pub(super) fn render_agent_status(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut items = Vec::new();
        for agent in [Agent::ClaudeCode, Agent::GeminiCli] {
            let clients = self.agent_clients(agent);
            if clients == 0 {
                continue;
            }
            let light: Hsla = theme.success.into();
            let tip = match clients {
                1 => format!("{} is connected", agent.name()),
                n => format!("{n} {} sessions are connected", agent.name()),
            };
            let id = match agent {
                Agent::ClaudeCode => "claude-status",
                Agent::GeminiCli => "gemini-status",
            };
            items.push(
                super::status_bar::status_button(id, theme)
                    .gap_1p5()
                    .hover(|s| s.text_color(theme.text_muted))
                    .child(div().size(px(6.)).rounded_full().bg(light).shadow(crate::ui::glow(theme, light.alpha(0.6), 4., 0.)))
                    .child(agent.short_name())
                    .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                    .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| ws.open_agent_menu(agent, event.position(), window, cx)))
                    .into_any_element(),
            );
        }
        items
    }

    fn open_agent_menu(&mut self, agent: Agent, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let mut entries = vec![MenuEntry::Header(agent.name().into())];
        let mut actions = vec![None];
        if agent == Agent::ClaudeCode && self.active_editor(cx).is_some_and(|e| e.read(cx).doc.read(cx).path.is_some()) {
            entries.push(MenuEntry::text("Add to Claude Context").shortcut(binding_label(&AddToClaudeContext, window)));
            actions.push(Some(MenuAction::Run(Box::new(AddToClaudeContext))));
        }
        entries.push(MenuEntry::item("Settings…", Icon::Settings));
        actions.push(Some(MenuAction::Call(std::rc::Rc::new(|_, _, cx: &mut Context<Workspace>| {
            cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Editor), cx));
        }))));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// Called each render: when another tab became active, or tabs opened or
    /// closed, the active editor is watched again and agents hear of it
    /// (read after the frame: the root view mustn't come to depend on
    /// editors, which notify as one types).
    pub(super) fn note_agent_editor(&mut self, cx: &mut Context<Self>) {
        if self.agents.claude.server.is_none() && self.agents.gemini.server.is_none() {
            return;
        }
        let seen = (self.active_item(cx).map(|item| item.entity_id()), self.editors.len());
        if Some(seen) == self.agents.seen {
            return;
        }
        self.agents.seen = Some(seen);
        cx.spawn(async move |ws, cx| ws.update(cx, |ws, cx| ws.watch_agent_editor(cx)).ok()).detach();
    }

    fn watch_agent_editor(&mut self, cx: &mut Context<Self>) {
        let editor = self.active_editor(cx);
        self.agents.watching = editor.as_ref().map(|editor| cx.observe(editor, |ws, editor, cx| ws.agent_editor_changed(&editor, cx)));
        if let Some(path) = editor.as_ref().and_then(|e| e.read(cx).doc.read(cx).path.clone()) {
            self.agents.focused.insert(path, unix_millis());
        }
        self.agents.sent = None;
        self.schedule_agent_context(cx);
    }

    /// The watched editor redrew: when its selection or text changed, the
    /// agents hear of it once it's been still a moment.
    fn agent_editor_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let e = editor.read(cx);
        if !e.is_loaded() || e.doc.read(cx).path.is_none() {
            return;
        }
        let key = (editor.entity_id(), e.doc.read(cx).version(), e.selections_generation());
        if self.agents.sent == Some(key) {
            return;
        }
        self.agents.sent = Some(key);
        self.schedule_agent_context(cx);
    }

    /// Sends what the connected agents follow after `CONTEXT_DELAY` (also
    /// when the project's trust changes, which Gemini CLI is told).
    pub(super) fn schedule_agent_context(&mut self, cx: &mut Context<Self>) {
        let connected = |server: bool, clients: usize| server && clients > 0;
        if !connected(self.agents.claude.server.is_some(), self.agents.claude.clients) && !connected(self.agents.gemini.server.is_some(), self.agents.gemini.clients) {
            return;
        }
        self.agents.sending = Some(cx.spawn(async move |ws, cx| {
            cx.background_executor().timer(CONTEXT_DELAY).await;
            ws.update(cx, |ws, cx| {
                ws.send_claude_selection(cx);
                ws.send_gemini_context(cx);
            })
            .ok();
        }));
    }
}

/// Now, in Unix milliseconds.
fn unix_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
