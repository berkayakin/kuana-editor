//! Preferences (JetBrains' Settings, in a window of its own: `window.rs`): every option in `settings.json`
//! and the key bindings in `keymap.json`, on pages with a search across all of
//! them. It looks the same on every platform. Changes apply and save right
//! away; the files stay the source of truth and keep their comments.

mod keymap_page;
mod window;

pub use window::{Shown, open};

use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Corner, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, Pixels, Render, ScrollHandle, SharedString, Stateful, Subscription, Task, Window,
    anchored, canvas, div, point, prelude::*, px, uniform_list,
};
use serde_json::{Value, json};

use crate::{
    assets::Icon,
    busy::Busy,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp},
    extensions::{self, ExtensionInfo, ExtensionsChanged, registry},
    format,
    git::PullStrategy,
    lsp::servers,
    settings::{self, AutoSave, BaseKeymap, RunConfiguration, Settings, ShowWhitespace, SidebarDock, VisualEffects},
    syntax::Language,
    theme::{self, Appearance, Theme, ThemeRegistry},
    workspace::Find,
    ui::{
        Button, HoverFill,
        GAP, focus_glow, icon_button, icon_el, input_box, island, popover, switch, text_button,
        tooltip_view,
    },
};

use keymap_page::KeymapPage;

pub enum PreferencesEvent {
    /// Show `settings.json` / `keymap.json` in a tab (Preferences closes).
    OpenSettingsFile,
    OpenKeymapFile,
    /// Problems found while applying a change, for the workspace to report.
    Problems(Vec<String>),
    Dismissed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Appearance,
    Editor,
    Saving,
    Languages,
    Extensions,
    Keymap,
    Terminal,
    Git,
    Trust,
    Updates,
}

impl Page {
    const ALL: [Page; 10] = [
        Page::Appearance,
        Page::Editor,
        Page::Saving,
        Page::Languages,
        Page::Extensions,
        Page::Keymap,
        Page::Terminal,
        Page::Git,
        Page::Trust,
        Page::Updates,
    ];

    fn title(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Editor => "Editor",
            Page::Saving => "Saving",
            Page::Languages => "Languages",
            Page::Extensions => "Extensions",
            Page::Keymap => "Keymap",
            Page::Terminal => "Terminal & Run",
            Page::Git => "Git",
            Page::Trust => "Trust",
            Page::Updates => "Updates",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Page::Appearance => "Color themes for the whole window",
            Page::Editor => "Font, indentation and line wrapping",
            Page::Saving => "When files are saved and what saving does",
            Page::Languages => "Per-language options, language servers and formatters",
            Page::Extensions => "More languages, themes and snippets; Zed's extensions work too",
            Page::Keymap => "A base keymap to start from, and shortcuts for every action on top",
            Page::Terminal => "The terminal's shell and your own Run configurations",
            Page::Git => "How Update Project brings in other people's commits",
            Page::Trust => "Folders whose language servers, formatters and tools Kuana may run",
            Page::Updates => "Getting new versions of Kuana",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Page::Appearance => Icon::Palette,
            Page::Editor => Icon::Type,
            Page::Saving => Icon::Save,
            Page::Languages => Icon::Braces,
            Page::Extensions => Icon::Puzzle,
            Page::Keymap => Icon::Keyboard,
            Page::Terminal => Icon::Terminal,
            Page::Git => Icon::GitBranch,
            Page::Trust => Icon::ShieldCheck,
            Page::Updates => Icon::Download,
        }
    }
}

/// Options with a row of their own. The Languages and Keymap pages list
/// languages and actions instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Setting {
    Theme,
    IconTheme,
    VisualEffects,
    SidebarDock,
    BaseKeymap,
    FontFamily,
    FontSize,
    LineHeight,
    TabSize,
    HardTabs,
    SoftWrap,
    Rulers,
    ShowWhitespace,
    InlayHints,
    CodeLens,
    Minimap,
    MinimapWidth,
    VimMode,
    AutoSave,
    AutoSaveDelay,
    FormatOnSave,
    TrimWhitespace,
    FinalNewline,
    Shell,
    RunConfigurations,
    TrustAll,
    NativeGrammars,
    AutoUpdate,
    PullStrategy,
}

impl Setting {
    const ALL: [Setting; 29] = [
        Setting::Theme,
        Setting::IconTheme,
        Setting::VisualEffects,
        Setting::SidebarDock,
        Setting::BaseKeymap,
        Setting::FontFamily,
        Setting::FontSize,
        Setting::LineHeight,
        Setting::TabSize,
        Setting::HardTabs,
        Setting::SoftWrap,
        Setting::Rulers,
        Setting::ShowWhitespace,
        Setting::InlayHints,
        Setting::CodeLens,
        Setting::Minimap,
        Setting::MinimapWidth,
        Setting::VimMode,
        Setting::AutoSave,
        Setting::AutoSaveDelay,
        Setting::FormatOnSave,
        Setting::TrimWhitespace,
        Setting::FinalNewline,
        Setting::Shell,
        Setting::RunConfigurations,
        Setting::TrustAll,
        Setting::NativeGrammars,
        Setting::AutoUpdate,
        Setting::PullStrategy,
    ];

    fn page(self) -> Page {
        match self {
            Setting::Theme | Setting::IconTheme | Setting::VisualEffects | Setting::SidebarDock => Page::Appearance,
            Setting::BaseKeymap => Page::Keymap,
            Setting::FontFamily | Setting::FontSize | Setting::LineHeight => Page::Editor,
            Setting::TabSize | Setting::HardTabs | Setting::SoftWrap => Page::Editor,
            Setting::Rulers | Setting::ShowWhitespace | Setting::InlayHints | Setting::CodeLens | Setting::VimMode => Page::Editor,
            Setting::Minimap | Setting::MinimapWidth => Page::Editor,
            Setting::AutoSave
            | Setting::AutoSaveDelay
            | Setting::FormatOnSave
            | Setting::TrimWhitespace
            | Setting::FinalNewline => Page::Saving,
            Setting::Shell | Setting::RunConfigurations => Page::Terminal,
            Setting::TrustAll | Setting::NativeGrammars => Page::Trust,
            Setting::AutoUpdate => Page::Updates,
            Setting::PullStrategy => Page::Git,
        }
    }

    fn section(self) -> &'static str {
        match self {
            Setting::Theme => "Theme",
            Setting::IconTheme => "File icons",
            Setting::VisualEffects => "Effects",
            Setting::SidebarDock => "Layout",
            Setting::BaseKeymap => "Keymap",
            Setting::FontFamily | Setting::FontSize | Setting::LineHeight => "Font",
            Setting::TabSize | Setting::HardTabs => "Indentation",
            Setting::SoftWrap => "Wrapping",
            Setting::Rulers | Setting::ShowWhitespace => "Guides",
            Setting::InlayHints | Setting::CodeLens => "Hints",
            Setting::Minimap | Setting::MinimapWidth => "Minimap",
            Setting::VimMode => "Vim",
            Setting::AutoSave | Setting::AutoSaveDelay => "Auto-save",
            Setting::FormatOnSave | Setting::TrimWhitespace | Setting::FinalNewline => "On save",
            Setting::Shell => "Terminal",
            Setting::RunConfigurations => "Run configurations",
            Setting::TrustAll | Setting::NativeGrammars => "Restricted Mode",
            Setting::AutoUpdate => "Updates",
            Setting::PullStrategy => "Update Project",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Setting::Theme => "Theme",
            Setting::IconTheme => "Icon theme",
            Setting::VisualEffects => "Visual effects",
            Setting::SidebarDock => "Sidebar position",
            Setting::BaseKeymap => "Base keymap",
            Setting::FontFamily => "Family",
            Setting::FontSize => "Size",
            Setting::LineHeight => "Line height",
            Setting::TabSize => "Tab size",
            Setting::HardTabs => "Indent with tabs",
            Setting::SoftWrap => "Soft wrap",
            Setting::Rulers => "Rulers",
            Setting::ShowWhitespace => "Show whitespace",
            Setting::InlayHints => "Inlay hints",
            Setting::CodeLens => "Code lens",
            Setting::Minimap => "Show minimap",
            Setting::MinimapWidth => "Width",
            Setting::VimMode => "Vim mode",
            Setting::AutoSave => "Auto-save",
            Setting::AutoSaveDelay => "Delay",
            Setting::FormatOnSave => "Reformat on save",
            Setting::TrimWhitespace => "Trim trailing whitespace",
            Setting::FinalNewline => "Insert final newline",
            Setting::Shell => "Shell",
            Setting::RunConfigurations => "Run configurations",
            Setting::TrustAll => "Trust all folders",
            Setting::NativeGrammars => "Native extension grammars",
            Setting::AutoUpdate => "Update automatically",
            Setting::PullStrategy => "Pull strategy",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Setting::Theme => "Add your own themes as .json files in the themes folder.",
            Setting::IconTheme => "File and folder icons. Extensions can add icon themes (Zed's work too).",
            Setting::VisualEffects => "Reduced uses solid colors instead of gradients; Minimal also drops shadows, glows and animations.",
            Setting::SidebarDock => "Which side of the editor the Files and Commit views sit on.",
            Setting::BaseKeymap => "The shortcuts Kuana starts from. Your own changes below stay on top of any of them.",
            Setting::FontFamily => "Monospaced fonts installed on this computer.",
            Setting::FontSize => "Zoom (Increase / Decrease Font Size) adds to this until you reset it.",
            Setting::LineHeight => "A multiple of the font size.",
            Setting::TabSize => "Columns per indent level and per tab. A file's own indentation and .editorconfig come first.",
            Setting::HardTabs => "Use tab characters when a file doesn't show a style of its own.",
            Setting::SoftWrap => "Wrap long lines at the edge of the editor. Markdown and plain text wrap either way.",
            Setting::Rulers => "Columns for vertical guides, e.g. 80, 120. Empty uses .editorconfig's max_line_length.",
            Setting::ShowWhitespace => "Mark spaces with dots and tabs with lines.",
            Setting::InlayHints => "Types and parameter names from the language server, shown in the code.",
            Setting::CodeLens => "The language server's links above lines: references, implementations, Run and Debug.",
            Setting::Minimap => "The file in miniature at the editor's right edge, in place of the scrollbar. Drag it or click to jump.",
            Setting::MinimapWidth => "In pixels.",
            Setting::VimMode => "Vim's Normal, Insert and Visual modes. The status bar shows the mode.",
            Setting::AutoSave => "Every mode but Off also saves when Kuana quits or a tab closes.",
            Setting::AutoSaveDelay => "How long after you stop typing.",
            Setting::FormatOnSave => "Reformat Code when you Save; auto-save never reformats while you type.",
            Setting::TrimWhitespace => "Remove spaces at the ends of lines, except on lines with a caret.",
            Setting::FinalNewline => "End every file with a line break.",
            Setting::Shell => "Program and arguments for new terminals. Empty uses your login shell.",
            Setting::RunConfigurations => "Commands for Run… next to the detected ones (cargo, npm scripts, go, make, pytest).",
            Setting::TrustAll => "Run language servers, formatters and project tools in every folder without asking. Not recommended.",
            Setting::NativeGrammars => "Compile extensions' grammars with your C compiler instead of WebAssembly, which runs them sandboxed. Only for extensions in trusted folders.",
            Setting::AutoUpdate => "Download new versions from GitHub in the background. Restart to Update in the status bar installs them.",
            Setting::PullStrategy => "How incoming commits join yours once both branches have new ones. Until one is picked, your Git config's pull.rebase / pull.ff decide; without them, Update Project asks the first time.",
        }
    }

    /// More words people may search for.
    fn keywords(self) -> &'static str {
        match self {
            Setting::Theme => "color scheme dark light appearance",
            Setting::IconTheme => "icons file icons folder icon theme",
            Setting::VisualEffects => "reduce simple simplify plain flat minimal gradient shadow glow animation transition motion effects decoration",
            Setting::SidebarDock => "sidebar side dock left right position move layout file tree project panel git commit",
            Setting::BaseKeymap => "keymap shortcuts key bindings vscode vs code jetbrains intellij webstorm sublime zed",
            Setting::FontFamily => "typeface family monospace",
            Setting::FontSize => "font zoom text",
            Setting::LineHeight => "spacing leading font",
            Setting::TabSize => "indent width spaces",
            Setting::HardTabs => "indentation spaces hard tabs",
            Setting::SoftWrap => "word wrap lines",
            Setting::Rulers => "ruler guide column line length margin 80",
            Setting::ShowWhitespace => "whitespace spaces tabs invisible characters dots render",
            Setting::InlayHints => "inlay hints types parameters names language server",
            Setting::CodeLens => "code lens references implementations run debug language server",
            Setting::Minimap => "minimap overview scrollbar outline map document preview scroll",
            Setting::MinimapWidth => "minimap width size pixels",
            Setting::VimMode => "vim vi modal normal insert visual keys",
            Setting::AutoSave => "autosave save automatically",
            Setting::AutoSaveDelay => "autosave milliseconds idle",
            Setting::FormatOnSave => "format prettier formatter",
            Setting::TrimWhitespace => "spaces trailing whitespace",
            Setting::FinalNewline => "newline end of file eof",
            Setting::Shell => "terminal bash zsh fish powershell",
            Setting::RunConfigurations => "run tasks commands scripts",
            Setting::TrustAll => "trust security restricted mode workspace untrusted safe",
            Setting::NativeGrammars => "extensions grammars tree-sitter native compiler wasm webassembly sandbox",
            Setting::AutoUpdate => "update upgrade version release download",
            Setting::PullStrategy => "git pull update project merge rebase fast-forward ff only diverged branches version control vcs",
        }
    }

    fn matches(self, words: &[String]) -> bool {
        let haystack = format!(
            "{} {} {} {} {}",
            self.label(),
            self.description(),
            self.section(),
            self.page().title(),
            self.keywords()
        )
        .to_lowercase();
        words.iter().all(|w| haystack.contains(w.as_str()))
    }
}

/// Which dropdown is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    AutoSave,
    IconTheme,
    ShowWhitespace,
    BaseKeymap,
    FontFamily,
    VisualEffects,
    SidebarDock,
    PullStrategy,
    LanguageServer,
    Formatter,
}

impl Menu {
    /// How many menus there are (sizes `menu_anchors`); keep it the last variant + 1.
    const COUNT: usize = Menu::Formatter as usize + 1;
}

/// How a language picks its server or formatter (`language_servers` and
/// `formatters` in settings).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToolMode {
    Automatic,
    LanguageServer,
    Off,
    Custom,
}

impl ToolMode {
    fn of(value: Option<&Value>) -> ToolMode {
        match value {
            Some(Value::Bool(false)) => ToolMode::Off,
            Some(Value::Array(_)) => ToolMode::Custom,
            Some(Value::String(s)) if s == "language_server" => ToolMode::LanguageServer,
            _ => ToolMode::Automatic,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ToolMode::Automatic => "Automatic",
            ToolMode::LanguageServer => "Language server",
            ToolMode::Off => "Off",
            ToolMode::Custom => "Custom command",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Server,
    Formatter,
}

impl Tool {
    fn key(self) -> &'static str {
        match self {
            Tool::Server => "language_servers",
            Tool::Formatter => "formatters",
        }
    }
}

/// An editor option that languages can override (`languages.<Name>`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LanguageOption {
    TabSize,
    HardTabs,
    SoftWrap,
    TrimWhitespace,
    FinalNewline,
    FormatOnSave,
    SyntaxErrors,
}

impl LanguageOption {
    const ALL: [LanguageOption; 7] = [
        LanguageOption::TabSize,
        LanguageOption::HardTabs,
        LanguageOption::SoftWrap,
        LanguageOption::TrimWhitespace,
        LanguageOption::FinalNewline,
        LanguageOption::FormatOnSave,
        LanguageOption::SyntaxErrors,
    ];

    fn key(self) -> &'static str {
        match self {
            LanguageOption::TabSize => "tab_size",
            LanguageOption::HardTabs => "hard_tabs",
            LanguageOption::SoftWrap => "soft_wrap",
            LanguageOption::TrimWhitespace => "trim_trailing_whitespace",
            LanguageOption::FinalNewline => "insert_final_newline",
            LanguageOption::FormatOnSave => "format_on_save",
            LanguageOption::SyntaxErrors => "syntax_errors",
        }
    }

    fn label(self) -> &'static str {
        match self {
            LanguageOption::TabSize => "Tab size",
            LanguageOption::HardTabs => "Indent with tabs",
            LanguageOption::SoftWrap => "Soft wrap",
            LanguageOption::TrimWhitespace => "Trim trailing whitespace",
            LanguageOption::FinalNewline => "Insert final newline",
            LanguageOption::FormatOnSave => "Reformat on save",
            LanguageOption::SyntaxErrors => "Show syntax errors",
        }
    }

    fn is_set(self, s: &settings::EditorSettings) -> bool {
        match self {
            LanguageOption::TabSize => s.tab_size.is_some(),
            LanguageOption::HardTabs => s.hard_tabs.is_some(),
            LanguageOption::SoftWrap => s.soft_wrap.is_some(),
            LanguageOption::TrimWhitespace => s.trim_trailing_whitespace.is_some(),
            LanguageOption::FinalNewline => s.insert_final_newline.is_some(),
            LanguageOption::FormatOnSave => s.format_on_save.is_some(),
            LanguageOption::SyntaxErrors => s.syntax_errors.is_some(),
        }
    }

    fn clear(self, s: &mut settings::EditorSettings) {
        match self {
            LanguageOption::TabSize => s.tab_size = None,
            LanguageOption::HardTabs => s.hard_tabs = None,
            LanguageOption::SoftWrap => s.soft_wrap = None,
            LanguageOption::TrimWhitespace => s.trim_trailing_whitespace = None,
            LanguageOption::FinalNewline => s.insert_final_newline = None,
            LanguageOption::FormatOnSave => s.format_on_save = None,
            LanguageOption::SyntaxErrors => s.syntax_errors = None,
        }
    }

    fn flag(self, f: &settings::FileSettings) -> bool {
        match self {
            LanguageOption::TabSize => false,
            LanguageOption::HardTabs => f.hard_tabs,
            LanguageOption::SoftWrap => f.soft_wrap,
            LanguageOption::TrimWhitespace => f.trim_trailing_whitespace,
            LanguageOption::FinalNewline => f.insert_final_newline,
            LanguageOption::FormatOnSave => f.format_on_save,
            LanguageOption::SyntaxErrors => f.syntax_errors,
        }
    }
}

struct RunRow {
    name: Entity<Editor>,
    command: Entity<Editor>,
    cwd: Entity<Editor>,
}

const DEFAULT_TAB_SIZE: usize = 4;
const DEFAULT_AUTO_SAVE_DELAY: u64 = 1000;
const ROW_HEIGHT: f32 = 52.;

pub struct Preferences {
    focus_handle: FocusHandle,
    page: Page,
    search: Entity<Editor>,
    scroll: ScrollHandle,
    /// Whether this frame's page scrolls in `body` (so `scroll` tracks it and
    /// gets a scrollbar); the Keymap page scrolls on its own.
    body_shown: Cell<bool>,
    /// The window is too narrow for a control beside its row's text: rows
    /// stack it under (`setting_row`). Set each frame from the window's width.
    stacked: Cell<bool>,
    fonts_scroll: gpui::UniformListScrollHandle,
    menu: Option<Menu>,
    font_filter: Entity<Editor>,
    /// Monospaced fonts, found when the font menu first opens.
    fonts: Option<Arc<Vec<SharedString>>>,
    fonts_task: Option<Task<()>>,
    /// Extensions' icon theme names, read from their files once rather than
    /// every frame; cleared when extensions change.
    icon_themes: Option<Arc<Vec<String>>>,
    /// Zed's installed extensions not added here, read from their manifests
    /// in the background when the Extensions page opens or extensions
    /// change, not every frame (`zed_extensions`).
    zed_extensions: Option<Arc<Vec<(String, std::path::PathBuf)>>>,
    zed_extensions_task: Option<Task<()>>,
    language: Language,
    shell: Entity<Editor>,
    rulers: Entity<Editor>,
    server_command: Entity<Editor>,
    formatter_command: Entity<Editor>,
    /// "Custom command" was picked for this tool but there's no command yet.
    pending_custom: Option<Tool>,
    runs: Vec<RunRow>,
    keymap: Entity<KeymapPage>,
    /// Why `settings.json` can't be changed right now.
    file_error: Option<String>,
    /// Writes typed text a moment after typing stops.
    commit_task: Option<Task<()>>,
    /// Where each dropdown's button was last drawn (by `Menu`), and the open
    /// one's list for this frame.
    menu_anchors: [Rc<Cell<Bounds<Pixels>>>; Menu::COUNT],
    overlay: Option<AnyElement>,
    /// Extensions page: the Git URL to install from, and how the last install went.
    extension_url: Entity<Editor>,
    extension_status: Option<(bool, String)>,
    installing: bool,
    /// The registry's extensions: the search over them, the list (or why
    /// there's none) once fetched, the fetch running, and the extensions
    /// being installed from it.
    registry_query: Entity<Editor>,
    registry: Option<Result<Arc<Vec<registry::Listed>>, String>>,
    registry_busy: Busy,
    registry_fetch: Option<Task<()>>,
    registry_installing: Vec<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PreferencesEvent> for Preferences {}

impl Preferences {
    pub fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| Editor::single_line("Search settings", cx));
        window.focus(&search.focus_handle(cx));
        let font_filter = cx.new(|cx| Editor::single_line("Filter fonts", cx));
        let login_shell = std::env::var("SHELL").unwrap_or_else(|_| "login shell".into());
        let shell = cx.new(|cx| Editor::single_line(format!("{login_shell} (login shell)"), cx));
        let server_command = cx.new(|cx| Editor::single_line("Command and arguments", cx));
        let rulers = cx.new(|cx| Editor::single_line("None", cx));
        let formatter_command = cx.new(|cx| Editor::single_line("Command and arguments ({file} is the file's path)", cx));
        let keymap = cx.new(|cx| KeymapPage::new(window, cx));
        let extension_url = cx.new(|cx| Editor::single_line("Git URL of an extension (Zed extensions work too)", cx));
        let registry_query = cx.new(|cx| Editor::single_line("Search available extensions", cx));
        let mut subscriptions = vec![
            // A grammar finished building, an extension came or went.
            cx.observe_global::<ExtensionsChanged>(|this, cx| {
                this.icon_themes = None;
                (this.zed_extensions, this.zed_extensions_task) = (None, None);
                cx.notify();
            }),
            cx.observe_global::<crate::trust::TrustChanged>(|_, cx| cx.notify()),
            cx.subscribe(&search, |this, _, _: &EditorEvent, cx| {
                this.menu = None;
                this.scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            }),
            cx.subscribe(&font_filter, |_, _, _: &EditorEvent, cx| cx.notify()),
            cx.subscribe(&registry_query, |_, _, _: &EditorEvent, cx| cx.notify()),
            cx.subscribe(&keymap, |_, _, event: &PreferencesEvent, cx| cx.emit(clone_event(event))),
        ];
        for field in [&shell, &rulers, &server_command, &formatter_command] {
            subscriptions.push(cx.subscribe(field, |this, _, _: &EditorEvent, cx| this.schedule_commit(cx)));
        }
        let settings = Settings::get(cx).clone();
        set_field(&shell, &settings.terminal_shell.as_deref().map(join_command).unwrap_or_default(), cx);
        set_field(&rulers, &rulers_text(settings.editor.rulers.as_deref()), cx);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            page,
            search,
            scroll: ScrollHandle::new(),
            body_shown: Cell::new(false),
            stacked: Cell::new(false),
            fonts_scroll: gpui::UniformListScrollHandle::new(),
            menu: None,
            font_filter,
            fonts: None,
            fonts_task: None,
            icon_themes: None,
            zed_extensions: None,
            zed_extensions_task: None,
            language: Language::JavaScript,
            shell,
            rulers,
            server_command,
            formatter_command,
            pending_custom: None,
            runs: Vec::new(),
            keymap,
            file_error: settings_file_error(),
            commit_task: None,
            menu_anchors: Default::default(),
            overlay: None,
            extension_url,
            extension_status: None,
            installing: false,
            registry_query,
            registry: None,
            registry_busy: Busy::default(),
            registry_fetch: None,
            registry_installing: Vec::new(),
            _subscriptions: subscriptions,
        };
        if page == Page::Extensions {
            this.fetch_registry(cx);
        }
        for run in &settings.run_configurations {
            this.add_run_row(run, cx);
        }
        this.load_language_fields(cx);
        this
    }

    pub fn page(&self) -> Page {
        self.page
    }

    fn query_words(&self, cx: &App) -> Vec<String> {
        self.search.read(cx).text(cx).to_lowercase().split_whitespace().map(str::to_string).collect()
    }

    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        self.menu = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        if !self.search.read(cx).text(cx).is_empty() {
            self.search.update(cx, |e, cx| e.set_text("", cx));
        }
        if page == Page::Keymap {
            self.keymap.update(cx, |k, cx| k.focus_filter(window, cx));
        }
        if page == Page::Extensions {
            self.fetch_registry(cx);
            // Zed may have installed some meanwhile.
            (self.zed_extensions, self.zed_extensions_task) = (None, None);
        }
        cx.notify();
    }

    /// ↑/↓ in the search field walk the pages (when not searching).
    fn on_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.step_page(-1, window, cx);
    }

    fn on_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.step_page(1, window, cx);
    }

    fn step_page(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.focus_handle(cx).is_focused(window) || !self.search.read(cx).text(cx).is_empty() {
            cx.propagate();
            return;
        }
        let ix = Page::ALL.iter().position(|p| *p == self.page).unwrap_or(0) as isize + step;
        let page = Page::ALL[ix.clamp(0, Page::ALL.len() as isize - 1) as usize];
        self.show_page(page, window, cx);
        // Keep typing in the search field.
        window.focus(&self.search.focus_handle(cx));
    }

    fn on_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        } else if self.keymap.read(cx).is_recording() {
            self.keymap.update(cx, |k, cx| k.stop_recording(cx));
        } else {
            cx.emit(PreferencesEvent::Dismissed);
        }
    }

    // ---- Writing settings ---------------------------------------------------------

    /// Applies what was just written (or reports why it couldn't be).
    fn applied(&mut self, results: Vec<std::io::Result<()>>, cx: &mut Context<Self>) {
        let mut problems: Vec<String> = results
            .into_iter()
            .filter_map(Result::err)
            .map(|e| {
                let text = e.to_string();
                if text.starts_with("settings.json") { text } else { format!("settings.json: {text}") }
            })
            .collect();
        problems.extend(settings::reload_now(cx));
        self.file_error = settings_file_error();
        if !problems.is_empty() {
            cx.emit(PreferencesEvent::Problems(problems));
        }
        cx.notify();
    }

    fn write(&mut self, result: std::io::Result<()>, cx: &mut Context<Self>) {
        self.applied(vec![result], cx);
    }

    /// Sets a top-level key, or removes it when `value` is the default, so
    /// the file only lists what the user changed.
    fn set_or_default(&mut self, key: &str, value: Value, default: Value, cx: &mut Context<Self>) {
        let result = if value == default { Settings::remove(key) } else { Settings::set(key, value) };
        self.write(result, cx);
    }

    fn reset(&mut self, key: &str, cx: &mut Context<Self>) {
        if key == "visual_effects" {
            // The older switch would speak for it otherwise.
            self.write(Settings::set_visual_effects(VisualEffects::default()), cx);
            return;
        }
        self.write(Settings::remove(key), cx);
        if key == "rulers" {
            set_field(&self.rulers, "", cx);
        }
        if key == "editor_font_size" {
            theme::zoom(None, cx);
        }
    }

    fn choose_theme(&mut self, name: SharedString, cx: &mut Context<Self>) {
        theme::apply_theme(&name, cx);
        let result = if name.as_ref() == theme::DEFAULT_THEME && Settings::get(cx).theme.is_none() {
            Ok(())
        } else {
            Settings::set("theme", name.as_ref().into())
        };
        self.write(result, cx);
    }

    fn schedule_commit(&mut self, cx: &mut Context<Self>) {
        self.commit_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(450)).await;
            this.update(cx, |this, cx| this.commit_text_fields(cx)).ok();
        }));
    }

    /// Writes anything typed but not yet saved; called before closing.
    pub fn flush(&mut self, cx: &mut Context<Self>) {
        if self.commit_task.take().is_some() {
            self.commit_text_fields(cx);
        }
    }

    /// Saves the text fields (shell, commands, Run configurations) that
    /// differ from the settings.
    fn commit_text_fields(&mut self, cx: &mut Context<Self>) {
        self.commit_task = None;
        let settings = Settings::get(cx).clone();
        let mut results = Vec::new();

        let rulers: Vec<usize> = self.rulers.read(cx).text(cx).split([',', ' ']).filter_map(|c| c.trim().parse().ok()).collect();
        let rulers = (!rulers.is_empty()).then_some(rulers);
        if rulers != settings.editor.rulers {
            results.push(match &rulers {
                Some(rulers) => Settings::set("rulers", json!(rulers)),
                None => Settings::remove("rulers"),
            });
        }

        let shell = split_command(&self.shell.read(cx).text(cx));
        let shell = (!shell.is_empty()).then_some(shell);
        if shell != settings.terminal_shell {
            results.push(match shell {
                Some(shell) => Settings::set("terminal_shell", json!(shell)),
                None => Settings::remove("terminal_shell"),
            });
        }

        let language = self.language;
        for (tool, field) in [(Tool::Server, &self.server_command), (Tool::Formatter, &self.formatter_command)] {
            let configured = tool_value(&settings, tool, language);
            let custom = ToolMode::of(configured.as_ref()) == ToolMode::Custom || self.pending_custom == Some(tool);
            let command = split_command(&field.read(cx).text(cx));
            if custom && !command.is_empty() && configured != Some(json!(command)) {
                results.push(Settings::update_language_entry(tool.key(), language, |_| Some(json!(command))));
                if self.pending_custom == Some(tool) {
                    self.pending_custom = None;
                }
            }
        }

        let runs: Vec<RunConfiguration> = self
            .runs
            .iter()
            .filter_map(|row| {
                let command = row.command.read(cx).text(cx).trim().to_string();
                let name = row.name.read(cx).text(cx).trim().to_string();
                let cwd = row.cwd.read(cx).text(cx).trim().to_string();
                (!command.is_empty()).then(|| RunConfiguration {
                    name: if name.is_empty() { command.clone() } else { name },
                    command,
                    cwd: (!cwd.is_empty()).then_some(cwd),
                })
            })
            .collect();
        if runs != settings.run_configurations {
            results.push(if runs.is_empty() {
                Settings::remove("run_configurations")
            } else {
                let list: Vec<Value> = runs
                    .iter()
                    .map(|r| {
                        let mut entry = json!({ "name": r.name, "command": r.command });
                        if let Some(cwd) = &r.cwd {
                            entry["cwd"] = cwd.as_str().into();
                        }
                        entry
                    })
                    .collect();
                Settings::set("run_configurations", Value::Array(list))
            });
        }
        if !results.is_empty() {
            self.applied(results, cx);
        }
    }

    // ---- Fonts --------------------------------------------------------------------

    fn toggle_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu == Some(menu) {
            self.menu = None;
        } else {
            self.menu = Some(menu);
            if menu == Menu::FontFamily {
                self.font_filter.update(cx, |e, cx| e.set_text("", cx));
                window.focus(&self.font_filter.focus_handle(cx));
                self.load_fonts(cx);
            }
        }
        cx.notify();
    }

    fn load_fonts(&mut self, cx: &mut Context<Self>) {
        if self.fonts.is_some() || self.fonts_task.is_some() {
            return;
        }
        let text_system = cx.text_system().clone();
        self.fonts_task = Some(cx.spawn(async move |this, cx| {
            let fonts = cx.background_executor().spawn(async move { monospaced_fonts(&text_system) }).await;
            this.update(cx, |this, cx| {
                this.fonts = Some(Arc::new(fonts));
                cx.notify();
            })
            .ok();
        }));
    }

    fn choose_font(&mut self, family: Option<SharedString>, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let result = match family {
            Some(family) => Settings::set("editor_font_family", family.as_ref().into()),
            None => Settings::remove("editor_font_family"),
        };
        self.write(result, cx);
        window.focus(&self.search.focus_handle(cx));
    }

    // ---- Languages ----------------------------------------------------------------

    fn select_language(&mut self, language: Language, cx: &mut Context<Self>) {
        self.flush(cx);
        self.language = language;
        self.menu = None;
        self.pending_custom = None;
        self.load_language_fields(cx);
        cx.notify();
    }

    fn load_language_fields(&mut self, cx: &mut Context<Self>) {
        let settings = Settings::get(cx).clone();
        for (tool, field) in [(Tool::Server, self.server_command.clone()), (Tool::Formatter, self.formatter_command.clone())] {
            let command = match tool_value(&settings, tool, self.language) {
                Some(Value::Array(items)) => join_command(&strings(&items)),
                _ => String::new(),
            };
            set_field(&field, &command, cx);
        }
        // Setting the text counts as an edit; nothing to save.
        self.commit_task = None;
    }

    fn set_tool_mode(&mut self, tool: Tool, mode: ToolMode, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let language = self.language;
        let field = match tool {
            Tool::Server => self.server_command.clone(),
            Tool::Formatter => self.formatter_command.clone(),
        };
        let value = match mode {
            ToolMode::Automatic => None,
            ToolMode::Off => Some(Value::Bool(false)),
            ToolMode::LanguageServer => Some("language_server".into()),
            ToolMode::Custom => {
                // Start from the command Kuana would use, or an empty field to type into.
                let typed = split_command(&field.read(cx).text(cx));
                let command = if typed.is_empty() { default_command(tool, language) } else { typed };
                if command.is_empty() {
                    self.pending_custom = Some(tool);
                    window.focus(&field.focus_handle(cx));
                    cx.notify();
                    return;
                }
                set_field(&field, &join_command(&command), cx);
                self.commit_task = None;
                Some(json!(command))
            }
        };
        self.pending_custom = None;
        self.write(Settings::update_language_entry(tool.key(), language, |_| value), cx);
    }

    fn set_language_option(&mut self, option: LanguageOption, value: Value, cx: &mut Context<Self>) {
        let language = self.language;
        // Matching what the language gets anyway is no override at all.
        let mut without = Settings::get(cx).clone();
        if let Some(overrides) = without.languages.get_mut(language.name()) {
            option.clear(overrides);
        }
        let inherited = without.for_file(language, None);
        let same = match option {
            LanguageOption::TabSize => value.as_u64() == Some(inherited.tab_size as u64),
            _ => value.as_bool() == Some(option.flag(&inherited)),
        };
        let value = (!same).then_some(value);
        self.write(Settings::set_language_option(language, option.key(), value), cx);
    }

    // ---- Run configurations ---------------------------------------------------------

    fn add_run_row(&mut self, run: &RunConfiguration, cx: &mut Context<Self>) {
        let name = cx.new(|cx| Editor::single_line("Name", cx));
        let command = cx.new(|cx| Editor::single_line("Command", cx));
        let cwd = cx.new(|cx| Editor::single_line("Folder", cx));
        set_field(&name, &run.name, cx);
        set_field(&command, &run.command, cx);
        set_field(&cwd, run.cwd.as_deref().unwrap_or(""), cx);
        for field in [&name, &command, &cwd] {
            self._subscriptions.push(cx.subscribe(field, |this, _, _: &EditorEvent, cx| this.schedule_commit(cx)));
        }
        self.runs.push(RunRow { name, command, cwd });
        self.commit_task = None;
    }

    fn new_run_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_run_row(&RunConfiguration::default(), cx);
        if let Some(row) = self.runs.last() {
            window.focus(&row.name.focus_handle(cx));
        }
        cx.notify();
    }

    fn remove_run_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.runs.len() {
            self.runs.remove(ix);
            self.commit_text_fields(cx);
            cx.notify();
        }
    }
}

impl Focusable for Preferences {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---- Rendering -----------------------------------------------------------------------

impl Render for Preferences {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let words = self.query_words(cx);
        self.overlay = None;
        self.stacked.set(window.viewport_size().width < px(STACK_BELOW));

        self.body_shown.set(false);
        let content = if words.is_empty() {
            self.render_page(window, cx)
        } else {
            self.render_results(&words, window, cx)
        };
        let content = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(content)
            .when(self.body_shown.get(), |el| el.child(crate::ui::scrollbar("pref-body-scrollbar", &self.scroll, cx)));

        // Fills its window (see `window.rs`), on the window's chrome.
        let panel = div()
            .id("preferences")
            .flex()
            .key_context("Preferences")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(|this, _: &Find, window, cx| {
                window.focus(&this.search.focus_handle(cx));
                this.search.update(cx, |e, cx| e.select_all_text(cx));
            }))
            .flex_row()
            .size_full()
            .text_size(px(13.))
            .font_family(theme.ui_font.clone())
            // Like the main window: the nav on the chrome, the page on an island.
            .child(self.render_nav(&words, window, cx))
            .child(
                island(&theme)
                    .p_0()
                    .flex_1()
                    .min_w_0()
                    .my(px(GAP))
                    .mr(px(GAP))
                    .child(self.render_header(&words, cx))
                    .when_some(self.file_error.clone(), |el, error| el.child(self.render_file_error(error, cx)))
                    .child(content),
            );
        // An open dropdown list goes after the panel, so it paints over every
        // part of it (the island clips its content to its rounded shape).
        div().relative().size_full().child(panel).children(self.overlay.take())
    }
}

impl Preferences {
    fn render_nav(&self, words: &[String], window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let searching = !words.is_empty();
        let mut items: Vec<AnyElement> = Vec::new();
        for page in Page::ALL {
            let selected = !searching && page == self.page;
            items.push(div()
                .id(("pref-page", page as usize))
                .relative()
                .flex()
                .items_center()
                .gap_2()
                .h(px(30.))
                .px_2p5()
                .rounded(px(8.))
                .cursor_pointer()
                .text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |el| el.bg(theme.accent_soft).font_weight(FontWeight::MEDIUM))
                .when(!selected, |el| el.hover(|s| s.text_color(theme.text)))
                .row_hover(theme.hover)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.show_page(page, window, cx)))
                .child(icon_el(page.icon(), px(14.), if selected { theme.accent.into() } else { theme.text_dim.into() }))
                .child(page.title())
                .into_any_element());
            // The open page's sections, to jump to.
            let sections = page_sections(page);
            if selected && sections.len() > 1 {
                for (ix, name) in sections.into_iter().enumerate() {
                    items.push(
                        div()
                            .id(("pref-section", ix))
                            .flex()
                            .items_center()
                            .h(px(24.))
                            .pl(px(34.))
                            .rounded(px(8.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(theme.text_dim)
                            .hover(|s| s.text_color(theme.text))
                            .row_hover(theme.hover)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.scroll.scroll_to_top_of_item(ix);
                                cx.notify();
                            }))
                            .child(name)
                            .into_any_element(),
                    );
                }
            }
        }
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(212.))
            .h_full()
            .child(div().flex().p_2().pt_3().child(input_box(&self.search, Some(Icon::Search), window, cx)))
            .child(div().flex().flex_col().gap_0p5().px_2().pt_1().flex_1().children(items))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_2()
                    .child(
                        link_row("pref-open-settings", Icon::FileCode, "Open settings.json", &theme)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::OpenSettingsFile))),
                    ),
            )
    }

    fn render_header(&self, words: &[String], cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (title, summary): (SharedString, SharedString) = if words.is_empty() {
            (self.page.title().into(), self.page.summary().into())
        } else {
            ("Search".into(), format!("Settings matching “{}”", words.join(" ")).into())
        };
        div()
            .flex()
            .items_start()
            .gap_3()
            .flex_shrink_0()
            .px(px(28.))
            .pt(px(22.))
            .pb(px(14.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title))
                    .child(div().text_color(theme.text_dim).child(summary)),
            )
            .when(words.is_empty() && self.page == Page::Keymap, |el| {
                el.child(
                    text_button("pref-open-keymap", "Open keymap.json", false, cx)
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::OpenKeymapFile))),
                )
            })
    }

    fn render_file_error(&self, error: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let text = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_auto()
            .min_w_0()
            .child(icon_el(Icon::TriangleAlert, px(14.), theme.warning.into()))
            .child(
                div()
                    .flex_auto()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(format!("Changes can't be saved until settings.json is fixed: {error}")),
            );
        let open = text_button("pref-fix-settings", "Open", false, cx)
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::OpenSettingsFile)));
        beside_or_under(text, open, self.stacked.get())
            .mx(px(28.))
            .mb_3()
            .px_3()
            .py_2()
            .rounded(px(8.))
            .bg(Hsla::from(theme.warning).alpha(0.1))
            .border_1()
            .border_color(Hsla::from(theme.warning).alpha(0.35))
    }

    /// The scrolling area under the header.
    fn body(&self) -> Stateful<Div> {
        self.body_shown.set(true);
        div()
            .id("pref-body")
            .flex()
            .flex_col()
            .gap_6()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(28.))
            .pb(px(28.))
    }

    fn render_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Languages => return self.render_languages(window, cx).into_any_element(),
            Page::Extensions => return self.render_extensions(window, cx).into_any_element(),
            Page::Trust => return self.render_trust(window, cx).into_any_element(),
            Page::Keymap => {
                let theme = Theme::get(cx).clone();
                let row = self.render_setting(Setting::BaseKeymap, window, cx);
                return div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .flex_1()
                    .min_h_0()
                    .child(div().px(px(28.)).child(section(Setting::BaseKeymap.section(), vec![row], &theme)))
                    .child(self.keymap.clone())
                    .into_any_element();
            }
            _ => {}
        }
        let mut sections: Vec<(&'static str, Vec<Setting>)> = Vec::new();
        for setting in Setting::ALL.into_iter().filter(|s| s.page() == self.page) {
            match sections.last_mut() {
                Some((name, list)) if *name == setting.section() => list.push(setting),
                _ => sections.push((setting.section(), vec![setting])),
            }
        }
        let theme = Theme::get(cx).clone();
        let sections: Vec<AnyElement> = sections
            .into_iter()
            .map(|(name, list)| {
                let rows: Vec<AnyElement> = list.into_iter().map(|s| self.render_setting(s, window, cx)).collect();
                section(name, rows, &theme).into_any_element()
            })
            .collect();
        self.body().children(sections).into_any_element()
    }

    /// Search results across pages, plus links into Languages and Keymap.
    fn render_results(&mut self, words: &[String], window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let mut groups: Vec<AnyElement> = Vec::new();
        for page in Page::ALL {
            let rows: Vec<AnyElement> = Setting::ALL
                .into_iter()
                .filter(|s| s.page() == page && s.matches(words))
                .map(|s| self.render_setting(s, window, cx))
                .collect();
            if !rows.is_empty() {
                groups.push(section(page.title(), rows, &theme).into_any_element());
            }
        }
        let languages: Vec<Language> = Language::all()
            .into_iter()
            .filter(|l| {
                let name = l.name().to_lowercase();
                words.iter().all(|w| name.contains(w.as_str()) || "language server formatter".contains(w.as_str()))
            })
            .collect();
        if !languages.is_empty() {
            let rows = languages
                .into_iter()
                .enumerate()
                .map(|(ix, language)| {
                    jump_row(("pref-find-lang", ix), Icon::Braces, language.name(), "Language settings", &theme)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.show_page(Page::Languages, window, cx);
                            this.select_language(language, cx);
                        }))
                        .into_any_element()
                })
                .collect();
            groups.push(section("Languages", rows, &theme).into_any_element());
        }
        let query = words.join(" ");
        let actions = self.keymap.read(cx).count_matching(&query, window);
        if actions > 0 {
            let detail = format!("{actions} action{} with a shortcut to change", if actions == 1 { "" } else { "s" });
            let row = jump_row("pref-find-keymap", Icon::Keyboard, format!("Keymap: “{query}”"), detail, &theme)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let query = query.clone();
                    this.show_page(Page::Keymap, window, cx);
                    this.keymap.update(cx, |k, cx| k.set_filter(&query, cx));
                }))
                .into_any_element();
            groups.push(section("Keymap", vec![row], &theme).into_any_element());
        }
        if groups.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .pt(px(60.))
                .flex_1()
                .text_color(theme.text_dim)
                .child(icon_el(Icon::Search, px(22.), theme.text_dim.into()))
                .child("No settings match")
                .into_any_element();
        }
        self.body().children(groups).into_any_element()
    }

    fn render_setting(&mut self, setting: Setting, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let settings = Settings::get(cx).clone();
        let key = setting_key(setting);
        let reset = (!key.is_empty() && settings.is_set(key)).then(|| {
            self.reset_button(("pref-reset", setting as usize), cx, move |this, cx| this.reset(key, cx)).into_any_element()
        });
        let control: AnyElement = match setting {
            Setting::Theme => return self.render_themes(cx).into_any_element(),
            Setting::RunConfigurations => return self.render_runs(window, cx).into_any_element(),
            Setting::FontFamily => {
                let (trigger, list) = self.render_font_menu(window, cx);
                let row = setting_row(setting.label(), Some(setting.description().into()), trigger, reset, self.stacked.get(), &theme);
                return div().flex().flex_col().child(row).children(list).into_any_element();
            }
            Setting::FontSize => {
                let size = settings.editor_font_size.unwrap_or(theme::DEFAULT_EDITOR_FONT_SIZE);
                self.stepper("pref-font-size", format!("{size}"), size > 6., size < 72., cx, |this, step, cx| {
                    let size = Settings::get(cx).editor_font_size.unwrap_or(theme::DEFAULT_EDITOR_FONT_SIZE);
                    let size = (size.round() + step as f32).clamp(6., 72.);
                    this.set_or_default("editor_font_size", json!(size), json!(theme::DEFAULT_EDITOR_FONT_SIZE), cx);
                    // What's picked here is what shows, without an old zoom on top.
                    theme::zoom(None, cx);
                })
            }
            Setting::LineHeight => {
                let current = Theme::get(cx);
                let height = settings
                    .editor_line_height
                    .unwrap_or(current.editor_line_height / current.editor_font_size);
                let label = format!("{:.1}", (height * 10.).round() / 10.);
                self.stepper("pref-line-height", label, height > 1.05, height < 2.95, cx, |this, step, cx| {
                    let current = Theme::get(cx);
                    let height = Settings::get(cx)
                        .editor_line_height
                        .unwrap_or(current.editor_line_height / current.editor_font_size);
                    let next = (((height * 10.).round() + step as f32) / 10.).clamp(1., 3.);
                    this.write(Settings::set("editor_line_height", json!((next * 10.).round() / 10.)), cx);
                })
            }
            Setting::TabSize => {
                let size = settings.editor.tab_size.unwrap_or(DEFAULT_TAB_SIZE);
                self.stepper("pref-tab-size", size.to_string(), size > 1, size < 16, cx, |this, step, cx| {
                    let size = Settings::get(cx).editor.tab_size.unwrap_or(DEFAULT_TAB_SIZE);
                    let size = (size as i64 + step).clamp(1, 16);
                    this.set_or_default("tab_size", json!(size), json!(DEFAULT_TAB_SIZE), cx);
                })
            }
            Setting::HardTabs => self.bool_switch("pref-hard-tabs", "hard_tabs", settings.editor.hard_tabs, cx),
            Setting::SoftWrap => self.bool_switch("pref-soft-wrap", "soft_wrap", settings.editor.soft_wrap, cx),
            Setting::Rulers => div().w(px(160.)).flex().child(input_box(&self.rulers, None, window, cx)).into_any_element(),
            Setting::ShowWhitespace => {
                let current = settings.editor.show_whitespace.unwrap_or_default();
                let options = ShowWhitespace::ALL.iter().map(|w| (w.label().into(), *w == current)).collect();
                self.dropdown(Menu::ShowWhitespace, current.label().into(), px(220.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    let choice = ShowWhitespace::ALL[ix];
                    this.set_or_default("show_whitespace", choice.key().into(), ShowWhitespace::default().key().into(), cx);
                })
            }
            Setting::IconTheme => {
                const BUILT_IN: &str = "Charmed Icons (built-in)";
                let current = settings.icon_theme.clone();
                let mut names: Vec<String> = vec![BUILT_IN.to_string()];
                let installed = self
                    .icon_themes
                    .get_or_insert_with(|| Arc::new(extensions::icon_themes().into_iter().map(|t| t.name).collect()));
                names.extend(installed.iter().cloned());
                let label = current.clone().unwrap_or_else(|| BUILT_IN.to_string());
                let options = names.iter().enumerate().map(|(ix, n)| (n.clone().into(), if ix == 0 { current.is_none() } else { current.as_deref() == Some(n.as_str()) })).collect();
                self.dropdown(Menu::IconTheme, label.into(), px(260.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    if ix == 0 {
                        this.reset("icon_theme", cx);
                    } else {
                        let result = Settings::set("icon_theme", json!(names[ix]));
                        this.write(result, cx);
                    }
                })
            }
            Setting::InlayHints => self.switch_with_default("pref-inlay-hints", "inlay_hints", settings.editor.inlay_hints, true, cx),
            Setting::CodeLens => self.switch_with_default("pref-code-lens", "code_lens", settings.editor.code_lens, true, cx),
            Setting::Minimap => self.bool_switch("pref-minimap", "minimap", settings.editor.minimap, cx),
            Setting::MinimapWidth => {
                let width = settings.minimap_width();
                let (min, max) = settings::MINIMAP_WIDTHS;
                let enabled = settings.editor.minimap.unwrap_or(false);
                div()
                    .when(!enabled, |el| el.opacity(0.45))
                    .child(self.stepper("pref-minimap-width", format!("{width} px"), enabled && width > min, enabled && width < max, cx, move |this, step, cx| {
                        let width = Settings::get(cx).minimap_width();
                        // Snap to tens.
                        let next = ((width / 10.).round() * 10. + step as f32 * 10.).clamp(min, max);
                        this.set_or_default("minimap_width", json!(next as i64), json!(settings::DEFAULT_MINIMAP_WIDTH as i64), cx);
                    }))
                    .into_any_element()
            }
            Setting::VimMode => self.bool_switch("pref-vim", "vim_mode", settings.vim_mode, cx),
            Setting::VisualEffects => {
                let current = settings.visual_effects();
                let options = VisualEffects::ALL.iter().map(|l| (visual_effects_label(*l).into(), *l == current)).collect();
                self.dropdown(Menu::VisualEffects, visual_effects_label(current).into(), px(200.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    this.write(Settings::set_visual_effects(VisualEffects::ALL[ix]), cx);
                })
            }
            Setting::SidebarDock => {
                let current = settings.sidebar_dock();
                let options = SidebarDock::ALL.iter().map(|d| (sidebar_dock_label(*d).into(), *d == current)).collect();
                self.dropdown(Menu::SidebarDock, sidebar_dock_label(current).into(), px(200.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    this.write(Settings::set_sidebar_dock(SidebarDock::ALL[ix]), cx);
                })
            }
            Setting::FormatOnSave => self.bool_switch("pref-format", "format_on_save", settings.editor.format_on_save, cx),
            Setting::TrustAll => self.switch_with_default("pref-trust-all", "trust_all_projects", settings.trust_all_projects, false, cx),
            Setting::NativeGrammars => self.switch_with_default("pref-native-grammars", "native_grammars", settings.native_grammars, false, cx),
            Setting::AutoUpdate => self.switch_with_default("pref-auto-update", "auto_update", settings.auto_update, true, cx),
            Setting::TrimWhitespace => {
                self.bool_switch("pref-trim", "trim_trailing_whitespace", settings.editor.trim_trailing_whitespace, cx)
            }
            Setting::FinalNewline => {
                self.bool_switch("pref-newline", "insert_final_newline", settings.editor.insert_final_newline, cx)
            }
            Setting::BaseKeymap => {
                let current = settings.base_keymap.unwrap_or_default();
                let options = BaseKeymap::ALL.iter().map(|k| (k.label().into(), *k == current)).collect();
                self.dropdown(Menu::BaseKeymap, current.label().into(), px(260.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    let keymap = BaseKeymap::ALL[ix];
                    this.set_or_default("base_keymap", keymap.key().into(), BaseKeymap::default().key().into(), cx);
                })
            }
            Setting::PullStrategy => {
                const ASK: &str = "Ask the first time";
                let current = settings.pull_strategy;
                let mut options = vec![(ASK.into(), current.is_none())];
                options.extend(PullStrategy::ALL.iter().map(|s| (s.label().into(), current == Some(*s))));
                let label = current.map_or(ASK, PullStrategy::label);
                self.dropdown(Menu::PullStrategy, label.into(), px(200.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    match ix.checked_sub(1) {
                        Some(ix) => this.write(Settings::set("pull_strategy", PullStrategy::ALL[ix].key().into()), cx),
                        None => this.reset("pull_strategy", cx),
                    }
                })
            }
            Setting::AutoSave => {
                let current = settings.auto_save.unwrap_or_default();
                let modes = [AutoSave::Off, AutoSave::AfterDelay, AutoSave::OnFocusChange, AutoSave::OnWindowChange];
                let options = modes.iter().map(|m| (auto_save_label(*m).into(), *m == current)).collect();
                self.dropdown(Menu::AutoSave, auto_save_label(current).into(), px(200.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    let mode = modes[ix];
                    this.set_or_default("auto_save", mode.key().into(), AutoSave::default().key().into(), cx);
                })
            }
            Setting::AutoSaveDelay => {
                let delay = settings.auto_save_delay.unwrap_or(DEFAULT_AUTO_SAVE_DELAY);
                let enabled = settings.auto_save.unwrap_or_default() == AutoSave::AfterDelay;
                let label = if delay >= 1000 { format!("{} s", delay as f64 / 1000.) } else { format!("{delay} ms") };
                div()
                    .when(!enabled, |el| el.opacity(0.45))
                    .child(self.stepper("pref-delay", label, enabled && delay > 250, enabled && delay < 10_000, cx, |this, step, cx| {
                        let delay = Settings::get(cx).auto_save_delay.unwrap_or(DEFAULT_AUTO_SAVE_DELAY) as i64;
                        // Snap to quarter seconds.
                        let next = ((delay + 125) / 250 * 250 + step * 250).clamp(250, 10_000);
                        this.set_or_default("auto_save_delay", json!(next), json!(DEFAULT_AUTO_SAVE_DELAY), cx);
                    }))
                    .into_any_element()
            }
            Setting::Shell => div().w(px(300.)).flex().child(input_box(&self.shell, None, window, cx)).into_any_element(),
        };
        setting_row(setting.label(), Some(setting.description().into()), control, reset, self.stacked.get(), &theme).into_any_element()
    }

    fn bool_switch(&self, id: &'static str, key: &'static str, value: Option<bool>, cx: &mut Context<Self>) -> AnyElement {
        self.switch_with_default(id, key, value, false, cx)
    }

    fn switch_with_default(
        &self,
        id: &'static str,
        key: &'static str,
        value: Option<bool>,
        default: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let on = value.unwrap_or(default);
        div()
            .id(id)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_or_default(key, json!(!on), json!(default), cx);
            }))
            .child(switch(on, &theme))
            .into_any_element()
    }

    fn reset_button(
        &self,
        id: impl Into<gpui::ElementId>,
        cx: &mut Context<Self>,
        on_reset: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        icon_button(id, Icon::Undo, theme.text_dim.into(), cx)
            .size(px(20.))
            .tooltip(|_, cx| tooltip_view("Reset to default", cx))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_reset(this, cx)))
    }

    /// `− value +`; `on_step` gets -1 or 1.
    fn stepper(
        &self,
        id: &'static str,
        value: String,
        can_decrease: bool,
        can_increase: bool,
        cx: &mut Context<Self>,
        on_step: impl Fn(&mut Self, i64, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let on_step = Rc::new(on_step);
        let button = |icon: Icon, step: i64, enabled: bool, cx: &mut Context<Self>| {
            let on_step = on_step.clone();
            div()
                .id((id, if step < 0 { 0usize } else { 1 }))
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .active(|s| s.bg(theme.active))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_step(this, step, cx)))
                })
                .hover_bg(if enabled { Hsla::from(theme.hover) } else { gpui::transparent_black() })
                .child(icon_el(icon, px(13.), if enabled { theme.text_muted.into() } else { Hsla::from(theme.text_dim).alpha(0.5) }))
        };
        div()
            .flex()
            .items_center()
            .h(px(30.))
            .p(px(2.))
            .rounded(px(7.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(button(Icon::Minus, -1, can_decrease, cx))
            .child(
                div()
                    .min_w(px(52.))
                    .flex()
                    .justify_center()
                    .font_family(theme.mono_font.clone())
                    .text_size(px(12.5))
                    .text_color(theme.text)
                    .child(value),
            )
            .child(button(Icon::Plus, 1, can_increase, cx))
            .into_any_element()
    }

    /// A button showing the current choice, with a list under it while open.
    /// A button showing the current choice. While open, its list is kept in
    /// `self.overlay`, which `render` draws last so it floats over the rows
    /// below. (The modal layer is already deferred, and GPUI can't nest
    /// deferred drawing.)
    fn dropdown(
        &mut self,
        menu: Menu,
        label: SharedString,
        width: Pixels,
        options: Vec<(SharedString, bool)>,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let open = self.menu == Some(menu);
        let on_pick = Rc::new(on_pick);
        let list = open.then(|| {
            let items = options.into_iter().enumerate().map(|(ix, (text, selected))| {
                let on_pick = on_pick.clone();
                div()
                    .id(("pref-option", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(26.))
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(theme.text)
                    .row_hover(theme.accent_soft)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| on_pick(this, ix, window, cx)))
                    .child(div().flex_1().min_w_0().truncate().child(text))
                    .when(selected, |el| el.child(icon_el(Icon::Check, px(13.), theme.accent.into())))
            });
            let anchor = self.menu_anchors[menu as usize].clone();
            let trigger = anchor.get();
            anchored()
                .position(point(trigger.right(), trigger.bottom() + px(4.)))
                .anchor(Corner::TopRight)
                .snap_to_window_with_margin(px(8.))
                .child(
                    popover(&theme)
                        .w(width)
                        .p_1()
                        // Drawn outside the panel (see `render`), so it doesn't
                        // inherit the panel's text style.
                        .text_size(px(13.))
                        .font_family(theme.ui_font.clone())
                        .occlude()
                        .on_mouse_down_out(cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            // The trigger's own click toggles it closed.
                            if !anchor.get().contains(&event.position) {
                                this.menu = None;
                                cx.notify();
                            }
                        }))
                        .children(items),
                )
                .into_any_element()
        });
        let anchor = self.menu_anchors[menu as usize].clone();
        if list.is_some() {
            self.overlay = list;
        }
        div()
            .relative()
            .child(dropdown_trigger(("pref-menu", menu as usize), label, width, open, &theme).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| this.toggle_menu(menu, window, cx),
            )))
            // Pinned to the corner: an absolute child without insets sits where
            // it would in the flow, below the button, and the list would open
            // a button's height too low.
            .child(canvas(move |bounds, _, _| anchor.set(bounds), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
            .into_any_element()
    }

    /// The font button, and while it's open, the list that unfolds under its
    /// row. (Not a floating menu: GPUI can't defer the filter field's
    /// editor inside another deferred element.)
    fn render_font_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> (AnyElement, Option<AnyElement>) {
        let theme = Theme::get(cx).clone();
        let configured = Settings::get(cx).editor_font_family.clone();
        let default_font = theme::default_fonts(cx).1;
        let label: SharedString = match &configured {
            Some(family) if *family != theme.mono_font.as_ref() => format!("{family} (not installed)").into(),
            Some(family) => family.clone().into(),
            None => format!("Default ({default_font})").into(),
        };
        let open = self.menu == Some(Menu::FontFamily);
        let width = px(240.);
        let list = open.then(|| {
            let filter = self.font_filter.read(cx).text(cx).to_lowercase();
            let mut rows: Vec<Option<SharedString>> = vec![None];
            if let Some(fonts) = &self.fonts {
                rows.extend(fonts.iter().filter(|f| f.to_lowercase().contains(&filter)).cloned().map(Some));
            }
            if !filter.is_empty() {
                rows.retain(Option::is_some);
            }
            let rows = Arc::new(rows);
            let count = rows.len();
            let loading = self.fonts.is_none();
            let list = uniform_list("pref-fonts", count, {
                let rows = rows.clone();
                let configured = configured.clone();
                let default_font = default_font.clone();
                cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                    let theme = Theme::get(cx).clone();
                    range
                        .map(|ix| {
                            let family = rows[ix].clone();
                            let selected = family.as_ref().map(|f| f.to_string()) == configured;
                            let text: SharedString = match &family {
                                Some(f) => f.clone(),
                                None => format!("Default ({default_font})").into(),
                            };
                            div().w_full().child(
                                div()
                                    .id(("pref-font", ix))
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .h(px(26.))
                                    .px_2()
                                    .rounded(px(5.))
                                    .cursor_pointer()
                                    .text_color(theme.text)
                                    .row_hover(theme.hover)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                        this.choose_font(family.clone(), window, cx)
                                    }))
                                    .child(div().flex_1().min_w_0().truncate().child(text))
                                    // A sample in the font itself; the name stays readable
                                    // even for symbol fonts.
                                    .when_some(rows[ix].clone(), |el, f| {
                                        el.child(div().font_family(f).text_color(theme.text_muted).child("Ag 0O il1 {}"))
                                    })
                                    .when(selected, |el| el.child(icon_el(Icon::Check, px(13.), theme.accent.into()))),
                            )
                        })
                        .collect()
                })
            })
            .track_scroll(self.fonts_scroll.clone())
            .h(px((count.clamp(1, 8) * 26) as f32))
            .w_full();
            let list = div().relative().child(list).child(crate::ui::list_scrollbar("pref-fonts-scrollbar", &self.fonts_scroll, cx));
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .mx_4()
                .mb_3()
                .p_1p5()
                .rounded(px(8.))
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .child(input_box(&self.font_filter, Some(Icon::Search), window, cx))
                .child(if loading {
                    div().h(px(26.)).px_2().flex().items_center().text_color(theme.text_dim).child("Finding fonts…").into_any_element()
                } else if count == 0 {
                    div().h(px(26.)).px_2().flex().items_center().text_color(theme.text_dim).child("No fonts match").into_any_element()
                } else {
                    list.into_any_element()
                })
                .into_any_element()
        });
        let trigger = dropdown_trigger("pref-font-menu", label, width, open, &theme)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_menu(Menu::FontFamily, window, cx)))
            .into_any_element();
        (trigger, list)
    }

    fn render_themes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let mut entries: Vec<(Arc<Theme>, bool)> =
            cx.global::<ThemeRegistry>().entries.iter().map(|e| (e.theme.clone(), e.path.is_some())).collect();
        entries.sort_by_key(|(t, _)| t.appearance == Appearance::Light);
        let errors = cx.global::<ThemeRegistry>().errors.clone();
        let cards: Vec<(Appearance, AnyElement)> = entries.into_iter().enumerate().map(|(ix, (t, custom))| {
            let appearance = t.appearance;
            let selected = t.name == theme.name;
            let name = t.name.clone();
            // A few lines of "code" in the theme's own colors, painted rather
            // than laid out: a page of cards made of divs took most of a
            // frame to lay out while scrolling.
            let s = &t.syntax;
            let code: [(f32, Vec<(f32, gpui::Rgba)>); 4] = [
                (0., vec![(16., s.keyword), (30., s.function), (8., s.punctuation)]),
                (10., vec![(12., s.keyword), (22., s.variable_builtin), (34., s.string)]),
                (10., vec![(44., s.comment)]),
                (0., vec![(8., s.punctuation), (18., s.type_), (10., s.number)]),
            ];
            let background = t.background;
            let card = div()
                .id(("pref-theme", ix))
                .flex()
                .flex_col()
                .flex_1()
                .flex_basis(px(0.))
                .min_w_0()
                .rounded(px(10.))
                .overflow_hidden()
                .border_1()
                // Only the current theme is outlined; the others keep a clear border so
                // every card has the same size.
                .border_color(if selected { Hsla::from(theme.accent) } else { gpui::transparent_black() })
                .when(selected, |el| el.shadow(focus_glow(&theme)))
                .cursor_pointer()
                .hover(|s| s.border_color(Hsla::from(theme.accent).alpha(0.6)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_theme(name.clone(), cx)))
                .child(
                    canvas(|_, _, _| {}, move |bounds, _, window, _| {
                        let top = gpui::Corners { top_left: px(9.), top_right: px(9.), ..Default::default() };
                        window.paint_quad(gpui::fill(bounds, background).corner_radii(top));
                        for (row, (indent, bars)) in code.iter().enumerate() {
                            let mut x = bounds.left() + px(10. + indent);
                            let y = bounds.top() + px(8. + row as f32 * 9.);
                            for &(width, color) in bars {
                                let bar = Bounds::new(point(x, y), gpui::size(px(width), px(4.)));
                                window.paint_quad(gpui::fill(bar, color).corner_radii(px(2.)));
                                x += px(width + 4.);
                            }
                        }
                    })
                    .w_full()
                    .h(px(52.)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .h(px(28.))
                        .px_2p5()
                        .rounded_b(px(9.))
                        .bg(t.panel)
                        .border_t_1()
                        .border_color(t.border)
                        .text_size(px(12.))
                        .text_color(t.text)
                        .child(div().flex_1().min_w_0().truncate().child(t.name.clone()))
                        .when(custom, |el| el.child(div().text_size(px(10.5)).text_color(t.text_dim).child("Custom")))
                        .when(selected, |el| el.child(icon_el(Icon::CircleCheck, px(13.), t.accent.into()))),
                );
            (appearance, card.into_any_element())
        }).collect();
        // Dark and light themes apart, in rows of four equal cards (GPUI's
        // grid doesn't size these).
        const PER_ROW: usize = 4;
        let mut groups: Vec<AnyElement> = Vec::new();
        let (dark, light): (Vec<_>, Vec<_>) = cards.into_iter().partition(|(a, _)| *a == Appearance::Dark);
        for (title, cards) in [("Dark", dark), ("Light", light)] {
            if cards.is_empty() {
                continue;
            }
            let mut cards: Vec<AnyElement> = cards.into_iter().map(|(_, c)| c).collect();
            while !cards.len().is_multiple_of(PER_ROW) {
                cards.push(div().flex_1().flex_basis(px(0.)).into_any_element());
            }
            let mut rows = Vec::new();
            while !cards.is_empty() {
                let rest = cards.split_off(PER_ROW);
                let row = std::mem::replace(&mut cards, rest);
                // A card's height (border, code, name), so a row needn't measure them.
                rows.push(div().flex().h(px(82.)).gap_3().children(row));
            }
            groups.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child(title))
                    .children(rows)
                    .into_any_element(),
            );
        }
        let theme_dir = theme::themes_dir().map(|d| theme::display_path(&d)).unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_3()
            .children(groups)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("pref-theme-hint")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(if errors.is_empty() { theme.text_dim } else { theme.warning })
                            .child(if errors.is_empty() {
                                format!("Your own themes go in {theme_dir}")
                            } else {
                                format!("{} theme problem(s): {}", errors.len(), errors[0])
                            })
                            .when(!errors.is_empty(), |el| {
                                let all = errors.join("\n");
                                el.tooltip(move |_, cx| tooltip_view(all.clone(), cx))
                            }),
                    )
                    .child(text_button("pref-reload-themes", "Reload", false, cx).on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                        theme::reload_themes(cx);
                        cx.notify();
                    })))
                    .child(text_button("pref-themes-folder", "Open Themes Folder", false, cx).on_click(cx.listener(
                        |_, _: &ClickEvent, _, cx| match theme::ensure_themes_dir() {
                            Ok(dir) => {
                                cx.open_url(&format!("file://{}", dir.display()));
                                theme::reload_themes(cx);
                                cx.notify();
                            }
                            Err(err) => cx.emit(PreferencesEvent::Problems(vec![format!("themes: {err}")])),
                        },
                    ))),
            )
    }

    // ---- Extensions ----------------------------------------------------------------

    fn install_from_git(&mut self, cx: &mut Context<Self>) {
        let url = self.extension_url.read(cx).text(cx).trim().to_string();
        if url.is_empty() {
            self.extension_status = Some((true, "Enter the URL of the extension's Git repository".into()));
            cx.notify();
            return;
        }
        self.installing = true;
        self.extension_status = Some((false, format!("Installing from {url}…")));
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { extensions::install_git(&url) }).await;
            this.update(cx, |this, cx| this.installed(result, cx)).ok();
        });
        task.detach();
    }

    fn install_from_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Install".into()),
        });
        let task = cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
            {
                this.update(cx, |this, cx| this.installed(extensions::install_folder(&path), cx)).ok();
            }
        });
        task.detach();
    }

    fn installed(&mut self, result: Result<String, String>, cx: &mut Context<Self>) {
        self.installing = false;
        self.extension_status = Some(match result {
            Ok(name) => {
                self.extension_url.update(cx, |e, cx| e.set_text("", cx));
                extensions::reload(cx);
                (false, format!("Installed {name}"))
            }
            Err(err) => (true, err),
        });
        cx.notify();
    }

    /// Zed's installed extensions not added here: as read last, or none
    /// while they're read (in the background).
    fn zed_extensions(&mut self, cx: &mut Context<Self>) -> Arc<Vec<(String, std::path::PathBuf)>> {
        if let Some(found) = &self.zed_extensions {
            return found.clone();
        }
        if self.zed_extensions_task.is_none() {
            self.zed_extensions_task = Some(cx.spawn(async move |this, cx| {
                let found = cx.background_executor().spawn(async { extensions::zed_extensions() }).await;
                this.update(cx, |this, cx| {
                    // This task is running; let it finish instead of dropping it.
                    if let Some(task) = this.zed_extensions_task.take() {
                        task.detach();
                    }
                    this.zed_extensions = Some(Arc::new(found));
                    cx.notify();
                })
                .ok();
            }));
        }
        Arc::default()
    }

    /// Gets the registry's list of extensions, unless it's here or coming.
    fn fetch_registry(&mut self, cx: &mut Context<Self>) {
        if matches!(self.registry, Some(Ok(_))) || self.registry_fetch.is_some() {
            return;
        }
        self.registry = None;
        self.registry_busy.set(Some("Loading the list of extensions…".into()), |this: &mut Self| &mut this.registry_busy, cx);
        self.registry_fetch = Some(cx.spawn(async move |this, cx| {
            let fetched = cx.background_executor().spawn(async move { registry::available() }).await;
            this.update(cx, |this, cx| {
                this.registry = Some(fetched);
                this.registry_fetch = None;
                this.registry_busy.set(None, |this: &mut Self| &mut this.registry_busy, cx);
            })
            .ok();
        }));
    }

    fn install_from_registry(&mut self, listed: registry::Listed, cx: &mut Context<Self>) {
        if self.registry_installing.contains(&listed.id) {
            return;
        }
        self.registry_installing.push(listed.id.clone());
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let package = listed.clone();
            let result = cx.background_executor().spawn(async move { registry::install(&package) }).await;
            this.update(cx, |this, cx| {
                this.registry_installing.retain(|id| *id != listed.id);
                this.extension_status = Some(match result {
                    Ok(name) => {
                        extensions::reload(cx);
                        (false, format!("Installed {name} {}", listed.version))
                    }
                    Err(err) => (true, err),
                });
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    fn uninstall(&mut self, extension: ExtensionInfo, window: &mut Window, cx: &mut Context<Self>) {
        let linked = extension.path.is_symlink();
        let detail = if linked { "Its folder stays where it is." } else { "Its folder goes to the Trash." };
        let answer = window.prompt(gpui::PromptLevel::Warning, &format!("Remove {}?", extension.name), Some(detail), &["Remove", "Cancel"], cx);
        let task = cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let result = extensions::uninstall(&extension.path);
            this.update(cx, |this, cx| {
                this.extension_status = Some(match result {
                    Ok(()) => (false, format!("Removed {}", extension.name)),
                    Err(err) => (true, err),
                });
                extensions::reload(cx);
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    fn render_extensions(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let installing = self.installing;
        let install = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(input_box(&self.extension_url, Some(Icon::Link), window, cx)))
                    .child(text_button("ext-install-git", "Install", true, cx).when(!installing, |el| {
                        el.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.install_from_git(cx)))
                    }))
                    .child(
                        text_button("ext-install-folder", "Install from Folder…", false, cx)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.install_from_folder(cx))),
                    ),
            )
            .when_some(self.extension_status.clone(), |el, (error, text)| {
                el.child(div().text_size(px(12.)).text_color(if error { theme.error } else { theme.text_dim }).child(text))
            });

        let installed = extensions::installed();
        // The registry's extensions: built from their authors' repositories,
        // grammars and code compiled.
        let mut available = vec![div().px_3().py_2p5().child(input_box(&self.registry_query, Some(Icon::Search), window, cx)).into_any_element()];
        let note = |text: String, color: Hsla| div().px_3().pb_2p5().text_size(px(12.5)).text_color(color).child(text).into_any_element();
        if let Some(label) = self.registry_busy.label() {
            available.push(note(label.to_string(), theme.text_dim.into()));
        }
        match &self.registry {
            Some(Ok(listed)) => {
                let found = registry::search(listed, &self.registry_query.read(cx).text(cx));
                if found.is_empty() {
                    available.push(note("No extensions match".into(), theme.text_dim.into()));
                }
                available.extend(found.iter().enumerate().map(|(ix, listed)| self.render_listed(ix, listed, &installed, &theme, cx)));
            }
            Some(Err(err)) => available.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .pb_2p5()
                    .child(div().flex_1().min_w_0().text_size(px(12.5)).text_color(theme.error).child(err.clone()))
                    .child(text_button("ext-registry-retry", "Try Again", false, cx).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.fetch_registry(cx))))
                    .into_any_element(),
            ),
            None => {}
        }
        let rows: Vec<AnyElement> = if installed.is_empty() {
            vec![
                div()
                    .px_3()
                    .py_2p5()
                    .text_size(px(12.5))
                    .text_color(theme.text_dim)
                    .child("No extensions yet. An extension can add a language (grammar, highlighting, outline, comments, language server, formatter), themes and snippets.")
                    .into_any_element(),
            ]
        } else {
            installed
                .into_iter()
                .enumerate()
                .map(|(ix, extension)| self.render_extension(ix, extension, &theme, cx))
                .collect()
        };

        // Zed's installed extensions come with prebuilt grammars.
        let zed: Vec<AnyElement> = self
            .zed_extensions(cx)
            .iter()
            .cloned()
            .enumerate()
            .map(|(ix, (name, path))| {
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .px_3()
                    .py_2()
                    .child(icon_el(Icon::Puzzle, px(15.), theme.text_muted.into()))
                    .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(name))
                    .child(
                        text_button(("ext-zed", ix), "Add", false, cx)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.installed(extensions::install_folder(&path), cx))),
                    )
                    .into_any_element()
            })
            .collect();

        let folder = extensions::extensions_dir().map(|d| theme::display_path(&d)).unwrap_or_default();
        self.body()
            .child(section("Install", vec![install.into_any_element()], &theme))
            .child(section("Installed", rows, &theme))
            .child(section("Available", available, &theme))
            .when(!zed.is_empty(), |el| el.child(section("Installed in Zed", zed, &theme)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(theme.text_dim).child(format!("Extensions live in {folder}")))
                    .child(text_button("ext-reload", "Reload", false, cx).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.extension_status = None;
                        extensions::reload(cx);
                        cx.notify();
                    })))
                    .child(text_button("ext-folder", "Open Extensions Folder", false, cx).on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                        if let Some(dir) = extensions::extensions_dir() {
                            let _ = std::fs::create_dir_all(&dir);
                            cx.open_url(&format!("file://{}", dir.display()));
                        }
                    }))),
            )
    }

    /// Restricted Mode's switch and the trusted folders, each removable.
    fn render_trust(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let switch_rows = vec![self.render_setting(Setting::TrustAll, window, cx), self.render_setting(Setting::NativeGrammars, window, cx)];
        let folders = crate::trust::folders();
        let rows: Vec<AnyElement> = if folders.is_empty() {
            vec![
                div()
                    .px_3()
                    .py_2p5()
                    .text_color(theme.text_dim)
                    .child("No trusted folders yet. Opening a folder asks whether to trust it.")
                    .into_any_element(),
            ]
        } else {
            folders
                .into_iter()
                .enumerate()
                .map(|(ix, folder)| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .px_3()
                        .py_2()
                        .child(icon_el(Icon::Folder, px(15.), theme.text_muted.into()))
                        .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(folder.display().to_string()))
                        .child(
                            icon_button(("trust-remove", ix), Icon::Trash, theme.text_muted.into(), cx)
                                .tooltip(|_, cx| tooltip_view("Don't trust", cx))
                                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| crate::trust::untrust(&folder, cx))),
                        )
                        .into_any_element()
                })
                .collect()
        };
        self.body()
            .child(section(Setting::TrustAll.section(), switch_rows, &theme))
            .child(section("Trusted folders", rows, &theme))
    }

    /// One of the registry's extensions: what it has, its license, and
    /// Install (Update) unless it's here already.
    fn render_listed(&self, ix: usize, listed: &registry::Listed, installed: &[ExtensionInfo], theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let state = registry::installed(listed, installed);
        let mut details: Vec<String> = Vec::new();
        if !listed.provides.is_empty() {
            details.push(listed.provides.iter().map(|p| p.replace('-', " ")).collect::<Vec<_>>().join(", "));
        }
        if !listed.license.is_empty() {
            details.push(listed.license.clone());
        }
        let action = if self.registry_installing.contains(&listed.id) {
            text_button(("ext-registry", ix), "Installing…", false, cx).into_any_element()
        } else if matches!(state, registry::Installed::No | registry::Installed::Outdated) {
            let label = if state == registry::Installed::No { "Install" } else { "Update" };
            let listed = listed.clone();
            text_button(("ext-registry", ix), label, true, cx)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.install_from_registry(listed.clone(), cx)))
                .into_any_element()
        } else {
            div().text_size(px(12.)).text_color(theme.text_dim).child("Installed").into_any_element()
        };
        div()
            .flex()
            .items_start()
            .gap_2p5()
            .px_3()
            .py_2p5()
            .child(div().pt(px(2.)).child(icon_el(Icon::Puzzle, px(15.), theme.text_muted.into())))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(format!("{}  {}", listed.name, listed.version)))
                    .when(!listed.description.is_empty(), |el| {
                        el.child(div().text_size(px(12.5)).text_color(theme.text_muted).child(listed.description.clone()))
                    })
                    .child(div().text_size(px(12.)).text_color(theme.text_dim).child(details.join(" · "))),
            )
            .child(action)
            .into_any_element()
    }

    fn render_extension(&self, ix: usize, extension: ExtensionInfo, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut provides: Vec<String> = Vec::new();
        if !extension.languages.is_empty() {
            provides.push(extension.languages.join(", "));
        }
        if extension.themes > 0 {
            provides.push(if extension.themes == 1 { "1 theme".into() } else { format!("{} themes", extension.themes) });
        }
        if extension.snippets {
            provides.push("snippets".into());
        }
        provides.extend(extension.servers.iter().map(|s| format!("server: {s}")));
        let status: Option<(Hsla, String)> = if !extension.building.is_empty() {
            Some((theme.text_dim.into(), format!("Building the {} grammar…", extension.building.join(", "))))
        } else {
            extension.problems.first().map(|p| {
                let more = extension.problems.len() - 1;
                (theme.warning.into(), if more > 0 { format!("{p} (+{more})") } else { p.clone() })
            })
        };
        let problems = extension.problems.join("\n");
        let header = format!("{}{}", extension.name, if extension.version.is_empty() { String::new() } else { format!("  {}", extension.version) });
        div()
            .flex()
            .items_start()
            .gap_2p5()
            .px_3()
            .py_2p5()
            .child(div().pt(px(2.)).child(icon_el(Icon::Puzzle, px(15.), theme.text_muted.into())))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    // An auto basis, like a setting row's text (see `setting_row`).
                    .flex_auto()
                    .min_w_0()
                    .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(header))
                    .when(!extension.description.is_empty(), |el| {
                        el.child(div().text_size(px(12.5)).text_color(theme.text_muted).child(extension.description.clone()))
                    })
                    .when(!provides.is_empty(), |el| el.child(div().text_size(px(12.)).text_color(theme.text_dim).child(provides.join(" · "))))
                    .when_some(status, |el, (color, text)| {
                        el.child(
                            div()
                                .id(("ext-status", ix))
                                .text_size(px(12.))
                                .text_color(color)
                                .truncate()
                                .child(text)
                                .when(!problems.is_empty(), |el| el.tooltip(move |_, cx| tooltip_view(problems.clone(), cx))),
                        )
                    }),
            )
            .child(
                icon_button(("ext-remove", ix), Icon::Trash, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Remove", cx))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.uninstall(extension.clone(), window, cx))),
            )
            .into_any_element()
    }

    fn render_runs(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let rows: Vec<AnyElement> = self
            .runs
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .when(ix > 0, |el| el.border_t_1().border_color(theme.border_subtle))
                    .child(icon_el(Icon::Play, px(13.), theme.text_dim.into()))
                    .child(div().w(px(150.)).flex().child(input_box(&row.name, None, window, cx)))
                    .child(div().flex_1().min_w_0().flex().child(input_box(&row.command, None, window, cx)))
                    .child(div().w(px(110.)).flex().child(input_box(&row.cwd, None, window, cx)))
                    .child(
                        icon_button(("pref-run-remove", ix), Icon::Trash, theme.text_dim.into(), cx)
                            .tooltip(|_, cx| tooltip_view("Remove", cx))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.remove_run_row(ix, cx))),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        div()
            .flex()
            .flex_col()
            .children(rows)
            // Laid out like a setting row (the button under the text when narrow).
            .child(
                beside_or_under(
                    div().flex_auto().min_w_0().text_size(px(12.)).text_color(theme.text_dim).child(Setting::RunConfigurations.description()),
                    text_button("pref-run-add", "Add Configuration", false, cx)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.new_run_row(window, cx))),
                    self.stacked.get(),
                )
                .px_4()
                .py_2p5()
                .when(!empty, |el| el.border_t_1().border_color(theme.border_subtle)),
            )
    }

    fn render_languages(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let settings = Settings::get(cx).clone();
        let customized = |l: Language| {
            settings.languages.contains_key(l.name())
                || settings.language_servers.contains_key(l.name())
                || settings.formatters.contains_key(l.name())
        };
        let chosen = self.language;
        let list: Vec<AnyElement> = Language::all().into_iter().enumerate().map(|(ix, language)| {
            let selected = language == chosen;
            div()
                .id(("pref-lang", ix))
                .relative()
                .flex()
                .items_center()
                .gap_2()
                .h(px(28.))
                .px_2p5()
                .rounded(px(8.))
                .cursor_pointer()
                .text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |el| el.bg(theme.accent_soft))
                .when(!selected, |el| el.hover(|s| s.text_color(theme.text)))
                .row_hover(theme.hover)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.select_language(language, cx)))
                .child(div().flex_1().child(language.name()))
                .when(customized(language), |el| {
                    el.child(div().size(px(6.)).rounded_full().bg(theme.text_muted))
                        .tooltip(|_, cx| tooltip_view("Has its own settings", cx))
                })
                .into_any_element()
        }).collect();

        let language = self.language;
        let overrides = settings.languages.get(language.name()).cloned().unwrap_or_default();
        let effective = settings.for_file(language, None);
        let mut editor_rows: Vec<AnyElement> = Vec::new();
        for option in LanguageOption::ALL {
            let set = option.is_set(&overrides);
            let reset = set.then(|| {
                self.reset_button(("pref-lang-reset", option as usize), cx, move |this, cx| {
                    let language = this.language;
                    this.write(Settings::set_language_option(language, option.key(), None), cx);
                })
                .into_any_element()
            });
            let control = match option {
                LanguageOption::TabSize => {
                    let size = effective.tab_size;
                    self.stepper("pref-lang-tab", size.to_string(), size > 1, size < 16, cx, move |this, step, cx| {
                        let size = Settings::get(cx).for_file(this.language, None).tab_size as i64;
                        this.set_language_option(option, json!((size + step).clamp(1, 16)), cx);
                    })
                }
                _ => {
                    let on = option.flag(&effective);
                    div()
                        .id(("pref-lang-switch", option as usize))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_language_option(option, json!(!on), cx)))
                        .child(switch(on, &theme))
                        .into_any_element()
                }
            };
            // An extension's grammar may reject valid code, so its parse
            // errors stay off unless asked for.
            let extension_default = option == LanguageOption::SyntaxErrors
                && language.extension().is_some()
                && settings.editor.syntax_errors.is_none();
            let note: SharedString = if set {
                format!("Set for {}", language.name()).into()
            } else if extension_default {
                "Off for extension languages, whose grammars may reject valid code".into()
            } else {
                "Same as the general setting".into()
            };
            editor_rows.push(setting_row(option.label(), Some(note), control, reset, self.stacked.get(), &theme).into_any_element());
        }

        let tool_rows = |this: &mut Self, tool: Tool, window: &mut Window, cx: &mut Context<Self>| -> Vec<AnyElement> {
            let theme = Theme::get(cx).clone();
            let settings = Settings::get(cx).clone();
            let configured = tool_value(&settings, tool, language);
            let mode = if this.pending_custom == Some(tool) { ToolMode::Custom } else { ToolMode::of(configured.as_ref()) };
            let modes: Vec<ToolMode> = match tool {
                Tool::Server => vec![ToolMode::Automatic, ToolMode::Off, ToolMode::Custom],
                Tool::Formatter => vec![ToolMode::Automatic, ToolMode::LanguageServer, ToolMode::Off, ToolMode::Custom],
            };
            let options = modes.iter().map(|m| (SharedString::from(m.label()), *m == mode)).collect();
            let menu = match tool {
                Tool::Server => Menu::LanguageServer,
                Tool::Formatter => Menu::Formatter,
            };
            let picker = this.dropdown(menu, mode.label().into(), px(180.), options, cx, move |this, ix, window, cx| {
                this.set_tool_mode(tool, modes[ix], window, cx)
            });
            let (label, description) = tool_description(tool, language, mode);
            let mut rows = vec![setting_row(label, Some(description.into()), picker, None, this.stacked.get(), &theme).into_any_element()];
            if mode == ToolMode::Custom {
                let field = match tool {
                    Tool::Server => &this.server_command,
                    Tool::Formatter => &this.formatter_command,
                };
                rows.push(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_4()
                        .py_2p5()
                        .border_t_1()
                        .border_color(theme.border_subtle)
                        .child(div().w(px(70.)).text_color(theme.text_muted).child("Command"))
                        .child(div().flex_1().min_w_0().flex().child(input_box(field, Some(Icon::Terminal), window, cx)))
                        .into_any_element(),
                );
            }
            rows
        };

        let mut sections = vec![section("Editor", editor_rows, &theme).into_any_element()];
        if servers::spec(language).is_some() {
            let rows = tool_rows(self, Tool::Server, window, cx);
            sections.push(section("Language server", rows, &theme).into_any_element());
        }
        if language != Language::PlainText {
            let rows = tool_rows(self, Tool::Formatter, window, cx);
            sections.push(section("Formatter", rows, &theme).into_any_element());
        }

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .gap_5()
            .pl(px(28.))
            .child(
                div()
                    .id("pref-languages")
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .flex_shrink_0()
                    .w(px(150.))
                    .pb(px(28.))
                    .overflow_y_scroll()
                    .children(list),
            )
            .child(self.body().pl_0().child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(language.name()).mb(px(-12.))).children(sections))
    }
}

// ---- Building blocks -----------------------------------------------------------------

fn clone_event(event: &PreferencesEvent) -> PreferencesEvent {
    match event {
        PreferencesEvent::OpenSettingsFile => PreferencesEvent::OpenSettingsFile,
        PreferencesEvent::OpenKeymapFile => PreferencesEvent::OpenKeymapFile,
        PreferencesEvent::Problems(p) => PreferencesEvent::Problems(p.clone()),
        PreferencesEvent::Dismissed => PreferencesEvent::Dismissed,
    }
}

/// Section names of a page with settings rows, in order.
fn page_sections(page: Page) -> Vec<&'static str> {
    let mut sections: Vec<&'static str> = Vec::new();
    for setting in Setting::ALL.into_iter().filter(|s| s.page() == page) {
        if sections.last() != Some(&setting.section()) {
            sections.push(setting.section());
        }
    }
    sections
}

/// The `settings.json` key a row resets; empty for rows without one.
fn setting_key(setting: Setting) -> &'static str {
    match setting {
        Setting::Theme | Setting::RunConfigurations => "",
        Setting::BaseKeymap => "base_keymap",
        Setting::FontFamily => "editor_font_family",
        Setting::FontSize => "editor_font_size",
        Setting::LineHeight => "editor_line_height",
        Setting::TabSize => "tab_size",
        Setting::HardTabs => "hard_tabs",
        Setting::IconTheme => "icon_theme",
        Setting::VisualEffects => "visual_effects",
        Setting::SidebarDock => "sidebar_dock",
        Setting::SoftWrap => "soft_wrap",
        Setting::Rulers => "rulers",
        Setting::ShowWhitespace => "show_whitespace",
        Setting::InlayHints => "inlay_hints",
        Setting::CodeLens => "code_lens",
        Setting::Minimap => "minimap",
        Setting::MinimapWidth => "minimap_width",
        Setting::VimMode => "vim_mode",
        Setting::AutoSave => "auto_save",
        Setting::AutoSaveDelay => "auto_save_delay",
        Setting::FormatOnSave => "format_on_save",
        Setting::TrimWhitespace => "trim_trailing_whitespace",
        Setting::FinalNewline => "insert_final_newline",
        Setting::Shell => "terminal_shell",
        Setting::TrustAll => "trust_all_projects",
        Setting::NativeGrammars => "native_grammars",
        Setting::AutoUpdate => "auto_update",
        Setting::PullStrategy => "pull_strategy",
    }
}

fn sidebar_dock_label(dock: SidebarDock) -> &'static str {
    match dock {
        SidebarDock::Left => "Left",
        SidebarDock::Right => "Right",
    }
}

fn visual_effects_label(level: VisualEffects) -> &'static str {
    match level {
        VisualEffects::Full => "Full",
        VisualEffects::Reduced => "Reduced",
        VisualEffects::Minimal => "Minimal",
    }
}

fn auto_save_label(mode: AutoSave) -> &'static str {
    match mode {
        AutoSave::Off => "Off",
        AutoSave::AfterDelay => "After a delay",
        AutoSave::OnFocusChange => "When switching tabs",
        AutoSave::OnWindowChange => "When leaving the window",
    }
}

fn tool_value(settings: &Settings, tool: Tool, language: Language) -> Option<Value> {
    let map = match tool {
        Tool::Server => &settings.language_servers,
        Tool::Formatter => &settings.formatters,
    };
    map.get(language.name()).cloned()
}

/// What Kuana runs for `tool` unless told otherwise, as a command line.
fn default_command(tool: Tool, language: Language) -> Vec<String> {
    let first = match tool {
        Tool::Server => servers::spec(language).and_then(|s| s.candidates.first()),
        Tool::Formatter => format::candidates(language).first(),
    };
    first.map(|(name, args)| std::iter::once(*name).chain(args.iter().copied()).map(str::to_string).collect()).unwrap_or_default()
}

fn tool_description(tool: Tool, language: Language, mode: ToolMode) -> (&'static str, String) {
    match tool {
        Tool::Server => {
            let names: Vec<&str> = servers::spec(language).map(|s| s.candidates.iter().map(|(n, _)| *n).collect()).unwrap_or_default();
            let detail = match mode {
                ToolMode::Off => "No completions, diagnostics or navigation from a server.".to_string(),
                ToolMode::Custom => "Runs this command in the project; it talks LSP over stdin and stdout.".to_string(),
                _ => format!("The first one installed: {}.", names.join(", ")),
            };
            ("Which server", format!("{detail} Open files pick this up after Restart Language Server."))
        }
        Tool::Formatter => {
            let names: Vec<&str> = format::candidates(language).iter().map(|(n, _)| *n).collect();
            let detail = match mode {
                ToolMode::Off => "Reformat Code does nothing for this language.".to_string(),
                ToolMode::LanguageServer => "Always asks the language server.".to_string(),
                ToolMode::Custom => "Reads the file on stdin and writes the result to stdout; {file} is its path.".to_string(),
                ToolMode::Automatic if names.is_empty() => "The language server, if it can format.".to_string(),
                ToolMode::Automatic => format!("The project's own {}, then the language server, then one on PATH.", names.join(" or ")),
            };
            ("Which formatter", detail)
        }
    }
}

fn strings(items: &[Value]) -> Vec<String> {
    items.iter().filter_map(Value::as_str).map(str::to_string).collect()
}

fn rulers_text(rulers: Option<&[usize]>) -> String {
    rulers.unwrap_or_default().iter().map(usize::to_string).collect::<Vec<_>>().join(", ")
}

fn set_field(field: &Entity<Editor>, text: &str, cx: &mut App) {
    field.update(cx, |e, cx| {
        e.set_text(text, cx);
        e.select_range(text.len()..text.len(), cx);
    });
}

/// Why `settings.json` can't be edited (it doesn't parse), if it can't.
fn settings_file_error() -> Option<String> {
    let text = Settings::path().and_then(|p| std::fs::read_to_string(p).ok())?;
    if text.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(&settings::strip_jsonc(&text)) {
        Ok(Value::Object(_)) => None,
        Ok(_) => Some("it should contain an object".into()),
        Err(err) => Some(err.to_string()),
    }
}

/// Installed font families whose letters are all one width.
fn monospaced_fonts(text_system: &gpui::TextSystem) -> Vec<SharedString> {
    let mut names = text_system.all_font_names();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
        .into_iter()
        // macOS lists private system fonts with a leading dot.
        .filter(|name| !name.starts_with('.'))
        .filter(|name| {
            let id = text_system.resolve_font(&gpui::font(name.clone()));
            let size = px(16.);
            match (text_system.advance(id, size, 'i'), text_system.advance(id, size, 'W')) {
                (Ok(narrow), Ok(wide)) => narrow.width > px(0.) && (narrow.width - wide.width).abs() < px(0.01),
                _ => false,
            }
        })
        .map(SharedString::from)
        .collect()
}

/// Splits a command line into words, honoring quotes ("my app" --flag).
pub fn split_command(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// The reverse of [`split_command`]: quotes words that need it.
pub fn join_command(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if !w.is_empty() && !w.contains(|c: char| c.is_whitespace() || "\"'\\".contains(c)) {
                w.clone()
            } else {
                format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A group of rows under a small heading.
fn section(title: &'static str, rows: Vec<AnyElement>, theme: &Theme) -> Div {
    let rows = rows.into_iter().enumerate().map(|(ix, row)| {
        div().when(ix > 0, |el| el.border_t_1().border_color(theme.border_subtle)).child(row)
    });
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .px_1()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_dim)
                .child(title.to_uppercase()),
        )
        .child(card(theme).children(rows))
}

fn card(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(12.))
        .bg(theme.elevated)
        .border_1()
        .border_color(Hsla::from(theme.border).alpha(0.6))
}

/// Below this window width, a row's control goes under its text instead of
/// beside it: beside, the text would get too little room (and a flex text
/// squeezed far enough breaks a letter per line).
const STACK_BELOW: f32 = 860.;

/// Label and description on the left, the control on the right; `stacked`
/// (a narrow window) puts the control under them, at the right edge.
fn setting_row(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    control: AnyElement,
    reset: Option<AnyElement>,
    stacked: bool,
    theme: &Theme,
) -> Div {
    let text = div()
        .flex()
        .flex_col()
        .gap_0p5()
        // Measured at its own width, then shrunk to fit: with a zero flex
        // basis (`flex_1`) GPUI can keep the text wrapped as if it had no
        // room at all, a letter per line, at some widths.
        .flex_auto()
        .min_w_0()
        // A changed option has a reset button right after its name.
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .min_h(px(20.))
                .child(div().text_color(theme.text).child(label.into()))
                .children(reset),
        )
        .when_some(description, |el, d| el.child(div().text_size(px(12.)).text_color(theme.text_dim).child(d)));
    beside_or_under(text, control, stacked).min_h(px(ROW_HEIGHT)).px_4().py_2p5()
}

/// A row of text with a control at its right edge, or (`stacked`) the
/// control under the text, still at the right.
fn beside_or_under(text: impl IntoElement, control: impl IntoElement, stacked: bool) -> Div {
    let control = div().flex_shrink_0().child(control);
    if stacked {
        div().flex().flex_col().gap_2().child(text).child(div().flex().justify_end().child(control))
    } else {
        div().flex().items_center().gap_4().child(text).child(control)
    }
}

fn dropdown_trigger(id: impl Into<gpui::ElementId>, label: SharedString, width: Pixels, open: bool, theme: &Theme) -> Button {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .w(width)
        .h(px(30.))
        .pl_2p5()
        .pr_2()
        .rounded(px(10.))
        .border_1()
        .border_color(if open { Hsla::from(theme.accent).alpha(0.7) } else { Hsla::from(theme.border).alpha(0.75) })
        .when(open, |el| el.shadow(focus_glow(theme)))
        .cursor_pointer()
        .hover_fill(theme.background, theme.hover)
        .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(label))
        .child(icon_el(Icon::ChevronDown, px(13.), theme.text_dim.into()))
}

/// A row that goes somewhere (search results pointing into a page).
fn jump_row(
    id: impl Into<gpui::ElementId>,
    icon: Icon,
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    theme: &Theme,
) -> Button {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .h(px(44.))
        .px_4()
        .rounded(px(9.))
        .cursor_pointer()
        .hover_bg(theme.hover)
        .child(icon_el(icon, px(14.), theme.text_dim.into()))
        .child(div().text_color(theme.text).child(label.into()))
        .child(div().flex_1().text_size(px(12.)).text_color(theme.text_dim).child(detail.into()))
        .child(icon_el(Icon::ChevronRight, px(13.), theme.text_dim.into()))
}

fn link_row(id: &'static str, icon: Icon, label: &'static str, theme: &Theme) -> Button {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(28.))
        .px_2p5()
        .rounded(px(8.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme.text_muted)
        .hover_bg(theme.hover)
        .hover(|s| s.text_color(theme.text))
        .child(icon_el(icon, px(13.), theme.text_dim.into()))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_joins_commands() {
        assert_eq!(split_command("  pylsp  "), vec!["pylsp"]);
        assert_eq!(split_command(r#"black --quiet "-""#), vec!["black", "--quiet", "-"]);
        assert_eq!(split_command(r#""/Applications/My App/bin" --x 'a b'"#), vec!["/Applications/My App/bin", "--x", "a b"]);
        assert_eq!(split_command(r#"a\ b "c\"d" """#), vec!["a b", "c\"d", ""]);
        assert!(split_command("   ").is_empty());
        for words in [vec!["fish", "--login"], vec!["/My App/x", "say \"hi\"", ""], vec!["{file}"]] {
            let words: Vec<String> = words.into_iter().map(str::to_string).collect();
            assert_eq!(split_command(&join_command(&words)), words);
        }
    }

    #[test]
    fn search_finds_settings_by_any_word() {
        let find = |q: &str| {
            let words: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
            Setting::ALL.into_iter().filter(|s| s.matches(&words)).collect::<Vec<_>>()
        };
        assert_eq!(find("tab size"), vec![Setting::TabSize]);
        assert!(find("font").contains(&Setting::FontFamily) && find("font").contains(&Setting::LineHeight));
        assert_eq!(find("zsh"), vec![Setting::Shell]);
        assert!(find("nothing like this").is_empty());
    }
}
