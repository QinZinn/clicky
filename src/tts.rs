//! Text-to-speech through ElevenLabs.

use serde_json::{Value, json};

pub const ELEVENLABS_BASE_URL: &str = "https://api.elevenlabs.io";

#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("{0}")]
    NotConfigured(String),
    #[error("could not reach ElevenLabs: {0}")]
    Network(String),
    #[error("ElevenLabs returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
}

/// Voice ids are interpolated into a URL path, so only allow what real ids
/// contain. This keeps a typo (or a pasted URL) from changing the request.
pub fn is_valid_voice_id(voice_id: &str) -> bool {
    !voice_id.is_empty()
        && voice_id.len() <= 64
        && voice_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

fn error_message(body: &str) -> String {
    // ElevenLabs: {"detail": {"status": "...", "message": "..."}} or {"detail": "..."}
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = value.pointer("/detail/message").and_then(Value::as_str) {
            return message.to_string();
        }
        if let Some(message) = value.get("detail").and_then(Value::as_str) {
            return message.to_string();
        }
    }
    body.chars().take(300).collect()
}

/// Converts `text` to MP3 audio.
pub async fn synthesize(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    voice_id: &str,
    model_id: &str,
    text: &str,
) -> Result<Vec<u8>, TtsError> {
    if api_key.is_empty() {
        return Err(TtsError::NotConfigured(
            "Add your ElevenLabs API key in Settings, or turn voice replies off.".into(),
        ));
    }
    if !is_valid_voice_id(voice_id) {
        return Err(TtsError::NotConfigured(
            "The ElevenLabs voice id looks wrong. It should be letters and digits only.".into(),
        ));
    }

    let url = format!(
        "{}/v1/text-to-speech/{voice_id}",
        base_url.trim_end_matches('/')
    );
    let response = http
        .post(url)
        .header("xi-api-key", api_key)
        .header("accept", "audio/mpeg")
        .json(&json!({
            "text": text,
            "model_id": model_id,
            "voice_settings": {"stability": 0.5, "similarity_boost": 0.75},
        }))
        .send()
        .await
        .map_err(|error| TtsError::Network(error.without_url().to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(TtsError::Http {
            status: status.as_u16(),
            message: error_message(&body),
        });
    }
    let audio = response
        .bytes()
        .await
        .map_err(|error| TtsError::Network(error.without_url().to_string()))?;
    Ok(audio.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_ids_are_restricted_to_safe_characters() {
        assert!(is_valid_voice_id("21m00Tcm4TlvDq8ikWAM"));
        assert!(is_valid_voice_id("my_voice-2"));
        assert!(!is_valid_voice_id(""));
        assert!(!is_valid_voice_id("../admin"));
        assert!(!is_valid_voice_id("abc/def"));
        assert!(!is_valid_voice_id("abc?x=1"));
        assert!(!is_valid_voice_id(&"a".repeat(65)));
    }

    #[test]
    fn extracts_elevenlabs_error_messages() {
        assert_eq!(
            error_message(
                r#"{"detail":{"status":"quota_exceeded","message":"You have run out of credits."}}"#
            ),
            "You have run out of credits."
        );
        assert_eq!(
            error_message(r#"{"detail":"Invalid API key"}"#),
            "Invalid API key"
        );
        assert_eq!(error_message("gateway timeout"), "gateway timeout");
    }
}
