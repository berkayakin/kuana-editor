//! The text editor view: multi-cursor editing, keyboard/mouse/IME input and a
//! custom element that paints only the visible lines straight to the GPU.

mod fold;
mod format;
mod conflict;
pub(crate) mod derived;
mod lsp;
mod markers;
mod minimap;
mod run;
mod snippet;
mod sticky;
mod transform;
mod unloaded;
mod vim;

pub use lsp::EditorLspEvent;
pub use run::{EditorBreakpointMenu, EditorBreakpointsChanged, EditorRunEvent};
pub use sticky::Crumb;
pub use unloaded::{EditorLoaded, Unloaded};

use std::{collections::HashMap, ops::Range, sync::Arc, time::Duration};

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    FontStyle,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    ScrollWheelEvent, ShapedLine, SharedString, Style, Task, TextRun, UTF16Selection,
    UnderlineStyle, Window, actions, div, fill, font, point, prelude::*, px, relative, size,
};

use crate::{
    assets::Icon,
    conflict::Resolution,
    document::{Document, EditKind, Selection},
    document_diff::DocumentDiff,
    git::{self, BlameLine, Hunk, HunkKind},
    lint::{self, Severity, external::ExternalLinter},
    search::SearchMatches,
    settings::{FileSettings, Settings, ShowWhitespace},
    theme::Theme,
    ui::HoverFill,
    wrap::{self, Break, WrapKey, WrapMap},
};

const SCROLLBAR_WIDTH: f32 = 12.0;
const TEXT_PADDING: f32 = 6.0;
/// Distance of the git change bar's center from the gutter's right edge.
const GIT_BAR_OFFSET: f32 = 8.0;
/// The fold arrow sits this far left of the gutter's right edge (to its right edge).
const FOLD_ZONE_RIGHT: f32 = 14.0;
const FOLD_ICON: f32 = 12.0;
/// The ▶ for runnable lines, at the gutter's left edge (where the problem dot goes).
const RUN_ICON: f32 = 11.0;

actions!(
    editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        MoveLineStart,
        MoveLineEnd,
        SelectLineStart,
        SelectLineEnd,
        MoveDocStart,
        MoveDocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        Newline,
        NewlineBelow,
        Indent,
        Outdent,
        SelectAll,
        SelectLine,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        ToggleComment,
        DuplicateLine,
        MoveLineUp,
        MoveLineDown,
        SelectNextOccurrence,
        AddCursorAbove,
        AddCursorBelow,
        Cancel,
        ShowCharacterPalette,
        DeleteLine,
        ExtendSelection,
        ShrinkSelection,
        UnselectOccurrence,
        SelectAllOccurrences,
        ToggleCase,
        ConvertToUpperCase,
        ConvertToLowerCase,
        ConvertToTitleCase,
        ConvertToSnakeCase,
        ConvertToKebabCase,
        ConvertToCamelCase,
        ConvertToPascalCase,
        SortLinesAscending,
        SortLinesDescending,
        ReverseLines,
        RemoveDuplicateLines,
        JoinLines,
        NewlineAbove,
        ToggleBlockComment,
        RollbackHunk,
        StageHunk,
        UnstageHunk,
        AcceptCurrent,
        AcceptIncoming,
        AcceptBoth,
        NextChange,
        PreviousChange,
        NextProblem,
        PreviousProblem,
        ToggleInlineBlame,
        ToggleSoftWrap,
        ToggleInlayHints,
        ToggleMarkdownPreview,
        ShowCompletions,
        QuickDocumentation,
        GoToDeclaration,
        GoToImplementation,
        GoToTypeDefinition,
        FindUsages,
        CallHierarchy,
        TypeHierarchy,
        RenameSymbol,
        ShowContextActions,
        ReformatCode,
        CollapseRegion,
        ExpandRegion,
        CollapseAllRegions,
        ExpandAllRegions,
        RunAtCaret,
        DebugAtCaret,
        ToggleBreakpoint,
        /// ⌘↩ / Ctrl+Enter: "new line below" in code, "submit" in text fields.
        Submit,
    ]
);

/// Where an open file lives in a Git repository.
#[derive(Clone, Debug, PartialEq)]
pub struct GitFile {
    pub root: std::path::PathBuf,
    pub rel: String,
    /// The repository's `user.name`, shown as "You" in blame.
    pub user_name: Option<String>,
    /// HEAD's commit when the file's diff base was read: blame is read once
    /// per file and HEAD.
    pub head: Option<String>,
}

/// A whole file's blame, read for one version of its text; a row of a
/// later version is followed back to it through the document's line edits.
struct Blame {
    git_file: GitFile,
    doc: u64,
    version: u64,
    text: ropey::Rope,
    lines: git::FileBlame,
}

impl Blame {
    /// The commit row `row` of the text as it is now came from: a line
    /// edited since, unless its text is the same again, isn't committed.
    /// Some(None): nothing to show; None: the edits since aren't kept (the
    /// file is blamed again).
    fn line(&self, row: usize, doc: &Document) -> Option<Option<BlameLine>> {
        let edits: Vec<crate::document::LineEdit> = doc.line_edits_since(self.version)?.copied().collect();
        let (mut old, mut edited) = (row, false);
        for edit in edits.iter().rev() {
            if old < edit.row {
                continue;
            }
            if old >= edit.row + edit.new_rows {
                old = old - edit.new_rows + edit.old_rows;
                continue;
            }
            // Among the lines an edit changed: the one as far from their
            // start (a line break added at the end), or from their end.
            edited = true;
            let (from_start, from_end) = (old - edit.row, edit.row + edit.new_rows - 1 - old);
            if from_start < edit.old_rows {
                old = edit.row + from_start;
            } else if from_end < edit.old_rows {
                old = edit.row + edit.old_rows - 1 - from_end;
            } else {
                return Some(Some(BlameLine::uncommitted()));
            }
        }
        // The same text, line break or not (a last line that got one).
        fn text(line: ropey::RopeSlice<'_>) -> ropey::RopeSlice<'_> {
            match line.len_chars() {
                n if n > 0 && line.char(n - 1) == '\n' => line.slice(..n - 1),
                _ => line,
            }
        }
        if edited && (old >= self.text.len_lines() || text(doc.rope().line(row)) != text(self.text.line(old))) {
            return Some(Some(BlameLine::uncommitted()));
        }
        Some(self.lines.line(old).cloned())
    }
}

/// A Git operation started from the editor (staging a change).
pub struct EditorGitEvent(pub crate::git_panel::GitOp);

impl EventEmitter<EditorGitEvent> for Editor {}

/// App-wide toggle for inline blame.
pub struct InlineBlame(pub bool);

impl gpui::Global for InlineBlame {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    /// A full code editor with gutter, linting and scrollbar.
    Full,
    /// A one-line text field, used by search inputs.
    SingleLine,
    /// A plain multi-line text field without code chrome (commit messages).
    MultiLine,
}

pub enum EditorEvent {
    Edited,
}

impl EventEmitter<EditorEvent> for Editor {}

/// A right-click in the code editor, at this window position. Separate from
/// [`EditorEvent`] because many views treat any `EditorEvent` as an edit.
pub struct EditorContextMenu(pub Point<Pixels>);

impl EventEmitter<EditorContextMenu> for Editor {}

/// The Markdown preview toggle was clicked: the workspace opens or closes
/// the document's preview (`ToggleMarkdownPreview` reaches it directly).
pub struct EditorPreviewToggle;

impl EventEmitter<EditorPreviewToggle> for Editor {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Char,
    Word,
    Line,
}

struct Drag {
    mode: DragMode,
    /// The unit (char/word/line range) where the drag started.
    origin: Range<usize>,
    /// With Alt: the row and display column it started at. Moving the
    /// pointer then selects a box (a selection per line) instead.
    column_from: Option<(usize, usize)>,
}

/// Geometry from the last frame, used for hit-testing and IME positioning.
#[derive(Clone)]
struct LayoutSnapshot {
    bounds: Bounds<Pixels>,
    text_left: Pixels,
    line_height: Pixels,
    char_width: Pixels,
    gutter_width: Pixels,
    rows_per_page: usize,
    /// Text width available to soft-wrapped rows.
    wrap_width: Pixels,
}

/// The run targets and test results `Editor::shown_run_rows` was made from,
/// and its rows.
type RunRows = (Option<(Arc<Vec<crate::run::RunTarget>>, Arc<crate::test_run::Statuses>)>, Arc<Vec<(usize, Option<crate::test_run::TestStatus>)>>);

pub struct Editor {
    pub doc: Entity<Document>,
    mode: EditorMode,
    placeholder: SharedString,
    search_matches: SearchMatches,
    active_match: Option<usize>,
    /// Selections before each ExtendSelection, for ShrinkSelection.
    expand_stack: Vec<Vec<Selection>>,
    expanding: bool,
    /// The selections Add Selection for Next Occurrence made, when it
    /// started from a caret: while they're still the selections, it adds
    /// only whole words (case-sensitively, as VS Code and Zed do).
    occurrence_words: Option<Vec<Selection>>,
    /// Sorted and apart (see `set_selections`), so a frame finds the ones on
    /// screen by binary search.
    selections: Vec<Selection>,
    /// Changes whenever `selections` do, for what's drawn from all of them.
    selections_generation: u64,
    /// `selected_chars` for (document, version, selections generation).
    selected_chars: std::cell::Cell<Option<((u64, u64, u64), usize)>>,
    /// Index of the most recently added selection.
    primary: usize,
    goal_x: Option<Pixels>,
    focus_handle: FocusHandle,
    scroll: Point<Pixels>,
    max_line_width: Pixels,
    autoscroll: bool,
    center_on_next_frame: bool,
    /// A buffer line to put at the top on the next frame (a restored tab's view).
    scroll_to_line: Option<usize>,
    /// After a reload: the line that was at the top of the view (its number
    /// then) and where its text is now, for `sync_wrap` to keep it there.
    reloaded_top: Option<(usize, usize)>,
    /// Like autoscroll, but centers the cursor when it is off screen.
    reveal_primary: bool,
    /// Vim mode's state while the `vim_mode` setting is on.
    vim: Option<vim::Vim>,
    layout: Option<LayoutSnapshot>,
    drag: Option<Drag>,
    scrollbar_drag: Option<Pixels>,
    /// The pointer is over the scrollbar (it shows while it is).
    scrollbar_hovered: bool,
    /// The thumb fades out when the text stops scrolling; the track's markers stay.
    scrollbar_fade: crate::ui::ScrollbarFade,
    minimap_cache: minimap::MinimapCache,
    /// The scrollbar's markers, kept until what they show changes.
    markers: markers::Markers,
    /// Block rows for what they were built from (see `blocks`).
    blocks_cache: Option<BlocksCache>,
    /// Finds the diagnostics reaching into what's on screen.
    reach: Reach,
    /// Rows with a gutter ▶, for the run targets they're from.
    run_rows: RunRows,
    /// The last results of the workspace's test runs, for the ▶'s pass / fail mark.
    test_statuses: Arc<crate::test_run::Statuses>,
    /// See `row_problems`, for a document state.
    row_problems: (Option<(u64, u64, u64)>, Arc<Vec<(usize, Option<usize>, usize)>>),
    marked_range: Option<Range<usize>>,
    indent_unit: String,
    /// Display width of a tab character (and of an indent level with tabs).
    tab_size: usize,
    /// Resolved settings for this file (see `apply_settings`).
    file_settings: FileSettings,
    /// What `apply_settings` last applied (None: not yet).
    applied_settings: Option<FileSettings>,
    cursor_visible: bool,
    blink_epoch: usize,
    blink_task: Option<Task<()>>,
    lint_task: Option<Task<()>>,
    linted_version: Option<u64>,
    external: Option<ExternalLinter>,
    external_error: Option<String>,
    external_task: Option<Task<()>>,
    /// The document's Git changes, shared with its other editors (see
    /// `DocumentDiff`); the fields below mirror it as of its last notify.
    git_diff: Option<(Entity<DocumentDiff>, gpui::Subscription)>,
    git_file: Option<GitFile>,
    /// There's a HEAD text the changes are shown against.
    has_diff_base: bool,
    hunks: Arc<Vec<Hunk>>,
    /// Per hunk: whether it's fully staged.
    hunk_staged: Arc<Vec<bool>>,
    /// Index into `hunks` of the change popover that is open.
    open_hunk: Option<usize>,
    /// Blame shown for the caret's line: its row, the version, the commit.
    blame: Option<(usize, u64, BlameLine)>,
    blame_request: Option<(usize, u64)>,
    /// The whole file's blame, for its file and HEAD.
    file_blame: Option<Blame>,
    /// The whole-file blame being read, for which file and HEAD and
    /// document; dropping it kills git.
    blame_task: Option<((GitFile, u64), Task<()>, git::KillOnDrop)>,
    soft_wrap: bool,
    /// Line breaks for soft wrap, recomputed lazily (see `sync_wrap`).
    wrap: Option<WrapMap>,
    /// Language server state: completion, hover, signature help.
    lsp: lsp::EditorLsp,
    /// A parse finishing in the background (see `schedule_parse`).
    parse_task: Option<Task<()>>,
    /// The file being read again in the background (`reload_if_changed`),
    /// and whether to read it once more after (asked again meanwhile).
    reload_task: Option<Task<()>>,
    reload_again: bool,
    /// The carets' bracket pairs, for a tree version and caret offsets.
    bracket_cache: Option<((u64, Vec<usize>), Arc<Vec<Range<usize>>>)>,
    /// Highlights of what was on screen: for a tree version, language and ranges.
    highlight_cache: Option<((u64, crate::syntax::Language, Vec<Range<usize>>), Arc<Vec<(Range<usize>, crate::syntax::Highlight)>>)>,
    /// Fold regions for a (version, tab size, language).
    fold_regions: derived::Derived<(u64, usize, crate::syntax::Language), Vec<crate::fold::FoldRegion>>,
    /// `⋯` placeholders on screen last frame, for clicks: (bounds, fold start).
    fold_pills: Vec<(Bounds<Pixels>, usize)>,
    /// This editor's folds: another editor of the file folds apart.
    folds: crate::fold::Folds,
    /// The tab stops of a snippet being filled in here (they follow edits).
    snippet: Option<crate::snippets::SnippetStops>,
    /// File Structure's symbols for a document version (breadcrumbs, sticky scroll).
    outline: derived::Derived<(u64, crate::syntax::Language), Vec<crate::syntax::Symbol>>,
    /// Markdown: a preview of the document is open (the workspace says so),
    /// which the preview toggle shows.
    previewed: bool,
    /// Pinned lines of sticky scroll, as last painted: bounds and where to go.
    sticky_rows: Vec<(Bounds<Pixels>, usize)>,
    /// `breadcrumbs` for an outline and caret.
    breadcrumbs: Option<(Arc<Vec<crate::syntax::Symbol>>, usize, Vec<Crumb>)>,
    /// The outline's rows for an outline and version (see `outline_rows`).
    sticky_spans: (Option<(Arc<Vec<crate::syntax::Symbol>>, u64)>, Arc<Vec<(usize, usize, usize)>>),
    /// The pointer is over the gutter, so fold arrows show.
    gutter_hovered: bool,
    /// Runnable targets for a version and path (gutter ▶).
    run_targets: derived::Derived<(u64, Option<std::path::PathBuf>, crate::syntax::Language), Vec<crate::run::RunTarget>>,
    /// Merge conflicts for a version.
    conflicts: derived::Derived<u64, Vec<crate::conflict::Conflict>>,
    /// Accept labels on screen last frame: (bounds, conflict start row, choice).
    conflict_hits: Vec<(Bounds<Pixels>, usize, crate::conflict::Resolution)>,
    /// Code lens titles on screen last frame, to click.
    lens_hits: Vec<(Bounds<Pixels>, crate::document::CodeLens)>,
    /// A restored tab whose file isn't read yet (`load` reads it).
    unloaded: Option<Unloaded>,
    /// The document's id and version as this editor last saw them (its own
    /// edits included): another editor's edits move its carets from there.
    seen: (u64, u64),
    _doc_observer: gpui::Subscription,
    /// A split of a restored tab not read yet: that tab reads the file.
    waiting: bool,
    /// A waiting split's own caret and top line from the session, set once
    /// its text is there.
    waiting_view: Option<(usize, Option<usize>)>,
}

impl Editor {
    pub fn new(doc: Document, cx: &mut Context<Self>) -> Self {
        Self::with_document(cx.new(|_| doc), cx)
    }

    /// An editor for a document entity, maybe one other editors show too.
    pub fn with_document(doc: Entity<Document>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::blank_in(doc, cx);
        this.apply_settings(cx);
        this.detect_external_linter(cx);
        this.schedule_lint(Duration::ZERO, cx);
        this.run_external_linter(cx);
        this.schedule_parse(cx);
        this
    }

    /// The editor's state for `doc`, before anything reads settings or
    /// starts work on the text.
    fn blank(doc: Document, cx: &mut Context<Self>) -> Self {
        Self::blank_in(cx.new(|_| doc), cx)
    }

    fn blank_in(doc: Entity<Document>, cx: &mut Context<Self>) -> Self {
        let (file_settings, seen) = {
            let d = doc.read(cx);
            (Settings::get(cx).for_file(d.language, d.path.as_deref()), (d.id(), d.version()))
        };
        let doc_observer = cx.observe(&doc, |editor, _, cx| editor.document_changed(cx));
        Self {
            doc,
            seen,
            _doc_observer: doc_observer,
            waiting: false,
            waiting_view: None,
            mode: EditorMode::Full,
            placeholder: SharedString::default(),
            search_matches: SearchMatches::default(),
            active_match: None,
            expand_stack: Vec::new(),
            expanding: false,
            occurrence_words: None,
            selections: vec![Selection::cursor(0)],
            selections_generation: 0,
            selected_chars: Default::default(),
            primary: 0,
            goal_x: None,
            focus_handle: cx.focus_handle(),
            scroll: Point::default(),
            max_line_width: px(0.),
            autoscroll: false,
            center_on_next_frame: false,
            scroll_to_line: None,
            reloaded_top: None,
            reveal_primary: false,
            vim: None,
            layout: None,
            drag: None,
            scrollbar_drag: None,
            scrollbar_hovered: false,
            scrollbar_fade: Default::default(),
            minimap_cache: Default::default(),
            markers: Default::default(),
            blocks_cache: None,
            reach: Reach::default(),
            run_rows: (None, Arc::default()),
            test_statuses: Arc::default(),
            row_problems: (None, Arc::default()),
            marked_range: None,
            indent_unit: String::new(),
            tab_size: file_settings.tab_size,
            file_settings,
            applied_settings: None,
            cursor_visible: true,
            blink_epoch: 0,
            blink_task: None,
            lint_task: None,
            linted_version: None,
            external: None,
            external_error: None,
            external_task: None,
            git_diff: None,
            git_file: None,
            has_diff_base: false,
            hunks: Arc::new(Vec::new()),
            hunk_staged: Arc::new(Vec::new()),
            open_hunk: None,
            blame: None,
            blame_request: None,
            file_blame: None,
            blame_task: None,
            soft_wrap: false,
            wrap: None,
            lsp: lsp::EditorLsp::default(),
            highlight_cache: None,
            parse_task: None,
            reload_task: None,
            reload_again: false,
            bracket_cache: None,
            fold_regions: derived::Derived::new(),
            fold_pills: Vec::new(),
            folds: Default::default(),
            snippet: None,
            outline: derived::Derived::new(),
            previewed: false,
            sticky_rows: Vec::new(),
            breadcrumbs: None,
            sticky_spans: (None, Arc::default()),
            gutter_hovered: false,
            run_targets: derived::Derived::new(),
            conflicts: derived::Derived::new(),
            conflict_hits: Vec::new(),
            lens_hits: Vec::new(),
            unloaded: None,
        }
    }

    /// A second editor for the same document (a split): it starts where
    /// this one is, then has carets and scrolling of its own.
    pub fn view_of(other: &Entity<Editor>, cx: &mut Context<Self>) -> Self {
        let (doc, selections, scroll, folds) = {
            let other = other.read(cx);
            (other.doc.clone(), other.selections.clone(), other.scroll, other.folds.clone())
        };
        let waiting = doc.read(cx).is_stand_in();
        let mut this = Self::blank_in(doc, cx);
        this.waiting = waiting;
        this.apply_settings(cx);
        this.detect_external_linter(cx);
        this.selections = selections;
        this.selections_generation += 1;
        this.scroll = scroll;
        // It starts folded like the view it splits from, then folds apart.
        this.folds = folds;
        this
    }

    /// Whether `other` shows the same document.
    pub fn shares_document(&self, other: &Editor) -> bool {
        self.doc == other.doc
    }

    /// Changes the text (or replaces it). This editor places its own carets,
    /// so its change isn't replayed onto them when the document notifies.
    fn change_doc<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Document) -> R) -> R {
        let result = f(&mut self.doc.as_mut(cx));
        let doc = self.doc.read(cx);
        if doc.id() == self.seen.0 {
            self.follow_edits(self.seen.1, cx);
        }
        let doc = self.doc.read(cx);
        self.seen = (doc.id(), doc.version());
        result
    }

    /// Moves what this editor keeps in the text (folds, a snippet's stops)
    /// across the document's edits since `version`.
    fn follow_edits(&mut self, version: u64, cx: &App) {
        let doc = self.doc.read(cx);
        self.folds.follow(doc, version);
        if let Some(snippet) = &mut self.snippet {
            let kept = doc.patches_since(version).is_some_and(|mut patches| patches.all(|patch| snippet.shift(patch)));
            if !kept {
                self.snippet = None;
            }
        }
    }

    /// The document changed: another editor of it edited (carets here move
    /// with its edits), or it was replaced as a whole (an unloaded tab of it
    /// read its file), or something it holds (diagnostics) did.
    fn document_changed(&mut self, cx: &mut Context<Self>) {
        let (id, version) = {
            let doc = self.doc.read(cx);
            (doc.id(), doc.version())
        };
        if (id, version) == self.seen {
            cx.notify();
            return;
        }
        if id != self.seen.0 {
            // Fresh state around the new text, outside this observer's callback.
            self.seen = (id, version);
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |editor, cx| {
                    let (focus_handle, waited, view) = (editor.focus_handle.clone(), editor.waiting, editor.waiting_view.take());
                    let git_diff = editor.git_diff.take();
                    *editor = Self::with_document(editor.doc.clone(), cx);
                    editor.focus_handle = focus_handle;
                    if let Some((diff, _)) = git_diff {
                        editor.set_git_diff(diff, cx);
                    }
                    // A split of a restored tab: its text is here now too.
                    if waited {
                        if let Some((cursor, top_line)) = view {
                            editor.restore_view(cursor, top_line, cx);
                        }
                        cx.emit(unloaded::EditorLoaded { error: None });
                    }
                    cx.notify();
                })
                .ok();
            });
            return;
        }
        let selections = self.moved_since(self.seen.1, &self.selections, cx);
        self.selections = self.clamp_selections(selections, cx);
        self.selections_generation += 1;
        self.follow_edits(self.seen.1, cx);
        self.seen = (id, version);
        cx.notify();
    }

    /// `selections` moved through the document's edits since `version`
    /// (left as they are when those aren't kept any more).
    fn moved_since(&self, version: u64, selections: &[Selection], cx: &App) -> Vec<Selection> {
        let Some(patches) = self.doc.read(cx).patches_since(version) else { return selections.to_vec() };
        let patches: Vec<&crate::document::Patch> = patches.collect();
        let map = |offset: usize| patches.iter().fold(offset, |offset, patch| patch.map(offset));
        selections.iter().map(|s| Selection { anchor: map(s.anchor), head: map(s.head) }).collect()
    }

    /// Finishes in the background a parse that ran out of time on the UI
    /// thread (a big file opening, an edit in one). One at a time: a parse
    /// that ends behind the edits made meanwhile starts the next.
    pub(super) fn schedule_parse(&mut self, cx: &mut Context<Self>) {
        if self.parse_task.is_some() {
            return;
        }
        let Some((version, job)) = self.doc.read(cx).parse_job() else { return };
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let syntax = cx.background_executor().spawn(async move { job() }).await;
            this.update(cx, |editor, cx| {
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = editor.parse_task.take() {
                    task.detach();
                }
                if editor.doc.as_mut(cx).finish_parse(version, syntax) {
                    editor.schedule_parse(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn single_line(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(Document::empty(), cx);
        this.mode = EditorMode::SingleLine;
        this.soft_wrap = false;
        this.placeholder = placeholder.into();
        this
    }

    pub fn multi_line(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::single_line(placeholder, cx);
        this.mode = EditorMode::MultiLine;
        this.soft_wrap = true;
        this
    }

    pub fn is_single_line(&self) -> bool {
        self.mode == EditorMode::SingleLine
    }

    /// Text fields (single- or multi-line) have no gutter, linting or scrollbar.
    fn is_chromeless(&self) -> bool {
        self.mode != EditorMode::Full
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_chromeless() {
            cx.propagate();
        } else {
            self.newline_below(&NewlineBelow, window, cx);
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.doc.read(cx).rope().to_string()
    }

    /// Replaces the whole text (as one undoable edit) and selects it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = if self.is_single_line() { text.replace('\n', " ") } else { text.to_string() };
        let before = self.selections.clone();
        let len = self.doc.read(cx).len();
        self.change_doc(cx, |doc| doc.edit(vec![(0..len, text)], &before, EditKind::Other));
        let len = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor: 0, head: len }], 0, cx);
        self.after_edit(cx);
    }

    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        let len = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor: 0, head: len }], 0, cx);
    }

    /// Selects a range and scrolls it to the middle of the view if needed.
    pub fn select_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let (s, e) = (self.doc.read(cx).clip(range.start), self.doc.read(cx).clip(range.end));
        self.set_selections(vec![Selection { anchor: s, head: e }], 0, cx);
        self.reveal_primary = true;
    }

    pub fn set_search_highlights(&mut self, matches: SearchMatches, active: Option<usize>, cx: &mut Context<Self>) {
        self.search_matches = matches;
        self.active_match = active;
        cx.notify();
    }

    /// Replaces many ranges in one undoable step (used by Replace All).
    pub fn replace_ranges(&mut self, edits: Vec<(Range<usize>, String)>, cx: &mut Context<Self>) {
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
        }
    }

    /// The project's ESLint, which runs its JavaScript config: only in a
    /// trusted project.
    pub fn detect_external_linter(&mut self, cx: &App) {
        self.external = match (&self.doc.read(cx).path, self.doc.read(cx).language) {
            (Some(path), _) if !crate::trust::is_trusted(path) => None,
            (
                Some(path),
                crate::syntax::Language::JavaScript | crate::syntax::Language::TypeScript | crate::syntax::Language::Tsx,
            ) => lint::external::find_for(path),
            _ => None,
        };
        self.external_error = None;
    }

    /// Name of the project linter in use and its last error, if any.
    pub fn external_linter_status(&self) -> Option<(&'static str, Option<&str>)> {
        self.external
            .as_ref()
            .map(|l| (l.name, self.external_error.as_deref()))
    }

    fn run_external_linter(&mut self, cx: &mut Context<Self>) {
        let (Some(linter), Some(path)) = (self.external.clone(), self.doc.read(cx).path.clone()) else {
            return;
        };
        let rope = self.doc.read(cx).rope().clone();
        let version = self.doc.read(cx).version();
        self.external_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { linter.run(&path, &rope) })
                .await;
            this.update(cx, |editor, cx| {
                let was_ok = editor.external_error.is_none();
                match result {
                    Ok(diagnostics) => {
                        editor.external_error = None;
                        if editor.doc.read(cx).version() == version {
                            editor.doc.as_mut(cx).set_external_diagnostics(diagnostics);
                        }
                    }
                    Err(err) => {
                        editor.external_error = Some(err);
                        editor.doc.as_mut(cx).set_external_diagnostics(Vec::new());
                    }
                }
                // Built-in rules fill in whenever the external linter isn't working.
                if was_ok != editor.external_error.is_none() {
                    editor.linted_version = None;
                    editor.schedule_lint(Duration::ZERO, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    /// The selections touching bytes `range` (an end on either of its ends
    /// too), by binary search: they're sorted and apart, and there can be
    /// thousands.
    fn selections_touching(&self, range: std::ops::RangeInclusive<usize>) -> &[Selection] {
        let first = self.selections.partition_point(|s| s.range().end < *range.start());
        let len = self.selections[first..].partition_point(|s| s.range().start <= *range.end());
        &self.selections[first..first + len]
    }

    pub fn primary_selection(&self) -> Selection {
        self.selections[self.primary]
    }

    /// 1-based (line, column) of the primary cursor, column in characters.
    pub fn cursor_position(&self, cx: &App) -> (usize, usize) {
        let head = self.primary_selection().head;
        let row = self.doc.read(cx).row_of(head);
        let start = self.doc.read(cx).line_start(row);
        let col = self.doc.read(cx).slice(start..head).chars().count();
        (row + 1, col + 1)
    }

    /// Characters selected, over all selections. Kept until the text or the
    /// selections change: the status bar asks at every frame, and there can
    /// be thousands of selections.
    pub fn selected_chars(&self, cx: &App) -> usize {
        let doc = self.doc.read(cx);
        let key = (doc.id(), doc.version(), self.selections_generation);
        if let Some((k, chars)) = self.selected_chars.get()
            && k == key
        {
            return chars;
        }
        let r = doc.rope();
        let chars = self.selections.iter().map(|s| r.byte_to_char(s.range().end) - r.byte_to_char(s.range().start)).sum();
        self.selected_chars.set(Some((key, chars)));
        chars
    }

    pub fn indent_label(&self) -> String {
        if self.indent_unit == "\t" {
            "Tabs".into()
        } else {
            format!("Spaces: {}", self.indent_unit.len())
        }
    }

    /// Moves the cursor to a byte offset and scrolls it into view.
    pub fn go_to(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        let offset = self.doc.read(cx).clip(offset);
        self.set_selections(vec![Selection::cursor(offset)], 0, cx);
        self.center_on_next_frame = true;
        window.focus(&self.focus_handle);
    }

    /// Shows the document's Git changes from `diff` (the same one for every
    /// editor of the document, so it's computed once).
    pub fn set_git_diff(&mut self, diff: Entity<DocumentDiff>, cx: &mut Context<Self>) {
        if self.git_diff.as_ref().is_some_and(|(d, _)| *d == diff) {
            return;
        }
        let observer = cx.observe(&diff, |editor, _, cx| editor.git_diff_changed(cx));
        self.git_diff = Some((diff, observer));
        self.git_diff_changed(cx);
    }

    fn git_diff_changed(&mut self, cx: &mut Context<Self>) {
        let Some((diff, _)) = &self.git_diff else { return };
        let diff = diff.read(cx);
        let git_file = diff.git_file().cloned();
        if self.git_file != git_file {
            self.blame = None;
            self.blame_request = None;
            self.file_blame = None;
            self.blame_task = None;
        }
        self.git_file = git_file;
        self.has_diff_base = diff.has_base();
        self.hunks = diff.hunks().clone();
        self.hunk_staged = diff.staged().clone();
        if self.open_hunk.is_some_and(|i| i >= self.hunks.len()) {
            self.open_hunk = None;
        }
        cx.notify();
    }

    /// The rows of each change, for tests.
    #[cfg(test)]
    pub(crate) fn hunk_rows(&self) -> Vec<Range<usize>> {
        self.hunks.iter().map(|h| h.new_rows.clone()).collect()
    }

    /// Re-reads the file if it changed on disk and has no unsaved edits
    /// (e.g. after a checkout, pull or discard): read and compared in the
    /// background, then taken in as one edit if the text is still the same
    /// (see `Document::apply_reload`).
    pub fn reload_if_changed(&mut self, cx: &mut Context<Self>) {
        // An unloaded tab reads the file as it is when it's shown.
        if self.doc.read(cx).is_dirty() || !self.is_loaded() {
            return;
        }
        if self.reload_task.is_some() {
            self.reload_again = true;
            return;
        }
        let Some(job) = self.doc.read(cx).reload_job() else { return };
        self.reload_task = Some(cx.spawn(async move |this, cx| {
            let read = cx.background_executor().spawn(async move { job() }).await;
            this.update(cx, |editor, cx| {
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = editor.reload_task.take() {
                    task.detach();
                }
                if let Ok(Some(reload)) = read {
                    let (before, path) = (editor.doc.read(cx).rope().clone(), editor.doc.read(cx).path.clone());
                    let changed = editor.reload(cx, |doc, selections| Ok(doc.apply_reload(reload, selections)));
                    // Local History: the text a change on disk replaced, and the new one.
                    if let (Ok(true), Some(path)) = (changed, path) {
                        let after = editor.doc.read(cx).rope().clone();
                        cx.background_executor()
                            .spawn(async move {
                                use crate::local_history::{Kind, record_rope};
                                record_rope(&path, &before, Kind::BeforeReload);
                                record_rope(&path, &after, Kind::Reloaded);
                            })
                            .detach();
                    }
                }
                // Changed again while it was read.
                if std::mem::take(&mut editor.reload_again) {
                    editor.reload_if_changed(cx);
                }
            })
            .ok();
        }));
    }

    /// Reads the file again as `encoding`; unsaved edits are dropped.
    pub fn reload_with_encoding(&mut self, encoding: crate::encoding::TextEncoding, cx: &mut Context<Self>) -> std::io::Result<()> {
        self.reload(cx, |doc, _| doc.reload_with_encoding(encoding).map(|()| true)).map(|_| ())
    }

    /// Replaces the text with what `read` reads (true: the text changed;
    /// it gets the carets as they are, for undo). Carets and the line at the
    /// top of the view follow the text through what changed (see
    /// `Document::apply_reload`); what's derived from the text is asked for
    /// again.
    fn reload(
        &mut self,
        cx: &mut Context<Self>,
        read: impl FnOnce(&mut Document, &[Selection]) -> std::io::Result<bool>,
    ) -> std::io::Result<bool> {
        let version = self.doc.read(cx).version();
        // Not drawn since the last reload: its top line is still to place.
        let top = self.reloaded_top.or_else(|| self.top_line(cx).map(|line| (line, self.doc.read(cx).line_start(line))));
        let before = self.selections.clone();
        if !self.change_doc(cx, |doc| read(doc, &before))? {
            return Ok(false);
        }
        let selections = self.moved_since(version, &self.selections, cx);
        let selections = self.clamp_selections(selections, cx);
        self.set_selections(selections, self.primary, cx);
        // For redo.
        self.doc.as_mut(cx).set_selections_after(&self.selections);
        if let Some((line, at)) = top {
            let at = self.moved_since(version, &[Selection::cursor(at)], cx)[0].head;
            match self.scroll_to_line.as_mut() {
                Some(pending) => *pending = self.doc.read(cx).row_of(at),
                None => self.reloaded_top = Some((line, at)),
            }
        }
        self.autoscroll = false;
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
        self.edited(cx);
        // The text is the file's again, as after a save: servers that check
        // on save (rust-analyzer's cargo check) check it again; until then
        // they send what they found in the old text where it was.
        self.lsp_did_save(cx);
        // Those in changed text went; the server's answer brings them back.
        self.schedule_inlay_hints(cx);
        self.schedule_code_lenses(cx);
        cx.notify();
        Ok(true)
    }

    /// Re-reads the settings and `.editorconfig` for this file: tab size,
    /// indentation, soft wrap and what saving does.
    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        // Applied when an unloaded tab is read.
        if !self.is_loaded() {
            return;
        }
        let fs = Settings::get(cx).for_file(self.doc.read(cx).language, self.doc.read(cx).path.as_deref());
        // Settings reloads come for every editor; most files resolve the same.
        self.sync_vim(cx);
        if self.applied_settings.as_ref() == Some(&fs) {
            return;
        }
        let relint = self.file_settings.syntax_errors != fs.syntax_errors;
        self.applied_settings = Some(fs.clone());
        self.file_settings = fs.clone();
        if relint {
            self.linted_version = None;
            self.schedule_lint(Duration::ZERO, cx);
        }
        self.tab_size = fs.tab_size;
        let configured = if fs.hard_tabs { "\t".to_string() } else { " ".repeat(fs.tab_size) };
        // A file's own indentation wins over settings, but not over .editorconfig.
        self.indent_unit = if fs.indent_is_explicit { configured } else { detect_indent(self.doc.read(cx)).unwrap_or(configured) };
        if self.mode == EditorMode::Full {
            // Wrapping a huge file means measuring every line.
            self.soft_wrap = fs.soft_wrap && !self.doc.read(cx).large;
        }
        self.wrap = None;
        self.schedule_inlay_hints(cx);
        self.schedule_code_lenses(cx);
        cx.notify();
    }

    /// `trim_trailing_whitespace` and `insert_final_newline`. Lines with a
    /// caret and a caret at the very end are left alone, so auto-save never
    /// eats a space or moves the caret mid-typing.
    fn apply_save_actions(&mut self, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full {
            return;
        }
        let FileSettings { trim_trailing_whitespace, insert_final_newline, .. } = self.file_settings;
        let caret_rows: Vec<usize> = self.selections.iter().map(|s| self.doc.read(cx).row_of(s.head)).collect();
        let mut edits = Vec::new();
        if trim_trailing_whitespace {
            for row in 0..self.doc.read(cx).line_count() {
                if caret_rows.contains(&row) {
                    continue;
                }
                let text = self.doc.read(cx).line_text(row);
                let kept = text.trim_end_matches([' ', '\t']).len();
                if kept < text.len() {
                    let start = self.doc.read(cx).line_start(row);
                    edits.push((start + kept..start + text.len(), String::new()));
                }
            }
        }
        let len = self.doc.read(cx).len();
        if insert_final_newline
            && len > 0
            && self.doc.read(cx).rope().byte(len - 1) != b'\n'
            && !self.selections.iter().any(|s| s.head == len)
        {
            edits.push((len..len, "\n".to_string()));
        }
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
        }
    }

    /// Saves here and now: closing a tab or window, and runs, decide on
    /// what's saved afterwards.
    pub fn save(&mut self, cx: &mut Context<Self>) -> std::io::Result<()> {
        let job = self.save_job(cx)?;
        self.doc.as_mut(cx).saved(job.write())?;
        self.after_save(cx);
        Ok(())
    }

    /// Saves in the background (Save, auto-save): the text is taken now,
    /// written off the UI thread, and once it's written the document counts
    /// as saved, servers hear of it and linters run. The save goes on if the
    /// future (its result) is dropped, or the tab closed meanwhile.
    pub fn save_in_background(&mut self, cx: &mut Context<Self>) -> impl Future<Output = std::io::Result<()>> + use<> {
        let (done, result) = futures::channel::oneshot::channel();
        match self.save_job(cx) {
            Err(err) => drop(done.send(Err(err))),
            Ok(job) => {
                let write = cx.background_executor().spawn(async move { job.write() });
                let doc = self.doc.downgrade();
                cx.spawn(async move |this, cx| {
                    let written = write.await;
                    // The document takes it in even with this tab gone (a split
                    // or the document store may still show it).
                    let saved = doc.update(cx, |doc, cx| {
                        let saved = doc.saved(written);
                        cx.notify();
                        saved
                    });
                    let saved = saved.unwrap_or(Ok(()));
                    if saved.is_ok() {
                        this.update(cx, |editor, cx| editor.after_save(cx)).ok();
                    }
                    drop(done.send(saved));
                })
                .detach();
            }
        }
        async move { result.await.unwrap_or_else(|_| Err(std::io::Error::other("the save was dropped"))) }
    }

    /// What a save writes, taken now (after the save actions), and the end
    /// of the undo step being typed.
    fn save_job(&mut self, cx: &mut Context<Self>) -> std::io::Result<crate::document::SaveJob> {
        // A restored tab saved before it was shown (Save All).
        self.load(cx)?;
        self.apply_save_actions(cx);
        let job = self.doc.as_mut(cx).save_job()?;
        self.doc.as_mut(cx).finalize_transaction();
        Ok(job)
    }

    /// The file has the text now: servers hear of it, linters that read
    /// the file run.
    fn after_save(&mut self, cx: &mut Context<Self>) {
        self.lsp_did_save(cx);
        self.schedule_lint(Duration::ZERO, cx);
        self.run_external_linter(cx);
        cx.notify();
    }

    /// Extensions changed: the file may have a language now, or a grammar
    /// for it. True when it was parsed again.
    pub fn refresh_language(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(path) = self.doc.read(cx).path.clone().filter(|_| self.is_loaded()) else { return false };
        let language = crate::syntax::Language::from_path(&path);
        if language == self.doc.read(cx).language && language.extension().is_none() {
            return false;
        }
        self.doc.as_mut(cx).set_language(language);
        self.highlight_cache = None;
        self.apply_settings(cx);
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
        cx.notify();
        true
    }

    /// The project's trust changed: ESLint may run now, or must stop.
    pub fn trust_changed(&mut self, cx: &mut Context<Self>) {
        if !self.is_loaded() {
            return;
        }
        self.detect_external_linter(cx);
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
    }

    pub fn set_path(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        // The language and settings come with the text.
        if !self.is_loaded() {
            self.doc.as_mut(cx).path = Some(path);
            return;
        }
        self.doc.as_mut(cx).set_path(path);
        self.apply_settings(cx);
        self.detect_external_linter(cx);
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
    }

    // ---- Selection management ----------------------------------------------

    fn set_selections(&mut self, mut selections: Vec<Selection>, primary: usize, cx: &mut Context<Self>) {
        let primary_sel = selections[primary.min(selections.len() - 1)];
        selections.sort_by_key(|s| s.range().start);
        // Merge overlapping selections.
        let mut merged: Vec<Selection> = Vec::with_capacity(selections.len());
        for sel in selections {
            if let Some(last) = merged.last_mut() {
                let lr = last.range();
                let sr = sel.range();
                if sr.start < lr.end || (sr.start == lr.end && (sel.is_empty() && last.is_empty())) {
                    let start = lr.start.min(sr.start);
                    let end = lr.end.max(sr.end);
                    *last = if last.reversed() {
                        Selection { anchor: end, head: start }
                    } else {
                        Selection { anchor: start, head: end }
                    };
                    continue;
                }
            }
            merged.push(sel);
        }
        self.primary = merged
            .iter()
            .position(|s| s.range().start <= primary_sel.range().start && primary_sel.range().end <= s.range().end)
            .unwrap_or(merged.len() - 1);
        self.selections = merged;
        self.selections_generation += 1;
        if !std::mem::take(&mut self.expanding) {
            self.expand_stack.clear();
        }
        self.marked_range = None;
        self.autoscroll = true;
        self.cursor_visible = true;
        self.blink_epoch += 1;
        self.blink_task = None;
        self.unfold_at_carets();
        self.schedule_symbol_highlights(cx);
        self.lsp_selection_changed(cx);
        cx.notify();
    }

    fn move_selections(
        &mut self,
        extend: bool,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&Self, &Selection, &App) -> usize,
    ) {
        let selections = self
            .selections
            .iter()
            .map(|s| {
                let head = f(self, s, cx);
                if extend {
                    Selection { anchor: s.anchor, head }
                } else {
                    Selection::cursor(head)
                }
            })
            .collect();
        self.goal_x = None;
        self.set_selections(selections, self.primary, cx);
    }

    // ---- Editing -----------------------------------------------------------

    /// Applies one edit per selection. The closure returns the replaced range,
    /// the new text, and where the cursor lands within the new text (`None`
    /// selects the whole inserted text).
    fn edit_selections(
        &mut self,
        kind: EditKind,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&Document, &Selection) -> (Range<usize>, String, Option<usize>),
    ) {
        let before = self.selections.clone();
        let mut edits = Vec::with_capacity(before.len());
        let mut placements = Vec::with_capacity(before.len());
        for sel in &before {
            let (range, text, cursor) = f(self.doc.read(cx), sel);
            placements.push(cursor);
            edits.push((range, text));
        }
        let new_ranges = self.change_doc(cx, |doc| doc.edit(edits, &before, kind));
        let selections = new_ranges
            .into_iter()
            .zip(placements)
            .map(|(r, place)| match place {
                Some(off) => Selection::cursor(r.start + off),
                None => Selection { anchor: r.start, head: r.end },
            })
            .collect();
        self.goal_x = None;
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    /// Makes the text `text`, as one edit that changes only the lines that
    /// differ (so carets and Undo behave): a Local History Restore.
    pub fn replace_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let current = self.doc.read(cx).rope().to_string();
        let edits = crate::format::line_edits(&current, text);
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
        }
    }

    /// Applies arbitrary edits and then remaps all selections through them.
    fn edit_and_remap(
        &mut self,
        mut edits: Vec<(Range<usize>, String)>,
        kind: EditKind,
        cx: &mut Context<Self>,
    ) {
        edits.sort_by_key(|e| e.0.start);
        edits.dedup_by(|a, b| a.0 == b.0);
        let map = |offset: usize, edits: &[(Range<usize>, String)]| {
            let mut delta: isize = 0;
            for (r, t) in edits {
                if r.start > offset {
                    break;
                }
                if r.is_empty() || r.end <= offset {
                    // Insertions at or before the offset, and replacements
                    // entirely before it, shift it.
                    delta += t.len() as isize - r.len() as isize;
                } else {
                    // Offset inside a replaced range: clamp into the new text.
                    let start = (r.start as isize + delta) as usize;
                    return start + t.len().min(offset - r.start);
                }
            }
            (offset as isize + delta).max(0) as usize
        };
        let before = self.selections.clone();
        let selections: Vec<Selection> = before
            .iter()
            .map(|s| Selection { anchor: map(s.anchor, &edits), head: map(s.head, &edits) })
            .collect();
        self.change_doc(cx, |doc| doc.edit(edits, &before, kind));
        self.goal_x = None;
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        self.doc.as_mut(cx).set_selections_after(&self.selections);
        self.schedule_inlay_hints(cx);
        self.schedule_code_lenses(cx);
        self.schedule_lint(Duration::from_millis(150), cx);
        self.edited(cx);
        cx.notify();
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let kind = if text.contains(char::is_whitespace) || text.len() > 1 {
            EditKind::Other
        } else {
            EditKind::Typing
        };
        let typed = text.to_string();
        // Text fields take text as typed: a closing bracket added to a search
        // would change what it finds.
        let pairs = !self.is_chromeless();
        self.edit_selections(kind, cx, |doc, sel| {
            let range = sel.range();
            let mut chars = typed.chars();
            let single = match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            };
            if let Some(c) = single.filter(|_| pairs) {
                let next = doc.char_at(range.end);
                // Type over an auto-inserted closing character.
                if sel.is_empty() && matches!(c, ')' | ']' | '}' | '"' | '\'' | '`') && next == Some(c) {
                    return (range.end..range.end + 1, c.to_string(), Some(1));
                }
                if let Some(close) = closing_pair(c) {
                    if !sel.is_empty() {
                        // Wrap the selection.
                        let inner = doc.slice(range.clone());
                        let len = inner.len();
                        return (range, format!("{c}{inner}{close}"), Some(len + 2));
                    }
                    let prev = doc.char_before(range.start);
                    let next_ok = next.is_none_or(|n| n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';' | '>'));
                    let is_quote = matches!(c, '"' | '\'' | '`');
                    let prev_ok = !is_quote || prev.is_none_or(|p| !p.is_alphanumeric() && p != c);
                    if next_ok && prev_ok {
                        return (range, format!("{c}{close}"), Some(1));
                    }
                }
            }
            let len = typed.len();
            (range, typed.clone(), Some(len))
        });
        self.after_typing(text, cx);
    }

    fn schedule_lint(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let language = self.doc.read(cx).language;
        if !language.is_lintable() {
            self.doc.as_mut(cx).set_builtin_diagnostics(Vec::new());
            return;
        }
        let version = self.doc.read(cx).version();
        if self.linted_version == Some(version) {
            return;
        }
        // Where the built-in lint is only syntax errors and they aren't
        // wanted, there's nothing to run.
        if lint::syntax_only(language) && !self.wants_syntax_errors(cx) {
            self.lint_task = None;
            self.doc.as_mut(cx).set_builtin_diagnostics(Vec::new());
            self.linted_version = Some(version);
            return;
        }
        let syntax_only = self.external.is_some() && self.external_error.is_none();
        let path = self.doc.read(cx).path.clone();
        self.lint_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            // The text as it is after the pause, with its syntax tree (the
            // document's: parsed already, it's no work to read).
            let Ok((version, rope, tree)) = this.update(cx, |editor, cx| {
                let doc = editor.doc.read(cx);
                (doc.version(), doc.rope().clone(), lint::syntax_only(language).then(|| doc.tree_job()))
            }) else {
                return;
            };
            let diagnostics = cx
                .background_executor()
                .spawn(async move {
                    match tree {
                        Some(tree) => tree().map_or_else(Vec::new, |tree| lint::syntax_errors(&tree, &rope)),
                        None => lint::lint(language, &rope.to_string(), path.as_deref(), syntax_only),
                    }
                })
                .await;
            this.update(cx, |editor, cx| {
                if editor.doc.read(cx).version() == version {
                    let mut diagnostics = diagnostics;
                    if !editor.wants_syntax_errors(cx) {
                        diagnostics.retain(|d| d.rule != "syntax");
                    }
                    editor.doc.as_mut(cx).set_builtin_diagnostics(diagnostics);
                    editor.linted_version = Some(version);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Whether the built-in lint reports syntax errors: not where a
    /// language server does (asked of the document's servers, not this
    /// editor: a split showing the file never gets the diagnostics itself),
    /// nor with `syntax_errors: false` (extension languages' default).
    fn wants_syntax_errors(&self, cx: &App) -> bool {
        !self.lsp.active && self.lsp_servers(cx).is_empty() && self.file_settings.syntax_errors
    }

    /// Blinks the caret while the editor has the keyboard in the active
    /// window. Otherwise the caret stays on and the timer stops: a frame drawn
    /// with focus starts it again (focus and activation draw), so editors in
    /// other tabs and panes don't wake up twice a second.
    fn restart_blink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let epoch = self.blink_epoch;
        self.blink_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(530)).await;
                let keep_going = this
                    .update_in(cx, |editor, window, cx| {
                        if editor.blink_epoch != epoch {
                            return false;
                        }
                        if editor.focus_handle.is_focused(window) && window.is_window_active() {
                            editor.cursor_visible = !editor.cursor_visible;
                            cx.notify();
                            return true;
                        }
                        if !editor.cursor_visible {
                            editor.cursor_visible = true;
                            cx.notify();
                        }
                        // This task is running; let it finish instead of dropping it.
                        if let Some(task) = editor.blink_task.take() {
                            task.detach();
                        }
                        false
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
    }

    // ---- Geometry ---------------------------------------------------------

    fn editor_font(&self, cx: &App) -> Font {
        let theme = Theme::get(cx);
        match self.mode {
            EditorMode::Full => font(theme.mono_font.clone()),
            EditorMode::SingleLine | EditorMode::MultiLine => font(theme.ui_font.clone()),
        }
    }

    fn font_size(&self, cx: &App) -> Pixels {
        let theme = Theme::get(cx);
        match self.mode {
            EditorMode::Full => px(theme.editor_font_size),
            EditorMode::SingleLine | EditorMode::MultiLine => px(theme.ui_font_size + 0.5),
        }
    }

    fn line_height(&self, cx: &App) -> Pixels {
        match self.mode {
            EditorMode::Full => px(Theme::get(cx).editor_line_height),
            EditorMode::SingleLine | EditorMode::MultiLine => px(20.),
        }
    }

    /// The inlay hints shown (none when they're off); the wrap map measures
    /// lines with them.
    fn inlays<'a>(&self, cx: &'a App) -> wrap::Inlays<'a> {
        let doc = self.doc.read(cx);
        if !self.file_settings.inlay_hints || doc.inlay_hints().is_empty() {
            return wrap::Inlays::NONE;
        }
        wrap::Inlays { hints: doc.inlay_hints(), generation: doc.inlay_generation() }
    }

    /// Line `row` as displayed: tabs expanded, inlay hints in place (soft
    /// wrap measures lines the same way), with its text. A line longer than
    /// `wrap::LONG_LINE` is built only for its rows `segs` (its display
    /// segments with those indices; see `DisplayLine::window`): a minified
    /// file's line can be megabytes long, and drawing it is per frame.
    fn display_part(&self, row: usize, segs: std::ops::RangeInclusive<usize>, cx: &App) -> (String, DisplayLine) {
        let doc = self.doc.read(cx);
        let start = doc.line_start(row);
        let (from, to) = self.part_bounds(row, segs, cx);
        let text = doc.slice(start + from.byte as usize..start + to);
        let (_, hints) = self.inlays(cx).of_line(doc.rope(), row);
        let shown = start + from.byte as usize..=start + to;
        let inlays: Vec<(usize, &str, bool, bool)> =
            hints.iter().filter(|h| shown.contains(&h.offset)).map(|h| (h.offset - start, h.label.as_str(), h.pad_left, h.pad_right)).collect();
        let dl = DisplayLine::window(&text, from, self.tab_size, &inlays);
        (text, dl)
    }

    /// Where `display_part` starts building line `row` for its rows `segs`
    /// and the column it stops at: the whole line, unless it's long.
    fn part_bounds(&self, row: usize, segs: std::ops::RangeInclusive<usize>, cx: &App) -> (wrap::Resume, usize) {
        let doc = self.doc.read(cx);
        let len = doc.line_end(row) - doc.line_start(row);
        let breaks = self.line_breaks(row);
        if len <= wrap::LONG_LINE || breaks.is_empty() {
            return (wrap::Resume::default(), len);
        }
        let (first, last) = ((*segs.start()).min(breaks.len()), (*segs.end()).min(breaks.len()));
        let from = if first == 0 { wrap::Resume::default() } else { breaks[first - 1].from };
        (from, breaks.get(last).map_or(len, |b| b.byte as usize).max(from.byte as usize))
    }

    /// The columns of line `row` laid out for display rows
    /// `first_drow..last_drow` (see `display_part`).
    fn shown_columns(&self, row: usize, first_drow: usize, last_drow: usize, cx: &App) -> Range<usize> {
        let first = self.first_display_row(row);
        let segs = first_drow.saturating_sub(first)..=last_drow.saturating_sub(1).saturating_sub(first);
        let (from, to) = self.part_bounds(row, segs, cx);
        from.byte as usize..to
    }

    /// Where buffer column `col` of line `row` shows (a display offset),
    /// building only the rows around it of a long line.
    fn display_column(&self, row: usize, col: usize, cx: &App) -> usize {
        let seg = self.line_breaks(row).partition_point(|b| b.byte as usize <= col);
        self.display_part(row, seg.saturating_sub(1)..=seg + 1, cx).1.to_display(col)
    }

    /// Shapes the part of `row` shown on display segment `seg`.
    fn shape_segment(&self, row: usize, seg: usize, window: &mut Window, cx: &App) -> RowShape {
        let theme = Theme::get(cx);
        let (_, dl) = self.display_part(row, seg..=seg, cx);
        let breaks = self.line_breaks(row);
        let (range, indent) = wrap::segment(breaks, seg.min(breaks.len()), dl.end());
        let last = seg >= breaks.len();
        let text = dl.slice(range.clone());
        let run = TextRun {
            len: text.len(),
            font: self.editor_font(cx),
            color: theme.text.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if text.is_empty() { vec![] } else { vec![run] };
        let line = window
            .text_system()
            .shape_line(SharedString::from(text.to_string()), self.font_size(cx), &runs, None);
        let indent = self.char_width() * indent as f32;
        RowShape { line, dl, range, indent, last }
    }

    fn char_width(&self) -> Pixels {
        self.layout.as_ref().map_or(px(8.), |l| l.char_width)
    }

    /// The x of `offset` within its display row (wrap indent included).
    fn x_for_offset(&self, offset: usize, window: &mut Window, cx: &App) -> Pixels {
        let row = self.doc.read(cx).row_of(offset);
        let col = self.display_column(row, offset - self.doc.read(cx).line_start(row), cx);
        let seg = wrap::segment_of(self.line_breaks(row), col);
        let shape = self.shape_segment(row, seg, window, cx);
        shape.indent + shape.line.x_for_index(col - shape.range.start)
    }

    fn offset_for_display_row_x(&self, drow: usize, x: Pixels, window: &mut Window, cx: &App) -> usize {
        let (row, seg) = self.line_of_display_row(drow, cx);
        let shape = self.shape_segment(row, seg, window, cx);
        let mut col = shape.range.start + shape.line.closest_index_for_x((x - shape.indent).max(px(0.)));
        // The end of a wrapped row is the start of the next one; stay on this row.
        if !shape.last && col >= shape.range.end && shape.range.end > shape.range.start {
            col = shape.dl.slice(shape.range.clone()).char_indices().last().map_or(col, |(i, _)| shape.range.start + i);
        }
        self.doc.read(cx).line_start(row) + shape.dl.to_buffer(col)
    }

    fn offset_for_position(&self, position: Point<Pixels>, window: &mut Window, cx: &App) -> usize {
        let Some(layout) = self.layout.clone() else {
            return 0;
        };
        let y = position.y - layout.bounds.top() + self.scroll.y;
        if y < px(0.) {
            return 0;
        }
        let drow = (y / layout.line_height).floor() as usize;
        if drow >= self.display_row_count(cx) {
            return self.doc.read(cx).len();
        }
        let x = position.x - layout.text_left + self.scroll.x;
        self.offset_for_display_row_x(drow, x.max(px(0.)), window, cx)
    }

    /// Whether `position` is on a block row (code lenses, a hunk's deleted
    /// lines): `offset_for_position` maps those to the line above, which
    /// isn't what the pointer is over.
    fn on_block_row(&self, position: Point<Pixels>) -> bool {
        let (Some(layout), Some(wrap)) = (self.layout.as_ref(), self.wrap.as_ref()) else { return false };
        let y = position.y - layout.bounds.top() + self.scroll.y;
        y >= px(0.) && wrap.block_at((y / layout.line_height).floor() as usize).is_some()
    }

    fn vertical_target(&self, sel: &Selection, rows: isize, goal: Pixels, window: &mut Window, cx: &App) -> usize {
        let mut drow = self.display_row_of(sel.head, cx) as isize + rows;
        // Carets don't stop on block rows.
        let step = if rows < 0 { -1 } else { 1 };
        while drow >= 0 && self.wrap.as_ref().is_some_and(|w| w.block_at(drow as usize).is_some()) {
            drow += step;
        }
        if drow < 0 {
            0
        } else if drow as usize >= self.display_row_count(cx) {
            self.doc.read(cx).len()
        } else {
            self.offset_for_display_row_x(drow as usize, goal, window, cx)
        }
    }

    // ---- Soft wrap -------------------------------------------------------------

    /// A Markdown file's preview opens in a pane item of its own; the
    /// workspace opens or closes it (see `Workspace::toggle_markdown_preview`).
    fn request_markdown_preview(&mut self, cx: &mut Context<Self>) {
        if self.is_markdown(cx) {
            cx.emit(EditorPreviewToggle);
        }
    }

    fn is_markdown(&self, cx: &App) -> bool {
        self.mode == EditorMode::Full && self.doc.read(cx).language == crate::syntax::Language::Markdown
    }

    /// Whether a preview of the document is open (the toggle shows it as on).
    pub fn set_previewed(&mut self, previewed: bool, cx: &mut Context<Self>) {
        if self.previewed != previewed {
            self.previewed = previewed;
            cx.notify();
        }
    }

    /// For this editor, until settings change (like Toggle Soft Wrap).
    fn toggle_inlay_hints(&mut self, _: &ToggleInlayHints, _: &mut Window, cx: &mut Context<Self>) {
        self.file_settings.inlay_hints = !self.file_settings.inlay_hints;
        self.schedule_inlay_hints(cx);
        self.schedule_code_lenses(cx);
        cx.notify();
    }

    fn toggle_soft_wrap(&mut self, _: &ToggleSoftWrap, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_chromeless() {
            return;
        }
        self.soft_wrap = !self.soft_wrap;
        self.wrap = None;
        self.scroll.x = px(0.);
        self.reveal_primary = true;
        cx.notify();
    }

    /// Recomputes line breaks when the text, width or font changed. Called
    /// before anything maps between offsets and display rows. The line at
    /// the top of the view stays there as rows above it come and go (code
    /// lenses or problems arriving after a restored tab took its place,
    /// folds, a new width, lines a reload added or removed above it).
    fn sync_wrap(&mut self, window: &mut Window, cx: &App) {
        let reloaded = self.reloaded_top.take();
        let top = self.layout.as_ref().map(|layout| {
            let line = self.line_at_row((self.scroll.y / layout.line_height).floor() as usize, cx);
            (line, self.first_display_row(line))
        });
        self.update_wrap(window, cx);
        if let (Some((line, before)), Some(layout)) = (top, self.layout.as_ref()) {
            // After a reload that line's text can be on another line now.
            let line = match reloaded {
                Some((old, at)) if old == line => self.doc.read(cx).row_of(at),
                _ => line,
            };
            let after = self.first_display_row(line);
            if after != before {
                self.scroll.y = (self.scroll.y + layout.line_height * (after as f32 - before as f32)).max(px(0.));
            }
        }
    }

    fn update_wrap(&mut self, window: &mut Window, cx: &App) {
        let wrap = self.soft_wrap && !self.is_single_line();
        let blocks = self.block_rows(cx);
        // Without wrapping, folds, long lines or blocks, display rows are buffer lines.
        if !wrap && blocks.is_empty() && self.folds.is_empty() && !self.doc.read(cx).has_long_lines() {
            self.wrap = None;
            return;
        }
        let Some(layout) = self.layout.as_ref() else { return };
        let font = self.editor_font(cx);
        let key = WrapKey {
            // Without soft wrap only long lines break, at a fixed column, so
            // resizing the view mustn't measure every line again.
            width: if wrap { layout.wrap_width } else { px(0.) },
            font_family: font.family.clone(),
            font_size: self.font_size(cx),
            tab_size: self.tab_size,
            wrap,
        };
        let (char_width, version, folds, rope) = (layout.char_width, self.doc.read(cx).version(), self.folds.ranges(), self.doc.read(cx).rope());
        let inlays = self.inlays(cx);
        // Edits and new hints re-measure only their lines; a new width, font
        // or tab size measures them all.
        if let Some(map) = self.wrap.as_mut().filter(|map| map.key == key) {
            let edits = self.doc.read(cx).line_edits_since(map.version);
            let caught_up = match edits {
                Some(edits) => map.update(edits, version, folds, inlays, rope, window.text_system(), font.clone(), char_width),
                None => false,
            };
            if caught_up {
                map.set_blocks(&blocks, rope);
                return;
            }
        }
        let mut map = WrapMap::new(key, version, folds, inlays, rope, window.text_system(), font, char_width);
        map.set_blocks(&blocks, rope);
        self.wrap = Some(map);
    }

    /// The block rows (see `build_blocks`), built again only when what they
    /// come from changes: they're asked for twice a frame, and a file can
    /// have a code lens above every other line.
    fn blocks(&mut self, cx: &App) -> Arc<Vec<Block>> {
        self.cached_blocks(cx).blocks.clone()
    }

    /// The blocks as (line, rows), for the wrap map.
    fn block_rows(&mut self, cx: &App) -> Arc<Vec<(usize, usize)>> {
        self.cached_blocks(cx).rows.clone()
    }

    fn cached_blocks(&mut self, cx: &App) -> &BlocksCache {
        let doc = self.doc.read(cx);
        let key = BlocksKey {
            mode: self.mode,
            doc: (doc.id(), doc.version(), doc.diagnostics_generation(), doc.code_lens_generation()),
            hunk: self.open_hunk.map(|ix| (ix, Same(self.hunks.clone()))),
            caret: (self.selections.len() == 1).then(|| self.primary_selection().head),
            layout: self.layout.as_ref().map(|l| (l.wrap_width, l.char_width)),
            tab_size: self.tab_size,
            code_lens: self.file_settings.code_lens,
        };
        let fresh = self.blocks_cache.as_ref().is_some_and(|c| c.key == key && c.folds == self.folds.ranges());
        if !fresh {
            let blocks = self.build_blocks(cx);
            let rows = blocks.iter().map(|b| (b.line, b.rows())).collect();
            let problems = blocks.iter().find_map(|b| match b.kind {
                BlockKind::Problems { of, .. } => Some((b.line, of)),
                _ => None,
            });
            let folds = self.folds.ranges().to_vec();
            self.blocks_cache = Some(BlocksCache { key, folds, blocks: Arc::new(blocks), rows: Arc::new(rows), problems });
        }
        self.blocks_cache.as_ref().expect("built above")
    }

    /// Rows between lines that aren't text (see `wrap`), in the order they
    /// sit above their line: a hunk opened from the gutter shows the lines it
    /// deleted, and code lenses sit right above their line.
    fn build_blocks(&self, cx: &App) -> Vec<Block> {
        if self.is_single_line() {
            return Vec::new();
        }
        let doc = self.doc.read(cx);
        let mut blocks = Vec::new();
        if let Some(hunk) = self.open_hunk.and_then(|ix| self.hunks.get(ix)) {
            let deleted: Vec<String> = hunk.old_text.lines().map(str::to_string).collect();
            if !deleted.is_empty() {
                blocks.push(Block { line: hunk.new_rows.start.min(doc.line_count()), kind: BlockKind::Deleted(deleted) });
            }
        }
        let problems = self.caret_problem_rows(cx);
        if !problems.is_empty() {
            let row = doc.row_of(self.primary_selection().head);
            // Under the line's fold when it's folded: above the first line it
            // doesn't hide (blocks above hidden lines take no rows).
            let mut below = row + 1;
            for fold in self.folds.ranges() {
                if (doc.row_of(fold.start)..=doc.row_of(fold.end)).contains(&below) {
                    below = doc.row_of(fold.end) + 1;
                }
            }
            blocks.push(Block { line: below, kind: BlockKind::Problems { of: row, rows: problems } });
        }
        if self.file_settings.code_lens {
            for lens in doc.code_lenses() {
                let line = doc.row_of(lens.line_start);
                match blocks.last_mut() {
                    Some(Block { line: l, kind: BlockKind::Lenses(lenses) }) if *l == line => lenses.push(lens.clone()),
                    _ => blocks.push(Block { line, kind: BlockKind::Lenses(vec![lens.clone()]) }),
                }
            }
        }
        // Above a line: the problems of the line before it, then deleted
        // lines, then the line's own lenses.
        let rank = |kind: &BlockKind| match kind {
            BlockKind::Problems { .. } => 0,
            BlockKind::Deleted(_) => 1,
            BlockKind::Lenses(_) => 2,
        };
        blocks.sort_by_key(|b| (b.line, rank(&b.kind)));
        blocks
    }

    /// The caret line's problems in full when its end-of-line hint can't
    /// show them: a message on several lines, one too long for the room
    /// after the code, or more than one problem. A row per line, wrapped to
    /// the view.
    fn caret_problem_rows(&self, cx: &App) -> Vec<(lint::Severity, String)> {
        let Some(layout) = self.layout.as_ref() else { return Vec::new() };
        if self.selections.len() != 1 || self.mode != EditorMode::Full {
            return Vec::new();
        }
        let doc = self.doc.read(cx);
        let row = doc.row_of(self.primary_selection().head);
        let (start, end) = (doc.line_start(row), doc.line_end(row));
        let first = doc.diagnostics.partition_point(|d| d.range.start < start);
        let problems: Vec<&lint::Diagnostic> =
            doc.diagnostics[first..].iter().take_while(|d| d.range.start <= end).filter(|d| d.severity.is_problem()).collect();
        let Some(only) = problems.first() else { return Vec::new() };
        let cols = (layout.wrap_width / layout.char_width).floor().max(20.) as usize;
        // Past the view's width it can't fit whatever the line's length.
        let line_cols = self.display_chars(row, cols + 1, cx);
        let message = only.message.trim();
        if problems.len() == 1 && !message.contains('\n') && line_cols + 4 + message.chars().count() <= cols {
            return Vec::new();
        }
        let indent = self.leading_whitespace(row, cx).0;
        let width = cols.saturating_sub(indent + 2).max(20);
        let mut rows = Vec::new();
        for problem in problems {
            for line in problem.message.lines().map(str::trim).filter(|l| !l.is_empty()) {
                rows.extend(wrap_words(line, width).into_iter().map(|chunk| (problem.severity, chunk)));
            }
        }
        rows.truncate(12);
        rows
    }

    /// Display characters of line `row` (tabs expanded, control characters
    /// as their symbols), counted no further than past `limit`: the line can
    /// be megabytes long.
    fn display_chars(&self, row: usize, limit: usize, cx: &App) -> usize {
        let doc = self.doc.read(cx);
        let mut col = 0;
        for ch in doc.rope().byte_slice(doc.line_start(row)..doc.line_end(row)).chars() {
            if col > limit {
                break;
            }
            col += match ch {
                '\t' => self.tab_size - col % self.tab_size,
                ch => control_symbol(ch).map_or(1, |symbol| symbol.chars().count()),
            };
        }
        col
    }

    /// Line `row`'s indentation as displayed (its length in display text,
    /// tabs expanded), and whether the line is nothing else: read from the
    /// rope, as the line can be megabytes long.
    fn leading_whitespace(&self, row: usize, cx: &App) -> (usize, bool) {
        let doc = self.doc.read(cx);
        let mut indent = String::new();
        for ch in doc.rope().byte_slice(doc.line_start(row)..doc.line_end(row)).chars() {
            if !ch.is_whitespace() {
                return (DisplayLine::new(&indent, self.tab_size).text.len(), false);
            }
            indent.push(ch);
        }
        (DisplayLine::new(&indent, self.tab_size).text.len(), true)
    }

    /// Where line `row`'s text starts and ends without the whitespace around
    /// it (columns), read from the rope.
    fn body_columns(&self, row: usize, cx: &App) -> (usize, usize) {
        let doc = self.doc.read(cx);
        let line = doc.rope().byte_slice(doc.line_start(row)..doc.line_end(row));
        let start = line.chars().take_while(|c| c.is_whitespace()).map(char::len_utf8).sum();
        let mut end = line.len_bytes();
        let mut chars = line.chars_at(line.len_chars());
        while let Some(c) = chars.prev().filter(|c| c.is_whitespace()) {
            end -= c.len_utf8();
        }
        (start, end)
    }

    /// Per line where diagnostics start: (line, its worst problem's index in
    /// `diagnostics`, how many start on it), kept for the document's state:
    /// a line's gutter dot and hint mustn't walk them.
    fn row_problems(&mut self, cx: &App) -> Arc<Vec<(usize, Option<usize>, usize)>> {
        let doc = self.doc.read(cx);
        let key = (doc.id(), doc.version(), doc.diagnostics_generation());
        if self.row_problems.0 != Some(key) {
            self.row_problems = (Some(key), Arc::new(markers::row_problems(doc)));
        }
        self.row_problems.1.clone()
    }

    /// Rows with a gutter ▶, sorted, each with its test's last status, for
    /// the run targets drawn (found again when they or the results change).
    fn shown_run_rows(&mut self, cx: &mut Context<Self>) -> Arc<Vec<(usize, Option<crate::test_run::TestStatus>)>> {
        let targets = self.shown_run_targets(cx);
        let current = self.run_rows.0.as_ref().is_some_and(|(t, s)| Arc::ptr_eq(t, &targets) && Arc::ptr_eq(s, &self.test_statuses));
        if !current {
            let statuses = self.test_statuses.clone();
            let mut rows: Vec<_> = targets
                .iter()
                .filter_map(|t| Some((t.row?, t.test.as_ref().and_then(|test| crate::test_run::status_of(test, &statuses)))))
                .collect();
            rows.sort_unstable_by_key(|(row, _)| *row);
            self.run_rows = (Some((targets, statuses)), Arc::new(rows));
        }
        self.run_rows.1.clone()
    }

    /// The workspace's test results changed: the ▶ marks follow.
    pub fn set_test_statuses(&mut self, statuses: Arc<crate::test_run::Statuses>, cx: &mut Context<Self>) {
        if !Arc::ptr_eq(&self.test_statuses, &statuses) {
            self.test_statuses = statuses;
            cx.notify();
        }
    }

    /// Both brackets of the pairs around carets at `heads` (a few, so many
    /// cursors stay cheap). Cached: in a big array or object finding a
    /// partner is a walk, and a caret blink draws again.
    fn bracket_pairs(&mut self, heads: Vec<usize>, cx: &App) -> Arc<Vec<Range<usize>>> {
        let key = (self.doc.read(cx).tree_version(), heads);
        if let Some((k, pairs)) = &self.bracket_cache
            && *k == key
        {
            return pairs.clone();
        }
        let syntax = self.doc.read(cx).syntax();
        let pairs: Arc<Vec<_>> =
            Arc::new(key.1.iter().filter_map(|&head| syntax.bracket_pair(self.doc.read(cx).rope(), head)).flat_map(|(a, b)| [a, b]).collect());
        self.bracket_cache = Some((key, pairs.clone()));
        pairs
    }

    /// Highlights for display rows `first_drow..last_drow` (buffer lines
    /// `first_row..last_row`): folded lines and a long line's rows off
    /// screen are left out. Cached, since a caret blink draws again.
    fn visible_highlights(&mut self, first_drow: usize, last_drow: usize, first_row: usize, last_row: usize, cx: &App) -> Arc<Vec<(Range<usize>, crate::syntax::Highlight)>> {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for row in (first_row..last_row).filter(|&row| !self.row_hidden(row)) {
            let (start, end) = (self.doc.read(cx).line_start(row), self.doc.read(cx).line_end(row));
            match ranges.last_mut() {
                Some(r) if r.end + 1 >= start => r.end = end,
                _ => ranges.push(start..end),
            }
        }
        let (row, seg) = self.line_of_display_row(first_drow, cx);
        if let (Some(r), Some(b)) = (ranges.first_mut(), seg.checked_sub(1).and_then(|s| self.line_breaks(row).get(s))) {
            // From the character the row's text starts in (see `display_part`).
            r.start = (self.doc.read(cx).line_start(row) + b.from.byte as usize).min(r.end);
        }
        let (row, seg) = self.line_of_display_row(last_drow.saturating_sub(1), cx);
        if let (Some(r), Some(b)) = (ranges.last_mut(), self.line_breaks(row).get(seg)) {
            r.end = (self.doc.read(cx).line_start(row) + b.byte as usize).max(r.start);
        }
        let key = (self.doc.read(cx).tree_version(), self.doc.read(cx).language, ranges);
        if let Some((k, highlights)) = &self.highlight_cache
            && *k == key
        {
            return highlights.clone();
        }
        let syntax = self.doc.read(cx).syntax();
        let highlights: Arc<Vec<_>> = Arc::new(key.2.iter().flat_map(|r| syntax.highlights(self.doc.read(cx).rope(), r.clone())).collect());
        self.highlight_cache = Some((key, highlights.clone()));
        highlights
    }

    fn display_row_count(&self, cx: &App) -> usize {
        self.wrap.as_ref().map_or(self.doc.read(cx).line_count(), WrapMap::row_count)
    }

    fn first_display_row(&self, row: usize) -> usize {
        self.wrap.as_ref().map_or(row, |w| w.first_display_row(row))
    }

    /// The buffer line and its wrap segment on display row `drow`.
    fn line_of_display_row(&self, drow: usize, cx: &App) -> (usize, usize) {
        match &self.wrap {
            Some(w) => w.line_of(drow),
            None => (drow.min(self.doc.read(cx).line_count().saturating_sub(1)), 0),
        }
    }

    /// Where buffer line `row`'s rows start, the ones above it that aren't
    /// text (code lenses) included.
    fn rows_start(&self, row: usize) -> usize {
        self.wrap.as_ref().map_or(row, |w| w.rows_start(row))
    }

    /// The buffer line whose rows (see `rows_start`) hold display row `drow`.
    fn line_at_row(&self, drow: usize, cx: &App) -> usize {
        match &self.wrap {
            Some(w) => w.line_at(drow),
            None => drow.min(self.doc.read(cx).line_count().saturating_sub(1)),
        }
    }

    fn line_breaks(&self, row: usize) -> &[Break] {
        self.wrap.as_ref().map_or(&[], |w| w.breaks(row))
    }

    /// The display row showing `offset` (on a wrap break, the later row).
    fn display_row_of(&self, offset: usize, cx: &App) -> usize {
        let row = self.doc.read(cx).row_of(offset);
        let breaks = self.line_breaks(row);
        if breaks.is_empty() {
            return self.first_display_row(row);
        }
        let col = self.display_column(row, offset - self.doc.read(cx).line_start(row), cx);
        self.first_display_row(row) + wrap::segment_of(breaks, col)
    }

    fn move_vertically(&mut self, rows: isize, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_wrap(window, cx);
        let goals: Vec<Pixels> = self
            .selections
            .iter()
            .map(|s| match self.goal_x {
                Some(g) if self.selections.len() == 1 => g,
                _ => self.x_for_offset(s.head, window, cx),
            })
            .collect();
        let selections: Vec<Selection> = self
            .selections
            .iter()
            .zip(&goals)
            .map(|(s, &goal)| {
                let start = if !extend && !s.is_empty() {
                    let r = s.range();
                    let at = if rows < 0 { r.start } else { r.end };
                    Selection::cursor(at)
                } else {
                    *s
                };
                let head = self.vertical_target(&start, rows, goal, window, cx);
                if extend {
                    Selection { anchor: s.anchor, head }
                } else {
                    Selection::cursor(head)
                }
            })
            .collect();
        let goal = goals[self.primary];
        self.set_selections(selections, self.primary, cx);
        self.goal_x = Some(goal);
    }

    // ---- Actions -------------------------------------------------------------

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| {
            if s.is_empty() { this.doc.read(cx).prev_char_boundary(s.head) } else { s.range().start }
        });
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| {
            if s.is_empty() { this.doc.read(cx).next_char_boundary(s.head) } else { s.range().end }
        });
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.doc.read(cx).prev_char_boundary(s.head));
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.doc.read(cx).next_char_boundary(s.head));
    }

    fn move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Move(-1), cx) {
            return;
        }
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.move_vertically(-1, false, window, cx);
    }

    fn move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Move(1), cx) {
            return;
        }
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.move_vertically(1, false, window, cx);
    }

    fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, true, window, cx);
    }

    fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, true, window, cx);
    }

    fn page_up(&mut self, _: &PageUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        let rows = self.layout.as_ref().map_or(20, |l| l.rows_per_page) as isize;
        if let Some(l) = &self.layout {
            self.scroll.y = (self.scroll.y - l.line_height * rows as f32).max(px(0.));
        }
        self.move_vertically(-rows, false, window, cx);
    }

    fn page_down(&mut self, _: &PageDown, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        let rows = self.layout.as_ref().map_or(20, |l| l.rows_per_page) as isize;
        if let Some(l) = &self.layout {
            self.scroll.y += l.line_height * rows as f32;
        }
        self.move_vertically(rows, false, window, cx);
    }

    fn move_word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| word_left(this.doc.read(cx), s.head));
    }

    fn move_word_right(&mut self, _: &MoveWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| word_right(this.doc.read(cx), s.head));
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| word_left(this.doc.read(cx), s.head));
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| word_right(this.doc.read(cx), s.head));
    }

    fn smart_home(&self, offset: usize, cx: &App) -> usize {
        let row = self.doc.read(cx).row_of(offset);
        let start = self.doc.read(cx).line_start(row);
        let text = self.doc.read(cx).line_text(row);
        let indent = text.len() - text.trim_start().len();
        if offset == start + indent { start } else { start + indent }
    }

    fn move_line_start(&mut self, _: &MoveLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| this.smart_home(s.head, cx));
    }

    fn move_line_end(&mut self, _: &MoveLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| this.doc.read(cx).line_end(this.doc.read(cx).row_of(s.head)));
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.smart_home(s.head, cx));
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.doc.read(cx).line_end(this.doc.read(cx).row_of(s.head)));
    }

    fn move_doc_start(&mut self, _: &MoveDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.set_selections(vec![Selection::cursor(0)], 0, cx);
    }

    fn move_doc_end(&mut self, _: &MoveDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.set_selections(vec![Selection::cursor(self.doc.read(cx).len())], 0, cx);
    }

    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        let anchor = self.primary_selection().anchor;
        self.set_selections(vec![Selection { anchor, head: 0 }], 0, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        let anchor = self.primary_selection().anchor;
        let head = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor, head }], 0, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor: 0, head: len }], 0, cx);
        self.autoscroll = false;
    }

    fn select_line(&mut self, _: &SelectLine, _: &mut Window, cx: &mut Context<Self>) {
        let selections = self
            .selections
            .iter()
            .map(|s| {
                let r = s.range();
                let start = self.doc.read(cx).line_start(self.doc.read(cx).row_of(r.start));
                let end_row = self.doc.read(cx).row_of(r.end);
                let end = (self.doc.read(cx).line_end(end_row) + 1).min(self.doc.read(cx).len());
                Selection { anchor: start, head: end }
            })
            .collect();
        self.set_selections(selections, self.primary, cx);
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let unit = self.indent_unit.clone();
        // Text fields don't pair brackets (see `insert`).
        let pairs = !self.is_chromeless();
        self.edit_selections(EditKind::Deleting, cx, |doc, s| {
            if !s.is_empty() {
                return (s.range(), String::new(), Some(0));
            }
            let head = s.head;
            let row_start = doc.line_start(doc.row_of(head));
            let before = doc.slice(row_start..head);
            // Delete a whole indent level when in leading whitespace.
            if !before.is_empty() && unit != "\t" && before.chars().all(|c| c == ' ') {
                let n = (before.len() - 1) % unit.len() + 1;
                return (head - n..head, String::new(), Some(0));
            }
            // Delete an auto-inserted pair together.
            let prev = doc.char_before(head);
            if pairs
                && let (Some(p), Some(n)) = (prev, doc.char_at(head))
                && closing_pair(p) == Some(n)
            {
                return (head - p.len_utf8()..head + n.len_utf8(), String::new(), Some(0));
            }
            (doc.prev_char_boundary(head)..head, String::new(), Some(0))
        });
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Deleting, cx, |doc, s| {
            let r = if s.is_empty() { s.head..doc.next_char_boundary(s.head) } else { s.range() };
            (r, String::new(), Some(0))
        });
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = if s.is_empty() { word_left(doc, s.head)..s.head } else { s.range() };
            (r, String::new(), Some(0))
        });
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = if s.is_empty() { s.head..word_right(doc, s.head) } else { s.range() };
            (r, String::new(), Some(0))
        });
    }

    fn delete_to_line_start(&mut self, _: &DeleteToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = if s.is_empty() {
                let start = doc.line_start(doc.row_of(s.head));
                if start == s.head { doc.prev_char_boundary(s.head)..s.head } else { start..s.head }
            } else {
                s.range()
            };
            (r, String::new(), Some(0))
        });
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Accept { replace: false }, cx) {
            return;
        }
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        let unit = self.indent_unit.clone();
        let is_html = self.doc.read(cx).language == crate::syntax::Language::Html;
        // Python blocks and YAML mappings open with a trailing `:`.
        let colon_opens = matches!(
            self.doc.read(cx).language,
            crate::syntax::Language::Python | crate::syntax::Language::Yaml
        );
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = s.range();
            let row = doc.row_of(r.start);
            let line = doc.line_text(row);
            let line_start = doc.line_start(row);
            let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let before = doc.slice(line_start..r.start);
            let before_trim = before.trim_end();
            let prev = before_trim.chars().last();
            let next = doc.char_at(r.end);
            let opens_html_tag = is_html && prev == Some('>') && !before_trim.ends_with("/>") && {
                let tag_start = before_trim.rfind('<');
                tag_start.is_some_and(|i| {
                    let tag = &before_trim[i..];
                    !tag.starts_with("</") && !tag.starts_with("<!")
                })
            };
            // `def f():`, `if x:`, `steps:`
            let opens_colon_block = colon_opens && prev == Some(':') && !before.trim_start().starts_with('#');
            // The language's indents.scm: an @indent node opens on this line.
            let opens_by_query = doc.syntax().indent_after(doc.rope(), r.start, row) == Some(true);
            let opens = matches!(prev, Some('{' | '[' | '(')) || opens_html_tag || opens_colon_block || opens_by_query;
            let closes_next = match prev {
                Some('{') => next == Some('}'),
                Some('[') => next == Some(']'),
                Some('(') => next == Some(')'),
                Some('>') => opens_html_tag && doc.slice(r.end..(r.end + 2).min(doc.len())) == "</",
                _ => false,
            };
            if opens && closes_next {
                let inner = format!("\n{indent}{unit}");
                let text = format!("{inner}\n{indent}");
                (r, text, Some(inner.len()))
            } else if opens {
                let text = format!("\n{indent}{unit}");
                let len = text.len();
                (r, text, Some(len))
            } else {
                let text = format!("\n{indent}");
                let len = text.len();
                (r, text, Some(len))
            }
        });
    }

    fn newline_below(&mut self, _: &NewlineBelow, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.move_selections(false, cx, |this, s, cx| this.doc.read(cx).line_end(this.doc.read(cx).row_of(s.head)));
        self.newline(&Newline, window, cx);
    }

    /// Rows covered by selections, as merged row ranges.
    fn selected_row_blocks(&self, cx: &App) -> Vec<Range<usize>> {
        let mut blocks: Vec<Range<usize>> = Vec::new();
        for s in &self.selections {
            let r = s.range();
            let start = self.doc.read(cx).row_of(r.start);
            let mut end = self.doc.read(cx).row_of(r.end);
            // A selection ending at column 0 doesn't include that row.
            if end > start && self.doc.read(cx).line_start(end) == r.end {
                end -= 1;
            }
            match blocks.last_mut() {
                Some(last) if start <= last.end => last.end = last.end.max(end + 1),
                _ => blocks.push(start..end + 1),
            }
        }
        blocks
    }

    fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Accept { replace: true }, cx) {
            return;
        }
        if self.is_chromeless() {
            cx.propagate();
            return;
        }
        if self.snippet_tab(1, cx) || self.expand_user_snippet(cx) {
            return;
        }
        if self.selections.iter().all(|s| s.is_empty()) {
            let unit = self.indent_unit.clone();
            let tab_size = self.tab_size;
            self.edit_selections(EditKind::Other, cx, |doc, s| {
                let text = if unit == "\t" {
                    "\t".to_string()
                } else {
                    let row_start = doc.line_start(doc.row_of(s.head));
                    let col = DisplayLine::new(&doc.slice(row_start..s.head), tab_size).text.chars().count();
                    " ".repeat(unit.len() - col % unit.len())
                };
                let len = text.len();
                (s.range(), text, Some(len))
            });
            return;
        }
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            for row in block {
                if !self.doc.read(cx).line_text(row).trim().is_empty() {
                    let at = self.doc.read(cx).line_start(row);
                    edits.push((at..at, self.indent_unit.clone()));
                }
            }
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_chromeless() {
            cx.propagate();
            return;
        }
        if self.snippet_tab(-1, cx) {
            return;
        }
        let mut edits = Vec::new();
        let width = if self.indent_unit == "\t" { self.tab_size } else { self.indent_unit.len() };
        for block in self.selected_row_blocks(cx) {
            for row in block {
                let text = self.doc.read(cx).line_text(row);
                let at = self.doc.read(cx).line_start(row);
                let n = if text.starts_with('\t') {
                    1
                } else {
                    text.chars().take(width).take_while(|c| *c == ' ').count()
                };
                if n > 0 {
                    edits.push((at..at + n, String::new()));
                }
            }
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn toggle_comment(&mut self, _: &ToggleComment, _: &mut Window, cx: &mut Context<Self>) {
        let language = self.doc.read(cx).language;
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            let rows: Vec<usize> = block.clone().filter(|&r| !self.doc.read(cx).line_text(r).trim().is_empty()).collect();
            if rows.is_empty() {
                continue;
            }
            match language.line_comment() {
                Some(prefix) => {
                    let token = prefix.trim_end();
                    let all_commented = rows.iter().all(|&r| self.doc.read(cx).line_text(r).trim_start().starts_with(token));
                    let min_indent = rows
                        .iter()
                        .map(|&r| {
                            let t = self.doc.read(cx).line_text(r);
                            t.len() - t.trim_start().len()
                        })
                        .min()
                        .unwrap_or(0);
                    for &r in &rows {
                        let t = self.doc.read(cx).line_text(r);
                        let start = self.doc.read(cx).line_start(r);
                        if all_commented {
                            let indent = t.len() - t.trim_start().len();
                            let len = if t[indent..].starts_with(prefix) { prefix.len() } else { token.len() };
                            edits.push((start + indent..start + indent + len, String::new()));
                        } else {
                            edits.push((start + min_indent..start + min_indent, prefix.to_string()));
                        }
                    }
                }
                // No line comments (HTML, CSS, Markdown): wrap each line in a block comment.
                None => {
                    let Some((open, close)) = language.block_comment() else {
                        continue;
                    };
                    let (open_token, close_token) = (open.trim_end(), close.trim_start());
                    let all_commented = rows.iter().all(|&r| {
                        let t = self.doc.read(cx).line_text(r);
                        let t = t.trim();
                        t.len() >= open_token.len() + close_token.len()
                            && t.starts_with(open_token)
                            && t.ends_with(close_token)
                    });
                    for &r in &rows {
                        let t = self.doc.read(cx).line_text(r);
                        let start = self.doc.read(cx).line_start(r);
                        let indent = t.len() - t.trim_start().len();
                        let end = start + t.trim_end().len();
                        if all_commented {
                            let body = t.trim();
                            let mut open_len = if body.starts_with(open) { open.len() } else { open_token.len() };
                            let mut close_len = if body.ends_with(close) { close.len() } else { close_token.len() };
                            // `/* */`: the padding spaces overlap, so remove only the tokens.
                            if open_len + close_len > body.len() {
                                (open_len, close_len) = (open_token.len(), close_token.len());
                            }
                            edits.push((end - close_len..end, String::new()));
                            edits.push((start + indent..start + indent + open_len, String::new()));
                        } else {
                            edits.push((end..end, close.into()));
                            edits.push((start + indent..start + indent, open.into()));
                        }
                    }
                }
            }
        }
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
        }
    }

    fn duplicate_line(&mut self, _: &DuplicateLine, _: &mut Window, cx: &mut Context<Self>) {
        // Each block's text is inserted after it; cursors move onto the copy.
        let blocks = self.selected_row_blocks(cx);
        let mut edits = Vec::new();
        let mut shifts: Vec<(usize, usize)> = Vec::new(); // (block end, cumulative shift)
        let mut total = 0;
        for block in &blocks {
            let start = self.doc.read(cx).line_start(block.start);
            let end = self.doc.read(cx).line_end(block.end - 1);
            let text = format!("\n{}", self.doc.read(cx).slice(start..end));
            total += text.len();
            shifts.push((end, total));
            edits.push((end..end, text));
        }
        let before = self.selections.clone();
        let selections = before
            .iter()
            .map(|s| {
                let shift = shifts
                    .iter()
                    .find(|(end, _)| s.range().end <= *end)
                    .map_or(total, |(_, shift)| *shift);
                Selection { anchor: s.anchor + shift, head: s.head + shift }
            })
            .collect();
        self.change_doc(cx, |doc| doc.edit(edits, &before, EditKind::Other));
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    fn move_lines(&mut self, up: bool, cx: &mut Context<Self>) {
        let blocks = self.selected_row_blocks(cx);
        let last_row = self.doc.read(cx).line_count() - 1;
        if (up && blocks.first().is_some_and(|b| b.start == 0)) || (!up && blocks.last().is_some_and(|b| b.end > last_row)) {
            return;
        }
        let mut edits = Vec::new();
        let mut shifts: Vec<(Range<usize>, isize)> = Vec::new();
        for block in &blocks {
            let block_start = self.doc.read(cx).line_start(block.start);
            let block_end = self.doc.read(cx).line_end(block.end - 1);
            let block_text = self.doc.read(cx).slice(block_start..block_end);
            if up {
                let prev_start = self.doc.read(cx).line_start(block.start - 1);
                let prev_text = self.doc.read(cx).line_text(block.start - 1);
                edits.push((prev_start..block_end, format!("{block_text}\n{prev_text}")));
                shifts.push((block_start..block_end + 1, -(prev_text.len() as isize + 1)));
            } else {
                let next_end = self.doc.read(cx).line_end(block.end);
                let next_text = self.doc.read(cx).line_text(block.end);
                edits.push((block_start..next_end, format!("{next_text}\n{block_text}")));
                shifts.push((block_start..block_end + 1, next_text.len() as isize + 1));
            }
        }
        let before = self.selections.clone();
        let selections = before
            .iter()
            .map(|s| {
                let shift = shifts
                    .iter()
                    .find(|(r, _)| r.contains(&s.range().start))
                    .map_or(0, |(_, d)| *d);
                Selection {
                    anchor: (s.anchor as isize + shift) as usize,
                    head: (s.head as isize + shift) as usize,
                }
            })
            .collect();
        self.change_doc(cx, |doc| doc.edit(edits, &before, EditKind::Other));
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    fn move_line_up(&mut self, _: &MoveLineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(true, cx);
    }

    fn move_line_down(&mut self, _: &MoveLineDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(false, cx);
    }

    fn select_next_occurrence(&mut self, _: &SelectNextOccurrence, _: &mut Window, cx: &mut Context<Self>) {
        self.add_next_occurrence(cx);
    }

    /// From carets, the words at them; then the next occurrence of the
    /// primary selection's text (around the end), as another selection.
    fn add_next_occurrence(&mut self, cx: &mut Context<Self>) {
        let primary = self.primary_selection();
        if primary.is_empty() {
            let selections = self
                .selections
                .iter()
                .map(|s| {
                    let r = word_range(self.doc.read(cx), s.head);
                    Selection { anchor: r.start, head: r.end }
                })
                .collect();
            self.set_selections(selections, self.primary, cx);
            self.occurrence_words = Some(self.selections.clone());
            return;
        }
        let whole_words = self.occurrence_words.as_ref() == Some(&self.selections);
        let doc = self.doc.read(cx);
        let needle = doc.slice(primary.range());
        let from = self.selections.iter().map(|s| s.range().end).max().unwrap_or(0);
        let found = find_occurrence(doc, &needle, from, doc.len(), whole_words)
            .or_else(|| find_occurrence(doc, &needle, 0, from, whole_words))
            .filter(|&i| !self.selections.iter().any(|s| s.range().start == i));
        if let Some(start) = found {
            let mut selections = self.selections.clone();
            selections.push(Selection { anchor: start, head: start + needle.len() });
            let primary = selections.len() - 1;
            self.set_selections(selections, primary, cx);
            if whole_words {
                self.occurrence_words = Some(self.selections.clone());
            }
        }
    }

    fn add_cursor_vertical(&mut self, rows: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_wrap(window, cx);
        let primary = self.primary_selection();
        let goal = self.goal_x.unwrap_or_else(|| self.x_for_offset(primary.head, window, cx));
        let edge = if rows < 0 {
            self.selections.iter().min_by_key(|s| s.head).copied()
        } else {
            self.selections.iter().max_by_key(|s| s.head).copied()
        }
        .unwrap_or(primary);
        let drow = self.display_row_of(edge.head, cx) as isize + rows;
        if drow < 0 || drow as usize >= self.display_row_count(cx) {
            return;
        }
        let head = self.offset_for_display_row_x(drow as usize, goal, window, cx);
        let mut selections = self.selections.clone();
        selections.push(Selection::cursor(head));
        let primary_ix = selections.len() - 1;
        self.set_selections(selections, primary_ix, cx);
        self.goal_x = Some(goal);
    }

    fn add_cursor_above(&mut self, _: &AddCursorAbove, window: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertical(-1, window, cx);
    }

    fn add_cursor_below(&mut self, _: &AddCursorBelow, window: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertical(1, window, cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        // Escape leaves a snippet's stops (and still does the rest below).
        self.snippet = None;
        if self.open_hunk.take().is_some() {
            cx.notify();
            return;
        }
        if self.dismiss_lsp_popovers(cx) {
            return;
        }
        let p = self.primary_selection();
        if self.is_single_line() || (self.selections.len() == 1 && p.is_empty()) {
            // Nothing to collapse: let the parent (find bar, modal) handle Escape.
            cx.propagate();
            return;
        }
        let sel = if self.selections.len() > 1 { p } else { Selection::cursor(p.head) };
        self.set_selections(vec![sel], 0, cx);
    }

    fn copy_text(&self, cx: &App) -> (String, bool) {
        if self.selections.iter().all(|s| s.is_empty()) {
            // Copy whole lines when nothing is selected.
            let mut out = String::new();
            for block in self.selected_row_blocks(cx) {
                let start = self.doc.read(cx).line_start(block.start);
                let end = self.doc.read(cx).line_end(block.end - 1);
                out.push_str(&self.doc.read(cx).slice(start..end));
                out.push('\n');
            }
            (out, true)
        } else {
            let parts: Vec<String> = self.selections.iter().map(|s| self.doc.read(cx).slice(s.range())).collect();
            (parts.join("\n"), false)
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let (text, _) = self.copy_text(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let (text, whole_lines) = self.copy_text(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if whole_lines {
            let mut edits = Vec::new();
            for block in self.selected_row_blocks(cx) {
                let start = self.doc.read(cx).line_start(block.start);
                let end = (self.doc.read(cx).line_end(block.end - 1) + 1).min(self.doc.read(cx).len());
                edits.push((start..end, String::new()));
            }
            self.edit_and_remap(edits, EditKind::Other, cx);
        } else {
            self.edit_selections(EditKind::Other, cx, |_, s| (s.range(), String::new(), Some(0)));
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let mut text = crate::encoding::to_lf(text);
        if self.is_single_line() {
            text = text.trim_end_matches('\n').replace('\n', " ");
        }
        let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
        let n = self.selections.len();
        if n > 1 && lines.len() == n {
            let mut i = 0;
            self.edit_selections(EditKind::Other, cx, |_, s| {
                let t = lines[i].to_string();
                i += 1;
                let len = t.len();
                (s.range(), t, Some(len))
            });
        } else {
            let len = text.len();
            self.edit_selections(EditKind::Other, cx, |_, s| (s.range(), text.clone(), Some(len)));
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_step(cx);
    }

    pub(crate) fn undo_step(&mut self, cx: &mut Context<Self>) {
        let undone = self.change_doc(cx, |doc| doc.undo());
        if let Some(selections) = undone {
            let selections = self.clamp_selections(selections, cx);
            self.set_selections(selections, 0, cx);
            self.schedule_lint(Duration::from_millis(150), cx);
            self.edited(cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.redo_step(cx);
    }

    pub(crate) fn redo_step(&mut self, cx: &mut Context<Self>) {
        let redone = self.change_doc(cx, |doc| doc.redo());
        if let Some(selections) = redone {
            let selections = self.clamp_selections(selections, cx);
            self.set_selections(selections, 0, cx);
            self.schedule_lint(Duration::from_millis(150), cx);
            self.edited(cx);
        }
    }

    fn clamp_selections(&self, selections: Vec<Selection>, cx: &App) -> Vec<Selection> {
        let mut s: Vec<Selection> = selections
            .into_iter()
            .map(|s| Selection { anchor: self.doc.read(cx).clip(s.anchor), head: self.doc.read(cx).clip(s.head) })
            .collect();
        if s.is_empty() {
            s.push(Selection::cursor(0));
        }
        s
    }

    fn show_character_palette(&mut self, _: &ShowCharacterPalette, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    // ---- Mouse ---------------------------------------------------------------

    fn scrollbar_geometry(&self, cx: &App) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        let l = self.layout.as_ref()?;
        let viewport = l.bounds.size.height;
        let content = l.line_height * self.display_row_count(cx) as f32 + viewport - l.line_height;
        if content <= viewport {
            return None;
        }
        let track = Bounds::new(
            point(l.bounds.right() - px(SCROLLBAR_WIDTH), l.bounds.top()),
            size(px(SCROLLBAR_WIDTH), viewport),
        );
        let thumb_h = (viewport * (viewport / content)).max(px(28.));
        let max_scroll = content - viewport;
        let t = (self.scroll.y / max_scroll).clamp(0., 1.);
        let thumb = Bounds::new(
            // As thin as the lists' (`ui::scrollbar`); the track stays wide for its markers.
            point(track.right() - px(8.), track.top() + (viewport - thumb_h) * t),
            size(px(6.), thumb_h),
        );
        Some((track, thumb))
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.sync_wrap(window, cx);
        let Some(layout) = self.layout.clone() else { return };

        if self.minimap_mouse_down(event.position, cx) {
            return;
        }
        if let Some((track, thumb)) = self.scrollbar_geometry(cx).filter(|_| !self.shows_minimap(cx))
            && track.contains(&event.position)
        {
            if thumb.contains(&event.position) {
                self.scrollbar_drag = Some(event.position.y - thumb.top());
            } else {
                self.scrollbar_drag = Some(thumb.size.height / 2.);
                self.drag_scrollbar_to(event.position.y, cx);
            }
            return;
        }

        // A pinned line jumps to its definition.
        if let Some(offset) = self.sticky_at(event.position) {
            self.set_selections(vec![Selection::cursor(offset)], 0, cx);
            self.center_on_next_frame = true;
            cx.notify();
            return;
        }

        if !self.is_chromeless() {
            // A ▶ runs its target.
            if event.position.x < layout.bounds.left() + px(5. + RUN_ICON + 4.) {
                let y = event.position.y - layout.bounds.top() + self.scroll.y;
                let (row, seg) = self.line_of_display_row((y / layout.line_height).floor().max(0.) as usize, cx);
                if seg == 0
                    && let Some(target) = self.run_target_at(row, cx)
                {
                    cx.emit(EditorRunEvent { target, debug: false, menu_at: Some(event.position) });
                    return;
                }
            }
            // A line number sets or clears a breakpoint (in files that can be debugged).
            if let Some(row) = self.breakpoint_row_at(event.position, &layout, cx) {
                self.toggle_breakpoint_at(row, cx);
                return;
            }
            // A code lens runs its command.
            if let Some((_, lens)) = self.lens_hits.iter().find(|(b, _)| b.contains(&event.position)).cloned() {
                self.run_code_lens(lens, cx);
                return;
            }
            // Accept Current / Incoming / Both on a conflict.
            if let Some(&(_, start, resolution)) = self.conflict_hits.iter().find(|(b, _, _)| b.contains(&event.position)) {
                self.resolve_conflict(start, resolution, cx);
                return;
            }
            // The `⋯` placeholder unfolds.
            if let Some(&(_, start)) = self.fold_pills.iter().find(|(b, _)| b.contains(&event.position)) {
                self.folds.unfold_where(|f| f.start == start);
                cx.notify();
                return;
            }
            // The fold arrow toggles its region.
            let arrow_right = layout.bounds.left() + layout.gutter_width - px(FOLD_ZONE_RIGHT);
            if event.position.x <= arrow_right + px(2.) && event.position.x >= arrow_right - px(FOLD_ICON + 4.) {
                let y = event.position.y - layout.bounds.top() + self.scroll.y;
                let (row, seg) = self.line_of_display_row((y / layout.line_height).floor().max(0.) as usize, cx);
                if seg == 0 && (self.fold_under(row, cx).is_some() || self.fold_regions(cx).iter().any(|r| r.start == row)) {
                    self.toggle_fold_at(row, cx);
                    return;
                }
            }
        }

        // Clicking a change marker in the gutter toggles its popover.
        let bar_x = layout.bounds.left() + layout.gutter_width - px(GIT_BAR_OFFSET);
        if !self.is_chromeless() && (event.position.x - bar_x).abs() < px(6.) {
            let y = event.position.y - layout.bounds.top() + self.scroll.y;
            let row_f = y / layout.line_height;
            if let Some(ix) = self.hunk_at(row_f, cx) {
                self.open_hunk = if self.open_hunk == Some(ix) { None } else { Some(ix) };
                cx.notify();
                return;
            }
        }
        self.open_hunk = None;
        self.dismiss_lsp_popovers(cx);

        let offset = self.offset_for_position(event.position, window, cx);
        let in_gutter = event.position.x < layout.bounds.left() + layout.gutter_width;
        // ⌘-click (Ctrl-click elsewhere): Go to Declaration.
        if event.modifiers.secondary() && !in_gutter && event.click_count == 1 && !self.is_chromeless() {
            // Focus may go to the declaration's file, and ⌘ up with it.
            self.link_word(None, cx);
            self.set_selections(vec![Selection::cursor(offset)], 0, cx);
            self.navigate_from(offset, cx);
            return;
        }
        let (mode, unit) = if in_gutter || event.click_count >= 3 {
            (DragMode::Line, self.line_range(offset, cx))
        } else if event.click_count == 2 {
            (DragMode::Word, word_range(self.doc.read(cx), offset))
        } else {
            (DragMode::Char, offset..offset)
        };

        if event.modifiers.shift && event.modifiers.alt && !in_gutter && !self.is_single_line() {
            // Alt+Shift+click: a box from the caret to here (VS Code).
            let head = self.primary_selection().head;
            let from = (self.doc.read(cx).row_of(head), self.display_column_of(head, cx));
            self.drag = Some(Drag { mode: DragMode::Char, origin: head..head, column_from: Some(from) });
            self.select_box(from, event.position, cx);
        } else if event.modifiers.shift {
            let p = self.primary_selection();
            let head = if offset < p.anchor { unit.start } else { unit.end };
            let mut selections = self.selections.clone();
            selections[self.primary] = Selection { anchor: p.anchor, head };
            self.set_selections(selections, self.primary, cx);
            self.drag = Some(Drag { mode, origin: p.anchor..p.anchor, column_from: None });
        } else {
            let sel = Selection { anchor: unit.start, head: unit.end };
            if event.modifiers.alt {
                let mut selections = self.selections.clone();
                selections.push(sel);
                let ix = selections.len() - 1;
                self.set_selections(selections, ix, cx);
            } else {
                self.set_selections(vec![sel], 0, cx);
            }
            // Alt+drag selects a box (JetBrains); Alt+click alone adds a caret.
            let column_from = (event.modifiers.alt && mode == DragMode::Char && !in_gutter && !self.is_single_line())
                .then(|| (self.doc.read(cx).row_of(offset), self.column_at_x(event.position.x)));
            self.drag = Some(Drag { mode, origin: unit, column_from });
        }
        self.goal_x = None;
        self.autoscroll = false;
        self.restart_blink(window, cx);
    }

    /// The display column (tabs expanded) nearest to a window x.
    fn column_at_x(&self, x: Pixels) -> usize {
        let Some(layout) = self.layout.as_ref() else { return 0 };
        ((x - layout.text_left + self.scroll.x) / layout.char_width).round().max(0.) as usize
    }

    fn display_column_of(&self, offset: usize, cx: &App) -> usize {
        let row = self.doc.read(cx).row_of(offset);
        let start = self.doc.read(cx).line_start(row);
        DisplayLine::new(&self.doc.read(cx).line_text(row), self.tab_size).to_display(offset - start)
    }

    /// Column (box) selection: on every line from `from`'s row to the one
    /// under `to`, the same columns. Lines too short for a box that has width
    /// are skipped; a zero-width box puts a caret on each line.
    fn select_box(&mut self, from: (usize, usize), to: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.as_ref() else { return };
        let y = to.y - layout.bounds.top() + self.scroll.y;
        let drow = ((y / layout.line_height).floor().max(0.) as usize).min(self.display_row_count(cx).saturating_sub(1));
        let row = self.line_of_display_row(drow, cx).0.min(self.doc.read(cx).line_count() - 1);
        let col = self.column_at_x(to.x);
        let (left, right) = (from.1.min(col), from.1.max(col));
        let rows: Vec<usize> = if row >= from.0 { (from.0..=row).collect() } else { (row..=from.0).rev().collect() };
        let mut selections = Vec::new();
        for r in rows.into_iter().filter(|&r| !self.row_hidden(r)) {
            let text = self.doc.read(cx).line_text(r);
            let dl = DisplayLine::new(&text, self.tab_size);
            if left > dl.text.len() && right > left {
                continue;
            }
            let start = self.doc.read(cx).line_start(r);
            let (a, b) = (start + dl.to_buffer(from.1), start + dl.to_buffer(col));
            selections.push(Selection { anchor: a, head: b });
        }
        if selections.is_empty() {
            let start = self.doc.read(cx).line_start(from.0);
            selections.push(Selection::cursor(start + DisplayLine::new(&self.doc.read(cx).line_text(from.0), self.tab_size).to_buffer(from.1)));
        }
        let primary = selections.len() - 1;
        self.set_selections(selections, primary, cx);
    }

    /// The row whose line number is at `position`, where clicks set breakpoints.
    fn breakpoint_row_at(&self, position: Point<Pixels>, layout: &LayoutSnapshot, cx: &App) -> Option<usize> {
        let number_right = layout.bounds.left() + layout.gutter_width - px(FOLD_ZONE_RIGHT + 18.);
        if !self.can_break(cx) || position.x < layout.bounds.left() + px(5. + RUN_ICON + 4.) || position.x > number_right + px(4.) {
            return None;
        }
        let y = position.y - layout.bounds.top() + self.scroll.y;
        let (row, seg) = self.line_of_display_row((y / layout.line_height).floor().max(0.) as usize, cx);
        (seg == 0 && row < self.doc.read(cx).line_count()).then_some(row)
    }

    /// Right-click: place the caret (unless clicking inside a selection, so
    /// Cut/Copy apply to it) and ask the workspace for the context menu. On
    /// a line number, the breakpoint menu instead.
    fn on_right_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full {
            return;
        }
        window.focus(&self.focus_handle);
        self.sync_wrap(window, cx);
        self.open_hunk = None;
        if let Some(layout) = self.layout.clone()
            && let Some(row) = self.breakpoint_row_at(event.position, &layout, cx)
        {
            cx.emit(EditorBreakpointMenu { row, position: event.position });
            return;
        }
        let offset = self.offset_for_position(event.position, window, cx);
        let inside = self.selections.iter().any(|s| !s.is_empty() && s.range().contains(&offset));
        if !inside {
            self.set_selections(vec![Selection::cursor(offset)], 0, cx);
            self.goal_x = None;
        }
        self.restart_blink(window, cx);
        cx.emit(EditorContextMenu(event.position));
    }

    pub fn soft_wrap(&self) -> bool {
        self.soft_wrap
    }

    /// Whether the caret is on a changed (git) line, for Rollback Lines.
    pub fn has_change_at_cursor(&self, cx: &App) -> bool {
        self.current_hunk(cx).is_some()
    }

    /// Whether the change at the caret is staged (`None`: no change there).
    pub fn change_at_cursor_staged(&self, cx: &App) -> Option<bool> {
        self.current_hunk(cx).map(|i| self.hunk_staged.get(i).copied().unwrap_or(false))
    }

    /// Whether the caret is in a merge conflict (as of the last frame).
    pub fn has_conflict_at_cursor(&self, cx: &App) -> bool {
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        self.conflicts.current(&self.doc.read(cx).version()).is_some_and(|c| c.iter().any(|c| c.rows().contains(&row)))
    }

    fn line_range(&self, offset: usize, cx: &App) -> Range<usize> {
        let row = self.doc.read(cx).row_of(offset);
        let start = self.doc.read(cx).line_start(row);
        let end = (self.doc.read(cx).line_end(row) + 1).min(self.doc.read(cx).len());
        start..end
    }

    fn drag_scrollbar_to(&mut self, y: Pixels, cx: &mut Context<Self>) {
        if self.shows_minimap(cx) {
            if let Some(grab) = self.scrollbar_drag {
                self.drag_minimap_to(y, grab, cx);
            }
            return;
        }
        let (Some(grab), Some((track, thumb)), Some(l)) = (self.scrollbar_drag, self.scrollbar_geometry(cx), self.layout.as_ref()) else {
            return;
        };
        let viewport = l.bounds.size.height;
        let content = l.line_height * self.display_row_count(cx) as f32 + viewport - l.line_height;
        let range = track.size.height - thumb.size.height;
        if range <= px(0.) {
            return;
        }
        let t = ((y - grab - track.top()) / range).clamp(0., 1.);
        self.scroll.y = (content - viewport) * t;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            self.scrollbar_drag = None;
            let over_gutter = !self.is_chromeless()
                && self.layout.as_ref().is_some_and(|l| {
                    l.bounds.contains(&event.position) && event.position.x < l.bounds.left() + l.gutter_width
                });
            if over_gutter != self.gutter_hovered {
                self.gutter_hovered = over_gutter;
                cx.notify();
            }
            // The pinned lines aren't the code under them, and block rows
            // (code lenses) aren't the line above.
            if self.sticky_at(event.position).is_some()
                || self.on_block_row(event.position)
                || self.minimap_geometry(cx).is_some_and(|g| g.bounds.contains(&event.position))
            {
                self.pointer_left(window, cx);
            } else {
                self.hover_at(event.position, window, cx);
            }
            self.update_link(event.position, event.modifiers.secondary(), window, cx);
            return;
        }
        if self.scrollbar_drag.is_some() {
            self.drag_scrollbar_to(event.position.y, cx);
            return;
        }
        let Some(drag) = self.drag.as_ref() else { return };
        let (mode, origin, column_from) = (drag.mode, drag.origin.clone(), drag.column_from);
        self.sync_wrap(window, cx);
        if let Some(from) = column_from {
            let row = self.doc.read(cx).row_of(self.offset_for_position(event.position, window, cx));
            // Still on the spot it started: an Alt+click, not a box yet.
            if (row, self.column_at_x(event.position.x)) != from {
                self.select_box(from, event.position, cx);
            }
            self.autoscroll = self.layout.as_ref().is_some_and(|l| !l.bounds.contains(&event.position));
            return;
        }
        let offset = self.offset_for_position(event.position, window, cx);
        let unit = match mode {
            DragMode::Char => offset..offset,
            DragMode::Word => word_range(self.doc.read(cx), offset),
            DragMode::Line => self.line_range(offset, cx),
        };
        let sel = if offset < origin.start {
            Selection { anchor: origin.end, head: unit.start }
        } else {
            Selection { anchor: origin.start, head: unit.end.max(origin.end) }
        };
        let mut selections = self.selections.clone();
        selections[self.primary] = sel;
        self.set_selections(selections, self.primary, cx);
        // Only scroll when the pointer leaves the viewport.
        self.autoscroll = self
            .layout
            .as_ref()
            .is_some_and(|l| !l.bounds.contains(&event.position));
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.vim_after_mouse(cx);
        }
        self.scrollbar_drag = None;
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        let mut delta = event.delta.pixel_delta(line_height);
        // Lock to the dominant axis to avoid diagonal jitter on trackpads.
        if delta.x.abs() > delta.y.abs() {
            delta.y = px(0.);
        } else {
            delta.x = px(0.);
        }
        self.scroll.y -= delta.y;
        self.scroll.x -= delta.x;
        self.clamp_scroll(cx);
        // The word moved away from the pointer; the next move finds the new one.
        self.link_word(None, cx);
        self.lsp.hover = None;
        cx.notify();
    }

    fn clamp_scroll(&mut self, cx: &App) {
        let Some(l) = self.layout.as_ref() else { return };
        let max_y = l.line_height * (self.display_row_count(cx).saturating_sub(1)) as f32;
        self.scroll.y = self.scroll.y.clamp(px(0.), max_y.max(px(0.)));
        if self.wrap.as_ref().is_some_and(|w| w.key.wrap) {
            self.scroll.x = px(0.);
            return;
        }
        let text_width = l.bounds.right() - l.text_left - self.right_edge_width(cx);
        let max_x = (self.max_line_width + l.char_width * 4. - text_width).max(px(0.));
        self.scroll.x = self.scroll.x.clamp(px(0.), max_x);
    }

    // ---- Frame layout ------------------------------------------------------------

    fn layout_frame(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> FrameLayout {
        let theme = Theme::get(cx).clone();
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();
        // Text fields skip the code chrome; only one-line fields center vertically.
        let single = self.is_chromeless();
        let font_size = self.font_size(cx);
        let line_height = self.line_height(cx);
        let editor_font = self.editor_font(cx);
        // Single-line fields center their one row vertically.
        let top = if self.is_single_line() {
            bounds.top() + ((bounds.size.height - line_height) / 2.).max(px(0.))
        } else {
            bounds.top()
        };
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&editor_font);
        let char_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(font_size * 0.6);

        let line_count = self.doc.read(cx).line_count();
        let digits = line_count.to_string().len().max(3);
        // Diagnostic dot, numbers, fold arrow, git bar.
        let gutter_width = if single { px(0.) } else { char_width * digits as f32 + px(58.) };
        let text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_width, bounds.top()),
            bounds.bottom_right(),
        );
        let text_left = text_bounds.left() + if single { px(0.) } else { px(TEXT_PADDING) };
        let rows_per_page = ((bounds.size.height / line_height).floor() as usize).saturating_sub(2).max(1);
        let wrap_width = (bounds.right() - text_left - if single { char_width } else { self.right_edge_width(cx) + char_width })
            .max(char_width * 10.);

        self.layout = Some(LayoutSnapshot {
            bounds,
            text_left,
            line_height,
            char_width,
            gutter_width,
            rows_per_page,
            wrap_width,
        });
        self.sync_wrap(window, cx);
        let wrapping = self.wrap.as_ref().is_some_and(|w| w.key.wrap);

        // Keep the primary cursor visible after keyboard navigation.
        if std::mem::take(&mut self.reveal_primary) {
            let drow = self.display_row_of(self.primary_selection().head, cx) as f32;
            let visible = self.scroll.y..self.scroll.y + bounds.size.height - line_height;
            if !visible.contains(&(line_height * drow)) {
                self.center_on_next_frame = true;
            } else {
                self.autoscroll = true;
            }
        }
        if let Some(line) = self.scroll_to_line.take().filter(|_| !single) {
            // A restored tab's view as it was, wherever its caret is.
            self.scroll.y = line_height * self.rows_start(line) as f32;
            self.scroll.x = px(0.);
            self.center_on_next_frame = false;
            self.autoscroll = false;
        } else if std::mem::take(&mut self.center_on_next_frame) && !single {
            let drow = self.display_row_of(self.primary_selection().head, cx);
            self.scroll.y = (line_height * drow as f32 - bounds.size.height / 3.).max(px(0.));
            self.scroll.x = px(0.);
        } else if std::mem::take(&mut self.autoscroll) {
            let head = self.primary_selection().head;
            let drow = self.display_row_of(head, cx);
            let margin = line_height * 3.;
            let top = line_height * drow as f32;
            let viewport = bounds.size.height;
            // Pinned lines cover the top; keep the caret below them.
            let top_margin = margin + line_height * self.sticky_rows.len() as f32;
            if top - top_margin < self.scroll.y {
                self.scroll.y = (top - top_margin).max(px(0.));
            } else if top + line_height + margin > self.scroll.y + viewport {
                self.scroll.y = top + line_height + margin - viewport;
            }
            if !wrapping {
                let x = self.x_for_offset(head, window, cx);
                let text_width = bounds.right() - text_left - if single { px(0.) } else { self.right_edge_width(cx) };
                if x < self.scroll.x + char_width * 2. {
                    self.scroll.x = (x - char_width * 4.).max(px(0.));
                } else if x > self.scroll.x + text_width - char_width * 3. {
                    self.scroll.x = x - text_width + char_width * 6.;
                }
            }
        }
        self.clamp_scroll(cx);

        // Visible display rows, and the buffer lines they show.
        let display_rows = self.display_row_count(cx);
        let first_drow = (self.scroll.y / line_height).floor() as usize;
        let visible_rows = (bounds.size.height / line_height).ceil() as usize + 1;
        let last_drow = (first_drow + visible_rows).min(display_rows);
        let first_drow = first_drow.min(last_drow);
        let first_row = self.line_of_display_row(first_drow, cx).0;
        let last_row = if last_drow == 0 { 0 } else { (self.line_of_display_row(last_drow - 1, cx).0 + 1).min(line_count) };
        // The bytes on screen: a long line's only on its rows on screen.
        let range_start = self.doc.read(cx).line_start(first_row) + self.shown_columns(first_row, first_drow, last_drow, cx).start;
        let range_end = if last_row == 0 {
            0
        } else {
            self.doc.read(cx).line_start(last_row - 1) + self.shown_columns(last_row - 1, first_drow, last_drow, cx).end
        };
        if !single {
            // Inlay hints and lens titles are asked for by what's in view.
            self.lsp_view(first_row..last_row, cx);
        }
        self.fold_pills.clear();
        // Fold arrows show for folded headers, and for every foldable line
        // while the pointer is over the gutter.
        let fold_regions = (!single && (self.gutter_hovered || !self.folds.is_empty())).then(|| self.shown_fold_regions(cx));
        let run_rows = if single { Arc::default() } else { self.shown_run_rows(cx) };
        let conflicts = if single { Arc::new(Vec::new()) } else { self.shown_conflicts(cx) };
        let debug_row = self.doc.read(cx).debug_row();
        self.conflict_hits.clear();
        let highlights = self.visible_highlights(first_drow, last_drow, first_row, last_row, cx);
        // Bracket pairs around carets: the first few, or with many, the first on screen.
        let brackets: Vec<Range<usize>> = if focused && !single {
            let pool = if self.selections.len() <= 8 { &self.selections[..] } else { self.selections_touching(range_start..=range_end) };
            let heads = pool.iter().filter(|s| s.is_empty()).take(8).map(|s| s.head).collect();
            self.bracket_pairs(heads, cx).iter().filter(|r| (range_start..=range_end).contains(&r.start)).cloned().collect()
        } else {
            Vec::new()
        };
        // The diagnostics on screen (sorted by start), without walking the
        // ones before them.
        self.reach.update(self.doc.read(cx));
        let diagnostics: Vec<&lint::Diagnostic> = self.reach.find(&self.doc.read(cx).diagnostics, range_start..=range_end);
        // Per line with problems: its worst one and how many start on it.
        let row_problems = if single { Arc::default() } else { self.row_problems(cx) };

        let mut frame = FrameLayout {
            text_bounds,
            line_height,
            ..Default::default()
        };
        // Rulers: full-height guides at fixed columns.
        if !single {
            for &col in &self.file_settings.rulers {
                let x = text_left - self.scroll.x + char_width * col as f32;
                if x >= text_bounds.left() && x <= text_bounds.right() {
                    frame.guides.push(fill(Bounds::new(point(x, text_bounds.top()), size(px(1.), text_bounds.size.height)), theme.indent_guide));
                }
            }
        }
        let show_whitespace = if single { ShowWhitespace::None } else { self.file_settings.show_whitespace };
        let whitespace_color = Hsla::from(theme.text_dim).alpha(0.55);
        let ui_font = font(theme.ui_font.clone());

        let tab_size = self.tab_size;
        // Guides at each indent level of the file's own style.
        let guide_step = if self.indent_unit == "\t" { tab_size } else { self.indent_unit.len().max(1) };
        // Indentation (in columns) for indent guides; None for blank lines.
        let indents: Vec<Option<usize>> = (first_row..last_row)
            .map(|row| match self.leading_whitespace(row, cx) {
                (_, true) => None,
                (indent, false) => Some(indent),
            })
            .collect();

        // Block rows, in the gaps the display map leaves above their lines.
        self.lens_hits.clear();
        // The line whose problems show in full under it (no hint at its end then).
        let mut problems_row = None;
        let blocks = if self.wrap.is_some() { self.blocks(cx) } else { Arc::default() };
        if let Some(map) = self.wrap.as_ref() {
            let mut lens_hits = Vec::new();
            let mut used: HashMap<usize, usize> = HashMap::new();
            let run = |text: &str, font: Font, color: Hsla| TextRun { len: text.len(), font, color, background_color: None, underline: None, strikethrough: None };
            // Only the blocks of lines on screen (blocks after the last line
            // too, at the end): they're sorted by line.
            let first_line = map.line_at(first_drow);
            let last_line = if last_drow >= display_rows { usize::MAX } else { map.line_at(last_drow.saturating_sub(1)) };
            let shown = blocks.partition_point(|b| b.line < first_line);
            for block in blocks[shown..].iter().take_while(|b| b.line <= last_line) {
                // A fold hides the blocks above the lines it hides (no rows
                // for them in the map; drawn, they'd cover the lines below).
                if block.line < line_count && map.hidden(block.line) {
                    continue;
                }
                let offset = used.entry(block.line).or_insert(0);
                let start = map.block_start(block.line) + *offset;
                *offset += block.rows();
                let visible = |drow: usize| (first_drow..last_drow).contains(&drow);
                let y_of = |drow: usize| top + line_height * drow as f32 - self.scroll.y;
                match &block.kind {
                    BlockKind::Deleted(lines) => {
                        let tint = Hsla::from(theme.git_deleted);
                        for (i, line) in lines.iter().enumerate().filter(|(i, _)| visible(start + i)) {
                            let y = y_of(start + i);
                            frame.backgrounds.push(fill(Bounds::from_corners(point(bounds.left(), y), point(bounds.right(), y + line_height)), tint.alpha(0.12)));
                            let text = DisplayLine::new(line, self.tab_size).text;
                            let shaped = text_system.shape_line(SharedString::from(text.clone()), font_size, &[run(&text, editor_font.clone(), theme.text_muted.into())], None);
                            frame.lines.push((point(text_left - self.scroll.x, y), shaped));
                            let minus = text_system.shape_line("−".into(), font_size, &[run("−", editor_font.clone(), tint)], None);
                            frame.numbers.push((point(text_left - char_width * 2., y), minus));
                        }
                    }
                    BlockKind::Lenses(lenses) if visible(start) => {
                        // Lined up with the line's text, smaller, like a caption.
                        let y = y_of(start);
                        let indent = self.leading_whitespace(block.line.min(line_count.saturating_sub(1)), cx).0;
                        let caption = font_size * 0.85;
                        let mut x = text_left - self.scroll.x + char_width * indent as f32;
                        // One still without its title keeps its row empty until it comes.
                        for (i, lens) in lenses.iter().filter(|l| l.unresolved.is_none()).enumerate() {
                            if i > 0 {
                                let bar = text_system.shape_line(" | ".into(), caption, &[run(" | ", ui_font.clone(), Hsla::from(theme.text_dim).alpha(0.6))], None);
                                let width = bar.width;
                                frame.lines.push((point(x, y + (line_height - caption) / 4.), bar));
                                x += width;
                            }
                            let title = text_system.shape_line(lens.title.clone().into(), caption, &[run(&lens.title, ui_font.clone(), theme.text_dim.into())], None);
                            let hit = Bounds::new(point(x, y), size(title.width, line_height));
                            lens_hits.push((hit, lens.clone()));
                            frame.lens_bounds.push(hit);
                            x += title.width;
                            frame.lines.push((point(hit.left(), y + (line_height - caption) / 4.), title));
                        }
                    }
                    BlockKind::Lenses(_) => {}
                    BlockKind::Problems { of, rows } => {
                        // Under the caret line, in the hint's colors, lined up with its text.
                        let indent = self.leading_whitespace(*of, cx).0;
                        let msg_size = px(theme.ui_font_size - 0.5);
                        for (i, (severity, text)) in rows.iter().enumerate().filter(|(i, _)| visible(start + i)) {
                            let y = y_of(start + i);
                            let color = severity_color(&theme, *severity);
                            frame.backgrounds.push(fill(
                                Bounds::from_corners(point(bounds.left() + gutter_width, y), point(bounds.right(), y + line_height)),
                                color.alpha(0.07),
                            ));
                            let shaped = text_system.shape_line(SharedString::from(text.clone()), msg_size, &[run(text, ui_font.clone(), color.alpha(0.9))], None);
                            frame.lines.push((point(text_left - self.scroll.x + char_width * indent as f32, y + (line_height - msg_size) / 4.), shaped));
                        }
                    }
                }
            }
            self.lens_hits = lens_hits;
        }
        // The caret line's problems show in full under it, on screen or not.
        if let (Some(map), Some((line, of))) = (self.wrap.as_ref(), self.blocks_cache.as_ref().and_then(|c| c.problems))
            && !(line < line_count && map.hidden(line))
        {
            problems_row = Some(of);
        }

        let mut hl_ix = 0;
        for row in first_row..last_row {
            if self.row_hidden(row) {
                continue;
            }
            let row_first_drow = self.first_display_row(row);
            let (line_start, line_end) = (self.doc.read(cx).line_start(row), self.doc.read(cx).line_end(row));
            // This line's rows on screen.
            let breaks_count = self.line_breaks(row).len();
            if row_first_drow + breaks_count < first_drow || row_first_drow >= last_drow {
                continue;
            }
            let (seg_lo, seg_hi) = (first_drow.saturating_sub(row_first_drow), (last_drow - 1 - row_first_drow).min(breaks_count));
            let segs = Segments::new(self.line_breaks(row), seg_lo..=seg_hi);
            // The part of the line laid out: all of it, or a long line's rows
            // on screen; its buffer text, from column `part.start`.
            let (text, dl) = self.display_part(row, seg_lo..=seg_hi, cx);
            let part = dl.columns();
            let (part_start, part_end) = (line_start + part.start, line_start + part.end);
            let col_of = |offset: usize| offset.saturating_sub(line_start).clamp(part.start, part.end);

            // Build styled runs from highlight spans and diagnostics.
            while hl_ix < highlights.len() && highlights[hl_ix].0.end <= part_start {
                hl_ix += 1;
            }
            let mut cuts: Vec<usize> = vec![part.start, part.end];
            for (r, _) in highlights[hl_ix..].iter().take_while(|(r, _)| r.start < part_end) {
                cuts.push(col_of(r.start));
                cuts.push(col_of(r.end));
            }
            let row_diags: Vec<&&lint::Diagnostic> = diagnostics
                .iter()
                .filter(|d| d.range.start <= part_end && d.range.end > part_start)
                .collect();
            for d in &row_diags {
                cuts.push(col_of(d.range.start));
                cuts.push(col_of(d.range.end));
            }
            if let Some(link) = self.lsp.link.as_ref().filter(|l| l.start < part_end && l.end > part_start) {
                cuts.push(col_of(link.start));
                cuts.push(col_of(link.end));
            }
            cuts.sort_unstable();
            cuts.dedup();

            let mut runs = Vec::with_capacity(cuts.len());
            let mut h = hl_ix;
            for w in cuts.windows(2) {
                let (s, e) = (w[0], w[1]);
                let abs = line_start + s;
                while h < highlights.len() && highlights[h].0.end <= abs {
                    h += 1;
                }
                let kind = highlights
                    .get(h)
                    .filter(|(r, _)| r.start <= abs && abs < r.end)
                    .map(|(_, k)| *k)
                    .unwrap_or_default();
                let mut run_font = editor_font.clone();
                if kind.base() == crate::syntax::Highlight::Comment && theme.italic_comments {
                    run_font.style = FontStyle::Italic;
                }
                let severity = row_diags
                    .iter()
                    .filter(|d| d.range.start <= abs && abs < d.range.end)
                    .map(|d| d.severity)
                    .min();
                let len = dl.to_display(e) - dl.to_display(s);
                if len == 0 {
                    continue;
                }
                // ⌘/Ctrl-hover link: a plain underline, like the terminal's links.
                let linked = self.lsp.link.as_ref().is_some_and(|l| l.start <= abs && abs < l.end);
                let underline = if linked {
                    Some(UnderlineStyle { thickness: px(1.), color: Some(theme.accent.into()), wavy: false })
                } else {
                    severity.map(|sev| UnderlineStyle { thickness: px(1.), color: Some(severity_color(&theme, sev)), wavy: true })
                };
                runs.push(TextRun {
                    len,
                    font: run_font,
                    color: theme.highlight_color(kind),
                    background_color: None,
                    underline,
                    strikethrough: None,
                });
            }

            if !dl.inlays.is_empty() {
                runs = inlay_runs(runs, &dl, editor_font.clone(), theme.text_dim.into());
            }
            if !dl.controls.is_empty() {
                let controls: Vec<Range<usize>> = dl.controls.iter().map(|r| dl.local(r.clone())).collect();
                runs = restyle_runs(runs, &controls, |piece| piece.color = theme.text_dim.into());
            }
            // The selections on this part of the line (sorted and apart: found
            // by binary search, however many there are).
            let row_sels: Vec<Selection> = self.selections_touching(part_start..=part_end).to_vec();
            let is_cursor_row = self.selections_touching(line_start..=line_end).iter().any(|s| s.is_empty());
            let indent = indents[row - first_row].unwrap_or_else(|| {
                let prev = indents[..row - first_row].iter().rev().find_map(|i| *i).unwrap_or(0);
                let next = indents[row - first_row..].iter().find_map(|i| *i).unwrap_or(0);
                prev.min(next)
            });
            // Hints get no dot or message at the line's end.
            let worst = row_problems
                .binary_search_by_key(&row, |&(r, _, _)| r)
                .ok()
                .and_then(|i| {
                    let (_, worst, count) = row_problems[i];
                    Some((self.doc.read(cx).diagnostics.get(worst?)?, count))
                });
            // Where the line's text starts and ends after its whitespace, for `show_whitespace`.
            let body = matches!(show_whitespace, ShowWhitespace::Boundary | ShowWhitespace::Trailing).then(|| self.body_columns(row, cx));

            // Each wrapped segment of the line is its own display row.
            for seg in seg_lo..=seg_hi {
                let drow = row_first_drow + seg;
                if drow < first_drow || drow >= last_drow {
                    continue;
                }
                let (seg_range, wrap_indent) = segs.range(seg, dl.end());
                let first_seg = seg == 0;
                let last_seg = seg == breaks_count;
                let seg_runs = if breaks_count == 0 { std::mem::take(&mut runs) } else { slice_runs(&runs, dl.local(seg_range.clone())) };
                let shaped = text_system.shape_line(
                    SharedString::from(dl.slice(seg_range.clone()).to_string()),
                    font_size,
                    &seg_runs,
                    None,
                );
                if !wrapping {
                    self.max_line_width = self.max_line_width.max(shaped.width);
                }
                let y = top + line_height * drow as f32 - self.scroll.y;
                let base_x = text_left - self.scroll.x;
                let origin = point(base_x + char_width * wrap_indent as f32, y);
                // x of a display column of the line, clamped to this segment.
                let x_of = |col: usize| shaped.x_for_index(col.clamp(seg_range.start, seg_range.end) - seg_range.start);
                // The part of a line-relative buffer range on this segment (in
                // display columns), and whether it runs on past the segment.
                let clip = |start: usize, end: usize, past_line_end: bool| -> Option<(usize, usize, bool)> {
                    let (ds, de) = (dl.to_display(start), dl.to_display(end));
                    if ds > seg_range.end || de < seg_range.start || (!last_seg && ds == seg_range.end) {
                        return None;
                    }
                    let (s, e) = (ds.max(seg_range.start), de.min(seg_range.end));
                    let continues = if last_seg { past_line_end } else { de >= seg_range.end };
                    (s < e || continues).then_some((s, e, continues))
                };

                // The line a debug session is paused on.
                if debug_row == Some(row) && !single {
                    frame.backgrounds.push(fill(
                        Bounds::from_corners(point(bounds.left(), y), point(bounds.right(), y + line_height)),
                        Hsla::from(theme.warning).alpha(0.16),
                    ));
                }

                // Current line highlight.
                if is_cursor_row && self.selections.len() == 1 && !single {
                    frame.backgrounds.push(fill(
                        Bounds::from_corners(point(bounds.left(), y), point(bounds.right(), y + line_height)),
                        theme.current_line,
                    ));
                }

                // Merge conflict regions: current side green, incoming blue,
                // markers stronger.
                if let Some(c) = conflicts.iter().find(|c| c.rows().contains(&row)) {
                    let marker = row == c.start || row == c.end || row + 1 == c.theirs.start || c.base.as_ref().is_some_and(|b| row + 1 == b.start);
                    let color: Hsla = if row == c.start || c.ours.contains(&row) {
                        theme.git_added.into()
                    } else if row == c.end || c.theirs.contains(&row) {
                        theme.git_modified.into()
                    } else {
                        theme.text_dim.into()
                    };
                    frame.backgrounds.push(fill(
                        Bounds::from_corners(point(bounds.left() + gutter_width, y), point(bounds.right(), y + line_height)),
                        color.alpha(if marker { 0.24 } else { 0.11 }),
                    ));
                }

                // Indent guides.
                let mut col = 0;
                while !single && col + guide_step <= indent.max(1) && col < indent {
                    let x = base_x + char_width * col as f32;
                    if x >= text_bounds.left() {
                        frame.guides.push(fill(
                            Bounds::new(point(x, y), size(px(1.), line_height)),
                            theme.indent_guide,
                        ));
                    }
                    col += guide_step;
                }

                // Inlay hints sit on a soft chip (sorted: the row's by binary search).
                let first_span = dl.inlays.partition_point(|s| s.label.start < seg_range.start);
                for span in dl.inlays[first_span..].iter().take_while(|s| s.label.start <= seg_range.end).filter(|s| s.label.end <= seg_range.end) {
                    let bounds = Bounds::from_corners(
                        point(origin.x + x_of(span.label.start) - px(2.), y + px(2.)),
                        point(origin.x + x_of(span.label.end) + px(2.), y + line_height - px(2.)),
                    );
                    frame.search.push(fill(bounds, Hsla::from(theme.text_dim).alpha(0.13)).corner_radii(px(3.)));
                }
                // So do control characters' symbols, each on its own.
                let first_symbol = dl.controls.partition_point(|r| r.start < seg_range.start);
                for symbol in dl.controls[first_symbol..].iter().take_while(|r| r.end <= seg_range.end) {
                    let bounds = Bounds::from_corners(
                        point(origin.x + x_of(symbol.start) + px(0.5), y + px(2.)),
                        point(origin.x + x_of(symbol.end) - px(0.5), y + line_height - px(2.)),
                    );
                    frame.search.push(fill(bounds, Hsla::from(theme.text_dim).alpha(0.13)).corner_radii(px(3.)));
                }

                // Spaces as dots and tabs as lines, where `show_whitespace` asks.
                if show_whitespace != ShowWhitespace::None {
                    let (body_start, body_end) = body.unwrap_or_default();
                    let selected: Vec<Range<usize>> = row_sels.iter().map(|s| s.range()).filter(|r| !r.is_empty()).collect();
                    // Only this row's part of the line (a long line has many rows).
                    let seg_text = dl.to_buffer(seg_range.start)..dl.to_buffer(seg_range.end).max(dl.to_buffer(seg_range.start));
                    let local = seg_text.start.saturating_sub(part.start).min(text.len())..seg_text.end.saturating_sub(part.start).min(text.len());
                    let chars = text[local].char_indices().map(|(i, c)| (seg_text.start + i, c));
                    for (i, c) in chars.filter(|(_, c)| *c == ' ' || *c == '\t') {
                        let shown = match show_whitespace {
                            ShowWhitespace::All => true,
                            ShowWhitespace::Boundary => i < body_start || i >= body_end,
                            ShowWhitespace::Trailing => i >= body_end,
                            ShowWhitespace::Selection => selected.iter().any(|r| r.contains(&(line_start + i))),
                            ShowWhitespace::None => false,
                        };
                        let (ds, de) = (dl.to_display(i), dl.to_display(i + 1));
                        if !shown || ds < seg_range.start || ds >= seg_range.end {
                            continue;
                        }
                        let (x0, x1) = (origin.x + x_of(ds), origin.x + x_of(de));
                        let mid = y + line_height / 2.;
                        let quad = if c == ' ' {
                            let d = (char_width * 0.16).max(px(1.5));
                            Bounds::new(point((x0 + x1) / 2. - d / 2., mid - d / 2.), size(d, d))
                        } else {
                            let inset = char_width * 0.3;
                            Bounds::from_corners(point(x0 + inset, mid), point(x1 - inset, mid + px(1.)))
                        };
                        frame.guides.push(fill(quad, whitespace_color));
                    }
                }

                // Search matches (found by binary search: there can be many).
                let matches = &self.search_matches.ranges;
                let first_match = matches.partition_point(|m| m.end < part_start);
                for (ix, m) in matches.iter().enumerate().skip(first_match) {
                    if m.start > part_end {
                        break;
                    }
                    let Some((s, e, continues)) =
                        clip(m.start.max(part_start) - line_start, m.end.min(part_end) - line_start, m.end > line_end)
                    else {
                        continue;
                    };
                    let x0 = x_of(s);
                    let mut x1 = x_of(e);
                    if continues {
                        x1 += char_width * 0.6;
                    }
                    let bounds = Bounds::from_corners(point(origin.x + x0, y + px(1.)), point(origin.x + x1, y + line_height - px(1.)));
                    let quad = if Some(ix) == self.active_match {
                        gpui::quad(bounds, px(3.), theme.search_active, px(1.), Hsla::from(theme.search_active).alpha(0.9), gpui::BorderStyle::Solid)
                    } else {
                        fill(bounds, theme.search_match).corner_radii(px(3.))
                    };
                    frame.search.push(quad);
                }

                // Other uses of the symbol at the caret (from the language server).
                let symbols = self.current_symbol_highlights(cx);
                for r in symbols.iter().skip(symbols.partition_point(|r| r.end < part_start)) {
                    if r.start > part_end {
                        break;
                    }
                    let Some((s, e, _)) = clip(r.start.max(part_start) - line_start, r.end.min(part_end) - line_start, false) else {
                        continue;
                    };
                    let bounds = Bounds::from_corners(point(origin.x + x_of(s), y + px(1.)), point(origin.x + x_of(e), y + line_height - px(1.)));
                    frame.search.push(fill(bounds, theme.bracket_match).corner_radii(px(3.)));
                }

                // Matching brackets (painted with search highlights, under the text).
                for b in brackets.iter().filter(|b| b.start >= part_start && b.start < part_end) {
                    let col = dl.to_display(b.start - line_start);
                    if segs.of(col) != seg {
                        continue;
                    }
                    let (x0, x1) = (x_of(col), x_of(dl.to_display(b.end.min(line_end) - line_start)));
                    frame.search.push(
                        fill(
                            Bounds::from_corners(point(origin.x + x0, y + px(1.)), point(origin.x + x1, y + line_height - px(1.))),
                            theme.bracket_match,
                        )
                        .corner_radii(px(2.)),
                    );
                }

                // Selections.
                for sel in &row_sels {
                    let r = sel.range();
                    if sel.is_empty() || r.end < part_start || r.start > part_end {
                        continue;
                    }
                    if r.end == line_start && r.start < line_start {
                        continue;
                    }
                    let Some((s, e, continues)) =
                        clip(r.start.max(part_start) - line_start, r.end.min(part_end) - line_start, r.end > line_end)
                    else {
                        continue;
                    };
                    let x0 = x_of(s);
                    let mut x1 = x_of(e);
                    if continues {
                        x1 += char_width * 0.6;
                    }
                    if x1 > x0 {
                        let color = if focused { theme.selection } else { theme.selection_inactive };
                        frame.selections.push(
                            fill(
                                Bounds::from_corners(point(origin.x + x0, y), point(origin.x + x1, y + line_height)),
                                color,
                            )
                            .corner_radii(px(3.)),
                        );
                    }
                }

                // Cursors (blocks in Vim's Normal and Visual modes).
                let block = self.vim_block_cursor();
                if focused && (self.cursor_visible || block) {
                    for sel in &row_sels {
                        let head = self.vim_cursor_of(sel);
                        if head < part_start || head > part_end {
                            continue;
                        }
                        let col = dl.to_display(head - line_start);
                        if segs.of(col) != seg {
                            continue;
                        }
                        let x = x_of(col);
                        let bounds = if block {
                            let next = dl.to_display((head - line_start + self.doc.read(cx).char_at(head).filter(|c| *c != '\n').map_or(0, char::len_utf8)).min(part.end));
                            let width = if next > col { x_of(next) - x } else { char_width };
                            Bounds::new(point(origin.x + x, y + px(1.)), size(width, line_height - px(2.)))
                        } else {
                            Bounds::new(point(origin.x + x - px(1.), y + px(2.)), size(px(2.), line_height - px(4.)))
                        };
                        let color = if block { Hsla::from(theme.cursor).alpha(0.55) } else { theme.cursor.into() };
                        frame.cursors.push(fill(bounds, color).corner_radii(px(1.)));
                    }
                }

                if single {
                    if first_seg && self.doc.read(cx).len() == 0 && !self.placeholder.is_empty() {
                        let placeholder = text_system.shape_line(
                            self.placeholder.clone(),
                            font_size,
                            &[TextRun {
                                len: self.placeholder.len(),
                                font: editor_font.clone(),
                                color: theme.text_dim.into(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        frame.lines.push((origin, placeholder));
                    }
                    frame.lines.push((origin, shaped));
                    continue;
                }

                // Gutter: line number and diagnostic dot, on the line's first row.
                if first_seg {
                    let number = SharedString::from((row + 1).to_string());
                    let number_color = if debug_row == Some(row) {
                        theme.warning
                    } else if is_cursor_row {
                        theme.line_number_active
                    } else {
                        theme.line_number
                    };
                    let number_line = text_system.shape_line(
                        number.clone(),
                        font_size,
                        &[TextRun {
                            len: number.len(),
                            font: editor_font.clone(),
                            color: number_color.into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let nx = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT + 18.) - number_line.width;
                    if let Some(breakpoint) = self.doc.read(cx).breakpoint(row) {
                        // A breakpoint covers its line number, as in JetBrains:
                        // a dot, "=" in it with a condition, a diamond for a logpoint.
                        let d = (line_height * 0.62).min(px(13.));
                        let right = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT + 18.);
                        let mark = Bounds::new(point(right - d, y + (line_height - d) / 2.), size(d, d));
                        if breakpoint.log_message.is_some() {
                            let r = d * 0.6;
                            let c = mark.center();
                            let mut diamond = gpui::PathBuilder::fill();
                            diamond.move_to(point(c.x, c.y - r));
                            diamond.line_to(point(c.x + r, c.y));
                            diamond.line_to(point(c.x, c.y + r));
                            diamond.line_to(point(c.x - r, c.y));
                            diamond.close();
                            if let Ok(path) = diamond.build() {
                                frame.gutter_paths.push((path, theme.error.into()));
                            }
                        } else {
                            frame.gutter_marks.push(fill(mark, theme.error).corner_radii(d / 2.));
                            if breakpoint.condition.is_some() {
                                let (w, h) = (d * 0.5, px(1.5));
                                for dy in [-d * 0.13, d * 0.13] {
                                    let bar = Bounds::new(point(mark.center().x - w / 2., mark.center().y + dy - h / 2.), size(w, h));
                                    frame.gutter_marks.push(fill(bar, theme.background));
                                }
                            }
                        }
                    } else {
                        frame.numbers.push((point(nx, y), number_line));
                    }

                    let folded = self.fold_under(row, cx).is_some();
                    let foldable = !folded
                        && self.gutter_hovered
                        && fold_regions.as_ref().is_some_and(|r| r.binary_search_by_key(&row, |region| region.start).is_ok());
                    if folded || foldable {
                        let icon_size = px(FOLD_ICON);
                        let x = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT) - icon_size;
                        let (icon, color) =
                            if folded { (Icon::ChevronRight, theme.text_muted) } else { (Icon::ChevronDown, theme.text_dim) };
                        frame.fold_icons.push((
                            Bounds::new(point(x, y + (line_height - icon_size) / 2.), size(icon_size, icon_size)),
                            icon.path(),
                            color.into(),
                        ));
                    }

                    if let Ok(ix) = run_rows.binary_search_by_key(&row, |(r, _)| *r) {
                        let icon_size = px(RUN_ICON);
                        // A test that ran shows how it went; clicking still runs it.
                        let (icon, color) = match run_rows[ix].1 {
                            Some(crate::test_run::TestStatus::Passed) => (Icon::CircleCheck, theme.success),
                            Some(crate::test_run::TestStatus::Failed) => (Icon::CircleX, theme.error),
                            _ => (Icon::Play, theme.success),
                        };
                        frame.fold_icons.push((
                            Bounds::new(point(bounds.left() + px(5.), y + (line_height - icon_size) / 2.), size(icon_size, icon_size)),
                            icon.path(),
                            color.into(),
                        ));
                    } else if let Some((worst, _)) = worst {
                        let dot = px(6.);
                        frame.gutter_marks.push(
                            fill(
                                Bounds::new(point(bounds.left() + px(10.), y + (line_height - dot) / 2.), size(dot, dot)),
                                severity_color(&theme, worst.severity),
                            )
                            .corner_radii(dot / 2.),
                        );
                    }
                }

                // Git change bars.
                let bar_x = bounds.left() + gutter_width - px(GIT_BAR_OFFSET) - px(1.5);
                // Hunks are sorted and don't overlap: just the ones at this row.
                let first_hunk = self.hunks.partition_point(|h| h.new_rows.end < row);
                for (hix, hunk) in self.hunks.iter().enumerate().skip(first_hunk).take_while(|(_, h)| h.new_rows.start <= row) {
                    let color: Hsla = match hunk.kind {
                        HunkKind::Added => theme.git_added,
                        HunkKind::Modified => theme.git_modified,
                        HunkKind::Deleted => theme.git_deleted,
                    }
                    .into();
                    // Staged changes are drawn faded.
                    let color = if self.hunk_staged.get(hix).copied().unwrap_or(false) { color.alpha(0.4) } else { color };
                    if hunk.kind == HunkKind::Deleted {
                        if hunk.new_rows.start == row && first_seg {
                            frame.gutter_marks.push(
                                fill(Bounds::new(point(bar_x - px(1.5), y - px(3.)), size(px(6.), px(6.))), color)
                                    .corner_radii(px(3.)),
                            );
                        }
                    } else if hunk.new_rows.contains(&row) {
                        let first = row == hunk.new_rows.start && first_seg;
                        let last = row + 1 == hunk.new_rows.end && last_seg;
                        let top = if first { y + px(1.) } else { y };
                        let bottom = if last { y + line_height - px(1.) } else { y + line_height };
                        frame.gutter_marks.push(
                            fill(Bounds::from_corners(point(bar_x, top), point(bar_x + px(3.), bottom)), color)
                                .corner_radii(gpui::Corners {
                                    top_left: if first { px(1.5) } else { px(0.) },
                                    top_right: if first { px(1.5) } else { px(0.) },
                                    bottom_left: if last { px(1.5) } else { px(0.) },
                                    bottom_right: if last { px(1.5) } else { px(0.) },
                                }),
                        );
                    }
                }

                // Inline diagnostic or blame after the line's text, on its last row.
                if !last_seg {
                    frame.lines.push((origin, shaped));
                    continue;
                }
                // A folded header ends with a `⋯` placeholder (click to unfold).
                let mut text_end = origin.x + shaped.width;
                if let Some(c) = conflicts.iter().find(|c| c.start == row) {
                    // Accept Current · Accept Incoming · Accept Both
                    let mut x = text_end + char_width * 3.;
                    for resolution in [Resolution::Current, Resolution::Incoming, Resolution::Both] {
                        let label = resolution.label();
                        let line = text_system.shape_line(
                            SharedString::from(label),
                            px(theme.ui_font_size - 0.5),
                            &[TextRun {
                                len: label.len(),
                                font: ui_font.clone(),
                                color: theme.text_muted.into(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        let pill = Bounds::new(point(x - px(5.), y + px(2.)), size(line.width + px(10.), line_height - px(4.)));
                        frame.inline_bgs.push(fill(pill, theme.hover).corner_radii(px(4.)));
                        x += line.width + px(16.);
                        frame.inline_texts.push((point(pill.left() + px(5.), y), line));
                        self.conflict_hits.push((pill, c.start, resolution));
                        // A diagnostic or blame follows the labels.
                        text_end = pill.right();
                    }
                } else if let Some(fold) = self.fold_under(row, cx) {
                    let label = self.fold_placeholder(&fold, cx);
                    let pill_line = text_system.shape_line(
                        SharedString::from(label.clone()),
                        font_size,
                        &[TextRun {
                            len: label.len(),
                            font: editor_font.clone(),
                            color: theme.text_muted.into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let x = text_end + char_width * 0.6;
                    let pill = Bounds::new(point(x - px(4.), y + px(2.)), size(pill_line.width + px(8.), line_height - px(4.)));
                    frame.inline_bgs.push(fill(pill, theme.active).corner_radii(px(4.)));
                    frame.inline_texts.push((point(x, y), pill_line));
                    self.fold_pills.push((pill, fold.start));
                    text_end = pill.right();
                }
                if let Some((worst, count)) = worst.filter(|_| problems_row != Some(row)) {
                    let color = severity_color(&theme, worst.severity);
                    // Multi-line messages (rust-analyzer's) fit on one line here;
                    // hovering shows them in full.
                    let mut message =
                        worst.message.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
                    if message.len() > 160 {
                        message.truncate(message.floor_char_boundary(160));
                        message.push('…');
                    }
                    if count > 1 {
                        message.push_str(&format!("  (+{})", count - 1));
                    }
                    let msg_size = px(theme.ui_font_size - 0.5);
                    let msg_line = text_system.shape_line(
                        SharedString::from(message.clone()),
                        msg_size,
                        &[TextRun {
                            len: message.len(),
                            font: ui_font.clone(),
                            color: color.alpha(0.9),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let x = text_end + char_width * 3.;
                    let pad = px(8.);
                    let pill_h = line_height - px(4.);
                    frame.inline_bgs.push(
                        fill(
                            Bounds::new(point(x - pad, y + px(2.)), size(msg_line.width + pad * 2., pill_h)),
                            color.alpha(0.1),
                        )
                        .corner_radii(px(4.)),
                    );
                    frame.inline_texts.push((point(x, y), msg_line));
                } else if focused
                    && self.selections.len() == 1
                    && is_cursor_row
                    && let Some((blame_row, _, blame)) = self.blame.as_ref()
                    && *blame_row == row
                {
                    // Inline blame for the cursor line (only when no diagnostic is shown).
                    let who = match &self.git_file {
                        _ if blame.uncommitted => "You".to_string(),
                        Some(g) if g.user_name.as_deref() == Some(blame.author.as_str()) => "You".to_string(),
                        _ => blame.author.clone(),
                    };
                    let text = if blame.uncommitted {
                        "You • Uncommitted changes".to_string()
                    } else {
                        let mut summary = blame.summary.clone();
                        if summary.len() > 80 {
                            summary.truncate(summary.floor_char_boundary(80));
                            summary.push('…');
                        }
                        format!("{who}, {} • {summary}", git::relative_time(blame.time))
                    };
                    let line = text_system.shape_line(
                        SharedString::from(text.clone()),
                        px(theme.ui_font_size - 0.5),
                        &[TextRun {
                            len: text.len(),
                            font: ui_font.clone(),
                            color: Hsla::from(theme.text_dim).alpha(0.9),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    frame.inline_texts.push((point(text_end + char_width * 4., y), line));
                }

                frame.lines.push((origin, shaped));
            }
        }

        if !single {
            self.request_blame(window, cx);
            let number_right = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT + 18.);
            self.layout_sticky(&mut frame, bounds, first_row, text_left, number_right, window, cx);
        }

        // Scrollbar, or the minimap, with diagnostic and search markers.
        let minimap = self.minimap_geometry(cx);
        let scrollbar = if minimap.is_some() { None } else { self.scrollbar_geometry(cx).filter(|_| !single) };
        if minimap.is_some() || scrollbar.is_some() {
            let active = self.scrollbar_drag.is_some() || self.scrollbar_hovered;
            let (fade, wake) = self.scrollbar_fade.step(self.scroll.y, active, theme.reduce_effects);
            match wake {
                crate::ui::FadeWake::Frame => window.request_animation_frame(),
                crate::ui::FadeWake::After(delay) => cx
                    .spawn(async move |this, cx| {
                        cx.background_executor().timer(delay).await;
                        this.update(cx, |_, cx| cx.notify()).ok();
                    })
                    .detach(),
                crate::ui::FadeWake::None => {}
            }
            let track = match (&minimap, scrollbar) {
                (Some(g), _) => {
                    frame.scrollbar_track = Some(g.bounds);
                    self.layout_minimap(&mut frame, g, fade, active, cx)
                }
                (None, Some((track, thumb))) => {
                    if fade > 0. {
                        let alpha = if active { 0.28 } else { 0.16 } * fade;
                        frame.scrollbar.push(fill(thumb, Hsla::from(theme.text).alpha(alpha)).corner_radii(px(3.)));
                    }
                    frame.scrollbar_track = Some(track);
                    track
                }
                (None, None) => unreachable!(),
            };
            // Problems, changes, matches and carets: laid out when they or
            // the rows change, not per frame.
            self.layout_markers(&mut frame, track, cx);
        }

        // The ⌘/Ctrl-hover link's word, for the pointing hand over it.
        if let Some(link) = self.lsp.link.clone() {
            let y = top + line_height * self.display_row_of(link.start, cx) as f32 - self.scroll.y;
            let left = text_left - self.scroll.x;
            let (start, end) = (left + self.x_for_offset(link.start, window, cx), left + self.x_for_offset(link.end, window, cx));
            frame.link_bounds = Some(Bounds::from_corners(point(start, y), point(end, y + line_height)).intersect(&frame.text_bounds));
        }

        // Only the focused editor blinks (see `restart_blink`).
        if self.blink_task.is_none() && focused {
            self.restart_blink(window, cx);
        }
        frame
    }
}

fn severity_color(theme: &Theme, severity: Severity) -> Hsla {
    match severity {
        Severity::Error => theme.error,
        Severity::Warning => theme.warning,
        Severity::Info => theme.info,
        Severity::Hint => theme.text_dim,
    }
    .into()
}

fn closing_pair(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        _ => None,
    }
}

/// The indentation a file already uses, if it has indented lines.
fn detect_indent(doc: &Document) -> Option<String> {
    let (mut tabs, mut spaces2, mut spaces4) = (0, 0, 0);
    for row in 0..doc.line_count().min(500) {
        let t = doc.line_text(row);
        if t.starts_with('\t') {
            tabs += 1;
        } else {
            let n = t.len() - t.trim_start_matches(' ').len();
            if n > 0 && !t.trim().is_empty() {
                if n.is_multiple_of(4) {
                    spaces4 += 1;
                } else if n.is_multiple_of(2) {
                    spaces2 += 1;
                }
            }
        }
    }
    if tabs + spaces2 + spaces4 == 0 {
        None
    } else if tabs > spaces2 + spaces4 {
        Some("\t".into())
    } else if spaces2 > spaces4 / 2 && spaces2 > 0 {
        Some("  ".into())
    } else {
        Some("    ".into())
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharKind {
    Word,
    Space,
    Punct,
    Newline,
}

fn char_kind(c: char) -> CharKind {
    if c == '\n' {
        CharKind::Newline
    } else if c.is_whitespace() {
        CharKind::Space
    } else if c.is_alphanumeric() || c == '_' || c == '$' {
        CharKind::Word
    } else {
        CharKind::Punct
    }
}

fn word_left(doc: &Document, mut o: usize) -> usize {
    while let Some(c) = doc.char_before(o) {
        if char_kind(c) == CharKind::Space { o -= c.len_utf8() } else { break }
    }
    if let Some(c) = doc.char_before(o) {
        if c == '\n' {
            return o - 1;
        }
        let k = char_kind(c);
        while let Some(c) = doc.char_before(o) {
            if char_kind(c) == k { o -= c.len_utf8() } else { break }
        }
    }
    o
}

fn word_right(doc: &Document, mut o: usize) -> usize {
    while let Some(c) = doc.char_at(o) {
        if char_kind(c) == CharKind::Space { o += c.len_utf8() } else { break }
    }
    if let Some(c) = doc.char_at(o) {
        if c == '\n' {
            return o + 1;
        }
        let k = char_kind(c);
        while let Some(c) = doc.char_at(o) {
            if char_kind(c) == k { o += c.len_utf8() } else { break }
        }
    }
    o
}

/// Where `needle` first occurs in `doc` starting from `from` up to (not
/// from) `to`, matching case; with `whole_words`, only where no word
/// character goes on past either end of it. Reads the rope chunk by chunk
/// rather than copying the text.
fn find_occurrence(doc: &Document, needle: &str, from: usize, to: usize, whole_words: bool) -> Option<usize> {
    let pattern = needle.as_bytes();
    if pattern.is_empty() || from >= to || from >= doc.len() {
        return None;
    }
    let is_word = |c: Option<char>| c.is_some_and(|c| char_kind(c) == CharKind::Word);
    let (word_start, word_end) = (is_word(needle.chars().next()), is_word(needle.chars().next_back()));
    let whole = |start: usize| {
        let joined_before = word_start && is_word(doc.char_before(start));
        let joined_after = word_end && is_word(doc.char_at(start + pattern.len()));
        !whole_words || !(joined_before || joined_after)
    };
    // The bytes from `window_start` on: what's left of the chunks before
    // (shorter than a match) and the current chunk.
    let (chunks, mut chunk_start, _, _) = doc.rope().chunks_at_byte(from);
    let (mut window, mut window_start) = (Vec::new(), from);
    for chunk in chunks {
        let bytes = chunk.as_bytes();
        window.extend_from_slice(&bytes[from.saturating_sub(chunk_start).min(bytes.len())..]);
        chunk_start += bytes.len();
        let mut at = 0;
        while let Some(i) = window[at..].windows(pattern.len()).position(|w| w == pattern) {
            let start = window_start + at + i;
            if start >= to {
                return None;
            }
            if whole(start) {
                return Some(start);
            }
            at += i + 1;
        }
        let keep = window.len().min(pattern.len() - 1);
        window_start += window.len() - keep;
        window.drain(..window.len() - keep);
        if window_start >= to {
            return None;
        }
    }
    None
}

/// Where Next (`forward`) or Previous Problem goes from `head`: the nearest
/// problem start after (before) it, else around the file, the first (last).
/// `starts` is sorted.
fn problem_from(starts: &[usize], head: usize, forward: bool) -> Option<usize> {
    if forward {
        starts.iter().copied().find(|&s| s > head).or(starts.first().copied())
    } else {
        starts.iter().rev().copied().find(|&s| s < head).or(starts.last().copied())
    }
}

fn word_range(doc: &Document, offset: usize) -> Range<usize> {
    let kind = match (doc.char_at(offset), doc.char_before(offset)) {
        (Some(c), _) if char_kind(c) == CharKind::Word => CharKind::Word,
        (_, Some(c)) if char_kind(c) == CharKind::Word => CharKind::Word,
        (Some(c), _) if c != '\n' => char_kind(c),
        _ => return offset..offset,
    };
    let mut start = offset;
    while let Some(c) = doc.char_before(start) {
        if char_kind(c) == kind { start -= c.len_utf8() } else { break }
    }
    let mut end = offset;
    while let Some(c) = doc.char_at(end) {
        if char_kind(c) == kind { end += c.len_utf8() } else { break }
    }
    start..end
}

/// One display row of a line: its slice of the display text, shaped.
struct RowShape {
    line: ShapedLine,
    dl: DisplayLine,
    range: Range<usize>,
    /// Wrap indent before the text.
    indent: Pixels,
    /// Whether this is the line's last row.
    last: bool,
}

/// A line's breaks around the rows laid out (a long line has thousands):
/// `wrap::segment` and `wrap::segment_of` for those rows.
struct Segments {
    /// The line's breaks from `first` on, as far as the rows need.
    breaks: Vec<Break>,
    first: usize,
    /// How many breaks the line has.
    count: usize,
}

impl Segments {
    fn new(all: &[Break], segs: std::ops::RangeInclusive<usize>) -> Self {
        let first = segs.start().saturating_sub(1).min(all.len());
        let end = (segs.end() + 1).clamp(first, all.len());
        Segments { breaks: all[first..end].to_vec(), first, count: all.len() }
    }

    /// The display range and wrap indent of segment `seg` (one of the rows
    /// given), the display text ending at `len`.
    fn range(&self, seg: usize, len: usize) -> (Range<usize>, u32) {
        let before = (seg > 0).then(|| self.breaks[seg - 1 - self.first]);
        let start = before.map_or(0, |b| b.ix as usize);
        let end = if seg >= self.count { len } else { self.breaks[seg - self.first].ix as usize };
        (start.min(len)..end.min(len).max(start.min(len)), before.map_or(0, |b| b.indent))
    }

    /// The segment that shows display offset `col`: exact for the rows given;
    /// one before or after them says it's not on them.
    fn of(&self, col: usize) -> usize {
        self.first + self.breaks.partition_point(|b| b.ix as usize <= col)
    }
}

/// The part of `runs` (covering a whole line) that falls in `range`.
fn slice_runs(runs: &[TextRun], range: Range<usize>) -> Vec<TextRun> {
    let mut out = Vec::new();
    let mut pos = 0;
    for run in runs {
        let (start, end) = (pos.max(range.start), (pos + run.len).min(range.end));
        pos += run.len;
        if start < end {
            out.push(TextRun { len: end - start, ..run.clone() });
        }
    }
    out
}

/// Runs for a line with inlay hints (covering `dl.text`): the hints' text in
/// `color`, plain, and a run for hints after the last character (the text's
/// runs end there).
fn inlay_runs(runs: Vec<TextRun>, dl: &DisplayLine, font: Font, color: Hsla) -> Vec<TextRun> {
    let mut runs = runs;
    let total: usize = runs.iter().map(|r| r.len).sum();
    if total < dl.text.len() {
        runs.push(TextRun { len: dl.text.len() - total, font, color, background_color: None, underline: None, strikethrough: None });
    }
    let hints: Vec<Range<usize>> = dl.inlays.iter().map(|s| dl.local(s.whole.clone())).collect();
    restyle_runs(runs, &hints, |piece| {
        piece.color = color;
        piece.underline = None;
        piece.background_color = None;
    })
}

/// `runs` cut where `ranges` (of the display text; sorted and apart, as
/// hints and symbols are) start and end, with `restyle` applied to the
/// pieces inside them. One walk over both: a long line's rows on screen can
/// hold thousands of each.
fn restyle_runs(runs: Vec<TextRun>, ranges: &[Range<usize>], restyle: impl Fn(&mut TextRun)) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2 * ranges.len());
    let mut pos = 0;
    // The first range that doesn't end at or before where the walk is.
    let mut next = 0;
    for run in runs {
        let end = pos + run.len;
        let mut start = pos;
        while start < end {
            while ranges.get(next).is_some_and(|r| r.end <= start) {
                next += 1;
            }
            let (cut, inside) = match ranges.get(next) {
                Some(r) if r.start <= start => (r.end.min(end), true),
                Some(r) => (r.start.min(end), false),
                None => (end, false),
            };
            let mut piece = TextRun { len: cut - start, ..run.clone() };
            if inside {
                restyle(&mut piece);
            }
            out.push(piece);
            start = cut;
        }
        pos = end;
    }
    out
}

/// A buffer line prepared for display: tabs are expanded to spaces and
/// control characters shown as symbols (`control_symbol`), with a mapping
/// between buffer columns and display columns (both in bytes). It can be a
/// window of the line (see `window`): positions stay the whole line's.
pub struct DisplayLine {
    /// The display text from display offset `start.ix` on.
    pub text: String,
    /// Where `text` starts: buffer column, display offset, tab column (zero
    /// for a whole line).
    start: wrap::Resume,
    /// Display offsets of buffer columns `start.byte..`; None when they're
    /// the same (no tabs, control characters or hints).
    map: Option<Vec<usize>>,
    /// Inlay hints shown in `text`: the inserted text (padding included), the
    /// label within it, and the buffer column they sit before.
    pub inlays: Vec<InlaySpan>,
    /// Where `text` shows control characters' symbols (dim, on a chip).
    pub controls: Vec<Range<usize>>,
}

/// How a control character shows, as it would otherwise draw as nothing (or
/// as a box): C0 controls and DEL as their Control Pictures (a form feed is
/// ␌), C1 controls and the Unicode line and paragraph separators by name.
/// A tab isn't one: it shows as spaces.
pub fn control_symbol(c: char) -> Option<&'static str> {
    const C0: [&str; 32] = [
        "␀", "␁", "␂", "␃", "␄", "␅", "␆", "␇", "␈", "␉", "␊", "␋", "␌", "␍", "␎", "␏", "␐", "␑", "␒", "␓", "␔", "␕", "␖", "␗",
        "␘", "␙", "␚", "␛", "␜", "␝", "␞", "␟",
    ];
    const C1: [&str; 32] = [
        "PAD", "HOP", "BPH", "NBH", "IND", "NEL", "SSA", "ESA", "HTS", "HTJ", "VTS", "PLD", "PLU", "RI", "SS2", "SS3", "DCS", "PU1",
        "PU2", "STS", "CCH", "MW", "SPA", "EPA", "SOS", "SGC", "SCI", "CSI", "ST", "OSC", "PM", "APC",
    ];
    match c {
        '\t' => None,
        '\0'..='\u{1f}' => Some(C0[c as usize]),
        '\u{7f}' => Some("␡"),
        '\u{80}'..='\u{9f}' => Some(C1[c as usize - 0x80]),
        '\u{2028}' => Some("LS"),
        '\u{2029}' => Some("PS"),
        _ => None,
    }
}

/// Whether `line` has a tab or a control character, which show as something
/// else (most lines have neither).
fn has_tabs_or_controls(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| match b {
        0..=0x1f | 0x7f => true,
        // C1 controls are U+0080 to U+009F; LS and PS are E2 80 A8 / A9.
        0xc2 => bytes.get(i + 1).is_some_and(|next| (0x80..=0x9f).contains(next)),
        0xe2 => bytes.get(i + 1) == Some(&0x80) && matches!(bytes.get(i + 2), Some(0xa8 | 0xa9)),
        _ => false,
    })
}

pub struct InlaySpan {
    pub whole: Range<usize>,
    pub label: Range<usize>,
    pub column: usize,
}

impl DisplayLine {
    pub fn new(line: &str, tab_size: usize) -> Self {
        Self::with_inlays(line, tab_size, &[])
    }

    /// With inlay hints (buffer column, label, pad left, pad right), sorted
    /// by column. A buffer column maps to the display column before its
    /// hints, so carets sit left of them.
    pub fn with_inlays(line: &str, tab_size: usize, inlays: &[(usize, &str, bool, bool)]) -> Self {
        Self::window(line, wrap::Resume::default(), tab_size, inlays)
    }

    /// Part of a line: `text` is its buffer text from `from` (see
    /// `wrap::Resume`) on, `inlays` its hints there (columns the line's, up
    /// to the end of `text` included). Positions in and out are the whole
    /// line's; ones outside the part are clamped to it.
    pub fn window(text: &str, from: wrap::Resume, tab_size: usize, inlays: &[(usize, &str, bool, bool)]) -> Self {
        let first = from.byte as usize;
        let inlays = &inlays[inlays.partition_point(|i| i.0 < first)..];
        if inlays.is_empty() && !has_tabs_or_controls(text) {
            return Self { text: text.to_string(), start: from, map: None, inlays: Vec::new(), controls: Vec::new() };
        }
        let base = from.ix as usize;
        let mut out = String::with_capacity(text.len() + 16);
        let mut map = Vec::with_capacity(text.len() + 1);
        let mut spans = Vec::new();
        let mut controls = Vec::new();
        let mut next = 0;
        let mut insert = |at: usize, out: &mut String| {
            while let Some(&(column, label, pad_left, pad_right)) = inlays.get(next).filter(|i| i.0 <= at) {
                let start = base + out.len();
                out.push_str(if pad_left { " " } else { "" });
                let label_start = base + out.len();
                out.push_str(label);
                let label_end = base + out.len();
                out.push_str(if pad_right { " " } else { "" });
                spans.push(InlaySpan { whole: start..base + out.len(), label: label_start..label_end, column });
                next += 1;
            }
        };
        let mut col = from.col as usize;
        for (i, ch) in text.char_indices() {
            let before = base + out.len();
            insert(first + i, &mut out);
            for _ in 0..ch.len_utf8() {
                map.push(before);
            }
            if ch == '\t' {
                let n = tab_size - col % tab_size;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            } else if let Some(symbol) = control_symbol(ch) {
                controls.push(base + out.len()..base + out.len() + symbol.len());
                out.push_str(symbol);
                col += symbol.chars().count();
            } else {
                out.push(ch);
                col += 1;
            }
        }
        let before = base + out.len();
        insert(first + text.len(), &mut out);
        map.push(before);
        Self { text: out, start: from, map: Some(map), inlays: spans, controls }
    }

    /// The buffer columns it shows: the line's, or a window's.
    pub fn columns(&self) -> Range<usize> {
        let first = self.start.byte as usize;
        first..first + self.map.as_ref().map_or(self.text.len(), |m| m.len() - 1)
    }

    /// Where the text ends (the display offset after it).
    pub fn end(&self) -> usize {
        self.start.ix as usize + self.text.len()
    }

    /// The display text in `range` (display offsets of the line).
    pub fn slice(&self, range: Range<usize>) -> &str {
        let range = self.local(range);
        &self.text[range]
    }

    /// `range` (display offsets of the line) as offsets into `text`.
    pub fn local(&self, range: Range<usize>) -> Range<usize> {
        let base = self.start.ix as usize;
        let start = range.start.saturating_sub(base).min(self.text.len());
        start..range.end.saturating_sub(base).clamp(start, self.text.len())
    }

    pub fn to_display(&self, col: usize) -> usize {
        let first = self.start.byte as usize;
        match &self.map {
            None => self.start.ix as usize + col.saturating_sub(first).min(self.text.len()),
            Some(m) => m[col.saturating_sub(first).min(m.len() - 1)],
        }
    }

    pub fn to_buffer(&self, display: usize) -> usize {
        // On a hint: the column it sits before.
        if let Some(span) = self.inlays.iter().find(|s| s.whole.start < display && display <= s.whole.end) {
            return span.column;
        }
        let first = self.start.byte as usize;
        match &self.map {
            None => first + display.saturating_sub(self.start.ix as usize).min(self.text.len()),
            Some(m) => first + m.partition_point(|&d| d < display).min(m.len() - 1),
        }
    }
}

// ---- Git ------------------------------------------------------------------------

impl Editor {
    /// The change marker under a gutter position (display rows as a fraction, so
    /// deletion markers on a row boundary can be hit).
    fn hunk_at(&self, row: f32, cx: &App) -> Option<usize> {
        let deleted = self.hunks.iter().position(|h| {
            h.kind == HunkKind::Deleted && (row - self.first_display_row(h.new_rows.start) as f32).abs() < 0.35
        });
        deleted.or_else(|| {
            let r = self.line_of_display_row(row.floor().max(0.) as usize, cx).0;
            self.hunks.iter().position(|h| h.kind != HunkKind::Deleted && h.new_rows.contains(&r))
        })
    }

    /// The change at the cursor, preferring an open popover.
    fn current_hunk(&self, cx: &App) -> Option<usize> {
        self.open_hunk.or_else(|| {
            let row = self.doc.read(cx).row_of(self.primary_selection().head);
            self.hunks.iter().position(|h| {
                h.new_rows.contains(&row) || (h.kind == HunkKind::Deleted && (h.new_rows.start == row || h.new_rows.start == row + 1))
            })
        })
    }

    fn rollback_hunk(&mut self, _: &RollbackHunk, _: &mut Window, cx: &mut Context<Self>) {
        let Some(hunk) = self.current_hunk(cx).and_then(|i| self.hunks.get(i)).cloned() else {
            return;
        };
        let line_count = self.doc.read(cx).line_count();
        let start = self.doc.read(cx).line_start(hunk.new_rows.start.min(line_count));
        let end = if hunk.new_rows.end >= line_count { self.doc.read(cx).len() } else { self.doc.read(cx).line_start(hunk.new_rows.end) };
        let mut text = hunk.old_text.clone();
        // Keep the file's final-newline state when the hunk reaches the end.
        if end == self.doc.read(cx).len() && !self.doc.read(cx).rope().to_string().ends_with('\n') && text.ends_with('\n') {
            text.pop();
        }
        self.open_hunk = None;
        self.edit_and_remap(vec![(start..end, text)], EditKind::Other, cx);
    }

    fn stage_hunk(&mut self, _: &StageHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.stage_current_hunk(true, cx);
    }

    fn unstage_hunk(&mut self, _: &UnstageHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.stage_current_hunk(false, cx);
    }

    /// Stages (or unstages) the change at the caret, as the buffer shows it,
    /// unsaved edits included.
    fn stage_current_hunk(&mut self, stage: bool, cx: &mut Context<Self>) {
        let (Some(hunk), Some(git_file)) = (self.current_hunk(cx).and_then(|i| self.hunks.get(i)).cloned(), self.git_file.clone()) else {
            cx.emit(EditorLspEvent::Message("No change at the caret".into()));
            return;
        };
        let (rel, text, rows) = (git_file.rel, self.text(cx), hunk.new_rows);
        let op = if stage {
            crate::git_panel::GitOp::StageLines { rel, text, rows }
        } else {
            crate::git_panel::GitOp::UnstageLines { rel, text: Some(text), rows }
        };
        cx.emit(EditorGitEvent(op));
    }

    fn jump_to_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.hunks.is_empty() {
            return;
        }
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        let starts: Vec<usize> = self.hunks.iter().map(|h| h.new_rows.start).collect();
        let target = if forward {
            starts.iter().copied().find(|&s| s > row).unwrap_or(starts[0])
        } else {
            starts.iter().rev().copied().find(|&s| s < row).unwrap_or(*starts.last().unwrap())
        };
        let offset = self.doc.read(cx).line_start(target.min(self.doc.read(cx).line_count() - 1));
        let ix = self.hunks.iter().position(|h| h.new_rows.start == target);
        if self.open_hunk.is_some() {
            self.open_hunk = ix;
        }
        self.select_range(offset..offset, cx);
    }

    fn next_change(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_change(true, cx);
    }

    fn previous_change(&mut self, _: &PreviousChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_change(false, cx);
    }

    /// Next / Previous Problem: the caret goes to where the next (previous)
    /// problem in the file starts, built-in lint's and language servers'
    /// alike, around the end; its message shows on that line.
    fn jump_to_problem(&mut self, forward: bool, cx: &mut Context<Self>) {
        let doc = self.doc.read(cx);
        let starts: Vec<usize> = doc.diagnostics.iter().filter(|d| d.severity.is_problem()).map(|d| d.range.start).collect();
        match problem_from(&starts, self.primary_selection().head, forward) {
            Some(at) => self.select_range(at..at, cx),
            None => cx.emit(EditorLspEvent::Message("No problems in this file".into())),
        }
    }

    fn next_problem(&mut self, _: &NextProblem, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_problem(true, cx);
    }

    fn previous_problem(&mut self, _: &PreviousProblem, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_problem(false, cx);
    }

    fn toggle_inline_blame(&mut self, _: &ToggleInlineBlame, _: &mut Window, cx: &mut Context<Self>) {
        let on = cx.try_global::<InlineBlame>().is_none_or(|b| b.0);
        cx.set_global(InlineBlame(!on));
        cx.refresh_windows();
    }

    /// Shows blame for the caret's line, from the whole file's blame: read
    /// once per file and HEAD in the background (`blame_file`), its rows
    /// followed through edits since.
    fn request_blame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let enabled = cx.try_global::<InlineBlame>().is_none_or(|b| b.0);
        let Some(git_file) = self.git_file.clone().filter(|_| enabled) else {
            return;
        };
        if !self.focus_handle.is_focused(window) || self.selections.len() != 1 || !self.has_diff_base {
            return;
        }
        let doc = self.doc.read(cx);
        let (row, version, id) = (doc.row_of(self.primary_selection().head), doc.version(), doc.id());
        if self.blame_request == Some((row, version)) {
            return;
        }
        self.blame_request = Some((row, version));
        match self.file_blame.as_ref().filter(|b| b.git_file == git_file && b.doc == id).and_then(|b| b.line(row, doc)) {
            Some(line) => self.blame = line.map(|line| (row, version, line)),
            None => {
                self.blame = None;
                self.blame_file(git_file, cx);
            }
        }
    }

    /// Reads the whole file's blame (as it is now, unsaved edits too) in
    /// the background, unless that's under way; one for another file, HEAD
    /// or document stops (git is killed).
    fn blame_file(&mut self, git_file: GitFile, cx: &mut Context<Self>) {
        let doc = self.doc.read(cx);
        let key = (git_file.clone(), doc.id());
        if self.blame_task.as_ref().is_some_and(|(running, ..)| *running == key) {
            return;
        }
        let (id, version, text) = (doc.id(), doc.version(), doc.rope().clone());
        let git = git::KillOnDrop::default();
        let stop = git.token();
        let task = cx.spawn(async move |this, cx| {
            let (root, rel, contents) = (git_file.root.clone(), git_file.rel.clone(), text.clone());
            let lines = cx
                .background_executor()
                .spawn(async move { git::blame_file(&root, &rel, contents.to_string().as_bytes(), &stop) })
                .await;
            this.update(cx, |editor, cx| {
                // This task is running; let it finish instead of dropping it.
                if let Some((_, task, _)) = editor.blame_task.take() {
                    task.detach();
                }
                // Failed (not committed yet, say): nothing to show, and
                // nothing asked again until HEAD moves.
                let lines = lines.unwrap_or_default();
                editor.file_blame = Some(Blame { git_file, doc: id, version, text, lines });
                // Shown for the caret's line on the next frame.
                editor.blame_request = None;
                cx.notify();
            })
            .ok();
        });
        self.blame_task = Some((key, task, git));
    }

    fn render_hunk_popover(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let ix = self.open_hunk?;
        let hunk = self.hunks.get(ix)?.clone();
        let layout = self.layout.as_ref()?;
        let theme = Theme::get(cx).clone();
        // Just above the hunk: its deleted lines (block rows, see `block_rows`)
        // or the lines it added, at the right so it covers little code.
        let deleted = hunk.old_text.lines().count();
        let first_row = self.first_display_row(hunk.new_rows.start).saturating_sub(deleted);
        let top = (layout.line_height * first_row as f32 - self.scroll.y - px(36.)).max(px(4.));
        let staged = self.hunk_staged.get(ix).copied().unwrap_or(false);
        let (title, color): (String, Hsla) = match hunk.kind {
            HunkKind::Added => (format!("{} added line{}", hunk.new_rows.len(), plural(hunk.new_rows.len())), theme.git_added.into()),
            HunkKind::Modified => (format!("{} changed line{}", hunk.new_rows.len(), plural(hunk.new_rows.len())), theme.git_modified.into()),
            HunkKind::Deleted => (format!("{} deleted line{}", hunk.old_rows.len(), plural(hunk.old_rows.len())), theme.git_deleted.into()),
        };
        let button = |id: &'static str, icon: Icon, tip: &'static str| {
            div()
                .id(id)
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| crate::ui::tooltip_view(tip, cx))
                .child(crate::ui::icon_el(icon, px(14.), theme.text_muted.into()))
        };
        Some(
            div()
                .id("hunk-popover")
                .absolute()
                .top(top)
                .right(px(28.))
                .flex()
                .flex_col()
                .bg(theme.elevated)
                .border_1()
                .border_color(Hsla::from(theme.border).alpha(0.85))
                .rounded(px(10.))
                .shadow(crate::ui::soft_shadow(&theme, 2))
                .overflow_hidden()
                .text_size(px(12.5))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(32.))
                        .px_2()
                        .border_b_1()
                        .border_color(theme.border_subtle)
                        .child(div().size(px(8.)).rounded_full().bg(color))
                        .child(div().pl_1().text_color(theme.text_muted).child(title))
                        .when(staged, |el| el.child(div().text_size(px(11.)).text_color(theme.text_dim).child("· Staged")))
                        .child(div().flex_1())
                        .child(if staged {
                            button("hunk-stage", Icon::Minus, "Unstage Lines").on_click(cx.listener(|e, _, _, cx| e.stage_current_hunk(false, cx)))
                        } else {
                            button("hunk-stage", Icon::Plus, "Stage Lines").on_click(cx.listener(|e, _, _, cx| e.stage_current_hunk(true, cx)))
                        })
                        .child(button("hunk-rollback", Icon::Undo, "Rollback Lines").on_click(cx.listener(|e, _, w, cx| e.rollback_hunk(&RollbackHunk, w, cx))))
                        .child(button("hunk-prev", Icon::ChevronUp, "Previous Change").on_click(cx.listener(|e, _, _, cx| e.jump_to_change(false, cx))))
                        .child(button("hunk-next", Icon::ChevronDown, "Next Change").on_click(cx.listener(|e, _, _, cx| e.jump_to_change(true, cx))))
                        .child(button("hunk-close", Icon::X, "Close (Esc)").on_click(cx.listener(|e, _, _, cx| {
                            e.open_hunk = None;
                            cx.notify();
                        }))),
                )
                .into_any_element(),
        )
    }
}

/// `text` in lines of at most `width` characters, broken between words.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ---- JetBrains-style editing actions ----------------------------------------

impl Editor {
    fn delete_line(&mut self, _: &DeleteLine, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            self.set_text("", cx);
            return;
        }
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            let mut start = self.doc.read(cx).line_start(block.start);
            let mut end = self.doc.read(cx).line_start(block.end);
            if block.end >= self.doc.read(cx).line_count() {
                // Last line: remove the newline before it instead.
                end = self.doc.read(cx).len();
                start = start.saturating_sub(1);
            }
            edits.push((start..end, String::new()));
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn extend_selection(&mut self, _: &ExtendSelection, _: &mut Window, cx: &mut Context<Self>) {
        let before = self.selections.clone();
        let len = self.doc.read(cx).len();
        let expanded: Vec<Selection> = before
            .iter()
            .map(|s| {
                let r = s.range();
                if r.is_empty() {
                    let w = word_range(self.doc.read(cx), r.start);
                    if !w.is_empty() {
                        return Selection { anchor: w.start, head: w.end };
                    }
                }
                let next = self.doc.read(cx).syntax().expand(r.clone()).unwrap_or(0..len);
                Selection { anchor: next.start, head: next.end }
            })
            .collect();
        if expanded == before {
            return;
        }
        self.expand_stack.push(before);
        self.expanding = true;
        self.set_selections(expanded, self.primary, cx);
    }

    fn shrink_selection(&mut self, _: &ShrinkSelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.expand_stack.pop() {
            self.expanding = true;
            let primary = self.primary.min(previous.len() - 1);
            self.set_selections(previous, primary, cx);
        }
    }

    fn unselect_occurrence(&mut self, _: &UnselectOccurrence, _: &mut Window, cx: &mut Context<Self>) {
        if self.selections.len() < 2 {
            return;
        }
        let whole_words = self.occurrence_words.as_ref() == Some(&self.selections);
        let mut selections = self.selections.clone();
        selections.remove(self.primary);
        let primary = self.primary.saturating_sub(1).min(selections.len() - 1);
        self.set_selections(selections, primary, cx);
        if whole_words {
            self.occurrence_words = Some(self.selections.clone());
        }
    }

    fn select_all_occurrences(&mut self, _: &SelectAllOccurrences, _: &mut Window, cx: &mut Context<Self>) {
        self.select_occurrences(cx);
    }

    fn select_occurrences(&mut self, cx: &mut Context<Self>) {
        let p = self.primary_selection();
        // From a caret, the word there wherever it's a whole word (as Add
        // Selection for Next Occurrence goes on from one).
        let whole_words = p.is_empty() || self.occurrence_words.as_ref() == Some(&self.selections);
        let doc = self.doc.read(cx);
        let range = if p.is_empty() { word_range(doc, p.head) } else { p.range() };
        if range.is_empty() {
            return;
        }
        let needle = doc.slice(range.clone());
        let mut selections = Vec::new();
        let mut from = 0;
        while let Some(start) = find_occurrence(doc, &needle, from, doc.len(), whole_words) {
            selections.push(Selection { anchor: start, head: start + needle.len() });
            from = start + needle.len();
        }
        if selections.is_empty() {
            return;
        }
        let primary = selections.iter().position(|s| s.anchor == range.start).unwrap_or(0);
        self.set_selections(selections, primary, cx);
        if whole_words {
            self.occurrence_words = Some(self.selections.clone());
        }
    }

    fn toggle_case(&mut self, _: &ToggleCase, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let range = if s.is_empty() { word_range(doc, s.head) } else { s.range() };
            let text = doc.slice(range.clone());
            let new = if text.chars().any(char::is_lowercase) {
                text.to_uppercase()
            } else {
                text.to_lowercase()
            };
            (range, new, None)
        });
    }

    fn join_lines(&mut self, _: &JoinLines, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            return;
        }
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            let last = if block.len() == 1 { block.start + 1 } else { block.end - 1 };
            for row in block.start..last.min(self.doc.read(cx).line_count() - 1) {
                let current = self.doc.read(cx).line_text(row);
                let next = self.doc.read(cx).line_text(row + 1);
                let trimmed_current = current.trim_end();
                let next_trimmed = next.trim_start();
                let start = self.doc.read(cx).line_start(row) + trimmed_current.len();
                let end = self.doc.read(cx).line_start(row + 1) + (next.len() - next_trimmed.len());
                let glue = if next_trimmed.is_empty()
                    || trimmed_current.is_empty()
                    || next_trimmed.starts_with([')', ']', '}', ',', ';', '.'])
                    || trimmed_current.ends_with(['(', '[', '{'])
                {
                    ""
                } else {
                    " "
                };
                edits.push((start..end, glue.to_string()));
            }
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn newline_above(&mut self, _: &NewlineAbove, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let row = doc.row_of(s.head);
            let start = doc.line_start(row);
            let line = doc.line_text(row);
            let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let len = indent.len();
            (start..start, format!("{indent}\n"), Some(len))
        });
    }

    fn toggle_block_comment(&mut self, _: &ToggleBlockComment, _: &mut Window, cx: &mut Context<Self>) {
        let Some((open, close)) = self.doc.read(cx).language.block_comment() else {
            return;
        };
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let range = s.range();
            let text = doc.slice(range.clone());
            if s.is_empty() {
                return (range, format!("{open}{close}"), Some(open.len()));
            }
            let (o, c) = (open.trim_end(), close.trim_start());
            if text.starts_with(o) && text.ends_with(c) && text.len() >= o.len() + c.len() {
                let inner = &text[o.len()..text.len() - c.len()];
                let inner = inner.strip_prefix(' ').unwrap_or(inner);
                let inner = inner.strip_suffix(' ').unwrap_or(inner);
                (range, inner.to_string(), None)
            } else {
                (range, format!("{open}{text}{close}"), None)
            }
        });
    }
}

// ---- IME / platform text input ---------------------------------------------

impl Editor {
    fn to_utf16(&self, offset: usize, cx: &App) -> usize {
        let r = self.doc.read(cx).rope();
        r.char_to_utf16_cu(r.byte_to_char(offset.min(r.len_bytes())))
    }

    fn from_utf16(&self, offset: usize, cx: &App) -> usize {
        let r = self.doc.read(cx).rope();
        let ch = r.utf16_cu_to_char(offset.min(r.len_utf16_cu()));
        r.char_to_byte(ch)
    }

    fn range_from_utf16(&self, r: &Range<usize>, cx: &App) -> Range<usize> {
        self.from_utf16(r.start, cx)..self.from_utf16(r.end, cx)
    }

    fn range_to_utf16(&self, r: &Range<usize>, cx: &App) -> Range<usize> {
        self.to_utf16(r.start, cx)..self.to_utf16(r.end, cx)
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16, cx);
        actual_range.replace(self.range_to_utf16(&range, cx));
        Some(self.doc.read(cx).slice(range))
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, cx: &mut Context<Self>) -> Option<UTF16Selection> {
        let s = self.primary_selection();
        Some(UTF16Selection {
            range: self.range_to_utf16(&s.range(), cx),
            reversed: s.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r, cx))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let explicit = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r, cx))
            .or(self.marked_range.clone());
        match explicit {
            Some(range) => {
                // IME commit: replace the composed range only.
                let before = self.selections.clone();
                let new = self.change_doc(cx, |doc| doc.edit(vec![(range, text.to_string())], &before, EditKind::Other));
                let end = new[0].end;
                self.set_selections(vec![Selection::cursor(end)], 0, cx);
                self.after_edit(cx);
            }
            None => self.insert(text, cx),
        }
        self.marked_range = None;
        self.restart_blink(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r, cx))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.primary_selection().range());
        let before = self.selections.clone();
        let new = self.change_doc(cx, |doc| doc.edit(vec![(range, new_text.to_string())], &before, EditKind::Other));
        let new_range = new[0].clone();
        let sel = match new_selected_range_utf16 {
            Some(r) => {
                let base = self.to_utf16(new_range.start, cx);
                let s = self.from_utf16(base + r.start, cx);
                let e = self.from_utf16(base + r.end, cx);
                Selection { anchor: s, head: e }
            }
            None => Selection::cursor(new_range.end),
        };
        self.set_selections(vec![sel], 0, cx);
        self.marked_range = (!new_text.is_empty()).then_some(new_range);
        self.after_edit(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.sync_wrap(window, cx);
        let layout = self.layout.clone()?;
        let range = self.range_from_utf16(&range_utf16, cx);
        let drow = self.display_row_of(range.start, cx);
        let y = layout.bounds.top() + layout.line_height * drow as f32 - self.scroll.y;
        let x0 = self.x_for_offset(range.start, window, cx);
        let end = range.end.max(range.start);
        let x1 = if self.display_row_of(end, cx) == drow { self.x_for_offset(end, window, cx) } else { x0 };
        let left = layout.text_left - self.scroll.x;
        Some(Bounds::from_corners(
            point(left + x0, y),
            point(left + x1, y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_for_position(point, window, cx);
        Some(self.to_utf16(offset, cx))
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popover = self.render_hunk_popover(cx);
        let lsp_popovers = self.render_lsp_popovers(window, cx);
        let preview_toggle = self.is_markdown(cx).then(|| {
            let theme = Theme::get(cx);
            let on = self.previewed;
            div()
                .id("markdown-preview-toggle")
                .absolute()
                .top(px(6.))
                .right(self.right_edge_width(cx) + px(6.))
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover_fill(if on { Hsla::from(theme.accent_soft) } else { Hsla::from(theme.elevated) }, theme.hover)
                .tooltip(move |_, cx| crate::ui::tooltip_view(if on { "Hide Preview" } else { "Show Preview" }, cx))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|e, _, _, cx| e.request_markdown_preview(cx)))
                .child(crate::ui::icon_el(
                    crate::assets::Icon::SplitRight,
                    px(13.),
                    if on { theme.accent.into() } else { theme.text_muted.into() },
                ))
        });
        let theme = Theme::get(cx);
        div()
            .id("editor")
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .map(|el| match self.mode {
                EditorMode::Full => el.size_full().bg(theme.background),
                EditorMode::SingleLine => el.w_full().h(px(20.)),
                EditorMode::MultiLine => el.size_full(),
            })
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::move_word_left))
            .on_action(cx.listener(Self::move_word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::move_line_start))
            .on_action(cx.listener(Self::move_line_end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::move_doc_start))
            .on_action(cx.listener(Self::move_doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::newline_below))
            .on_action(cx.listener(Self::indent))
            .on_action(cx.listener(Self::outdent))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::select_line))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::toggle_comment))
            .on_action(cx.listener(Self::duplicate_line))
            .on_action(cx.listener(Self::move_line_up))
            .on_action(cx.listener(Self::move_line_down))
            .on_action(cx.listener(Self::select_next_occurrence))
            .on_action(cx.listener(Self::add_cursor_above))
            .on_action(cx.listener(Self::add_cursor_below))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::delete_line))
            .on_action(cx.listener(Self::extend_selection))
            .on_action(cx.listener(Self::shrink_selection))
            .on_action(cx.listener(Self::unselect_occurrence))
            .on_action(cx.listener(Self::select_all_occurrences))
            .on_action(cx.listener(Self::toggle_case))
            .on_action(cx.listener(Self::toggle_inlay_hints))
            .on_action(cx.listener(Self::convert_to_upper_case))
            .on_action(cx.listener(Self::convert_to_lower_case))
            .on_action(cx.listener(Self::convert_to_title_case))
            .on_action(cx.listener(Self::convert_to_snake_case))
            .on_action(cx.listener(Self::convert_to_kebab_case))
            .on_action(cx.listener(Self::convert_to_camel_case))
            .on_action(cx.listener(Self::convert_to_pascal_case))
            .on_action(cx.listener(Self::sort_lines_ascending))
            .on_action(cx.listener(Self::sort_lines_descending))
            .on_action(cx.listener(Self::reverse_lines))
            .on_action(cx.listener(Self::remove_duplicate_lines))
            .on_action(cx.listener(Self::join_lines))
            .on_action(cx.listener(Self::newline_above))
            .on_action(cx.listener(Self::toggle_block_comment))
            .on_action(cx.listener(Self::rollback_hunk))
            .on_action(cx.listener(Self::stage_hunk))
            .on_action(cx.listener(Self::unstage_hunk))
            .on_action(cx.listener(Self::accept_current))
            .on_action(cx.listener(Self::accept_incoming))
            .on_action(cx.listener(Self::accept_both))
            .on_action(cx.listener(Self::next_change))
            .on_action(cx.listener(Self::previous_change))
            .on_action(cx.listener(Self::next_problem))
            .on_action(cx.listener(Self::previous_problem))
            .on_action(cx.listener(Self::toggle_inline_blame))
            .on_action(cx.listener(Self::toggle_soft_wrap))
            .on_action(cx.listener(Self::show_completions))
            .on_action(cx.listener(Self::quick_documentation))
            .on_action(cx.listener(Self::go_to_declaration))
            .on_action(cx.listener(Self::go_to_implementation))
            .on_action(cx.listener(Self::go_to_type_definition))
            .on_action(cx.listener(Self::find_usages))
            .on_action(cx.listener(Self::call_hierarchy))
            .on_action(cx.listener(Self::type_hierarchy))
            .on_action(cx.listener(Self::rename_symbol))
            .on_action(cx.listener(Self::show_context_actions))
            .on_action(cx.listener(Self::reformat_code))
            .on_action(cx.listener(Self::collapse_region))
            .on_action(cx.listener(Self::expand_region))
            .on_action(cx.listener(Self::collapse_all))
            .on_action(cx.listener(Self::expand_all))
            .on_action(cx.listener(Self::run_at_caret))
            .on_action(cx.listener(Self::debug_at_caret))
            .on_action(cx.listener(Self::toggle_breakpoint))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_hover(cx.listener(|editor, hovered: &bool, window, cx| {
                if !*hovered {
                    editor.pointer_left(window, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .relative()
            .child(EditorElement { editor: cx.entity() })
            .children(popover)
            .children(lsp_popovers)
            .children(preview_toggle)
    }
}

// ---- Element ---------------------------------------------------------------------

/// What block rows show (see `Editor::blocks`).
#[derive(Clone)]
enum BlockKind {
    /// The problems of line `of` (the caret's) in full, a row each, under
    /// that line or its fold.
    Problems { of: usize, rows: Vec<(lint::Severity, String)> },
    /// An opened hunk's deleted lines, a row each.
    Deleted(Vec<String>),
    /// A line's code lenses, on one row.
    Lenses(Vec<crate::document::CodeLens>),
}

#[derive(Clone)]
struct Block {
    /// The line it sits above (one past the last: after it).
    line: usize,
    kind: BlockKind,
}

impl Block {
    fn rows(&self) -> usize {
        match &self.kind {
            BlockKind::Deleted(lines) => lines.len(),
            BlockKind::Problems { rows, .. } => rows.len(),
            BlockKind::Lenses(_) => 1,
        }
    }
}

/// An `Arc` compared by address, for what a cache was built from: holding it
/// keeps the address from going to another value.
#[derive(Clone)]
struct Same<T>(Arc<T>);

impl<T> PartialEq for Same<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What blocks are built from (see `Editor::cached_blocks`), folds aside.
#[derive(Clone, PartialEq)]
struct BlocksKey {
    mode: EditorMode,
    /// The document's id, version, diagnostics and code lens generations.
    doc: (u64, u64, u64, u64),
    /// The open hunk and the list it's in.
    hunk: Option<(usize, Same<Vec<Hunk>>)>,
    /// The caret, when it's the only one (its line's problems can show).
    caret: Option<usize>,
    /// Wrap width and character width, which problems wrap to.
    layout: Option<(Pixels, Pixels)>,
    tab_size: usize,
    code_lens: bool,
}

struct BlocksCache {
    key: BlocksKey,
    folds: Vec<Range<usize>>,
    blocks: Arc<Vec<Block>>,
    rows: Arc<Vec<(usize, usize)>>,
    /// The caret line's problems block: (its line, the caret's).
    problems: Option<(usize, usize)>,
}

/// Finds the diagnostics that reach into a byte range without walking the
/// ones before it: they're sorted by start, and a tree of the farthest end
/// under each node skips those that end sooner. Built per document state.
#[derive(Default)]
struct Reach {
    /// Document id, version and diagnostics generation it's for.
    key: Option<(u64, u64, u64)>,
    /// The farthest end under each node of a complete binary tree whose
    /// leaves, from `leaves` on, are the diagnostics.
    tree: Vec<usize>,
    leaves: usize,
}

impl Reach {
    fn update(&mut self, doc: &Document) {
        let key = (doc.id(), doc.version(), doc.diagnostics_generation());
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        self.leaves = doc.diagnostics.len().next_power_of_two();
        self.tree = vec![0; 2 * self.leaves];
        for (i, d) in doc.diagnostics.iter().enumerate() {
            self.tree[self.leaves + i] = d.range.end;
        }
        for node in (1..self.leaves).rev() {
            self.tree[node] = self.tree[2 * node].max(self.tree[2 * node + 1]);
        }
    }

    /// The diagnostics (of the document it was updated for) that start by
    /// `range.end` and end at `range.start` or later, in order.
    fn find<'a>(&self, diagnostics: &'a [lint::Diagnostic], range: std::ops::RangeInclusive<usize>) -> Vec<&'a lint::Diagnostic> {
        let below = diagnostics.partition_point(|d| d.range.start <= *range.end());
        let mut found = Vec::new();
        // (node, first leaf under it, leaves under it)
        let mut stack = vec![(1, 0, self.leaves)];
        while let Some((node, first, len)) = stack.pop() {
            if first >= below || self.tree.get(node).is_none_or(|&end| end < *range.start()) {
                continue;
            }
            if len == 1 {
                found.push(&diagnostics[first]);
            } else {
                // The left half after the right, so it's taken first.
                stack.push((2 * node + 1, first + len / 2, len / 2));
                stack.push((2 * node, first, len / 2));
            }
        }
        found
    }
}

#[derive(Default)]
pub struct FrameLayout {
    text_bounds: Bounds<Pixels>,
    line_height: Pixels,
    backgrounds: Vec<PaintQuad>,
    guides: Vec<PaintQuad>,
    search: Vec<PaintQuad>,
    selections: Vec<PaintQuad>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursors: Vec<PaintQuad>,
    numbers: Vec<(Point<Pixels>, ShapedLine)>,
    gutter_marks: Vec<PaintQuad>,
    /// Shapes that aren't rectangles (logpoints' diamonds).
    gutter_paths: Vec<(gpui::Path<Pixels>, Hsla)>,
    /// Fold arrows: bounds, icon and color.
    fold_icons: Vec<(Bounds<Pixels>, &'static str, Hsla)>,
    inline_bgs: Vec<PaintQuad>,
    /// Code lens titles, for the pointer cursor.
    lens_bounds: Vec<Bounds<Pixels>>,
    lens_hitboxes: Vec<Hitbox>,
    /// The ⌘/Ctrl-hover link's word, for the pointer cursor.
    link_bounds: Option<Bounds<Pixels>>,
    link_hitbox: Option<Hitbox>,
    inline_texts: Vec<(Point<Pixels>, ShapedLine)>,
    scrollbar: Vec<PaintQuad>,
    /// The minimap's background, blocks and slider, painted inside its bounds.
    minimap: Vec<PaintQuad>,
    minimap_bounds: Option<Bounds<Pixels>>,
    /// The scrollbar's (or minimap's) strip, for its cursor and hover.
    scrollbar_track: Option<Bounds<Pixels>>,
    scrollbar_hitbox: Option<Hitbox>,
    sticky_bgs: Vec<PaintQuad>,
    sticky_lines: Vec<(Point<Pixels>, ShapedLine)>,
    sticky_numbers: Vec<(Point<Pixels>, ShapedLine)>,
    hitbox: Option<Hitbox>,
}

struct EditorElement {
    editor: Entity<Editor>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = FrameLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> FrameLayout {
        let mut frame = self
            .editor
            .update(cx, |editor, cx| editor.layout_frame(bounds, window, cx));
        frame.hitbox = Some(window.insert_hitbox(frame.text_bounds, HitboxBehavior::Normal));
        // Over the text's, so a code lens shows it can be clicked.
        frame.lens_hitboxes = frame.lens_bounds.iter().map(|b| window.insert_hitbox(*b, HitboxBehavior::Normal)).collect();
        frame.link_hitbox = frame.link_bounds.map(|b| window.insert_hitbox(b, HitboxBehavior::Normal));
        frame.scrollbar_hitbox = frame.scrollbar_track.map(|b| window.insert_hitbox(b, HitboxBehavior::Normal));
        frame
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut FrameLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.editor.clone()), cx);
        if let Some(hitbox) = frame.hitbox.as_ref() {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }
        for hitbox in frame.lens_hitboxes.iter().chain(&frame.link_hitbox) {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        if let Some(hitbox) = frame.scrollbar_hitbox.clone() {
            window.set_cursor_style(CursorStyle::Arrow, &hitbox);
            // Anywhere in the window: leaving the editor straight from the bar
            // ends its hover too.
            let editor = self.editor.clone();
            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                let over = hitbox.is_hovered(window);
                if phase == gpui::DispatchPhase::Bubble && editor.read(cx).scrollbar_hovered != over {
                    editor.update(cx, |e, cx| {
                        e.scrollbar_hovered = over;
                        cx.notify();
                    });
                }
            });
        }

        let line_height = frame.line_height;
        for q in frame.backgrounds.drain(..) {
            window.paint_quad(q);
        }
        window.with_content_mask(Some(ContentMask { bounds: frame.text_bounds }), |window| {
            for q in frame.guides.drain(..) {
                window.paint_quad(q);
            }
            for q in frame.search.drain(..) {
                window.paint_quad(q);
            }
            for q in frame.selections.drain(..) {
                window.paint_quad(q);
            }
            for q in frame.inline_bgs.drain(..) {
                window.paint_quad(q);
            }
            for (origin, line) in &frame.lines {
                line.paint(*origin, line_height, window, cx).ok();
            }
            for (origin, line) in &frame.inline_texts {
                line.paint(*origin, line_height, window, cx).ok();
            }
            for q in frame.cursors.drain(..) {
                window.paint_quad(q);
            }
        });
        for (origin, line) in &frame.numbers {
            line.paint(*origin, line_height, window, cx).ok();
        }
        for q in frame.gutter_marks.drain(..) {
            window.paint_quad(q);
        }
        for (path, color) in frame.gutter_paths.drain(..) {
            window.paint_path(path, color);
        }
        for (bounds, icon, color) in frame.fold_icons.drain(..) {
            window.paint_svg(bounds, icon.into(), gpui::TransformationMatrix::unit(), color, cx).ok();
        }
        // Sticky scroll's pinned lines, over everything but the scrollbar.
        for q in frame.sticky_bgs.drain(..) {
            window.paint_quad(q);
        }
        window.with_content_mask(Some(ContentMask { bounds: frame.text_bounds }), |window| {
            for (origin, line) in &frame.sticky_lines {
                line.paint(*origin, line_height, window, cx).ok();
            }
        });
        for (origin, line) in &frame.sticky_numbers {
            line.paint(*origin, line_height, window, cx).ok();
        }
        if let Some(bounds) = frame.minimap_bounds {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                for q in frame.minimap.drain(..) {
                    window.paint_quad(q);
                }
            });
        }
        for q in frame.scrollbar.drain(..) {
            window.paint_quad(q);
        }
    }
}

#[cfg(test)]
mod save_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Save writes in the background: the tab is unsaved until the file has
    /// the text, the save goes on if nothing waits for it, and a save made
    /// here and now (closing) overtakes one still being written.
    #[gpui::test]
    fn saves_in_the_background(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("kuana-editor-saves-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let editor = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        editor.update(cx, |e, cx| e.replace_ranges(vec![(0..0, "zero ".into())], cx));
        let saving = editor.update(cx, |e, cx| e.save_in_background(cx));
        editor.read_with(cx, |e, cx| assert!(e.doc.read(cx).is_dirty() && e.doc.read(cx).is_being_saved()));
        cx.run_until_parked();
        assert!(futures::FutureExt::now_or_never(saving).is_some_and(|saved| saved.is_ok()));
        editor.read_with(cx, |e, cx| assert!(!e.doc.read(cx).is_dirty() && !e.doc.read(cx).is_being_saved()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "zero one\n");

        // Nothing waits for it: written all the same.
        editor.update(cx, |e, cx| e.replace_ranges(vec![(0..0, "a ".into())], cx));
        drop(editor.update(cx, |e, cx| e.save_in_background(cx)));
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a zero one\n");

        editor.update(cx, |e, cx| e.replace_ranges(vec![(0..0, "b ".into())], cx));
        drop(editor.update(cx, |e, cx| e.save_in_background(cx)));
        editor.update(cx, |e, cx| e.save(cx)).unwrap();
        editor.read_with(cx, |e, cx| assert!(!e.doc.read(cx).is_dirty()));
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b a zero one\n");
        editor.read_with(cx, |e, cx| assert!(!e.doc.read(cx).is_dirty() && !e.doc.read(cx).changed_on_disk()));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod blame_tests {
    use super::*;

    /// A row is followed back to the blamed text through the edits since:
    /// lines above and below keep their commit, a line edited isn't
    /// committed until its text is the same again, and a line break added
    /// at either end of a line leaves the line its commit.
    #[test]
    fn blamed_rows_follow_edits() {
        let mut doc = Document::from_text("a\nb\nc\n".into(), None);
        let commit = |summary: &str| BlameLine { author: "A".into(), time: 0, summary: summary.into(), sha: summary.into(), uncommitted: false };
        let git_file = GitFile { root: "/r".into(), rel: "f".into(), user_name: None, head: None };
        let lines = git::FileBlame::of(vec![commit("a"), commit("b"), commit("c")], vec![0, 1, 2]);
        let blame = Blame { git_file, doc: doc.id(), version: doc.version(), text: doc.rope().clone(), lines };
        let shown = |doc: &Document| -> Vec<String> {
            (0..doc.line_count() - 1)
                .map(|row| match blame.line(row, doc).expect("followed") {
                    Some(line) if line.uncommitted => "new".to_string(),
                    Some(line) => line.summary,
                    None => "-".to_string(),
                })
                .collect()
        };
        assert_eq!(shown(&doc), ["a", "b", "c"]);
        doc.edit(vec![(1..1, "x".into())], &[], EditKind::Other);
        assert_eq!(shown(&doc), ["new", "b", "c"]);
        doc.undo();
        assert_eq!(shown(&doc), ["a", "b", "c"]);
        // Enter at the end of `a`, then at the start of `c`.
        doc.edit(vec![(1..1, "\n".into())], &[], EditKind::Other);
        let c = doc.line_start(3);
        doc.edit(vec![(c..c, "\n".into())], &[], EditKind::Other);
        assert_eq!(shown(&doc), ["a", "new", "b", "new", "c"]);
        // Lines added above move the rest down.
        doc.edit(vec![(0..0, "x\ny\n".into())], &[], EditKind::Other);
        assert_eq!(shown(&doc), ["new", "new", "a", "new", "b", "new", "c"]);

        // A last line without a line break keeps its commit when it gets one.
        let mut doc = Document::from_text("a\nb".into(), None);
        let lines = git::FileBlame::of(vec![commit("a"), commit("b")], vec![0, 1]);
        let blame = Blame { git_file: blame.git_file.clone(), doc: doc.id(), version: doc.version(), text: doc.rope().clone(), lines };
        doc.edit(vec![(3..3, "\n".into())], &[], EditKind::Other);
        let summary = |row| blame.line(row, &doc).flatten().map(|l| if l.uncommitted { "new".to_string() } else { l.summary });
        assert_eq!((summary(0), summary(1)), (Some("a".into()), Some("b".into())));
    }
}

#[cfg(test)]
mod lint_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Syntax errors come from the document's tree, and where a language
    /// server reports them (or they're turned off) nothing runs at all.
    #[gpui::test]
    fn syntax_errors_unless_a_server_reports_them(cx: &mut TestAppContext) {
        let doc = Document::from_text("fn main() {\n    let x = ;\n}\n".into(), Some("a.rs".into()));
        let editor = cx.new(|cx| Editor::new(doc, cx));
        cx.run_until_parked();
        let syntax_errors = |editor: &Entity<Editor>, cx: &mut TestAppContext| {
            editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().filter(|d| d.rule == "syntax").count())
        };
        assert_eq!(syntax_errors(&editor, cx), 1);
        editor.update(cx, |e, cx| {
            e.lsp.active = true;
            e.replace_ranges(vec![(0..0, " ".into())], cx);
            assert!(e.lint_task.is_none(), "nothing to run");
        });
        assert_eq!(syntax_errors(&editor, cx), 0);
        editor.update(cx, |e, cx| {
            e.lsp.active = false;
            e.file_settings.syntax_errors = false;
            e.replace_ranges(vec![(0..0, " ".into())], cx);
            assert!(e.lint_task.is_none());
            e.file_settings.syntax_errors = true;
            e.replace_ranges(vec![(0..0, " ".into())], cx);
        });
        // After the pause after typing.
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(syntax_errors(&editor, cx), 1);
    }
}

#[cfg(test)]
mod block_tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;

    #[test]
    fn wraps_between_words() {
        assert_eq!(wrap_words("expected `i32`, found `&str`", 12), ["expected", "`i32`, found", "`&str`"]);
        assert_eq!(wrap_words("short", 40), ["short"]);
    }

    #[gpui::test]
    fn the_caret_lines_problems_show_in_full_when_the_hint_cant(cx: &mut TestAppContext) {
        let editor = cx.new(|cx| Editor::new(Document::from_text("let x = 1;\nlet y = 2;\n".into(), None), cx));
        let problem = |at: usize, message: &str| lint::Diagnostic {
            range: at..at + 1,
            severity: lint::Severity::Error,
            message: message.into(),
            rule: "e".into(),
        };
        editor.update(cx, |e, cx| {
            // 50 columns of text.
            e.layout = Some(LayoutSnapshot {
                bounds: Bounds::default(),
                text_left: px(0.),
                line_height: px(20.),
                char_width: px(8.),
                gutter_width: px(0.),
                rows_per_page: 10,
                wrap_width: px(400.),
            });
            // Fits after the code: the end-of-line hint is enough.
            e.doc.as_mut(cx).set_lsp_diagnostics(vec![problem(4, "unused")]);
            assert!(e.caret_problem_rows(cx).is_empty());
            // Several lines: each gets a row, under the line.
            e.doc.as_mut(cx).set_lsp_diagnostics(vec![problem(4, "mismatched types\nexpected `i32`")]);
            let rows: Vec<String> = e.caret_problem_rows(cx).into_iter().map(|(_, t)| t).collect();
            assert_eq!(rows, ["mismatched types", "expected `i32`"]);
            assert_eq!(*e.block_rows(cx), [(1, 2)]);
            // Folded, the line's problems go under the lines its fold hides.
            e.folds.fold_rows(e.doc.read(cx), 1, 1);
            assert_eq!(*e.block_rows(cx), [(2, 2)]);
            e.folds.unfold_where(|_| true);
            // Two problems on the line: both.
            e.doc.as_mut(cx).set_lsp_diagnostics(vec![problem(0, "first"), problem(4, "second")]);
            assert_eq!(e.caret_problem_rows(cx).len(), 2);
            // Another line's problems don't show; nor with several carets.
            e.set_selections(vec![Selection::cursor(12)], 0, cx);
            assert!(e.caret_problem_rows(cx).is_empty());
            e.set_selections(vec![Selection::cursor(0), Selection::cursor(12)], 0, cx);
            assert!(e.caret_problem_rows(cx).is_empty());
        });
    }

    #[gpui::test]
    fn next_problem_steps_through_the_files_problems(cx: &mut TestAppContext) {
        let editor = cx.new(|cx| Editor::new(Document::from_text("let a = 1;\nlet b = 2;\nlet c = 3;\n".into(), None), cx));
        let at = |start: usize, severity: lint::Severity| lint::Diagnostic { range: start..start + 1, severity, message: "m".into(), rule: "r".into() };
        editor.update(cx, |e, cx| {
            // A language server's problem, a built-in one, and a hint, which isn't one.
            e.doc.as_mut(cx).set_lsp_diagnostics(vec![at(15, lint::Severity::Warning), at(26, lint::Severity::Hint)]);
            e.doc.as_mut(cx).set_builtin_diagnostics(vec![at(4, lint::Severity::Error)]);
            let mut steps = Vec::new();
            for forward in [true, true, true, false, false] {
                e.jump_to_problem(forward, cx);
                steps.push(e.primary_selection().head);
            }
            // In document order, around the end both ways.
            assert_eq!(steps, [4, 15, 4, 15, 4]);
        });
    }

    #[gpui::test]
    fn occurrences_of_a_word_at_the_caret_are_whole_words(cx: &mut TestAppContext) {
        let editor = cx.new(|cx| Editor::new(Document::from_text("foo foobar Foo foo_x foo\nfoo\n".into(), None), cx));
        editor.update(cx, |e, cx| {
            let selected = |e: &Editor| e.selections.iter().map(Selection::range).collect::<Vec<_>>();
            // From a caret: its word, then the same word, matching case.
            e.set_selections(vec![Selection::cursor(1)], 0, cx);
            for _ in 0..4 {
                e.add_next_occurrence(cx);
            }
            assert_eq!(selected(e), [0..3, 21..24, 25..28]);
            // Selected by hand: any match, still matching case.
            e.set_selections(vec![Selection { anchor: 0, head: 3 }], 0, cx);
            e.add_next_occurrence(cx);
            assert_eq!(selected(e), [0..3, 4..7]);
            // Select All Occurrences from a caret: whole words too.
            e.set_selections(vec![Selection::cursor(22)], 0, cx);
            e.select_occurrences(cx);
            assert_eq!(selected(e), [0..3, 21..24, 25..28]);
        });
    }

    #[test]
    fn occurrences_are_found_across_the_ropes_chunks() {
        let needle = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
        let text: String = (0..200).map(|i| format!("{needle}-{i:04}|")).collect();
        let doc = Document::from_text(text.clone(), None);
        assert!(doc.rope().chunks().count() > 4);
        for from in (0..text.len()).step_by(7) {
            let expected = text[from..].find(needle).map(|i| i + from);
            assert_eq!(find_occurrence(&doc, needle, from, text.len(), false), expected, "from {from}");
        }
        // Only matches starting before `to`; whole words with word characters around fail.
        assert_eq!(find_occurrence(&doc, needle, 1, 46, false), None);
        assert_eq!(find_occurrence(&doc, needle, 0, 1, false), Some(0));
        assert_eq!(find_occurrence(&doc, "efgh", 0, text.len(), true), None);
        assert_eq!(find_occurrence(&doc, "-0001|", 0, text.len(), true), Some(86));
    }

    #[test]
    fn problems_are_found_around_the_file() {
        let starts = [5, 5, 20, 40];
        assert_eq!(problem_from(&starts, 0, true), Some(5));
        assert_eq!(problem_from(&starts, 5, true), Some(20));
        assert_eq!(problem_from(&starts, 40, true), Some(5));
        assert_eq!(problem_from(&starts, 30, false), Some(20));
        assert_eq!(problem_from(&starts, 5, false), Some(40));
        assert_eq!(problem_from(&[], 3, true), None);
    }
}

#[cfg(test)]
mod text_field_tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;

    #[gpui::test]
    fn text_fields_take_brackets_and_quotes_as_typed(cx: &mut TestAppContext) {
        let typed = |editor: &Entity<Editor>, text: &str, cx: &mut TestAppContext| {
            editor.update(cx, |e, cx| {
                for c in text.chars() {
                    e.insert(&c.to_string(), cx);
                }
                e.text(cx)
            })
        };
        // A search for `fn new(` finds that, not `fn new()`.
        let search = cx.new(|cx| Editor::single_line("Find", cx));
        assert_eq!(typed(&search, "fn new(", cx), "fn new(");
        assert_eq!(typed(&search, ")", cx), "fn new()");
        let message = cx.new(|cx| Editor::multi_line("Commit message", cx));
        assert_eq!(typed(&message, "Fix \"quotes\" [x]", cx), "Fix \"quotes\" [x]");
        // The code editor pairs them.
        let code = cx.new(|cx| Editor::new(Document::empty(), cx));
        assert_eq!(typed(&code, "f(", cx), "f()");
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn control_characters_show_as_symbols() {
        let symbols: Vec<Option<&str>> =
            ['\0', '\x0b', '\x0c', '\r', '\x1b', '\x7f', '\u{85}', '\u{2028}', '\u{2029}'].into_iter().map(control_symbol).collect();
        let expected = ["␀", "␋", "␌", "␍", "␛", "␡", "NEL", "LS", "PS"];
        assert_eq!(symbols, expected.map(Some));
        for plain in ['\t', ' ', 'a', '\u{a0}', 'é', '—', '\u{2027}', '\u{202a}'] {
            assert_eq!(control_symbol(plain), None, "{plain:?}");
        }
        // The quick check agrees with the table, byte by byte.
        for c in ('\0'..='\u{3000}').filter(|&c| c != '\t' && c != '\n') {
            assert_eq!(has_tabs_or_controls(&format!("a{c}b")), control_symbol(c).is_some(), "{c:?}");
        }
    }

    /// Restyling walks runs and ranges once; it cuts and restyles as
    /// checking every range for every piece would.
    #[test]
    fn restyled_runs_are_cut_at_the_ranges() {
        let run = |len: usize| TextRun { len, font: font("Test"), color: gpui::black(), background_color: None, underline: None, strikethrough: None };
        let runs = vec![run(4), run(6), run(3), run(7)];
        let ranges = [1..2, 3..5, 5..5, 9..13, 15..20];
        let restyled = restyle_runs(runs, &ranges, |piece| piece.underline = Some(UnderlineStyle::default()));
        let pieces: Vec<(usize, bool)> = restyled.iter().map(|r| (r.len, r.underline.is_some())).collect();
        assert_eq!(
            pieces,
            [(1, false), (1, true), (1, false), (1, true), (1, true), (4, false), (1, true), (3, true), (2, false), (5, true)]
        );
    }

    #[test]
    fn symbols_map_to_their_one_character() {
        // A form feed, a line separator and a tab: "a␌bLSc  d".
        let line = "a\x0cb\u{2028}c\td";
        let dl = DisplayLine::new(line, 4);
        assert_eq!(dl.text, "a␌bLSc  d");
        assert_eq!(dl.controls, [1..4, 5..7]);
        let (ff, b, ls, c, tab, d) = (1, 2, 3, 6, 7, 8);
        let shown: Vec<usize> = [ff, b, ls, c, tab, d, line.len()].map(|col| dl.to_display(col)).to_vec();
        assert_eq!(shown, [1, 4, 5, 7, 8, 10, 11]);
        // A click before or after a symbol is at its character or the next.
        assert_eq!((dl.to_buffer(1), dl.to_buffer(4), dl.to_buffer(5), dl.to_buffer(7)), (ff, b, ls, c));
        // Without tabs or control characters, the text is the line itself.
        let plain = DisplayLine::new("a — b", 4);
        assert!(plain.controls.is_empty() && plain.map.is_none());
    }
}

/// What a frame does doesn't grow with the document: lists as long as the
/// document (carets, matches, problems, lenses) are looked up for the rows
/// on screen, or kept from frame to frame until they change, and a long
/// line is built only where it shows.
#[cfg(test)]
mod frame_tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{AnyView, AppContext, TestAppContext, VisualTestContext};

    use super::*;

    const LINES: usize = 10_000;
    const LINE: &str = "let e = e + e * e - e / e; // e e e e e e e e e e e e e\n";

    /// A view whose element lays the editor out (as `EditorElement` does,
    /// which layout needs: it's in a view's draw) and keeps the frame.
    struct Host {
        editor: Entity<Editor>,
        frame: Rc<RefCell<Option<FrameLayout>>>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Probe { editor: self.editor.clone(), frame: self.frame.clone() }
        }
    }

    struct Probe {
        editor: Entity<Editor>,
        frame: Rc<RefCell<Option<FrameLayout>>>,
    }

    impl IntoElement for Probe {
        type Element = Self;

        fn into_element(self) -> Self {
            self
        }
    }

    impl Element for Probe {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            None
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
            let mut style = Style::default();
            style.size.width = relative(1.).into();
            style.size.height = relative(1.).into();
            (window.request_layout(style, [], cx), ())
        }

        fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
            let frame = self.editor.update(cx, |e, cx| e.layout_frame(bounds, window, cx));
            *self.frame.borrow_mut() = Some(frame);
        }

        fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), _: &mut Window, _: &mut App) {}
    }

    /// An editor of `text`, focused, in a window 800 by 600, without soft
    /// wrap (long lines break at a fixed column then).
    fn editor(text: String, cx: &mut TestAppContext) -> (Entity<Editor>, Entity<Host>, &mut VisualTestContext) {
        cx.update(|cx| cx.set_global(Theme::blank_for_tests()));
        let editor = cx.new(|cx| {
            let mut e = Editor::new(Document::from_text(text, None), cx);
            e.soft_wrap = false;
            e
        });
        let host = cx.new(|_| Host { editor: editor.clone(), frame: Rc::default() });
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            window.activate_window();
            window.focus(&editor.focus_handle(cx));
        });
        cx.run_until_parked();
        (editor, host, cx)
    }

    fn frame(host: &Entity<Host>, cx: &mut VisualTestContext) -> FrameLayout {
        cx.draw(point(px(0.), px(0.)), size(px(800.), px(600.)), |_, _| AnyView::from(host.clone()));
        host.update(cx, |h, _| h.frame.borrow_mut().take()).expect("laid out")
    }

    /// A file of `lines` lines with a caret, a problem and a code lens on
    /// each, and 20 search matches on each (200k for `LINES`).
    fn busy_editor(lines: usize, cx: &mut TestAppContext) -> (Entity<Editor>, Entity<Host>, &mut VisualTestContext) {
        let (editor, host, cx) = editor(LINE.repeat(lines), cx);
        editor.update(cx, |e, cx| {
            let starts: Vec<usize> = (0..lines).map(|row| row * LINE.len()).collect();
            e.set_selections(starts.iter().map(|&at| Selection::cursor(at + 4)).collect(), 0, cx);
            let problem = |at: usize| lint::Diagnostic { range: at..at + 3, severity: Severity::Warning, message: "unused".into(), rule: "r".into() };
            e.doc.as_mut(cx).set_lsp_diagnostics(starts.iter().map(|&at| problem(at + 4)).collect());
            let lens = |line_start| crate::document::CodeLens {
                id: 0,
                line_start,
                title: "1 reference".into(),
                command: Arc::new(serde_json::Value::Null),
                server: "s".into(),
                unresolved: None,
            };
            let stamp = e.doc.read(cx).lens_stamp();
            e.doc.as_mut(cx).set_code_lenses(starts.iter().map(|&at| lens(at)).collect(), stamp);
            e.file_settings.code_lens = true;
            let text = e.text(cx);
            let ranges: Vec<Range<usize>> = text.match_indices('e').map(|(at, _)| at..at + 1).collect();
            assert_eq!(ranges.len(), 20 * lines);
            let matches = SearchMatches::new(ranges, e.doc.read(cx).rope());
            e.set_search_highlights(matches, Some(0), cx);
        });
        (editor, host, cx)
    }

    #[gpui::test]
    fn frames_keep_what_the_whole_document_gives(cx: &mut TestAppContext) {
        let (editor, host, cx) = busy_editor(LINES, cx);
        let first = frame(&host, cx);
        let (rebuilds, blocks) = editor.update(cx, |e, cx| (e.markers.rebuilds(), e.blocks(cx)));
        assert!(rebuilds > 0);
        // The scrollbar: a marker per pixel row of its track at most, per
        // kind (problems, changes, matches, carets), plus the thumb.
        assert!(first.scrollbar.len() <= 4 * 600 + 1, "{} quads", first.scrollbar.len());
        // A caret blink, then a scroll: what comes from the whole document
        // stays as it was.
        for _ in 0..2 {
            editor.update(cx, |e, _| e.cursor_visible = !e.cursor_visible);
            frame(&host, cx);
        }
        editor.update(cx, |e, _| e.scroll.y = px(20. * 5_000.));
        let scrolled = frame(&host, cx);
        editor.update(cx, |e, cx| {
            assert_eq!(e.markers.rebuilds(), rebuilds);
            assert!(Arc::ptr_eq(&e.blocks(cx), &blocks));
        });
        // A screen's worth drawn: a caret and its lens per line on it.
        assert!(scrolled.cursors.len() <= 31 && !scrolled.cursors.is_empty(), "{}", scrolled.cursors.len());
        assert!(scrolled.lens_bounds.len() <= 31 && !scrolled.lens_bounds.is_empty());
        // An edit changes what the markers come from.
        editor.update(cx, |e, cx| e.insert("x", cx));
        frame(&host, cx);
        editor.update(cx, |e, _| assert!(e.markers.rebuilds() > rebuilds));
    }

    /// `pieces` numbers with a tab and a control character each, on a line:
    /// 1.6 MB for 100k.
    fn long_line(pieces: usize) -> String {
        (0..pieces).map(|i| format!("{i:06}\t\u{1}ab,")).collect()
    }

    #[gpui::test]
    fn a_long_lines_rows_are_built_where_they_show(cx: &mut TestAppContext) {
        let line = long_line(100_000);
        let (editor, host, cx) = editor(format!("{line}\nend\n"), cx);
        let shaped = |f: &FrameLayout| f.lines.iter().map(|(_, l)| l.text.len()).sum::<usize>();
        let top = frame(&host, cx);
        // A screen of rows, not the line.
        assert!(shaped(&top) <= 31 * 512 * 3, "{} bytes shaped", shaped(&top));
        // In the middle of it: the rows there, as the whole line shows them.
        editor.update(cx, |e, _| e.scroll.y = px(20. * 1_000.));
        let middle = frame(&host, cx);
        let whole = DisplayLine::new(&line, 4);
        let breaks = editor.update(cx, |e, _| e.line_breaks(0).to_vec());
        assert_eq!(middle.lines.len(), 31);
        for (i, (_, row)) in middle.lines.iter().enumerate() {
            let (range, _) = wrap::segment(&breaks, 1_000 + i, whole.end());
            assert_eq!(row.text.as_ref(), whole.slice(range), "row {}", 1_000 + i);
        }
        // A caret there shows where the whole line has it.
        let at = line.len() / 2;
        editor.update(cx, |e, cx| {
            e.set_selections(vec![Selection::cursor(at)], 0, cx);
            e.autoscroll = false;
            let drow = e.display_row_of(at, cx);
            assert_eq!(drow, wrap::segment_of(&breaks, whole.to_display(at)));
            e.scroll.y = px(20. * drow as f32);
        });
        let caret = frame(&host, cx);
        assert_eq!(caret.cursors.len(), 1);
    }

    /// The time per frame (the fastest of a few) of an editor `make` sets up.
    fn frame_time(cx: &mut TestAppContext, make: impl FnOnce(&mut TestAppContext) -> (Entity<Editor>, Entity<Host>, &mut VisualTestContext)) -> std::time::Duration {
        let (editor, host, cx) = make(cx);
        frame(&host, cx);
        (0..20)
            .map(|i| {
                // Blinks and scrolls, as frames go (within the smaller documents).
                editor.update(cx, |e, _| {
                    e.cursor_visible = i % 2 == 0;
                    e.scroll.y = px(20. * (10 * (i / 2)) as f32);
                });
                let start = std::time::Instant::now();
                frame(&host, cx);
                start.elapsed()
            })
            .min()
            .unwrap_or_default()
    }

    /// `cargo test bench_frames -- --ignored --nocapture`: frames cost what's
    /// on screen, whatever the document holds. A file with a caret, a problem,
    /// a lens and 20 matches per line, and a long line, each at a tenth of
    /// the size and at full size (10k carets, problems and lenses, 200k
    /// matches; a 1.6 MB line): ten times the document, the same frame.
    #[gpui::test]
    #[ignore]
    fn bench_frames(cx: &mut TestAppContext) {
        let plain = frame_time(cx, |cx| editor(LINE.repeat(LINES), cx));
        let busy = [LINES / 10, LINES].map(|lines| frame_time(cx, |cx| busy_editor(lines, cx)));
        let long = [10_000, 100_000].map(|pieces| frame_time(cx, |cx| editor(long_line(pieces), cx)));
        println!("frame: plain {plain:?}; carets, problems, lenses and matches: {:?} → {:?}; a long line: {:?} → {:?}", busy[0], busy[1], long[0], long[1]);
        // Walking the document would make them ten times slower.
        for [small, big] in [busy, long] {
            assert!(big < small * 2, "{big:?} vs {small:?}");
        }
    }
}
