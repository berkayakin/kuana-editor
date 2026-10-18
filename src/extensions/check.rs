//! `kuana extension check <folder>`: what Kuana can verify about an extension
//! without running it. The manifest and each language's config.toml (keys
//! Kuana doesn't know), each grammar (fetched and built as Kuana would), and
//! every query against its grammar: that it compiles, and has the captures
//! Kuana looks for.

use std::path::Path;

use toml::{Table, Value};
use tree_sitter::{Language as TsLanguage, Query};

use super::{OutlineConfig, build, needs_build, read_extension, read_toml};
use crate::syntax::Highlight;

/// The extension format version this Kuana reads (Zed's `schema_version`).
pub const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    /// Fine, but worth knowing (a Zed key Kuana doesn't use).
    Note,
}

#[derive(Debug)]
pub struct Finding {
    pub level: Level,
    pub message: String,
}

const MANIFEST_KEYS: &[&str] =
    &["id", "name", "version", "schema_version", "description", "authors", "repository", "grammars", "language_servers", "debug_adapters"];
/// Zed's, for things Kuana doesn't run (code, AI features).
const ZED_MANIFEST_KEYS: &[&str] =
    &["lib", "slash_commands", "context_servers", "indexed_docs_providers", "snippets", "capabilities", "agent_servers", "themes", "icon_themes", "languages"];
const LANGUAGE_KEYS: &[&str] = &[
    "name", "grammar", "path_suffixes", "line_comments", "line_comment", "block_comment", "tab_size", "hard_tabs", "formatter", "language_id",
    "brackets", "decrease_indent_pattern", "decrease_indent_patterns",
];
const ZED_LANGUAGE_KEYS: &[&str] = &[
    "autoclose_before", "word_characters", "completion_query_characters", "linked_edit_characters", "code_fence_block_name",
    "first_line_pattern", "prettier_parser_name", "prettier_plugins", "scope_opt_in_language_servers", "increase_indent_pattern",
    "collapsed_placeholder", "documentation", "documentation_comment",
    "wrap_characters", "jsx_tag_auto_close", "hidden", "auto_indent_using_last_non_empty_line", "auto_indent_on_paste", "overrides",
    "soft_wrap", "ignored_import_segments", "import_path_strip_regex", "debuggers", "rewrap_prefixes", "unordered_list", "ordered_list",
    "task_list", "libraries",
];

/// Captures with no color by design (spell-checking regions, predicates' helpers).
fn uncolored(capture: &str) -> bool {
    capture.starts_with('_') || matches!(capture, "spell" | "nospell" | "none" | "conceal")
}

pub fn check(dir: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut add = |level: Level, message: String| out.push(Finding { level, message });

    let manifest = match read_toml(&dir.join("extension.toml")) {
        Ok(manifest) => manifest,
        Err(err) => {
            add(Level::Error, err);
            return out;
        }
    };
    // Its code runs for its language servers when it's for API 0.6 or 0.7.
    let runs_code = super::wasm::supported(&manifest, dir);
    let known: Vec<&str> = MANIFEST_KEYS.iter().copied().chain(runs_code.then_some("lib")).collect();
    keys(&manifest, "extension.toml", &known, ZED_MANIFEST_KEYS, &mut add);
    if let Some(version) = manifest.get("lib").and_then(|lib| lib.get("version")).and_then(Value::as_str).filter(|_| !runs_code) {
        let why = if dir.join("extension.wasm").is_file() {
            format!("its code is for extension API {version}, which Kuana doesn't run (0.6 and 0.7 only)")
        } else {
            "there's no extension.wasm (its compiled code)".to_string()
        };
        add(Level::Note, format!("extension.toml: {why}, so its language servers are guessed"));
    }
    match manifest.get("schema_version") {
        Some(Value::Integer(n)) if *n > SCHEMA_VERSION => {
            add(Level::Warning, format!("extension.toml: schema_version {n} is newer than the {SCHEMA_VERSION} Kuana reads"))
        }
        Some(Value::Integer(_)) => {}
        Some(_) => add(Level::Error, "extension.toml: schema_version should be a number".into()),
        None => add(Level::Note, format!("extension.toml: no schema_version (Zed asks for {SCHEMA_VERSION})")),
    }

    let (info, languages, _, grammars) = read_extension(dir);
    for problem in info.problems {
        // Zed starts some debug adapters in extension code, which Kuana doesn't run.
        let level = if problem.ends_with("it needs a command") { Level::Warning } else { Level::Error };
        add(level, problem);
    }
    if runs_code {
        let adapters = manifest.get("debug_adapters").and_then(Value::as_table).into_iter().flatten();
        for (id, _) in adapters.filter(|(_, adapter)| adapter.get("command").is_none()) {
            add(Level::Note, format!("debug adapter {id}: its code starts it, which Kuana doesn't run yet"));
        }
        match super::wasm::check(dir, &info.id) {
            Ok(()) => add(Level::Note, "extension.wasm: its code loads".into()),
            Err(err) => add(Level::Error, format!("extension.wasm: {err}")),
        }
    }
    let mut names: Vec<&String> = grammars.keys().collect();
    names.sort();
    for name in names {
        let grammar = &grammars[name];
        if needs_build(grammar) {
            match build(grammar) {
                Ok(()) => add(Level::Note, format!("grammar {name}: built")),
                Err(err) => add(Level::Error, format!("grammar {name}: {err}")),
            }
        }
    }

    for def in &languages {
        let folder = format!("languages/{}", def.dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()));
        if let Ok(config) = read_toml(&def.dir.join("config.toml")) {
            keys(&config, &format!("{folder}/config.toml"), LANGUAGE_KEYS, ZED_LANGUAGE_KEYS, &mut add);
        }
        let language = match def.tree_sitter_language() {
            Some(Ok(language)) => language,
            Some(Err(err)) => {
                add(Level::Error, format!("{folder}: its grammar doesn't load: {err}"));
                continue;
            }
            None => {
                add(Level::Error, format!("{folder}: its grammar isn't built"));
                continue;
            }
        };
        let file_source = |file: &str| std::fs::read_to_string(def.dir.join(file)).ok();
        queries(&language, &folder, &def.highlights, def.outline.as_deref(), file_source, &mut add);
    }
    out
}

/// Every query a language has, compiled against its grammar.
fn queries(
    language: &TsLanguage,
    folder: &str,
    highlights: &str,
    outline: Option<&str>,
    source: impl Fn(&str) -> Option<String>,
    add: &mut impl FnMut(Level, String),
) {
    let compile = |file: &str, text: &str, add: &mut dyn FnMut(Level, String)| match Query::new(language, text) {
        Ok(query) => Some(query),
        Err(e) => {
            add(Level::Error, format!("{folder}/{file}, line {}: {}", e.row + 1, e.message));
            None
        }
    };
    if highlights.trim().is_empty() {
        add(Level::Warning, format!("{folder}: no highlights.scm (and none from its grammar), so it isn't highlighted"));
    } else if let Some(query) = compile("highlights.scm", highlights, add) {
        let mut unknown: Vec<&str> =
            query.capture_names().iter().copied().filter(|c| !uncolored(c) && Highlight::from_capture(c).base() == Highlight::None).collect();
        unknown.dedup();
        if !unknown.is_empty() {
            let list = unknown.iter().map(|c| format!("@{c}")).collect::<Vec<_>>().join(", ");
            add(Level::Warning, format!("{folder}/highlights.scm: no built-in color for {list} (themes can name them)"));
        }
    }
    if let Some(text) = outline
        && let Err(err) = OutlineConfig::build(language.clone(), text)
    {
        add(Level::Error, format!("{folder}: {err}"));
    }
    // What each query needs to do anything in Kuana.
    let needs: [(&str, &[&[&str]]); 5] = [
        ("injections.scm", &[&["injection.content", "content"]]),
        ("brackets.scm", &[&["open"], &["close"]]),
        ("indents.scm", &[&["indent"]]),
        ("textobjects.scm", &[]),
        ("runnables.scm", &[&["run"]]),
    ];
    for (file, captures) in needs {
        let Some(text) = source(file) else { continue };
        let Some(query) = compile(file, &text, add) else { continue };
        for alternatives in captures {
            if !alternatives.iter().any(|c| query.capture_index_for_name(c).is_some()) {
                add(Level::Warning, format!("{folder}/{file}: no @{} capture, so Kuana doesn't use it", alternatives[0]));
            }
        }
    }
}

fn keys(table: &Table, file: &str, known: &[&str], zed: &[&str], add: &mut impl FnMut(Level, String)) {
    let mut unused = Vec::new();
    for (key, value) in table {
        // Zed's installed manifests list every section, mostly empty.
        let empty = matches!(value, Value::Array(a) if a.is_empty()) || matches!(value, Value::Table(t) if t.is_empty());
        if known.contains(&key.as_str()) || empty {
            continue;
        }
        if zed.contains(&key.as_str()) {
            unused.push(key.as_str());
        } else {
            let hint = crate::theme::closest(key, known.iter().chain(zed).copied()).map(|k| format!(" (did you mean \"{k}\"?)")).unwrap_or_default();
            add(Level::Warning, format!("{file}: unknown key \"{key}\"{hint}"));
        }
    }
    if !unused.is_empty() {
        add(Level::Note, format!("{file}: Kuana doesn't use {}", unused.join(", ")));
    }
}

/// `kuana extension check [folder]`: prints the findings; the exit code is 1
/// if there are errors.
pub fn run(dir: &Path) -> i32 {
    let findings = check(dir);
    let count = |level| findings.iter().filter(|f| f.level == level).count();
    for finding in &findings {
        let label = match finding.level {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Note => "note",
        };
        println!("{label}: {}", finding.message);
    }
    let (errors, warnings) = (count(Level::Error), count(Level::Warning));
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    println!("{}: {}, {}", dir.display(), plural(errors, "error"), plural(warnings, "warning"));
    i32::from(errors > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::tests::{temp_dir, write};

    #[test]
    fn reports_keys_queries_and_captures() {
        let dir = temp_dir("check");
        write(dir.join("extension.toml"), "id = \"x\"\nname = \"X\"\nschema_version = 1\nversoin = \"1\"\nslash_commands = { x = 1 }\nthemes = []\n");
        // Queries for the built-in Rust grammar: no build needed.
        write(dir.join("languages/rust/config.toml"), "name = \"Rust\"\ngrammar = \"rust\"\nautoclose_before = \";\"\ntab_sise = 2\n");
        write(dir.join("languages/rust/highlights.scm"), "(identifier) @variable\n(line_comment) @spell\n(string_literal) @fancy.thing\n");
        write(dir.join("languages/rust/brackets.scm"), "(\"(\" @open)\n");
        write(dir.join("languages/rust/indents.scm"), "(block @indent\n");
        let findings = check(&dir);
        let has = |level: Level, text: &str| findings.iter().any(|f| f.level == level && f.message.contains(text));
        assert!(has(Level::Warning, "unknown key \"versoin\" (did you mean \"version\"?)"), "{findings:?}");
        assert!(has(Level::Note, "extension.toml: Kuana doesn't use slash_commands"), "{findings:?}");
        assert!(has(Level::Note, "config.toml: Kuana doesn't use autoclose_before"), "{findings:?}");
        assert!(has(Level::Warning, "unknown key \"tab_sise\" (did you mean \"tab_size\"?)"), "{findings:?}");
        assert!(has(Level::Warning, "no built-in color for @fancy.thing"), "{findings:?}");
        assert!(!has(Level::Warning, "@spell"), "{findings:?}");
        assert!(has(Level::Warning, "brackets.scm: no @close capture"), "{findings:?}");
        assert!(has(Level::Error, "languages/rust/indents.scm, line 1"), "{findings:?}");
        assert_eq!(run(&dir), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
