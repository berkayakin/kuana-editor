//! A conversation as Kuana keeps it, whatever provider it goes to: what's
//! sent ([`Request`]), what streams back ([`StreamEvent`]), and the reply
//! those events add up to ([`Accumulator`] → [`Reply`]).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    /// Base64 image data.
    Image { media_type: String, data: String },
    /// The model's reasoning. Anthropic's must go back unchanged in a tool
    /// loop, signature included; other providers' are dropped when sent.
    Thinking { text: String, signature: Option<String> },
    RedactedThinking { data: String },
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, content: String, is_error: bool },
}

impl Part {
    pub fn text(text: impl Into<String>) -> Part {
        Part::Text { text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Message {
        Message { role: Role::User, content: vec![Part::text(text)] }
    }

    /// Its text parts, joined.
    pub fn text(&self) -> String {
        self.content.iter().filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None }).collect()
    }
}

/// A tool the model may call: its name, what it does and its arguments'
/// JSON Schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// How hard the model thinks, where the provider lets one say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn key(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub effort: Option<Effort>,
}

impl Request {
    /// A one-shot request: a system prompt and one user message.
    pub fn simple(model: impl Into<String>, system: impl Into<String>, user: impl Into<String>) -> Request {
        Request { model: model.into(), system: system.into(), messages: vec![Message::user(user)], tools: Vec::new(), max_tokens: 8192, effort: None }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    /// Tokens the request held: what's counted against the context window.
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    fn add(&mut self, other: Usage) {
        // Providers report running totals in later events; the larger wins.
        self.input = self.input.max(other.input);
        self.output = self.output.max(other.output);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write = self.cache_write.max(other.cache_write);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    ToolUse,
    /// The provider declined (Anthropic's `refusal`, OpenAI's `content_filter`).
    Refusal,
    /// Anthropic paused a long turn: send the reply back to continue it.
    PauseTurn,
    Other(String),
}

/// What a provider streams, by the reply part each piece belongs to.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Text { index: usize, text: String },
    Thinking { index: usize, text: String },
    Signature { index: usize, signature: String },
    Redacted { index: usize, data: String },
    ToolStart { index: usize, id: String, name: String },
    /// A piece of a tool call's JSON arguments.
    ToolInput { index: usize, json: String },
    Usage(Usage),
    Stop(StopReason),
    Error(String),
}

/// A finished reply.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    pub parts: Vec<Part>,
    pub stop: StopReason,
    pub usage: Usage,
    /// Tool calls whose arguments weren't valid JSON (cut off by the token
    /// limit, say), by id: they're answered with an error, not run.
    pub invalid_tool_inputs: Vec<String>,
}

impl Reply {
    #[cfg(test)]
    pub fn text(&self) -> String {
        Message { role: Role::Assistant, content: self.parts.clone() }.text()
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolCall { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

enum Building {
    Text(String),
    Thinking(String, Option<String>),
    Redacted(String),
    Tool { id: String, name: String, json: String },
}

/// Adds stream events up into the reply.
#[derive(Default)]
pub struct Accumulator {
    parts: Vec<(usize, Building)>,
    stop: Option<StopReason>,
    usage: Usage,
    error: Option<String>,
}

impl Accumulator {
    pub fn push(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Text { index, text } => {
                if let Building::Text(s) = self.part(*index, || Building::Text(String::new())) {
                    s.push_str(text);
                }
            }
            StreamEvent::Thinking { index, text } => {
                if let Building::Thinking(s, _) = self.part(*index, || Building::Thinking(String::new(), None)) {
                    s.push_str(text);
                }
            }
            StreamEvent::Signature { index, signature } => {
                if let Building::Thinking(_, s) = self.part(*index, || Building::Thinking(String::new(), None)) {
                    s.get_or_insert_with(String::new).push_str(signature);
                }
            }
            StreamEvent::Redacted { index, data } => {
                let data = data.clone();
                self.part(*index, move || Building::Redacted(data));
            }
            StreamEvent::ToolStart { index, id, name } => {
                let (id, name) = (id.clone(), name.clone());
                self.part(*index, move || Building::Tool { id, name, json: String::new() });
            }
            StreamEvent::ToolInput { index, json } => {
                if let Building::Tool { json: s, .. } = self.part(*index, || Building::Tool { id: String::new(), name: String::new(), json: String::new() }) {
                    s.push_str(json);
                }
            }
            StreamEvent::Usage(usage) => self.usage.add(*usage),
            StreamEvent::Stop(stop) => self.stop = Some(stop.clone()),
            StreamEvent::Error(error) => self.error = Some(error.clone()),
        }
    }

    fn part(&mut self, index: usize, new: impl FnOnce() -> Building) -> &mut Building {
        let at = match self.parts.iter().position(|(i, _)| *i == index) {
            Some(at) => at,
            None => {
                self.parts.push((index, new()));
                self.parts.len() - 1
            }
        };
        &mut self.parts[at].1
    }

    /// The reply, or the error the stream ended with.
    pub fn finish(mut self) -> Result<Reply, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        self.parts.sort_by_key(|(i, _)| *i);
        let mut reply = Reply { stop: self.stop.unwrap_or_default(), usage: self.usage, ..Reply::default() };
        for (_, part) in self.parts {
            reply.parts.push(match part {
                Building::Text(text) if text.is_empty() => continue,
                Building::Text(text) => Part::Text { text },
                Building::Thinking(text, signature) => Part::Thinking { text, signature },
                Building::Redacted(data) => Part::RedactedThinking { data },
                Building::Tool { id, name, json } => {
                    let input = if json.trim().is_empty() { Ok(Value::Object(Default::default())) } else { serde_json::from_str::<Value>(&json) };
                    let input = match input {
                        Ok(value) if value.is_object() => value,
                        _ => {
                            reply.invalid_tool_inputs.push(id.clone());
                            Value::Object(Default::default())
                        }
                    };
                    Part::ToolCall { id, name, input }
                }
            });
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_add_up_in_part_order() {
        let mut acc = Accumulator::default();
        for event in [
            StreamEvent::Thinking { index: 0, text: "Let me ".into() },
            StreamEvent::Thinking { index: 0, text: "look.".into() },
            StreamEvent::Signature { index: 0, signature: "sig".into() },
            StreamEvent::ToolStart { index: 2, id: "t1".into(), name: "read_file".into() },
            StreamEvent::Text { index: 1, text: "Reading ".into() },
            StreamEvent::ToolInput { index: 2, json: "{\"path\":".into() },
            StreamEvent::Text { index: 1, text: "it.".into() },
            StreamEvent::ToolInput { index: 2, json: "\"a.rs\"}".into() },
            StreamEvent::Usage(Usage { input: 10, ..Default::default() }),
            StreamEvent::Usage(Usage { output: 5, ..Default::default() }),
            StreamEvent::Stop(StopReason::ToolUse),
        ] {
            acc.push(&event);
        }
        let reply = acc.finish().unwrap();
        assert_eq!(reply.parts, vec![
            Part::Thinking { text: "Let me look.".into(), signature: Some("sig".into()) },
            Part::text("Reading it."),
            Part::ToolCall { id: "t1".into(), name: "read_file".into(), input: json!({ "path": "a.rs" }) },
        ]);
        assert_eq!(reply.stop, StopReason::ToolUse);
        assert_eq!((reply.usage.input, reply.usage.output), (10, 5));
        assert_eq!(reply.text(), "Reading it.");
        assert_eq!(reply.tool_calls().count(), 1);
    }

    #[test]
    fn cut_off_tool_arguments_are_flagged_and_errors_win() {
        let mut acc = Accumulator::default();
        acc.push(&StreamEvent::ToolStart { index: 0, id: "t1".into(), name: "write_file".into() });
        acc.push(&StreamEvent::ToolInput { index: 0, json: "{\"path\": \"a".into() });
        acc.push(&StreamEvent::Stop(StopReason::MaxTokens));
        let reply = acc.finish().unwrap();
        assert_eq!(reply.invalid_tool_inputs, vec!["t1".to_string()]);

        let mut acc = Accumulator::default();
        acc.push(&StreamEvent::Text { index: 0, text: "hi".into() });
        acc.push(&StreamEvent::Error("overloaded".into()));
        assert_eq!(acc.finish(), Err("overloaded".into()));
    }

    #[test]
    fn a_tool_without_arguments_gets_an_empty_object() {
        let mut acc = Accumulator::default();
        acc.push(&StreamEvent::ToolStart { index: 0, id: "t1".into(), name: "list".into() });
        let reply = acc.finish().unwrap();
        assert_eq!(reply.parts, vec![Part::ToolCall { id: "t1".into(), name: "list".into(), input: json!({}) }]);
        assert!(reply.invalid_tool_inputs.is_empty());
    }
}
