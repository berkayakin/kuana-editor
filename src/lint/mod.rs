//! Linting runs off the main thread on a snapshot of the buffer text.

pub mod external;
mod html;
mod js;
mod markdown;
mod syntax_errors;

use std::{borrow::Cow, ops::Range, sync::Arc};

use crate::syntax::Language;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    /// Underlined in the text, but not a problem (see `is_problem`).
    Info,
    /// A language server's suggestion (LSP's Hint): drawn faintly in the
    /// text, but not a problem.
    Hint,
}

impl Severity {
    /// Counted in the status bar, listed in Problems, marked in the gutter
    /// and stepped through by Next Problem: errors and warnings, as in Zed.
    /// Information and hints show where they are (underlined, on hover).
    pub fn is_problem(self) -> bool {
        matches!(self, Severity::Error | Severity::Warning)
    }
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// Byte range in the document.
    pub range: Range<usize>,
    pub severity: Severity,
    pub message: Arc<str>,
    /// Rule name, e.g. `no-unused-vars`.
    pub rule: Cow<'static, str>,
    /// Code a language server tagged as unused (LSP's `Unnecessary`): faded,
    /// and underlined only when it's a problem, as in Zed.
    pub unnecessary: bool,
}

/// Whether `language`'s built-in lint is only its syntax errors, which
/// come from the document's syntax tree (`syntax_errors`), not from `lint`.
pub fn syntax_only(language: Language) -> bool {
    matches!(
        language,
        Language::Css
            | Language::Rust
            | Language::Go
            | Language::Python
            | Language::Php
            | Language::Shell
            | Language::Json
            | Language::Yaml
            | Language::Toml
            | Language::Extension(_)
    )
}

/// The syntax errors in `tree`, the document's for `rope` (see `syntax_only`).
pub fn syntax_errors(tree: &tree_sitter::Tree, rope: &ropey::Rope) -> Vec<Diagnostic> {
    let mut diagnostics = syntax_errors::lint(tree, rope);
    let next_char = |at: usize| rope.get_char(rope.byte_to_char(at));
    finish(&mut diagnostics, rope.len_bytes(), next_char);
    diagnostics
}

/// Runs the built-in linter of languages with rules of their own (the
/// others: `syntax_errors`). With `syntax_only`, style/correctness rules are
/// skipped because an external linter with project config takes over.
/// `path` locates project config (e.g. `.markdownlint.json`) and relative links.
pub fn lint(language: Language, text: &str, path: Option<&std::path::Path>, syntax_only: bool) -> Vec<Diagnostic> {
    let mut diagnostics = match language {
        Language::JavaScript => js::lint(text, 0, js::Context::File, js::Dialect::JavaScript),
        Language::TypeScript | Language::Tsx => {
            let declaration = path
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|n| [".d.ts", ".d.mts", ".d.cts"].iter().any(|ext| n.ends_with(ext)));
            let dialect = match language {
                _ if declaration => js::Dialect::Declaration,
                Language::Tsx => js::Dialect::Tsx,
                _ => js::Dialect::TypeScript,
            };
            js::lint(text, 0, js::Context::File, dialect)
        }
        Language::Html => html::lint(text),
        Language::Markdown => markdown::lint(text, path),
        _ => Vec::new(),
    };
    if syntax_only {
        diagnostics.retain(|d| d.rule == "syntax");
    }
    finish(&mut diagnostics, text.len(), |at| text[at..].chars().next());
    diagnostics
}

/// Clamps diagnostics to the text (`len` bytes, `next_char` the character
/// at an offset), widens empty ones to cover a character, and sorts them.
fn finish(diagnostics: &mut [Diagnostic], len: usize, next_char: impl Fn(usize) -> Option<char>) {
    for d in diagnostics.iter_mut() {
        d.range.start = d.range.start.min(len);
        d.range.end = d.range.end.clamp(d.range.start, len);
        // Make zero-width diagnostics visible by covering at least one char.
        if d.range.is_empty() {
            d.range.end += next_char(d.range.start).filter(|c| *c != '\n').map_or(0, char::len_utf8);
        }
    }
    diagnostics.sort_by_key(|d| (d.range.start, d.severity));
}
