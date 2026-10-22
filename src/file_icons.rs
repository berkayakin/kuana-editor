//! File-type icons from Charmed Icons (MIT, see `assets/file-icons/LICENSE`). Unlike the UI
//! icons they keep their own colors, so they're drawn with `img()` instead of as masks: the
//! Base set on dark themes, the Light set on light ones. `charmed.json` is the pack's Zed
//! icon theme, vendored unchanged; updating the pack means copying its SVGs and that file
//! (the test below catches a missing or unused SVG).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock},
};

use gpui::{ImageSource, SharedString};

use crate::theme::Appearance;

macro_rules! file_icons {
    ($($name:literal),* $(,)?) => {
        pub static FILE_ICONS: &[(&str, &[u8])] = &[$(
            (concat!("file-icons/dark/", $name, ".svg"), include_bytes!(concat!("../assets/file-icons/dark/", $name, ".svg"))),
            (concat!("file-icons/light/", $name, ".svg"), include_bytes!(concat!("../assets/file-icons/light/", $name, ".svg"))),
        )*];
    };
}

file_icons![
    "_file",
    "_folder",
    "_folder_open",
    "assembly",
    "astro-config",
    "astro",
    "audio",
    "binary",
    "bun-lock",
    "bun",
    "c-header",
    "c",
    "changelog",
    "code-of-conduct",
    "codeowners",
    "config",
    "cpp-header",
    "cpp",
    "cs",
    "css",
    "csv",
    "dart",
    "database",
    "docker",
    "drizzle-orm",
    "eslint",
    "event",
    "font",
    "fortran-fixed",
    "fortran",
    "git",
    "gleam",
    "go-mod",
    "go",
    "godot-assets",
    "godot",
    "hcl",
    "html",
    "image",
    "java",
    "javascript-config",
    "javascript",
    "json",
    "julia",
    "just",
    "key",
    "kotlin",
    "latex",
    "license",
    "lock",
    "lua-config",
    "lua",
    "luau-config",
    "luau-def",
    "luau",
    "makefile",
    "markdown",
    "markdownx",
    "next",
    "nim",
    "nix",
    "node",
    "npm-lock",
    "npm",
    "nuxt",
    "odin",
    "oxie",
    "package-config",
    "package-lock",
    "pcss",
    "pdf",
    "perl",
    "php",
    "powershell",
    "python",
    "react-typescript",
    "react",
    "readme",
    "roblox-config",
    "roblox-lock",
    "roblox-model",
    "roblox",
    "ruby",
    "rust-config",
    "rust",
    "scala",
    "scss",
    "security",
    "shell",
    "storybook",
    "svelte",
    "svg",
    "swift",
    "tailwind",
    "terraform",
    "test-blue",
    "test-teal",
    "test-yellow",
    "text",
    "todo",
    "toml",
    "typescript-config",
    "typescript-def",
    "typescript",
    "video",
    "visual-studio",
    "vite",
    "vscode",
    "vue",
    "wally-lock",
    "wally",
    "web-assembly",
    "workflow",
    "xml",
    "yaml",
    "yarn-lock",
    "yarn",
    "zig",
    "zip",
];

const DEFAULT: &str = "_file";

/// Icons for languages Charmed Icons doesn't cover, by lowercased suffix or file name.
/// Kept out of `charmed.json` so the vendored pack stays unchanged.
const EXTRA: &[(&str, &str)] = &[("oxi", "oxie")];

/// Asset paths of one icon in both sets.
#[derive(Clone)]
struct Paths {
    dark: SharedString,
    light: SharedString,
}

impl Paths {
    fn new(icon: &str) -> Self {
        Self { dark: format!("file-icons/dark/{icon}.svg").into(), light: format!("file-icons/light/{icon}.svg").into() }
    }

    fn get(&self, appearance: Appearance) -> SharedString {
        match appearance {
            Appearance::Dark => self.dark.clone(),
            Appearance::Light => self.light.clone(),
        }
    }
}

struct Associations {
    /// Lowercased file names and suffixes ("cargo.lock", "config.ts", "rs") to icons.
    by_name: HashMap<String, Paths>,
    file: Paths,
    folder: Paths,
    folder_open: Paths,
}

/// Reads `charmed.json`: `file_stems` (whole names) and `file_suffixes` map to a type, and
/// `file_icons` maps a type to its SVG.
fn parse(json: &str) -> HashMap<String, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else { return HashMap::new() };
    let theme = &value["themes"][0];
    let svg_of = |kind: &str| -> Option<String> {
        let path = theme["file_icons"][kind]["path"].as_str()?;
        Some(path.rsplit('/').next()?.strip_suffix(".svg")?.to_string())
    };
    let mut map = HashMap::new();
    // Names win over suffixes for the same key.
    for table in ["file_suffixes", "file_stems"] {
        for (key, kind) in theme[table].as_object().into_iter().flatten() {
            if let Some(svg) = kind.as_str().and_then(svg_of) {
                map.insert(key.to_lowercase(), svg);
            }
        }
    }
    map
}

fn associations() -> &'static Associations {
    static ASSOCIATIONS: OnceLock<Associations> = OnceLock::new();
    ASSOCIATIONS.get_or_init(|| Associations {
        by_name: parse(include_str!("../assets/file-icons/charmed.json"))
            .into_iter()
            .chain(EXTRA.iter().map(|(key, icon)| (key.to_string(), icon.to_string())))
            .map(|(key, icon)| (key, Paths::new(&icon)))
            .collect(),
        file: Paths::new(DEFAULT),
        folder: Paths::new("_folder"),
        folder_open: Paths::new("_folder_open"),
    })
}

/// The icon's asset path for a file: its whole name first, then ever shorter suffixes
/// (`app.config.ts` → `config.ts` → `ts`), case-insensitively.
pub fn file(path: &Path, appearance: Appearance) -> SharedString {
    let a = associations();
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
    let mut rest = name.as_str();
    loop {
        if let Some(paths) = a.by_name.get(rest) {
            return paths.get(appearance);
        }
        match rest.split_once('.') {
            Some((_, suffix)) => rest = suffix,
            None => return a.file.get(appearance),
        }
    }
}

pub fn folder(open: bool, appearance: Appearance) -> SharedString {
    let a = associations();
    if open { a.folder_open.get(appearance) } else { a.folder.get(appearance) }
}

// ---- Icon themes from extensions -----------------------------------------------

/// An extension's icon theme (Zed's format), with its SVGs on disk.
struct IconTheme {
    by_name: HashMap<String, PathBuf>,
    file: Option<PathBuf>,
    folder: Option<PathBuf>,
    folder_open: Option<PathBuf>,
}

/// The `icon_theme` in use, when it isn't the built-in one.
static SELECTED: RwLock<Option<Arc<IconTheme>>> = RwLock::new(None);

/// Uses theme `index` of the icon theme file `json` (paths in it are relative
/// to the extension's folder), or the built-in icons for None.
pub fn select(theme: Option<(&Path, usize)>) {
    let loaded = theme.and_then(|(json, index)| load_theme(json, index)).map(Arc::new);
    *SELECTED.write().unwrap_or_else(|e| e.into_inner()) = loaded;
}

fn load_theme(json: &Path, index: usize) -> Option<IconTheme> {
    let value: serde_json::Value = serde_json::from_str(&crate::settings::strip_jsonc(&std::fs::read_to_string(json).ok()?)).ok()?;
    let theme = &value["themes"][index];
    let root = json.parent()?.parent()?;
    let path = |p: &serde_json::Value| p.as_str().map(|p| root.join(p.trim_start_matches("./")));
    let svg_of = |kind: &str| path(&theme["file_icons"][kind]["path"]);
    let mut by_name = HashMap::new();
    for table in ["file_suffixes", "file_stems"] {
        for (key, kind) in theme[table].as_object().into_iter().flatten() {
            if let Some(svg) = kind.as_str().and_then(svg_of) {
                by_name.insert(key.to_lowercase(), svg);
            }
        }
    }
    Some(IconTheme {
        by_name,
        file: svg_of("default"),
        folder: path(&theme["directory_icons"]["collapsed"]),
        folder_open: path(&theme["directory_icons"]["expanded"]),
    })
}

/// A file's icon: the selected icon theme's, else the built-in one.
pub fn file_source(path: &Path, appearance: Appearance) -> ImageSource {
    if let Some(theme) = SELECTED.read().unwrap_or_else(|e| e.into_inner()).clone() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
        let mut rest = name.as_str();
        let found = loop {
            if let Some(svg) = theme.by_name.get(rest) {
                break Some(svg.clone());
            }
            match rest.split_once('.') {
                Some((_, suffix)) => rest = suffix,
                None => break theme.file.clone(),
            }
        };
        if let Some(svg) = found {
            return svg.into();
        }
    }
    file(path, appearance).into()
}

pub fn folder_source(open: bool, appearance: Appearance) -> ImageSource {
    if let Some(theme) = SELECTED.read().unwrap_or_else(|e| e.into_inner()).clone()
        && let Some(svg) = if open { theme.folder_open.clone() } else { theme.folder.clone() }
    {
        return svg.into();
    }
    folder(open, appearance).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(name: &str) -> String {
        file(Path::new(name), Appearance::Dark).trim_start_matches("file-icons/dark/").trim_end_matches(".svg").to_string()
    }

    #[test]
    fn every_icon_is_embedded_and_used() {
        let used: std::collections::HashSet<String> = parse(include_str!("../assets/file-icons/charmed.json"))
            .into_values()
            .chain([DEFAULT, "_folder", "_folder_open"].map(String::from))
            .chain(EXTRA.iter().map(|(_, icon)| icon.to_string()))
            .collect();
        let embedded: Vec<&str> = FILE_ICONS.iter().map(|(p, _)| *p).collect();
        for icon in &used {
            for set in ["dark", "light"] {
                let path = format!("file-icons/{set}/{icon}.svg");
                assert!(embedded.contains(&path.as_str()), "{path} is used by charmed.json but not embedded");
            }
        }
        for path in embedded {
            let icon = path.rsplit('/').next().unwrap().trim_end_matches(".svg");
            assert!(used.contains(icon), "{path} is embedded but unused");
        }
    }

    #[test]
    fn matches_names_then_suffixes() {
        assert_eq!(icon("main.rs"), "rust");
        assert_eq!(icon("main.oxi"), "oxie");
        assert_eq!(icon("App.TSX"), "react-typescript");
        assert_eq!(icon("tsconfig.json"), "typescript-config");
        assert_eq!(icon("vite.config.ts"), "vite");
        assert_eq!(icon("package.json"), "node");
        assert_eq!(icon(".gitignore"), "git");
        assert_eq!(icon("Dockerfile"), "docker");
        assert_eq!(icon("LICENSE"), "license");
        assert_eq!(icon("data.unknownext"), "_file");
        assert_eq!(icon(""), "_file");
        assert_eq!(folder(true, Appearance::Light).as_ref(), "file-icons/light/_folder_open.svg");
    }
}
