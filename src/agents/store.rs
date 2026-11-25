//! The app's agents and sessions, one store for every window: the agents
//! Kuana can run (its own and the ACP registry's), installs in progress, the
//! sessions open now and the saved ones' summaries.

use std::{collections::HashMap, path::Path};

use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, Task};

use super::{
    history,
    session::{Launcher, Session, SessionEvent},
};
use crate::acp::{install, registry};
use crate::settings::Settings;

pub const KUANA_AGENT: &str = "kuana";

/// An agent the user can start a session with.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentChoice {
    pub id: String,
    pub name: String,
    pub description: String,
    pub installed: bool,
    pub installing: bool,
    pub update: bool,
    /// Why the last install failed.
    pub error: Option<String>,
}

pub struct AgentStore {
    /// Installed agents' versions, by id, as last read (`refresh_installed`):
    /// what's installed is drawn every frame, the disk read only when it changes.
    installed: HashMap<String, String>,
    registry: Vec<registry::Entry>,
    installing: HashMap<String, Task<()>>,
    errors: HashMap<String, String>,
    sessions: Vec<Entity<Session>>,
    saved: Vec<history::Summary>,
    subscriptions: HashMap<String, Subscription>,
    /// Held while a session works (`agent_keep_awake`): the computer doesn't
    /// idle-sleep then. `keep_awake` says whether it's wanted (the hold is
    /// taken in the background).
    awake: Option<kuana_os::power::Awake>,
    keep_awake: bool,
    _settings: Subscription,
    _quit: Subscription,
}

/// What the system shows where it lists what keeps the computer awake.
const AWAKE_REASON: &str = "Kuana: an agent session is working";

struct GlobalStore(Entity<AgentStore>);

impl Global for GlobalStore {}

/// Makes the store (from `main`); the saved sessions' index and the registry
/// are read in the background.
pub fn init(cx: &mut App) {
    let store = cx.new(|cx| {
        let index = cx.background_executor().spawn(async { (history::index(), registry::load()) });
        cx.spawn(async move |this, cx| {
            let (saved, registry) = index.await;
            this.update(cx, |s: &mut AgentStore, cx| {
                s.saved = saved;
                s.registry = registry;
                s.refresh_installed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        AgentStore {
            installed: HashMap::new(),
            registry: registry::fallback(),
            installing: HashMap::new(),
            errors: HashMap::new(),
            sessions: Vec::new(),
            saved: Vec::new(),
            subscriptions: HashMap::new(),
            awake: None,
            keep_awake: false,
            _settings: cx.observe_global::<Settings>(|s, cx| s.sync_awake(cx)),
            _quit: cx.on_app_quit(|s, cx| {
                s.quit(cx);
                async {}
            }),
        }
    });
    store.update(cx, |s, cx| s.refresh_installed(cx));
    cx.set_global(GlobalStore(store));
}

pub fn store(cx: &App) -> Option<Entity<AgentStore>> {
    cx.try_global::<GlobalStore>().map(|g| g.0.clone())
}

impl AgentStore {
    /// Kuana's agent first, then the registry's, installed ones first.
    pub fn choices(&self) -> Vec<AgentChoice> {
        let mut choices = vec![AgentChoice {
            id: KUANA_AGENT.into(),
            name: "Kuana Agent".into(),
            description: "Kuana's own agent, on the model you choose in Settings → AI".into(),
            installed: true,
            installing: false,
            update: false,
            error: None,
        }];
        let mut others: Vec<AgentChoice> = self
            .registry
            .iter()
            .filter(|e| e.install_kind().is_some())
            .map(|e| AgentChoice {
                id: e.id.clone(),
                name: e.name.clone(),
                description: e.description.clone(),
                installed: self.installed.contains_key(&e.id),
                installing: self.installing.contains_key(&e.id),
                update: self.installed.get(&e.id).is_some_and(|v| !e.version.is_empty() && *v != e.version),
                error: self.errors.get(&e.id).cloned(),
            })
            .collect();
        others.sort_by(|a, b| b.installed.cmp(&a.installed).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        choices.extend(others);
        choices
    }

    pub fn agent_name(&self, id: &str) -> String {
        if id == KUANA_AGENT {
            return "Kuana Agent".into();
        }
        self.registry.iter().find(|e| e.id == id).map_or_else(|| id.to_string(), |e| e.name.clone())
    }

    /// Removes an installed agent (to the Trash). Sessions already running
    /// keep their process.
    pub fn uninstall(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_string();
        let job = cx.background_executor().spawn({
            let id = id.clone();
            async move { install::uninstall(&id) }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |s, cx| {
                match result {
                    Ok(()) => s.errors.remove(&id),
                    Err(error) => s.errors.insert(id, format!("Couldn't uninstall: {error}")),
                };
                s.refresh_installed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads what's installed again (in the background).
    /// The registry's agents and every folder in Kuana's agents folder, so
    /// what's installed is known before the registry is (offline too).
    fn refresh_installed(&mut self, cx: &mut Context<Self>) {
        let mut ids: Vec<String> = self.registry.iter().map(|e| e.id.clone()).collect();
        let read = cx.background_executor().spawn(async move {
            let folders = install::agents_dir().and_then(|d| std::fs::read_dir(d).ok());
            ids.extend(folders.into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()));
            ids.sort();
            ids.dedup();
            ids.into_iter().filter_map(|id| install::installed(&id).map(|i| (id, i.version().to_string()))).collect()
        });
        cx.spawn(async move |this, cx| {
            let installed = read.await;
            this.update(cx, |s, cx| {
                s.installed = installed;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Installs (or updates) a registry agent in the background.
    pub fn install(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.registry.iter().find(|e| e.id == id).cloned() else { return };
        if self.installing.contains_key(id) {
            return;
        }
        self.errors.remove(id);
        let job = cx.background_executor().spawn(async move { install::install(&entry) });
        let key = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |s, cx| {
                s.installing.remove(&key);
                match result {
                    // Known at once: a session waiting for it starts now.
                    Ok(installed) => {
                        s.installed.insert(key, installed.version().to_string());
                    }
                    Err(error) => {
                        s.errors.insert(key, format!("Couldn't install: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.installing.insert(id.to_string(), task);
        cx.notify();
    }

    /// How a session with `agent` starts in `cwd`.
    pub fn launcher(&self, agent: &str, cwd: &Path, cx: &App) -> Result<Launcher, String> {
        if agent == KUANA_AGENT {
            let settings = Settings::get(cx).ai.clone().unwrap_or_default();
            return Ok(Launcher::Kuana(std::sync::Arc::new(move || settings.choice())));
        }
        let installed = install::installed(agent).ok_or_else(|| format!("{} isn't installed", self.agent_name(agent)))?;
        install::launch(&installed, cwd).map(Launcher::Process)
    }

    /// A new session (not started until its first prompt).
    pub fn new_session(&mut self, agent: &str, cwd: &Path, cx: &mut Context<Self>) -> Result<Entity<Session>, String> {
        let launcher = self.launcher(agent, cwd, cx)?;
        let name = self.agent_name(agent);
        let session = cx.new(|_| Session::new(agent.to_string(), name, cwd.to_path_buf(), launcher));
        self.adopt(&session, cx);
        Ok(session)
    }

    /// An open session by id, or a saved one opened.
    pub fn open(&mut self, id: &str, cx: &mut Context<Self>) -> Option<Entity<Session>> {
        if let Some(session) = self.sessions.iter().find(|s| s.read(cx).id == id) {
            return Some(session.clone());
        }
        let saved = history::load(id)?;
        let launcher = match self.launcher(&saved.summary.agent, &saved.summary.cwd, cx) {
            Ok(launcher) => launcher,
            // Its agent is gone: the transcript still opens, Kuana's agent
            // takes the next prompt.
            Err(_) => self.launcher(KUANA_AGENT, &saved.summary.cwd, cx).ok()?,
        };
        let session = cx.new(|_| Session::restored(saved, launcher));
        self.adopt(&session, cx);
        Some(session)
    }

    fn adopt(&mut self, session: &Entity<Session>, cx: &mut Context<Self>) {
        let id = session.read(cx).id.clone();
        let subscription = cx.subscribe(session, |store, session, event: &SessionEvent, cx| {
            if matches!(event, SessionEvent::Finished | SessionEvent::TitleChanged) {
                let summary = session.read(cx).summary();
                if !session.read(cx).transcript.entries.is_empty() {
                    store.saved.retain(|s| s.id != summary.id);
                    store.saved.insert(0, summary);
                }
                cx.notify();
            }
        });
        self.subscriptions.insert(id.clone(), subscription);
        // A session starting or ending a turn keeps the computer awake or lets it sleep.
        let observed = cx.observe(session, |store, _, cx| store.sync_awake(cx));
        self.subscriptions.insert(format!("{id}:awake"), observed);
        self.sessions.push(session.clone());
        cx.notify();
    }

    /// Keeps the computer awake while any session works (not while it waits
    /// for the user's answer: nothing goes on then), if `agent_keep_awake`.
    fn sync_awake(&mut self, cx: &mut Context<Self>) {
        let wanted = Settings::get(cx).agent_keep_awake()
            && self.sessions.iter().any(|s| {
                let s = s.read(cx);
                s.working() && s.state != super::session::State::Waiting
            });
        if wanted == self.keep_awake {
            return;
        }
        self.keep_awake = wanted;
        if !wanted {
            // Letting go may be a D-Bus call (Linux): off the UI thread.
            if let Some(awake) = self.awake.take() {
                cx.background_executor().spawn(async move { drop(awake) }).detach();
            }
            return;
        }
        let taken = cx.background_executor().spawn(async { kuana_os::power::keep_awake(AWAKE_REASON) });
        cx.spawn(async move |this, cx| {
            let awake = taken.await;
            this.update(cx, |s, cx| {
                // Wanted no longer (the turn ended meanwhile): dropped at once.
                if s.keep_awake && s.awake.is_none() {
                    s.awake = awake;
                } else if let Some(awake) = awake {
                    cx.background_executor().spawn(async move { drop(awake) }).detach();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Kuana quits: every live session is saved as it stands and its agent
    /// stopped (GPUI doesn't drop globals at quit, and an agent's process
    /// would outlive Kuana).
    fn quit(&mut self, cx: &mut Context<Self>) {
        for session in std::mem::take(&mut self.sessions) {
            session.update(cx, |s, _| s.shut_down(true));
        }
        self.subscriptions.clear();
    }

    /// Closes a session: its agent stops (it stays saved).
    pub fn close(&mut self, id: &str, cx: &mut Context<Self>) {
        self.release(id, true, cx);
    }

    fn release(&mut self, id: &str, save: bool, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.iter().find(|s| s.read(cx).id == id).cloned() {
            session.update(cx, |s, cx| {
                s.shut_down(save);
                cx.notify();
            });
        }
        self.sessions.retain(|s| s.read(cx).id != id);
        self.subscriptions.remove(id);
        self.subscriptions.remove(&format!("{id}:awake"));
        self.sync_awake(cx);
        cx.notify();
    }

    /// Deletes a session for good (a save still on its way is dropped:
    /// `history` remembers it's deleted).
    pub fn delete(&mut self, id: &str, cx: &mut Context<Self>) {
        // Its agent's own id, which a session not saved yet knows only live.
        let acp = self.sessions.iter().find(|s| s.read(cx).id == id).and_then(|s| s.read(cx).acp_session().map(str::to_string));
        if let Some(acp) = acp {
            super::kuana_agent::forget(&acp);
        }
        self.release(id, false, cx);
        self.saved.retain(|s| s.id != id);
        let id = id.to_string();
        cx.background_executor()
            .spawn(async move {
                let _ = history::delete(&id);
            })
            .detach();
        cx.notify();
    }

    /// The sessions of a project (open ones and saved ones), newest first.
    pub fn sessions_in(&self, folders: &[std::path::PathBuf], cx: &App) -> Vec<Row> {
        let inside = |cwd: &Path| folders.iter().any(|f| cwd.starts_with(f));
        let mut rows: Vec<Row> = self
            .sessions
            .iter()
            .filter(|s| inside(&s.read(cx).cwd))
            .map(|s| {
                let session = s.read(cx);
                Row { summary: session.summary(), live: Some(s.clone()) }
            })
            .collect();
        for saved in &self.saved {
            if inside(&saved.cwd) && !rows.iter().any(|r| r.summary.id == saved.id) {
                rows.push(Row { summary: saved.clone(), live: None });
            }
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.summary.updated));
        rows
    }

    pub fn live(&self) -> &[Entity<Session>] {
        &self.sessions
    }
}

/// A session in the Agents view.
pub struct Row {
    pub summary: history::Summary,
    pub live: Option<Entity<Session>>,
}
