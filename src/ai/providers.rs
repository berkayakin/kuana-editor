//! The model providers Kuana talks to, how to reach each, and its key.
//!
//! Keys live in the system keychain (service "Kuana AI", one entry per
//! provider), else come from the provider's usual environment variable
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`…). Local servers (Ollama, LM
//! Studio) need none.

use serde_json::Value;

use super::{anthropic, http, openai};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    OpenAi(openai::Flavor),
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// The API's base URL (`…/v1`).
    pub url: &'static str,
    pub wire: Wire,
    /// The environment variable a key may come from.
    pub key_var: Option<&'static str>,
    /// Whether it works without a key (local servers).
    pub keyless: bool,
    /// A model to start with before the list is read (the provider's
    /// current best for coding, where it's stable enough to name).
    pub default_model: Option<&'static str>,
    /// Where to get a key, for the settings page.
    pub keys_url: Option<&'static str>,
}

const COMPAT: Wire = Wire::OpenAi(openai::Flavor::Compatible);

pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: "anthropic",
        name: "Anthropic",
        url: "https://api.anthropic.com/v1",
        wire: Wire::Anthropic,
        key_var: Some("ANTHROPIC_API_KEY"),
        keyless: false,
        default_model: Some("claude-opus-5-5"),
        keys_url: Some("https://console.anthropic.com/settings/keys"),
    },
    ProviderInfo {
        id: "openai",
        name: "OpenAI",
        url: "https://api.openai.com/v1",
        wire: Wire::OpenAi(openai::Flavor::OpenAi),
        key_var: Some("OPENAI_API_KEY"),
        keyless: false,
        default_model: None,
        keys_url: Some("https://platform.openai.com/api-keys"),
    },
    ProviderInfo {
        id: "openrouter",
        name: "OpenRouter",
        url: "https://openrouter.ai/api/v1",
        wire: COMPAT,
        key_var: Some("OPENROUTER_API_KEY"),
        keyless: false,
        default_model: Some("anthropic/claude-opus-5-5"),
        keys_url: Some("https://openrouter.ai/keys"),
    },
    ProviderInfo {
        id: "gemini",
        name: "Google Gemini",
        url: "https://generativelanguage.googleapis.com/v1beta/openai",
        wire: COMPAT,
        key_var: Some("GEMINI_API_KEY"),
        keyless: false,
        default_model: None,
        keys_url: Some("https://aistudio.google.com/apikey"),
    },
    ProviderInfo {
        id: "mistral",
        name: "Mistral",
        url: "https://api.mistral.ai/v1",
        wire: COMPAT,
        key_var: Some("MISTRAL_API_KEY"),
        keyless: false,
        default_model: None,
        keys_url: Some("https://console.mistral.ai/api-keys"),
    },
    ProviderInfo {
        id: "deepseek",
        name: "DeepSeek",
        url: "https://api.deepseek.com/v1",
        wire: COMPAT,
        key_var: Some("DEEPSEEK_API_KEY"),
        keyless: false,
        default_model: Some("deepseek-chat"),
        keys_url: Some("https://platform.deepseek.com/api_keys"),
    },
    ProviderInfo {
        id: "xai",
        name: "xAI",
        url: "https://api.x.ai/v1",
        wire: COMPAT,
        key_var: Some("XAI_API_KEY"),
        keyless: false,
        default_model: None,
        keys_url: Some("https://console.x.ai"),
    },
    ProviderInfo {
        id: "groq",
        name: "Groq",
        url: "https://api.groq.com/openai/v1",
        wire: COMPAT,
        key_var: Some("GROQ_API_KEY"),
        keyless: false,
        default_model: None,
        keys_url: Some("https://console.groq.com/keys"),
    },
    ProviderInfo {
        id: "ollama",
        name: "Ollama",
        url: "http://localhost:11434/v1",
        wire: COMPAT,
        key_var: None,
        keyless: true,
        default_model: None,
        keys_url: None,
    },
    ProviderInfo {
        id: "lmstudio",
        name: "LM Studio",
        url: "http://localhost:1234/v1",
        wire: COMPAT,
        key_var: None,
        keyless: true,
        default_model: None,
        keys_url: None,
    },
    ProviderInfo {
        id: "custom",
        name: "OpenAI-compatible server",
        url: "http://localhost:8000/v1",
        wire: COMPAT,
        key_var: None,
        keyless: true,
        default_model: None,
        keys_url: None,
    },
];

pub fn info(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// A provider ready to call: where, how and with which key.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub provider: &'static ProviderInfo,
    pub url: String,
    pub key: Option<String>,
}

impl Endpoint {
    /// A provider's endpoint, `url` replacing its usual one (settings), the
    /// key from the keychain or environment; an error says what's missing.
    pub fn of(provider: &str, url: Option<&str>) -> Result<Endpoint, String> {
        let info = info(provider).ok_or_else(|| format!("Unknown AI provider \"{provider}\""))?;
        let key = keys::key_for(info);
        if key.is_none() && !info.keyless {
            return Err(format!("No API key for {}: add one in Settings → AI", info.name));
        }
        let url = url.filter(|u| !u.trim().is_empty()).unwrap_or(info.url).trim_end_matches('/').to_string();
        Ok(Endpoint { provider: info, url, key })
    }

    /// Headers every request carries: the key and, for Anthropic, the API version.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        match self.provider.wire {
            Wire::Anthropic => {
                if let Some(key) = &self.key {
                    headers.push(("x-api-key".into(), key.clone()));
                }
                headers.push(("anthropic-version".into(), anthropic::VERSION.into()));
            }
            Wire::OpenAi(_) => {
                if let Some(key) = &self.key {
                    headers.push(("authorization".into(), format!("Bearer {key}")));
                }
                if self.provider.id == "openrouter" {
                    headers.push(("x-title".into(), "Kuana".into()));
                    headers.push(("http-referer".into(), "https://github.com/berkayakin/kuana-editor".into()));
                }
            }
        }
        headers
    }

    /// The models this endpoint offers, `(id, display name)`; blocking.
    pub fn models(&self) -> Result<Vec<(String, String)>, String> {
        let url = match self.provider.wire {
            Wire::Anthropic => format!("{}/models?limit=1000", self.url),
            Wire::OpenAi(_) => format!("{}/models", self.url),
        };
        let value: Value = http::get_json(&url, &self.headers()).map_err(|e| e.to_string())?;
        Ok(match self.provider.wire {
            Wire::Anthropic => anthropic::parse_models(&value),
            Wire::OpenAi(_) => openai::parse_models(&value),
        })
    }
}

/// API keys in the keychain. Tests never touch it.
pub mod keys {
    use super::ProviderInfo;

    const SERVICE: &str = "Kuana AI";

    /// Where a provider's key comes from, for the settings page.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub enum Source {
        Keychain,
        Environment,
        Missing,
    }

    pub fn saved(provider: &str) -> Option<String> {
        if cfg!(test) {
            return None;
        }
        keyring::Entry::new(SERVICE, provider).ok()?.get_password().ok().filter(|k| !k.trim().is_empty())
    }

    pub fn save(provider: &str, key: &str) -> Result<(), String> {
        if cfg!(test) {
            return Ok(());
        }
        let entry = keyring::Entry::new(SERVICE, provider).map_err(|e| e.to_string())?;
        if key.trim().is_empty() {
            let _ = entry.delete_credential();
            return Ok(());
        }
        entry.set_password(key.trim()).map_err(|e| e.to_string())
    }

    pub fn key_for(info: &ProviderInfo) -> Option<String> {
        saved(info.id).or_else(|| info.key_var.and_then(|var| std::env::var(var).ok()).filter(|k| !k.trim().is_empty()))
    }

    pub fn source(info: &ProviderInfo) -> Source {
        if saved(info.id).is_some() {
            Source::Keychain
        } else if info.key_var.is_some_and(|var| std::env::var(var).is_ok_and(|k| !k.trim().is_empty())) {
            Source::Environment
        } else {
            Source::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_is_reachable_by_id_and_unique() {
        for (i, p) in PROVIDERS.iter().enumerate() {
            assert_eq!(info(p.id), Some(p));
            assert!(PROVIDERS[i + 1..].iter().all(|q| q.id != p.id), "{} twice", p.id);
            assert!(p.url.starts_with("http") && !p.url.ends_with('/'));
            assert!(p.keyless || p.key_var.is_some(), "{} has no way to get a key", p.id);
        }
    }

    #[test]
    fn endpoints_need_a_key_unless_local() {
        assert!(Endpoint::of("nope", None).unwrap_err().contains("Unknown"));
        let local = Endpoint::of("ollama", Some("http://box:11434/v1/")).unwrap();
        assert_eq!(local.url, "http://box:11434/v1");
        assert!(local.headers().is_empty());
        // No keychain under test; an environment key may or may not be set.
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            assert!(Endpoint::of("anthropic", None).unwrap_err().contains("No API key"));
        }
        let anthropic = Endpoint { provider: info("anthropic").unwrap(), url: "u".into(), key: Some("k".into()) };
        assert!(anthropic.headers().contains(&("anthropic-version".into(), anthropic::VERSION.into())));
        assert!(anthropic.headers().contains(&("x-api-key".into(), "k".into())));
    }
}
