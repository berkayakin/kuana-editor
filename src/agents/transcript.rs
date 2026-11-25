//! A session's conversation as Kuana shows and saves it: what the user sent,
//! what the agent answered and thought, the tools it ran (with their diffs,
//! terminals and places), its plan, and notices. Built from ACP's
//! `session/update`s ([`Transcript::apply`]); Kuana's own types, so saved
//! sessions don't depend on the protocol crate's. Pure, tested.

use std::path::PathBuf;

use agent_client_protocol_schema::v1 as acp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    User { text: String, #[serde(default)] context: Vec<String> },
    Assistant { text: String },
    Thought { text: String },
    Tool(Tool),
    Plan { items: Vec<PlanItem> },
    Notice { level: Level, text: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
}

/// What kind of step a tool call is, for its icon and the folded summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    #[default]
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub id: String,
    pub title: String,
    pub kind: Kind,
    pub status: Status,
    #[serde(default)]
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub locations: Vec<Location>,
    #[serde(default)]
    pub input: Option<Value>,
    /// The agent asked whether it may: its options, and what was chosen.
    #[serde(default)]
    pub permission: Option<Permission>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text { text: String },
    /// `old` None: a new file.
    Diff { path: PathBuf, old: Option<String>, new: String },
    Terminal { id: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub path: PathBuf,
    pub line: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Permission {
    pub options: Vec<PermissionOption>,
    /// The option picked; None while asking (or when the turn was stopped).
    pub chosen: Option<String>,
    /// Whether the question is still open.
    pub open: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once` or `reject_always`.
    pub kind: String,
}

impl PermissionOption {
    pub fn allows(&self) -> bool {
        self.kind.starts_with("allow")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanItem {
    pub text: String,
    pub status: Status,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    /// Changes whenever an entry does (views key their caches on it).
    #[serde(skip)]
    pub generation: u64,
}

impl Transcript {
    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.generation += 1;
    }

    /// Forgets the entries from `len` on (the conversation went back).
    pub fn truncate(&mut self, len: usize) {
        if len < self.entries.len() {
            self.entries.truncate(len);
            self.generation += 1;
        }
    }

    pub fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.push(Entry::Notice { level, text: text.into() });
    }

    /// Applies a `session/update`; true when it changed the transcript (the
    /// rest, modes and commands and usage, is the session's).
    pub fn apply(&mut self, update: &acp::SessionUpdate) -> bool {
        match update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => self.append(text_of(&chunk.content), |e| match e {
                Entry::Assistant { text } => Some(text),
                _ => None,
            }, |text| Entry::Assistant { text }),
            acp::SessionUpdate::AgentThoughtChunk(chunk) => self.append(text_of(&chunk.content), |e| match e {
                Entry::Thought { text } => Some(text),
                _ => None,
            }, |text| Entry::Thought { text }),
            // The user's own messages come back when a session is loaded.
            acp::SessionUpdate::UserMessageChunk(chunk) => self.append(text_of(&chunk.content), |e| match e {
                Entry::User { text, .. } => Some(text),
                _ => None,
            }, |text| Entry::User { text, context: Vec::new() }),
            acp::SessionUpdate::ToolCall(call) => {
                let tool = Tool {
                    id: call.tool_call_id.to_string(),
                    title: call.title.clone(),
                    kind: kind_of(&call.kind),
                    status: status_of(&call.status),
                    content: call.content.iter().filter_map(content_of).collect(),
                    locations: call.locations.iter().map(|l| Location { path: l.path.clone(), line: l.line }).collect(),
                    input: call.raw_input.clone(),
                    permission: None,
                };
                match self.tool_mut(&tool.id) {
                    Some(existing) => {
                        let permission = existing.permission.take();
                        *existing = Tool { permission, ..tool };
                    }
                    None => self.entries.push(Entry::Tool(tool)),
                }
                self.generation += 1;
                true
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.to_string();
                let fields = &update.fields;
                if self.tool_mut(&id).is_none() {
                    // An update before its call: start one from what it says.
                    self.entries.push(Entry::Tool(Tool {
                        id: id.clone(),
                        title: fields.title.clone().unwrap_or_else(|| "Tool".into()),
                        kind: Kind::Other,
                        status: Status::Pending,
                        content: Vec::new(),
                        locations: Vec::new(),
                        input: None,
                        permission: None,
                    }));
                }
                let Some(tool) = self.tool_mut(&id) else { return false };
                if let Some(title) = &fields.title {
                    tool.title = title.clone();
                }
                if let Some(kind) = &fields.kind {
                    tool.kind = kind_of(kind);
                }
                if let Some(status) = &fields.status {
                    tool.status = status_of(status);
                }
                if let Some(content) = &fields.content {
                    tool.content = content.iter().filter_map(content_of).collect();
                }
                if let Some(locations) = &fields.locations {
                    tool.locations = locations.iter().map(|l| Location { path: l.path.clone(), line: l.line }).collect();
                }
                if let Some(input) = &fields.raw_input {
                    tool.input = Some(input.clone());
                }
                self.generation += 1;
                true
            }
            acp::SessionUpdate::Plan(plan) => {
                let items: Vec<PlanItem> = plan
                    .entries
                    .iter()
                    .map(|e| PlanItem {
                        text: e.content.clone(),
                        status: match e.status {
                            acp::PlanEntryStatus::InProgress => Status::Running,
                            acp::PlanEntryStatus::Completed => Status::Done,
                            _ => Status::Pending,
                        },
                    })
                    .collect();
                // One plan per turn: a newer one replaces it where it stands.
                match self.entries.iter_mut().rev().take_while(|e| !matches!(e, Entry::User { .. })).find_map(|e| match e {
                    Entry::Plan { items } => Some(items),
                    _ => None,
                }) {
                    Some(existing) => *existing = items,
                    None => self.entries.push(Entry::Plan { items }),
                }
                self.generation += 1;
                true
            }
            _ => false,
        }
    }

    fn append(&mut self, text: String, last: impl Fn(&mut Entry) -> Option<&mut String>, new: impl Fn(String) -> Entry) -> bool {
        if text.is_empty() {
            return false;
        }
        match self.entries.last_mut().and_then(last) {
            Some(existing) => existing.push_str(&text),
            None => self.entries.push(new(text)),
        }
        self.generation += 1;
        true
    }

    pub fn tool_mut(&mut self, id: &str) -> Option<&mut Tool> {
        self.entries.iter_mut().rev().find_map(|e| match e {
            Entry::Tool(tool) if tool.id == id => Some(tool),
            _ => None,
        })
    }

    pub fn tool(&self, id: &str) -> Option<&Tool> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Tool(tool) if tool.id == id => Some(tool),
            _ => None,
        })
    }

    /// Marks open permission questions as answered with nothing (the turn
    /// was stopped) and running tools as failed.
    pub fn settle(&mut self) {
        for entry in &mut self.entries {
            if let Entry::Tool(tool) = entry {
                if let Some(permission) = &mut tool.permission {
                    permission.open = false;
                }
                if matches!(tool.status, Status::Pending | Status::Running) {
                    tool.status = Status::Failed;
                }
            }
        }
        self.generation += 1;
    }

    /// The text of the first user message, for a title.
    pub fn first_prompt(&self) -> Option<&str> {
        self.entries.iter().find_map(|e| match e {
            Entry::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// The conversation as Markdown (Copy as Markdown, export).
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            match entry {
                Entry::User { text, .. } => out.push_str(&format!("## You\n\n{text}\n\n")),
                Entry::Assistant { text } => out.push_str(&format!("{text}\n\n")),
                Entry::Thought { .. } => {}
                Entry::Tool(tool) => out.push_str(&format!("- {} {}\n\n", status_mark(tool.status), tool.title)),
                Entry::Plan { items } => {
                    for item in items {
                        let mark = if item.status == Status::Done { "x" } else { " " };
                        out.push_str(&format!("- [{mark}] {}\n", item.text));
                    }
                    out.push('\n');
                }
                Entry::Notice { text, .. } => out.push_str(&format!("> {text}\n\n")),
            }
        }
        out.trim_end().to_string() + "\n"
    }

    /// Display groups: entries as they are, except runs of two or more
    /// finished tool calls, which read as one line until opened.
    pub fn groups(&self) -> Vec<Group> {
        let mut groups = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        let flush = |run: &mut Vec<usize>, groups: &mut Vec<Group>| {
            match run.len() {
                0 => {}
                1 => groups.push(Group::One(run[0])),
                _ => groups.push(Group::Tools(run.clone())),
            }
            run.clear();
        };
        for (ix, entry) in self.entries.iter().enumerate() {
            let foldable = match entry {
                Entry::Tool(tool) => tool.status == Status::Done && tool.permission.as_ref().is_none_or(|p| !p.open) && !has_diff(tool),
                _ => false,
            };
            if foldable {
                run.push(ix);
            } else {
                flush(&mut run, &mut groups);
                groups.push(Group::One(ix));
            }
        }
        flush(&mut run, &mut groups);
        groups
    }

    /// "Read 4 files, edited 2 files, ran 1 command" for a run of tools.
    pub fn summary(&self, run: &[usize]) -> String {
        let mut counts: Vec<(Kind, usize)> = Vec::new();
        for ix in run {
            if let Some(Entry::Tool(tool)) = self.entries.get(*ix) {
                match counts.iter_mut().find(|(k, _)| *k == tool.kind) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((tool.kind, 1)),
                }
            }
        }
        let parts: Vec<String> = counts
            .iter()
            .map(|(kind, n)| {
                let (verb, one, many) = match kind {
                    Kind::Read => ("Read", "file", "files"),
                    Kind::Edit => ("Edited", "file", "files"),
                    Kind::Delete => ("Deleted", "file", "files"),
                    Kind::Move => ("Moved", "file", "files"),
                    Kind::Search => ("Ran", "search", "searches"),
                    Kind::Execute => ("Ran", "command", "commands"),
                    Kind::Think => ("Thought", "time", "times"),
                    Kind::Fetch => ("Fetched", "page", "pages"),
                    Kind::Other => ("Used", "tool", "tools"),
                };
                format!("{verb} {n} {}", if *n == 1 { one } else { many })
            })
            .collect();
        // One sentence: only its first verb capitalized.
        let text = parts.join(", ");
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_string() + &chars.as_str().to_lowercase(),
            None => text,
        }
    }

}

/// One thing the view draws.
#[derive(Clone, Debug, PartialEq)]
pub enum Group {
    One(usize),
    Tools(Vec<usize>),
}

fn has_diff(tool: &Tool) -> bool {
    tool.content.iter().any(|c| matches!(c, ToolContent::Diff { .. }))
}

fn status_mark(status: Status) -> &'static str {
    match status {
        Status::Done => "✓",
        Status::Failed => "✗",
        _ => "…",
    }
}

/// The text of a content block; other kinds as a short mention.
pub fn text_of(block: &acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => format!("[{}]({})", link.name, link.uri),
        acp::ContentBlock::Image(_) => "[image]".into(),
        acp::ContentBlock::Audio(_) => "[audio]".into(),
        acp::ContentBlock::Resource(_) => "[resource]".into(),
        _ => String::new(),
    }
}

fn kind_of(kind: &acp::ToolKind) -> Kind {
    match kind {
        acp::ToolKind::Read => Kind::Read,
        acp::ToolKind::Edit => Kind::Edit,
        acp::ToolKind::Delete => Kind::Delete,
        acp::ToolKind::Move => Kind::Move,
        acp::ToolKind::Search => Kind::Search,
        acp::ToolKind::Execute => Kind::Execute,
        acp::ToolKind::Think => Kind::Think,
        acp::ToolKind::Fetch => Kind::Fetch,
        _ => Kind::Other,
    }
}

fn status_of(status: &acp::ToolCallStatus) -> Status {
    match status {
        acp::ToolCallStatus::InProgress => Status::Running,
        acp::ToolCallStatus::Completed => Status::Done,
        acp::ToolCallStatus::Failed => Status::Failed,
        _ => Status::Pending,
    }
}

fn content_of(content: &acp::ToolCallContent) -> Option<ToolContent> {
    Some(match content {
        acp::ToolCallContent::Content(c) => ToolContent::Text { text: text_of(&c.content) },
        acp::ToolCallContent::Diff(d) => ToolContent::Diff { path: d.path.clone(), old: d.old_text.clone(), new: d.new_text.clone() },
        acp::ToolCallContent::Terminal(t) => ToolContent::Terminal { id: t.terminal_id.to_string() },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(value: Value) -> acp::SessionUpdate {
        serde_json::from_value(value).unwrap()
    }

    fn chunk(kind: &str, text: &str) -> acp::SessionUpdate {
        update(json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }))
    }

    #[test]
    fn chunks_join_into_messages_and_thoughts() {
        let mut t = Transcript::default();
        t.push(Entry::User { text: "Fix it".into(), context: vec![] });
        assert!(t.apply(&chunk("agent_thought_chunk", "Look")));
        t.apply(&chunk("agent_thought_chunk", "ing."));
        t.apply(&chunk("agent_message_chunk", "Done "));
        t.apply(&chunk("agent_message_chunk", "now."));
        assert_eq!(t.entries[1], Entry::Thought { text: "Looking.".into() });
        assert_eq!(t.entries[2], Entry::Assistant { text: "Done now.".into() });
        assert!(!t.apply(&update(json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }))));
        assert_eq!(t.first_prompt(), Some("Fix it"));
    }

    #[test]
    fn tool_calls_update_in_place() {
        let mut t = Transcript::default();
        t.apply(&update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.rs", "kind": "edit", "status": "pending",
            "locations": [{ "path": "/p/a.rs", "line": 3 }] })));
        t.apply(&update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            "content": [{ "type": "diff", "path": "/p/a.rs", "oldText": "a", "newText": "b" }] })));
        let tool = t.tool("t1").unwrap();
        assert_eq!((tool.kind, tool.status), (Kind::Edit, Status::Done));
        assert_eq!(tool.locations[0].line, Some(3));
        assert_eq!(tool.content[0], ToolContent::Diff { path: "/p/a.rs".into(), old: Some("a".into()), new: "b".into() });
        // An update for a call not seen yet starts one.
        t.apply(&update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2", "title": "Run tests", "status": "in_progress" })));
        assert_eq!(t.tool("t2").unwrap().status, Status::Running);
        t.settle();
        assert_eq!(t.tool("t2").unwrap().status, Status::Failed);
    }

    #[test]
    fn plans_replace_each_other_within_a_turn() {
        let mut t = Transcript::default();
        let plan = |status: &str| update(json!({ "sessionUpdate": "plan", "entries": [{ "content": "Write tests", "priority": "high", "status": status }] }));
        t.apply(&plan("pending"));
        t.apply(&chunk("agent_message_chunk", "Working."));
        t.apply(&plan("completed"));
        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entries[0], Entry::Plan { items: vec![PlanItem { text: "Write tests".into(), status: Status::Done }] });
    }

    #[test]
    fn finished_tool_runs_fold_into_a_summary() {
        let mut t = Transcript::default();
        for (id, kind) in [("a", "read"), ("b", "read"), ("c", "execute")] {
            t.apply(&update(json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": id, "kind": kind, "status": "completed" })));
        }
        t.apply(&chunk("agent_message_chunk", "Done."));
        t.apply(&update(json!({ "sessionUpdate": "tool_call", "toolCallId": "d", "title": "d", "kind": "read", "status": "completed" })));
        assert_eq!(t.groups(), vec![Group::Tools(vec![0, 1, 2]), Group::One(3), Group::One(4)]);
        assert_eq!(t.summary(&[0, 1, 2]), "Read 2 files, ran 1 command");
    }

    #[test]
    fn saves_round_trip_and_read_as_markdown() {
        let mut t = Transcript::default();
        t.push(Entry::User { text: "Hi".into(), context: vec!["a.rs".into()] });
        t.apply(&chunk("agent_message_chunk", "Hello."));
        t.notice(Level::Error, "The agent stopped");
        let saved = serde_json::to_string(&t).unwrap();
        let back: Transcript = serde_json::from_str(&saved).unwrap();
        assert_eq!(back.entries, t.entries);
        assert_eq!(t.markdown(), "## You\n\nHi\n\nHello.\n\n> The agent stopped\n");
    }
}
