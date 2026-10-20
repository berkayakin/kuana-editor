//! Themes are JSON files. Built-in themes live in `assets/themes/` and are
//! compiled in; user themes are loaded from the config directory
//! (`~/.config/kuana/themes`, or `%APPDATA%\kuana\themes` on Windows).
//!
//! A theme only has to specify what it changes: anything missing is taken
//! from the theme named in `"extends"`, or from the default theme of the
//! same appearance ("Clay Black" / "Clay Light").

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{App, Global, Hsla, Rgba, SharedString, rgba};
use serde::Deserialize;

use crate::{settings::Settings, state::SessionState, syntax::Highlight};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    Dark,
    Light,
}

#[derive(Clone)]
pub struct SyntaxColors {
    pub plain: Rgba,
    pub keyword: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub constant: Rgba,
    pub function: Rgba,
    pub method: Rgba,
    pub property: Rgba,
    pub type_: Rgba,
    pub variable_builtin: Rgba,
    pub comment: Rgba,
    pub operator: Rgba,
    pub punctuation: Rgba,
    pub tag: Rgba,
    pub attribute: Rgba,
    pub escape: Rgba,
}

#[derive(Clone)]
pub struct Theme {
    pub name: SharedString,
    pub appearance: Appearance,
    pub italic_comments: bool,
    /// The theme file's `deep_chrome`: keep its chrome as dark as written
    /// instead of capping it at `MAX_CHROME_STEP` from the islands.
    pub deep_chrome: bool,
    /// From the `visual_effects` setting (not theme files): draw gradients
    /// (Full only).
    pub gradients: bool,
    /// From the `visual_effects` setting: no shadows, glows or animations
    /// (Minimal).
    pub reduce_effects: bool,
    /// From the `ui_style` setting: the Classic layout (square areas between
    /// separator lines, rectangular tabs) instead of islands.
    pub classic: bool,

    pub ui_font: SharedString,
    pub mono_font: SharedString,
    pub ui_font_size: f32,
    pub editor_font_size: f32,
    pub editor_line_height: f32,

    pub background: Rgba,
    pub panel: Rgba,
    pub elevated: Rgba,
    pub border: Rgba,
    pub border_subtle: Rgba,
    pub hover: Rgba,
    pub active: Rgba,

    pub text: Rgba,
    pub text_muted: Rgba,
    pub text_dim: Rgba,
    pub accent: Rgba,
    pub accent_soft: Rgba,

    pub cursor: Rgba,
    pub selection: Rgba,
    pub selection_inactive: Rgba,
    pub current_line: Rgba,
    pub line_number: Rgba,
    pub line_number_active: Rgba,
    pub indent_guide: Rgba,
    pub search_match: Rgba,
    pub search_active: Rgba,
    pub bracket_match: Rgba,

    pub error: Rgba,
    pub warning: Rgba,
    pub info: Rgba,
    pub success: Rgba,

    pub git_added: Rgba,
    pub git_modified: Rgba,
    pub git_deleted: Rgba,
    pub git_untracked: Rgba,
    pub git_conflict: Rgba,

    /// The terminal's 16 ANSI colors.
    pub terminal_black: Rgba,
    pub terminal_red: Rgba,
    pub terminal_green: Rgba,
    pub terminal_yellow: Rgba,
    pub terminal_blue: Rgba,
    pub terminal_magenta: Rgba,
    pub terminal_cyan: Rgba,
    pub terminal_white: Rgba,
    pub terminal_bright_black: Rgba,
    pub terminal_bright_red: Rgba,
    pub terminal_bright_green: Rgba,
    pub terminal_bright_yellow: Rgba,
    pub terminal_bright_blue: Rgba,
    pub terminal_bright_magenta: Rgba,
    pub terminal_bright_cyan: Rgba,
    pub terminal_bright_white: Rgba,

    pub syntax: SyntaxColors,
    /// Colors for capture names besides the base kinds ("keyword.control",
    /// "string.regex"), from the theme's `syntax`. Shared: views copy the
    /// theme to draw, many times a frame.
    pub syntax_names: Arc<HashMap<String, Rgba>>,
}

impl Global for Theme {}

/// Maps JSON keys to theme fields, so the key list lives in one place.
macro_rules! color_keys {
    ($($key:literal => $($field:ident).+),* $(,)?) => {
        pub const COLOR_KEYS: &[&str] = &[$($key),*];

        fn color_mut<'a>(theme: &'a mut Theme, key: &str) -> Option<&'a mut Rgba> {
            match key {
                $($key => Some(&mut theme.$($field).+),)*
                _ => None,
            }
        }
    };
}

color_keys! {
    "background" => background,
    "panel" => panel,
    "elevated" => elevated,
    "border" => border,
    "border_subtle" => border_subtle,
    "hover" => hover,
    "active" => active,
    "text" => text,
    "text_muted" => text_muted,
    "text_dim" => text_dim,
    "accent" => accent,
    "accent_soft" => accent_soft,
    "cursor" => cursor,
    "selection" => selection,
    "selection_inactive" => selection_inactive,
    "current_line" => current_line,
    "line_number" => line_number,
    "line_number_active" => line_number_active,
    "indent_guide" => indent_guide,
    "search_match" => search_match,
    "search_active" => search_active,
    "bracket_match" => bracket_match,
    "error" => error,
    "warning" => warning,
    "info" => info,
    "success" => success,
    "git_added" => git_added,
    "git_modified" => git_modified,
    "git_deleted" => git_deleted,
    "git_untracked" => git_untracked,
    "git_conflict" => git_conflict,
    "terminal_black" => terminal_black,
    "terminal_red" => terminal_red,
    "terminal_green" => terminal_green,
    "terminal_yellow" => terminal_yellow,
    "terminal_blue" => terminal_blue,
    "terminal_magenta" => terminal_magenta,
    "terminal_cyan" => terminal_cyan,
    "terminal_white" => terminal_white,
    "terminal_bright_black" => terminal_bright_black,
    "terminal_bright_red" => terminal_bright_red,
    "terminal_bright_green" => terminal_bright_green,
    "terminal_bright_yellow" => terminal_bright_yellow,
    "terminal_bright_blue" => terminal_bright_blue,
    "terminal_bright_magenta" => terminal_bright_magenta,
    "terminal_bright_cyan" => terminal_bright_cyan,
    "terminal_bright_white" => terminal_bright_white,
    "syntax.plain" => syntax.plain,
    "syntax.keyword" => syntax.keyword,
    "syntax.string" => syntax.string,
    "syntax.number" => syntax.number,
    "syntax.constant" => syntax.constant,
    "syntax.function" => syntax.function,
    "syntax.method" => syntax.method,
    "syntax.property" => syntax.property,
    "syntax.type" => syntax.type_,
    "syntax.variable_builtin" => syntax.variable_builtin,
    "syntax.comment" => syntax.comment,
    "syntax.operator" => syntax.operator,
    "syntax.punctuation" => syntax.punctuation,
    "syntax.tag" => syntax.tag,
    "syntax.attribute" => syntax.attribute,
    "syntax.escape" => syntax.escape,
}

impl Theme {
    pub fn get(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }

    #[cfg(test)]
    pub fn blank_for_tests() -> Self {
        Self::blank(Appearance::Dark)
    }

    /// Neutral starting point before a theme file is applied.
    fn blank(appearance: Appearance) -> Self {
        let (bg, fg) = match appearance {
            Appearance::Dark => (rgba(0x1e1e1eff), rgba(0xe0e0e0ff)),
            Appearance::Light => (rgba(0xffffffff), rgba(0x202020ff)),
        };
        let syntax = SyntaxColors {
            plain: fg,
            keyword: fg,
            string: fg,
            number: fg,
            constant: fg,
            function: fg,
            method: fg,
            property: fg,
            type_: fg,
            variable_builtin: fg,
            comment: fg,
            operator: fg,
            punctuation: fg,
            tag: fg,
            attribute: fg,
            escape: fg,
        };
        Self {
            name: "".into(),
            appearance,
            italic_comments: true,
            deep_chrome: false,
            gradients: true,
            reduce_effects: false,
            classic: false,
            ui_font: ".SystemUIFont".into(),
            mono_font: "Menlo".into(),
            ui_font_size: 13.0,
            editor_font_size: DEFAULT_EDITOR_FONT_SIZE,
            editor_line_height: 20.0,
            background: bg,
            panel: bg,
            elevated: bg,
            border: fg,
            border_subtle: fg,
            hover: rgba(0x80808020),
            active: rgba(0x80808040),
            text: fg,
            text_muted: fg,
            text_dim: fg,
            accent: fg,
            accent_soft: rgba(0x80808040),
            cursor: fg,
            selection: rgba(0x80808060),
            selection_inactive: rgba(0x80808040),
            current_line: rgba(0x80808010),
            line_number: fg,
            line_number_active: fg,
            indent_guide: rgba(0x80808020),
            search_match: rgba(0xffcc0040),
            search_active: rgba(0xffcc0080),
            bracket_match: rgba(0x80808040),
            error: rgba(0xff5555ff),
            warning: rgba(0xffaa00ff),
            info: rgba(0x55aaffff),
            success: rgba(0x55cc55ff),
            git_added: rgba(0x55cc55ff),
            git_modified: rgba(0x55aaffff),
            git_deleted: rgba(0xff5555ff),
            git_untracked: rgba(0xcc8855ff),
            git_conflict: rgba(0xffaa00ff),
            terminal_black: rgba(0x2b2b2bff),
            terminal_red: rgba(0xe06c75ff),
            terminal_green: rgba(0x98c379ff),
            terminal_yellow: rgba(0xe5c07bff),
            terminal_blue: rgba(0x61afefff),
            terminal_magenta: rgba(0xc678ddff),
            terminal_cyan: rgba(0x56b6c2ff),
            terminal_white: rgba(0xc8c8c8ff),
            terminal_bright_black: rgba(0x6b6b6bff),
            terminal_bright_red: rgba(0xef8a92ff),
            terminal_bright_green: rgba(0xb0d48fff),
            terminal_bright_yellow: rgba(0xefcf94ff),
            terminal_bright_blue: rgba(0x85c1f5ff),
            terminal_bright_magenta: rgba(0xd595e6ff),
            terminal_bright_cyan: rgba(0x7ccbd4ff),
            terminal_bright_white: rgba(0xffffffff),
            syntax,
            syntax_names: Arc::default(),
        }
    }

    /// The color for a capture: the theme's color for the longest prefix of
    /// its name ("keyword.control.return" → "keyword.control"), stopping at
    /// the level Kuana's base kinds cover, whose color is then used.
    pub fn highlight_color(&self, h: Highlight) -> Hsla {
        if !self.syntax_names.is_empty() {
            let mut name = h.name();
            while !name.is_empty() {
                if let Some(color) = self.syntax_names.get(name) {
                    return (*color).into();
                }
                if Highlight::base_at(name).is_some() {
                    break;
                }
                name = name.rfind('.').map_or("", |dot| &name[..dot]);
            }
        }
        let s = &self.syntax;
        match h.base() {
            Highlight::None | Highlight::Variable => s.plain,
            Highlight::Keyword => s.keyword,
            Highlight::String => s.string,
            Highlight::Number => s.number,
            Highlight::Constant => s.constant,
            Highlight::Function => s.function,
            Highlight::Method => s.method,
            Highlight::Property => s.property,
            Highlight::Type => s.type_,
            Highlight::VariableBuiltin => s.variable_builtin,
            Highlight::Comment => s.comment,
            Highlight::Operator => s.operator,
            Highlight::Punctuation => s.punctuation,
            Highlight::Tag => s.tag,
            Highlight::Attribute => s.attribute,
            Highlight::Escape => s.escape,
            _ => s.plain,
        }
        .into()
    }

    fn with_fonts(mut self, from: &Theme) -> Self {
        self.ui_font = from.ui_font.clone();
        self.mono_font = from.mono_font.clone();
        self.ui_font_size = from.ui_font_size;
        self.editor_font_size = from.editor_font_size;
        self.editor_line_height = from.editor_line_height;
        self
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
pub fn parse_color(s: &str) -> Option<Rgba> {
    let hex = s.trim().strip_prefix('#')?;
    let expanded: String = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).chain("ff".chars()).collect(),
        6 => format!("{hex}ff"),
        8 => hex.to_string(),
        _ => return None,
    };
    u32::from_str_radix(&expanded, 16).ok().map(rgba)
}

// ---- Theme files -------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
pub struct ThemeFile {
    pub name: String,
    #[serde(default)]
    pub appearance: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub italic_comments: Option<bool>,
    #[serde(default)]
    pub deep_chrome: Option<bool>,
    #[serde(default)]
    pub colors: HashMap<String, String>,
    #[serde(default)]
    pub syntax: HashMap<String, String>,
}

impl ThemeFile {
    fn appearance(&self) -> Option<Appearance> {
        match self.appearance.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("light") => Some(Appearance::Light),
            Some("dark") => Some(Appearance::Dark),
            _ => None,
        }
    }
}

static BUILTIN: &[&str] = &[
    include_str!("../assets/themes/clay-black.json"),
    include_str!("../assets/themes/onyx.json"),
    include_str!("../assets/themes/clay-dark.json"),
    include_str!("../assets/themes/camellia-light.json"),
    include_str!("../assets/themes/clay-light.json"),
    include_str!("../assets/themes/one-dark.json"),
    include_str!("../assets/themes/dracula.json"),
    include_str!("../assets/themes/nord.json"),
    include_str!("../assets/themes/tokyo-night.json"),
    include_str!("../assets/themes/gruvbox-dark.json"),
    include_str!("../assets/themes/catppuccin-mocha.json"),
    include_str!("../assets/themes/rose-pine.json"),
    include_str!("../assets/themes/solarized-dark.json"),
    include_str!("../assets/themes/ember-soft.json"),
    include_str!("../assets/themes/ember.json"),
    include_str!("../assets/themes/everforest-dark.json"),
    include_str!("../assets/themes/kanagawa.json"),
    include_str!("../assets/themes/ayu-dark.json"),
    include_str!("../assets/themes/night-owl.json"),
    include_str!("../assets/themes/poimandres.json"),
    include_str!("../assets/themes/oxocarbon.json"),
    include_str!("../assets/themes/synthwave-84.json"),
    include_str!("../assets/themes/vesper.json"),
    include_str!("../assets/themes/horizon.json"),
    include_str!("../assets/themes/vitesse-dark.json"),
    include_str!("../assets/themes/cobalt2.json"),
    include_str!("../assets/themes/sunset-fig.json"),
    include_str!("../assets/themes/campfire.json"),
    include_str!("../assets/themes/olive-grove.json"),
    include_str!("../assets/themes/lighthouse-night.json"),
    include_str!("../assets/themes/ultraviolet.json"),
    include_str!("../assets/themes/pistachio-night.json"),
    include_str!("../assets/themes/mulberry.json"),
    include_str!("../assets/themes/camellia.json"),
    include_str!("../assets/themes/firefly.json"),
    include_str!("../assets/themes/lagoon.json"),
    include_str!("../assets/themes/blue-hour.json"),
    include_str!("../assets/themes/merlot.json"),
    include_str!("../assets/themes/solarized-light.json"),
    include_str!("../assets/themes/catppuccin-latte.json"),
    include_str!("../assets/themes/rose-pine-dawn.json"),
    include_str!("../assets/themes/gruvbox-light.json"),
    include_str!("../assets/themes/one-light.json"),
    include_str!("../assets/themes/sherbet.json"),
    include_str!("../assets/themes/campfire-light.json"),
    include_str!("../assets/themes/peach-blossom.json"),
    include_str!("../assets/themes/lighthouse.json"),
    include_str!("../assets/themes/pistachio.json"),
    include_str!("../assets/themes/blue-hour-light.json"),
    include_str!("../assets/themes/merlot-light.json"),
];

/// The theme used when settings name none.
pub const DEFAULT_THEME: &str = "Ember";
/// What themes without `extends` fill missing colors from, per appearance.
pub const DEFAULT_DARK: &str = "Clay Black";
pub const DEFAULT_LIGHT: &str = "Clay Light";

pub struct ThemeEntry {
    pub theme: Arc<Theme>,
    /// `Some` for themes loaded from the user's themes folder.
    pub path: Option<PathBuf>,
}

/// All available themes, resolved and ready to apply.
pub struct ThemeRegistry {
    pub entries: Vec<ThemeEntry>,
    /// Problems found while loading user themes.
    pub errors: Vec<String>,
}

impl Global for ThemeRegistry {}

impl ThemeRegistry {
    pub fn load() -> Self {
        let mut files: Vec<(ThemeFile, Option<PathBuf>)> = Vec::new();
        let mut errors = Vec::new();
        for source in BUILTIN {
            let file: ThemeFile = serde_json::from_str(source).expect("invalid built-in theme");
            files.push((file, None));
        }
        // Extensions' themes, then the user's own, which win.
        let mut dirs = crate::extensions::theme_dirs();
        dirs.extend(themes_dir());
        for dir in dirs {
            let Ok(read) = std::fs::read_dir(&dir) else { continue };
            let mut paths: Vec<PathBuf> = read
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            paths.sort();
            for path in paths {
                // Comments and trailing commas are fine (Zed's theme files have them).
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => crate::settings::strip_jsonc(&text),
                    Err(e) => {
                        errors.push(format!("{}: {e}", file_label(&path)));
                        continue;
                    }
                };
                // A Zed theme family (several themes), else Kuana's format.
                let parsed = match zed_theme_family(&text) {
                    Some(family) => Ok(family),
                    None => serde_json::from_str::<ThemeFile>(&text).map(|file| vec![file]).map_err(|e| e.to_string()),
                };
                match parsed {
                    Ok(themes) => {
                        for file in themes {
                            // A theme with a built-in's name replaces it.
                            files.retain(|(f, _)| f.name != file.name);
                            files.push((file, Some(path.clone())));
                        }
                    }
                    Err(e) => errors.push(format!("{}: {e}", file_label(&path))),
                }
            }
        }

        let by_name: HashMap<String, ThemeFile> =
            files.iter().map(|(f, _)| (f.name.clone(), f.clone())).collect();
        let mut entries = Vec::new();
        for (file, path) in &files {
            if let Some(theme) = resolve(file, &by_name, &mut Vec::new(), &mut errors) {
                entries.push(ThemeEntry {
                    theme: Arc::new(theme),
                    path: path.clone(),
                });
            }
        }
        errors.dedup();
        Self { entries, errors }
    }

    pub fn find(&self, name: &str) -> Option<&ThemeEntry> {
        self.entries.iter().find(|e| e.theme.name.as_ref() == name)
    }
}

/// Zed's theme files: a family of themes, each with a `style` of dotted keys
/// (`editor.background`), `players` (cursor, selection) and a `syntax` of
/// `{ "color": … }`. What has no counterpart comes from the base theme.
fn zed_theme_family(text: &str) -> Option<Vec<ThemeFile>> {
    let family: serde_json::Value = serde_json::from_str(text).ok()?;
    let themes = family.get("themes")?.as_array()?;
    let mut out = Vec::new();
    for theme in themes {
        let style = theme.get("style")?;
        let color = |key: &str| style.get(key).and_then(|v| v.as_str()).filter(|c| parse_color(c).is_some()).map(str::to_string);
        let mut colors = HashMap::new();
        let mut put = |kuana: &str, zed: &[&str]| {
            if let Some(value) = zed.iter().find_map(|k| color(k)) {
                colors.insert(kuana.to_string(), value);
            }
        };
        put("background", &["editor.background", "background"]);
        put("panel", &["panel.background", "background"]);
        put("elevated", &["elevated_surface.background", "surface.background"]);
        put("border", &["border"]);
        put("border_subtle", &["border.variant", "border"]);
        put("hover", &["element.hover", "ghost_element.hover"]);
        put("active", &["element.selected", "element.active", "ghost_element.selected"]);
        put("text", &["text", "editor.foreground"]);
        put("text_muted", &["text.muted"]);
        put("text_dim", &["text.placeholder", "text.disabled"]);
        put("accent", &["text.accent", "icon.accent"]);
        put("accent_soft", &["element.selected"]);
        put("current_line", &["editor.active_line.background"]);
        put("line_number", &["editor.line_number"]);
        put("line_number_active", &["editor.active_line_number"]);
        put("indent_guide", &["editor.indent_guide"]);
        put("search_match", &["search.match_background"]);
        put("search_active", &["search.match_background"]);
        put("bracket_match", &["editor.document_highlight.bracket_background"]);
        put("error", &["error"]);
        put("warning", &["warning"]);
        put("info", &["info"]);
        put("success", &["success"]);
        put("git_added", &["created", "version_control.added"]);
        put("git_modified", &["modified", "version_control.modified"]);
        put("git_deleted", &["deleted", "version_control.deleted"]);
        put("git_untracked", &["created", "version_control.added"]);
        put("git_conflict", &["conflict", "version_control.conflict"]);
        for name in ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"] {
            put(&format!("terminal_{name}"), &[&format!("terminal.ansi.{name}")]);
            put(&format!("terminal_bright_{name}"), &[&format!("terminal.ansi.bright_{name}")]);
        }
        let player = style.get("players").and_then(|p| p.get(0));
        let player_color = |key: &str| player.and_then(|p| p.get(key)).and_then(|v| v.as_str()).map(str::to_string);
        colors.extend(player_color("cursor").map(|c| ("cursor".to_string(), c)));
        colors.extend(player_color("selection").map(|c| ("selection".to_string(), c)));

        let syntax_style = style.get("syntax");
        let syntax_color = |key: &str| {
            syntax_style.and_then(|s| s.get(key)).and_then(|s| s.get("color")).and_then(|v| v.as_str()).map(str::to_string)
        };
        let mut syntax = HashMap::new();
        for (kuana, zed) in [
            ("plain", &["variable", "primary"][..]),
            ("keyword", &["keyword"]),
            ("string", &["string"]),
            ("number", &["number", "constant"]),
            ("constant", &["constant", "boolean"]),
            ("function", &["function"]),
            ("method", &["function.method", "function"]),
            ("property", &["property"]),
            ("type", &["type"]),
            ("variable_builtin", &["variable.special", "variable"]),
            ("comment", &["comment"]),
            ("operator", &["operator"]),
            ("punctuation", &["punctuation", "punctuation.bracket"]),
            ("tag", &["tag"]),
            ("attribute", &["attribute"]),
            ("escape", &["string.escape", "string.special"]),
        ] {
            if let Some(value) = zed.iter().find_map(|k| syntax_color(k)) {
                syntax.insert(kuana.to_string(), value);
            }
        }
        // And every name the theme colors, for captures more specific than
        // the base kinds.
        for (name, style) in syntax_style.and_then(|s| s.as_object()).into_iter().flatten() {
            if let Some(color) = style.get("color").and_then(|v| v.as_str()) {
                syntax.entry(name.clone()).or_insert_with(|| color.to_string());
            }
        }
        out.push(ThemeFile {
            name: theme.get("name")?.as_str()?.to_string(),
            appearance: theme.get("appearance").and_then(|a| a.as_str()).map(str::to_string),
            extends: None,
            italic_comments: None,
            deep_chrome: None,
            colors,
            syntax,
        });
    }
    (!out.is_empty()).then_some(out)
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn resolve(
    file: &ThemeFile,
    all: &HashMap<String, ThemeFile>,
    stack: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> Option<Theme> {
    if stack.contains(&file.name) || stack.len() > 8 {
        errors.push(format!("{}: circular \"extends\"", file.name));
        return None;
    }
    stack.push(file.name.clone());

    let parent_name = match &file.extends {
        Some(parent) => Some(parent.clone()),
        None => {
            let default = match file.appearance().unwrap_or(Appearance::Dark) {
                Appearance::Dark => DEFAULT_DARK,
                Appearance::Light => DEFAULT_LIGHT,
            };
            (file.name != default).then(|| default.to_string())
        }
    };
    let mut theme = match parent_name.as_deref().and_then(|p| all.get(p)) {
        Some(parent) => resolve(parent, all, stack, errors)?,
        None => {
            if let Some(p) = &file.extends {
                errors.push(format!("{}: extends unknown theme \"{p}\"", file.name));
            }
            Theme::blank(file.appearance().unwrap_or(Appearance::Dark))
        }
    };
    stack.pop();

    theme.name = file.name.clone().into();
    if let Some(a) = file.appearance() {
        theme.appearance = a;
    }
    if let Some(i) = file.italic_comments {
        theme.italic_comments = i;
    }
    if let Some(d) = file.deep_chrome {
        theme.deep_chrome = d;
    }
    let entries = file
        .colors
        .iter()
        .map(|(k, v)| (k.clone(), v))
        .chain(file.syntax.iter().map(|(k, v)| (format!("syntax.{k}"), v)));
    for (key, value) in entries {
        // Any capture name is a syntax key; only near-misses of the base
        // names ("syntax.keywrd") are reported.
        if let Some(name) = key.strip_prefix("syntax.").filter(|_| color_mut(&mut theme, &key).is_none()) {
            match parse_color(value) {
                Some(_) if !name.contains('.') && Highlight::base_at(name).is_none() && !ZED_SYNTAX_NAMES.contains(&name) && suggest_key(&key).is_some() => {
                    let hint = suggest_key(&key).map(|k| format!(" (did you mean \"{k}\"?)")).unwrap_or_default();
                    errors.push(format!("{}: unknown color \"{key}\"{hint}", file.name));
                }
                Some(color) => {
                    Arc::make_mut(&mut theme.syntax_names).insert(name.to_string(), color);
                }
                None => errors.push(format!("{}: invalid color \"{value}\" for \"{key}\"", file.name)),
            }
            continue;
        }
        match (color_mut(&mut theme, &key), parse_color(value)) {
            (Some(slot), Some(color)) => *slot = color,
            (None, _) => {
                let hint = suggest_key(&key).map(|k| format!(" (did you mean \"{k}\"?)")).unwrap_or_default();
                errors.push(format!("{}: unknown color \"{key}\"{hint}", file.name));
            }
            (_, None) => errors.push(format!("{}: invalid color \"{value}\" for \"{key}\"", file.name)),
        }
    }
    // Once, on the theme as a whole (a parent's own result is still edited above).
    if stack.is_empty() {
        cap_chrome_step(&mut theme);
    }
    Some(theme)
}

/// How far, in perceived lightness (CIE L*), a dark theme's chrome may be
/// from its islands. Dark themes often make their panels much darker than the
/// editor (Cobalt2 by 8): around every island that reads as a dark trench, and
/// a window full of them is tiring to look at. Light themes keep theirs.
pub const MAX_CHROME_STEP: f32 = 2.5;

/// Moves a dark theme's panel (the chrome) toward its background until the two
/// are at most `MAX_CHROME_STEP` apart, keeping the theme's own hue between them.
fn cap_chrome_step(theme: &mut Theme) {
    if theme.appearance != Appearance::Dark || theme.deep_chrome {
        return;
    }
    let (panel, island) = (theme.panel, theme.background);
    let step = |c: gpui::Rgba| (lightness(c) - lightness(island)).abs();
    if step(panel) <= MAX_CHROME_STEP {
        return;
    }
    let mix = |t: f32| gpui::Rgba {
        r: panel.r + (island.r - panel.r) * t,
        g: panel.g + (island.g - panel.g) * t,
        b: panel.b + (island.b - panel.b) * t,
        a: 1.,
    };
    // The step shrinks steadily toward the island color: find where it's the cap.
    let (mut far, mut near) = (0f32, 1f32);
    for _ in 0..24 {
        let t = (far + near) / 2.;
        if step(mix(t)) > MAX_CHROME_STEP { far = t } else { near = t }
    }
    theme.panel = mix(near);
}

/// CIE L* (0 black – 100 white) of an sRGB color.
pub fn lightness(c: gpui::Rgba) -> f32 {
    let linear = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    let y = 0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b);
    if y > 0.008856 { 116. * y.cbrt() - 16. } else { 903.3 * y }
}

/// The known key closest to a misspelled one, if any is close enough.
/// Syntax names Zed's themes color that aren't Kuana's base kinds. They're
/// close to some ("title" to "type"), but meant: never report them as typos.
const ZED_SYNTAX_NAMES: &[&str] =
    &["emphasis", "enum", "hint", "link_text", "link_uri", "namespace", "predictive", "preproc", "primary", "selector", "title", "variant"];

fn suggest_key(key: &str) -> Option<&'static str> {
    closest(key, COLOR_KEYS.iter().copied())
}

/// The candidate closest to a misspelled word (edit distance ≤ 3), for
/// "did you mean" hints.
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    fn distance(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut row = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                row.push((prev[j] + (ca != *cb) as usize).min(prev[j + 1] + 1).min(row[j] + 1));
            }
            prev = row;
        }
        prev[b.len()]
    }
    candidates
        .into_iter()
        .map(|k| (distance(word, k), k))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// A path for display, with the home directory shortened to `~`.
pub fn display_path(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap().display())
        }
        _ => path.display().to_string(),
    }
}

// ---- Settings ------------------------------------------------------------------

/// Where user configuration lives: `~/.config/kuana` (or `$XDG_CONFIG_HOME/kuana`)
/// on macOS and Linux, `%APPDATA%\kuana` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("kuana"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("kuana"))
}

/// Kuana was called Kin up to 0.1.6. Its config folder (settings, sessions,
/// installed servers and extensions) moves to the new name the first time
/// Kuana starts, unless a `kuana` folder is already there.
pub fn move_legacy_config() {
    let Some(dir) = config_dir() else { return };
    let legacy = dir.with_file_name("kin");
    if !dir.exists()
        && legacy.is_dir()
        && let Err(err) = std::fs::rename(&legacy, &dir)
    {
        eprintln!("kuana: couldn't move {} to {}: {err}", legacy.display(), dir.display());
    }
}

pub fn themes_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("themes"))
}

/// Makes `name` the active theme, keeping the current fonts.
pub fn apply_theme(name: &str, cx: &mut App) -> bool {
    let Some(theme) = cx
        .try_global::<ThemeRegistry>()
        .and_then(|r| r.find(name))
        .map(|e| (*e.theme).clone())
    else {
        return false;
    };
    let theme = match cx.try_global::<Theme>() {
        Some(current) => theme.with_fonts(current),
        None => theme,
    };
    cx.set_global(theme);
    cx.refresh_windows();
    true
}

/// Rescans theme files and re-applies the active theme so edits show up.
pub fn reload_themes(cx: &mut App) {
    let current = Theme::get(cx).name.to_string();
    cx.set_global(ThemeRegistry::load());
    if !apply_theme(&current, cx) {
        apply_theme(DEFAULT_THEME, cx);
    }
}

/// Writes a starter theme into the user's themes folder (if it has none)
/// and returns the folder.
pub fn ensure_themes_dir() -> std::io::Result<PathBuf> {
    let dir = themes_dir().ok_or_else(|| std::io::Error::other("no config directory"))?;
    std::fs::create_dir_all(&dir)?;
    let has_theme = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|x| x == "json"));
    if !has_theme {
        std::fs::write(dir.join("my-theme.json"), EXAMPLE_THEME)?;
    }
    Ok(dir)
}

const EXAMPLE_THEME: &str = r##"{
  "name": "My Theme",
  "appearance": "dark",
  "extends": "Clay Dark",
  "colors": {
    "accent": "#7fb4ca",
    "accent_soft": "#7fb4ca2b",
    "cursor": "#7fb4ca",
    "selection": "#7fb4ca40"
  },
  "syntax": {
    "keyword": "#c4a7e7",
    "function": "#9ccfd8"
  }
}
"##;

/// The UI and editor fonts `pick_fonts` chose and the caption buttons' font,
/// kept by `apply_fonts`: listing the installed fonts takes tens of
/// milliseconds, too slow for a render.
struct DefaultFonts(SharedString, SharedString, &'static str);

/// The font with the glyphs of Windows' caption buttons (minimize, maximize,
/// restore, close). Segoe Fluent Icons comes with Windows 11; Windows 10 has
/// the same glyphs at the same code points in Segoe MDL2 Assets, and shows
/// boxes for the other.
pub fn caption_font(cx: &App) -> &'static str {
    cx.try_global::<DefaultFonts>().map_or(FLUENT_ICONS, |fonts| fonts.2)
}

const FLUENT_ICONS: &str = "Segoe Fluent Icons";

fn pick_caption_font(installed: &[String]) -> &'static str {
    if installed.iter().any(|f| f == FLUENT_ICONS) { FLUENT_ICONS } else { "Segoe MDL2 Assets" }
}

impl Global for DefaultFonts {}

/// The UI and editor fonts used when settings name none.
pub fn default_fonts(cx: &App) -> (SharedString, SharedString) {
    match cx.try_global::<DefaultFonts>() {
        Some(fonts) => (fonts.0.clone(), fonts.1.clone()),
        None => pick_fonts(&cx.text_system().all_font_names()),
    }
}

/// Picks the first installed font from a preference list. Keeps the editor
/// looking native on each platform without bundling fonts.
fn pick_fonts(installed: &[String]) -> (SharedString, SharedString) {
    let has = |name: &str| installed.iter().any(|f| f == name);

    let mono_candidates: &[&str] = if cfg!(target_os = "macos") {
        &["Lilex", "JetBrains Mono", "SF Mono", "Menlo"]
    } else if cfg!(target_os = "windows") {
        &["Lilex", "JetBrains Mono", "Cascadia Code", "Cascadia Mono", "Consolas"]
    } else {
        &["Lilex", "JetBrains Mono", "DejaVu Sans Mono", "monospace"]
    };
    let mono = mono_candidates
        .iter()
        .copied()
        .find(|f| has(f))
        .unwrap_or(mono_candidates[mono_candidates.len() - 1]);

    let ui = if cfg!(target_os = "windows") {
        ["Segoe UI Variable Text", "Segoe UI"]
            .into_iter()
            .find(|f| has(f))
            .unwrap_or("Segoe UI")
    } else {
        ".SystemUIFont"
    };

    (ui.into(), mono.into())
}

/// Installs the theme registry and the saved (or default) theme.
pub fn init(cx: &mut App) {
    let registry = ThemeRegistry::load();
    let wanted = Settings::get(cx).theme.clone().unwrap_or_else(|| DEFAULT_THEME.to_string());
    let entry = registry
        .find(&wanted)
        .or_else(|| registry.find(DEFAULT_THEME))
        .expect("default theme is built in");
    let theme = (*entry.theme).clone();
    cx.set_global(registry);
    cx.set_global(theme);
    apply_fonts(cx);
}

/// Default editor size; zoom and `editor_font_size` move away from it.
pub const DEFAULT_EDITOR_FONT_SIZE: f32 = 13.0;
/// The spacing Kuana has always used: 22px lines at 14px (20px at the
/// default 13px).
const DEFAULT_LINE_HEIGHT: f32 = 22.0 / 14.0;

/// Sets the fonts from the settings and the remembered zoom.
pub fn apply_fonts(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    let (ui_font, mono_font) = pick_fonts(&installed);
    cx.set_global(DefaultFonts(ui_font.clone(), mono_font.clone(), pick_caption_font(&installed)));
    let settings = Settings::get(cx).clone();
    let mono_font = match settings.editor_font_family.as_deref() {
        Some(family) if installed.iter().any(|f| f == family) => family.to_string().into(),
        _ => mono_font,
    };
    let zoom = SessionState::load().font_zoom.unwrap_or(0.);
    let size = (settings.editor_font_size.unwrap_or(DEFAULT_EDITOR_FONT_SIZE) + zoom).clamp(6., 72.);
    let line_height = settings.editor_line_height.unwrap_or(DEFAULT_LINE_HEIGHT).clamp(1., 3.);
    let mut theme = Theme::get(cx).clone();
    theme.ui_font = ui_font;
    theme.mono_font = mono_font;
    theme.editor_font_size = size;
    theme.editor_line_height = (size * line_height).round();
    let effects = settings.visual_effects();
    theme.gradients = effects.gradients();
    theme.reduce_effects = effects.minimal();
    theme.classic = settings.ui_style() == crate::settings::UiStyle::Classic;
    cx.set_global(theme);
    cx.refresh_windows();
}

/// Changes the editor zoom by `delta` points (or resets it with `None`)
/// and remembers it for the next launch.
pub fn zoom(delta: Option<f32>, cx: &mut App) {
    let base = Settings::get(cx).editor_font_size.unwrap_or(DEFAULT_EDITOR_FONT_SIZE);
    let zoom = delta.map(|d| (SessionState::load().font_zoom.unwrap_or(0.) + d).clamp(6. - base, 72. - base)).filter(|z| *z != 0.);
    SessionState::change_soon("font_zoom", 0, move |state| state.font_zoom = zoom);
    crate::state::write_soon(cx).detach();
    apply_fonts(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_take_the_longest_prefix_a_theme_colors() {
        let file: ThemeFile = serde_json::from_str(
            r##"{ "name": "T", "syntax": { "keyword": "#111111", "keyword.control": "#222222", "variable": "#333333", "keywrd": "#444444" } }"##,
        )
        .unwrap();
        let mut errors = Vec::new();
        let theme = resolve(&file, &builtins(), &mut Vec::new(), &mut errors).unwrap();
        let color = |name: &str| theme.highlight_color(Highlight::from_capture(name));
        let hex = |s: &str| Hsla::from(parse_color(s).unwrap());
        assert_eq!(color("keyword.control.return"), hex("#222222"));
        assert_eq!(color("keyword.function"), hex("#111111"));
        assert_eq!(color("variable.parameter"), hex("#333333"));
        // A more specific base kind wins over a shorter name the theme colors.
        assert_eq!(color("variable.builtin"), Hsla::from(theme.syntax.variable_builtin));
        assert_eq!(color("function.method.call"), Hsla::from(theme.syntax.method));
        // A misspelled base name is still reported.
        assert!(errors.iter().any(|e| e.contains("keywrd") && e.contains("keyword")), "{errors:?}");
    }

    #[test]
    fn caption_buttons_use_the_icon_font_windows_has() {
        let installed = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        // Windows 10.
        assert_eq!(pick_caption_font(&installed(&["Segoe UI", "Segoe MDL2 Assets"])), "Segoe MDL2 Assets");
        // Windows 11 has both.
        assert_eq!(pick_caption_font(&installed(&["Segoe MDL2 Assets", "Segoe Fluent Icons"])), "Segoe Fluent Icons");
    }

    #[test]
    fn zed_syntax_names_are_not_typos() {
        let syntax: Vec<String> = ZED_SYNTAX_NAMES.iter().map(|n| format!("\"{n}\": \"#123456\"")).collect();
        let file: ThemeFile = serde_json::from_str(&format!(r#"{{ "name": "T", "syntax": {{ {} }} }}"#, syntax.join(", "))).unwrap();
        let mut errors = Vec::new();
        resolve(&file, &builtins(), &mut Vec::new(), &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn reads_zed_theme_families() {
        let family = r##"{
            "name": "Sea", "author": "x",
            "themes": [
                { "name": "Sea Dark", "appearance": "dark", "style": {
                    "editor.background": "#101820ff", "text": "#e0e0e0", "border": "#303840",
                    "terminal.ansi.red": "#ff5555", "players": [{ "cursor": "#ffcc00", "selection": "#ffcc0033" }],
                    "syntax": { "keyword": { "color": "#ff79c6", "font_weight": 700 }, "comment": { "color": "#6272a4" },
                                "string.regex": { "color": "#ff9900" } }
                } },
                { "name": "Sea Light", "appearance": "light", "style": {} }
            ]
        }"##;
        let themes = zed_theme_family(family).unwrap();
        assert_eq!(themes.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Sea Dark", "Sea Light"]);
        // Zed's theme files may start with a license comment.
        let commented = format!("// Copyright 2025 Someone.\n/* more */\n{family}");
        assert!(zed_theme_family(&crate::settings::strip_jsonc(&commented)).is_some());
        let dark = &themes[0];
        assert_eq!(dark.colors["background"], "#101820ff");
        assert_eq!(dark.colors["cursor"], "#ffcc00");
        assert_eq!(dark.colors["terminal_red"], "#ff5555");
        assert_eq!(dark.syntax["keyword"], "#ff79c6");
        let theme = resolve(dark, &builtins(), &mut Vec::new(), &mut Vec::new()).unwrap();
        assert_eq!(theme.background, parse_color("#101820ff").unwrap());
        // Colors for names Kuana has no base kind for are kept.
        assert_eq!(theme.highlight_color(Highlight::from_capture("string.regex")), Hsla::from(parse_color("#ff9900").unwrap()));
        assert!(zed_theme_family("{\"name\": \"not a family\"}").is_none());
    }

    fn builtins() -> HashMap<String, ThemeFile> {
        BUILTIN
            .iter()
            .map(|s| serde_json::from_str::<ThemeFile>(s).unwrap())
            .map(|f| (f.name.clone(), f))
            .collect()
    }

    #[test]
    fn builtins_are_complete() {
        let all = builtins();
        assert_eq!(all.len(), 50);
        for file in all.values() {
            let mut errors = Vec::new();
            resolve(file, &all, &mut Vec::new(), &mut errors).unwrap();
            assert!(errors.is_empty(), "{errors:?}");
            // Capture names ("type.builtin") are optional extras on top of the base keys.
            let base = file.syntax.keys().filter(|k| COLOR_KEYS.contains(&format!("syntax.{k}").as_str())).count();
            let defined = file.colors.len() + base;
            assert_eq!(defined, COLOR_KEYS.len(), "{} is missing keys", file.name);
        }
        let light = all.values().filter(|f| f.appearance() == Some(Appearance::Light)).count();
        assert_eq!(light, 14);
    }

    #[test]
    fn dark_chrome_stays_near_the_islands() {
        let all = builtins();
        let theme = |name: &str| resolve(&all[name], &all, &mut Vec::new(), &mut Vec::new()).unwrap();
        let step = |t: &Theme| (lightness(t.panel) - lightness(t.background)).abs();
        // Cobalt2's panel is 8 L* darker than its editor: it moves up to the cap.
        let cobalt = theme("Cobalt2");
        assert!((step(&cobalt) - MAX_CHROME_STEP).abs() < 0.05, "{}", step(&cobalt));
        assert!(lightness(cobalt.panel) < lightness(cobalt.background));
        // Ayu Dark's is already close, and light themes keep theirs.
        let ayu = theme("Ayu Dark");
        assert_eq!(ayu.panel, parse_color(&all["Ayu Dark"].colors["panel"]).unwrap());
        let solarized = theme("Solarized Light");
        assert_eq!(solarized.panel, parse_color(&all["Solarized Light"].colors["panel"]).unwrap());
        // A theme with `deep_chrome` keeps its own, however dark.
        let onyx = theme("Onyx");
        assert_eq!(onyx.panel, parse_color(&all["Onyx"].colors["panel"]).unwrap());
        assert!(step(&onyx) > MAX_CHROME_STEP);
    }

    #[test]
    fn partial_themes_inherit() {
        let mut all = builtins();
        let custom: ThemeFile = serde_json::from_str(EXAMPLE_THEME).unwrap();
        all.insert(custom.name.clone(), custom.clone());
        let mut errors = Vec::new();
        let theme = resolve(&custom, &all, &mut Vec::new(), &mut errors).unwrap();
        let base = resolve(&all["Clay Dark"], &all, &mut Vec::new(), &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(theme.accent, parse_color("#7fb4ca").unwrap());
        assert_eq!(theme.background, base.background);
    }

    #[test]
    fn reports_bad_keys_and_cycles() {
        let mut all = builtins();
        let a: ThemeFile = serde_json::from_str(r##"{"name":"A","extends":"B","colors":{"nope":"#fff","text":"blue"}}"##).unwrap();
        let b: ThemeFile = serde_json::from_str(r#"{"name":"B","extends":"A"}"#).unwrap();
        all.insert("A".into(), a.clone());
        all.insert("B".into(), b);
        let mut errors = Vec::new();
        assert!(resolve(&a, &all, &mut Vec::new(), &mut errors).is_none());
        assert!(errors.iter().any(|e| e.contains("circular")));
        assert_eq!(suggest_key("backgrond"), Some("background"));
        assert_eq!(suggest_key("syntax.keywrd"), Some("syntax.keyword"));
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#fff"), Some(rgba(0xffffffff)));
        assert_eq!(parse_color("#d97757"), Some(rgba(0xd97757ff)));
        assert_eq!(parse_color("#d977572b"), Some(rgba(0xd977572b)));
        assert_eq!(parse_color("red"), None);
    }
}
