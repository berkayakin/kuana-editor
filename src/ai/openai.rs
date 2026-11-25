//! OpenAI's chat completions, which most providers speak (OpenAI,
//! OpenRouter, Ollama, LM Studio, Groq, Mistral, DeepSeek, xAI, Gemini's
//! OpenAI endpoint, a custom server): the request body and the streamed
//! chunks, `data: {…}` lines ending with `data: [DONE]`.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::message::{Part, Request, Role, StopReason, StreamEvent, Usage};
use super::sse;

/// Indexes of the reply's parts: reasoning, then text, then tool calls in
/// the order the provider numbers them.
const THINKING: usize = 0;
const TEXT: usize = 1;
const FIRST_TOOL: usize = 2;

/// How a provider names the output limit: OpenAI's own API wants
/// `max_completion_tokens` (its reasoning models refuse `max_tokens`), the
/// rest `max_tokens`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    OpenAi,
    Compatible,
}

pub fn body(request: &Request, flavor: Flavor) -> Value {
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({ "role": "system", "content": request.system }));
    }
    for message in &request.messages {
        match message.role {
            Role::User => {
                // Tool results are messages of their own, before what the user adds.
                let mut parts = Vec::new();
                for part in &message.content {
                    match part {
                        Part::ToolResult { id, content, is_error } => {
                            let content = if *is_error { format!("Error: {content}") } else { content.clone() };
                            messages.push(json!({ "role": "tool", "tool_call_id": id, "content": content }));
                        }
                        Part::Text { text } => parts.push(json!({ "type": "text", "text": text })),
                        Part::Image { media_type, data } => {
                            parts.push(json!({ "type": "image_url", "image_url": { "url": format!("data:{media_type};base64,{data}") } }))
                        }
                        _ => {}
                    }
                }
                if parts.len() == 1 && parts[0]["type"] == "text" {
                    messages.push(json!({ "role": "user", "content": parts[0]["text"] }));
                } else if !parts.is_empty() {
                    messages.push(json!({ "role": "user", "content": parts }));
                }
            }
            Role::Assistant => {
                let text = message.text();
                let calls: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        Part::ToolCall { id, name, input } => {
                            Some(json!({ "id": id, "type": "function", "function": { "name": name, "arguments": input.to_string() } }))
                        }
                        _ => None,
                    })
                    .collect();
                // Nothing to send back (an empty reply, reasoning alone):
                // left out, since content may be empty only beside tool calls.
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut m = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
                if !calls.is_empty() {
                    m["tool_calls"] = json!(calls);
                }
                messages.push(m);
            }
        }
    }
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let limit = match flavor {
        Flavor::OpenAi => "max_completion_tokens",
        Flavor::Compatible => "max_tokens",
    };
    body[limit] = json!(request.max_tokens);
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.schema } }))
            .collect();
    }
    if let (Flavor::OpenAi, Some(effort)) = (flavor, request.effort) {
        // OpenAI's reasoning models take low/medium/high; more is high.
        let effort = match effort.key() {
            "low" => "low",
            "medium" => "medium",
            _ => "high",
        };
        body["reasoning_effort"] = json!(effort);
    }
    body
}

/// Turns the chunks into Kuana's events.
#[derive(Default)]
pub struct Decoder {
    /// Tool calls by the provider's index: their part index.
    tools: HashMap<u64, usize>,
}

impl Decoder {
    pub fn event(&mut self, event: &sse::Event) -> Vec<StreamEvent> {
        if event.data.trim() == "[DONE]" {
            return Vec::new();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else { return Vec::new() };
        let mut events = Vec::new();
        if let Some(error) = chunk.get("error") {
            let message = error.get("message").and_then(Value::as_str).or_else(|| error.as_str()).unwrap_or("The provider reported an error");
            return vec![StreamEvent::Error(message.to_string())];
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            let n = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
            let cached = u.get("prompt_tokens_details").map_or(0, |d| n(d, "cached_tokens"));
            events.push(StreamEvent::Usage(Usage { input: n(u, "prompt_tokens").saturating_sub(cached), output: n(u, "completion_tokens"), cache_read: cached, cache_write: 0 }));
        }
        let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else { return events };
        let delta = choice.get("delta").cloned().unwrap_or_default();
        // Servers name it one way or the other (some send both, the same text).
        if let Some(text) = ["reasoning_content", "reasoning"].iter().find_map(|key| delta.get(*key).and_then(Value::as_str).filter(|t| !t.is_empty())) {
            events.push(StreamEvent::Thinking { index: THINKING, text: text.to_string() });
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            events.push(StreamEvent::Text { index: TEXT, text: text.to_string() });
        }
        for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let n = call.get("index").and_then(Value::as_u64).unwrap_or(self.tools.len() as u64);
            let function = call.get("function").cloned().unwrap_or_default();
            let index = match self.tools.get(&n) {
                Some(index) => *index,
                None => {
                    let index = FIRST_TOOL + self.tools.len();
                    self.tools.insert(n, index);
                    let id = call.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("call_{n}"));
                    let name = function.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    events.push(StreamEvent::ToolStart { index, id, name });
                    index
                }
            };
            if let Some(args) = function.get("arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
                events.push(StreamEvent::ToolInput { index, json: args.to_string() });
            }
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            events.push(StreamEvent::Stop(match finish {
                "stop" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "content_filter" => StopReason::Refusal,
                other => StopReason::Other(other.to_string()),
            }));
        }
        events
    }
}

/// `GET /models`: model ids, sorted.
pub fn parse_models(value: &Value) -> Vec<(String, String)> {
    let mut models: Vec<(String, String)> = value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let name = m.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some((id, name))
        })
        .collect();
    models.sort();
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::message::{Accumulator, Effort, Message, ToolSpec};

    fn data(value: Value) -> sse::Event {
        sse::Event { event: None, data: value.to_string() }
    }

    #[test]
    fn conversations_map_onto_chat_messages() {
        let mut request = Request::simple("gpt-test", "Be brief.", "Read a.rs");
        request.effort = Some(Effort::Max);
        request.tools = vec![ToolSpec { name: "read".into(), description: "Reads".into(), schema: json!({ "type": "object" }) }];
        request.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Part::Thinking { text: "hm".into(), signature: None },
                Part::text("Reading."),
                Part::ToolCall { id: "c1".into(), name: "read".into(), input: json!({ "path": "a.rs" }) },
            ],
        });
        request.messages.push(Message {
            role: Role::User,
            content: vec![Part::ToolResult { id: "c1".into(), content: "no such file".into(), is_error: true }, Part::text("Try b.rs")],
        });
        let body = body(&request, Flavor::OpenAi);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0], json!({ "role": "system", "content": "Be brief." }));
        assert_eq!(messages[1], json!({ "role": "user", "content": "Read a.rs" }));
        assert_eq!(messages[2]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(messages[2]["content"], "Reading.");
        assert_eq!(messages[3], json!({ "role": "tool", "tool_call_id": "c1", "content": "Error: no such file" }));
        assert_eq!(messages[4], json!({ "role": "user", "content": "Try b.rs" }));
        assert_eq!(body["max_completion_tokens"], 8192);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["tools"][0]["function"]["name"], "read");

        let other = super::body(&Request::simple("llama", "", "hi"), Flavor::Compatible);
        assert_eq!(other["max_tokens"], 8192);
        assert!(other.get("reasoning_effort").is_none() && other.get("tools").is_none());
    }

    #[test]
    fn chunks_decode_into_a_reply() {
        let mut decoder = Decoder::default();
        let mut acc = Accumulator::default();
        for chunk in [
            json!({ "choices": [{ "delta": { "role": "assistant", "reasoning_content": "Think." } }] }),
            json!({ "choices": [{ "delta": { "content": "Calling" } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "id": "call_a", "function": { "name": "read", "arguments": "" } }] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "{\"path\":" } }] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "\"a\"}" } }] } }] }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            json!({ "choices": [], "usage": { "prompt_tokens": 50, "completion_tokens": 7, "prompt_tokens_details": { "cached_tokens": 20 } } }),
        ] {
            for ev in decoder.event(&data(chunk)) {
                acc.push(&ev);
            }
        }
        assert!(decoder.event(&sse::Event { event: None, data: "[DONE]".into() }).is_empty());
        let reply = acc.finish().unwrap();
        assert_eq!(reply.stop, StopReason::ToolUse);
        assert_eq!(reply.text(), "Calling");
        assert_eq!(reply.usage, Usage { input: 30, output: 7, cache_read: 20, cache_write: 0 });
        assert_eq!(reply.tool_calls().next(), Some(("call_a", "read", &json!({ "path": "a" }))));
        assert!(matches!(reply.parts[0], Part::Thinking { .. }));
    }

    #[test]
    fn stream_errors_and_models() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.event(&data(json!({ "error": { "message": "bad key" } }))), vec![StreamEvent::Error("bad key".into())]);
        assert_eq!(decoder.event(&data(json!({ "error": "model not found" }))), vec![StreamEvent::Error("model not found".into())]);
        let list = json!({ "data": [{ "id": "b" }, { "id": "a", "name": "Model A" }] });
        assert_eq!(parse_models(&list), vec![("a".into(), "Model A".into()), ("b".into(), "b".into())]);
    }
}
