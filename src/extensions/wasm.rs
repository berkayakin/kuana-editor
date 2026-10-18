//! Zed extensions' own code: `extension.wasm`, a WebAssembly component built
//! against Zed's extension API, run with wasmtime. Kuana runs it for language
//! servers only: the command that starts one (the code may download the
//! server first), its initialization options and its workspace
//! configuration. API 0.6.0 and 0.7.0 share one interface (`wit/`, from Zed's
//! `zed_extension_api` crate); other extensions keep Kuana's guess
//! (`extensions::KNOWN_SERVERS`).
//!
//! The code runs in WASI's sandbox with one folder of its own,
//! `<config>/extensions/work/<id>/` (its current directory, where servers are
//! downloaded to; removed with the extension). Everything else goes through
//! the host functions here, which use what Kuana has: `curl` for HTTP and
//! GitHub, its Node.js and npm, the augmented PATH, the `lsp.<id>` settings
//! (by Zed's server id). Running a program (`process`) needs the project to be
//! trusted. A call may run only so much of the code (`FUEL`), in so much
//! memory (`MEMORY`), for so long (`CALL_DEADLINE`; a program it runs,
//! `COMMAND_TIMEOUT`). Slash commands, context servers, the docs database and
//! debug adapters answer with errors.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdout, Stdio},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

use futures::future::Either;

use serde_json::{Value, json};
use wasmtime::{
    Engine, Store, StoreLimits, StoreLimitsBuilder, Trap,
    component::{Component as WasmComponent, HasSelf, Linker, Resource, ResourceTable},
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::{lsp::servers, settings::LspSettings};

wasmtime::component::bindgen!({
    world: "extension",
    path: "src/extensions/wit/since_v0.6.0",
    with: {
        "worktree": ProjectRoot,
        "zed:extension/http-client/http-response-stream": ResponseStream,
    },
});

use zed::extension::{
    dap::{TcpArguments, TcpArgumentsTemplate},
    github::{GithubRelease, GithubReleaseAsset, GithubReleaseOptions},
    http_client::{HttpMethod, HttpRequest, HttpResponse, RedirectPolicy},
    platform::{Architecture, Os},
    process::Output,
};

/// Whether Kuana runs an extension's code: an `extension.wasm` built for
/// extension API 0.6 or 0.7 (`[lib] version` in its manifest).
pub fn supported(manifest: &toml::Table, dir: &Path) -> bool {
    let version = manifest.get("lib").and_then(|lib| lib.get("version")).and_then(toml::Value::as_str).unwrap_or_default();
    let mut parts = version.split('.');
    matches!((parts.next(), parts.next()), (Some("0"), Some("6" | "7"))) && dir.join("extension.wasm").is_file()
}

/// An extension whose code Kuana runs.
#[derive(Debug)]
pub struct WasmExtension {
    pub id: String,
    /// Its folder, with `extension.wasm`.
    pub dir: PathBuf,
}

/// An extension's servers for a language, which its code starts: Zed's ids
/// for them, in the manifest's order. The first one the code can start runs.
#[derive(Debug)]
pub struct WasmServers {
    pub extension: &'static WasmExtension,
    pub ids: &'static [&'static str],
}

/// Kuana's settings as the extension's code may read them, as of the call.
#[derive(Clone, Default, Debug)]
pub struct CallSettings {
    /// `lsp.<id>`, by Zed's server id.
    pub lsp: HashMap<String, LspSettings>,
    /// Tab sizes by language name (the server's language).
    pub tab_sizes: Vec<(String, usize)>,
}

/// How to start one of an extension's servers, from its code.
#[derive(Debug)]
pub struct Started {
    /// Zed's id for the server (its `lsp.<id>` settings).
    pub server: &'static str,
    pub command: servers::Command,
    pub initialization_options: Option<Value>,
}

/// Whom to ask for a server's workspace configuration when extensions' code
/// has a say: its own extension, when that started it, and the others for
/// its language, which may add to it. Asked again when settings change, since
/// the code reads them (and shapes them as its server wants).
#[derive(Clone, Debug)]
pub struct Asking {
    /// The server's own extension and Zed's id for it.
    pub own: Option<(&'static WasmExtension, &'static str)>,
    pub others: Vec<&'static WasmServers>,
    /// Zed's id for the server, as the others know it.
    pub target: String,
    pub root: PathBuf,
    pub tab_sizes: Vec<(String, usize)>,
}

impl Asking {
    /// The server's workspace configuration from extensions' code, which
    /// runs on their threads (see `WasmExtension::run`): this only waits.
    pub async fn workspace_configuration(&self, lsp: &HashMap<String, LspSettings>) -> Value {
        let settings = CallSettings { lsp: lsp.clone(), tab_sizes: self.tab_sizes.clone() };
        let mut value = Value::Null;
        if let Some((extension, server)) = self.own {
            let (root, call) = (self.root.clone(), settings.clone());
            let answer = extension
                .run(move |extension, instance| {
                    extension.call(instance, &root, &call, &|e, store, project| e.call_language_server_workspace_configuration(store, server, project))
                })
                .await;
            match answer.and_then(json_answer) {
                Ok(own) => value = own.unwrap_or_default(),
                Err(err) => eprintln!("kuana: {} extension, {server}: {err}", extension.id),
            }
        }
        for other in &self.others {
            let more = other
                .additional(&self.target, &self.root, &settings, |e, store, server, target, project| {
                    e.call_language_server_additional_workspace_configuration(store, server, target, project)
                })
                .await;
            servers::merge(&mut value, &more.unwrap_or_default());
        }
        value
    }

    /// What the other extensions add to its initialization options (their
    /// code runs on their threads: this only waits).
    pub async fn additional_options(&self, lsp: &HashMap<String, LspSettings>) -> Value {
        let settings = CallSettings { lsp: lsp.clone(), tab_sizes: self.tab_sizes.clone() };
        let mut value = Value::Null;
        for other in &self.others {
            let more = other
                .additional(&self.target, &self.root, &settings, |e, store, server, target, project| {
                    e.call_language_server_additional_initialization_options(store, server, target, project)
                })
                .await;
            servers::merge(&mut value, &more.unwrap_or_default());
        }
        value
    }
}

type Additional = fn(&Extension, &mut Store<State>, &str, &str, Resource<ProjectRoot>) -> wasmtime::Result<Result<Option<String>, String>>;

impl WasmServers {
    /// Asks the extension's code to start its first server it can, for the
    /// project at `root`. The code may download the server first, which can
    /// take minutes: it runs on the extension's thread, and this only waits.
    pub fn start(&'static self, root: &Path, settings: &CallSettings) -> impl Future<Output = Result<Started, String>> + Send + 'static {
        let (root, settings, ids) = (root.to_path_buf(), settings.clone(), self.ids);
        self.extension.run(move |extension, instance| {
            let mut first_error = None;
            for &server in ids {
                match extension.start(instance, server, &root, &settings) {
                    Ok(started) => return Ok(started),
                    Err(err) => {
                        eprintln!("kuana: {} extension: {err}", extension.id);
                        first_error.get_or_insert(err);
                    }
                }
            }
            Err(first_error.unwrap_or_else(|| format!("the {} extension has no language server", extension.id)))
        })
    }

    /// What this extension's servers add (through `ask`: initialization
    /// options or workspace configuration) to another server's, `target` by
    /// Zed's id: Vue's TypeScript plugin for vtsls, say.
    fn additional(&'static self, target: &str, root: &Path, settings: &CallSettings, ask: Additional) -> impl Future<Output = Option<Value>> + Send + 'static {
        let (target, root, settings, ids) = (target.to_string(), root.to_path_buf(), settings.clone(), self.ids);
        let answer = self.extension.run(move |extension, instance| {
            let mut out = None;
            for &server in ids.iter().filter(|id| **id != target) {
                match extension.call(instance, &root, &settings, &|e, store, project| ask(e, store, server, &target, project)).and_then(json_answer) {
                    Ok(Some(value)) => servers::merge(out.get_or_insert(Value::Null), &value),
                    Ok(None) => {}
                    Err(err) => eprintln!("kuana: {} extension, {server} for {target}: {err}", extension.id),
                }
            }
            Ok(out)
        });
        async move { answer.await.ok().flatten() }
    }
}

impl WasmExtension {
    fn start(&self, instance: &mut Option<Instance>, server: &'static str, root: &Path, settings: &CallSettings) -> Result<Started, String> {
        // The code's own errors go with the server's name.
        let own = |err: String| format!("{server}: {err}");
        let command =
            self.call(instance, root, settings, &|e, store, project| e.call_language_server_command(store, server, project))?.map_err(own)?;
        let initialization_options = json_answer(
            self.call(instance, root, settings, &|e, store, project| e.call_language_server_initialization_options(store, server, project))?,
        )
        .map_err(own)?;
        let work = work_dir(&self.id)?;
        Ok(Started {
            server,
            command: servers::Command { program: program_path(&work, &command.command), args: command.args, env: command.env },
            initialization_options,
        })
    }

    /// Runs `job` on the extension's own thread with its instance (None
    /// until a call makes one), after the jobs sent before it. The future
    /// only waits for the answer: no thread is held while the code waits on
    /// the network or npm.
    fn run<T: Send + 'static>(
        &'static self,
        job: impl FnOnce(&'static WasmExtension, &mut Option<Instance>) -> Result<T, String> + Send + 'static,
    ) -> impl Future<Output = Result<T, String>> + Send + 'static {
        self.run_within(CALL_DEADLINE, job)
    }

    /// `run`, answered with an error after `deadline`. Fuel doesn't count
    /// the time the code waits in Kuana (a download, a program), so a call
    /// stuck there would hold the extension's queue, and with it every
    /// server start for its languages: past the deadline the thread is left
    /// to finish on its own and later calls get a new one (and instance).
    fn run_within<T: Send + 'static>(
        &'static self,
        deadline: Duration,
        job: impl FnOnce(&'static WasmExtension, &mut Option<Instance>) -> Result<T, String> + Send + 'static,
    ) -> impl Future<Output = Result<T, String>> + Send + 'static {
        let (answer, answered) = futures::channel::oneshot::channel();
        let job: Job = Box::new(move |instance| {
            answer.send(job(self, instance)).ok();
        });
        // A job that isn't run (no thread) drops its sender: an error below.
        let generation = worker(&self.id).and_then(|(generation, jobs)| jobs.send(job).ok().map(|()| generation));
        let late = after(deadline);
        let not_running = move || Err(format!("the {} extension's code isn't running", self.id));
        async move {
            match futures::future::select(answered, late).await {
                Either::Left((answer, _)) => answer.unwrap_or_else(|_| not_running()),
                Either::Right((Ok(()), _)) => {
                    if let Some(generation) = generation {
                        abandon(&self.id, generation);
                    }
                    Err(format!("the {} extension's code took over {} s, so Kuana stopped waiting", self.id, deadline.as_secs()))
                }
                // No timer: no deadline.
                Either::Right((Err(_), answered)) => answered.await.unwrap_or_else(|_| not_running()),
            }
        }
    }

    /// Runs `f` on the extension's instance (made first if it has none, or
    /// its `extension.wasm` changed), with the project at `root` as its
    /// worktree. A trap ends the instance; the next call makes a new one.
    /// On the extension's thread (`run`).
    fn call<T>(
        &self,
        slot: &mut Option<Instance>,
        root: &Path,
        settings: &CallSettings,
        f: &dyn Fn(&Extension, &mut Store<State>, Resource<ProjectRoot>) -> wasmtime::Result<T>,
    ) -> Result<T, String> {
        let wasm = self.dir.join("extension.wasm");
        let modified = std::fs::metadata(&wasm).and_then(|m| m.modified()).ok();
        if slot.as_ref().is_none_or(|instance| instance.modified != modified || instance.wasm != wasm) {
            *slot = None;
            *slot = Some(Instance::new(self, wasm, modified)?);
        }
        let instance = slot.as_mut().expect("just made");
        let store = &mut instance.store;
        store.set_fuel(FUEL).map_err(|e| e.to_string())?;
        store.data_mut().call = Some(Call { root: root.to_path_buf(), settings: settings.clone() });
        let project = store.data_mut().table.push(ProjectRoot { root: root.to_path_buf() }).map_err(|e| e.to_string())?;
        let rep = project.rep();
        let result = f(&instance.bindings, store, project);
        store.data_mut().table.delete(Resource::<ProjectRoot>::new_own(rep)).ok();
        store.data_mut().call = None;
        result.map_err(|trap| {
            *slot = None;
            if trap.downcast_ref::<Trap>() == Some(&Trap::OutOfFuel) {
                return format!("the {} extension's code ran too long", self.id);
            }
            format!("the {} extension's code failed: {trap:#}", self.id)
        })
    }
}

/// A JSON answer (`option<string>`) parsed.
fn json_answer(answer: Result<Option<String>, String>) -> Result<Option<Value>, String> {
    match answer? {
        Some(text) => serde_json::from_str(&text).map(Some).map_err(|e| format!("the extension's JSON didn't parse: {e}")),
        None => Ok(None),
    }
}

/// A command the code returned: absolute, or relative to its work folder.
/// A bare name the folder doesn't have is looked up on PATH.
fn program_path(work: &Path, command: &str) -> PathBuf {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let inside = work.join(path);
    if path.components().count() == 1 && !inside.exists() {
        return servers::which(command).unwrap_or(inside);
    }
    inside
}

/// Whether an extension's code loads (`kuana extension check`): it compiles,
/// links against this host and sets itself up, in a work folder of its own
/// that goes afterwards. None of its servers is started.
pub fn check(dir: &Path, id: &str) -> Result<(), String> {
    let work = std::env::temp_dir().join(format!("kuana-check-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let extension = WasmExtension { id: id.to_string(), dir: dir.to_path_buf() };
    let loaded = Instance::in_folder(&extension, dir.join("extension.wasm"), None, work.clone()).map(|_| ());
    std::fs::remove_dir_all(&work).ok();
    loaded
}

/// Where an extension's code keeps its files (and runs).
pub fn work_dir(id: &str) -> Result<PathBuf, String> {
    let dir = work_dir_of(id).ok_or_else(|| format!("\"{id}\" isn't a usable extension id"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make {}: {e}", dir.display()))?;
    Ok(dir)
}

fn work_dir_of(id: &str) -> Option<PathBuf> {
    let usable = !id.is_empty() && !id.contains(['/', '\\']) && !id.starts_with('.');
    usable.then(|| Some(super::extensions_dir()?.join("work").join(id))).flatten()
}

/// An uninstalled extension: its instance ends and its work folder (the
/// servers its code downloaded) goes.
pub fn forget(id: &str) {
    let jobs = WORKERS.lock().unwrap_or_else(|e| e.into_inner()).remove(id).map(|(_, jobs)| jobs);
    let work = work_dir_of(id);
    if let Some(dir) = &work {
        std::fs::remove_dir_all(dir).ok();
    }
    // Its thread ends, and its instance with it, after the jobs it has: what
    // those download goes after them (unless it's in use again by then).
    let id = id.to_string();
    if let Some(jobs) = jobs {
        jobs.send(Box::new(move |instance| {
            *instance = None;
            if let Some(dir) = work.filter(|_| !WORKERS.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id)) {
                std::fs::remove_dir_all(dir).ok();
            }
        }))
        .ok();
    }
}

// ---- Instances -------------------------------------------------------------------------

/// How much of its own code an extension may run per call (about one unit
/// per WebAssembly instruction): far more than finding a server takes, so a
/// loop ends with an error instead of hanging the server's start. Waiting for
/// downloads and npm, in Kuana, doesn't count.
const FUEL: u64 = 10_000_000_000;
/// How large an extension's memory may grow.
const MEMORY: usize = 512 << 20;

static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true).consume_fuel(true);
    Engine::new(&config).expect("wasmtime's default configuration")
});

static LINKER: LazyLock<Result<Linker<State>, String>> = LazyLock::new(|| {
    let mut linker = Linker::new(&ENGINE);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| e.to_string())?;
    Extension::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state).map_err(|e| e.to_string())?;
    Ok(linker)
});

/// How long a call may take, downloads and programs included: longer than
/// a server's download may (`updater::download`, 15 minutes).
const CALL_DEADLINE: Duration = Duration::from_secs(20 * 60);
/// How long a program the code runs (`process`) may take before it's ended.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Each extension's thread, by id, to send it jobs (with a number that tells
/// it from a later one). The thread keeps the extension's instance between
/// calls (its code remembers things, such as where it put a server) and runs
/// one job at a time. A call can wait on the
/// network for minutes (a download, npm): it does so on that thread, not on
/// GPUI's shared ones (a thread per core on Linux), which parsing, linting
/// and search need, and other calls for the extension wait in its queue
/// instead of each holding a thread.
static WORKERS: LazyLock<Mutex<HashMap<String, (u64, mpsc::Sender<Job>)>>> = LazyLock::new(Default::default);

/// A job for an extension's thread, given its instance.
type Job = Box<dyn FnOnce(&mut Option<Instance>) + Send>;

/// The queue of extension `id`'s thread (and its number), started first if
/// it has none; None when no thread could start.
fn worker(id: &str) -> Option<(u64, mpsc::Sender<Job>)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut workers = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((generation, jobs)) = workers.get(id) {
        return Some((*generation, jobs.clone()));
    }
    let (jobs, queue) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name(format!("extension {id}"))
        // wasmtime runs the code on this stack (up to its own 512 KiB limit)
        // and compiles it here first.
        .stack_size(8 << 20)
        .spawn(move || {
            let mut instance = None;
            // Until the extension is forgotten and its last job is done.
            for job in queue {
                // A job that panicked loses its answer (its caller gets an
                // error) and the instance, which may be half way through a call.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut instance))).is_err() {
                    instance = None;
                }
            }
        })
        .ok()?;
    let generation = NEXT.fetch_add(1, Ordering::Relaxed);
    workers.insert(id.to_string(), (generation, jobs.clone()));
    Some((generation, jobs))
}

/// Stops sending extension `id`'s calls to thread `generation`, stuck in a
/// call: it ends once that call and those queued behind it return.
fn abandon(id: &str, generation: u64) {
    let mut workers = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    if workers.get(id).is_some_and(|(current, _)| *current == generation) {
        workers.remove(id);
    }
}

/// Ready after `duration` (an error if there's no timer thread). One thread
/// keeps every deadline; there are only as many as calls under way.
fn after(duration: Duration) -> futures::channel::oneshot::Receiver<()> {
    type Timer = (Instant, futures::channel::oneshot::Sender<()>);
    static TIMERS: LazyLock<Mutex<mpsc::Sender<Timer>>> = LazyLock::new(|| {
        let (add, added) = mpsc::channel::<Timer>();
        let timers = move || {
            let mut waiting: Vec<Timer> = Vec::new();
            loop {
                let now = Instant::now();
                let (due, later) = waiting.drain(..).partition::<Vec<_>, _>(|(at, _)| *at <= now);
                for (_, fire) in due {
                    fire.send(()).ok();
                }
                // Those whose call was answered go too.
                waiting = later.into_iter().filter(|(_, fire)| !fire.is_canceled()).collect();
                let next = match waiting.iter().map(|(at, _)| *at).min() {
                    Some(at) => added.recv_timeout(at.saturating_duration_since(now)),
                    None => added.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                match next {
                    Ok(timer) => waiting.push(timer),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        };
        std::thread::Builder::new().name("extension deadlines".into()).spawn(timers).ok();
        Mutex::new(add)
    });
    let (fire, fired) = futures::channel::oneshot::channel();
    TIMERS.lock().unwrap_or_else(|e| e.into_inner()).send((Instant::now() + duration, fire)).ok();
    fired
}

struct Instance {
    store: Store<State>,
    bindings: Extension,
    wasm: PathBuf,
    modified: Option<SystemTime>,
}

impl Instance {
    /// Compiles and instantiates the extension's code, then lets it set up
    /// (`init-extension`).
    fn new(extension: &WasmExtension, wasm: PathBuf, modified: Option<SystemTime>) -> Result<Self, String> {
        Self::in_folder(extension, wasm, modified, work_dir(&extension.id)?)
    }

    fn in_folder(extension: &WasmExtension, wasm: PathBuf, modified: Option<SystemTime>, work: PathBuf) -> Result<Self, String> {
        let failed = |e: wasmtime::Error| format!("couldn't run the {} extension's code: {e:#}", extension.id);
        let component = WasmComponent::from_file(&ENGINE, &wasm).map_err(failed)?;
        let linker = LINKER.as_ref().map_err(|e| format!("couldn't set up extensions' code: {e}"))?;
        // Its current directory is the work folder (the API reads PWD), which
        // it reaches by that path and as `.`.
        let work_name = work.to_string_lossy().into_owned();
        let mut wasi = WasiCtxBuilder::new();
        wasi.inherit_stdout().inherit_stderr().allow_blocking_current_thread(true).env("PWD", &work_name);
        wasi.preopened_dir(&work, ".", DirPerms::all(), FilePerms::all()).map_err(failed)?;
        wasi.preopened_dir(&work, &work_name, DirPerms::all(), FilePerms::all()).map_err(failed)?;
        let limits = StoreLimitsBuilder::new().memory_size(MEMORY).build();
        let state = State { wasi: wasi.build(), table: ResourceTable::new(), limits, extension: extension.id.clone(), work, call: None };
        let mut store = Store::new(&ENGINE, state);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(FUEL).map_err(failed)?;
        let bindings = Extension::instantiate(&mut store, &component, linker).map_err(failed)?;
        bindings.call_init_extension(&mut store).map_err(failed)?;
        Ok(Instance { store, bindings, wasm, modified })
    }
}

// ---- Host ------------------------------------------------------------------------------

/// What the host functions work with.
pub struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    extension: String,
    /// The extension's own folder (its WASI sandbox).
    work: PathBuf,
    /// The call in progress.
    call: Option<Call>,
}

struct Call {
    /// The project the server is for.
    root: PathBuf,
    settings: CallSettings,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

/// The `worktree` an extension's code gets: the server's project folder.
pub struct ProjectRoot {
    root: PathBuf,
}

/// An HTTP response being read (`fetch-stream`): curl writing the body.
pub struct ResponseStream {
    child: Child,
    body: ChildStdout,
    headers: PathBuf,
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_file(&self.headers).ok();
    }
}

impl State {
    fn root(&self, project: &Resource<ProjectRoot>) -> Result<&Path, String> {
        self.table.get(project).map(|p| p.root.as_path()).map_err(|e| e.to_string())
    }

    /// A path the code names in its work folder, which it can't leave
    /// (through `..`, or a link it made): its folder is made, and must lead
    /// there too.
    fn in_work(&self, path: &str) -> Result<PathBuf, String> {
        let inside = inside(&self.work, path)?;
        let parent = inside.parent().filter(|_| inside != self.work).unwrap_or(&self.work);
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        self.check_real(parent, path)?;
        Ok(inside)
    }

    fn check_real(&self, path: &Path, named: &str) -> Result<(), String> {
        check_real(&self.work, path, named)
    }
}

/// That `path` really is in `work` (no link leads out).
fn check_real(work: &Path, path: &Path, named: &str) -> Result<(), String> {
    let real = |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()));
    if real(path)?.starts_with(real(work)?) { Ok(()) } else { Err(format!("{named} is outside the extension's folder")) }
}

/// `path` (relative to `base`, or absolute in it) inside `base`, unless `..`
/// leads out.
fn inside(base: &Path, path: &str) -> Result<PathBuf, String> {
    let given = Path::new(path);
    let relative = match given.strip_prefix(base) {
        Ok(rest) => rest,
        Err(_) if given.is_absolute() => return Err(format!("{path} is outside {}", base.display())),
        Err(_) => given,
    };
    let mut out = base.to_path_buf();
    let mut depth = 0;
    for part in relative.components() {
        match part {
            Component::Normal(name) => {
                out.push(name);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => {
                out.pop();
                depth -= 1;
            }
            _ => return Err(format!("{path} is outside {}", base.display())),
        }
    }
    Ok(out)
}

impl ExtensionImports for State {
    fn get_settings(&mut self, _location: Option<SettingsLocation>, category: String, key: Option<String>) -> Result<String, String> {
        let settings = self.call.as_ref().map(|c| &c.settings).ok_or("settings are only read during a call")?;
        match category.as_str() {
            "lsp" => {
                let entry = key.as_deref().and_then(|id| settings.lsp.get(id)).cloned().unwrap_or_default();
                Ok(lsp_settings_json(&entry).to_string())
            }
            "language" => {
                let tab_size = settings.tab_sizes.iter().find(|(name, _)| Some(name) == key.as_ref()).map_or(4, |(_, size)| *size);
                Ok(json!({ "tab_size": tab_size }).to_string())
            }
            other => Err(format!("Kuana has no {other} settings for extensions")),
        }
    }

    fn download_file(&mut self, url: String, file_path: String, file_type: DownloadedFileType) -> Result<(), String> {
        let destination = self.in_work(&file_path)?;
        let name = url.rsplit('/').next().unwrap_or_default();
        download(&url, &destination, &self.work, file_type, &format!("{name} ({} extension)", self.extension))
    }

    fn make_file_executable(&mut self, filepath: String) -> Result<(), String> {
        let path = self.in_work(&filepath)?;
        if !path.is_file() {
            return Err(format!("{filepath} isn't a file"));
        }
        // Not a link it made to a file elsewhere.
        self.check_real(&path, &filepath)?;
        crate::lsp::install::make_executable(&path)
    }

    fn set_language_server_installation_status(&mut self, server: String, status: LanguageServerInstallationStatus) {
        let status = match status {
            LanguageServerInstallationStatus::None => return,
            LanguageServerInstallationStatus::Downloading => "downloading".to_string(),
            LanguageServerInstallationStatus::CheckingForUpdate => "checking for updates".to_string(),
            LanguageServerInstallationStatus::Failed(err) => format!("failed: {err}"),
        };
        eprintln!("kuana: {} extension, {server}: {status}", self.extension);
    }
}

/// `lsp.<id>` as the API's `LspSettings` (JSON).
fn lsp_settings_json(entry: &LspSettings) -> Value {
    let binary = entry.binary.as_ref().map(|b| json!({ "path": b.path, "arguments": b.arguments, "env": null }));
    json!({ "binary": binary, "initialization_options": entry.initialization_options, "settings": entry.settings })
}

impl HostWorktree for State {
    fn id(&mut self, project: Resource<ProjectRoot>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        self.root(&project).ok().hash(&mut hasher);
        hasher.finish()
    }

    fn root_path(&mut self, project: Resource<ProjectRoot>) -> String {
        self.root(&project).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn read_text_file(&mut self, project: Resource<ProjectRoot>, path: String) -> Result<String, String> {
        let file = inside(self.root(&project)?, &path)?;
        std::fs::read_to_string(&file).map_err(|e| format!("{path}: {e}"))
    }

    fn which(&mut self, _project: Resource<ProjectRoot>, name: String) -> Option<String> {
        servers::which(&name).map(|p| p.to_string_lossy().into_owned())
    }

    fn shell_env(&mut self, _project: Resource<ProjectRoot>) -> EnvVars {
        let path = servers::search_path().to_string_lossy().into_owned();
        let mut env: EnvVars = std::env::vars().filter(|(key, _)| key != "PATH").collect();
        env.push(("PATH".into(), path));
        env
    }

    fn drop(&mut self, project: Resource<ProjectRoot>) -> wasmtime::Result<()> {
        self.table.delete(project)?;
        Ok(())
    }
}

impl HostProject for State {
    fn worktree_ids(&mut self, _project: Resource<Project>) -> Vec<u64> {
        Vec::new()
    }

    fn drop(&mut self, _project: Resource<Project>) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl HostKeyValueStore for State {
    fn insert(&mut self, _store: Resource<KeyValueStore>, _key: String, _value: String) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }

    fn drop(&mut self, _store: Resource<KeyValueStore>) -> wasmtime::Result<()> {
        Ok(())
    }
}

const UNSUPPORTED: &str = "Kuana runs extensions' language servers only";

impl zed::extension::common::Host for State {}
impl zed::extension::context_server::Host for State {}
impl zed::extension::lsp::Host for State {}
impl zed::extension::slash_command::Host for State {}

impl zed::extension::dap::Host for State {
    fn resolve_tcp_template(&mut self, _template: TcpArgumentsTemplate) -> Result<TcpArguments, String> {
        Err(UNSUPPORTED.into())
    }
}

impl zed::extension::platform::Host for State {
    fn current_platform(&mut self) -> (Os, Architecture) {
        let os = match std::env::consts::OS {
            "macos" => Os::Mac,
            "windows" => Os::Windows,
            _ => Os::Linux,
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => Architecture::Aarch64,
            "x86" => Architecture::X86,
            _ => Architecture::X8664,
        };
        (os, arch)
    }
}

impl zed::extension::process::Host for State {
    fn run_command(&mut self, command: Command) -> Result<Output, String> {
        // Extensions run tools the project may configure (its own binaries).
        let root = self.call.as_ref().map(|c| c.root.clone()).ok_or("programs only run during a call")?;
        if !crate::trust::is_trusted(&root) {
            return Err(format!("{} isn't trusted, so extensions don't run programs for it", root.display()));
        }
        let mut program = crate::process::command(program_path(&self.work, &command.command));
        program.args(&command.args).env("PATH", servers::search_path()).envs(command.env).current_dir(&self.work);
        let output = output_within(program, COMMAND_TIMEOUT).map_err(|e| format!("{}: {e}", command.command))?;
        Ok(Output { status: output.status.code(), stdout: output.stdout, stderr: output.stderr })
    }
}

/// `command`'s output, or an error once it has run for `timeout` (it's
/// ended then, with what it started).
fn output_within(mut command: std::process::Command, timeout: Duration) -> Result<std::process::Output, String> {
    // Its own process group, so ending it reaches what it started too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child =
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("couldn't run it: {e}"))?;
    fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut pipe) = pipe {
                pipe.read_to_end(&mut out).ok();
            }
            out
        })
    }
    let (stdout, stderr) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() >= timeout {
            #[cfg(unix)]
            // SAFETY: signals the process group made for the child above.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            crate::process::kill_tree(&mut child);
            return Err(format!("it ran for over {} s, so Kuana ended it", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(std::process::Output { status, stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default() })
}

impl zed::extension::nodejs::Host for State {
    fn node_binary_path(&mut self) -> Result<String, String> {
        crate::lsp::install::node().map(|p| p.to_string_lossy().into_owned())
    }

    fn npm_package_latest_version(&mut self, package: String) -> Result<String, String> {
        crate::lsp::install::npm_latest(npm_name(&package)?)
    }

    fn npm_package_installed_version(&mut self, package: String) -> Result<Option<String>, String> {
        let manifest = self.work.join("node_modules").join(npm_name(&package)?).join("package.json");
        let Ok(text) = std::fs::read_to_string(&manifest) else { return Ok(None) };
        let value: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
        Ok(value["version"].as_str().map(str::to_string))
    }

    fn npm_install_package(&mut self, package: String, version: String) -> Result<(), String> {
        if version.is_empty() || !version.chars().all(|c| c.is_ascii_alphanumeric() || ".-+_".contains(c)) {
            return Err(format!("\"{version}\" isn't a version"));
        }
        crate::lsp::install::npm_install(&self.work, npm_name(&package)?, &version)
    }
}

/// A package name as npm writes them (`name`, `@scope/name`), nothing that
/// leaves `node_modules` or reads as an option.
fn npm_name(package: &str) -> Result<&str, String> {
    let name = package.strip_prefix('@').map_or(package, |scoped| scoped);
    let parts: Vec<&str> = name.split('/').collect();
    let part = |p: &&str| !p.starts_with(['.', '-', '_']) && !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c));
    let ok = parts.iter().all(part) && parts.len() == if package.starts_with('@') { 2 } else { 1 };
    if ok { Ok(package) } else { Err(format!("\"{package}\" isn't an npm package name")) }
}

// ---- GitHub and HTTP -------------------------------------------------------------------

impl zed::extension::github::Host for State {
    fn latest_github_release(&mut self, repo: String, options: GithubReleaseOptions) -> Result<GithubRelease, String> {
        let releases = github(&format!("{}/releases?per_page=100", repo_url(&repo)?))?;
        releases
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["draft"] != true && (options.pre_release || r["prerelease"] != true))
            .filter_map(release)
            .find(|r| !options.require_assets || !r.assets.is_empty())
            .ok_or_else(|| format!("{repo} has no release"))
    }

    fn github_release_by_tag_name(&mut self, repo: String, tag: String) -> Result<GithubRelease, String> {
        let mut url = url::Url::parse(&format!("{}/releases/tags/", repo_url(&repo)?)).map_err(|e| e.to_string())?;
        url.path_segments_mut().map_err(|_| "not a URL")?.pop_if_empty().push(&tag);
        release(&github(url.as_str())?).ok_or_else(|| format!("{repo} has no release {tag}"))
    }
}

fn repo_url(repo: &str) -> Result<String, String> {
    let valid = |part: &str| !part.is_empty() && !part.starts_with('.') && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok(format!("https://api.github.com/repos/{owner}/{name}")),
        _ => Err(format!("\"{repo}\" isn't a GitHub repository (owner/name)")),
    }
}

fn github(url: &str) -> Result<Value, String> {
    serde_json::from_slice(&crate::updater::fetch(url)?).map_err(|e| format!("GitHub's answer didn't parse: {e}"))
}

fn release(value: &Value) -> Option<GithubRelease> {
    let assets = value["assets"].as_array()?.iter().filter_map(|asset| {
        Some(GithubReleaseAsset { name: asset["name"].as_str()?.to_string(), download_url: asset["browser_download_url"].as_str()?.to_string() })
    });
    Some(GithubRelease { version: value["tag_name"].as_str()?.to_string(), assets: assets.collect() })
}

impl zed::extension::http_client::Host for State {
    fn fetch(&mut self, request: HttpRequest) -> Result<HttpResponse, String> {
        let headers = header_file()?;
        let result = (|| {
            let mut child = curl(&request, &headers)?.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("couldn't run curl: {e}"))?;
            send_body(&mut child, request.body)?;
            let output = child.wait_with_output().map_err(|e| format!("curl failed: {e}"))?;
            if !output.status.success() {
                return Err(curl_error(&output.stderr));
            }
            // With --head, curl prints the headers where the body goes.
            let body = if request.method == HttpMethod::Head { Vec::new() } else { output.stdout };
            Ok(HttpResponse { headers: read_headers(&headers), body })
        })();
        std::fs::remove_file(&headers).ok();
        result
    }

    fn fetch_stream(&mut self, request: HttpRequest) -> Result<Resource<ResponseStream>, String> {
        let headers = header_file()?;
        let mut child = curl(&request, &headers)?.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("couldn't run curl: {e}"))?;
        send_body(&mut child, request.body)?;
        let body = child.stdout.take().ok_or("curl gave no output")?;
        self.table.push(ResponseStream { child, body, headers }).map_err(|e| e.to_string())
    }
}

impl zed::extension::http_client::HostHttpResponseStream for State {
    fn next_chunk(&mut self, stream: Resource<ResponseStream>) -> Result<Option<Vec<u8>>, String> {
        let stream = self.table.get_mut(&stream).map_err(|e| e.to_string())?;
        let mut chunk = vec![0; 64 * 1024];
        let n = stream.body.read(&mut chunk).map_err(|e| e.to_string())?;
        if n > 0 {
            chunk.truncate(n);
            return Ok(Some(chunk));
        }
        let status = stream.child.wait().map_err(|e| e.to_string())?;
        if status.success() {
            return Ok(None);
        }
        let mut stderr = Vec::new();
        if let Some(mut pipe) = stream.child.stderr.take() {
            pipe.read_to_end(&mut stderr).ok();
        }
        Err(curl_error(&stderr))
    }

    fn drop(&mut self, stream: Resource<ResponseStream>) -> wasmtime::Result<()> {
        self.table.delete(stream)?;
        Ok(())
    }
}

/// curl for an extension's request, its response headers written to `headers`.
fn curl(request: &HttpRequest, headers: &Path) -> Result<std::process::Command, String> {
    if !(request.url.starts_with("https://") || request.url.starts_with("http://")) {
        return Err(format!("{} isn't an HTTP address", request.url));
    }
    let mut command = crate::process::command("curl");
    command.args(["--silent", "--show-error", "--proto", "=http,https", "--proto-redir", "=http,https", "--connect-timeout", "20"]);
    command.args(["--max-time", "600", "--dump-header"]).arg(headers);
    match request.redirect_policy {
        RedirectPolicy::NoFollow => {}
        RedirectPolicy::FollowLimit(limit) => {
            command.args(["--location", "--max-redirs", &limit.to_string()]);
        }
        RedirectPolicy::FollowAll => {
            command.args(["--location", "--max-redirs", "-1"]);
        }
    }
    let method = match request.method {
        HttpMethod::Head => None,
        HttpMethod::Get => Some("GET"),
        HttpMethod::Post => Some("POST"),
        HttpMethod::Put => Some("PUT"),
        HttpMethod::Delete => Some("DELETE"),
        HttpMethod::Options => Some("OPTIONS"),
        HttpMethod::Patch => Some("PATCH"),
    };
    match method {
        Some(method) => command.args(["--request", method]),
        None => command.arg("--head"),
    };
    for (name, value) in &request.headers {
        if format!("{name}{value}").contains(['\r', '\n']) || name.contains(':') {
            return Err(format!("the header {name} isn't valid"));
        }
        command.arg("--header").arg(format!("{name}: {value}"));
    }
    if request.body.is_some() {
        command.args(["--data-binary", "@-"]);
    }
    command.arg("--url").arg(&request.url).stdin(if request.body.is_some() { Stdio::piped() } else { Stdio::null() });
    Ok(command)
}

/// Writes a request's body to curl, which reads it all before sending.
fn send_body(child: &mut Child, body: Option<Vec<u8>>) -> Result<(), String> {
    if let (Some(body), Some(mut stdin)) = (body, child.stdin.take()) {
        stdin.write_all(&body).map_err(|e| format!("couldn't send the request: {e}"))?;
    }
    Ok(())
}

/// Where curl writes a response's headers: a folder of Kuana's own, not the
/// shared temporary one (where another user could put a link by that name
/// first) nor the extension's (where its code could).
fn header_file() -> Result<PathBuf, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = super::extensions_dir().ok_or("Kuana has no config folder")?.join(".http");
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make {}: {e}", dir.display()))?;
    Ok(dir.join(format!("headers-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))))
}

/// The last response's headers (after redirects) from curl's `--dump-header`.
fn read_headers(path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let last = text.split("\r\n\r\n").filter(|block| !block.trim().is_empty()).last().unwrap_or_default();
    last.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect()
}

fn curl_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    format!("the request failed: {}", text.trim().trim_start_matches("curl: "))
}

/// Downloads `url` to `destination` in the work folder, unpacked as the code
/// asked: a file, or a folder for an archive.
fn download(url: &str, destination: &Path, work: &Path, file_type: DownloadedFileType, label: &str) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = work.join(format!(".download-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
    crate::updater::download(url, &temp, label, None)?;
    let unpacked = unpack(&temp, destination, work, file_type);
    std::fs::remove_file(&temp).ok();
    unpacked
}

/// A downloaded file (`temp`) put at `destination`, unpacked as asked.
fn unpack(temp: &Path, destination: &Path, work: &Path, file_type: DownloadedFileType) -> Result<(), String> {
    let name = destination.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match file_type {
        DownloadedFileType::Uncompressed => {
            remove_link(destination);
            std::fs::rename(temp, destination).map_err(|e| e.to_string())
        }
        DownloadedFileType::Gzip => {
            remove_link(destination);
            let mut input = flate2::read::GzDecoder::new(std::fs::File::open(temp).map_err(|e| e.to_string())?);
            let mut output = std::fs::File::create(destination).map_err(|e| e.to_string())?;
            std::io::copy(&mut input, &mut output).map(|_| ()).map_err(|e| format!("couldn't unpack {name}: {e}"))
        }
        DownloadedFileType::GzipTar | DownloadedFileType::Zip => {
            std::fs::create_dir_all(destination).map_err(|e| e.to_string())?;
            // The folder itself may be a link the code made to one elsewhere.
            check_real(work, destination, &name)?;
            crate::lsp::install::unzip(temp, destination)
        }
    }
}

/// A link where a file goes is replaced, not written through.
fn remove_link(path: &Path) {
    if path.is_symlink() {
        std::fs::remove_file(path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_api_0_6_and_0_7() {
        let dir = std::env::temp_dir().join(format!("kuana-wasm-supported-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = |version: &str| -> toml::Table { format!("[lib]\nkind = \"Rust\"\nversion = \"{version}\"\n").parse().unwrap() };
        assert!(!supported(&manifest("0.7.0"), &dir), "no extension.wasm");
        std::fs::write(dir.join("extension.wasm"), b"").unwrap();
        assert!(supported(&manifest("0.7.0"), &dir) && supported(&manifest("0.6.0"), &dir));
        assert!(!supported(&manifest("0.5.0"), &dir) && !supported(&manifest("0.1.0"), &dir) && !supported(&manifest("1.7.0"), &dir));
        assert!(!supported(&toml::Table::new(), &dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paths_stay_in_their_folder() {
        let base = Path::new("/work/php");
        assert_eq!(inside(base, "phpactor-1/phpactor.phar"), Ok(PathBuf::from("/work/php/phpactor-1/phpactor.phar")));
        assert_eq!(inside(base, "./a/../b"), Ok(PathBuf::from("/work/php/b")));
        assert_eq!(inside(base, "/work/php/x"), Ok(PathBuf::from("/work/php/x")));
        assert!(inside(base, "../other").is_err() && inside(base, "a/../../x").is_err());
        assert!(inside(base, "/etc/passwd").is_err() && inside(base, "/work/php/../x").is_err());
        assert_eq!(program_path(base, "/usr/bin/dart"), PathBuf::from("/usr/bin/dart"));
        assert_eq!(program_path(base, "v1/server"), PathBuf::from("/work/php/v1/server"));
    }

    #[test]
    fn names_and_repositories() {
        assert!(npm_name("intelephense").is_ok() && npm_name("@zed-industries/vscode-langservers-extracted").is_ok());
        assert!(npm_name("../x").is_err() && npm_name("@a/../b").is_err() && npm_name("--global").is_err());
        assert!(npm_name("a/b").is_err() && npm_name("@a").is_err() && npm_name("").is_err());
        assert_eq!(repo_url("phpactor/phpactor"), Ok("https://api.github.com/repos/phpactor/phpactor".to_string()));
        assert!(repo_url("a").is_err() && repo_url("a/b/c").is_err() && repo_url("../x").is_err() && repo_url("a/b?x").is_err());
    }

    #[test]
    fn github_releases() {
        let value = json!({ "tag_name": "v1", "assets": [{ "name": "x.gz", "browser_download_url": "https://x/x.gz" }] });
        let release = release(&value).unwrap();
        assert_eq!((release.version.as_str(), release.assets[0].name.as_str()), ("v1", "x.gz"));
    }

    #[test]
    fn settings_in_the_apis_shape() {
        let entry = LspSettings {
            settings: Some(json!({ "a": 1 })),
            binary: Some(crate::settings::LspBinary { path: Some("/bin/dart".into()), arguments: None }),
            ..Default::default()
        };
        assert_eq!(
            lsp_settings_json(&entry),
            json!({ "binary": { "path": "/bin/dart", "arguments": null, "env": null }, "initialization_options": null, "settings": { "a": 1 } })
        );
    }

    /// Zed's own extensions, as Zed installed them (copied, never changed):
    /// Dart's server from PATH, HTML's installed with npm, PHP's phpactor
    /// downloaded from GitHub and intelephense from npm. Points the config
    /// folder elsewhere, so it runs alone:
    /// `cargo test runs_zed_extensions -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs Zed's dart, html and php extensions, the network, node and dart; changes XDG_CONFIG_HOME"]
    fn runs_zed_extensions() {
        use futures::executor::block_on;
        let zed = super::super::zed_installed_dir().expect("Zed's extensions folder");
        let config = std::env::temp_dir().join(format!("kuana-wasm-zed-{}", std::process::id()));
        // SAFETY: this test runs on its own (it's ignored by default).
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
        let extensions = super::super::extensions_dir().unwrap();
        std::fs::create_dir_all(&extensions).unwrap();
        let project = config.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let settings = CallSettings::default();
        let extension = |id: &str| -> &'static WasmExtension {
            let dir = extensions.join(id);
            let copied = std::process::Command::new("cp").arg("-R").arg(zed.join(id)).arg(&dir).status().unwrap();
            assert!(copied.success());
            let start = std::time::Instant::now();
            WasmComponent::from_file(&ENGINE, dir.join("extension.wasm")).unwrap();
            eprintln!("{id}: compiled in {:?}", start.elapsed());
            Box::leak(Box::new(WasmExtension { id: id.to_string(), dir }))
        };
        // On the extension's thread, as Kuana runs it.
        let timed = |extension: &'static WasmExtension, server: &'static str| {
            let start = std::time::Instant::now();
            let (project, settings) = (project.clone(), settings.clone());
            let started = block_on(extension.run(move |e, instance| e.start(instance, server, &project, &settings)));
            eprintln!("{server}: {:?} in {:?}", started.as_ref().map(|s| &s.command), start.elapsed());
            started
        };

        // Dart: `which`, then the user's `lsp.dart` settings, which its code
        // puts in a section of their own.
        let dart_extension = extension("dart");
        let dart = timed(dart_extension, "dart").unwrap();
        assert!(dart.command.program.ends_with("dart") && dart.command.args == ["language-server", "--protocol=lsp"], "{dart:?}");
        let asking = Asking { own: Some((dart_extension, "dart")), others: Vec::new(), target: "dart".into(), root: project.clone(), tab_sizes: Vec::new() };
        let lsp: HashMap<String, LspSettings> = [("dart".to_string(), LspSettings { settings: Some(json!({ "lineLength": 100 })), ..Default::default() })].into();
        assert_eq!(block_on(asking.workspace_configuration(&lsp)), json!({ "dart": { "lineLength": 100 } }));

        // HTML: npm into its work folder, run by node (an absolute path from
        // its current directory, which must be the work folder).
        let html = extension("html");
        let started = timed(html, "vscode-html-language-server").unwrap();
        let work = work_dir("html").unwrap();
        assert_eq!(started.command.program.file_name().unwrap(), "node", "{started:?}");
        assert!(Path::new(&started.command.args[0]).starts_with(&work) && Path::new(&started.command.args[0]).is_file(), "{started:?}");
        assert_eq!(started.initialization_options, Some(json!({ "provideFormatter": true })));
        // Asked again, the same instance finds it installed.
        timed(html, "vscode-html-language-server").unwrap();

        // PHP: phpactor.phar from its latest GitHub release, made executable.
        let php = extension("php");
        let phpactor = timed(php, "phpactor").unwrap();
        let program = &phpactor.command.program;
        assert!(program.starts_with(work_dir("php").unwrap()) && program.ends_with("phpactor.phar"), "{phpactor:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(std::fs::metadata(program).unwrap().permissions().mode() & 0o111 != 0);
        }
        let servers: &'static WasmServers = Box::leak(Box::new(WasmServers { extension: php, ids: &["intelephense", "phpactor"] }));
        let first = block_on(servers.start(&project, &settings)).unwrap();
        assert_eq!(first.server, "intelephense");
        let asking = Asking { own: None, others: vec![servers], target: "vtsls".into(), root: project.clone(), tab_sizes: Vec::new() };
        let answers = (block_on(asking.workspace_configuration(&settings.lsp)), block_on(asking.additional_options(&settings.lsp)));
        assert_eq!(answers, (Value::Null, Value::Null));

        // Uninstalled (`extensions::uninstall`, which also trashes the
        // extension), an extension's downloads go with it.
        forget("html");
        assert!(!work.exists());
        std::fs::remove_dir_all(&config).ok();
    }

    #[test]
    fn response_headers_after_redirects() {
        let path = std::env::temp_dir().join(format!("kuana-wasm-headers-{}", std::process::id()));
        std::fs::write(&path, "HTTP/2 302\r\nlocation: x\r\n\r\nHTTP/2 200\r\ncontent-type: text/plain\r\nx-a: b: c\r\n\r\n").unwrap();
        assert_eq!(read_headers(&path), [("content-type".to_string(), "text/plain".to_string()), ("x-a".into(), "b: c".into())]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_stuck_call_doesnt_hold_later_ones() {
        let extension: &'static WasmExtension = Box::leak(Box::new(WasmExtension { id: "deadline-test".into(), dir: PathBuf::new() }));
        let (release, stuck) = mpsc::channel::<()>();
        let started = Instant::now();
        let first = futures::executor::block_on(extension.run_within(Duration::from_millis(100), move |_, _| {
            stuck.recv().ok();
            Ok(1)
        }));
        assert!(first.unwrap_err().contains("stopped waiting"));
        // A new thread answers, not the stuck one's queue.
        let second = futures::executor::block_on(extension.run_within(Duration::from_secs(30), |_, _| Ok(2)));
        assert_eq!(second, Ok(2));
        assert!(started.elapsed() < Duration::from_secs(10));
        release.send(()).ok();
    }

    #[cfg(unix)]
    #[test]
    fn programs_end_at_their_timeout() {
        let mut quick = crate::process::command("sh");
        quick.args(["-c", "echo hi"]);
        assert_eq!(output_within(quick, Duration::from_secs(30)).unwrap().stdout, b"hi\n");
        // What it started in the background goes too, or its pipes stay open.
        let mut slow = crate::process::command("sh");
        slow.args(["-c", "sleep 30 & sleep 30"]);
        let started = Instant::now();
        assert!(output_within(slow, Duration::from_millis(200)).unwrap_err().contains("ended it"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn archives_dont_unpack_through_a_link() {
        let root = std::env::temp_dir().join(format!("kuana-wasm-unpack-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let (work, elsewhere) = (root.join("work"), root.join("elsewhere"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, work.join("server")).unwrap();
        let download = work.join(".download-0");
        std::fs::write(&download, b"an archive").unwrap();
        for file_type in [DownloadedFileType::GzipTar, DownloadedFileType::Zip] {
            let err = unpack(&download, &work.join("server"), &work, file_type).unwrap_err();
            assert!(err.contains("outside"), "{err}");
        }
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        unpack(&download, &work.join("file"), &work, DownloadedFileType::Uncompressed).unwrap();
        assert!(work.join("file").is_file());
        std::fs::remove_dir_all(&root).ok();
    }
}
