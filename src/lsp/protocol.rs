//! The slice of the Language Server Protocol Kuana uses, parsed from
//! `serde_json::Value` by hand (the full `lsp-types` crate is large), plus
//! conversions between LSP positions and Kuana's byte offsets.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use ropey::Rope;
use serde_json::{Value, json};

use crate::lint::{Diagnostic, Severity};

/// How a server counts columns. LSP's default is UTF-16 code units; servers
/// that offer UTF-8 save Kuana the conversion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    Utf8,
    Utf16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn to_json(self) -> Value {
        json!({ "line": self.line, "character": self.character })
    }

    pub fn from_json(value: &Value) -> Option<Position> {
        Some(Position {
            line: value.get("line")?.as_u64()? as u32,
            character: value.get("character")?.as_u64()? as u32,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LspRange {
    pub start: Position,
    pub end: Position,
}

impl LspRange {
    pub fn to_json(self) -> Value {
        json!({ "start": self.start.to_json(), "end": self.end.to_json() })
    }

    pub fn from_json(value: &Value) -> Option<LspRange> {
        Some(LspRange { start: Position::from_json(value.get("start")?)?, end: Position::from_json(value.get("end")?)? })
    }
}

/// The LSP position of a byte offset.
pub fn offset_to_position(rope: &Rope, offset: usize, encoding: Encoding) -> Position {
    let offset = offset.min(rope.len_bytes());
    let line = rope.byte_to_line(offset);
    let line_start = rope.line_to_byte(line);
    let character = match encoding {
        Encoding::Utf8 => offset - line_start,
        Encoding::Utf16 => {
            let (c0, c1) = (rope.byte_to_char(line_start), rope.byte_to_char(offset));
            rope.char_to_utf16_cu(c1) - rope.char_to_utf16_cu(c0)
        }
    };
    Position { line: line as u32, character: character as u32 }
}

/// The byte offset of an LSP position, clamped to the document and to the
/// end of its line. O(log n) however long the line (a minified file's).
pub fn position_to_offset(rope: &Rope, position: Position, encoding: Encoding) -> usize {
    let line = position.line as usize;
    if line >= rope.len_lines() {
        return rope.len_bytes();
    }
    let line_start = rope.line_to_byte(line);
    // The line's last byte, not its last char: ropey's `Chars::last` walks
    // the whole line.
    let line_end = rope.line_to_byte(line + 1);
    let content_len = line_end - line_start - usize::from(line_end > line_start && rope.byte(line_end - 1) == b'\n');
    let col = match encoding {
        Encoding::Utf8 => (position.character as usize).min(content_len),
        Encoding::Utf16 => {
            let c0 = rope.byte_to_char(line_start);
            let target = rope.char_to_utf16_cu(c0) + position.character as usize;
            let end_char = rope.byte_to_char(line_start + content_len);
            let ch = rope.utf16_cu_to_char(target.min(rope.len_utf16_cu())).min(end_char);
            rope.char_to_byte(ch) - line_start
        }
    };
    // Snap to a char boundary in case a UTF-8 column lands mid-character.
    let offset = line_start + col;
    rope.char_to_byte(rope.byte_to_char(offset))
}

pub fn range_to_offsets(rope: &Rope, range: LspRange, encoding: Encoding) -> Range<usize> {
    let start = position_to_offset(rope, range.start, encoding);
    let end = position_to_offset(rope, range.end, encoding);
    start.min(end)..end.max(start)
}

pub fn offsets_to_range(rope: &Rope, range: Range<usize>, encoding: Encoding) -> LspRange {
    LspRange {
        start: offset_to_position(rope, range.start, encoding),
        end: offset_to_position(rope, range.end, encoding),
    }
}

pub fn path_to_uri(path: &Path) -> String {
    url::Url::from_file_path(path).map(String::from).unwrap_or_else(|_| format!("file://{}", path.display()))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

// ---- Edits and locations -----------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct TextEdit {
    pub range: LspRange,
    pub new_text: String,
}

impl TextEdit {
    pub fn from_json(value: &Value) -> Option<TextEdit> {
        // An InsertReplaceEdit has `insert`/`replace` instead of `range`.
        let range = value.get("range").or_else(|| value.get("replace")).or_else(|| value.get("insert"))?;
        Some(TextEdit {
            range: LspRange::from_json(range)?,
            new_text: value.get("newText")?.as_str()?.to_string(),
        })
    }
}

pub fn text_edits(value: Option<&Value>) -> Vec<TextEdit> {
    value.and_then(Value::as_array).map(|a| a.iter().filter_map(TextEdit::from_json).collect()).unwrap_or_default()
}

/// A place in a file, as a server reported it.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub range: LspRange,
}

/// `Location | Location[] | LocationLink[] | null`.
pub fn locations(value: &Value) -> Vec<Location> {
    let one = |v: &Value| -> Option<Location> {
        // A LocationLink points at its target's name, not the whole item.
        let (uri, range) = match v.get("targetUri") {
            Some(uri) => (uri, v.get("targetSelectionRange").or_else(|| v.get("targetRange"))?),
            None => (v.get("uri")?, v.get("range")?),
        };
        Some(Location { path: uri_to_path(uri.as_str()?)?, range: LspRange::from_json(range)? })
    };
    match value {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        Value::Null => Vec::new(),
        v => one(v).into_iter().collect(),
    }
}

/// `workspace/symbol` results: each symbol's name, what it's in (a module,
/// class…) and where. A `WorkspaceSymbol` may name only its file.
pub fn workspace_symbols(value: &Value) -> Vec<(String, Option<String>, Location)> {
    let Some(items) = value.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let location = item.get("location")?;
            let path = uri_to_path(location.get("uri")?.as_str()?)?;
            let range = location.get("range").and_then(LspRange::from_json).unwrap_or_default();
            let container = item.get("containerName").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string);
            Some((item.get("name")?.as_str()?.to_string(), container, Location { path, range }))
        })
        .collect()
}

/// One file's part of a WorkspaceEdit.
#[derive(Clone, Debug)]
pub struct FileEdits {
    pub path: PathBuf,
    /// The document version the edits were computed for (`documentChanges`
    /// carry it): they apply to that text, which may be older than today's.
    pub version: Option<u64>,
    pub edits: Vec<TextEdit>,
}

/// The text edits of a WorkspaceEdit, per file. File creation, renames and
/// deletions are not supported (the client doesn't announce them) and skipped.
pub fn workspace_edit(value: &Value) -> Vec<FileEdits> {
    let mut out: Vec<FileEdits> = Vec::new();
    if let Some(changes) = value.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let (Some(doc), Some(edits)) = (change.get("textDocument"), change.get("edits")) else { continue };
            if let Some(path) = doc.get("uri").and_then(Value::as_str).and_then(uri_to_path) {
                let version = doc.get("version").and_then(Value::as_u64);
                out.push(FileEdits { path, version, edits: text_edits(Some(edits)) });
            }
        }
    } else if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(path) = uri_to_path(uri) {
                out.push(FileEdits { path, version: None, edits: text_edits(Some(edits)) });
            }
        }
    }
    out.retain(|f| !f.edits.is_empty());
    out
}

/// Byte-offset edits for a document, from LSP edits in that document.
/// Several insertions at one position become one, in order, since the
/// editor applies one edit per range.
pub fn resolve_edits(rope: &Rope, edits: &[TextEdit], encoding: Encoding) -> Vec<(Range<usize>, String)> {
    let mut out: Vec<(Range<usize>, String)> =
        edits.iter().map(|e| (range_to_offsets(rope, e.range, encoding), e.new_text.clone())).collect();
    out.sort_by_key(|(r, _)| r.start);
    out.dedup_by(|b, a| {
        let same_insert = a.0 == b.0 && a.0.is_empty();
        if same_insert {
            a.1.push_str(&b.1);
        }
        same_insert
    });
    out
}

// ---- Diagnostics -------------------------------------------------------------------

pub fn diagnostics(value: &Value, rope: &Rope, encoding: Encoding, server: &'static str) -> Vec<Diagnostic> {
    let Some(items) = value.get("diagnostics").and_then(Value::as_array) else { return Vec::new() };
    items
        .iter()
        .filter_map(|d| {
            let range = range_to_offsets(rope, LspRange::from_json(d.get("range")?)?, encoding);
            let severity = match d.get("severity").and_then(Value::as_u64) {
                Some(1) | None => Severity::Error,
                Some(2) => Severity::Warning,
                Some(4) => Severity::Hint,
                _ => Severity::Info,
            };
            let code = match d.get("code") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            let source = d.get("source").and_then(Value::as_str).unwrap_or(server);
            let rule = match code {
                Some(code) => format!("{source}({code})"),
                None => source.to_string(),
            };
            Some(Diagnostic {
                range,
                severity,
                message: d.get("message")?.as_str()?.into(),
                rule: rule.into(),
                unnecessary: d.get("tags").and_then(Value::as_array).is_some_and(|tags| tags.contains(&json!(1))),
            })
        })
        .collect()
}

// ---- Completion --------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    /// The label in lowercase, which orders items that match alike: made
    /// once with the item, not again on every key.
    pub sort_label: String,
    pub kind: Option<&'static str>,
    pub detail: Option<String>,
    pub filter_text: String,
    pub sort_text: String,
    /// The server's pick for the selection when the menu opens.
    pub preselect: bool,
    pub edit: Option<TextEdit>,
    pub insert_text: Option<String>,
    pub is_snippet: bool,
    pub additional_edits: Vec<TextEdit>,
    /// The item as received, for `completionItem/resolve`.
    pub raw: Value,
}

impl CompletionItem {
    pub fn from_json(raw: Value) -> Option<CompletionItem> {
        let label = raw.get("label")?.as_str()?.to_string();
        let str_of = |k: &str| raw.get(k).and_then(Value::as_str).map(str::to_string);
        Some(CompletionItem {
            kind: raw.get("kind").and_then(Value::as_u64).map(completion_kind),
            detail: str_of("detail")
                .or_else(|| raw.get("labelDetails").and_then(|l| l.get("description")).and_then(Value::as_str).map(str::to_string)),
            filter_text: str_of("filterText").unwrap_or_else(|| label.clone()),
            sort_text: str_of("sortText").unwrap_or_else(|| label.clone()),
            preselect: raw.get("preselect").and_then(Value::as_bool).unwrap_or(false),
            edit: raw.get("textEdit").and_then(TextEdit::from_json),
            insert_text: str_of("insertText"),
            is_snippet: raw.get("insertTextFormat").and_then(Value::as_u64) == Some(2),
            additional_edits: text_edits(raw.get("additionalTextEdits")),
            sort_label: label.to_lowercase(),
            label,
            raw,
        })
    }

    /// The text to insert, as written (still in snippet syntax when `is_snippet`).
    pub fn raw_text(&self) -> &str {
        self.edit.as_ref().map(|e| e.new_text.as_str()).or(self.insert_text.as_deref()).unwrap_or(&self.label)
    }
}

/// `CompletionList | CompletionItem[] | null`.
pub fn completion_items(value: Value) -> Vec<CompletionItem> {
    let items = match value {
        Value::Array(items) => items,
        Value::Object(mut list) => match list.remove("items") {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    items.into_iter().filter_map(CompletionItem::from_json).collect()
}

fn completion_kind(kind: u64) -> &'static str {
    match kind {
        2 => "method",
        3 => "function",
        4 => "constructor",
        5 => "field",
        6 => "variable",
        7 => "class",
        8 => "interface",
        9 => "module",
        10 => "property",
        11 => "unit",
        12 => "value",
        13 => "enum",
        14 => "keyword",
        15 => "snippet",
        16 => "color",
        17 => "file",
        18 => "reference",
        19 => "folder",
        20 => "variant",
        21 => "constant",
        22 => "struct",
        23 => "event",
        24 => "operator",
        25 => "type",
        _ => "text",
    }
}

/// Expands snippet syntax to plain text and its tab stops in Tab order:
/// `$1`, `$2`… then `$0` (the end of the text when there's none). A stop used
/// in several places lists each. Plain text has no stops.
pub fn expand_snippet(snippet: &str) -> (String, Vec<Vec<Range<usize>>>) {
    let chars: Vec<char> = snippet.chars().collect();
    let mut out = String::new();
    // (tab stop number, range in `out`)
    let mut found: Vec<(u32, Range<usize>)> = Vec::new();
    expand_into(&chars, &mut 0, &mut out, &mut found, false);
    if found.is_empty() {
        return (out, Vec::new());
    }
    let mut numbers: Vec<u32> = found.iter().map(|(n, _)| *n).filter(|n| *n > 0).collect();
    numbers.sort_unstable();
    numbers.dedup();
    let mut stops: Vec<Vec<Range<usize>>> = numbers
        .into_iter()
        .map(|n| {
            let mut ranges: Vec<_> = found.iter().filter(|(m, _)| *m == n).map(|(_, r)| r.clone()).collect();
            ranges.sort_by_key(|r| r.start);
            ranges
        })
        .collect();
    let end = found.iter().find(|(n, _)| *n == 0).map_or(out.len()..out.len(), |(_, r)| r.clone());
    stops.push(vec![end]);
    (out, stops)
}

fn expand_into(chars: &[char], i: &mut usize, out: &mut String, stops: &mut Vec<(u32, Range<usize>)>, nested: bool) {
    while *i < chars.len() {
        let c = chars[*i];
        match c {
            '\\' if *i + 1 < chars.len() => {
                out.push(chars[*i + 1]);
                *i += 2;
            }
            '}' if nested => return,
            '$' => {
                *i += 1;
                if *i < chars.len() && chars[*i].is_ascii_digit() {
                    let n = read_number(chars, i);
                    let start = out.len();
                    mirror(n, out, stops);
                    stops.push((n, start..out.len()));
                } else if *i < chars.len() && chars[*i] == '{' {
                    *i += 1;
                    if *i < chars.len() && chars[*i].is_ascii_digit() {
                        let n = read_number(chars, i);
                        let start = out.len();
                        match chars.get(*i) {
                            Some(':') => {
                                *i += 1;
                                expand_into(chars, i, out, stops, true);
                            }
                            // A choice: keep the first option.
                            Some('|') => {
                                *i += 1;
                                let mut first = true;
                                while *i < chars.len() && chars[*i] != '|' {
                                    if chars[*i] == ',' {
                                        first = false;
                                    } else if first {
                                        out.push(chars[*i]);
                                    }
                                    *i += 1;
                                }
                                *i += 1;
                            }
                            _ => mirror(n, out, stops),
                        }
                        stops.push((n, start..out.len()));
                    } else {
                        // A variable: `${NAME}` or `${NAME:default}`; Kuana has none, so use the default.
                        while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '_') {
                            *i += 1;
                        }
                        if chars.get(*i) == Some(&':') {
                            *i += 1;
                            expand_into(chars, i, out, stops, true);
                        }
                    }
                    // Skip to the closing brace.
                    while *i < chars.len() && chars[*i] != '}' {
                        *i += 1;
                    }
                    *i += 1;
                } else if *i < chars.len() && (chars[*i].is_alphabetic() || chars[*i] == '_') {
                    while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '_') {
                        *i += 1;
                    }
                } else {
                    out.push('$');
                }
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
}

/// A repeated tab stop starts with the text its first place has (`${1:v} = $1`).
fn mirror(n: u32, out: &mut String, stops: &[(u32, Range<usize>)]) {
    if let Some((_, r)) = stops.iter().find(|(m, r)| *m == n && !r.is_empty()) {
        let text = out[r.clone()].to_string();
        out.push_str(&text);
    }
}

fn read_number(chars: &[char], i: &mut usize) -> u32 {
    let mut n = 0u32;
    while *i < chars.len() && chars[*i].is_ascii_digit() {
        n = n.saturating_mul(10).saturating_add(chars[*i].to_digit(10).unwrap_or(0));
        *i += 1;
    }
    n
}

// ---- Hover and signature help ------------------------------------------------------

/// A paragraph of hover text, or a code block.
#[derive(Clone, Debug, PartialEq)]
pub struct HoverBlock {
    pub code: bool,
    pub text: String,
}

/// `MarkupContent | MarkedString | MarkedString[]` as text blocks.
pub fn hover_blocks(value: &Value) -> Vec<HoverBlock> {
    let contents = value.get("contents").unwrap_or(&Value::Null);
    let mut blocks = Vec::new();
    let mut add = |v: &Value| match v {
        Value::String(s) => blocks.extend(markdown_blocks(s)),
        Value::Object(o) => {
            let text = o.get("value").and_then(Value::as_str).unwrap_or_default();
            if o.contains_key("language") {
                blocks.push(HoverBlock { code: true, text: text.trim_end().to_string() });
            } else if o.get("kind").and_then(Value::as_str) == Some("plaintext") {
                blocks.push(HoverBlock { code: false, text: text.trim().to_string() });
            } else {
                blocks.extend(markdown_blocks(text));
            }
        }
        _ => {}
    };
    match contents {
        Value::Array(items) => items.iter().for_each(&mut add),
        v => add(v),
    }
    blocks.retain(|b| !b.text.trim().is_empty());
    blocks
}

/// Splits Markdown into code fences and prose, with light inline cleanup.
pub fn markdown_blocks(markdown: &str) -> Vec<HoverBlock> {
    let mut blocks = Vec::new();
    let mut prose = String::new();
    let mut code: Option<String> = None;
    let flush = |prose: &mut String, blocks: &mut Vec<HoverBlock>| {
        let text = prose.trim();
        if !text.is_empty() {
            blocks.push(HoverBlock { code: false, text: clean_inline_markdown(text) });
        }
        prose.clear();
    };
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            match code.take() {
                Some(text) => blocks.push(HoverBlock { code: true, text: text.trim_end().to_string() }),
                None => {
                    flush(&mut prose, &mut blocks);
                    code = Some(String::new());
                }
            }
            continue;
        }
        match code.as_mut() {
            Some(text) => {
                text.push_str(line);
                text.push('\n');
            }
            None if trimmed == "---" || trimmed == "***" => flush(&mut prose, &mut blocks),
            None => {
                prose.push_str(line);
                prose.push('\n');
            }
        }
    }
    if let Some(text) = code {
        blocks.push(HoverBlock { code: true, text: text.trim_end().to_string() });
    }
    flush(&mut prose, &mut blocks);
    blocks
}

/// Drops Markdown markup that would show as noise in plain text.
fn clean_inline_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '`' => {}
            '\\' if chars.peek().is_some_and(|n| n.is_ascii_punctuation()) => out.push(chars.next().unwrap()),
            // [text](url) → text
            '[' => {
                let rest: String = chars.clone().collect();
                if let Some(close) = rest.find("](")
                    && let Some(end) = rest[close..].find(')')
                {
                    out.extend(rest[..close].chars().filter(|c| !matches!(c, '`' | '*')));
                    for _ in 0..rest[..close + end + 1].chars().count() {
                        chars.next();
                    }
                } else {
                    out.push('[');
                }
            }
            '#' if out.is_empty() || out.ends_with('\n') => {
                while chars.peek() == Some(&'#') {
                    chars.next();
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// The active signature's label and the byte range of its active parameter.
pub fn signature(value: &Value) -> Option<(String, Option<Range<usize>>)> {
    let signatures = value.get("signatures")?.as_array()?;
    let active = value.get("activeSignature").and_then(Value::as_u64).unwrap_or(0) as usize;
    let sig = signatures.get(active).or(signatures.first())?;
    let label = sig.get("label")?.as_str()?.to_string();
    let param_ix = sig
        .get("activeParameter")
        .or_else(|| value.get("activeParameter"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let param = sig.get("parameters").and_then(Value::as_array).and_then(|p| p.get(param_ix));
    let range = param.and_then(|p| match p.get("label")? {
        Value::String(name) => label.find(name.as_str()).map(|s| s..s + name.len()),
        // [start, end] in UTF-16 code units of the label.
        Value::Array(bounds) => {
            let to_byte = |u16_ix: u64| -> usize {
                let mut units = 0;
                for (b, ch) in label.char_indices() {
                    if units >= u16_ix as usize {
                        return b;
                    }
                    units += ch.len_utf16();
                }
                label.len()
            };
            Some(to_byte(bounds.first()?.as_u64()?)..to_byte(bounds.get(1)?.as_u64()?))
        }
        _ => None,
    });
    Some((label, range))
}

// ---- Code actions ------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CodeAction {
    pub title: String,
    pub preferred: bool,
    /// The action as received: edit, command, or data to resolve.
    pub raw: Value,
}

pub fn code_actions(value: &Value) -> Vec<CodeAction> {
    let Some(items) = value.as_array() else { return Vec::new() };
    let mut actions: Vec<CodeAction> = items
        .iter()
        .filter(|a| a.get("disabled").is_none())
        .filter_map(|a| {
            Some(CodeAction {
                title: a.get("title")?.as_str()?.to_string(),
                preferred: a.get("isPreferred").and_then(Value::as_bool).unwrap_or(false),
                raw: a.clone(),
            })
        })
        .collect();
    // Preferred fixes first, like VS Code's lightbulb.
    actions.sort_by_key(|a| !a.preferred);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/rest` as a path and a file URI; on Windows under C:, since a URI
    /// without a drive isn't a Windows path.
    fn file(rest: &str) -> (PathBuf, String) {
        let drive = if cfg!(windows) { "C:" } else { "" };
        (PathBuf::from(format!("{drive}{rest}")), format!("file://{}{drive}{rest}", if cfg!(windows) { "/" } else { "" }))
    }

    #[test]
    fn workspace_symbols_take_both_shapes() {
        let (lib, lib_uri) = file("/p/src/lib.rs");
        let value = json!([
            { "name": "parse", "kind": 12, "containerName": "config",
              "location": { "uri": file("/p/src/config.rs").1, "range": { "start": { "line": 4, "character": 7 }, "end": { "line": 4, "character": 12 } } } },
            // WorkspaceSymbol may point at a file only.
            { "name": "Config", "kind": 23, "location": { "uri": lib_uri } },
        ]);
        let symbols = workspace_symbols(&value);
        assert_eq!(symbols.len(), 2);
        assert_eq!((symbols[0].0.as_str(), symbols[0].1.as_deref()), ("parse", Some("config")));
        assert_eq!(symbols[0].2.range.start, Position { line: 4, character: 7 });
        assert_eq!(symbols[1].2.path, lib);
        assert_eq!(symbols[1].2.range, LspRange::default());
    }

    #[test]
    fn converts_positions() {
        let rope = Rope::from_str("aé😀b\nxyz\n");
        // "a"=1 byte, "é"=2 bytes/1 unit, "😀"=4 bytes/2 units.
        let b_offset = 1 + 2 + 4;
        assert_eq!(offset_to_position(&rope, b_offset, Encoding::Utf16), Position { line: 0, character: 4 });
        assert_eq!(offset_to_position(&rope, b_offset, Encoding::Utf8), Position { line: 0, character: 7 });
        assert_eq!(position_to_offset(&rope, Position { line: 0, character: 4 }, Encoding::Utf16), b_offset);
        assert_eq!(position_to_offset(&rope, Position { line: 0, character: 7 }, Encoding::Utf8), b_offset);
        // Past the end of a line clamps to it; past the last line, to the end.
        assert_eq!(position_to_offset(&rope, Position { line: 1, character: 99 }, Encoding::Utf16), b_offset + 1 + 1 + 3);
        assert_eq!(position_to_offset(&rope, Position { line: 9, character: 0 }, Encoding::Utf16), rope.len_bytes());
        // A UTF-8 column inside a character snaps back.
        assert_eq!(position_to_offset(&rope, Position { line: 0, character: 2 }, Encoding::Utf8), 1);
        // The last line, with no line break or no text.
        let rope = Rope::from_str("ab\ncd");
        assert_eq!(position_to_offset(&rope, Position { line: 1, character: 9 }, Encoding::Utf16), 5);
        assert_eq!(position_to_offset(&Rope::from_str("ab\n"), Position { line: 1, character: 9 }, Encoding::Utf8), 3);
    }

    #[test]
    fn reads_diagnostic_severities() {
        let rope = Rope::from_str("let x = 1;\n");
        let problem = |severity: Option<u64>| {
            let range = json!({ "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 5 } });
            let mut problem = json!({ "range": range, "message": "m" });
            if let Some(severity) = severity {
                problem["severity"] = json!(severity);
            }
            problem
        };
        let params = json!({ "diagnostics": [problem(Some(1)), problem(Some(2)), problem(Some(3)), problem(Some(4)), problem(None)] });
        let severities: Vec<Severity> = diagnostics(&params, &rope, Encoding::Utf16, "s").iter().map(|d| d.severity).collect();
        assert_eq!(severities, [Severity::Error, Severity::Warning, Severity::Info, Severity::Hint, Severity::Error]);
        // Information and hints are drawn, but aren't problems.
        assert!(Severity::Warning.is_problem() && !Severity::Info.is_problem() && !Severity::Hint.is_problem());
        // Unused code comes tagged `Unnecessary` (1); `Deprecated` (2) isn't that.
        let mut unused = problem(Some(4));
        unused["tags"] = json!([1]);
        let mut deprecated = problem(Some(4));
        deprecated["tags"] = json!([2]);
        let params = json!({ "diagnostics": [unused, deprecated, problem(Some(4))] });
        let tagged: Vec<bool> = diagnostics(&params, &rope, Encoding::Utf16, "s").iter().map(|d| d.unnecessary).collect();
        assert_eq!(tagged, [true, false, false]);
    }

    #[test]
    fn uris() {
        let (path, plain) = file("/tmp/a b/é.rs");
        let uri = path_to_uri(&path);
        assert_eq!(uri, plain.replace(' ', "%20").replace('é', "%C3%A9"));
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[test]
    fn expands_snippets() {
        assert_eq!(expand_snippet("println!(\"$1\")$0"), ("println!(\"\")".to_string(), vec![vec![10..10], vec![12..12]]));
        assert_eq!(expand_snippet("foo(${1:a}, ${2:b})"), ("foo(a, b)".to_string(), vec![vec![4..5], vec![7..8], vec![9..9]]));
        assert_eq!(expand_snippet("${1|one,two|} \\$x ${TM_FILENAME:file}$0"), ("one $x file".to_string(), vec![vec![0..3], vec![11..11]]));
        assert_eq!(expand_snippet("plain"), ("plain".to_string(), Vec::new()));
        assert_eq!(expand_snippet("a${1:b${2:c}}d"), ("abcd".to_string(), vec![vec![1..3], vec![2..3], vec![4..4]]));
        // Mirrors: both places of $1, in order.
        assert_eq!(expand_snippet("${1:x} = $1;"), ("x = x;".to_string(), vec![vec![0..1, 4..5], vec![6..6]]));
    }

    #[test]
    fn parses_locations_and_edits() {
        let (a, a_uri) = file("/a.rs");
        let link = json!([{ "targetUri": a_uri, "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 1}},
            "targetSelectionRange": {"start": {"line": 1, "character": 3}, "end": {"line": 1, "character": 6}} }]);
        let locs = locations(&link);
        assert_eq!(locs[0].path, a);
        assert_eq!(locs[0].range.start, Position { line: 1, character: 3 });
        assert!(locations(&Value::Null).is_empty());

        let edit = json!({ "changes": { a_uri.clone(): [{ "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "x" }] } });
        let files = workspace_edit(&edit);
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].edits[0].new_text.as_str(), files[0].version), ("x", None));
        // `documentChanges` say which version of the file they're for.
        let edit = json!({ "documentChanges": [{ "textDocument": { "uri": a_uri, "version": 7 },
            "edits": [{ "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "y" }] }] });
        assert_eq!(workspace_edit(&edit)[0].version, Some(7));
    }

    #[test]
    fn merges_inserts_at_one_position() {
        let rope = Rope::from_str("ab");
        let at = |c| LspRange { start: Position { line: 0, character: c }, end: Position { line: 0, character: c } };
        let edits = [
            TextEdit { range: at(1), new_text: "x".into() },
            TextEdit { range: at(1), new_text: "y".into() },
            TextEdit { range: at(0), new_text: "z".into() },
        ];
        assert_eq!(resolve_edits(&rope, &edits, Encoding::Utf8), [(0..0, "z".to_string()), (1..1, "xy".to_string())]);
    }

    #[test]
    fn parses_hover_and_signatures() {
        let hover = json!({ "contents": { "kind": "markdown", "value": "```rust\nfn f(a: u8)\n```\n---\nDoes **things** with [`a`](https://x)." } });
        assert_eq!(
            hover_blocks(&hover),
            [
                HoverBlock { code: true, text: "fn f(a: u8)".into() },
                HoverBlock { code: false, text: "Does things with a.".into() }
            ]
        );
        let help = json!({ "signatures": [{ "label": "f(a: u8, b: u8)", "parameters": [{ "label": [2, 7] }, { "label": "b: u8" }] }], "activeParameter": 1 });
        let (label, range) = signature(&help).unwrap();
        assert_eq!(&label[range.unwrap()], "b: u8");
    }

    #[test]
    fn parses_completions() {
        let items = completion_items(json!({ "isIncomplete": false, "items": [
            { "label": "push", "kind": 2, "detail": "fn(&mut self, T)", "insertTextFormat": 2, "preselect": true,
              "textEdit": { "range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 4}}, "newText": "push(${1:value})$0" } },
            { "label": "pop" }
        ] }));
        assert_eq!(items[0].kind, Some("method"));
        assert_eq!((items[0].preselect, items[1].preselect), (true, false));
        assert_eq!(items[0].raw_text(), "push(${1:value})$0");
        assert_eq!(expand_snippet(items[0].raw_text()), ("push(value)".to_string(), vec![vec![5..10], vec![11..11]]));
    }
}
