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
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use futures::StreamExt as _;
use gpui::{Context, EventEmitter, Task};
use serde_json::{Value, json};

use super::{
    history, kuana_agent,
    terminals::Terminals,
    transcript::{Entry, Level, Permission, PermissionOption, Tool, Transcript},
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
    pub category: Option<String>,
    pub current: Value,
    /// `(value, name)`; empty for a yes/no option.
    pub choices: Vec<(String, String)>,
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
}

impl ContextItem {
    pub fn label(&self) -> String {
        let name = |p: &PathBuf| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        match self {
            ContextItem::File { path, .. } => name(path),
            ContextItem::Selection { path, lines: (a, b), .. } if a == b => format!("{}:{a}", name(path)),
            ContextItem::Selection { path, lines: (a, b), .. } => format!("{}:{a}-{b}", name(path)),
            ContextItem::Text { label, .. } => label.clone(),
        }
    }

    fn block(&self, embedded: bool) -> Value {
        let (uri, text) = match self {
            ContextItem::File { path, text } => (format!("file://{}", path.display()), text.clone()),
            ContextItem::Selection { path, lines: (a, b), text } => (format!("file://{}#L{a}-L{b}", path.display()), text.clone()),
            ContextItem::Text { label, text } => (format!("kuana:{}", label.replace(' ', "-").to_lowercase()), text.clone()),
        };
        if embedded {
            json!({ "type": "resource", "resource": { "uri": uri, "text": text, "mimeType": "text/plain" } })
        } else {
            json!({ "type": "text", "text": format!("<context source=\"{}\">\n{text}\n</context>", uri) })
        }
    }
}

/// A file the session's agent changed: what it held before the turn's
/// first write to it (None: the agent created it).
#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: PathBuf,
    pub before: Option<String>,
    pub turn: usize,
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
    pub changes: Vec<FileChange>,
    pub terminals: Terminals,
    /// Prompts sent while a turn ran, sent after it.
    pub queued: Vec<(String, Vec<ContextItem>)>,
    launcher: Launcher,
    agent: Option<Arc<Agent>>,
    acp_session: Option<String>,
    embedded_context: bool,
    /// While `session/load` replays the conversation, which the saved
    /// transcript already holds.
    replaying: bool,
    turn: usize,
    pending_prompt: Option<Vec<Value>>,
    permissions: HashMap<String, Responder>,
    requests: HashMap<u64, Responder>,
    next_request: u64,
    pump: Option<Task<()>>,
    turn_task: Option<Task<()>>,
    connecting: Option<Task<()>>,
    ticker: Option<Task<()>>,
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
        Session::with(s.id, s.agent, s.agent_name, s.cwd, launcher, saved.transcript, saved.acp_session, s.title, s.created, s.updated)
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
            changes: Vec::new(),
            terminals: Terminals::default(),
            queued: Vec::new(),
            launcher,
            agent: None,
            acp_session,
            embedded_context: false,
            replaying: false,
            turn: 0,
            pending_prompt: None,
            permissions: HashMap::new(),
            requests: HashMap::new(),
            next_request: 0,
            pump: None,
            turn_task: None,
            connecting: None,
            ticker: None,
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
        }
    }

    fn saved(&self) -> Option<history::Saved> {
        if self.transcript.entries.is_empty() {
            return None;
        }
        Some(history::Saved { summary: self.summary(), acp_session: self.acp_session.clone(), transcript: self.transcript.clone() })
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
    pub fn shut_down(&mut self) {
        self.save_now();
        if let (Some(agent), Some(id)) = (&self.agent, &self.acp_session) {
            agent.cancel(id);
        }
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        self.terminals.kill_all();
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

    pub fn is_connected(&self) -> bool {
        self.agent.as_ref().is_some_and(|a| !a.is_closed())
    }

    // ---- Connecting -------------------------------------------------------------

    fn connect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, State::Connecting) {
            return;
        }
        self.state = State::Connecting;
        let launcher = self.launcher.clone();
        let (cwd, previous) = (self.cwd.clone(), self.acp_session.clone());
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
            let auth: Vec<AuthMethod> = agent.init.auth_methods.iter().filter_map(|m| auth_method(serde_json::to_value(m).ok()?)).collect();
            let pump = cx.spawn({
                let this = this.clone();
                async move |cx| {
                    let mut events = events;
                    while let Some(event) = events.next().await {
                        if this.update(cx, |s, cx| s.on_agent(event, cx)).is_err() {
                            return;
                        }
                    }
                    this.update(cx, |s, cx| s.disconnected(cx)).ok();
                }
            });
            this.update(cx, |s, _| {
                s.agent = Some(agent.clone());
                s.embedded_context = embedded;
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
        let Some(agent) = self.agent.clone() else { return };
        let loading = previous.is_some();
        self.replaying = loading;
        self.connecting = Some(cx.spawn(async move |this, cx| {
            let opened = match &previous {
                Some(id) => agent.load_session(id, &cwd, Vec::new()).await.map(|r| (id.clone(), r.modes, r.config_options)),
                None => agent.new_session(&cwd, Vec::new()).await.map(|r| (r.session_id.to_string(), r.modes, r.config_options)),
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
                        if let Some(blocks) = s.pending_prompt.take() {
                            s.run_prompt(blocks, cx);
                        }
                    }
                    Err(e) if e.code == RpcError::AUTH_REQUIRED => {
                        s.state = State::NeedsAuth;
                        s.transcript.notice(Level::Warning, format!("Sign in to {} to start.", s.agent_name));
                    }
                    // The agent no longer has it: a new one (with a notice
                    // that the agent forgot the conversation).
                    Err(_) if loading => s.open_session(None, cwd, cx),
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
        self.transcript.notice(Level::Error, error);
        self.agent = None;
        cx.emit(SessionEvent::Finished);
        cx.notify();
    }

    fn disconnected(&mut self, cx: &mut Context<Self>) {
        let tail = self.agent.as_ref().map(|a| a.stderr_tail()).unwrap_or_default();
        self.agent = None;
        self.pump = None;
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
    pub fn retry(&mut self, cx: &mut Context<Self>) {
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
        if self.state.busy() {
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
        let mut blocks = vec![json!({ "type": "text", "text": text })];
        blocks.extend(context.iter().map(|c| c.block(self.embedded_context)));
        if self.is_connected() && self.acp_session.is_some() {
            self.run_prompt(blocks, cx);
        } else {
            self.pending_prompt = Some(blocks);
            if !matches!(self.state, State::NeedsAuth) {
                self.connect(cx);
            }
        }
        cx.notify();
    }

    fn run_prompt(&mut self, blocks: Vec<Value>, cx: &mut Context<Self>) {
        let (Some(agent), Some(id)) = (self.agent.clone(), self.acp_session.clone()) else { return };
        self.state = State::Running;
        self.turn_task = Some(cx.spawn(async move |this, cx| {
            let result = agent.prompt(&id, blocks).await;
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
            Err(e) if e.code == RpcError::CLOSED => {}
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
        if let (Some(agent), Some(id)) = (&self.agent, &self.acp_session) {
            agent.cancel(id);
        }
        for (_, responder) in self.permissions.drain() {
            responder.ok(json!({ "outcome": { "outcome": "cancelled" } }));
        }
        self.queued.clear();
        if matches!(self.state, State::Connecting) {
            self.connecting = None;
            self.pending_prompt = None;
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

    /// Notes a file the agent changed. The first write wins: rejecting the
    /// change puts back what the file held before the session touched it.
    pub fn note_change(&mut self, path: PathBuf, before: Option<String>) {
        if !self.changes.iter().any(|c| c.path == path) {
            self.changes.push(FileChange { path, before, turn: self.turn });
        }
    }

    pub fn needs_you(&self) -> bool {
        self.state == State::Waiting || self.state == State::NeedsAuth
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
                if !self.transcript.apply(update) {
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
    let mut choices = Vec::new();
    let mut add = |options: &Value| {
        for o in options.as_array().into_iter().flatten() {
            if let Some(inner) = o.get("options") {
                // A group of options.
                for g in inner.as_array().into_iter().flatten() {
                    if let Some(v) = g.get("value").and_then(Value::as_str) {
                        choices.push((v.to_string(), g.get("name").and_then(Value::as_str).unwrap_or(v).to_string()));
                    }
                }
            } else if let Some(v) = o.get("value").and_then(Value::as_str) {
                choices.push((v.to_string(), o.get("name").and_then(Value::as_str).unwrap_or(v).to_string()));
            }
        }
    };
    if let Some(options) = value.get("options") {
        add(options);
    }
    Some(ConfigOption {
        name: value.get("name").and_then(Value::as_str).unwrap_or(&id).to_string(),
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
        assert_eq!(option.choices, vec![("a".into(), "A".into()), ("b".into(), "b".into())]);
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
            (200, "text/event-stream", tool_turn("t3", "run_command", json!({ "command": "printf ran" }))),
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
                        s.note_change(path.clone(), before);
                        s.answer(*id, Ok(json!({})));
                    });
                }
                SessionEvent::NeedsYou => asked.set(asked.get() + 1),
                _ => {}
            })
            .detach();
        });

        session.update(cx, |s, cx| s.send("Say goodbye and check it".into(), Vec::new(), cx));
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
            assert_eq!(s.terminals.shown(&terminal).unwrap().0, "ran");
            assert_eq!(s.changes.len(), 1);
            assert_eq!(s.changes[0].before.as_deref(), Some("hello world\n"));
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
        session.update(cx, |s, _| s.shut_down());
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

    #[test]
    fn context_items_label_and_embed() {
        let sel = ContextItem::Selection { path: "/p/a.rs".into(), lines: (3, 5), text: "x".into() };
        assert_eq!(sel.label(), "a.rs:3-5");
        assert_eq!(sel.block(true)["resource"]["uri"], "file:///p/a.rs#L3-L5");
        assert!(sel.block(false)["text"].as_str().unwrap().starts_with("<context source=\"file:///p/a.rs#L3-L5\">"));
        assert_eq!(ContextItem::Text { label: "Problems".into(), text: "".into() }.label(), "Problems");
    }
}
