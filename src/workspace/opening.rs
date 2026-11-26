//! Opening a file in a tab. What the path is (a folder, a picture, text, a
//! binary file) and the file's text are found off the UI thread, so a big
//! file, a slow disk or a remote machine that takes its time never stops
//! the window; the tab comes once the text is there. Files asked for one
//! after another get their tabs in that order, whichever is read first.
//!
//! So nothing can use a file's tab right after asking for it: what's to
//! happen there (a jump to a line, a selection) is handed over with the
//! request (`open_path_then`, `navigate_to`) and runs once the tab shows.
//! A file that's in a tab already shows at once, as before.

use gpui::WeakEntity;

use super::*;
use crate::nav_history::Place;

/// What runs once a file's tab shows, with its editor (None: a picture's tab).
type Then = Box<dyn FnOnce(&mut Workspace, Option<&Entity<Editor>>, &mut Window, &mut Context<Workspace>)>;

/// What waits for a restored tab's file to be read (`Workspace::when_loaded`).
pub(super) type WhenLoaded = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// How showing the tab counts for Back / Forward.
enum Nav {
    /// A tab switch like any other.
    Plain,
    /// Back / Forward itself: it keeps the history's lists by hand.
    Silent,
    /// A navigation from this place (None: from somewhere without one).
    From(Option<Place>),
}

/// One request to open a file.
struct Opening {
    /// As text even if it looks binary (Open Anyway).
    anyway: bool,
    nav: Nav,
    /// The pane that was current when it was asked for: a new tab goes there.
    pane: WeakEntity<Pane>,
    then: Option<Then>,
}

/// What a path to open turned out to be.
enum Found {
    Folder,
    /// In a tab already when it was asked for (under its real path).
    Open,
    Image,
    Text(Box<Document>),
    /// It looks binary, and wasn't asked for as text anyway.
    Binary,
    Failed(String),
}

/// Looks at `path` and reads it if it's text: everything opening a file
/// asks of the disk (or the remote machine), for a background thread.
/// Returns its real path too. `open`: the files in tabs.
fn find(path: PathBuf, anyway: bool, open: &HashSet<PathBuf>) -> (PathBuf, Found) {
    let path = crate::paths::canonicalize(&path).unwrap_or(path);
    let found = if open.contains(&path) {
        Found::Open
    } else if path.is_dir_on_host() {
        Found::Folder
    } else if crate::preview::is_image(&path) {
        match crate::fs::metadata(&path) {
            Ok(_) => Found::Image,
            Err(err) => Found::Failed(err.to_string()),
        }
    } else {
        match if anyway { Document::open_anyway(&path) } else { Document::open(&path) } {
            Ok(doc) => {
                // What its editor asks for next, read here so it's known by then:
                // the project settings and `.editorconfig` files above it.
                crate::project_settings::layers(&path);
                crate::editorconfig::EditorConfig::for_file(&path);
                Found::Text(Box::new(doc))
            }
            Err(err) if BinaryFile::is(&err) => Found::Binary,
            Err(err) => Found::Failed(err.to_string()),
        }
    };
    (path, found)
}

impl Workspace {
    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.start_opening(path, Nav::Plain, None, window, cx);
    }

    /// Opens a file and runs `then` once its tab shows, with its editor and
    /// the text there (None: a picture's tab). Not run when nothing opens
    /// (a folder, an unreadable file, a binary one left closed).
    pub(super) fn open_path_then(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Option<&Entity<Editor>>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.start_opening(path, Nav::Plain, Some(Box::new(then)), window, cx);
    }

    /// Opens a file as a navigation (see `navigate`): `go` runs in its editor
    /// once the text is there, and the place left can be gone back to.
    pub(super) fn navigate_to(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        go: impl FnOnce(&mut Self, &Entity<Editor>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let from = self.here(cx);
        let then: Then = Box::new(move |ws, editor, window, cx| {
            if let Some(editor) = editor {
                go(ws, editor, window, cx);
            }
        });
        self.start_opening(path, Nav::From(from), Some(then), window, cx);
    }

    /// Opens a file for Back / Forward, which record nothing on the way.
    pub(super) fn open_for_history(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Option<&Entity<Editor>>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.start_opening(path, Nav::Silent, Some(Box::new(then)), window, cx);
    }

    /// Opens a file at a 1-based line and column (from terminal output).
    pub(super) fn open_at(&mut self, path: PathBuf, line: Option<u32>, column: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_to(path, window, cx, move |_, editor, window, cx| {
            let Some(line) = line else { return };
            editor.update(cx, |e, cx| {
                let row = (line as usize).saturating_sub(1).min(e.doc.read(cx).line_count().saturating_sub(1));
                let start = e.doc.read(cx).line_start(row);
                let text = e.doc.read(cx).line_text(row);
                let col = column.map_or(0, |c| c as usize).saturating_sub(1);
                let offset = start + text.char_indices().nth(col).map_or(text.len(), |(i, _)| i);
                e.go_to(offset, window, cx);
            });
            window.focus(&editor.focus_handle(cx));
        });
    }

    /// The tab showing the file at `path` itself (a preview or diff of it
    /// doesn't count).
    fn file_tab(&self, path: &Path, cx: &App) -> Option<PaneItem> {
        self.items(cx).into_iter().find(|t| t.is_file() && t.path(cx).as_deref() == Some(path))
    }

    fn start_opening(&mut self, path: PathBuf, nav: Nav, then: Option<Then>, window: &mut Window, cx: &mut Context<Self>) {
        let opening = Opening { anyway: false, nav, pane: self.active_pane.downgrade(), then };
        self.open_with(path, opening, window, cx);
    }

    fn open_with(&mut self, path: PathBuf, opening: Opening, window: &mut Window, cx: &mut Context<Self>) {
        // In a tab already: shown at once, unless files asked for before it
        // are still being read (it comes after them, as it was asked).
        if self.opens == 0
            && let Some(item) = self.file_tab(&path, cx)
        {
            return self.show_opened(item, false, opening, window, cx);
        }
        let open: HashSet<PathBuf> = self.items(cx).iter().filter(|t| t.is_file()).filter_map(|t| t.path(cx)).collect();
        let anyway = opening.anyway;
        // Reading starts now; its tab waits for those of the files asked for before.
        let find = cx.background_executor().spawn(async move { find(path, anyway, &open) });
        let before = self.opening.take();
        self.opens += 1;
        let task = cx.spawn_in(window, async move |this, cx| {
            let (path, found) = find.await;
            if let Some(before) = before {
                before.await;
            }
            this.update_in(cx, |ws, window, cx| {
                ws.opens -= 1;
                ws.opened(path, found, opening, window, cx);
            })
            .ok();
        });
        self.opening = Some(futures::FutureExt::shared(task));
    }

    /// A file asked for was looked at (and read, if it's text): its tab.
    fn opened(&mut self, path: PathBuf, found: Found, opening: Opening, window: &mut Window, cx: &mut Context<Self>) {
        let found = match found {
            Found::Folder => return self.set_root(path, window, cx),
            Found::Binary => return self.ask_to_open_binary(path, opening, window, cx),
            Found::Failed(detail) => {
                let message = format!("Couldn't open {}", path.display());
                let _ = window.prompt(PromptLevel::Critical, &message, Some(&detail), &["OK"], cx);
                return;
            }
            found => found,
        };
        // In a tab by now (asked for twice, or by another name of the file).
        if let Some(item) = self.file_tab(&path, cx) {
            return self.show_opened(item, false, opening, window, cx);
        }
        // Open without a tab (a workspace edit changed it): that document,
        // edits and undo history included.
        let kept = self.documents.read(cx).get(&path, cx);
        let item = match (kept, found) {
            (Some(doc), _) => PaneItem::from(cx.new(|cx| Editor::with_document(doc, cx))),
            (None, Found::Image) => PaneItem::from(cx.new(|cx| ImageView::new(path.clone(), cx))),
            (None, Found::Text(doc)) => {
                if doc.large {
                    let detail = "Highlighting, linting and language servers are off for files this big.";
                    self.toast(ToastKind::Info, "Opened as plain text", Some(detail.into()), cx);
                }
                PaneItem::from(cx.new(|cx| Editor::new(*doc, cx)))
            }
            // Its tab closed while the path was looked at: read it after all.
            (None, _) => return self.open_with(path, opening, window, cx),
        };
        self.show_opened(item, true, opening, window, cx);
    }

    /// Shows a file's tab (`new`: adds it first) and runs what waited for it.
    fn show_opened(&mut self, item: PaneItem, new: bool, opening: Opening, window: &mut Window, cx: &mut Context<Self>) {
        let Opening { nav, pane, then, .. } = opening;
        let quiet = !matches!(nav, Nav::Plain);
        if quiet {
            self.nav.pause();
        }
        if new {
            if let Some(pane) = pane.upgrade().filter(|pane| self.center.panes().contains(pane)) {
                self.active_pane = pane;
            }
            self.add_tab(item.clone(), window, cx);
        } else {
            self.activate_item(&item, window, cx);
        }
        if quiet {
            self.nav.resume();
        }
        // A restored tab shown for the first time: its file is being read
        // (`load_tab`), and the rest happens once the text is there.
        if let Some(editor) = item.editor().filter(|editor| !editor.read(cx).is_loaded()) {
            let id = editor.entity_id();
            let rest: WhenLoaded = Box::new(move |ws, window, cx| {
                // Still the tab to show, whatever was clicked meanwhile.
                if quiet {
                    ws.nav.pause();
                }
                ws.activate_item(&item, window, cx);
                if quiet {
                    ws.nav.resume();
                }
                ws.arrived(item, nav, then, window, cx);
            });
            self.when_loaded.entry(id).or_default().push(rest);
            return;
        }
        self.arrived(item, nav, then, window, cx);
    }

    /// A file's tab shows with its text: what was to happen there, and the
    /// navigation it was.
    fn arrived(&mut self, item: PaneItem, nav: Nav, then: Option<Then>, window: &mut Window, cx: &mut Context<Self>) {
        let quiet = !matches!(nav, Nav::Plain);
        if quiet {
            self.nav.pause();
        }
        if let Some(then) = then {
            then(self, item.editor(), window, cx);
        }
        if quiet {
            self.nav.resume();
        }
        // Inside another navigation, that one records.
        if let Nav::From(Some(from)) = nav
            && !self.nav.is_paused()
        {
            let to = self.here(cx);
            self.nav.record(from, to.as_ref());
        }
    }

    /// A file that looks binary opens as text only when asked to (Open
    /// Anyway), as in VS Code.
    fn ask_to_open_binary(&mut self, path: PathBuf, mut opening: Opening, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let message = format!("“{name}” looks like a binary file");
        let detail = "As text it may not show correctly, and saving it can damage it.";
        let answer = window.prompt(PromptLevel::Warning, &message, Some(detail), &["Open Anyway", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                opening.anyway = true;
                this.update_in(cx, |ws, window, cx| ws.open_with(path, opening, window, cx)).ok();
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-opening-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::paths::canonicalize(&dir).unwrap()
    }

    #[test]
    fn finds_what_a_path_is_and_reads_text() {
        let dir = temp_dir("find");
        let none = HashSet::new();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let (path, found) = find(dir.join("a.txt"), false, &none);
        assert_eq!(path, dir.join("a.txt"));
        match found {
            Found::Text(doc) => {
                assert_eq!(doc.rope().to_string(), "one\ntwo\n");
                assert_eq!(doc.path.as_deref(), Some(dir.join("a.txt").as_path()));
            }
            _ => panic!("a text file is read"),
        }
        assert!(matches!(find(dir.clone(), false, &none).1, Found::Folder));
        assert!(matches!(find(dir.join("missing.txt"), false, &none).1, Found::Failed(_)));
        // A picture is only checked to be there.
        std::fs::write(dir.join("p.png"), [0x89, b'P', b'N', b'G']).unwrap();
        assert!(matches!(find(dir.join("p.png"), false, &none).1, Found::Image));
        assert!(matches!(find(dir.join("gone.png"), false, &none).1, Found::Failed(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_binary_file_is_read_only_when_asked_to() {
        let dir = temp_dir("binary");
        let none = HashSet::new();
        std::fs::write(dir.join("b.bin"), [0u8, 159, 146, 150, 0, 1, 2, 3]).unwrap();
        assert!(matches!(find(dir.join("b.bin"), false, &none).1, Found::Binary));
        assert!(matches!(find(dir.join("b.bin"), true, &none).1, Found::Text(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_file_in_a_tab_under_its_real_path_is_not_read_again() {
        let dir = temp_dir("open");
        std::fs::write(dir.join("real.txt"), "text\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();
        let open: HashSet<PathBuf> = [dir.join("real.txt")].into();
        let (path, found) = find(dir.join("link.txt"), false, &open);
        assert_eq!(path, dir.join("real.txt"));
        assert!(matches!(found, Found::Open));
        std::fs::remove_dir_all(&dir).ok();
    }
}
