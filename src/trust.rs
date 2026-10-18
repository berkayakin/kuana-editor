//! Project trust (Restricted Mode). A cloned repository can make an editor
//! run its code without being asked: a language server builds it, ESLint and
//! Prettier load its JavaScript config, `node_modules/.bin` and venv tools are
//! its own programs, a venv's activate script is a shell script, and Git runs
//! `core.fsmonitor` from its config. Until the folder is trusted, Kuana does
//! none of that; editing, Git and anything run explicitly (Run, Debug) work.
//!
//! Trusting a folder trusts everything inside it, so trusting a parent folder
//! covers every project in it. Trusted folders live in `trust.json`, a file of
//! their own: Kuana versions from before trust existed rewrite `state.json`
//! without the keys they don't know, which forgot every trusted folder. The
//! `trust_all_projects` setting turns the checks off without saving anything.

use std::{
    path::{Path, PathBuf},
    sync::{
        RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};

use crate::state::SessionState;

/// Trusted folders, loaded from `trust.json` on first use.
static TRUSTED: RwLock<Option<Vec<PathBuf>>> = RwLock::new(None);
static TRUST_ALL: AtomicBool = AtomicBool::new(false);

/// Bumped when trust changes, so windows start or stop what it allows.
pub struct TrustChanged(pub u64);
impl gpui::Global for TrustChanged {}

pub fn set_trust_all(on: bool) {
    TRUST_ALL.store(on, Ordering::Relaxed);
}

/// Whether Kuana may run what `path` (a project folder or a file) configures.
pub fn is_trusted(path: &Path) -> bool {
    is_trusted_by_user(path)
        // Kuana's own settings, keymap and themes.
        || crate::theme::config_dir().is_some_and(|dir| path.starts_with(dir))
}

/// Whether the user trusted the folder `path` is in, Kuana's config folder
/// not counting: what's installed there (extensions from the registry or
/// Git, servers) came from elsewhere.
pub fn is_trusted_by_user(path: &Path) -> bool {
    TRUST_ALL.load(Ordering::Relaxed) || trusted_by(path).is_some()
}

/// The trusted folder that covers `path`.
pub fn trusted_by(path: &Path) -> Option<PathBuf> {
    with_trusted(|folders| folders.iter().filter(|f| path.starts_with(f)).min_by_key(|f| f.as_os_str().len()).cloned())
}

fn with_trusted<T>(f: impl FnOnce(&Vec<PathBuf>) -> T) -> T {
    {
        let trusted = TRUSTED.read().unwrap_or_else(|e| e.into_inner());
        if let Some(folders) = trusted.as_ref() {
            return f(folders);
        }
    }
    let mut trusted = TRUSTED.write().unwrap_or_else(|e| e.into_inner());
    f(trusted.get_or_insert_with(load))
}

/// `trust.json`.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct TrustFile {
    folders: Vec<PathBuf>,
}

fn file_path() -> Option<PathBuf> {
    crate::theme::config_dir().map(|d| d.join("trust.json"))
}

/// The trusted folders on disk: `trust.json`, plus any still in `state.json`
/// from before it existed (moved over right away, before an older Kuana can
/// rewrite `state.json` without them).
fn load() -> Vec<PathBuf> {
    // One that doesn't read is kept aside: rewriting it with only the next
    // trusted folder would forget all the others for good.
    let file: TrustFile = match file_path().and_then(|p| Some((std::fs::read_to_string(&p).ok()?, p))) {
        Some((text, path)) => crate::state::read_lenient(&text).unwrap_or_else(|err| {
            crate::state::set_aside(&path, "The list of trusted folders (trust.json)", err);
            TrustFile::default()
        }),
        None => TrustFile::default(),
    };
    let legacy = SessionState::load().trusted_folders;
    let moving = !legacy.is_empty();
    let folders = merged(file.folders, legacy);
    if moving && let Err(err) = save(&folders) {
        eprintln!("couldn't move trusted folders to trust.json: {err}");
    }
    folders
}

fn merged(mut folders: Vec<PathBuf>, legacy: Vec<PathBuf>) -> Vec<PathBuf> {
    for folder in legacy {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders.sort();
    folders
}

/// Writes `trust.json` (through a temporary file, so a reader never sees half
/// of it) and empties `state.json`'s old list, which `load` would merge back.
fn save(folders: &[PathBuf]) -> std::io::Result<()> {
    let path = file_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    let json = serde_json::to_string_pretty(&TrustFile { folders: folders.to_vec() })? + "\n";
    crate::state::write_atomic(&path, json.as_bytes())?;
    if !SessionState::load().trusted_folders.is_empty() {
        SessionState::change(|state| state.trusted_folders.clear())?;
    }
    Ok(())
}

/// Trusts `folder` and everything in it, and remembers it.
pub fn trust(folder: &Path, cx: &mut gpui::App) {
    change(cx, |folders| {
        if !folders.iter().any(|f| folder.starts_with(f)) {
            // Folders inside it are covered now.
            folders.retain(|f| !f.starts_with(folder));
            folders.push(folder.to_path_buf());
        }
    });
}

/// Stops trusting `folder` (an entry, as `trusted_by` gives it).
pub fn untrust(folder: &Path, cx: &mut gpui::App) {
    change(cx, |folders| folders.retain(|f| f != folder));
}

fn change(cx: &mut gpui::App, edit: impl FnOnce(&mut Vec<PathBuf>)) {
    let folders = {
        let mut trusted = TRUSTED.write().unwrap_or_else(|e| e.into_inner());
        // The file has the latest list (another window may have changed it).
        let folders = trusted.insert(load());
        edit(folders);
        folders.sort();
        folders.clone()
    };
    if let Err(err) = save(&folders) {
        eprintln!("couldn't save trusted folders: {err}");
    }
    changed(cx);
}

/// Tells windows that trust changed.
pub fn changed(cx: &mut gpui::App) {
    let next = cx.try_global::<TrustChanged>().map_or(1, |t| t.0 + 1);
    cx.set_global(TrustChanged(next));
}

/// Every trusted folder.
pub fn folders() -> Vec<PathBuf> {
    with_trusted(|folders| folders.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_cover_their_contents() {
        *TRUSTED.write().unwrap() = Some(vec![PathBuf::from("/work/clients")]);
        // `trusted_by`, not `is_trusted`: LSP tests turn trust-all on meanwhile.
        assert!(trusted_by(Path::new("/work/clients/acme/src/main.rs")).is_some());
        assert!(trusted_by(Path::new("/work/clients-old")).is_none());
        assert!(trusted_by(Path::new("/work")).is_none());
        assert_eq!(trusted_by(Path::new("/work/clients/acme")), Some(PathBuf::from("/work/clients")));
    }

    #[test]
    fn folders_from_state_json_are_kept() {
        let folders = merged(vec![PathBuf::from("/b")], vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(folders, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn state_keeps_keys_it_does_not_know() {
        let state: SessionState = serde_json::from_str(r#"{"last_root": "/p", "from_a_newer_kuana": [1, 2]}"#).unwrap();
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains(r#""from_a_newer_kuana":[1,2]"#), "{json}");
    }
}
