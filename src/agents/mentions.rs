//! `@` mentions in a session's composer: the `@word` being typed at the
//! caret, and the project's files that match it, best first.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

/// Suggestions shown at most.
pub const SHOWN: usize = 6;

/// The `@…` being typed that ends at `caret`: from its `@` (at the start or
/// after a space) up to the caret, when nothing but path characters follow
/// the `@`.
pub fn mention_at(text: &str, caret: usize) -> Option<Range<usize>> {
    let caret = caret.min(text.len());
    if !text.is_char_boundary(caret) {
        return None;
    }
    let before = &text[..caret];
    let at = before.rfind('@')?;
    let word = &before[at + 1..];
    if word.contains(char::is_whitespace) {
        return None;
    }
    let starts_word = before[..at].chars().next_back().is_none_or(char::is_whitespace);
    starts_word.then_some(at..caret)
}

/// The files (under `root`) whose path matches `query`, best first: the
/// file name starting with it, then containing it, then the path containing
/// it, then its letters in order; shorter paths first within each.
pub fn matching(files: &[PathBuf], root: &Path, query: &str) -> Vec<PathBuf> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &PathBuf)> = files
        .iter()
        .filter_map(|path| {
            let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_lowercase();
            let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            let rank = if query.is_empty() {
                3
            } else if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else if rel.contains(&query) {
                2
            } else if in_order(&rel, &query) {
                3
            } else {
                return None;
            };
            Some((rank, rel.len(), path))
        })
        .collect();
    let keep = SHOWN.min(scored.len());
    if keep == 0 {
        return Vec::new();
    }
    scored.select_nth_unstable_by_key(keep - 1, |(rank, len, _)| (*rank, *len));
    scored.truncate(keep);
    scored.sort_by_key(|(rank, len, path)| (*rank, *len, (*path).clone()));
    scored.into_iter().map(|(_, _, path)| path.clone()).collect()
}

/// Whether `query`'s characters appear in `text` in order.
fn in_order(text: &str, query: &str) -> bool {
    let mut chars = text.chars();
    query.chars().all(|q| chars.any(|c| c == q))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_are_words_starting_with_at() {
        assert_eq!(mention_at("look at @src/ma", 15), Some(8..15));
        assert_eq!(mention_at("@", 1), Some(0..1));
        assert_eq!(mention_at("mail me@home", 12), None);
        assert_eq!(mention_at("@src/main.rs and", 16), None);
        assert_eq!(mention_at("@a b", 2), Some(0..2));
    }

    #[test]
    fn files_match_by_name_first() {
        let root = Path::new("/p");
        let files: Vec<PathBuf> = ["/p/src/main.rs", "/p/src/domain/mainline.rs", "/p/docs/main_guide.md", "/p/src/remain.rs", "/p/README.md"].iter().map(PathBuf::from).collect();
        let found = matching(&files, root, "main");
        assert_eq!(found[..3], [PathBuf::from("/p/src/main.rs"), PathBuf::from("/p/docs/main_guide.md"), PathBuf::from("/p/src/domain/mainline.rs")]);
        assert_eq!(found[3], PathBuf::from("/p/src/remain.rs"));
        // Letters in order, the shortest path first.
        assert_eq!(matching(&files, root, "rdm")[0], PathBuf::from("/p/README.md"));
        assert_eq!(matching(&files, root, "zzz"), Vec::<PathBuf>::new());
        assert_eq!(matching(&files, root, "").len(), 5);
    }
}
