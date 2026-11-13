//! Postfix templates: `expr.if` → `if expr { … }`, as in JetBrains IDEs. A
//! language's templates are the `[postfix]` table of its `config.toml`
//! (built-in or an extension's): `key = "body"` or `key = { body = "…",
//! description = "…" }`, bodies in snippet syntax with `$EXPR` for the
//! expression. After a `.`, the completion menu lists them; accepting one
//! replaces `expr.key` with its body (`kuanaPostfix` on the item says where
//! the expression starts).

use serde_json::json;
use toml::{Table, Value};

use crate::{lsp::protocol::CompletionItem, syntax::Language};

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub key: String,
    pub body: String,
    pub description: Option<String>,
}

/// A config's `[postfix]` table, in key order.
pub fn parse(config: &Table) -> Vec<Template> {
    let Some(table) = config.get("postfix").and_then(Value::as_table) else { return Vec::new() };
    table
        .iter()
        .filter_map(|(key, value)| {
            let (body, description) = match value {
                Value::String(body) => (body.clone(), None),
                Value::Table(t) => (
                    t.get("body")?.as_str()?.to_string(),
                    t.get("description").and_then(Value::as_str).map(str::to_string),
                ),
                _ => return None,
            };
            Some(Template { key: key.clone(), body, description })
        })
        .collect()
}

/// The templates of `language`: its built-in ones, else its extension's.
pub fn templates(language: Language) -> Vec<Template> {
    if let Some(def) = language.extension() {
        return def.postfix.clone();
    }
    crate::languages::def(language).map(|d| d.postfix.clone()).unwrap_or_default()
}

/// Where the expression a postfix template would wrap starts in `before`,
/// the line's text up to the `.`: a chain of names, calls, indexes and
/// literals (`self.items[0].len()`, `"text"`, `foo()?`), joined by `.`,
/// `::`, `->` or `?.`. None when there's none (a number, an operator).
pub fn expression_start(before: &str) -> Option<usize> {
    let b = before.as_bytes();
    let mut p = b.len();
    loop {
        let q = operand_start(b, p)?;
        p = q;
        // A connector with another operand before it.
        let joined = [&b"?."[..], b"::", b"->", b"."].into_iter().find_map(|c| {
            let at = p.checked_sub(c.len())?;
            (&b[at..p] == c).then_some(at)
        });
        match joined.and_then(|at| operand_start(b, at).map(|_| at)) {
            Some(at) => p = at,
            None => break,
        }
    }
    let expr = &before[p..];
    (!expr.bytes().all(|c| c.is_ascii_digit() || c == b'_')).then_some(p)
}

/// The start of one operand ending at `end`: bracket groups, `?` and `!`
/// after a name or another group, then a name, a string or a group.
fn operand_start(b: &[u8], end: usize) -> Option<usize> {
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    let mut p = end;
    let mut found = false;
    while p > 0 {
        match b[p - 1] {
            b')' | b']' | b'}' => {
                p = matching_open(b, p - 1)?;
                found = true;
                // `vec![…]`, `println!(…)`: a macro's name goes with it.
                if p >= 2 && b[p - 1] == b'!' && is_word(b[p - 2]) {
                    p -= 1;
                }
            }
            b'?' if p >= 2 && (is_word(b[p - 2]) || matches!(b[p - 2], b')' | b']')) => p -= 1,
            q @ (b'"' | b'\'' | b'`') => {
                let open = b[..p - 1].iter().rposition(|&c| c == q)?;
                p = open;
                // A prefix: `f"…"`, `r"…"`, `b'…'`.
                while p > 0 && is_word(b[p - 1]) {
                    p -= 1;
                }
                return Some(p);
            }
            c if is_word(c) => {
                while p > 0 && is_word(b[p - 1]) {
                    p -= 1;
                }
                return Some(p);
            }
            _ => break,
        }
    }
    found.then_some(p)
}

/// Where the bracket closed at `close` opens, counting nested ones.
fn matching_open(b: &[u8], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for i in (0..=close).rev() {
        match b[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The templates as completion items for the expression `expr`, which
/// starts at `start` in the document.
pub fn items(templates: &[Template], start: usize, expr: &str) -> Vec<CompletionItem> {
    templates
        .iter()
        .filter_map(|t| {
            CompletionItem::from_json(json!({
                "label": t.key,
                "kind": 15,
                "detail": t.description.clone().unwrap_or_else(|| "postfix".into()),
                "insertText": t.body,
                "insertTextFormat": 2,
                // After what the server lists for the same letters.
                "sortText": format!("~{}", t.key),
                "kuanaPostfix": { "start": start, "expr": expr },
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(before: &str) -> Option<&str> {
        expression_start(before).map(|i| &before[i..])
    }

    #[test]
    fn finds_the_expression_before_the_dot() {
        assert_eq!(expr("    let n = items"), Some("items"));
        assert_eq!(expr("x = self.items[0].len()"), Some("self.items[0].len()"));
        assert_eq!(expr("if foo(a, b(c))"), Some("foo(a, b(c))"));
        assert_eq!(expr("return Vec::new()"), Some("Vec::new()"));
        assert_eq!(expr("let v = read()?"), Some("read()?"));
        assert_eq!(expr("print(f\"{x}\""), Some("f\"{x}\""));
        assert_eq!(expr("vec![1, 2]"), Some("vec![1, 2]"));
        assert_eq!(expr("a?.b"), Some("a?.b"));
        assert_eq!(expr("$this->items"), Some("$this->items"));
        assert_eq!(expr("a + (b * c)"), Some("(b * c)"));
        assert_eq!(expr("x = 1"), None);
        assert_eq!(expr("foo("), None);
        assert_eq!(expr(""), None);
        assert_eq!(expr("a + "), None);
    }

    #[test]
    fn reads_the_postfix_table() {
        let config: Table = toml::from_str("[postfix]\nnot = \"!$EXPR\"\nif = { body = \"if $EXPR {\\n\\t$0\\n}\", description = \"if expr\" }\n").unwrap();
        let templates = parse(&config);
        assert_eq!(templates.len(), 2);
        let if_ = templates.iter().find(|t| t.key == "if").unwrap();
        assert_eq!(if_.description.as_deref(), Some("if expr"));
        assert!(if_.body.starts_with("if $EXPR {"));
        let items = items(&templates, 7, "x");
        assert_eq!(items[0].raw["kuanaPostfix"]["start"], 7);
    }

    #[test]
    fn built_in_languages_have_templates() {
        for language in [Language::Rust, Language::Python, Language::JavaScript, Language::TypeScript, Language::Tsx, Language::Go] {
            let t = templates(language);
            assert!(t.iter().any(|t| t.key == "if"), "{language:?}");
            assert!(t.iter().all(|t| t.body.contains("$EXPR") || t.body.contains("${EXPR")), "{language:?}");
        }
    }
}
