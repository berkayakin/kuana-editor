//! Tree-sitter powered syntax highlighting.
//!
//! Trees are re-parsed incrementally on every edit; highlight queries only run
//! over the byte range that is actually on screen, so cost is proportional to
//! the viewport rather than the file.

use std::{
    collections::HashMap,
    ops::{ControlFlow, Deref, DerefMut, Range},
    path::Path,
    sync::{LazyLock, Mutex, RwLock},
    time::{Duration, Instant},
};

use ropey::Rope;
use tree_sitter::{
    InputEdit, Language as TsLanguage, Node, ParseOptions, Parser, Query, QueryCursor, StreamingIterator,
    TextProvider, Tree,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    JavaScript,
    TypeScript,
    Tsx,
    Html,
    Css,
    Rust,
    Go,
    Python,
    Php,
    Shell,
    Json,
    Yaml,
    Toml,
    Markdown,
    PlainText,
    /// A language an extension adds: an index into `crate::extensions`'
    /// registry, stable while Kuana runs.
    Extension(u16),
}

impl Language {
    /// Every language, in the order Preferences lists them.
    pub const ALL: [Language; 15] = [
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Html,
        Language::Css,
        Language::Rust,
        Language::Go,
        Language::Python,
        Language::Php,
        Language::Shell,
        Language::Json,
        Language::Yaml,
        Language::Toml,
        Language::Markdown,
        Language::PlainText,
    ];

    /// The built-in languages, then the extensions', then plain text.
    pub fn all() -> Vec<Language> {
        let mut all: Vec<Language> = Language::ALL.into_iter().filter(|l| *l != Language::PlainText).collect();
        all.extend(crate::extensions::languages().into_iter().map(Language::Extension));
        all.push(Language::PlainText);
        all
    }

    /// An extension's language naming this very file, else the built-in
    /// language for `path`, else an extension's by extension.
    pub fn from_path(path: &Path) -> Self {
        if let Some(ix) = crate::extensions::language_for_file_name(path) {
            return Language::Extension(ix);
        }
        match crate::languages::for_path(path) {
            Some(language) => language,
            None => crate::extensions::language_for_path(path).map_or(Language::PlainText, Language::Extension),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::PlainText => "Plain Text",
            Language::Extension(ix) => crate::extensions::language(ix).map_or("Plain Text", |l| l.name),
            _ => crate::languages::def(self).map_or("Plain Text", |d| d.name),
        }
    }

    /// The extension's definition, for an extension language.
    pub fn extension(self) -> Option<&'static crate::extensions::LanguageDef> {
        match self {
            Language::Extension(ix) => crate::extensions::language(ix),
            _ => None,
        }
    }

    pub fn is_lintable(self) -> bool {
        self != Language::PlainText
    }

    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::Extension(_) => self.extension().and_then(|l| l.line_comment),
            _ => crate::languages::def(self).and_then(|d| d.line_comment),
        }
    }

    pub fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Language::Extension(_) => self.extension().and_then(|l| l.block_comment),
            _ => crate::languages::def(self).and_then(|d| d.block_comment),
        }
    }

    /// What typing does here: the pairs it closes, the lines it outdents.
    pub fn editing(self) -> &'static EditingRules {
        static DEFAULT: LazyLock<EditingRules> = LazyLock::new(EditingRules::default);
        let rules = match self {
            Language::Extension(_) => self.extension().map(|l| &l.editing),
            _ => crate::languages::def(self).map(|d| &d.editing),
        };
        rules.unwrap_or(&DEFAULT)
    }
}

/// A pair typing the opening character closes, from a language's
/// `brackets` (Zed's format; only one-character ones with `close`).
#[derive(Clone, Debug, PartialEq)]
pub struct AutoPair {
    pub open: char,
    pub close: char,
    /// Where it isn't closed: `"string"`, `"comment"` (see `Syntax::scope_at`).
    pub not_in: Vec<String>,
}

/// A line that goes back a level once it reads like this (`else:`), Zed's
/// `decrease_indent_patterns`: level with the nearest less indented line
/// above, or the nearest one starting with a word of `valid_after`.
#[derive(Clone, Debug)]
pub struct Decrease {
    pub pattern: regex::Regex,
    pub valid_after: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct EditingRules {
    pub pairs: Vec<AutoPair>,
    pub decrease: Vec<Decrease>,
}

impl Default for EditingRules {
    /// Brackets and quotes, closed outside strings and comments.
    fn default() -> Self {
        let pairs = [('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\''), ('`', '`')]
            .into_iter()
            .map(|(open, close)| AutoPair { open, close, not_in: vec!["string".into(), "comment".into()] })
            .collect();
        EditingRules { pairs, decrease: Vec::new() }
    }
}

impl EditingRules {
    /// From a `config.toml`: `brackets` (else the default pairs) and
    /// `decrease_indent_pattern` / `decrease_indent_patterns`.
    pub(crate) fn from_config(config: &toml::Table) -> Self {
        use toml::Value;
        let single = |v: Option<&Value>| {
            let mut chars = v.and_then(Value::as_str)?.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        let words = |v: Option<&Value>| -> Vec<String> {
            v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
        };
        let pairs = match config.get("brackets").and_then(Value::as_array) {
            Some(brackets) => brackets
                .iter()
                .filter_map(Value::as_table)
                .filter(|b| b.get("close").and_then(Value::as_bool) != Some(false))
                .filter_map(|b| Some(AutoPair { open: single(b.get("start"))?, close: single(b.get("end"))?, not_in: words(b.get("not_in")) }))
                .collect(),
            None => Self::default().pairs,
        };
        let pattern = |p: &str| regex::Regex::new(p).ok();
        let mut decrease: Vec<Decrease> =
            config.get("decrease_indent_pattern").and_then(Value::as_str).and_then(pattern).map(|pattern| Decrease { pattern, valid_after: Vec::new() }).into_iter().collect();
        for entry in config.get("decrease_indent_patterns").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_table) {
            if let Some(pattern) = entry.get("pattern").and_then(Value::as_str).and_then(pattern) {
                decrease.push(Decrease { pattern, valid_after: words(entry.get("valid_after")) });
            }
        }
        EditingRules { pairs, decrease }
    }

    /// The pair `c` opens.
    pub fn pair_opened_by(&self, c: char) -> Option<&AutoPair> {
        self.pairs.iter().find(|p| p.open == c)
    }
}

/// The tree-sitter grammar for `language`, used for highlighting and for
/// syntax-error linting.
pub fn grammar(language: Language) -> Option<TsLanguage> {
    match language {
        Language::Extension(_) => language.extension()?.grammar(),
        _ => Some(crate::languages::def(language)?.grammar()),
    }
}

/// What a highlight query captured: the capture's name ("keyword.control",
/// "function.method"), interned. A theme colors a name by the longest dotted
/// prefix it defines (`Theme::highlight_color`), else by its `base`: the
/// few kinds every theme has a color for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Highlight(u16);

/// The base kinds' capture names, in the order of their `Highlight` ids.
const BASE: [&str; 17] = [
    "", "keyword", "string", "number", "constant", "function", "function.method", "property", "type", "variable",
    "variable.builtin", "comment", "operator", "punctuation", "tag", "attribute", "escape",
];

struct Names {
    /// Per id: the name and its base kind.
    list: Vec<(&'static str, Highlight)>,
    ids: HashMap<&'static str, u16>,
}

static NAMES: LazyLock<RwLock<Names>> = LazyLock::new(|| {
    let list: Vec<(&'static str, Highlight)> = BASE.iter().enumerate().map(|(i, n)| (*n, Highlight(i as u16))).collect();
    let ids = list.iter().enumerate().map(|(i, (n, _))| (*n, i as u16)).collect();
    RwLock::new(Names { list, ids })
});

// The base kinds, named like the variants they used to be.
#[allow(non_upper_case_globals)]
impl Highlight {
    pub const None: Highlight = Highlight(0);
    pub const Keyword: Highlight = Highlight(1);
    pub const String: Highlight = Highlight(2);
    pub const Number: Highlight = Highlight(3);
    pub const Constant: Highlight = Highlight(4);
    pub const Function: Highlight = Highlight(5);
    pub const Method: Highlight = Highlight(6);
    pub const Property: Highlight = Highlight(7);
    pub const Type: Highlight = Highlight(8);
    pub const Variable: Highlight = Highlight(9);
    pub const VariableBuiltin: Highlight = Highlight(10);
    pub const Comment: Highlight = Highlight(11);
    pub const Operator: Highlight = Highlight(12);
    pub const Punctuation: Highlight = Highlight(13);
    pub const Tag: Highlight = Highlight(14);
    pub const Attribute: Highlight = Highlight(15);
    pub const Escape: Highlight = Highlight(16);
}

impl Highlight {
    /// The id of a capture name (queries are compiled once, so names are few).
    pub fn from_capture(name: &str) -> Self {
        if let Some(&id) = NAMES.read().unwrap().ids.get(name) {
            return Highlight(id);
        }
        let mut names = NAMES.write().unwrap();
        if let Some(&id) = names.ids.get(name) {
            return Highlight(id);
        }
        let Ok(id) = u16::try_from(names.list.len()) else { return Highlight::None };
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        names.list.push((name, Self::base_of(name)));
        names.ids.insert(name, id);
        Highlight(id)
    }

    pub fn name(self) -> &'static str {
        NAMES.read().unwrap().list.get(self.0 as usize).map_or("", |(n, _)| n)
    }

    /// The base kind, for themes that don't color this name.
    pub fn base(self) -> Highlight {
        if (self.0 as usize) < BASE.len() {
            return self;
        }
        NAMES.read().unwrap().list.get(self.0 as usize).map_or(Highlight::None, |(_, b)| *b)
    }

    /// The base kind of the longest prefix of `name` that has one.
    fn base_of(name: &str) -> Highlight {
        let mut prefix = name;
        loop {
            if let Some(base) = Self::base_at(prefix) {
                return base;
            }
            match prefix.rfind('.') {
                Some(dot) => prefix = &prefix[..dot],
                None => return Highlight::None,
            }
        }
    }

    /// The base kind for exactly this name (one level of a dotted name).
    pub(crate) fn base_at(name: &str) -> Option<Highlight> {
        Some(match name {
            "function.method" => Highlight::Method,
            "variable.builtin" => Highlight::VariableBuiltin,
            "string.special" => Highlight::Escape,
            "string.special.key" => Highlight::Property,
            "tag.error" => Highlight::Tag,
            // Markdown (nvim-treesitter style "text.*" captures).
            "text.title" | "text.strong" => Highlight::Keyword,
            "text.literal" => Highlight::String,
            "text.uri" => Highlight::Property,
            "text.reference" => Highlight::Function,
            "text.emphasis" => Highlight::Type,
            // CSS at-rules capture under their own names.
            "keyword" | "charset" | "import" | "keyframes" | "media" | "namespace" | "supports" => Highlight::Keyword,
            "string" => Highlight::String,
            "number" => Highlight::Number,
            "constant" | "boolean" | "label" => Highlight::Constant,
            "function" => Highlight::Function,
            "property" => Highlight::Property,
            "constructor" | "type" => Highlight::Type,
            "variable" | "embedded" => Highlight::Variable,
            "comment" => Highlight::Comment,
            "operator" => Highlight::Operator,
            "punctuation" => Highlight::Punctuation,
            "tag" => Highlight::Tag,
            "attribute" => Highlight::Attribute,
            "escape" => Highlight::Escape,
            _ => return None,
        })
    }
}

pub(crate) struct LangConfig {
    language: TsLanguage,
    query: Query,
    /// Highlight per capture index, resolved once.
    captures: Vec<Highlight>,
}

impl LangConfig {
    #[cfg(test)]
    pub(crate) fn capture_names(&self) -> &[&str] {
        self.query.capture_names()
    }

    fn new(language: TsLanguage, source: &str) -> Self {
        Self::build(language, source).expect("invalid highlight query")
    }

    /// For an extension's query, which may not fit its grammar.
    pub(crate) fn build(language: TsLanguage, source: &str) -> Result<Self, String> {
        let query = Query::new(&language, source).map_err(|e| format!("highlights.scm, line {}: {}", e.row + 1, e.message))?;
        let captures = query
            .capture_names()
            .iter()
            .map(|n| Highlight::from_capture(n))
            .collect();
        Ok(Self {
            language,
            query,
            captures,
        })
    }
}

/// Markdown's inline syntax: a second grammar, injected into its blocks.
static MARKDOWN_INLINE: LazyLock<LangConfig> = LazyLock::new(|| {
    LangConfig::new(tree_sitter_md::INLINE_LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_INLINE)
});

// ---- Outline -------------------------------------------------------------------

/// A named definition in a file, for File Structure.
#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    /// What it is: "function", "class", "heading"…
    pub kind: &'static str,
    /// Start of the name, where navigation lands.
    pub offset: usize,
    /// Nesting level among the other symbols.
    pub depth: usize,
    /// The whole definition (header and body).
    pub range: Range<usize>,
}

/// Outline queries capture `@name` inside a `@definition.<kind>` node, the
/// convention of tree-sitter's tags queries, or inside an `@item` (Zed's
/// `outline.scm`, whose kind comes from the node).
pub(crate) struct OutlineConfig {
    query: Query,
    /// Per capture index: `Some(kind)` for definitions, `None` otherwise.
    kinds: Vec<Option<&'static str>>,
    name: u32,
    /// Zed's `@context`: the keywords around the name (`pub`, `fn`), which
    /// say what an `@item` is.
    context: Option<u32>,
}

impl OutlineConfig {
    /// None for a query that marks no items: Zed's outline queries for
    /// template languages (EEx, HEEx) mark only comments, `@annotation`.
    pub(crate) fn build(language: TsLanguage, source: &str) -> Result<Option<Self>, String> {
        let query = Query::new(&language, source).map_err(|e| format!("outline query, line {}: {}", e.row + 1, e.message))?;
        let kinds: Vec<Option<&'static str>> = query
            .capture_names()
            .iter()
            .map(|n| if *n == "item" { Some("symbol") } else { n.strip_prefix("definition.").map(symbol_kind) })
            .collect();
        let Some(name) = query.capture_index_for_name("name") else {
            return if kinds.iter().any(Option::is_some) { Err("the outline query has no @name".into()) } else { Ok(None) };
        };
        let context = query.capture_index_for_name("context");
        Ok(Some(Self { query, kinds, name, context }))
    }
}

fn symbol_kind(kind: &str) -> &'static str {
    match kind {
        "function" => "function",
        "method" => "method",
        "class" => "class",
        "interface" => "interface",
        "module" => "module",
        "macro" => "macro",
        "constant" => "constant",
        "type" => "type",
        "enum" => "enum",
        "implementation" => "impl",
        "heading" => "heading",
        "selector" => "selector",
        "keyframes" => "keyframes",
        "table" => "table",
        "key" => "key",
        _ => "symbol",
    }
}

/// Tags queries group structs, enums and type aliases as "class"; the node
/// says which one it is.
fn node_kind_label(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "struct_item" | "struct_type" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "union_item" => "union",
        "trait_item" | "trait_declaration" => "trait",
        "type_item" | "type_alias_declaration" | "type_spec" => "type",
        "interface_declaration" | "interface_type" => "interface",
        "const_item" | "static_item" => "constant",
        _ => return None,
    })
}

/// A kind for an outline `@item` from the keywords before its name.
fn context_kind(words: &[String]) -> Option<&'static str> {
    words.iter().find_map(|word| {
        Some(match word.as_str() {
            "fn" | "func" | "function" | "def" | "defp" | "fun" | "sub" | "proc" => "function",
            "defmacro" | "defmacrop" | "macro" | "macro_rules!" => "macro",
            "struct" => "struct",
            "class" | "object" | "record" => "class",
            "enum" => "enum",
            "interface" | "protocol" => "interface",
            "trait" | "typeclass" => "trait",
            "impl" | "instance" | "extension" => "impl",
            "mod" | "module" | "defmodule" | "namespace" | "package" => "module",
            "type" | "typealias" | "newtype" | "data" => "type",
            "const" | "static" => "constant",
            "test" => "test",
            _ => return None,
        })
    })
}

/// A kind for an outline `@item` (any grammar), from its node's name.
fn guess_kind(node: &str) -> &'static str {
    const KINDS: [(&str, &str); 15] = [
        ("method", "method"),
        ("field", "field"),
        ("argument", "field"),
        ("constructor", "constructor"),
        ("function", "function"),
        ("class", "class"),
        ("struct", "struct"),
        ("enum", "enum"),
        ("interface", "interface"),
        ("protocol", "interface"),
        ("trait", "trait"),
        ("impl", "impl"),
        ("module", "module"),
        ("namespace", "module"),
        ("macro", "macro"),
    ];
    KINDS.iter().find(|(word, _)| node.contains(word)).map_or("symbol", |(_, kind)| kind)
}

fn outline_config(language: Language) -> Option<&'static OutlineConfig> {
    match language {
        Language::Extension(_) => language.extension()?.outline_config(),
        _ => match crate::extensions::override_for(language) {
            Some(def) => def.outline_config(),
            None => crate::languages::def(language)?.outline_config(),
        },
    }
}

/// Huge data files (lockfiles) have more keys than anyone browses.
const MAX_SYMBOLS: usize = 5000;

fn config(language: Language) -> Option<&'static LangConfig> {
    match language {
        Language::Extension(_) => language.extension()?.highlight_config(),
        _ => match crate::extensions::override_for(language) {
            Some(def) => def.highlight_config(),
            None => Some(crate::languages::def(language)?.highlight_config()),
        },
    }
}

/// Queries a language brings besides highlights and outline, in Zed's format: `injections.scm`, `brackets.scm`, `indents.scm`,
/// `textobjects.scm` and `runnables.scm`.
pub(crate) struct LanguageQueries {
    injections: Option<InjectionQuery>,
    brackets: Option<Query>,
    indents: Option<Query>,
    textobjects: Option<Query>,
    pub(crate) runnables: Option<Query>,
}

struct InjectionQuery {
    query: Query,
    /// `@injection.content` (or the older `@content`).
    content: Option<u32>,
    /// `@injection.language` (or `@language`): the node naming the language.
    language: Option<u32>,
}

impl LanguageQueries {
    #[cfg(test)]
    pub(crate) fn has_injections(&self) -> bool {
        self.injections.is_some()
    }

    /// Compiles the queries that are there; one that doesn't fit the grammar
    /// is reported and left out.
    pub(crate) fn build(language: &TsLanguage, source: impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let mut compile = |file: &str| {
            let text = source(file)?;
            Query::new(language, &text).map_err(|e| problems.push(format!("{file}, line {}: {}", e.row + 1, e.message))).ok()
        };
        let injections = compile("injections.scm").map(|query| {
            let index = |names: [&str; 2]| names.iter().find_map(|n| query.capture_index_for_name(n));
            let content = index(["injection.content", "content"]);
            let language = index(["injection.language", "language"]);
            InjectionQuery { query, content, language }
        });
        Self {
            injections,
            brackets: compile("brackets.scm"),
            indents: compile("indents.scm"),
            textobjects: compile("textobjects.scm"),
            runnables: compile("runnables.scm"),
        }
    }
}

pub(crate) fn queries(language: Language) -> Option<&'static LanguageQueries> {
    match language {
        Language::Extension(_) => language.extension()?.queries(),
        _ => match crate::extensions::override_for(language) {
            Some(def) => def.queries(),
            None => Some(crate::languages::def(language)?.queries()),
        },
    }
}

/// An embedded language named in an injection query (a built-in language,
/// an extension's, or one known by its file extension): the language, its
/// highlighting, and its queries for what it embeds in turn. Markdown's
/// inline grammar is Markdown.
fn injected(name: &str) -> Option<(Language, &'static LangConfig, Option<&'static LanguageQueries>)> {
    let name = name.trim().to_lowercase();
    let builtin = match name.as_str() {
        "javascript" | "js" | "jsx" => Language::JavaScript,
        "typescript" | "ts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "html" => Language::Html,
        "css" => Language::Css,
        "rust" | "rs" => Language::Rust,
        "go" => Language::Go,
        "python" | "py" => Language::Python,
        "php" => Language::Php,
        "bash" | "sh" | "shell" | "zsh" => Language::Shell,
        "json" => Language::Json,
        "yaml" | "yml" => Language::Yaml,
        "toml" => Language::Toml,
        "markdown" | "md" => Language::Markdown,
        "markdown_inline" | "markdown-inline" => return Some((Language::Markdown, &MARKDOWN_INLINE, None)),
        _ => {
            let extension = crate::extensions::languages().into_iter().map(Language::Extension).find(|l| {
                l.extension().is_some_and(|d| d.name.to_lowercase() == name || d.id == name)
            });
            extension.unwrap_or_else(|| Language::from_path(Path::new(&format!("file.{name}"))))
        }
    };
    Some((builtin, config(builtin)?, queries(builtin)))
}

/// An embedded language's tree: one per region (a code fence, a
/// `<script>`), or one for all of a language's regions in its host when the
/// query says `injection.combined` (the HTML around PHP). Injections nest:
/// an injected tree has injections of its own (HTML in Markdown, then the
/// CSS in that HTML), up to `MAX_INJECTION_DEPTH`.
struct Injection {
    /// What editing there follows: comments, pairs, indentation.
    language: Language,
    config: &'static LangConfig,
    /// Its language's queries, for the injections inside it.
    queries: Option<&'static LanguageQueries>,
    tree: Option<Tree>,
    /// The text it parses, sorted and apart; edits move it like the tree.
    ranges: Vec<Range<usize>>,
    combined: bool,
    /// 1 inside the file's own language, 2 inside one of those…
    depth: u8,
    /// Its ranges changed: parse it again.
    dirty: bool,
}

impl Injection {
    fn copy(&self) -> Self {
        Injection { tree: self.tree.clone(), ranges: self.ranges.clone(), ..*self }
    }
}

const MAX_INJECTION_DEPTH: u8 = 4;

/// Parsers kept for reuse. A parser for a WASM grammar needs a `WasmStore`,
/// and making one compiles tree-sitter's WASM runtime again; short-lived
/// parses (the syntax-error lint, Markdown code blocks, diffs) are frequent.
static PARSERS: Mutex<Vec<Parser>> = Mutex::new(Vec::new());
const PARSER_POOL_SIZE: usize = 16;

/// A parser set to a language, from the pool; it goes back when dropped.
struct PooledParser(Option<Parser>);

impl PooledParser {
    fn new(language: &TsLanguage) -> Option<Self> {
        let mut parser = PARSERS.lock().ok().and_then(|mut pool| pool.pop()).unwrap_or_default();
        crate::extensions::prepare_parser(&mut parser, language);
        // On failure it goes back to the pool too.
        let mut pooled = PooledParser(Some(parser));
        pooled.set_language(language).ok()?;
        Some(pooled)
    }
}

impl Deref for PooledParser {
    type Target = Parser;
    fn deref(&self) -> &Parser {
        self.0.as_ref().expect("taken only on drop")
    }
}

impl DerefMut for PooledParser {
    fn deref_mut(&mut self) -> &mut Parser {
        self.0.as_mut().expect("taken only on drop")
    }
}

impl Drop for PooledParser {
    fn drop(&mut self) {
        let Some(mut parser) = self.0.take() else { return };
        parser.reset();
        if parser.set_included_ranges(&[]).is_ok()
            && let Ok(mut pool) = PARSERS.lock()
            && pool.len() < PARSER_POOL_SIZE
        {
            pool.push(parser);
        }
    }
}

/// Runs `f` with a parser for `language` from the pool.
pub fn with_parser<R>(language: &TsLanguage, f: impl FnOnce(&mut Parser) -> R) -> Option<R> {
    let mut parser = PooledParser::new(language)?;
    Some(f(&mut parser))
}

pub struct Syntax {
    language: Language,
    /// None for plain text (or a grammar that failed to load).
    parser: Option<PooledParser>,
    tree: Option<Tree>,
    /// Embedded languages (see the languages' injection queries), by depth.
    injections: Vec<Injection>,
    /// What edits touched since the last full parse: injections are looked
    /// for again there, not in the whole file.
    pending: Vec<Range<usize>>,
}

impl Syntax {
    pub fn new(language: Language, rope: &Rope) -> Self {
        let mut this = Self::unparsed(language);
        this.reparse(rope);
        this
    }

    /// Parsed for at most `budget`; `None` from `tree()` until a parse finishes.
    pub fn new_within(language: Language, rope: &Rope, budget: Duration) -> (Self, bool) {
        let mut this = Self::unparsed(language);
        let done = this.reparse_within(rope, Some(budget));
        (this, done)
    }

    fn unparsed(language: Language) -> Self {
        let parser = config(language).and_then(|config| PooledParser::new(&config.language));
        Self { language, parser, tree: None, injections: Vec::new(), pending: Vec::new() }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// A full parse to run in the background, from this syntax's trees (as
    /// edited since their last parse) and `rope`.
    pub fn parse_job(&self, rope: &Rope) -> impl FnOnce() -> Syntax + Send + 'static {
        let (language, rope, tree) = (self.language, rope.clone(), self.tree.clone());
        let injections: Vec<Injection> = self.injections.iter().map(Injection::copy).collect();
        let pending = self.pending.clone();
        move || {
            let mut syntax = Syntax::unparsed(language);
            syntax.tree = tree;
            syntax.injections = injections;
            syntax.pending = pending;
            syntax.reparse(&rope);
            syntax
        }
    }

    /// The trees of embedded languages, then the file's own.
    fn trees(&self) -> impl Iterator<Item = &Tree> {
        self.injections.iter().filter_map(|i| i.tree.as_ref()).chain(self.tree.as_ref())
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// The file's own tree for `rope`, to read off the UI thread: this one
    /// when it's `parsed` for that text (a cheap clone), else parsed again
    /// from it as edited (only what changed: an edit's parse still running).
    pub fn tree_job(&self, rope: &Rope, parsed: bool) -> impl FnOnce() -> Option<Tree> + Send + use<> {
        let tree = self.tree.clone();
        let grammar = if parsed { None } else { grammar(self.language) };
        let rope = rope.clone();
        move || match grammar {
            Some(grammar) => {
                with_parser(&grammar, |parser| parser.parse_with_options(&mut |byte, _| rope_chunk(&rope, byte), tree.as_ref(), None)).flatten()
            }
            None => tree,
        }
    }

    /// How many trees an edit edits: the file's and its embedded languages'.
    pub fn tree_count(&self) -> usize {
        self.trees().count()
    }

    /// Informs the old trees about an edit so the next parse can reuse nodes
    /// (embedded languages' trees too).
    pub fn edit(&mut self, edit: &InputEdit) {
        /// Past this many edits since the last parse, what they touched is
        /// kept as one range around them all, so an edit of many places
        /// (carets, a reload) doesn't move each earlier one per edit.
        const PENDING: usize = 64;
        for tree in self.tree.iter_mut().chain(self.injections.iter_mut().filter_map(|i| i.tree.as_mut())) {
            tree.edit(edit);
        }
        for range in self.injections.iter_mut().flat_map(|i| &mut i.ranges).chain(&mut self.pending) {
            *range = shift_range(range.clone(), edit);
        }
        self.pending.push(edit.start_byte..edit.new_end_byte);
        if self.pending.len() > PENDING {
            let start = self.pending.iter().map(|r| r.start).min().unwrap_or(0);
            let end = self.pending.iter().map(|r| r.end).max().unwrap_or(0);
            self.pending = vec![start..end];
        }
    }

    pub fn reparse(&mut self, rope: &Rope) {
        self.reparse_within(rope, None);
    }

    /// Parses again, giving up after `budget`: false then, and the trees are
    /// the old ones, edited, until a later parse finishes (see `parse_job`).
    pub fn reparse_within(&mut self, rope: &Rope, budget: Option<Duration>) -> bool {
        let Some(parser) = self.parser.as_mut() else { return true };
        let started = Instant::now();
        let in_time = move |_: &tree_sitter::ParseState| match budget {
            Some(budget) if started.elapsed() > budget => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        };
        let old = self.tree.clone();
        let mut callback = in_time;
        let options = ParseOptions::new().progress_callback(&mut callback);
        match parser.parse_with_options(&mut |byte, _| rope_chunk(rope, byte), self.tree.as_ref(), Some(options)) {
            Some(tree) => self.tree = Some(tree),
            None if budget.is_some() => {
                // Halted: start over next time rather than resume on other text.
                parser.reset();
                return false;
            }
            None => self.tree = None,
        }
        let Some(tree) = self.tree.clone() else {
            self.injections.clear();
            self.pending.clear();
            return true;
        };
        // Where injections may differ: what edits touched and what the new
        // tree says changed, widened to whole top-level nodes (a code fence's
        // language and its text belong together).
        let mut changed = std::mem::take(&mut self.pending);
        match &old {
            Some(old) => changed.extend(old.changed_ranges(&tree).map(|r| r.start_byte..r.end_byte)),
            None => changed = vec![0..rope.len_bytes()],
        }
        let changed = widen_to_top_level(&tree, changed, rope.len_bytes());
        if changed.is_empty() && !self.injections.iter().any(|i| i.dirty) {
            return true;
        }
        if !self.update_injections(&tree, rope, &changed, &mut callback) {
            self.pending = changed;
            return false;
        }
        true
    }

    /// Finds the injections in `changed` again (in the file's tree, then in
    /// the injected trees parsed meanwhile) and parses what's new or moved;
    /// injections elsewhere stay as they are. False when out of time.
    fn update_injections(
        &mut self,
        tree: &Tree,
        rope: &Rope,
        changed: &[Range<usize>],
        in_time: &mut dyn FnMut(&tree_sitter::ParseState) -> ControlFlow<()>,
    ) -> bool {
        // Separate injections in what changed are found again (their trees
        // kept for reuse); combined ones lose just the ranges there.
        let (mut reusable, kept): (Vec<Injection>, Vec<Injection>) =
            std::mem::take(&mut self.injections).into_iter().partition(|i| !i.combined && intersects(&i.ranges, changed));
        self.injections = kept;
        for injection in self.injections.iter_mut().filter(|i| i.combined) {
            let before = injection.ranges.len();
            injection.ranges.retain(|r| !intersects(std::slice::from_ref(r), changed));
            injection.dirty |= injection.ranges.len() != before;
        }
        // Each host: its language's queries, its own config (no injecting
        // itself), its tree, and the depth of what it injects.
        let mut hosts = vec![(queries(self.language), config(self.language), tree.clone(), 1u8)];
        while let Some((host_queries, own, host_tree, depth)) = hosts.pop() {
            if depth > MAX_INJECTION_DEPTH {
                continue;
            }
            let Some(query) = host_queries.and_then(|q| q.injections.as_ref()) else { continue };
            for found in find_injections(query, own, &host_tree, rope, changed) {
                if found.combined {
                    match self.injections.iter_mut().find(|i| i.combined && i.depth == depth && std::ptr::eq(i.config, found.config)) {
                        Some(injection) => {
                            injection.ranges.extend(found.ranges);
                            injection.dirty = true;
                        }
                        None => self.injections.push(Injection { language: found.language, config: found.config, queries: found.queries, tree: None, ranges: found.ranges, combined: true, depth, dirty: true }),
                    }
                    continue;
                }
                // The same region as before (edited inside): reuse its tree.
                let same = reusable.iter().position(|i| {
                    i.depth == depth && std::ptr::eq(i.config, found.config) && i.ranges.first().map(|r| r.start) == found.ranges.first().map(|r| r.start)
                });
                let tree = same.and_then(|ix| reusable.swap_remove(ix).tree);
                self.injections.push(Injection { language: found.language, config: found.config, queries: found.queries, tree, ranges: found.ranges, combined: false, depth, dirty: true });
            }
            for injection in self.injections.iter_mut().filter(|i| i.dirty) {
                normalize(&mut injection.ranges);
                let ranges: Vec<tree_sitter::Range> = injection.ranges.iter().map(|r| ts_range(rope, r.clone())).collect();
                let Some(mut parser) = PooledParser::new(&injection.config.language) else {
                    injection.dirty = false;
                    continue;
                };
                if ranges.is_empty() || parser.set_included_ranges(&ranges).is_err() {
                    injection.tree = None;
                    injection.dirty = false;
                    continue;
                }
                let mut in_time = &mut *in_time;
                let options = ParseOptions::new().progress_callback(&mut in_time);
                match parser.parse_with_options(&mut |byte, _| rope_chunk(rope, byte), injection.tree.as_ref(), Some(options)) {
                    Some(parsed) => injection.tree = Some(parsed),
                    // Out of time: the rest is finished later (it stays dirty).
                    None => return false,
                }
                injection.dirty = false;
                if let Some(tree) = &injection.tree {
                    hosts.push((injection.queries, Some(injection.config), tree.clone(), injection.depth + 1));
                }
            }
        }
        self.injections.sort_by_key(|i| (i.depth, i.ranges.first().map_or(0, |r| r.start)));
        true
    }

    /// The innermost layer at `offset`: the deepest embedded language whose
    /// text holds it, else the file's own. A `caret` sits between
    /// characters, so a region's end still counts (typing at the end of a
    /// `<script>`'s text is JavaScript); otherwise `offset` is a character's.
    fn layer_at(&self, offset: usize, caret: bool) -> Layer<'_> {
        let holds = |r: &Range<usize>| r.start <= offset && (offset < r.end || caret && offset == r.end);
        // Sorted by depth: the deepest last.
        match self.injections.iter().rev().find(|i| i.ranges.iter().any(holds)) {
            Some(i) => Layer { language: i.language, tree: i.tree.as_ref(), queries: i.queries },
            None => Layer { language: self.language, tree: self.tree.as_ref(), queries: queries(self.language) },
        }
    }

    /// The language editing at a caret at `offset` follows (pairs, comments,
    /// indentation): JavaScript in an HTML `<script>`, Rust in a Markdown
    /// fence, as Zed's `language_scope_at`.
    pub fn language_at(&self, offset: usize) -> Language {
        self.layer_at(offset, true).language
    }

    /// The language of the character at `offset`: for a line's first
    /// character, which a `</script>` right after the script's text isn't in.
    pub fn language_of_char(&self, offset: usize) -> Language {
        self.layer_at(offset, false).language
    }

    /// What a caret at `offset` is inside, by the innermost tree: `"string"`
    /// or `"comment"` (the names Zed's `not_in` uses), None in code. Strings'
    /// interpolations (`${…}`, `{…}` in an f-string) are code.
    pub fn scope_at(&self, rope: &Rope, offset: usize) -> Option<&'static str> {
        let layer = self.layer_at(offset, true);
        // From the character before the caret: a line comment ending at the
        // caret holds it, and nothing else at that point does.
        let mut node = layer.tree?.root_node().descendant_for_byte_range(offset.saturating_sub(1), offset)?;
        loop {
            let kind = node.kind();
            if ["substitution", "interpolation", "expansion"].iter().any(|k| kind.contains(k)) {
                return None;
            }
            let (start, end) = (node.start_byte(), node.end_byte());
            if kind.contains("comment") && start < offset {
                // A line comment ends at its line's end, where the caret still is in it.
                let line_comment = || {
                    layer.language.line_comment().is_some_and(|token| rope.byte_slice(start..end).to_string().starts_with(token.trim_end()))
                };
                if offset < end || offset == end && line_comment() {
                    return Some("comment");
                }
            }
            if (kind.contains("string") || kind.ends_with("quote_scalar")) && start < offset && offset < end {
                return Some("string");
            }
            node = node.parent()?;
        }
    }

    /// Where the `open` bracket a `close` typed at `offset` would close
    /// starts: the nearest one before it that isn't closed in between, by the
    /// grammar's tokens (so not brackets in strings and comments). Read before
    /// the closing one is typed, as the tree for code still being written
    /// has its closing brackets missing.
    pub fn unclosed_bracket(&self, offset: usize, open: &str, close: &str) -> Option<usize> {
        let tree = self.layer_at(offset, true).tree?;
        let mut depth = 0;
        unclosed_in(tree.root_node(), offset, open, close, &mut depth)
    }

    /// For a line whose first character is at `first`, after typing made a
    /// token there end at `caret`: the language's `indents.scm` says it goes
    /// back out. An `@end` (a closing `}`, `end`) goes level with the row its
    /// `@indent` node starts on; an `@outdent` one level less than the line
    /// above. As Zed reads these captures.
    pub fn dedent_at(&self, rope: &Rope, first: usize, caret: usize) -> Option<Dedent> {
        let layer = self.layer_at(first, false);
        let query = layer.queries?.indents.as_ref()?;
        let tree = layer.tree?;
        let (end, outdent) = (query.capture_index_for_name("end"), query.capture_index_for_name("outdent"));
        if end.is_none() && outdent.is_none() {
            return None;
        }
        let indent = query.capture_index_for_name("indent");
        let row = rope.byte_to_line(first);
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(first..caret.max(first + 1));
        let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
        while let Some(m) = matches.next() {
            let typed = |c: &tree_sitter::QueryCapture| c.node.start_byte() == first && c.node.end_byte() == caret;
            if m.captures.iter().any(|c| Some(c.index) == outdent && typed(c)) {
                return Some(Dedent::OneLevel);
            }
            if m.captures.iter().any(|c| Some(c.index) == end && typed(c))
                && let Some(node) = m.captures.iter().find(|c| Some(c.index) == indent).map(|c| c.node)
                && node.start_position().row < row
            {
                return Some(Dedent::LikeRow(node.start_position().row));
            }
        }
        None
    }

    /// Whether a line break at `offset` (on `row`) should indent the next
    /// line, by the language's `indents.scm`: an `@indent` node starts on
    /// this row and goes on past the caret. With an `@start` in the match
    /// (`then`, a `:`), that token is on this row before the caret instead.
    /// None without such a query.
    pub fn indent_after(&self, rope: &Rope, offset: usize, row: usize) -> Option<bool> {
        let layer = self.layer_at(offset, true);
        let query = layer.queries?.indents.as_ref()?;
        let tree = layer.tree?;
        let indent = query.capture_index_for_name("indent")?;
        let start = query.capture_index_for_name("start");
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(offset.saturating_sub(1)..offset + 1);
        let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
        while let Some(m) = matches.next() {
            let Some(node) = m.captures.iter().find(|c| c.index == indent).map(|c| c.node) else { continue };
            if node.end_byte() <= offset {
                continue;
            }
            let opens = match m.captures.iter().find(|c| Some(c.index) == start) {
                Some(c) => c.node.start_position().row == row && c.node.end_byte() <= offset,
                None => node.start_position().row == row && node.start_byte() < offset,
            };
            if opens {
                return Some(true);
            }
        }
        Some(false)
    }

    /// A function, class or comment around `offset` from `textobjects.scm`
    /// (`@function.around`, `@class.inside` …). An `inside` can be several
    /// nodes, a body's statements or an enum's variants: it spans them all,
    /// without the white space at its ends. The innermost object whose
    /// `around` holds `offset` wins, so `inside` works from a function's
    /// first line or between its statements too.
    pub fn text_object(&self, rope: &Rope, offset: usize, object: &str, around: bool) -> Option<Range<usize>> {
        let query = queries(self.language)?.textobjects.as_ref()?;
        let tree = self.tree.as_ref()?;
        let outer = query.capture_index_for_name(&format!("{object}.around"));
        let inner = query.capture_index_for_name(&format!("{object}.inside"));
        if outer.is_none() && inner.is_none() {
            return None;
        }
        let join = |a: Option<Range<usize>>, b: Range<usize>| a.map_or(b.clone(), |a| a.start.min(b.start)..a.end.max(b.end));
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(offset..offset + 1);
        let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
        // Per pattern and `around` node, the extent of `inside`: separators
        // between the nodes split one capture into several matches.
        let mut objects: Vec<(usize, Option<Range<usize>>, Option<Range<usize>>)> = Vec::new();
        while let Some(m) = matches.next() {
            let (mut inside, mut outside) = (None, None);
            for c in m.captures {
                if Some(c.index) == inner {
                    inside = Some(join(inside, c.node.byte_range()));
                } else if Some(c.index) == outer {
                    outside = Some(join(outside, c.node.byte_range()));
                }
            }
            match objects.iter_mut().find(|o| outside.is_some() && o.0 == m.pattern_index && o.2 == outside) {
                Some(o) => o.1 = inside.into_iter().chain(o.1.clone()).reduce(|a, b| join(Some(a), b)),
                None if inside.is_some() || outside.is_some() => objects.push((m.pattern_index, inside, outside)),
                None => {}
            }
        }
        // The smallest inside wins, then the biggest around: a decorated
        // function over the same function undecorated.
        let size = |(_, i, a): &(usize, Option<Range<usize>>, Option<Range<usize>>)| {
            (i.as_ref().or(a.as_ref()).map_or(0, |r| r.len()), usize::MAX - a.as_ref().map_or(0, |r| r.len()))
        };
        let (_, inside, outside) = objects
            .into_iter()
            .filter(|o| !around || o.2.is_some())
            .filter(|o| o.2.as_ref().or(o.1.as_ref()).is_some_and(|r| r.start <= offset && offset < r.end.max(r.start + 1)))
            .min_by_key(size)?;
        let range = if around { outside? } else { inside.or(outside)? };
        let text = rope.byte_slice(range.clone()).to_string();
        let start = range.start + text.len() - text.trim_start().len();
        let end = range.end - (text.len() - text.trim_end().len());
        Some(if start < end { start..end } else { range })
    }

    /// The smallest syntax node that strictly contains `range`, for
    /// Extend Selection. Considers embedded `<script>` trees too.
    pub fn expand(&self, range: Range<usize>) -> Option<Range<usize>> {
        let grow = |tree: &Tree| -> Option<Range<usize>> {
            let mut node = tree.root_node().descendant_for_byte_range(range.start, range.end)?;
            loop {
                let r = node.byte_range();
                if r.start <= range.start && r.end >= range.end && r != range {
                    return Some(r);
                }
                node = node.parent()?;
            }
        };
        self.trees().filter_map(grow).min_by_key(|r| r.len())
    }

    /// The bracket at `offset` (or just before it) and its partner. Brackets
    /// in strings and comments aren't grammar tokens, so they never match.
    pub fn matching_bracket(&self, rope: &Rope, offset: usize) -> Option<(usize, usize)> {
        self.bracket_pair(rope, offset).map(|(a, b)| (a.start, b.start))
    }

    /// Like `matching_bracket`, with both brackets' extents: `brackets.scm`
    /// pairs can be words (`do` … `end`).
    pub fn bracket_pair(&self, rope: &Rope, offset: usize) -> Option<(Range<usize>, Range<usize>)> {
        // The bracket after the cursor wins, as in JetBrains.
        for at in [Some(offset), offset.checked_sub(1)].into_iter().flatten() {
            if let Some(pair) = self.query_bracket(rope, at) {
                return Some(pair);
            }
            for tree in self.trees() {
                if let Some(partner) = bracket_partner(tree.root_node(), at) {
                    return Some((at..at + 1, partner..partner + 1));
                }
            }
        }
        None
    }

    /// The pair from `brackets.scm` (`@open`, `@close`) one end of which starts at `at`.
    fn query_bracket(&self, rope: &Rope, at: usize) -> Option<(Range<usize>, Range<usize>)> {
        let layer = self.layer_at(at, false);
        let query = layer.queries?.brackets.as_ref()?;
        let tree = layer.tree?;
        let (open, close) = (query.capture_index_for_name("open")?, query.capture_index_for_name("close")?);
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(at..at + 1);
        let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
        while let Some(m) = matches.next() {
            let node = |index| m.captures.iter().find(|c| c.index == index).map(|c| c.node.byte_range());
            let (Some(o), Some(c)) = (node(open), node(close)) else { continue };
            // Quotes pair a string's ends; skip pairs that are the same token.
            if o == c {
                continue;
            }
            if o.start == at {
                return Some((o, c));
            }
            if c.start == at {
                return Some((c, o));
            }
        }
        None
    }

    /// The file's definitions in document order, nested by containment.
    pub fn symbols(&self, rope: &Rope) -> Vec<Symbol> {
        match self.tree.as_ref() {
            Some(tree) => symbols(self.language, tree, rope),
            None => Vec::new(),
        }
    }

    /// Returns non-overlapping highlighted spans (absolute byte ranges) for `range`.
    pub fn highlights(&self, rope: &Rope, range: Range<usize>) -> Vec<(Range<usize>, Highlight)> {
        let Some(tree) = self.tree.as_ref() else {
            return Vec::new();
        };
        let len = range.end.saturating_sub(range.start);
        if len == 0 {
            return Vec::new();
        }
        // A per-byte map of the visible slice. Captures are applied in query
        // order so later (more specific) patterns and nested nodes win.
        let mut map = vec![Highlight::None; len];
        let Some(config) = config(self.language) else {
            return Vec::new();
        };
        apply_query(config, tree.root_node(), rope, range.clone(), &mut map);
        // Deeper injections last: the innermost language colors its text.
        for injection in self.injections.iter().filter(|i| intersects(&i.ranges, std::slice::from_ref(&range))) {
            if let Some(tree) = injection.tree.as_ref() {
                apply_query(injection.config, tree.root_node(), rope, range.clone(), &mut map);
            }
        }

        let mut spans = Vec::new();
        let mut start = 0;
        for i in 1..=len {
            if i == len || map[i] != map[start] {
                if map[start] != Highlight::None {
                    spans.push((range.start + start..range.start + i, map[start]));
                }
                start = i;
            }
        }
        spans
    }
}

/// One language's part of a file: the file's own, or an embedded one's.
struct Layer<'a> {
    language: Language,
    /// None until it's parsed.
    tree: Option<&'a Tree>,
    queries: Option<&'static LanguageQueries>,
}

/// Where a line goes when a token typed at its start closes a block (see
/// `Syntax::dedent_at`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dedent {
    /// Level with this row.
    LikeRow(usize),
    /// One level less than the line above.
    OneLevel,
}

/// `node`'s tokens before `before`, from the last, for `unclosed_bracket`:
/// a whole node without errors holds its brackets in pairs, so only the one
/// holding `before` and broken ones are looked into.
fn unclosed_in(node: Node, before: usize, open: &str, close: &str, depth: &mut usize) -> Option<usize> {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).take_while(|c| c.start_byte() < before).collect();
    for child in children.into_iter().rev() {
        if child.is_missing() {
            continue;
        }
        if child.child_count() == 0 {
            if child.is_named() {
                continue;
            }
            if child.kind() == close {
                *depth += 1;
            } else if child.kind() == open {
                if *depth == 0 {
                    return Some(child.start_byte());
                }
                *depth -= 1;
            }
        } else if (child.has_error() || child.end_byte() > before)
            && let Some(found) = unclosed_in(child, before, open, close, depth)
        {
            return Some(found);
        }
    }
    None
}

/// The offset of the bracket matching the bracket token at `at`: the nearest
/// sibling of the other kind, since a pair shares a parent node.
fn bracket_partner(root: Node, at: usize) -> Option<usize> {
    let node = root.descendant_for_byte_range(at, at + 1)?;
    if node.is_named() || node.byte_range() != (at..at + 1) {
        return None;
    }
    let (partner, forward) = match node.kind() {
        "(" => (")", true),
        "[" => ("]", true),
        "{" => ("}", true),
        ")" => ("(", false),
        "]" => ("[", false),
        "}" => ("{", false),
        _ => return None,
    };
    // A cursor steps to the next sibling in constant time; `Node`'s sibling
    // methods start over from the parent each time, which is quadratic in a
    // big array or object. Backwards, the nearest partner before the bracket
    // is found in one pass from the first sibling.
    let parent = node.parent()?;
    let mut cursor = parent.walk();
    if forward {
        cursor.goto_first_child_for_byte(at)?;
        if cursor.node() != node {
            return None;
        }
        while cursor.goto_next_sibling() {
            let sibling = cursor.node();
            if !sibling.is_named() && sibling.kind() == partner {
                return Some(sibling.start_byte());
            }
        }
        None
    } else {
        cursor.goto_first_child();
        let mut found = None;
        loop {
            let sibling = cursor.node();
            if sibling == node {
                return found;
            }
            if !sibling.is_named() && sibling.kind() == partner {
                found = Some(sibling.start_byte());
            }
            if !cursor.goto_next_sibling() {
                return None;
            }
        }
    }
}

fn apply_query(
    config: &LangConfig,
    root: Node,
    rope: &Rope,
    range: Range<usize>,
    map: &mut [Highlight],
) {
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range.clone());
    let mut captures = cursor.captures(&config.query, root, RopeProvider(rope));
    while let Some((m, ix)) = captures.next() {
        let capture = m.captures[*ix];
        let h = config.captures[capture.index as usize];
        let node = capture.node;
        let s = node.start_byte().max(range.start) - range.start;
        let e = node.end_byte().min(range.end).saturating_sub(range.start);
        if s < e {
            map[s..e].fill(h);
        }
    }
}

/// An injection a query found: the language and the text it covers.
struct Found {
    language: Language,
    config: &'static LangConfig,
    queries: Option<&'static LanguageQueries>,
    ranges: Vec<Range<usize>>,
    combined: bool,
}

/// The injections `query` finds in `tree` within `within` (each match one
/// region; `injection.combined` ones joined later by language).
fn find_injections(
    query: &InjectionQuery,
    own: Option<&'static LangConfig>,
    tree: &Tree,
    rope: &Rope,
    within: &[Range<usize>],
) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    let mut known: HashMap<String, Option<(Language, &'static LangConfig, Option<&'static LanguageQueries>)>> = HashMap::new();
    let mut cursor = QueryCursor::new();
    for range in within {
        cursor.set_byte_range(range.clone());
        let mut matches = cursor.matches(&query.query, tree.root_node(), RopeProvider(rope));
        while let Some(m) = matches.next() {
            let named = m.captures.iter().find(|c| Some(c.index) == query.language).map(|c| rope.byte_slice(c.node.byte_range()).to_string());
            let settings = query.query.property_settings(m.pattern_index);
            let set = || {
                settings.iter().find_map(|p| matches!(p.key.as_ref(), "injection.language" | "language").then(|| p.value.as_deref().map(str::to_string)).flatten())
            };
            // Looked up once per name: comments (JSDoc) and regexes match often,
            // and are usually languages Kuana doesn't have.
            let Some((language, config, queries)) = named.or_else(set).and_then(|name| *known.entry(name).or_insert_with_key(|name| injected(name))) else {
                continue;
            };
            if own.is_some_and(|own| std::ptr::eq(own, config)) {
                continue;
            }
            let ranges: Vec<Range<usize>> =
                m.captures.iter().filter(|c| Some(c.index) == query.content).map(|c| c.node.byte_range()).filter(|r| !r.is_empty()).collect();
            if ranges.is_empty() || out.iter().any(|f| std::ptr::eq(f.config, config) && f.ranges.first() == ranges.first()) {
                continue;
            }
            let combined = settings.iter().any(|p| p.key.as_ref() == "injection.combined");
            out.push(Found { language, config, queries, ranges, combined });
        }
    }
    out
}

/// Sorts ranges and drops overlaps: a parser takes them in order and apart.
fn normalize(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_by_key(|r| r.start);
    let mut end = 0;
    ranges.retain(|r| {
        let keep = r.start >= end && r.end > r.start;
        if keep {
            end = r.end;
        }
        keep
    });
}

/// Whether any of `a` touches any of `b` (both sorted or not, small).
fn intersects(a: &[Range<usize>], b: &[Range<usize>]) -> bool {
    a.iter().any(|x| b.iter().any(|y| x.start <= y.end && y.start <= x.end))
}

/// `ranges` grown to the top-level nodes of `tree` they touch, merged.
fn widen_to_top_level(tree: &Tree, ranges: Vec<Range<usize>>, len: usize) -> Vec<Range<usize>> {
    let root = tree.root_node();
    let mut out: Vec<Range<usize>> = ranges
        .into_iter()
        .map(|r| {
            let (start, end) = (r.start.min(len), r.end.min(len).max(r.start.min(len)));
            let mut node = root.descendant_for_byte_range(start, end).unwrap_or(root);
            while let Some(parent) = node.parent().filter(|p| p.id() != root.id()) {
                node = parent;
            }
            let wide = node.byte_range();
            wide.start.min(start)..wide.end.max(end)
        })
        .collect();
    out.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(out.len());
    for r in out {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// Where a range of the text before `edit` is after it: moved by an edit
/// before it; an end inside the replaced text goes to the end of the new.
fn shift_range(range: Range<usize>, edit: &InputEdit) -> Range<usize> {
    let map = |x: usize| {
        if x <= edit.start_byte {
            x
        } else if x >= edit.old_end_byte {
            x - edit.old_end_byte + edit.new_end_byte
        } else {
            edit.new_end_byte
        }
    };
    let start = map(range.start);
    start..map(range.end).max(start)
}

/// A byte range as tree-sitter wants it, with rows and columns.
fn ts_range(rope: &Rope, range: Range<usize>) -> tree_sitter::Range {
    let point = |byte: usize| {
        let byte = byte.min(rope.len_bytes());
        let row = rope.byte_to_line(byte);
        tree_sitter::Point::new(row, byte - rope.line_to_byte(row))
    };
    tree_sitter::Range { start_byte: range.start, end_byte: range.end, start_point: point(range.start), end_point: point(range.end) }
}

pub fn rope_chunk(rope: &Rope, byte: usize) -> &[u8] {
    if byte >= rope.len_bytes() {
        return &[];
    }
    let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
    &chunk.as_bytes()[byte - chunk_start..]
}

struct RopeProvider<'a>(&'a Rope);

impl<'a> TextProvider<&'a [u8]> for RopeProvider<'a> {
    type I = Box<dyn Iterator<Item = &'a [u8]> + 'a>;

    fn text(&mut self, node: Node) -> Self::I {
        let rope = self.0;
        let start = rope.byte_to_char(node.start_byte().min(rope.len_bytes()));
        let end = rope.byte_to_char(node.end_byte().min(rope.len_bytes()));
        Box::new(rope.slice(start..end).chunks().map(str::as_bytes))
    }
}

pub fn input_edit(
    rope_before: &Rope,
    range: Range<usize>,
    new_text: &str,
) -> InputEdit {
    let point = |rope: &Rope, byte: usize| {
        let line = rope.byte_to_line(byte);
        tree_sitter::Point::new(line, byte - rope.line_to_byte(line))
    };
    let start_position = point(rope_before, range.start);
    let old_end_position = point(rope_before, range.end);
    let new_end_byte = range.start + new_text.len();
    let newlines = new_text.matches('\n').count();
    let new_end_position = if newlines == 0 {
        tree_sitter::Point::new(start_position.row, start_position.column + new_text.len())
    } else {
        let last = new_text.rfind('\n').unwrap();
        tree_sitter::Point::new(start_position.row + newlines, new_text.len() - last - 1)
    };
    InputEdit {
        start_byte: range.start,
        old_end_byte: range.end,
        new_end_byte,
        start_position,
        old_end_position,
        new_end_position,
    }
}

/// File Structure's definitions in `tree` (see `Syntax::symbols`); takes no
/// `Syntax`, so it can run in the background on a tree's clone.
pub fn symbols(language: Language, tree: &Tree, rope: &Rope) -> Vec<Symbol> {
    let Some(config) = outline_config(language) else {
        return Vec::new();
    };
    // (definition range, kind, name range)
    let mut found: Vec<(Range<usize>, &'static str, Range<usize>)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&config.query, tree.root_node(), RopeProvider(rope));
    while let Some(m) = matches.next() {
        let mut def = None;
        let mut name = None;
        let context: Vec<String> = m
            .captures
            .iter()
            .filter(|c| Some(c.index) == config.context && c.node.byte_range().len() < 40)
            .map(|c| rope.byte_slice(c.node.byte_range()).to_string())
            .collect();
        for c in m.captures {
            if c.index == config.name {
                name = Some(c.node.byte_range());
            } else if let Some(kind) = config.kinds[c.index as usize] {
                let kind = match kind {
                    "symbol" => context_kind(&context).unwrap_or_else(|| guess_kind(c.node.kind())),
                    kind => node_kind_label(c.node.kind()).unwrap_or(kind),
                };
                def = Some((c.node.byte_range(), kind));
            }
        }
        if let (Some((range, kind)), Some(name)) = (def, name) {
            found.push((range, kind, name));
        }
        if found.len() >= MAX_SYMBOLS * 2 {
            break;
        }
    }
    // Several patterns can match one definition (a Rust method is also a
    // function); the first, more specific one wins.
    found.sort_by_key(|(range, _, _)| (range.start, std::cmp::Reverse(range.end)));
    found.dedup_by(|b, a| a.0 == b.0);

    let mut symbols = Vec::new();
    let mut open: Vec<usize> = Vec::new(); // ends of enclosing definitions
    for (range, kind, name) in found.into_iter().take(MAX_SYMBOLS) {
        while open.last().is_some_and(|&end| end <= range.start) {
            open.pop();
        }
        let text = rope.byte_slice(name.clone()).to_string();
        let mut name_text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if name_text.len() > 100 {
            name_text.truncate(name_text.floor_char_boundary(100));
            name_text.push('…');
        }
        if !name_text.is_empty() {
            symbols.push(Symbol { name: name_text, kind, offset: name.start, depth: open.len(), range: range.clone() });
        }
        open.push(range.end);
    }
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_trees_are_edited_and_reused() {
        let before = "<p>x</p><script>let a = 1;\nlet b = 2;</script>";
        let after = "<p>x</p><script>let a = 10;\nlet b = 2;</script>";
        let mut syntax = Syntax::new(Language::Html, &Rope::from_str(before));
        let at = before.find("1;").unwrap() + 1;
        let rope = Rope::from_str(after);
        syntax.edit(&InputEdit {
            start_byte: at,
            old_end_byte: at,
            new_end_byte: at + 1,
            start_position: tree_sitter::Point::new(0, at),
            old_end_position: tree_sitter::Point::new(0, at),
            new_end_position: tree_sitter::Point::new(0, at + 1),
        });
        syntax.reparse(&rope);
        let fresh = Syntax::new(Language::Html, &rope);
        let sexp = |s: &Syntax| s.injections.iter().map(|i| i.tree.as_ref().unwrap().root_node().to_sexp()).collect::<Vec<_>>();
        assert_eq!(sexp(&syntax), sexp(&fresh));
        let number = |s: &Syntax| s.injections[0].tree.as_ref().unwrap().root_node().descendant_for_byte_range(at, at).map(|n| n.byte_range());
        assert_eq!(number(&syntax), number(&fresh));
    }

    /// Each fenced block is its own tree (an unclosed brace in one doesn't
    /// spill into the next), injections nest (CSS in HTML in Markdown), and
    /// PHP's HTML pieces are one tree (`injection.combined`).
    #[test]
    fn injections_are_per_region_nested_or_combined() {
        let md = highlights_of("a.md", "```js\nfunction f() {\n```\n\n```js\nconst x = 1;\n```\n");
        assert!(has(&md, "const", Highlight::Keyword), "{md:?}");
        let nested = highlights_of("a.md", "```html\n<style>body { color: red }</style>\n```\n");
        assert!(has(&nested, "color", Highlight::Property), "{nested:?}");
        let rope = Rope::from_str("<p>a</p>\n<?php echo 1; ?>\n<b>b</b>\n");
        let php = Syntax::new(Language::Php, &rope);
        let html: Vec<&Injection> = php.injections.iter().filter(|i| i.combined).collect();
        assert_eq!(html.len(), 1, "one tree for the HTML around PHP");
        assert!(html[0].ranges.len() >= 2);
    }

    /// After an edit, injections are looked for again only around it, and
    /// come out as a fresh parse has them.
    #[test]
    fn injections_follow_edits_like_a_fresh_parse() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("## Part {i}\n\n```rust\nfn f{i}() {{}}\n```\n\n"));
        }
        let mut rope = Rope::from_str(&text);
        let mut syntax = Syntax::new(Language::Markdown, &rope);
        let layers = |s: &Syntax| {
            let mut l: Vec<(String, Vec<(usize, usize)>, u8)> = s
                .injections
                .iter()
                .map(|i| (i.tree.as_ref().map(|t| t.root_node().to_sexp()).unwrap_or_default(), i.ranges.iter().map(|r| (r.start, r.end)).collect(), i.depth))
                .collect();
            l.sort();
            l
        };
        // Typing in one fence, then turning one into another language.
        for (find, insert) in [("fn f7", "pub "), ("```rust\nfn f20", "```js\n")] {
            let at = text.find(find).unwrap();
            let edit = input_edit(&rope, at..at, insert);
            text.insert_str(at, insert);
            rope = Rope::from_str(&text);
            syntax.edit(&edit);
            assert!(syntax.reparse_within(&rope, None));
            assert_eq!(layers(&syntax), layers(&Syntax::new(Language::Markdown, &rope)), "after inserting {insert:?}");
        }
    }

    /// The language at a caret is the innermost embedded one holding it.
    #[test]
    fn the_language_at_an_offset_is_the_innermost() {
        let text = "<p>a</p>\n<script>\nlet a = 1;\n</script>\n<style>\nb { c: d }\n</style>\n";
        let syntax = Syntax::new(Language::Html, &Rope::from_str(text));
        let at = |s: &str| text.find(s).unwrap();
        assert_eq!(syntax.language_at(at("<p>")), Language::Html);
        assert_eq!(syntax.language_at(at("let")), Language::JavaScript);
        assert_eq!(syntax.language_at(at("c: d")), Language::Css);
        // A caret at the end of the script's text is in it; `</script>` isn't.
        assert_eq!(syntax.language_at(at("</script>")), Language::JavaScript);
        assert_eq!(syntax.language_of_char(at("</script>")), Language::Html);

        let md = "Some *text*.\n\n```python\nx = 1\n```\n\n```html\n<style>a { b: c }</style>\n```\n";
        let syntax = Syntax::new(Language::Markdown, &Rope::from_str(md));
        let at = |s: &str| md.find(s).unwrap();
        assert_eq!(syntax.language_at(at("text")), Language::Markdown);
        assert_eq!(syntax.language_at(at("x = 1")), Language::Python);
        assert_eq!(syntax.language_at(at("<style>")), Language::Html);
        assert_eq!(syntax.language_at(at("b: c")), Language::Css);
        assert_eq!(syntax.language_of_char(at("```\n\n```html")), Language::Markdown);
    }

    #[test]
    fn strings_and_comments_are_found_at_a_caret() {
        let scope = |language: Language, text: &str| {
            let at = text.find('|').unwrap();
            let rope = Rope::from_str(&text.replacen('|', "", 1));
            Syntax::new(language, &rope).scope_at(&rope, at)
        };
        assert_eq!(scope(Language::Rust, "let s = \"a|b\";"), Some("string"));
        assert_eq!(scope(Language::Rust, "let s = |\"ab\";"), None);
        assert_eq!(scope(Language::Rust, "let s = \"ab\"|;"), None);
        assert_eq!(scope(Language::Rust, "// a comment|\nfn f() {}"), Some("comment"));
        assert_eq!(scope(Language::Rust, "/* a */| fn f() {}"), None);
        assert_eq!(scope(Language::Python, "x = f\"{y|}\""), None);
        assert_eq!(scope(Language::Python, "x = f\"a|{y}\""), Some("string"));
        assert_eq!(scope(Language::JavaScript, "let s = `a${b|}`;"), None);
        assert_eq!(scope(Language::Html, "<!-- a| -->"), Some("comment"));
        assert_eq!(scope(Language::Html, "<script>let s = '|';</script>"), Some("string"));
    }

    /// The bracket a closing one typed would close, by tokens: complete pairs
    /// and brackets in strings don't count, broken code does.
    #[test]
    fn unclosed_brackets_are_found_before_a_caret() {
        let unclosed = |language: Language, text: &str, open: &str, close: &str| {
            let at = text.find('|').unwrap();
            let rope = Rope::from_str(&text.replacen('|', "", 1));
            Syntax::new(language, &rope).unclosed_bracket(at, open, close)
        };
        assert_eq!(unclosed(Language::Rust, "fn f() {\n    g(\"{\", { 1 });\n    |", "{", "}"), Some(7));
        assert_eq!(unclosed(Language::Rust, "fn f() {\n    if x {\n        |\n}", "{", "}"), Some(18));
        assert_eq!(unclosed(Language::Python, "f(a, [b],\n  |", "(", ")"), Some(1));
        assert_eq!(unclosed(Language::Rust, "fn f() {}\n|", "{", "}"), None);
    }

    #[test]
    fn pooled_parsers_come_back_clean() {
        // HTML's script injection parses only included ranges; the parser
        // must read whole files again when the pool hands it out next.
        for _ in 0..3 {
            let html = Rope::from_str("<p>x</p><script>let a = 1;</script>");
            let syntax = Syntax::new(Language::Html, &html);
            assert_eq!(syntax.injections.len(), 1);
            drop(syntax);
            let js = Rope::from_str("let b = 2;\nlet c = 3;\n");
            let syntax = Syntax::new(Language::JavaScript, &js);
            let root = syntax.tree().unwrap().root_node();
            assert!(!root.has_error());
            assert_eq!(root.named_child_count(), 2, "{}", root.to_sexp());
        }
        let tree = with_parser(&config(Language::Json).unwrap().language, |p| p.parse("[1, 2]", None)).flatten();
        assert_eq!(tree.unwrap().root_node().kind(), "document");
    }

    #[test]
    fn highlight_queries_compile() {
        for language in ALL_HIGHLIGHTED {
            let config = config(language).unwrap();
            assert!(!config.captures.is_empty(), "{language:?}");
        }
    }

    const ALL_HIGHLIGHTED: [Language; 14] = [
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Html,
        Language::Css,
        Language::Rust,
        Language::Go,
        Language::Python,
        Language::Php,
        Language::Shell,
        Language::Json,
        Language::Yaml,
        Language::Toml,
        Language::Markdown,
    ];

    fn highlights_of(path: &str, src: &str) -> Vec<(String, Highlight)> {
        let language = Language::from_path(Path::new(path));
        let rope = Rope::from_str(src);
        let syntax = Syntax::new(language, &rope);
        syntax
            .highlights(&rope, 0..src.len())
            .into_iter()
            .map(|(r, h)| (src[r].to_string(), h))
            .collect()
    }

    fn has(spans: &[(String, Highlight)], text: &str, h: Highlight) -> bool {
        spans.iter().any(|(t, x)| t == text && x.base() == h)
    }

    fn outline(path: &str, src: &str) -> Vec<(String, &'static str, usize)> {
        let language = Language::from_path(Path::new(path));
        let rope = Rope::from_str(src);
        Syntax::new(language, &rope).symbols(&rope).into_iter().map(|s| (s.name, s.kind, s.depth)).collect()
    }

    #[test]
    fn go_names_keep_their_colors() {
        let src = "package main\n\nimport \"fmt\"\n\nconst Max = 10\n\ntype Point struct {\n\tX int\n}\n\n\
                   func (p Point) Len(scale int) int {\n\treturn p.X * scale\n}\n\n\
                   func main() {\n\tp := Point{X: 1}\n\tfmt.Println(p.Len(2), len(\"x\"))\n}\n";
        let go = highlights_of("a.go", src);
        let expected = [
            ("main", Highlight::Function),
            ("Len", Highlight::Method),
            ("Println", Highlight::Method),
            ("len", Highlight::Function),
            ("Max", Highlight::Constant),
            ("Point", Highlight::Type),
            ("X", Highlight::Property),
            ("(", Highlight::Punctuation),
            ("{", Highlight::Punctuation),
        ];
        for (text, h) in expected {
            assert!(has(&go, text, h), "{text} as {}: {go:?}", h.name());
        }
    }

    #[test]
    fn grammars_injections_highlight_embedded_code() {
        let html = highlights_of("a.html", "<style>body { color: red }</style>\n");
        assert!(has(&html, "color", Highlight::Property), "{html:?}");
        let md = highlights_of("a.md", "# Title\n\n```rust\nfn main() {}\n```\n");
        assert!(has(&md, "fn", Highlight::Keyword), "{md:?}");
        let php = highlights_of("a.php", "<p class=\"x\">hi</p>\n<?php echo 1; ?>\n");
        assert!(has(&php, "p", Highlight::Tag) && has(&php, "echo", Highlight::Keyword), "{php:?}");
        let js = highlights_of("a.js", "const s = html`<b>bold</b>`;\n");
        assert!(has(&js, "b", Highlight::Tag), "{js:?}");
    }

    #[test]
    fn outline_queries_compile() {
        for language in ALL_HIGHLIGHTED {
            if language != Language::Html {
                assert!(outline_config(language).is_some(), "{language:?}");
            }
        }
        // A query marking only comments (Zed's for templates) has no items;
        // one marking items needs their names.
        let rust: TsLanguage = tree_sitter_rust::LANGUAGE.into();
        assert!(OutlineConfig::build(rust.clone(), "(line_comment) @annotation").unwrap().is_none());
        assert!(OutlineConfig::build(rust.clone(), "(function_item) @item").is_err());
        assert!(OutlineConfig::build(rust, "(function_item name: (identifier) @name) @item").unwrap().is_some());
    }

    #[test]
    fn builds_outlines() {
        let rust = outline("a.rs", "struct Point;\nimpl Point {\n    fn new() -> Self { Point }\n}\nfn main() {}\n");
        assert_eq!(
            rust,
            [
                ("Point".to_string(), "struct", 0),
                ("Point".to_string(), "impl", 0),
                ("new".to_string(), "method", 1),
                ("main".to_string(), "function", 0)
            ]
        );
        let ts = outline("a.ts", "interface P { x: number }\nclass A {\n  run() {}\n}\nexport function f() {}\n");
        assert!(ts.contains(&("P".to_string(), "interface", 0)), "{ts:?}");
        assert!(ts.contains(&("A".to_string(), "class", 0)), "{ts:?}");
        assert!(ts.contains(&("run".to_string(), "method", 1)), "{ts:?}");
        assert!(ts.contains(&("f".to_string(), "function", 0)), "{ts:?}");
        let md = outline("a.md", "# Title\n\nText\n\n## Part one\n\n## Part two\n");
        assert_eq!(
            md,
            [
                ("Title".to_string(), "heading", 0),
                ("Part one".to_string(), "heading", 1),
                ("Part two".to_string(), "heading", 1)
            ]
        );
        let toml = outline("Cargo.toml", "[package]\nname = \"x\"\n[dependencies]\n");
        assert_eq!(toml.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["package", "dependencies"]);
        let css = outline("a.css", ".a, .b { color: red; }\n");
        assert_eq!(css, [(".a, .b".to_string(), "selector", 0)]);
        let json = outline("a.json", "{\"a\": {\"b\": 1}}\n");
        assert_eq!(json, [("a".to_string(), "key", 0), ("b".to_string(), "key", 1)]);
    }

    #[test]
    fn matches_brackets_of_big_arrays_quickly() {
        // 40 000 elements: stepping sibling by sibling from the parent each
        // time took seconds.
        let src = format!("[{}[1]]", "1,".repeat(40_000));
        let rope = Rope::from_str(&src);
        let syntax = Syntax::new(Language::Json, &rope);
        let (last, inner) = (src.len() - 1, src.len() - 4);
        let started = std::time::Instant::now();
        assert_eq!(syntax.matching_bracket(&rope, 0), Some((0, last)));
        assert_eq!(syntax.matching_bracket(&rope, last), Some((last, 0)));
        assert_eq!(syntax.matching_bracket(&rope, inner), Some((inner, inner + 2)));
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn matches_brackets_outside_strings() {
        let src = "fn f(a: [u8; 2]) { let s = \"a(\"; }\n";
        let rope = Rope::from_str(src);
        let syntax = Syntax::new(Language::Rust, &rope);
        let at = |c: char, nth: usize| src.match_indices(c).nth(nth).unwrap().0;
        assert_eq!(syntax.matching_bracket(&rope, at('(', 0)), Some((at('(', 0), at(')', 0))));
        // After a closing bracket, the cursor's left neighbor matches.
        assert_eq!(syntax.matching_bracket(&rope, at(')', 0) + 1), Some((at(')', 0), at('(', 0))));
        assert_eq!(syntax.matching_bracket(&rope, at('[', 0)), Some((at('[', 0), at(']', 0))));
        assert_eq!(syntax.matching_bracket(&rope, at('{', 0)), Some((at('{', 0), at('}', 0))));
        // The "(" inside the string literal isn't a bracket.
        assert_eq!(syntax.matching_bracket(&rope, at('(', 1)), None);
        assert_eq!(syntax.matching_bracket(&rope, at('l', 0)), None);
    }

    #[test]
    fn detects_languages_by_name_and_extension() {
        for (path, language) in [
            ("a.ts", Language::TypeScript),
            ("a.d.ts", Language::TypeScript),
            ("a.mts", Language::TypeScript),
            ("App.tsx", Language::Tsx),
            ("style.css", Language::Css),
            ("run.sh", Language::Shell),
            (".zshrc", Language::Shell),
            ("package.json", Language::Json),
            ("tsconfig.jsonc", Language::Json),
            ("ci.yml", Language::Yaml),
            ("Cargo.toml", Language::Toml),
            ("Cargo.lock", Language::Toml),
            ("yarn.lock", Language::PlainText),
        ] {
            assert_eq!(Language::from_path(Path::new(path)), language, "{path}");
        }
    }

    #[test]
    fn new_languages_highlight() {
        let ts = highlights_of("a.ts", "interface Point { x: number }\nconst p: Point = { x: 1 };\n");
        assert!(has(&ts, "interface", Highlight::Keyword), "{ts:?}");
        assert!(has(&ts, "number", Highlight::Type), "{ts:?}");
        assert!(has(&ts, "const", Highlight::Keyword), "{ts:?}");
        let tsx = highlights_of("a.tsx", "const a = <div className=\"x\" />;\n");
        assert!(has(&tsx, "div", Highlight::Tag), "{tsx:?}");
        assert!(has(&tsx, "className", Highlight::Attribute), "{tsx:?}");
        let css = highlights_of("a.css", "@media screen { .a { color: red; } }\n");
        assert!(has(&css, "@media", Highlight::Keyword), "{css:?}");
        assert!(has(&css, "color", Highlight::Property), "{css:?}");
        let json = highlights_of("a.json", "{\"name\": \"kuana\", \"n\": 1, \"ok\": true}\n");
        assert!(has(&json, "\"name\"", Highlight::Property), "{json:?}");
        assert!(has(&json, "\"kuana\"", Highlight::String), "{json:?}");
        assert!(has(&json, "true", Highlight::Constant), "{json:?}");
        let yaml = highlights_of("a.yaml", "name: kuana\non: true\n");
        assert!(has(&yaml, "name", Highlight::Property), "{yaml:?}");
        assert!(has(&yaml, "kuana", Highlight::String), "{yaml:?}");
        let toml = highlights_of("a.toml", "[package]\nname = \"kuana\"\n");
        assert!(has(&toml, "\"kuana\"", Highlight::String), "{toml:?}");
        let sh = highlights_of("a.sh", "# hi\nif [ -n \"$X\" ]; then echo ok; fi\n");
        assert!(has(&sh, "# hi", Highlight::Comment), "{sh:?}");
        assert!(has(&sh, "if", Highlight::Keyword), "{sh:?}");
    }

    #[test]
    fn rust_text_objects() {
        let src = "impl P {\n    fn new() -> Self {\n        let a = 1;\n‸        P { x: a }\n    }\n}\n";
        assert_eq!(object("a.rs", src, "function", false).as_deref(), Some("let a = 1;\n        P { x: a }"));
        assert_eq!(object("a.rs", src, "function", true).as_deref(), Some("fn new() -> Self {\n        let a = 1;\n        P { x: a }\n    }"));
        assert_eq!(object("a.rs", src, "class", false).as_deref(), Some("fn new() -> Self {\n        let a = 1;\n        P { x: a }\n    }"));
        assert!(object("a.rs", src, "class", true).is_some_and(|s| s.starts_with("impl P {") && s.ends_with('}')));
        // From the signature too; a closure inside is its own function.
        let src = "fn f‸() { let g = |x| x + 1; }\n";
        assert_eq!(object("a.rs", src, "function", false).as_deref(), Some("let g = |x| x + 1;"));
        let src = "fn f() { let g = |x| x ‸+ 1; }\n";
        assert_eq!(object("a.rs", src, "function", false).as_deref(), Some("x + 1"));
        assert_eq!(object("a.rs", src, "function", true).as_deref(), Some("|x| x + 1"));
        // An empty body has no inside: the whole closure.
        assert_eq!(object("a.rs", "fn f() { let g = || {‸}; }\n", "function", false).as_deref(), Some("|| {}"));
        let src = "enum E {\n    A,‸\n    B,\n}\n";
        assert_eq!(object("a.rs", src, "class", false).as_deref(), Some("A,\n    B"));
    }

    #[test]
    fn go_text_objects() {
        let src = "package p\n\ntype P struct {\n\tX ‸int\n}\n\nfunc (p P) Get() int {\n\treturn p.X\n}\n";
        assert_eq!(object("a.go", src, "class", false).as_deref(), Some("X int"));
        assert_eq!(object("a.go", src, "class", true).as_deref(), Some("type P struct {\n\tX int\n}"));
        let src = "package p\n\nfunc (p P) Get() int {\n\tx := 1\n\treturn ‸x\n}\n";
        assert_eq!(object("a.go", src, "function", false).as_deref(), Some("x := 1\n\treturn x"));
        assert_eq!(object("a.go", src, "function", true).as_deref(), Some("func (p P) Get() int {\n\tx := 1\n\treturn x\n}"));
    }

    #[test]
    fn rust_and_go_indent_after_openers() {
        assert!(indents("a.rs", "fn f() {‸\n}\n"));
        assert!(indents("a.rs", "fn f() {\n    foo(a,‸\n        b);\n}\n"));
        assert!(indents("a.rs", "fn f() {\n    match x {\n        Some(y) =>‸\n            y,\n        None => 0,\n    }\n}\n"));
        assert!(indents("a.rs", "fn f<T>()\nwhere‸\n    T: Copy,\n{\n}\n"));
        assert!(!indents("a.rs", "fn f() {\n    let a = 1;‸\n}\n"));
        assert!(!indents("a.rs", "fn f() {}‸\n"));
        assert!(indents("a.go", "package p\n\nfunc f() {‸\n}\n"));
        assert!(indents("a.go", "package p\n\nfunc f() {\n\tswitch x {\n\tcase 1:‸\n\t\tg()\n\t}\n}\n"));
        assert!(indents("a.go", "package p\n\nconst (‸\n\tA = 1\n)\n"));
        assert!(!indents("a.go", "package p\n\nfunc f() {\n\tg()‸\n}\n"));
    }

    #[test]
    fn rust_and_go_pair_quotes_and_angle_brackets() {
        let at = |text: &str, n: usize| (text.to_string(), n);
        assert_eq!(pair("a.rs", "type V = Vec‸<u8>;\n"), Some((at("<", 12), at(">", 15))));
        assert_eq!(pair("a.rs", "const S: &str = ‸\"ab\";\n"), Some((at("\"", 16), at("\"", 19))));
        assert_eq!(pair("a.rs", "const S: &str = \"ab\"‸;\n"), Some((at("\"", 19), at("\"", 16))));
        assert_eq!(pair("a.rs", "fn f() { let g = ‸|a| a; }\n"), Some((at("|", 17), at("|", 19))));
        // Brackets still pair by the grammar's tokens.
        assert_eq!(pair("a.rs", "fn f‸() {}\n"), Some((at("(", 4), at(")", 5))));
        assert_eq!(pair("a.go", "package p\n\nvar s = ‸`a`\n"), Some((at("`", 19), at("`", 21))));
        assert_eq!(pair("a.go", "package p\n\nvar s = ‸\"a\"\n"), Some((at("\"", 19), at("\"", 21))));
    }

    #[test]
    fn javascript_and_typescript_text_objects() {
        let src = "function f(a) {\n  const b = a;\n  return ‸b;\n}\n";
        assert_eq!(object("a.js", src, "function", false).as_deref(), Some("const b = a;\n  return b;"));
        assert_eq!(object("a.js", src, "function", true).as_deref(), Some(src.replace('‸', "").trim_end()));
        let src = "const g = (x) => x ‸* 2;\n";
        assert_eq!(object("a.js", src, "function", false).as_deref(), Some("x * 2"));
        assert_eq!(object("a.js", src, "function", true).as_deref(), Some("(x) => x * 2"));
        assert_eq!(object("a.js", "const h = () => {‸};\n", "function", false).as_deref(), Some("() => {}"));
        let src = "class A {\n  m() {\n    return 1;\n  }\n  ‸n() {}\n}\n";
        assert_eq!(object("a.js", src, "class", false).as_deref(), Some("m() {\n    return 1;\n  }\n  n() {}"));
        assert_eq!(object("a.js", src, "function", true).as_deref(), Some("n() {}"));
        let src = "interface P {\n  x: number;\n  ‸y: string;\n}\n";
        assert_eq!(object("a.ts", src, "class", false).as_deref(), Some("x: number;\n  y: string"));
        assert_eq!(object("a.ts", src, "class", true).as_deref(), Some(src.replace('‸', "").trim_end()));
        let src = "enum E { A, ‸B }\n";
        assert_eq!(object("a.ts", src, "class", false).as_deref(), Some("A, B"));
        let src = "const C = () => <div>‸hi</div>;\n";
        assert_eq!(object("a.tsx", src, "function", false).as_deref(), Some("<div>hi</div>"));
        assert_eq!(object("a.tsx", "class A {\n  ‸x = 1;\n}\n", "class", false).as_deref(), Some("x = 1"));
    }

    #[test]
    fn web_languages_indent_after_openers() {
        assert!(indents("a.js", "function f() {‸\n}\n"));
        assert!(indents("a.js", "foo(a,‸\n  b);\n"));
        assert!(indents("a.js", "switch (x) {\n  case 1:‸\n    g();\n}\n"));
        assert!(indents("a.js", "const f = () =>‸\n  1;\n"));
        assert!(!indents("a.js", "function f() {\n  const a = 1;‸\n}\n"));
        assert!(indents("a.ts", "interface P {‸\n}\n"));
        assert!(indents("a.ts", "type T = Map<‸\n  string, number>;\n"));
        assert!(indents("a.tsx", "const a = (\n  <div>‸\n    hi\n  </div>\n);\n"));
        assert!(indents("a.tsx", "const a = <div‸\n  id=\"x\" />;\n"));
        assert!(indents("a.css", "a {‸\n}\n"));
        assert!(indents("a.css", "a {\n  grid-template-areas:‸\n    \"a b\";\n}\n"));
        assert!(!indents("a.css", "a {\n  color: red;‸\n}\n"));
        assert!(indents("a.json", "{\"a\": [1,‸\n  2]}\n"));
        assert!(!indents("a.json", "{\n  \"a\": 1,‸\n  \"b\": 2\n}\n"));
    }

    #[test]
    fn web_languages_pair_quotes_and_tags() {
        let at = |text: &str, n: usize| (text.to_string(), n);
        assert_eq!(pair("a.js", "const s = ‸'ab';\n"), Some((at("'", 10), at("'", 13))));
        assert_eq!(pair("a.js", "const s = ‸`a${b}`;\n"), Some((at("`", 10), at("`", 16))));
        assert_eq!(pair("a.js", "const s = `a‸${b}`;\n"), Some((at("${", 12), at("}", 15))));
        assert_eq!(pair("a.js", "f‸(a);\n"), Some((at("(", 1), at(")", 3))));
        assert_eq!(pair("a.jsx", "const a = ‸<div></div>;\n"), Some((at("<", 10), at(">", 14))));
        assert_eq!(pair("a.tsx", "const a = <div>‸</div>;\n"), Some((at("</", 15), at(">", 20))));
        assert_eq!(pair("a.ts", "let a: Array‸<number>;\n"), Some((at("<", 12), at(">", 19))));
        assert_eq!(pair("a.css", "a { content: ‸\"x\"; }\n"), Some((at("\"", 13), at("\"", 15))));
        assert_eq!(pair("a.json", "{‸\"a\": 1}\n"), Some((at("\"", 1), at("\"", 3))));
        assert_eq!(pair("a.json", "‸{\"a\": 1}\n"), Some((at("{", 0), at("}", 7))));
    }

    #[test]
    fn python_php_and_shell_text_objects() {
        let src = "class A:\n    @staticmethod\n    def f(x):\n        y = x\n        ‸return y\n";
        assert_eq!(object("a.py", src, "function", false).as_deref(), Some("y = x\n        return y"));
        assert_eq!(object("a.py", src, "function", true).as_deref(), Some("@staticmethod\n    def f(x):\n        y = x\n        return y"));
        assert_eq!(object("a.py", src, "class", false).as_deref(), Some("@staticmethod\n    def f(x):\n        y = x\n        return y"));
        assert_eq!(object("a.py", src, "class", true).as_deref(), Some(src.replace('‸', "").trim_end()));
        assert_eq!(object("a.py", "g = lambda x: x ‸+ 1\n", "function", false).as_deref(), Some("x + 1"));
        let src = "<?php\nclass A {\n    public function f() {\n        $a = 1;\n        ‸return $a;\n    }\n}\n";
        assert_eq!(object("a.php", src, "function", false).as_deref(), Some("$a = 1;\n        return $a;"));
        assert_eq!(object("a.php", src, "function", true).as_deref(), Some("public function f() {\n        $a = 1;\n        return $a;\n    }"));
        assert_eq!(object("a.php", src, "class", false).as_deref(), Some("public function f() {\n        $a = 1;\n        return $a;\n    }"));
        assert_eq!(object("a.php", "<?php\n$g = fn($x) => $x ‸* 2;\n", "function", false).as_deref(), Some("$x * 2"));
        let src = "f() {\n  a\n  ‸b\n}\n";
        assert_eq!(object("a.sh", src, "function", false).as_deref(), Some("a\n  b"));
        assert_eq!(object("a.sh", src, "function", true).as_deref(), Some("f() {\n  a\n  b\n}"));
    }

    #[test]
    fn python_php_and_shell_indent_after_openers() {
        assert!(indents("a.py", "def f():‸\n    pass\n"));
        assert!(indents("a.py", "def f():  # why‸\n    pass\n"));
        assert!(indents("a.py", "if (a and\n        b):‸\n    pass\n"));
        assert!(indents("a.py", "foo(a,‸\n    b)\n"));
        assert!(indents("a.py", "if (a and‸\n        b):\n    pass\n"));
        assert!(!indents("a.py", "if x:\n    pass‸\n"));
        assert!(indents("a.php", "<?php\nfunction f() {‸\n}\n"));
        assert!(indents("a.php", "<?php\nif ($x):‸\n    echo 1;\nendif;\n"));
        assert!(indents("a.php", "<?php\n$a = match ($x) {\n    1 =>‸\n        'a',\n};\n"));
        assert!(!indents("a.php", "<?php\nfunction f() {\n    echo 1;‸\n}\n"));
        assert!(indents("a.sh", "if true; then‸\n  echo\nfi\n"));
        assert!(indents("a.sh", "if true; then\n  a\nelse‸\n  b\nfi\n"));
        assert!(indents("a.sh", "for x in a; do‸\n  echo\ndone\n"));
        assert!(indents("a.sh", "case $x in‸\n  a) echo ;;\nesac\n"));
        assert!(indents("a.sh", "f() {‸\n  echo\n}\n"));
        // `then` and `do` may start the next line, level with `if` and `for`.
        assert!(!indents("a.sh", "if true‸\nthen\n  echo\nfi\n"));
        assert!(!indents("a.sh", "for x in a‸\ndo\n  echo\ndone\n"));
        assert!(!indents("a.sh", "if true; then\n  echo‸\nfi\n"));
    }

    #[test]
    fn python_php_and_shell_pair_words_and_quotes() {
        let at = |text: &str, n: usize| (text.to_string(), n);
        let find = |src: &str, word: &str| (word.to_string(), src.replace('‸', "").find(word).unwrap());
        assert_eq!(pair("a.py", "s = ‸\"ab\"\n"), Some((at("\"", 4), at("\"", 7))));
        assert_eq!(pair("a.py", "s = ‸f'a{b}'\n"), Some((at("f'", 4), at("'", 10))));
        assert_eq!(pair("a.py", "s = f'a‸{b}'\n"), Some((at("{", 7), at("}", 9))));
        let src = "<?php\n‸if ($x):\n    echo 1;\nendif;\n";
        assert_eq!(pair("a.php", src), Some((find(src, "if"), find(src, "endif"))));
        let src = "<?php\nforeach ($a as $b):\n    echo 1;\n‸endforeach;\n";
        assert_eq!(pair("a.php", src), Some((find(src, "endforeach"), find(src, "foreach"))));
        assert_eq!(pair("a.php", "<?php\n$s = ‸'ab';\n"), Some((at("'", 11), at("'", 14))));
        assert_eq!(pair("a.php", "<?php\n$s = ‸\"a$b\";\n"), Some((at("\"", 11), at("\"", 15))));
        let src = "for x in a; ‸do echo; done\n";
        assert_eq!(pair("a.sh", src), Some((find(src, "do"), find(src, "done"))));
        let src = "if true; then :; ‸fi\n";
        assert_eq!(pair("a.sh", src), Some((find(src, "fi"), find(src, "if"))));
        let src = "‸case $x in a) ;; esac\n";
        assert_eq!(pair("a.sh", src), Some((find(src, "case"), find(src, "esac"))));
        assert_eq!(pair("a.sh", "echo ‸\"$x\"\n"), Some((at("\"", 5), at("\"", 8))));
        assert_eq!(pair("a.sh", "echo ‸${x}\n"), Some((at("${", 5), at("}", 8))));
        assert_eq!(pair("a.sh", "echo ‸$(ls)\n"), Some((at("$(", 5), at(")", 9))));
        assert_eq!(pair("a.sh", "‸[[ -n $x ]]\n"), Some((at("[[", 0), at("]]", 9))));
    }

    #[test]
    fn html_pairs_tags_and_quotes() {
        let at = |text: &str, n: usize| (text.to_string(), n);
        assert_eq!(pair("a.html", "‸<p class=\"a\">x</p>\n"), Some((at("<", 0), at(">", 12))));
        assert_eq!(pair("a.html", "<p class=\"a\">x‸</p>\n"), Some((at("</", 14), at(">", 17))));
        assert_eq!(pair("a.html", "<p class=‸\"a\">x</p>\n"), Some((at("\"", 9), at("\"", 11))));
        assert_eq!(pair("a.html", "‸<br/>\n"), Some((at("<", 0), at("/>", 3))));
    }

    /// `src` without its `‸`, and the offset the `‸` marks.
    fn caret(src: &str) -> (String, usize) {
        (src.replacen('‸', "", 1), src.find('‸').expect("a ‸ in the source"))
    }

    fn parsed(path: &str, src: &str) -> (String, usize, Rope, Syntax) {
        let (src, at) = caret(src);
        let rope = Rope::from_str(&src);
        let syntax = Syntax::new(Language::from_path(Path::new(path)), &rope);
        (src, at, rope, syntax)
    }

    /// The text of the `object` text object at the caret.
    fn object(path: &str, src: &str, object: &str, around: bool) -> Option<String> {
        let (src, at, rope, syntax) = parsed(path, src);
        syntax.text_object(&rope, at, object, around).map(|r| src[r].to_string())
    }

    /// Whether `indents.scm` indents the line after a line break at the caret.
    fn indents(path: &str, src: &str) -> bool {
        let (_, at, rope, syntax) = parsed(path, src);
        syntax.indent_after(&rope, at, rope.byte_to_line(at)).expect("an indents.scm")
    }

    /// The bracket pair at the caret: both ends' text and offset.
    fn pair(path: &str, src: &str) -> Option<((String, usize), (String, usize))> {
        let (src, at, rope, syntax) = parsed(path, src);
        let (a, b) = syntax.bracket_pair(&rope, at)?;
        Some(((src[a.clone()].to_string(), a.start), (src[b.clone()].to_string(), b.start)))
    }
}
