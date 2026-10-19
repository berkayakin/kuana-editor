//! Delegates to the project's own ESLint (with its config) when installed.
//! Runs as a child process with the buffer on stdin, so unsaved text works.

use std::{
    borrow::Cow,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};

use ropey::Rope;
use serde_json::Value;

use super::{Diagnostic, Severity};

#[derive(Clone, Debug)]
pub struct ExternalLinter {
    pub name: &'static str,
    program: PathBuf,
    project_dir: PathBuf,
}

/// Walks up from the file looking for `node_modules/.bin/eslint`.
pub fn find_for(file: &Path) -> Option<ExternalLinter> {
    let bin = if cfg!(windows) { "eslint.cmd" } else { "eslint" };
    file.ancestors().skip(1).find_map(|dir| {
        let program = dir.join("node_modules").join(".bin").join(bin);
        program.is_file().then(|| ExternalLinter {
            name: "ESLint",
            program,
            project_dir: dir.to_path_buf(),
        })
    })
}

impl ExternalLinter {
    /// Runs the linter; blocking, so call it from a background thread.
    pub fn run(&self, file: &Path, rope: &Rope) -> Result<Vec<Diagnostic>, String> {
        let mut command = crate::process::command(&self.program);
        command
            .current_dir(&self.project_dir)
            .args(["--format", "json", "--stdin", "--stdin-filename"])
            .arg(file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            for chunk in rope.chunks() {
                stdin.write_all(chunk.as_bytes()).map_err(|e| e.to_string())?;
            }
        }
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        // ESLint exits with 1 when it found problems; 2 means it failed.
        if output.status.code() == Some(2) || output.stdout.is_empty() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("ESLint failed").to_string());
        }
        let json: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        Ok(parse_eslint(&json, rope))
    }
}

fn parse_eslint(json: &Value, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(results) = json.as_array() else { return out };
    for message in results
        .iter()
        .filter_map(|r| r.get("messages")?.as_array())
        .flatten()
    {
        let line = message.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
        let column = message.get("column").and_then(Value::as_u64).unwrap_or(1) as usize;
        let start = offset_for(rope, line, column);
        let end = match (
            message.get("endLine").and_then(Value::as_u64),
            message.get("endColumn").and_then(Value::as_u64),
        ) {
            (Some(l), Some(c)) => offset_for(rope, l as usize, c as usize),
            _ => start,
        };
        let severity = match message.get("severity").and_then(Value::as_u64) {
            Some(2) => Severity::Error,
            _ => Severity::Warning,
        };
        let rule = message
            .get("ruleId")
            .and_then(Value::as_str)
            .map(|r| Cow::Owned(r.to_string()))
            .unwrap_or(Cow::Borrowed("eslint"));
        out.push(Diagnostic {
            range: start..end.max(start),
            severity,
            message: message.get("message").and_then(Value::as_str).unwrap_or_default().into(),
            rule,
            unnecessary: false,
        });
    }
    out
}

/// Converts ESLint's 1-based line and UTF-16 column into a byte offset.
fn offset_for(rope: &Rope, line: usize, column: usize) -> usize {
    let row = line.saturating_sub(1).min(rope.len_lines().saturating_sub(1));
    let line_char = rope.line_to_char(row);
    let line_utf16 = rope.char_to_utf16_cu(line_char);
    let target = (line_utf16 + column.saturating_sub(1)).min(rope.len_utf16_cu());
    rope.char_to_byte(rope.utf16_cu_to_char(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eslint_json() {
        let rope = Rope::from_str("let a = 1;\nfoo();\n");
        let json: Value = serde_json::from_str(
            r#"[{"messages":[{"ruleId":"no-undef","severity":2,"message":"'foo' is not defined.","line":2,"column":1,"endLine":2,"endColumn":4}]}]"#,
        )
        .unwrap();
        let d = parse_eslint(&json, &rope);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].range, 11..14);
        assert_eq!(d[0].rule, "no-undef");
        assert_eq!(d[0].severity, Severity::Error);
    }
}
