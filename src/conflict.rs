//! Merge conflict regions marked by `<<<<<<<`, `|||||||` (diff3 base),
//! `=======` and `>>>>>>>` lines, and how to resolve them.
//!
//! Git merges by lines, so changes to the same or neighboring lines conflict
//! even when they don't touch the same words. With the base in the markers
//! (Kuana runs merges with `zdiff3`), `merge3` merges such a conflict word by
//! word, as JetBrains' "Resolve simple conflicts" does: it combines the two
//! sides when their changes neither overlap nor touch, or are the same.

use std::ops::Range;
use std::time::Duration;

/// One conflict. Rows are buffer lines; the content ranges exclude markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The `<<<<<<<` row.
    pub start: usize,
    /// The `>>>>>>>` row.
    pub end: usize,
    pub ours: Range<usize>,
    pub base: Option<Range<usize>>,
    pub theirs: Range<usize>,
    /// What follows the markers, e.g. "HEAD" and "feature".
    pub ours_label: String,
    pub theirs_label: String,
    /// Both sides' changes combined, when they combine cleanly (`merge3`).
    pub merged: Option<String>,
}

impl Conflict {
    pub fn rows(&self) -> Range<usize> {
        self.start..self.end + 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Keep the current branch's side (ours).
    Current,
    /// Keep the merged-in side (theirs).
    Incoming,
    /// Keep both, current first.
    Both,
    /// Both sides' changes merged word by word (`Conflict::merged`).
    Combination,
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Resolution::Current => "Accept Current",
            Resolution::Incoming => "Accept Incoming",
            Resolution::Both => "Accept Both",
            Resolution::Combination => "Accept Combination",
        }
    }

    /// The rows that replace the whole conflict (`Combination` replaces it
    /// with `Conflict::merged` instead).
    pub fn kept(self, c: &Conflict) -> Vec<Range<usize>> {
        match self {
            Resolution::Current => vec![c.ours.clone()],
            Resolution::Incoming => vec![c.theirs.clone()],
            Resolution::Both => vec![c.ours.clone(), c.theirs.clone()],
            Resolution::Combination => Vec::new(),
        }
    }
}

fn marker(line: &str, mark: char) -> Option<&str> {
    let line = line.trim_end_matches(['\n', '\r']);
    let rest = line.strip_prefix(&*mark.to_string().repeat(7))?;
    // Exactly seven: `========` is a Markdown underline, not a marker.
    if rest.starts_with(mark) {
        return None;
    }
    match rest.strip_prefix(' ') {
        Some(label) => Some(label.trim()),
        None if rest.is_empty() => Some(""),
        None => None,
    }
}

/// The conflicts in `text` (rows are `\n`-separated lines).
pub fn parse(text: &str) -> Vec<Conflict> {
    if !text.contains("<<<<<<<") {
        return Vec::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    // Rows inside a conflict, each with its line break.
    let rows = |r: &Range<usize>| -> String { lines[r.clone()].iter().flat_map(|l| [*l, "\n"]).collect() };
    let mut out = Vec::new();
    let mut open: Option<(usize, String, Option<usize>, Option<usize>)> = None; // start, label, base row, ======= row
    for (row, &line) in lines.iter().enumerate() {
        if let Some(label) = marker(line, '<') {
            open = Some((row, label.to_string(), None, None));
        } else if let Some((_, _, base, sep @ None)) = open.as_mut() {
            if marker(line, '|').is_some() && base.is_none() {
                *base = Some(row);
            } else if marker(line, '=').is_some_and(str::is_empty) {
                *sep = Some(row);
            }
        } else if let Some(label) = marker(line, '>')
            && let Some((start, ours_label, base, Some(sep))) = open.take()
        {
            let (ours, base, theirs) = (start + 1..base.unwrap_or(sep), base.map(|b| b + 1..sep), sep + 1..row);
            let (ours_text, theirs_text) = (rows(&ours), rows(&theirs));
            let merged = match &base {
                Some(base) => merge3(&rows(base), &ours_text, &theirs_text),
                None => (ours_text == theirs_text).then_some(ours_text),
            };
            out.push(Conflict { start, end: row, ours, base, theirs, ours_label, theirs_label: label.to_string(), merged });
        }
    }
    out
}

/// `text` with every conflict that combines cleanly replaced by its
/// combination; also how many were and how many are left.
pub fn resolve_simple(text: &str) -> (String, usize, usize) {
    let conflicts = parse(text);
    let (mut resolved, mut left) = (0, 0);
    if conflicts.iter().all(|c| c.merged.is_none()) {
        return (text.to_string(), 0, conflicts.len());
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut row = 0;
    for c in &conflicts {
        for line in &lines[row..c.start] {
            out.push_str(line);
            out.push('\n');
        }
        match &c.merged {
            Some(merged) => {
                out.push_str(merged);
                resolved += 1;
            }
            None => {
                for line in &lines[c.start..=c.end] {
                    out.push_str(line);
                    out.push('\n');
                }
                left += 1;
            }
        }
        row = c.end + 1;
    }
    if row == lines.len() {
        // The last conflict ended the file, without a final line break.
        out.pop();
    } else {
        out.push_str(&lines[row..].join("\n"));
    }
    (out, resolved, left)
}

/// Merges two edits of `base` word by word: `None` when they change the same
/// or neighboring words differently (a real conflict).
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Option<String> {
    if ours == theirs || base == theirs {
        return Some(ours.to_string());
    }
    if base == ours {
        return Some(theirs.to_string());
    }
    // A conflict this big is better looked at than combined.
    if base.len() + ours.len() + theirs.len() > 256 * 1024 {
        return None;
    }
    let (b, o, t) = (tokens(base), tokens(ours), tokens(theirs));
    let (ours_edits, theirs_edits) = (edits(&b, &o), edits(&b, &t));
    let text = |toks: &[&str], r: &Range<usize>| toks[r.clone()].concat();
    let mut out = String::with_capacity(ours.len().max(theirs.len()));
    let mut at = 0;
    let mut apply = |edit: &Edit, side: &[&str], out: &mut String| {
        out.push_str(&b[at..edit.old.start].concat());
        out.push_str(&text(side, &edit.new));
        at = edit.old.end;
    };
    let (mut i, mut j) = (0, 0);
    loop {
        match (ours_edits.get(i), theirs_edits.get(j)) {
            (None, None) => break,
            (Some(a), None) => {
                apply(a, &o, &mut out);
                i += 1;
            }
            (None, Some(c)) => {
                apply(c, &t, &mut out);
                j += 1;
            }
            (Some(a), Some(c)) => {
                // Overlapping or touching (an insertion right where the other
                // side changes something): only the same change goes in.
                if a.old.start <= c.old.end && c.old.start <= a.old.end {
                    if a.old != c.old || text(&o, &a.new) != text(&t, &c.new) {
                        return None;
                    }
                    apply(a, &o, &mut out);
                    i += 1;
                    j += 1;
                } else if a.old.start < c.old.start {
                    apply(a, &o, &mut out);
                    i += 1;
                } else {
                    apply(c, &t, &mut out);
                    j += 1;
                }
            }
        }
    }
    out.push_str(&b[at..].concat());
    Some(out)
}

/// A change from base tokens `old` to a side's tokens `new`.
struct Edit {
    old: Range<usize>,
    new: Range<usize>,
}

/// The changes from `base` to `side`, neighboring ones joined.
fn edits(base: &[&str], side: &[&str]) -> Vec<Edit> {
    // Past the deadline the diff gets coarser, which only means more conflicts.
    let diff = similar::TextDiff::configure().timeout(Duration::from_millis(50)).diff_slices(base, side);
    let mut out: Vec<Edit> = Vec::new();
    for op in diff.ops() {
        if op.tag() == similar::DiffTag::Equal {
            continue;
        }
        let (old, new) = (op.old_range(), op.new_range());
        match out.last_mut() {
            Some(last) if last.old.end == old.start && last.new.end == new.start => {
                last.old.end = old.end;
                last.new.end = new.end;
            }
            _ => out.push(Edit { old, new }),
        }
    }
    out
}

/// Words (letters, digits, `_`), runs of spaces and tabs, line breaks and
/// single other characters. Dotted numbers are one word: `0.1.1` and `0.2.0`
/// are two versions, not changes to different parts to combine into `0.2.1`.
fn tokens(text: &str) -> Vec<&str> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c == ' ' || c == '\t' {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let kind = class(c);
        let mut end = i + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|&(_, n)| n == '\n') {
            end += 1;
            chars.next();
        } else if kind != Class::Other {
            while let Some(&(j, n)) = chars.peek() {
                let dotted_number = kind == Class::Word
                    && n == '.'
                    && text[..j].ends_with(|c: char| c.is_ascii_digit())
                    && text[j + 1..].starts_with(|c: char| c.is_ascii_digit());
                if class(n) != kind && !dotted_number {
                    break;
                }
                end = j + n.len_utf8();
                chars.next();
            }
        }
        out.push(&text[i..end]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_and_three_way_conflicts() {
        let text = "a\n<<<<<<< HEAD\nours\n=======\ntheirs 1\ntheirs 2\n>>>>>>> feature\nb\n<<<<<<< HEAD\nx\n||||||| base\no\n=======\ny\n>>>>>>> other\n";
        let c = parse(text);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].start, c[0].end, c[0].ours.clone(), c[0].theirs.clone()), (1, 6, 2..3, 4..6));
        assert_eq!((c[0].ours_label.as_str(), c[0].theirs_label.as_str()), ("HEAD", "feature"));
        assert_eq!((c[1].ours.clone(), c[1].base.clone(), c[1].theirs.clone()), (9..10, Some(11..12), 13..14));
        assert_eq!(Resolution::Both.kept(&c[0]), [2..3, 4..6]);
    }

    #[test]
    fn ignores_lookalikes_and_unfinished_conflicts() {
        assert!(parse("Title\n========\n<<<<<<<< not a marker\n").is_empty());
        assert!(parse("<<<<<<< HEAD\nours\n=======\ntheirs\n").is_empty());
        // An empty side is fine.
        let c = parse("<<<<<<< HEAD\n=======\nt\n>>>>>>> x\n");
        assert_eq!((c[0].ours.clone(), c[0].theirs.clone()), (1..1, 2..3));
    }

    #[test]
    fn merges_changes_to_different_words() {
        // Neighboring lines: Git's line merge conflicts, the words don't.
        let base = "let x = 1;\nlet y = 2;\n";
        assert_eq!(merge3(base, "let x = 10;\nlet y = 2;\n", "let x = 1;\nlet y = 20;\n").as_deref(), Some("let x = 10;\nlet y = 20;\n"));
        // The same line, different words.
        assert_eq!(merge3("call(a, b)\n", "call(alpha, b)\n", "call(a, beta)\n").as_deref(), Some("call(alpha, beta)\n"));
        // One side added a line, the other changed one further down.
        assert_eq!(merge3("a\nb\nc\n", "a\nnew\nb\nc\n", "a\nb\nC\n").as_deref(), Some("a\nnew\nb\nC\n"));
        // The same change on both sides, plus one of each.
        assert_eq!(merge3("f(1, 2, 3)\n", "f(1, 5, 3) // x\n", "g(1, 5, 3)\n").as_deref(), Some("g(1, 5, 3) // x\n"));
        // Only one side changed anything.
        assert_eq!(merge3("a\n", "a\n", "b\n").as_deref(), Some("b\n"));
        // CRLF line breaks stay as they are.
        assert_eq!(merge3("a = 1\r\nb = 2\r\n", "a = 3\r\nb = 2\r\n", "a = 1\r\nb = 4\r\n").as_deref(), Some("a = 3\r\nb = 4\r\n"));
    }

    #[test]
    fn keeps_real_conflicts() {
        // The same word, changed differently.
        assert_eq!(merge3("x = 1\n", "x = 2\n", "x = 3\n"), None);
        // Both added something at the same place.
        assert_eq!(merge3("a\n", "a\nb\n", "a\nc\n"), None);
        // Versions are one word.
        assert_eq!(merge3("v = 0.1.0\n", "v = 0.1.1\n", "v = 0.2.0\n"), None);
        // An insertion right next to the other side's change.
        assert_eq!(merge3("foo\n", "foo()\n", "bar\n"), None);
        // One side deleted what the other changed.
        assert_eq!(merge3("a\nb\nc\n", "a\nc\n", "a\nB\nc\n"), None);
    }

    #[test]
    fn resolves_the_simple_conflicts_in_a_file() {
        let text = "start\n<<<<<<< HEAD\nx = 10; y = 2\n||||||| base\nx = 1; y = 2\n=======\nx = 1; y = 20\n>>>>>>> them\nmiddle\n<<<<<<< HEAD\nz = 1\n||||||| base\nz = 0\n=======\nz = 2\n>>>>>>> them\nend";
        let c = parse(text);
        assert_eq!(c[0].merged.as_deref(), Some("x = 10; y = 20\n"));
        assert_eq!(c[1].merged, None);
        let (out, resolved, left) = resolve_simple(text);
        assert_eq!((resolved, left), (1, 1));
        assert_eq!(out, text.replace("<<<<<<< HEAD\nx = 10; y = 2\n||||||| base\nx = 1; y = 2\n=======\nx = 1; y = 20\n>>>>>>> them\n", "x = 10; y = 20\n"));
        // Without the base, only identical sides combine.
        let (out, resolved, left) = resolve_simple("<<<<<<< HEAD\nsame\n=======\nsame\n>>>>>>> x\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x");
        assert_eq!((out.as_str(), resolved, left), ("same\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x", 1, 1));
        // A file ending in a resolved conflict keeps having no final line break.
        assert_eq!(resolve_simple("<<<<<<< HEAD\nv\n=======\nv\n>>>>>>> x").0, "v");
    }
}
