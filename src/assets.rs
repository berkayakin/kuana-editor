//! Icons and fonts are compiled into the binary so the app is a single file.

use std::borrow::Cow;

use gpui::{App, AssetSource, Result, SharedString};

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(
            (concat!("icons/", $name, ".svg"), include_bytes!(concat!("../assets/icons/", $name, ".svg"))),
        )*]
    };
}

static ICONS: &[(&str, &[u8])] = icons![
    "braces",
    "chevron-down",
    "chevron-right",
    "circle-alert",
    "shield-alert",
    "shield-check",
    "circle-check",
    "circle-x",
    "code-xml",
    "file",
    "file-code",
    "file-plus",
    "file-text",
    "folder",
    "folder-open",
    "folder-search",
    "panel-bottom",
    "panel-left",
    "panel-right",
    "menu",
    "triangle-alert",
    "search",
    "chevron-up",
    "replace",
    "command",
    "history",
    "text-search",
    "hash",
    "palette",
    "undo",
    "redo",
    "git-branch",
    "git-commit",
    "git-compare",
    "arrow-up",
    "arrow-right",
    "pause",
    "step-over",
    "step-into",
    "step-out",
    "bug",
    "puzzle",
    "arrow-down",
    "refresh-cw",
    "plus",
    "minus",
    "check",
    "square",
    "square-check",
    "square-minus",
    "folder-tree",
    "copy",
    "pencil",
    "clipboard",
    "trash",
    "folder-plus",
    "eye",
    "eye-off",
    "chevrons-down-up",
    "x",
    "download",
    "link",
    "cloud",
    "archive",
    "tag",
    "external-link",
    "keyboard",
    "save",
    "terminal",
    "pin",
    "columns-2",
    "rows-2",
    "play",
    "settings",
    "type",
    "stop",
];

/// The app logo, sized for the 96pt welcome-screen mark (2x pixels).
pub const LOGO: &str = "app-icon/logo-192.png";

static IMAGES: &[(&str, &[u8])] = &[(LOGO, include_bytes!("../assets/app-icon/logo-192.png"))];

/// Vidaloka (SIL Open Font License, see `assets/fonts/OFL.txt`), used for the app name.
pub const TITLE_FONT: &str = "Vidaloka";

pub fn load_fonts(cx: &App) {
    let fonts = vec![Cow::Borrowed(&include_bytes!("../assets/fonts/Vidaloka-Regular.ttf")[..])];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("couldn't load bundled fonts: {e}");
    }
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .chain(IMAGES)
            .chain(crate::file_icons::FILE_ICONS)
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .chain(IMAGES)
            .chain(crate::file_icons::FILE_ICONS)
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    ChevronDown,
    ChevronRight,
    CircleAlert,
    ShieldAlert,
    ShieldCheck,
    CircleCheck,
    CircleX,
    CodeXml,
    Braces,
    File,
    FileCode,
    FilePlus,
    FileText,
    Folder,
    FolderOpen,
    FolderSearch,
    PanelBottom,
    PanelLeft,
    PanelRight,
    Menu,
    TriangleAlert,
    Search,
    ChevronUp,
    Replace,
    Command,
    History,
    TextSearch,
    Hash,
    Palette,
    Undo,
    Redo,
    GitBranch,
    GitCommit,
    GitCompare,
    ArrowUp,
    ArrowRight,
    Pause,
    StepOver,
    StepInto,
    StepOut,
    Bug,
    Puzzle,
    ArrowDown,
    Refresh,
    Plus,
    Minus,
    Check,
    Square,
    SquareCheck,
    SquareMinus,
    FolderTree,
    Copy,
    Pencil,
    Clipboard,
    Trash,
    FolderPlus,
    Eye,
    EyeOff,
    ChevronsDownUp,
    X,
    Download,
    Link,
    Cloud,
    Archive,
    Tag,
    ExternalLink,
    Keyboard,
    Save,
    Terminal,
    Pin,
    SplitRight,
    SplitDown,
    Play,
    Settings,
    Type,
    Stop,
}

impl Icon {
    pub fn path(self) -> &'static str {
        match self {
            Icon::ChevronDown => "icons/chevron-down.svg",
            Icon::ChevronRight => "icons/chevron-right.svg",
            Icon::CircleAlert => "icons/circle-alert.svg",
            Icon::ShieldAlert => "icons/shield-alert.svg",
            Icon::ShieldCheck => "icons/shield-check.svg",
            Icon::CircleCheck => "icons/circle-check.svg",
            Icon::CircleX => "icons/circle-x.svg",
            Icon::CodeXml => "icons/code-xml.svg",
            Icon::Braces => "icons/braces.svg",
            Icon::File => "icons/file.svg",
            Icon::FileCode => "icons/file-code.svg",
            Icon::FilePlus => "icons/file-plus.svg",
            Icon::FileText => "icons/file-text.svg",
            Icon::Folder => "icons/folder.svg",
            Icon::FolderOpen => "icons/folder-open.svg",
            Icon::FolderSearch => "icons/folder-search.svg",
            Icon::PanelBottom => "icons/panel-bottom.svg",
            Icon::PanelLeft => "icons/panel-left.svg",
            Icon::PanelRight => "icons/panel-right.svg",
            Icon::Menu => "icons/menu.svg",
            Icon::TriangleAlert => "icons/triangle-alert.svg",
            Icon::Search => "icons/search.svg",
            Icon::ChevronUp => "icons/chevron-up.svg",
            Icon::Replace => "icons/replace.svg",
            Icon::Command => "icons/command.svg",
            Icon::History => "icons/history.svg",
            Icon::TextSearch => "icons/text-search.svg",
            Icon::Hash => "icons/hash.svg",
            Icon::Palette => "icons/palette.svg",
            Icon::Undo => "icons/undo.svg",
            Icon::Redo => "icons/redo.svg",
            Icon::GitBranch => "icons/git-branch.svg",
            Icon::GitCommit => "icons/git-commit.svg",
            Icon::GitCompare => "icons/git-compare.svg",
            Icon::ArrowUp => "icons/arrow-up.svg",
            Icon::ArrowRight => "icons/arrow-right.svg",
            Icon::Pause => "icons/pause.svg",
            Icon::StepOver => "icons/step-over.svg",
            Icon::StepInto => "icons/step-into.svg",
            Icon::StepOut => "icons/step-out.svg",
            Icon::Bug => "icons/bug.svg",
            Icon::Puzzle => "icons/puzzle.svg",
            Icon::ArrowDown => "icons/arrow-down.svg",
            Icon::Refresh => "icons/refresh-cw.svg",
            Icon::Plus => "icons/plus.svg",
            Icon::Minus => "icons/minus.svg",
            Icon::Check => "icons/check.svg",
            Icon::Square => "icons/square.svg",
            Icon::SquareCheck => "icons/square-check.svg",
            Icon::SquareMinus => "icons/square-minus.svg",
            Icon::FolderTree => "icons/folder-tree.svg",
            Icon::Copy => "icons/copy.svg",
            Icon::Pencil => "icons/pencil.svg",
            Icon::Clipboard => "icons/clipboard.svg",
            Icon::Trash => "icons/trash.svg",
            Icon::FolderPlus => "icons/folder-plus.svg",
            Icon::Eye => "icons/eye.svg",
            Icon::EyeOff => "icons/eye-off.svg",
            Icon::ChevronsDownUp => "icons/chevrons-down-up.svg",
            Icon::X => "icons/x.svg",
            Icon::Download => "icons/download.svg",
            Icon::Link => "icons/link.svg",
            Icon::Cloud => "icons/cloud.svg",
            Icon::Archive => "icons/archive.svg",
            Icon::Tag => "icons/tag.svg",
            Icon::ExternalLink => "icons/external-link.svg",
            Icon::Keyboard => "icons/keyboard.svg",
            Icon::Save => "icons/save.svg",
            Icon::Terminal => "icons/terminal.svg",
            Icon::Pin => "icons/pin.svg",
            Icon::SplitRight => "icons/columns-2.svg",
            Icon::SplitDown => "icons/rows-2.svg",
            Icon::Play => "icons/play.svg",
            Icon::Settings => "icons/settings.svg",
            Icon::Type => "icons/type.svg",
            Icon::Stop => "icons/stop.svg",
        }
    }
}
