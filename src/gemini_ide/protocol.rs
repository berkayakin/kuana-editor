//! What Gemini CLI says to its IDE companion and what it gets back
//! (`docs/ide-integration/ide-companion-spec.md` in google-gemini/gemini-cli):
//! MCP over JSON-RPC 2.0, one message per HTTP POST, notifications to it on
//! the session's event stream. Pure: the discovery file, the tools, the
//! context it's told.

use std::path::PathBuf;

use ropey::Rope;
use serde_json::{Value, json};

pub use crate::agent_ide::Incoming;
use crate::agent_ide::{tool, window_tool};

/// How Gemini CLI names Kuana (`ideInfo`). The CLI takes these from the
/// discovery file; without them it only knows IDEs it can tell from their
/// terminals' variables.
pub const IDE_NAME: &str = "kuana";
pub const IDE_DISPLAY_NAME: &str = "Kuana";
/// How many files and how much selected text the CLI keeps; more is wasted.
const MAX_OPEN_FILES: usize = 10;
const MAX_SELECTED_TEXT: usize = 16 * 1024;

/// The tools, both answered by the window, with the string arguments each
/// one needs. The CLI proposes edits only when both are listed.
const WINDOW_TOOLS: &[(&str, &[&str])] = &[("openDiff", &["filePath", "newContent"]), ("closeDiff", &["filePath"])];

/// Answers one JSON-RPC message.
pub fn handle(message: &Value) -> Incoming {
    crate::agent_ide::handle(message, tool_definitions, |id, name, arguments| match WINDOW_TOOLS.iter().find(|(tool, _)| *tool == name) {
        Some((_, required)) => window_tool(id, name, arguments, required),
        None => Incoming::Reply(crate::agent_ide::error(id, -32602, &format!("Unknown tool: {name}"))),
    })
}

/// Whether a message is the session's first, `initialize`.
pub fn is_initialize(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("initialize") && message.get("id").is_some()
}

fn tool_definitions() -> Vec<Value> {
    let path = json!({ "type": "string", "description": "The absolute path of the file" });
    vec![
        tool(
            "openDiff",
            "(IDE Tool) Open a diff view to create or modify a file. Returns a notification once the diff has been accepted or rejected.",
            json!({ "filePath": path, "newContent": { "type": "string", "description": "The proposed new content of the file" } }),
            &["filePath", "newContent"],
        ),
        tool(
            "closeDiff",
            "(IDE Tool) Close an open diff view for a specific file.",
            json!({ "filePath": path, "suppressNotification": { "type": "boolean", "description": "Deprecated; no notification is sent either way" } }),
            &["filePath"],
        ),
    ]
}

/// `closeDiff`'s answer: JSON holding the text the diff proposed, which the
/// CLI writes when the user accepted in the terminal (null: no diff).
pub fn closed_diff(text: Option<&str>) -> String {
    json!({ "content": text }).to_string()
}

/// The discovery file's name: Gemini CLI prefers the one named with the
/// process its terminal belongs to.
pub fn file_name(pid: u32, port: u16) -> String {
    format!("gemini-ide-server-{pid}-{port}.json")
}

/// The process id in a discovery file's name.
pub fn pid_in(file_name: &str) -> Option<u32> {
    let rest = file_name.strip_prefix("gemini-ide-server-")?.strip_suffix(".json")?;
    let (pid, port) = rest.split_once('-')?;
    port.parse::<u16>().ok()?;
    pid.parse().ok()
}

/// The project's folders as one string, joined as PATH is (`:`, `;` on
/// Windows): the CLI connects only when its working folder is in one.
pub fn workspace_path(folders: &[PathBuf]) -> String {
    let separator = if cfg!(windows) { ";" } else { ":" };
    folders.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(separator)
}

/// The discovery file's content.
pub fn discovery(port: u16, folders: &[PathBuf], token: &str) -> Value {
    json!({
        "port": port,
        "workspacePath": workspace_path(folders),
        "authToken": token,
        "ideInfo": { "name": IDE_NAME, "displayName": IDE_DISPLAY_NAME },
    })
}

/// A file open in a tab, as `ide/contextUpdate` lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenFile {
    pub path: PathBuf,
    /// When it was last the active tab (Unix milliseconds; 0: not since
    /// Kuana started).
    pub timestamp: u64,
    /// The active file's caret and selection.
    pub active: Option<Caret>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Caret {
    /// 1-based, the column in UTF-16 units as VS Code counts.
    pub line: usize,
    pub character: usize,
    pub selected: String,
}

/// The caret (the selection's moving end) and the selected text of a
/// selection from `anchor` to `head` (bytes).
pub fn caret(rope: &Rope, anchor: usize, head: usize) -> Caret {
    let len = rope.len_bytes();
    let (head, anchor) = (head.min(len), anchor.min(len));
    let position = crate::lsp::protocol::offset_to_position(rope, head, crate::lsp::protocol::Encoding::Utf16);
    let (start, end) = (anchor.min(head), anchor.max(head));
    Caret { line: position.line as usize + 1, character: position.character as usize + 1, selected: rope.byte_slice(start..end).to_string() }
}

/// `ide/contextUpdate`'s params: the most recently active files first, the
/// active one with its caret, and whether Kuana trusts the project.
pub fn context(mut files: Vec<OpenFile>, trusted: bool) -> Value {
    files.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.active.is_some().cmp(&a.active.is_some())));
    files.truncate(MAX_OPEN_FILES);
    let open: Vec<Value> = files
        .into_iter()
        .map(|file| {
            let mut entry = json!({ "path": file.path, "timestamp": file.timestamp });
            if let Some(caret) = file.active {
                entry["isActive"] = json!(true);
                entry["cursor"] = json!({ "line": caret.line, "character": caret.character });
                if !caret.selected.is_empty() {
                    entry["selectedText"] = json!(truncate(&caret.selected, MAX_SELECTED_TEXT));
                }
            }
            entry
        })
        .collect();
    json!({ "workspaceState": { "openFiles": open, "isTrusted": trusted } })
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Whether a discovery file in Gemini CLI's folder is one of Kuana's.
pub fn is_kuanas(content: &Value) -> bool {
    content.pointer("/ideInfo/name").and_then(Value::as_str) == Some(IDE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 4, "method": method, "params": params })
    }

    fn reply(incoming: Incoming) -> Value {
        match incoming {
            Incoming::Reply(value) => value,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn lists_both_diff_tools() {
        let answer = reply(handle(&request("tools/list", json!({}))));
        let names: Vec<&str> = answer["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["openDiff", "closeDiff"]);
        assert_eq!(answer["result"]["tools"][0]["inputSchema"]["required"], json!(["filePath", "newContent"]));
    }

    #[test]
    fn hands_diff_calls_to_the_window() {
        let open = request("tools/call", json!({ "name": "openDiff", "arguments": { "filePath": "/w/a.rs", "newContent": "" } }));
        match handle(&open) {
            Incoming::Tool { id, name, arguments } => assert_eq!((id, name.as_str(), &arguments["newContent"]), (json!(4), "openDiff", &json!(""))),
            other => panic!("expected a tool call, got {other:?}"),
        }
        let close = request("tools/call", json!({ "name": "closeDiff", "arguments": { "filePath": "/w/a.rs", "suppressNotification": true } }));
        assert!(matches!(handle(&close), Incoming::Tool { .. }));
        let missing = reply(handle(&request("tools/call", json!({ "name": "openDiff", "arguments": { "filePath": "/w/a.rs" } }))));
        assert_eq!(missing["result"]["content"][0]["text"], "Missing argument: newContent");
        assert_eq!(reply(handle(&request("tools/call", json!({ "name": "openFile" }))))["error"]["code"], -32602);
    }

    #[test]
    fn knows_the_first_message() {
        assert!(is_initialize(&request("initialize", json!({}))));
        assert!(!is_initialize(&json!({ "jsonrpc": "2.0", "method": "initialize" })));
        assert!(!is_initialize(&request("tools/list", json!({}))));
    }

    #[test]
    fn closing_a_diff_answers_its_text() {
        assert_eq!(serde_json::from_str::<Value>(&closed_diff(Some("new"))).unwrap(), json!({ "content": "new" }));
        assert_eq!(serde_json::from_str::<Value>(&closed_diff(None)).unwrap(), json!({ "content": null }));
    }

    #[test]
    fn discovery_files_name_kuana_and_its_folders() {
        let folders = [PathBuf::from("/w/app"), PathBuf::from("/w/lib")];
        let content = discovery(4321, &folders, "secret");
        assert_eq!(content["port"], 4321);
        assert_eq!(content["authToken"], "secret");
        assert_eq!(content["ideInfo"], json!({ "name": "kuana", "displayName": "Kuana" }));
        let joined = if cfg!(windows) { "/w/app;/w/lib" } else { "/w/app:/w/lib" };
        assert_eq!(content["workspacePath"], joined);
        assert!(is_kuanas(&content));
        assert!(!is_kuanas(&json!({ "port": 1 })));
        assert_eq!(file_name(77, 4321), "gemini-ide-server-77-4321.json");
        assert_eq!(pid_in("gemini-ide-server-77-4321.json"), Some(77));
        assert_eq!(pid_in("gemini-ide-server-77.json"), None);
        assert_eq!(pid_in("other-77-1.json"), None);
    }

    #[test]
    fn carets_are_one_based_with_utf16_columns() {
        let rope = Rope::from_str("fn main() {}\nlet é = \"😀x\";\n");
        let start = "fn main() {}\n".len() + "let é = \"".len();
        let end = start + "😀x".len();
        let forward = caret(&rope, start, end);
        assert_eq!((forward.line, forward.character, forward.selected.as_str()), (2, 13, "😀x"));
        // Selected backwards, the caret is at the start.
        let backward = caret(&rope, end, start);
        assert_eq!((backward.line, backward.character, backward.selected.as_str()), (2, 10, "😀x"));
        assert_eq!(caret(&rope, 0, 0), Caret { line: 1, character: 1, selected: String::new() });
    }

    #[test]
    fn context_lists_recent_files_first_and_caps_them() {
        let file = |name: &str, timestamp: u64| OpenFile { path: PathBuf::from(format!("/w/{name}")), timestamp, active: None };
        let mut files: Vec<OpenFile> = (0..12).map(|i| file(&format!("f{i}"), i)).collect();
        files.push(OpenFile {
            active: Some(Caret { line: 3, character: 1, selected: "é".repeat(MAX_SELECTED_TEXT) }),
            ..file("active.rs", 100)
        });
        let value = context(files, true);
        let open = value["workspaceState"]["openFiles"].as_array().unwrap();
        assert_eq!(open.len(), MAX_OPEN_FILES);
        assert_eq!(open[0]["path"], "/w/active.rs");
        assert_eq!(open[0]["isActive"], true);
        assert_eq!(open[0]["cursor"], json!({ "line": 3, "character": 1 }));
        let selected = open[0]["selectedText"].as_str().unwrap();
        assert!(selected.len() <= MAX_SELECTED_TEXT && selected.chars().all(|c| c == 'é'));
        assert_eq!(open[1]["path"], "/w/f11");
        assert!(open[1].get("isActive").is_none() && open[1].get("cursor").is_none());
        assert_eq!(value["workspaceState"]["isTrusted"], true);
        // No selection, no selectedText.
        let plain = context(vec![OpenFile { active: Some(Caret { line: 1, character: 1, selected: String::new() }), ..file("a", 1) }], false);
        assert!(plain["workspaceState"]["openFiles"][0].get("selectedText").is_none());
    }
}
