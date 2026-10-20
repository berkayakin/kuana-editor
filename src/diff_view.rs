//! The diff of one file, shown in a pane's tab: a working-tree change (from
//! the Commit panel), a file in a commit (from the Git log) or a Local
//! History version against the file as it is. Side by side or unified, with
//! syntax colors, changed words emphasized, and Stage / Unstage per change.

use std::{
    cell::Cell,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla, IntoElement, ListAlignment,
    ListOffset, ListState, Pixels, Render, Rgba, SharedString, StyledText, Subscription, Task, Window, canvas, div, list,
    pattern_slash, prelude::*, px,
};
use ropey::Rope;

use crate::{
    assets::Icon,
    contrast,
    document::Document,
    editor::{NextChange, PreviousChange},
    git::{self, Change, CommitFile, DiffDocument, DiffLineKind, FileStatus},
    preview::{self, ImageInfo},
    git_panel::{GitOp, change_color},
    syntax::{Highlight, Language, Syntax},
    theme::{Appearance, Theme},
    transition::mix,
    ui::{HoverFill, icon_el, text_button, tooltip_view},
};

/// A line's height, the editor's: a row is one line, more when its text wraps.
fn row_height(theme: &Theme) -> f32 {
    theme.editor_line_height
}

/// Stripes for the side that has no line, about 10px apart. Each row draws
/// them from its own corner, so their period (in device pixels, which the
/// shader counts in) divides the row's height: they run on from row to row.
fn stripes(theme: &Theme, scale: f32) -> gpui::Background {
    let row = (row_height(theme) * scale).round();
    let periods = (row / (10. * scale)).round().max(1.);
    let width = scale.round().max(1.);
    pattern_slash(theme.border.into(), width, row / periods - width)
}
/// Room on the right of a change's first row for its Stage / Unstage button.
const CHANGE_BUTTON_ROOM: f32 = 84.;
/// The bar at the left of a changed line.
const BAR_WIDTH: f32 = 3.;
/// Narrower than this, two halves are too narrow to read: the diff shows
/// unified (as VS Code's does) and drops the switch, keeping the choice.
const SIDE_BY_SIDE_MIN_WIDTH: f32 = 720.;

/// Whether a view `width` wide fits side by side (0: not laid out yet).
fn fits_side_by_side(width: Pixels) -> bool {
    width == px(0.) || width >= px(SIDE_BY_SIDE_MIN_WIDTH)
}

pub enum DiffViewEvent {
    /// Nothing left to show (a working tree change staged or discarded): its tab closes.
    Close,
    OpenFile(PathBuf),
    Run(GitOp),
    /// Put this text (a Local History version) back in the file.
    Restore { path: PathBuf, text: Arc<str> },
    /// The user switched between side by side and unified.
    Layout { side_by_side: bool },
}

/// What is being compared.
#[derive(Clone)]
pub enum DiffSource {
    /// Staged: HEAD → index. Unstaged: index → working tree (or the open
    /// document's text, so unsaved edits show).
    Working { file: FileStatus, staged: bool },
    /// A commit's change to a file: first parent → commit.
    Commit { sha: String, summary: String, file: CommitFile },
    /// A Local History version (`label`: when and why it was kept) → the
    /// file as it is (the open document's text, so unsaved edits show).
    History { path: PathBuf, label: String, text: Arc<str> },
}

impl DiffSource {
    /// Whether two sources show the same diff, so one tab serves both.
    /// Local History versions of a file share a tab (stepping through them
    /// replaces it); the rest go by file and side.
    pub fn same_tab(&self, other: &DiffSource) -> bool {
        match (self, other) {
            (DiffSource::Working { file: a, staged: x }, DiffSource::Working { file: b, staged: y }) => a.rel == b.rel && x == y,
            (DiffSource::Commit { sha: a, file: f, .. }, DiffSource::Commit { sha: b, file: g, .. }) => a == b && f.rel == g.rel,
            (DiffSource::History { path: a, .. }, DiffSource::History { path: b, .. }) => a == b,
            _ => false,
        }
    }

    /// The file whose open document is the "after" side: the working tree
    /// file of an unstaged change or a Local History version.
    pub fn live_file(&self) -> Option<&Path> {
        match self {
            DiffSource::Working { file, staged: false } => Some(&file.path),
            DiffSource::History { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// How long typing in the file's document waits before the diff follows.
const FOLLOW_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Syntax spans per line, in tab-expanded byte offsets.
type LineSpans = Vec<Vec<(Range<usize>, Highlight)>>;

/// The two versions of an image: a temporary copy of each and its details.
struct ImageSides {
    before: Option<(PathBuf, ImageInfo)>,
    after: Option<(PathBuf, ImageInfo)>,
}

struct Loaded {
    /// Binary content has no meaningful text diff.
    binary: bool,
    /// Images are compared as pictures.
    images: Option<ImageSides>,
    doc: DiffDocument,
    old_spans: LineSpans,
    new_spans: LineSpans,
    /// The compared "after" text (what Stage Lines stages from).
    current: Arc<str>,
}

pub struct DiffView {
    repo: PathBuf,
    source: DiffSource,
    /// The chosen layout.
    side_by_side: bool,
    /// The layout shown: unified when the view is too narrow for halves.
    shown_side_by_side: bool,
    /// The view's width last frame.
    width: Rc<Cell<Pixels>>,
    /// Show every line instead of collapsing unchanged runs.
    expanded: bool,
    loaded: Option<Arc<Loaded>>,
    /// The change last jumped to with F7 / ⇧F7.
    current_change: Option<usize>,
    focus_handle: FocusHandle,
    /// The rows, which wrap long lines and so differ in height.
    list: ListState,
    /// The open document of the "after" side (`DiffSource::live_file`), whose
    /// text (unsaved edits included) the diff shows and follows.
    document: Option<(Entity<Document>, Subscription)>,
    /// The document version last diffed.
    seen_version: u64,
    /// The diff being computed; a newer one replaces (cancels) it.
    loading: Option<Task<()>>,
    /// The pause after an edit of `document` before the diff follows it.
    follow: Option<Task<()>>,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    /// `document`: the open document of `source.live_file()`, if any.
    pub fn new(repo: PathBuf, source: DiffSource, document: Option<Entity<Document>>, side_by_side: bool, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            repo,
            source,
            side_by_side,
            shown_side_by_side: side_by_side,
            width: Rc::default(),
            expanded: false,
            loaded: None,
            current_change: None,
            focus_handle: cx.focus_handle(),
            list: ListState::new(0, ListAlignment::Top, px(400.)).measure_all(),
            document: None,
            seen_version: 0,
            loading: None,
            follow: None,
        };
        view.set_document(document, cx);
        view.reload(cx);
        view
    }

    /// The same diff in another view, for a split.
    pub fn split_of(other: &Entity<DiffView>, cx: &mut Context<Self>) -> Self {
        let o = other.read(cx);
        let (repo, source, side_by_side) = (o.repo.clone(), o.source.clone(), o.side_by_side);
        let document = o.document.as_ref().map(|(doc, _)| doc.clone());
        Self::new(repo, source, document, side_by_side, cx)
    }

    pub fn source(&self) -> &DiffSource {
        &self.source
    }

    /// Shows another diff in this view (a Local History version of the same file).
    pub fn set_source(&mut self, source: DiffSource, document: Option<Entity<Document>>, cx: &mut Context<Self>) {
        self.source = source;
        self.current_change = None;
        self.set_document(document, cx);
        self.reload(cx);
    }

    /// Follows edits of the "after" side's open document, a moment after typing.
    fn set_document(&mut self, document: Option<Entity<Document>>, cx: &mut Context<Self>) {
        self.document = document.map(|doc| {
            self.seen_version = doc.read(cx).version();
            let subscription = cx.observe(&doc, |this, doc, cx| {
                if doc.read(cx).version() == this.seen_version {
                    return;
                }
                this.seen_version = doc.read(cx).version();
                this.follow = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(FOLLOW_DELAY).await;
                    this.update(cx, |this, cx| this.reload(cx)).ok();
                }));
            });
            (doc, subscription)
        });
    }

    /// Recomputes the diff (after staging, or to expand unchanged lines).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let (repo, source) = (self.repo.clone(), self.source.clone());
        let context = if self.expanded { usize::MAX } else { 3 };
        // The open document's text, unless it's a restored tab's stand-in
        // that hasn't read its file yet.
        let buffer = self.document.as_ref().map(|(doc, _)| doc.read(cx)).filter(|doc| !doc.is_stand_in()).map(|doc| doc.rope().to_string());
        self.loading = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    if let Some(images) = load_images(&repo, &source) {
                        return Loaded {
                            binary: true,
                            images: Some(images),
                            doc: DiffDocument::default(),
                            old_spans: Vec::new(),
                            new_spans: Vec::new(),
                            current: Arc::from(""),
                        };
                    }
                    let (base, current, path) = match &source {
                        DiffSource::Working { file, staged: true, .. } => (
                            git::head_text(&repo, &file.rel).unwrap_or_default(),
                            git::index_text(&repo, &file.rel).unwrap_or_default(),
                            file.path.clone(),
                        ),
                        DiffSource::Working { file, staged: false } => {
                            let base = if file.unstaged == Some(Change::Untracked) {
                                String::new()
                            } else {
                                git::index_text(&repo, &file.rel).unwrap_or_default()
                            };
                            // Read bytes, not UTF-8 text, so binary files are detected rather than empty.
                            let read = || std::fs::read(&file.path).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
                            let current = buffer.clone().or_else(read).unwrap_or_default();
                            (base, current, file.path.clone())
                        }
                        DiffSource::Commit { sha, file, .. } => {
                            let (base, current) = git::commit_file_texts(&repo, sha, file);
                            (base, current, repo.join(&file.rel))
                        }
                        DiffSource::History { path, text, .. } => {
                            let read = || std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
                            (text.to_string(), buffer.clone().or_else(read).unwrap_or_default(), path.clone())
                        }
                    };
                    if git::is_binary(&base) || git::is_binary(&current) {
                        return Loaded {
                            binary: true,
                            images: None,
                            doc: DiffDocument::default(),
                            old_spans: Vec::new(),
                            new_spans: Vec::new(),
                            current: current.into(),
                        };
                    }
                    let language = Language::from_path(&path);
                    Loaded {
                        binary: false,
                        images: None,
                        doc: git::diff_document(&base, &current, context),
                        old_spans: line_spans(language, &base),
                        new_spans: line_spans(language, &current),
                        current: current.into(),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.loaded = Some(Arc::new(loaded));
                // A change staged: the same place, by row (the rows are much the same).
                let top = this.list.logical_scroll_top();
                this.list.reset(this.rows_len());
                this.list.scroll_to(top);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The repository's status changed (a commit, staging, maybe outside
    /// Kuana): a working tree diff follows its file, and closes once the file
    /// has nothing left on its side, as after staging the whole file here.
    pub fn status_changed(&mut self, status: &git::RepoStatus, cx: &mut Context<Self>) {
        let DiffSource::Working { file, staged, .. } = &mut self.source else { return };
        let side = |f: &FileStatus| if *staged { f.staged } else { f.unstaged };
        match status.files.iter().find(|f| f.rel == file.rel && side(f).is_some()) {
            Some(now) => {
                *file = now.clone();
                self.reload(cx);
            }
            None => cx.emit(DiffViewEvent::Close),
        }
    }

    /// The working tree file an unstaged diff shows, which it reloads with.
    pub fn working_tree_file(&self) -> Option<&Path> {
        match &self.source {
            DiffSource::Working { file, staged: false, .. } => Some(&file.path),
            _ => None,
        }
    }

    /// The file this diff is of, for its tab.
    pub fn file(&self) -> PathBuf {
        self.file_label().0
    }

    fn file_label(&self) -> (PathBuf, String, Change) {
        match &self.source {
            DiffSource::Working { file, staged, .. } => {
                let change = if *staged { file.staged } else { file.unstaged }.unwrap_or(Change::Modified);
                (file.path.clone(), file.rel.clone(), change)
            }
            DiffSource::Commit { file, .. } => (self.repo.join(&file.rel), file.rel.clone(), file.change),
            DiffSource::History { path, .. } => {
                let rel = path.strip_prefix(&self.repo).unwrap_or(path).to_string_lossy().replace('\\', "/");
                (path.clone(), rel, Change::Modified)
            }
        }
    }

    /// The Stage / Unstage operation for a change, if this diff can do that.
    fn change_op(&self, ix: usize) -> Option<(GitOp, &'static str)> {
        let DiffSource::Working { file, staged, .. } = &self.source else { return None };
        let loaded = self.loaded.as_ref()?;
        let change = loaded.doc.changes.get(ix)?;
        let rel = file.rel.clone();
        Some(if *staged {
            // The index is the "after" side here.
            (GitOp::UnstageLines { rel, text: None, rows: change.new_rows.clone() }, "Unstage")
        } else {
            (GitOp::StageLines { rel, text: loaded.current.to_string(), rows: change.new_rows.clone() }, "Stage")
        })
    }

    fn rows_len(&self) -> usize {
        self.loaded.as_ref().map_or(0, |l| if self.shown_side_by_side { l.doc.split.len() } else { l.doc.unified.len() })
    }

    /// Row of the first line of change `ix` in the current layout.
    fn row_of_change(&self, ix: usize) -> Option<usize> {
        let loaded = self.loaded.as_ref()?;
        if self.shown_side_by_side {
            loaded.doc.split.iter().position(|r| r.change == Some((ix, true)))
        } else {
            loaded.doc.unified.iter().position(|l| l.change == Some((ix, true)))
        }
    }

    fn jump(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(count) = self.loaded.as_ref().map(|l| l.doc.changes.len()).filter(|n| *n > 0) else { return };
        let next = match (self.current_change, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(i), true) => (i + 1) % count,
            (Some(i), false) => (i + count - 1) % count,
        };
        self.current_change = Some(next);
        if let Some(row) = self.row_of_change(next) {
            self.list.scroll_to(ListOffset { item_ix: row.saturating_sub(3), offset_in_item: px(0.) });
        }
        cx.notify();
    }

    fn on_next(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump(true, cx);
    }

    fn on_previous(&mut self, _: &PreviousChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump(false, cx);
    }

    fn set_side_by_side(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.side_by_side != on {
            self.side_by_side = on;
            self.shown_side_by_side = on && fits_side_by_side(self.width.get());
            self.list.reset(self.rows_len());
            cx.emit(DiffViewEvent::Layout { side_by_side: on });
            cx.notify();
        }
    }
}

/// For an image file, both versions as temporary files (`None` for other files).
fn load_images(repo: &Path, source: &DiffSource) -> Option<ImageSides> {
    let (path, before, after) = match source {
        DiffSource::Working { file, staged: true, .. } => (
            file.path.clone(),
            git::blob_bytes(repo, &format!("HEAD:{}", file.rel)),
            git::blob_bytes(repo, &format!(":{}", file.rel)),
        ),
        DiffSource::Working { file, staged: false, .. } => {
            let before = if file.unstaged == Some(Change::Untracked) { None } else { git::blob_bytes(repo, &format!(":{}", file.rel)) };
            (file.path.clone(), before, std::fs::read(&file.path).ok())
        }
        // Kept as text: no pictures.
        DiffSource::History { .. } => return None,
        DiffSource::Commit { sha, file, .. } => {
            let old_rel = file.old_rel.as_deref().unwrap_or(&file.rel);
            let before = (file.change != Change::Added).then(|| git::blob_bytes(repo, &format!("{sha}^:{old_rel}"))).flatten();
            let after = (file.change != Change::Deleted).then(|| git::blob_bytes(repo, &format!("{sha}:{}", file.rel))).flatten();
            (repo.join(&file.rel), before, after)
        }
    };
    if !preview::is_image(&path) {
        return None;
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let side = |bytes: Option<Vec<u8>>| {
        let copy = preview::temp_copy(&bytes?, &extension)?;
        let mut info = ImageInfo::read(&copy);
        // The copy is named by its hash; show the real file's format.
        info.format = ImageInfo::read(&path).format;
        Some((copy, info))
    };
    Some(ImageSides { before: side(before), after: side(after) })
}

/// Syntax spans per line of `text`, in the tab-expanded offsets the diff uses.
fn line_spans(language: Language, text: &str) -> LineSpans {
    // Very large files aren't worth highlighting here.
    if text.len() > 2_000_000 {
        return Vec::new();
    }
    let rope = Rope::from_str(text);
    let syntax = Syntax::new(language, &rope);
    let spans = syntax.highlights(&rope, 0..text.len());
    let mut out = Vec::new();
    let mut ix = 0;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.len();
        let mut line_spans = Vec::new();
        while ix < spans.len() && spans[ix].0.end <= start {
            ix += 1;
        }
        for (range, kind) in spans[ix..].iter().take_while(|(r, _)| r.start < end) {
            let (s, e) = (range.start.max(start) - start, range.end.min(end) - start);
            if s < e {
                line_spans.push((expanded_offset(line, s)..expanded_offset(line, e), *kind));
            }
        }
        out.push(line_spans);
        start = end;
    }
    out
}

/// Where byte `offset` of `line` lands once tabs become four spaces.
fn expanded_offset(line: &str, offset: usize) -> usize {
    offset + 3 * line.as_bytes()[..offset.min(line.len())].iter().filter(|b| **b == b'\t').count()
}

/// How one side of a change is drawn, from the theme's Git color (as Zed
/// does): the color itself for line numbers and the bar, a light fill for its
/// rows and a stronger one for changed words. Fills are opaque (blended onto
/// the background), so text contrast on them can be measured.
#[derive(Clone, Copy)]
struct Tint {
    color: Hsla,
    row: Hsla,
    word: Hsla,
}

#[derive(Clone, Copy)]
struct Palette {
    /// The rows' background.
    base: Hsla,
    added: Tint,
    removed: Tint,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        let base = Hsla { a: 1., ..theme.background.into() };
        let dark = theme.appearance == Appearance::Dark;
        let tint = |color: Rgba| {
            let color = Hsla::from(color);
            // Git colors on dark themes are light (they color text): mixed in
            // strongly, they'd lift the fill toward the text's own lightness.
            // A deep shade of the hue keeps changed words dark, the text on them readable.
            let word = if dark { mix(base, Hsla { s: color.s.min(0.75), l: 0.4, ..color }, 0.45) } else { mix(base, color, 0.38) };
            Tint { color, row: mix(base, color, if dark { 0.12 } else { 0.16 }), word }
        };
        Palette { base, added: tint(theme.git_added), removed: tint(theme.git_deleted) }
    }
}

/// A line with syntax colors; on a changed line (`tint`), changed words on a
/// stronger fill and colors made readable on the fills (`contrast::keep_legible`).
fn styled_line(text: &str, spans: Option<&Vec<(Range<usize>, Highlight)>>, emphasis: &[Range<usize>], tint: Option<Tint>, base: Hsla, theme: &Theme) -> StyledText {
    let mut cuts = vec![0, text.len()];
    let spans: &[(Range<usize>, Highlight)] = spans.map_or(&[], |s| s.as_slice());
    for (r, _) in spans {
        cuts.extend([r.start.min(text.len()), r.end.min(text.len())]);
    }
    for r in emphasis {
        cuts.extend([r.start.min(text.len()), r.end.min(text.len())]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let highlights = cuts.windows(2).filter(|w| w[0] < w[1] && text.is_char_boundary(w[0]) && text.is_char_boundary(w[1])).map(|w| {
        let kind = spans.iter().find(|(r, _)| r.start <= w[0] && w[0] < r.end).map_or(Highlight::None, |(_, k)| *k);
        let color = theme.highlight_color(kind);
        let (color, background_color) = match tint {
            Some(tint) if emphasis.iter().any(|r| r.start <= w[0] && w[0] < r.end) => {
                (contrast::keep_legible(color, tint.word, base), Some(tint.word))
            }
            Some(tint) => (contrast::keep_legible(color, tint.row, base), None),
            None => (color, None),
        };
        (w[0]..w[1], HighlightStyle { color: Some(color), background_color, ..Default::default() })
    });
    StyledText::new(SharedString::from(text.to_string())).with_highlights(highlights.collect::<Vec<_>>())
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // Narrowed or widened past where halves fit: the other layout.
        let fits = fits_side_by_side(self.width.get());
        if self.shown_side_by_side != (self.side_by_side && fits) {
            self.shown_side_by_side = self.side_by_side && fits;
            self.list.reset(self.rows_len());
        }
        let (path, rel, change) = self.file_label();
        let file_icon = crate::ui::file_icon(&path, px(16.), &theme);
        let (added, removed) = self.loaded.as_ref().map_or((0, 0), |l| (l.doc.added, l.doc.removed));
        // Line counts and layouts mean nothing for a binary file.
        let binary = self.loaded.as_ref().is_some_and(|l| l.binary);
        let deleted = change == Change::Deleted;
        let icon_button = |id: &'static str, icon: Icon, tip: &'static str| {
            div()
                .id(id)
                .size(px(28.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .child(icon_el(icon, px(15.), theme.text_muted.into()))
        };
        let badge = match &self.source {
            DiffSource::Working { staged, .. } => format!("{} · {}", change.label(), if *staged { "Staged" } else { "Unstaged" }),
            DiffSource::Commit { sha, summary, .. } => format!("{} · {}", &sha[..sha.len().min(8)], summary),
            DiffSource::History { label, .. } => format!("Local History · {label}"),
        };
        // Side by side / Unified switch.
        let segment = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .px_2()
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .text_size(px(12.))
                .cursor_pointer()
                .when(on, |el| crate::ui::segment_on(el, &theme).text_color(theme.text))
                .when(!on, |el| el.text_color(theme.text_muted).hover(|s| s.text_color(theme.text)))
                .child(label)
        };

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(46.))
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(icon_el(Icon::GitCompare, px(15.), theme.text_muted.into()))
            .child(file_icon)
            // A long path (a file outside the project) gives way, so the
            // buttons at the right stay on screen.
            .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(rel.clone()))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .px_1p5()
                    .rounded(px(4.))
                    .bg(change_color(&theme, change).alpha(0.15))
                    .text_size(px(11.))
                    .text_color(change_color(&theme, change))
                    .child(badge),
            )
            .when(!binary, |el| {
                el.child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.git_added).child(format!("+{added}")))
                    .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.git_deleted).child(format!("−{removed}")))
            })
            .child(div().flex_1())
            .when(!binary && fits, |el| el.child(
                crate::ui::dock(&theme)
                    .child(segment("diff-split", "Side by Side", self.side_by_side).on_click(cx.listener(|this, _, _, cx| this.set_side_by_side(true, cx))))
                    .child(segment("diff-unified", "Unified", !self.side_by_side).on_click(cx.listener(|this, _, _, cx| this.set_side_by_side(false, cx)))),
            ))
            .when_some(
                match &self.source {
                    DiffSource::Working { file, staged, .. } => Some((file.clone(), *staged)),
                    DiffSource::Commit { .. } | DiffSource::History { .. } => None,
                },
                |el, (file, staged)| {
                    let rel = file.rel.clone();
                    el.when(!staged, |el| {
                        el.child(
                            text_button("diff-rollback", "Rollback", false, cx)
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::Run(GitOp::Discard(vec![file.clone()]))))),
                        )
                    })
                    .child(text_button("diff-stage", if staged { "Unstage File" } else { "Stage File" }, true, cx).on_click(cx.listener(
                        move |_, _, _, cx| {
                            let op = if staged { GitOp::Unstage(vec![rel.clone()]) } else { GitOp::Stage(vec![rel.clone()]) };
                            cx.emit(DiffViewEvent::Run(op));
                        },
                    )))
                },
            )
            .when_some(
                match &self.source {
                    DiffSource::History { path, text, .. } => Some((path.clone(), text.clone())),
                    _ => None,
                },
                |el, (path, text)| {
                    el.child(text_button("diff-restore", "Restore", true, cx).on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(DiffViewEvent::Restore { path: path.clone(), text: text.clone() })
                    })))
                },
            )
            .when(!deleted, |el| {
                let path = path.clone();
                el.child(icon_button("diff-open", Icon::File, "Open File").on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::OpenFile(path.clone())))))
            });

        let body = match self.loaded.clone() {
            None => div().flex_1().p_4().text_color(theme.text_dim).child("Loading diff…").into_any_element(),
            Some(loaded) if loaded.images.is_some() => self.render_images(loaded.images.as_ref().unwrap(), &theme).into_any_element(),
            Some(loaded) if loaded.binary => div()
                .flex_1()
                .p_4()
                .text_color(theme.text_dim)
                .child("Binary file: there's no text diff to show")
                .into_any_element(),
            Some(loaded) if loaded.doc.changes.is_empty() => {
                div().flex_1().p_4().text_color(theme.text_dim).child("No differences").into_any_element()
            }
            Some(loaded) => {
                let side_by_side = self.shown_side_by_side;
                let current_change = self.current_change;
                let row_theme = theme.clone();
                let rows = list(
                    self.list.clone(),
                    cx.processor(move |this, ix: usize, window, cx| {
                        this.render_row(&loaded, ix, side_by_side, current_change, &row_theme, window.scale_factor(), cx)
                    }),
                )
                .flex_1()
                .w_full()
                .pt_1()
                .pb(px(10.))
                .bg(theme.background);
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(rows)
                    .child(crate::ui::scrollbar("diff-scrollbar", &self.list, cx))
                    .into_any_element()
            }
        };

        let width = self.width.clone();
        div()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.elevated)
            // It sits in a pane's island: follow the island's rounded corners
            // inside its inset.
            .rounded(px(crate::ui::area_radius(&theme) - crate::ui::area_inset(&theme)))
            .key_context("DiffView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_previous))
            .size_full()
            .child(header)
            .child(body)
            // Its width, for the layout: a frame again when halves stop or start fitting.
            .child(
                canvas(
                    move |bounds, window, _| {
                        let was = width.replace(bounds.size.width);
                        // (A refresh while drawing is ignored; this asks for the next frame.)
                        if fits_side_by_side(was) != fits_side_by_side(bounds.size.width) {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}

impl DiffView {
    /// Before and after, side by side, scaled down to fit.
    fn render_images(&self, sides: &ImageSides, theme: &Theme) -> impl IntoElement {
        let side = |title: &'static str, version: &Option<(PathBuf, ImageInfo)>, missing: &'static str| {
            let column = div().flex_1().min_w_0().h_full().flex().flex_col().gap_2().p_3();
            let header = div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(12.))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child(title))
                .when_some(version.as_ref(), |el, (_, info)| el.child(div().text_color(theme.text_dim).child(info.label())));
            let picture = match version {
                Some((path, _)) => div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .rounded(px(8.))
                    .bg(theme.background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(gpui::img(path.clone()).size_full().object_fit(gpui::ObjectFit::ScaleDown)),
                None => div()
                    .flex_1()
                    .min_h_0()
                    .rounded(px(8.))
                    .bg(theme.background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(theme.text_dim)
                    .child(missing),
            };
            column.child(header).child(picture)
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(side("Before", &sides.before, "No previous version (new file)"))
            .child(div().w(px(1.)).h_full().bg(theme.border_subtle))
            .child(side("After", &sides.after, "Deleted"))
    }

    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &self,
        loaded: &Loaded,
        ix: usize,
        side_by_side: bool,
        current_change: Option<usize>,
        theme: &Theme,
        scale: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let palette = Palette::new(theme);
        // A changed line's number takes its side's color, as in Zed.
        let number = |n: Option<usize>, tint: Option<Tint>| {
            div()
                .w(px(44.))
                .pr_2()
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_color(tint.map_or(theme.line_number.into(), |t| t.color))
                .child(n.map(|n| n.to_string()).unwrap_or_default())
        };
        let bar = |tint: Tint| div().absolute().left_0().top_0().bottom_0().w(px(BAR_WIDTH)).bg(tint.color);
        // The editor's font, set on each row: a `list`'s rows don't get the
        // list's text style. Wrapped lines stack at the row's height, so a
        // row is a whole number of lines (the stripes rely on it).
        let row = div()
            .id(ix)
            .relative()
            .w_full()
            .flex()
            .min_h(px(row_height(theme)))
            .font_family(theme.mono_font.clone())
            .text_size(px(theme.editor_font_size))
            .line_height(px(row_height(theme)));

        let (kind, label, change) = if side_by_side {
            let r = &loaded.doc.split[ix];
            (r.kind, r.label.clone(), r.change)
        } else {
            let l = &loaded.doc.unified[ix];
            (l.kind, l.text.clone(), l.change)
        };
        if kind == DiffLineKind::Skipped {
            return row
                .h(px(row_height(theme)))
                .justify_center()
                .items_center()
                .bg(theme.panel)
                .text_color(theme.text_dim)
                .font_family(theme.ui_font.clone())
                .text_size(px(11.5))
                .cursor_pointer()
                .hover(|s| s.text_color(theme.text_muted))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.expanded = true;
                    this.reload(cx);
                }))
                .child(format!("⋯ {label} (click to show all)"))
                .into_any_element();
        }

        // A change's first row has its Stage / Unstage button at the right:
        // the text under it wraps before it.
        let button = match change {
            Some((c, true)) => self.change_op(c),
            _ => None,
        };
        let text = |line: StyledText, room: bool| div().flex_1().min_w_0().when(room, |el| el.pr(px(CHANGE_BUTTON_ROOM))).child(line);
        let row = if side_by_side {
            let r = &loaded.doc.split[ix];
            let half = |side: Option<&git::DiffSide>, spans: &LineSpans, tint: Option<Tint>, room: bool| {
                let cell = div().relative().flex_1().min_w_0().flex().overflow_hidden();
                match side {
                    Some(s) => cell
                        .when_some(tint, |el, t| el.bg(t.row).child(bar(t)))
                        .child(number(Some(s.no), tint))
                        .child(text(styled_line(&s.text, spans.get(s.no - 1), &s.emphasis, tint, palette.base, theme), room)),
                    // The other side has no line here: stripes, as in Zed.
                    None => cell.bg(stripes(theme, scale)),
                }
            };
            let changed = r.kind != DiffLineKind::Context;
            row.child(half(r.left.as_ref(), &loaded.old_spans, changed.then_some(palette.removed), false))
                .child(div().w(px(1.)).h_full().flex_shrink_0().bg(theme.border))
                .child(half(r.right.as_ref(), &loaded.new_spans, changed.then_some(palette.added), button.is_some()))
        } else {
            let l = &loaded.doc.unified[ix];
            let (tint, sign, spans): (Option<Tint>, &str, Option<&Vec<_>>) = match l.kind {
                DiffLineKind::Added => (Some(palette.added), "+", l.new_no.and_then(|n| loaded.new_spans.get(n - 1))),
                DiffLineKind::Removed => (Some(palette.removed), "−", l.old_no.and_then(|n| loaded.old_spans.get(n - 1))),
                _ => (None, " ", l.new_no.and_then(|n| loaded.new_spans.get(n - 1))),
            };
            row.when_some(tint, |el, t| el.bg(t.row).child(bar(t)))
                .child(number(l.old_no, tint))
                .child(number(l.new_no, tint))
                .child(div().w(px(16.)).flex_shrink_0().text_color(tint.map_or(theme.text_dim.into(), |t| t.color)).child(sign.to_string()))
                .child(text(styled_line(&l.text, spans, &l.emphasis, tint, palette.base, theme), button.is_some()))
        };

        // The first row of a change: a marker for F7, and Stage / Unstage.
        let row = match change {
            Some((c, true)) => {
                let row = row.when(current_change == Some(c), |el| el.border_t_1().border_color(theme.accent));
                match button {
                    Some((op, label)) => row.child(
                        div()
                            .id(("change-op", c))
                            .absolute()
                            .right(px(8.))
                            .top(px(1.))
                            .h(px(18.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(theme.border)
                            .font_family(theme.ui_font.clone())
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover_fill(theme.elevated, theme.hover)
                            .hover(|s| s.text_color(theme.text))
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::Run(op.clone()))))
                            .child(label),
                    ),
                    None => row,
                }
            }
            _ => row,
        };
        row.into_any_element()
    }
}
