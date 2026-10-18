//! UI state remembered between launches (panel layout, last project, each
//! project's open tabs). Kept separate from `settings.json`, which holds user
//! preferences.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, PoisonError},
    time::Duration,
};

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};

use crate::{document::Breakpoint, run::RunTarget, theme::config_dir};

/// Writes a file whole or not at all: to a temporary file in the same
/// folder, flushed to disk, then renamed over it, so a crash or a full disk
/// mid-write never leaves it truncated. A symlinked file (dotfiles repos)
/// stays a link: its target is written.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Its own temporary file: writes can overlap (background session saves,
    // another window), and one mustn't rename another's half-written file.
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = target.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()));
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(err) = written.and_then(|_| std::fs::rename(&temp, &target)) {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

/// Problems reading Kuana's own files (a session or state that didn't
/// parse and was kept aside), for the next window to show.
static PROBLEMS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// What went wrong reading Kuana's files since last asked.
pub fn take_problems() -> Vec<String> {
    PROBLEMS.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
}

/// Moves an unreadable file aside (`<name>.corrupt-<unix time>`) instead of
/// letting the next save overwrite it, and notes it for the user.
pub fn set_aside(path: &Path, what: &str, error: impl std::fmt::Display) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let aside = path.with_file_name(format!("{name}.corrupt-{secs}"));
    let message = match std::fs::rename(path, &aside) {
        Ok(()) => format!("{what} couldn't be read ({error}); it was kept as {}", aside.display()),
        Err(err) => format!("{what} couldn't be read ({error}) or kept aside ({err})"),
    };
    eprintln!("{message}");
    if let Ok(mut problems) = PROBLEMS.lock() {
        problems.push(message);
    }
}

/// Reads JSON a newer or older Kuana may have written: a value of the wrong
/// shape costs only its own key (and keys this version doesn't know are
/// ignored or kept in a flattened map), never the whole file. Err only when
/// the text isn't a JSON object at all.
pub fn read_lenient<T: DeserializeOwned + Default>(text: &str) -> Result<T, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let serde_json::Value::Object(map) = value else { return Err("not a JSON object".into()) };
    if let Ok(whole) = serde_json::from_value(serde_json::Value::Object(map.clone())) {
        return Ok(whole);
    }
    let mut kept = serde_json::Map::new();
    for (key, value) in map {
        kept.insert(key.clone(), value);
        if serde_json::from_value::<T>(serde_json::Value::Object(kept.clone())).is_err() {
            kept.remove(&key);
        }
    }
    Ok(serde_json::from_value(serde_json::Value::Object(kept)).unwrap_or_default())
}

/// A list where an entry that doesn't read (written by another version) is
/// skipped instead of failing the rest.
fn skip_bad<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(values.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
}

/// An optional value that doesn't read (another version's) becomes None.
fn none_if_bad<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SessionState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_root: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidebar_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problems_height: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_sidebar: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_problems: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_terminal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidebar_mode: Option<String>,
    /// Editor font zoom in points, added to the configured size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_zoom: Option<f32>,
    /// Diffs show side by side (`false`: unified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_side_by_side: Option<bool>,
    /// The bottom panel shows the Git log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_git_log: Option<bool>,
    /// Recently opened project folders, newest first (the welcome screen's list).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_projects: Vec<PathBuf>,
    /// Where trusted folders were kept before `trust.json`; `trust.rs` moves
    /// them over and clears this.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trusted_folders: Vec<PathBuf>,
    /// Welcome Setup was finished (Get Started or Escape). Only finishing it
    /// sets this; the older `onboarded` key was also set by guessing from
    /// existing files, so installs that never saw the setup missed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub welcome_done: Option<bool>,
    /// Where the last moved or resized window was, for the next launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowPlacement>,
    /// Where the Preferences window was last, to reopen it there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferences_window: Option<WindowPlacement>,
    /// Keys this version doesn't know (a newer Kuana wrote them), kept so that
    /// saving here doesn't drop them.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// A window's place on screen, in points.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct WindowPlacement {
    /// The display it was on (its UUID), so it reopens there while connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Top-left corner, relative to that display.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fullscreen: bool,
}

impl SessionState {
    /// Whether Welcome Setup should show: until it's been finished once, on
    /// any platform and whatever else is in the config folder.
    pub fn needs_onboarding() -> bool {
        Self::load().welcome_done != Some(true)
    }

    pub fn set_onboarded() {
        if let Err(err) = Self::change(|state| state.welcome_done = Some(true)) {
            eprintln!("couldn't save window state: {err}");
        }
    }

    /// The saved state, with the changes not written yet (`change_soon`); a
    /// value that doesn't read costs only its key, and a file that isn't
    /// JSON at all is kept aside (and reported) rather than overwritten
    /// with defaults.
    pub fn load() -> Self {
        STATE.load()
    }

    /// Reads the file, makes `change` (after the changes waiting) and writes
    /// it, here and now.
    pub fn change(change: impl FnOnce(&mut Self)) -> std::io::Result<()> {
        STATE.change(change)
    }

    /// Makes `change` at once for whoever loads the state and writes it with
    /// the next write (`write_soon`, or quitting): panel toggles and resizes
    /// don't wait for the disk. It replaces the change waiting for the same
    /// `what` and `owner` (a window), so it must set values, not step them.
    pub fn change_soon(what: &'static str, owner: u64, change: impl Fn(&mut Self) + Send + Sync + 'static) {
        STATE.change_soon((what, owner), change);
    }

    /// Writes the changes waiting, here and now (quitting); a write under
    /// way finishes first.
    pub fn write_pending() -> std::io::Result<()> {
        STATE.write_pending()
    }
}

/// How long changes wait (`SessionState::change_soon`) before they're
/// written, so a burst (panels toggled, a window dragged) is one write.
const WRITE_DELAY: Duration = Duration::from_millis(500);

/// Writes the changes waiting a moment from now, in the background (with
/// `write_atomic`'s flush to disk). Keep the task to put the write off again
/// with the next change, or detach it.
pub fn write_soon(cx: &gpui::App) -> gpui::Task<()> {
    let executor = cx.background_executor().clone();
    cx.background_executor().spawn(async move {
        executor.timer(WRITE_DELAY).await;
        if let Err(err) = SessionState::write_pending() {
            eprintln!("couldn't save window state: {err}");
        }
    })
}

/// Writes the changes still waiting when Kuana quits.
pub fn init(cx: &mut gpui::App) {
    cx.on_app_quit(|_| {
        if let Err(err) = SessionState::write_pending() {
            eprintln!("couldn't save window state: {err}");
        }
        async {}
    })
    .detach();
}

/// A change to `state.json` waiting to be written.
type StateChange = Arc<dyn Fn(&mut SessionState) + Send + Sync>;

static STATE: LazyLock<StateFile> = LazyLock::new(|| StateFile::at(config_dir().map(|d| d.join("state.json"))));

/// `state.json`, changed from any window or thread. Every write reads the
/// file, makes the changes waiting and its own, and writes it, all while
/// holding `writing`, so writers never drop each other's changes; reads see
/// the changes waiting too.
struct StateFile {
    path: Option<PathBuf>,
    /// The changes waiting, by what and whose, in the order they were made.
    pending: Mutex<Vec<((&'static str, u64), StateChange)>>,
    writing: Mutex<()>,
}

impl StateFile {
    fn at(path: Option<PathBuf>) -> Self {
        Self { path, pending: Mutex::new(Vec::new()), writing: Mutex::new(()) }
    }

    fn read(&self) -> SessionState {
        let Some(path) = &self.path else { return SessionState::default() };
        let Ok(text) = std::fs::read_to_string(path) else { return SessionState::default() };
        read_lenient(&text).unwrap_or_else(|err| {
            set_aside(path, "Kuana's window state (state.json)", err);
            SessionState::default()
        })
    }

    fn waiting(&self) -> Vec<StateChange> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|(_, change)| change.clone()).collect()
    }

    fn load(&self) -> SessionState {
        let mut state = self.read();
        for change in self.waiting() {
            change(&mut state);
        }
        state
    }

    fn change(&self, change: impl FnOnce(&mut SessionState)) -> std::io::Result<()> {
        let path = self.path.as_ref().ok_or_else(|| std::io::Error::other("no config directory"))?;
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let waiting = self.waiting();
        let mut state = self.read();
        for change in &waiting {
            change(&mut state);
        }
        change(&mut state);
        write_atomic(path, (serde_json::to_string_pretty(&state)? + "\n").as_bytes())?;
        // Written; a change made meanwhile stays for the next write.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).retain(|(_, change)| !waiting.iter().any(|w| Arc::ptr_eq(w, change)));
        Ok(())
    }

    fn change_soon(&self, key: (&'static str, u64), change: impl Fn(&mut SessionState) + Send + Sync + 'static) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.retain(|(k, _)| *k != key);
        pending.push((key, Arc::new(change)));
    }

    fn write_pending(&self) -> std::io::Result<()> {
        if self.pending.lock().unwrap_or_else(PoisonError::into_inner).is_empty() {
            drop(self.writing.lock().unwrap_or_else(PoisonError::into_inner));
            return Ok(());
        }
        self.change(|_| {})
    }
}

/// A project's open tabs, restored when it's opened again. Unsaved edits are
/// kept too ("hot exit"), so closing a project window never loses work.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ProjectSession {
    pub root: PathBuf,
    /// A tab another Kuana version wrote in a way this one can't read is
    /// skipped, never the others (they hold unsaved text).
    #[serde(deserialize_with = "skip_bad")]
    pub tabs: Vec<TabSession>,
    /// Index into `tabs` of the focused tab.
    pub active: usize,
    /// Split panes as older Kuanas kept them (one row or column); read, no
    /// longer written (see `panes`).
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "none_if_bad")]
    pub layout: Option<PaneLayout>,
    /// Split panes, as a tree of rows and columns; absent for a single pane.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "none_if_bad")]
    pub panes: Option<PaneNode>,
    /// Expanded folders in the file tree, relative to `root`.
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "skip_bad")]
    pub expanded: Vec<PathBuf>,
    /// Line breakpoints per file, open or not.
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "skip_bad")]
    pub breakpoints: Vec<FileBreakpoints>,
    /// The Python venv picked for the project (a path, or "off"); detected
    /// when absent.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "none_if_bad")]
    pub python_venv: Option<String>,
    /// The terminal panel; absent in sessions from before it was saved.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "none_if_bad")]
    pub terminals: Option<TerminalLayout>,
    /// The configuration the title bar's run widget shows (Run runs it
    /// again); one from a newer Kuana (a new way to debug) is dropped.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "none_if_bad")]
    pub last_run: Option<RunTarget>,
    /// Keys this version doesn't know (a newer Kuana wrote them), kept so
    /// that saving here doesn't drop them.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// The terminal panel's shells, by group (splits side by side).
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TerminalLayout {
    /// The panel was showing.
    pub open: bool,
    pub groups: Vec<TerminalGroupSession>,
    /// Index into `groups` of the one with the keyboard.
    pub active: usize,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TerminalGroupSession {
    pub tabs: Vec<TerminalTabSession>,
    /// Index into `tabs` of the current one.
    pub active: usize,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TerminalTabSession {
    /// The name the user gave it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The folder its shell was in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

/// A file's line breakpoints.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct FileBreakpoints {
    /// Relative to the project.
    pub path: PathBuf,
    pub breakpoints: Vec<Breakpoint>,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct PaneLayout {
    /// Stacked rather than side by side.
    pub column: bool,
    /// Each pane's share of the space.
    pub sizes: Vec<f32>,
    /// Each pane's active tab, counted within the pane.
    pub active: Vec<usize>,
}

impl PaneLayout {
    /// The row or column of panes as a tree.
    fn to_tree(&self) -> PaneNode {
        let parts = self
            .sizes
            .iter()
            .enumerate()
            .map(|(ix, &share)| PaneShare { share, node: PaneNode::Pane { active: self.active.get(ix).copied().unwrap_or(0) } })
            .collect();
        if self.column { PaneNode::Column(parts) } else { PaneNode::Row(parts) }
    }
}

/// How panes divide the editor area: rows of panes side by side and columns
/// of stacked ones, nested. Panes are numbered in tree order (depth first, a
/// split's first member first): the number its tabs have as `pane`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PaneNode {
    /// A pane; `active` is its current tab, counted among its tabs.
    Pane { active: usize },
    Row(Vec<PaneShare>),
    Column(Vec<PaneShare>),
}

/// A member of a row or column, with its part of the room.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PaneShare {
    pub share: f32,
    #[serde(flatten)]
    pub node: PaneNode,
}

impl PaneNode {
    pub fn pane_count(&self) -> usize {
        match self {
            PaneNode::Pane { .. } => 1,
            PaneNode::Row(parts) | PaneNode::Column(parts) => parts.iter().map(|p| p.node.pane_count()).sum(),
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TabSession {
    /// What the tab shows; absent for a text editor (and in sessions from
    /// before other kinds, where an image's tab comes back as an image).
    #[serde(skip_serializing_if = "TabKind::is_editor")]
    pub kind: TabKind,
    /// None for an untitled buffer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The caret, as a byte offset.
    pub cursor: usize,
    /// The line at the top of the view; absent, the caret is centered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_line: Option<usize>,
    /// The whole text, when the buffer had unsaved edits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsaved: Option<String>,
    /// Which pane the tab is in.
    #[serde(skip_serializing_if = "is_zero")]
    pub pane: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// Folded regions as (header row, last hidden row).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub folds: Vec<[usize; 2]>,
    /// A file's encoding when it isn't UTF-8 (a chosen one isn't guessed again).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

/// The kinds of pane items a session brings back (see `pane_item`).
#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TabKind {
    #[default]
    Editor,
    Image,
    /// The Markdown preview of the file at `path`, which a text tab has open.
    MarkdownPreview,
}

impl TabKind {
    fn is_editor(&self) -> bool {
        *self == TabKind::Editor
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ProjectSession {
    /// The split panes: the tree, else an older session's row or column.
    pub fn pane_layout(&self) -> Option<PaneNode> {
        self.panes.clone().or_else(|| self.layout.as_ref().filter(|l| l.sizes.len() > 1).map(PaneLayout::to_tree))
    }

    /// `sessions/<folder name>-<hash of the full path>.json`: readable, and
    /// unique for same-named folders.
    fn file_name(root: &Path) -> String {
        let name: String = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .take(40)
            .collect();
        format!("{name}-{:016x}.json", fnv1a(root.to_string_lossy().as_bytes()))
    }

    fn path(root: &Path) -> Option<PathBuf> {
        config_dir().map(|d| d.join("sessions").join(Self::file_name(root)))
    }

    /// The project's last session. It may hold the only copy of unsaved
    /// text, so it's read leniently (a part another Kuana version wrote
    /// differently is skipped, not the rest), and a file that doesn't read
    /// at all is kept aside and reported instead of being overwritten by
    /// the next save.
    pub fn load(root: &Path) -> Option<Self> {
        let path = Self::path(root)?;
        let text = std::fs::read_to_string(&path).ok()?;
        match read_lenient::<Self>(&text) {
            Ok(session) => {
                if !session.other.is_empty()
                    && let Ok(mut kept) = KEPT_KEYS.lock()
                {
                    kept.insert(root.to_path_buf(), session.other.clone());
                }
                Some(session).filter(|s| s.root == root)
            }
            Err(err) => {
                set_aside(&path, &format!("The session of {}", root.display()), err);
                None
            }
        }
    }

    /// Written whole or not at all (see `write_atomic`), so a crash
    /// mid-write can't lose the unsaved text in it.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path(&self.root).ok_or_else(|| std::io::Error::other("no config directory"))?;
        let mut value = serde_json::to_value(self)?;
        // Keys a newer Kuana wrote, read with the session, go back with it.
        if let (Some(map), Some(kept)) = (value.as_object_mut(), KEPT_KEYS.lock().ok().and_then(|k| k.get(&self.root).cloned())) {
            for (key, old) in kept {
                map.entry(key).or_insert(old);
            }
        }
        write_atomic(&path, (serde_json::to_string_pretty(&value)? + "\n").as_bytes())
    }
}

/// Per project, the session keys this version doesn't know (see
/// `ProjectSession::other`), kept from its load for its saves.
static KEPT_KEYS: Mutex<std::collections::BTreeMap<PathBuf, serde_json::Map<String, serde_json::Value>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// FNV-1a: a hash that, unlike std's, is stable across Rust versions.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::DebugSpec;

    #[test]
    fn session_file_names_are_stable_and_distinct() {
        let a = ProjectSession::file_name(Path::new("/work/kuana"));
        assert_eq!(a, ProjectSession::file_name(Path::new("/work/kuana")));
        assert!(a.starts_with("kuana-") && a.ends_with(".json"), "{a}");
        assert_ne!(a, ProjectSession::file_name(Path::new("/other/kuana")));
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
    }

    #[test]
    fn sessions_round_trip() {
        let session = ProjectSession {
            root: "/work/kuana".into(),
            tabs: vec![
                TabSession { kind: TabKind::Editor, path: Some("/work/kuana/a.rs".into()), cursor: 12, top_line: Some(40), unsaved: None, pane: 0, pinned: true, folds: vec![[3, 9]], encoding: Some("windows-1254".into()) },
                TabSession { kind: TabKind::Editor, path: None, cursor: 0, top_line: None, unsaved: Some("draft".into()), pane: 1, pinned: false, folds: Vec::new(), encoding: None },
                TabSession { kind: TabKind::MarkdownPreview, path: Some("/work/kuana/README.md".into()), pane: 1, ..TabSession::default() },
                TabSession { kind: TabKind::Image, path: Some("/work/kuana/logo.png".into()), pane: 3, ..TabSession::default() },
            ],
            active: 1,
            layout: None,
            // A 2x2 grid: two columns side by side.
            panes: Some(PaneNode::Row(vec![
                PaneShare {
                    share: 0.4,
                    node: PaneNode::Column(vec![
                        PaneShare { share: 0.5, node: PaneNode::Pane { active: 0 } },
                        PaneShare { share: 0.5, node: PaneNode::Pane { active: 1 } },
                    ]),
                },
                PaneShare {
                    share: 0.6,
                    node: PaneNode::Column(vec![
                        PaneShare { share: 0.7, node: PaneNode::Pane { active: 0 } },
                        PaneShare { share: 0.3, node: PaneNode::Pane { active: 0 } },
                    ]),
                },
            ])),
            expanded: vec!["src".into(), "src/lsp".into()],
            python_venv: Some("off".into()),
            terminals: Some(TerminalLayout {
                open: true,
                groups: vec![
                    TerminalGroupSession { tabs: vec![TerminalTabSession { name: Some("server".into()), cwd: Some("/work/kuana/web".into()) }], active: 0 },
                    TerminalGroupSession { tabs: vec![TerminalTabSession::default(), TerminalTabSession::default()], active: 1 },
                ],
                active: 1,
            }),
            breakpoints: vec![FileBreakpoints {
                path: "src/a.rs".into(),
                breakpoints: vec![Breakpoint::at(3), Breakpoint { condition: Some("i > 2".into()), ..Breakpoint::at(9) }],
            }],
            last_run: Some(RunTarget {
                name: "Tests in 'tests'".into(),
                command: "cargo test --bin kuana -- tests".into(),
                cwd: "/work/kuana".into(),
                row: None,
                rows: 0..0,
                debug: Some(DebugSpec::Cargo { build: vec!["test".into(), "--no-run".into()], args: vec!["tests".into()] }),
                test: None,
            }),
            other: Default::default(),
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(serde_json::from_str::<ProjectSession>(&json).unwrap(), session);
        // Text tabs don't name their kind, as before there were others.
        assert!(json.contains(r#""kind":"markdown_preview""#) && !json.contains(r#""kind":"editor""#));
        // A member names its kind beside its share.
        assert!(json.contains(r#""panes":{"row":[{"share":0.4,"column":[{"share":0.5,"pane":{"active":0}}"#), "{json}");
        assert_eq!(session.pane_layout().map(|p| p.pane_count()), Some(4));

        // Every way to debug comes back.
        let go = RunTarget {
            name: "Test 'TestA'".into(),
            command: "go test -run '^TestA$' ./pkg".into(),
            cwd: "/work/kuana".into(),
            row: None,
            rows: 0..0,
            debug: Some(DebugSpec::Go { mode: "test".into(), program: "/work/kuana/pkg".into(), args: vec!["-test.run".into()] }),
            test: None,
        };
        let json = serde_json::to_string(&go).unwrap();
        assert_eq!(serde_json::from_str::<RunTarget>(&json).unwrap(), go);
    }

    /// Older sessions kept one row or column of panes as `layout`; it reads
    /// as a tree, and a single pane needs none.
    #[test]
    fn an_older_row_of_panes_reads_as_a_tree() {
        let text = r#"{
            "root": "/work/p",
            "tabs": [{ "path": "/work/p/a.rs", "cursor": 0 }, { "path": "/work/p/b.rs", "cursor": 0, "pane": 1 }],
            "active": 1,
            "layout": { "column": true, "sizes": [0.3, 0.7], "active": [0, 0] }
        }"#;
        let session: ProjectSession = read_lenient(text).unwrap();
        let pane = |share, active| PaneShare { share, node: PaneNode::Pane { active } };
        assert_eq!(session.pane_layout(), Some(PaneNode::Column(vec![pane(0.3, 0), pane(0.7, 0)])));
        // Written again, it's a tree.
        let json = serde_json::to_string(&ProjectSession { layout: None, panes: session.pane_layout(), ..session }).unwrap();
        assert!(json.contains(r#""panes":{"column":"#) && !json.contains("layout"), "{json}");
        let single = ProjectSession { layout: Some(PaneLayout { column: false, sizes: vec![1.], active: vec![0] }), ..ProjectSession::default() };
        assert_eq!(single.pane_layout(), None);
    }

    /// A session written by another Kuana version still brings back its
    /// unsaved text: a way to debug this one doesn't know, a tab of a newer
    /// shape and a new key cost only themselves.
    #[test]
    fn sessions_from_other_versions_keep_their_unsaved_text() {
        let text = r#"{
            "root": "/work/p",
            "tabs": [
                { "path": "/work/p/a.rs", "cursor": 3, "unsaved": "draft" },
                { "path": 7 },
                { "cursor": 0, "unsaved": "untitled" }
            ],
            "active": 1,
            "expanded": "not a list",
            "last_run": { "name": "x", "command": "x", "cwd": "/", "debug": { "Wasm": { "module": "m" } } },
            "panes": { "grid": [[{ "pane": { "active": 0 } }]] },
            "minimap": { "on": true }
        }"#;
        let session: ProjectSession = read_lenient(text).unwrap();
        let unsaved: Vec<_> = session.tabs.iter().map(|t| t.unsaved.as_deref()).collect();
        assert_eq!(unsaved, [Some("draft"), Some("untitled")]);
        assert!(session.expanded.is_empty() && session.last_run.is_none());
        // A way to split panes this version doesn't know: the panes are lost, not the tabs.
        assert_eq!(session.pane_layout(), None);
        assert_eq!(session.other.get("minimap"), Some(&serde_json::json!({ "on": true })));
        assert!(read_lenient::<ProjectSession>("{ truncated").is_err());
        // State with one bad value keeps the rest.
        let state: SessionState = read_lenient(r#"{ "sidebar_width": "wide", "welcome_done": true }"#).unwrap();
        assert_eq!((state.sidebar_width, state.welcome_done), (None, Some(true)));
    }

    /// Changes made to be written later are seen by every read at once,
    /// written together, and kept by writes made meanwhile; writers on
    /// several threads don't drop each other's changes.
    #[test]
    fn changes_wait_to_be_written_and_writers_keep_each_others() {
        let dir = std::env::temp_dir().join(format!("kuana-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = Arc::new(StateFile::at(Some(dir.join("state.json"))));
        let on_disk = || serde_json::from_str::<SessionState>(&std::fs::read_to_string(dir.join("state.json")).unwrap_or_default()).unwrap_or_default();
        file.change_soon(("layout", 1), |s| s.sidebar_width = Some(200.));
        file.change_soon(("layout", 2), |s| s.problems_height = Some(100.));
        // The later change of the same window replaces its earlier one.
        file.change_soon(("layout", 1), |s| s.sidebar_width = Some(300.));
        assert_eq!((file.load().sidebar_width, file.load().problems_height), (Some(300.), Some(100.)));
        assert_eq!(on_disk(), SessionState::default(), "nothing written yet");
        // A write made now writes what waits too, and nothing waits after.
        file.change(|s| s.recent_projects = vec!["/p".into()]).unwrap();
        assert_eq!((on_disk().sidebar_width, on_disk().problems_height, on_disk().recent_projects.len()), (Some(300.), Some(100.), 1));
        assert!(file.pending.lock().unwrap().is_empty());
        file.change_soon(("window", 0), |s| s.welcome_done = Some(true));
        file.write_pending().unwrap();
        assert_eq!((on_disk().welcome_done, on_disk().sidebar_width), (Some(true), Some(300.)));
        file.write_pending().unwrap();

        let writers: Vec<_> = (0..8)
            .map(|i| {
                let file = file.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        let project = PathBuf::from(format!("/{i}/{j}"));
                        if j % 2 == 0 {
                            file.change(|s| s.recent_projects.push(project)).unwrap();
                        } else {
                            file.change_soon(("t", i * 100 + j), move |s| s.trusted_folders.push(project.clone()));
                            file.write_pending().unwrap();
                        }
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let state = on_disk();
        assert_eq!((state.recent_projects.len(), state.trusted_folders.len()), (1 + 40, 40));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_writes_replace_whole_files_and_keep_links() {
        let dir = std::env::temp_dir().join(format!("kuana-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("deep/state.json");
        write_atomic(&file, b"one").unwrap();
        write_atomic(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        // No temporary files left behind.
        assert_eq!(std::fs::read_dir(dir.join("deep")).unwrap().count(), 1);
        #[cfg(unix)]
        {
            let link = dir.join("settings.json");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            write_atomic(&link, b"three").unwrap();
            assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read(&file).unwrap(), b"three");
        }
        // An unreadable file is kept aside, not lost.
        set_aside(&file, "test file", "bad");
        assert!(!file.exists());
        assert!(std::fs::read_dir(dir.join("deep")).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("state.json.corrupt-")));
        assert!(take_problems().iter().any(|p| p.starts_with("test file couldn't be read")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
