//! Model providers for Kuana's AI features: Anthropic's Messages API and
//! OpenAI-style chat completions (which most other providers and local
//! servers speak), streamed, with tool calls and reasoning, on threads of
//! their own. GPUI-free: the session views, Kuana's own agent, inline assist
//! and commit messages build on [`stream::start`].
//!
//! Nothing is sent anywhere until the user picks a provider (`ai.provider`
//! in settings, only the user's own settings, never a project's) and gives
//! it a key; see `plans/ai.md`.

pub mod anthropic;
pub mod http;
pub mod message;
pub mod openai;
pub mod providers;
pub mod sse;
pub mod stream;

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;

pub use message::{Effort, Message, Part, Request, Role, StopReason, StreamEvent, ToolSpec};
pub use providers::{Endpoint, PROVIDERS};

/// The `ai` settings object.
#[derive(Deserialize, JsonSchema, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AiSettings {
    /// The provider Kuana's features use (`anthropic`, `openai`, `ollama`…);
    /// none until chosen.
    pub provider: Option<String>,
    /// The model, by the provider's id.
    pub model: Option<String>,
    /// How hard the model thinks where it can say (`low` … `max`).
    pub effort: Option<Effort>,
    /// Per provider: another URL (a proxy, a remote Ollama, the custom
    /// server's).
    pub providers: BTreeMap<String, ProviderSettings>,
}

#[derive(Deserialize, JsonSchema, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ProviderSettings {
    pub url: Option<String>,
}

/// What a feature calls: the endpoint and model the settings choose.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub endpoint: Endpoint,
    pub model: String,
    pub effort: Option<Effort>,
}

impl AiSettings {
    /// The provider and model to use, or why there's none yet.
    pub fn choice(&self) -> Result<Choice, String> {
        let provider = self.provider.as_deref().ok_or("No AI provider chosen: pick one in Settings → AI")?;
        let url = self.providers.get(provider).and_then(|p| p.url.as_deref());
        let endpoint = Endpoint::of(provider, url)?;
        let model = self
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| endpoint.provider.default_model.map(str::to_string))
            .ok_or_else(|| format!("No model chosen for {}: pick one in Settings → AI", endpoint.provider.name))?;
        Ok(Choice { endpoint, model, effort: self.effort })
    }
}

impl Choice {
    /// Starts a request on the chosen model (its `model` and, unless set,
    /// `effort` filled in).
    pub fn stream(&self, mut request: Request) -> stream::Stream {
        request.model = self.model.clone();
        if request.effort.is_none() {
            request.effort = self.effort;
        }
        stream::start(self.endpoint.clone(), request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_needs_a_provider_and_a_model() {
        assert!(AiSettings::default().choice().unwrap_err().contains("No AI provider"));
        let ollama: AiSettings = serde_json::from_value(serde_json::json!({ "provider": "ollama" })).unwrap();
        assert!(ollama.choice().unwrap_err().contains("No model chosen for Ollama"));
        let ollama: AiSettings =
            serde_json::from_value(serde_json::json!({ "provider": "ollama", "model": "qwen3-coder", "providers": { "ollama": { "url": "http://gpu:11434/v1" } } }))
                .unwrap();
        let choice = ollama.choice().unwrap();
        assert_eq!((choice.model.as_str(), choice.endpoint.url.as_str()), ("qwen3-coder", "http://gpu:11434/v1"));
    }
}
