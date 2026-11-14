//! Inline values while debugging, as in JetBrains and Zed: paused, the
//! lines of the paused function up to the paused line end with the values of
//! the variables they name (`a = 1, name = "x"`). The values are the top
//! scope's (Locals) of the selected frame (`DebugEvent::Values`), kept on the
//! document beside its debug line (`Document::debug_values`); which names a
//! line has comes from its syntax tree, so strings, comments and fields
//! (`p.x`) don't count.
//!
//! Hovering a name (or a selection) in the paused file evaluates it in the
//! selected frame (`EditorEvaluate` → `DebugSession::evaluate_for_hover`) and
//! the hover shows its value, with children to expand (`EditorDebugChildren`).

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};

use gpui::{AnyElement, App, ClickEvent, Context, EventEmitter, Font, Hsla, SharedString, div, prelude::*, px};
use ropey::Rope;
use tree_sitter::{Node, Tree};

use super::Editor;
use crate::{
    assets::Icon,
    debugger::Var,
    theme::Theme,
    ui::{HoverFill, icon_el},
};

/// Hovering in the paused file: the workspace evaluates `expression` and
/// answers with `Editor::debug_hover_result`.
pub struct EditorEvaluate {
    pub expression: String,
}

impl EventEmitter<EditorEvaluate> for Editor {}

/// A value in the hover was expanded: the workspace answers with
/// `Editor::debug_hover_children`.
pub struct EditorDebugChildren {
    pub reference: i64,
}

impl EventEmitter<EditorDebugChildren> for Editor {}

/// The debugger's part of a hover.
pub(super) struct DebugHover {
    pub expression: String,
    /// None while it's being evaluated.
    pub result: Option<Var>,
    pub children: HashMap<i64, Vec<Var>>,
    pub expanded: HashSet<i64>,
}

/// Longest expression a hover evaluates (a selection can be anything).
const MAX_EXPRESSION: usize = 200;
/// Rows a hover's value tree shows at most (a big array opened).
const MAX_HOVER_ROWS: usize = 200;

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

    /// What hovering at `offset` evaluates while the file is paused: the
    /// selection when it holds `offset`, else the name there with what it's
    /// a field of (`self.items`, `p.x`).
    pub(super) fn debug_expression(&self, offset: usize, cx: &App) -> Option<String> {
        let doc = self.doc.read(cx);
        doc.debug_row()?;
        let selection = self.primary_selection();
        let range = if !selection.is_empty() && selection.range().contains(&offset) {
            selection.range()
        } else {
            hover_expression(doc.syntax().tree()?, offset)?
        };
        let text = doc.slice(range);
        (!text.trim().is_empty() && !text.contains('\n') && text.len() <= MAX_EXPRESSION).then(|| text.trim().to_string())
    }

    /// The debugger's answer for the hover's expression (an error shows nothing).
    pub fn debug_hover_result(&mut self, expression: &str, result: Result<Var, String>, cx: &mut Context<Self>) {
        let Some(hover) = self.lsp.hover.as_mut() else { return };
        if hover.debug.as_ref().is_none_or(|d| d.expression != expression) {
            return;
        }
        match result {
            Ok(var) => {
                if let Some(debug) = hover.debug.as_mut() {
                    debug.result = Some(var);
                }
            }
            Err(_) => {
                hover.debug = None;
                if hover.is_empty() {
                    self.lsp.hover = None;
                }
            }
        }
        cx.notify();
    }

    /// A hovered value's children arrived.
    pub fn debug_hover_children(&mut self, reference: i64, vars: Vec<Var>, cx: &mut Context<Self>) {
        if let Some(debug) = self.lsp.hover.as_mut().and_then(|h| h.debug.as_mut()) {
            debug.children.insert(reference, vars);
            cx.notify();
        }
    }

    /// Opens or closes a value in the hover, asking for its children once.
    pub(super) fn toggle_debug_child(&mut self, reference: i64, cx: &mut Context<Self>) {
        let Some(debug) = self.lsp.hover.as_mut().and_then(|h| h.debug.as_mut()) else { return };
        if !debug.expanded.remove(&reference) {
            debug.expanded.insert(reference);
            if !debug.children.contains_key(&reference) {
                cx.emit(EditorDebugChildren { reference });
            }
        }
        cx.notify();
    }

    /// The hover's value as a tree, as the Debug panel's Variables draws it:
    /// the expression and its value, then children opened with their arrows.
    pub(super) fn debug_hover_rows(&self, debug: &DebugHover, mono: &Font, cx: &mut Context<Self>) -> Option<AnyElement> {
        let var = debug.result.clone()?;
        let theme = Theme::get(cx).clone();
        let code_size = px(theme.editor_font_size - 1.);
        let mut rows = Vec::new();
        let mut stack: Vec<(Var, usize, String)> = vec![(var, 0, "0".to_string())];
        while let Some((var, depth, key)) = stack.pop() {
            if rows.len() >= MAX_HOVER_ROWS {
                break;
            }
            let reference = var.reference;
            let open = debug.expanded.contains(&reference);
            rows.push(
                div()
                    .id(SharedString::from(format!("debug-hover-{key}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .min_h(px(20.))
                    .pl(px(depth as f32 * 14.))
                    // The first row leaves room for the popover's Copy button.
                    .pr(px(if depth == 0 { 26. } else { 4. }))
                    .rounded(px(4.))
                    .font(mono.clone())
                    .text_size(code_size)
                    .when(reference > 0, |el| {
                        el.cursor_pointer().on_click(cx.listener(move |e, _: &ClickEvent, _, cx| e.toggle_debug_child(reference, cx)))
                    })
                    .row_hover(if reference > 0 { Hsla::from(theme.hover) } else { gpui::transparent_black() })
                    .child(div().w(px(12.)).flex_shrink_0().when(reference > 0, |el| {
                        el.child(icon_el(if open { Icon::ChevronDown } else { Icon::ChevronRight }, px(11.), theme.text_dim.into()))
                    }))
                    .child(div().flex_shrink_0().text_color(if depth == 0 { theme.text } else { theme.syntax.property }).child(var.name.clone()))
                    .when(!var.value.is_empty(), |el| {
                        el.child(div().text_color(theme.text_dim).child("="))
                            .child(div().flex_1().min_w_0().text_color(theme.info).child(var.value.clone()))
                    })
                    .when_some(var.ty.clone(), |el, ty| el.child(div().flex_shrink_0().pl_2().text_size(px(11.)).text_color(theme.text_dim).child(ty)))
                    .into_any_element(),
            );
            if open && let Some(children) = debug.children.get(&reference) {
                for (ix, child) in children.iter().enumerate().rev() {
                    stack.push((child.clone(), depth + 1, format!("{key}-{ix}")));
                }
            }
        }
        Some(div().flex().flex_col().children(rows).into_any_element())
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

/// Member accesses: a name after the object is the whole access.
const MEMBER_KINDS: &[&str] = &["field_expression", "member_expression", "attribute", "selector_expression"];

/// The expression a hover at `offset` evaluates: the name there (a variable,
/// a field, `self`/`this`) widened to what it's a field of, so `items` in
/// `self.items.len()` is `self.items`. None over anything else.
pub(crate) fn hover_expression(tree: &Tree, offset: usize) -> Option<Range<usize>> {
    let node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    let kind = node.kind();
    let name = (kind.contains("identifier") && kind != "type_identifier") || matches!(kind, "self" | "this");
    if !name || node.child_count() > 0 {
        return None;
    }
    let mut node = node;
    while let Some(parent) = node.parent().filter(|p| MEMBER_KINDS.contains(&p.kind()) && node.start_byte() > p.start_byte()) {
        node = parent;
    }
    Some(node.start_byte()..node.end_byte())
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

    fn hovered(language: Language, text: &str, at: &str) -> Option<String> {
        let rope = Rope::from_str(text);
        let syntax = Syntax::new(language, &rope);
        let offset = text.find(at).unwrap();
        hover_expression(syntax.tree().unwrap(), offset).map(|r| text[r].to_string())
    }

    #[test]
    fn hovers_evaluate_names_with_what_they_are_fields_of() {
        let rust = "fn f(&self) -> usize {\n    let n = self.items.len() + p.x;\n    n\n}\n";
        assert_eq!(hovered(Language::Rust, rust, "n =").as_deref(), Some("n"));
        assert_eq!(hovered(Language::Rust, rust, "items").as_deref(), Some("self.items"));
        assert_eq!(hovered(Language::Rust, rust, "self.items").as_deref(), Some("self"));
        assert_eq!(hovered(Language::Rust, rust, "x;").as_deref(), Some("p.x"));
        assert_eq!(hovered(Language::Rust, rust, "usize"), None);
        assert_eq!(hovered(Language::Rust, rust, "let"), None);

        let python = "def f(self):\n    total = self.count + other.size\n";
        assert_eq!(hovered(Language::Python, python, "count").as_deref(), Some("self.count"));
        assert_eq!(hovered(Language::Python, python, "size").as_deref(), Some("other.size"));
        assert_eq!(hovered(Language::Python, python, "total").as_deref(), Some("total"));

        let js = "function f(a) {\n  return a.b.c + this.d;\n}\n";
        assert_eq!(hovered(Language::JavaScript, js, "c +").as_deref(), Some("a.b.c"));
        assert_eq!(hovered(Language::JavaScript, js, "b.c").as_deref(), Some("a.b"));
        assert_eq!(hovered(Language::JavaScript, js, "d;").as_deref(), Some("this.d"));
        assert_eq!(hovered(Language::JavaScript, js, "return"), None);
    }

    #[test]
    fn no_values_no_lines() {
        assert!(values_in(Language::Rust, "fn main() {}\n", 0, &[]).is_empty());
        assert!(values_in(Language::Rust, "fn main() {}\n", 9, &[("a", "1")]).is_empty());
    }
}
