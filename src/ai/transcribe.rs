//! Speech to text by a transcription service, for dictation where the
//! system has no speech recognition of its own (Linux) or the settings ask
//! for one: OpenAI's `/audio/transcriptions` and the servers that speak it
//! (Groq's, a local Whisper server as the Custom provider). Blocking: call it
//! off the UI thread.

use serde_json::Value;

use super::providers::{Endpoint, Wire};

/// The model a provider transcribes with unless the settings name one.
pub fn default_model(provider: &str) -> &'static str {
    match provider {
        "openai" => "gpt-4o-mini-transcribe",
        "groq" => "whisper-large-v3-turbo",
        _ => "whisper-1",
    }
}

/// The words in `wav` (a WAV file). `language`: a BCP 47 tag, sent as its
/// language part (`tr-TR` → `tr`); `prompt`: words to expect.
pub fn transcribe(endpoint: &Endpoint, model: &str, wav: Vec<u8>, language: Option<&str>, prompt: &str) -> Result<String, String> {
    if matches!(endpoint.provider.wire, Wire::Anthropic) {
        return Err(format!("{} doesn't turn speech into text: choose OpenAI, Groq or a Whisper server (Custom) for dictation", endpoint.provider.name));
    }
    let boundary = format!("kuana-{:016x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64));
    let mut fields: Vec<(&str, String)> = vec![("model", model.to_string()), ("response_format", "json".to_string())];
    if let Some(language) = language.and_then(|l| l.split(['-', '_']).next()).filter(|l| !l.is_empty()) {
        fields.push(("language", language.to_lowercase()));
    }
    if !prompt.trim().is_empty() {
        fields.push(("prompt", prompt.to_string()));
    }
    let body = multipart(&boundary, &fields, ("file", "speech.wav", "audio/wav", &wav));
    let reply = super::http::post_json_reply(&format!("{}/audio/transcriptions", endpoint.url), &endpoint.headers(), &format!("multipart/form-data; boundary={boundary}"), body).map_err(|e| e.to_string())?;
    reply.get("text").and_then(Value::as_str).map(|t| t.trim().to_string()).ok_or_else(|| "The service answered without text".to_string())
}

/// A multipart/form-data body: text fields, then one file.
fn multipart(boundary: &str, fields: &[(&str, String)], file: (&str, &str, &str, &[u8])) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    let (name, filename, kind, bytes) = file;
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {kind}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{providers, stream::test_server};

    #[test]
    fn speech_goes_up_as_a_form_and_its_text_comes_back() {
        let server = test_server::start(vec![(200, "application/json", r#"{"text": " Fix the cart total. "}"#.into())]);
        let endpoint = Endpoint { provider: providers::info("openai").unwrap(), url: server.url.clone(), key: Some("test-key".into()) };
        let wav = kuana_os::dictation::wav(&[0, 0, 1, 0], 16_000);
        let text = transcribe(&endpoint, "gpt-4o-mini-transcribe", wav, Some("en-US"), "kuana, cart.py").unwrap();
        assert_eq!(text, "Fix the cart total.");
        let requests = server.requests.lock().unwrap();
        let (head, body) = &requests[0];
        assert!(head.starts_with("POST /audio/transcriptions"), "{head}");
        assert!(head.to_lowercase().contains("authorization: bearer test-key"));
        assert!(head.contains("multipart/form-data; boundary=kuana-"));
        assert!(body.contains("name=\"model\"\r\n\r\ngpt-4o-mini-transcribe\r\n"));
        assert!(body.contains("name=\"language\"\r\n\r\nen\r\n"), "the language part only");
        assert!(body.contains("name=\"prompt\"\r\n\r\nkuana, cart.py\r\n"));
        assert!(body.contains("filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF"));
    }

    #[test]
    fn a_service_error_says_what_went_wrong() {
        let server = test_server::start(vec![(401, "application/json", r#"{"error": {"message": "Incorrect API key"}}"#.into())]);
        let endpoint = Endpoint { provider: providers::info("groq").unwrap(), url: server.url.clone(), key: Some("bad".into()) };
        let error = transcribe(&endpoint, default_model("groq"), Vec::new(), None, "").unwrap_err();
        assert!(error.contains("Incorrect API key"), "{error}");
        let anthropic = Endpoint { provider: providers::info("anthropic").unwrap(), url: server.url.clone(), key: Some("k".into()) };
        assert!(transcribe(&anthropic, "x", Vec::new(), None, "").unwrap_err().contains("doesn't turn speech into text"));
    }
}
