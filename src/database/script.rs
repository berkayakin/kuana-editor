//! A SQL script as Oracle's tools read it (SQL*Plus, SQL Developer): statements
//! end at a `;` outside strings and comments, PL/SQL units (`BEGIN`, `DECLARE`,
//! `CREATE … PACKAGE`, …) at a line holding only `/`, which also ends any
//! statement. SQL*Plus commands (`SET SERVEROUTPUT ON`, `PROMPT`, …) are one
//! line each and aren't sent to the database; `EXEC p` is `BEGIN p; END;`.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// SQL: sent without its `;`.
    Sql,
    /// A PL/SQL block or unit: sent with its last `;` (`END;`).
    PlSql,
    /// A SQL*Plus command, skipped.
    SqlPlus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    /// The statement's bytes in the script (without the `;` or `/` that
    /// ends it, for SQL).
    pub range: Range<usize>,
    pub kind: Kind,
}

impl Statement {
    /// What's sent to the database.
    pub fn sql(&self, script: &str) -> String {
        let text = script[self.range.clone()].trim();
        // `EXECUTE IMMEDIATE` is PL/SQL's, not a SQL*Plus EXEC.
        let exec = starts_with_word(text, "exec") || (starts_with_word(text, "execute") && !first_words(text, 2).iter().any(|w| w == "immediate"));
        if self.kind == Kind::Sql && exec {
            let call = text.split_once(char::is_whitespace).map_or("", |(_, rest)| rest).trim().trim_end_matches(';');
            return format!("BEGIN {call}; END;");
        }
        text.to_string()
    }
}

/// SQL*Plus commands: run by the client, not the database.
const SQLPLUS: &[&str] = &[
    "set", "prompt", "spool", "rem", "remark", "whenever", "col", "column", "clear", "ttitle", "btitle", "break", "compute", "define",
    "undefine", "accept", "pause", "host", "show", "conn", "connect", "disconnect", "exit", "quit", "@", "@@", "start", "sho", "desc",
    "describe", "var", "variable", "print", "timing", "repheader", "repfooter", "store", "archive", "recover", "startup", "shutdown",
];

fn starts_with_word(text: &str, word: &str) -> bool {
    text.len() >= word.len()
        && text[..word.len()].eq_ignore_ascii_case(word)
        && text[word.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '$' && c != '#')
}

/// The words a statement starts with, lowercased (comments skipped), up to `n`.
fn first_words(text: &str, n: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = text;
    while words.len() < n {
        rest = skip_trivia(rest);
        let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '#')).unwrap_or(rest.len());
        if end == 0 {
            break;
        }
        words.push(rest[..end].to_ascii_lowercase());
        rest = &rest[end..];
    }
    words
}

/// `text` after its leading whitespace and comments.
fn skip_trivia(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        if let Some(rest) = trimmed.strip_prefix("--") {
            text = rest.find('\n').map_or("", |i| &rest[i..]);
        } else if let Some(rest) = trimmed.strip_prefix("/*") {
            text = rest.find("*/").map_or("", |i| &rest[i + 2..]);
        } else {
            return trimmed;
        }
    }
}

/// Whether a statement starting with `text` is PL/SQL, which only a `/`
/// line ends (its body holds `;`s).
fn is_plsql(text: &str) -> bool {
    let words = first_words(text, 8);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    match w.first() {
        Some(&"begin") | Some(&"declare") => true,
        Some(&"create") => {
            let mut rest = &w[1..];
            if rest.first() == Some(&"or") && rest.get(1) == Some(&"replace") {
                rest = &rest[2..];
            }
            while matches!(rest.first(), Some(&"editionable") | Some(&"noneditionable") | Some(&"editioning") | Some(&"force") | Some(&"nonforce")) {
                rest = &rest[1..];
            }
            matches!(rest.first(), Some(&"package") | Some(&"procedure") | Some(&"function") | Some(&"trigger") | Some(&"type") | Some(&"library") | Some(&"java"))
                // CREATE TYPE … AS OBJECT is ended by `/` too; only a forward
                // declaration (`CREATE TYPE t;`) isn't, which `;` handles below.
        }
        _ => false,
    }
}

/// The script's statements, in order.
pub fn split(script: &str) -> Vec<Statement> {
    let bytes = script.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // The next statement starts after whitespace, comments and stray `/` lines.
        let rest = skip_trivia(&script[i..]);
        let start = script.len() - rest.len();
        if start >= script.len() {
            break;
        }
        if slash_line(script, start).is_some() {
            i = line_end(script, start);
            continue;
        }
        // SQL*Plus commands are one line.
        let words = first_words(rest, 1);
        let first = words.first().map(String::as_str).unwrap_or("");
        if rest.starts_with('@') || SQLPLUS.contains(&first) {
            let end = line_end(script, start);
            out.push(Statement { range: start..script[start..end].trim_end().len() + start, kind: Kind::SqlPlus });
            i = end;
            continue;
        }
        let plsql = is_plsql(rest);
        let (end, next) = scan(script, start, plsql);
        let mut range = start..end;
        // Trailing whitespace isn't part of it.
        range.end = start + script[range.clone()].trim_end().len();
        if !range.is_empty() {
            out.push(Statement { range, kind: if plsql { Kind::PlSql } else { Kind::Sql } });
        }
        i = next;
    }
    out
}

/// Whether the line `at` starts on holds only `/` (spaces around it allowed):
/// its end if so.
fn slash_line(script: &str, at: usize) -> Option<usize> {
    let line_start = script[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = line_end(script, at);
    (script[line_start..end].trim() == "/").then_some(end)
}

fn line_end(script: &str, at: usize) -> usize {
    script[at..].find('\n').map_or(script.len(), |i| at + i + 1)
}

/// From `start`, where the statement ends and where the next one may start.
fn scan(script: &str, start: usize, plsql: bool) -> (usize, usize) {
    let bytes = script.as_bytes();
    let mut i = start;
    let mut at_line_start = false;
    while i < bytes.len() {
        let b = bytes[i];
        if at_line_start {
            let line_end = line_end(script, i);
            if script[i..line_end].trim() == "/" {
                return (i, line_end);
            }
        }
        at_line_start = false;
        match b {
            b'\n' => {
                at_line_start = true;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = script[i..].find('\n').map_or(bytes.len(), |n| i + n),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = script[i + 2..].find("*/").map_or(bytes.len(), |n| i + 2 + n + 2),
            b'\'' => i = skip_string(script, i),
            b'"' => i = script[i + 1..].find('"').map_or(bytes.len(), |n| i + 1 + n + 1),
            b';' if !plsql => return (i, i + 1),
            _ => i += 1,
        }
    }
    (bytes.len(), bytes.len())
}

/// Past a string literal starting at `at` (a `'`): `''` escapes, and Oracle's
/// q-quotes (`q'[…]'`, `nq'{…}'`) when the `'` follows a `q`.
fn skip_string(script: &str, at: usize) -> usize {
    let bytes = script.as_bytes();
    let q = at > 0
        && matches!(bytes[at - 1], b'q' | b'Q')
        && (at < 2 || !(bytes[at - 2].is_ascii_alphanumeric() || bytes[at - 2] == b'_') || matches!(bytes[at - 2], b'n' | b'N'));
    if q && let Some(&open) = bytes.get(at + 1) {
        let close = match open {
            b'[' => b']',
            b'{' => b'}',
            b'(' => b')',
            b'<' => b'>',
            other => other,
        };
        let mut i = at + 2;
        while i + 1 < bytes.len() {
            if bytes[i] == close && bytes[i + 1] == b'\'' {
                return i + 2;
            }
            i += 1;
        }
        return bytes.len();
    }
    let mut i = at + 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// Where each statement sent to the database starts (its first word, past
/// comments): the gutter's ▶ goes on that line, and `at` finds it again.
pub fn starts(script: &str) -> Vec<usize> {
    split(script)
        .into_iter()
        .filter(|s| s.kind != Kind::SqlPlus)
        .map(|s| {
            let text = &script[s.range.clone()];
            s.range.start + text.len() - skip_trivia(text).len()
        })
        .collect()
}

/// The statement to run for a caret at `offset`: the one it's in, else the
/// one ending before it on its line or above (blank lines and comments
/// between), else the next one.
pub fn at(statements: &[Statement], script: &str, offset: usize) -> Option<usize> {
    if let Some(ix) = statements.iter().position(|s| s.range.start <= offset && offset <= s.range.end + 1) {
        return Some(ix);
    }
    let before = statements.iter().rposition(|s| s.range.end <= offset);
    if let Some(ix) = before {
        // Only comments, `;`, `/` and whitespace between it and the caret.
        let gap = &script[statements[ix].range.end..offset];
        if gap.trim_start_matches([';', ' ', '\t', '\r']).split('\n').count() <= 2 {
            return Some(ix);
        }
    }
    statements.iter().position(|s| s.range.start >= offset).or(before)
}

/// An Oracle error's offset (characters into the statement) as a byte offset
/// into the script.
pub fn error_offset(script: &str, statement: &Statement, chars: usize) -> usize {
    let text = &script[statement.range.clone()];
    let lead = text.len() - text.trim_start().len();
    let body = &text[lead..];
    statement.range.start + lead + body.char_indices().nth(chars).map_or(body.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqls(script: &str) -> Vec<(String, Kind)> {
        split(script).into_iter().map(|s| (s.sql(script), s.kind)).collect()
    }

    #[test]
    fn splits_sql_on_semicolons() {
        let script = "select 1 from dual;\n\n-- a comment; with a semicolon\nupdate t set a = 'x;y' where b = \"c;d\";\ndelete from t";
        assert_eq!(
            sqls(script),
            vec![
                ("select 1 from dual".into(), Kind::Sql),
                ("update t set a = 'x;y' where b = \"c;d\"".into(), Kind::Sql),
                ("delete from t".into(), Kind::Sql),
            ]
        );
    }

    #[test]
    fn starts_are_first_words_past_comments() {
        let script = "set serveroutput on\n-- the users\nselect 1 from dual;\n\n/* all */ select 2\nfrom dual;\nbegin\n  null;\nend;\n/\n";
        let found = starts(script);
        let words: Vec<&str> = found.iter().map(|&at| &script[at..at + 6]).collect();
        assert_eq!(words, ["select", "select", "begin\n"]);
        // Each finds its own statement again (what its ▶ runs).
        let statements = split(script);
        for &start in &found {
            let ix = at(&statements, script, start).unwrap();
            assert!(statements[ix].range.contains(&start));
        }
    }

    #[test]
    fn plsql_ends_at_a_slash_line() {
        let script = "create or replace package body p as\n  procedure a is begin null; end;\nend p;\n/\nbegin\n  dbms_output.put_line('a;b');\nend;\n/\nselect 2 from dual;";
        let s = sqls(script);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], ("create or replace package body p as\n  procedure a is begin null; end;\nend p;".into(), Kind::PlSql));
        assert_eq!(s[1].1, Kind::PlSql);
        assert!(s[1].0.ends_with("end;"));
        assert_eq!(s[2], ("select 2 from dual".into(), Kind::Sql));
    }

    #[test]
    fn plsql_without_a_slash_runs_to_the_end() {
        let script = "DECLARE\n  x NUMBER := 1;\nBEGIN\n  x := x + 1;\nEND;";
        assert_eq!(sqls(script), vec![(script.into(), Kind::PlSql)]);
    }

    #[test]
    fn editionable_units_and_types() {
        assert!(is_plsql("CREATE OR REPLACE EDITIONABLE PACKAGE x AS END;"));
        assert!(is_plsql("/* c */ create type t as object (a number);"));
        assert!(!is_plsql("create table t (a number)"));
        assert!(!is_plsql("create or replace view v as select 1 x from dual"));
    }

    #[test]
    fn sqlplus_commands_are_skipped_and_exec_is_a_block() {
        let script = "set serveroutput on\nprompt hello; there\nexec pkg.run(1);\n@other.sql\nselect 1 from dual;";
        let s = sqls(script);
        assert_eq!(s[0].1, Kind::SqlPlus);
        assert_eq!(s[1], ("prompt hello; there".into(), Kind::SqlPlus));
        assert_eq!(s[2], ("BEGIN pkg.run(1); END;".into(), Kind::Sql));
        assert_eq!(s[3].1, Kind::SqlPlus);
        assert_eq!(s[4], ("select 1 from dual".into(), Kind::Sql));
    }

    #[test]
    fn q_quotes() {
        let script = "select q'[it's; fine]' from dual; select nq'{a;b}' x from dual;";
        assert_eq!(sqls(script).len(), 2);
    }

    #[test]
    fn statement_at_the_caret() {
        let script = "select 1 from dual;\n\nselect 2 from dual;\n";
        let s = split(script);
        assert_eq!(at(&s, script, 3), Some(0));
        // Right after the `;`, and on the blank line after it.
        assert_eq!(at(&s, script, 19), Some(0));
        assert_eq!(at(&s, script, 20), Some(0));
        assert_eq!(at(&s, script, 25), Some(1));
        assert_eq!(at(&s, script, script.len()), Some(1));
    }

    #[test]
    fn error_offsets_count_characters() {
        let script = "select 1 from dual;\n  selct 'é' x from dual;";
        let s = split(script);
        assert_eq!(&script[error_offset(script, &s[1], 0)..][..5], "selct");
        assert_eq!(&script[error_offset(script, &s[1], 10)..][..1], "x");
    }
}
