//! What Claude Code says to an IDE and what it gets back: MCP over JSON-RPC
//! 2.0, one message per WebSocket text frame. Pure: the server hands each
//! frame to [`handle`] with what it knows without asking the window (the
//! project's folders, the last selection), and gets back either the answer
//! or a tool call for the window to answer ([`tool_result`]).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The MCP version answered when the client names none.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// What the server knows without asking the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// The project's folders, the primary first.
    pub folders: Vec<PathBuf>,
    /// The `selection_changed` payload of the active editor's selection,
    /// as last sent; None until there's been one.
    pub selection: Option<Value>,
}

/// What a frame calls for.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Send this back.
    Reply(Value),
    /// A tool the window answers; its answer goes back with [`tool_result`].
    Tool { id: Value, name: String, arguments: Value },
    /// Nothing to answer: a notification, or a response to something sent.
    Nothing,
}

/// Tools the window answers (the others are answered here), with the
/// string arguments each one needs.
const WINDOW_TOOLS: &[(&str, &[&str])] = &[
    ("openFile", &["filePath"]),
    ("getOpenEditors", &[]),
    ("getDiagnostics", &[]),
    ("checkDocumentDirty", &["filePath"]),
    ("saveDocument", &["filePath"]),
];

/// Answers one text frame.
pub fn handle(text: &str, state: &State) -> Incoming {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return Incoming::Reply(error(Value::Null, -32700, "Parse error"));
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response to a request of ours (none are sent yet).
        return Incoming::Nothing;
    };
    let Some(id) = message.get("id").cloned() else {
        // `notifications/initialized` and the like.
        return Incoming::Nothing;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => initialize(&params),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_definitions() }),
        "prompts/list" => json!({ "prompts": [] }),
        "resources/list" => json!({ "resources": [] }),
        "tools/call" => return call(id, &params, state),
        _ => return Incoming::Reply(error(id, -32601, &format!("Method not found: {method}"))),
    };
    Incoming::Reply(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// A tool's answer from the window: text items (most of them JSON), or why
/// it failed.
pub fn tool_result(id: Value, answer: Result<Vec<String>, String>) -> Value {
    let result = match answer {
        Ok(texts) => json!({ "content": texts.into_iter().map(|t| json!({ "type": "text", "text": t })).collect::<Vec<_>>() }),
        Err(message) => json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A notification to send to Claude.
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A file's `file://` URL, as Claude names files.
pub fn file_url(path: &Path) -> String {
    url::Url::from_file_path(path).map(String::from).unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// The path a `file://` URL (or a plain path) names.
pub fn path_of(uri: &str) -> PathBuf {
    url::Url::parse(uri).ok().filter(|u| u.scheme() == "file").and_then(|u| u.to_file_path().ok()).unwrap_or_else(|| PathBuf::from(uri))
}

fn initialize(params: &Value) -> Value {
    let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "logging": {}, "prompts": { "listChanged": true }, "tools": { "listChanged": true } },
        "serverInfo": { "name": "kuana", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn call(id: Value, params: &Value, state: &State) -> Incoming {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let arguments = params.get("arguments").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    let answer = match name {
        "getWorkspaceFolders" => Ok(vec![workspace_folders(&state.folders).to_string()]),
        // Claude's own plugins keep both: the selection now and the last
        // one made (which survives moving to the terminal); Kuana's last
        // sent is both.
        "getCurrentSelection" | "getLatestSelection" => Ok(vec![
            state.selection.clone().unwrap_or_else(|| json!({ "success": false, "message": "No active editor found" })).to_string(),
        ]),
        // Claude calls it at the start of each turn, unadvertised. Without
        // `openDiff` there are never any to close.
        "closeAllDiffTabs" => Ok(vec!["CLOSED_0_DIFF_TABS".to_string()]),
        _ => match WINDOW_TOOLS.iter().find(|(tool, _)| *tool == name) {
            Some((_, required)) => match required.iter().find(|arg| arguments.get(**arg).and_then(Value::as_str).is_none()) {
                Some(missing) => Err(format!("Missing argument: {missing}")),
                None => return Incoming::Tool { id, name: name.to_string(), arguments },
            },
            None => return Incoming::Reply(error(id, -32602, &format!("Unknown tool: {name}"))),
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

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// What `tools/list` advertises. Proposing edits as diffs (`openDiff`) isn't
/// offered yet, so Claude writes its edits itself.
fn tool_definitions() -> Vec<Value> {
    let path = |about: &str| json!({ "filePath": { "type": "string", "description": about } });
    vec![
        tool(
            "openFile",
            "Opens a file in the editor, optionally selecting lines or the text between two patterns.",
            json!({
                "filePath": { "type": "string", "description": "The file to open" },
                "preview": { "type": "boolean", "description": "Open it as a preview tab", "default": false },
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
    ]
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required, "additionalProperties": false },
    })
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
    fn initialize_echoes_the_clients_version() {
        let answer = reply(handle(&request("initialize", json!({ "protocolVersion": "2024-11-05" })), &State::default()));
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(answer["result"]["serverInfo"]["name"], "kuana");
        let answer = reply(handle(&request("initialize", json!({})), &State::default()));
        assert_eq!(answer["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn lists_the_tools_it_answers() {
        let answer = reply(handle(&request("tools/list", json!({})), &State::default()));
        let names: Vec<&str> = answer["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        for (tool, _) in WINDOW_TOOLS {
            assert!(names.contains(tool), "{tool} isn't listed");
        }
        assert!(names.contains(&"getWorkspaceFolders"));
        assert!(!names.contains(&"openDiff"));
    }

    #[test]
    fn notifications_and_responses_get_no_answer() {
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        assert_eq!(handle(&initialized, &State::default()), Incoming::Nothing);
        let response = json!({ "jsonrpc": "2.0", "id": 1, "result": {} }).to_string();
        assert_eq!(handle(&response, &State::default()), Incoming::Nothing);
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

        let answer = reply(handle(&request("tools/call", json!({ "name": "closeAllDiffTabs" })), &state));
        assert_eq!(answer["result"]["content"][0]["text"], "CLOSED_0_DIFF_TABS");
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
        // Without arguments at all, a tool that needs none still goes.
        let diagnostics = request("tools/call", json!({ "name": "getDiagnostics" }));
        assert!(matches!(handle(&diagnostics, &State::default()), Incoming::Tool { .. }));
    }

    #[test]
    fn tool_results_carry_text_items_or_an_error() {
        let ok = tool_result(json!(3), Ok(vec!["a".into(), "b".into()]));
        assert_eq!(ok["result"]["content"][1]["text"], "b");
        assert!(ok["result"].get("isError").is_none());
        let failed = tool_result(json!(3), Err("no".into()));
        assert_eq!(failed["result"]["isError"], true);
    }

    #[test]
    fn file_urls_go_both_ways() {
        let path = if cfg!(windows) { PathBuf::from(r"C:\work\a b.rs") } else { PathBuf::from("/work/a b.rs") };
        let url = file_url(&path);
        assert!(url.starts_with("file:///") && url.contains("a%20b.rs"), "{url}");
        assert_eq!(path_of(&url), path);
        assert_eq!(path_of("/plain/path.rs"), PathBuf::from("/plain/path.rs"));
    }
}
