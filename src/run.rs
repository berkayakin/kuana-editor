//! Run configurations: what can run in a file (`main`, tests) and in the
//! project (cargo, npm scripts, go, make, pytest), as shell commands that
//! the workspace runs in a terminal tab, and the problems their output
//! reports (for the Problems panel).

use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Tree};

use crate::{lint::Severity, syntax::Language, test_run::Framework};

/// Something runnable: a configuration in Run…, or a gutter ▶ on `row`.
/// The project's session keeps the last one run (without its place in a file).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunTarget {
    /// "Run 'main'", "Test 'parses_input'", "npm run dev".
    pub name: String,
    /// A shell command line.
    pub command: String,
    pub cwd: PathBuf,
    /// The line it belongs to, for gutter targets.
    #[serde(skip)]
    pub row: Option<usize>,
    /// Lines it spans (the function or block), for Run at caret.
    #[serde(skip)]
    pub rows: std::ops::Range<usize>,
    /// How to start it under a debugger instead, when that's possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug: Option<DebugSpec>,
    /// The test, or group of tests, it runs, as test results name it
    /// (`test_run::key`), for the gutter's pass / fail mark.
    #[serde(skip)]
    pub test: Option<(Framework, String)>,
}

/// What a debug session starts (see `crate::debugger`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DebugSpec {
    /// Rust: cargo builds it (`build …` or `test --no-run …`), then lldb-dap
    /// runs the executable it made with `args`.
    Cargo { build: Vec<String>, args: Vec<String> },
    /// Go, through Delve: mode "debug" (a main package) or "test", the
    /// package folder (or a file), and the program's arguments.
    Go { mode: String, program: PathBuf, args: Vec<String> },
    /// Python, through debugpy: a script, or a module (pytest) with arguments.
    Python { program: Option<PathBuf>, module: Option<String>, args: Vec<String> },
    /// JavaScript and TypeScript, through js-debug: a command (`node`, `npm`,
    /// `npx`…) whose Node processes are all debugged.
    Node { runtime: String, args: Vec<String> },
    /// A running process; its executable decides the adapter.
    Attach { pid: u32, program: PathBuf },
    /// A configuration as written (`.vscode/launch.json`, `.zed/debug.json`):
    /// the adapter by name, `launch` or `attach`, and its arguments.
    Config { adapter: String, request: String, config: serde_json::Value },
}

impl DebugSpec {
    /// Attaching to a running program (which ending the session leaves running).
    pub fn is_attach(&self) -> bool {
        match self {
            DebugSpec::Attach { .. } => true,
            DebugSpec::Config { request, .. } => request == "attach",
            _ => false,
        }
    }
}

/// What rust-analyzer's Run / Debug lens describes (its `runnable`: cargo's
/// arguments, the test binary's, and where to run), as a target to run or
/// debug the same way Kuana's own Rust targets are.
pub fn from_rust_analyzer(runnable: &serde_json::Value) -> Option<RunTarget> {
    let args = runnable.get("args")?;
    let strings = |key: &str| -> Vec<String> {
        args.get(key).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let (cargo, exe) = (strings("cargoArgs"), strings("executableArgs"));
    let subcommand = cargo.first()?.clone();
    let cwd = ["cwd", "workspaceRoot"].iter().find_map(|k| args.get(*k).and_then(|v| v.as_str())).map(PathBuf::from)?;
    let quote = |list: &[String]| list.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    let mut command = format!("cargo {}", quote(&cargo));
    if !exe.is_empty() {
        command.push_str(&format!(" -- {}", quote(&exe)));
    }
    // lldb runs the executable cargo builds: `run` builds, `test` builds without running.
    let build: Option<Vec<String>> = match subcommand.as_str() {
        "run" => Some(std::iter::once("build".to_string()).chain(cargo[1..].iter().cloned()).collect()),
        "test" => Some(["test".to_string(), "--no-run".into()].into_iter().chain(cargo[1..].iter().cloned()).collect()),
        _ => None,
    };
    Some(RunTarget {
        name: runnable.get("label").and_then(|l| l.as_str()).unwrap_or("cargo").to_string(),
        command,
        cwd,
        row: None,
        rows: 0..0,
        test: (subcommand == "test").then(|| exe.first().filter(|a| !a.starts_with('-')).map(|a| (Framework::Cargo, a.clone()))).flatten(),
        debug: build.map(|build| DebugSpec::Cargo { build, args: exe }),
    })
}

/// Quotes for the shell Run tabs use, when needed: PowerShell on Windows,
/// else a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if cfg!(windows) { powershell_quote(s) } else { posix_quote(s) }
}

fn posix_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@+%,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// In PowerShell `,` and `@` make arrays and splats, and `\` is plain.
fn powershell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || r"-_./:=+%\".contains(c)) {
        s.to_string()
    } else {
        powershell_string(s)
    }
}

/// A PowerShell string literal: single-quoted, verbatim but for its quote
/// characters (curly ones count too), which are doubled.
pub fn powershell_string(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out + "'"
}

/// Python's command. On Windows `python3` is the Microsoft Store's stub
/// unless Python came from there, and a venv has no `python3.exe`.
const PYTHON: &str = if cfg!(windows) { "python" } else { "python3" };

fn nearest_with(file: &Path, marker: &str) -> Option<PathBuf> {
    file.ancestors().skip(1).find(|d| d.join(marker).is_file()).map(Path::to_path_buf)
}

fn node_text<'a>(node: Node, text: &'a str) -> &'a str {
    &text[node.byte_range()]
}

fn target(name: String, command: String, cwd: &Path, node: Node, header: Node, debug: Option<DebugSpec>) -> RunTarget {
    RunTarget {
        name,
        command,
        cwd: cwd.to_path_buf(),
        row: Some(header.start_position().row),
        rows: node.start_position().row..node.end_position().row + 1,
        debug,
        test: None,
    }
}

/// `target` names the test it runs.
fn testing(mut target: RunTarget, framework: Framework, key: String) -> RunTarget {
    target.test = Some((framework, key));
    target
}

/// `[package] name` from a Cargo.toml (the default binary's name).
fn cargo_package_name(crate_dir: &Path) -> Option<String> {
    let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml")).ok()?;
    let mut in_package = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package
            && let Some(value) = line.strip_prefix("name").map(str::trim_start).and_then(|l| l.strip_prefix('='))
        {
            return Some(value.trim().trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

/// Runnable things in a file, for the gutter and Run at caret.
pub fn file_targets(language: Language, path: &Path, text: &str, tree: &Tree) -> Vec<RunTarget> {
    let root = tree.root_node();
    let mut out = Vec::new();
    match language {
        Language::Rust => rust_targets(path, text, root, &mut out),
        Language::Go => go_targets(path, text, root, &mut out),
        Language::Python => python_targets(path, text, root, &mut out),
        Language::JavaScript | Language::TypeScript | Language::Tsx => js_targets(path, text, root, &mut out),
        Language::Extension(_) => extension_targets(language, path, text, tree, &mut out),
        _ => {}
    }
    out.sort_by_key(|t| t.row);
    out
}

// ---- Extensions --------------------------------------------------------------------

/// Zed's runnables: `runnables.scm` captures `@run` with `(#set! tag …)`, and
/// the language's `tasks.json` says what runs for each tag, with `$ZED_…`
/// variables (`$ZED_FILE`, `$ZED_ROW`, `$ZED_SYMBOL`, `$ZED_CUSTOM_<capture>` …).
fn extension_targets(language: Language, path: &Path, text: &str, tree: &Tree, out: &mut Vec<RunTarget>) {
    use tree_sitter::{QueryCursor, StreamingIterator};
    let Some(def) = language.extension() else { return };
    let Some(query) = crate::syntax::queries(language).and_then(|q| q.runnables.as_ref()) else { return };
    let Some(run) = query.capture_index_for_name("run") else { return };
    let dir = path.parent().unwrap_or(path);
    let root = path.ancestors().find(|d| d.join(".git").exists()).unwrap_or(dir);
    let display = |p: &Path| p.to_string_lossy().into_owned();
    let base = [
        ("ZED_FILE", display(path)),
        ("ZED_FILENAME", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        ("ZED_STEM", path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        ("ZED_DIRNAME", display(dir)),
        ("ZED_WORKTREE_ROOT", display(root)),
        ("ZED_RELATIVE_FILE", display(path.strip_prefix(root).unwrap_or(path))),
    ];
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        let Some(node) = m.captures.iter().find(|c| c.index == run).map(|c| c.node) else { continue };
        let tags: Vec<&str> = query.property_settings(m.pattern_index).iter().filter(|p| p.key.as_ref() == "tag").filter_map(|p| p.value.as_deref()).collect();
        let mut vars: Vec<(String, String)> = base.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        vars.push(("ZED_ROW".into(), (node.start_position().row + 1).to_string()));
        vars.push(("ZED_COLUMN".into(), (node.start_position().column + 1).to_string()));
        vars.push(("ZED_SYMBOL".into(), text[node.byte_range()].to_string()));
        for c in m.captures {
            let name = query.capture_names()[c.index as usize];
            if c.index != run && !name.starts_with('_') {
                vars.push((format!("ZED_CUSTOM_{name}"), text[c.node.byte_range()].to_string()));
            }
        }
        let plain = |s: &str| expand_vars(s, &vars, str::to_string);
        for task in def.tasks.iter().filter(|t| t.tags.iter().any(|tag| tags.contains(&tag.as_str()))) {
            // The command is shell text, so values put into it are quoted (a
            // path with spaces, or a symbol that looks like `$(…)`); each
            // argument stays one word.
            let command = std::iter::once(expand_vars(&task.command, &vars, shell_quote))
                .chain(task.args.iter().map(|a| shell_quote(&plain(a))))
                .collect::<Vec<_>>()
                .join(" ");
            let test = oxie_test(language, path, task, &vars, &command).or_else(|| lyth_test(language, task, &vars, &command));
            out.push(RunTarget {
                name: plain(&task.label),
                command,
                cwd: task.cwd.as_deref().map(|c| PathBuf::from(plain(c))).unwrap_or_else(|| root.to_path_buf()),
                row: Some(node.start_position().row),
                rows: node.start_position().row..node.end_position().row + 1,
                debug: None,
                test,
            });
        }
    }
}

/// `$NAME` and `${NAME}` replaced by their values passed through `quote`, in
/// one pass, so a value is never expanded again. Names that aren't variables
/// stay as written (the shell's own, like `$HOME`).
fn expand_vars(s: &str, vars: &[(String, String)], quote: impl Fn(&str) -> String) -> String {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, len) = match after.strip_prefix('{') {
            Some(braced) => match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            },
            None => {
                let end = after.find(|c| !is_name(c)).unwrap_or(after.len());
                (&after[..end], end)
            }
        };
        match vars.iter().find(|(k, _)| !name.is_empty() && k == name) {
            Some((_, value)) => out.push_str(&quote(value)),
            None => out.push_str(&rest[i..i + 1 + len]),
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

// ---- Rust ------------------------------------------------------------------------

/// The module path of a source file inside its crate: `src/a/b.rs` → `a::b`.
fn rust_module_path(path: &Path, crate_dir: &Path) -> Vec<String> {
    let Ok(rel) = path.strip_prefix(crate_dir.join("src")) else { return Vec::new() };
    let mut parts: Vec<String> = rel.with_extension("").iter().map(|p| p.to_string_lossy().into_owned()).collect();
    if parts.last().is_some_and(|p| p == "mod" || p == "lib" || p == "main") {
        parts.pop();
    }
    // Binaries in src/bin are their own crates.
    if parts.first().is_some_and(|p| p == "bin") {
        return parts.into_iter().skip(2).collect();
    }
    parts
}

fn rust_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let Some(crate_dir) = nearest_with(path, "Cargo.toml") else { return };
    let rel = path.strip_prefix(&crate_dir).unwrap_or(path);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    // `cargo test` needs to know which target an integration test or binary is.
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let target_flag = if rel_str.starts_with("tests/") {
        format!(" --test {}", shell_quote(&stem))
    } else if rel_str.starts_with("src/bin/") {
        format!(" --bin {}", shell_quote(&stem))
    } else if rel_str.starts_with("examples/") {
        format!(" --example {}", shell_quote(&stem))
    } else {
        String::new()
    };
    let module = rust_module_path(path, &crate_dir);
    // Which compile target a debug build picks: one executable, so lldb has
    // exactly one program to run.
    let package = cargo_package_name(&crate_dir);
    let cargo_target: Vec<String> = if rel_str.starts_with("tests/") {
        vec!["--test".into(), stem.clone()]
    } else if rel_str.starts_with("src/bin/") {
        vec!["--bin".into(), stem.clone()]
    } else if rel_str.starts_with("examples/") {
        vec!["--example".into(), stem.clone()]
    } else if rel_str != "src/main.rs" && crate_dir.join("src/lib.rs").is_file() {
        vec!["--lib".into()]
    } else {
        package.iter().flat_map(|p| ["--bin".to_string(), p.clone()]).collect()
    };
    let debug_test = |filter: &str, exact: bool| {
        let mut build = vec!["test".to_string(), "--no-run".into()];
        build.extend(cargo_target.clone());
        let mut args = vec![filter.to_string()];
        if exact {
            args.push("--exact".into());
        }
        args.push("--nocapture".into());
        Some(DebugSpec::Cargo { build, args })
    };

    fn has_test_attr(node: Node, text: &str) -> bool {
        let mut prev = node.prev_named_sibling();
        while let Some(p) = prev.filter(|p| p.kind() == "attribute_item" || p.kind() == "line_comment") {
            let attr = node_text(p, text);
            if attr.starts_with("#[test") || attr.contains("::test") || attr.starts_with("#[rstest") {
                return true;
            }
            prev = p.prev_named_sibling();
        }
        false
    }

    fn walk(node: Node, text: &str, mods: &mut Vec<String>, f: &mut dyn FnMut(Node, &[String])) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            f(child, mods);
            if child.kind() == "mod_item"
                && let (Some(name), Some(body)) = (child.child_by_field_name("name"), child.child_by_field_name("body"))
            {
                mods.push(node_text(name, text).to_string());
                walk(body, text, mods, f);
                mods.pop();
            }
        }
    }

    let is_main_file = rel_str == "src/main.rs" || rel_str.starts_with("src/bin/") || rel_str.starts_with("examples/");
    walk(root, text, &mut Vec::new(), &mut |node, mods| {
        let Some(name) = node.child_by_field_name("name") else { return };
        let ident = node_text(name, text);
        let full: Vec<&str> = module.iter().chain(mods.iter()).map(String::as_str).chain([ident]).collect();
        match node.kind() {
            "function_item" if has_test_attr(node, text) => {
                let filter = full.join("::");
                let test = target(
                    format!("Test '{ident}'"),
                    format!("cargo test{target_flag} -- {} --exact", shell_quote(&filter)),
                    &crate_dir,
                    node,
                    name,
                    debug_test(&filter, true),
                );
                out.push(testing(test, Framework::Cargo, filter));
            }
            "function_item" if ident == "main" && mods.is_empty() && is_main_file => {
                let run = if target_flag.contains("--test") { String::new() } else { target_flag.clone() };
                let label = if stem == "main" { "main" } else { stem.as_str() };
                let mut build = vec!["build".to_string()];
                build.extend(cargo_target.clone());
                let debug = Some(DebugSpec::Cargo { build, args: Vec::new() });
                out.push(target(format!("Run '{label}'"), format!("cargo run{run}"), &crate_dir, node, name, debug));
            }
            "mod_item"
                if node.child_by_field_name("body").is_some()
                    && (node_text(node, text).contains("#[test") || has_cfg_test(node, text)) =>
            {
                let filter = full.join("::");
                let command = format!("cargo test{target_flag} -- {}", shell_quote(&filter));
                let tests = target(format!("Tests in '{ident}'"), command, &crate_dir, node, name, debug_test(&filter, false));
                out.push(testing(tests, Framework::Cargo, filter));
            }
            _ => {}
        }
    });
}

fn has_cfg_test(node: Node, text: &str) -> bool {
    node.prev_named_sibling().is_some_and(|p| p.kind() == "attribute_item" && node_text(p, text).contains("cfg(test)"))
}

// ---- Go --------------------------------------------------------------------------

fn go_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let dir = path.parent().unwrap_or(path);
    let is_test_file = path.to_string_lossy().ends_with("_test.go");
    let is_main_package = text.lines().any(|l| l.trim() == "package main");
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor).filter(|n| n.kind() == "function_declaration") {
        let Some(name) = node.child_by_field_name("name") else { continue };
        let ident = node_text(name, text);
        if is_test_file && (ident.starts_with("Test") || ident.starts_with("Benchmark") || ident.starts_with("Example")) {
            let bench = ident.starts_with("Benchmark");
            let flag = if bench { "-bench" } else { "-run" };
            let pattern = format!("^{ident}$");
            let args = if bench {
                vec!["-test.run".into(), "^$".into(), "-test.bench".into(), pattern.clone()]
            } else {
                vec!["-test.run".into(), pattern.clone()]
            };
            // -v lists every test's result, which the test results read.
            let test = target(
                format!("Test '{ident}'"),
                format!("go test -v {flag} {} .", shell_quote(&pattern)),
                dir,
                node,
                name,
                Some(DebugSpec::Go { mode: "test".into(), program: dir.to_path_buf(), args }),
            );
            out.push(testing(test, Framework::Go, ident.to_string()));
        } else if ident == "main" && is_main_package {
            let debug = Some(DebugSpec::Go { mode: "debug".into(), program: dir.to_path_buf(), args: Vec::new() });
            out.push(target("Run 'main'".into(), "go run .".into(), dir, node, name, debug));
        }
    }
}

// ---- Python ----------------------------------------------------------------------

fn python_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let dir = path.parent().unwrap_or(path);
    let file = shell_quote(&path.to_string_lossy());
    // Test results name pytest's tests by file name and node path.
    let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let key = |parts: &[&str]| std::iter::once(base.as_str()).chain(parts.iter().copied()).collect::<Vec<_>>().join("::");
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        let node = if node.kind() == "decorated_definition" { node.child_by_field_name("definition").unwrap_or(node) } else { node };
        match node.kind() {
            "function_definition" => {
                let Some(name) = node.child_by_field_name("name") else { continue };
                let ident = node_text(name, text);
                if ident.starts_with("test") {
                    let debug = pytest(path, &[ident]);
                    let test = target(format!("Test '{ident}'"), format!("{PYTHON} -m pytest -v {file}::{ident}"), dir, node, name, debug);
                    out.push(testing(test, Framework::Pytest, key(&[ident])));
                }
            }
            "class_definition" => {
                let Some(name) = node.child_by_field_name("name") else { continue };
                let class = node_text(name, text);
                if !class.starts_with("Test") {
                    continue;
                }
                let debug = pytest(path, &[class]);
                let tests = target(format!("Tests in '{class}'"), format!("{PYTHON} -m pytest -v {file}::{class}"), dir, node, name, debug);
                out.push(testing(tests, Framework::Pytest, key(&[class])));
                let Some(body) = node.child_by_field_name("body") else { continue };
                let mut c = body.walk();
                for method in body.named_children(&mut c) {
                    let method = if method.kind() == "decorated_definition" {
                        method.child_by_field_name("definition").unwrap_or(method)
                    } else {
                        method
                    };
                    if method.kind() != "function_definition" {
                        continue;
                    }
                    let Some(mname) = method.child_by_field_name("name") else { continue };
                    let m = node_text(mname, text);
                    if m.starts_with("test") {
                        let debug = pytest(path, &[class, m]);
                        let test = target(format!("Test '{m}'"), format!("{PYTHON} -m pytest -v {file}::{class}::{m}"), dir, method, mname, debug);
                        out.push(testing(test, Framework::Pytest, key(&[class, m])));
                    }
                }
            }
            "if_statement" if node_text(node, text).starts_with("if __name__") => {
                let debug = Some(DebugSpec::Python { program: Some(path.to_path_buf()), module: None, args: Vec::new() });
                out.push(target("Run file".into(), format!("{PYTHON} {file}"), dir, node, node, debug));
            }
            _ => {}
        }
    }
}

/// Debugging a pytest node (`file::Class::test`).
fn pytest(path: &Path, parts: &[&str]) -> Option<DebugSpec> {
    let mut id = path.to_string_lossy().into_owned();
    for part in parts {
        id.push_str("::");
        id.push_str(part);
    }
    Some(DebugSpec::Python { program: None, module: Some("pytest".into()), args: vec![id] })
}

// ---- JavaScript / TypeScript -------------------------------------------------------

fn node_debug(runtime: &str, args: &[&str]) -> Option<DebugSpec> {
    Some(DebugSpec::Node { runtime: runtime.into(), args: args.iter().map(|a| a.to_string()).collect() })
}

/// The test runner a package uses.
fn js_test_runner(package_dir: &Path) -> &'static str {
    let package = std::fs::read_to_string(package_dir.join("package.json")).unwrap_or_default();
    if package.contains("\"vitest\"") {
        "vitest"
    } else if package.contains("\"jest\"") {
        "jest"
    } else {
        "node"
    }
}

fn js_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let package_dir = nearest_with(path, "package.json").unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf());
    let runner = js_test_runner(&package_dir);
    let file = shell_quote(&path.to_string_lossy());
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
        if node.kind() != "call_expression" {
            continue;
        }
        let Some(function) = node.child_by_field_name("function") else { continue };
        // `test`, `it`, `describe` and their `.only` / `.skip` forms.
        let callee = node_text(function, text);
        let base = callee.split('.').next().unwrap_or(callee);
        if !matches!(base, "test" | "it" | "describe") {
            continue;
        }
        let Some(args) = node.child_by_field_name("arguments") else { continue };
        let Some(first) = args.named_child(0).filter(|a| matches!(a.kind(), "string" | "template_string")) else { continue };
        let title = node_text(first, text).trim_matches(['"', '\'', '`']).to_string();
        let pattern = regex::escape(&title);
        let command = match runner {
            // The verbose reporter lists every test's result.
            "vitest" => format!("npx vitest run --reporter=verbose {file} -t {}", shell_quote(&pattern)),
            "jest" => format!("npx jest {file} -t {}", shell_quote(&pattern)),
            _ => format!("node --test --test-name-pattern={} {file}", shell_quote(&pattern)),
        };
        let path = path.to_string_lossy().into_owned();
        let debug = match runner {
            "vitest" => node_debug("npx", &["vitest", "run", &path, "-t", &pattern]),
            // In one process, so breakpoints don't depend on attaching to workers.
            "jest" => node_debug("npx", &["jest", &path, "-t", &pattern, "--runInBand"]),
            _ => node_debug("node", &["--test", &format!("--test-name-pattern={pattern}"), &path]),
        };
        let found = target(String::new(), command, &package_dir, node, node, debug);
        // Results name a test by its title only (a suite's aren't listed).
        out.push(match base {
            "describe" => RunTarget { name: format!("Tests in '{title}'"), ..found },
            _ => RunTarget { name: format!("Test '{title}'"), ..testing(found, Framework::Js, title.clone()) },
        });
    }
}

// ---- Project configurations --------------------------------------------------------

/// What the project can run: build tools found at its root, then the
/// current file.
pub fn project_targets(root: &Path, current: Option<(&Path, Language)>) -> Vec<RunTarget> {
    let mut out = Vec::new();
    let mut add = |name: String, command: String, cwd: &Path, debug: Option<DebugSpec>| {
        out.push(RunTarget { name, command, cwd: cwd.to_path_buf(), row: None, rows: 0..0, debug, test: None });
    };
    let cargo = |build: &[&str]| Some(DebugSpec::Cargo { build: build.iter().map(|s| s.to_string()).collect(), args: Vec::new() });
    // Oxie projects: Cargo crates whose sources are .oxi files
    if is_oxie_project(root) {
        add("oxie run".into(), "oxie run".into(), root, None);
        // Without `--quiet`'s format, so passed tests are named too (test results)
        add("oxie test".into(), "oxie test -- --format=pretty".into(), root, None);
        add("oxie build".into(), "oxie build".into(), root, None);
        add("oxie fmt".into(), "oxie fmt".into(), root, None);
    }
    if root.join("Cargo.toml").is_file() {
        // The default binary by name, so the debug build makes one executable.
        let run_debug = match cargo_package_name(root) {
            Some(bin) if root.join("src/main.rs").is_file() => cargo(&["build", "--bin", &bin]),
            _ => None,
        };
        add("cargo run".into(), "cargo run".into(), root, run_debug);
        add("cargo test".into(), "cargo test".into(), root, None);
        add("cargo build".into(), "cargo build".into(), root, None);
        if let Ok(bins) = std::fs::read_dir(root.join("src/bin")) {
            for bin in bins.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "rs")) {
                let name = bin.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let debug = cargo(&["build", "--bin", &name]);
                add(format!("cargo run --bin {name}"), format!("cargo run --bin {}", shell_quote(&name)), root, debug);
            }
        }
    }
    if let Ok(package) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&package)
        && let Some(scripts) = json.get("scripts").and_then(|s| s.as_object())
    {
        let manager = if root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
            "bun"
        } else {
            "npm"
        };
        for name in scripts.keys() {
            // Bun isn't Node: js-debug can't debug it.
            let debug = if manager == "bun" { None } else { node_debug(manager, &["run", name]) };
            add(format!("{manager} run {name}"), format!("{manager} run {}", shell_quote(name)), root, debug);
        }
    }
    // Lyth modules: Go modules whose packages are .lyth files, which lyth compiles
    if root.join("go.mod").is_file() && has_lyth_files(root, 3) {
        add("lyth run .".into(), "lyth run .".into(), root, None);
        // -v names every test, for the test results
        add("lyth test ./...".into(), "lyth test -v ./...".into(), root, None);
        add("lyth build ./...".into(), "lyth build ./...".into(), root, None);
        add("lyth vet ./...".into(), "lyth vet ./...".into(), root, None);
    }
    if root.join("go.mod").is_file() {
        let debug = Some(DebugSpec::Go { mode: "debug".into(), program: root.to_path_buf(), args: Vec::new() });
        add("go run .".into(), "go run .".into(), root, debug);
        add("go test ./...".into(), "go test -v ./...".into(), root, None);
        add("go build ./...".into(), "go build ./...".into(), root, None);
    }
    if let Ok(makefile) = std::fs::read_to_string(root.join("Makefile")) {
        for line in makefile.lines() {
            // `name:` rules, not variables (`A := b`) or special targets.
            let Some((target, rest)) = line.split_once(':') else { continue };
            if rest.starts_with('=') || target.starts_with(['.', '\t', ' ', '#']) || target.contains(['$', '%', '=']) {
                continue;
            }
            for t in target.split_whitespace() {
                add(format!("make {t}"), format!("make {}", shell_quote(t)), root, None);
            }
        }
    }
    if root.join("pyproject.toml").is_file() || root.join("pytest.ini").is_file() {
        let debug = Some(DebugSpec::Python { program: None, module: Some("pytest".into()), args: Vec::new() });
        add("pytest".into(), format!("{PYTHON} -m pytest -v"), root, debug);
    }
    if let Some((path, language)) = current {
        let file = shell_quote(&path.to_string_lossy());
        let dir = path.parent().unwrap_or(root);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (command, debug) = match language {
            Language::Python => {
                let debug = DebugSpec::Python { program: Some(path.to_path_buf()), module: None, args: Vec::new() };
                (Some(format!("{PYTHON} {file}")), Some(debug))
            }
            Language::JavaScript => (Some(format!("node {file}")), node_debug("node", &[&path.to_string_lossy()])),
            Language::TypeScript | Language::Tsx => (Some(format!("npx tsx {file}")), node_debug("npx", &["tsx", &path.to_string_lossy()])),
            Language::Shell => (Some(format!("bash {file}")), None),
            Language::Go => {
                let debug = DebugSpec::Go { mode: "debug".into(), program: path.to_path_buf(), args: Vec::new() };
                (Some(format!("go run {file}")), Some(debug))
            }
            Language::Php => (Some(format!("php {file}")), None),
            Language::Extension(_) if language.name() == "Oxie" => (Some(format!("oxie run {file}")), None),
            Language::Extension(_) if language.name() == "Lyth" => (Some(format!("lyth run {file}")), None),
            _ => (None, None),
        };
        if let Some(command) = command {
            add(format!("Run '{name}'"), command, dir, debug);
        }
    }
    out
}

/// A Cargo crate written in Oxie: it builds with `oxie-build`, or has `src/main.oxi` / `src/lib.oxi`.
fn is_oxie_project(root: &Path) -> bool {
    root.join("src/main.oxi").is_file()
        || root.join("src/lib.oxi").is_file()
        || std::fs::read_to_string(root.join("Cargo.toml")).is_ok_and(|manifest| manifest.contains("oxie-build"))
}

/// Whether `dir` or its folders `depth` levels down hold a .lyth file
/// (hidden folders, dependencies and build output aside).
fn has_lyth_files(dir: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut subdirs = Vec::new();
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        if path.extension().is_some_and(|e| e == "lyth") && path.is_file() {
            return true;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if depth > 0 && path.is_dir() && !name.starts_with('.') && !matches!(name.as_str(), "node_modules" | "target" | "vendor") {
            subdirs.push(path);
        }
    }
    subdirs.iter().any(|d| has_lyth_files(d, depth - 1))
}

/// The test a Lyth extension task runs, by the name `go test -v` reports: a
/// task given a test function's name (`lyth test -v -run '^TestAdd$' .`),
/// which `lyth test` runs with go test.
fn lyth_test(language: Language, task: &crate::extensions::Task, vars: &[(String, String)], command: &str) -> Option<(Framework, String)> {
    const NAME: &str = "ZED_CUSTOM_LYTH_TEST_NAME";
    if language.name() != "Lyth" || Framework::of(command) != Some(Framework::Go) {
        return None;
    }
    // A task that runs the whole package shares the row, but not the test.
    if !std::iter::once(&task.label).chain(&task.args).any(|a| a.contains(NAME)) {
        return None;
    }
    Some((Framework::Go, vars.iter().find(|(k, _)| k == NAME)?.1.clone()))
}

/// The test an Oxie extension task runs, by the id `oxie test` reports
/// (libtest's): a task given a `test "…"` block's name gets
/// `[module::]__oxie_tests::test_<slug>`, as the compiler names its Rust test.
fn oxie_test(language: Language, path: &Path, task: &crate::extensions::Task, vars: &[(String, String)], command: &str) -> Option<(Framework, String)> {
    const NAME: &str = "ZED_CUSTOM_OXIE_TEST_NAME";
    if language.name() != "Oxie" || Framework::of(command) != Some(Framework::Cargo) {
        return None;
    }
    // A task that runs the whole file shares the row, but not the test.
    if !std::iter::once(&task.label).chain(&task.args).any(|a| a.contains(NAME)) {
        return None;
    }
    let name = &vars.iter().find(|(k, _)| k == NAME)?.1;
    let slug = oxie_slug(name);
    let test = if slug.starts_with("test") { slug } else { format!("test_{slug}") };
    let id: Vec<String> = oxie_module(path).into_iter().chain(["__oxie_tests".to_string(), test]).collect();
    Some((Framework::Cargo, id.join("::")))
}

/// A test's name as an identifier, as the Oxie compiler makes it.
fn oxie_slug(name: &str) -> String {
    let mut slug: String = name.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    while slug.contains("__") {
        slug = slug.replace("__", "_");
    }
    let slug = slug.trim_matches('_');
    if slug.is_empty() || slug.starts_with(|c: char| c.is_ascii_digit()) { format!("test_{slug}") } else { slug.to_string() }
}

/// The module an .oxi file is in a project (`src/a/b.oxi` → `a::b`; `main`,
/// `lib` and `mod` files as in Rust); none for a script outside one.
fn oxie_module(path: &Path) -> Vec<String> {
    let Some(src) = path.ancestors().skip(1).find(|d| d.file_name().is_some_and(|n| n == "src") && d.parent().is_some_and(is_oxie_project)) else {
        return Vec::new();
    };
    let Ok(rel) = path.with_extension("").strip_prefix(src).map(Path::to_path_buf) else { return Vec::new() };
    let mut parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    match parts.last().map(String::as_str) {
        Some("main" | "lib") if parts.len() == 1 => Vec::new(),
        Some("mod") => {
            parts.pop();
            parts
        }
        _ => parts,
    }
}

// ---- Problems in output ------------------------------------------------------------

/// An error or warning that a compiler, test runner or crash printed.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputProblem {
    pub path: PathBuf,
    /// 1-based, like `column`.
    pub line: u32,
    pub column: Option<u32>,
    pub severity: Severity,
    pub message: String,
}

/// Enough to be useful when something prints thousands.
const MAX_PROBLEMS: usize = 500;

/// The problems in a Run tab's output: rustc and cargo (`error[E…]: …`, then
/// `--> file:line:col`), Rust panics, tsc, Python tracebacks, Node stack
/// traces, and the common `file:line[:col]: message` (Go, gcc/clang,
/// linters). Only files that exist, relative to `cwd`, count.
pub fn output_problems(output: &str, cwd: &Path) -> Vec<OutputProblem> {
    fn re(pattern: &str) -> Regex {
        Regex::new(pattern).expect("valid pattern")
    }
    static RUST: LazyLock<Regex> = LazyLock::new(|| re(r"^(error|warning)(?:\[\w+\])?: (.+)$"));
    static RUST_AT: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*--> (.+?):(\d+):(\d+)$"));
    static PANIC: LazyLock<Regex> = LazyLock::new(|| re(r"panicked at (.+?):(\d+):(\d+):?$"));
    static TSC: LazyLock<Regex> = LazyLock::new(|| re(r"^(.+?)(?:\((\d+),(\d+)\):|:(\d+):(\d+) -) (error|warning) (TS\d+: .+)$"));
    static PY_FRAME: LazyLock<Regex> = LazyLock::new(|| re(r#"^\s*File "(.+?)", line (\d+)"#));
    static JS_ERROR: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:Uncaught )?(?:[A-Z]\w*)?Error(?: \[\w+\])?: .+$"));
    static JS_FRAME: LazyLock<Regex> = LazyLock::new(|| re(r"^\s+at (?:.+? \()?(?:file://)?(.+?):(\d+):(\d+)\)?$"));
    static PLAIN: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*([^\s:]+):(\d+)(?::(\d+))?: ?(.+)$"));

    let file = |text: &str| -> Option<PathBuf> {
        let path = Path::new(text.trim());
        // Without `./` in it, so it matches the path of an open tab.
        let path: PathBuf = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }.components().collect();
        path.is_file().then_some(path)
    };
    let number = |text: Option<regex::Match>| text.and_then(|m| m.as_str().parse::<u32>().ok());
    let mut out: Vec<OutputProblem> = Vec::new();
    let mut add = |path: PathBuf, line: u32, column: Option<u32>, severity: Severity, message: &str| {
        let message = message.trim().to_string();
        if out.len() < MAX_PROBLEMS && !out.iter().any(|p| p.path == path && p.line == line && p.message == message) {
            out.push(OutputProblem { path, line, column, severity, message });
        }
    };
    let severity = |word: &str| if word == "warning" { Severity::Warning } else { Severity::Error };

    let lines: Vec<&str> = output.lines().collect();
    // Waiting for where they happened: rustc's message (its `-->` follows),
    // a Python traceback's innermost frame in the project (the exception
    // follows), Node's error (its stack follows).
    let mut rust: Option<(Severity, &str)> = None;
    let mut python: Option<(PathBuf, u32)> = None;
    let mut js: Option<&str> = None;
    for (ix, line) in lines.iter().enumerate() {
        if let Some(c) = RUST.captures(line) {
            rust = Some((severity(&c[1]), c.get(2).map_or("", |m| m.as_str())));
            continue;
        }
        if let Some(c) = RUST_AT.captures(line) {
            if let Some((severity, message)) = rust.take()
                && let Some(path) = file(&c[1])
            {
                add(path, number(c.get(2)).unwrap_or(1), number(c.get(3)), severity, message);
            }
            continue;
        }
        if let Some(c) = PANIC.captures(line) {
            if let Some(path) = file(&c[1]) {
                let message = lines[ix + 1..].iter().find(|l| !l.trim().is_empty()).copied().unwrap_or("panicked");
                add(path, number(c.get(2)).unwrap_or(1), number(c.get(3)), Severity::Error, message);
            }
            continue;
        }
        if let Some(c) = TSC.captures(line) {
            if let Some(path) = file(&c[1]) {
                let line = number(c.get(2).or(c.get(4))).unwrap_or(1);
                add(path, line, number(c.get(3).or(c.get(5))), severity(&c[6]), &c[7]);
            }
            continue;
        }
        if let Some(c) = PY_FRAME.captures(line) {
            if let Some(path) = file(&c[1]) {
                python = Some((path, number(c.get(2)).unwrap_or(1)));
            }
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        if !indented
            && !line.trim().is_empty()
            && !line.starts_with("Traceback")
            && let Some((path, at)) = python.take()
        {
            add(path, at, None, Severity::Error, line);
            continue;
        }
        if JS_ERROR.is_match(line) {
            js = Some(line);
            continue;
        }
        if let Some(c) = JS_FRAME.captures(line) {
            if let Some(message) = js
                && !c[1].contains("node_modules")
                && let Some(path) = file(&c[1])
            {
                add(path, number(c.get(2)).unwrap_or(1), number(c.get(3)), Severity::Error, message);
                js = None;
            }
            continue;
        }
        js = None;
        if let Some(c) = PLAIN.captures(line) {
            let message = &c[4];
            let lower = message.to_ascii_lowercase();
            // Pointers to related places, not problems of their own.
            if lower.starts_with("note") || lower.starts_with("help") {
                continue;
            }
            let (severity, message) = match lower.split_once(':') {
                Some((word, _)) if word.trim() == "warning" => (Severity::Warning, message.split_once(':').map_or(message, |m| m.1)),
                Some((word, _)) if word.trim() == "error" || word.trim() == "fatal error" => {
                    (Severity::Error, message.split_once(':').map_or(message, |m| m.1))
                }
                _ => (Severity::Error, message),
            };
            if let Some(path) = file(&c[1]) {
                add(path, number(c.get(2)).unwrap_or(1), number(c.get(3)), severity, message);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn targets(language: Language, path: &Path, text: &str) -> Vec<(String, String, Option<usize>)> {
        let syntax = crate::syntax::Syntax::new(language, &Rope::from_str(text));
        file_targets(language, path, text, syntax.tree().unwrap())
            .into_iter()
            .map(|t| (t.name, t.command, t.row))
            .collect()
    }

    fn temp_project(files: &[(&str, &str)]) -> PathBuf {
        // Tests run in parallel, several with the same first file.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kuana-run-{}-{n}-{}", std::process::id(), files[0].0.replace('/', "_")));
        for (rel, content) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    #[test]
    fn task_variables_are_expanded_once_and_quoted() {
        let vars = vec![
            ("ZED_FILE".to_string(), "/tmp/my project/a b.rs".to_string()),
            ("ZED_FILENAME".to_string(), "a b.rs".to_string()),
            ("ZED_SYMBOL".to_string(), "$(rm -rf ~)".to_string()),
            ("ZED_ROW".to_string(), "$ZED_FILE".to_string()),
        ];
        assert_eq!(
            expand_vars("run $ZED_FILE ${ZED_FILENAME} $ZED_SYMBOL:$ZED_ROW $HOME ${X} $ ${", &vars, shell_quote),
            "run '/tmp/my project/a b.rs' 'a b.rs' '$(rm -rf ~)':'$ZED_FILE' $HOME ${X} $ ${"
        );
        // Unquoted for labels and folders; values aren't expanded again.
        assert_eq!(expand_vars("test ${ZED_ROW}x", &vars, str::to_string), "test $ZED_FILEx");
    }

    #[test]
    fn rust_tests_and_main() {
        let src = "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() {}\n}\n";
        let dir = temp_project(&[("Cargo.toml", "[package]\nname = \"x\"\n"), ("src/main.rs", src)]);
        let got = targets(Language::Rust, &dir.join("src/main.rs"), src);
        assert_eq!(got[0], ("Run 'main'".into(), "cargo run".into(), Some(0)));
        assert_eq!(got[1], ("Tests in 'tests'".into(), "cargo test -- tests".into(), Some(3)));
        assert_eq!(got[2], ("Test 'adds'".into(), "cargo test -- tests::adds --exact".into(), Some(5)));
        let lib = "pub fn f() {}\n#[test]\nfn top() {}\n";
        let got = targets(Language::Rust, &dir.join("src/parser/mod.rs"), lib);
        assert_eq!(got, [("Test 'top'".into(), "cargo test -- parser::top --exact".into(), Some(2))]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn go_python_and_js() {
        let go = "package main\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {}\n";
        let got = targets(Language::Go, Path::new("/p/add_test.go"), go);
        assert_eq!(got, [("Test 'TestAdd'".into(), "go test -v -run '^TestAdd$' .".into(), Some(4))]);

        let py = "def test_one():\n    pass\n\nclass TestThing:\n    def test_two(self):\n        pass\n\nif __name__ == \"__main__\":\n    main()\n";
        let got = targets(Language::Python, Path::new("/p/t.py"), py);
        assert_eq!(got.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["Test 'test_one'", "Tests in 'TestThing'", "Test 'test_two'", "Run file"]);
        assert_eq!(got[2].1, format!("{PYTHON} -m pytest -v /p/t.py::TestThing::test_two"));

        let dir = temp_project(&[("package.json", "{\"devDependencies\": {\"vitest\": \"1\"}}"), ("a.test.ts", "")]);
        let ts = "describe('math', () => {\n  it('adds 1+1', () => {});\n});\n";
        let got = targets(Language::TypeScript, &dir.join("a.test.ts"), ts);
        assert_eq!(got[0].0, "Tests in 'math'");
        assert!(got[1].1.starts_with("npx vitest run --reporter=verbose ") && got[1].1.ends_with("-t 'adds 1\\+1'"), "{}", got[1].1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runs_what_a_rust_analyzer_lens_describes() {
        let runnable = serde_json::json!({
            "label": "test tests::doubles",
            "kind": "cargo",
            "args": { "workspaceRoot": "/w", "cargoArgs": ["test", "--package", "demo", "--bin", "demo"], "executableArgs": ["tests::doubles", "--exact"] },
        });
        let target = from_rust_analyzer(&runnable).unwrap();
        assert_eq!(target.name, "test tests::doubles");
        assert_eq!(target.command, "cargo test --package demo --bin demo -- tests::doubles --exact");
        assert_eq!(target.cwd, PathBuf::from("/w"));
        let Some(DebugSpec::Cargo { build, args }) = target.debug else { panic!("no debug spec") };
        assert_eq!((build[..2].to_vec(), args), (vec!["test".to_string(), "--no-run".into()], vec!["tests::doubles".to_string(), "--exact".into()]));
    }

    #[test]
    fn finds_problems_in_output() {
        let dir = temp_project(&[("src/main.rs", ""), ("main.go", ""), ("src/a.ts", ""), ("app.py", ""), ("app.js", "")]);
        let output = format!(
            "   Compiling demo v0.1.0\n\
             warning: unused variable: `x`\n --> src/main.rs:2:9\n  |\n\
             error[E0308]: mismatched types\n --> src/main.rs:4:18\n\
             note: function defined here\n --> src/lib.rs:1:4\n\
             error: could not compile `demo` (bin \"demo\") due to 1 previous error\n\
             thread 'tests::adds' panicked at src/main.rs:17:9:\nassertion `left == right` failed\n\
             ./main.go:5:2: undefined: total\n\
             src/a.ts(3,5): error TS2322: Type 'string' is not assignable to type 'number'.\n\
             src/a.ts:7:1 - warning TS6133: 'y' is declared but its value is never read.\n\
             Traceback (most recent call last):\n  File \"{py}\", line 9, in <module>\n    main()\n  File \"/usr/lib/python3/x.py\", line 3, in f\nValueError: bad value\n\
             TypeError: foo is not a function\n    at node:internal/main:1:1\n    at Object.<anonymous> ({js}:3:5)\n\
             missing.c:1:1: error: nothing here\n\
             Process finished with exit code 1\n",
            py = dir.join("app.py").display(),
            js = dir.join("app.js").display(),
        );
        let found: Vec<(String, u32, Option<u32>, Severity, String)> = output_problems(&output, &dir)
            .into_iter()
            .map(|p| (p.path.strip_prefix(&dir).unwrap().display().to_string().replace('\\', "/"), p.line, p.column, p.severity, p.message))
            .collect();
        let e = Severity::Error;
        assert_eq!(
            found,
            [
                ("src/main.rs".into(), 2, Some(9), Severity::Warning, "unused variable: `x`".into()),
                ("src/main.rs".into(), 4, Some(18), e, "mismatched types".into()),
                ("src/main.rs".into(), 17, Some(9), e, "assertion `left == right` failed".into()),
                ("main.go".into(), 5, Some(2), e, "undefined: total".into()),
                ("src/a.ts".into(), 3, Some(5), e, "TS2322: Type 'string' is not assignable to type 'number'.".into()),
                ("src/a.ts".into(), 7, Some(1), Severity::Warning, "TS6133: 'y' is declared but its value is never read.".into()),
                ("app.py".into(), 9, None, e, "ValueError: bad value".into()),
                ("app.js".into(), 3, Some(5), e, "TypeError: foo is not a function".into()),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn project_configurations() {
        let dir = temp_project(&[
            ("package.json", "{\"scripts\": {\"dev\": \"vite\", \"test\": \"vitest\"}}"),
            ("pnpm-lock.yaml", ""),
            ("Makefile", "VERSION := 1\n.PHONY: all\nall: build\nbuild:\n\tcc x.c\n"),
        ]);
        let targets = project_targets(&dir, None);
        let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["pnpm run dev", "pnpm run test", "make all", "make build"]);
        // Scripts debug through js-debug, with the package manager starting them.
        assert_eq!(targets[0].debug, Some(DebugSpec::Node { runtime: "pnpm".into(), args: vec!["run".into(), "dev".into()] }));
        assert_eq!(targets[2].debug, None);
        let current = project_targets(&dir, Some((&dir.join("a.ts"), Language::TypeScript)));
        let path = dir.join("a.ts").to_string_lossy().into_owned();
        assert_eq!(current.last().unwrap().debug, Some(DebugSpec::Node { runtime: "npx".into(), args: vec!["tsx".into(), path] }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oxie_project_targets() {
        let dir = temp_project(&[
            ("Cargo.toml", "[package]\nname = \"app\"\n\n[build-dependencies]\noxie-build = { path = \"../oxie-build\" }\n"),
            ("src/main.oxi", "fun main() {}\n"),
        ]);
        let targets = project_targets(&dir, None);
        let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(&names[..4], ["oxie run", "oxie test", "oxie build", "oxie fmt"]);
        assert!(names.contains(&"cargo run"));
        // Test ids as the compiler names them, in the file's module.
        assert_eq!(oxie_module(&dir.join("src/main.oxi")), Vec::<String>::new());
        assert_eq!(oxie_module(&dir.join("src/parser.oxi")), ["parser"]);
        assert_eq!(oxie_module(&dir.join("src/net/http.oxi")), ["net", "http"]);
        assert_eq!(oxie_module(&dir.join("src/net/mod.oxi")), ["net"]);
        assert_eq!(oxie_module(&std::env::temp_dir().join("script.oxi")), Vec::<String>::new());
        assert_eq!(oxie_slug("Adds two  numbers!"), "adds_two_numbers");
        assert_eq!(oxie_slug("2 + 2"), "test_2_2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lyth_project_targets() {
        let dir = temp_project(&[("go.mod", "module app\n\ngo 1.26\n"), ("cmd/app/main.lyth", "func main() {}\n")]);
        let targets = project_targets(&dir, None);
        let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(&names[..4], ["lyth run .", "lyth test ./...", "lyth build ./...", "lyth vet ./..."]);
        assert!(names.contains(&"go run ."));
        assert_eq!(targets.iter().find(|t| t.name == "lyth test ./...").map(|t| t.command.as_str()), Some("lyth test -v ./..."));
        // A Go module without .lyth files has no lyth commands.
        let plain = temp_project(&[("go.mod", "module app\n"), ("main.go", "package main\n")]);
        assert!(!project_targets(&plain, None).iter().any(|t| t.name.starts_with("lyth")));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&plain).ok();
    }

    #[test]
    fn quotes_for_each_shell() {
        assert_eq!(posix_quote("a b'c"), "'a b'\\''c'");
        assert_eq!(posix_quote("src/a.rs"), "src/a.rs");
        assert_eq!(powershell_quote("doesn't crash"), "'doesn''t crash'");
        assert_eq!(powershell_quote("it\u{2019}s"), "'it\u{2019}\u{2019}s'");
        assert_eq!(powershell_quote(r"C:\src\a.rs"), r"C:\src\a.rs");
        assert_eq!(powershell_quote("a,b"), "'a,b'");
        assert_eq!(powershell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }
}
