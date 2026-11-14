//! Formatting with command-line formatters (Prettier, rustfmt, gofmt, ruff,
//! black, taplo, shfmt), and turning a formatter's output into minimal edits
//! so carets and undo behave. Language-server formatting is in
//! `editor/format.rs`.

use crate::fs::PathExt as _;
use std::{
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{lsp::servers, syntax::Language};

/// A formatter command that reads the file on stdin and writes the result
/// to stdout.
#[derive(Clone, Debug, PartialEq)]
pub struct Formatter {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Installed in the project (node_modules, a venv), so it wins over a language server.
    pub project_local: bool,
}

/// Command-line formatters per language, in order of preference. `{file}`
/// is replaced with the file's path (formatters use it to find their
/// config and to pick a parser).
pub fn candidates(language: Language) -> &'static [(&'static str, &'static [&'static str])] {
    const PRETTIER: (&str, &[&str]) = ("prettier", &["--stdin-filepath", "{file}"]);
    match language {
        Language::JavaScript
        | Language::TypeScript
        | Language::Tsx
        | Language::Css
        | Language::Html
        | Language::Json
        | Language::Yaml
        | Language::Markdown => &[PRETTIER],
        Language::Rust => &[("rustfmt", &["--emit", "stdout", "--edition", "{edition}"])],
        Language::Go => &[("gofmt", &[])],
        Language::Python => &[
            ("ruff", &["format", "--stdin-filename", "{file}", "-"]),
            ("black", &["--quiet", "--stdin-filename", "{file}", "-"]),
        ],
        Language::Toml => &[("taplo", &["fmt", "-"])],
        Language::Shell => &[("shfmt", &["-filename", "{file}"])],
        Language::Php | Language::PlainText | Language::Extension(_) => &[],
    }
}

/// The formatter for `file`: a configured command, or the first installed
/// candidate (in the project's `node_modules/.bin` or Python venv, then on the
/// search path).
pub fn find(language: Language, file: &Path, configured: Option<&[String]>) -> Option<Formatter> {
    // Settings, else the extension that adds the language.
    let configured = configured.or_else(|| language.extension()?.formatter.as_deref());
    if let Some(command) = configured {
        let custom = servers::custom(command)?;
        let name = command.first()?.clone();
        let args = custom.args.iter().map(|a| expand(a, file)).collect();
        return Some(Formatter { name, program: custom.program, args, project_local: true });
    }
    let venv = (language == Language::Python).then(|| crate::python::venv(file.parent()?)).flatten();
    for (name, args) in candidates(language) {
        let local = file
            .ancestors()
            .skip(1)
            .map(|d| d.join("node_modules").join(".bin").join(name))
            .chain(venv.as_deref().map(|v| crate::python::bin_dir(v).join(name)))
            .find(|p| p.is_file_on_host());
        let project_local = local.is_some();
        let Some(program) = local.or_else(|| servers::which(name)) else { continue };
        let args = args.iter().map(|a| expand(a, file)).collect();
        return Some(Formatter { name: name.to_string(), program, args, project_local });
    }
    None
}

fn expand(arg: &str, file: &Path) -> String {
    match arg {
        "{file}" => file.to_string_lossy().into_owned(),
        "{edition}" => rust_edition(file),
        other => other.replace("{file}", &file.to_string_lossy()),
    }
}

/// The edition from the nearest Cargo.toml, so rustfmt parses the file the
/// way cargo does.
fn rust_edition(file: &Path) -> String {
    file.ancestors()
        .skip(1)
        .find_map(|d| crate::fs::read_to_string(d.join("Cargo.toml")).ok())
        .and_then(|toml| {
            toml.lines().find_map(|l| {
                let (key, value) = l.split_once('=')?;
                (key.trim() == "edition").then(|| value.trim().trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| "2021".to_string())
}

impl Formatter {
    /// Formats `text`; blocking, so run it on a background thread. Gives up
    /// after ten seconds.
    pub fn run(&self, text: &str, file: &Path) -> Result<String, String> {
        let dir = file.parent().unwrap_or(Path::new("."));
        let mut child = crate::exec::Command::new(&self.program)
            .args(&self.args)
            .current_dir(dir)
            .env("PATH", servers::search_path())
            .stdin(crate::exec::Stdio::piped())
            .stdout(crate::exec::Stdio::piped())
            .stderr(crate::exec::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Couldn't run {}: {e}", self.name))?;
        // Write on another thread: a formatter may fill stdout before
        // reading all of stdin.
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let input = text.to_string();
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let output = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| format!("{} took too long", self.name))?
            .map_err(|e| e.to_string())?;
        let _ = writer.join();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("it failed").trim();
            return Err(format!("{}: {first}", self.name));
        }
        String::from_utf8(output.stdout)
            .map(crate::encoding::to_lf)
            .map_err(|_| format!("{} returned text that isn't UTF-8", self.name))
    }
}

/// The smallest line-based edits that turn `old` into `new`, as byte
/// ranges of `old`.
pub fn line_edits(old: &str, new: &str) -> Vec<(Range<usize>, String)> {
    use similar::DiffOp;
    let old_lines = crate::git::split_lines(old);
    let new_lines = crate::git::split_lines(new);
    let diff = similar::TextDiff::configure().timeout(Duration::from_millis(500)).diff_slices(&old_lines, &new_lines);
    let mut starts = Vec::with_capacity(old_lines.len() + 1);
    let mut offset = 0;
    for line in &old_lines {
        starts.push(offset);
        offset += line.len();
    }
    starts.push(offset);
    let mut edits = Vec::new();
    for op in diff.ops() {
        let (old_range, new_range) = match *op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, new_index } => (old_index..old_index + old_len, new_index..new_index),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index..old_index, new_index..new_index + new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                (old_index..old_index + old_len, new_index..new_index + new_len)
            }
        };
        let bytes = starts[old_range.start]..starts[old_range.end];
        edits.push((bytes, new_lines[new_range].concat()));
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut out = old.to_string();
        for (range, text) in edits.iter().rev() {
            out.replace_range(range.clone(), text);
        }
        out
    }

    #[test]
    fn minimal_line_edits() {
        let old = "fn main(){\nlet x=1;\n  println!(\"{x}\");\n}\n";
        let new = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let edits = line_edits(old, new);
        assert_eq!(apply(old, &edits), new);
        // The closing brace line is untouched.
        assert!(edits.iter().all(|(r, _)| r.end <= old.len() - 2));
        assert!(line_edits("same\n", "same\n").is_empty());
        // A missing final newline is added.
        assert_eq!(apply("a", &line_edits("a", "a\n")), "a\n");
    }

    #[test]
    fn reads_rust_edition() {
        let dir = std::env::temp_dir().join(format!("kuana-fmt-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\nedition = \"2024\"\n").unwrap();
        assert_eq!(rust_edition(&dir.join("src/main.rs")), "2024");
        std::fs::remove_dir_all(&dir).ok();
    }
}
