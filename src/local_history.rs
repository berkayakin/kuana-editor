//! Local History, as in JetBrains IDEs: a file's earlier versions, kept
//! outside the project (`<config>/local_history/`), whatever Git knows. A
//! version is kept when the file is saved (and the first time Kuana saves
//! it, what it was before), before and after a change from outside replaces
//! an open file's text, before a Git rollback and before a Restore. Show
//! Local History lists them; the diff view compares one with the file as it
//! is and restores it.
//!
//! Each file has a folder named by a hash of its path (with the path itself
//! in `path`), holding one file per version: `<unix millis>-<kind>-<content
//! hash>`. A version the same as the newest isn't kept again, saves less
//! than `SAVE_MERGE` apart keep only the last, and versions older than
//! `MAX_AGE` or past `MAX_VERSIONS` go. Nothing is kept until `init` names
//! the folder (the app does at startup; tests don't, so they write nothing).

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Files larger than this have no history (a log, a dump).
const MAX_SIZE: usize = 2 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
const MAX_VERSIONS: usize = 200;
/// Saves this close together (auto-save while typing) keep only the last.
const SAVE_MERGE: Duration = Duration::from_secs(60);

/// Keeps history in `<config_dir>/local_history`, and forgets what's past
/// its age there (in the background).
pub fn init(config_dir: &Path) {
    let root = config_dir.join("local_history");
    if ROOT.set(root.clone()).is_ok() {
        std::thread::Builder::new().name("local-history-prune".into()).spawn(move || prune_all(&root, SystemTime::now())).ok();
    }
}

/// Why a version was kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// What the file was before Kuana first saved it.
    Original,
    Saved,
    /// An open file's text before a change on disk replaced it, and after.
    BeforeReload,
    Reloaded,
    BeforeRollback,
    BeforeRestore,
}

impl Kind {
    const ALL: [Kind; 6] = [Kind::Original, Kind::Saved, Kind::BeforeReload, Kind::Reloaded, Kind::BeforeRollback, Kind::BeforeRestore];

    fn tag(self) -> &'static str {
        match self {
            Kind::Original => "original",
            Kind::Saved => "saved",
            Kind::BeforeReload => "before-reload",
            Kind::Reloaded => "reloaded",
            Kind::BeforeRollback => "before-rollback",
            Kind::BeforeRestore => "before-restore",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Original => "Before Kuana saved it",
            Kind::Saved => "Saved",
            Kind::BeforeReload => "Before it changed on disk",
            Kind::Reloaded => "Changed on disk",
            Kind::BeforeRollback => "Before Rollback",
            Kind::BeforeRestore => "Before Restore",
        }
    }
}

/// One kept version of a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub time: SystemTime,
    pub kind: Kind,
    file: PathBuf,
    hash: String,
}

impl Version {
    pub fn read(&self) -> Option<String> {
        std::fs::read_to_string(&self.file).ok()
    }

    /// Reads it from its file name (`<millis>-<kind>-<hash>`).
    fn parse(file: PathBuf) -> Option<Version> {
        let name = file.file_name()?.to_str()?;
        let (millis, rest) = name.split_once('-')?;
        let (tag, hash) = rest.rsplit_once('-')?;
        let kind = Kind::ALL.into_iter().find(|k| k.tag() == tag)?;
        let time = UNIX_EPOCH + Duration::from_millis(millis.parse().ok()?);
        Some(Version { time, kind, hash: hash.to_string(), file })
    }
}

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn folder(root: &Path, path: &Path) -> PathBuf {
    root.join(hash(&path.to_string_lossy()))
}

/// Keeps `text` as a version of `path` (blocking: off the UI thread).
pub fn record(path: &Path, text: &str, kind: Kind) {
    if let Some(root) = ROOT.get() {
        record_in(root, path, text, kind, SystemTime::now()).ok();
    }
}

/// `record` for a document's text, without copying one too large to keep.
pub fn record_rope(path: &Path, rope: &ropey::Rope, kind: Kind) {
    if ROOT.get().is_some() && rope.len_bytes() <= MAX_SIZE {
        record(path, &rope.to_string(), kind);
    }
}

/// Keeps what `path` holds on disk now, if it's text.
pub fn record_disk(path: &Path, kind: Kind) {
    if let Ok(text) = std::fs::read(path).map(String::from_utf8)
        && let Ok(text) = text
    {
        record(path, &text, kind);
    }
}

/// Whether anything is kept for `path` yet.
pub fn has_history(path: &Path) -> bool {
    ROOT.get().is_some_and(|root| folder(root, path).is_dir())
}

/// `path`'s versions, the newest first.
pub fn versions(path: &Path) -> Vec<Version> {
    ROOT.get().map(|root| versions_in(&folder(root, path))).unwrap_or_default()
}

fn versions_in(dir: &Path) -> Vec<Version> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut list: Vec<Version> = entries.filter_map(|e| Version::parse(e.ok()?.path())).collect();
    list.sort_by(|a, b| b.time.cmp(&a.time));
    list
}

fn record_in(root: &Path, path: &Path, text: &str, kind: Kind, now: SystemTime) -> std::io::Result<()> {
    if text.len() > MAX_SIZE {
        return Ok(());
    }
    let dir = folder(root, path);
    std::fs::create_dir_all(&dir)?;
    if !dir.join("path").exists() {
        std::fs::write(dir.join("path"), path.to_string_lossy().as_bytes())?;
    }
    let mut list = versions_in(&dir);
    let hash = hash(text);
    if list.first().is_some_and(|newest| newest.hash == hash) {
        return Ok(());
    }
    if kind == Kind::Saved
        && let Some(newest) = list.first()
        && newest.kind == Kind::Saved
        && now.duration_since(newest.time).is_ok_and(|age| age < SAVE_MERGE)
    {
        std::fs::remove_file(&newest.file).ok();
        list.remove(0);
    }
    // Later than the newest, so versions keep the order they were kept in.
    let mut time = now;
    if let Some(newest) = list.first()
        && time <= newest.time
    {
        time = newest.time + Duration::from_millis(1);
    }
    let millis = time.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    crate::state::write_atomic(&dir.join(format!("{millis}-{}-{hash}", kind.tag())), text.as_bytes())?;
    for old in list.iter().skip(MAX_VERSIONS - 1).chain(list.iter().filter(|v| now.duration_since(v.time).is_ok_and(|age| age > MAX_AGE))) {
        std::fs::remove_file(&old.file).ok();
    }
    Ok(())
}

/// Removes versions past their age everywhere, and folders left empty.
fn prune_all(root: &Path, now: SystemTime) {
    let Ok(folders) = std::fs::read_dir(root) else { return };
    for dir in folders.filter_map(|e| Some(e.ok()?.path())).filter(|p| p.is_dir()) {
        let list = versions_in(&dir);
        let old: Vec<&Version> = list.iter().filter(|v| now.duration_since(v.time).is_ok_and(|age| age > MAX_AGE)).collect();
        for version in &old {
            std::fs::remove_file(&version.file).ok();
        }
        if old.len() == list.len() {
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_versions_newest_first_without_repeats() {
        let root = std::env::temp_dir().join(format!("kuana-local-history-{}", std::process::id()));
        let file = Path::new("/work/p/src/main.rs");
        let start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let at = |secs| start + Duration::from_secs(secs);
        record_in(&root, file, "one", Kind::Original, at(0)).unwrap();
        record_in(&root, file, "two", Kind::Saved, at(1)).unwrap();
        // The same text again isn't another version.
        record_in(&root, file, "two", Kind::Saved, at(2)).unwrap();
        // A save soon after replaces the last save.
        record_in(&root, file, "three", Kind::Saved, at(30)).unwrap();
        record_in(&root, file, "four", Kind::BeforeReload, at(30)).unwrap();
        let list = versions_in(&folder(&root, file));
        let texts: Vec<_> = list.iter().map(|v| (v.read().unwrap(), v.kind)).collect();
        assert_eq!(texts, [("four".into(), Kind::BeforeReload), ("three".into(), Kind::Saved), ("one".into(), Kind::Original)]);
        assert!(list[0].time > list[1].time, "kept at the same moment, still in order");
        assert_eq!(std::fs::read_to_string(folder(&root, file).join("path")).unwrap(), "/work/p/src/main.rs");

        // Past its age, a version goes; a folder with nothing left goes too.
        prune_all(&root, at(0) + MAX_AGE + Duration::from_secs(10));
        assert_eq!(versions_in(&folder(&root, file)).len(), 2);
        prune_all(&root, at(60) + MAX_AGE * 2);
        assert!(!folder(&root, file).exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn keeps_at_most_so_many() {
        let root = std::env::temp_dir().join(format!("kuana-local-history-max-{}", std::process::id()));
        let file = Path::new("/work/p/a.txt");
        let start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for n in 0..MAX_VERSIONS + 5 {
            record_in(&root, file, &n.to_string(), Kind::BeforeReload, start + Duration::from_secs(n as u64)).unwrap();
        }
        let list = versions_in(&folder(&root, file));
        assert_eq!(list.len(), MAX_VERSIONS);
        assert_eq!(list[0].read().as_deref(), Some((MAX_VERSIONS + 4).to_string().as_str()));
        std::fs::remove_dir_all(&root).ok();
    }
}
