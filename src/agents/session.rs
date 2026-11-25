//! One agent session: the connection to its agent (started on the first
//! prompt, or when reopened), its transcript, the turn in progress, what the
//! agent asks (permissions, files, terminals) and the files it changed.
//!
//! Requests that need the workspace (reading and writing files through open
//! documents, Kuana's problems) go out as [`SessionEvent`]s with an id; the
//! workspace answers through [`Session::answer`]. Terminals are the
//! session's own ([`super::terminals`]).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use futures::StreamExt as _;
use gpui::{Context, EventEmitter, Task};
use serde_json::{Value, json};

pub use super::checkpoints::Before;
use super::{
    checkpoints::Checkpoints,
    history, kuana_agent,
    terminals::Terminals,
    transcript::{Entry, Kind, Level, Permission, PermissionOption, Tool, ToolContent, Transcript},
};
use crate::acp::{self, Agent, FromAgent, Responder, RpcError, schema};

/// How to start a session's agent.
#[derive(Clone)]
pub enum Launcher {
    Process(acp::Launch),
    /// Kuana's own agent, in-process.
    Kuana(kuana_agent::Chooser),
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Not started yet, or waiting for the next prompt.
    Idle,
    Connecting,
    Running,
    /// The agent asked for permission.
    Waiting,
    /// The agent needs the user to sign in first.
    NeedsAuth,
    Failed(String),
}

impl State {
    pub fn busy(&self) -> bool {
        matches!(self, State::Connecting | State::Running | State::Waiting)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// A session option the agent offers (its model, effort…).
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// ACP's: `mode`, `model`, `thought_level`, or the agent's own.
    pub category: Option<String>,
    pub current: Value,
    /// Empty for a yes/no option.
    pub choices: Vec<Choice>,
}

/// One value a session option can take.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

impl ConfigOption {
    /// The current value's name (its value when it isn't listed).
    pub fn current_name(&self) -> String {
        let current = self.current.as_str().unwrap_or_default();
        self.choices.iter().find(|c| c.value == current).map_or_else(|| current.to_string(), |c| c.name.clone())
    }

    /// For an option that's on or off (its choices are those two), whether
    /// it's on.
    pub fn switch(&self) -> Option<bool> {
        let is = |c: &Choice, word: &str| c.value.eq_ignore_ascii_case(word) || c.name.eq_ignore_ascii_case(word);
        let on = self.choices.iter().find(|c| is(c, "on"))?;
        self.choices.iter().find(|c| is(c, "off"))?;
        (self.choices.len() == 2).then(|| self.current.as_str() == Some(on.value.as_str()))
    }

    /// The value that turns a `switch` on or off.
    pub fn switch_value(&self, on: bool) -> Option<String> {
        let word = if on { "on" } else { "off" };
        self.choices.iter().find(|c| c.value.eq_ignore_ascii_case(word) || c.name.eq_ignore_ascii_case(word)).map(|c| c.value.clone())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// A terminal sign-in: what to run (the agent's program with these).
    pub terminal: Option<(Vec<String>, Vec<(String, String)>)>,
}

/// Something given with a prompt.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextItem {
    /// A whole file, its text taken now (unsaved edits included).
    File { path: PathBuf, text: String },
    /// Lines of a file (1-based, inclusive).
    Selection { path: PathBuf, lines: (usize, usize), text: String },
    /// Anything else Kuana gathered (the problems, a Git diff, a run's output).
    Text { label: String, text: String },
    /// A picture (pasted, or a file): its name, MIME type and bytes.
    Image { name: String, mime: String, data: Arc<[u8]> },
}

/// How long after a transcript change it's saved (`save_soon`).
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

/// The largest picture sent (providers refuse bigger ones).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The MIME type of a picture models take, by its file suffix.
pub fn image_mime(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_string_lossy().to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

impl ContextItem {
    pub fn label(&self) -> String {
        let name = |p: &PathBuf| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        match self {
            ContextItem::File { path, .. } => name(path),
            ContextItem::Selection { path, lines: (a, b), .. } if a == b => format!("{}:{a}", name(path)),
            ContextItem::Selection { path, lines: (a, b), .. } => format!("{}:{a}-{b}", name(path)),
            ContextItem::Text { label, .. } | ContextItem::Image { name: label, .. } => label.clone(),
        }
    }

    /// As a prompt's content block: embedded (`resource`) where the agent
    /// takes files, else text; a picture as an image where it takes those.
    fn block(&self, embedded: bool, images: bool) -> Value {
        if let ContextItem::Image { name, mime, data } = self {
            return if images {
                json!({ "type": "image", "mimeType": mime, "data": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data) })
            } else {
                json!({ "type": "text", "text": format!("[The user attached a picture, {name}, but this agent doesn't take pictures.]") })
            };
        }
        let (uri, text) = match self {
            ContextItem::File { path, text } => (format!("file://{}", path.display()), text.clone()),
            ContextItem::Selection { path, lines: (a, b), text } => (format!("file://{}#L{a}-L{b}", path.display()), text.clone()),
            ContextItem::Text { label, text } => (format!("kuana:{}", label.replace(' ', "-").to_lowercase()), text.clone()),
            ContextItem::Image { .. } => unreachable!("answered above"),
        };
        if embedded {
            json!({ "type": "resource", "resource": { "uri": uri, "text": text, "mimeType": "text/plain" } })
        } else {
            json!({ "type": "text", "text": format!("<context source=\"{}\">\n{text}\n</context>", uri) })
        }
    }
}

/// What the workspace hears.
pub enum SessionEvent {
    /// The agent asked for permission (the user should know if they're elsewhere).
    NeedsYou,
    /// A turn ended.
    Finished,
    TitleChanged,
    /// Answer through `Session::answer` with `{ "content" }`.
    ReadFile { id: u64, path: PathBuf, line: Option<u32>, limit: Option<u32> },
    /// Answer with `{}` once written.
    WriteFile { id: u64, path: PathBuf, content: String },
    /// A tool call is about to change `path` (its first in this turn): what
    /// the file is now goes to `note_change`. Agents that write the disk
    /// themselves (Claude's) never send `WriteFile`. `old`/`new`: the
    /// call's diff, when it has one (`old` None: a new file), to tell a
    /// change that already landed.
    WillEdit { path: PathBuf, old: Option<String>, new: Option<String> },
    /// A `_kuana/…` request.
    Ext { id: u64, method: String, params: Value },
    /// A terminal sign-in to run for the user.
    RunSignIn { program: PathBuf, args: Vec<String>, env: Vec<(String, String)>, cwd: PathBuf },
}

pub struct Session {
    pub id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub cwd: PathBuf,
    pub title: String,
    pub created: u64,
    pub updated: u64,
    pub transcript: Transcript,
    pub state: State,
    pub modes: Vec<Mode>,
    pub mode: Option<String>,
    pub options: Vec<ConfigOption>,
    /// The agent's `/` commands: `(name, description)`.
    pub commands: Vec<(String, String)>,
    /// Tokens used of the context window.
    pub usage: Option<(u64, u64)>,
    pub auth_methods: Vec<AuthMethod>,
    /// What the agent changed, turn by turn (`note_change`, `changes`).
    pub checkpoints: Checkpoints,
    /// The Git worktree of its own it works in (`worktree.rs`), if any.
    pub worktree: Option<super::worktree::Worktree>,
    pub terminals: Terminals,
    /// Prompts sent while a turn ran, sent after it.
    pub queued: Vec<(String, Vec<ContextItem>)>,
    launcher: Launcher,
    agent: Option<Arc<Agent>>,
    acp_session: Option<String>,
    embedded_context: bool,
    /// Whether the agent takes pictures in a prompt.
    takes_images: bool,
    /// While `session/load` replays the conversation, which the saved
    /// transcript already holds.
    replaying: bool,
    turn: usize,
    /// A prompt sent before the agent was ready: made into content blocks
    /// once it's connected, when it's known whether it takes embedded files.
    pending_prompt: Option<(String, Vec<ContextItem>)>,
    /// The turn an edited message goes back to, until its prompt is sent.
    pending_rewind: Option<usize>,
    /// The MCP servers the agent was given (ACP's `mcpServers`).
    mcp_servers: Vec<Value>,
    permissions: HashMap<String, Responder>,
    requests: HashMap<u64, Responder>,
    next_request: u64,
    pump: Option<Task<()>>,
    turn_task: Option<Task<()>>,
    connecting: Option<Task<()>>,
    ticker: Option<Task<()>>,
    /// A save on its way (`save_soon`).
    saving: Option<Task<()>>,
}

impl EventEmitter<SessionEvent> for Session {}

impl Session {
    pub fn new(agent_id: String, agent_name: String, cwd: PathBuf, launcher: Launcher) -> Session {
        let now = history::now();
        let id = format!("{}-{:x}", now, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()));
        Session::with(id, agent_id, agent_name, cwd, launcher, Transcript::default(), None, "New session".into(), now, now)
    }

    /// A saved session, connected again on its next prompt.
    pub fn restored(saved: history::Saved, launcher: Launcher) -> Session {
        let s = saved.summary;
        let mut session = Session::with(s.id, s.agent, s.agent_name, s.cwd, launcher, saved.transcript, saved.acp_session, s.title, s.created, s.updated);
        session.checkpoints = saved.checkpoints;
        session.worktree = s.worktree;
        session
    }

    #[allow(clippy::too_many_arguments)]
    fn with(
        id: String,
        agent_id: String,
        agent_name: String,
        cwd: PathBuf,
        launcher: Launcher,
        transcript: Transcript,
        acp_session: Option<String>,
        title: String,
        created: u64,
        updated: u64,
    ) -> Session {
        let user_messages = transcript.entries.iter().filter(|e| matches!(e, Entry::User { .. })).count();
        Session {
            id,
            agent_id,
            agent_name,
            cwd,
            title,
            created,
            updated,
            transcript,
            state: State::Idle,
            modes: Vec::new(),
            mode: None,
            options: Vec::new(),
            commands: Vec::new(),
            usage: None,
            auth_methods: Vec::new(),
            checkpoints: Checkpoints::default(),
            worktree: None,
            terminals: Terminals::default(),
            queued: Vec::new(),
            launcher,
            agent: None,
            acp_session,
            embedded_context: false,
            takes_images: false,
            replaying: false,
            // Turns go on from the saved transcript's (`turn_of` counts them).
            turn: user_messages,
            pending_prompt: None,
            mcp_servers: Vec::new(),
            pending_rewind: None,
            permissions: HashMap::new(),
            requests: HashMap::new(),
            next_request: 0,
            pump: None,
            turn_task: None,
            connecting: None,
            ticker: None,
            saving: None,
        }
    }

    pub fn summary(&self) -> history::Summary {
        history::Summary {
            id: self.id.clone(),
            agent: self.agent_id.clone(),
            agent_name: self.agent_name.clone(),
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            created: self.created,
            updated: self.updated,
            worktree: self.worktree.clone(),
        }
    }

    /// The project folder the session belongs to: its worktree's, or where
    /// it works (it's listed there; prompts sent are kept by it).
    pub fn folder(&self) -> &Path {
        self.worktree.as_ref().map_or(&self.cwd, |w| &w.project)
    }

    /// Its worktree was removed: its agent stops, and the session stays to
    /// read (`connect` says why it can't go on).
    pub fn worktree_removed(&mut self, cx: &mut Context<Self>) {
        let Some(wt) = &mut self.worktree else { return };
        wt.removed = true;
        let branch = wt.branch.clone();
        self.shut_down(false);
        // Its changes went with the worktree (or into a merge): nothing to
        // review or put back, and no agent to pick a mode or model for.
        self.checkpoints.clear();
        self.modes.clear();
        self.mode = None;
        self.options.clear();
        self.agent = None;
        self.transcript.notice(Level::Info, format!("Its worktree was removed ({branch}). Start a new session to go on."));
        self.save(cx);
        cx.notify();
    }

    /// Works somewhere else from now on (its own worktree, or the project
    /// folder again), before anything was sent: the agent starts over there.
    pub fn move_to(&mut self, cwd: PathBuf, worktree: Option<super::worktree::Worktree>, launcher: Launcher, cx: &mut Context<Self>) {
        if !self.transcript.entries.is_empty() {
            return;
        }
        self.shut_down(false);
        self.agent = None;
        self.acp_session = None;
        self.modes.clear();
        self.mode = None;
        self.options.clear();
        self.commands.clear();
        self.usage = None;
        self.auth_methods.clear();
        self.state = State::Idle;
        self.cwd = cwd;
        self.worktree = worktree;
        self.launcher = launcher;
        self.start(cx);
        cx.notify();
    }

    fn saved(&self) -> Option<history::Saved> {
        if self.transcript.entries.is_empty() {
            return None;
        }
        Some(history::Saved { summary: self.summary(), acp_session: self.acp_session.clone(), transcript: self.transcript.clone(), checkpoints: self.checkpoints.clone() })
    }

    /// Saves the transcript right away (Kuana is quitting), its unfinished
    /// tool calls settled as they'd be read back.
    pub fn save_now(&self) {
        if let Some(mut saved) = self.saved() {
            saved.transcript.settle();
            if let Err(err) = history::save(&saved) {
                eprintln!("couldn't save the agent session: {err}");
            }
        }
    }

    /// Saves the session and stops its agent and terminals (Kuana quits).
    /// (`save`: saved first; a deleted session isn't.)
    pub fn shut_down(&mut self, save: bool) {
        // A save on its way would write a deleted session back.
        self.saving = None;
        if save {
            self.save_now();
        }
        if let (Some(agent), Some(id)) = (&self.agent, &self.acp_session) {
            agent.cancel(id);
        }
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        self.terminals.kill_all();
        self.pump = None;
        self.connecting = None;
        self.turn_task = None;
        self.pending_prompt = None;
        self.pending_rewind = None;
        self.queued.clear();
        // Dropped unanswered, they answer the agent with an error.
        self.permissions.clear();
        self.requests.clear();
        if self.state.busy() {
            self.transcript.settle();
            self.state = State::Idle;
        }
    }

    /// Saves `SAVE_DELAY` after the transcript changes, one save for the
    /// changes meanwhile: a turn's progress isn't lost if Kuana is killed
    /// before the turn ends (the end saves too).
    fn save_soon(&mut self, cx: &mut Context<Self>) {
        if self.saving.is_some() {
            return;
        }
        self.saving = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |s, cx| {
                s.saving = None;
                s.save(cx);
            })
            .ok();
        }));
    }

    /// Saves the transcript (in the background).
    pub fn save(&self, cx: &mut Context<Self>) {
        let Some(saved) = self.saved() else { return };
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = history::save(&saved) {
                    eprintln!("couldn't save the agent session: {err}");
                }
            })
            .detach();
    }

    /// The agent's own id for the session, once it has one.
    pub fn acp_session(&self) -> Option<&str> {
        self.acp_session.as_deref()
    }

    pub fn is_connected(&self) -> bool {
        self.agent.as_ref().is_some_and(|a| !a.is_closed())
    }

    /// Whether a turn runs, or a prompt waits for the agent to start: what
    /// Stop stops. The agent starting with nothing to send (`start`) isn't.
    pub fn working(&self) -> bool {
        match self.state {
            State::Running | State::Waiting => true,
            State::Connecting => self.pending_prompt.is_some(),
            _ => false,
        }
    }

    /// The option that picks the permission mode: one of category `mode`
    /// (ACP's successor to modes, which agents send both of for a while).
    pub fn mode_option(&self) -> Option<&ConfigOption> {
        self.options.iter().find(|o| o.category.as_deref() == Some("mode") && !o.choices.is_empty())
    }

    /// The current permission mode's name, from the mode option or the modes.
    pub fn mode_name(&self) -> Option<String> {
        match self.mode_option() {
            Some(option) => Some(option.current_name()),
            None => self.mode.as_ref().and_then(|m| self.modes.iter().find(|x| &x.id == m)).map(|m| m.name.clone()),
        }
    }

    // ---- Connecting -------------------------------------------------------------

    /// Starts the agent before the first prompt (a new session; a restored
    /// one once something's typed), so what it offers shows while the prompt
    /// is written: its modes and models, its `/` commands, a sign-in.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.agent.is_none() && self.state == State::Idle {
            self.connect(cx);
        }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, State::Connecting) {
            return;
        }
        if self.worktree.as_ref().is_some_and(|w| w.removed || !w.path.exists()) {
            self.fail("This session's worktree was removed. Start a new session to go on.".into(), cx);
            return;
        }
        self.state = State::Connecting;
        let launcher = self.launcher.clone();
        let (cwd, previous) = (self.cwd.clone(), self.acp_session.clone());
        // The MCP servers as the settings have them now.
        self.mcp_servers = super::mcp::acp_servers(&crate::settings::Settings::get(cx).ai.clone().unwrap_or_default().mcp_servers);
        self.connecting = Some(cx.spawn(async move |this, cx| {
            let connected = match launcher {
                Launcher::Process(launch) => cx.background_executor().spawn(async move { Agent::spawn(&launch).await }).await,
                Launcher::Kuana(choose) => match kuana_agent::start(choose) {
                    Ok((read, write)) => Agent::connect(read, write).await,
                    Err(err) => Err(format!("Couldn't start Kuana's agent: {err}")),
                },
            };
            let (agent, events) = match connected {
                Ok(connected) => connected,
                Err(error) => {
                    this.update(cx, |s, cx| s.fail(error, cx)).ok();
                    return;
                }
            };
            let agent = Arc::new(agent);
            let can_load = agent.init.agent_capabilities.load_session;
            let embedded = agent.init.agent_capabilities.prompt_capabilities.embedded_context;
            let images = agent.init.agent_capabilities.prompt_capabilities.image;
            let auth: Vec<AuthMethod> = agent.init.auth_methods.iter().filter_map(|m| auth_method(serde_json::to_value(m).ok()?)).collect();
            let pump = cx.spawn({
                let (this, gone) = (this.clone(), Arc::downgrade(&agent));
                async move |cx| {
                    let mut events = events;
                    while let Some(event) = events.next().await {
                        if this.update(cx, |s, cx| s.on_agent(event, cx)).is_err() {
                            return;
                        }
                    }
                    this.update(cx, |s, cx| s.disconnected(&gone, cx)).ok();
                }
            });
            this.update(cx, |s, _| {
                s.agent = Some(agent.clone());
                s.embedded_context = embedded;
                s.takes_images = images;
                s.auth_methods = auth;
                s.pump = Some(pump);
            })
            .ok();
            this.update(cx, |s, cx| s.open_session(previous.filter(|_| can_load), cwd, cx)).ok();
        }));
        cx.notify();
    }

    /// Opens the agent's side: loads the earlier session when it can, else a new one.
    fn open_session(&mut self, previous: Option<String>, cwd: PathBuf, cx: &mut Context<Self>) {
        let Some(agent) = self.agent.clone() else {
            self.fail(format!("{} stopped while starting.", self.agent_name), cx);
            return;
        };
        let mcp = self.mcp_servers.clone();
        let loading = previous.is_some();
        self.replaying = loading;
        self.connecting = Some(cx.spawn(async move |this, cx| {
            let opened = match &previous {
                Some(id) => agent.load_session(id, &cwd, mcp).await.map(|r| (id.clone(), r.modes, r.config_options)),
                None => agent.new_session(&cwd, mcp).await.map(|r| (r.session_id.to_string(), r.modes, r.config_options)),
            };
            this.update(cx, |s, cx| {
                s.replaying = false;
                match opened {
                    Ok((id, modes, options)) => {
                        if !loading && !s.transcript.entries.is_empty() && s.acp_session.is_some() {
                            s.transcript.notice(Level::Info, format!("{} can't continue the earlier conversation; it starts afresh.", s.agent_name));
                        }
                        s.acp_session = Some(id);
                        s.set_modes(modes);
                        if let Some(options) = options {
                            s.set_options(&options);
                        }
                        s.state = State::Idle;
                        if let Some((text, context)) = s.pending_prompt.take() {
                            let blocks = s.blocks(&text, &context);
                            let rewind = s.pending_rewind.take();
                            s.run_prompt(rewind, blocks, cx);
                        }
                    }
                    Err(e) if e.code == RpcError::AUTH_REQUIRED => {
                        s.state = State::NeedsAuth;
                        s.transcript.notice(Level::Warning, format!("Sign in to {} to start.", s.agent_name));
                    }
                    // The agent no longer has it: a new one (with a notice
                    // that the agent forgot the conversation).
                    Err(e) if loading && e.code != RpcError::CLOSED && s.agent.is_some() => s.open_session(None, cwd, cx),
                    Err(e) => s.fail(e.message, cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        self.state = State::Failed(error.clone());
        self.pending_prompt = None;
        self.pending_rewind = None;
        self.transcript.notice(Level::Error, error);
        self.drop_queued();
        self.agent = None;
        cx.emit(SessionEvent::Finished);
        cx.notify();
    }

    /// Messages waiting for a turn that won't come: said in the transcript,
    /// not left waiting.
    fn drop_queued(&mut self) {
        if !self.queued.is_empty() {
            let n = self.queued.len();
            self.queued.clear();
            self.transcript.notice(Level::Warning, format!("{n} queued message{} weren't sent.", if n == 1 { "" } else { "s" }));
        }
    }

    /// An agent's events ended. Only the session's current agent counts: an
    /// old one's end, after a new one was started, changes nothing.
    fn disconnected(&mut self, gone: &std::sync::Weak<Agent>, cx: &mut Context<Self>) {
        let current = matches!((&self.agent, gone.upgrade()), (Some(agent), Some(gone)) if Arc::ptr_eq(agent, &gone));
        if !current {
            return;
        }
        let tail = self.agent.as_ref().map(|a| a.stderr_tail()).unwrap_or_default();
        self.agent = None;
        self.pump = None;
        // A connection being opened ends with its own error.
        if self.state == State::Connecting {
            cx.notify();
            return;
        }
        for (_, responder) in self.permissions.drain() {
            drop(responder);
        }
        if self.state.busy() {
            let mut text = format!("{} stopped.", self.agent_name);
            if let Some(last) = tail.lines().rev().find(|l| !l.trim().is_empty()) {
                text.push_str(&format!(" {last}"));
            }
            self.transcript.settle();
            self.transcript.notice(Level::Error, text);
            self.state = State::Idle;
            self.drop_queued();
            cx.emit(SessionEvent::Finished);
        }
        cx.notify();
    }

    /// Signs in with one of the agent's methods, then opens the session.
    pub fn sign_in(&mut self, method: &str, cx: &mut Context<Self>) {
        let Some(chosen) = self.auth_methods.iter().find(|m| m.id == method).cloned() else { return };
        if let Some((args, env)) = &chosen.terminal {
            if let Launcher::Process(launch) = &self.launcher {
                let mut all = launch.args.clone();
                all.extend(args.iter().cloned());
                let mut all_env = launch.env.clone();
                all_env.extend(env.iter().cloned());
                cx.emit(SessionEvent::RunSignIn { program: launch.program.clone(), args: all, env: all_env, cwd: self.cwd.clone() });
            }
            return;
        }
        let Some(agent) = self.agent.clone() else { return };
        let cwd = self.cwd.clone();
        self.state = State::Connecting;
        self.connecting = Some(cx.spawn(async move |this, cx| {
            let result = agent.authenticate(&chosen.id).await;
            this.update(cx, |s, cx| match result {
                Ok(()) => s.open_session(None, cwd, cx),
                Err(e) => {
                    s.state = State::NeedsAuth;
                    s.transcript.notice(Level::Error, format!("Sign-in failed: {}", e.message));
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// After a terminal sign-in: tries the session again.
    pub fn signed_in(&mut self, cx: &mut Context<Self>) {
        match &self.agent {
            Some(_) => {
                let cwd = self.cwd.clone();
                self.state = State::Connecting;
                self.open_session(None, cwd, cx);
            }
            None => {
                self.state = State::Idle;
                self.connect(cx);
            }
        }
        cx.notify();
    }

    fn set_modes(&mut self, modes: Option<schema::SessionModeState>) {
        if let Some(modes) = modes {
            self.mode = Some(modes.current_mode_id.to_string());
            self.modes = modes.available_modes.iter().map(|m| Mode { id: m.id.to_string(), name: m.name.clone(), description: m.description.clone() }).collect();
        }
    }

    fn set_options(&mut self, options: &[schema::SessionConfigOption]) {
        self.options = options.iter().filter_map(|o| config_option(serde_json::to_value(o).ok()?)).collect();
    }

    // ---- Prompts ------------------------------------------------------------------

    /// Sends a prompt (queued while a turn runs).
    pub fn send(&mut self, text: String, context: Vec<ContextItem>, cx: &mut Context<Self>) {
        if text.trim().is_empty() && context.is_empty() {
            return;
        }
        // While the agent starts with no prompt waiting (`start`), this one
        // waits for it; otherwise it waits for the turn to end.
        let starting = self.state == State::Connecting && self.pending_prompt.is_none();
        if self.state.busy() && !starting {
            self.queued.push((text, context));
            cx.notify();
            return;
        }
        if self.transcript.first_prompt().is_none() && self.title == "New session" {
            self.title = history::title_from(&text);
            cx.emit(SessionEvent::TitleChanged);
        }
        self.transcript.push(Entry::User { text: text.clone(), context: context.iter().map(ContextItem::label).collect() });
        self.turn += 1;
        self.updated = history::now();
        // Saved now too, so a turn cut short (a quit, a crash) keeps the prompt.
        self.save(cx);
        if !starting && self.is_connected() && self.acp_session.is_some() {
            let blocks = self.blocks(&text, &context);
            let rewind = self.pending_rewind.take();
            self.run_prompt(rewind, blocks, cx);
        } else {
            self.pending_prompt = Some((text, context));
            if !matches!(self.state, State::NeedsAuth) {
                self.connect(cx);
            }
        }
        cx.notify();
    }

    /// A prompt's content blocks: its text, then its context (embedded files
    /// where the agent takes them, else text).
    fn blocks(&self, text: &str, context: &[ContextItem]) -> Vec<Value> {
        let mut blocks = vec![json!({ "type": "text", "text": text })];
        blocks.extend(context.iter().map(|c| c.block(self.embedded_context, self.takes_images)));
        blocks
    }

    /// Sends a prompt; after the agent went back to before turn `rewind`
    /// when there's one (an edited message). If it can't go back, the prompt
    /// goes on the conversation as it is, and the transcript says so.
    fn run_prompt(&mut self, rewind: Option<usize>, blocks: Vec<Value>, cx: &mut Context<Self>) {
        let (Some(agent), Some(id)) = (self.agent.clone(), self.acp_session.clone()) else { return };
        self.state = State::Running;
        self.turn_task = Some(cx.spawn(async move |this, cx| {
            if let Some(turn) = rewind
                && let Err(error) = agent.ext::<Value>("_kuana/rewind", json!({ "sessionId": id, "turn": turn })).await
            {
                this.update(cx, |s, cx| {
                    s.transcript.notice(Level::Warning, format!("{} couldn't go back to that message ({}); it continues the conversation as it was.", s.agent_name, error.message));
                    cx.notify();
                })
                .ok();
            }
            let result = agent.prompt(&id, blocks).await;
            this.update(cx, |s, cx| s.turn_ended(result, cx)).ok();
        }));
        cx.notify();
    }

    /// Whether the agent can go back in the conversation: Kuana's own (the
    /// others have no way to in ACP).
    pub fn can_rewind(&self) -> bool {
        matches!(self.launcher, Launcher::Kuana(_))
    }

    /// The transcript's index of the user's `turn`-th message (1-based).
    fn user_entry(&self, turn: usize) -> Option<usize> {
        self.transcript.entries.iter().enumerate().filter(|(_, e)| matches!(e, Entry::User { .. })).nth(turn.checked_sub(1)?).map(|(ix, _)| ix)
    }

    /// An earlier message, edited: the conversation goes back to before it
    /// (the transcript at once, the agent with the prompt) and the new text
    /// is sent in its place. `files_put_back`: the files the agent changed
    /// since were put back (else they stay as they are, and count as the
    /// turn before's).
    pub fn edit_and_send(&mut self, turn: usize, text: String, context: Vec<ContextItem>, files_put_back: bool, cx: &mut Context<Self>) {
        if self.working() || !self.can_rewind() {
            return;
        }
        let Some(at) = self.user_entry(turn) else { return };
        self.transcript.truncate(at);
        if files_put_back {
            self.checkpoints.forget_from(turn);
        } else {
            self.checkpoints.fold_from(turn);
        }
        self.turn = turn - 1;
        self.pending_rewind = Some(turn);
        self.send(text, context, cx);
    }

    /// Retry: the last answer goes and the agent answers the last message
    /// again.
    pub fn retry_answer(&mut self, cx: &mut Context<Self>) {
        let (Some(agent), Some(id)) = (self.agent.clone(), self.acp_session.clone()) else { return };
        if self.state.busy() || !self.can_rewind() || !self.is_connected() {
            return;
        }
        let turn = self.turn;
        let Some(at) = self.user_entry(turn) else { return };
        self.transcript.truncate(at + 1);
        self.state = State::Running;
        self.updated = history::now();
        self.turn_task = Some(cx.spawn(async move |this, cx| {
            let result = agent.ext::<schema::PromptResponse>("_kuana/retry", json!({ "sessionId": id, "turn": turn })).await;
            this.update(cx, |s, cx| s.turn_ended(result, cx)).ok();
        }));
        cx.notify();
    }

    fn turn_ended(&mut self, result: Result<schema::PromptResponse, RpcError>, cx: &mut Context<Self>) {
        self.turn_task = None;
        match result {
            Ok(response) => match response.stop_reason {
                schema::StopReason::Cancelled => {
                    self.transcript.settle();
                    self.transcript.notice(Level::Info, "Stopped.");
                }
                schema::StopReason::MaxTokens => self.transcript.notice(Level::Warning, "The answer reached the model's output limit."),
                schema::StopReason::MaxTurnRequests => self.transcript.notice(Level::Warning, "The agent took as many steps as one turn allows."),
                schema::StopReason::Refusal => self.transcript.notice(Level::Warning, "The agent declined to go on."),
                _ => {}
            },
            Err(e) if e.code == RpcError::AUTH_REQUIRED => {
                self.state = State::NeedsAuth;
                self.transcript.notice(Level::Warning, format!("Sign in to {} to go on.", self.agent_name));
            }
            // The agent died: the next prompt starts another (its pump's end,
            // coming after, is the old agent's and changes nothing).
            Err(e) if e.code == RpcError::CLOSED => {
                let tail = self.agent.take().map(|a| a.stderr_tail()).unwrap_or_default();
                self.pump = None;
                let mut text = format!("{} stopped.", self.agent_name);
                if let Some(last) = tail.lines().rev().find(|l| !l.trim().is_empty()) {
                    text.push_str(&format!(" {last}"));
                }
                self.transcript.settle();
                self.transcript.notice(Level::Error, text);
            }
            Err(e) => {
                self.transcript.settle();
                self.transcript.notice(Level::Error, e.message);
            }
        }
        if self.state.busy() {
            self.state = State::Idle;
        }
        self.updated = history::now();
        self.save(cx);
        cx.emit(SessionEvent::Finished);
        if !self.queued.is_empty() && self.state == State::Idle {
            let (text, context) = self.queued.remove(0);
            self.send(text, context, cx);
        }
        cx.notify();
    }

    /// Stops the turn: the agent is told, open questions are answered "cancelled".
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        // Starting with nothing to send: nothing to stop.
        if self.state == State::Connecting && self.pending_prompt.is_none() {
            return;
        }
        if let (Some(agent), Some(id)) = (&self.agent, &self.acp_session) {
            agent.cancel(id);
        }
        for (_, responder) in self.permissions.drain() {
            responder.ok(json!({ "outcome": { "outcome": "cancelled" } }));
        }
        self.queued.clear();
        if matches!(self.state, State::Connecting) {
            // The agent may be up but the session not opened on it: it starts
            // over on the next prompt.
            self.connecting = None;
            self.pending_prompt = None;
            self.pending_rewind = None;
            self.agent = None;
            self.pump = None;
            self.replaying = false;
            self.state = State::Idle;
            self.transcript.notice(Level::Info, "Stopped.");
        }
        self.transcript.settle();
        cx.notify();
    }

    pub fn answer_permission(&mut self, tool_id: &str, option: &str, cx: &mut Context<Self>) {
        let Some(responder) = self.permissions.remove(tool_id) else { return };
        responder.ok(json!({ "outcome": { "outcome": "selected", "optionId": option } }));
        if let Some(tool) = self.transcript.tool_mut(tool_id)
            && let Some(permission) = &mut tool.permission
        {
            permission.chosen = Some(option.to_string());
            permission.open = false;
        }
        self.transcript.generation += 1;
        if self.permissions.is_empty() && self.state == State::Waiting {
            self.state = State::Running;
        }
        cx.notify();
    }

    pub fn set_mode(&mut self, mode: &str, cx: &mut Context<Self>) {
        let (Some(agent), Some(id)) = (self.agent.clone(), self.acp_session.clone()) else { return };
        self.mode = Some(mode.to_string());
        let mode = mode.to_string();
        cx.spawn(async move |this, cx| {
            if let Err(e) = agent.set_mode(&id, &mode).await {
                this.update(cx, |s, cx| {
                    s.transcript.notice(Level::Error, format!("Couldn't switch mode: {}", e.message));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    pub fn set_option(&mut self, option: &str, value: Value, cx: &mut Context<Self>) {
        let (Some(agent), Some(id)) = (self.agent.clone(), self.acp_session.clone()) else { return };
        if let Some(o) = self.options.iter_mut().find(|o| o.id == option) {
            o.current = value.clone();
        }
        let option = option.to_string();
        cx.spawn(async move |this, cx| {
            let result = agent.set_config_option(&id, &option, value).await;
            this.update(cx, |s, cx| {
                match result {
                    Ok(response) => s.set_options(&response.config_options),
                    Err(e) => s.transcript.notice(Level::Error, format!("Couldn't change {option}: {}", e.message)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Answers a request the session handed out as an event.
    pub fn answer(&mut self, id: u64, result: Result<Value, RpcError>) {
        if let Some(responder) = self.requests.remove(&id) {
            match result {
                Ok(value) => responder.ok(value),
                Err(error) => responder.err(error),
            }
        }
    }

    /// Notes a file the agent is about to write, with what it is now: the
    /// first write per turn counts, so files can go back to before any turn
    /// (or the session).
    pub fn note_change(&mut self, path: &std::path::Path, before: Before) {
        self.checkpoints.note(self.turn, path, before);
    }

    /// Whether what `path` was before this turn is kept already.
    pub fn noted(&self, path: &std::path::Path) -> bool {
        self.checkpoints.has(self.turn, path)
    }

    /// The files noted in the current turn, with what they were before it.
    pub fn noted_this_turn(&self) -> Vec<(PathBuf, Before)> {
        self.checkpoints.of_turn(self.turn)
    }

    /// `path` ended the turn as it began it (an edit refused): it didn't change.
    pub fn unchanged(&mut self, path: &std::path::Path) {
        self.checkpoints.forget(self.turn, path);
    }

    /// The files a tool call is about to change (its diffs, and an edit's
    /// places), each told once a turn: `SessionEvent::WillEdit`.
    fn announce_edits(&mut self, tool_id: &str, cx: &mut Context<Self>) {
        let Some(tool) = self.transcript.tool(tool_id) else { return };
        let mut edits: Vec<(PathBuf, Option<String>, Option<String>)> = Vec::new();
        for content in &tool.content {
            if let ToolContent::Diff { path, old, new } = content
                && !edits.iter().any(|(p, ..)| p == path)
            {
                edits.push((path.clone(), old.clone(), Some(new.clone())));
            }
        }
        if matches!(tool.kind, Kind::Edit | Kind::Delete | Kind::Move) {
            for location in &tool.locations {
                if !edits.iter().any(|(p, ..)| *p == location.path) {
                    edits.push((location.path.clone(), None, None));
                }
            }
        }
        for (path, old, new) in edits {
            if path.is_absolute() && !self.noted(&path) {
                cx.emit(SessionEvent::WillEdit { path, old, new });
            }
        }
    }

    /// Every file the agent changed, with what it was before the session.
    pub fn changes(&self) -> Vec<(PathBuf, Before)> {
        self.checkpoints.files()
    }

    /// The turn a user message (by its index in the transcript) began.
    pub fn turn_of(&self, entry: usize) -> usize {
        self.transcript.entries.iter().take(entry + 1).filter(|e| matches!(e, Entry::User { .. })).count()
    }

    pub fn needs_you(&self) -> bool {
        self.state == State::Waiting || self.state == State::NeedsAuth
    }

    /// What the agent asks now: the title of the step it wants permission for
    /// (a notification says so), or that it needs a sign-in.
    pub fn open_question(&self) -> Option<String> {
        if self.state == State::NeedsAuth {
            return Some(format!("Sign in to {} to go on", self.agent_name));
        }
        self.transcript.entries.iter().rev().find_map(|entry| match entry {
            Entry::Tool(tool) if tool.permission.as_ref().is_some_and(|p| p.open) => Some(format!("May it: {}?", tool.title.trim_end_matches(['.', '?']))),
            _ => None,
        })
    }

    // ---- The agent's side -----------------------------------------------------------

    fn on_agent(&mut self, event: FromAgent, cx: &mut Context<Self>) {
        match event {
            FromAgent::Update(notification) => {
                if self.acp_session.as_deref().is_some_and(|id| id != notification.session_id.to_string().as_str()) {
                    return;
                }
                let update = &notification.update;
                let conversation = matches!(
                    update,
                    schema::SessionUpdate::UserMessageChunk(_)
                        | schema::SessionUpdate::AgentMessageChunk(_)
                        | schema::SessionUpdate::AgentThoughtChunk(_)
                        | schema::SessionUpdate::ToolCall(_)
                        | schema::SessionUpdate::ToolCallUpdate(_)
                        | schema::SessionUpdate::Plan(_)
                );
                if conversation && self.replaying {
                    return;
                }
                let tool_id = match update {
                    schema::SessionUpdate::ToolCall(call) => Some(call.tool_call_id.to_string()),
                    schema::SessionUpdate::ToolCallUpdate(call) => Some(call.tool_call_id.to_string()),
                    _ => None,
                };
                if self.transcript.apply(update) {
                    if let Some(id) = tool_id {
                        self.announce_edits(&id, cx);
                    }
                    self.save_soon(cx);
                } else {
                    match update {
                        schema::SessionUpdate::AvailableCommandsUpdate(u) => {
                            self.commands = u.available_commands.iter().map(|c| (c.name.clone(), c.description.clone())).collect();
                        }
                        schema::SessionUpdate::CurrentModeUpdate(u) => self.mode = Some(u.current_mode_id.to_string()),
                        schema::SessionUpdate::ConfigOptionUpdate(u) => self.set_options(&u.config_options),
                        schema::SessionUpdate::SessionInfoUpdate(u) => {
                            if let agent_client_protocol_schema::MaybeUndefined::Value(title) = &u.title
                                && !title.trim().is_empty()
                            {
                                self.title = title.clone();
                                cx.emit(SessionEvent::TitleChanged);
                                self.save_soon(cx);
                            }
                        }
                        schema::SessionUpdate::UsageUpdate(u) => self.usage = Some((u.used, u.size)),
                        _ => {}
                    }
                }
                cx.notify();
            }
            FromAgent::Permission(request, responder) => {
                let id = request.tool_call.tool_call_id.to_string();
                let fields = &request.tool_call.fields;
                if self.transcript.tool(&id).is_none() {
                    self.transcript.push(Entry::Tool(Tool {
                        id: id.clone(),
                        title: fields.title.clone().unwrap_or_else(|| "The agent asks to go on".into()),
                        kind: Default::default(),
                        status: Default::default(),
                        content: Vec::new(),
                        locations: Vec::new(),
                        input: fields.raw_input.clone(),
                        permission: None,
                    }));
                }
                let update = schema::SessionUpdate::ToolCallUpdate(request.tool_call.clone());
                self.transcript.apply(&update);
                self.announce_edits(&id, cx);
                if let Some(tool) = self.transcript.tool_mut(&id) {
                    tool.permission = Some(Permission {
                        options: request
                            .options
                            .iter()
                            .map(|o| PermissionOption {
                                id: o.option_id.to_string(),
                                name: o.name.clone(),
                                kind: serde_json::to_value(o.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
                            })
                            .collect(),
                        chosen: None,
                        open: true,
                    });
                }
                self.transcript.generation += 1;
                // A second question for the same call replaces the first.
                if let Some(old) = self.permissions.insert(id, responder) {
                    old.ok(json!({ "outcome": { "outcome": "cancelled" } }));
                }
                self.state = State::Waiting;
                self.save(cx);
                cx.emit(SessionEvent::NeedsYou);
                cx.notify();
            }
            FromAgent::ReadFile(request, responder) => {
                let id = self.hold(responder);
                cx.emit(SessionEvent::ReadFile { id, path: request.path.clone(), line: request.line, limit: request.limit });
            }
            FromAgent::WriteFile(request, responder) => {
                let id = self.hold(responder);
                cx.emit(SessionEvent::WriteFile { id, path: request.path.clone(), content: request.content.clone() });
            }
            FromAgent::Ext { method, params, responder } => {
                let id = self.hold(responder);
                cx.emit(SessionEvent::Ext { id, method, params });
            }
            FromAgent::CreateTerminal(request, responder) => {
                let cwd = request.cwd.clone().unwrap_or_else(|| self.cwd.clone());
                let env: Vec<(String, String)> = request.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect();
                let limit = request.output_byte_limit.map(|l| l as usize);
                match self.terminals.create(&request.command, &request.args, &cwd, &env, limit) {
                    Ok(id) => {
                        responder.ok(json!({ "terminalId": id }));
                        self.tick(cx);
                    }
                    Err(error) => responder.err(RpcError::internal(error)),
                }
            }
            FromAgent::TerminalOutput(request, responder) => match self.terminals.output(&request.terminal_id.to_string()) {
                Some(output) => responder.ok(output),
                None => responder.err(RpcError::new(RpcError::NOT_FOUND, "No such terminal")),
            },
            FromAgent::WaitForExit(request, responder) => match self.terminals.wait(&request.terminal_id.to_string()) {
                Some(exited) => {
                    cx.background_executor()
                        .spawn(async move {
                            match exited.await {
                                Ok(exit) => responder.ok(json!({ "exitCode": exit.code, "signal": exit.signal.map(|s| s.to_string()) })),
                                Err(_) => responder.err(RpcError::internal("The terminal went")),
                            }
                        })
                        .detach();
                }
                None => responder.err(RpcError::new(RpcError::NOT_FOUND, "No such terminal")),
            },
            FromAgent::KillTerminal(request, responder) => {
                self.terminals.kill(&request.terminal_id.to_string());
                responder.ok(json!({}));
            }
            FromAgent::ReleaseTerminal(request, responder) => {
                // Its output stays shown in the tool call.
                self.terminals.kill(&request.terminal_id.to_string());
                responder.ok(json!({}));
            }
        }
    }

    fn hold(&mut self, responder: Responder) -> u64 {
        self.next_request += 1;
        self.requests.insert(self.next_request, responder);
        self.next_request
    }

    /// Redraws while terminals run, as their output arrives.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.ticker.is_some() {
            return;
        }
        let generation = self.terminals.generation.clone();
        self.ticker = Some(cx.spawn(async move |this, cx| {
            let mut seen = u64::MAX;
            loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                let now = generation.load(Ordering::Relaxed);
                let running = this
                    .update(cx, |s, cx| {
                        if now != seen {
                            s.transcript.generation += 1;
                            cx.notify();
                        }
                        s.terminals.running()
                    })
                    .unwrap_or(false);
                seen = now;
                if !running {
                    this.update(cx, |s, cx| {
                        s.ticker = None;
                        s.transcript.generation += 1;
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            }
        }));
    }

}

impl Drop for Session {
    fn drop(&mut self) {
        if let (Some(agent), Some(id)) = (&self.agent, &self.acp_session) {
            agent.cancel(id);
        }
    }
}

fn auth_method(value: Value) -> Option<AuthMethod> {
    let id = value.get("id")?.as_str()?.to_string();
    let terminal = (value.get("type").and_then(Value::as_str) == Some("terminal")).then(|| {
        let args = value.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let env = value
            .get("env")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default();
        (args, env)
    });
    Some(AuthMethod {
        name: value.get("name").and_then(Value::as_str).unwrap_or(&id).to_string(),
        description: value.get("description").and_then(Value::as_str).map(str::to_string),
        id,
        terminal,
    })
}

fn config_option(value: Value) -> Option<ConfigOption> {
    let id = value.get("id")?.as_str()?.to_string();
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).filter(|t| !t.trim().is_empty()).map(str::to_string);
    let mut choices = Vec::new();
    let mut add = |options: &Value| {
        let mut push = |o: &Value| {
            if let Some(v) = o.get("value").and_then(Value::as_str) {
                choices.push(Choice { value: v.to_string(), name: text(o, "name").unwrap_or_else(|| v.to_string()), description: text(o, "description") });
            }
        };
        for o in options.as_array().into_iter().flatten() {
            match o.get("options") {
                // A group of options.
                Some(inner) => inner.as_array().into_iter().flatten().for_each(&mut push),
                None => push(o),
            }
        }
    };
    if let Some(options) = value.get("options") {
        add(options);
    }
    Some(ConfigOption {
        name: value.get("name").and_then(Value::as_str).unwrap_or(&id).to_string(),
        description: text(&value, "description"),
        category: value.get("category").and_then(Value::as_str).map(str::to_string),
        current: value.get("currentValue").cloned().unwrap_or(Value::Null),
        id,
        choices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_and_sign_ins_read_from_json() {
        let option = config_option(json!({ "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "a",
            "options": [{ "value": "a", "name": "A" }, { "group": "g", "name": "More", "options": [{ "value": "b" }] }] }))
        .unwrap();
        let choices: Vec<(&str, &str)> = option.choices.iter().map(|c| (c.value.as_str(), c.name.as_str())).collect();
        assert_eq!(choices, vec![("a", "A"), ("b", "b")]);
        assert_eq!(option.current, json!("a"));
        let method = auth_method(json!({ "id": "login", "name": "Log in", "type": "terminal", "args": ["login"], "env": { "X": "1" } })).unwrap();
        assert_eq!(method.terminal, Some((vec!["login".to_string()], vec![("X".to_string(), "1".to_string())])));
        assert!(auth_method(json!({ "id": "key", "name": "API key" })).unwrap().terminal.is_none());
    }

    /// A whole session with Kuana's agent against a fake provider: the
    /// prompt, a read, an edit and a command the user allows, the file
    /// written through the workspace's side, the answer, and the save.
    #[gpui::test]
    fn a_session_runs_a_turn_with_permissions_and_saves(cx: &mut gpui::TestAppContext) {
        use crate::agents::transcript::Status;
        use crate::ai::stream::test_server;
        use gpui::AppContext as _;

        let dir = std::env::temp_dir().join(format!("kuana-session-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();

        let sse = |events: Vec<(&str, Value)>| events.into_iter().map(|(n, d)| format!("event: {n}\ndata: {d}\n\n")).collect::<String>();
        let tool_turn = |id: &str, name: &str, input: Value| {
            sse(vec![
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": input.to_string() } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } })),
            ])
        };
        let server = test_server::start(vec![
            (200, "application/json", json!({ "data": [{ "id": "claude-opus-5-5" }] }).to_string()),
            (200, "text/event-stream", tool_turn("t1", "read_file", json!({ "path": "a.txt" }))),
            (200, "text/event-stream", tool_turn("t2", "edit_file", json!({ "path": "a.txt", "old_text": "hello", "new_text": "goodbye" }))),
            (200, "text/event-stream", tool_turn("t3", "run_command", json!({ "command": "echo ran" }))),
            (200, "text/event-stream", sse(vec![
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Done." } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ])),
            // After a restart: the follow-up's answer.
            (200, "application/json", json!({ "data": [{ "id": "claude-opus-5-5" }] }).to_string()),
            (200, "text/event-stream", sse(vec![
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Still here." } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ])),
        ]);
        let url = server.url.clone();
        let choose: crate::agents::kuana_agent::Chooser = Arc::new(move || {
            let endpoint = crate::ai::Endpoint { provider: crate::ai::providers::info("anthropic").unwrap(), url: url.clone(), key: Some("test-key".into()) };
            Ok(crate::ai::Choice { endpoint, model: "claude-opus-5-5".into(), effort: None })
        });
        let session = cx.new(|_| Session::new("kuana".into(), "Kuana Agent".into(), dir.clone(), Launcher::Kuana(choose)));

        // The workspace's part: files read and written, as `on_session_event` does.
        let needs_you = std::rc::Rc::new(std::cell::Cell::new(0));
        let asked = needs_you.clone();
        cx.update(|cx| {
            cx.subscribe(&session, move |session, event: &SessionEvent, cx| match event {
                SessionEvent::ReadFile { id, path, .. } => {
                    let content = std::fs::read_to_string(path).unwrap();
                    session.update(cx, |s, _| s.answer(*id, Ok(json!({ "content": content }))));
                }
                SessionEvent::WriteFile { id, path, content } => {
                    let before = std::fs::read_to_string(path).ok();
                    std::fs::write(path, content).unwrap();
                    session.update(cx, |s, _| {
                        s.note_change(path, before.map_or(Before::Missing, Before::Text));
                        s.answer(*id, Ok(json!({})));
                    });
                }
                SessionEvent::NeedsYou => asked.set(asked.get() + 1),
                _ => {}
            })
            .detach();
        });

        // Sent before the agent is connected: its context still goes embedded.
        let notes = ContextItem::File { path: dir.join("notes.md"), text: "be brief".into() };
        session.update(cx, |s, cx| s.send("Say goodbye and check it".into(), vec![notes], cx));
        assert_eq!(session.read_with(cx, |s, _| s.title.clone()), "Say goodbye and check it");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            cx.run_until_parked();
            // Allow whatever the agent asks.
            let waiting: Vec<String> = session.read_with(cx, |s, _| {
                s.transcript.entries.iter().filter_map(|e| match e {
                    Entry::Tool(t) if t.permission.as_ref().is_some_and(|p| p.chosen.is_none()) => Some(t.id.clone()),
                    _ => None,
                }).collect()
            });
            for id in waiting {
                session.update(cx, |s, cx| s.answer_permission(&id, "allow", cx));
            }
            let (state, done) = session.read_with(cx, |s, _| (s.state.clone(), s.transcript.entries.iter().any(|e| matches!(e, Entry::Assistant { .. }))));
            if done && state == State::Idle {
                break;
            }
            assert!(!matches!(state, State::Failed(_)), "{state:?}");
            assert!(std::time::Instant::now() < deadline, "the turn didn't end: {state:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "goodbye world\n");
        let first: Value = serde_json::from_str(&server.requests.lock().unwrap()[1].1).unwrap();
        // Embedded: Kuana's agent names its file by path, not as a URL in text.
        let parts = first["messages"][0]["content"].as_array().unwrap();
        let embedded = parts.iter().filter_map(|p| p["text"].as_str()).find(|t| t.ends_with("notes.md\">\nbe brief\n</context>"));
        assert!(embedded.is_some_and(|t| !t.contains("file://")), "{first}");
        assert_eq!(needs_you.get(), 2, "the edit and the command asked");
        session.read_with(cx, |s, _| {
            let tools: Vec<(&str, Status)> = s.transcript.entries.iter().filter_map(|e| match e {
                Entry::Tool(t) => Some((t.title.as_str(), t.status)),
                _ => None,
            }).collect();
            assert_eq!(tools.len(), 3, "{tools:?}");
            assert!(tools.iter().all(|(_, status)| *status == Status::Done), "{tools:?}");
            let command = s.transcript.entries.iter().find_map(|e| match e {
                Entry::Tool(t) if t.title.starts_with("Run") => Some(t.clone()),
                _ => None,
            }).unwrap();
            let terminal = command.content.iter().find_map(|c| match c {
                crate::agents::transcript::ToolContent::Terminal { id } => Some(id.clone()),
                _ => None,
            }).expect("the command's output shows in its card");
            assert_eq!(s.terminals.tail(&terminal, 5).unwrap().0.trim(), "ran");
            let changes = s.changes();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].1.text().as_deref(), Some("hello world\n"));
        });

        // Saved at the turn's end, and read back.
        cx.run_until_parked();
        let id = session.read_with(cx, |s, _| s.id.clone());
        let saved = history::load(&id).expect("saved");
        assert_eq!(saved.summary.title, "Say goodbye and check it");
        assert_eq!(saved.transcript.first_prompt(), Some("Say goodbye and check it"));

        // Kuana restarts: the session comes back from its save and the agent
        // continues the same conversation with the model.
        let choose = session.read_with(cx, |s, _| match &s.launcher {
            Launcher::Kuana(choose) => choose.clone(),
            Launcher::Process(_) => unreachable!(),
        });
        session.update(cx, |s, _| s.shut_down(true));
        drop(session);
        let entries = saved.transcript.entries.len();
        let session = cx.new(|_| Session::restored(saved, Launcher::Kuana(choose)));
        session.update(cx, |s, cx| s.send("Are you still there?".into(), Vec::new(), cx));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !session.read_with(cx, |s, _| s.state == State::Idle && s.transcript.entries.len() > entries + 1) {
            assert!(std::time::Instant::now() < deadline, "the follow-up didn't end");
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        session.read_with(cx, |s, _| {
            let shown: Vec<&Entry> = s.transcript.entries[entries..].iter().collect();
            assert!(!shown.iter().any(|e| matches!(e, Entry::Notice { .. })), "no 'starts afresh' notice: {shown:?}");
            assert!(matches!(shown.last(), Some(Entry::Assistant { text }) if text == "Still here."), "{shown:?}");
        });
        let requests = server.requests.lock().unwrap();
        let last: Value = serde_json::from_str(&requests.last().unwrap().1).unwrap();
        let messages = last["messages"].as_array().unwrap();
        assert!(messages.len() > 2, "the earlier conversation went with it: {} messages", messages.len());
        assert!(last.to_string().contains("Say goodbye and check it"));
        drop(requests);
        history::delete(&id).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Kuana's agent uses an MCP server from the settings: it starts on the
    /// first prompt, its tool is offered to the model, asked about, called.
    #[cfg(unix)]
    #[gpui::test]
    fn kuana_agent_calls_mcp_tools(cx: &mut gpui::TestAppContext) {
        use crate::ai::stream::test_server;
        use gpui::AppContext as _;

        // An MCP server in `sh`: canned answers, in the order the client asks.
        let script = r#"read l; echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"t"}}}'
read l; read l; echo '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"forecast","description":"The weather","inputSchema":{"type":"object"}}]}}'
read l; echo '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"sunny"}]}}'
read l"#;
        let settings = crate::settings::Settings::parse(&json!({ "ai": { "mcp_servers": { "weather": { "command": "sh", "args": ["-c", script] } } } }).to_string()).unwrap();
        cx.update(|cx| cx.set_global(settings));
        let sse = |events: Vec<(&str, Value)>| events.into_iter().map(|(n, d)| format!("event: {n}\ndata: {d}\n\n")).collect::<String>();
        let server = test_server::start(vec![
            (200, "application/json", json!({ "data": [] }).to_string()),
            (200, "text/event-stream", sse(vec![
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "t1", "name": "mcp__weather__forecast", "input": {} } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"city\":\"Warsaw\"}" } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } })),
            ])),
            (200, "text/event-stream", sse(vec![
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "It's sunny." } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ])),
        ]);
        let url = server.url.clone();
        let choose: crate::agents::kuana_agent::Chooser = Arc::new(move || {
            let endpoint = crate::ai::Endpoint { provider: crate::ai::providers::info("anthropic").unwrap(), url: url.clone(), key: Some("k".into()) };
            Ok(crate::ai::Choice { endpoint, model: "claude-opus-5-5".into(), effort: None })
        });
        let session = cx.new(|_| Session::new("kuana".into(), "Kuana Agent".into(), std::env::temp_dir(), Launcher::Kuana(choose)));
        session.update(cx, |s, cx| s.send("Weather in Warsaw?".into(), Vec::new(), cx));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            cx.run_until_parked();
            let waiting: Option<String> = session.read_with(cx, |s, _| {
                s.transcript.entries.iter().find_map(|e| match e {
                    Entry::Tool(t) if t.permission.as_ref().is_some_and(|p| p.chosen.is_none()) => Some(t.id.clone()),
                    _ => None,
                })
            });
            if let Some(id) = waiting {
                session.update(cx, |s, cx| s.answer_permission(&id, "allow", cx));
            }
            if session.read_with(cx, |s, _| s.state == State::Idle && s.transcript.entries.iter().any(|e| matches!(e, Entry::Assistant { text } if text == "It's sunny."))) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the turn didn't end");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        session.read_with(cx, |s, _| {
            let tool = s.transcript.tool("t1").expect("the MCP call shows");
            assert_eq!((tool.title.as_str(), tool.status), ("Use forecast (weather)", crate::agents::transcript::Status::Done));
        });
        let requests = server.requests.lock().unwrap();
        let first: Value = serde_json::from_str(&requests[1].1).unwrap();
        assert!(first["tools"].as_array().unwrap().iter().any(|t| t["name"] == "mcp__weather__forecast"), "offered to the model");
        let second: Value = serde_json::from_str(&requests[2].1).unwrap();
        assert!(second.to_string().contains("sunny"), "{second}");
        drop(requests);
        session.update(cx, |s, _| s.shut_down(true));
    }

    /// Retry answers the last message again, and an edited earlier message
    /// replaces it and what came after, for the agent too: the model never
    /// sees what was taken back.
    #[gpui::test]
    fn retry_and_edit_go_back_in_the_conversation(cx: &mut gpui::TestAppContext) {
        use crate::ai::stream::test_server;
        use gpui::AppContext as _;

        let answer = |text: &str| {
            [
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ]
            .iter()
            .map(|(n, d)| format!("event: {n}\ndata: {d}\n\n"))
            .collect::<String>()
        };
        let server = test_server::start(vec![
            (200, "application/json", json!({ "data": [] }).to_string()),
            (200, "text/event-stream", answer("A")),
            (200, "text/event-stream", answer("B")),
            (200, "text/event-stream", answer("C")),
            (200, "text/event-stream", answer("D")),
        ]);
        let url = server.url.clone();
        let choose: crate::agents::kuana_agent::Chooser = Arc::new(move || {
            let endpoint = crate::ai::Endpoint { provider: crate::ai::providers::info("anthropic").unwrap(), url: url.clone(), key: Some("k".into()) };
            Ok(crate::ai::Choice { endpoint, model: "claude-opus-5-5".into(), effort: None })
        });
        let session = cx.new(|_| Session::new("kuana".into(), "Kuana Agent".into(), std::env::temp_dir(), Launcher::Kuana(choose)));
        let settle = |session: &gpui::Entity<Session>, answers: usize, cx: &mut gpui::TestAppContext| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                cx.run_until_parked();
                let done = session.read_with(cx, |s, _| s.state == State::Idle && s.transcript.entries.iter().filter(|e| matches!(e, Entry::Assistant { .. })).count() == answers);
                if done {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "no answer");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let said = |session: &gpui::Entity<Session>, cx: &mut gpui::TestAppContext| -> Vec<String> {
            session.read_with(cx, |s, _| {
                s.transcript
                    .entries
                    .iter()
                    .filter_map(|e| match e {
                        Entry::User { text, .. } | Entry::Assistant { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            })
        };
        let sent = |server: &test_server::Server, n: usize| -> Vec<String> {
            let body: Value = serde_json::from_str(&server.requests.lock().unwrap()[n].1).unwrap();
            body["messages"].as_array().unwrap().iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).filter_map(|p| p["text"].as_str().map(str::to_string)).collect()
        };

        session.update(cx, |s, cx| s.send("first".into(), Vec::new(), cx));
        settle(&session, 1, cx);
        session.update(cx, |s, cx| s.send("second".into(), Vec::new(), cx));
        settle(&session, 2, cx);
        assert_eq!(said(&session, cx), ["first", "A", "second", "B"]);

        session.update(cx, |s, cx| s.retry_answer(cx));
        settle(&session, 2, cx);
        assert_eq!(said(&session, cx), ["first", "A", "second", "C"]);
        assert_eq!(sent(&server, 3), ["first", "A", "second"], "B isn't sent back");

        session.update(cx, |s, cx| s.edit_and_send(2, "changed".into(), Vec::new(), false, cx));
        settle(&session, 2, cx);
        assert_eq!(said(&session, cx), ["first", "A", "changed", "D"]);
        assert_eq!(sent(&server, 4), ["first", "A", "changed"], "neither the old message nor its answer");
        session.update(cx, |s, _| s.shut_down(false));
    }

    #[test]
    fn context_items_label_and_embed() {
        let sel = ContextItem::Selection { path: "/p/a.rs".into(), lines: (3, 5), text: "x".into() };
        assert_eq!(sel.label(), "a.rs:3-5");
        assert_eq!(sel.block(true, true)["resource"]["uri"], "file:///p/a.rs#L3-L5");
        assert!(sel.block(false, true)["text"].as_str().unwrap().starts_with("<context source=\"file:///p/a.rs#L3-L5\">"));
        let image = ContextItem::Image { name: "shot.png".into(), mime: "image/png".into(), data: Arc::from(&b"png"[..]) };
        assert_eq!(image.label(), "shot.png");
        assert_eq!(image.block(true, true), json!({ "type": "image", "mimeType": "image/png", "data": "cG5n" }));
        assert!(image.block(true, false)["text"].as_str().unwrap().contains("doesn't take pictures"));
        assert_eq!(image_mime(std::path::Path::new("a/B.JPG")), Some("image/jpeg"));
        assert_eq!(image_mime(std::path::Path::new("a.svg")), None);
        assert_eq!(ContextItem::Text { label: "Problems".into(), text: "".into() }.label(), "Problems");
    }
}
