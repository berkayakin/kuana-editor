//! What Claude Code says to an IDE and what it gets back: MCP over JSON-RPC
//! 2.0, one message per WebSocket text frame. Pure: the server hands each
//! frame to [`handle`] with what it knows without asking the window (the
//! project's folders, the last selection), and gets back either the answer
//! or a tool call for the window to answer ([`tool_result`]).

use std::path::PathBuf;

use serde_json::{Value, json};

pub use crate::agent_ide::Incoming;
use crate::agent_ide::{file_url, tool, tool_result, window_tool};

/// What the server knows without asking the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// The project's folders, the primary first.
    pub folders: Vec<PathBuf>,
    /// The `selection_changed` payload of the active editor's selection,
    /// as last sent; None until there's been one.
    pub selection: Option<Value>,
}

/// Tools the window answers (the others are answered here), with the
/// string arguments each one needs.
const WINDOW_TOOLS: &[(&str, &[&str])] = &[
    ("openFile", &["filePath"]),
    ("getOpenEditors", &[]),
    ("getDiagnostics", &[]),
    ("checkDocumentDirty", &["filePath"]),
    ("saveDocument", &["filePath"]),
    // Answered once the user accepts or rejects the change.
    ("openDiff", &["old_file_path", "new_file_path", "new_file_contents", "tab_name"]),
    // Claude closes a diff it opened (answered in the terminal instead),
    // and all of them at the start of each turn.
    ("close_tab", &["tab_name"]),
    ("closeAllDiffTabs", &[]),
];

/// Answers one text frame.
pub fn handle(text: &str, state: &State) -> Incoming {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return Incoming::Reply(crate::agent_ide::error(Value::Null, -32700, "Parse error"));
    };
    crate::agent_ide::handle(&message, tool_definitions, |id, name, arguments| call(id, name, arguments, state))
}

fn call(id: Value, name: &str, arguments: Value, state: &State) -> Incoming {
    let answer = match name {
        "getWorkspaceFolders" => Ok(vec![workspace_folders(&state.folders).to_string()]),
        // Claude's own plugins keep both: the selection now and the last
        // one made (which survives moving to the terminal); Kuana's last
        // sent is both.
        "getCurrentSelection" | "getLatestSelection" => Ok(vec![
            state.selection.clone().unwrap_or_else(|| json!({ "success": false, "message": "No active editor found" })).to_string(),
        ]),
        _ => match WINDOW_TOOLS.iter().find(|(tool, _)| *tool == name) {
            Some((_, required)) => return window_tool(id, name, arguments, required),
            None => return Incoming::Reply(crate::agent_ide::error(id, -32602, &format!("Unknown tool: {name}"))),
        },
    };
    Incoming::Reply(tool_result(id, answer))
}

fn workspace_folders(folders: &[PathBuf]) -> Value {
    let entries: Vec<Value> = folders
        .iter()
        .map(|f| {
            let name = f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned());
            json!({ "name": name, "uri": file_url(f), "path": f })
        })
        .collect();
    json!({ "success": true, "folders": entries, "rootPath": folders.first() })
}

/// What `tools/list` advertises (`close_tab` is Claude's own, unlisted).
fn tool_definitions() -> Vec<Value> {
    let path = |about: &str| json!({ "filePath": { "type": "string", "description": about } });
    vec![
        tool(
            "openFile",
            "Opens a file in the editor, optionally selecting lines or the text between two patterns.",
            json!({
                "filePath": { "type": "string", "description": "The file to open" },
                // Claude's plugins take it; Kuana has no preview tabs, so a file opens as any other.
                "preview": { "type": "boolean", "description": "Ignored: Kuana has no preview tabs", "default": false },
                "startLine": { "type": "integer", "description": "First line to select (1-based)" },
                "endLine": { "type": "integer", "description": "Last line to select (1-based)" },
                "startText": { "type": "string", "description": "Text where the selection starts" },
                "endText": { "type": "string", "description": "Text where the selection ends (after startText)" },
                "selectToEndOfLine": { "type": "boolean", "description": "Extend the selection to the end of its last line", "default": false },
                "makeFrontmost": { "type": "boolean", "description": "Make it the active tab and focus it", "default": true },
            }),
            &["filePath"],
        ),
        tool("getCurrentSelection", "The text selected in the active editor, with its file and position.", json!({}), &[]),
        tool("getLatestSelection", "The most recent text selection in an editor.", json!({}), &[]),
        tool("getOpenEditors", "The files open in the editor's tabs.", json!({}), &[]),
        tool("getWorkspaceFolders", "The folders of the project open in the editor.", json!({}), &[]),
        tool(
            "getDiagnostics",
            "Errors and warnings the editor shows (language servers and linters), for one file or all of them.",
            json!({ "uri": { "type": "string", "description": "A file URI; all files with problems when left out" } }),
            &[],
        ),
        tool("checkDocumentDirty", "Whether an open file has unsaved changes.", path("The file to check"), &["filePath"]),
        tool("saveDocument", "Saves an open file's unsaved changes.", path("The file to save"), &["filePath"]),
        tool(
            "openDiff",
            "Shows a proposed change to a file as a diff in the editor and waits until the user accepts or rejects it.",
            json!({
                "old_file_path": { "type": "string", "description": "The file as it is" },
                "new_file_path": { "type": "string", "description": "The file the change is for" },
                "new_file_contents": { "type": "string", "description": "The file's whole text with the change" },
                "tab_name": { "type": "string", "description": "A name for the diff's tab" },
            }),
            &["old_file_path", "new_file_path", "new_file_contents", "tab_name"],
        ),
        tool("closeAllDiffTabs", "Closes every diff tab opened with openDiff.", json!({}), &[]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }).to_string()
    }

    fn reply(incoming: Incoming) -> Value {
        match incoming {
            Incoming::Reply(value) => value,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn lists_the_tools_it_answers() {
        let answer = reply(handle(&request("tools/list", json!({})), &State::default()));
        let names: Vec<&str> = answer["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        for (tool, _) in WINDOW_TOOLS.iter().filter(|(t, _)| *t != "close_tab") {
            assert!(names.contains(tool), "{tool} isn't listed");
        }
        assert!(names.contains(&"getWorkspaceFolders"));
        assert!(!names.contains(&"close_tab"));
    }

    #[test]
    fn unknown_methods_and_bad_json_are_errors() {
        assert_eq!(reply(handle(&request("nope", json!({})), &State::default()))["error"]["code"], -32601);
        assert_eq!(reply(handle("{", &State::default()))["error"]["code"], -32700);
        let unknown = request("tools/call", json!({ "name": "launchRockets" }));
        assert_eq!(reply(handle(&unknown, &State::default()))["error"]["code"], -32602);
    }

    #[test]
    fn answers_folders_and_selection_itself() {
        let state = State {
            folders: vec![PathBuf::from("/work/app"), PathBuf::from("/work/lib")],
            selection: Some(json!({ "text": "fn main", "filePath": "/work/app/main.rs" })),
        };
        let answer = reply(handle(&request("tools/call", json!({ "name": "getWorkspaceFolders" })), &state));
        let folders: Value = serde_json::from_str(answer["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(folders["rootPath"], "/work/app");
        assert_eq!(folders["folders"][1]["name"], "lib");
        assert_eq!(folders["folders"][1]["uri"], "file:///work/lib");

        let answer = reply(handle(&request("tools/call", json!({ "name": "getCurrentSelection" })), &state));
        let selection: Value = serde_json::from_str(answer["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(selection["text"], "fn main");

        let answer = reply(handle(&request("tools/call", json!({ "name": "getLatestSelection" })), &State::default()));
        let none: Value = serde_json::from_str(answer["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(none["success"], false);
    }

    #[test]
    fn window_tools_are_handed_on_once_their_arguments_are_there() {
        let open = request("tools/call", json!({ "name": "openFile", "arguments": { "filePath": "/a.rs", "startLine": 3 } }));
        match handle(&open, &State::default()) {
            Incoming::Tool { id, name, arguments } => {
                assert_eq!((id, name.as_str()), (json!(7), "openFile"));
                assert_eq!(arguments["startLine"], 3);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        let missing = request("tools/call", json!({ "name": "saveDocument", "arguments": {} }));
        let answer = reply(handle(&missing, &State::default()));
        assert_eq!(answer["result"]["isError"], true);
        assert_eq!(answer["result"]["content"][0]["text"], "Missing argument: filePath");
        let diff = request("tools/call", json!({ "name": "openDiff", "arguments": { "old_file_path": "/a.rs", "new_file_path": "/a.rs", "new_file_contents": "", "tab_name": "a" } }));
        assert!(matches!(handle(&diff, &State::default()), Incoming::Tool { .. }), "empty contents are contents");
        // Without arguments at all, a tool that needs none still goes.
        let diagnostics = request("tools/call", json!({ "name": "getDiagnostics" }));
        assert!(matches!(handle(&diagnostics, &State::default()), Incoming::Tool { .. }));
    }
}
