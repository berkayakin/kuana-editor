//! Text search shared by the in-file find bar and Find in Files.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone, Debug)]
pub struct SearchQuery {
    regex: Regex,
    is_regex: bool,
}

impl SearchQuery {
    /// Returns `Ok(None)` for an empty query and `Err` for an invalid regex.
    pub fn new(text: &str, options: SearchOptions) -> Result<Option<Self>, String> {
        if text.is_empty() {
            return Ok(None);
        }
        let mut pattern = if options.regex {
            text.to_string()
        } else {
            regex::escape(text)
        };
        if options.whole_word {
            pattern = if options.regex {
                // A regex's edges aren't its first and last characters.
                format!(r"\b(?:{pattern})\b")
            } else {
                // A boundary only on a side that ends in a word character
                // (as Zed does): `\b` beside `@`, `$`, `.` or `(` would need a
                // word character next to it, so `@Override` or `foo(` never matched.
                let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                let start = if word(text.chars().next()) { r"\b" } else { "" };
                let end = if word(text.chars().next_back()) { r"\b" } else { "" };
                format!("{start}{pattern}{end}")
            };
        }
        RegexBuilder::new(&pattern)
            .case_insensitive(!options.case_sensitive)
            .multi_line(true)
            .build()
            .map(|regex| {
                Some(Self {
                    regex,
                    is_regex: options.regex,
                })
            })
            .map_err(|e| match e {
                regex::Error::Syntax(s) => s.lines().last().unwrap_or("Invalid regex").trim().to_string(),
                other => other.to_string(),
            })
    }

    pub fn find_all(&self, text: &str) -> Vec<Range<usize>> {
        self.regex
            .find_iter(text)
            .filter(|m| !m.is_empty())
            .map(|m| m.range())
            .collect()
    }

    /// The replacement text for a match, expanding `$1`-style groups in regex mode.
    pub fn replacement_for(&self, text: &str, range: Range<usize>, replacement: &str) -> String {
        if !self.is_regex {
            return replacement.to_string();
        }
        match self.regex.captures_at(text, range.start) {
            Some(caps) if caps.get(0).map(|m| m.range()) == Some(range) => {
                let mut out = String::new();
                caps.expand(replacement, &mut out);
                out
            }
            _ => replacement.to_string(),
        }
    }
}

/// A search's matches in one document, shared by the find bar and the
/// editor: one letter in a big file finds hundreds of thousands, so they're
/// never copied, and drawing looks up only the ones on screen.
#[derive(Clone, Default)]
pub struct SearchMatches {
    /// Sorted by start, apart.
    pub ranges: Arc<Vec<Range<usize>>>,
    /// The lines they start on, each once, in order (the scrollbar's markers).
    pub rows: Arc<Vec<usize>>,
}

impl SearchMatches {
    /// `ranges` (sorted) in `rope`'s text.
    pub fn new(ranges: Vec<Range<usize>>, rope: &ropey::Rope) -> Self {
        let mut rows: Vec<usize> = Vec::new();
        // The next line's start: offsets before it are on the last line found.
        let mut next = 0;
        for r in &ranges {
            if r.start >= next || rows.is_empty() {
                let row = rope.byte_to_line(r.start.min(rope.len_bytes()));
                next = if row + 1 < rope.len_lines() { rope.line_to_byte(row + 1) } else { usize::MAX };
                if rows.last() != Some(&row) {
                    rows.push(row);
                }
            }
        }
        SearchMatches { ranges: Arc::new(ranges), rows: Arc::new(rows) }
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

#[derive(Clone)]
pub struct ProjectMatch {
    pub path: PathBuf,
    pub relative: String,
    /// Zero-based line number.
    pub line: usize,
    /// Where that line starts in the file.
    pub line_start: usize,
    /// The match's line around it, at most `SNIPPET_BEFORE` +
    /// `SNIPPET_AFTER` bytes beside the match: a minified file's line can be
    /// megabytes long, and every match would copy it.
    pub line_text: String,
    /// Match range within `line_text`.
    pub line_range: Range<usize>,
    /// Match range within the whole file.
    pub range: Range<usize>,
    /// The file's text, shared by its matches, for the preview.
    pub contents: Arc<str>,
}

/// How a search ended.
pub struct SearchEnd {
    /// It stopped at `MAX_FILES` or `MAX_MATCHES`.
    pub truncated: bool,
}

/// The most files and matches a search lists.
pub const MAX_FILES: usize = 5_000;
pub const MAX_MATCHES: usize = 10_000;
const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024;
const SNIPPET_BEFORE: usize = 80;
const SNIPPET_AFTER: usize = 200;
/// How often found matches go to the list while a search runs.
const BATCH_EVERY: Duration = Duration::from_millis(50);

/// Every file under `root`, honoring .gitignore. Paths are absolute.
pub fn project_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = walker(root)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .collect();
    files.sort();
    files
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name();
            name != ".git" && name != "node_modules" && name != ".DS_Store"
        })
        .build()
}

/// Searches every text file of `paths` (the project's, see
/// `crate::project`). `overrides` supplies the contents of unsaved open
/// buffers so results reflect what the user sees. Matches go to `found` in
/// batches as they're found (with the number of files with matches so
/// far), so the list fills while it runs; `cancel` stops it between files
/// (a newer query took over).
pub fn search_project(
    root: &Path,
    paths: Vec<PathBuf>,
    query: &SearchQuery,
    overrides: &HashMap<PathBuf, Arc<str>>,
    cancel: &AtomicBool,
    mut found: impl FnMut(Vec<ProjectMatch>, usize),
) -> SearchEnd {
    let mut batch = Vec::new();
    let (mut files, mut total) = (0, 0);
    let mut last_batch = Instant::now();
    for path in paths {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if files >= MAX_FILES || total >= MAX_MATCHES {
            found(batch, files);
            return SearchEnd { truncated: true };
        }
        let contents: Arc<str> = match overrides.get(&path) {
            Some(text) => text.clone(),
            None => {
                if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE_SIZE).unwrap_or(true) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if bytes.iter().take(8000).any(|b| *b == 0) {
                    continue; // binary
                }
                // Line ends as an editor of the file has them, so a match's
                // offsets select the same text there.
                match String::from_utf8(bytes) {
                    Ok(s) => crate::encoding::to_lf(s).into(),
                    Err(_) => continue,
                }
            }
        };
        let ranges = query.find_all(&contents);
        if ranges.is_empty() {
            continue;
        }
        files += 1;
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
        let mut line = 0;
        let mut line_start = 0;
        let mut scanned = 0;
        for range in ranges.into_iter().take(MAX_MATCHES - total) {
            // Advance line counters incrementally.
            for (i, b) in contents.as_bytes()[scanned..range.start].iter().enumerate() {
                if *b == b'\n' {
                    line += 1;
                    line_start = scanned + i + 1;
                }
            }
            scanned = range.start;
            let line_end = contents[line_start..].find('\n').map_or(contents.len(), |e| line_start + e);
            let match_end = range.end.min(line_end);
            let from = contents.floor_char_boundary(range.start.saturating_sub(SNIPPET_BEFORE).max(line_start));
            let to = contents.ceil_char_boundary((match_end + SNIPPET_AFTER).min(line_end));
            batch.push(ProjectMatch {
                path: path.clone(),
                relative: relative.clone(),
                line,
                line_start,
                line_text: contents[from..to].to_string(),
                line_range: range.start - from..match_end - from,
                range,
                contents: contents.clone(),
            });
            total += 1;
        }
        if last_batch.elapsed() >= BATCH_EVERY {
            found(std::mem::take(&mut batch), files);
            last_batch = Instant::now();
        }
    }
    found(batch, files);
    SearchEnd { truncated: total >= MAX_MATCHES }
}

/// The lines around line `line` (which starts at `line_start`), `context`
/// on each side, each cut at `max_len` bytes: (line number, text). Only
/// those lines are looked at, however big the file.
pub fn context_lines(contents: &str, line_start: usize, line: usize, context: usize, max_len: usize) -> Vec<(usize, &str)> {
    let mut first = (line, line_start);
    while first.0 + context > line && first.1 > 0 {
        let start = contents[..first.1 - 1].rfind('\n').map_or(0, |i| i + 1);
        first = (first.0 - 1, start);
    }
    let mut out = Vec::new();
    let (mut row, mut at) = first;
    while row <= line + context && at <= contents.len() {
        let end = contents[at..].find('\n').map_or(contents.len(), |i| at + i);
        out.push((row, &contents[at..contents.floor_char_boundary(end.min(at + max_len))]));
        if end == contents.len() {
            break;
        }
        (row, at) = (row + 1, end + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_know_their_lines() {
        let text = "ab\nb\n\nbb b\nx";
        let rope = ropey::Rope::from_str(text);
        let q = SearchQuery::new("b", SearchOptions::default()).unwrap().unwrap();
        let matches = SearchMatches::new(q.find_all(text), &rope);
        assert_eq!(matches.len(), 5);
        assert_eq!(*matches.rows, [0, 1, 3]);
        assert!(SearchMatches::new(Vec::new(), &rope).rows.is_empty());
    }

    #[test]
    fn options() {
        let q = |t: &str, o| SearchQuery::new(t, o).unwrap().unwrap();
        let text = "Item item items";
        assert_eq!(q("item", SearchOptions::default()).find_all(text).len(), 3);
        let cs = SearchOptions { case_sensitive: true, ..Default::default() };
        assert_eq!(q("item", cs).find_all(text).len(), 2);
        let ww = SearchOptions { whole_word: true, ..Default::default() };
        assert_eq!(q("item", ww).find_all(text).len(), 2);
        let re = SearchOptions { regex: true, ..Default::default() };
        assert_eq!(q("i\\w+s", re).find_all(text), vec![10..15]);
        assert!(SearchQuery::new("(", re).is_err());
        assert_eq!(q("a.b", SearchOptions::default()).find_all("a.b axb"), vec![0..3]);
    }

    #[test]
    fn whole_word_bounds_only_word_edges() {
        let ww = SearchOptions { whole_word: true, ..Default::default() };
        let find = |t: &str, text: &str| SearchQuery::new(t, ww).unwrap().unwrap().find_all(text);
        assert_eq!(find("@Override", " @Override\n@Overrides"), vec![1..10]);
        assert_eq!(find("$id", "x = $id; $idx"), vec![4..7]);
        assert_eq!(find(".map", "a.map(f) a.mapped"), vec![1..5]);
        assert_eq!(find("foo(", "foo(1) barfoo(2)"), vec![0..4]);
        assert_eq!(find("item", "item items"), vec![0..4]);
        // Non-ASCII letters are word characters too.
        assert_eq!(find("ğ", "ğ ağ"), vec![0..2]);
    }

    #[test]
    fn project_files_skip_ignored_files_and_folders() {
        let dir = std::env::temp_dir().join(format!("kuana-search-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in [".git", "config", "build", "node_modules/pkg"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join(".gitignore"), "partners.yml\nbuild/\n").unwrap();
        for file in ["config/partners.yml", "config/partners.yml.example", "build/out.js", "node_modules/pkg/index.js", ".git/HEAD"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let names = |files: Vec<PathBuf>| -> Vec<String> {
            files.iter().map(|f| f.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect()
        };
        assert_eq!(names(project_files(&dir)), [".gitignore", "config/partners.yml.example"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn project_search_keeps_short_snippets_and_stops_when_cancelled() {
        let q = SearchQuery::new("x", SearchOptions::default()).unwrap().unwrap();
        // One long line (minified): each match keeps a little of it.
        let long: Arc<str> = format!("{}x{}x", "a".repeat(5000), "b".repeat(5000)).into();
        let path = PathBuf::from("/p/min.js");
        let overrides = HashMap::from([(path.clone(), long.clone())]);
        let mut all = Vec::new();
        let end = search_project(Path::new("/p"), vec![path.clone()], &q, &overrides, &AtomicBool::new(false), |batch, _| all.extend(batch));
        assert!(!end.truncated);
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|m| m.line_text.len() <= SNIPPET_BEFORE + 1 + SNIPPET_AFTER));
        assert_eq!(&all[1].line_text[all[1].line_range.clone()], "x");
        // Cancelled before it starts: nothing.
        let mut none = Vec::new();
        search_project(Path::new("/p"), vec![path], &q, &overrides, &AtomicBool::new(true), |batch, _| none.extend(batch));
        assert!(none.is_empty());
    }

    #[test]
    fn context_lines_look_only_near_the_match() {
        let text = "zero\none\ntwo\nthree\nfour\nfive";
        let lines = context_lines(text, text.find("three").unwrap(), 3, 1, 3);
        assert_eq!(lines, [(2, "two"), (3, "thr"), (4, "fou")]);
        assert_eq!(context_lines(text, 0, 0, 2, 99), [(0, "zero"), (1, "one"), (2, "two")]);
        assert_eq!(context_lines(text, text.find("five").unwrap(), 5, 1, 99), [(4, "four"), (5, "five")]);
    }

    #[test]
    fn regex_replacement_groups() {
        let re = SearchOptions { regex: true, case_sensitive: true, ..Default::default() };
        let q = SearchQuery::new(r"(\w+)@(\w+)", re).unwrap().unwrap();
        let text = "a@b c@d";
        assert_eq!(q.replacement_for(text, 4..7, "$2@$1"), "d@c");
        let found: Vec<String> = q.find_all(text).into_iter().map(|r| q.replacement_for(text, r, "$2@$1")).collect();
        assert_eq!(found, ["b@a", "d@c"]);
    }
}
