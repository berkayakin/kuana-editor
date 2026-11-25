//! One-shot model requests the editor makes (no agent, no tools): their
//! prompts and how their replies are cleaned up. Pure, so tested here; the
//! workspace sends them and shows the results.
//!
//! - Inline Assist: a selection rewritten (or code written at the caret) as
//!   the user asks, the reply being the new code alone.
//! - Commit messages: from the staged (or all) changes and the repository's
//!   recent subjects, so the style matches.

use std::ops::Range;

use super::message::{Effort, Request};

/// The most of a file Inline Assist sends; past it, lines around the
/// selection.
const FILE_BYTES: usize = 120_000;
/// The most of a diff a commit message request sends.
const DIFF_BYTES: usize = 60_000;

/// Inline Assist's request: `text` is the whole file, `range` the selection
/// (empty: write at the caret), `language` its name.
pub fn inline_assist(path: &str, language: &str, text: &str, range: Range<usize>, instruction: &str) -> Request {
    let range = clamp(text, range);
    let (before, selected, after) = (&text[..range.start], &text[range.clone()], &text[range.end..]);
    let (before, after) = around(before, after, FILE_BYTES.saturating_sub(selected.len()));
    let inserting = range.is_empty();
    let marked = if inserting { format!("{before}<caret/>{after}") } else { format!("{before}<selection>{selected}</selection>{after}") };
    let task = if inserting {
        "Write the code to insert at <caret/>, as the instruction asks."
    } else {
        "Rewrite the code inside <selection>, as the instruction asks."
    };
    let system = format!(
        "You are an expert programmer working in Kuana, a code editor. {task}\n\n\
         Reply with only the code that goes {place}: no explanation, no Markdown code fences, no \
         <selection> or <caret/> tags. Match the file's language, style and indentation; the first line \
         continues where {place_short} starts, so don't indent it again. Keep everything the instruction \
         doesn't ask to change.",
        place = if inserting { "at the caret" } else { "in place of the selection" },
        place_short = if inserting { "the caret" } else { "the selection" },
    );
    let user = format!("<file path=\"{path}\" language=\"{language}\">\n{marked}\n</file>\n\n<instruction>\n{}\n</instruction>", instruction.trim());
    Request::simple("", system, user)
}

/// The code in Inline Assist's reply, fitted to what it replaces: fences and
/// echoed tags removed, and a final line break where the selection had one.
pub fn assist_code(reply: &str, replaced: &str) -> String {
    let mut code = strip_fences(reply);
    for tag in ["<selection>", "</selection>", "<caret/>"] {
        code = code.replace(tag, "");
    }
    let code = code.trim_start_matches(['\n', '\r']);
    let mut code = code.trim_end().to_string();
    // A selection of whole lines keeps its last line break; one inside a
    // line gets none.
    if replaced.ends_with('\n') && !code.is_empty() {
        code.push('\n');
    }
    code
}

/// A partial reply while it streams: what [`assist_code`] would give so far,
/// without the final fitting (the code is still growing).
pub fn assist_code_so_far(reply: &str) -> String {
    let mut code = strip_fences(reply);
    for tag in ["<selection>", "</selection>", "<caret/>"] {
        code = code.replace(tag, "");
    }
    code.trim_start_matches(['\n', '\r']).to_string()
}

/// A commit message request: `diff` is what's committed (staged, else all
/// changes), `recent` the repository's last subjects, newest first, `notes`
/// what the user had typed in the message box (a hint, maybe empty).
pub fn commit_message(diff: &str, recent: &[String], branch: Option<&str>, notes: &str) -> Request {
    let diff = cut(diff, DIFF_BYTES);
    let style = if recent.is_empty() {
        "The repository has no commits yet.".to_string()
    } else {
        format!("Recent subjects in this repository, newest first:\n{}", recent.iter().take(15).map(|s| format!("- {s}")).collect::<Vec<_>>().join("\n"))
    };
    let system = "You write Git commit messages. Reply with the message only: a subject line of at most 72 \
                  characters, then, if the change needs explaining, a blank line and a short body wrapped at 72 \
                  characters saying what changed and why. Follow the style of the repository's recent subjects \
                  (their tense, capitalization, prefixes such as `feat:` or a component name, and whether they end \
                  with a period). No Markdown fences, no quotes around it, no sign-offs or trailers.";
    let branch = branch.map(|b| format!("Branch: {b}\n\n")).unwrap_or_default();
    let notes = match notes.trim() {
        "" => String::new(),
        notes => format!("\n\nThe user's notes on this change (build the message around them):\n{notes}"),
    };
    let mut request = Request::simple("", system, format!("{branch}{style}\n\n<diff>\n{diff}\n</diff>{notes}"));
    request.max_tokens = 1024;
    request.effort = Some(Effort::Low);
    request
}

/// The commit message in a reply: fences, quotes and surrounding blank lines
/// removed, the subject's trailing whitespace too.
pub fn commit_text(reply: &str) -> String {
    let text = strip_fences(reply);
    let text = text.trim();
    let text = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(text);
    text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// A reply without the Markdown fence around it (the first and last fence
/// lines, when the reply is one fenced block, maybe after a line of prose).
fn strip_fences(reply: &str) -> String {
    let trimmed = reply.trim_matches(['\n', '\r']);
    let lines: Vec<&str> = trimmed.lines().collect();
    let Some(open) = lines.iter().position(|l| l.trim_start().starts_with("```")) else { return trimmed.to_string() };
    // Only prose before the fence: at most one line, as "Here's the code:".
    if open > 1 {
        return trimmed.to_string();
    }
    let rest = &lines[open + 1..];
    let close = rest.iter().rposition(|l| l.trim() == "```").unwrap_or(rest.len());
    let mut code = rest[..close].join("\n");
    if close < rest.len() || trimmed.ends_with('\n') {
        code.push('\n');
    }
    code
}

fn clamp(text: &str, range: Range<usize>) -> Range<usize> {
    let mut start = range.start.min(text.len());
    let mut end = range.end.clamp(start, text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while !text.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

/// The ends of `before` and the start of `after`, at most `budget` bytes
/// together, cut at line starts, half each way (more where one side is short).
fn around<'a>(before: &'a str, after: &'a str, budget: usize) -> (&'a str, &'a str) {
    if before.len() + after.len() <= budget {
        return (before, after);
    }
    let half = budget / 2;
    let (b, a) = if before.len() < half {
        (before.len(), budget - before.len())
    } else if after.len() < half {
        (budget - after.len(), after.len())
    } else {
        (half, budget - half)
    };
    let mut from = before.len() - b.min(before.len());
    while !before.is_char_boundary(from) {
        from += 1;
    }
    let from = before[from..].find('\n').map_or(from, |n| from + n + 1).min(before.len());
    let mut to = a.min(after.len());
    while !after.is_char_boundary(to) {
        to -= 1;
    }
    let to = after[..to].rfind('\n').map_or(to, |n| n + 1);
    (&before[from..], &after[..to])
}

/// At most `max` bytes of `text`, cut at a line, with a note when cut.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = text[..end].rfind('\n').map_or(end, |n| n + 1);
    format!("{}\n[… {} more bytes of the diff left out]", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_assist_marks_the_selection_or_caret() {
        let text = "fn a() {\n    1\n}\n";
        let request = inline_assist("src/a.rs", "Rust", text, 13..15, "make it two");
        let user = request.messages[0].text();
        assert!(user.contains("fn a() {\n    <selection>1\n</selection>}"), "{user}");
        assert!(user.contains("<instruction>\nmake it two\n</instruction>"));
        assert!(request.system.contains("in place of the selection"));
        let request = inline_assist("src/a.rs", "Rust", text, 9..9, "add a line");
        assert!(request.messages[0].text().contains("fn a() {\n<caret/>    1"));
        assert!(request.system.contains("at the caret"));
    }

    #[test]
    fn big_files_send_lines_around_the_selection() {
        let line = "let x = 1;\n";
        let text = line.repeat(30_000);
        let start = line.len() * 15_000;
        let request = inline_assist("a.rs", "Rust", &text, start..start + line.len(), "x");
        let user = request.messages[0].text();
        assert!(user.len() < FILE_BYTES + 500, "{}", user.len());
        assert!(user.contains("<selection>let x = 1;\n</selection>"));
        // Cut at line starts.
        assert!(user.contains("language=\"Rust\">\nlet x = 1;\n"));
        assert!(user.ends_with("let x = 1;\n\n</file>\n\n<instruction>\nx\n</instruction>"));
    }

    #[test]
    fn assist_replies_lose_fences_and_fit_the_selection() {
        assert_eq!(assist_code("```rust\n    2\n```\n", "    1\n"), "    2\n");
        assert_eq!(assist_code("Here's the code:\n```\nfoo()\n```", "bar()"), "foo()");
        assert_eq!(assist_code("<selection>x + 1</selection>", "x"), "x + 1");
        assert_eq!(assist_code("a\nb", "old\n"), "a\nb\n");
        // Prose with a fence deep inside stays as written.
        assert_eq!(assist_code("one\ntwo\n```\nthree\n```", ""), "one\ntwo\n```\nthree\n```");
        assert_eq!(assist_code_so_far("```py\nprint(1"), "print(1");
    }

    #[test]
    fn commit_messages_follow_the_repository() {
        let request = commit_message("diff --git a/x b/x\n+y\n", &["Add x".into(), "Fix y".into()], Some("main"), "");
        let user = request.messages[0].text();
        assert!(user.starts_with("Branch: main\n\nRecent subjects in this repository, newest first:\n- Add x\n- Fix y"));
        assert!(user.contains("<diff>\ndiff --git a/x b/x\n+y\n\n</diff>"));
        assert_eq!(request.effort, Some(Effort::Low));
        let long = "+line\n".repeat(20_000);
        let user = commit_message(&long, &[], None, "  ").messages[0].text();
        assert!(user.contains("more bytes of the diff left out") && user.len() < DIFF_BYTES + 400);
        assert!(user.starts_with("The repository has no commits yet."));
        assert!(!user.contains("notes"));
        let user = commit_message("+y", &[], None, "fix login").messages[0].text();
        assert!(user.ends_with("The user's notes on this change (build the message around them):\nfix login"));
    }

    #[test]
    fn commit_text_is_the_message_alone() {
        assert_eq!(commit_text("```\nAdd the parser\n\nIt reads TOML.  \n```"), "Add the parser\n\nIt reads TOML.");
        assert_eq!(commit_text("\"Fix the build\"\n"), "Fix the build");
    }
}
