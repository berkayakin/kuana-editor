//! The project's file tree (the sidebar's Files view): which folders are
//! open, the rows they make, drag & drop between them and from Finder. File
//! operations are the workspace's: the tree reports what was asked
//! (`TreeEvent`) and the workspace tells it what changed (expand, remap,
//! reveal, paths created or removed) and how rows look (Git changes, the
//! open file, a menu's target). Folders are read in the background and
//! kept: rows are made from what was read, and a change reads again only
//! the folders it touched.
//!
//! A click on a file opens it and leaves the keyboard in the tree (typing
//! searches it); a double click or Enter moves to its editor. A click on a
//! folder opens or closes it. ↑/↓ move, ←/→ close and open folders. Typing a
//! name, or Find, searches the tree as JetBrains' speed search does: the tree
//! stays as it is, the matching part of each match's name is marked, and the
//! first match is selected, the folders down to it opened (closed folders are
//! searched too, ignored ones once the tree has read them). ↑/↓ go from match
//! to match, Enter opens one, Escape (or leaving the tree) ends the search;
//! Escape without one goes back to the editor, as in JetBrains.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    AnyWindowHandle, Bounds, Context, DragMoveEvent, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    FontWeight, HighlightStyle, Hsla, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollStrategy, SharedString, StyledText, Subscription,
    Task, UTF16Selection, UniformListScrollHandle, Window, canvas, div, prelude::*, px, uniform_list,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveLeft, MoveRight, MoveUp, Newline},
    git::Change,
    git_panel::change_color,
    theme::Theme,
    ui::{DragLabel, HoverFill, file_icon, folder_icon, icon_el},
    workspace::{Find, RedoFileChange, UndoFileChange},
};

const ROW_HEIGHT: f32 = 26.0;

#[derive(Clone)]
struct TreeEntry {
    path: PathBuf,
    name: SharedString,
    depth: usize,
    is_dir: bool,
}

/// An entry of a folder, as read.
struct Child {
    path: PathBuf,
    name: SharedString,
    is_dir: bool,
    /// A link to a folder it's in: it would expand forever, so it doesn't.
    cycle: bool,
}

/// A tree row being dragged.
#[derive(Clone)]
pub struct DraggedEntry {
    pub path: PathBuf,
    pub name: SharedString,
}

pub enum TreeEvent {
    /// A file to open; `focus`: in its editor (a double click, Enter), else
    /// the tree keeps the keyboard (a click).
    Open { path: PathBuf, focus: bool },
    /// A row (or empty space: the project folder) was right-clicked.
    Menu { path: PathBuf, is_dir: bool, position: Point<Pixels> },
    /// A row was dropped into a folder.
    Move { from: PathBuf, into: PathBuf },
    /// Files from Finder were dropped into a folder.
    CopyIn { paths: Vec<PathBuf>, into: PathBuf },
    /// ⌘Z / ⇧⌘Z with the tree focused: undo or redo a file operation.
    History { redo: bool },
    /// Escape with no search to end: the keyboard goes back to the editor.
    Leave,
    /// A folder opened or closed (the session keeps which).
    Toggled,
    /// A drag over the tree started or stopped targeting the project folder
    /// (the Files tab lights up with the tree).
    RootDrop(bool),
}

impl EventEmitter<TreeEvent> for ProjectTree {}

/// Typing in the tree searches it, as JetBrains' speed search does: the
/// query field over the tree, and the matches, marked in their rows.
struct Search {
    input: Entity<Editor>,
    /// The query the matches are for.
    query: String,
    /// The matches in tree order, from the whole project; none until the
    /// first are found.
    found: Option<Vec<PathBuf>>,
    /// The bytes of each match's name that match.
    marks: HashMap<PathBuf, Vec<Range<usize>>>,
    /// The folders the search opened to show its match; showing another
    /// closes them again.
    opened: HashSet<PathBuf>,
    /// Searching in the background, for the query as it is; a newer query
    /// drops it.
    task: Option<Task<()>>,
    /// The project's files changed while it searched: it searches again
    /// when it's done.
    stale: bool,
    /// Enter came before the query's matches: the first opens when they do.
    open_first: bool,
    /// The window, for what comes when the matches do.
    window: AnyWindowHandle,
    _subscriptions: [Subscription; 2],
}

impl Search {
    /// Lets go of the matches off the UI thread: a broad query's in a big
    /// project are tens of thousands of paths, milliseconds to free.
    fn free_matches(&mut self, cx: &gpui::App) {
        let matches = (self.found.take(), std::mem::take(&mut self.marks));
        cx.background_executor().spawn(async move { drop(matches) }).detach();
    }
}

pub struct ProjectTree {
    root: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    /// The folders read so far, their entries in order (folders first).
    /// A closed one is kept too, until something in it changes.
    listings: HashMap<PathBuf, Arc<[Child]>>,
    /// Folders to read (again) once the running read is done.
    unread: HashSet<PathBuf>,
    /// Folders the running read is reading.
    reading: HashSet<PathBuf>,
    read_task: Option<Task<()>>,
    /// A row to scroll to once it's listed (`reveal_and_scroll`).
    scroll_to: Option<PathBuf>,
    entries: Vec<TreeEntry>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// While dragging over the tree: the folder a drop would go into.
    drop_target: Option<PathBuf>,
    /// Change per file (and "contains changes" per folder).
    git: Arc<HashMap<PathBuf, Change>>,
    /// What Git ignores: files, and folders with everything in them. Dimmed.
    ignored: Arc<HashSet<PathBuf>>,
    /// The open file, marked.
    active: Option<PathBuf>,
    /// The row a context menu is open for, outlined.
    menu_target: Option<PathBuf>,
    /// The project's files (Go to File's), so a search finds what's in
    /// folders the tree hasn't read.
    files: Arc<Vec<PathBuf>>,
    /// The entry the keyboard is on, filled while the tree has focus; kept
    /// by path, as rows are made again.
    selected: Option<PathBuf>,
    search: Option<Search>,
}

impl ProjectTree {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            root: None,
            expanded: HashSet::new(),
            listings: HashMap::new(),
            unread: HashSet::new(),
            reading: HashSet::new(),
            read_task: None,
            scroll_to: None,
            entries: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            drop_target: None,
            git: Arc::default(),
            ignored: Arc::default(),
            active: None,
            menu_target: None,
            files: Arc::default(),
            selected: None,
            search: None,
        }
    }

    /// A new project, with the folders that were open in it last time.
    pub fn set_root(&mut self, root: Option<PathBuf>, expanded: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        self.expanded = root.iter().cloned().chain(expanded).collect();
        self.root = root;
        self.listings.clear();
        self.unread.clear();
        self.reading.clear();
        self.read_task = None;
        self.scroll_to = None;
        self.files = Arc::default();
        self.selected = None;
        self.search = None;
        self.rebuild(cx);
    }

    /// The project's files, as Go to File lists them: a search looks
    /// through them too, again if they changed (the same ones come again
    /// when a file is written on Windows). One under way finishes first: a
    /// busy project would keep starting it over.
    pub fn set_files(&mut self, files: Arc<Vec<PathBuf>>, cx: &mut Context<Self>) {
        let changed = self.search.is_some() && *files != *self.files;
        self.files = files;
        let Some(search) = self.search.as_mut().filter(|_| changed) else { return };
        if search.task.is_some() {
            search.stale = true;
        } else {
            self.research(cx);
        }
    }

    /// The open folders.
    pub fn expanded(&self) -> &HashSet<PathBuf> {
        &self.expanded
    }

    /// Whether the tree has read `dir` (or is reading it): a change in it
    /// is for `paths_changed`.
    fn has_read(&self, dir: &Path) -> bool {
        self.listings.contains_key(dir) || self.reading.contains(dir)
    }

    /// The folders the tree has read (or is reading), for sorting out
    /// changes on disk elsewhere: a change in them is for `paths_changed`.
    pub fn read_folders(&self) -> HashSet<PathBuf> {
        self.listings.keys().chain(&self.reading).cloned().collect()
    }

    /// Makes the rows again from the folders read; open folders not read
    /// yet are read in the background, their rows coming then.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        if let Some(root) = self.root.clone() {
            self.push_children(&root, 0);
        }
        if let Some(target) = self.scroll_to.clone() {
            if let Some(ix) = self.entries.iter().position(|e| e.path == target) {
                self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Center);
                self.scroll_to = None;
            } else if self.unread.is_empty() && self.read_task.is_none() {
                // Everything open is listed, and it isn't there.
                self.scroll_to = None;
            }
        }
        self.read_unread(cx);
        cx.notify();
    }

    fn push_children(&mut self, dir: &Path, depth: usize) {
        let Some(listing) = self.listings.get(dir).cloned() else {
            if !self.reading.contains(dir) {
                self.unread.insert(dir.to_path_buf());
            }
            return;
        };
        for child in listing.iter() {
            self.entries.push(TreeEntry { path: child.path.clone(), name: child.name.clone(), depth, is_dir: child.is_dir });
            if child.is_dir && !child.cycle && self.expanded.contains(&child.path) {
                self.push_children(&child.path, depth + 1);
            }
        }
    }

    /// Reads the folders waiting for it in the background, then makes the
    /// rows again. One read at a time: folders asked for meanwhile wait.
    fn read_unread(&mut self, cx: &mut Context<Self>) {
        if self.read_task.is_some() || self.unread.is_empty() {
            return;
        }
        let dirs: Vec<PathBuf> = self.unread.drain().collect();
        self.reading = dirs.iter().cloned().collect();
        self.read_task = Some(cx.spawn(async move |tree, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { dirs.into_iter().map(|dir| (read_listing(&dir), dir)).collect::<Vec<_>>() })
                .await;
            tree.update(cx, |tree, cx| {
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = tree.read_task.take() {
                    task.detach();
                }
                tree.reading.clear();
                // One that changed while it was read is in `unread` again.
                tree.listings.extend(read.into_iter().map(|(listing, dir)| (dir, listing)));
                tree.rebuild(cx);
            })
            .ok();
        }));
    }

    /// Paths created, removed or renamed (on disk, or by a file operation):
    /// the folders they're in are read again, in the background, if they're
    /// open, else forgotten until they are; so is a folder that went or
    /// came back, with the folders in it. The rows are made again (folders
    /// opened or forgotten meanwhile), and again when the reads are done.
    pub fn paths_changed<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>, cx: &mut Context<Self>) {
        let mut touched = HashSet::new();
        for path in paths {
            if let Some(parent) = path.parent() {
                touched.insert(parent.to_path_buf());
            }
            if self.has_read(path) {
                touched.extend(self.listings.keys().chain(&self.reading).filter(|d| d.starts_with(path)).cloned());
            }
        }
        self.relist(touched);
        self.rebuild(cx);
    }

    /// Anything may have changed (the watcher lost track of the disk, or
    /// there's none): every folder read is read again or forgotten.
    pub fn relist_all(&mut self, cx: &mut Context<Self>) {
        let all: Vec<PathBuf> = self.listings.keys().chain(&self.reading).cloned().collect();
        self.relist(all);
        self.rebuild(cx);
    }

    /// Has `dirs` read again if they're open, forgets them if not; a folder
    /// being read is read once more (what it found may be from before).
    fn relist(&mut self, dirs: impl IntoIterator<Item = PathBuf>) {
        for dir in dirs {
            if self.reading.contains(&dir) || (self.listings.contains_key(&dir) && self.expanded.contains(&dir)) {
                self.unread.insert(dir);
            } else {
                self.listings.remove(&dir);
            }
        }
    }

    /// Opens a folder (listed on the next `rebuild`).
    pub fn expand(&mut self, dir: PathBuf) {
        // Not the search's to close again, then.
        if let Some(search) = &mut self.search {
            search.opened.remove(&dir);
        }
        self.expanded.insert(dir);
    }

    /// Closes every folder but the project's.
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        self.expanded.retain(|p| Some(p) == root.as_ref());
        self.rebuild(cx);
    }

    /// `path` is gone: so are its open folders. A selection in it goes to the
    /// row that takes its place: the next one in its folder, else the one
    /// before it.
    pub fn forget(&mut self, path: &Path) {
        self.expanded.retain(|p| !p.starts_with(path));
        self.listings.retain(|p, _| !p.starts_with(path));
        if let Some(search) = &mut self.search {
            search.opened.retain(|p| !p.starts_with(path));
        }
        if self.selected.as_deref().is_some_and(|s| s.starts_with(path)) {
            let at = self.entries.iter().position(|e| e.path.starts_with(path));
            let next = at.and_then(|at| self.entries[at..].iter().find(|e| !e.path.starts_with(path))).filter(|e| e.path.parent() == path.parent());
            let before = at.and_then(|at| at.checked_sub(1)).map(|at| &self.entries[at]);
            self.selected = next.or(before).map(|e| e.path.clone()).or_else(|| path.parent().map(Path::to_path_buf));
        }
    }

    /// Open folders follow a move or rename (read again at their new place),
    /// and so does the selection.
    pub fn remap(&mut self, remap: impl Fn(&Path) -> Option<PathBuf>) {
        self.expanded = self.expanded.iter().map(|p| remap(p).unwrap_or_else(|| p.clone())).collect();
        self.listings.retain(|p, _| remap(p).is_none());
        if let Some(search) = &mut self.search {
            search.opened = search.opened.iter().map(|p| remap(p).unwrap_or_else(|| p.clone())).collect();
        }
        if let Some(moved) = self.selected.as_deref().and_then(&remap) {
            self.selected = Some(moved);
        }
    }

    /// Opens the folders down to `path` (listed on the next `rebuild`).
    pub fn reveal(&mut self, path: &Path) {
        let Some(root) = self.root.clone() else { return };
        for dir in path.ancestors().skip(1) {
            if !dir.starts_with(&root) {
                break;
            }
            self.expand(dir.to_path_buf());
        }
    }

    /// Opens the folders down to `path`, selects it and scrolls its row into
    /// the middle (once its folder is read).
    pub fn reveal_and_scroll(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.reveal(path);
        self.selected = Some(path.to_path_buf());
        self.scroll_to = Some(path.to_path_buf());
        self.rebuild(cx);
    }

    pub fn has_drop_target(&self) -> bool {
        self.drop_target.is_some()
    }

    pub fn set_drop_target(&mut self, target: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.drop_target != target {
            let was_root = self.drop_on_root();
            self.drop_target = target;
            if self.drop_on_root() != was_root {
                cx.emit(TreeEvent::RootDrop(!was_root));
            }
            cx.notify();
        }
    }

    fn drop_on_root(&self) -> bool {
        self.drop_target.is_some() && self.drop_target == self.root
    }

    /// How rows look: Git changes, what Git ignores, the open file, a context
    /// menu's row.
    pub fn decorate(
        &mut self,
        git: Arc<HashMap<PathBuf, Change>>,
        ignored: Arc<HashSet<PathBuf>>,
        active: Option<PathBuf>,
        menu_target: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if Arc::ptr_eq(&self.git, &git) && Arc::ptr_eq(&self.ignored, &ignored) && self.active == active && self.menu_target == menu_target {
            return;
        }
        (self.git, self.ignored, self.active, self.menu_target) = (git, ignored, active, menu_target);
        cx.notify();
    }

    /// Whether Git ignores `path`: itself, or a folder it's in.
    fn is_ignored(&self, path: &Path) -> bool {
        !self.ignored.is_empty() && path.ancestors().any(|p| self.ignored.contains(p))
    }

    /// A row chosen (Enter, a double click, ←/→ on a folder).
    fn click(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix).cloned() else { return };
        self.open_entry(entry.path, entry.is_dir, true, cx);
    }

    /// An entry chosen: it's selected; a folder opens or closes, a file opens
    /// (`focus`: in its editor, else the tree keeps the keyboard).
    fn open_entry(&mut self, path: PathBuf, is_dir: bool, focus: bool, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        // Never open a folder as a file (opening a folder makes it the project).
        if is_dir || path.is_dir() {
            if !self.expanded.remove(&path) {
                self.expanded.insert(path);
            }
            self.rebuild(cx);
            cx.emit(TreeEvent::Toggled);
        } else {
            cx.emit(TreeEvent::Open { path, focus });
            cx.notify();
        }
    }

    // ---- Keyboard and search -------------------------------------------------------

    /// The selected entry's row, or the row of the nearest folder around it
    /// that shows (it's in a closed one).
    fn selected_row(&self) -> Option<usize> {
        self.selected.as_deref()?.ancestors().find_map(|p| self.entries.iter().position(|e| e.path.as_path() == p))
    }

    /// Selects a row, scrolled into view only as far as it takes: one below
    /// the view comes in at its bottom edge, one above at its top, so ↑/↓
    /// scroll a row at a time.
    fn select_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let strategy = match self.selected_row() {
            Some(row) if ix < row => ScrollStrategy::Top,
            Some(_) => ScrollStrategy::Bottom,
            None => ScrollStrategy::Center,
        };
        self.selected = self.entries.get(ix).map(|e| e.path.clone());
        self.scroll.scroll_to_item(ix, strategy);
        cx.notify();
    }

    /// A click on a row selects it: a file opens with the keyboard left in the
    /// tree (to type a search), a double click moves it to the file's editor,
    /// and a folder opens or closes.
    fn clicked(&mut self, ix: usize, count: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.end_search(window, cx);
        let Some(entry) = self.entries.get(ix).cloned() else { return };
        if entry.is_dir && count > 1 {
            // A double click's second click leaves a folder as its first left it.
            self.select_row(ix, cx);
        } else {
            self.open_entry(entry.path, entry.is_dir, count > 1, cx);
        }
    }

    /// ↑/↓: the row above or below; while searching, the match.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(found) = self.search.as_ref().and_then(|s| s.found.as_ref()) {
            let Some(last) = found.len().checked_sub(1) else { return };
            let next = match self.selected.as_ref().and_then(|s| found.iter().position(|p| p == s)) {
                Some(at) => (at as isize + delta).clamp(0, last as isize) as usize,
                None => 0,
            };
            let path = found[next].clone();
            self.show_match(path, cx);
            return;
        }
        let Some(last) = self.entries.len().checked_sub(1) else { return };
        let next = match self.selected_row() {
            Some(row) => (row as isize + delta).clamp(0, last as isize) as usize,
            None => 0,
        };
        self.select_row(next, cx);
    }

    /// Selects a match and shows it: the folders down to it open, and the
    /// ones the search opened for another match close again.
    fn show_match(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (Some(search), Some(root)) = (&mut self.search, &self.root) else { return };
        for dir in search.opened.drain() {
            self.expanded.remove(&dir);
        }
        for dir in path.ancestors().skip(1).take_while(|dir| dir.starts_with(root)) {
            if self.expanded.insert(dir.to_path_buf()) {
                search.opened.insert(dir.to_path_buf());
            }
        }
        self.selected = Some(path.clone());
        self.scroll_to = Some(path);
        self.rebuild(cx);
    }

    /// Starts a search (if there's none) and gives its query field the
    /// keyboard.
    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<Editor>> {
        self.root.as_ref()?;
        let input = match &self.search {
            Some(search) => search.input.clone(),
            None => {
                let input = cx.new(|cx| Editor::single_line("Search files", cx));
                let edited = cx.subscribe(&input, |tree, _, _: &EditorEvent, cx| tree.research(cx));
                // Leaving the tree (for the editor, a menu, a dialog) ends it. That's
                // told while the window draws, when a change can't have it drawn
                // again: it ends right after.
                let left = cx.on_focus_out(&self.focus, window, |_, _, window, cx| cx.defer_in(window, |tree, window, cx| tree.end_search(window, cx)));
                self.search = Some(Search {
                    input: input.clone(),
                    query: String::new(),
                    found: None,
                    marks: HashMap::new(),
                    opened: HashSet::new(),
                    task: None,
                    stale: false,
                    open_first: false,
                    window: window.window_handle(),
                    _subscriptions: [edited, left],
                });
                input
            }
        };
        window.focus(&input.focus_handle(cx));
        cx.notify();
        Some(input)
    }

    /// Ends the search. What it shows stays (the match selected, its folders
    /// open), and the keyboard goes back to the tree if the query field had it.
    fn end_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut search) = self.search.take() else { return };
        search.free_matches(cx);
        if search.input.focus_handle(cx).is_focused(window) {
            window.focus(&self.focus);
        }
        if !search.opened.is_empty() {
            cx.emit(TreeEvent::Toggled);
        }
        cx.notify();
    }

    /// Searches for the query as it is now, in the background (a project
    /// can have many files); the marks shown stay until it's done. Not while
    /// an input method or a dead key composes (a lone accent finds nothing):
    /// what it commits is an edit, searched then.
    fn research(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let Some(search) = self.search.as_mut() else { return };
        if search.input.read(cx).composing() {
            return;
        }
        let query = search.input.read(cx).text(cx);
        search.stale = false;
        if query.trim().is_empty() {
            search.free_matches(cx);
            (search.task, search.open_first) = (None, false);
            search.query.clear();
            cx.notify();
            return;
        }
        let (files, listings, window) = (self.files.clone(), self.listings.clone(), search.window);
        search.task = Some(cx.spawn(async move |tree, cx| {
            let (found, marks) = cx
                .background_executor()
                .spawn({
                    let query = query.clone();
                    async move { find_matches(&root, &files, &listings, &query) }
                })
                .await;
            window.update(cx, |_, window, cx| tree.update(cx, |tree, cx| tree.searched(query, found, marks, window, cx))).ok();
        }));
    }

    /// A search's matches came, marked if they're for the query as it is. A
    /// new query's are shown: the selection if it's one, else the first. Enter
    /// that came before them opens it.
    fn searched(
        &mut self,
        query: String,
        found: Vec<PathBuf>,
        marks: HashMap<PathBuf, Vec<Range<usize>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self.search.as_mut() else { return };
        if search.input.read(cx).text(cx) != query {
            return;
        }
        // This task is running; let it finish instead of dropping it.
        if let Some(task) = search.task.take() {
            task.detach();
        }
        let open = std::mem::take(&mut search.open_first) && !found.is_empty();
        let shown = if search.query != query || open {
            self.selected.clone().filter(|s| marks.contains_key(s)).or_else(|| found.first().cloned())
        } else {
            None
        };
        let again = std::mem::take(&mut search.stale);
        search.query = query;
        search.free_matches(cx);
        (search.found, search.marks) = (Some(found), marks);
        match shown {
            Some(path) => self.show_match(path, cx),
            None => cx.notify(),
        }
        if open {
            self.open_match(window, cx);
        } else if again {
            self.research(cx);
        }
    }

    /// Ends the search and opens its match in its editor (a folder opens or
    /// closes).
    fn open_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.end_search(window, cx);
        // A match's row may not be listed yet: its folders were just opened.
        if let Some(path) = self.selected.clone() {
            let is_dir = self.entries.iter().any(|e| e.path == path && e.is_dir);
            self.open_entry(path, is_dir, true, cx);
        }
    }

    /// The query field that text typed in the tree goes to: the search's, or
    /// a new one's if the text starts one.
    fn search_for(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<Editor>> {
        if self.search.is_none() && !starts_search(text) {
            return None;
        }
        self.open_search(window, cx)
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    /// ←: closes the selected folder, else goes to the folder it's in.
    fn on_left(&mut self, _: &MoveLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.end_search(window, cx);
        let Some(ix) = self.selected_row() else { return };
        let entry = &self.entries[ix];
        if entry.is_dir && self.expanded.contains(&entry.path) {
            self.click(ix, cx);
        } else if let Some(parent) = self.entries[..ix].iter().rposition(|e| e.depth < entry.depth) {
            self.select_row(parent, cx);
        }
    }

    /// →: opens the selected folder, else goes to the first entry in it.
    fn on_right(&mut self, _: &MoveRight, window: &mut Window, cx: &mut Context<Self>) {
        self.end_search(window, cx);
        let Some(ix) = self.selected_row() else { return };
        let entry = &self.entries[ix];
        if entry.is_dir && !self.expanded.contains(&entry.path) {
            self.click(ix, cx);
        } else if self.entries.get(ix + 1).is_some_and(|next| next.depth > entry.depth) {
            self.select_row(ix + 1, cx);
        }
    }

    /// Enter opens the selected file, or opens or closes the folder; a
    /// search ends, opening its match (once the query's matches are in).
    fn on_enter(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            if let Some(ix) = self.selected_row() {
                self.click(ix, cx);
            }
            return;
        };
        if search.task.is_some() && search.query != search.input.read(cx).text(cx) {
            search.open_first = true;
            return;
        }
        self.open_match(window, cx);
    }

    /// Escape ends a search; without one, it goes back to the editor, as in
    /// JetBrains (the editor's Find bar stays either way).
    fn on_escape(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.end_search(window, cx);
        } else {
            cx.emit(TreeEvent::Leave);
        }
    }

    /// Find with the tree focused searches it (as in VS Code and JetBrains).
    fn on_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.open_search(window, cx) {
            input.update(cx, |input, cx| input.select_all_text(cx));
        }
    }
}

/// Typing in the tree comes as the platform's text input, so dead keys,
/// AltGr and input methods compose as in any text field. It goes to the
/// search's query field, which a character opens; what comes before that
/// field has the keyboard goes there too.
impl EntityInputHandler for ProjectTree {
    fn text_for_range(&mut self, range: Range<usize>, adjusted: &mut Option<Range<usize>>, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let input = self.search.as_ref()?.input.clone();
        input.update(cx, |input, cx| input.text_for_range(range, adjusted, window, cx))
    }

    fn selected_text_range(&mut self, ignore_disabled_input: bool, window: &mut Window, cx: &mut Context<Self>) -> Option<UTF16Selection> {
        match self.search.as_ref().map(|search| search.input.clone()) {
            Some(input) => input.update(cx, |input, cx| input.selected_text_range(ignore_disabled_input, window, cx)),
            None => Some(UTF16Selection { range: 0..0, reversed: false }),
        }
    }

    fn marked_text_range(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>> {
        let input = self.search.as_ref()?.input.clone();
        input.update(cx, |input, cx| input.marked_text_range(window, cx))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.search.as_ref().map(|search| search.input.clone()) {
            input.update(cx, |input, cx| input.unmark_text(window, cx));
        }
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.search_for(text, window, cx) {
            input.update(cx, |input, cx| input.replace_text_in_range(range, text, window, cx));
        }
    }

    /// What an input method composes (or a dead key's accent) starts a search
    /// too, the composing going on in the query field.
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(input) = self.search_for(text, window, cx) {
            input.update(cx, |input, cx| input.replace_and_mark_text_in_range(range, text, selected, window, cx));
        }
    }

    fn bounds_for_range(&mut self, range: Range<usize>, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        match self.search.as_ref().map(|search| search.input.clone()) {
            Some(input) => input.update(cx, |input, cx| input.bounds_for_range(range, bounds, window, cx)),
            None => Some(bounds),
        }
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

impl Focusable for ProjectTree {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ProjectTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let drop_on_root = self.drop_on_root();
        // The keyboard's row (in a closed folder, that folder's) is filled while
        // the tree (or its query field) has focus.
        let cursor_row = self.focus.contains_focused(window, cx).then(|| self.selected_row()).flatten();
        let list = uniform_list(
            "file-tree",
            self.entries.len(),
            cx.processor(move |tree, range: std::ops::Range<usize>, _window, cx| {
                let theme = Theme::get(cx);
                range
                    .map(|ix| {
                        let e = &tree.entries[ix];
                        let selected = tree.active.as_deref() == Some(e.path.as_path());
                        let cursor = cursor_row == Some(ix);
                        let change = tree.git.get(&e.path).copied();
                        // Ignored by Git: dimmed, like VS Code.
                        let name_color: Hsla = match change {
                            Some(c) => change_color(theme, c),
                            None if tree.is_ignored(&e.path) => theme.text_dim.into(),
                            None if selected || cursor => theme.text.into(),
                            None => theme.text_muted.into(),
                        };
                        // A search's match: the part of its name that matches.
                        let marks = tree.search.as_ref().and_then(|s| s.marks.get(&e.path));
                        let is_open = e.is_dir && tree.expanded.contains(&e.path);
                        let icon = if e.is_dir { folder_icon(is_open, px(16.), theme) } else { file_icon(&e.path, px(16.), theme) };
                        // Dropping on a file moves into the folder it's in.
                        let drop_dir = if e.is_dir { e.path.clone() } else { e.path.parent().map(Path::to_path_buf).unwrap_or_default() };
                        let dragged = DraggedEntry { path: e.path.clone(), name: e.name.clone() };
                        let is_target = e.is_dir && tree.drop_target.as_deref() == Some(e.path.as_path());
                        div().w_full().px_1().child(
                            div()
                                .id(ix)
                                .relative()
                                .w_full()
                                .flex()
                                .items_center()
                                .h(px(ROW_HEIGHT))
                                .pl(px(8. + e.depth as f32 * 14.))
                                .pr_2()
                                .gap_1p5()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_color(if selected || cursor { theme.text } else { theme.text_muted })
                                // The open file has the accent fill, the keyboard's row the selection's.
                                .when(selected, |el| el.bg(theme.accent_soft))
                                .when(cursor, |el| el.bg(theme.selection))
                                .row_hover(theme.hover)
                                .on_click(cx.listener(move |tree, event: &gpui::ClickEvent, window, cx| tree.clicked(ix, event.click_count(), window, cx)))
                                .on_drag(dragged, |entry, _, _, cx| cx.new(|_| DragLabel(entry.name.clone())))
                                // While dragging, the folder a drop goes into lights up.
                                .when(is_target, |el| el.bg(theme.accent_soft).border_1().border_color(Hsla::from(theme.accent).alpha(0.7)))
                                .on_drag_move(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |tree, ev: &DragMoveEvent<DraggedEntry>, _, cx| {
                                        if ev.bounds.contains(&ev.event.position) {
                                            tree.set_drop_target(Some(dir.clone()), cx);
                                        }
                                    }
                                }))
                                .on_drag_move(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |tree, ev: &DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                                        if ev.bounds.contains(&ev.event.position) {
                                            tree.set_drop_target(Some(dir.clone()), cx);
                                        }
                                    }
                                }))
                                .on_drop(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |tree, dragged: &DraggedEntry, _, cx| {
                                        cx.stop_propagation();
                                        tree.set_drop_target(None, cx);
                                        cx.emit(TreeEvent::Move { from: dragged.path.clone(), into: dir.clone() });
                                    }
                                }))
                                .on_drop(cx.listener(move |tree, paths: &gpui::ExternalPaths, _, cx| {
                                    cx.stop_propagation();
                                    tree.set_drop_target(None, cx);
                                    cx.emit(TreeEvent::CopyIn { paths: paths.paths().to_vec(), into: drop_dir.clone() });
                                }))
                                .when(tree.menu_target.as_deref() == Some(e.path.as_path()), |el| {
                                    el.border_1().border_color(Hsla::from(theme.accent).alpha(0.6))
                                })
                                .on_mouse_down(MouseButton::Right, {
                                    let (path, is_dir) = (e.path.clone(), e.is_dir);
                                    cx.listener(move |_, ev: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(TreeEvent::Menu { path: path.clone(), is_dir, position: ev.position });
                                    })
                                })
                                .child(div().w(px(14.)).flex_shrink_0().when(e.is_dir, |el| {
                                    el.child(icon_el(if is_open { Icon::ChevronDown } else { Icon::ChevronRight }, px(13.), theme.text_dim.into()))
                                }))
                                .child(icon)
                                .child(div().flex_1().min_w_0().truncate().text_color(name_color).map(|el| match marks {
                                    // In the color of the editor's current search match, the name
                                    // kept readable on it (a dim, ignored one most of all) over the
                                    // row's own fill.
                                    Some(marks) => {
                                        let fill: Hsla = if cursor {
                                            theme.selection.into()
                                        } else if selected {
                                            theme.accent_soft.into()
                                        } else {
                                            gpui::transparent_black()
                                        };
                                        let row = Hsla { a: 1., ..theme.background.into() }.blend(fill);
                                        let color = crate::contrast::keep_legible(name_color, row.blend(theme.search_active.into()), row);
                                        let marked =
                                            HighlightStyle { color: Some(color), background_color: Some(theme.search_active.into()), ..Default::default() };
                                        el.child(StyledText::new(e.name.clone()).with_highlights(marks.iter().map(|r| (r.clone(), marked))))
                                    }
                                    None => el.child(e.name.clone()),
                                }))
                                .when_some(change.filter(|_| !e.is_dir), |el, c| {
                                    el.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(11.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(change_color(theme, c))
                                            .child(c.letter()),
                                    )
                                }),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.scroll.clone())
        .size_full()
        // Over the tree but not on a row, a drop goes to the project folder;
        // rows (told later) narrow it to theirs.
        .on_drag_move(cx.listener(|tree, ev: &DragMoveEvent<DraggedEntry>, _, cx| {
            let target = ev.bounds.contains(&ev.event.position).then(|| tree.root.clone()).flatten();
            tree.set_drop_target(target, cx);
        }))
        .on_drag_move(cx.listener(|tree, ev: &DragMoveEvent<gpui::ExternalPaths>, _, cx| {
            let target = ev.bounds.contains(&ev.event.position).then(|| tree.root.clone()).flatten();
            tree.set_drop_target(target, cx);
        }))
        .on_drop(cx.listener(|tree, dragged: &DraggedEntry, _, cx| {
            tree.set_drop_target(None, cx);
            if let Some(root) = tree.root.clone() {
                cx.emit(TreeEvent::Move { from: dragged.path.clone(), into: root });
            }
        }))
        .on_drop(cx.listener(|tree, paths: &gpui::ExternalPaths, _, cx| {
            cx.stop_propagation();
            tree.set_drop_target(None, cx);
            if let Some(root) = tree.root.clone() {
                cx.emit(TreeEvent::CopyIn { paths: paths.paths().to_vec(), into: root });
            }
        }))
        // Puts the first row's text level with the tab labels beside it
        // (GPUI applies a uniform list's own padding twice, so this is 2px).
        .pt(px(1.))
        .pb_2()
        // Right-clicking empty space targets the project root.
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|tree, ev: &MouseDownEvent, _, cx| {
                if let Some(root) = tree.root.clone() {
                    cx.emit(TreeEvent::Menu { path: root, is_dir: true, position: ev.position });
                }
            }),
        );
        let search_bar = self.search.as_ref().map(|search| {
            // Counted as the terminal's Find counts.
            let count = match &search.found {
                Some(found) if found.is_empty() => "No results".to_string(),
                Some(found) => match self.selected.as_ref().and_then(|s| found.iter().position(|p| p == s)) {
                    Some(at) => format!("{}/{}", at + 1, found.len()),
                    None => found.len().to_string(),
                },
                None => String::new(),
            };
            // A header over the tree, as the Commit view's toolbar is over its list.
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1p5()
                .h(px(30.))
                .px_2()
                .mb_1()
                .border_b_1()
                .border_color(theme.border_subtle)
                // A click around the field leaves the keyboard in it.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|tree, _: &MouseDownEvent, window, cx| {
                        if let Some(search) = &tree.search {
                            window.prevent_default();
                            window.focus(&search.input.focus_handle(cx));
                        }
                    }),
                )
                .child(icon_el(Icon::Search, px(14.), theme.text_dim.into()))
                .child(div().flex_1().min_w_0().overflow_hidden().child(search.input.clone()))
                .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(count))
        });
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .key_context("FileTree")
            .on_action(cx.listener(|_, _: &UndoFileChange, _, cx| cx.emit(TreeEvent::History { redo: false })))
            .on_action(cx.listener(|_, _: &RedoFileChange, _, cx| cx.emit(TreeEvent::History { redo: true })))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_find))
            .children(search_bar)
            .child(
                div()
                    .id("file-tree-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // A click below the rows ends a search, as one on a row does.
                    .on_click(cx.listener(|tree, _: &gpui::ClickEvent, window, cx| tree.end_search(window, cx)))
                    .child(list)
                    .child(crate::ui::list_scrollbar("file-tree-scrollbar", &self.scroll, cx)),
            )
            // Typing comes in as the platform's text input (see `EntityInputHandler`).
            .child({
                let (tree, focus) = (cx.entity(), self.focus.clone());
                canvas(|_, _, _| {}, move |bounds, _, window, cx| window.handle_input(&focus, ElementInputHandler::new(bounds, tree), cx))
                    .absolute()
                    .size_full()
            })
            // The project folder itself has no row: the whole sidebar island
            // lights up as an editor pane does, without moving anything: out
            // through the island's inset to just inside its outline (which, at
            // the top, the Files tab covers in the same color).
            .when(drop_on_root, |el| {
                let inset = px(crate::ui::area_inset(&theme));
                el.child(
                    div()
                        .absolute()
                        .top(-inset)
                        .left(-inset)
                        .right(-inset)
                        .bottom(-inset)
                        .rounded(px((crate::ui::area_radius(&theme) - 1.).max(0.)))
                        .bg(crate::ui::drop_lift(&theme).0),
                )
            })
    }
}

/// A folder's entries as the tree lists them (blocking: in the background).
/// One that can't be read lists nothing.
fn read_listing(dir: &Path) -> Arc<[Child]> {
    children(dir).into_iter().map(|(path, is_dir, cycle, name)| Child { path, name: name.into(), is_dir, cycle }).collect()
}

/// A folder's entries, folders first: (path, is a folder, is a link back to
/// a folder it's in, name).
pub fn children(dir: &Path) -> Vec<(PathBuf, bool, bool, String)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut here: Option<Option<PathBuf>> = None;
    let mut children: Vec<(PathBuf, bool, bool, String)> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if hidden(&name) {
                return None;
            }
            let file_type = e.file_type().ok()?;
            let path = e.path();
            // A link to a folder is a folder; one to a folder around it
            // would expand forever, so it doesn't.
            let (is_dir, cycle) = if file_type.is_symlink() {
                let target = std::fs::canonicalize(&path).ok().filter(|t| t.is_dir());
                let here = here.get_or_insert_with(|| std::fs::canonicalize(dir).ok());
                let cycle = target.as_ref().zip(here.as_ref()).is_some_and(|(t, h)| h.starts_with(t));
                (target.is_some(), cycle)
            } else {
                (file_type.is_dir(), false)
            };
            Some((path, is_dir, cycle, name))
        })
        .collect();
    // Each name lowercased once, not at every comparison.
    children.sort_by_cached_key(|(_, is_dir, _, name)| (!is_dir, name.to_lowercase()));
    children
}

/// Entries the tree never shows.
fn hidden(name: &str) -> bool {
    matches!(name, ".git" | ".DS_Store" | "target")
}

/// Whether text typed in the tree starts a search: characters do (what the
/// platform types for a key: shortcuts type nothing), a space, a tab or a
/// newline doesn't.
fn starts_search(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// A search's matches (blocking: in the background): the entries whose name
/// has each word of `query`, in any case, in tree order (folders first, by
/// name, as if every folder were open), and the bytes of each one's name that
/// match. Entries come from the project's `files` (with the folders they're
/// in) and from the folders the tree has read (`listings`: ignored or empty
/// ones too), so closed folders are searched; what the tree hides stays hidden.
fn find_matches(
    root: &Path,
    files: &[PathBuf],
    listings: &HashMap<PathBuf, Arc<[Child]>>,
    query: &str,
) -> (Vec<PathBuf>, HashMap<PathBuf, Vec<Range<usize>>>) {
    let pattern = Pattern::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Substring);
    if pattern.atoms.is_empty() {
        return Default::default();
    }
    // Every entry to look at, and whether it's a folder.
    let mut candidates: HashMap<&Path, bool> = HashMap::new();
    for file in files {
        let Ok(rel) = file.strip_prefix(root) else { continue };
        if rel.components().any(|c| hidden(&c.as_os_str().to_string_lossy())) {
            continue;
        }
        candidates.insert(file, false);
        for dir in file.ancestors().skip(1).take_while(|d| *d != root) {
            // Its folders are in too, then.
            if candidates.insert(dir, true).is_some() {
                break;
            }
        }
    }
    for child in listings.values().flat_map(|listing| listing.iter()) {
        candidates.insert(&child.path, child.is_dir);
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let (mut chars, mut spans, mut indices) = (Vec::new(), Vec::new(), Vec::new());
    let mut marks: HashMap<&Path, Vec<Range<usize>>> = HashMap::new();
    for &path in candidates.keys() {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else { continue };
        let ascii = name.is_ascii();
        let haystack = if ascii {
            Utf32Str::Ascii(name.as_bytes())
        } else {
            name_chars(&name, &mut chars, &mut spans);
            Utf32Str::Unicode(&chars)
        };
        indices.clear();
        if pattern.indices(haystack, &mut matcher, &mut indices).is_some() {
            indices.sort_unstable();
            indices.dedup();
            // The bytes each matching char stands for.
            let bytes = indices.iter().filter_map(|&i| if ascii { Some(i as usize..i as usize + 1) } else { spans.get(i as usize).cloned() });
            marks.insert(path, joined(bytes));
        }
    }
    // Tree order: each folder's entries on the way to a match, sorted as the
    // tree sorts them, depth first.
    let mut children: HashMap<&Path, Vec<(&Path, bool)>> = HashMap::new();
    let mut placed: HashSet<&Path> = HashSet::new();
    for &path in marks.keys() {
        for entry in path.ancestors().take_while(|p| *p != root) {
            // Its folders are placed too, then.
            if !placed.insert(entry) {
                break;
            }
            let Some(parent) = entry.parent() else { break };
            children.entry(parent).or_default().push((entry, candidates.get(entry).copied().unwrap_or(true)));
        }
    }
    for entries in children.values_mut() {
        entries.sort_by_cached_key(|&(path, is_dir)| (!is_dir, path.file_name().map(|n| n.to_string_lossy().to_lowercase())));
    }
    let mut found = Vec::new();
    push_matches(root, &children, &marks, &mut found);
    let marks = marks.into_iter().map(|(path, ranges)| (path.to_path_buf(), ranges)).collect();
    (found, marks)
}

/// The matches among the entries of `dir` and, depth first, under each.
fn push_matches<'a>(
    dir: &Path,
    children: &HashMap<&'a Path, Vec<(&'a Path, bool)>>,
    marks: &HashMap<&'a Path, Vec<Range<usize>>>,
    found: &mut Vec<PathBuf>,
) {
    for &(path, _) in children.get(dir).into_iter().flatten() {
        if marks.contains_key(path) {
            found.push(path.to_path_buf());
        }
        push_matches(path, children, marks, found);
    }
}

/// A name's chars as a search matches them (`chars`), and the bytes of the
/// name each one stands for (`spans`). Char by char (not by grapheme, as
/// `Utf32Str::new` goes, keeping a grapheme's first char only), but a letter
/// written decomposed (`e` and a combining accent, as macOS writes some
/// names) is the one char it composes to, so `é` and `e` both find it.
fn name_chars(name: &str, chars: &mut Vec<char>, spans: &mut Vec<Range<usize>>) {
    chars.clear();
    spans.clear();
    for (at, c) in name.char_indices() {
        let end = at + c.len_utf8();
        if let (Some(last), Some(span)) = (chars.last_mut(), spans.last_mut())
            && let Some(composed) = unicode_normalization::char::compose(*last, c)
        {
            *last = composed;
            span.end = end;
        } else {
            chars.push(c);
            spans.push(at..end);
        }
    }
    // nucleo-matcher 0.3.1 never tries the last place a word can start in a
    // name that isn't ASCII (an off by one): a char no query has makes room.
    chars.push('\0');
}

/// Byte ranges, in order, with the ones that touch joined.
fn joined(ranges: impl IntoIterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let mut joined: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match joined.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => joined.push(range),
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use gpui::{AppContext, Modifiers, TestAppContext, VisualTestContext};

    use super::*;

    #[gpui::test]
    fn folders_are_read_in_the_background_and_again_only_where_changed(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("kuana-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("B")).unwrap();
        for file in ["a/x.txt", "B/y.txt", "Readme.md", "build.rs"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let tree = cx.new(ProjectTree::new);
        let rows = |cx: &mut TestAppContext| {
            tree.read_with(cx, |t, _| t.entries.iter().map(|e| format!("{}{}", "  ".repeat(e.depth), e.name)).collect::<Vec<_>>())
        };
        tree.update(cx, |t, cx| t.set_root(Some(dir.clone()), [dir.join("a")], cx));
        assert!(rows(cx).is_empty(), "read in the background");
        cx.run_until_parked();
        // Folders first, then by name whatever its case.
        assert_eq!(rows(cx), ["a", "  x.txt", "B", "build.rs", "Readme.md"]);

        // A file created in `a`: only `a` is read again.
        let top = tree.read_with(cx, |t, _| t.listings[&dir].clone());
        std::fs::write(dir.join("a/new.txt"), "").unwrap();
        tree.update(cx, |t, cx| t.paths_changed([dir.join("a/new.txt").as_path()], cx));
        cx.run_until_parked();
        assert_eq!(rows(cx), ["a", "  new.txt", "  x.txt", "B", "build.rs", "Readme.md"]);
        tree.read_with(cx, |t, _| assert!(Arc::ptr_eq(&t.listings[&dir], &top), "the project folder wasn't read again"));

        // A closed folder keeps its entries, until something in it changes.
        tree.update(cx, |t, cx| t.click(3, cx));
        cx.run_until_parked();
        assert_eq!(rows(cx)[3..5], ["B", "  y.txt"]);
        tree.update(cx, |t, cx| t.click(3, cx));
        std::fs::write(dir.join("B/z.txt"), "").unwrap();
        tree.update(cx, |t, cx| t.paths_changed([dir.join("B/z.txt").as_path()], cx));
        tree.read_with(cx, |t, _| assert!(!t.listings.contains_key(&dir.join("B"))));
        tree.update(cx, |t, cx| t.click(3, cx));
        cx.run_until_parked();
        assert_eq!(rows(cx)[3..6], ["B", "  y.txt", "  z.txt"]);

        // A folder that went goes with what's in it; a row to reveal is
        // scrolled to once its folder is read.
        std::fs::remove_dir_all(dir.join("a")).unwrap();
        tree.update(cx, |t, cx| t.paths_changed([dir.join("a").as_path()], cx));
        cx.run_until_parked();
        assert_eq!(rows(cx), ["B", "  y.txt", "  z.txt", "build.rs", "Readme.md"]);
        std::fs::create_dir_all(dir.join("c/d")).unwrap();
        std::fs::write(dir.join("c/d/deep.txt"), "").unwrap();
        tree.update(cx, |t, cx| {
            t.paths_changed([dir.join("c").as_path()], cx);
            t.reveal_and_scroll(&dir.join("c/d/deep.txt"), cx);
        });
        tree.read_with(cx, |t, _| assert!(t.scroll_to.is_some()));
        cx.run_until_parked();
        assert_eq!(rows(cx)[3..6], ["c", "  d", "    deep.txt"]);
        tree.read_with(cx, |t, _| assert!(t.scroll_to.is_none(), "scrolled to it"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn what_git_ignores_is_dimmed_with_everything_in_it(cx: &mut TestAppContext) {
        let tree = cx.new(ProjectTree::new);
        let ignored: Arc<HashSet<PathBuf>> = Arc::new(["/p/target", "/p/run.log"].map(PathBuf::from).into_iter().collect());
        tree.update(cx, |t, cx| t.decorate(Arc::default(), ignored, None, None, cx));
        tree.read_with(cx, |t, _| {
            assert!(t.is_ignored(Path::new("/p/target")));
            assert!(t.is_ignored(Path::new("/p/target/debug/a.o")));
            assert!(t.is_ignored(Path::new("/p/run.log")));
            assert!(!t.is_ignored(Path::new("/p/src/main.rs")));
            // A name that only starts like an ignored folder's.
            assert!(!t.is_ignored(Path::new("/p/target-notes.md")));
        });
    }

    #[test]
    fn a_search_finds_matches_in_closed_folders_in_tree_order() {
        let root = PathBuf::from("/p");
        let files = ["src/ui/Button.tsx", "src/main.rs", "docs/Buttons.md", "docs/Résumé.md", "docs/Cafe\u{301}.txt", "target/button.o", "README.md"];
        let files: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
        // A folder the tree has read that Go to File doesn't list (Git ignores it).
        let built = Child { path: root.join("dist/button.js"), name: "button.js".into(), is_dir: false, cycle: false };
        let listings: HashMap<PathBuf, Arc<[Child]>> = HashMap::from([(root.join("dist"), Arc::from(vec![built]))]);
        // The matches in order, their marked bytes in `[]`.
        let matches = |query: &str| -> Vec<String> {
            let (found, marks) = find_matches(&root, &files, &listings, query);
            found
                .iter()
                .map(|path| {
                    let mut name = path.file_name().unwrap().to_string_lossy().into_owned();
                    for r in marks[path].iter().rev() {
                        name.insert(r.end, ']');
                        name.insert(r.start, '[');
                    }
                    let folder = path.parent().unwrap().strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                    if folder.is_empty() { name } else { format!("{folder}/{name}") }
                })
                .collect()
        };
        // In any case, from closed folders and read ones, in tree order
        // (folders first); what the tree hides (`target`) stays hidden.
        assert_eq!(matches("BUTTON"), ["dist/[button].js", "docs/[Button]s.md", "src/ui/[Button].tsx"]);
        // Each word, anywhere in the name; a folder matches too.
        assert_eq!(matches("rs ma"), ["src/[ma]in.[rs]"]);
        assert_eq!(matches("doc"), ["[doc]s"]);
        // Marked bytes are whole chars, wherever they are, at a name's end too.
        assert_eq!(matches("resu"), ["docs/[Résu]mé.md"]);
        assert_eq!(matches("és"), ["docs/R[és]umé.md"]);
        assert_eq!(matches("md"), ["docs/Buttons.[md]", "docs/Résumé.[md]", "README.[md]"]);
        // A letter written decomposed (`e` and an accent, as macOS writes some
        // names) is found with the accent and without, marked with it.
        assert_eq!(matches("cafe"), ["docs/[Cafe\u{301}].txt"]);
        assert_eq!(matches("café"), ["docs/[Cafe\u{301}].txt"]);
        assert!(matches("nothing").is_empty());
    }

    #[test]
    fn characters_start_a_search_spaces_dont() {
        assert!(starts_search("a"));
        assert!(starts_search("é"));
        assert!(starts_search("@"));
        // What an input method composes, or a dead key's accent.
        assert!(starts_search("か"));
        assert!(starts_search("´"));
        assert!(!starts_search(""));
        assert!(!starts_search(" "));
        assert!(!starts_search("\t"));
        assert!(!starts_search("\n"));
    }

    /// A deleted entry's row is taken by the next one in its folder, or the
    /// one before it at the end; a moved one keeps the selection.
    #[gpui::test]
    fn the_selection_moves_on_from_a_delete_and_follows_a_move(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("kuana-tree-selection-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for file in ["src/a.rs", "src/b.rs", "src/c.rs", "z.md"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let tree = cx.new(ProjectTree::new);
        tree.update(cx, |t, cx| t.set_root(Some(dir.clone()), [dir.join("src")], cx));
        cx.run_until_parked();
        let selected = |cx: &mut TestAppContext| tree.read_with(cx, |t, _| t.selected.clone());
        let delete = |path: PathBuf, cx: &mut TestAppContext| {
            std::fs::remove_file(&path).unwrap();
            tree.update(cx, |t, cx| {
                t.forget(&path);
                t.paths_changed([path.as_path()], cx);
            });
            cx.run_until_parked();
        };
        tree.update(cx, |t, _| t.selected = Some(dir.join("src/b.rs")));
        delete(dir.join("src/b.rs"), cx);
        assert_eq!(selected(cx), Some(dir.join("src/c.rs")));
        delete(dir.join("src/c.rs"), cx);
        assert_eq!(selected(cx), Some(dir.join("src/a.rs")));
        tree.update(cx, |t, _| t.remap(|p| (p == dir.join("src/a.rs").as_path()).then(|| dir.join("src/d.rs"))));
        assert_eq!(selected(cx), Some(dir.join("src/d.rs")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A click opens a file with the keyboard left in the tree, a double
    /// click in its editor. Typing searches the whole project as JetBrains'
    /// speed search does: the tree stays, the first match is shown (its
    /// folders opened), ↓ goes to the next one (the folders opened for the
    /// last close again), Enter opens it (once the query's matches are in).
    /// Escape ends a search, the match staying selected, and so does leaving
    /// the tree; without a search, Escape goes back to the editor. A dead
    /// key's accent opens a search, searched once it's composed. Find starts
    /// one too.
    #[gpui::test]
    fn clicks_open_and_typing_searches_the_tree(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("kuana-tree-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for file in ["src/main.rs", "src/ui/domain.rs", "docs/maintaining.md", "README.md"] {
            std::fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
            std::fs::write(dir.join(file), "").unwrap();
        }
        cx.update(|cx| {
            cx.set_global(Theme::blank_for_tests());
            cx.bind_keys(crate::keymap::bindings(crate::settings::BaseKeymap::JetBrains));
        });
        let (tree, cx) = cx.add_window_view(|_, cx| ProjectTree::new(cx));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let left = Rc::new(Cell::new(0));
        cx.update(|window, cx| {
            // Focus changes are told in an active window only.
            window.activate_window();
            let (opened, left) = (opened.clone(), left.clone());
            cx.subscribe(&tree, move |_, event: &TreeEvent, _| match event {
                TreeEvent::Open { path, focus } => opened.borrow_mut().push((path.clone(), *focus)),
                TreeEvent::Leave => left.set(left.get() + 1),
                _ => {}
            })
            .detach();
        });
        tree.update(cx, |t, cx| {
            t.set_root(Some(dir.clone()), [], cx);
            t.set_files(crate::project::ProjectSnapshot::scan(&dir).openable(), cx);
        });
        cx.run_until_parked();
        let rows = |cx: &mut VisualTestContext| {
            tree.read_with(cx, |t, _| t.entries.iter().map(|e| format!("{}{}", "  ".repeat(e.depth), e.name)).collect::<Vec<_>>())
        };
        let selected = |cx: &mut VisualTestContext| tree.read_with(cx, |t, _| t.selected.clone());
        let click = |ix: usize, count: usize, cx: &mut VisualTestContext| {
            cx.update(|window, cx| tree.update(cx, |t, cx| t.clicked(ix, count, window, cx)))
        };
        assert_eq!(rows(cx), ["docs", "src", "README.md"]);

        // A click opens a file, the keyboard staying in the tree, and a double
        // click opens it in its editor; a folder opens at once, and a double
        // click's second click leaves it open.
        click(2, 1, cx);
        assert_eq!(selected(cx), Some(dir.join("README.md")));
        assert_eq!(opened.borrow().as_slice(), [(dir.join("README.md"), false)]);
        click(2, 2, cx);
        assert_eq!(opened.borrow().last(), Some(&(dir.join("README.md"), true)));
        click(1, 1, cx);
        click(1, 2, cx);
        cx.run_until_parked();
        assert_eq!(rows(cx), ["docs", "src", "  ui", "  main.rs", "README.md"]);
        // ← closes the selected folder.
        cx.update(|window, cx| window.focus(&tree.focus_handle(cx)));
        cx.simulate_keystrokes("left");
        assert_eq!(rows(cx), ["docs", "src", "README.md"]);

        // A space doesn't start a search, a character does. The tree stays as
        // it is; the first match is shown, its folder opened.
        cx.simulate_keystrokes("space");
        tree.read_with(cx, |t, _| assert!(t.search.is_none()));
        cx.simulate_keystrokes("m a i n");
        assert_eq!(rows(cx), ["docs", "  maintaining.md", "src", "README.md"]);
        assert_eq!(selected(cx), Some(dir.join("docs/maintaining.md")));
        // ↓: the next match, and only its folders open.
        cx.simulate_keystrokes("down");
        assert_eq!(rows(cx), ["docs", "src", "  ui", "    domain.rs", "  main.rs", "README.md"]);
        assert_eq!(selected(cx), Some(dir.join("src/ui/domain.rs")));
        cx.simulate_keystrokes("down");
        assert_eq!(rows(cx), ["docs", "src", "  ui", "  main.rs", "README.md"]);
        tree.read_with(cx, |t, _| assert_eq!(t.search.as_ref().unwrap().marks[&dir.join("src/main.rs")], [0..4]));
        // Enter opens it and ends the search; what it showed stays.
        cx.simulate_keystrokes("enter");
        assert_eq!(opened.borrow().last(), Some(&(dir.join("src/main.rs"), true)));
        tree.read_with(cx, |t, _| assert!(t.search.is_none()));
        assert_eq!(rows(cx), ["docs", "src", "  ui", "  main.rs", "README.md"]);

        // Enter before a query's matches come opens the first of them then,
        // not a match of the query before.
        cx.simulate_keystrokes("r e a d enter");
        assert_eq!(opened.borrow().last(), Some(&(dir.join("README.md"), true)));
        tree.read_with(cx, |t, _| assert!(t.search.is_none()));
        cx.simulate_keystrokes("m");
        assert_eq!(selected(cx), Some(dir.join("README.md")), "a match already, it stays");
        cx.simulate_keystrokes("a i n . r enter");
        assert_eq!(opened.borrow().last(), Some(&(dir.join("src/ui/domain.rs"), true)));

        // Escape ends a search, the match staying selected; without a search,
        // it goes back to the editor.
        cx.simulate_keystrokes("r e a d");
        assert_eq!(selected(cx), Some(dir.join("README.md")));
        cx.simulate_keystrokes("escape");
        tree.read_with(cx, |t, _| assert!(t.search.is_none()));
        assert_eq!(selected(cx), Some(dir.join("README.md")));
        assert!(cx.update(|window, cx| tree.read(cx).focus.is_focused(window)));
        assert_eq!(left.get(), 0);
        cx.simulate_keystrokes("escape");
        assert_eq!(left.get(), 1);

        // A dead key's accent (or what an input method composes) opens the
        // search but isn't searched for; what it commits is.
        cx.update(|window, cx| tree.update(cx, |t, cx| t.replace_and_mark_text_in_range(None, "´", None, window, cx)));
        cx.run_until_parked();
        tree.read_with(cx, |t, _| assert!(t.search.as_ref().is_some_and(|s| s.task.is_none() && s.found.is_none())));
        cx.update(|window, cx| tree.update(cx, |t, cx| t.replace_text_in_range(None, "é", window, cx)));
        cx.run_until_parked();
        tree.read_with(cx, |t, _| assert_eq!(t.search.as_ref().and_then(|s| s.found.clone()), Some(Vec::new())));
        cx.simulate_keystrokes("escape");
        assert_eq!(left.get(), 1);

        // Find starts a search too; leaving the tree ends it.
        cx.simulate_keystrokes("secondary-f d o c");
        assert_eq!(selected(cx), Some(dir.join("docs")));
        cx.update(|window, _| window.blur());
        // The window draws before the next key, telling the tree its focus went.
        cx.simulate_keystrokes("escape");
        tree.read_with(cx, |t, _| assert!(t.search.is_none()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A search closes only the folders it opened, shows a selection that's
    /// a match where it is, looks again only when the project's files
    /// change, and ends on a click below the rows.
    #[gpui::test]
    fn a_search_keeps_what_it_did_not_do(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("kuana-tree-search-keeps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for file in ["a/one.rs", "b/one.md", "c/deep/x.txt"] {
            std::fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
            std::fs::write(dir.join(file), "").unwrap();
        }
        cx.update(|cx| {
            cx.set_global(Theme::blank_for_tests());
            cx.bind_keys(crate::keymap::bindings(crate::settings::BaseKeymap::JetBrains));
        });
        let (tree, cx) = cx.add_window_view(|_, cx| ProjectTree::new(cx));
        let files = crate::project::ProjectSnapshot::scan(&dir).openable();
        tree.update(cx, |t, cx| {
            t.set_root(Some(dir.clone()), [], cx);
            t.set_files(files.clone(), cx);
        });
        cx.update(|window, cx| {
            window.activate_window();
            window.focus(&tree.focus_handle(cx));
        });
        cx.run_until_parked();
        let rows = |cx: &mut VisualTestContext| {
            tree.read_with(cx, |t, _| t.entries.iter().map(|e| format!("{}{}", "  ".repeat(e.depth), e.name)).collect::<Vec<_>>())
        };
        let selected = |cx: &mut VisualTestContext| tree.read_with(cx, |t, _| t.selected.clone());

        // Select Opened File opens `c` and `deep` meanwhile: ↓ (to the first
        // match, the selection being none, then the next) keeps them open.
        cx.simulate_keystrokes("o n e");
        assert_eq!(rows(cx), ["a", "  one.rs", "b", "c"]);
        tree.update(cx, |t, cx| t.reveal_and_scroll(&dir.join("c/deep/x.txt"), cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("down down");
        assert_eq!(selected(cx), Some(dir.join("b/one.md")));
        assert_eq!(rows(cx), ["a", "b", "  one.md", "c", "  deep", "    x.txt"]);

        // The same files again: no search; others: one, after the one under way.
        tree.update(cx, |t, cx| t.set_files(files.clone(), cx));
        tree.read_with(cx, |t, _| assert!(t.search.as_ref().unwrap().task.is_none()));
        std::fs::write(dir.join("a/two.rs"), "").unwrap();
        let more = crate::project::ProjectSnapshot::scan(&dir).openable();
        tree.update(cx, |t, cx| {
            t.set_files(more.clone(), cx);
            assert!(t.search.as_ref().unwrap().task.is_some());
            t.set_files(Arc::new(more[1..].to_vec()), cx);
            assert!(t.search.as_ref().unwrap().stale, "searched again when it's done");
        });
        cx.run_until_parked();
        tree.read_with(cx, |t, _| assert!(t.search.as_ref().is_some_and(|s| s.task.is_none() && !s.stale)));

        // A click below the rows ends the search.
        let size = cx.update(|window, _| window.viewport_size());
        cx.simulate_click(gpui::point(size.width / 2., size.height - px(4.)), Modifiers::none());
        tree.read_with(cx, |t, _| assert!(t.search.is_none()));

        // A selection that's a match, in a closed folder, is shown there.
        tree.update(cx, |t, cx| t.collapse_all(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&tree.focus_handle(cx)));
        cx.simulate_keystrokes("o n e");
        assert_eq!(selected(cx), Some(dir.join("b/one.md")));
        assert_eq!(rows(cx), ["a", "b", "  one.md", "c"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
