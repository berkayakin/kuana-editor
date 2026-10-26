//! The window's root view: title bar, file tree, tabs, problems panel and
//! status bar around the active editor.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, AnyView, App, Bounds, ClickEvent, Context, Div, Entity, EntityId, FocusHandle, Focusable, FontWeight, Hsla,
    CursorStyle, KeyDownEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PathPromptOptions, Pixels, PromptLevel, Render,
    SharedString, Stateful, StyleRefinement, Subscription, Task, TitlebarOptions, Window,
    WindowBounds, WindowControlArea, WindowHandle, WindowOptions, actions, div, img, point,
    prelude::*, px, relative, rgb, size,
};

use crate::{
    assets::{self, Icon},
    busy::Busy,
    document::{BinaryFile, Breakpoint, Document},
    document_store::{DocumentStore, DocumentStoreEvent},
    branch_picker::{BranchPicker, BranchPickerEvent},
    clone_dialog::{self, CloneDialog, CloneDialogEvent},
    context_menu::{ContextMenu, ContextMenuEvent, MenuEntry},
    input_dialog::{InputDialog, InputDialogEvent},
    encoding::TextEncoding,
    watcher::{self, FsChange, GitPath, Watcher},
    onboarding::{Onboarding, OnboardingEvent},
    diff_view::{DiffSource, DiffView, DiffViewEvent},
    push_dialog::{PushDialog, PushDialogEvent},
    remotes_dialog::{RemotesDialog, RemotesEvent},
    git_log::{GitLog, GitLogEvent},
    debugger::{DebugEvent, DebugSession, launch},
    extensions,
    editor::{
        self, Cancel, Editor, EditorBreakpointMenu, EditorBreakpointsChanged, EditorContextMenu, EditorEvent, EditorGitEvent, EditorLoaded,
        EditorLspEvent, EditorPreviewToggle, EditorRunEvent, GitFile, ToggleMarkdownPreview, Unloaded,
    },
    image_view::ImageView,
    markdown_preview::{MarkdownPreview, PreviewEvent},
    pane_item::{PaneItem, Saving, TabIcon, TabLook},
    git::{self, Change, FileStatus, RepoStatus},
    git_panel::{GitOp, GitPanel, GitPanelEvent},
    problems_panel::{ProblemsEvent, ProblemsPanel},
    results_panel::{ResultsEvent, ResultsPanel},
    project::{Applied, ProjectSnapshot, project_ignore},
    project_tree::{ProjectTree, TreeEvent},
    find_bar::{FindBar, FindBarEvent},
    keymap,
    lint::Severity,
    lsp::{
        FileChange, LspEvent, LspStatus, LspStore, Server,
        protocol::{self, CodeAction, Encoding, Location, Position},
    },
    picker::{ActionEntry, GoToLine as GoToLineModal, GoToLineEvent, LocationRow, Picker, PickerEvent, PickerMode},
    project_search::{ProjectSearch, ProjectSearchEvent},
    search::{SearchQuery},
    state::{
        FileBreakpoints, PaneNode, PaneShare, ProjectSession, SessionState, TabKind, TabSession, TerminalGroupSession, TerminalLayout, TerminalTabSession,
        WindowPlacement,
    },
    syntax::Language,
    settings::{self, AutoSave, Settings, StartupProblems},
    run::{self, DebugSpec, RunTarget},
    terminal::{RunState, TerminalEvent, TerminalView},
    theme::{self, Theme},
    preferences,
    theme_picker::{ThemePicker, ThemePickerEvent},
    transition::{self, TransitionExt},
    ui::{Button, DragLabel, HoverFill, binding_label, icon_button, icon_el, kbd, switch, tooltip_view},
    updater,
};

use crate::ui::{
    GAP, area, chrome_bg, soft_shadow, file_icon, layout_gap,
};

mod app_menu;
mod background;
mod commands;
mod file_tree;
mod git_ops;
mod copilot;
mod language_servers;
mod line_endings;
mod local_history;
mod menus;
mod navigation;
mod pane;
mod pane_tree;
mod pane_view;
mod panes;
mod problems;
mod rendering;
mod running;
mod saving;
mod sessions;
mod sidebar;
mod status_bar;
mod tab_bar;
mod terminal_panel;
mod terminals;
mod title_bar;
mod watching;
mod welcome;
mod windows;
use self::{
    file_tree::*,
    pane::{Pane, PaneChrome, PaneEvent, file_drop_overlay},
    pane_tree::{Axis, Member, PaneTree, Side, SplitId, can_split, pane_toward},
    tab_bar::*,
    terminal_panel::{TerminalPanel, TerminalPanelEvent},
    title_bar::*,
    welcome::*,
    windows::*,
};
pub use self::{
    sessions::update_recent_projects,
    title_bar::{window_controls, window_drag_area},
    windows::{modal_max_height, open_window, placement_bounds, placement_of, quit},
};

actions!(
    workspace,
    [
        NewFile,
        NewWindow,
        CloseWindow,
        MinimizeWindow,
        ZoomWindow,
        OpenFile,
        OpenFolder,
        OpenFolderInThisWindow,
        RecentProjects,
        Save,
        SaveAs,
        CloseTab,
        NextTab,
        PrevTab,
        /// Ctrl+Tab: the tab used before this one (see `TabSwitch`).
        RecentTab,
        /// Ctrl+Shift+Tab: the same order, the other way.
        RecentTabBack,
        ToggleSidebar,
        ToggleSidebarPosition,
        /// Switches the window between the Modern and Classic layouts (`ui_style`).
        ToggleClassicLayout,
        ToggleProblems,
        ToggleTerminal,
        ToggleGitLog,
        ShowFileHistory,
        NewTerminal,
        CloseTerminal,
        SplitRight,
        SplitDown,
        NextPane,
        PreviousPane,
        /// The pane on that side of the current one.
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        RunConfiguration,
        Rerun,
        RerunFailedTests,
        StopRun,
        DebugConfiguration,
        AttachToProcess,
        AddConditionalBreakpoint,
        AddLogpoint,
        OpenExtensions,
        ProjectTrust,
        UndoFileChange,
        RedoFileChange,
        /// Vim's `:` command line.
        VimCommandLine,
        ReloadExtensions,
        Resume,
        PauseProgram,
        StepOver,
        StepInto,
        StepOut,
        ToggleDebugPanel,
        CloseOtherTabs,
        CloseAllTabs,
        TogglePinTab,
        Quit,
        Find,
        Replace,
        FindNext,
        FindPrevious,
        FindInFiles,
        ReplaceInFiles,
        SearchEverywhere,
        GoToFile,
        RecentFiles,
        FindAction,
        GoToLine,
        FileStructure,
        OpenSettings,
        OpenSettingsFile,
        OpenProjectSettings,
        ToggleResults,
        ShowLocalHistory,
        NextResult,
        PreviousResult,
        OpenKeymap,
        RestartLanguageServer,
        CopilotSignIn,
        CopilotSignOut,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        SelectTheme,
        OpenThemesFolder,
        OpenSnippetsFolder,
        ReloadThemes,
        ShowCommit,
        ToggleCommit,
        GitPush,
        GitPull,
        GitFetch,
        GitBranches,
        GitStash,
        ManageRemotes,
        GitClone,
        GitInit,
        OpenRemote,
        ToggleAutoSave,
        CheckForUpdates,
        WelcomeSetup,
        GoToSymbol,
        SplitTerminal,
        RenameTerminal,
        SelectOpenedFile,
        InstallShellCommand,
    ]
);

/// How many folders the welcome screen remembers.
const MAX_RECENT_PROJECTS: usize = 10;

/// Where a folder opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenIn {
    /// Replace this window's project (asking about unsaved edits first).
    ThisWindow,
    NewWindow,
    /// This window if it has no project yet, otherwise a new one.
    Auto,
}

/// Why the workspace is being closed down; decides what happens once
/// unsaved edits are dealt with.
#[derive(Clone)]
enum CloseIntent {
    CloseWindow,
    Quit,
    /// Close everything and open this folder in the same window.
    ReplaceProject(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Files,
    Commit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Success,
    /// Neutral news ("No usages found").
    Info,
    Error,
}

struct Toast {
    id: usize,
    kind: ToastKind,
    message: SharedString,
    detail: Option<SharedString>,
    /// A button for what the toast reports (Undo, Save All).
    button: Option<ToastButton>,
    /// Dismissed, fading out before it's removed.
    leaving: bool,
}

/// What a toast's button does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastButton {
    /// Undo a file operation, by its id in the file history; shown only
    /// while it's still the one ⌘Z in the tree would undo.
    UndoFile(u64),
    /// Save the files changed without a tab; shown while there are any.
    SaveBackground,
    /// The last test run's results, in the Find panel.
    ShowTestResults,
}

impl ToastButton {
    fn label(self) -> &'static str {
        match self {
            ToastButton::UndoFile(_) => "Undo",
            ToastButton::SaveBackground => "Save All",
            ToastButton::ShowTestResults => "Show Results",
        }
    }
}

/// Asks whether to go ahead with `action`, resolving to `true` if so. One
/// that can't be undone (`undoable` false) lists Cancel first, which makes it
/// the default button on Windows and on macOS leaves no button on Return
/// (Escape cancels): Return never confirms it.
fn ask(window: &mut Window, message: &str, detail: &str, action: &str, undoable: bool, cx: &mut App) -> impl Future<Output = bool> + use<> {
    let answers = if undoable { [action, "Cancel"] } else { ["Cancel", action] };
    let rx = window.prompt(PromptLevel::Warning, message, Some(detail), &answers, cx);
    let yes = usize::from(!undoable);
    async move { rx.await == Ok(yes) }
}

/// Where deleted files go, as the platform calls it.
fn trash_name() -> &'static str {
    if cfg!(windows) { "Recycle Bin" } else { "Trash" }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// The Problems panel's top edge.
    Problems,
    /// The file tree's right edge.
    Sidebar,
    /// The boundary after member `ix` of a split of the editor area.
    Split { split: SplitId, axis: Axis, ix: usize },
}

#[derive(Clone, Copy)]
struct Resize {
    edge: Edge,
    /// Pointer position along the drag axis when the drag started.
    start: Pixels,
    /// Panel size when the drag started.
    size: Pixels,
}

/// Something the file-tree context menu can do.
#[derive(Clone)]
enum TreeAction {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Open(PathBuf),
    Rename(PathBuf),
    Duplicate(PathBuf),
    Delete(PathBuf),
    CopyPath(PathBuf),
    CopyRelativePath(PathBuf),
    Reveal(PathBuf),
    FindInFolder(PathBuf),
    CollapseAll,
    Git(GitOp),
    ShowDiff(FileStatus, bool),
    AddToGitignore(String),
    /// The commits touching this repository-relative path.
    ShowHistory(String),
    /// This file's Local History.
    LocalHistory(PathBuf),
    /// Asks for a name, then runs a Git operation with it.
    AskGit(GitPrompt),
    CopyText(String),
    UndoFileChange,
    RedoFileChange,
}

/// What an open input dialog will do with the name it collects.
enum PendingInput {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    /// Rename a symbol through its language server.
    RenameSymbol { server: Arc<Server>, path: PathBuf, position: Position, current: String },
    /// A Vim `:` command.
    VimCommand,
    Git(GitPrompt),
}

/// A name a Git operation needs.
#[derive(Clone)]
enum GitPrompt {
    RenameBranch(String),
    /// A new branch, or a tag, at a commit.
    BranchAt(String),
    TagAt(String),
    /// The stash's message (optional).
    Stash,
}

/// The panels the bottom area shows, one at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BottomPanel {
    Problems,
    /// The Find panel (`results_panel`).
    Results,
    Terminal,
    GitLog,
    Debug,
}

/// Ctrl+Tab going through a pane's tabs in the order they were last used,
/// while Ctrl (any modifier of the shortcut) is held: each press takes the
/// next one in the order the first press found.
struct TabSwitch {
    pane: EntityId,
    order: Vec<PaneItem>,
    at: usize,
    /// `recent` when it started: only the tab it ends on counts as used.
    recent: Vec<PathBuf>,
}

/// A pane's tabs, by index, in the order they were last used: the current
/// one, then the others by their files' places in `recent` (newest first),
/// then those it doesn't list (not shown yet, untitled) in pane order.
fn tabs_by_use(paths: &[Option<PathBuf>], active: usize, recent: &[PathBuf]) -> Vec<usize> {
    let rank = |ix: usize| {
        let used = paths[ix].as_ref().and_then(|path| recent.iter().position(|r| r == path));
        match used {
            _ if ix == active => (0, 0),
            Some(at) => (1, at),
            None => (2, ix),
        }
    };
    let mut order: Vec<usize> = (0..paths.len()).collect();
    order.sort_by_key(|&ix| rank(ix));
    order
}

/// Terminal tabs shown together in the bottom panel; splitting the terminal
/// adds a group beside it, like a pane.
struct TerminalGroup {
    tabs: Vec<Entity<TerminalView>>,
    active: usize,
}

impl TerminalGroup {
    fn new(terminal: Entity<TerminalView>) -> Self {
        Self { tabs: vec![terminal], active: 0 }
    }
}

/// Where `open_terminal` puts a new terminal.
#[derive(Clone, Copy)]
enum TerminalPlace {
    /// A tab in a group: at `ix` (clamped), or after its tabs.
    Tab { group: usize, ix: Option<usize> },
    /// A group of its own right after this one (a split).
    GroupAfter(usize),
}

/// A tab being dragged to a new place or pane.
#[derive(Clone)]
struct DraggedTab {
    item: PaneItem,
    name: SharedString,
}


/// Tab context menu commands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabAction {
    Close,
    CloseOthers,
    CloseAll,
    CloseToTheRight,
    TogglePin,
    SplitRight,
    SplitDown,
    SelectInTree,
    /// From the tab strip's overflow menu.
    Activate,
}

/// What a context menu item does.
enum MenuAction {
    Tree(TreeAction),
    /// An action dispatched to the focused editor.
    Run(Box<dyn gpui::Action>),
    /// A language server's fix or refactoring.
    CodeAction(Arc<Server>, CodeAction),
    Tab(TabAction, PaneItem),
    /// Status bar: save with these line endings (true: CRLF).
    LineEndings(bool, Entity<Editor>),
    Encoding(TextEncoding, Entity<Editor>),
    /// A gutter ▶'s Run (false) or Debug (true).
    Launch(RunTarget, bool),
    /// A line number's breakpoint menu.
    Breakpoint(BreakpointEdit, Entity<Editor>, usize),
    /// Status bar: the project's Python venv (None: detect).
    Venv(Option<crate::python::Choice>),
    /// Status bar: pick a venv folder.
    ChooseVenv,
    /// A terminal tab from its strip's overflow menu.
    Terminal(Entity<TerminalView>),
    /// A terminal tab's right-click menu.
    TerminalTab(TerminalTabAction, Entity<TerminalView>),
    /// Status bar: a file changed without a tab.
    OpenFile(PathBuf),
    /// Status bar: save the files changed without a tab.
    SaveBackground,
}

/// What a terminal tab's menu does with it.
#[derive(Clone, Copy, PartialEq)]
enum TerminalTabAction {
    Rename,
    ResetName,
    Clear,
    Rerun,
    Stop,
    New,
    Split,
    Close,
    CloseOthers,
    CloseToTheRight,
    CloseAll,
}

/// What the breakpoint menu does with a row's breakpoint.
#[derive(Clone, Copy, PartialEq)]
enum BreakpointEdit {
    Toggle,
    Condition,
    LogMessage,
}

impl Clone for MenuAction {
    fn clone(&self) -> Self {
        match self {
            MenuAction::Tree(action) => MenuAction::Tree(action.clone()),
            MenuAction::Run(action) => MenuAction::Run(action.boxed_clone()),
            MenuAction::CodeAction(server, action) => MenuAction::CodeAction(server.clone(), action.clone()),
            MenuAction::Tab(action, editor) => MenuAction::Tab(*action, editor.clone()),
            MenuAction::LineEndings(crlf, editor) => MenuAction::LineEndings(*crlf, editor.clone()),
            MenuAction::Encoding(encoding, editor) => MenuAction::Encoding(*encoding, editor.clone()),
            MenuAction::Launch(target, debug) => MenuAction::Launch(target.clone(), *debug),
            MenuAction::Breakpoint(edit, editor, row) => MenuAction::Breakpoint(*edit, editor.clone(), *row),
            MenuAction::Venv(choice) => MenuAction::Venv(choice.clone()),
            MenuAction::ChooseVenv => MenuAction::ChooseVenv,
            MenuAction::Terminal(terminal) => MenuAction::Terminal(terminal.clone()),
            MenuAction::TerminalTab(action, terminal) => MenuAction::TerminalTab(*action, terminal.clone()),
            MenuAction::OpenFile(path) => MenuAction::OpenFile(path.clone()),
            MenuAction::SaveBackground => MenuAction::SaveBackground,
        }
    }
}

struct OpenContextMenu {
    menu: Entity<ContextMenu>,
    position: gpui::Point<Pixels>,
    actions: Vec<Option<MenuAction>>,
    /// The tree entry that was right-clicked (highlighted while open).
    target: Option<PathBuf>,
    /// The Git panel row that was right-clicked: relative path, staged.
    git_target: Option<(String, bool)>,
    /// Which of the title bar's menus (☰, see `app_menu.rs`) this is.
    app_menu: Option<usize>,
    _subscription: Subscription,
}

enum Modal {
    Picker(Entity<Picker>),
    GoToLine(Entity<GoToLineModal>),
    ProjectSearch(Entity<ProjectSearch>),
    Theme(Entity<ThemePicker>),
    Branches(Entity<BranchPicker>),
    Input(Entity<InputDialog>),
    Clone(Entity<CloneDialog>),
    Push(Entity<PushDialog>),
    Remotes(Entity<RemotesDialog>),
    Trust(Entity<crate::trust_dialog::TrustDialog>),
}

/// The modal over the workspace, if any, with its subscription: they're
/// set and cleared together.
#[derive(Default)]
struct ModalLayer {
    current: Option<Modal>,
    _subscription: Option<Subscription>,
}

impl ModalLayer {
    /// Shows `modal` in place of any other; `subscription` lives as long.
    fn open(&mut self, modal: Modal, subscription: Subscription) {
        self.current = Some(modal);
        self._subscription = Some(subscription);
    }

    /// Closes the modal; returns it, if there was one.
    fn take(&mut self) -> Option<Modal> {
        self._subscription = None;
        self.current.take()
    }
}

/// The least room a pane keeps, splitting and resizing: its current tab, the
/// overflow button and the strip's ends across; the strip, breadcrumbs and a
/// few lines down.
const MIN_PANE_WIDTH: f32 = 240.0;
const MIN_PANE_HEIGHT: f32 = 140.0;
/// How long two Shift taps may be apart to count as a double Shift.
const DOUBLE_SHIFT_WINDOW: Duration = Duration::from_millis(400);
/// How long Save waits for format on save before saving unformatted.
const FORMAT_ON_SAVE_TIMEOUT: Duration = Duration::from_secs(3);

pub const TITLEBAR_HEIGHT: f32 = 40.0;
const STATUSBAR_HEIGHT: f32 = 30.0;
const TAB_HEIGHT: f32 = 38.0;
/// The editor tabs' strip on the chrome above each pane; the pane's island
/// starts 1px above its bottom, so the current tab covers the island's top
/// border and joins it.
const TAB_STRIP_HEIGHT: f32 = 38.0;
/// Room above the tabs in their strip.
const TAB_STRIP_TOP: f32 = 6.0;
const TAB_RADIUS: f32 = 10.0;
/// The concave corners joining the current tab to its island.
const TAB_FOOT: f32 = 8.0;
/// A tab strip's padding at each end: the island's corner radius plus a
/// foot, so the end tabs' feet land on the island's straight edge.
const TAB_STRIP_INSET: f32 = crate::ui::ISLAND_RADIUS + TAB_FOOT;
/// The sidebar's narrowest: room for its two tabs as icons (see
/// `render_sidebar_tabs`), and for a file tree that's still usable.
const SIDEBAR_MIN_WIDTH: f32 = 160.0;
/// A sidebar tab's padding, icon and gap around its name.
const SIDEBAR_TAB_CHROME: f32 = 44.0;
const _: () = assert!(SIDEBAR_MIN_WIDTH - 2. * TAB_STRIP_INSET >= 2. * TAB_ICON_WIDTH);

/// A terminal strip's buttons (+, split, hide), each with its margin.
const TERMINAL_BUTTON: f32 = 28.0;
/// Tab widths when they don't all fit (see `fit_tabs`).
/// Below this, a name is too short to read: inactive tabs show only their icon.
const TAB_NAMED_MIN: f32 = 96.0;
const TAB_ACTIVE_MIN: f32 = 140.0;
const TAB_ICON_WIDTH: f32 = 40.0;
/// The "⋯ N" button listing the tabs that don't fit.
const TAB_OVERFLOW_WIDTH: f32 = 48.0;
/// A tab's padding, icon, gaps and close button, around its name.
const TAB_CHROME: f32 = 63.0;
/// The error dot and its gap, when the file has errors.
const TAB_ERROR_DOT: f32 = 12.0;
/// A compact tab's padding, icon and gap: no close button (see `TabSlot::Compact`).
const TAB_COMPACT_CHROME: f32 = 39.0;
/// Narrower than this, a compact tab couldn't show a character and "…": icon only.
const TAB_COMPACT_MIN: f32 = 54.0;



pub struct Workspace {
    focus_handle: FocusHandle,
    file_history: crate::file_history::FileHistory,
    root: Option<PathBuf>,
    /// Every open text editor, in any pane (for saving, searching, language
    /// servers); panes hold them and the other kinds of tabs.
    editors: Vec<Entity<Editor>>,
    /// How the editor area is split into panes.
    center: PaneTree<Entity<Pane>>,
    /// The pane with the keyboard, or the one that had it last.
    active_pane: Entity<Pane>,
    /// What each pane reports (see `PaneEvent`), dropped with the pane.
    pane_subscriptions: HashMap<EntityId, Subscription>,
    show_sidebar: bool,
    /// What the bottom area shows; its height is `problems_height`.
    bottom: Option<BottomPanel>,
    /// Height of the bottom panel (Problems or Terminal; one shows at a time).
    problems_height: Pixels,

    /// For Rerun.
    last_run: Option<RunTarget>,
    /// Run tabs still running: their configurations' names and commands.
    /// Kept from the terminal panel's events, so drawing the run widget
    /// reads no terminal: one printing would redraw the window at every
    /// batch, its panel closed or its tab out of sight.
    running: Vec<(String, String)>,
    /// The debug session is on (not ended), kept from its events likewise.
    debug_active: bool,
    /// The debug session (running or finished) whose panel shows in the bottom area.
    debug: Option<(Entity<DebugSession>, Subscription)>,
    last_debug: Option<RunTarget>,
    welcome_scroll: gpui::ScrollHandle,
    /// A drag over the file tree targets the project folder: the Files tab
    /// lights up with the tree's island.
    tree_root_drop: bool,
    /// Where the debug session is paused (file, 1-based line), to mark it in that file's editor.
    debug_location: Option<(PathBuf, u32)>,
    /// Breakpoints of files that aren't open; open files keep theirs in their documents.
    breakpoints: HashMap<PathBuf, Vec<Breakpoint>>,
    /// The status bar's venv for a folder (looked up once, not every frame).
    venv_shown: std::cell::RefCell<Option<(PathBuf, Option<PathBuf>)>>,
    sidebar_width: Pixels,
    /// The panel edge being dragged, if any.
    resizing: Option<Resize>,
    window_title: String,
    find_bar: Entity<FindBar>,
    show_find: bool,
    modals: ModalLayer,
    /// All project files, refreshed in the background.
    /// The project's files (see `crate::project`), kept up to date from the watcher.
    project: Arc<ProjectSnapshot>,
    /// Scanning the project, or catching up with changes (one at a time).
    project_task: Option<Task<()>>,
    /// Changes waiting for the running update.
    project_changes: Vec<PathBuf>,
    /// The project folder was deleted (or moved) while open: reported once,
    /// its Git state dropped; detected again if it comes back.
    root_gone: bool,
    /// Most recently used files, newest first.
    recent: Vec<PathBuf>,
    /// Ctrl+Tab held down, going through the tabs.
    tab_switch: Option<TabSwitch>,
    actions: Arc<Vec<ActionEntry>>,
    shift_down: bool,
    last_shift_tap: Option<Instant>,
    sidebar_mode: SidebarMode,
    git_panel: Entity<GitPanel>,
    /// The Files view.
    tree: Entity<ProjectTree>,
    /// The shells in the bottom area.
    terminals: Entity<TerminalPanel>,
    /// The active file's problems and the last Run's.
    problems: Entity<ProblemsPanel>,
    /// The Find panel: Find Usages' results.
    results: Entity<ResultsPanel>,
    /// Whether it has any, for the title bar's toggle (the root render
    /// doesn't read the panel, which notifies as its selection moves).
    has_results: bool,
    /// The last status of each test run here, for the gutter's marks
    /// (given to every editor), and the last test run, for Rerun Failed.
    test_statuses: Arc<crate::test_run::Statuses>,
    last_tests: Option<(RunTarget, crate::test_run::TestRun)>,
    git_log: Entity<GitLog>,
    /// The diff tab opened from the Push dialog, and the dialog: it comes back
    /// when that tab closes while still the current one.
    diff_return: Option<(gpui::EntityId, Entity<PushDialog>)>,
    /// Working-tree root of the project's Git repository.
    repo: Option<PathBuf>,
    git_status: Arc<RepoStatus>,
    /// Change per file (and "contains changes" per folder) for the file tree.
    git_paths: Arc<HashMap<PathBuf, Change>>,
    /// What Git ignores (files, and folders as a whole), dimmed in the tree.
    git_ignored: Arc<HashSet<PathBuf>>,
    git_user: Option<String>,
    /// URL of the repository's `origin` (or first) remote.
    git_remote: Option<String>,
    last_commit: Option<git::CommitInfo>,
    /// Project folders opened before, newest first.
    recent_projects: Vec<PathBuf>,
    /// The running Git operation (one at a time), shown in the status bar.
    git_busy: Busy,
    /// Operations started while another ran: they run next, in order (Git
    /// takes one at a time).
    git_queue: std::collections::VecDeque<GitOp>,
    status_task: Option<Task<()>>,
    git_op_task: Option<Task<()>>,
    toasts: Vec<Toast>,
    next_toast: usize,
    context_menu: Option<OpenContextMenu>,
    /// Where the title bar's menu names are while one is open, by menu.
    app_menu_names: std::rc::Rc<std::cell::RefCell<Vec<Bounds<Pixels>>>>,
    pending_input: Option<PendingInput>,
    auto_save: AutoSave,
    auto_save_delay: Duration,
    /// Pending `after_delay` saves, restarted by every edit.
    auto_save_tasks: HashMap<EntityId, Task<()>>,
    /// Tabs whose auto-save failure was already reported, to avoid repeats.
    auto_save_warned: HashSet<EntityId>,
    /// The user chose to discard unsaved changes, so closing goes ahead.
    close_confirmed: bool,
    /// Debounced write of the project session (see `save_session`).
    session_task: Option<Task<()>>,
    /// Each tab's real path (symlinks resolved), with the path it's for:
    /// worked out once, not per lookup (`tab_for`).
    real_paths: std::cell::RefCell<HashMap<EntityId, (PathBuf, Option<PathBuf>)>>,
    /// Usages being listed, while the files they're in are read.
    locations_task: Option<Task<()>>,
    /// Debounced write of `state.json` (layout, the window's place on
    /// screen: see `save_state`).
    state_task: Option<Task<()>>,
    /// Watches the project folder; `watch_task` applies what it reports.
    watcher: Option<Watcher>,
    watch_task: Option<Task<()>>,
    /// The project's .gitignore, to skip Git refreshes for ignored churn.
    ignore: Option<Arc<ignore::gitignore::Gitignore>>,
    /// Git status asked for while a read ran (`read_status`).
    git_pending: git_ops::StatusWork,
    /// The running Go to Symbol request; replaced as the query changes.
    symbol_task: Option<Task<()>>,
    /// Welcome Setup, shown in place of everything below the title bar.
    onboarding: Option<(Entity<Onboarding>, Subscription)>,
    /// The project's trust question, held back while Welcome Setup shows
    /// (showing it closes dialogs): asked once nothing covers the project,
    /// `ask_waiting_trust`.
    trust_waits: bool,
    /// Language servers for this window's files.
    lsp: Entity<LspStore>,
    /// This window's documents by path, with a tab or without (edited by a
    /// workspace edit or Replace in Files, and not saved yet).
    documents: Entity<DocumentStore>,
    _subscriptions: Vec<Subscription>,
    /// Each tab's subscriptions, dropped with the tab (they'd pile up).
    tab_subscriptions: HashMap<gpui::EntityId, Vec<Subscription>>,
}

/// A location's file for usage, symbol and hierarchy lists: relative to the
/// project, or, outside it (a library's sources), just its folder and name.
fn location_label(root: Option<&Path>, path: &Path) -> String {
    if let Some(rel) = root.and_then(|r| path.strip_prefix(r).ok()).filter(|r| !r.as_os_str().is_empty()) {
        return rel.display().to_string();
    }
    let mut parts: Vec<_> = path.iter().rev().take(2).map(|p| p.to_string_lossy()).collect();
    parts.reverse();
    format!("…/{}", parts.join("/"))
}

/// A path as the UI shows it: relative to the project folder `root` (the
/// folder's own name for the folder itself), else in full.
fn display_rel(root: Option<&Path>, path: &Path) -> String {
    match root.and_then(|r| path.strip_prefix(r).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => root.map(folder_name).unwrap_or_default(),
        Some(rel) => rel.display().to_string(),
        None => path.display().to_string(),
    }
}

impl Workspace {
    fn shows(&self, panel: BottomPanel) -> bool {
        self.bottom == Some(panel)
    }

    /// Closes `panel` if it's the one showing.
    fn hide_bottom(&mut self, panel: BottomPanel) {
        if self.shows(panel) {
            self.bottom = None;
        }
    }

    /// Shows `panel` in place of any other, or closes it if it's showing.
    fn toggle_bottom(&mut self, panel: BottomPanel) {
        self.bottom = if self.shows(panel) { None } else { Some(panel) };
    }

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let find_bar = cx.new(FindBar::new);
        let find_subscription = cx.subscribe_in(&find_bar, window, |ws, _, event: &FindBarEvent, window, cx| match event {
            FindBarEvent::Dismissed => {
                ws.show_find = false;
                ws.focus_active(window, cx);
                cx.notify();
            }
        });
        let state = SessionState::load();
        let git_panel = cx.new(GitPanel::new);
        let git_subscription = cx.subscribe_in(&git_panel, window, |ws, _, event: &GitPanelEvent, window, cx| match event {
            GitPanelEvent::Run(op) => ws.run_git(op.clone(), window, cx),
            GitPanelEvent::Stash => ws.git_stash(&GitStash, window, cx),
            GitPanelEvent::OpenDiff { file, staged } => ws.open_diff(file.clone(), *staged, window, cx),
            GitPanelEvent::OpenFile(path) => ws.open_path(path.clone(), window, cx),
            GitPanelEvent::Menu { file, staged, position } => ws.open_git_file_menu(file.clone(), *staged, *position, window, cx),
        });
        let tree = cx.new(ProjectTree::new);
        let tree_subscription = cx.subscribe_in(&tree, window, |ws, _, event: &TreeEvent, window, cx| match event {
            TreeEvent::Open { path, focus } => {
                ws.open_path(path.clone(), window, cx);
                // A click shows the file and leaves the keyboard in the tree (typing
                // searches it), unless opening it asked something first: the
                // question has the keyboard then.
                if !*focus && ws.active_item(cx).is_some_and(|item| item.focus_handle(cx).is_focused(window)) {
                    window.focus(&ws.tree.focus_handle(cx));
                }
            }
            TreeEvent::Menu { path, is_dir, position } => ws.open_tree_menu(path.clone(), *is_dir, *position, window, cx),
            TreeEvent::Move { from, into } => ws.move_entry(from.clone(), into.clone(), cx),
            TreeEvent::CopyIn { paths, into } => ws.drop_into_tree(paths.clone(), into.clone(), window, cx),
            TreeEvent::History { redo } => ws.step_file_history(*redo, true, window, cx),
            // With no editor open, the keyboard stays in the tree rather than going nowhere.
            TreeEvent::Leave => {
                if ws.active_item(cx).is_some() {
                    ws.focus_active(window, cx);
                }
            }
            TreeEvent::Toggled => ws.schedule_session_save(cx),
            TreeEvent::RootDrop(on) => {
                ws.tree_root_drop = *on;
                cx.notify();
            }
        });
        let terminals = cx.new(|_| TerminalPanel::new());
        let terminals_subscription = cx.subscribe_in(&terminals, window, |ws, _, event: &TerminalPanelEvent, window, cx| match event {
            TerminalPanelEvent::OpenFile { path, line, column } => ws.open_at(path.clone(), *line, *column, window, cx),
            TerminalPanelEvent::RunExited(terminal) => ws.collect_run_problems(terminal, cx),
            TerminalPanelEvent::RunsChanged => ws.update_running(cx),
            TerminalPanelEvent::Close(terminal) => ws.close_terminals(vec![terminal.clone()], window, cx),
            TerminalPanelEvent::Exited(terminal) => ws.remove_terminal(terminal, window, cx),
            TerminalPanelEvent::Failed(terminal, error) => ws.terminal_failed(terminal, error.clone(), window, cx),
            TerminalPanelEvent::TabMenu { terminal, position } => ws.open_terminal_tab_menu(terminal.clone(), *position, window, cx),
            TerminalPanelEvent::Overflow { terminals, position } => {
                let entries = terminals.iter().map(|(_, name)| MenuEntry::item(name.clone(), Icon::Terminal)).collect();
                let actions = terminals.iter().map(|(t, _)| Some(MenuAction::Terminal(t.clone()))).collect();
                ws.show_context_menu(entries, actions, *position, None, window, cx);
            }
            TerminalPanelEvent::New(group) => {
                ws.terminals.update(cx, |p, _| p.set_active_group(*group));
                ws.new_terminal(&NewTerminal, window, cx);
            }
            TerminalPanelEvent::Split(group) => {
                ws.terminals.update(cx, |p, _| p.set_active_group(*group));
                ws.split_terminal(&SplitTerminal, window, cx);
            }
            TerminalPanelEvent::Hide => {
                ws.hide_bottom(BottomPanel::Terminal);
                ws.focus_active(window, cx);
                ws.save_state(cx);
                ws.schedule_session_save(cx);
                cx.notify();
            }
            TerminalPanelEvent::Activated => ws.schedule_session_save(cx),
        });
        let problems = cx.new(|_| ProblemsPanel::new());
        let problems_subscription = cx.subscribe_in(&problems, window, |ws, _, event: &ProblemsEvent, window, cx| match event {
            ProblemsEvent::Open { path, line, column } => ws.open_at(path.clone(), Some(*line), *column, window, cx),
            ProblemsEvent::Close => ws.toggle_problems(&ToggleProblems, window, cx),
        });
        let results = cx.new(ResultsPanel::new);
        let results_subscription = cx.subscribe_in(&results, window, |ws, _, event: &ResultsEvent, window, cx| match event {
            ResultsEvent::Open { target, focus } => ws.open_result(target.clone(), *focus, window, cx),
            ResultsEvent::Leave => ws.focus_active(window, cx),
            ResultsEvent::Close => ws.hide_results(window, cx),
            ResultsEvent::Emptied => {
                ws.has_results = false;
                ws.hide_results(window, cx);
            }
        });
        let git_log = cx.new(GitLog::new);
        let log_subscription = cx.subscribe_in(&git_log, window, |ws, _, event: &GitLogEvent, window, cx| match event {
            GitLogEvent::OpenDiff(source) => {
                ws.show_diff(source.clone(), window, cx);
            }
            GitLogEvent::Menu { entry, position } => ws.open_commit_menu(entry.clone(), *position, window, cx),
            GitLogEvent::Close => {
                ws.hide_bottom(BottomPanel::GitLog);
                ws.focus_active(window, cx);
                ws.save_state(cx);
                cx.notify();
            }
        });
        // Keep Git state fresh: when the window regains focus, and every few seconds.
        // Leaving the window saves edits, like VS Code.
        let activation = cx.observe_window_activation(window, |ws, window, cx| {
            if window.is_window_active() {
                ws.check_config(cx);
                // The watcher reports changes to Git status as they come.
                if ws.watcher.is_none() {
                    ws.refresh_git(cx);
                }
                ws.reload_changed_files(cx);
                // Other windows may have opened projects meanwhile.
                ws.recent_projects = SessionState::load().recent_projects;
                cx.notify();
            } else if ws.auto_save != AutoSave::Off {
                ws.auto_save_all(false, cx);
            }
        });
        // Last chance to write edits when the app quits some other way
        // (Dock menu, logout).
        let quit = cx.on_app_quit(|ws, cx| {
            if ws.auto_save != AutoSave::Off {
                ws.auto_save_all(true, cx);
            }
            if let Err(err) = ws.save_session(cx) {
                eprintln!("couldn't save the session: {err}");
            }
            ws.lsp.update(cx, |s, _| s.shutdown_all());
            if let Some((session, _)) = ws.debug.take() {
                session.update(cx, |s, cx| s.stop(cx));
            }
            async {}
        });
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |ws, cx| ws.prepare_to_close(CloseIntent::CloseWindow, window, cx)).unwrap_or(true)
        });
        let lsp = cx.new(|_| LspStore::new());
        let lsp_subscription = cx.subscribe(&lsp, |ws, _, event: &LspEvent, cx| match event {
            LspEvent::ApplyEdit { key, request_id, encoding, edits } => {
                let applied = ws.apply_workspace_edit(edits.clone(), *encoding, cx);
                let (key, id) = (key.clone(), request_id.clone());
                cx.spawn(async move |ws, cx| {
                    let applied = applied.await;
                    ws.update(cx, |ws, cx| ws.lsp.read(cx).respond(&key, id, serde_json::json!({ "applied": applied }))).ok();
                })
                .detach();
            }
            LspEvent::Message(message) => ws.toast(ToastKind::Error, message.clone(), None, cx),
            LspEvent::Installed(command) => ws.toast(ToastKind::Success, format!("Installed {command}"), None, cx),
        });
        // A server was installed (here or in another window): start the missing ones.
        let servers_observer =
            cx.observe_global::<crate::lsp::install::ServersInstalled>(|ws, cx| ws.lsp.update(cx, |s, cx| s.retry_missing(cx)));
        let lsp_observer = cx.observe(&lsp, |_, _, cx| cx.notify());
        let documents = cx.new(|_| DocumentStore::new());
        let documents_subscription = cx.subscribe(&documents, |ws, _, event: &DocumentStoreEvent, cx| match event {
            DocumentStoreEvent::KeptChanged => {
                ws.schedule_session_save(cx);
                cx.notify();
            }
        });
        let settings = Settings::get(cx).clone();
        // Settings changed (saved settings.json): re-apply everywhere.
        let settings_observer = cx.observe_global::<Settings>(|ws, cx| {
            let settings = Settings::get(cx).clone();
            ws.auto_save = settings.auto_save.unwrap_or_default();
            ws.auto_save_delay = Duration::from_millis(settings.auto_save_delay.unwrap_or(1000).max(50));
            for tab in ws.editors.clone() {
                tab.update(cx, |e, cx| e.apply_settings(cx));
            }
            ws.lsp.update(cx, |s, cx| s.settings_changed(cx));
            cx.notify();
        });
        // An update finished downloading: show Restart to Update.
        let updater_observer = cx.observe_global::<updater::Updater>(|_, cx| cx.notify());
        // The Preferences window opened, closed or changed page: the title
        // bar's Settings and Extensions buttons light up for it.
        let preferences_observer = cx.observe_global::<preferences::Shown>(|_, cx| cx.notify());
        // Extensions reloaded, or a grammar finished building.
        let extensions_observer =
            cx.observe_global_in::<extensions::ExtensionsChanged>(window, |ws, window, cx| ws.extensions_changed(window, cx));
        // A folder was trusted or stopped being trusted (here or in another window).
        let trust_observer = cx.observe_global::<crate::trust::TrustChanged>(|ws, cx| ws.trust_changed(cx));
        // Extensions load before the first window: it asks for what they wait for.
        cx.defer_in(window, |ws, window, cx| ws.offer_wasi_sdk(window, cx));
        // Downloads run on other threads: redraw the status bar's progress
        // while there are any, and once when they start or end.
        cx.spawn_in(window, async move |this, cx| {
            let mut seen = crate::downloads::generation();
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let generation = crate::downloads::generation();
                if generation == seen && !crate::downloads::any() {
                    continue;
                }
                seen = generation;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Git status follows the project watcher (and window activation); a
        // poll is only the fallback for a folder that couldn't be watched.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(4)).await;
                let alive = this
                    .update_in(cx, |ws, window, cx| {
                        if window.is_window_active() && ws.watcher.is_none() {
                            ws.refresh_git(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
        let pane = cx.new(|_| Pane::new());
        let pane_subscriptions = HashMap::from([(pane.entity_id(), cx.subscribe_in(&pane, window, Self::on_pane_event))]);
        let mut this = Self {
            focus_handle,
            file_history: Default::default(),
            root: None,
            editors: Vec::new(),
            center: PaneTree::new(pane.clone()),
            active_pane: pane,
            pane_subscriptions,
            // Without a project there's nothing to browse; opening one shows
            // the sidebar as it was left (see `set_root`).
            show_sidebar: false,
            // One panel at a time; the terminal wins, as it always opened over the others.
            bottom: if state.show_terminal.unwrap_or(false) {
                Some(BottomPanel::Terminal)
            } else if state.show_problems.unwrap_or(false) {
                Some(BottomPanel::Problems)
            } else if state.show_git_log.unwrap_or(false) {
                Some(BottomPanel::GitLog)
            } else {
                None
            },
            // Closed unless it was open when the window last closed.

            last_run: None,
            running: Vec::new(),
            debug_active: false,
            debug: None,
            last_debug: None,
            welcome_scroll: gpui::ScrollHandle::new(),
            tree_root_drop: false,
            debug_location: None,
            breakpoints: HashMap::new(),
            venv_shown: Default::default(),
            problems_height: px(state.problems_height.unwrap_or(200.).clamp(90., 4000.)),
            sidebar_width: px(state.sidebar_width.unwrap_or(250.).clamp(SIDEBAR_MIN_WIDTH, 4000.)),
            resizing: None,
            window_title: String::new(),
            find_bar,
            show_find: false,
            modals: ModalLayer::default(),
            project: Arc::default(),
            project_task: None,
            project_changes: Vec::new(),
            root_gone: false,
            recent: Vec::new(),
            tab_switch: None,
            actions: Arc::new(keymap::action_entries()),
            shift_down: false,
            last_shift_tap: None,
            sidebar_mode: if state.sidebar_mode.as_deref() == Some("commit") { SidebarMode::Commit } else { SidebarMode::Files },
            git_panel,
            tree,
            terminals,
            problems,
            results,
            has_results: false,
            test_statuses: Arc::default(),
            last_tests: None,
            git_log,
            diff_return: None,
            repo: None,
            git_status: Arc::new(RepoStatus::default()),
            git_paths: Arc::new(HashMap::new()),
            git_ignored: Arc::default(),
            git_user: None,
            git_remote: None,
            last_commit: None,
            recent_projects: state.recent_projects.clone(),
            git_busy: Busy::default(),
            git_queue: Default::default(),
            status_task: None,
            git_op_task: None,
            toasts: Vec::new(),
            next_toast: 0,
            context_menu: None,
            app_menu_names: Default::default(),
            pending_input: None,
            auto_save: settings.auto_save.unwrap_or_default(),
            auto_save_delay: Duration::from_millis(settings.auto_save_delay.unwrap_or(1000).max(50)),
            auto_save_tasks: HashMap::new(),
            auto_save_warned: HashSet::new(),
            close_confirmed: false,
            session_task: None,
            real_paths: Default::default(),
            locations_task: None,
            state_task: None,
            onboarding: None,
            trust_waits: false,
            watcher: None,
            watch_task: None,
            ignore: None,
            git_pending: git_ops::StatusWork::default(),
            symbol_task: None,
            lsp,
            documents,
            tab_subscriptions: HashMap::new(),
            _subscriptions: vec![
                // Panes are told what they show of the workspace whenever it
                // changes (see `sync_panes`).
                cx.observe_self(|ws, cx| ws.sync_panes(cx)),
                find_subscription,
                git_subscription,
                log_subscription,
                problems_subscription,
                results_subscription,
                tree_subscription,
                terminals_subscription,
                activation,
                quit,
                settings_observer,
                updater_observer,
                preferences_observer,
                extensions_observer,
                trust_observer,
                lsp_subscription,
                documents_subscription,
                lsp_observer,
                servers_observer,
                cx.observe_window_bounds(window, |ws, window, cx| ws.remember_placement(window, cx)),
            ],
        };
        // Problems with settings.json / keymap.json found at startup.
        if let Some(problems) = cx.try_global::<StartupProblems>().map(|p| p.0.clone()) {
            cx.set_global(StartupProblems::default());
            this.report_config_problems(problems, cx);
        }
        this.sync_panes(cx);
        this
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((onboarding, _)) = &self.onboarding {
            window.focus(&onboarding.focus_handle(cx));
            return;
        }
        match self.active_item(cx) {
            Some(item) => window.focus(&item.focus_handle(cx)),
            None => window.focus(&self.focus_handle),
        }
    }

    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path_as(path, false, window, cx);
    }

    /// `binary`: a file that looks binary opens as text too (Open Anyway).
    fn open_path_as(&mut self, path: PathBuf, binary: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = crate::paths::canonicalize(&path).unwrap_or(path);
        if path.is_dir() {
            self.set_root(path, window, cx);
            return;
        }
        // Open already (a preview or diff of it doesn't count).
        let open = self.items(cx).into_iter().find(|t| t.is_file() && t.path(cx).as_deref() == Some(path.as_path()));
        if let Some(item) = open {
            self.activate_item(&item, window, cx);
            return;
        }
        // Open without a tab already (a workspace edit changed it): that one,
        // edits and undo history included.
        if let Some(doc) = self.documents.read(cx).get(&path, cx) {
            let editor = cx.new(|cx| Editor::with_document(doc, cx));
            self.add_tab(PaneItem::from(editor), window, cx);
            return;
        }
        let mut large = false;
        let opened = if crate::preview::is_image(&path) {
            std::fs::metadata(&path).map(|_| PaneItem::from(cx.new(|cx| ImageView::new(path.clone(), cx))))
        } else {
            let read = if binary { Document::open_anyway(&path) } else { Document::open(&path) };
            match read {
                Err(err) if BinaryFile::is(&err) => return self.ask_to_open_binary(path, window, cx),
                read => read.map(|doc| {
                    large = doc.large;
                    PaneItem::from(cx.new(|cx| Editor::new(doc, cx)))
                }),
            }
        };
        match opened {
            Ok(item) => {
                if large {
                    let detail = "Highlighting, linting and language servers are off for files this big.";
                    self.toast(ToastKind::Info, "Opened as plain text", Some(detail.into()), cx);
                }
                self.add_tab(item, window, cx);
            }
            Err(err) => {
                let message = format!("Couldn't open {}", path.display());
                let detail = err.to_string();
                let _ = window.prompt(PromptLevel::Critical, &message, Some(&detail), &["OK"], cx);
            }
        }
    }

    /// A file that looks binary opens as text only when asked to (Open
    /// Anyway), as in VS Code.
    fn ask_to_open_binary(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let message = format!("“{name}” looks like a binary file");
        let detail = "As text it may not show correctly, and saving it can damage it.";
        let answer = window.prompt(PromptLevel::Warning, &message, Some(detail), &["Open Anyway", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |ws, window, cx| ws.open_path_as(path, true, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Opens a file at a 1-based line and column (from terminal output).
    fn open_at(&mut self, path: PathBuf, line: Option<u32>, column: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let path = crate::paths::canonicalize(&path).unwrap_or(path);
        self.open_path(path.clone(), window, cx);
        let (Some(line), Some(editor)) = (line, self.tab_for(&path, cx)) else { return };
        editor.update(cx, |e, cx| {
            let row = (line as usize).saturating_sub(1).min(e.doc.read(cx).line_count().saturating_sub(1));
            let start = e.doc.read(cx).line_start(row);
            let text = e.doc.read(cx).line_text(row);
            let col = column.map_or(0, |c| c as usize).saturating_sub(1);
            let offset = start + text.char_indices().nth(col).map_or(text.len(), |(i, _)| i);
            e.go_to(offset, window, cx);
        });
        if let Some(editor) = self.tab_for(&path, cx) {
            window.focus(&editor.focus_handle(cx));
        }
    }

    fn add_tab(&mut self, item: PaneItem, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.insert_tab(item, false, window, cx);
        let pane = self.active_pane.clone();
        self.activate_in(&pane, at, window, cx);
    }

    /// Adds a tab to the active pane, after its current tab, without showing
    /// it; returns its index. The pane keeps how its tabs look (titles, dirty
    /// dots, errors), so only a tab that's shown, or whose look changed,
    /// redraws anything.
    fn insert_tab(&mut self, item: PaneItem, pinned: bool, window: &mut Window, cx: &mut Context<Self>) -> usize {
        match &item {
            PaneItem::Editor(editor) => self.add_editor(editor, window, cx),
            PaneItem::Diff(diff) => self.add_diff(diff, window, cx),
            PaneItem::Preview(preview) => self.add_preview(preview, window, cx),
            PaneItem::Image(_) => {}
        }
        self.active_pane.update(cx, |pane, cx| {
            let at = pane.next_slot();
            pane.insert(item, at, pinned, cx)
        })
    }

    /// A new text tab: the workspace hears what it asks for and starts what
    /// its file needs.
    fn add_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(editor, |ws, editor, _: &EditorEvent, cx| {
            ws.schedule_auto_save(&editor, cx);
            ws.schedule_session_save(cx);
        }));
        subscriptions.push(cx.subscribe_in(editor, window, |ws, editor, event: &EditorContextMenu, window, cx| {
            ws.open_editor_menu(editor.clone(), event.0, window, cx);
        }));
        subscriptions.push(cx.subscribe_in(editor, window, |ws, _, event: &EditorLspEvent, window, cx| {
            ws.on_editor_lsp_event(event, window, cx);
        }));
        subscriptions.push(cx.subscribe_in(editor, window, |ws, _, event: &EditorRunEvent, window, cx| {
            ws.on_run_event(event, window, cx);
        }));
        subscriptions.push(cx.subscribe(editor, |ws, editor, _: &EditorBreakpointsChanged, cx| {
            ws.breakpoints_changed(&editor, cx);
        }));
        subscriptions.push(cx.subscribe_in(editor, window, |ws, editor, event: &EditorBreakpointMenu, window, cx| {
            ws.open_breakpoint_menu(editor.clone(), event.row, event.position, window, cx);
        }));
        subscriptions.push(cx.subscribe_in(editor, window, |ws, _, event: &EditorGitEvent, window, cx| {
            ws.run_git(event.0.clone(), window, cx);
        }));
        subscriptions.push(cx.subscribe_in(editor, window, |ws, editor, event: &EditorLoaded, window, cx| {
            ws.tab_loaded(editor, event, window, cx);
        }));
        subscriptions.push(cx.subscribe_in(editor, window, |ws, editor, _: &EditorPreviewToggle, window, cx| {
            ws.toggle_preview_of(editor, window, cx);
        }));
        // Dropped with the tab (see `remove_item`), so they don't pile up.
        self.tab_subscriptions.insert(editor.entity_id(), subscriptions);
        // A tab holds it now; the store finds it by its path.
        let doc = editor.read(cx).doc.clone();
        self.documents.update(cx, |s, cx| {
            s.add(&doc, cx);
            s.release(&doc, cx);
        });
        if editor.read(cx).is_loaded() {
            self.file_opened(editor, cx);
        }
        self.editors.push(editor.clone());
        // A preview of its document may be open (a split of the file).
        self.sync_previewed(cx);
    }

    /// The Run tabs still running, from the terminal panel (see `running`).
    fn update_running(&mut self, cx: &mut Context<Self>) {
        let running: Vec<(String, String)> = self
            .all_terminals(cx)
            .iter()
            .filter_map(|t| match t.read(cx).run() {
                Some((target, RunState::Running)) => Some((target.name.clone(), target.command.clone())),
                _ => None,
            })
            .collect();
        if running != self.running {
            self.running = running;
            cx.notify();
        }
    }

    /// What starts once a tab's text is there (when it opens, or when a
    /// restored tab is first shown): breakpoints, language server, Git changes.
    fn file_opened(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let statuses = self.test_statuses.clone();
        editor.update(cx, |e, cx| e.set_test_statuses(statuses, cx));
        // Another editor of the document has what's per document already
        // (breakpoints, the language server, the Git changes): this one shows them.
        if self.siblings(editor, cx).iter().any(|s| s.read(cx).is_loaded()) {
            self.lsp.update(cx, |s, cx| s.register(editor, cx));
            let doc = editor.read(cx).doc.clone();
            let diff = self.documents.update(cx, |s, cx| s.diff_for(&doc, cx));
            editor.update(cx, |e, cx| e.set_git_diff(diff, cx));
            return;
        }
        // Breakpoints it had while closed, and the paused line if it's here.
        let path = editor.read(cx).doc.read(cx).path.clone();
        if let Some(path) = path {
            if let Some(breakpoints) = self.breakpoints.remove(&path) {
                editor.update(cx, |e, cx| e.set_breakpoints(&breakpoints, cx));
            }
            if let Some((at, line)) = self.debug_location.clone().filter(|(at, _)| *at == path) {
                let _ = at;
                editor.update(cx, |e, cx| e.set_debug_line(Some(line.saturating_sub(1) as usize), cx));
            }
        }
        self.lsp.update(cx, |s, cx| s.register(editor, cx));
        if self.repo.is_none() && self.root.is_none() {
            // A lone file: use the repository it lives in, if any.
            if let Some(dir) = editor.read(cx).doc.read(cx).path.as_deref().and_then(Path::parent) {
                self.detect_repo(dir.to_path_buf(), cx);
            }
        } else {
            self.load_diff_base(editor, cx);
        }
    }

    /// A restored tab read its file (or couldn't: it closes).
    fn tab_loaded(&mut self, editor: &Entity<Editor>, event: &EditorLoaded, window: &mut Window, cx: &mut Context<Self>) {
        match &event.error {
            None => self.file_opened(editor, cx),
            Some(err) => {
                let name = editor.read(cx).doc.read(cx).file_name();
                self.toast(ToastKind::Error, format!("Couldn't open {name}"), Some(err.clone()), cx);
                self.remove_tab(editor, window, cx);
            }
        }
        cx.notify();
    }

    /// Reads the restored tabs that are showing: each pane's current one.
    /// Activation reads its tab already; this covers a pane whose current tab
    /// changed otherwise (a tab closed or moved out of it).
    fn load_shown_tabs(&mut self, cx: &mut Context<Self>) {
        let shown: Vec<PaneItem> = self.center.panes().iter().filter_map(|pane| pane.read(cx).active_item().cloned()).collect();
        for item in shown {
            self.load_item(&item, cx);
        }
    }

    /// Reads the file behind a tab if it's a restored one not read yet: an
    /// editor's, or the document a Markdown preview shows.
    fn load_item(&mut self, item: &PaneItem, cx: &mut Context<Self>) {
        match item {
            PaneItem::Editor(editor) => self.load_tab(editor, cx),
            PaneItem::Preview(preview) => {
                let doc = preview.read(cx).document().clone();
                if let Some(editor) = self.editors.iter().find(|e| e.read(cx).doc == doc).cloned() {
                    self.load_tab(&editor, cx);
                }
            }
            PaneItem::Image(_) | PaneItem::Diff(_) => {}
        }
    }

    /// Reads a restored tab's file; for a split of one, the tab holding the
    /// restored state reads it (the document is theirs together). A failure
    /// is reported (and the tab closed) through `EditorLoaded`.
    fn load_tab(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if editor.read(cx).is_loaded() {
            return;
        }
        let reader = match editor.read(cx).unloaded().is_some() {
            true => Some(editor.clone()),
            false => self.siblings(editor, cx).into_iter().find(|s| s.read(cx).unloaded().is_some()),
        };
        if let Some(reader) = reader {
            let _ = reader.update(cx, |e, cx| e.load(cx));
        }
    }

    /// The other tabs showing `editor`'s document.
    fn siblings(&self, editor: &Entity<Editor>, cx: &App) -> Vec<Entity<Editor>> {
        let e = editor.read(cx);
        self.editors.iter().filter(|t| *t != editor && t.read(cx).shares_document(e)).cloned().collect()
    }

    /// Shows an open editor in its pane and focuses it.
    fn activate_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((pane, ix)) = self.locate(editor.entity_id(), cx) {
            self.activate_in(&pane, ix, window, cx);
        }
    }

    /// Shows a tab in its pane and focuses it.
    fn activate_item(&mut self, item: &PaneItem, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((pane, ix)) = self.locate(item.entity_id(), cx) {
            self.activate_in(&pane, ix, window, cx);
        }
    }

    /// The pane holding a tab (by its view's id) and its index there.
    fn locate(&self, id: EntityId, cx: &App) -> Option<(Entity<Pane>, usize)> {
        self.center.panes().into_iter().find_map(|pane| pane.read(cx).position(id).map(|ix| (pane, ix)))
    }

    fn activate_in(&mut self, pane: &Entity<Pane>, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = pane.read(cx).tabs().get(ix).cloned() {
            if let Some(previous) = self.active_editor(cx)
                && Some(&previous) != item.editor()
                && matches!(self.auto_save, AutoSave::AfterDelay | AutoSave::OnFocusChange)
            {
                self.auto_save(&previous, cx);
            }
            // Another tab: the diff opened from the Push dialog no longer goes back to it.
            if self.diff_return.as_ref().is_some_and(|(id, _)| *id != item.entity_id()) {
                self.diff_return = None;
            }
            self.active_pane = pane.clone();
            pane.update(cx, |p, cx| p.set_active(ix, cx));
            // A restored tab reads its file when first shown.
            self.load_item(&item, cx);
            window.focus(&item.focus_handle(cx));
            // A preview or diff is of a file (opened already for a preview).
            if let Some(path) = item.path(cx).filter(|_| item.is_file()) {
                self.recent.retain(|p| p != &path);
                self.recent.insert(0, path);
                self.recent.truncate(50);
            }
            if self.show_find {
                self.find_bar.update(cx, |f, cx| f.set_target(item.editor().cloned(), cx));
            }
            self.schedule_session_save(cx);
        }
        cx.notify();
    }

    /// The current tab of the current pane.
    fn active_item(&self, cx: &App) -> Option<PaneItem> {
        self.active_pane.read(cx).active_item().cloned()
    }

    /// The current tab, if it's a text editor.
    fn active_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.active_item(cx)?.editor().cloned()
    }

    /// Every tab, in pane order.
    fn items(&self, cx: &App) -> Vec<PaneItem> {
        self.center.panes().iter().flat_map(|pane| pane.read(cx).tabs().iter().cloned()).collect()
    }

    /// The image tabs.
    fn images(&self, cx: &App) -> Vec<Entity<ImageView>> {
        self.items(cx).into_iter().filter_map(|t| t.image().cloned()).collect()
    }

    /// A new pane, without tabs or a place yet; the workspace hears what it
    /// reports.
    fn new_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Pane> {
        let pane = cx.new(|_| Pane::new());
        self.pane_subscriptions.insert(pane.entity_id(), cx.subscribe_in(&pane, window, Self::on_pane_event));
        pane
    }

    /// What a pane asks for (see `PaneEvent`).
    fn on_pane_event(&mut self, pane: &Entity<Pane>, event: &PaneEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PaneEvent::Activate(ix) => self.activate_in(pane, *ix, window, cx),
            PaneEvent::Focus => {
                self.active_pane = pane.clone();
                if pane.read(cx).is_empty() {
                    // Nothing to focus: the workspace takes the keyboard, so
                    // it doesn't stay in the pane left.
                    window.focus(&self.focus_handle);
                    cx.notify();
                } else {
                    let ix = pane.read(cx).active_ix();
                    self.activate_in(pane, ix, window, cx);
                }
            }
            PaneEvent::Tab(action, item) => self.run_tab_action(*action, item, window, cx),
            PaneEvent::Menu { item, position } => self.open_tab_menu(item.clone(), *position, window, cx),
            PaneEvent::Overflow { items, position } => {
                let entries = items.iter().map(|t| MenuEntry::item(t.tab_title(cx), Icon::File)).collect();
                let actions = items.iter().map(|t| Some(MenuAction::Tab(TabAction::Activate, t.clone()))).collect();
                self.show_context_menu(entries, actions, *position, None, window, cx);
            }
            PaneEvent::Drop { item, before } => self.move_tab(item, pane, *before, window, cx),
            PaneEvent::DropSplit { item, side } => self.move_to_new_pane(item, pane, *side, window, cx),
            PaneEvent::OpenPaths(paths) => {
                self.active_pane = pane.clone();
                self.open_dropped(paths, window, cx);
                cx.notify();
            }
            PaneEvent::OpenPath(path) => {
                self.active_pane = pane.clone();
                self.open_path(path.clone(), window, cx);
                cx.notify();
            }
        }
    }

    /// Tells each pane what it shows of the workspace (see `PaneChrome`).
    /// Runs after every notify of the workspace, outside drawing: a cached
    /// pane told during the workspace's render wouldn't be redrawn.
    fn sync_panes(&mut self, cx: &mut Context<Self>) {
        let panes = self.center.panes();
        for pane in &panes {
            let active = *pane == self.active_pane;
            let chrome = PaneChrome {
                active,
                outlined: active && panes.len() > 1,
                at_top: self.center.at_top(pane),
                find_bar: (active && self.show_find).then(|| self.find_bar.clone()),
                root: self.root.clone(),
            };
            pane.update(cx, |p, cx| p.set_chrome(chrome, cx));
        }
    }
}


/// A child view (editor, terminal, panel) drawn from its last layout until it
/// notifies, so a caret blink or terminal output doesn't lay out the whole
/// window again. It fills its parent. State it draws must reach it through
/// its own `cx.notify()`; focus changes and theme switches refresh the window.
fn cached<V: Render>(view: &Entity<V>) -> AnyView {
    cached_view(AnyView::from(view.clone()))
}

/// [`cached`] for a view of any kind (a pane's tab).
fn cached_view(view: AnyView) -> AnyView {
    view.cached(StyleRefinement::default().size_full())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The platform's name for showing a file in the file manager.
fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(windows) {
        "Show in Explorer"
    } else {
        "Open Containing Folder"
    }
}

/// A tooltip naming the action and its current shortcut.
fn status_tooltip(label: &str, action: &dyn gpui::Action, window: &mut Window, cx: &mut App) -> gpui::AnyView {
    tooltip_view(with_keys(label, binding_label(action, window).as_deref()), cx)
}

/// A tooltip's label with the shortcut for what it does, if there is one.
fn with_keys(label: &str, keys: Option<&str>) -> String {
    match keys {
        Some(keys) => format!("{label} ({keys})"),
        None => label.to_string(),
    }
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A small all-caps heading above a welcome-screen section.
fn section_label(text: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .px_1()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_dim)
        .child(text.to_uppercase())
}

/// A rounded panel grouping welcome-screen rows.
fn card(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(12.))
        .bg(theme.panel)
        .border_1()
        .border_color(theme.border_subtle)
        .overflow_hidden()
}

/// `text` cut to `max` characters with a trailing "…". GPUI's own truncation
/// doesn't apply in the title bar: a flex row measures text at full width
/// first, and the text keeps that measurement.
fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    // "git.…" would read as four dots.
    format!("{}…", kept.trim_end().trim_end_matches('.'))
}

#[cfg(test)]
mod tests {
    use super::ellipsize;

    #[test]
    fn tabs_go_in_the_order_they_were_used() {
        use std::path::PathBuf;
        let path = |name: &str| Some(PathBuf::from(format!("/p/{name}")));
        let paths = [path("a"), path("b"), path("c"), None, path("d")];
        let recent: Vec<PathBuf> = ["c", "a", "x", "b"].iter().map(|n| PathBuf::from(format!("/p/{n}"))).collect();
        // The current tab, then the last used before it; the untitled and
        // the never shown come last.
        assert_eq!(super::tabs_by_use(&paths, 2, &recent), [2, 0, 1, 3, 4]);
        assert_eq!(super::tabs_by_use(&paths, 3, &recent), [3, 2, 0, 1, 4]);
        assert_eq!(super::tabs_by_use(&[], 0, &recent), Vec::<usize>::new());
    }

    #[cfg(unix)]
    #[test]
    fn linked_folders_are_folders_in_the_tree() {
        use std::path::Path;

        use crate::project_tree::children as tree_children;

        let dir = std::env::temp_dir().join(format!("kuana-tree-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real/inner")).unwrap();
        std::fs::write(dir.join("file.txt"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("linked")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("real/inner/up")).unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("broken")).unwrap();

        let entries = |d: &Path| tree_children(d).into_iter().map(|(_, dir, cycle, name)| (name, dir, cycle)).collect::<Vec<_>>();
        let top = entries(&dir);
        assert_eq!(top, [("linked".into(), true, false), ("real".into(), true, false), ("broken".into(), false, false), ("file.txt".into(), false, false)]);
        // A link back up to a folder around it doesn't expand.
        assert_eq!(entries(&dir.join("linked/inner")), [("up".into(), true, true)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ellipsize_cuts_long_names_only() {
        assert_eq!(ellipsize("main", 12), "main");
        assert_eq!(ellipsize("exactly-twelve", 14), "exactly-twelve");
        assert_eq!(ellipsize("feature/an-extremely-long-branch", 12), "feature/an-…");
        // A cut right after a space doesn't leave it before the ellipsis.
        assert_eq!(ellipsize("my long project", 4), "my…");
        assert_eq!(ellipsize("çok-uzun-isim", 5), "çok-…");
        assert_eq!(ellipsize("git.rs", 5), "git…");
    }

    #[test]
    fn tabs_fit_like_a_browser() {
        use super::{TAB_ACTIVE_MIN, TAB_ICON_WIDTH, TabSlot::*, fit_tabs};
        // They fit: as they are.
        assert_eq!(fit_tabs(&[100., 120.], 0, 400.), vec![Natural, Natural]);
        // Too wide: only the widest shrink, to a shared limit; short ones stay whole.
        assert_eq!(fit_tabs(&[300., 300., 300.], 1, 600.), vec![Fixed(200.), Fixed(200.), Fixed(200.)]);
        assert_eq!(fit_tabs(&[80., 300., 250.], 0, 500.), vec![Natural, Fixed(210.), Fixed(210.)]);
        assert_eq!(fit_tabs(&[80., 300., 150.], 0, 500.), vec![Natural, Fixed(270.), Natural]);
        // Crowded: the current tab keeps its name, the others drop their close
        // buttons and keep what they can of theirs.
        let slots = fit_tabs(&[150.; 6], 2, 500.);
        assert_eq!(slots[2], Fixed(TAB_ACTIVE_MIN));
        assert!(slots.iter().enumerate().all(|(i, s)| i == 2 || *s == Compact(72.)));
        // A short name stays whole there.
        assert_eq!(fit_tabs(&[90., 150., 150., 150.], 3, 360.)[0], Compact(66.));
        // Not even ~3 characters: icons.
        let slots = fit_tabs(&[150.; 8], 2, 450.);
        assert!(slots.iter().enumerate().all(|(i, s)| i == 2 || matches!(s, Icon(_))));
        // Too many even for icons: the farthest go to the menu, never the current one.
        let slots = fit_tabs(&[150.; 20], 19, 400.);
        assert_eq!(slots[19], Fixed(TAB_ACTIVE_MIN));
        assert!(matches!(slots[18], Icon(w) if w >= TAB_ICON_WIDTH));
        assert_eq!(slots[0], Hidden);
        let widths: f32 = slots
            .iter()
            .map(|s| match s {
                Fixed(w) | Compact(w) | Icon(w) => *w,
                _ => 0.,
            })
            .sum();
        assert!(widths + super::TAB_OVERFLOW_WIDTH <= 400.);
        // The overflow mode's tabs share the room: 1 fits beside the current one and
        // the menu, and gets all of it (enough for the start of its name).
        let slots = fit_tabs(&[88.; 7], 6, 209.);
        assert_eq!(slots[6], Fixed(88.));
        assert_eq!(slots[5], Compact(64.));
        assert_eq!(slots.iter().filter(|s| **s == Hidden).count(), 5);
    }
}
