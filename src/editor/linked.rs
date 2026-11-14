//! Linked editing, as VS Code's `editor.linkedEditing` and Zed: typing in a
//! tag's name changes its closing (or opening) tag's name with it, in one
//! undo step. The pair comes from the syntax tree (`tag_pair`), so it works
//! for HTML, JSX/TSX and grammars shaped like them (Vue, Svelte, XML from
//! extensions) without a language server. Once a pair is being edited it's
//! kept (`Editor::linked`) while the two names stay equal, so emptying a
//! name and typing a new one still renames both, though `<>` has no name in
//! the tree.

use std::ops::Range;

use gpui::{App, Context};
use tree_sitter::{Node, Tree};

use super::Editor;

/// Opening and closing tags in the grammars above.
const OPENING: &[&str] = &["start_tag", "jsx_opening_element", "STag"];
const CLOSING: &[&str] = &["end_tag", "jsx_closing_element", "ETag"];
/// What a tag's name is, when the grammar has no `name` field.
const NAMES: &[&str] = &["tag_name", "Name", "identifier", "member_expression", "nested_identifier", "jsx_namespace_name"];

impl Editor {
    /// For an edit at one caret: the same edit in the linked tag name, if
    /// the edit is inside a name whose pair reads the same and leaves a name.
    /// Remembers the pair as it will be after both edits.
    pub(super) fn linked_edit(&mut self, edits: &[(Range<usize>, String)], cx: &mut Context<Self>) -> Option<(Range<usize>, String)> {
        let found = self.linked_pair(edits, cx);
        self.linked = None;
        let ((range, text), (a, b)) = found?;
        let doc = self.doc.read(cx);
        let name = format!("{}{}{}", doc.slice(a.start..range.start), text, doc.slice(range.end..a.end));
        if !name.chars().all(is_name_char) {
            return None;
        }
        let at = b.start + (range.start - a.start);
        let mirror = (at..at + range.len(), text.clone());
        // Where both names will be once both edits are in.
        let delta = text.len() as isize - range.len() as isize;
        let grow = |r: &Range<usize>| r.start..(r.end as isize + delta) as usize;
        let shift = |r: &Range<usize>| (r.start as isize + delta) as usize..(r.end as isize + 2 * delta) as usize;
        self.linked = Some(if a.start < b.start { (grow(&a), shift(&b)) } else { (shift(&a), grow(&b)) });
        Some(mirror)
    }

    /// The one edit, and the pair of names it's in (it's in the first).
    #[allow(clippy::type_complexity)]
    fn linked_pair(&self, edits: &[(Range<usize>, String)], cx: &App) -> Option<((Range<usize>, String), (Range<usize>, Range<usize>))> {
        let [edit] = edits else { return None };
        if !self.file_settings.linked_editing || self.is_chromeless() {
            return None;
        }
        let doc = self.doc.read(cx);
        let inside = |r: &Range<usize>| r.start <= edit.0.start && edit.0.end <= r.end;
        let kept = self.linked.clone().filter(|_| self.linked_version == doc.version()).and_then(|(a, b)| {
            if inside(&a) {
                Some((a, b))
            } else if inside(&b) {
                Some((b, a))
            } else {
                None
            }
        });
        let (a, b) = kept.or_else(|| tag_pair(doc.syntax().tree()?, edit.0.start).filter(|(a, _)| inside(a)))?;
        let in_text = a.end <= doc.len() && b.end <= doc.len();
        (in_text && a != b && doc.slice(a.clone()) == doc.slice(b.clone())).then(|| (edit.clone(), (a, b)))
    }
}

/// Characters a tag's name can have (HTML's custom elements, JSX's
/// `Foo.Bar`, XML's `ns:name`).
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '$')
}

/// The name of the tag at `offset` (from its first character to just past
/// its last) and the name of the tag it pairs with.
pub(crate) fn tag_pair(tree: &Tree, offset: usize) -> Option<(Range<usize>, Range<usize>)> {
    // Just past a name (the caret after `<div`) is in it too.
    let leaf = tree.root_node().descendant_for_byte_range(offset, offset)?;
    let before = offset.checked_sub(1).and_then(|o| tree.root_node().descendant_for_byte_range(o, o));
    [Some(leaf), before].into_iter().flatten().find_map(|node| {
        let tag = std::iter::successors(Some(node), |n| n.parent()).take(4).find(|n| is_tag(n.kind()))?;
        let name = tag_name(tag)?;
        if !(name.start_byte() <= offset && offset <= name.end_byte()) {
            return None;
        }
        let opening = OPENING.contains(&tag.kind());
        let element = tag.parent()?;
        let mut cursor = element.walk();
        let other = element
            .children(&mut cursor)
            .find(|c| c.id() != tag.id() && if opening { CLOSING.contains(&c.kind()) } else { OPENING.contains(&c.kind()) })?;
        let other_name = tag_name(other)?;
        Some((name.byte_range(), other_name.byte_range()))
    })
}

fn is_tag(kind: &str) -> bool {
    OPENING.contains(&kind) || CLOSING.contains(&kind)
}

fn tag_name(tag: Node) -> Option<Node> {
    tag.child_by_field_name("name").or_else(|| {
        let mut cursor = tag.walk();
        tag.named_children(&mut cursor).find(|c| NAMES.contains(&c.kind()))
    })
}

#[cfg(test)]
mod tests {
    use ropey::Rope;

    use super::*;
    use crate::syntax::{Language, Syntax};

    /// The pair's texts for the caret at the `|` in `text`.
    fn pair(language: Language, text: &str) -> Option<(String, String)> {
        let at = text.find('|').unwrap();
        let text = text.replacen('|', "", 1);
        let rope = Rope::from_str(&text);
        let syntax = Syntax::new(language, &rope);
        let (a, b) = tag_pair(syntax.tree().unwrap(), at)?;
        Some((format!("{}@{}", &text[a.clone()], a.start), format!("{}@{}", &text[b.clone()], b.start)))
    }

    #[test]
    fn html_pairs_opening_and_closing_names() {
        let p = |t| pair(Language::Html, t);
        assert_eq!(p("<d|iv>x</div>"), Some(("div@1".into(), "div@8".into())));
        // Either end of the name, and from the closing tag.
        assert_eq!(p("<|div>x</div>"), Some(("div@1".into(), "div@8".into())));
        assert_eq!(p("<div|>x</div>"), Some(("div@1".into(), "div@8".into())));
        assert_eq!(p("<div>x</di|v>"), Some(("div@8".into(), "div@1".into())));
        // Nested elements of the same name pair with their own.
        assert_eq!(p("<div><d|iv></div></div>"), Some(("div@6".into(), "div@12".into())));
        assert_eq!(p("<div><div></div></d|iv>"), Some(("div@18".into(), "div@1".into())));
        // Attributes, text and tags without a pair don't.
        assert_eq!(p("<div cl|ass=\"a\">x</div>"), None);
        assert_eq!(p("<div>t|ext</div>"), None);
        assert_eq!(p("<b|r>"), None);
        assert_eq!(p("<p|>unclosed"), None);
    }

    #[test]
    fn jsx_pairs_names_and_member_names() {
        let p = |t| pair(Language::Tsx, t);
        assert_eq!(p("const a = <Fo|o>x</Foo>;"), Some(("Foo@11".into(), "Foo@18".into())));
        assert_eq!(p("const a = <UI.Bu|tton>x</UI.Button>;"), Some(("UI.Button@11".into(), "UI.Button@24".into())));
        assert_eq!(p("const a = <Foo x={1} />;".replacen("Foo", "F|oo", 1).as_str()), None);
        assert_eq!(pair(Language::JavaScript, "const a = <di|v>x</div>;"), Some(("div@11".into(), "div@18".into())));
    }
}
