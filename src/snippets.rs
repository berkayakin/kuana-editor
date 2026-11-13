//! Snippets: the tab stops of an inserted snippet (from completion or the
//! user's own), and user snippets in VS Code's format. Those live in
//! `snippets/` in the config folder: `<language id>.json` (`rust.json`,
//! `typescript.json`…) for one language, `*.code-snippets` for several (with
//! an optional `"scope": "rust,go"`). Comments and trailing commas are fine.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

use serde_json::Value;

use crate::{document::Patch, settings::strip_jsonc, syntax::Language, theme::config_dir};

/// The tab stops of a snippet being filled in, in Tab order; the last is
/// where it ends (`$0`). A stop typed in several places lists them all, and
/// they're selected together so typing fills each.
#[derive(Clone, Debug, PartialEq)]
pub struct SnippetStops {
    pub stops: Vec<Vec<Range<usize>>>,
    pub current: usize,
}

impl SnippetStops {
    /// Follows an edit replacing `range` with `new_len` bytes. The current
    /// stop grows with typing in it; an edit cutting across a stop ends the
    /// snippet (returns false).
    pub fn shift(&mut self, patch: &Patch) -> bool {
        let move_by = |offset: usize, delta: isize| (offset as isize + delta) as usize;
        for (ix, stop) in self.stops.iter_mut().enumerate() {
            for r in stop {
                let (mut start, near) = patch.around(r.clone());
                let mut end = start;
                for (range, new_len) in near {
                    let delta = Patch::change(range, *new_len);
                    if ix == self.current && range.start >= r.start && range.end <= r.end {
                        end += delta;
                    } else if range.end <= r.start {
                        start += delta;
                        end += delta;
                    } else if range.start < r.end {
                        return false;
                    }
                }
                *r = move_by(r.start, start)..move_by(r.end, end);
            }
        }
        true
    }
}

/// A user snippet: what to type, then Tab.
#[derive(Clone, Debug, PartialEq)]
pub struct UserSnippet {
    pub prefix: String,
    pub body: String,
    pub description: String,
}

/// VS Code's language id for a language, which names its snippet file.
fn language_id(language: Language) -> &'static str {
    match language {
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Tsx => "typescriptreact",
        Language::Html => "html",
        Language::Css => "css",
        Language::Rust => "rust",
        Language::Go => "go",
        Language::Python => "python",
        Language::Php => "php",
        Language::Shell => "shellscript",
        Language::Json => "json",
        Language::Yaml => "yaml",
        Language::Toml => "toml",
        Language::Markdown => "markdown",
        Language::PlainText => "plaintext",
        Language::Extension(_) => language.extension().map_or("plaintext", |l| l.id),
    }
}

pub fn snippets_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("snippets"))
}

/// The user's snippets for `language`, the user's own and extensions' (Zed
/// and VS Code use the same files). Asked for on every completion request
/// and Tab, so files are read again only when they changed (see `Cache`):
/// edits to them still apply right away.
pub fn user_snippets(language: Language) -> Vec<UserSnippet> {
    let mut dirs: Vec<PathBuf> = snippets_dir().into_iter().collect();
    dirs.extend(crate::extensions::snippet_dirs());
    snippets_in(&dirs, language_id(language))
}

/// The snippets for language `id` in the files of `dirs`.
fn snippets_in(dirs: &[PathBuf], id: &str) -> Vec<UserSnippet> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for dir in dirs {
        for path in cache.files_in(dir) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // A language's own file, or one for several (`scope` says which).
            let global = name.ends_with(".code-snippets");
            if !global && name != format!("{id}.json") {
                continue;
            }
            let snippets = cache.snippets(&path);
            out.extend(snippets.iter().filter(|s| !global || s.applies_to(id)).map(|s| s.snippet.clone()));
        }
    }
    out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    out
}

/// Snippet folders and files as last read, to read again only what changed.
#[derive(Default)]
struct Cache {
    /// Each folder's snippet files, as of its modification time (adding,
    /// removing or renaming a file changes it); None: list it again.
    dirs: HashMap<PathBuf, (Option<SystemTime>, Vec<PathBuf>)>,
    /// Each file's snippets, as of its modification time and size; None:
    /// read it again.
    files: HashMap<PathBuf, (Option<(SystemTime, u64)>, Vec<Scoped>)>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// Whether a modification time is old enough that a change made after it
/// gets a later one: file systems' clocks tick coarsely (Linux's every few
/// milliseconds), so a change right after a read could keep its time.
fn settled(modified: SystemTime) -> bool {
    SystemTime::now().duration_since(modified).is_ok_and(|age| age > std::time::Duration::from_secs(1))
}

impl Cache {
    fn files_in(&mut self, dir: &Path) -> Vec<PathBuf> {
        let modified = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
        if let Some((Some(at), files)) = self.dirs.get(dir)
            && Some(*at) == modified
        {
            return files.clone();
        }
        let files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "json" || e == "code-snippets"))
            .collect();
        // Files that went are forgotten.
        self.files.retain(|path, _| path.parent() != Some(dir) || files.contains(path));
        self.dirs.insert(dir.to_path_buf(), (modified.filter(|m| settled(*m)), files.clone()));
        files
    }

    /// A file's snippets (none when it doesn't read), parsed again when its
    /// modification time or size changed.
    fn snippets(&mut self, path: &Path) -> &[Scoped] {
        let Some(stamp) = std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len()))) else {
            self.files.remove(path);
            return &[];
        };
        if !matches!(self.files.get(path), Some((Some(at), _)) if *at == stamp) {
            let snippets = std::fs::read_to_string(path).map(|text| parse(&text)).unwrap_or_default();
            self.files.insert(path.to_path_buf(), (settled(stamp.0).then_some(stamp), snippets));
        }
        &self.files[path].1
    }
}

/// A snippet and, in a `.code-snippets` file, the language ids its `scope`
/// lists (None: every language).
#[derive(Clone, Debug)]
struct Scoped {
    scope: Option<String>,
    snippet: UserSnippet,
}

impl Scoped {
    fn applies_to(&self, id: &str) -> bool {
        self.scope.as_deref().is_none_or(|scope| scope.split(',').any(|s| s.trim() == id))
    }
}

/// Parses a snippet file: every snippet, with its scope.
fn parse(text: &str) -> Vec<Scoped> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&strip_jsonc(text)) else { return Vec::new() };
    let lines = |v: Option<&Value>| -> Vec<String> {
        match v {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => Vec::new(),
        }
    };
    let mut out = Vec::new();
    for (name, snippet) in map {
        let scope = snippet.get("scope").and_then(Value::as_str).map(str::to_string);
        let body = lines(snippet.get("body")).join("\n");
        let description = snippet.get("description").and_then(Value::as_str).map_or(name.clone(), str::to_string);
        for prefix in lines(snippet.get("prefix")) {
            out.push(Scoped { scope: scope.clone(), snippet: UserSnippet { prefix, body: body.clone(), description: description.clone() } });
        }
    }
    out
}

/// Lines after the first start at the caret line's indentation, and tabs in
/// the body become the file's indent unit, as in VS Code.
pub fn indent_body(body: &str, line_indent: &str, indent_unit: &str) -> String {
    body.replace('\t', indent_unit).replace('\n', &format!("\n{line_indent}"))
}

/// Starts a snippet in the user's snippets folder when it has none, as an example.
/// What snippets' variables are filled from, VS Code's set: the file, the
/// selection and line at the caret, the clipboard, the date and time,
/// comments, random values; and `EXPR`, a postfix template's expression.
#[derive(Clone, Debug, Default)]
pub struct Variables {
    pub file: Option<PathBuf>,
    /// The project folder.
    pub workspace: Option<PathBuf>,
    pub selected: String,
    /// The caret's line, its word and number (1-based).
    pub line: String,
    pub word: String,
    pub line_number: usize,
    pub clipboard: Option<String>,
    pub line_comment: Option<String>,
    pub block_comment: Option<(String, String)>,
    pub expr: Option<String>,
    /// The time `CURRENT_*` show (now, unless a test sets it).
    pub now: Option<crate::local_time::LocalTime>,
}

const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

impl Variables {
    /// A variable's value; None for a name it doesn't know, so the snippet's
    /// default is used.
    pub fn get(&self, name: &str) -> Option<String> {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.to_string_lossy().into_owned());
        let now = || self.now.unwrap_or_else(crate::local_time::LocalTime::now);
        let random = || {
            use std::hash::{BuildHasher, Hasher};
            std::collections::hash_map::RandomState::new().build_hasher().finish()
        };
        Some(match name {
            "EXPR" => self.expr.clone()?,
            "TM_SELECTED_TEXT" | "SELECTION" => self.selected.clone(),
            "TM_CURRENT_LINE" => self.line.clone(),
            "TM_CURRENT_WORD" => self.word.clone(),
            "TM_LINE_INDEX" => self.line_number.saturating_sub(1).to_string(),
            "TM_LINE_NUMBER" => self.line_number.to_string(),
            "TM_FILENAME" => self.file.as_ref()?.file_name()?.to_string_lossy().into_owned(),
            "TM_FILENAME_BASE" => {
                let name = self.file.as_ref()?.file_name()?.to_string_lossy().into_owned();
                name.split_once('.').map_or(name.clone(), |(base, _)| base.to_string())
            }
            "TM_DIRECTORY" => self.file.as_ref()?.parent()?.to_string_lossy().into_owned(),
            "TM_FILEPATH" => path(&self.file)?,
            "RELATIVE_FILEPATH" => {
                let file = self.file.as_ref()?;
                match self.workspace.as_ref().and_then(|w| file.strip_prefix(w).ok()) {
                    Some(rel) => rel.to_string_lossy().into_owned(),
                    None => file.to_string_lossy().into_owned(),
                }
            }
            "WORKSPACE_FOLDER" => path(&self.workspace)?,
            "WORKSPACE_NAME" => self.workspace.as_ref()?.file_name()?.to_string_lossy().into_owned(),
            "CLIPBOARD" => self.clipboard.clone().unwrap_or_default(),
            "CURRENT_YEAR" => now().year.to_string(),
            "CURRENT_YEAR_SHORT" => format!("{:02}", now().year.rem_euclid(100)),
            "CURRENT_MONTH" => format!("{:02}", now().month),
            "CURRENT_MONTH_NAME" => MONTHS[now().month as usize - 1].to_string(),
            "CURRENT_MONTH_NAME_SHORT" => MONTHS[now().month as usize - 1][..3].to_string(),
            "CURRENT_DATE" => format!("{:02}", now().day),
            "CURRENT_DAY_NAME" => DAYS[now().weekday as usize].to_string(),
            "CURRENT_DAY_NAME_SHORT" => DAYS[now().weekday as usize][..3].to_string(),
            "CURRENT_HOUR" => format!("{:02}", now().hour),
            "CURRENT_MINUTE" => format!("{:02}", now().minute),
            "CURRENT_SECOND" => format!("{:02}", now().second),
            "CURRENT_SECONDS_UNIX" => now().unix.to_string(),
            "RANDOM" => format!("{:06}", random() % 1_000_000),
            "RANDOM_HEX" => format!("{:06x}", random() & 0xff_ffff),
            "UUID" => {
                let (a, b) = (random(), random());
                // Version 4, variant 1.
                let b = (b & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
                let a = (a & 0xffff_ffff_ffff_0fff) | 0x4000;
                format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xffff, a & 0xffff, b >> 48, b & 0xffff_ffff_ffff)
            }
            "LINE_COMMENT" => self.line_comment.clone()?,
            "BLOCK_COMMENT_START" => self.block_comment.as_ref()?.0.clone(),
            "BLOCK_COMMENT_END" => self.block_comment.as_ref()?.1.clone(),
            _ => return None,
        })
    }
}

pub fn ensure_snippets_dir() -> std::io::Result<PathBuf> {
    let dir = snippets_dir().ok_or_else(|| std::io::Error::other("no config directory"))?;
    std::fs::create_dir_all(&dir)?;
    let has_any = std::fs::read_dir(&dir)?.flatten().next().is_some();
    if !has_any {
        std::fs::write(dir.join("example.code-snippets"), EXAMPLE)?;
    }
    Ok(dir)
}

const EXAMPLE: &str = r##"// Snippets in VS Code's format: type the prefix, then Tab. $1, $2… are the
// places Tab moves to, ${1:text} has default text, $0 is where it ends.
// Put language-only snippets in <language id>.json (rust.json, typescript.json…),
// or limit one here with "scope": "rust,go".
{
  "Print to console": {
    "scope": "javascript,typescript,typescriptreact",
    "prefix": "log",
    "body": ["console.log($1);$0"],
    "description": "console.log"
  },
  "Test function": {
    "scope": "rust",
    "prefix": "test",
    "body": ["#[test]", "fn ${1:name}() {", "\t$0", "}"]
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_follow_typing_and_end_on_crossing_edits() {
        // "foo(a, b)": $1 = a at 4..5, $2 = b at 7..8, $0 at 9.
        let mut s = SnippetStops { stops: vec![vec![4..5], vec![7..8], vec![9..9]], current: 0 };
        // Typing "xyz" over the selected `a`.
        assert!(s.shift(&Patch::new(vec![(4..5, 3)])));
        assert_eq!(s.stops, vec![vec![4..7], vec![9..10], vec![11..11]]);
        // Typing at the end of the current stop grows it; later stops move.
        assert!(s.shift(&Patch::new(vec![(7..7, 1)])));
        assert_eq!(s.stops, vec![vec![4..8], vec![10..11], vec![12..12]]);
        // An edit across stops ends the snippet.
        assert!(!s.shift(&Patch::new(vec![(5..11, 0)])));
    }

    #[test]
    fn mirrors_move_together() {
        // `$1 = $1;` with both empty at 0 and 3.
        let mut s = SnippetStops { stops: vec![vec![0..0, 3..3], vec![4..4]], current: 0 };
        // Typing `x` in both places, the later one first (as multi-cursor edits apply).
        assert!(s.shift(&Patch::new(vec![(3..3, 1)])));
        assert!(s.shift(&Patch::new(vec![(0..0, 1)])));
        assert_eq!(s.stops, vec![vec![0..1, 4..5], vec![6..6]]);
    }

    #[test]
    fn parses_vscode_snippet_files() {
        let text = r#"{
            // A comment.
            "Log": { "prefix": ["log", "cl"], "body": ["console.log($1);", "$0"], "description": "Log it", },
            "Rust only": { "scope": "rust", "prefix": "t", "body": "x" },
        }"#;
        let all = parse(text);
        assert_eq!(all.len(), 3);
        let log = &all.iter().find(|s| s.snippet.prefix == "cl").unwrap().snippet;
        assert_eq!((log.body.as_str(), log.description.as_str()), ("console.log($1);\n$0", "Log it"));
        let prefixes = |id: &str| all.iter().filter(|s| s.applies_to(id)).map(|s| s.snippet.prefix.clone()).collect::<Vec<_>>();
        assert!(!prefixes("javascript").contains(&"t".to_string()));
        assert!(prefixes("rust").contains(&"t".to_string()));
        assert_eq!(indent_body("a {\n\t$0\n}", "    ", "  "), "a {\n      $0\n    }");
    }

    #[test]
    fn snippet_files_are_read_again_when_they_change() {
        let dir = std::env::temp_dir().join(format!("kuana-snippets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = [dir.clone()];
        let prefixes = || snippets_in(&dirs, "rust").into_iter().map(|s| s.prefix).collect::<Vec<_>>();
        std::fs::write(dir.join("rust.json"), r#"{ "A": { "prefix": "a", "body": "x" } }"#).unwrap();
        std::fs::write(dir.join("go.json"), r#"{ "G": { "prefix": "g", "body": "x" } }"#).unwrap();
        assert_eq!(prefixes(), ["a"]);
        // Changed in place, then a file for several languages added.
        std::fs::write(dir.join("rust.json"), r#"{ "B": { "prefix": "bb", "body": "y" } }"#).unwrap();
        assert_eq!(prefixes(), ["bb"]);
        std::fs::write(dir.join("all.code-snippets"), r#"{ "C": { "prefix": "c", "body": "z", "scope": "rust" } }"#).unwrap();
        assert_eq!(prefixes(), ["bb", "c"]);
        // Removed.
        std::fs::remove_file(dir.join("rust.json")).unwrap();
        assert_eq!(prefixes(), ["c"]);
        // A file not changed since it was read isn't read again: one rewritten
        // behind the cache's back (same size and time) still gives what was read.
        let an_hour_ago = SystemTime::now() - std::time::Duration::from_secs(3600);
        let rewrite = |prefix: &str| {
            std::fs::write(dir.join("rust.json"), format!(r#"{{ "S": {{ "prefix": "{prefix}", "body": "x" }} }}"#)).unwrap();
            std::fs::File::options().write(true).open(dir.join("rust.json")).unwrap().set_modified(an_hour_ago).unwrap();
        };
        rewrite("aa");
        assert_eq!(prefixes(), ["aa", "c"]);
        rewrite("zz");
        assert_eq!(prefixes(), ["aa", "c"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn variables_fill_from_the_file_caret_and_time() {
        let vars = Variables {
            file: Some(PathBuf::from("/p/src/main.test.rs")),
            workspace: Some(PathBuf::from("/p")),
            selected: "sel".into(),
            line_number: 4,
            line_comment: Some("//".into()),
            now: Some(crate::local_time::LocalTime { year: 2026, month: 11, day: 3, hour: 9, minute: 5, second: 7, weekday: 1, unix: 0 }),
            ..Default::default()
        };
        let (text, _) = crate::lsp::protocol::expand_snippet_with(
            "$TM_FILENAME_BASE|$TM_FILENAME|$RELATIVE_FILEPATH|$WORKSPACE_NAME|$TM_LINE_NUMBER|$SELECTION|$CLIPBOARD|$LINE_COMMENT",
            &|n| vars.get(n),
        );
        assert_eq!(text, "main|main.test.rs|src/main.test.rs|p|4|sel||//");
        let (date, _) = crate::lsp::protocol::expand_snippet_with(
            "$CURRENT_YEAR-$CURRENT_MONTH-$CURRENT_DATE $CURRENT_HOUR:$CURRENT_MINUTE:$CURRENT_SECOND $CURRENT_DAY_NAME_SHORT $CURRENT_MONTH_NAME",
            &|n| vars.get(n),
        );
        assert_eq!(date, "2026-11-03 09:05:07 Tue November");
        // Unknown here: the default.
        assert_eq!(crate::lsp::protocol::expand_snippet_with("${BLOCK_COMMENT_START:/*}", &|n| vars.get(n)).0, "/*");
        let uuid = vars.get("UUID").unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
    }
}
