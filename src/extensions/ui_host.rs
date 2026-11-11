//! Runs extensions' `kuana.wasm`: code built for Kuana's own interface
//! (`kuana-wit/kuana.wit`), which answers for the views, commands and panels
//! the extension declares (`contrib`). Kuana draws everything; the code only
//! answers with data and asks for things through the host calls here.
//!
//! Like Zed extensions' code (`wasm`), each extension's runs on a thread of
//! its own with its instance kept between calls, in WASI's sandbox with its
//! work folder, within fuel, memory and time limits. Host calls that reach
//! the UI (refresh a view, show a panel, open a file, ask the user) become a
//! `Request` on one channel that the app reads on its main thread
//! (`workspace::extension_requests`); the ones that wait for an answer (a
//! prompt, a permission) block only the extension's thread. Network, programs
//! and project files need a permission the manifest declares and the user
//! allows (`permissions`).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime},
};

use futures::{
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::Either,
};
use wasmtime::{
    Store, StoreLimits, StoreLimitsBuilder, Trap,
    component::{Component as WasmComponent, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::{
    contrib::{Contributions, Permission},
    permissions,
    wasm::{self, ENGINE},
};
use crate::lsp::servers;

mod bindings {
    wasmtime::component::bindgen!({
        world: "extension",
        path: "src/extensions/kuana-wit",
    });
}

use bindings::{Extension, kuana::extension::types as wit};

// ---- What the UI gets -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Success,
    Warning,
    Error,
}

impl From<wit::Tone> for Tone {
    fn from(tone: wit::Tone) -> Self {
        match tone {
            wit::Tone::Normal => Tone::Normal,
            wit::Tone::Muted => Tone::Muted,
            wit::Tone::Success => Tone::Success,
            wit::Tone::Warning => Tone::Warning,
            wit::Tone::Error => Tone::Error,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TreeItem {
    pub id: String,
    pub label: String,
    pub detail: Option<String>,
    pub icon: Option<String>,
    pub tone: Tone,
    pub kind: Option<String>,
    pub expandable: bool,
    pub command: Option<String>,
}

impl From<wit::TreeItem> for TreeItem {
    fn from(item: wit::TreeItem) -> Self {
        TreeItem {
            id: item.id,
            label: item.label,
            detail: item.detail,
            icon: item.icon,
            tone: item.tone.into(),
            kind: item.kind,
            expandable: item.expandable,
            command: item.command,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(Vec<String>),
    Table { columns: Vec<String>, rows: Vec<Vec<Option<String>>> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub line: u32,
    pub column: u32,
    pub end_column: Option<u32>,
    pub severity: crate::lint::Severity,
    pub message: String,
}

/// What a call asks of the UI. `window` is the workspace window the call is
/// for (`Caller::window`), extension ids are the extension's.
pub enum Request {
    Refresh { extension: String, view: String, item: Option<String> },
    ShowPanel { window: u64, extension: String, panel: String, title: Option<String>, content: Content },
    OpenFile { window: u64, path: PathBuf, line: Option<u32> },
    Notify { window: u64, extension: String, tone: Tone, message: String, detail: Option<String> },
    AskInput { window: u64, extension: String, prompt: String, initial: String, secret: bool, answer: oneshot::Sender<Option<String>> },
    Confirm { window: u64, message: String, detail: String, action: String, answer: oneshot::Sender<bool> },
    Permission { window: u64, name: String, permission: Permission, answer: oneshot::Sender<bool> },
    Diagnostics { extension: String, path: PathBuf, problems: Vec<Problem> },
    RunInTerminal { window: u64, extension: String, title: String, command: String },
}

static REQUESTS: LazyLock<(UnboundedSender<Request>, Mutex<Option<UnboundedReceiver<Request>>>)> = LazyLock::new(|| {
    let (send, receive) = unbounded();
    (send, Mutex::new(Some(receive)))
});

/// The requests' channel, for the app to read (once).
pub fn take_requests() -> Option<UnboundedReceiver<Request>> {
    REQUESTS.1.lock().unwrap_or_else(|e| e.into_inner()).take()
}

fn send(request: Request) {
    REQUESTS.0.unbounded_send(request).ok();
}

/// Who a call is for.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    /// The workspace window (its entity id).
    pub window: u64,
    /// Its project folder.
    pub root: Option<PathBuf>,
    /// The extension's `extensions.<id>` settings, as JSON.
    pub settings: String,
}

// ---- Calls ----------------------------------------------------------------------------

/// How long a view may take to list items: it's waited for while drawing.
const CHILDREN_DEADLINE: Duration = Duration::from_secs(15);
/// A command may ask the user things and run programs.
const COMMAND_DEADLINE: Duration = Duration::from_secs(10 * 60);
const SAVED_DEADLINE: Duration = Duration::from_secs(2 * 60);
/// How much of its own code a call may run (as for Zed extensions' code).
const FUEL: u64 = 10_000_000_000;
const MEMORY: usize = 512 << 20;
/// How long a program the code runs may take.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The largest project file `read-project-file` reads.
const MAX_READ: u64 = 16 << 20;

/// A view's items: the roots, or `parent`'s children.
pub fn children(
    extension: Arc<Contributions>,
    caller: Caller,
    view: String,
    parent: Option<String>,
) -> impl Future<Output = Result<Vec<TreeItem>, String>> + Send + 'static {
    run(extension, caller, CHILDREN_DEADLINE, move |bindings, store| {
        bindings.call_children(store, &view, parent.as_deref()).map(|r| r.map(|items| items.into_iter().map(TreeItem::from).collect()))
    })
}

pub fn run_command(
    extension: Arc<Contributions>,
    caller: Caller,
    command: String,
    item: Option<String>,
) -> impl Future<Output = Result<(), String>> + Send + 'static {
    run(extension, caller, COMMAND_DEADLINE, move |bindings, store| bindings.call_run_command(store, &command, item.as_deref()))
}

/// The user saved `path`, a file in the extension's work folder.
pub fn file_saved(extension: Arc<Contributions>, caller: Caller, path: PathBuf) -> impl Future<Output = Result<(), String>> + Send + 'static {
    let path = path.to_string_lossy().into_owned();
    run(extension, caller, SAVED_DEADLINE, move |bindings, store| bindings.call_file_saved(store, &path))
}

/// Whether `path` is in extension `id`'s work folder (its files go to its
/// `file-saved` when saved).
pub fn in_work_folder(id: &str, path: &Path) -> bool {
    wasm::work_dir(id).is_ok_and(|work| path.starts_with(&work))
}

/// Runs `call` on the extension's thread with its instance, answered with an
/// error after `deadline` (the thread is then left to finish and later calls
/// get a new one).
fn run<T: Send + 'static>(
    extension: Arc<Contributions>,
    caller: Caller,
    deadline: Duration,
    call: impl FnOnce(&Extension, &mut Store<HostState>) -> wasmtime::Result<Result<T, String>> + Send + 'static,
) -> impl Future<Output = Result<T, String>> + Send + 'static {
    let id = extension.extension.clone();
    let (answer, answered) = oneshot::channel();
    let job: Job = Box::new(move |slot| {
        answer.send(Instance::call(slot, &extension, caller, call)).ok();
    });
    let generation = worker(&id).and_then(|(generation, jobs)| jobs.send(job).ok().map(|()| generation));
    let late = wasm::after(deadline);
    async move {
        let not_running = || Err(format!("the {id} extension's code isn't running"));
        match futures::future::select(answered, late).await {
            Either::Left((answer, _)) => answer.unwrap_or_else(|_| not_running()),
            Either::Right((Ok(()), _)) => {
                if let Some(generation) = generation {
                    abandon(&id, generation);
                }
                Err(format!("the {id} extension took over {} s to answer", deadline.as_secs()))
            }
            Either::Right((Err(_), answered)) => answered.await.unwrap_or_else(|_| not_running()),
        }
    }
}

type Job = Box<dyn FnOnce(&mut Option<Instance>) + Send>;

/// Each extension's thread for its Kuana code, by id (apart from its Zed
/// code's thread in `wasm`, which may be busy downloading a server).
static WORKERS: LazyLock<Mutex<HashMap<String, (u64, mpsc::Sender<Job>)>>> = LazyLock::new(Default::default);

fn worker(id: &str) -> Option<(u64, mpsc::Sender<Job>)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut workers = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((generation, jobs)) = workers.get(id) {
        return Some((*generation, jobs.clone()));
    }
    let (jobs, queue) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name(format!("extension ui {id}"))
        .stack_size(8 << 20)
        .spawn(move || {
            let mut instance = None;
            for job in queue {
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

fn abandon(id: &str, generation: u64) {
    let mut workers = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    if workers.get(id).is_some_and(|(current, _)| *current == generation) {
        workers.remove(id);
    }
}

/// An uninstalled (or reloaded) extension: its instance ends after the
/// calls it has queued.
pub fn forget(id: &str) {
    WORKERS.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
}

static LINKER: LazyLock<Result<Linker<HostState>, String>> = LazyLock::new(|| {
    let mut linker = Linker::new(&ENGINE);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| e.to_string())?;
    Extension::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state).map_err(|e| e.to_string())?;
    Ok(linker)
});

struct Instance {
    store: Store<HostState>,
    bindings: Extension,
    wasm: PathBuf,
    modified: Option<SystemTime>,
}

impl Instance {
    /// Compiles and instantiates the code, then `activate`s it.
    fn new(extension: &Contributions, wasm: PathBuf, modified: Option<SystemTime>, work: PathBuf) -> Result<Self, String> {
        let id = &extension.extension;
        let failed = |e: wasmtime::Error| format!("couldn't run the {id} extension's code: {e:#}");
        let component = WasmComponent::from_file(&ENGINE, &wasm).map_err(failed)?;
        let linker = LINKER.as_ref().map_err(|e| format!("couldn't set up extensions' code: {e}"))?;
        let work_name = work.to_string_lossy().into_owned();
        let mut wasi = WasiCtxBuilder::new();
        wasi.inherit_stdout().inherit_stderr().allow_blocking_current_thread(true).env("PWD", &work_name);
        wasi.preopened_dir(&work, ".", DirPerms::all(), FilePerms::all()).map_err(failed)?;
        wasi.preopened_dir(&work, &work_name, DirPerms::all(), FilePerms::all()).map_err(failed)?;
        let state = HostState {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(MEMORY).build(),
            extension: id.clone(),
            name: extension.name.clone(),
            declared: extension.permissions.clone(),
            panels: extension.panels.iter().map(|p| p.id.clone()).collect(),
            work,
            caller: Caller::default(),
        };
        let mut store = Store::new(&ENGINE, state);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(FUEL).map_err(failed)?;
        let bindings = Extension::instantiate(&mut store, &component, linker).map_err(failed)?;
        bindings.call_activate(&mut store).map_err(failed)?.map_err(|e| format!("the {id} extension didn't start: {e}"))?;
        Ok(Instance { store, bindings, wasm, modified })
    }

    /// Runs `f` on the extension's instance, made first (or again, when its
    /// `kuana.wasm` changed). A trap ends the instance.
    fn call<T>(
        slot: &mut Option<Instance>,
        extension: &Contributions,
        caller: Caller,
        f: impl FnOnce(&Extension, &mut Store<HostState>) -> wasmtime::Result<Result<T, String>>,
    ) -> Result<T, String> {
        let id = &extension.extension;
        let wasm = extension.code.clone().ok_or_else(|| format!("the {id} extension has no kuana.wasm"))?;
        let modified = std::fs::metadata(&wasm).and_then(|m| m.modified()).ok();
        if slot.as_ref().is_none_or(|i| i.modified != modified || i.wasm != wasm) {
            *slot = None;
            *slot = Some(Instance::new(extension, wasm, modified, wasm::work_dir(id)?)?);
        }
        let instance = slot.as_mut().expect("just made");
        instance.store.set_fuel(FUEL).map_err(|e| e.to_string())?;
        instance.store.data_mut().caller = caller;
        let result = f(&instance.bindings, &mut instance.store);
        instance.store.data_mut().caller = Caller::default();
        match result {
            Ok(answer) => answer,
            Err(trap) => {
                *slot = None;
                if trap.downcast_ref::<Trap>() == Some(&Trap::OutOfFuel) {
                    return Err(format!("the {id} extension's code ran too long"));
                }
                Err(format!("the {id} extension's code failed: {trap:#}"))
            }
        }
    }
}

/// Whether an extension's `kuana.wasm` loads (`kuana extension check`): it
/// compiles, links against this host and activates, in a temporary folder.
pub fn check(extension: &Contributions) -> Result<(), String> {
    let wasm = extension.code.clone().ok_or("no kuana.wasm")?;
    let work = std::env::temp_dir().join(format!("kuana-check-ui-{}-{}", std::process::id(), extension.extension));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let loaded = Instance::new(extension, wasm, None, work.clone()).map(|_| ());
    std::fs::remove_dir_all(&work).ok();
    loaded
}

// ---- Host ------------------------------------------------------------------------------

pub struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    extension: String,
    name: String,
    declared: Vec<Permission>,
    panels: Vec<String>,
    /// Its work folder (its sandbox).
    work: PathBuf,
    /// The call in progress.
    caller: Caller,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl HostState {
    /// Allowed to use `permission`: declared, and allowed by the user (asked
    /// now, the first time).
    fn allow(&mut self, permission: Permission) -> Result<(), String> {
        let refused = || format!("the {} permission wasn't allowed", permission.key());
        if !self.declared.contains(&permission) {
            return Err(format!("the extension doesn't declare the {} permission in [kuana] permissions", permission.key()));
        }
        if permissions::is_granted(&self.extension, permission) {
            return Ok(());
        }
        if permissions::was_declined(&self.extension, permission) {
            return Err(refused());
        }
        let (answer, answered) = oneshot::channel();
        send(Request::Permission { window: self.caller.window, name: self.name.clone(), permission, answer });
        let allowed = futures::executor::block_on(answered).unwrap_or(false);
        permissions::answer(&self.extension, permission, allowed);
        if allowed { Ok(()) } else { Err(refused()) }
    }

    /// A path the code names: relative ones are in its work folder; absolute
    /// ones in its work folder, or in the project (`workspace-files`).
    fn resolve(&mut self, path: &str) -> Result<PathBuf, String> {
        let given = Path::new(path);
        if !given.is_absolute() || given.starts_with(&self.work) {
            let inside = wasm::inside(&self.work, path)?;
            if let Some(parent) = inside.parent().filter(|p| p.exists()) {
                wasm::check_real(&self.work, parent, path)?;
            }
            return Ok(inside);
        }
        let root = self.caller.root.clone().ok_or_else(|| format!("{path} is outside the extension's folder"))?;
        if !given.starts_with(&root) || given.components().any(|c| c == std::path::Component::ParentDir) {
            return Err(format!("{path} is outside the extension's folder and the project"));
        }
        self.allow(Permission::WorkspaceFiles)?;
        Ok(given.to_path_buf())
    }

    fn keychain(&self, key: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&format!("Kuana extension {}", self.extension), key).map_err(|e| e.to_string())
    }
}

impl wit::Host for HostState {}

impl bindings::kuana::extension::host::Host for HostState {
    fn refresh(&mut self, view: String, item: Option<String>) {
        send(Request::Refresh { extension: self.extension.clone(), view, item });
    }

    fn show_panel(&mut self, panel: String, title: Option<String>, content: wit::PanelContent) {
        if !self.panels.contains(&panel) {
            return;
        }
        let content = match content {
            wit::PanelContent::Text(lines) => Content::Text(lines),
            wit::PanelContent::Table(table) => Content::Table { columns: table.columns, rows: table.rows },
        };
        send(Request::ShowPanel { window: self.caller.window, extension: self.extension.clone(), panel, title, content });
    }

    fn open_file(&mut self, path: String, line: Option<u32>) -> Result<(), String> {
        let path = self.resolve(&path)?;
        if !path.is_file() {
            return Err(format!("{} isn't a file", path.display()));
        }
        send(Request::OpenFile { window: self.caller.window, path, line });
        Ok(())
    }

    fn notify(&mut self, tone: wit::Tone, message: String, detail: Option<String>) {
        send(Request::Notify { window: self.caller.window, extension: self.extension.clone(), tone: tone.into(), message, detail });
    }

    fn ask_input(&mut self, prompt: String, initial: String, secret: bool) -> Option<String> {
        let (answer, answered) = oneshot::channel();
        send(Request::AskInput { window: self.caller.window, extension: self.extension.clone(), prompt, initial, secret, answer });
        futures::executor::block_on(answered).ok().flatten()
    }

    fn confirm(&mut self, message: String, detail: String, action: String) -> bool {
        let (answer, answered) = oneshot::channel();
        send(Request::Confirm { window: self.caller.window, message, detail, action, answer });
        futures::executor::block_on(answered).unwrap_or(false)
    }

    fn settings(&mut self) -> String {
        if self.caller.settings.is_empty() { "{}".into() } else { self.caller.settings.clone() }
    }

    fn project_root(&mut self) -> Option<String> {
        self.caller.root.as_ref().map(|r| r.to_string_lossy().into_owned())
    }

    fn secret_get(&mut self, key: String) -> Option<String> {
        self.keychain(&key).ok()?.get_password().ok()
    }

    fn secret_set(&mut self, key: String, value: Option<String>) -> Result<(), String> {
        let entry = self.keychain(&key)?;
        match value {
            Some(value) => entry.set_password(&value).map_err(|e| e.to_string()),
            None => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            },
        }
    }

    fn set_diagnostics(&mut self, path: String, diagnostics: Vec<wit::Diagnostic>) {
        let Ok(path) = self.resolve(&path) else { return };
        let problems = diagnostics
            .into_iter()
            .map(|d| Problem {
                line: d.line,
                column: d.column,
                end_column: d.end_column,
                severity: match d.severity {
                    wit::Severity::Error => crate::lint::Severity::Error,
                    wit::Severity::Warning => crate::lint::Severity::Warning,
                    wit::Severity::Info => crate::lint::Severity::Info,
                },
                message: d.message,
            })
            .collect();
        send(Request::Diagnostics { extension: self.extension.clone(), path, problems });
    }

    fn run_in_terminal(&mut self, title: String, command: String) -> Result<(), String> {
        self.allow(Permission::Process)?;
        self.trusted()?;
        send(Request::RunInTerminal { window: self.caller.window, extension: self.extension.clone(), title, command });
        Ok(())
    }

    fn run_process(&mut self, program: String, args: Vec<String>, cwd: Option<String>) -> Result<wit::ProcessOutput, String> {
        self.allow(Permission::Process)?;
        self.trusted()?;
        let dir = match cwd {
            Some(cwd) => self.resolve(&cwd)?,
            None => self.caller.root.clone().unwrap_or_else(|| self.work.clone()),
        };
        let mut command = crate::process::command(wasm::program_path(&self.work, &program));
        command.args(&args).env("PATH", servers::search_path()).current_dir(dir);
        let output = wasm::output_within(command, PROCESS_TIMEOUT).map_err(|e| format!("{program}: {e}"))?;
        Ok(wit::ProcessOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn http_request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<wit::HttpResponse, String> {
        self.allow(Permission::Network)?;
        let (status, headers, body) = wasm::http(&method, &url, headers, body)?;
        Ok(wit::HttpResponse { status, headers, body })
    }

    fn read_project_file(&mut self, path: String) -> Result<String, String> {
        let root = self.caller.root.clone().ok_or("no project is open")?;
        let full = if Path::new(&path).is_absolute() { PathBuf::from(&path) } else { root.join(&path) };
        if !full.starts_with(&root) || full.components().any(|c| c == std::path::Component::ParentDir) {
            return Err(format!("{path} is outside the project"));
        }
        self.allow(Permission::WorkspaceFiles)?;
        let size = std::fs::metadata(&full).map_err(|e| format!("{path}: {e}"))?.len();
        if size > MAX_READ {
            return Err(format!("{path} is over {} MB", MAX_READ >> 20));
        }
        std::fs::read_to_string(&full).map_err(|e| format!("{path}: {e}"))
    }
}

impl HostState {
    /// Programs run only for a trusted project (none open is fine: nothing
    /// of a project's can steer them then).
    fn trusted(&self) -> Result<(), String> {
        match &self.caller.root {
            Some(root) if !crate::trust::is_trusted(root) => {
                Err(format!("{} isn't trusted, so extensions don't run programs for it", root.display()))
            }
            _ => Ok(()),
        }
    }
}
