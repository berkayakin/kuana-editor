//! Semantic tokens (`textDocument/semanticTokens/full`): the language
//! server's colors over tree-sitter's, for what only it knows (a name that's
//! a type, a constant, an enum member or a macro; a parameter; a mutable
//! variable, underlined as VS Code and JetBrains do). Each token type the
//! themes have a color for maps to a capture name (`capture`); others leave
//! tree-sitter's color. Tokens are asked for when the document's stamp
//! (version, refreshes) moves (`want_semantic_tokens`, each frame), decoded
//! off the UI thread, kept on the document (shifted by edits until the next
//! answer) and laid over the syntax spans in view (`overlay`).

use std::{
    ops::Range,
    sync::{Arc, LazyLock},
    time::Duration,
};

use gpui::{App, Context};
use ropey::Rope;
use serde_json::{Value, json};

use super::Editor;
use crate::{
    lsp::protocol::{self, Encoding, Position},
    syntax::Highlight,
};

/// How long after an edit tokens are asked for again.
const SEMANTIC_PAUSE: Duration = Duration::from_millis(250);

/// The token types and modifiers Kuana tells servers it knows (all of LSP's).
pub const TOKEN_TYPES: &[&str] = &[
    "namespace", "type", "class", "enum", "interface", "struct", "typeParameter", "parameter", "variable", "property", "enumMember",
    "event", "function", "method", "macro", "keyword", "modifier", "comment", "string", "number", "regexp", "operator", "decorator",
];
pub const TOKEN_MODIFIERS: &[&str] = &[
    "declaration", "definition", "readonly", "static", "deprecated", "abstract", "async", "modification", "documentation",
    "defaultLibrary",
];

/// The names a mutable variable gets: underlined.
static MUTABLE: LazyLock<[Highlight; 3]> = LazyLock::new(|| {
    ["variable.mutable", "variable.parameter.mutable", "variable.builtin.mutable"].map(Highlight::from_capture)
});

pub(super) fn is_mutable(highlight: Highlight) -> bool {
    MUTABLE.contains(&highlight)
}

/// The capture name for a token of `kind` with `modifiers`, if themes color it.
pub(crate) fn capture(kind: &str, modifiers: &[&str]) -> Option<String> {
    let builtin = modifiers.contains(&"defaultLibrary");
    let base = match kind {
        "namespace" | "toolModule" | "crateRoot" => "namespace",
        "type" | "class" | "struct" | "enum" | "union" | "typeAlias" | "typeParameter" => {
            if builtin {
                "type.builtin"
            } else {
                "type"
            }
        }
        "interface" | "trait" => "type.interface",
        "builtinType" | "selfTypeKeyword" => "type.builtin",
        "parameter" => "variable.parameter",
        "variable" => {
            if builtin {
                "variable.builtin"
            } else {
                "variable"
            }
        }
        "selfKeyword" => "variable.builtin",
        "property" | "event" => "property",
        "enumMember" | "constParameter" => "constant",
        "boolean" => "constant.builtin",
        "function" => {
            if builtin {
                "function.builtin"
            } else {
                "function"
            }
        }
        "method" => "function.method",
        "macro" => "function.macro",
        "keyword" | "modifier" => "keyword",
        "comment" => "comment",
        "string" => "string",
        "number" => "number",
        "regexp" => "string.regex",
        "escapeSequence" | "formatSpecifier" => "escape",
        "operator" | "arithmetic" | "bitwise" | "comparison" | "logical" => "operator",
        "decorator" | "attribute" | "builtinAttribute" | "derive" | "deriveHelper" => "attribute",
        // Punctuation, lifetimes, labels, unresolved names…: tree-sitter's.
        _ => return None,
    };
    let mutable = modifiers.contains(&"mutable") && base.starts_with("variable");
    Some(if mutable { format!("{base}.mutable") } else { base.to_string() })
}

/// The tokens of a `semanticTokens/full` answer, as byte ranges with their
/// highlights (types and modifiers named by the server's `legend`).
pub(crate) fn decode(data: &[u64], legend: &Value, rope: &Rope, encoding: Encoding) -> Vec<(Range<usize>, Highlight)> {
    let names = |key: &str| -> Vec<String> { legend[key].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect() };
    let (types, modifiers) = (names("tokenTypes"), names("tokenModifiers"));
    // A highlight per (type, modifiers) pair, made once.
    let mut made: std::collections::HashMap<(u64, u64), Option<Highlight>> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(data.len() / 5);
    let (mut line, mut start) = (0u64, 0u64);
    for &[delta_line, delta_start, length, kind, mods] in data.as_chunks::<5>().0 {
        if delta_line > 0 {
            line += delta_line;
            start = delta_start;
        } else {
            start += delta_start;
        }
        let highlight = *made.entry((kind, mods)).or_insert_with(|| {
            let kind = types.get(kind as usize)?;
            let mods: Vec<&str> = modifiers.iter().enumerate().filter(|(i, _)| mods & (1 << i) != 0).map(|(_, m)| m.as_str()).collect();
            capture(kind, &mods).map(|c| Highlight::from_capture(&c))
        });
        let Some(highlight) = highlight else { continue };
        if line as usize >= rope.len_lines() {
            break;
        }
        let at = |character: u64| protocol::position_to_offset(rope, Position { line: line as u32, character: character as u32 }, encoding);
        let range = at(start)..at(start + length);
        if range.start < range.end {
            out.push((range, highlight));
        }
    }
    out
}

/// `base` (syntax spans, sorted and apart) with `top` (tokens, the same)
/// laid over it: where a token is, its highlight.
pub(crate) fn overlay(base: &[(Range<usize>, Highlight)], top: &[(Range<usize>, Highlight)]) -> Vec<(Range<usize>, Highlight)> {
    if top.is_empty() {
        return base.to_vec();
    }
    let mut cuts: Vec<usize> = base.iter().chain(top).flat_map(|(r, _)| [r.start, r.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let (mut b, mut t) = (0, 0);
    let mut out: Vec<(Range<usize>, Highlight)> = Vec::with_capacity(base.len() + top.len());
    for w in cuts.windows(2) {
        let (s, e) = (w[0], w[1]);
        while b < base.len() && base[b].0.end <= s {
            b += 1;
        }
        while t < top.len() && top[t].0.end <= s {
            t += 1;
        }
        let covering = |list: &[(Range<usize>, Highlight)], i: usize| list.get(i).filter(|(r, _)| r.start <= s).map(|(_, h)| *h);
        let Some(highlight) = covering(top, t).or_else(|| covering(base, b)) else { continue };
        match out.last_mut() {
            Some((r, h)) if r.end == s && *h == highlight => r.end = e,
            _ => out.push((s..e, highlight)),
        }
    }
    out
}

impl Editor {
    pub(super) fn semantic_on(&self, cx: &App) -> bool {
        self.mode == super::EditorMode::Full && !self.doc.read(cx).large && self.file_settings.semantic_tokens
    }

    /// Each frame: asks the server for the tokens of today's text once edits
    /// pause (or when it says they changed).
    pub(super) fn want_semantic_tokens(&mut self, cx: &mut Context<Self>) {
        if !self.semantic_on(cx) {
            return;
        }
        let stamp = self.doc.read(cx).semantic_stamp();
        if self.semantic_asked == Some(stamp) {
            return;
        }
        let first = self.semantic_asked.is_none();
        self.semantic_asked = Some(stamp);
        self.semantic_task = Some(cx.spawn(async move |this, cx| {
            if !first {
                cx.background_executor().timer(SEMANTIC_PAUSE).await;
            }
            let asked = this.update(cx, |e, cx| {
                let server = e.lsp_server_with("semanticTokensProvider", cx).filter(|s| {
                    let full = &s.capability_value("semanticTokensProvider")["full"];
                    !full.is_null() && full != &Value::Bool(false)
                })?;
                let params = json!({ "textDocument": e.text_document(cx)? });
                Some((server, params, e.doc.read(cx).rope().clone(), e.doc.read(cx).version()))
            });
            let Ok(Some((server, params, rope, version))) = asked else { return };
            let Ok(answer) = server.request("textDocument/semanticTokens/full", params).await else { return };
            let legend = server.capability_value("semanticTokensProvider")["legend"].clone();
            let encoding = server.encoding;
            let tokens = cx
                .background_executor()
                .spawn(async move {
                    let data: Vec<u64> = answer["data"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
                    decode(&data, &legend, &rope, encoding)
                })
                .await;
            this.update(cx, |e, cx| {
                e.doc.as_mut(cx).set_semantic_tokens(version, Arc::new(tokens));
                cx.notify();
            })
            .ok();
        }));
    }

    /// A server started, or settings changed: tokens are asked for anew.
    pub(super) fn semantic_due(&mut self) {
        self.semantic_asked = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(h: Highlight) -> &'static str {
        h.name()
    }

    #[test]
    fn token_types_map_to_names_themes_color() {
        assert_eq!(capture("parameter", &[]).as_deref(), Some("variable.parameter"));
        assert_eq!(capture("variable", &["mutable"]).as_deref(), Some("variable.mutable"));
        assert_eq!(capture("function", &["defaultLibrary"]).as_deref(), Some("function.builtin"));
        assert_eq!(capture("macro", &[]).as_deref(), Some("function.macro"));
        assert_eq!(capture("enumMember", &["declaration"]).as_deref(), Some("constant"));
        // Left to tree-sitter.
        assert_eq!(capture("brace", &[]), None);
        assert_eq!(capture("unresolvedReference", &[]), None);
        assert!(is_mutable(Highlight::from_capture("variable.mutable")));
        assert!(!is_mutable(Highlight::Variable));
    }

    #[test]
    fn decodes_relative_tokens_by_the_legend() {
        let rope = Rope::from_str("let mut ok = vec![1];\nprint(ok);\n");
        let legend = json!({ "tokenTypes": ["variable", "macro", "function", "brace"], "tokenModifiers": ["declaration", "mutable"] });
        // `ok` (variable, declaration+mutable), `vec` (macro), a brace (skipped); line 2: `print`, `ok`.
        let data = [0, 8, 2, 0, 0b11, 0, 5, 3, 1, 0, 0, 4, 1, 3, 0, 1, 0, 5, 2, 0, 0, 6, 2, 0, 0b10];
        let tokens: Vec<(&str, &str)> = decode(&data, &legend, &rope, Encoding::Utf16)
            .iter()
            .map(|(r, h)| (rope.byte_slice(r.clone()).as_str().unwrap_or(""), name(*h)))
            .collect();
        assert_eq!(tokens, [("ok", "variable.mutable"), ("vec", "function.macro"), ("print", "function"), ("ok", "variable.mutable")]);
    }

    #[test]
    fn edits_move_tokens_and_drop_the_ones_they_touch() {
        use crate::document::{Document, EditKind};
        let mut doc = Document::from_text("aa bb cc\n".to_string(), None);
        let v = Highlight::Variable;
        doc.set_semantic_tokens(doc.version(), Arc::new(vec![(0..2, v), (3..5, v), (6..8, v)]));
        assert_eq!(doc.semantic_generation(), 1);
        // Typing inside `bb`: it goes; `cc` moves; `aa` stays.
        doc.edit(vec![(4..4, "x".into())], &[], EditKind::Typing);
        let left: Vec<Range<usize>> = doc.semantic_in(0..20).iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(left, [0..2, 7..9]);
        // An answer for an older version is dropped.
        doc.set_semantic_tokens(0, Arc::new(Vec::new()));
        assert_eq!(doc.semantic_in(0..20).len(), 2);
    }

    #[test]
    fn tokens_lay_over_syntax_spans() {
        let (v, f, t) = (Highlight::Variable, Highlight::Function, Highlight::Type);
        let base = vec![(0..10, v), (12..20, f)];
        let top = vec![(2..4, t), (8..14, t)];
        assert_eq!(overlay(&base, &top), vec![(0..2, v), (2..4, t), (4..8, v), (8..14, t), (14..20, f)]);
        assert_eq!(overlay(&base, &[]), base);
        // A token where tree-sitter had nothing.
        assert_eq!(overlay(&[(0..2, v)], &[(5..7, f)]), vec![(0..2, v), (5..7, f)]);
    }
}
