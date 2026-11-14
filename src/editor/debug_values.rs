//! Inline values while debugging, as in JetBrains and Zed: paused, the
//! lines of the paused function up to the paused line end with the values of
//! the variables they name (`a = 1, name = "x"`). The values are the top
//! scope's (Locals) of the selected frame (`DebugEvent::Values`), kept on the
//! document beside its debug line (`Document::debug_values`); which names a
//! line has comes from its syntax tree, so strings, comments and fields
//! (`p.x`) don't count.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::{App, Context};
use ropey::Rope;
use tree_sitter::{Node, Tree};

use super::Editor;

/// Lines above the paused one that can show values, at most.
const MAX_ROWS: usize = 300;
/// A value shown longer than this is cut.
const MAX_VALUE: usize = 40;
/// A line's values, together, at most.
const MAX_LINE: usize = 120;

impl Editor {
    /// Per line with values (sorted): what's shown after it. Kept for the
    /// document's text, tree and values, so a frame only looks it up.
    pub(super) fn debug_value_rows(&mut self, cx: &App) -> Arc<Vec<(usize, String)>> {
        let doc = self.doc.read(cx);
        let (Some(values), Some(row), Some(tree)) = (doc.debug_values(), doc.debug_row(), doc.syntax().tree()) else {
            return Arc::default();
        };
        if !self.file_settings.inline_values {
            return Arc::default();
        }
        let key = (doc.id(), doc.version(), doc.tree_version(), Arc::as_ptr(values) as usize, row);
        if self.debug_values.0 != Some(key) {
            let rows = inline_values(tree, doc.rope(), row, values);
            self.debug_values = (Some(key), Arc::new(rows));
        }
        self.debug_values.1.clone()
    }

    /// Shows these values (None: none) on the document's paused lines.
    pub fn set_debug_values(&mut self, values: Option<Arc<Vec<(String, String)>>>, cx: &mut Context<Self>) {
        self.doc.as_mut(cx).set_debug_values(values);
        cx.notify();
    }
}

/// For the function holding `stop_row` (or the lines above it, up to
/// `MAX_ROWS`), from its first line to `stop_row`: per line naming any of
/// `values`, those names' values in the order they first appear there.
pub(crate) fn inline_values(tree: &Tree, rope: &Rope, stop_row: usize, values: &[(String, String)]) -> Vec<(usize, String)> {
    if values.is_empty() || stop_row >= rope.len_lines() {
        return Vec::new();
    }
    let lookup: HashMap<&str, &str> = values.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let line = rope.line(stop_row);
    let indent = line.chars().take_while(|c| c.is_whitespace() && *c != '\n').map(char::len_utf8).sum::<usize>();
    let at = rope.line_to_byte(stop_row) + indent;
    let lowest = stop_row.saturating_sub(MAX_ROWS);
    let start_row = tree
        .root_node()
        .descendant_for_byte_range(at, at)
        .and_then(|node| std::iter::successors(Some(node), |n| n.parent()).find(|n| is_function(n.kind())))
        .map_or(lowest, |f| f.start_position().row.max(lowest));
    let end = if stop_row + 1 < rope.len_lines() { rope.line_to_byte(stop_row + 1) } else { rope.len_bytes() };
    let range = rope.line_to_byte(start_row)..end;

    let mut names: Vec<(usize, Vec<&str>)> = Vec::new();
    visit(tree.root_node(), &range, &mut |node| {
        let Some(name) = variable_name(node, rope) else { return };
        let Some((&name, _)) = lookup.get_key_value(name.as_str()) else { return };
        let row = node.start_position().row;
        match names.last_mut() {
            Some((r, list)) if *r == row => {
                if !list.contains(&name) {
                    list.push(name);
                }
            }
            _ => names.push((row, vec![name])),
        }
    });
    names
        .into_iter()
        .map(|(row, list)| {
            let mut text = String::new();
            for name in list {
                if !text.is_empty() {
                    text.push_str(", ");
                }
                text.push_str(name);
                text.push_str(" = ");
                text.push_str(&short(lookup[name]));
                if text.len() > MAX_LINE {
                    text.truncate(text.floor_char_boundary(MAX_LINE));
                    text.push('…');
                    break;
                }
            }
            (row, text)
        })
        .collect()
}

/// Depth first, in source order, the nodes overlapping `range`.
fn visit<'t>(node: Node<'t>, range: &Range<usize>, f: &mut impl FnMut(Node<'t>)) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.end_byte() <= range.start || child.start_byte() >= range.end {
            continue;
        }
        if child.child_count() == 0 {
            f(child);
        } else {
            visit(child, range, f);
        }
    }
}

/// A function's node in the grammars Kuana knows (Rust, Go, Python,
/// JavaScript/TypeScript, C/C++ and the like).
fn is_function(kind: &str) -> bool {
    kind.contains("function") || kind.contains("method") || kind == "closure_expression" || kind == "func_literal" || kind == "lambda"
}

/// The name a leaf names as a variable: an identifier, not a field or
/// method after `.`, `->` or `::`.
fn variable_name(node: Node, rope: &Rope) -> Option<String> {
    let kind = node.kind();
    if kind != "identifier" && !kind.starts_with("shorthand_property_identifier") {
        return None;
    }
    if node.prev_sibling().is_some_and(|p| matches!(p.kind(), "." | "->" | "::" | "?.")) {
        return None;
    }
    Some(rope.byte_slice(node.start_byte()..node.end_byte()).to_string())
}

/// A value on one line, cut to `MAX_VALUE`.
fn short(value: &str) -> String {
    let mut text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() > MAX_VALUE {
        text.truncate(text.floor_char_boundary(MAX_VALUE));
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{Language, Syntax};

    fn values_in(language: Language, text: &str, stop_row: usize, values: &[(&str, &str)]) -> Vec<(usize, String)> {
        let rope = Rope::from_str(text);
        let syntax = Syntax::new(language, &rope);
        let values: Vec<(String, String)> = values.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        inline_values(syntax.tree().unwrap(), &rope, stop_row, &values)
    }

    #[test]
    fn lines_of_the_function_up_to_the_paused_one() {
        let src = "fn other() {\n    let a = 0;\n}\n\nfn main() {\n    let a = 1;\n    // a in a comment\n    let s = \"a\";\n    let b = a + p.a;\n    println!(\"{b}\");\n    let c = a;\n}\n";
        let values = [("a", "1"), ("b", "3"), ("s", "\"a\""), ("c", "1")];
        assert_eq!(
            values_in(Language::Rust, src, 8, &values),
            vec![(5, "a = 1".to_string()), (7, "s = \"a\"".to_string()), (8, "b = 3, a = 1".to_string())],
        );
    }

    #[test]
    fn python_skips_attributes_and_cuts_long_values() {
        let src = "def f(items):\n    total = len(items)\n    self.total = total\n    return total\n";
        let long = "x".repeat(100);
        let values = [("items", long.as_str()), ("total", "3")];
        let rows = values_in(Language::Python, src, 2, &values);
        assert_eq!(rows[0].0, 0);
        assert!(rows[0].1.starts_with("items = xxx") && rows[0].1.ends_with('…') && rows[0].1.len() < 60);
        assert_eq!(rows[1], (1, format!("total = 3, items = {}…", "x".repeat(MAX_VALUE))));
        assert_eq!(rows[2], (2, "total = 3".to_string()));
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn no_values_no_lines() {
        assert!(values_in(Language::Rust, "fn main() {}\n", 0, &[]).is_empty());
        assert!(values_in(Language::Rust, "fn main() {}\n", 9, &[("a", "1")]).is_empty());
    }
}
