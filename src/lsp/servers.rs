//! Which language server each language uses, and finding it on disk: the
//! project's (`node_modules/.bin`, a venv), then one on PATH, then a copy Kuana
//! installed itself (see `install.rs`, the status bar's Install).

use crate::fs::PathExt as _;
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use serde_json::{Value, json};

use crate::{
    extensions::wasm::{Asking, WasmServers},
    settings::LspSettings,
    syntax::Language,
};

pub struct ServerSpec {
    /// Shared by languages one server handles (JS/TS/TSX).
    pub id: &'static str,
    /// Executables to try, in order, with their arguments.
    pub candidates: &'static [(&'static str, &'static [&'static str])],
    /// How to install the first candidate.
    pub install: &'static str,
    /// Files that mark a project root for this server.
    pub root_markers: &'static [&'static str],
    /// From an extension's manifest: what it passes the server.
    pub initialization_options: Option<&'static Value>,
    pub settings: Option<&'static Value>,
    /// An extension's servers its own code starts (`candidates` are then
    /// only the guess when the code fails).
    pub wasm: Option<&'static WasmServers>,
}

/// The fields a spec usually leaves empty, for `..SPEC`.
pub const SPEC: ServerSpec =
    ServerSpec { id: "", candidates: &[], install: "", root_markers: &[], initialization_options: None, settings: None, wasm: None };

/// GitHub Copilot's language server, which every file gets beside its own
/// servers when Copilot suggests edits (`edit_prediction_provider`): it only
/// gives ghost text (`textDocument/inlineCompletion`), so editors keep it
/// out of their other requests (see `lsp::copilot`).
pub static COPILOT: ServerSpec = ServerSpec {
    id: "copilot",
    candidates: &[("copilot-language-server", &["--stdio"])],
    install: "npm install -g @github/copilot-language-server",
    ..SPEC
};

/// For a language whose server is only a command in `language_servers`.
pub static CUSTOM: ServerSpec = ServerSpec { id: "custom", install: "Check the command in language_servers (settings.json)", ..SPEC };

/// The main server for `language` (see `specs`).
pub fn spec(language: Language) -> Option<&'static ServerSpec> {
    specs(language).into_iter().next()
}

/// Every server a `language` document gets, the main one first: Kuana's own
/// choice, then the ones extensions bring, which run beside it (ESLint or
/// Tailwind beside TypeScript). Requests go to the first server that can
/// answer; diagnostics, completion and code actions come from all. An
/// extension's server that's the same program as an earlier one (a choice
/// Kuana already makes) isn't started twice.
pub fn specs(language: Language) -> Vec<&'static ServerSpec> {
    let builtin = builtin(language).into_iter().chain(beside(language)).collect();
    combine(builtin, crate::extensions::servers_for(language.name()))
}

fn combine(builtin: Vec<&'static ServerSpec>, extensions: Vec<&'static ServerSpec>) -> Vec<&'static ServerSpec> {
    let mut out = builtin;
    for spec in extensions {
        let same = |s: &&ServerSpec| s.candidates.iter().any(|(a, _)| spec.candidates.iter().any(|(b, _)| a == b));
        if !out.iter().any(same) {
            out.push(spec);
        }
    }
    out
}

/// Kuana's own server for a built-in language.
fn builtin(language: Language) -> Option<&'static ServerSpec> {
    static RUST: ServerSpec = ServerSpec {
        id: "rust-analyzer",
        candidates: &[("rust-analyzer", &[])],
        install: "rustup component add rust-analyzer",
        root_markers: &["Cargo.toml"],
        ..SPEC
    };
    static GO: ServerSpec = ServerSpec {
        id: "gopls",
        candidates: &[("gopls", &[])],
        install: "go install golang.org/x/tools/gopls@latest",
        root_markers: &["go.mod", "go.work"],
        ..SPEC
    };
    static TYPESCRIPT: ServerSpec = ServerSpec {
        id: "typescript",
        candidates: &[("typescript-language-server", &["--stdio"]), ("vtsls", &["--stdio"])],
        install: "npm install -g typescript typescript-language-server",
        root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
        ..SPEC
    };
    static PYTHON: ServerSpec = ServerSpec {
        id: "python",
        candidates: &[
            ("basedpyright-langserver", &["--stdio"]),
            ("pyright-langserver", &["--stdio"]),
            ("pylsp", &[]),
        ],
        install: "pip install basedpyright",
        root_markers: &["pyproject.toml", "setup.py", "requirements.txt"],
        ..SPEC
    };
    // phpactor first, as in Zed (intelephense reports types it can't find in
    // most projects), when PHP is there to run it (`usable`); on Windows
    // intelephense, as in Zed.
    static PHP: ServerSpec = ServerSpec {
        id: "php",
        candidates: if cfg!(windows) {
            &[("intelephense", &["--stdio"]), ("phpactor", &["language-server"])]
        } else {
            &[("phpactor", &["language-server"]), ("intelephense", &["--stdio"])]
        },
        install: "npm install -g intelephense",
        root_markers: &["composer.json"],
        ..SPEC
    };
    static CSS: ServerSpec = ServerSpec {
        id: "css",
        candidates: &[("vscode-css-language-server", &["--stdio"])],
        install: "npm install -g vscode-langservers-extracted",
        root_markers: &["package.json"],
        ..SPEC
    };
    static HTML: ServerSpec = ServerSpec {
        id: "html",
        candidates: &[("vscode-html-language-server", &["--stdio"])],
        install: "npm install -g vscode-langservers-extracted",
        root_markers: &["package.json"],
        ..SPEC
    };
    static JSON: ServerSpec = ServerSpec {
        id: "json",
        candidates: &[("vscode-json-language-server", &["--stdio"])],
        install: "npm install -g vscode-langservers-extracted",
        root_markers: &["package.json"],
        ..SPEC
    };
    static YAML: ServerSpec = ServerSpec {
        id: "yaml",
        candidates: &[("yaml-language-server", &["--stdio"])],
        install: "npm install -g yaml-language-server",
        root_markers: &[],
        ..SPEC
    };
    static TOML: ServerSpec = ServerSpec {
        id: "taplo",
        candidates: &[("taplo", &["lsp", "stdio"])],
        install: "cargo install taplo-cli --locked --features lsp",
        root_markers: &[],
        ..SPEC
    };
    static SHELL: ServerSpec = ServerSpec {
        id: "bash",
        candidates: &[("bash-language-server", &["start"])],
        install: "npm install -g bash-language-server",
        root_markers: &[],
        ..SPEC
    };
    static MARKDOWN: ServerSpec = ServerSpec {
        id: "marksman",
        candidates: &[("marksman", &["server"])],
        install: "brew install marksman",
        root_markers: &[],
        ..SPEC
    };
    Some(match language {
        Language::Rust => &RUST,
        Language::Go => &GO,
        Language::JavaScript | Language::TypeScript | Language::Tsx => &TYPESCRIPT,
        Language::Python => &PYTHON,
        Language::Php => &PHP,
        Language::Css => &CSS,
        Language::Html => &HTML,
        Language::Json => &JSON,
        Language::Yaml => &YAML,
        Language::Toml => &TOML,
        Language::Shell => &SHELL,
        Language::Markdown => &MARKDOWN,
        Language::PlainText | Language::Extension(_) => return None,
    })
}

/// ESLint's language server (VS Code's, from `vscode-langservers-extracted`):
/// the project's own ESLint and config, as you type, with its quick fixes.
pub static ESLINT: ServerSpec = ServerSpec {
    id: "eslint",
    candidates: &[("vscode-eslint-language-server", &["--stdio"])],
    install: "npm install -g vscode-langservers-extracted",
    root_markers: &["package.json"],
    ..SPEC
};

/// A server Kuana runs beside a language's own when it's installed (and
/// says nothing when it isn't), as Zed does: Ruff's beside basedpyright, for
/// its lint rules (pyflakes' and pycodestyle's errors, by default) and fixes;
/// ESLint's beside TypeScript's, in projects that use ESLint (`wanted`).
fn beside(language: Language) -> Option<&'static ServerSpec> {
    static RUFF: ServerSpec = ServerSpec {
        id: "ruff",
        candidates: &[("ruff", &["server"])],
        install: "pip install ruff",
        root_markers: &["pyproject.toml", "ruff.toml", ".ruff.toml"],
        ..SPEC
    };
    match language {
        Language::Python => Some(&RUFF),
        Language::JavaScript | Language::TypeScript | Language::Tsx => Some(&ESLINT),
        _ => None,
    }
}

/// Whether a server is for `file` at all: ESLint's only where the project
/// has ESLint installed (its server runs the project's own library, and
/// elsewhere would only complain that there's none).
pub fn wanted(spec: &ServerSpec, file: &Path) -> bool {
    !std::ptr::eq(spec, &ESLINT) || crate::lint::external::find_for(file).is_some()
}

/// The settings ESLint's server asks for (`workspace/configuration` with
/// the file as its scope), as VS Code's ESLint extension sends them: Kuana's
/// defaults and the user's `lsp.eslint.settings` (`settings`), with the
/// project folder and, unless set, whether the project uses a flat config
/// (`eslint.config.*` from the file's folder up to `root`).
pub fn eslint_configuration(settings: &Value, scope: Option<&Path>, root: &Path) -> Value {
    let mut out = settings.clone();
    if !out.is_object() {
        out = json!({});
    }
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    out["workspaceFolder"] = json!({ "uri": crate::lsp::protocol::path_to_uri(root), "name": name });
    let set = out.get("useFlatConfig").is_some_and(|v| !v.is_null()) || out.pointer("/experimental/useFlatConfig").is_some_and(|v| !v.is_null());
    if !set {
        const FLAT: [&str; 6] = ["eslint.config.js", "eslint.config.mjs", "eslint.config.cjs", "eslint.config.ts", "eslint.config.mts", "eslint.config.cts"];
        let start = scope.and_then(Path::parent).unwrap_or(root);
        let flat = start.ancestors().take_while(|d| d.starts_with(root)).any(|d| FLAT.iter().any(|f| d.join(f).is_file_on_host()));
        out["useFlatConfig"] = json!(flat);
        merge(&mut out, &json!({ "experimental": { "useFlatConfig": flat } }));
    }
    out
}

/// Whether a server program can run here: phpactor needs PHP (Kuana's copy
/// is its `.phar`). Read where servers are found, off the main thread first.
pub fn usable(program: &str) -> bool {
    static PHP: LazyLock<bool> = LazyLock::new(|| which("php").is_some());
    program != "phpactor" || *PHP
}

/// The `languageId` for `textDocument/didOpen`.
pub fn language_id(language: Language, path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    match language {
        Language::JavaScript if ext == "jsx" => "javascriptreact",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Tsx => "typescriptreact",
        Language::Json if ext == "jsonc" => "jsonc",
        Language::Json => "json",
        Language::Html => "html",
        Language::Css => "css",
        Language::Rust => "rust",
        Language::Go => "go",
        Language::Python => "python",
        Language::Php => "php",
        Language::Shell => "shellscript",
        Language::Yaml => "yaml",
        Language::Toml => "toml",
        Language::Markdown => "markdown",
        Language::PlainText => "plaintext",
        Language::Extension(_) => language.extension().map_or("plaintext", |l| l.id),
    }
}

/// The project root a server should use for `file`: the project folder
/// holding it, if any, else the nearest folder with a root marker.
pub fn root_for(spec: &ServerSpec, file: &Path, folder: Option<&Path>) -> PathBuf {
    if let Some(root) = folder.filter(|r| file.starts_with(r)) {
        return root.to_path_buf();
    }
    let dir = file.parent().unwrap_or(file);
    dir.ancestors()
        .find(|d| spec.root_markers.iter().chain(&[".git"]).any(|m| d.join(m).exists_on_host()))
        .unwrap_or(dir)
        .to_path_buf()
}

/// Zed's id for a server program where it isn't the program's name, so
/// `lsp.<id>` settings written for Zed find it.
pub fn zed_id(program: &str) -> &str {
    match program {
        "basedpyright-langserver" => "basedpyright",
        "pyright-langserver" => "pyright",
        "vscode-json-language-server" => "json-language-server",
        other => other,
    }
}

/// The user's `lsp.<id>` entry for a server: by Kuana's id, else by the
/// program's Zed id or name (`program`, else any of its candidates').
fn user_entry<'a>(spec: &ServerSpec, program: Option<&str>, user: &'a HashMap<String, LspSettings>) -> Option<&'a LspSettings> {
    let mut keys = vec![spec.id];
    match program {
        Some(program) => keys.extend([zed_id(program), program]),
        None => keys.extend(spec.candidates.iter().flat_map(|(program, _)| [zed_id(program), *program])),
    }
    keys.into_iter().find_map(|key| user.get(key))
}

/// The program `lsp.<id>.binary` sets for a server, if any.
pub fn user_binary(spec: &ServerSpec, user: &HashMap<String, LspSettings>) -> Option<Command> {
    let binary = user_entry(spec, None, user)?.binary.as_ref()?;
    let mut command = vec![binary.path.clone()?];
    command.extend(binary.arguments.clone().unwrap_or_default());
    custom(&command)
}

/// What a server is started with and told: its initialization options and
/// settings (`Null` for none).
#[derive(Clone, Debug, PartialEq)]
pub struct ServerConfig {
    pub initialization_options: Value,
    pub settings: Value,
}

/// What extensions' code said about a server: its own extension's, with
/// what other extensions add to it.
#[derive(Clone, Debug, Default)]
pub struct FromCode {
    /// Zed's id for the server, when its extension's code started it: its
    /// `lsp.<id>` settings go by that, and reach it through that code.
    pub server: Option<String>,
    pub initialization_options: Value,
    pub settings: Value,
    /// Whom to ask for `settings` again when the user's settings change.
    pub asking: Option<Asking>,
}

/// A server's configuration: Kuana's defaults for its program, then its
/// extension's manifest's, then what extensions' code said (taken as it is:
/// no dotted keys expanded), then the user's `lsp.<id>`, each merged over the
/// one before. A server its extension's code started gets the user's
/// settings from that code instead, as in Zed (Dart's put them in a section).
pub fn config(spec: &ServerSpec, program: &Path, user: &HashMap<String, LspSettings>, code: &FromCode) -> ServerConfig {
    let name = program.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (mut initialization_options, mut settings) = defaults(&name);
    merge(&mut initialization_options, spec.initialization_options.unwrap_or(&Value::Null));
    merge(&mut settings, &expand_dotted(spec.settings.unwrap_or(&Value::Null)));
    merge(&mut initialization_options, &code.initialization_options);
    merge(&mut settings, &code.settings);
    let entry = code.server.as_deref().and_then(|id| user.get(id)).or_else(|| user_entry(spec, Some(&name), user));
    if let Some(entry) = entry {
        merge(&mut initialization_options, entry.initialization_options.as_ref().unwrap_or(&Value::Null));
        if code.server.is_none() {
            merge(&mut settings, &expand_dotted(entry.settings.as_ref().unwrap_or(&Value::Null)));
        }
    }
    ServerConfig { initialization_options, settings }
}

/// Settings with dotted keys as nested objects, the way servers ask for
/// them: `{"basedpyright.analysis": {…}}` (as Zed's and VS Code's settings
/// write it) becomes `{"basedpyright": {"analysis": {…}}}`.
pub fn expand_dotted(value: &Value) -> Value {
    let Value::Object(map) = value else { return value.clone() };
    let mut out = Value::Object(serde_json::Map::new());
    for (key, value) in map {
        let mut nested = expand_dotted(value);
        for part in key.rsplit('.') {
            nested = json!({ part: nested });
        }
        merge(&mut out, &nested);
    }
    out
}

/// What Kuana passes a server program unless told otherwise:
/// (initialization options, settings). Inlay hints are off in TypeScript's
/// and Go's servers until asked for, and formatting in the JSON, CSS and
/// HTML servers (which VS Code's own extensions turn on). basedpyright
/// checks in its "recommended" mode unless told otherwise, which warns on
/// nearly every line of untyped code; Zed tones it down to "standard" (what
/// pyright itself uses), and so does Kuana. A project's `pyrightconfig.json`
/// or `[tool.basedpyright]` still wins over this.
fn defaults(program: &str) -> (Value, Value) {
    match program {
        // Ruff organizes imports beside it, as in Zed.
        "basedpyright-langserver" => {
            (Value::Null, json!({ "basedpyright": { "analysis": { "typeCheckingMode": "standard" }, "disableOrganizeImports": true } }))
        }
        "typescript-language-server" => {
            let hints = json!({
                "includeInlayParameterNameHints": "all",
                "includeInlayParameterNameHintsWhenArgumentMatchesName": false,
                "includeInlayFunctionParameterTypeHints": true,
                "includeInlayVariableTypeHints": true,
                "includeInlayVariableTypeHintsWhenTypeMatchesName": false,
                "includeInlayPropertyDeclarationTypeHints": true,
                "includeInlayFunctionLikeReturnTypeHints": true,
                "includeInlayEnumMemberValueHints": true,
            });
            (json!({ "preferences": hints }), Value::Null)
        }
        "vtsls" => {
            let hints = json!({ "inlayHints": {
                "parameterNames": { "enabled": "all", "suppressWhenArgumentMatchesName": true },
                "parameterTypes": { "enabled": true },
                "variableTypes": { "enabled": true, "suppressWhenTypeMatchesName": true },
                "propertyDeclarationTypes": { "enabled": true },
                "functionLikeReturnTypes": { "enabled": true },
                "enumMemberValues": { "enabled": true },
            } });
            (Value::Null, json!({ "typescript": hints, "javascript": hints }))
        }
        "gopls" => {
            let hints = json!({
                "assignVariableTypes": true,
                "compositeLiteralFields": true,
                "compositeLiteralTypes": true,
                "constantValues": true,
                "functionTypeParameters": true,
                "parameterNames": true,
                "rangeVariableTypes": true,
            });
            (Value::Null, json!({ "gopls": { "hints": hints } }))
        }
        // Schemas for Kuana's own settings, keymap and theme files.
        "vscode-json-language-server" => match crate::schemas::json_schemas() {
            Value::Null => (json!({ "provideFormatter": true }), Value::Null),
            schemas => (json!({ "provideFormatter": true }), json!({ "json": { "schemas": schemas } })),
        },
        "vscode-css-language-server" | "vscode-html-language-server" => (json!({ "provideFormatter": true }), Value::Null),
        // What VS Code's ESLint extension sends, which the server expects
        // every key of (`eslint_configuration` adds the project's).
        "vscode-eslint-language-server" => (
            Value::Null,
            json!({
                "validate": "on",
                "run": "onType",
                "packageManager": "npm",
                "useESLintClass": false,
                "codeActionOnSave": { "mode": "all" },
                "format": false,
                "quiet": false,
                "onIgnoredFiles": "off",
                "options": {},
                "rulesCustomizations": [],
                "problems": { "shortenToSingleLine": false },
                "nodePath": null,
                "workingDirectory": { "mode": "location" },
                "codeAction": {
                    "disableRuleComment": { "enable": true, "location": "separateLine", "commentStyle": "line" },
                    "showDocumentation": { "enable": true },
                },
            }),
        ),
        // Copilot wants to know the editor it serves.
        "copilot-language-server" => {
            let version = env!("CARGO_PKG_VERSION");
            let options = json!({
                "editorInfo": { "name": "Kuana", "version": version },
                "editorPluginInfo": { "name": "Kuana Copilot", "version": version },
            });
            (options, Value::Null)
        }
        _ => (Value::Null, Value::Null),
    }
}

/// `over` merged into `base`: objects key by key, anything else replaces
/// (`null` leaves `base` as it is).
pub fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (_, Value::Null) => {}
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                merge(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, over) => *base = over.clone(),
    }
}

/// The answer to a `workspace/configuration` item: the whole settings, or
/// the `section` ("python.analysis"), a path into them.
pub fn configuration(settings: &Value, section: Option<&str>) -> Value {
    let Some(section) = section.filter(|s| !s.is_empty()) else { return settings.clone() };
    section.split('.').try_fold(settings, |value, key| value.get(key)).cloned().unwrap_or(Value::Null)
}

/// A server command: the executable and its arguments (and environment, as
/// an extension's code may set it).
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Finds an installed server: the project's `node_modules/.bin` or Python
/// venv first (a pinned version), then the search path.
pub fn find(spec: &ServerSpec, file: &Path, venv: Option<&Path>) -> Option<Command> {
    let command = |program: PathBuf, args: &[&str]| Command { program, args: args.iter().map(|a| a.to_string()).collect(), env: Vec::new() };
    // A remote project's servers are found there: its PATH, its node_modules and venv.
    let remote = crate::fs::is_remote(file);
    for (name, args) in spec.candidates.iter().filter(|(name, _)| remote || usable(name)) {
        let local = file
            .ancestors()
            .skip(1)
            .find_map(|d| executable(&d.join("node_modules").join(".bin").join(name)))
            .or_else(|| executable(&crate::python::bin_dir(venv?).join(name)));
        let on_path = || match file.parent().and_then(|dir| crate::exec::which_on(dir, name)) {
            Some(there) => there,
            None => which(name).filter(|p| !broken_rustup_proxy(p)),
        };
        if let Some(program) = local.or_else(on_path) {
            return Some(command(program, args));
        }
    }
    // Kuana's own copies are this computer's.
    if remote {
        return None;
    }
    // Only then Kuana's own copies: the user's servers, any of them, come first.
    spec.candidates.iter().filter(|(name, _)| usable(name)).find_map(|(name, args)| Some(command(super::install::installed(name)?, args)))
}

/// rustup's `rust-analyzer` in `~/.cargo/bin` is there without the component
/// installed; it then only says so. Such a proxy doesn't count as a server.
fn broken_rustup_proxy(program: &Path) -> bool {
    program.file_stem().is_some_and(|n| n == "rust-analyzer")
        && program.parent().is_some_and(|dir| dir.ends_with(".cargo/bin"))
        && !crate::process::command(program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// A user-configured command (`language_servers` in settings).
pub fn custom(command: &[String]) -> Option<Command> {
    let (name, args) = command.split_first()?;
    let path = Path::new(name);
    let program = if path.components().count() > 1 { executable(path)? } else { which(name)? };
    Some(Command { program, args: args.to_vec(), env: Vec::new() })
}

pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(search_path()).find_map(|dir| executable(&dir.join(name)))
}

pub(crate) fn executable(path: &Path) -> Option<PathBuf> {
    // On a remote host: its file, with its execute bits.
    if crate::fs::is_remote(path) {
        let meta = crate::fs::metadata(path).ok()?;
        return (meta.is_file() && meta.executable()).then(|| path.to_path_buf());
    }
    #[cfg(windows)]
    {
        for ext in ["exe", "cmd", "bat"] {
            let p = path.with_extension(ext);
            if p.is_file_on_host() {
                return Some(p);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        // This computer's PATH (a remote host's programs are found there).
        let meta = std::fs::metadata(path).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| path.to_path_buf())
    }
}

/// PATH for finding and running servers. Apps started from the Dock or
/// Finder get a minimal PATH, so this adds the login shell's PATH and the
/// usual install folders (cargo, go, Homebrew, npm via nvm…).
pub fn search_path() -> &'static OsString {
    static PATH: LazyLock<OsString> = LazyLock::new(|| {
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        let mut add = |dir: PathBuf| {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        };
        if let Some(shell_path) = login_shell_path() {
            std::env::split_paths(&shell_path).for_each(&mut add);
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for sub in [".cargo/bin", "go/bin", ".local/bin", ".bun/bin", ".deno/bin", ".volta/bin"] {
                add(home.join(sub));
            }
            // nvm keeps node (and global npm packages) per version; use the newest.
            if let Ok(versions) = crate::fs::read_dir(home.join(".nvm/versions/node")) {
                let mut found: Vec<PathBuf> = versions.filter_map(|e| e.ok()).map(|e| e.path().join("bin")).collect();
                found.sort();
                if let Some(newest) = found.pop() {
                    add(newest);
                }
            }
        }
        for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
            add(PathBuf::from(dir));
        }
        std::env::join_paths(dirs).unwrap_or_default()
    });
    &PATH
}

/// `$PATH` as the user's login shell sets it, with a timeout so a slow or
/// interactive shell config can't hang Kuana.
#[cfg(unix)]
fn login_shell_path() -> Option<OsString> {
    use std::{io::Read, os::unix::process::CommandExt, process::Stdio, time::Duration};
    const MARK: &str = "__KUANA_PATH__";
    let shell = std::env::var("SHELL").ok()?;
    let mut command = crate::process::command(shell);
    command
        .args(["-l", "-i", "-c", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // An interactive shell opens /dev/tty and makes itself the terminal's foreground job,
    // which stops (or breaks) the shell Kuana was started from. Its own session has no terminal.
    // SAFETY: setsid is async-signal-safe, as pre_exec requires.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    for _ in 0..60 {
        if child.try_wait().ok()?.is_some() {
            let mut out = String::new();
            child.stdout.take()?.read_to_string(&mut out).ok()?;
            let start = out.find(MARK)? + MARK.len();
            let end = start + out[start..].find(MARK)?;
            return Some(out[start..end].into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    None
}

#[cfg(not(unix))]
fn login_shell_path() -> Option<OsString> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_but_plain_text_has_a_server() {
        for language in [Language::Rust, Language::Tsx, Language::Markdown, Language::Toml] {
            assert!(spec(language).is_some());
        }
        assert!(spec(Language::PlainText).is_none());
        assert_eq!(spec(Language::JavaScript).unwrap().id, spec(Language::Tsx).unwrap().id);
        assert_eq!(language_id(Language::JavaScript, Path::new("a.jsx")), "javascriptreact");
    }

    #[test]
    fn extension_servers_run_beside_the_built_in_one() {
        static ESLINT: ServerSpec = ServerSpec { id: "eslint", candidates: &[("vscode-eslint-language-server", &["--stdio"])], ..SPEC };
        static VTSLS: ServerSpec = ServerSpec { id: "ext:vtsls", candidates: &[("vtsls", &["--stdio"])], ..SPEC };
        let ids = |specs: Vec<&ServerSpec>| specs.iter().map(|s| s.id).collect::<Vec<_>>();
        // ESLint joins TypeScript; an extension's vtsls is one of Kuana's own
        // choices for TypeScript already, so it isn't started a second time.
        assert_eq!(ids(combine(builtin(Language::TypeScript).into_iter().collect(), vec![&ESLINT, &VTSLS])), ["typescript", "eslint"]);
        // A language Kuana has no server for gets the extensions' alone.
        assert_eq!(ids(combine(Vec::new(), vec![&VTSLS, &ESLINT])), ["ext:vtsls", "eslint"]);
    }

    #[test]
    fn servers_as_in_zed() {
        // Ruff beside basedpyright for Python, nothing beside the others.
        let python: Vec<&str> = builtin(Language::Python).into_iter().chain(beside(Language::Python)).map(|s| s.id).collect();
        assert_eq!(python, ["python", "ruff"]);
        assert!(beside(Language::Go).is_none());
        assert!(std::ptr::eq(beside(Language::Tsx).unwrap(), &ESLINT));
        // phpactor before intelephense, except on Windows; only phpactor needs PHP.
        let php = builtin(Language::Php).unwrap();
        let first = if cfg!(windows) { "intelephense" } else { "phpactor" };
        assert_eq!(php.candidates[0].0, first);
        assert!(usable("intelephense") && usable("ruff"));
        if cfg!(windows) {
            return;
        }
        // Ruff is found in the project's venv, the way the project pins it.
        let dir = std::env::temp_dir().join(format!("kuana-ruff-{}", std::process::id()));
        let venv = dir.join(".venv");
        std::fs::create_dir_all(venv.join("bin")).unwrap();
        std::fs::write(venv.join("bin/ruff"), "#!/bin/sh\n").unwrap();
        crate::lsp::install::make_executable(&venv.join("bin/ruff")).unwrap();
        let found = find(beside(Language::Python).unwrap(), &dir.join("a.py"), Some(&venv)).unwrap();
        assert_eq!((found.program, found.args), (venv.join("bin/ruff"), vec!["server".to_string()]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eslint_runs_where_the_project_has_it_with_the_settings_it_expects() {
        use crate::settings::LspSettings;
        let dir = std::env::temp_dir().join(format!("kuana-eslint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        // No ESLint in the project: no server for it (TypeScript's still).
        assert!(!wanted(&ESLINT, &app.join("src/a.ts")));
        assert!(wanted(builtin(Language::TypeScript).unwrap(), &app.join("src/a.ts")));
        let bin = app.join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(if cfg!(windows) { "eslint.cmd" } else { "eslint" }), "").unwrap();
        assert!(wanted(&ESLINT, &app.join("src/a.ts")));

        // VS Code's settings, the project's folder, and the config's kind found.
        let none = HashMap::new();
        let base = config(&ESLINT, Path::new("/bin/vscode-eslint-language-server"), &none, &FromCode::default()).settings;
        let answer = eslint_configuration(&base, Some(&app.join("src/a.ts")), &app);
        assert_eq!((answer["validate"].as_str(), answer["run"].as_str()), (Some("on"), Some("onType")));
        assert_eq!(answer["workspaceFolder"]["name"], "app");
        assert_eq!(answer["workspaceFolder"]["uri"], crate::lsp::protocol::path_to_uri(&app));
        assert_eq!((answer["useFlatConfig"].as_bool(), answer["experimental"]["useFlatConfig"].as_bool()), (Some(false), Some(false)));
        std::fs::write(app.join("eslint.config.mjs"), "export default [];\n").unwrap();
        let answer = eslint_configuration(&base, Some(&app.join("src/a.ts")), &app);
        assert_eq!(answer["useFlatConfig"], true);
        // The user's `lsp.eslint.settings` win, flat config too.
        let user: HashMap<String, LspSettings> =
            serde_json::from_value(json!({ "eslint": { "settings": { "run": "onSave", "experimental.useFlatConfig": false } } })).unwrap();
        let mine = config(&ESLINT, Path::new("/bin/vscode-eslint-language-server"), &user, &FromCode::default()).settings;
        let answer = eslint_configuration(&mine, Some(&app.join("src/a.ts")), &app);
        assert_eq!((answer["run"].as_str(), answer["experimental"]["useFlatConfig"].as_bool()), (Some("onSave"), Some(false)));
        assert!(answer.get("useFlatConfig").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configuration_merges_defaults_extension_and_user_settings() {
        use crate::settings::{LspBinary, LspSettings};
        let typescript = builtin(Language::TypeScript).unwrap();
        // Kuana's defaults turn on inlay hints, by program.
        let none = HashMap::new();
        let ts = config(typescript, Path::new("/bin/typescript-language-server"), &none, &FromCode::default());
        assert_eq!(ts.initialization_options["preferences"]["includeInlayParameterNameHints"], "all");
        let vtsls = config(typescript, Path::new("/bin/vtsls"), &none, &FromCode::default());
        assert_eq!(vtsls.settings["typescript"]["inlayHints"]["parameterNames"]["enabled"], "all");
        // VS Code's JSON, CSS and HTML servers format only when told they may.
        for (language, program) in
            [(Language::Json, "vscode-json-language-server"), (Language::Css, "vscode-css-language-server"), (Language::Html, "vscode-html-language-server")]
        {
            let config = config(builtin(language).unwrap(), &Path::new("/bin").join(program), &none, &FromCode::default());
            assert_eq!(config.initialization_options["provideFormatter"], true, "{program}");
        }

        // The user's entry by Kuana's id or Zed's, merged over the defaults.
        let user: HashMap<String, LspSettings> = [(
            "vtsls".to_string(),
            LspSettings { settings: Some(json!({ "typescript": { "inlayHints": { "parameterNames": { "enabled": "none" } } } })), ..Default::default() },
        )]
        .into();
        let merged = config(typescript, Path::new("/bin/vtsls"), &user, &FromCode::default());
        assert_eq!(merged.settings["typescript"]["inlayHints"]["parameterNames"]["enabled"], "none");
        assert_eq!(merged.settings["typescript"]["inlayHints"]["variableTypes"]["enabled"], true);
        let python = builtin(Language::Python).unwrap();
        let user: HashMap<String, LspSettings> =
            [("basedpyright".to_string(), LspSettings { initialization_options: Some(json!({ "a": 1 })), ..Default::default() })].into();
        assert_eq!(config(python, Path::new("/bin/basedpyright-langserver"), &user, &FromCode::default()).initialization_options, json!({ "a": 1 }));
        // basedpyright checks in pyright's "standard" mode unless the user picks another.
        let mode = |user: &HashMap<String, LspSettings>| {
            config(python, Path::new("/bin/basedpyright-langserver"), user, &FromCode::default()).settings["basedpyright"]["analysis"]
                ["typeCheckingMode"]
                .clone()
        };
        assert_eq!(mode(&none), "standard");
        let strict: HashMap<String, LspSettings> = [(
            "basedpyright".to_string(),
            LspSettings { settings: Some(json!({ "basedpyright.analysis": { "typeCheckingMode": "recommended" } })), ..Default::default() },
        )]
        .into();
        assert_eq!(mode(&strict), "recommended");
        // Ruff organizes imports, so basedpyright doesn't offer it too.
        let based = config(python, Path::new("/bin/basedpyright-langserver"), &none, &FromCode::default());
        assert_eq!(based.settings["basedpyright"]["disableOrganizeImports"], true);

        // An extension's own values sit between the two.
        static EXT: ServerSpec = ServerSpec { id: "ext:x", ..SPEC };
        let ext_settings: &'static Value = Box::leak(Box::new(json!({ "x": { "a": 1, "b": 2 } })));
        let ext = ServerSpec { settings: Some(ext_settings), ..SPEC };
        let user: HashMap<String, LspSettings> =
            [("ext:x".to_string(), LspSettings { settings: Some(json!({ "x": { "b": 3 } })), ..Default::default() })].into();
        assert_eq!(config(&ServerSpec { id: EXT.id, ..ext }, Path::new("/bin/x"), &user, &FromCode::default()).settings, json!({ "x": { "a": 1, "b": 3 } }));

        // What an extension's code said comes before the user's entry, which
        // goes by Zed's id for the server it started (the program is node);
        // the user's settings reach that server through the code.
        let code = FromCode { server: Some("html".into()), initialization_options: json!({ "a": 1, "b": 1 }), settings: json!({ "x.y": 1 }), asking: None };
        let user: HashMap<String, LspSettings> = [(
            "html".to_string(),
            LspSettings { initialization_options: Some(json!({ "b": 2 })), settings: Some(json!({ "z": 1 })), ..Default::default() },
        )]
        .into();
        let from_code = config(&EXT, Path::new("/bin/node"), &user, &code);
        assert_eq!((from_code.initialization_options, from_code.settings), (json!({ "a": 1, "b": 2 }), json!({ "x.y": 1 })));

        // Another program, by any of the server's names.
        if cfg!(windows) {
            return;
        }
        let user: HashMap<String, LspSettings> = [(
            "pyright".to_string(),
            LspSettings { binary: Some(LspBinary { path: Some("/bin/sh".into()), arguments: Some(vec!["-c".into()]) }), ..Default::default() },
        )]
        .into();
        let binary = user_binary(python, &user).unwrap();
        assert_eq!((binary.program, binary.args), (PathBuf::from("/bin/sh"), vec!["-c".to_string()]));
    }

    #[test]
    fn configuration_sections() {
        // basedpyright asks for "basedpyright"; settings say "basedpyright.analysis".
        let settings = expand_dotted(&json!({
            "basedpyright.analysis": { "typeCheckingMode": "off" },
            "basedpyright": { "disableLanguageServices": false },
            "gopls": { "hints": { "a": true } },
        }));
        assert_eq!(
            configuration(&settings, Some("basedpyright")),
            json!({ "analysis": { "typeCheckingMode": "off" }, "disableLanguageServices": false })
        );
        assert_eq!(configuration(&settings, Some("basedpyright.analysis")), json!({ "typeCheckingMode": "off" }));
        assert_eq!(configuration(&settings, Some("gopls.hints")), json!({ "a": true }));
        assert_eq!(configuration(&settings, Some("missing")), Value::Null);
        assert_eq!(configuration(&settings, None), settings);
    }

    #[test]
    fn roots() {
        let rust = spec(Language::Rust).unwrap();
        assert_eq!(root_for(rust, Path::new("/w/src/a.rs"), Some(Path::new("/w"))), PathBuf::from("/w"));
        let dir = std::env::temp_dir().join(format!("kuana-root-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crate/src")).unwrap();
        std::fs::write(dir.join("crate/Cargo.toml"), "").unwrap();
        assert_eq!(root_for(rust, &dir.join("crate/src/main.rs"), None), dir.join("crate"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
