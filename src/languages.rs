//! Kuana's built-in languages, as data: `assets/languages/<dir>/` holds each
//! one's `config.toml` (the keys extensions use: `name`, `path_suffixes`,
//! comments, `tab_size`, `hard_tabs`; Kuana's `soft_wrap` and `[queries]`) and
//! its query files. They compile the way extensions' do (`LangConfig::build`,
//! `OutlineConfig::build`, `LanguageQueries::build`), so built-ins get
//! injections, brackets, indents, text objects and runnables the same way.
//! Only the grammars are compiled in (the tree-sitter crates); `[queries]`
//! lists, per kind, the queries those crates ship (`crate:rust/highlights`)
//! and this folder's files, joined in order.

use std::{
    path::Path,
    sync::{LazyLock, OnceLock},
};

use toml::{Table, Value};
use tree_sitter::Language as TsLanguage;

use crate::syntax::{EditingRules, LangConfig, Language, LanguageQueries, OutlineConfig};

/// The files under `assets/languages`, by path.
const FILES: &[(&str, &str)] = &[
    ("css/brackets.scm", include_str!("../assets/languages/css/brackets.scm")),
    ("css/config.toml", include_str!("../assets/languages/css/config.toml")),
    ("css/indents.scm", include_str!("../assets/languages/css/indents.scm")),
    ("css/outline.scm", include_str!("../assets/languages/css/outline.scm")),
    ("go/brackets.scm", include_str!("../assets/languages/go/brackets.scm")),
    ("go/config.toml", include_str!("../assets/languages/go/config.toml")),
    ("go/highlights.scm", include_str!("../assets/languages/go/highlights.scm")),
    ("go/indents.scm", include_str!("../assets/languages/go/indents.scm")),
    ("go/textobjects.scm", include_str!("../assets/languages/go/textobjects.scm")),
    ("html/brackets.scm", include_str!("../assets/languages/html/brackets.scm")),
    ("html/config.toml", include_str!("../assets/languages/html/config.toml")),
    ("html/highlights.scm", include_str!("../assets/languages/html/highlights.scm")),
    ("javascript/brackets.scm", include_str!("../assets/languages/javascript/brackets.scm")),
    ("javascript/config.toml", include_str!("../assets/languages/javascript/config.toml")),
    ("javascript/highlights.scm", include_str!("../assets/languages/javascript/highlights.scm")),
    ("javascript/indents.scm", include_str!("../assets/languages/javascript/indents.scm")),
    ("javascript/textobjects.scm", include_str!("../assets/languages/javascript/textobjects.scm")),
    ("json/brackets.scm", include_str!("../assets/languages/json/brackets.scm")),
    ("json/config.toml", include_str!("../assets/languages/json/config.toml")),
    ("json/highlights.scm", include_str!("../assets/languages/json/highlights.scm")),
    ("json/indents.scm", include_str!("../assets/languages/json/indents.scm")),
    ("json/outline.scm", include_str!("../assets/languages/json/outline.scm")),
    ("markdown/config.toml", include_str!("../assets/languages/markdown/config.toml")),
    ("markdown/injections.scm", include_str!("../assets/languages/markdown/injections.scm")),
    ("markdown/outline.scm", include_str!("../assets/languages/markdown/outline.scm")),
    ("php/brackets.scm", include_str!("../assets/languages/php/brackets.scm")),
    ("php/config.toml", include_str!("../assets/languages/php/config.toml")),
    ("php/indents.scm", include_str!("../assets/languages/php/indents.scm")),
    ("php/injections.scm", include_str!("../assets/languages/php/injections.scm")),
    ("php/textobjects.scm", include_str!("../assets/languages/php/textobjects.scm")),
    ("python/brackets.scm", include_str!("../assets/languages/python/brackets.scm")),
    ("python/config.toml", include_str!("../assets/languages/python/config.toml")),
    ("python/indents.scm", include_str!("../assets/languages/python/indents.scm")),
    ("python/textobjects.scm", include_str!("../assets/languages/python/textobjects.scm")),
    ("rust/brackets.scm", include_str!("../assets/languages/rust/brackets.scm")),
    ("rust/config.toml", include_str!("../assets/languages/rust/config.toml")),
    ("rust/highlights.scm", include_str!("../assets/languages/rust/highlights.scm")),
    ("rust/indents.scm", include_str!("../assets/languages/rust/indents.scm")),
    ("rust/outline.scm", include_str!("../assets/languages/rust/outline.scm")),
    ("rust/textobjects.scm", include_str!("../assets/languages/rust/textobjects.scm")),
    ("shell/brackets.scm", include_str!("../assets/languages/shell/brackets.scm")),
    ("shell/config.toml", include_str!("../assets/languages/shell/config.toml")),
    ("shell/indents.scm", include_str!("../assets/languages/shell/indents.scm")),
    ("shell/outline.scm", include_str!("../assets/languages/shell/outline.scm")),
    ("shell/textobjects.scm", include_str!("../assets/languages/shell/textobjects.scm")),
    ("toml/config.toml", include_str!("../assets/languages/toml/config.toml")),
    ("toml/outline.scm", include_str!("../assets/languages/toml/outline.scm")),
    ("tsx/brackets.scm", include_str!("../assets/languages/tsx/brackets.scm")),
    ("tsx/config.toml", include_str!("../assets/languages/tsx/config.toml")),
    ("tsx/highlights.scm", include_str!("../assets/languages/tsx/highlights.scm")),
    ("tsx/indents.scm", include_str!("../assets/languages/tsx/indents.scm")),
    ("tsx/outline.scm", include_str!("../assets/languages/tsx/outline.scm")),
    ("tsx/textobjects.scm", include_str!("../assets/languages/tsx/textobjects.scm")),
    ("typescript/brackets.scm", include_str!("../assets/languages/typescript/brackets.scm")),
    ("typescript/config.toml", include_str!("../assets/languages/typescript/config.toml")),
    ("typescript/highlights.scm", include_str!("../assets/languages/typescript/highlights.scm")),
    ("typescript/indents.scm", include_str!("../assets/languages/typescript/indents.scm")),
    ("typescript/outline.scm", include_str!("../assets/languages/typescript/outline.scm")),
    ("typescript/textobjects.scm", include_str!("../assets/languages/typescript/textobjects.scm")),
    ("yaml/config.toml", include_str!("../assets/languages/yaml/config.toml")),
    ("yaml/outline.scm", include_str!("../assets/languages/yaml/outline.scm")),
];

/// A built-in language's definition.
pub struct BuiltinDef {
    pub language: Language,
    pub name: &'static str,
    /// Its folder: "rust", "shell".
    pub dir: &'static str,
    /// File extensions and whole file names, compared without case.
    suffixes: Vec<String>,
    pub line_comment: Option<&'static str>,
    pub block_comment: Option<(&'static str, &'static str)>,
    pub tab_size: Option<usize>,
    pub hard_tabs: Option<bool>,
    pub soft_wrap: Option<bool>,
    /// Auto-closed pairs and outdenting lines (`brackets`, `decrease_indent_patterns`).
    pub editing: EditingRules,
    grammar: String,
    queries: Table,
    loaded: OnceLock<Loaded>,
}

struct Loaded {
    highlights: LangConfig,
    outline: Option<OutlineConfig>,
    queries: LanguageQueries,
}

/// Each built-in language's folder.
fn dir(language: Language) -> Option<&'static str> {
    Some(match language {
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Tsx => "tsx",
        Language::Html => "html",
        Language::Css => "css",
        Language::Rust => "rust",
        Language::Go => "go",
        Language::Python => "python",
        Language::Php => "php",
        Language::Shell => "shell",
        Language::Json => "json",
        Language::Yaml => "yaml",
        Language::Toml => "toml",
        Language::Markdown => "markdown",
        Language::PlainText | Language::Extension(_) => return None,
    })
}

static BUILTINS: LazyLock<Vec<BuiltinDef>> =
    LazyLock::new(|| Language::ALL.into_iter().filter_map(|l| Some(read(l, dir(l)?))).collect());

/// The built-in definition of `language`.
pub fn def(language: Language) -> Option<&'static BuiltinDef> {
    BUILTINS.iter().find(|d| d.language == language)
}

/// The built-in language with this name ("rust" finds Rust).
pub fn by_name(name: &str) -> Option<&'static BuiltinDef> {
    BUILTINS.iter().find(|d| d.name.eq_ignore_ascii_case(name))
}

/// The built-in language a file name or extension says `path` is.
pub fn for_path(path: &Path) -> Option<Language> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    BUILTINS.iter().find(|d| d.suffixes.iter().any(|s| name == *s || name.strip_suffix(s.as_str()).is_some_and(|rest| rest.ends_with('.')))).map(|d| d.language)
}

fn file(path: &str) -> Option<&'static str> {
    FILES.iter().find(|(p, _)| *p == path).map(|(_, text)| *text)
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn read(language: Language, dir: &'static str) -> BuiltinDef {
    let config: Table = toml::from_str(file(&format!("{dir}/config.toml")).unwrap_or_default())
        .unwrap_or_else(|e| panic!("assets/languages/{dir}/config.toml: {e}"));
    let text = |key: &str| config.get(key).and_then(Value::as_str).map(str::to_string);
    let (line_comment, block_comment) = crate::extensions::comment_tokens(&config);
    BuiltinDef {
        language,
        name: leak(text("name").unwrap_or_else(|| dir.to_string())),
        dir,
        suffixes: config.get("path_suffixes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_lowercase).collect(),
        line_comment: line_comment.map(leak),
        block_comment: block_comment.map(|(start, end)| (leak(start), leak(end))),
        tab_size: config.get("tab_size").and_then(Value::as_integer).map(|n| n.clamp(1, 16) as usize),
        hard_tabs: config.get("hard_tabs").and_then(Value::as_bool),
        soft_wrap: config.get("soft_wrap").and_then(Value::as_bool),
        editing: EditingRules::from_config(&config),
        grammar: text("grammar").unwrap_or_else(|| dir.to_string()),
        queries: config.get("queries").and_then(Value::as_table).cloned().unwrap_or_default(),
        loaded: OnceLock::new(),
    }
}

impl BuiltinDef {
    pub fn grammar(&self) -> TsLanguage {
        native_grammar(&self.grammar).unwrap_or_else(|| panic!("no grammar named {}", self.grammar))
    }

    pub(crate) fn highlight_config(&'static self) -> &'static LangConfig {
        &self.load().highlights
    }

    pub(crate) fn outline_config(&'static self) -> Option<&'static OutlineConfig> {
        self.load().outline.as_ref()
    }

    pub(crate) fn queries(&'static self) -> &'static LanguageQueries {
        &self.load().queries
    }

    /// The text of one kind of query ("highlights", "injections"…): what
    /// `[queries]` lists for it, else the folder's `<kind>.scm`.
    pub(crate) fn source(&self, kind: &str) -> Option<String> {
        let own = |name: &str| file(&format!("{}/{name}", self.dir)).map(str::to_string);
        match self.queries.get(kind).and_then(Value::as_array) {
            Some(sources) => {
                let parts: Vec<String> = sources
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|source| match source.strip_prefix("crate:") {
                        Some(name) => crate_query(name).map(str::to_string).unwrap_or_else(|| panic!("{}: no crate query {name}", self.dir)),
                        None => own(source).unwrap_or_else(|| panic!("{}: no file {source}", self.dir)),
                    })
                    .collect();
                Some(parts.join("\n"))
            }
            None => own(&format!("{kind}.scm")),
        }
    }

    fn load(&self) -> &Loaded {
        self.loaded.get_or_init(|| {
            let grammar = self.grammar();
            // Built-in queries are Kuana's own, checked by tests: a mistake is a bug.
            let highlights = LangConfig::build(grammar.clone(), &self.source("highlights").unwrap_or_default())
                .unwrap_or_else(|e| panic!("{}: {e}", self.name));
            let outline = self.source("outline").and_then(|source| OutlineConfig::build(grammar.clone(), &source).unwrap_or_else(|e| panic!("{}: {e}", self.name)));
            let mut problems = Vec::new();
            let queries = LanguageQueries::build(&grammar, |file| self.source(file.strip_suffix(".scm").unwrap_or(file)), &mut problems);
            assert!(problems.is_empty(), "{}: {problems:?}", self.name);
            Loaded { highlights, outline, queries }
        })
    }
}

/// The grammars compiled into Kuana.
pub fn native_grammar(name: &str) -> Option<TsLanguage> {
    Some(match name {
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "html" => tree_sitter_html::LANGUAGE.into(),
        "css" => tree_sitter_css::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        // The PHP grammar that also accepts HTML around `<?php` tags.
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        "json" => tree_sitter_json::LANGUAGE.into(),
        "yaml" => tree_sitter_yaml::LANGUAGE.into(),
        "toml" => tree_sitter_toml_ng::LANGUAGE.into(),
        // Block structure only; inline syntax is a second grammar, injected.
        "markdown" => tree_sitter_md::LANGUAGE.into(),
        "markdown_inline" => tree_sitter_md::INLINE_LANGUAGE.into(),
        _ => return None,
    })
}

/// The queries the grammar crates ship, by `<grammar>/<kind>`.
fn crate_query(name: &str) -> Option<&'static str> {
    Some(match name {
        "javascript/highlights" => tree_sitter_javascript::HIGHLIGHT_QUERY,
        "javascript/jsx" => tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        "javascript/injections" => tree_sitter_javascript::INJECTIONS_QUERY,
        "javascript/tags" => tree_sitter_javascript::TAGS_QUERY,
        "typescript/highlights" => tree_sitter_typescript::HIGHLIGHTS_QUERY,
        "typescript/tags" => tree_sitter_typescript::TAGS_QUERY,
        "html/highlights" => tree_sitter_html::HIGHLIGHTS_QUERY,
        "html/injections" => tree_sitter_html::INJECTIONS_QUERY,
        "css/highlights" => tree_sitter_css::HIGHLIGHTS_QUERY,
        "rust/highlights" => tree_sitter_rust::HIGHLIGHTS_QUERY,
        "rust/injections" => tree_sitter_rust::INJECTIONS_QUERY,
        "rust/tags" => tree_sitter_rust::TAGS_QUERY,
        "go/highlights" => tree_sitter_go::HIGHLIGHTS_QUERY,
        "go/tags" => tree_sitter_go::TAGS_QUERY,
        "python/highlights" => tree_sitter_python::HIGHLIGHTS_QUERY,
        "python/tags" => tree_sitter_python::TAGS_QUERY,
        "php/highlights" => tree_sitter_php::HIGHLIGHTS_QUERY,
        "php/injections" => tree_sitter_php::INJECTIONS_QUERY,
        "php/tags" => tree_sitter_php::TAGS_QUERY,
        "bash/highlights" => tree_sitter_bash::HIGHLIGHT_QUERY,
        "json/highlights" => tree_sitter_json::HIGHLIGHTS_QUERY,
        "yaml/highlights" => tree_sitter_yaml::HIGHLIGHTS_QUERY,
        "toml/highlights" => tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        "markdown/highlights" => tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        "markdown/injections" => tree_sitter_md::INJECTION_QUERY_BLOCK,
        "markdown_inline/highlights" => tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_language_loads_from_its_files() {
        for language in Language::ALL.into_iter().filter(|l| *l != Language::PlainText) {
            let def = def(language).unwrap_or_else(|| panic!("{language:?} has no folder"));
            assert_eq!(def.name, language.name());
            // Compiles every query (a mistake panics).
            def.highlight_config();
            def.queries();
        }
    }

    #[test]
    fn files_get_their_language_comments_and_defaults() {
        let of = |name: &str| Language::from_path(Path::new(name));
        assert_eq!(of("src/main.rs"), Language::Rust);
        assert_eq!(of("App.JSX"), Language::JavaScript);
        assert_eq!(of("/home/me/.zshrc"), Language::Shell);
        assert_eq!(of("Cargo.lock"), Language::Toml);
        assert_eq!(of("notes.markdown"), Language::Markdown);
        assert_eq!(of("main.trs"), Language::PlainText);
        assert_eq!(of("README"), Language::PlainText);

        assert_eq!(Language::Json.line_comment(), Some("// "));
        assert_eq!(Language::Html.block_comment(), Some(("<!-- ", " -->")));
        assert_eq!(Language::Python.block_comment(), None);
        assert_eq!(def(Language::Go).and_then(|d| d.hard_tabs), Some(true));
        assert_eq!(def(Language::Yaml).and_then(|d| d.tab_size), Some(2));
        assert_eq!(def(Language::Markdown).and_then(|d| d.soft_wrap), Some(true));
    }

    #[test]
    fn the_file_list_matches_the_folder() {
        let mut on_disk: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/languages")).unwrap().flatten() {
            for file in std::fs::read_dir(entry.path()).unwrap().flatten() {
                on_disk.push(format!("{}/{}", entry.file_name().to_string_lossy(), file.file_name().to_string_lossy()));
            }
        }
        on_disk.sort();
        let listed: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(listed, on_disk, "add new files under assets/languages to FILES");
    }
}
