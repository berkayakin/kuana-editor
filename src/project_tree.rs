//! The project's file tree (the sidebar's Files view): which folders are
//! open, the rows they make, drag & drop between them and from Finder. File
//! operations are the workspace's: the tree reports what was asked
//! (`TreeEvent`) and the workspace tells it what changed (expand, remap,
//! reveal, paths created or removed) and how rows look (Git changes, the
//! open file, a menu's target). Folders are read in the background and
//! kept: rows are made from what was read, and a change reads again only
//! the folders it touched.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    Context, DragMoveEvent, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Point, Render,
    SharedString, Task, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    git::Change,
    git_panel::change_color,
    theme::Theme,
    ui::{DragLabel, HoverFill, file_icon, folder_icon, icon_el},
    workspace::{RedoFileChange, UndoFileChange},
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
    /// A file was clicked.
    Open(PathBuf),
    /// A row (or empty space: the project folder) was right-clicked.
    Menu { path: PathBuf, is_dir: bool, position: Point<Pixels> },
    /// A row was dropped into a folder.
    Move { from: PathBuf, into: PathBuf },
    /// Files from Finder were dropped into a folder.
    CopyIn { paths: Vec<PathBuf>, into: PathBuf },
    /// ⌘Z / ⇧⌘Z with the tree focused: undo or redo a file operation.
    History { redo: bool },
    /// A folder opened or closed (the session keeps which).
    Toggled,
    /// A drag over the tree started or stopped targeting the project folder
    /// (the Files tab lights up with the tree).
    RootDrop(bool),
}

impl EventEmitter<TreeEvent> for ProjectTree {}

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
        self.rebuild(cx);
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
        self.expanded.insert(dir);
    }

    /// Closes every folder but the project's.
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        self.expanded.retain(|p| Some(p) == root.as_ref());
        self.rebuild(cx);
    }

    /// `path` is gone: so are its open folders.
    pub fn forget(&mut self, path: &Path) {
        self.expanded.retain(|p| !p.starts_with(path));
        self.listings.retain(|p, _| !p.starts_with(path));
    }

    /// Open folders follow a move or rename (read again at their new place).
    pub fn remap(&mut self, remap: impl Fn(&Path) -> Option<PathBuf>) {
        self.expanded = self.expanded.iter().map(|p| remap(p).unwrap_or_else(|| p.clone())).collect();
        self.listings.retain(|p, _| remap(p).is_none());
    }

    /// Opens the folders down to `path` (listed on the next `rebuild`).
    pub fn reveal(&mut self, path: &Path) {
        let Some(root) = self.root.clone() else { return };
        for dir in path.ancestors().skip(1) {
            if !dir.starts_with(&root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
        }
    }

    /// Opens the folders down to `path` and scrolls its row into the middle
    /// (once its folder is read).
    pub fn reveal_and_scroll(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.reveal(path);
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

    fn click(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix).cloned() else { return };
        // Never open a folder as a file (opening a folder makes it the project).
        if entry.is_dir || entry.path.is_dir() {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            self.rebuild(cx);
            cx.emit(TreeEvent::Toggled);
        } else {
            cx.emit(TreeEvent::Open(entry.path));
        }
    }
}

impl Focusable for ProjectTree {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ProjectTree {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let drop_on_root = self.drop_on_root();
        let list = uniform_list(
            "file-tree",
            self.entries.len(),
            cx.processor(move |tree, range: std::ops::Range<usize>, _window, cx| {
                let theme = Theme::get(cx);
                range
                    .map(|ix| {
                        let e = &tree.entries[ix];
                        let selected = tree.active.as_deref() == Some(e.path.as_path());
                        let change = tree.git.get(&e.path).copied();
                        // Ignored by Git: dimmed, like VS Code.
                        let name_color: Hsla = match change {
                            Some(c) => change_color(theme, c),
                            None if tree.is_ignored(&e.path) => theme.text_dim.into(),
                            None if selected => theme.text.into(),
                            None => theme.text_muted.into(),
                        };
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
                                .text_color(if selected { theme.text } else { theme.text_muted })
                                // The open file has the accent fill.
                                .when(selected, |el| el.bg(theme.accent_soft))
                                .row_hover(theme.hover)
                                .on_click(cx.listener(move |tree, _, _, cx| tree.click(ix, cx)))
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
                                .child(div().flex_1().min_w_0().truncate().text_color(name_color).child(e.name.clone()))
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
        .track_focus(&self.focus)
        .key_context("FileTree")
        .on_action(cx.listener(|_, _: &UndoFileChange, _, cx| cx.emit(TreeEvent::History { redo: false })))
        .on_action(cx.listener(|_, _: &RedoFileChange, _, cx| cx.emit(TreeEvent::History { redo: true })))
        // Right-clicking empty space targets the project root.
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|tree, ev: &MouseDownEvent, _, cx| {
                if let Some(root) = tree.root.clone() {
                    cx.emit(TreeEvent::Menu { path: root, is_dir: true, position: ev.position });
                }
            }),
        );
        div()
            .relative()
            .size_full()
            .child(list)
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
            .child(crate::ui::list_scrollbar("file-tree-scrollbar", &self.scroll, cx))
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
            if matches!(name.as_str(), ".git" | ".DS_Store" | "target") {
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

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

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
}
