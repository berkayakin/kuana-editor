//! TODO items: comments that start with one of the `todo_patterns` words
//! (TODO, FIXME, HACK, XXX by default), as JetBrains' TODO tool window lists
//! them. A file's comments are found from its language's comment tokens
//! (strings skipped), so files that aren't open need no parse; the editor
//! highlights them inside the comments its syntax tree knows.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use regex::Regex;

use crate::syntax::Language;

/// The words looked for when `todo_patterns` isn't set.
pub const DEFAULT_PATTERNS: [&str; 4] = ["TODO", "FIXME", "HACK", "XXX"];

/// Files bigger than this aren't read (minified bundles, data).
const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024;
/// How much of a TODO's text is kept.
const MAX_TEXT: usize = 300;

/// One TODO item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    /// 0-based.
    pub line: usize,
    /// Where the word starts in its line (bytes).
    pub column: usize,
    /// The word's length (bytes): what's highlighted.
    pub keyword: usize,
    /// From the word to the comment's end on that line.
    pub text: String,
}

/// The patterns' regex: a whole word of any of them, in any case. None
/// when there are none.
pub fn regex(patterns: &[String]) -> Option<Arc<Regex>> {
    // Built once per list: the editor asks every frame.
    static CACHE: Mutex<Option<(Vec<String>, Option<Arc<Regex>>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((key, regex)) = cache.as_ref()
        && key == patterns
    {
        return regex.clone();
    }
    let words: Vec<String> = patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).map(regex::escape).collect();
    let regex = (!words.is_empty()).then(|| Regex::new(&format!(r"(?i)\b(?:{})\b", words.join("|"))).ok()).flatten().map(Arc::new);
    *cache = Some((patterns.to_vec(), regex.clone()));
    regex
}

/// The TODO items in a file's text: the first pattern match in each comment
/// (or each line of a block comment), with the text after it.
pub fn find(text: &str, language: Language, regex: &Regex) -> Vec<Todo> {
    // Most files have none: no need to look for comments.
    if !regex.is_match(text) {
        return Vec::new();
    }
    let line_token = language.line_comment().map(str::trim).filter(|t| !t.is_empty());
    let block = language.block_comment().map(|(open, close)| (open.trim(), close.trim())).filter(|(o, c)| !o.is_empty() && !c.is_empty());
    if line_token.is_none() && block.is_none() {
        return Vec::new();
    }
    let mut todos = Vec::new();
    let mut in_block = false;
    for (row, line) in text.lines().enumerate() {
        for span in comment_spans(line, line_token, block, &mut in_block) {
            let Some(m) = regex.find(&line[span.clone()]) else { continue };
            let column = span.start + m.start();
            let rest = line[column..span.end].trim_end();
            let rest = block.and_then(|(_, close)| rest.strip_suffix(close)).unwrap_or(rest).trim_end();
            let mut end = rest.len().min(MAX_TEXT);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            todos.push(Todo { line: row, column, keyword: m.len(), text: rest[..end].to_string() });
        }
    }
    todos
}

/// The parts of `line` inside comments, given whether a block comment is
/// open at its start (updated for the next line). Quoted strings are
/// skipped: a `//` or `#` in one starts no comment.
fn comment_spans(line: &str, line_token: Option<&str>, block: Option<(&str, &str)>, in_block: &mut bool) -> Vec<Range<usize>> {
    let bytes = line.as_bytes();
    let at = |i: usize, token: &str| bytes[i..].starts_with(token.as_bytes());
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if *in_block {
            let (_, close) = block.expect("only open with a block token");
            match line[i..].find(close) {
                Some(end) => {
                    spans.push(i..i + end);
                    i += end + close.len();
                    *in_block = false;
                }
                None => {
                    spans.push(i..bytes.len());
                    break;
                }
            }
            continue;
        }
        if let Some(token) = line_token.filter(|t| at(i, t)) {
            spans.push(i + token.len()..bytes.len());
            break;
        }
        if let Some((open, _)) = block.filter(|(open, _)| at(i, open)) {
            *in_block = true;
            i += open.len();
            continue;
        }
        match bytes[i] {
            quote @ (b'"' | b'\'' | b'`') => {
                // To its closing quote on this line; an unclosed ' is a
                // Rust lifetime or an apostrophe, an unclosed " or ` goes on.
                let mut j = i + 1;
                let mut closed = None;
                while j < bytes.len() {
                    match bytes[j] {
                        b'\\' => j += 2,
                        b if b == quote => {
                            closed = Some(j);
                            break;
                        }
                        _ => j += 1,
                    }
                }
                match closed {
                    Some(end) => i = end + 1,
                    None if quote == b'\'' => i += 1,
                    None => break,
                }
            }
            _ => i += 1,
        }
    }
    spans
}

/// Reads a file the way Find in Files does: not too big, not binary, UTF-8,
/// with `\n` line ends.
fn read(path: &Path) -> Option<String> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_SIZE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok().map(crate::encoding::to_lf)
}

/// Each file's TODO items (none for a file that's gone or unreadable), on
/// several threads; `overrides` are open files' unsaved text. Stops between
/// files once `cancel` is set.
pub fn scan(paths: &[PathBuf], overrides: &HashMap<PathBuf, Arc<str>>, regex: &Regex, cancel: &AtomicBool) -> Vec<(PathBuf, Vec<Todo>)> {
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).clamp(1, paths.len().max(1));
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::with_capacity(paths.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !cancel.load(Ordering::Relaxed) {
                    let Some(path) = paths.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                    let todos = match overrides.get(path) {
                        Some(text) => find(text, Language::from_path(path), regex),
                        None => read(path).map(|text| find(&text, Language::from_path(path), regex)).unwrap_or_default(),
                    };
                    found.lock().unwrap_or_else(|e| e.into_inner()).push((path.clone(), todos));
                }
            });
        }
    });
    found.into_inner().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todos(text: &str, language: Language) -> Vec<(usize, String)> {
        let regex = regex(&DEFAULT_PATTERNS.map(String::from)).unwrap();
        find(text, language, &regex).into_iter().map(|t| (t.line, t.text)).collect()
    }

    #[test]
    fn finds_todos_in_comments_only() {
        let rust = "// TODO: first\nlet s = \"// TODO: not this\"; // fixme later\nfn f<'a>(x: &'a str) {} // HACK: lifetimes\n/* a\n   XXX inside a block */ let todo = 1;\nlet todo_list = 2; // todos are not TODO items\n";
        assert_eq!(
            todos(rust, Language::Rust),
            [
                (0, "TODO: first".to_string()),
                (1, "fixme later".to_string()),
                (2, "HACK: lifetimes".to_string()),
                (4, "XXX inside a block".to_string()),
                (5, "TODO items".to_string()),
            ]
        );
        assert_eq!(todos("x = '#TODO' # TODO(bob): python\n", Language::Python), [(0, "TODO(bob): python".to_string())]);
        assert_eq!(todos("<!-- TODO: html -->\n<p>TODO</p>\n", Language::Html), [(0, "TODO: html".to_string())]);
        // No comments in plain text.
        assert!(todos("TODO: buy milk\n", Language::PlainText).is_empty());
    }

    #[test]
    fn patterns_are_whole_words_in_any_case() {
        let regex = regex(&["NOTE".to_string(), " ".to_string()]).unwrap();
        assert!(regex.is_match("// note: this"));
        assert!(!regex.is_match("// notes"));
        assert!(super::regex(&[]).is_none());
    }

    #[test]
    fn scans_files_and_open_text() {
        let dir = std::env::temp_dir().join(format!("kuana-todo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, gone) = (dir.join("a.rs"), dir.join("b.rs"), dir.join("gone.rs"));
        std::fs::write(&a, "// TODO: on disk\r\n").unwrap();
        std::fs::write(&b, "// TODO: saved\n").unwrap();
        let overrides = HashMap::from([(b.clone(), Arc::from("fn x() {}\n// FIXME: unsaved\n"))]);
        let regex = regex(&DEFAULT_PATTERNS.map(String::from)).unwrap();
        let mut found = scan(&[a.clone(), b.clone(), gone.clone()], &overrides, &regex, &AtomicBool::new(false));
        found.sort_by(|x, y| x.0.cmp(&y.0));
        let texts: Vec<Vec<String>> = found.iter().map(|(_, t)| t.iter().map(|t| t.text.clone()).collect()).collect();
        assert_eq!(texts, [vec!["TODO: on disk".to_string()], vec!["FIXME: unsaved".to_string()], vec![]]);
        assert_eq!(found[1].1[0].line, 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
