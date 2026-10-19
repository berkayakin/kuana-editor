//! Syntax errors for any language with a tree-sitter grammar: reports the
//! `ERROR` and `MISSING` nodes the parser produced while recovering.

use ropey::Rope;
use tree_sitter::{Node, Tree};

use super::{Diagnostic, Severity};

/// Broken files can produce an error at almost every line; past this many
/// the rest are noise.
const MAX_ERRORS: usize = 100;

/// The errors in `tree`, the syntax tree of `text`.
pub fn lint(tree: &Tree, text: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        // Subtrees without errors are skipped. Recovery can wrap large valid
        // regions (even the whole file) in an error node; when it holds more
        // specific errors, report those instead of the wrapper.
        let descend = if node.is_error() {
            let nested = (0..node.child_count()).any(|i| node.child(i as u32).is_some_and(|c| c.has_error()));
            if !nested {
                out.push(unexpected(node, text));
            }
            nested
        } else if node.is_missing() {
            out.push(missing(node));
            false
        } else {
            node.has_error()
        };
        if out.len() >= MAX_ERRORS {
            break;
        }
        if descend && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
    out
}

fn unexpected(node: Node, text: &Rope) -> Diagnostic {
    let range = node.byte_range();
    // An error node can swallow many lines; underline only its first one.
    let line = text.byte_to_line(range.start.min(text.len_bytes()));
    let line_end = if line + 1 < text.len_lines() { text.line_to_byte(line + 1) - 1 } else { text.len_bytes() };
    // Name the token only when it is the whole error; in a larger error node
    // the first token is usually valid code the parser gave up on.
    let token = first_token(node)
        .filter(|t| t.byte_range() == range && range.len() <= 200)
        .and_then(|t| text.get_byte_slice(t.byte_range()))
        .map(|t| t.to_string().trim().to_string())
        .filter(|t| !t.is_empty() && t.len() <= 40 && !t.contains('\n'));
    let message = match token {
        Some(token) => format!("Syntax error: unexpected `{token}`"),
        None => "Syntax error".to_string(),
    };
    Diagnostic {
        range: range.start..range.end.min(line_end).max(range.start),
        severity: Severity::Error,
        message: message.into(),
        rule: "syntax".into(),
        unnecessary: false,
    }
}

fn missing(node: Node) -> Diagnostic {
    let kind = node.kind();
    let message = if node.is_named() {
        format!("Syntax error: missing {}", kind.replace('_', " "))
    } else {
        format!("Syntax error: missing `{kind}`")
    };
    Diagnostic {
        range: node.byte_range(),
        severity: Severity::Error,
        message: message.into(),
        rule: "syntax".into(),
        unnecessary: false,
    }
}

fn first_token(mut node: Node) -> Option<Node> {
    while node.child_count() > 0 {
        node = node.child(0)?;
    }
    Some(node)
}

#[cfg(test)]
mod tests {
    use crate::syntax::{Language, grammar};

    fn errors(language: Language, src: &str) -> Vec<(String, String)> {
        let tree = crate::syntax::with_parser(&grammar(language).unwrap(), |p| p.parse(src, None)).flatten().unwrap();
        super::lint(&tree, &ropey::Rope::from_str(src)).into_iter().map(|d| (src[d.range].to_string(), d.message.to_string())).collect()
    }

    #[test]
    fn valid_code_has_no_errors() {
        assert!(errors(Language::Rust, "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n").is_empty());
        assert!(errors(Language::Go, "package main\n\nfunc main() {\n\tx := 1\n\t_ = x\n}\n").is_empty());
        assert!(errors(Language::Python, "def f(x):\n    return x + 1\n").is_empty());
        assert!(errors(Language::Php, "<p><?php echo $x; ?></p>\n").is_empty());
        assert!(errors(Language::Css, "@media screen {\n  .a > b:hover { color: #fff; }\n}\n").is_empty());
        assert!(errors(Language::Shell, "#!/bin/sh\nif [ -n \"$1\" ]; then\n  echo \"$1\"\nfi\n").is_empty());
        assert!(errors(Language::Json, "{\n  // comment\n  \"a\": [1, true, null]\n}\n").is_empty());
        assert!(errors(Language::Yaml, "name: ci\non:\n  push:\n    branches: [main]\nsteps:\n  - run: echo hi\n").is_empty());
        assert!(errors(Language::Toml, "[package]\nname = \"kuana\"\n\n[dependencies]\nserde = { version = \"1\" }\n").is_empty());
    }

    #[test]
    fn reports_syntax_errors() {
        for (language, src) in [
            (Language::Rust, "fn main() {\n    let x = ;\n}\n"),
            (Language::Rust, "fn main() {\n    let x = 1\n}\n"),
            (Language::Go, "package main\n\nfunc main() {\n\tx := \n}\n"),
            (Language::Python, "def f(x)\n    return x\n"),
            (Language::Php, "<?php\n$x = ;\n"),
            (Language::Css, ".a { color: red;\n"),
            (Language::Shell, "if true; then\n  echo hi\n"),
            (Language::Json, "{\"a\": 1,, \"b\": 2}\n"),
            (Language::Yaml, "a: [1, 2\nb: 3\n"),
            (Language::Toml, "[package\nname = \"kuana\"\n"),
        ] {
            let found = errors(language, src);
            eprintln!("{language:?}: {found:?}");
            assert!(!found.is_empty(), "{language:?} missed an error in {src:?}");
        }
    }

    #[test]
    fn reports_inner_error_not_file_wide_wrapper() {
        // Python recovery wraps the whole module in an error node here.
        let src = "import os\n\ndef f():\n    return 1\n\ndef main()\n    print(1)\n";
        assert_eq!(errors(Language::Python, src), [("def main()".to_string(), "Syntax error".to_string())]);
    }
}
