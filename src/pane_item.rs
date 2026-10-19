//! What a pane's tab shows: a text editor, an image, a Markdown preview or a
//! Git / Local History diff.
//! Each kind is its own view implementing
//! [`Item`], the surface the workspace needs to draw its tab, save, close
//! and keep it in the session; [`PaneItem`] is the handle panes hold.
//!
//! The handle is an enum, not a `dyn` trait object: the kinds are all in
//! this crate (nothing outside adds pane items), so a closed set keeps the
//! handle `Clone + Eq + Hash` without downcasting, `editor()` is a match,
//! and a new kind makes the compiler point at every place that must handle
//! it (the session, splitting, saving). To add one: a view implementing
//! `Item` (and `Render`), a variant here, a `TabKind` for the session, and
//! where it opens in the workspace.

use std::path::PathBuf;

use gpui::{AnyView, App, AppContext, Entity, EntityId, FocusHandle, Focusable};
use ropey::Rope;

use crate::{
    assets::Icon,
    diff_view::DiffView,
    editor::Editor,
    encoding::TextEncoding,
    image_view::ImageView,
    markdown_preview::MarkdownPreview,
    state::{TabKind, TabSession},
};

/// What Save and Save As do with an item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Saving {
    /// Text: Save writes it (formatting first if asked to), Save As writes
    /// it to a new file and goes on there.
    Text,
    /// A file shown as it is (an image): Save has nothing to write, Save As
    /// copies the file and shows the copy.
    CopyFile,
    /// Nothing of its own (a preview of another tab's document).
    Nothing,
}

/// A tab's icon.
#[derive(Clone, PartialEq)]
pub enum TabIcon {
    /// The file type's icon (`ui::file_icon`).
    File(PathBuf),
    /// One of Kuana's own icons, tinted.
    Ui(Icon),
}

/// What the workspace asks of anything a pane shows. Views also implement
/// `Render`, which isn't object safe, so it's left to the handle.
pub trait Item: Focusable {
    /// The tab's name, also used in the window title.
    fn tab_title(&self, cx: &App) -> String;

    /// The file it's about: the tab's tooltip and icon, breadcrumbs, Copy
    /// Path, Select in File Tree, recent files.
    fn path(&self, cx: &App) -> Option<PathBuf>;

    fn tab_icon(&self, cx: &App) -> TabIcon {
        TabIcon::File(self.path(cx).unwrap_or_default())
    }

    /// Unsaved changes: the tab shows a dot, and closing the last tab of
    /// them asks first.
    fn is_dirty(&self, _: &App) -> bool {
        false
    }

    /// Errors in it: a red dot on the tab.
    fn has_errors(&self, _: &App) -> bool {
        false
    }

    fn saving(&self) -> Saving;

    /// The tab as the project session keeps it (the workspace adds its pane
    /// and pin), with its text when that's unsaved; None if it isn't kept.
    fn session(&self, cx: &App) -> Option<(TabSession, Option<Rope>)>;
}

impl Item for Editor {
    fn tab_title(&self, cx: &App) -> String {
        self.doc.read(cx).file_name()
    }

    fn path(&self, cx: &App) -> Option<PathBuf> {
        self.doc.read(cx).path.clone()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.doc.read(cx).is_dirty()
    }

    fn has_errors(&self, cx: &App) -> bool {
        self.doc.read(cx).has_errors()
    }

    fn saving(&self) -> Saving {
        Saving::Text
    }

    fn session(&self, cx: &App) -> Option<(TabSession, Option<Rope>)> {
        let doc = self.doc.read(cx);
        let mut tab = TabSession { path: doc.path.clone(), ..TabSession::default() };
        let text = if let Some(u) = self.unloaded() {
            // A tab not read yet goes back as it was restored.
            tab.cursor = u.cursor;
            tab.top_line = u.top_line;
            tab.folds = u.folds.clone();
            tab.encoding = u.encoding.filter(|&enc| enc != TextEncoding::UTF8).map(TextEncoding::key);
            u.unsaved.clone()
        } else if !self.is_loaded() {
            // A split of one: its own caret and view (the text, folds and
            // encoding go with that tab).
            (tab.cursor, tab.top_line) = self.waiting_view().unwrap_or_default();
            None
        } else {
            // An untitled buffer without edits has nothing to bring back.
            if doc.path.is_none() && !doc.is_dirty() {
                return None;
            }
            tab.cursor = self.primary_selection().head;
            tab.top_line = self.top_line(cx);
            tab.folds = self.folded_rows(cx);
            tab.encoding = (doc.encoding != TextEncoding::UTF8).then(|| doc.encoding.key());
            doc.is_dirty().then(|| doc.rope().clone())
        };
        Some((tab, text))
    }
}

impl Item for ImageView {
    fn tab_title(&self, _: &App) -> String {
        self.path().file_name().map_or_else(|| self.path().display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        Some(self.path().to_path_buf())
    }

    fn saving(&self) -> Saving {
        Saving::CopyFile
    }

    fn session(&self, _: &App) -> Option<(TabSession, Option<Rope>)> {
        Some((TabSession { kind: TabKind::Image, path: Some(self.path().to_path_buf()), ..TabSession::default() }, None))
    }
}

impl Item for MarkdownPreview {
    fn tab_title(&self, cx: &App) -> String {
        self.title(cx)
    }

    fn path(&self, cx: &App) -> Option<PathBuf> {
        MarkdownPreview::path(self, cx)
    }

    fn tab_icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(Icon::Eye)
    }

    fn saving(&self) -> Saving {
        Saving::Nothing
    }

    fn session(&self, cx: &App) -> Option<(TabSession, Option<Rope>)> {
        // An untitled buffer's preview has no file to find it by again.
        let path = MarkdownPreview::path(self, cx)?;
        Some((TabSession { kind: TabKind::MarkdownPreview, path: Some(path), ..TabSession::default() }, None))
    }
}

impl Item for DiffView {
    fn tab_title(&self, _: &App) -> String {
        let path = self.file();
        path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        Some(self.file())
    }

    fn tab_icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(Icon::GitCompare)
    }

    fn saving(&self) -> Saving {
        Saving::Nothing
    }

    fn session(&self, _: &App) -> Option<(TabSession, Option<Rope>)> {
        // What it compared (a status, a Local History version) may be gone by then.
        None
    }
}

/// What a tab bar shows of an item. The workspace keeps it and updates it
/// when the item changes, so drawing the tab bars reads no item: one that
/// changes (diagnostics arriving for a tab out of sight) would redraw the
/// window, though nothing it shows changed.
#[derive(Clone, PartialEq)]
pub struct TabLook {
    pub title: String,
    /// The tab's tooltip: the file's full path.
    pub path: Option<PathBuf>,
    pub icon: TabIcon,
    pub dirty: bool,
    pub errors: bool,
}

/// A pane's tab: the view it shows.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum PaneItem {
    Editor(Entity<Editor>),
    Image(Entity<ImageView>),
    Preview(Entity<MarkdownPreview>),
    Diff(Entity<DiffView>),
}

impl From<Entity<Editor>> for PaneItem {
    fn from(editor: Entity<Editor>) -> Self {
        PaneItem::Editor(editor)
    }
}

impl From<Entity<ImageView>> for PaneItem {
    fn from(image: Entity<ImageView>) -> Self {
        PaneItem::Image(image)
    }
}

impl From<Entity<MarkdownPreview>> for PaneItem {
    fn from(preview: Entity<MarkdownPreview>) -> Self {
        PaneItem::Preview(preview)
    }
}

impl From<Entity<DiffView>> for PaneItem {
    fn from(diff: Entity<DiffView>) -> Self {
        PaneItem::Diff(diff)
    }
}

impl PaneItem {
    /// The view, as an [`Item`].
    pub fn read<'a>(&self, cx: &'a App) -> &'a dyn Item {
        match self {
            PaneItem::Editor(e) => e.read(cx),
            PaneItem::Image(i) => i.read(cx),
            PaneItem::Preview(p) => p.read(cx),
            PaneItem::Diff(d) => d.read(cx),
        }
    }

    pub fn entity_id(&self) -> EntityId {
        match self {
            PaneItem::Editor(e) => e.entity_id(),
            PaneItem::Image(i) => i.entity_id(),
            PaneItem::Preview(p) => p.entity_id(),
            PaneItem::Diff(d) => d.entity_id(),
        }
    }

    pub fn editor(&self) -> Option<&Entity<Editor>> {
        match self {
            PaneItem::Editor(e) => Some(e),
            _ => None,
        }
    }

    pub fn preview(&self) -> Option<&Entity<MarkdownPreview>> {
        match self {
            PaneItem::Preview(p) => Some(p),
            _ => None,
        }
    }

    pub fn diff(&self) -> Option<&Entity<DiffView>> {
        match self {
            PaneItem::Diff(d) => Some(d),
            _ => None,
        }
    }

    /// The file itself (a text editor, an image), not a view of it (a
    /// preview, a diff): opening the file finds it, Recent Files lists it.
    pub fn is_file(&self) -> bool {
        matches!(self, PaneItem::Editor(_) | PaneItem::Image(_))
    }

    pub fn image(&self) -> Option<&Entity<ImageView>> {
        match self {
            PaneItem::Image(i) => Some(i),
            _ => None,
        }
    }

    pub fn view(&self) -> AnyView {
        match self {
            PaneItem::Editor(e) => e.clone().into(),
            PaneItem::Image(i) => i.clone().into(),
            PaneItem::Preview(p) => p.clone().into(),
            PaneItem::Diff(d) => d.clone().into(),
        }
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    pub fn tab_title(&self, cx: &App) -> String {
        self.read(cx).tab_title(cx)
    }

    pub fn path(&self, cx: &App) -> Option<PathBuf> {
        self.read(cx).path(cx)
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    /// What its tab shows now.
    pub fn look(&self, cx: &App) -> TabLook {
        let item = self.read(cx);
        TabLook {
            title: item.tab_title(cx),
            path: item.path(cx),
            icon: item.tab_icon(cx),
            dirty: item.is_dirty(cx),
            errors: item.has_errors(cx),
        }
    }

    /// A second view of the same thing, for a split: an editor of the same
    /// document (its own carets and scroll), the same image, a preview of
    /// the same document.
    pub fn split(&self, cx: &mut App) -> PaneItem {
        match self {
            PaneItem::Editor(e) => PaneItem::Editor(cx.new(|cx| Editor::view_of(e, cx))),
            PaneItem::Image(i) => {
                let path = i.read(cx).path().to_path_buf();
                PaneItem::Image(cx.new(|cx| ImageView::new(path, cx)))
            }
            PaneItem::Preview(p) => PaneItem::Preview(cx.new(|cx| MarkdownPreview::split_of(p, cx))),
            PaneItem::Diff(d) => PaneItem::Diff(cx.new(|cx| DiffView::split_of(d, cx))),
        }
    }
}
