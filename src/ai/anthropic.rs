//! Anthropic's Messages API: the request body and the streamed events.
//! See the API reference for `POST /v1/messages` with `stream: true`.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::message::{Effort, Message, Part, Request, Role, StopReason, StreamEvent, Usage};
use super::sse;

/// The API version Kuana's requests are written for.
pub const VERSION: &str = "2026-10-21";
/// Lets the API re-run a request on another model when a safety
/// classifier declines it, instead of ending the turn.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Whether a model takes adaptive thinking (Claude 4.6 and later).
pub fn adaptive_thinking(model: &str) -> bool {
    ["opus-4-6", "opus-4-7", "opus-4-8", "opus-5", "sonnet-4-6", "sonnet-5", "haiku-5", "fable", "mythos"].iter().any(|m| model.contains(m))
}

/// The effort a model takes for the one asked: None where it takes none;
/// levels it doesn't have come down to its highest (Opus 4.5 has low to
/// high, the 4.6 models no `xhigh`).
fn effort_for(model: &str, effort: Effort) -> Option<Effort> {
    if model.contains("opus-4-5") {
        return Some(effort.min(Effort::High));
    }
    if !adaptive_thinking(model) {
        return None;
    }
    if (model.contains("opus-4-6") || model.contains("sonnet-4-6")) && effort == Effort::Xhigh {
        return Some(Effort::High);
    }
    Some(effort)
}

/// Whether the server-side refusal fallback applies (and is worth having):
/// the models whose classifiers may decline mid-task.
fn takes_fallback(model: &str) -> bool {
    ["claude-opus-5", "claude-sonnet-5-5", "claude-fable-5-1", "claude-fable-5-2"].iter().any(|m| model.starts_with(m))
}

/// Extra headers a request needs beyond the key and version.
pub fn beta_headers(request: &Request) -> Vec<(String, String)> {
    if takes_fallback(&request.model) { vec![("anthropic-beta".into(), FALLBACK_BETA.into())] } else { Vec::new() }
}

pub fn body(request: &Request) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "stream": true,
        // A message with nothing Anthropic takes (an empty reply, another
        // provider's reasoning alone) is left out: empty content is refused.
        "messages": request.messages.iter().map(message).filter(|m| m["content"].as_array().is_some_and(|c| !c.is_empty())).collect::<Vec<_>>(),
        // The longest stable prefix (tools, system, the conversation so far)
        // is cached: each turn of an agent loop re-sends it.
        "cache_control": { "type": "ephemeral" },
    });
    if !request.system.is_empty() {
        body["system"] = json!(request.system);
    }
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema, "eager_input_streaming": true }))
            .collect();
    }
    if adaptive_thinking(&request.model) {
        body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
    }
    if let Some(effort) = request.effort.and_then(|e| effort_for(&request.model, e)) {
        body["output_config"] = json!({ "effort": effort.key() });
    }
    if takes_fallback(&request.model) {
        body["fallbacks"] = json!("default");
    }
    body
}

fn message(message: &Message) -> Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = message
        .content
        .iter()
        .filter_map(|part| {
            Some(match part {
                Part::Text { text } => json!({ "type": "text", "text": text }),
                Part::Image { media_type, data } => json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } }),
                // Reasoning goes back exactly as Anthropic sent it; without its
                // signature (another provider's), it can't go back at all.
                Part::Thinking { text, signature: Some(signature) } => json!({ "type": "thinking", "thinking": text, "signature": signature }),
                Part::Thinking { signature: None, .. } => return None,
                Part::RedactedThinking { data } => json!({ "type": "redacted_thinking", "data": data }),
                Part::ToolCall { id, name, input } => json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
                Part::ToolResult { id, content, is_error } => json!({ "type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error }),
            })
        })
        .collect();
    json!({ "role": role, "content": content })
}

/// Turns the stream's events into Kuana's, keeping which block each index is.
#[derive(Default)]
pub struct Decoder {
    tools: HashMap<u64, ()>,
}

impl Decoder {
    pub fn event(&mut self, event: &sse::Event) -> Vec<StreamEvent> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else { return Vec::new() };
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        let at = index as usize;
        let str_of = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        match data.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => data.get("message").and_then(|m| m.get("usage")).map(usage).map(StreamEvent::Usage).into_iter().collect(),
            "content_block_start" => {
                let block = data.get("content_block").cloned().unwrap_or_default();
                match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text" => vec![StreamEvent::Text { index: at, text: str_of(&block, "text") }],
                    "thinking" => vec![StreamEvent::Thinking { index: at, text: str_of(&block, "thinking") }],
                    "redacted_thinking" => vec![StreamEvent::Redacted { index: at, data: str_of(&block, "data") }],
                    "tool_use" => {
                        self.tools.insert(index, ());
                        vec![StreamEvent::ToolStart { index: at, id: str_of(&block, "id"), name: str_of(&block, "name") }]
                    }
                    // Server tools, fallback markers and the like: nothing to keep.
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let delta = data.get("delta").cloned().unwrap_or_default();
                match delta.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text_delta" => vec![StreamEvent::Text { index: at, text: str_of(&delta, "text") }],
                    "thinking_delta" => vec![StreamEvent::Thinking { index: at, text: str_of(&delta, "thinking") }],
                    "signature_delta" => vec![StreamEvent::Signature { index: at, signature: str_of(&delta, "signature") }],
                    "input_json_delta" if self.tools.contains_key(&index) => vec![StreamEvent::ToolInput { index: at, json: str_of(&delta, "partial_json") }],
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                let mut events: Vec<StreamEvent> = data.get("usage").map(usage).map(StreamEvent::Usage).into_iter().collect();
                if let Some(stop) = data.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    events.push(StreamEvent::Stop(stop_reason(stop)));
                }
                events
            }
            "error" => {
                let error = data.get("error").cloned().unwrap_or_default();
                let message = error.get("message").and_then(Value::as_str).unwrap_or("The provider reported an error");
                vec![StreamEvent::Error(message.to_string())]
            }
            _ => Vec::new(),
        }
    }
}

fn usage(value: &Value) -> Usage {
    let n = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage { input: n("input_tokens"), output: n("output_tokens"), cache_read: n("cache_read_input_tokens"), cache_write: n("cache_creation_input_tokens") }
}

fn stop_reason(stop: &str) -> StopReason {
    match stop {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        "pause_turn" => StopReason::PauseTurn,
        other => StopReason::Other(other.to_string()),
    }
}

/// The models the key may use, from `GET /v1/models`, newest first.
pub fn parse_models(value: &Value) -> Vec<(String, String)> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let name = m.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some((id, name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::message::{Accumulator, Effort, ToolSpec};

    fn event(name: &str, data: Value) -> sse::Event {
        sse::Event { event: Some(name.into()), data: data.to_string() }
    }

    #[test]
    fn bodies_follow_the_model() {
        let mut request = Request::simple("claude-opus-5-5", "Be brief.", "Hi");
        request.effort = Some(Effort::High);
        request.tools = vec![ToolSpec { name: "read".into(), description: "Reads".into(), schema: json!({ "type": "object" }) }];
        request.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Part::Thinking { text: "".into(), signature: Some("sig".into()) },
                Part::ToolCall { id: "t1".into(), name: "read".into(), input: json!({ "path": "a" }) },
            ],
        });
        request.messages.push(Message { role: Role::User, content: vec![Part::ToolResult { id: "t1".into(), content: "text".into(), is_error: false }] });
        let body = body(&request);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert_eq!(body["messages"][1]["content"][0], json!({ "type": "thinking", "thinking": "", "signature": "sig" }));
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "t1");
        assert!(body.get("temperature").is_none());
        assert_eq!(beta_headers(&request)[0].1, FALLBACK_BETA);

        let old = body_of("claude-haiku-4-5");
        assert!(old.get("thinking").is_none() && old.get("output_config").is_none() && old.get("fallbacks").is_none());
    }

    fn body_of(model: &str) -> Value {
        let mut request = Request::simple(model, "", "Hi");
        request.effort = Some(Effort::Low);
        body(&request)
    }

    #[test]
    fn a_streamed_tool_turn_decodes() {
        let mut decoder = Decoder::default();
        let mut acc = Accumulator::default();
        for e in [
            event("message_start", json!({ "type": "message_start", "message": { "usage": { "input_tokens": 120, "cache_read_input_tokens": 100 } } })),
            event("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "" } })),
            event("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "Need the file." } })),
            event("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "abc" } })),
            event("content_block_start", json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } })),
            event("content_block_delta", json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "Reading." } })),
            event("content_block_start", json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "read", "input": {} } })),
            event("content_block_delta", json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"a.rs\"}" } })),
            event("content_block_stop", json!({ "type": "content_block_stop", "index": 2 })),
            event("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 40 } })),
            event("message_stop", json!({ "type": "message_stop" })),
        ] {
            for ev in decoder.event(&e) {
                acc.push(&ev);
            }
        }
        let reply = acc.finish().unwrap();
        assert_eq!(reply.stop, StopReason::ToolUse);
        assert_eq!(reply.usage, Usage { input: 120, output: 40, cache_read: 100, cache_write: 0 });
        assert_eq!(reply.parts[0], Part::Thinking { text: "Need the file.".into(), signature: Some("abc".into()) });
        assert_eq!(reply.tool_calls().next().unwrap().2, &json!({ "path": "a.rs" }));
    }

    #[test]
    fn errors_and_refusals_come_through() {
        let mut decoder = Decoder::default();
        let error = event("error", json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }));
        assert_eq!(decoder.event(&error), vec![StreamEvent::Error("Overloaded".into())]);
        let refusal = event("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "refusal" } }));
        assert_eq!(decoder.event(&refusal), vec![StreamEvent::Stop(StopReason::Refusal)]);
    }

    #[test]
    fn models_list() {
        let list = json!({ "data": [{ "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5" }, { "id": "claude-haiku-5-5" }] });
        assert_eq!(parse_models(&list), vec![("claude-opus-5-5".into(), "Claude Opus 5.5".into()), ("claude-haiku-5-5".into(), "claude-haiku-5-5".into())]);
    }
}
