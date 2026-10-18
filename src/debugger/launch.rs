//! What a debug session runs: the adapter for a [`DebugSpec`] (lldb-dap for
//! Rust and other native code, Delve for Go, debugpy for Python, js-debug
//! for JavaScript; found on PATH, through Xcode or in Kuana's config folder,
//! never downloaded), the cargo build that makes a Rust executable, the
//! `launch` or `attach` arguments each adapter expects, and the processes
//! Attach to Process lists.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};

use crate::{
    lsp::servers,
    run::{DebugSpec, RunTarget},
    syntax::Language,
};

/// How to start an adapter.
pub struct Adapter {
    /// `adapterID` for `initialize`.
    pub id: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Listens on this port instead of talking over stdio (Delve).
    pub port: Option<u16>,
}

/// A debug adapter Kuana can start: built in (lldb-dap, Delve, debugpy,
/// js-debug) or from an extension.
pub trait DebugAdapter: Sync {
    /// Its `adapterID`.
    fn id(&self) -> &'static str;
    /// Other names configurations call it: VS Code's `type`, Zed's `adapter`.
    fn aliases(&self) -> &[&'static str] {
        &[]
    }
    /// How to start it, or what to install when it's missing.
    fn start(&self, cwd: &Path) -> Result<Adapter, String>;
}

struct LldbDap;
struct Delve;
struct Debugpy;
struct JsDebug;

impl DebugAdapter for LldbDap {
    fn id(&self) -> &'static str {
        "lldb-dap"
    }
    // CodeLLDB's configurations mostly read the same (program, args, cwd).
    fn aliases(&self) -> &[&'static str] {
        &["lldb", "lldb-vscode", "codelldb", "CodeLLDB"]
    }
    fn start(&self, _: &Path) -> Result<Adapter, String> {
        lldb_dap()
    }
}

impl DebugAdapter for Delve {
    fn id(&self) -> &'static str {
        "go"
    }
    fn aliases(&self) -> &[&'static str] {
        &["delve", "Delve", "dlv"]
    }
    fn start(&self, _: &Path) -> Result<Adapter, String> {
        delve()
    }
}

impl DebugAdapter for Debugpy {
    fn id(&self) -> &'static str {
        "python"
    }
    fn aliases(&self) -> &[&'static str] {
        &["debugpy", "Debugpy"]
    }
    fn start(&self, cwd: &Path) -> Result<Adapter, String> {
        debugpy(cwd)
    }
}

impl DebugAdapter for JsDebug {
    fn id(&self) -> &'static str {
        "pwa-node"
    }
    fn aliases(&self) -> &[&'static str] {
        &["node", "JavaScript", "js-debug"]
    }
    fn start(&self, _: &Path) -> Result<Adapter, String> {
        js_debug()
    }
}

/// An adapter an extension declares (`[debug_adapters.<id>]`): a command
/// that talks DAP over stdio, or over TCP on the port given in its
/// arguments as `{port}`.
#[derive(Debug)]
pub struct ExtensionAdapter {
    pub id: &'static str,
    pub aliases: Vec<&'static str>,
    pub command: String,
    pub args: Vec<String>,
    pub tcp: bool,
}

impl DebugAdapter for ExtensionAdapter {
    fn id(&self) -> &'static str {
        self.id
    }
    fn aliases(&self) -> &[&'static str] {
        &self.aliases
    }
    fn start(&self, _: &Path) -> Result<Adapter, String> {
        let program = servers::custom(std::slice::from_ref(&self.command))
            .map(|c| c.program)
            .ok_or_else(|| format!("The {} debugger needs {}, which isn't installed", self.id, self.command))?;
        let port = if self.tcp { Some(crate::dap::free_port().map_err(|e| e.to_string())?) } else { None };
        let args = self.args.iter().map(|a| port.map_or(a.clone(), |p| a.replace("{port}", &p.to_string()))).collect();
        Ok(Adapter { id: self.id, program, args, port })
    }
}

static EXTENSION_ADAPTERS: std::sync::RwLock<Vec<&'static ExtensionAdapter>> = std::sync::RwLock::new(Vec::new());

/// Native code's file extensions. lldb-dap debugs C, C++, Objective-C, Swift
/// and Zig (from a debug configuration, or through Attach to Process),
/// whether or not an extension adds the language.
const NATIVE_SOURCES: &[&str] = &["c", "h", "cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "inl", "ipp", "m", "mm", "swift", "zig"];

/// What a breakpoint in a file no debugger stops in gets told.
pub const NO_DEBUGGER: &str = "No debugger can stop in this kind of file";

/// Whether a debugger can stop in `path` (written in `language`), so a
/// breakpoint there means something: languages Kuana debugs itself, native
/// code, and extension languages that come with a debugger.
pub fn can_stop_in(language: Language, path: &Path) -> bool {
    match language {
        Language::Rust | Language::Go | Language::Python | Language::JavaScript | Language::TypeScript | Language::Tsx => return true,
        Language::Extension(ix) if crate::extensions::language(ix).is_some_and(|def| def.debuggable) => return true,
        _ => {}
    }
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    NATIVE_SOURCES.iter().any(|native| native.eq_ignore_ascii_case(extension))
}

/// Replaces the adapters extensions declare (after they reload).
pub fn set_extension_adapters(adapters: Vec<&'static ExtensionAdapter>) {
    *EXTENSION_ADAPTERS.write().unwrap() = adapters;
}

/// The adapter a configuration names, by id or alias: Kuana's, then extensions'.
pub fn find_adapter(name: &str) -> Option<&'static dyn DebugAdapter> {
    let built_in: [&'static dyn DebugAdapter; 4] = [&LldbDap, &Delve, &Debugpy, &JsDebug];
    let named = |a: &&'static dyn DebugAdapter| a.id() == name || a.aliases().contains(&name);
    if let Some(adapter) = built_in.into_iter().find(named) {
        return Some(adapter);
    }
    let extensions = EXTENSION_ADAPTERS.read().unwrap();
    extensions.iter().map(|a| *a as &'static dyn DebugAdapter).find(named)
}

/// The adapter that debugs `spec`.
fn adapter_for(spec: &DebugSpec) -> Option<&'static dyn DebugAdapter> {
    Some(match spec {
        DebugSpec::Cargo { .. } => &LldbDap,
        DebugSpec::Go { .. } => &Delve,
        DebugSpec::Python { .. } => &Debugpy,
        DebugSpec::Node { .. } => &JsDebug,
        // What the process runs decides; anything else is native code.
        DebugSpec::Attach { program, .. } => {
            let name = program.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if name == "node" {
                &JsDebug
            } else if name.starts_with("python") {
                &Debugpy
            } else if is_go_program(program) {
                &Delve
            } else {
                &LldbDap
            }
        }
        DebugSpec::Config { adapter, .. } => return find_adapter(adapter),
    })
}

/// The adapter for `spec`, started, or what to install when it's missing.
pub fn adapter(spec: &DebugSpec, cwd: &Path) -> Result<Adapter, String> {
    match (adapter_for(spec), spec) {
        (Some(adapter), _) => adapter.start(cwd),
        (None, DebugSpec::Config { adapter, .. }) => Err(format!("Kuana has no debugger for \"{adapter}\"")),
        (None, _) => Err("This can't be debugged".into()),
    }
}

fn lldb_dap() -> Result<Adapter, String> {
    let program = servers::which("lldb-dap")
        .or_else(xcrun_lldb_dap)
        .or_else(|| servers::which("lldb-vscode"))
        .ok_or("Debugging Rust needs lldb-dap: install Xcode's command line tools (xcode-select --install) or LLVM.")?;
    Ok(Adapter { id: "lldb-dap", program, args: Vec::new(), port: None })
}

fn delve() -> Result<Adapter, String> {
    let program = servers::which("dlv").ok_or("Debugging Go needs Delve: go install github.com/go-delve/delve/cmd/dlv@latest")?;
    let port = crate::dap::free_port().map_err(|e| e.to_string())?;
    let args = vec!["dap".into(), format!("--listen=127.0.0.1:{port}")];
    Ok(Adapter { id: "go", program, args, port: Some(port) })
}

fn debugpy(cwd: &Path) -> Result<Adapter, String> {
    let python = python_for(cwd).ok_or("Debugging Python needs python3 on PATH.")?;
    let has_debugpy = crate::process::command(&python)
        .args(["-c", "import debugpy"])
        .env("PATH", servers::search_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_debugpy {
        return Err(format!("Debugging Python needs debugpy: {} -m pip install debugpy", python.display()));
    }
    Ok(Adapter { id: "python", program: python, args: vec!["-m".into(), "debugpy.adapter".into()], port: None })
}

/// Microsoft's js-debug, as its standalone DAP server: `js-debug-adapter` on
/// PATH (as Mason installs it), else a release unpacked in the config folder.
fn js_debug() -> Result<Adapter, String> {
    let port = crate::dap::free_port().map_err(|e| e.to_string())?;
    let listen = vec![port.to_string(), "127.0.0.1".to_string()];
    if let Some(program) = servers::which("js-debug-adapter") {
        return Ok(Adapter { id: "pwa-node", program, args: listen, port: Some(port) });
    }
    let server = crate::theme::config_dir().map(|d| d.join("debug-adapters/js-debug/src/dapDebugServer.js")).filter(|p| p.is_file());
    let Some(server) = server else {
        return Err("Debugging JavaScript needs js-debug: unpack js-debug-dap-*.tar.gz from github.com/microsoft/vscode-js-debug/releases \
                    into ~/.config/kuana/debug-adapters (or put js-debug-adapter on PATH)."
            .into());
    };
    let node = servers::which("node").ok_or("Debugging JavaScript needs Node.js on PATH.")?;
    let mut args = vec![server.to_string_lossy().into_owned()];
    args.extend(listen);
    Ok(Adapter { id: "pwa-node", program: node, args, port: Some(port) })
}

/// Go executables carry build info that `go version` reads: "<path>: go1.22.1".
/// (For others it says why not, and still exits with 0.)
fn is_go_program(program: &Path) -> bool {
    let Some(go) = servers::which("go") else { return false };
    let Ok(output) = crate::process::command(go).arg("version").arg(program).stderr(Stdio::null()).output() else { return false };
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().rsplit(": ").next().is_some_and(|version| version.starts_with("go") || version.starts_with("devel"))
}

/// Xcode ships lldb-dap without putting it on PATH.
fn xcrun_lldb_dap() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = crate::process::command("xcrun").args(["-f", "lldb-dap"]).stderr(Stdio::null()).output().ok()?;
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (output.status.success() && path.is_file()).then_some(path)
}

/// The project's venv Python, else python3 on PATH.
fn python_for(cwd: &Path) -> Option<PathBuf> {
    crate::python::venv(cwd).map(|v| crate::python::interpreter(&v)).filter(|p| p.is_file()).or_else(|| servers::which("python3"))
}

/// `initialize` arguments (what Kuana supports as a client).
pub fn initialize_arguments(adapter_id: &str) -> Value {
    json!({
        "clientID": "kuana",
        "clientName": "Kuana",
        "adapterID": adapter_id,
        "pathFormat": "path",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        "supportsVariableType": true,
        "supportsRunInTerminalRequest": false,
        // js-debug debugs each Node process in a child session.
        "supportsStartDebuggingRequest": adapter_id == "pwa-node",
        "locale": "en",
    })
}

/// The request that starts debugging: `attach` to a running process, else `launch`.
pub fn request(spec: &DebugSpec) -> &'static str {
    if spec.is_attach() { "attach" } else { "launch" }
}

/// LLDB setup: Step Into skips the standard library (it ships with line info,
/// and lldb only avoids `std::` by default; legacy symbols name trait impls
/// `_$LT$alloc..vec..Vec…`), and Rust's own formatters, so String, Vec and
/// Option read naturally.
fn lldb_commands(cwd: &Path) -> Vec<String> {
    let mut commands = vec![r"settings set target.process.thread.step-avoid-regexp ^(<|_\$LT\$)?(std|core|alloc)(::|\.\.)".to_string()];
    commands.extend(rust_formatters(cwd).unwrap_or_default());
    commands
}

/// Arguments for [`request`]. `program` is the executable a cargo build
/// made; `adapter_id` says which adapter attaches.
pub fn launch_arguments(spec: &DebugSpec, cwd: &Path, program: Option<&Path>, adapter_id: &str) -> Value {
    match spec {
        DebugSpec::Cargo { args, .. } => json!({
            "program": program.map(|p| p.to_string_lossy().into_owned()),
            "args": args,
            "cwd": cwd,
            "stopOnEntry": false,
            "initCommands": lldb_commands(cwd),
        }),
        DebugSpec::Go { mode, program, args } => json!({
            "mode": mode,
            "program": program,
            "args": args,
            "cwd": cwd,
            // Program output as DAP output events, not Delve's own stdout.
            "outputMode": "remote",
            // The build goes to the temp folder, not the project (Delve removes it after).
            "output": std::env::temp_dir().join(format!("kuana-debug-{}-{}", std::process::id(), unique())),
        }),
        DebugSpec::Python { program, module, args } => {
            let mut launch = json!({
                "args": args,
                "cwd": cwd,
                "console": "internalConsole",
                "justMyCode": true,
                "redirectOutput": true,
            });
            if let Some(program) = program {
                launch["program"] = json!(program);
            }
            if let Some(module) = module {
                launch["module"] = json!(module);
            }
            launch
        }
        // js-debug reads `request` from the configuration, not the command.
        DebugSpec::Node { runtime, args } => json!({
            "type": "pwa-node",
            "request": "launch",
            "runtimeExecutable": runtime,
            "runtimeArgs": args,
            "cwd": cwd,
            // The program's stdout and stderr as output events.
            "console": "internalConsole",
            "outputCapture": "std",
            "skipFiles": ["<node_internals>/**"],
            // Source maps for the project's code (TypeScript), not its dependencies'.
            "resolveSourceMapLocations": ["**", "!**/node_modules/**"],
        }),
        DebugSpec::Attach { pid, program } => match adapter_id {
            "go" => json!({ "mode": "local", "processId": pid }),
            "python" => json!({ "processId": pid, "justMyCode": true }),
            // The inspector's port comes from `open_node_inspector`.
            "pwa-node" => json!({ "type": "pwa-node", "request": "attach", "skipFiles": ["<node_internals>/**"] }),
            // The executable too, or lldb finds no debug info for it.
            _ => json!({ "pid": pid, "program": program, "initCommands": lldb_commands(cwd) }),
        },
        // As written, in the project's folder unless it says otherwise.
        DebugSpec::Config { request, config, .. } => {
            let mut config = config.clone();
            if config.get("cwd").is_none() {
                config["cwd"] = json!(cwd);
            }
            // js-debug reads these from the configuration itself.
            if adapter_id == "pwa-node" {
                config["type"] = json!("pwa-node");
                config["request"] = json!(request);
            }
            config
        }
    }
}

/// The debug configurations a project has written for VS Code
/// (`.vscode/launch.json`) or Zed (`.zed/debug.json`), with their variables
/// filled in (`${workspaceFolder}`, `${file}`, `${env:HOME}`; Zed's
/// `$ZED_WORKTREE_ROOT`, `$ZED_FILE`…). `file` is the current file.
pub fn project_configurations(root: &Path, file: Option<&Path>) -> Vec<RunTarget> {
    let read = |path: &Path| -> Option<Value> { serde_json::from_str(&crate::settings::strip_jsonc(&std::fs::read_to_string(path).ok()?)).ok() };
    // (name, adapter, configuration)
    let mut found: Vec<(String, String, Value)> = Vec::new();
    if let Some(launch) = read(&root.join(".vscode/launch.json")) {
        for config in launch["configurations"].as_array().into_iter().flatten() {
            if let (Some(name), Some(kind)) = (config["name"].as_str(), config["type"].as_str()) {
                found.push((name.to_string(), kind.to_string(), config.clone()));
            }
        }
    }
    if let Some(Value::Array(configs)) = read(&root.join(".zed/debug.json")) {
        for config in configs {
            if let (Some(name), Some(adapter)) = (config["label"].as_str(), config["adapter"].as_str()) {
                found.push((name.to_string(), adapter.to_string(), config.clone()));
            }
        }
    }
    let lookup = |name: &str| variable(name, root, file);
    found
        .into_iter()
        .map(|(name, adapter, mut config)| {
            fill_in(&mut config, &lookup);
            let request = config["request"].as_str().unwrap_or("launch").to_string();
            if let Value::Object(map) = &mut config {
                for key in ["name", "label", "type", "adapter", "request", "preLaunchTask", "postDebugTask"] {
                    map.remove(key);
                }
            }
            let cwd = config["cwd"].as_str().map(PathBuf::from).unwrap_or_else(|| root.to_path_buf());
            RunTarget { name, command: String::new(), cwd, row: None, rows: 0..0, debug: Some(DebugSpec::Config { adapter, request, config }), test: None }
        })
        .collect()
}

/// A configuration variable's value: VS Code's names or Zed's.
fn variable(name: &str, root: &Path, file: Option<&Path>) -> Option<String> {
    let text = |p: &Path| p.to_string_lossy().into_owned();
    if let Some(var) = name.strip_prefix("env:") {
        return Some(std::env::var(var).unwrap_or_default());
    }
    let file_part = |f: fn(&Path) -> Option<String>| file.and_then(f);
    match name {
        "workspaceFolder" | "workspaceRoot" | "ZED_WORKTREE_ROOT" => Some(text(root)),
        "workspaceFolderBasename" => root.file_name().map(|n| n.to_string_lossy().into_owned()),
        "file" | "ZED_FILE" => file.map(text),
        "relativeFile" | "ZED_RELATIVE_FILE" => file.map(|f| text(f.strip_prefix(root).unwrap_or(f))),
        "fileDirname" | "ZED_DIRNAME" => file_part(|f| Some(f.parent()?.to_string_lossy().into_owned())),
        "fileBasename" | "ZED_FILENAME" => file_part(|f| Some(f.file_name()?.to_string_lossy().into_owned())),
        "fileBasenameNoExtension" | "ZED_STEM" => file_part(|f| Some(f.file_stem()?.to_string_lossy().into_owned())),
        "fileExtname" => file_part(|f| Some(format!(".{}", f.extension()?.to_string_lossy()))),
        "userHome" => std::env::var("HOME").ok(),
        "pathSeparator" => Some(std::path::MAIN_SEPARATOR.to_string()),
        _ => None,
    }
}

/// Fills in `${name}` and `$ZED_NAME` in every string of `value`; names it
/// doesn't know stay as written.
fn fill_in(value: &mut Value, lookup: &dyn Fn(&str) -> Option<String>) {
    match value {
        Value::String(text) => *text = substitute(text, lookup),
        Value::Array(items) => items.iter_mut().for_each(|v| fill_in(v, lookup)),
        Value::Object(map) => map.values_mut().for_each(|v| fill_in(v, lookup)),
        _ => {}
    }
}

fn substitute(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, len) = match after.strip_prefix('{').and_then(|b| Some((&b[..b.find('}')?], b.find('}')? + 2))) {
            Some(braced) => braced,
            None => {
                let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
                // Bare names only for Zed's variables ($HOME stays for the shell).
                if after[..end].starts_with("ZED_") { (&after[..end], end) } else { ("", 0) }
            }
        };
        match (!name.is_empty()).then(|| lookup(name)).flatten() {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[i..i + 1 + len]),
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// Node listens for a debugger after a SIGUSR1 (js-debug's own server
/// doesn't send it, unlike VS Code): sends it and returns the inspector's
/// port, from `--inspect…` flags or Node's default.
pub fn open_node_inspector(pid: u32) -> u16 {
    let pid = pid.to_string();
    let command = crate::process::command("ps").args(["-o", "args=", "-p", &pid]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let _ = crate::process::command("kill").args(["-USR1", &pid]).status();
    inspector_port(&command.unwrap_or_default()).unwrap_or(9229)
}

/// The port of `--inspect[-brk|-wait|-port][=[host:]port]`.
fn inspector_port(command: &str) -> Option<u16> {
    command.split_whitespace().find_map(|arg| {
        let rest = ["--inspect-brk", "--inspect-wait", "--inspect-port", "--inspect"].iter().find_map(|flag| arg.strip_prefix(flag))?;
        match rest.strip_prefix('=') {
            Some(value) => value.rsplit(':').next()?.parse().ok(),
            None => rest.is_empty().then_some(9229),
        }
    })
}

/// Different on every call (a Go build's name).
fn unique() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

/// A running process of the user's, for Attach to Process.
pub struct Process {
    pub pid: u32,
    /// The executable's file name.
    pub name: String,
    /// The command line.
    pub command: String,
    pub program: PathBuf,
}

/// The user's processes, newest first (from `ps`), without Kuana itself.
pub fn processes() -> Vec<Process> {
    let column = |name: &str| -> Vec<(u32, String)> {
        let Ok(output) = crate::process::command("ps").args(["x", "-o", "pid=", "-o"]).arg(format!("{name}=")).output() else { return Vec::new() };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (pid, rest) = line.trim_start().split_once(' ')?;
                Some((pid.parse().ok()?, rest.trim().to_string()))
            })
            .collect()
    };
    let commands: std::collections::HashMap<u32, String> = column("args").into_iter().collect();
    let mut processes: Vec<Process> = column("comm")
        .into_iter()
        .filter(|(pid, _)| *pid != std::process::id())
        .map(|(pid, comm)| {
            // Linux's `comm` is a short name; the real path is in /proc.
            let program = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap_or_else(|_| PathBuf::from(&comm));
            let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(comm);
            let command = commands.get(&pid).cloned().unwrap_or_default();
            Process { pid, name, command, program }
        })
        .collect();
    processes.sort_by(|a, b| b.pid.cmp(&a.pid));
    processes
}

/// LLDB commands that load the toolchain's Rust formatters.
fn rust_formatters(cwd: &Path) -> Option<Vec<String>> {
    let output = crate::process::command("rustc")
        .args(["--print", "sysroot"])
        .current_dir(cwd)
        .env("PATH", servers::search_path())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let etc = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).join("lib/rustlib/etc");
    let (lookup, commands) = (etc.join("lldb_lookup.py"), etc.join("lldb_commands"));
    (lookup.is_file() && commands.is_file()).then(|| {
        vec![
            format!("command script import \"{}\"", lookup.display()),
            format!("command source -s 0 \"{}\"", commands.display()),
        ]
    })
}

/// A running cargo build, which Stop kills.
pub type Build = Arc<Mutex<Option<std::process::Child>>>;

/// Builds with cargo (`build …` or `test --no-run …`) and returns the
/// executable it made. Cargo's progress and compiler messages go to
/// `on_line` as they come. Blocks: run it off the main thread.
pub fn cargo_build(cwd: &Path, args: &[String], build: &Build, on_line: impl FnMut(String) + Send + 'static) -> Result<PathBuf, String> {
    let mut command = crate::process::command("cargo");
    command
        .args(args)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(cwd)
        .env("PATH", servers::search_path())
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("couldn't run cargo: {e}"))?;
    let stdout = child.stdout.take().ok_or("no output from cargo")?;
    let stderr = child.stderr.take().ok_or("no output from cargo")?;
    *build.lock().unwrap() = Some(child);
    // Progress and rendered diagnostics come on stderr.
    // Read to the end even past a line that isn't UTF-8: cargo would fail
    // writing to a pipe nobody reads.
    let progress = std::thread::spawn(move || crate::lsp::rpc::for_each_line(stderr, on_line));
    let mut artifacts: Vec<Value> = Vec::new();
    crate::lsp::rpc::for_each_line(stdout, |line| {
        if let Ok(message) = serde_json::from_str::<Value>(&line)
            && message["reason"] == "compiler-artifact"
            && message["executable"].is_string()
        {
            artifacts.push(message);
        }
    });
    let _ = progress.join();
    let status = build.lock().unwrap().take().map(|mut child| child.wait());
    match status {
        Some(Ok(status)) if status.success() => {}
        Some(Ok(_)) => return Err("The build failed".into()),
        Some(Err(err)) => return Err(err.to_string()),
        None => return Err("The build was stopped".into()),
    }
    pick_executable(&artifacts, args).ok_or_else(|| "cargo didn't build an executable for this".into())
}

/// The executable among cargo's artifacts: the test binary for a test
/// build, else the binary asked for by name (or the last one).
fn pick_executable(artifacts: &[Value], args: &[String]) -> Option<PathBuf> {
    let test = args.first().is_some_and(|a| a == "test");
    let wanted = args.iter().position(|a| a == "--bin" || a == "--example").and_then(|i| args.get(i + 1));
    let candidates: Vec<&Value> = artifacts.iter().filter(|a| a["profile"]["test"].as_bool() == Some(test)).collect();
    let named = wanted.and_then(|name| candidates.iter().rev().find(|a| a["target"]["name"].as_str() == Some(name.as_str())));
    named.or(candidates.last()).and_then(|a| a["executable"].as_str()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_go_where_a_debugger_stops() {
        assert!(can_stop_in(Language::Rust, Path::new("src/main.rs")));
        assert!(can_stop_in(Language::Tsx, Path::new("app.tsx")));
        // lldb-dap debugs C and C++ with or without a language for them.
        assert!(can_stop_in(Language::PlainText, Path::new("main.c")));
        assert!(can_stop_in(Language::PlainText, Path::new("include/Thing.HPP")));
        assert!(!can_stop_in(Language::Markdown, Path::new("README.md")));
        assert!(!can_stop_in(Language::PlainText, Path::new("notes.txt")));
        assert!(!can_stop_in(Language::Json, Path::new("package.json")));
    }

    #[test]
    fn reads_vs_code_and_zed_debug_configurations() {
        let root = std::env::temp_dir().join(format!("kuana-debug-configs-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".vscode")).unwrap();
        std::fs::create_dir_all(root.join(".zed")).unwrap();
        std::fs::write(
            root.join(".vscode/launch.json"),
            r#"{
              // VS Code allows comments and trailing commas here.
              "version": "0.2.0",
              "configurations": [
                { "type": "node", "request": "launch", "name": "Server", "program": "${workspaceFolder}/server.js",
                  "env": { "HOME_DIR": "${env:KUANA_TEST_DEBUG_VAR}" }, "preLaunchTask": "build", },
                { "type": "debugpy", "request": "attach", "name": "Attach", "connect": { "port": 5678 } },
              ],
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".zed/debug.json"),
            r#"[{ "label": "Go tests", "adapter": "Delve", "request": "launch", "mode": "test", "program": "$ZED_WORKTREE_ROOT/pkg", "cwd": "${fileDirname}" }]"#,
        )
        .unwrap();
        // SAFETY: only this test reads the variable.
        unsafe { std::env::set_var("KUANA_TEST_DEBUG_VAR", "/home/k") };
        let file = root.join("src/main.go");
        let targets = project_configurations(&root, Some(&file));
        let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Server", "Attach", "Go tests"]);
        let Some(DebugSpec::Config { adapter, request, config }) = &targets[0].debug else { panic!() };
        assert_eq!((adapter.as_str(), request.as_str()), ("node", "launch"));
        assert_eq!(config["program"], json!(format!("{}/server.js", root.display())));
        assert_eq!(config["env"]["HOME_DIR"], "/home/k");
        assert!(config.get("name").is_none() && config.get("preLaunchTask").is_none() && config.get("type").is_none());
        assert!(targets[1].debug.as_ref().unwrap().is_attach());
        let Some(DebugSpec::Config { adapter, config, .. }) = &targets[2].debug else { panic!() };
        assert_eq!((adapter.as_str(), &config["program"]), ("Delve", &json!(format!("{}/pkg", root.display()))));
        assert_eq!(targets[2].cwd, root.join("src"));
        // Unknown variables stay as written; $HOME is the shell's.
        assert_eq!(substitute("${nope} $HOME $ZED_NOPE", &|_| None), "${nope} $HOME $ZED_NOPE");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn configurations_name_adapters_the_way_editors_do() {
        let id = |name: &str| find_adapter(name).map(|a| a.id());
        assert_eq!(id("node"), Some("pwa-node"));
        assert_eq!(id("JavaScript"), Some("pwa-node"));
        assert_eq!(id("Debugpy"), Some("python"));
        assert_eq!(id("Delve"), Some("go"));
        assert_eq!(id("CodeLLDB"), Some("lldb-dap"));
        assert_eq!(id("chrome"), None);

        // As written, with the folder and what js-debug reads from it.
        let spec = DebugSpec::Config { adapter: "node".into(), request: "attach".into(), config: json!({ "port": 9229 }) };
        let args = launch_arguments(&spec, Path::new("/p"), None, "pwa-node");
        assert_eq!(args, json!({ "port": 9229, "cwd": "/p", "type": "pwa-node", "request": "attach" }));
        assert_eq!(request(&spec), "attach");
    }

    #[test]
    fn extension_adapters_listen_on_a_port_of_their_own() {
        let adapter = ExtensionAdapter { id: "test-dap", aliases: vec![], command: "sh".into(), args: vec!["--port={port}".into()], tcp: true };
        let started = adapter.start(Path::new("/")).unwrap();
        let port = started.port.unwrap();
        assert_eq!(started.args, [format!("--port={port}")]);
        assert_eq!(started.id, "test-dap");
    }

    #[test]
    fn picks_the_executable_cargo_made() {
        let artifacts = vec![
            json!({"target": {"name": "kuana", "kind": ["bin"]}, "profile": {"test": false}, "executable": "/t/debug/kuana"}),
            json!({"target": {"name": "helper", "kind": ["bin"]}, "profile": {"test": false}, "executable": "/t/debug/helper"}),
            json!({"target": {"name": "kuana", "kind": ["bin"]}, "profile": {"test": true}, "executable": "/t/debug/deps/kuana-1a2b"}),
        ];
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_executable(&artifacts, &args(&["build", "--bin", "kuana"])), Some("/t/debug/kuana".into()));
        assert_eq!(pick_executable(&artifacts, &args(&["build"])), Some("/t/debug/helper".into()));
        assert_eq!(pick_executable(&artifacts, &args(&["test", "--no-run", "--bin", "kuana"])), Some("/t/debug/deps/kuana-1a2b".into()));
    }

    #[test]
    fn launch_arguments_per_adapter() {
        let go = DebugSpec::Go { mode: "test".into(), program: "/p/pkg".into(), args: vec!["-test.run".into(), "^TestA$".into()] };
        let launch = launch_arguments(&go, Path::new("/p/pkg"), None, "go");
        assert_eq!((launch["mode"].as_str(), launch["args"][1].as_str()), (Some("test"), Some("^TestA$")));
        let py = DebugSpec::Python { program: None, module: Some("pytest".into()), args: vec!["t.py::test_a".into()] };
        let launch = launch_arguments(&py, Path::new("/p"), None, "python");
        assert_eq!((launch["module"].as_str(), launch.get("program")), (Some("pytest"), None));
        let node = DebugSpec::Node { runtime: "npm".into(), args: vec!["run".into(), "dev".into()] };
        let launch = launch_arguments(&node, Path::new("/p"), None, "pwa-node");
        assert_eq!((launch["type"].as_str(), launch["runtimeArgs"][1].as_str()), (Some("pwa-node"), Some("dev")));
        let attach = DebugSpec::Attach { pid: 42, program: "/usr/local/bin/node".into() };
        assert_eq!(request(&attach), "attach");
        assert_eq!(launch_arguments(&attach, Path::new("/p"), None, "pwa-node")["request"], "attach");
        assert_eq!(inspector_port("node --inspect=127.0.0.1:9333 server.js"), Some(9333));
        assert_eq!(inspector_port("node --inspect-brk server.js"), Some(9229));
        assert_eq!(inspector_port("node server.js --inspector"), None);
        assert_eq!(launch_arguments(&attach, Path::new("/p"), None, "go")["processId"], 42);
        assert_eq!(launch_arguments(&attach, Path::new("/p"), None, "lldb-dap")["pid"], 42);
    }

    #[cfg(unix)]
    #[test]
    fn lists_processes() {
        let all = processes();
        assert!(all.iter().all(|p| p.pid != std::process::id()));
        assert!(all.windows(2).all(|w| w[0].pid > w[1].pid));
        assert!(all.iter().any(|p| !p.name.is_empty() && !p.command.is_empty()), "no processes from ps");
    }
}
