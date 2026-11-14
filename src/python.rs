//! Python virtual environments. A project's venv (`.venv`, `venv`, `env` or
//! `.env` holding `pyvenv.cfg`, in the file's folder or above) is activated in
//! new terminals, put first on PATH for Run tabs, language servers and
//! formatters, and used by the debugger. A project can pick another venv or
//! none (the status bar's menu); the choice is saved in its session, like the
//! interpreter VS Code remembers per workspace.

use crate::fs::PathExt as _;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::RwLock,
};

const NAMES: [&str; 4] = [".venv", "venv", "env", ".env"];

/// A project's choice; without one, venvs are detected.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    Off,
    Venv(PathBuf),
}

impl Choice {
    /// How sessions store it.
    pub fn to_saved(&self) -> String {
        match self {
            Choice::Off => "off".into(),
            Choice::Venv(path) => path.to_string_lossy().into_owned(),
        }
    }

    pub fn from_saved(saved: &str) -> Self {
        match saved {
            "off" => Choice::Off,
            path => Choice::Venv(PathBuf::from(path)),
        }
    }
}

/// Each open project's choice, read wherever a venv is needed (also off the
/// main thread: language servers, formatters, the debugger).
static CHOICES: RwLock<Vec<(PathBuf, Choice)>> = RwLock::new(Vec::new());

pub fn set_choice(root: &Path, choice: Option<Choice>) {
    let mut choices = CHOICES.write().unwrap_or_else(|e| e.into_inner());
    choices.retain(|(r, _)| r != root);
    if let Some(choice) = choice {
        choices.push((root.to_path_buf(), choice));
    }
}

pub fn choice(root: &Path) -> Option<Choice> {
    let choices = CHOICES.read().unwrap_or_else(|e| e.into_inner());
    choices.iter().find(|(r, _)| r == root).map(|(_, c)| c.clone())
}

/// The venv for files in `dir`: the choice of the project holding it, else
/// the nearest one found in `dir` or above.
pub fn venv(dir: &Path) -> Option<PathBuf> {
    let chosen = {
        let choices = CHOICES.read().unwrap_or_else(|e| e.into_inner());
        choices.iter().filter(|(r, _)| dir.starts_with(r)).max_by_key(|(r, _)| r.as_os_str().len()).map(|(_, c)| c.clone())
    };
    match chosen {
        Some(Choice::Off) => None,
        Some(Choice::Venv(path)) => is_venv(&path).then_some(path),
        None => detect(dir),
    }
}

/// The nearest venv in `dir` or above.
pub fn detect(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find_map(|d| NAMES.iter().map(|n| d.join(n)).find(|v| is_venv(v)))
}

/// Venvs in the project's folder and the folders directly in it.
pub fn candidates(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(entries) = crate::fs::read_dir(root) {
        let mut subdirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.') || n == "node_modules" || n == "target"))
            .collect();
        subdirs.sort();
        dirs.extend(subdirs);
    }
    dirs.iter().flat_map(|d| NAMES.iter().map(move |n| d.join(n))).filter(|v| is_venv(v)).collect()
}

fn is_venv(dir: &Path) -> bool {
    dir.join("pyvenv.cfg").is_file_on_host()
}

/// Where the venv keeps its executables.
pub fn bin_dir(venv: &Path) -> PathBuf {
    venv.join(if windows_venv(venv) { "Scripts" } else { "bin" })
}

/// A venv made by Windows' Python (a remote project's is its host's, Linux or macOS).
fn windows_venv(venv: &Path) -> bool {
    cfg!(windows) && !crate::fs::is_remote(venv)
}

/// The venv's interpreter.
pub fn interpreter(venv: &Path) -> PathBuf {
    bin_dir(venv).join(if windows_venv(venv) { "python.exe" } else { "python" })
}

/// `path` with the venv's executables first.
pub fn path_with(venv: Option<&Path>, path: &std::ffi::OsStr) -> OsString {
    let Some(venv) = venv else { return path.to_os_string() };
    let dirs = std::iter::once(bin_dir(venv)).chain(std::env::split_paths(path));
    std::env::join_paths(dirs).unwrap_or_else(|_| path.to_os_string())
}

/// The line that activates the venv in `shell` (a program name or path),
/// typed into a new terminal like a user would. The leading space keeps it
/// out of the history in shells set to ignore such lines.
pub fn activate_command(venv: &Path, shell: &str) -> Option<String> {
    let name = Path::new(shell).file_stem()?.to_string_lossy().to_lowercase();
    let bin = bin_dir(venv);
    let (script, line): (&str, fn(&str) -> String) = match name.as_str() {
        "fish" => ("activate.fish", |s| format!("source {}", quote(s))),
        "csh" | "tcsh" => ("activate.csh", |s| format!("source {}", quote(s))),
        "nu" => ("activate.nu", |s| format!("overlay use {}", quote(s))),
        "pwsh" | "powershell" => ("Activate.ps1", |s| format!("& {}", crate::run::powershell_string(s))),
        "cmd" => ("activate.bat", |s| format!("\"{s}\"")),
        _ => ("activate", |s| format!(". {}", quote(s))),
    };
    let script = bin.join(script);
    script.is_file_on_host().then(|| format!(" {}", line(&script.to_string_lossy())))
}

/// Shell lines that put the venv first on PATH, like its activate script,
/// for a POSIX shell (Run tabs).
pub fn exports(venv: &Path) -> String {
    format!(
        "export VIRTUAL_ENV={}\nexport PATH={}:\"$PATH\"\nunset PYTHONHOME\n",
        quote(&venv.to_string_lossy()),
        quote(&bin_dir(venv).to_string_lossy())
    )
}

/// The same for PowerShell (Run tabs on Windows).
pub fn powershell_exports(venv: &Path) -> String {
    use crate::run::powershell_string;
    format!(
        "$env:VIRTUAL_ENV = {}\n$env:PATH = {} + $env:PATH\nRemove-Item Env:PYTHONHOME -ErrorAction SilentlyContinue\n",
        powershell_string(&venv.to_string_lossy()),
        powershell_string(&format!("{};", bin_dir(venv).to_string_lossy()))
    )
}

/// Single-quoted for POSIX shells and fish.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A short name for the status bar: the venv's folder, with its parent
/// when the folder has a generic name.
pub fn label(venv: &Path, root: Option<&Path>) -> String {
    let name = venv.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match (venv.parent(), root) {
        (Some(parent), Some(root)) if parent != root && parent.starts_with(root) => {
            let rel = parent.strip_prefix(root).unwrap_or(parent);
            format!("{}/{name}", rel.display())
        }
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_venv(dir: &Path) {
        std::fs::create_dir_all(bin_dir(dir)).unwrap();
        std::fs::write(dir.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        std::fs::write(bin_dir(dir).join("activate"), "").unwrap();
    }

    #[test]
    fn finds_and_names_venvs() {
        let root = std::env::temp_dir().join(format!("kuana-venv-{}", std::process::id()));
        let app = root.join("services/app");
        std::fs::create_dir_all(&app).unwrap();
        // A plain folder named env isn't a venv.
        std::fs::create_dir_all(root.join("env")).unwrap();
        make_venv(&root.join(".venv"));
        make_venv(&root.join("services/venv"));

        assert_eq!(venv(&app), Some(root.join("services/venv")));
        assert_eq!(venv(&root), Some(root.join(".venv")));
        assert_eq!(candidates(&root), vec![root.join(".venv"), root.join("services/venv")]);

        set_choice(&root, Some(Choice::Venv(root.join(".venv"))));
        assert_eq!(venv(&app), Some(root.join(".venv")));
        set_choice(&root, Some(Choice::Off));
        assert_eq!(venv(&app), None);
        assert_eq!(Choice::from_saved(&Choice::Off.to_saved()), Choice::Off);
        set_choice(&root, None);
        assert_eq!(venv(&app), Some(root.join("services/venv")));

        assert_eq!(label(&root.join(".venv"), Some(&root)), ".venv");
        assert_eq!(label(&root.join("services/venv"), Some(&root)), "services/venv");

        let activate = activate_command(&root.join(".venv"), "/bin/zsh").unwrap();
        assert!(activate.starts_with(" . '") && activate.ends_with(&format!("{}activate'", bin_dir(Path::new("")).join("").display())), "{activate}");
        // No activate.fish in this venv.
        assert_eq!(activate_command(&root.join(".venv"), "fish"), None);
        std::fs::remove_dir_all(&root).ok();
    }
}
