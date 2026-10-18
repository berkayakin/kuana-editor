//! Language servers (LSP): diagnostics, completion, hover, navigation,
//! rename and code actions from the server for each language.
//!
//! An `LspStore` per window starts one server per kind and project root the
//! first time a matching file opens, keeps open documents in sync and routes
//! what servers send back. A document can have several servers (Kuana's own,
//! then extensions': ESLint beside TypeScript); they all get its text, and
//! their diagnostics are combined. Editors talk to the servers directly for
//! requests: the first that can answer, or all for completion and code
//! actions; see `Editor::lsp_server_with`.

pub mod install;
pub mod protocol;
pub mod rpc;
pub mod servers;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use gpui::{App, Context, Entity, EventEmitter, Task, WeakEntity};
use ropey::Rope;
use serde_json::{Value, json};

use crate::{
    document::{Document, LspChanges},
    editor::Editor,
    extensions::wasm::{Asking, CallSettings, WasmServers},
    settings::Settings,
    syntax::Language,
};
use protocol::{Encoding, path_to_uri};
use rpc::{Client, Incoming, RpcError};

/// Servers are shared by documents of the same kind under the same root.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ServerKey {
    id: String,
    root: PathBuf,
}

enum ServerState {
    /// Looking for the executable and starting it.
    Finding,
    /// Running, waiting for `initialize` to finish; holds its spec in case
    /// it turns out not to be installed after all (rustup's proxy).
    Starting(Arc<Client>, &'static servers::ServerSpec),
    Running(Arc<Server>),
    /// Not installed (Install can fix that when the spec has an installer).
    Missing(&'static servers::ServerSpec),
    Failed(String),
}

/// A running, initialized server.
pub struct Server {
    /// Which server this is (`typescript`, an extension's): unique among a
    /// document's servers, unlike `name`.
    pub id: String,
    /// The executable's name, for the status bar and diagnostics.
    pub name: String,
    pub client: Arc<Client>,
    pub encoding: Encoding,
    capabilities: Value,
    /// `textDocumentSync` kind: 0 none, 1 full text, 2 incremental.
    sync: u64,
}

impl Server {
    /// A capability's options (`codeLensProvider` → `{ "resolveProvider": true }`).
    pub fn capability_value(&self, name: &str) -> &Value {
        self.capabilities.get(name).unwrap_or(&Value::Null)
    }

    /// Whether the server runs `command` through `workspace/executeCommand`.
    pub fn executes(&self, command: &str) -> bool {
        self.capability_value("executeCommandProvider")["commands"].as_array().is_some_and(|c| c.iter().any(|c| c == command))
    }

    pub fn capability(&self, name: &str) -> bool {
        self.capabilities.get(name).is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    }

    /// Characters that open completion (`.`, `::`, `<`…).
    pub fn completion_triggers(&self) -> Vec<String> {
        self.trigger_list("completionProvider", "triggerCharacters")
    }

    pub fn signature_triggers(&self) -> Vec<String> {
        let mut list = self.trigger_list("signatureHelpProvider", "triggerCharacters");
        list.extend(self.trigger_list("signatureHelpProvider", "retriggerCharacters"));
        list
    }

    fn trigger_list(&self, provider: &str, key: &str) -> Vec<String> {
        self.capabilities
            .get(provider)
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub fn resolves(&self, provider: &str) -> bool {
        self.capabilities.get(provider).and_then(|p| p.get("resolveProvider")).and_then(Value::as_bool).unwrap_or(false)
    }

    /// Sends a request and waits for its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.client.request(method, params).await
    }
}

/// A `$/progress` report, like "Indexing 40% — crate foo".
#[derive(Default, Clone)]
struct Progress {
    title: String,
    message: Option<String>,
    percentage: Option<u64>,
    /// When it was last shown (see `PROGRESS_THROTTLE`).
    shown: Option<Instant>,
}

/// How often one piece of a server's work in progress is shown again at
/// most: a server indexing reports hundreds of times a second, and each
/// showing redraws the workspace. Reports in between wait (`progress_redraw`).
const PROGRESS_THROTTLE: Duration = Duration::from_millis(100);

impl Progress {
    fn label(&self) -> String {
        let mut label = self.title.clone();
        if let Some(pct) = self.percentage {
            label.push_str(&format!(" {pct}%"));
        }
        if let Some(message) = &self.message {
            label.push_str(" — ");
            label.push_str(message);
        }
        label
    }
}

struct OpenDocument {
    doc: WeakEntity<Document>,
    /// Its real path (`document_store::key`), which a server's name for it
    /// leads to too (`registered_path`).
    real: PathBuf,
    /// The editor that gets its diagnostics. None for a document without a
    /// tab (edited by a workspace edit): diagnostics wait for one (`register`).
    editor: Option<WeakEntity<Editor>>,
    /// Its servers, the main one first, and whether each got `didOpen` (a
    /// server may still be starting).
    servers: Vec<(ServerKey, bool)>,
    language_id: &'static str,
    /// Registered without a tab, it skipped servers that weren't running
    /// (nobody looks at it); a tab showing it starts them.
    partial: bool,
    /// Closes it with the servers when the document goes (a tab-less one
    /// that was saved, see `DocumentStore::keep`).
    _released: gpui::Subscription,
}

impl OpenDocument {
    fn has(&self, key: &ServerKey) -> bool {
        self.servers.iter().any(|(k, _)| k == key)
    }

    fn opened(&self, key: &ServerKey) -> bool {
        self.servers.iter().any(|(k, opened)| k == key && *opened)
    }

    fn set_opened(&mut self, key: &ServerKey, opened: bool) {
        for (k, o) in &mut self.servers {
            if k == key {
                *o = opened;
            }
        }
    }
}

/// What the status bar shows for a file's server.
#[derive(Clone, PartialEq, Debug)]
pub enum LspStatus {
    Starting,
    /// Work in progress, e.g. "Indexing 40%".
    Busy(String),
    Ready(String),
    /// Not installed: how to install it by hand, and Kuana's installer if it has one.
    Missing { command: &'static str, installer: Option<&'static install::Installer> },
    /// Kuana is installing it (this command).
    Installing(&'static str),
    Failed(String),
}

/// One server's diagnostics for a document, as it published them.
pub struct LspDiagnostics {
    /// `Server::id`.
    pub id: String,
    /// `Server::name`, shown as the diagnostics' source.
    pub server: String,
    pub encoding: Encoding,
    pub params: Value,
}

pub enum LspEvent {
    /// A server asked to change files (`workspace/applyEdit`); answer with
    /// [`LspStore::respond`].
    ApplyEdit { key: ServerKey, request_id: Value, encoding: Encoding, edits: Vec<protocol::FileEdits> },
    /// An error message from a server.
    Message(String),
    /// Kuana installed this server.
    Installed(&'static str),
}

impl EventEmitter<LspEvent> for LspStore {}

pub struct LspStore {
    workspace_root: Option<PathBuf>,
    servers: HashMap<ServerKey, ServerState>,
    documents: HashMap<PathBuf, OpenDocument>,
    /// Latest diagnostics per file and server, open or not, so a file gets
    /// them when it opens later.
    diagnostics: HashMap<(PathBuf, ServerKey), Value>,
    /// The real path of each path servers named (`registered_path`): a server
    /// publishes for the same files again and again, thousands of them.
    real_paths: HashMap<PathBuf, PathBuf>,
    /// Work-done progress per server, by token.
    progress: HashMap<ServerKey, HashMap<String, Progress>>,
    /// Shows the progress reports `PROGRESS_THROTTLE` held back.
    progress_redraw: Option<Task<()>>,
    /// Which start of each server is current: a server replaced by a
    /// restart may still answer, start late or exit, and mustn't touch its
    /// successor's state.
    generations: HashMap<ServerKey, u64>,
    next_generation: u64,
    /// Each started server's spec, program and the configuration it was
    /// given (with what extensions' code said), to answer
    /// `workspace/configuration` and notice changes.
    configs: HashMap<ServerKey, (&'static servers::ServerSpec, PathBuf, servers::ServerConfig, servers::FromCode)>,
    /// Files each server asked to hear about (`workspace/didChangeWatchedFiles`),
    /// by registration id.
    watchers: HashMap<ServerKey, Vec<(String, Vec<FileWatcher>)>>,
    /// The last error each server showed (`window/showMessage`) and when, so
    /// the same one sent again right after shows once (`repeats_message`).
    shown_messages: HashMap<ServerKey, (String, Instant)>,
    /// Tests: the servers every document gets, instead of its language's.
    #[cfg(test)]
    pub(crate) test_specs: Option<Vec<&'static servers::ServerSpec>>,
    /// Pull diagnostics (`textDocument/diagnostic`): the request waiting or
    /// running per file and server (a newer one replaces it, which cancels
    /// it), and the id of the last report, so an unchanged one isn't sent again.
    pulls: HashMap<(PathBuf, ServerKey), Task<()>>,
    result_ids: HashMap<(PathBuf, ServerKey), String>,
    /// Pull diagnostics registered later (`client/registerCapability`, as
    /// pyright and ESLint do), by registration id: their options.
    pull_registrations: HashMap<ServerKey, Vec<(String, Value)>>,
    /// Every server process started, so the ones still running (shutting
    /// down already or not) are stopped when the app quits.
    started: Vec<std::sync::Weak<Client>>,
    /// Stops them as the app quits; registered with the first server.
    quit_hook: Option<gpui::Subscription>,
    _tasks: Vec<Task<()>>,
}

/// A closing window drops its store. Its servers are asked to shut down and
/// exit first (a server then ends its own helpers, such as tsserver), as on a
/// project change, instead of only being killed as their clients go.
impl Drop for LspStore {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

/// How long the app's quit waits for servers to exit before killing them
/// (GPUI itself waits only a moment for quit hooks).
const QUIT_WAIT: std::time::Duration = std::time::Duration::from_millis(80);

/// Pulled diagnostics are asked for this long after an edit, so typing
/// doesn't send a request per key; other open files of a server whose
/// diagnostics span files (`interFileDependencies`), a while later.
const PULL_DELAY: Duration = Duration::from_millis(200);
const PULL_OTHERS_DELAY: Duration = Duration::from_millis(800);
/// How long a rename or move in the file tree waits for servers to say what
/// else changes with it (`workspace/willRenameFiles`); later, it goes ahead
/// without them, as VS Code does after its timeout.
const WILL_RENAME_WAIT: Duration = Duration::from_secs(5);
/// A server's error that repeats the last one within this long is shown
/// once: rust-analyzer sends "Failed to discover workspace" twice in a row.
/// About as long as its toast stays.
const MESSAGE_REPEAT: Duration = Duration::from_secs(10);

/// Whether `text` repeats the message `shown` holds for `key` from less than
/// `MESSAGE_REPEAT` before `now`; if not, it becomes the one held. A repeat
/// doesn't restart the wait, so a message sent again and again still shows
/// every so often.
fn repeats_message(shown: &mut HashMap<ServerKey, (String, Instant)>, key: &ServerKey, text: &str, now: Instant) -> bool {
    if shown.get(key).is_some_and(|(last, at)| last == text && now.duration_since(*at) < MESSAGE_REPEAT) {
        return true;
    }
    shown.insert(key.clone(), (text.to_string(), now));
    false
}

/// A server's edits for a rename (`LspStore::will_rename`).
pub struct RenameEdits {
    pub encoding: Encoding,
    pub edits: Vec<protocol::FileEdits>,
}

impl LspStore {
    pub fn new() -> Self {
        Self {
            watchers: HashMap::new(),
            shown_messages: HashMap::new(),
            workspace_root: None,
            servers: HashMap::new(),
            documents: HashMap::new(),
            diagnostics: HashMap::new(),
            real_paths: HashMap::new(),
            progress: HashMap::new(),
            progress_redraw: None,
            configs: HashMap::new(),
            generations: HashMap::new(),
            next_generation: 0,
            pulls: HashMap::new(),
            result_ids: HashMap::new(),
            pull_registrations: HashMap::new(),
            #[cfg(test)]
            test_specs: None,
            started: Vec::new(),
            quit_hook: None,
            _tasks: Vec::new(),
        }
    }

    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.workspace_root = root;
    }

    /// Starts tracking an editor's document with its language's server.
    pub fn register(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        // An unloaded tab registers once its file is read.
        if !editor.read(cx).is_loaded() {
            return;
        }
        let doc = editor.read(cx).doc.clone();
        self.register_document(&doc, Some(editor), cx);
    }

    /// Starts tracking a document, shown by `editor` or by no tab at all (a
    /// workspace edit changed it): servers have to know its text either way,
    /// or they'd answer for what's on disk. Without a tab it goes only to
    /// servers already running; a tab showing it later starts the others.
    pub fn register_document(&mut self, doc: &Entity<Document>, editor: Option<&Entity<Editor>>, cx: &mut Context<Self>) {
        let (path, language) = {
            let d = doc.read(cx);
            match d.path.clone() {
                Some(path) if !d.is_stand_in() => (path, d.language),
                _ => return,
            }
        };
        let store = cx.weak_entity();
        if let Some(open) = self.documents.get_mut(&path).filter(|d| d.doc == doc.downgrade()) {
            // Known already (a split, or a document a tab shows now): the
            // servers have it, and this editor sends its edits through the
            // same store.
            let Some(editor) = editor else { return };
            editor.update(cx, |e, _| e.set_lsp(store));
            if open.editor.as_ref().and_then(WeakEntity::upgrade).is_some() {
                return;
            }
            open.editor = Some(editor.downgrade());
            let partial = std::mem::take(&mut open.partial);
            // What its servers published while it had no tab.
            let keys: Vec<ServerKey> = open.servers.iter().filter(|(_, opened)| *opened).map(|(k, _)| k.clone()).collect();
            for key in keys {
                self.apply_diagnostics(&path, &key, cx);
            }
            if partial {
                let mut keys = self.server_keys(language, &path, cx);
                keys.retain(|(key, _, _)| crate::trust::is_trusted(&key.root));
                self.connect(&path, language, keys, cx);
            }
            return;
        }
        let mut keys = self.server_keys(language, &path, cx);
        let all = keys.len();
        // Servers run their root's code (build scripts, plugins, its own
        // binaries), so it's the root that has to be trusted: a dependency
        // outside the project (the standard library under ~/.rustup) joins
        // the project's running server.
        keys.retain(|(key, _, _)| crate::trust::is_trusted(&key.root));
        if editor.is_none() {
            keys.retain(|(key, _, _)| self.servers.contains_key(key));
        }
        if keys.is_empty() {
            return;
        }
        if let Some(editor) = editor {
            editor.update(cx, |e, _| e.set_lsp(store));
        }
        let id = doc.entity_id();
        let released_path = path.clone();
        let released = cx.observe_release(doc, move |store: &mut Self, _, _| {
            if store.documents.get(&released_path).is_some_and(|d| d.doc.entity_id() == id) {
                store.did_close(&released_path);
            }
        });
        self.documents.insert(
            path.clone(),
            OpenDocument {
                doc: doc.downgrade(),
                real: crate::document_store::key(&path),
                editor: editor.map(Entity::downgrade),
                servers: Vec::new(),
                language_id: servers::language_id(language, &path),
                partial: keys.len() < all,
                _released: released,
            },
        );
        self.connect(&path, language, keys, cx);
    }

    /// Opens a tracked document with those of `keys` it isn't with yet,
    /// starting servers that aren't running.
    fn connect(&mut self, path: &Path, language: Language, keys: Vec<(ServerKey, Option<Vec<String>>, &'static servers::ServerSpec)>, cx: &mut Context<Self>) {
        for (key, custom, spec) in keys {
            let Some(doc) = self.documents.get_mut(path).filter(|d| !d.has(&key)) else { continue };
            doc.servers.push((key.clone(), false));
            match self.servers.get(&key) {
                Some(ServerState::Running(server)) => {
                    let server = server.clone();
                    self.open_document(path, &key, &server, cx);
                }
                Some(_) => {}
                None => self.start(key, spec, custom, language, path.to_path_buf(), cx),
            }
        }
    }

    /// Sends a document's edits since they were last sent (a document
    /// without a tab; editors send their own, `Editor::edited`).
    pub fn send_edits(&mut self, doc: &Entity<Document>, cx: &mut Context<Self>) {
        let Some(path) = doc.read(cx).path.clone() else { return };
        let changes = doc.update(cx, |d, _| d.take_lsp_changes());
        if changes.is_empty() {
            return;
        }
        // What servers answer for this version is read in this text.
        doc.update(cx, |d, _| d.note_lsp_version());
        let d = doc.read(cx);
        self.did_change(&path, d.version(), changes, d.rope());
        self.pull_soon(&path, cx);
    }

    /// The servers for `language` at `path`, the main one first (see
    /// `servers::specs`), each with a user-configured command if any. None
    /// when turned off in settings; a command there replaces them all.
    #[allow(clippy::type_complexity)]
    fn server_keys(&self, language: Language, path: &Path, cx: &App) -> Vec<(ServerKey, Option<Vec<String>>, &'static servers::ServerSpec)> {
        let custom = match Settings::get(cx).at(path).language_servers.get(language.name()) {
            Some(Value::Bool(false)) => return Vec::new(),
            Some(Value::Array(command)) => Some(command.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()),
            _ => None,
        };
        let workspace_root = self.workspace_root.as_deref();
        let root = |spec: &servers::ServerSpec| {
            // A file outside the project (a dependency Go to Declaration
            // opened): a server of the same kind that's already running
            // knows it, the project's first; starting another would index
            // the dependency on its own.
            if !workspace_root.is_some_and(|r| path.starts_with(r)) {
                let running = self.servers.keys().filter(|k| k.id == spec.id);
                if let Some(key) = running.min_by_key(|k| Some(k.root.as_path()) != workspace_root) {
                    return key.root.clone();
                }
            }
            servers::root_for(spec, path, workspace_root)
        };
        #[cfg(test)]
        if let Some(specs) = &self.test_specs {
            return specs.iter().map(|spec| (ServerKey { id: spec.id.to_string(), root: root(spec) }, None, *spec)).collect();
        }
        // A command in settings works for languages without a server of their own too.
        if let Some(command) = custom {
            let spec = servers::spec(language).unwrap_or(&servers::CUSTOM);
            let key = ServerKey { id: format!("custom:{}", command.join(" ")), root: root(spec) };
            return vec![(key, Some(command), spec)];
        }
        servers::specs(language).into_iter().map(|spec| (ServerKey { id: spec.id.to_string(), root: root(spec) }, None, spec)).collect()
    }

    fn start(
        &mut self,
        key: ServerKey,
        spec: &'static servers::ServerSpec,
        custom: Option<Vec<String>>,
        language: Language,
        file: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.servers.insert(key.clone(), ServerState::Finding);
        self.next_generation += 1;
        let generation = self.next_generation;
        self.generations.insert(key.clone(), generation);
        cx.notify();
        // The user's settings with the project's for the server's folder.
        let user = Settings::get(cx).at(&key.root).lsp.clone();
        // What extensions' code may read, and the other extensions' servers
        // for the language, which may add to this one's configuration.
        let tab_size = Settings::get(cx).for_file(language, Some(&file)).tab_size;
        let call_settings = CallSettings { lsp: user.clone(), tab_sizes: vec![(language.name().to_string(), tab_size)] };
        let others: Vec<&'static WasmServers> =
            servers::specs(language).into_iter().filter(|s| !std::ptr::eq(*s, spec)).filter_map(|s| s.wasm).collect();
        let task = cx.spawn(async move |this, cx| {
            // Finding the executable (and the login shell's PATH) touches the
            // disk. An extension's code may download its server: it runs on
            // the extension's own thread, which this waits for without
            // holding one of the executor's.
            let root = key.root.clone();
            let user_for_find = user.clone();
            let (spawned, code) = cx
                .background_executor()
                .spawn(async move {
                    // Python servers find the project's packages through `python` on PATH.
                    let venv = (language == Language::Python).then(|| crate::python::venv(file.parent()?)).flatten();
                    let mut code = servers::FromCode::default();
                    let mut own = None;
                    // `lsp.<id>.binary`, else the extension's code, else where Kuana finds it.
                    let command = match &custom {
                        Some(command) => Ok(servers::custom(command)),
                        None => match (servers::user_binary(spec, &user_for_find), spec.wasm) {
                            (Some(command), _) => Ok(Some(command)),
                            (None, Some(wasm)) => match wasm.start(&root, &call_settings).await {
                                Ok(started) => {
                                    code.server = Some(started.server.to_string());
                                    code.initialization_options = started.initialization_options.unwrap_or_default();
                                    own = Some((wasm.extension, started.server));
                                    Ok(Some(started.command))
                                }
                                // Kuana's guess, else what the code said.
                                Err(err) => servers::find(spec, &file, venv.as_deref()).map(Some).ok_or(err),
                            },
                            (None, None) => Ok(servers::find(spec, &file, venv.as_deref())),
                        },
                    };
                    if let Ok(Some(command)) = &command
                        && (own.is_some() || !others.is_empty())
                    {
                        let program = command.program.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let target = code.server.clone().unwrap_or_else(|| servers::zed_id(&program).to_string());
                        let asking = Asking { own, others, target, root: root.clone(), tab_sizes: call_settings.tab_sizes };
                        servers::merge(&mut code.initialization_options, &asking.additional_options(&user_for_find).await);
                        code.settings = asking.workspace_configuration(&user_for_find).await;
                        code.asking = Some(asking);
                    }
                    // Kuana's own Node.js runs the npm servers it installed.
                    let path = crate::python::path_with(venv.as_deref(), &install::path_with(install::node_bin().as_deref()));
                    let spawned = command.map(|c| c.map(|c| (Client::spawn(&c.program, &c.args, &root, &path, &c.env), c)));
                    if let Ok(Some((Ok(_), c))) = &spawned {
                        let program = c.program.clone();
                        std::thread::spawn(move || install::check_for_update(&program));
                    }
                    (spawned, code)
                })
                .await;
            // Stopped or restarted meanwhile: this start is history.
            let current = |s: &Self| s.generations.get(&key) == Some(&generation);
            let (client, incoming, command) = match spawned {
                Err(message) => {
                    this.update(cx, |s, cx| {
                        if current(s) {
                            s.fail(&key, message, cx);
                        }
                    })
                    .ok();
                    return;
                }
                Ok(None) => {
                    this.update(cx, |s, cx| {
                        if current(s) {
                            s.servers.insert(key.clone(), ServerState::Missing(spec));
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
                Ok(Some((Err(err), c))) => {
                    let message = format!("Couldn't start {}: {err}", c.program.display());
                    this.update(cx, |s, cx| {
                        if current(s) {
                            s.fail(&key, message, cx);
                        }
                    })
                    .ok();
                    return;
                }
                Ok(Some((Ok((client, incoming)), c))) => (client, incoming, c),
            };
            // Shown in the status bar: "pyright", not "pyright.cmd" on Windows.
            let program = &command.program;
            let windows_ext = program.extension().is_some_and(|e| ["exe", "cmd", "bat"].iter().any(|w| e.eq_ignore_ascii_case(w)));
            let name = if windows_ext { program.file_stem() } else { program.file_name() };
            let name = name.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let config = servers::config(spec, &command.program, &user, &code);
            let initialization_options = config.initialization_options.clone();
            let started = this.update(cx, |s, cx| {
                if !current(s) {
                    return false;
                }
                s.servers.insert(key.clone(), ServerState::Starting(client.clone(), spec));
                s.configs.insert(key.clone(), (spec, command.program.clone(), config, code));
                s.listen(key.clone(), generation, incoming, cx);
                s.started.retain(|c| c.strong_count() > 0);
                s.started.push(Arc::downgrade(&client));
                if s.quit_hook.is_none() {
                    s.quit_hook = Some(cx.on_app_quit(|s, _| {
                        s.stop_all_for_quit();
                        async {}
                    }));
                }
                true
            });
            if !started.unwrap_or(false) {
                // Dropping the client stops the process.
                return;
            }

            let init = client.request("initialize", initialize_params(&key.root, initialization_options));
            let result = match init.await {
                Ok(result) => result,
                // An exit is reported (and explained) by `Closed`.
                Err(err) if err.message == rpc::STOPPED => return,
                Err(err) => {
                    this.update(cx, |s, cx| {
                        if current(s) {
                            s.fail(&key, format!("{name} failed to start: {err}"), cx);
                        }
                    })
                    .ok();
                    return;
                }
            };
            client.notify("initialized", json!({}));
            // Servers that don't ask (`workspace/configuration`) take their settings this way.
            if let Ok(Some(settings)) = this.update(cx, |s, _| s.configs.get(&key).map(|(_, _, c, _)| c.settings.clone()))
                && !settings.is_null()
            {
                client.notify("workspace/didChangeConfiguration", json!({ "settings": settings }));
            }
            let capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
            let encoding = match capabilities.get("positionEncoding").and_then(Value::as_str) {
                Some("utf-8") => Encoding::Utf8,
                _ => Encoding::Utf16,
            };
            let sync = match capabilities.get("textDocumentSync") {
                Some(Value::Number(n)) => n.as_u64().unwrap_or(1),
                Some(Value::Object(o)) => o.get("change").and_then(Value::as_u64).unwrap_or(0),
                _ => 0,
            };
            let server = Arc::new(Server { id: key.id.clone(), name, client, encoding, capabilities, sync });
            this.update(cx, |s, cx| {
                if !current(s) {
                    return;
                }
                s.servers.insert(key.clone(), ServerState::Running(server.clone()));
                let waiting: Vec<PathBuf> =
                    s.documents.iter().filter(|(_, d)| d.has(&key) && !d.opened(&key)).map(|(p, _)| p.clone()).collect();
                for path in waiting {
                    s.open_document(&path, &key, &server, cx);
                }
                cx.notify();
            })
            .ok();
        });
        self._tasks.push(task);
    }

    fn fail(&mut self, key: &ServerKey, message: String, cx: &mut Context<Self>) {
        self.servers.insert(key.clone(), ServerState::Failed(message.clone()));
        self.progress.remove(key);
        cx.emit(LspEvent::Message(message));
        cx.notify();
    }

    /// Handles what the server sends on its own until it exits.
    fn listen(&mut self, key: ServerKey, generation: u64, mut incoming: futures::channel::mpsc::UnboundedReceiver<Incoming>, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = incoming.next().await {
                let handled = this.update(cx, |s, cx| {
                    // A replaced server's last words (its exit above all) are ignored.
                    if s.generations.get(&key) == Some(&generation) {
                        s.handle(&key, message, cx);
                    }
                });
                if handled.is_err() {
                    break;
                }
                // Messages queued up (rust-analyzer publishing diagnostics for
                // thousands of files) would otherwise all be handled in one go,
                // holding up input and drawing until the last.
                YieldNow(false).await;
            }
        });
        self._tasks.push(task);
    }

    fn client(&self, key: &ServerKey) -> Option<Arc<Client>> {
        match self.servers.get(key)? {
            ServerState::Starting(client, _) => Some(client.clone()),
            ServerState::Running(server) => Some(server.client.clone()),
            _ => None,
        }
    }

    fn handle(&mut self, key: &ServerKey, message: Incoming, cx: &mut Context<Self>) {
        match message {
            Incoming::Notification { method, params } => match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    let Some(path) = params.get("uri").and_then(Value::as_str).and_then(protocol::uri_to_path) else {
                        return;
                    };
                    let path = self.registered_path(path);
                    self.diagnostics.insert((path.clone(), key.clone()), params);
                    self.apply_diagnostics(&path, key, cx);
                }
                "$/progress" => {
                    let token = match params.get("token") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => return,
                    };
                    let value = params.get("value").unwrap_or(&Value::Null);
                    let entries = self.progress.entry(key.clone()).or_default();
                    // Whether to show it now: work starting or ending shows at
                    // once, a report once `PROGRESS_THROTTLE` passed since the last.
                    let now = match value.get("kind").and_then(Value::as_str) {
                        Some("end") => {
                            entries.remove(&token);
                            true
                        }
                        Some(kind) => {
                            let entry = entries.entry(token).or_default();
                            if kind == "begin" {
                                entry.title = value.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
                            }
                            if let Some(message) = value.get("message").and_then(Value::as_str) {
                                entry.message = Some(message.to_string()).filter(|m| !m.is_empty());
                            }
                            if let Some(pct) = value.get("percentage").and_then(Value::as_u64) {
                                entry.percentage = Some(pct);
                            }
                            let due = kind == "begin" || entry.shown.is_none_or(|at| at.elapsed() >= PROGRESS_THROTTLE);
                            if due {
                                entry.shown = Some(Instant::now());
                            }
                            due
                        }
                        None => return,
                    };
                    if now {
                        cx.notify();
                    } else if self.progress_redraw.is_none() {
                        self.progress_redraw = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(PROGRESS_THROTTLE).await;
                            this.update(cx, |s, cx| {
                                s.progress_redraw = None;
                                let shown = Instant::now();
                                s.progress.values_mut().flat_map(HashMap::values_mut).for_each(|p| p.shown = Some(shown));
                                cx.notify();
                            })
                            .ok();
                        }));
                    }
                }
                "window/showMessage" => {
                    // 1 = error; warnings and info are mostly chatter.
                    if params.get("type").and_then(Value::as_u64) == Some(1)
                        && let Some(text) = params.get("message").and_then(Value::as_str)
                        && !repeats_message(&mut self.shown_messages, key, text, Instant::now())
                    {
                        cx.emit(LspEvent::Message(text.to_string()));
                    }
                }
                _ => {}
            },
            Incoming::Request { id, method, params } => {
                let Some(client) = self.client(key) else { return };
                match method.as_str() {
                    // From `lsp.<id>.settings` (and Kuana's defaults); null means "use yours".
                    "workspace/configuration" => {
                        let settings = self.configs.get(key).map_or(Value::Null, |(_, _, c, _)| c.settings.clone());
                        let items = params.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                        let answers = items.iter().map(|item| servers::configuration(&settings, item.get("section").and_then(Value::as_str)));
                        client.respond(id, Value::Array(answers.collect()));
                    }
                    "workspace/workspaceFolders" => client.respond(id, workspace_folders(&key.root)),
                    "client/registerCapability" => {
                        let mut pulls_now = false;
                        for registration in params.get("registrations").and_then(Value::as_array).into_iter().flatten() {
                            if registration.get("method").and_then(Value::as_str) == Some("textDocument/diagnostic") {
                                let id = registration.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                                let options = registration.get("registerOptions").cloned().unwrap_or(json!({}));
                                let list = self.pull_registrations.entry(key.clone()).or_default();
                                list.retain(|(existing, _)| *existing != id);
                                list.push((id, options));
                                pulls_now = true;
                                continue;
                            }
                            if registration.get("method").and_then(Value::as_str) != Some("workspace/didChangeWatchedFiles") {
                                continue;
                            }
                            let id = registration.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                            let watchers = registration.pointer("/registerOptions/watchers").and_then(Value::as_array);
                            let watchers: Vec<FileWatcher> = watchers.into_iter().flatten().filter_map(FileWatcher::parse).collect();
                            let list = self.watchers.entry(key.clone()).or_default();
                            list.retain(|(existing, _)| *existing != id);
                            list.push((id, watchers));
                        }
                        client.respond(id, Value::Null);
                        // Its documents open already are asked for now.
                        if pulls_now {
                            self.pull_all(key, cx);
                        }
                    }
                    "client/unregisterCapability" => {
                        let gone = params.get("unregisterations").and_then(Value::as_array);
                        for item in gone.into_iter().flatten() {
                            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                            if let Some(list) = self.watchers.get_mut(key) {
                                list.retain(|(existing, _)| existing != id);
                            }
                            if let Some(list) = self.pull_registrations.get_mut(key) {
                                list.retain(|(existing, _)| existing != id);
                            }
                        }
                        client.respond(id, Value::Null);
                    }
                    "workspace/applyEdit" => {
                        let encoding = match self.servers.get(key) {
                            Some(ServerState::Running(server)) => server.encoding,
                            _ => Encoding::Utf16,
                        };
                        let edits = protocol::workspace_edit(params.get("edit").unwrap_or(&Value::Null));
                        cx.emit(LspEvent::ApplyEdit { key: key.clone(), request_id: id, encoding, edits });
                    }
                    // Progress shows through `$/progress` (Kuana says it does: `workDoneProgress`);
                    // a message's choices aren't offered, so none was picked.
                    "window/workDoneProgress/create" | "window/showMessageRequest" => client.respond(id, Value::Null),
                    // Its hints or lenses changed (it finished loading the
                    // project, another file changed): its documents' are due
                    // again, asked for by the editors that show them.
                    // What it found may have changed (a dependency, its settings):
                    // its open documents' diagnostics are asked for again.
                    "workspace/diagnostic/refresh" => {
                        client.respond(id, Value::Null);
                        self.pull_all(key, cx);
                    }
                    "workspace/inlayHint/refresh" | "workspace/codeLens/refresh" => {
                        client.respond(id, Value::Null);
                        let hints = method == "workspace/inlayHint/refresh";
                        let docs: Vec<Entity<Document>> = self.documents.values().filter(|d| d.opened(key)).filter_map(|d| d.doc.upgrade()).collect();
                        for doc in docs {
                            doc.update(cx, |d, cx| {
                                if hints {
                                    d.hints_changed();
                                } else {
                                    d.lenses_changed();
                                }
                                cx.notify();
                            });
                        }
                    }
                    _ => client.respond_not_found(id, &method),
                }
            }
            Incoming::Closed { stderr_tail } => {
                self.watchers.remove(key);
                self.pull_registrations.remove(key);
                // Stopped on purpose (Restricted Mode, closing): nothing to report.
                if !self.servers.contains_key(key) {
                    return;
                }
                // rustup's proxy runs, but exits when the component is missing.
                if let Some(ServerState::Starting(_, spec)) = self.servers.get(key)
                    && (stderr_tail.contains("Unknown binary") || stderr_tail.contains("is not installed"))
                {
                    let spec = *spec;
                    self.servers.insert(key.clone(), ServerState::Missing(spec));
                    cx.notify();
                    return;
                }
                let detail = stderr_tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                let message = if detail.is_empty() {
                    "The language server stopped".to_string()
                } else {
                    format!("The language server stopped: {detail}")
                };
                if !matches!(self.servers.get(key), Some(ServerState::Failed(_))) {
                    self.fail(key, message, cx);
                }
                // What it found is no longer kept up to date.
                self.forget_diagnostics(key, cx);
                for doc in self.documents.values_mut() {
                    doc.set_opened(key, false);
                }
            }
        }
    }

    /// Files changed on disk (created, changed or deleted): each server hears
    /// about the ones it watches.
    #[cfg(test)]
    pub fn files_changed(&self, changes: &[(PathBuf, FileChange)]) {
        self.send_file_events(self.file_watchers().events(changes));
    }

    /// What the servers watch, to match changed files against off the UI
    /// thread (`FileWatchers::events`).
    pub fn file_watchers(&self) -> FileWatchers {
        let watchers = self.watchers.iter().map(|(key, registrations)| (key.clone(), registrations.iter().flat_map(|(_, w)| w.iter().cloned()).collect()));
        FileWatchers(watchers.collect())
    }

    /// Tells each server about the changes it watches (`FileWatchers::events`).
    pub fn send_file_events(&self, events: Vec<(ServerKey, Value)>) {
        for (key, params) in events {
            if let Some(client) = self.client(&key) {
                client.notify("workspace/didChangeWatchedFiles", params);
            }
        }
    }

    /// Answers a server request (after the workspace applied an edit).
    pub fn respond(&self, key: &ServerKey, request_id: Value, result: Value) {
        if let Some(client) = self.client(key) {
            client.respond(request_id, result);
        }
    }

    /// The path a document is registered under, for a path a server named it
    /// by: servers spell paths their own way (on Windows a lowercase drive
    /// letter and no `\\?\`, which `canonicalize` gives Kuana's; a path
    /// through a symlink). A file that isn't open goes by its real path.
    fn registered_path(&mut self, path: PathBuf) -> PathBuf {
        /// Past this many paths known, they're found again (a long session).
        const KEEP: usize = 100_000;
        if self.documents.contains_key(&path) {
            return path;
        }
        let real = match self.real_paths.get(&path) {
            Some(real) => real.clone(),
            None => {
                // Not there (yet): not kept, it may be made.
                let Ok(real) = crate::paths::canonicalize(&path) else {
                    return self.documents.iter().find(|(_, d)| d.real == path).map_or(path, |(registered, _)| registered.clone());
                };
                if self.real_paths.len() >= KEEP {
                    self.real_paths.clear();
                }
                self.real_paths.insert(path, real.clone());
                real
            }
        };
        self.documents.iter().find(|(_, d)| d.real == real).map_or(real, |(registered, _)| registered.clone())
    }

    /// Gives the editor of `path` one server's diagnostics for it (the
    /// others' stay as they are, following edits).
    fn apply_diagnostics(&mut self, path: &Path, key: &ServerKey, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get(path).filter(|d| d.has(key)) else { return };
        let Some(ServerState::Running(server)) = self.servers.get(key) else { return };
        let Some(params) = self.diagnostics.get(&(path.to_path_buf(), key.clone())) else { return };
        let source = LspDiagnostics { id: server.id.clone(), server: server.name.clone(), encoding: server.encoding, params: params.clone() };
        // Without a tab, they wait for one: `register` applies them then.
        let Some(editor) = doc.editor.as_ref().and_then(WeakEntity::upgrade) else { return };
        editor.update(cx, |e, cx| e.set_server_diagnostics(source, cx));
    }

    /// What server `id` last published for `path` (raw LSP diagnostics),
    /// for code action requests: a server gets its own back.
    pub fn published(&self, path: &Path, id: &str) -> Vec<Value> {
        self.diagnostics
            .iter()
            .filter(|((p, key), _)| p == path && key.id == id)
            .filter_map(|(_, params)| params.get("diagnostics")?.as_array().cloned())
            .flatten()
            .collect()
    }

    /// A server stopped (or restarts): its diagnostics go, everywhere.
    fn forget_diagnostics(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        self.diagnostics.retain(|(_, k), _| k != key);
        self.pulls.retain(|(_, k), _| k != key);
        self.result_ids.retain(|(_, k), _| k != key);
        let docs: Vec<Entity<Document>> = self.documents.values().filter(|d| d.has(key)).filter_map(|d| d.doc.upgrade()).collect();
        for doc in docs {
            doc.update(cx, |d, cx| {
                d.clear_server_diagnostics(&key.id);
                cx.notify();
            });
        }
    }

    fn open_document(&mut self, path: &Path, key: &ServerKey, server: &Arc<Server>, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get_mut(path) else { return };
        let Some(document) = doc.doc.upgrade() else {
            self.documents.remove(path);
            return;
        };
        // The server gets the current text, so earlier edits don't matter
        // (unless another server still has to hear about them).
        let first = !doc.servers.iter().any(|(_, opened)| *opened);
        let (text, version) = document.update(cx, |d, _| {
            if first {
                d.take_lsp_changes();
            }
            d.note_lsp_version();
            (d.rope().to_string(), d.version())
        });
        server.client.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": path_to_uri(path), "languageId": doc.language_id, "version": version as i64, "text": text
            } }),
        );
        doc.set_opened(key, true);
        let editor = doc.editor.as_ref().and_then(WeakEntity::upgrade);
        self.apply_diagnostics(path, key, cx);
        self.pull(path, key, Duration::ZERO, cx);
        // What else it knows of the file can be asked for now.
        if let Some(editor) = editor {
            editor.update(cx, |e, cx| e.lsp_opened(cx));
        }
    }

    /// Asks a server that has pull diagnostics (`diagnosticProvider`) for an
    /// open document's, `delay` from now. The answer is kept like a publish
    /// (`diagnostics`, with the version it's for), so everything that reads
    /// those works the same for either kind of server.
    fn pull(&mut self, path: &Path, key: &ServerKey, delay: Duration, cx: &mut Context<Self>) {
        let Some(ServerState::Running(server)) = self.servers.get(key) else { return };
        let Some(open) = self.documents.get(path).filter(|d| d.opened(key)) else { return };
        let Some(options) = self.pull_options(key, open.language_id) else { return };
        let Some(document) = open.doc.upgrade() else { return };
        let version = document.read(cx).version();
        let mut params = json!({ "textDocument": { "uri": path_to_uri(path) } });
        if let Some(identifier) = options.get("identifier").filter(|i| i.is_string()) {
            params["identifier"] = identifier.clone();
        }
        let slot = (path.to_path_buf(), key.clone());
        if let Some(previous) = self.result_ids.get(&slot) {
            params["previousResultId"] = previous.clone().into();
        }
        let (server, generation) = (server.clone(), self.generations.get(key).copied());
        let (path, key) = slot.clone();
        let task = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            // A failed or cancelled pull leaves what's shown; the next edit asks again.
            let Ok(report) = server.request("textDocument/diagnostic", params).await else { return };
            this.update(cx, |s, cx| {
                if s.generations.get(&key).copied() == generation {
                    s.pulled(&path, &key, Some(version), &report, cx);
                }
            })
            .ok();
        });
        self.pulls.insert(slot, task);
    }

    /// A pull's report: a full one replaces the file's diagnostics from that
    /// server, an unchanged one keeps them; reports for related files too.
    fn pulled(&mut self, path: &Path, key: &ServerKey, version: Option<u64>, report: &Value, cx: &mut Context<Self>) {
        let slot = (path.to_path_buf(), key.clone());
        match report.get("resultId").and_then(Value::as_str) {
            Some(id) => self.result_ids.insert(slot.clone(), id.to_string()),
            None => self.result_ids.remove(&slot),
        };
        if report.get("kind").and_then(Value::as_str) == Some("full") {
            let mut params = json!({ "uri": path_to_uri(path), "diagnostics": report.get("items").cloned().unwrap_or(json!([])) });
            if let Some(version) = version {
                params["version"] = version.into();
            }
            self.diagnostics.insert(slot, params);
            self.apply_diagnostics(path, key, cx);
        }
        for (uri, related) in report.get("relatedDocuments").and_then(Value::as_object).into_iter().flatten() {
            let Some(other) = protocol::uri_to_path(uri).map(|p| self.registered_path(p)) else { continue };
            // Its version isn't known: read in today's text.
            self.pulled(&other, key, None, related, cx);
        }
    }

    /// An edit to `path`: its servers that pull diagnostics are asked again
    /// a moment later, and those whose diagnostics span files for their other
    /// open documents too.
    pub fn pull_soon(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get(path) else { return };
        let keys: Vec<ServerKey> = doc.servers.iter().filter(|(_, opened)| *opened).map(|(k, _)| k.clone()).collect();
        for key in keys {
            self.pull(path, &key, PULL_DELAY, cx);
            let language_id = self.documents.get(path).map_or("", |d| d.language_id);
            let spans_files = self.pull_options(&key, language_id).is_some_and(|o| o["interFileDependencies"] == Value::Bool(true));
            if spans_files {
                for other in self.opened_by(&key).into_iter().filter(|p| p != path) {
                    self.pull(&other, &key, PULL_OTHERS_DELAY, cx);
                }
            }
        }
    }

    /// How a server pulls diagnostics for documents of `language_id`: its
    /// `diagnosticProvider`, else a registration whose document selector
    /// (when it names languages) has the language. None: it publishes them.
    fn pull_options(&self, key: &ServerKey, language_id: &str) -> Option<Value> {
        let Some(ServerState::Running(server)) = self.servers.get(key) else { return None };
        match server.capability_value("diagnosticProvider") {
            Value::Null | Value::Bool(false) => {}
            Value::Bool(true) => return Some(json!({})),
            options => return Some(options.clone()),
        }
        let selects = |options: &Value| match options.get("documentSelector").and_then(Value::as_array) {
            Some(filters) if filters.iter().any(|f| f.get("language").is_some()) => {
                filters.iter().any(|f| f.get("language").and_then(Value::as_str) == Some(language_id))
            }
            _ => true,
        };
        self.pull_registrations.get(key)?.iter().map(|(_, options)| options).find(|o| selects(o)).cloned()
    }

    /// Pulls every document a server has open (it asked for a refresh).
    fn pull_all(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        for path in self.opened_by(key) {
            self.pull(&path, key, Duration::ZERO, cx);
        }
    }

    fn opened_by(&self, key: &ServerKey) -> Vec<PathBuf> {
        self.documents.iter().filter(|(_, d)| d.opened(key)).map(|(p, _)| p.clone()).collect()
    }

    /// Before files are renamed or moved (`(old, new, is a folder)`): the
    /// running servers whose `workspace.fileOperations.willRename` filters
    /// cover one of them say what else changes with it (imports, Rust's `mod`
    /// lines), as edits to apply first. None when no server asks; an error
    /// when they took longer than `WILL_RENAME_WAIT` (the rename goes ahead).
    pub fn will_rename(&self, renames: &[(PathBuf, PathBuf, bool)], cx: &App) -> Option<Task<Result<Vec<RenameEdits>, ()>>> {
        let servers = self.file_operation_servers("willRename", renames);
        if servers.is_empty() {
            return None;
        }
        let params = rename_params(renames);
        let requests: Vec<_> = servers
            .into_iter()
            .map(|server| {
                let encoding = server.encoding;
                let request = server.client.request("workspace/willRenameFiles", params.clone());
                async move { (encoding, request.await) }
            })
            .collect();
        let timer = cx.background_executor().timer(WILL_RENAME_WAIT);
        Some(cx.background_executor().spawn(async move {
            let answers = futures::future::join_all(requests);
            futures::pin_mut!(answers);
            match futures::future::select(answers, timer).await {
                futures::future::Either::Left((answers, _)) => Ok(answers
                    .into_iter()
                    .filter_map(|(encoding, answer)| Some(RenameEdits { encoding, edits: protocol::workspace_edit(&answer.ok()?) }))
                    .filter(|e| !e.edits.is_empty())
                    .collect()),
                futures::future::Either::Right(_) => Err(()),
            }
        }))
    }

    /// After files were renamed or moved: the servers that asked hear of it
    /// (`workspace/didRenameFiles`).
    pub fn did_rename(&self, renames: &[(PathBuf, PathBuf, bool)]) {
        let params = rename_params(renames);
        for server in self.file_operation_servers("didRename", renames) {
            server.client.notify("workspace/didRenameFiles", params.clone());
        }
    }

    /// The running servers of the project a rename is in whose file
    /// operation filters (`operation`: `willRename`, `didRename`) match one
    /// of its paths.
    fn file_operation_servers(&self, operation: &str, renames: &[(PathBuf, PathBuf, bool)]) -> Vec<Arc<Server>> {
        self.servers
            .iter()
            .filter_map(|(key, state)| match state {
                ServerState::Running(server) if renames.iter().any(|(old, _, _)| old.starts_with(&key.root)) => Some(server),
                _ => None,
            })
            .filter(|server| {
                let filters = &server.capability_value("workspace")["fileOperations"][operation]["filters"];
                renames.iter().any(|(old, _, is_dir)| file_operation_matches(filters, old, *is_dir))
            })
            .cloned()
            .collect()
    }

    /// The running server for an open document.
    /// Every server that's up, for project-wide requests.
    pub fn running_servers(&self) -> Vec<Arc<Server>> {
        self.servers
            .values()
            .filter_map(|s| match s {
                ServerState::Running(server) => Some(server.clone()),
                _ => None,
            })
            .collect()
    }

    /// The running servers that have an open document, the main one first.
    pub fn servers_for(&self, path: &Path) -> Vec<Arc<Server>> {
        let Some(doc) = self.documents.get(path) else { return Vec::new() };
        doc.servers
            .iter()
            .filter(|(_, opened)| *opened)
            .filter_map(|(key, _)| match self.servers.get(key)? {
                ServerState::Running(server) => Some(server.clone()),
                _ => None,
            })
            .collect()
    }

    /// Sends an editor's edits since the last call to each of its servers.
    pub fn did_change(&self, path: &Path, version: u64, changes: LspChanges, rope: &Rope) {
        if changes.is_empty() {
            return;
        }
        for server in self.servers_for(path) {
            Self::send_changes(&server, path, version, &changes, rope);
        }
    }

    fn send_changes(server: &Server, path: &Path, version: u64, changes: &LspChanges, rope: &Rope) {
        if server.sync == 0 {
            return;
        }
        let content_changes: Vec<Value> = if changes.full || server.sync == 1 {
            vec![json!({ "text": rope.to_string() })]
        } else {
            let col = |(_, utf8, utf16): (u32, u32, u32)| if server.encoding == Encoding::Utf8 { utf8 } else { utf16 };
            changes
                .changes
                .iter()
                .map(|c| {
                    json!({
                        "range": {
                            "start": { "line": c.start.0, "character": col(c.start) },
                            "end": { "line": c.end.0, "character": col(c.end) },
                        },
                        "text": c.text,
                    })
                })
                .collect()
        };
        server.client.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": path_to_uri(path), "version": version as i64 }, "contentChanges": content_changes }),
        );
    }

    pub fn did_save(&self, path: &Path, rope: &Rope) {
        for server in self.servers_for(path) {
            Self::send_save(&server, path, rope);
        }
    }

    fn send_save(server: &Server, path: &Path, rope: &Rope) {
        let save = server.capabilities.get("textDocumentSync").and_then(|s| s.get("save"));
        if save.is_none_or(|s| s == &Value::Bool(false)) {
            return;
        }
        let include_text = save.and_then(|s| s.get("includeText")).and_then(Value::as_bool).unwrap_or(false);
        let mut params = json!({ "textDocument": { "uri": path_to_uri(path) } });
        if include_text {
            params["text"] = Value::String(rope.to_string());
        }
        server.client.notify("textDocument/didSave", params);
    }

    /// Stops tracking a closed tab's document.
    /// The editor the store tracks `path` through is closing while another
    /// editor of the same document stays: track it through that one.
    pub fn rebind(&mut self, path: &Path, editor: &Entity<Editor>) {
        if let Some(doc) = self.documents.get_mut(path) {
            doc.editor = Some(editor.downgrade());
        }
    }

    pub fn did_close(&mut self, path: &Path) {
        self.pulls.retain(|(p, _), _| p != path);
        self.result_ids.retain(|(p, _), _| p != path);
        let Some(doc) = self.documents.remove(path) else { return };
        for (key, _) in doc.servers.iter().filter(|(_, opened)| *opened) {
            if let Some(client) = self.client(key) {
                client.notify("textDocument/didClose", json!({ "textDocument": { "uri": path_to_uri(path) } }));
            }
        }
    }

    /// An editor's file got another path (rename, move, Save As): the server
    /// closes the old URI and opens the new one (a new name can also mean
    /// another language and server).
    pub fn moved(&mut self, old: &Path, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if editor.read(cx).doc.read(cx).path.as_deref() != Some(old) {
            self.did_close(old);
        }
        self.register(editor, cx);
    }

    /// Installs the missing server of `path`'s document; when it's done,
    /// every window starts the servers it was missing.
    pub fn install_server(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get(path) else { return };
        let Some(spec) = doc.servers.iter().find_map(|(key, _)| match self.servers.get(key) {
            Some(ServerState::Missing(spec)) => Some(*spec),
            _ => None,
        }) else {
            return;
        };
        let Some(installer) = install::for_spec(spec) else { return };
        if install::installing(installer.command) {
            return;
        }
        let task = cx.background_executor().spawn(async move { install::install(installer) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |_, cx| match result {
                Ok(()) => {
                    cx.emit(LspEvent::Installed(installer.command));
                    install::installed_now(cx);
                }
                Err(err) => {
                    cx.emit(LspEvent::Message(format!("Couldn't install {}: {err}", installer.command)));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        // Shown as Installing from now on (`status`).
        cx.notify();
    }

    /// Starts again every server that was missing (one was installed).
    pub fn retry_missing(&mut self, cx: &mut Context<Self>) {
        let missing: Vec<ServerKey> = self.servers.iter().filter(|(_, s)| matches!(s, ServerState::Missing(_))).map(|(k, _)| k.clone()).collect();
        for key in &missing {
            self.servers.remove(key);
        }
        self.reregister(|d| missing.iter().any(|key| d.has(key)), cx);
        cx.notify();
    }

    /// Settings changed: running servers whose settings differ now are told
    /// (initialization options and programs apply when a server restarts).
    /// Extensions' code is asked again for the servers it configures.
    pub fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<ServerKey> = self.configs.keys().cloned().collect();
        for key in keys {
            let user = Settings::get(cx).at(&key.root).lsp.clone();
            self.update_settings(&key, None, &user);
            let Some(asking) = self.configs.get(&key).and_then(|(_, _, _, code)| code.asking.clone()) else { continue };
            let generation = self.generations.get(&key).copied();
            let user = user.clone();
            cx.spawn(async move |this, cx| {
                // The code runs on its extension's thread; this only waits.
                let settings = asking.workspace_configuration(&user).await;
                this.update(cx, |s, _| {
                    if s.generations.get(&key).copied() == generation {
                        s.update_settings(&key, Some(settings), &user);
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// A server's settings again, with what extensions' code said now
    /// (`from_code`); tells it when they changed.
    fn update_settings(&mut self, key: &ServerKey, from_code: Option<Value>, user: &HashMap<String, crate::settings::LspSettings>) {
        let Some((spec, program, config, code)) = self.configs.get_mut(key) else { return };
        if let Some(settings) = from_code {
            code.settings = settings;
        }
        let fresh = servers::config(spec, program, user, code);
        if fresh.settings == config.settings {
            return;
        }
        config.settings = fresh.settings;
        if let Some(ServerState::Running(server)) = self.servers.get(key) {
            server.client.notify("workspace/didChangeConfiguration", json!({ "settings": config.settings }));
        }
    }

    /// Registers again the documents `which` picks (their servers changed).
    fn reregister(&mut self, which: impl Fn(&OpenDocument) -> bool, cx: &mut Context<Self>) {
        let docs: Vec<(Entity<Document>, Option<Entity<Editor>>)> = self
            .documents
            .values()
            .filter(|d| which(d))
            .filter_map(|d| Some((d.doc.upgrade()?, d.editor.as_ref().and_then(WeakEntity::upgrade))))
            .collect();
        self.documents.retain(|_, d| !which(d));
        for (doc, editor) in docs {
            self.register_document(&doc, editor.as_ref(), cx);
        }
    }

    /// Restarts a document's servers (after installing one, or if it's stuck).
    pub fn restart(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(keys) = self.documents.get(path).map(|d| d.servers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()) else { return };
        for key in &keys {
            if let Some(client) = self.client(key) {
                std::thread::spawn(move || client.shutdown());
            }
            self.forget_diagnostics(key, cx);
            self.servers.remove(key);
            self.progress.remove(key);
            self.configs.remove(key);
            self.generations.remove(key);
            // The old process's registrations: its `Closed` is ignored now.
            self.watchers.remove(key);
            self.pull_registrations.remove(key);
        }
        self.reregister(|d| keys.iter().any(|key| d.has(key)), cx);
    }

    /// The main server's state for the status bar; when it's up, the names
    /// of all that are ("typescript-language-server, eslint").
    pub fn status(&self, path: &Path) -> Option<LspStatus> {
        let doc = self.documents.get(path)?;
        let (main, _) = doc.servers.first()?;
        let names = || {
            let names: Vec<String> = doc
                .servers
                .iter()
                .filter_map(|(key, _)| match self.servers.get(key)? {
                    ServerState::Running(server) => Some(server.name.clone()),
                    _ => None,
                })
                .collect();
            names.join(", ")
        };
        Some(match self.servers.get(main)? {
            ServerState::Finding | ServerState::Starting(..) => LspStatus::Starting,
            ServerState::Running(server) => match doc.servers.iter().find_map(|(key, _)| self.progress.get(key)?.values().next()) {
                Some(work) => LspStatus::Busy(format!("{}: {}", server.name, work.label())),
                None => LspStatus::Ready(names()),
            },
            ServerState::Missing(spec) => match install::for_spec(spec) {
                Some(installer) if install::installing(installer.command) => LspStatus::Installing(installer.command),
                installer => LspStatus::Missing { command: spec.install, installer },
            },
            ServerState::Failed(message) => LspStatus::Failed(message.clone()),
        })
    }

    /// Stops every server (another project, the window closing, trust withdrawn), all at once
    /// and off the main thread.
    pub fn shutdown_all(&mut self) {
        let clients: Vec<Arc<Client>> = self.servers.keys().cloned().collect::<Vec<_>>().iter().filter_map(|k| self.client(k)).collect();
        self.servers.clear();
        self.configs.clear();
        self.generations.clear();
        self.documents.clear();
        for client in clients {
            std::thread::spawn(move || client.shutdown());
        }
    }

    /// The app is quitting: every server process still running (shutting
    /// down already or not) gets `shutdown` and `exit` at once and a moment
    /// to go, then is killed, so none outlives Kuana. Blocks for about
    /// `QUIT_WAIT`.
    fn stop_all_for_quit(&mut self) {
        let clients: Vec<Arc<Client>> = self.started.drain(..).filter_map(|c| c.upgrade()).collect();
        let stopping: Vec<_> = clients.into_iter().map(|client| std::thread::spawn(move || client.stop(QUIT_WAIT))).collect();
        for thread in stopping {
            let _ = thread.join();
        }
    }
}

/// Lets the executor run what else is waiting before the task goes on: not
/// ready the first time it's polled (asking to be polled again), ready the next.
struct YieldNow(bool);

impl std::future::Future for YieldNow {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.0 {
            return std::task::Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}

fn workspace_folders(root: &Path) -> Value {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    json!([{ "uri": path_to_uri(root), "name": name }])
}

/// What Kuana tells servers it can do.
fn initialize_params(root: &Path, initialization_options: Value) -> Value {
    let mut params = json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "Kuana", "version": env!("CARGO_PKG_VERSION") },
        "rootUri": path_to_uri(root),
        "rootPath": root.to_string_lossy(),
        "workspaceFolders": workspace_folders(root),
        "capabilities": {
            "general": { "positionEncodings": ["utf-8", "utf-16"] },
            "workspace": {
                "applyEdit": true,
                "workspaceEdit": { "documentChanges": true },
                "configuration": true,
                "workspaceFolders": true,
                "didChangeWatchedFiles": { "dynamicRegistration": true, "relativePatternSupport": true },
                // Hints and lenses are asked for again when it says they changed.
                "inlayHint": { "refreshSupport": true },
                "codeLens": { "refreshSupport": true },
                "diagnostics": { "refreshSupport": true },
                "fileOperations": { "willRename": true, "didRename": true },
            },
            "window": { "workDoneProgress": true },
            "textDocument": {
                "synchronization": { "didSave": true },
                "completion": {
                    "completionItem": {
                        "snippetSupport": true,
                        "labelDetailsSupport": true,
                        "resolveSupport": { "properties": ["additionalTextEdits", "detail", "documentation"] },
                    },
                    "contextSupport": true,
                },
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "signatureHelp": {
                    "signatureInformation": {
                        "parameterInformation": { "labelOffsetSupport": true },
                        "activeParameterSupport": true,
                    },
                },
                "definition": { "linkSupport": true },
                "references": {},
                "diagnostic": { "dynamicRegistration": true, "relatedDocumentSupport": true },
                "callHierarchy": {},
                "typeHierarchy": {},
                "rename": { "prepareSupport": false },
                "codeAction": {
                    "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [
                        "", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite",
                        "source", "source.organizeImports",
                    ] } },
                    "resolveSupport": { "properties": ["edit"] },
                    "dataSupport": true,
                    "isPreferredSupport": true,
                },
                "publishDiagnostics": {},
                "inlayHint": {},
                "codeLens": {},
            },
            // Commands Kuana runs itself, so servers send code lenses that use
            // them (rust-analyzer's Run, Debug and references lenses).
            "experimental": { "commands": { "commands": CLIENT_COMMANDS } },
        },
    });
    if !initialization_options.is_null() {
        params["initializationOptions"] = initialization_options;
    }
    params
}

/// Code lens commands the editor carries out (see `Editor::run_code_lens`).
pub const CLIENT_COMMANDS: &[&str] = &[
    "rust-analyzer.runSingle",
    "rust-analyzer.debugSingle",
    "rust-analyzer.showReferences",
    "rust-analyzer.gotoLocation",
    "editor.action.showReferences",
];

impl Default for LspStore {
    fn default() -> Self {
        Self::new()
    }
}

/// How a file changed on disk, as `workspace/didChangeWatchedFiles` numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

/// The servers' file watchers, by server (`LspStore::file_watchers`).
#[derive(Clone, Default)]
pub struct FileWatchers(Vec<(ServerKey, Vec<FileWatcher>)>);

impl FileWatchers {
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|(_, watchers)| watchers.is_empty())
    }

    /// Each server's `workspace/didChangeWatchedFiles` parameters: the
    /// changes it watches, for those that watch any.
    pub fn events(&self, changes: &[(PathBuf, FileChange)]) -> Vec<(ServerKey, Value)> {
        let mut events = Vec::new();
        for (key, watchers) in &self.0 {
            let changes: Vec<Value> = changes
                .iter()
                .filter(|(path, change)| watchers.iter().any(|w| w.matches(path, *change)))
                .map(|(path, change)| json!({ "uri": path_to_uri(path), "type": *change as u8 }))
                .collect();
            if !changes.is_empty() {
                events.push((key.clone(), json!({ "changes": changes })));
            }
        }
        events
    }
}

/// One of a server's file watchers: a glob, relative to `base` when given
/// (else matched against the whole path), and which changes it wants.
#[derive(Clone)]
struct FileWatcher {
    base: Option<PathBuf>,
    glob: globset::GlobMatcher,
    /// Bits: 1 created, 2 changed, 4 deleted.
    kind: u64,
}

impl FileWatcher {
    fn parse(watcher: &Value) -> Option<Self> {
        let pattern = watcher.get("globPattern")?;
        let (base, glob) = match pattern {
            Value::String(glob) => (None, glob.as_str()),
            // A RelativePattern: a base folder (a URI or a workspace folder) and a glob.
            _ => {
                let base = pattern.get("baseUri").and_then(|b| b.as_str().or_else(|| b.get("uri")?.as_str()))?;
                // Its real path, as the watcher's are (see `registered_path`).
                let base = crate::document_store::key(&protocol::uri_to_path(base)?);
                (Some(base), pattern.get("pattern")?.as_str()?)
            }
        };
        let glob = globset::GlobBuilder::new(glob).literal_separator(true).build().ok()?.compile_matcher();
        Some(Self { base, glob, kind: watcher.get("kind").and_then(Value::as_u64).unwrap_or(7) })
    }

    fn matches(&self, path: &Path, change: FileChange) -> bool {
        let bit = match change {
            FileChange::Created => 1,
            FileChange::Changed => 2,
            FileChange::Deleted => 4,
        };
        if self.kind & bit == 0 {
            return false;
        }
        match &self.base {
            Some(base) => path.strip_prefix(base).is_ok_and(|rel| self.glob.is_match(rel)),
            None => self.glob.is_match(path),
        }
    }
}

fn rename_params(renames: &[(PathBuf, PathBuf, bool)]) -> Value {
    let files: Vec<Value> = renames.iter().map(|(old, new, _)| json!({ "oldUri": path_to_uri(old), "newUri": path_to_uri(new) })).collect();
    json!({ "files": files })
}

/// Whether a server's `FileOperationFilter`s cover `path`: a `file` scheme
/// (or none), its glob over the whole path, and `matches` (file or folder)
/// when it says.
fn file_operation_matches(filters: &Value, path: &Path, is_dir: bool) -> bool {
    let Some(filters) = filters.as_array() else { return false };
    let text = path.to_string_lossy().replace('\\', "/");
    filters.iter().any(|filter| {
        if filter.get("scheme").and_then(Value::as_str).is_some_and(|s| s != "file") {
            return false;
        }
        let pattern = &filter["pattern"];
        match pattern.get("matches").and_then(Value::as_str) {
            Some("file") if is_dir => return false,
            Some("folder") if !is_dir => return false,
            _ => {}
        }
        let Some(glob) = pattern.get("glob").and_then(Value::as_str) else { return false };
        let ignore_case = pattern.pointer("/options/ignoreCase").and_then(Value::as_bool).unwrap_or(false);
        globset::GlobBuilder::new(glob).literal_separator(true).case_insensitive(ignore_case).build().is_ok_and(|g| g.compile_matcher().is_match(&text))
    })
}

/// A language server for tests: `python3 -c FAKE_SERVER <name> <trigger chars>`.
/// It publishes one diagnostic per opened file ("<name> problem", followed
/// by its initialization option `greeting` and the `mode` in its `fake`
/// settings section when it has them), completes with "<name>-item", offers "<name> fix
/// (<diagnostics sent>)", puts every declaration at line 0, column 3, and finds two usages.
/// Named "hints", it also gives inlay hints (": hint <how many it was asked for>") at the
/// end of the lines asked for whose number divides by 100, and on a save checks the file as
/// rust-analyzer does ("hints checked <first line>") and says its hints changed.
/// Named "lenses", its code lenses are on every 100th line.
/// Named "respell", it names the file in its answers as other servers may.
/// Named "goodbye", it notes being told to shut down: a `<file>.shutdown` beside its file.
/// Named "asks", it sends a request Kuana doesn't handle and a progress token's, and
/// adds the first's error code and whether the second was accepted to its problem.
/// Named "resolve", it completes with "foo", whose edits besides (`use foo;` on the first
/// line, ` // end` at the end of the completion's line) come only on `completionItem/resolve`,
/// which it answers after a pause. Like rust-analyzer, it gives them only while the text is
/// the version the item was made for, and when no change came while it worked.
/// Named "pull", it publishes nothing: asked (`textDocument/diagnostic`), it reports
/// "pulled v<version> <first line>", and an unchanged report for the version it last
/// reported. Named "renames", it asks before `.rs` files are renamed and answers with an
/// edit putting `// moved` at the top of the old file; told of a rename, it publishes
/// "renames heard <new name>".
#[cfg(test)]
pub(crate) const FAKE_SERVER: &str = r#"
import sys, json, time, queue, threading
from urllib.parse import unquote, urlparse
name = sys.argv[1]
triggers = [t for t in sys.argv[2].split(",") if t] if len(sys.argv) > 2 else []
def spelled(uri):
    # Named "respell", it names files as other servers may: vscode-uri's
    # lowercase drive letter and encoded colon, or not through macOS's /private.
    if name != "respell":
        return uri
    uri = uri.replace("file:///private/", "file:///")
    if uri[8:9].isalpha() and uri[9:10] == ":":
        uri = uri[:8] + uri[8].lower() + "%3A" + uri[10:]
    return uri
# Messages are read as they come, so a handler can see what came meanwhile.
inbox = queue.Queue()
def reader():
    while True:
        headers = {}
        while True:
            line = sys.stdin.buffer.readline()
            if not line:
                inbox.put(None)
                return
            line = line.strip()
            if not line:
                break
            k, v = line.split(b":", 1)
            headers[k.lower()] = v.strip()
        inbox.put(json.loads(sys.stdin.buffer.read(int(headers[b"content-length"]))))
threading.Thread(target=reader, daemon=True).start()
def read():
    m = inbox.get()
    if m is None:
        sys.exit(0)
    return m
def changed_meanwhile():
    with inbox.mutex:
        waiting = list(inbox.queue)
    return any(m and m.get("method") == "textDocument/didChange" for m in waiting + held)
def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()
# What came while it waited for an answer, handled after.
held = []
def answer_to(ids):
    while True:
        m = read()
        if m.get("id") in ids and "method" not in m:
            return m
        held.append(m)
init = {}
last_uri = None
text = ""
version = 0
hint_requests = 0
while True:
    m = held.pop(0) if held else read()
    method, id = m.get("method"), m.get("id")
    if method is None:
        continue
    if method == "initialized":
        # Watches TOML files, and tells (as a diagnostic) what it hears of them.
        watchers = [{"globPattern": "**/*.toml"}]
        send({"jsonrpc": "2.0", "id": "reg", "method": "client/registerCapability", "params": {"registrations": [{"id": "w", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": watchers}}]}})
    elif method == "workspace/didChangeWatchedFiles" and last_uri:
        heard = ", ".join("%d %s" % (c["type"], c["uri"].rsplit("/", 1)[-1]) for c in m["params"]["changes"])
        problem = {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 3, "message": name + " watched " + heard}
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": last_uri, "diagnostics": [problem]}})
    elif method == "initialize":
        init = m["params"].get("initializationOptions") or {}
        caps = {"textDocumentSync": 1, "completionProvider": {"triggerCharacters": triggers}, "codeActionProvider": True, "codeLensProvider": {"resolveProvider": True}, "definitionProvider": True, "referencesProvider": True}
        if name == "hints":
            caps["inlayHintProvider"] = True
            caps["textDocumentSync"] = {"openClose": True, "change": 1, "save": True}
        if name == "resolve":
            caps["completionProvider"]["resolveProvider"] = True
        if name == "pull":
            caps["diagnosticProvider"] = {"identifier": "fake", "interFileDependencies": False, "workspaceDiagnostics": False}
        if name == "renames":
            rs = {"filters": [{"scheme": "file", "pattern": {"glob": "**/*.rs", "matches": "file"}}]}
            caps["workspace"] = {"fileOperations": {"willRename": rs, "didRename": rs}}
        send({"jsonrpc": "2.0", "id": id, "result": {"capabilities": caps}})
    elif method == "textDocument/diagnostic":
        if m["params"].get("previousResultId") == str(version):
            send({"jsonrpc": "2.0", "id": id, "result": {"kind": "unchanged", "resultId": str(version)}})
        else:
            problem = {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 1, "message": "pulled v%d %s" % (version, text.split("\n", 1)[0])}
            send({"jsonrpc": "2.0", "id": id, "result": {"kind": "full", "resultId": str(version), "items": [problem]}})
    elif method == "workspace/willRenameFiles":
        old = m["params"]["files"][0]["oldUri"]
        top = {"line": 0, "character": 0}
        send({"jsonrpc": "2.0", "id": id, "result": {"changes": {old: [{"range": {"start": top, "end": top}, "newText": "// moved\n"}]}}})
    elif method == "workspace/didRenameFiles" and last_uri:
        heard = m["params"]["files"][0]["newUri"].rsplit("/", 1)[-1]
        problem = {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 3, "message": "renames heard " + heard}
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": last_uri, "diagnostics": [problem]}})
    elif method == "textDocument/didChange":
        text = m["params"]["contentChanges"][-1]["text"]
        version = m["params"]["textDocument"]["version"]
    elif method == "textDocument/didSave" and last_uri:
        problem = {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 2, "message": name + " checked " + text.split("\n", 1)[0]}
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": last_uri, "diagnostics": [problem]}})
        if name == "hints":
            send({"jsonrpc": "2.0", "id": "refresh", "method": "workspace/inlayHint/refresh", "params": None})
    elif method == "textDocument/inlayHint":
        hint_requests += 1
        lines = text.split("\n")
        asked = m["params"]["range"]
        rows = range(asked["start"]["line"], min(asked["end"]["line"] + 1, len(lines)))
        hints = [{"position": {"line": n, "character": len(lines[n])}, "label": ": hint %d" % hint_requests} for n in rows if n % 100 == 0]
        send({"jsonrpc": "2.0", "id": id, "result": hints})
    elif method == "textDocument/didOpen":
        uri = m["params"]["textDocument"]["uri"]
        last_uri = uri
        text = m["params"]["textDocument"]["text"]
        version = m["params"]["textDocument"]["version"]
        # Asks for its whole section and reads `mode` in it, as basedpyright
        # asks for "basedpyright" and reads its "analysis".
        send({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": [{"section": "fake"}]}})
        section = answer_to(["cfg"])["result"][0] or {}
        extra = [str(v) for v in (init.get("greeting"), section.get("mode")) if v is not None]
        if name == "asks":
            # A request Kuana doesn't handle, and one it says it does.
            send({"jsonrpc": "2.0", "id": "odd", "method": "kuana/noSuchRequest", "params": {}})
            send({"jsonrpc": "2.0", "id": "wdp", "method": "window/workDoneProgress/create", "params": {"token": "t"}})
            answers = {}
            while len(answers) < 2:
                answer = answer_to(["odd", "wdp"])
                answers[answer["id"]] = answer
            extra += [str(answers["odd"].get("error", {}).get("code")), "progress " + ("refused" if "error" in answers["wdp"] else "ok")]
        message = " ".join([name + " problem"] + extra)
        problem = {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 1, "message": message}
        if name != "pull":
            send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": spelled(uri), "diagnostics": [problem]}})
    elif method == "textDocument/codeLens":
        # Unresolved: the title comes with codeLens/resolve.
        lines = range(0, len(text.split("\n")) - 1, 100) if name == "lenses" else [0]
        lenses = [{"range": {"start": {"line": n, "character": 0}, "end": {"line": n, "character": 1}}, "data": n} for n in lines]
        send({"jsonrpc": "2.0", "id": id, "result": lenses})
    elif method == "codeLens/resolve":
        lens = m["params"]
        uri = last_uri or ""
        location = {"uri": uri, "range": lens["range"]}
        lens["command"] = {"title": name + " lens", "command": "editor.action.showReferences", "arguments": [uri, lens["range"]["start"], [location]]}
        send({"jsonrpc": "2.0", "id": id, "result": lens})
    elif method == "textDocument/definition":
        uri = spelled(m["params"]["textDocument"]["uri"])
        # Nothing from the third line on has a declaration.
        if m["params"]["position"]["line"] >= 2:
            send({"jsonrpc": "2.0", "id": id, "result": None})
        else:
            send({"jsonrpc": "2.0", "id": id, "result": {"uri": uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}}})
    elif method == "textDocument/references":
        uri = spelled(m["params"]["textDocument"]["uri"])
        at = lambda line, ch: {"uri": uri, "range": {"start": {"line": line, "character": ch}, "end": {"line": line, "character": ch + 1}}}
        send({"jsonrpc": "2.0", "id": id, "result": [at(0, 3), at(1, 9)]})
    elif method == "textDocument/completion":
        line = m["params"]["position"]["line"]
        items = [{"label": "foo", "data": {"version": version, "line": line}}] if name == "resolve" else [{"label": name + "-item"}]
        send({"jsonrpc": "2.0", "id": id, "result": items})
    elif method == "completionItem/resolve" and name == "resolve":
        item = m["params"]
        if item["data"]["version"] == version:
            time.sleep(0.4)
        # A change that came meanwhile spoils the work, as in rust-analyzer.
        if item["data"]["version"] == version and not changed_meanwhile():
            top = {"line": 0, "character": 0}
            line = item["data"]["line"]
            end = {"line": line, "character": len(text.split("\n")[line])}
            item["additionalTextEdits"] = [{"range": {"start": top, "end": top}, "newText": "use foo;\n"}, {"range": {"start": end, "end": end}, "newText": " // end"}]
        send({"jsonrpc": "2.0", "id": id, "result": item})
    elif method == "textDocument/codeAction":
        sent = len(m["params"]["context"]["diagnostics"])
        send({"jsonrpc": "2.0", "id": id, "result": [{"title": "%s fix (%d)" % (name, sent)}]})
    elif method == "shutdown":
        # Named "goodbye", it leaves a note beside its file when told to shut down.
        if name == "goodbye" and last_uri:
            path = unquote(urlparse(last_uri).path)
            open((path[1:] if path[2:3] == ":" else path) + ".shutdown", "w").close()
        send({"jsonrpc": "2.0", "id": id, "result": None})
    elif method == "exit":
        sys.exit(0)
    elif id is not None:
        send({"jsonrpc": "2.0", "id": id, "result": None})
"#;

/// A spec that runs [`FAKE_SERVER`] (leaked; tests only).
#[cfg(test)]
pub(crate) fn fake_server(name: &str, triggers: &str) -> &'static servers::ServerSpec {
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let args: &'static [&'static str] = Box::leak(vec!["-c", FAKE_SERVER, leak(name.to_string()), leak(triggers.to_string())].into_boxed_slice());
    let candidates: &'static [(&'static str, &'static [&'static str])] = Box::leak(vec![("python3", args)].into_boxed_slice());
    Box::leak(Box::new(servers::ServerSpec { id: leak(format!("fake-{name}")), candidates, ..servers::SPEC }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn files_outside_the_project_use_its_server(cx: &mut gpui::TestAppContext) {
        let spec = fake_server("outside", "");
        let mut store = LspStore::new();
        store.test_specs = Some(vec![spec]);
        store.set_root(Some(PathBuf::from("/work/project")));
        let root_of = |store: &LspStore, path: &str, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| store.server_keys(Language::Rust, Path::new(path), cx))[0].0.root.clone()
        };
        // Nothing running: a dependency gets a root of its own.
        assert_eq!(root_of(&store, "/deps/serde/src/lib.rs", cx), PathBuf::from("/deps/serde/src"));
        // The project's server is running: the dependency goes to it.
        store.servers.insert(ServerKey { id: spec.id.to_string(), root: PathBuf::from("/work/project") }, ServerState::Finding);
        assert_eq!(root_of(&store, "/deps/serde/src/lib.rs", cx), PathBuf::from("/work/project"));
        assert_eq!(root_of(&store, "/work/project/src/main.rs", cx), PathBuf::from("/work/project"));
    }

    #[gpui::test]
    fn documents_without_a_tab_reach_their_servers(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        use crate::document::{Document, EditKind};
        if servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-tabless-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (shown, hidden) = (dir.join("a.rs"), dir.join("b.rs"));
        std::fs::write(&shown, "fn a() {}\n").unwrap();
        std::fs::write(&hidden, "fn old() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("hints", "")]);
            store.set_root(Some(dir.clone()));
            store
        });
        let wait = |cx: &mut gpui::TestAppContext, what: &str, done: &dyn Fn(&mut gpui::TestAppContext) -> bool| {
            let started = std::time::Instant::now();
            while !done(cx) {
                assert!(started.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for {what}");
                std::thread::sleep(std::time::Duration::from_millis(20));
                cx.run_until_parked();
            }
        };
        // Without a running server, a document without a tab starts none.
        let doc = cx.new(|_| Document::open(&hidden).unwrap());
        store.update(cx, |s, cx| s.register_document(&doc, None, cx));
        assert!(store.read_with(cx, |s, _| s.servers.is_empty() && s.documents.is_empty()));
        // A tab starts it.
        let editor = cx.new(|cx| Editor::new(Document::open(&shown).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait(cx, "the server", &|cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));

        // Now the tab-less one goes to it, and its edits follow.
        store.update(cx, |s, cx| s.register_document(&doc, None, cx));
        // (The fake server reads nothing else until it has its configuration.)
        wait(cx, "its first diagnostics", &|cx| store.read_with(cx, |s, _| s.diagnostics.keys().any(|(p, _)| *p == hidden)));
        doc.update(cx, |d, _| d.edit(vec![(3..6, "new".into())], &[], EditKind::Other));
        store.update(cx, |s, cx| s.send_edits(&doc, cx));
        let rope = doc.read_with(cx, |d, _| d.rope().clone());
        store.read_with(cx, |s, _| s.did_save(&hidden, &rope));
        cx.run_until_parked();
        // What its server says waits for a tab, which gets it.
        assert!(doc.read_with(cx, |d, _| d.diagnostics.is_empty()));
        let tab = cx.new(|cx| Editor::with_document(doc.clone(), cx));
        wait(cx, "the server's check", &|cx| {
            store.update(cx, |s, cx| s.register(&tab, cx));
            doc.read_with(cx, |d, _| d.diagnostics.iter().any(|d| &*d.message == "hints checked fn new() {}"))
        });

        // Gone with its document.
        drop((tab, doc));
        cx.update(|_| {});
        cx.run_until_parked();
        assert!(store.read_with(cx, |s, _| !s.documents.contains_key(&hidden) && s.documents.contains_key(&shown)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_operation_filters_match_paths() {
        let filters = json!([
            { "scheme": "file", "pattern": { "glob": "**/*.rs", "matches": "file" } },
            { "pattern": { "glob": "**/src", "matches": "folder" } },
            { "scheme": "untitled", "pattern": { "glob": "**/*.py" } },
            { "pattern": { "glob": "**/*.TS", "options": { "ignoreCase": true } } },
        ]);
        let matches = |path: &str, dir| file_operation_matches(&filters, Path::new(path), dir);
        assert!(matches("/work/p/src/util.rs", false));
        assert!(!matches("/work/p/src/util.rs", true), "a folder named like a file");
        assert!(matches("/work/p/src", true));
        assert!(!matches("/work/p/src/x.py", false), "another scheme");
        assert!(matches("/work/p/app.ts", false));
        assert!(!matches("/work/p/README.md", false));
        assert!(!file_operation_matches(&Value::Null, Path::new("/a.rs"), false));
    }

    /// A server with pull diagnostics is asked for them when a document
    /// opens and again after an edit; an unchanged report keeps them.
    #[gpui::test]
    fn pulled_diagnostics_follow_edits(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        use crate::document::{Document, EditKind};
        if servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-pull-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("pull", "")]);
            store.set_root(Some(dir.clone()));
            store
        });
        let editor = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let message = |cx: &mut gpui::TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.first().map(|d| d.message.to_string()));
        let wait = |cx: &mut gpui::TestAppContext, want: &str| {
            let started = std::time::Instant::now();
            while message(cx).as_deref() != Some(want) {
                assert!(started.elapsed() < std::time::Duration::from_secs(10), "waited for {want:?}, have {:?}", message(cx));
                std::thread::sleep(std::time::Duration::from_millis(20));
                cx.executor().advance_clock(PULL_DELAY);
                cx.run_until_parked();
            }
        };
        let version = |cx: &mut gpui::TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).version());
        let first = version(cx);
        wait(cx, &format!("pulled v{first} fn a() {{}}"));

        let doc = editor.read_with(cx, |e, _| e.doc.clone());
        doc.update(cx, |d, _| d.edit(vec![(3..4, "b".into())], &[], EditKind::Other));
        store.update(cx, |s, cx| s.send_edits(&doc, cx));
        let second = version(cx);
        wait(cx, &format!("pulled v{second} fn b() {{}}"));
        // Asked again with nothing new, it says unchanged: they stay.
        store.update(cx, |s, cx| s.pull_soon(&path, cx));
        cx.executor().advance_clock(PULL_DELAY);
        std::thread::sleep(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        assert_eq!(message(cx), Some(format!("pulled v{second} fn b() {{}}")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A server that asks before renames gives its edits, and hears of the
    /// rename after.
    #[gpui::test]
    fn renames_ask_servers_first(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        use crate::document::Document;
        if servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-renames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("a.rs"), dir.join("b.rs"));
        std::fs::write(&old, "fn a() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("renames", "")]);
            store.set_root(Some(dir.clone()));
            store
        });
        let editor = cx.new(|cx| Editor::new(Document::open(&old).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let started = std::time::Instant::now();
        while !store.read_with(cx, |s, _| s.running_servers().len() == 1) {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "the server didn't start");
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        // Markdown isn't in its filters; Rust is.
        let renames = |old: &Path, new: &Path| [(old.to_path_buf(), new.to_path_buf(), false)];
        assert!(store.read_with(cx, |s, cx| s.will_rename(&renames(&dir.join("a.md"), &dir.join("b.md")), cx)).is_none());
        let asking = store.read_with(cx, |s, cx| s.will_rename(&renames(&old, &new), cx)).unwrap();
        let answer = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = answer.clone();
        cx.spawn(async move |_| *slot.borrow_mut() = Some(asking.await)).detach();
        let started = std::time::Instant::now();
        while answer.borrow().is_none() {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "no answer");
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        let edits = answer.borrow_mut().take().unwrap().expect("answered in time");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].edits[0].path, old);
        assert_eq!(edits[0].edits[0].edits[0].new_text, "// moved\n");

        store.read_with(cx, |s, _| s.did_rename(&renames(&old, &new)));
        let started = std::time::Instant::now();
        while !editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().any(|d| &*d.message == "renames heard b.rs")) {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "didRenameFiles not heard");
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A closing window drops its store: its servers are told to shut down
    /// (and so end what they started), not only killed.
    #[gpui::test]
    fn a_dropped_store_shuts_its_servers_down(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        use crate::document::Document;
        if servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-goodbye-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("goodbye", "")]);
            store
        });
        let editor = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let started = std::time::Instant::now();
        while !editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()) {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for the server");
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        drop(store);
        cx.update(|_| {});
        let note = dir.join("a.rs.shutdown");
        let started = std::time::Instant::now();
        while !note.exists() {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "the server wasn't told to shut down");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A request Kuana doesn't handle gets MethodNotFound (a `null` result
    /// would read as done), one it handles its answer.
    #[gpui::test]
    fn unknown_requests_are_answered_method_not_found(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        use crate::document::Document;
        if servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-asks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("asks", "")]);
            store
        });
        let editor = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let started = std::time::Instant::now();
        while !editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()) {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for the server");
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        let message = editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics[0].message.to_string());
        assert_eq!(message, format!("asks problem {} progress ok", rpc::METHOD_NOT_FOUND));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A server indexing reports progress many times a second; each showing
    /// redraws the workspace, so reports in quick succession show together.
    #[gpui::test]
    fn progress_shows_at_most_every_so_often(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        let store = cx.new(|_| LspStore::new());
        let shown = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = shown.clone();
        let _observer = cx.update(|cx| cx.observe(&store, move |_, _| count.set(count.get() + 1)));
        let key = ServerKey { id: "s".into(), root: PathBuf::from("/w") };
        let send = |kind: &str, percentage: u64, cx: &mut gpui::TestAppContext| {
            let params = json!({ "token": "t", "value": { "kind": kind, "title": "Indexing", "percentage": percentage } });
            store.update(cx, |s, cx| s.handle(&key, Incoming::Notification { method: "$/progress".into(), params }, cx));
        };
        // Starting shows at once; the reports right after it wait.
        send("begin", 0, cx);
        assert_eq!(shown.get(), 1);
        let started = Instant::now();
        for percentage in 1..=50 {
            send("report", percentage, cx);
        }
        if started.elapsed() < PROGRESS_THROTTLE {
            assert_eq!(shown.get(), 1);
        }
        // The last one shows once the pause is over.
        cx.executor().advance_clock(PROGRESS_THROTTLE);
        cx.run_until_parked();
        let after_pause = shown.get();
        assert!((2..=3).contains(&after_pause), "{after_pause}");
        assert_eq!(store.read_with(cx, |s, _| s.progress[&key]["t"].percentage), Some(50));
        // Ending shows at once.
        send("end", 0, cx);
        assert_eq!(shown.get(), after_pause + 1);
        assert!(store.read_with(cx, |s, _| s.progress[&key].is_empty()));
    }

    /// rust-analyzer sends the same error twice in a row; it shows once. Other
    /// errors, and the same one from another server, still show.
    #[gpui::test]
    fn a_repeated_server_error_shows_once(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        let store = cx.new(|_| LspStore::new());
        let messages = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = messages.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&store, move |_, event: &LspEvent, _| {
                if let LspEvent::Message(text) = event {
                    seen.borrow_mut().push(text.clone());
                }
            })
        });
        let show = |root: &str, kind: u64, text: &str, cx: &mut gpui::TestAppContext| {
            let key = ServerKey { id: "s".into(), root: PathBuf::from(root) };
            let params = json!({ "type": kind, "message": text });
            store.update(cx, |s, cx| s.handle(&key, Incoming::Notification { method: "window/showMessage".into(), params }, cx));
        };
        show("/w", 2, "Failed to discover workspace", cx);
        show("/w", 1, "Failed to discover workspace", cx);
        show("/w", 1, "Failed to discover workspace", cx);
        show("/w", 1, "Failed to load workspaces", cx);
        show("/other", 1, "Failed to load workspaces", cx);
        assert_eq!(*messages.borrow(), ["Failed to discover workspace", "Failed to load workspaces", "Failed to load workspaces"]);
    }

    /// The same error shows again once `MESSAGE_REPEAT` has passed since it
    /// last showed, however often it came in between.
    #[test]
    fn a_server_error_shows_again_after_a_while() {
        let key = ServerKey { id: "s".into(), root: PathBuf::from("/w") };
        let mut shown = HashMap::new();
        let start = Instant::now();
        let at = |seconds: f32| start + Duration::from_secs_f32(seconds);
        assert!(!repeats_message(&mut shown, &key, "broken", at(0.)));
        assert!(repeats_message(&mut shown, &key, "broken", at(0.1)));
        assert!(repeats_message(&mut shown, &key, "broken", at(9.)));
        assert!(!repeats_message(&mut shown, &key, "broken", at(10.)));
        assert!(repeats_message(&mut shown, &key, "broken", at(15.)));
        // Something else in between: the first one is new again.
        assert!(!repeats_message(&mut shown, &key, "other", at(16.)));
        assert!(!repeats_message(&mut shown, &key, "broken", at(17.)));
    }

    /// Messages queued up are handled one at a time, with other work let in
    /// between: thousands of files' diagnostics don't freeze the window.
    #[gpui::test]
    fn a_burst_of_messages_lets_other_work_in(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;
        const BURST: usize = 50;
        let store = cx.new(|_| LspStore::new());
        let key = ServerKey { id: "s".into(), root: PathBuf::from("/w") };
        let (tx, rx) = futures::channel::mpsc::unbounded();
        for i in 0..BURST {
            let params = json!({ "uri": format!("file:///nowhere/{i}.rs"), "diagnostics": [] });
            tx.unbounded_send(Incoming::Notification { method: "textDocument/publishDiagnostics".into(), params }).unwrap();
        }
        store.update(cx, |s, cx| {
            s.generations.insert(key.clone(), 1);
            s.listen(key.clone(), 1, rx, cx);
        });
        // Another task looks how far it got each time it runs.
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (slot, weak) = (seen.clone(), store.downgrade());
        cx.spawn(async move |cx| {
            loop {
                let handled = weak.read_with(&cx, |s, _| s.diagnostics.len()).unwrap_or(BURST);
                slot.borrow_mut().push(handled);
                if handled == BURST {
                    break;
                }
                YieldNow(false).await;
            }
        })
        .detach();
        cx.run_until_parked();
        let mut seen = seen.borrow().clone();
        seen.dedup();
        assert_eq!(seen.last(), Some(&BURST));
        assert!(seen.len() > BURST / 2, "it ran in between only at {seen:?}");
    }

    /// The real path of a path a server names is looked up once.
    #[cfg(unix)]
    #[test]
    fn real_paths_of_what_servers_name_are_found_once() {
        let dir = std::env::temp_dir().join(format!("kuana-lsp-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (real, link) = (dir.join("real.rs"), dir.join("link.rs"));
        std::fs::write(&real, "").unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut store = LspStore::new();
        assert_eq!(store.registered_path(link.clone()), real);
        // Known now: the link isn't looked at again.
        std::fs::remove_file(&link).unwrap();
        assert_eq!(store.registered_path(link.clone()), real);
        // A path that isn't there is its own, and isn't kept: it may be made.
        let missing = dir.join("missing.rs");
        assert_eq!(store.registered_path(missing.clone()), missing);
        assert!(!store.real_paths.contains_key(&missing));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A restart drops what the old process registered: the new one
    /// registers its own.
    #[gpui::test]
    fn a_restarted_server_keeps_none_of_the_old_ones_registrations(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        use crate::document::Document;
        if servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("restart", "")]);
            store
        });
        let editor = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let registrations = |cx: &mut gpui::TestAppContext| store.read_with(cx, |s, _| s.watchers.values().map(Vec::len).sum::<usize>());
        let wait = |cx: &mut gpui::TestAppContext, what: &str| {
            let started = std::time::Instant::now();
            while registrations(cx) == 0 {
                assert!(started.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for {what}");
                std::thread::sleep(std::time::Duration::from_millis(20));
                cx.run_until_parked();
            }
        };
        wait(cx, "the server's watchers");
        assert_eq!(registrations(cx), 1);
        store.update(cx, |s, cx| s.restart(&path, cx));
        assert_eq!(registrations(cx), 0);
        wait(cx, "the new server's watchers");
        assert_eq!(registrations(cx), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn advertises_both_encodings() {
        let params = initialize_params(Path::new("/w"), json!({ "a": 1 }));
        assert_eq!(params["initializationOptions"], json!({ "a": 1 }));
        assert!(initialize_params(Path::new("/w"), Value::Null).get("initializationOptions").is_none());
        assert_eq!(params["capabilities"]["general"]["positionEncodings"], json!(["utf-8", "utf-16"]));
        assert_eq!(params["rootUri"], "file:///w");
    }
}
