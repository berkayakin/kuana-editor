//! A real `Workspace` in a test window, for tests of what the window does
//! with files: opening, saving, closing, hot exit.
//!
//! Everything runs on GPUI's test executor: background reads and writes run
//! when the test lets the executor run (`settle`), timers fire only when the
//! test moves the clock (`wait`), and native prompts wait for the test to
//! answer them (`answer`). So a test is deterministic as long as it settles
//! after each step, and leaves no prompt unanswered it didn't mean to.
//!
//! What the harness keeps out: the user's configuration (`theme::config_dir`
//! is a folder of the test run's under `cfg(test)`), Git (`git_store::NO_GIT`:
//! the projects aren't repositories, and looking would ask the login shell
//! for its PATH), the file watcher (`watching::NO_WATCHER`: its events come
//! from another thread at any time; a test that changes a file on disk
//! makes the change it means with `disk_changed`), language servers
//! (`lsp::NO_SERVERS`: no document connects to one, whatever its language)
//! and the Dock's recent items.
//!
//! The window as a test finds it: `open_project` trusts the project first
//! (`trust::trust_in_this_test`; `init` gives the test a trusted list of its
//! own, since the process's is changed by the tests running beside it), so
//! no trust dialog covers the workspace and the project isn't in Restricted
//! Mode; a test of Restricted Mode opens it with `open_project_restricted`.
//! No modal is open, the current tab has the keyboard once it's shown, and
//! settings are the defaults: auto-save after a delay (see `set_auto_save`).
//! The test window never counts as the active one, so what Kuana does when
//! its window gains or loses focus doesn't happen.

use gpui::{EntityInputHandler, TestAppContext, WindowHandle};

use super::*;

/// A project folder with files, removed when the test ends. Its path is the
/// real one (macOS's temp folder is behind a link), as the workspace keeps it.
pub(super) struct TempProject {
    pub dir: PathBuf,
}

impl TempProject {
    pub fn new(files: &[(&str, &str)]) -> Self {
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kuana-workspace-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = crate::paths::canonicalize(&dir).unwrap();
        let project = Self { dir };
        for (name, text) in files {
            project.write(name, text);
        }
        project
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Writes a file as another program would (folders made as needed).
    pub fn write(&self, name: &str, text: &str) {
        let path = self.path(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    pub fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap()
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Sets up the app as `main` does, as far as a workspace needs: the theme
/// (a blank one; settings are the defaults, as without a settings file),
/// and Git left out. Call once per test, before `workspace`.
pub(super) fn init(cx: &mut TestAppContext) {
    crate::git_store::NO_GIT.with(|off| off.set(true));
    crate::lsp::NO_SERVERS.with(|off| off.set(true));
    crate::trust::trust_only_in_this_test();
    watching::NO_WATCHER.with(|off| off.set(true));
    cx.update(|cx| {
        cx.set_global(Theme::blank_for_tests());
        // As if extensions had loaded (none are installed in the test config).
        cx.set_global(extensions::ExtensionsChanged::default());
    });
}

/// A window with a workspace, as `open_window` makes it.
pub(super) fn workspace(cx: &mut TestAppContext) -> Ws {
    let window = cx.add_window(Workspace::new);
    let ws = Ws(window);
    ws.update(cx, |ws, window, cx| ws.window_ready(window, cx));
    cx.run_until_parked();
    ws
}

/// A test's handle on its workspace window: each method acts as the user
/// would and lets what that started finish (`settle`), unless it says not to.
#[derive(Clone, Copy)]
pub(super) struct Ws(pub WindowHandle<Workspace>);

impl Ws {
    pub fn update<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R) -> R {
        self.0.update(cx, f).expect("the workspace window is open")
    }

    pub fn read<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&Workspace, &App) -> R) -> R {
        self.0.read_with(cx, f).expect("the workspace window is open")
    }

    /// Opens `dir` as the project, as File → Open does with a folder, the
    /// folder trusted already (as after the trust dialog's Trust).
    pub fn open_project(&self, cx: &mut TestAppContext, dir: &Path) {
        crate::trust::trust_in_this_test(dir);
        self.open_project_restricted(cx, dir);
    }

    /// Opens `dir` as the project without trusting it: Restricted Mode,
    /// the trust dialog open over the workspace (unless the folder was
    /// trusted earlier in the test).
    pub fn open_project_restricted(&self, cx: &mut TestAppContext, dir: &Path) {
        let dir = dir.to_path_buf();
        self.update(cx, |ws, window, cx| ws.set_root(dir, window, cx));
        settle(cx);
    }

    /// Opens a file and waits for its tab.
    pub fn open(&self, cx: &mut TestAppContext, path: &Path) {
        self.open_soon(cx, path);
        settle(cx);
    }

    /// Asks for a file without waiting for its tab.
    pub fn open_soon(&self, cx: &mut TestAppContext, path: &Path) {
        let path = path.to_path_buf();
        self.update(cx, |ws, window, cx| ws.open_path(path, window, cx));
    }

    /// The current tab's file.
    pub fn active_path(&self, cx: &mut TestAppContext) -> Option<PathBuf> {
        self.read(cx, |ws, cx| ws.active_item(cx).and_then(|item| item.path(cx)))
    }

    pub fn active_editor(&self, cx: &mut TestAppContext) -> Entity<Editor> {
        self.read(cx, |ws, cx| ws.active_editor(cx)).expect("a text tab is current")
    }

    /// The current tab's text, as it shows.
    pub fn active_text(&self, cx: &mut TestAppContext) -> String {
        let editor = self.active_editor(cx);
        editor.read_with(cx, |e, cx| e.text(cx))
    }

    /// Whether the current tab has unsaved edits.
    pub fn active_dirty(&self, cx: &mut TestAppContext) -> bool {
        let editor = self.active_editor(cx);
        editor.read_with(cx, |e, cx| e.doc.read(cx).is_dirty())
    }

    /// The current tab's caret as 1-based (line, column).
    pub fn active_caret(&self, cx: &mut TestAppContext) -> (usize, usize) {
        let editor = self.active_editor(cx);
        editor.read_with(cx, |e, cx| e.cursor_position(cx))
    }

    /// Each pane's tabs in order, by file name (a tab without a file: "").
    pub fn tabs(&self, cx: &mut TestAppContext) -> Vec<Vec<String>> {
        self.read(cx, |ws, cx| {
            let name = |item: &PaneItem| item.path(cx).and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
            ws.center.panes().iter().map(|pane| pane.read(cx).tabs().iter().map(name).collect()).collect()
        })
    }

    /// The tab showing `path`'s file.
    pub fn editor_for(&self, cx: &mut TestAppContext, path: &Path) -> Option<Entity<Editor>> {
        self.read(cx, |ws, cx| ws.editors.iter().find(|e| e.read(cx).doc.read(cx).path.as_deref() == Some(path)).cloned())
    }

    /// Types into the current tab where its caret is, as the keyboard does
    /// (through the input handler).
    pub fn type_text(&self, cx: &mut TestAppContext, text: &str) {
        let editor = self.active_editor(cx);
        self.update(cx, |_, window, cx| editor.update(cx, |e, cx| e.replace_text_in_range(None, text, window, cx)));
        settle(cx);
    }

    /// Moves the current tab's caret to a byte offset.
    pub fn put_caret(&self, cx: &mut TestAppContext, offset: usize) {
        let editor = self.active_editor(cx);
        self.update(cx, |_, window, cx| editor.update(cx, |e, cx| e.go_to(offset, window, cx)));
    }

    /// The toasts showing, by message.
    pub fn toasts(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.read(cx, |ws, _| ws.toasts.iter().filter(|t| !t.leaving).map(|t| t.message.to_string()).collect())
    }

    /// Auto-save as the `auto_save` setting would set it. It's on (after a
    /// delay) by default, and then closing a tab saves it rather than asking.
    pub fn set_auto_save(&self, cx: &mut TestAppContext, auto_save: AutoSave) {
        self.update(cx, |ws, _, _| ws.auto_save = auto_save);
    }

    /// Runs an action handler on the workspace, as its shortcut would.
    pub fn act(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)) {
        self.update(cx, f);
        settle(cx);
    }
}

/// Lets everything that's ready run: background reads and writes, and what
/// waits for them. Timers don't fire (see `wait`).
pub(super) fn settle(cx: &mut TestAppContext) {
    cx.run_until_parked();
}

/// Moves the clock on by `ms` and settles: debounced work (the session's
/// save 800 ms after a change) runs.
pub(super) fn wait(cx: &mut TestAppContext, ms: u64) {
    cx.executor().advance_clock(Duration::from_millis(ms));
    cx.run_until_parked();
}

/// The native prompt showing: its message and detail.
pub(super) fn prompt(cx: &mut TestAppContext) -> Option<(String, String)> {
    cx.pending_prompt()
}

/// Clicks `button` in the native prompt showing, and settles.
#[track_caller]
pub(super) fn answer(cx: &mut TestAppContext, button: &str) {
    cx.simulate_prompt_answer(button);
    settle(cx);
}

/// Makes a file's modification time differ from what an open document
/// knows, as a write by another program does. Filesystems with coarse
/// timestamps (and fast tests) can give two writes the same time, so the
/// time is set explicitly.
pub(super) fn disk_changed(path: &Path, text: &str) {
    let before = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    std::fs::write(path, text).unwrap();
    let later = before.unwrap_or_else(std::time::SystemTime::now) + Duration::from_secs(2);
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(later).unwrap();
}
