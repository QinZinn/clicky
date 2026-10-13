//! Streaming chat client for every supported provider.
//!
//! One public entry point, [`chat`], builds the right request for the
//! provider's wire format, streams the reply, and hands text to the caller as
//! it arrives. If a model rejects screenshots, it transparently retries once
//! without them.

use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::config::{Secret, Settings};
use crate::providers::{self, WireFormat};
use crate::sse::{SseEvent, SseParser};

/// One earlier turn of the conversation, kept so the model remembers context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub user: String,
    pub assistant: String,
}

/// A JPEG screenshot plus the caption that tells the model which screen it is
/// and how large the image is (so its coordinates match our scaling).
#[derive(Clone)]
pub struct ChatImage {
    pub jpeg: Vec<u8>,
    pub label: String,
}

pub struct ChatInput<'a> {
    pub system_prompt: &'a str,
    pub history: &'a [Exchange],
    pub user_text: &'a str,
    pub images: &'a [ChatImage],
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("{0}")]
    Config(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("the provider returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("the stream failed: {0}")]
    Stream(String),
    #[error(
        "the model returned no text. Reasoning models can spend the whole token budget \
         thinking; try raising 'Max output tokens' in settings."
    )]
    EmptyResponse,
}

impl LlmError {
    /// True when the failure looks like "this model cannot take images", so a
    /// text-only retry is worth trying.
    pub fn looks_like_image_rejection(&self) -> bool {
        let LlmError::Http { status, body } = self else {
            return false;
        };
        if !matches!(*status, 400 | 413 | 415 | 422) {
            return false;
        }
        let body = body.to_lowercase();
        [
            "image",
            "vision",
            "multimodal",
            "modalit",
            "content type",
            "image_url",
        ]
        .iter()
        .any(|needle| body.contains(needle))
    }
}

/// Everything needed to send one request. Built from [`Settings`].
#[derive(Debug, Clone)]
pub struct LlmTarget {
    pub format: WireFormat,
    pub endpoint: String,
    pub api_key: Secret,
    pub model: String,
    pub max_output_tokens: u32,
    pub max_tokens_field: &'static str,
}

impl LlmTarget {
    pub fn from_settings(settings: &Settings) -> Result<LlmTarget, LlmError> {
        let provider_config = settings.active_provider_config();
        let preset = providers::preset(&settings.active_provider);
        let provider_name = preset.map(|preset| preset.name).unwrap_or("the provider");

        if provider_config.base_url.trim().is_empty() {
            return Err(LlmError::Config(format!(
                "Set a base URL for {provider_name} in settings."
            )));
        }
        if provider_config.model.trim().is_empty() {
            return Err(LlmError::Config(format!(
                "Choose a model for {provider_name} in settings."
            )));
        }
        let api_key = settings.active_api_key();
        if preset.is_some_and(|preset| preset.needs_api_key) && api_key.is_empty() {
            return Err(LlmError::Config(format!(
                "Add your {provider_name} API key in settings."
            )));
        }

        let format = providers::resolve_wire_format(
            &settings.active_provider,
            provider_config.api_style,
            &provider_config.model,
        )
        .map_err(|error| LlmError::Config(error.to_string()))?;

        Ok(LlmTarget {
            format,
            endpoint: providers::endpoint_url(&provider_config.base_url, format),
            api_key: Secret::new(api_key),
            model: provider_config.model.trim().to_string(),
            max_output_tokens: settings.max_output_tokens.max(64),
            max_tokens_field: providers::max_tokens_field_name(&provider_config.base_url),
        })
    }
}

pub fn build_http_client(settings: &Settings) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("clicky/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        // Idle timeout between chunks, so long answers are not cut off.
        .read_timeout(Duration::from_secs(settings.request_timeout_secs.max(10)))
        .build()
        .expect("static reqwest client configuration is valid")
}

fn encode_jpeg_as_base64(jpeg: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(jpeg)
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

pub fn build_request_body(
    target: &LlmTarget,
    input: &ChatInput<'_>,
    include_images: bool,
) -> Value {
    let images: &[ChatImage] = if include_images { input.images } else { &[] };
    match target.format {
        WireFormat::OpenAiChat => build_openai_chat_body(target, input, images),
        WireFormat::OpenAiResponses => build_openai_responses_body(target, input, images),
        WireFormat::AnthropicMessages => build_anthropic_body(target, input, images),
    }
}

fn build_openai_chat_body(
    target: &LlmTarget,
    input: &ChatInput<'_>,
    images: &[ChatImage],
) -> Value {
    let mut messages = vec![json!({"role": "system", "content": input.system_prompt})];
    for exchange in input.history {
        messages.push(json!({"role": "user", "content": exchange.user}));
        messages.push(json!({"role": "assistant", "content": exchange.assistant}));
    }

    // Plain-string content when there are no images: some servers and
    // text-only models reject the array form.
    let user_content = if images.is_empty() {
        json!(input.user_text)
    } else {
        let mut parts = Vec::new();
        for image in images {
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:image/jpeg;base64,{}", encode_jpeg_as_base64(&image.jpeg))}
            }));
            parts.push(json!({"type": "text", "text": image.label}));
        }
        parts.push(json!({"type": "text", "text": input.user_text}));
        json!(parts)
    };
    messages.push(json!({"role": "user", "content": user_content}));

    let mut body = json!({
        "model": target.model,
        "stream": true,
        "messages": messages,
    });
    body[target.max_tokens_field] = json!(target.max_output_tokens);
    body
}

fn build_openai_responses_body(
    target: &LlmTarget,
    input: &ChatInput<'_>,
    images: &[ChatImage],
) -> Value {
    let mut items = Vec::new();
    for exchange in input.history {
        items.push(json!({"role": "user", "content": exchange.user}));
        items.push(json!({"role": "assistant", "content": exchange.assistant}));
    }

    let mut parts = Vec::new();
    for image in images {
        parts.push(json!({
            "type": "input_image",
            "image_url": format!("data:image/jpeg;base64,{}", encode_jpeg_as_base64(&image.jpeg))
        }));
        parts.push(json!({"type": "input_text", "text": image.label}));
    }
    parts.push(json!({"type": "input_text", "text": input.user_text}));
    items.push(json!({"role": "user", "content": parts}));

    json!({
        "model": target.model,
        "instructions": input.system_prompt,
        "input": items,
        "stream": true,
        // Do not ask the provider to retain screenshots of the user's screen.
        "store": false,
        "max_output_tokens": target.max_output_tokens,
    })
}

fn build_anthropic_body(target: &LlmTarget, input: &ChatInput<'_>, images: &[ChatImage]) -> Value {
    let mut messages = Vec::new();
    for exchange in input.history {
        messages.push(json!({"role": "user", "content": exchange.user}));
        messages.push(json!({"role": "assistant", "content": exchange.assistant}));
    }

    let mut parts = Vec::new();
    for image in images {
        parts.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/jpeg",
                "data": encode_jpeg_as_base64(&image.jpeg),
            }
        }));
        parts.push(json!({"type": "text", "text": image.label}));
    }
    parts.push(json!({"type": "text", "text": input.user_text}));
    messages.push(json!({"role": "user", "content": parts}));

    json!({
        "model": target.model,
        "max_tokens": target.max_output_tokens,
        "stream": true,
        "system": input.system_prompt,
        "messages": messages,
    })
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

fn apply_authentication(
    request: reqwest::RequestBuilder,
    format: WireFormat,
    endpoint: &str,
    api_key: &Secret,
) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        return request;
    }
    match format {
        WireFormat::AnthropicMessages => {
            let request = request
                .header("x-api-key", api_key.expose())
                .header("anthropic-version", "2023-06-01");
            // Gateways that expose Anthropic's format (OpenCode Zen, ...) may
            // expect a bearer token instead, so send both to them. The real
            // Anthropic API gets only its own header.
            if endpoint.contains("api.anthropic.com") {
                request
            } else {
                request.bearer_auth(api_key.expose())
            }
        }
        WireFormat::OpenAiChat | WireFormat::OpenAiResponses => {
            request.bearer_auth(api_key.expose())
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    Text(String),
    Finished,
    Ignore,
}

fn error_message_from_json(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    if let Some(message) = error.as_str() {
        return Some(message.to_string());
    }
    Some(error.to_string())
}

fn decode_event(format: WireFormat, event: &SseEvent) -> Result<DecodedEvent, LlmError> {
    let data = event.data.trim();
    if data == "[DONE]" {
        return Ok(DecodedEvent::Finished);
    }
    // Providers occasionally send non-JSON keep-alives; skip rather than fail.
    let Ok(payload) = serde_json::from_str::<Value>(data) else {
        return Ok(DecodedEvent::Ignore);
    };

    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");

    if payload_type == "error" || payload_type == "response.failed" {
        let message = error_message_from_json(&payload)
            .or_else(|| {
                payload
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(LlmError::Stream(message));
    }
    if payload.get("error").is_some() && payload.get("choices").is_none() {
        return Err(LlmError::Stream(
            error_message_from_json(&payload).unwrap_or_else(|| "unknown error".to_string()),
        ));
    }

    match format {
        WireFormat::OpenAiChat => {
            if let Some(error_message) = payload
                .pointer("/choices/0/error/message")
                .and_then(Value::as_str)
            {
                return Err(LlmError::Stream(error_message.to_string()));
            }
            let content = payload.pointer("/choices/0/delta/content");
            match content {
                Some(Value::String(text)) if !text.is_empty() => {
                    Ok(DecodedEvent::Text(text.clone()))
                }
                // A few servers send content as a list of {"text": ...} parts.
                Some(Value::Array(parts)) => {
                    let text: String = parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect();
                    if text.is_empty() {
                        Ok(DecodedEvent::Ignore)
                    } else {
                        Ok(DecodedEvent::Text(text))
                    }
                }
                _ => Ok(DecodedEvent::Ignore),
            }
        }
        WireFormat::AnthropicMessages => match payload_type {
            "content_block_delta" => {
                let delta_type = payload.pointer("/delta/type").and_then(Value::as_str);
                match (
                    delta_type,
                    payload.pointer("/delta/text").and_then(Value::as_str),
                ) {
                    (Some("text_delta"), Some(text)) if !text.is_empty() => {
                        Ok(DecodedEvent::Text(text.to_string()))
                    }
                    _ => Ok(DecodedEvent::Ignore),
                }
            }
            "message_stop" => Ok(DecodedEvent::Finished),
            _ => Ok(DecodedEvent::Ignore),
        },
        WireFormat::OpenAiResponses => match payload_type {
            "response.output_text.delta" => match payload.get("delta").and_then(Value::as_str) {
                Some(text) if !text.is_empty() => Ok(DecodedEvent::Text(text.to_string())),
                _ => Ok(DecodedEvent::Ignore),
            },
            "response.completed" | "response.incomplete" => Ok(DecodedEvent::Finished),
            _ => Ok(DecodedEvent::Ignore),
        },
    }
}

/// Pulls a human-readable message out of a JSON error body, falling back to
/// the (truncated) raw text.
pub fn summarize_error_body(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = error_message_from_json(&value) {
            return message;
        }
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            return message.to_string();
        }
    }
    let trimmed = body.trim();
    if trimmed.chars().count() > 400 {
        let shortened: String = trimmed.chars().take(400).collect();
        format!("{shortened}…")
    } else {
        trimmed.to_string()
    }
}

/// Sends one streaming request and returns the full reply text. `on_text`
/// receives each new piece as it arrives.
pub async fn stream_chat<F>(
    http: &reqwest::Client,
    target: &LlmTarget,
    input: &ChatInput<'_>,
    include_images: bool,
    on_text: &mut F,
) -> Result<String, LlmError>
where
    F: FnMut(&str) + Send,
{
    let body = build_request_body(target, input, include_images);
    let request = http
        .post(&target.endpoint)
        .header("accept", "text/event-stream")
        .json(&body);
    let request = apply_authentication(request, target.format, &target.endpoint, &target.api_key);

    let response = request
        .send()
        .await
        .map_err(|error| LlmError::Network(error.without_url().to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(LlmError::Http {
            status: status.as_u16(),
            body: summarize_error_body(&body),
        });
    }

    let mut parser = SseParser::new();
    let mut full_text = String::new();
    let mut byte_stream = response.bytes_stream();
    let mut finished = false;

    'stream: while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|error| LlmError::Network(error.without_url().to_string()))?;
        for event in parser.push(&chunk) {
            match decode_event(target.format, &event)? {
                DecodedEvent::Text(text) => {
                    full_text.push_str(&text);
                    on_text(&text);
                }
                DecodedEvent::Finished => {
                    finished = true;
                    break 'stream;
                }
                DecodedEvent::Ignore => {}
            }
        }
    }
    if !finished
        && let Some(event) = parser.finish()
        && let DecodedEvent::Text(text) = decode_event(target.format, &event)?
    {
        full_text.push_str(&text);
        on_text(&text);
    }

    if full_text.trim().is_empty() {
        return Err(LlmError::EmptyResponse);
    }
    Ok(full_text)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatOutcome {
    pub text: String,
    /// True when the model refused the screenshots and the reply is based on
    /// the transcript alone.
    pub screenshots_dropped: bool,
}

/// Streams a reply, retrying once without screenshots if the model rejects
/// them.
pub async fn chat<F>(
    http: &reqwest::Client,
    target: &LlmTarget,
    input: &ChatInput<'_>,
    send_screenshots: bool,
    mut on_text: F,
) -> Result<ChatOutcome, LlmError>
where
    F: FnMut(&str) + Send,
{
    let include_images = send_screenshots && !input.images.is_empty();
    match stream_chat(http, target, input, include_images, &mut on_text).await {
        Ok(text) => Ok(ChatOutcome {
            text,
            screenshots_dropped: false,
        }),
        Err(error) if include_images && error.looks_like_image_rejection() => {
            tracing::warn!("model rejected images, retrying without them: {error}");
            let text = stream_chat(http, target, input, false, &mut on_text).await?;
            Ok(ChatOutcome {
                text,
                screenshots_dropped: true,
            })
        }
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Model discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    /// `Some` only when the provider reports input modalities (OpenRouter does).
    pub accepts_images: Option<bool>,
}

pub fn parse_models_response(body: &Value) -> Vec<ModelInfo> {
    let entries = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array);
    let mut models: Vec<ModelInfo> = entries
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let raw_id = entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)?;
            let id = raw_id.strip_prefix("models/").unwrap_or(raw_id).to_string();
            let accepts_images = entry
                .pointer("/architecture/input_modalities")
                .and_then(Value::as_array)
                .map(|modalities| modalities.iter().any(|modality| modality == "image"));
            Some(ModelInfo { id, accepts_images })
        })
        .collect();
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    models
}

/// Fetches the provider's model list (`GET {base}/models`).
pub async fn list_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    format: WireFormat,
) -> Result<Vec<ModelInfo>, LlmError> {
    let url = providers::models_url(base_url);
    let request = apply_authentication(http.get(&url), format, &url, &Secret::new(api_key));
    let response = request
        .send()
        .await
        .map_err(|error| LlmError::Network(error.without_url().to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| LlmError::Network(error.without_url().to_string()))?;
    if !status.is_success() {
        return Err(LlmError::Http {
            status: status.as_u16(),
            body: summarize_error_body(&text),
        });
    }
    let body: Value = serde_json::from_str(&text)
        .map_err(|error| LlmError::Stream(format!("unexpected model list: {error}")))?;
    Ok(parse_models_response(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(format: WireFormat) -> LlmTarget {
        LlmTarget {
            format,
            endpoint: "https://example.test/v1/x".into(),
            api_key: Secret::new("key"),
            model: "m".into(),
            max_output_tokens: 512,
            max_tokens_field: "max_tokens",
        }
    }

    fn input<'a>(images: &'a [ChatImage], history: &'a [Exchange]) -> ChatInput<'a> {
        ChatInput {
            system_prompt: "be brief",
            history,
            user_text: "what is this?",
            images,
        }
    }

    fn sample_image() -> ChatImage {
        ChatImage {
            jpeg: vec![0xFF, 0xD8, 0xFF],
            label: "screen 1 (100x50 pixels)".into(),
        }
    }

    #[test]
    fn openai_chat_body_has_data_url_images_and_system_message() {
        let images = [sample_image()];
        let history = [Exchange {
            user: "hi".into(),
            assistant: "hello".into(),
        }];
        let body = build_request_body(
            &target(WireFormat::OpenAiChat),
            &input(&images, &history),
            true,
        );

        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["messages"][2]["role"], "assistant");
        let last = &body["messages"][3]["content"];
        assert_eq!(last[0]["type"], "image_url");
        assert!(
            last[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
        assert_eq!(last[1]["text"], "screen 1 (100x50 pixels)");
        assert_eq!(last[2]["text"], "what is this?");
    }

    #[test]
    fn openai_chat_without_images_uses_plain_string_content() {
        let body = build_request_body(&target(WireFormat::OpenAiChat), &input(&[], &[]), true);
        assert_eq!(body["messages"][1]["content"], "what is this?");
    }

    #[test]
    fn dropping_images_removes_them_from_the_body() {
        let images = [sample_image()];
        let body = build_request_body(&target(WireFormat::OpenAiChat), &input(&images, &[]), false);
        assert!(!body.to_string().contains("image_url"));
    }

    #[test]
    fn openai_uses_max_completion_tokens_when_configured() {
        let mut openai_target = target(WireFormat::OpenAiChat);
        openai_target.max_tokens_field = "max_completion_tokens";
        let body = build_request_body(&openai_target, &input(&[], &[]), false);
        assert_eq!(body["max_completion_tokens"], 512);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn responses_body_does_not_store_and_uses_input_image() {
        let images = [sample_image()];
        let body = build_request_body(
            &target(WireFormat::OpenAiResponses),
            &input(&images, &[]),
            true,
        );
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(body["max_output_tokens"], 512);
    }

    #[test]
    fn anthropic_body_uses_top_level_system_and_base64_source() {
        let images = [sample_image()];
        let body = build_request_body(
            &target(WireFormat::AnthropicMessages),
            &input(&images, &[]),
            true,
        );
        assert_eq!(body["system"], "be brief");
        assert_eq!(
            body["messages"][0]["content"][0]["source"]["type"],
            "base64"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["source"]["media_type"],
            "image/jpeg"
        );
        assert_eq!(body["max_tokens"], 512);
    }

    fn event(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
        }
    }

    #[test]
    fn decodes_openai_chat_deltas() {
        let delta = event(r#"{"choices":[{"delta":{"content":"hel"}}]}"#);
        assert_eq!(
            decode_event(WireFormat::OpenAiChat, &delta).unwrap(),
            DecodedEvent::Text("hel".into())
        );
        let reasoning_only = event(r#"{"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#);
        assert_eq!(
            decode_event(WireFormat::OpenAiChat, &reasoning_only).unwrap(),
            DecodedEvent::Ignore
        );
        assert_eq!(
            decode_event(WireFormat::OpenAiChat, &event("[DONE]")).unwrap(),
            DecodedEvent::Finished
        );
    }

    #[test]
    fn decodes_anthropic_and_responses_deltas() {
        let anthropic =
            event(r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"yo"}}"#);
        assert_eq!(
            decode_event(WireFormat::AnthropicMessages, &anthropic).unwrap(),
            DecodedEvent::Text("yo".into())
        );
        let thinking = event(
            r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hm"}}"#,
        );
        assert_eq!(
            decode_event(WireFormat::AnthropicMessages, &thinking).unwrap(),
            DecodedEvent::Ignore
        );
        let responses = event(r#"{"type":"response.output_text.delta","delta":"ok"}"#);
        assert_eq!(
            decode_event(WireFormat::OpenAiResponses, &responses).unwrap(),
            DecodedEvent::Text("ok".into())
        );
    }

    #[test]
    fn in_stream_errors_are_surfaced() {
        let error = event(r#"{"error":{"message":"rate limited"}}"#);
        assert!(matches!(
            decode_event(WireFormat::OpenAiChat, &error),
            Err(LlmError::Stream(message)) if message == "rate limited"
        ));
        let anthropic_error =
            event(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
        assert!(matches!(
            decode_event(WireFormat::AnthropicMessages, &anthropic_error),
            Err(LlmError::Stream(message)) if message == "Overloaded"
        ));
    }

    #[test]
    fn image_rejection_detection_is_narrow() {
        let rejection = LlmError::Http {
            status: 400,
            body: "this model does not support image input".into(),
        };
        assert!(rejection.looks_like_image_rejection());
        let auth = LlmError::Http {
            status: 401,
            body: "invalid image key".into(),
        };
        assert!(!auth.looks_like_image_rejection());
        let unrelated = LlmError::Http {
            status: 400,
            body: "max_tokens too large".into(),
        };
        assert!(!unrelated.looks_like_image_rejection());
    }

    #[test]
    fn parses_openrouter_model_list_with_modalities() {
        let body = json!({"data": [
            {"id": "b/text-only", "architecture": {"input_modalities": ["text"]}},
            {"id": "a/vision", "architecture": {"input_modalities": ["text", "image"]}},
            {"id": "plain"},
            {"id": "models/gemini-x"}
        ]});
        let models = parse_models_response(&body);
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["a/vision", "b/text-only", "gemini-x", "plain"]);
        assert_eq!(models[0].accepts_images, Some(true));
        assert_eq!(models[1].accepts_images, Some(false));
        assert_eq!(models[3].accepts_images, None);
    }

    #[test]
    fn error_bodies_are_summarised() {
        assert_eq!(
            summarize_error_body(r#"{"error":{"message":"bad key"}}"#),
            "bad key"
        );
        assert_eq!(summarize_error_body(r#"{"error":"nope"}"#), "nope");
        assert_eq!(summarize_error_body("plain text"), "plain text");
        // Real body returned by OpenCode Zen for its free tier.
        assert_eq!(
            summarize_error_body(
                r#"{"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode."}}"#
            ),
            "OpenCode's free tier can only be used from within OpenCode."
        );
    }

    #[test]
    fn target_requires_key_model_and_supported_format() {
        let mut settings = Settings::default();
        settings.active_provider = "openai".into();
        assert!(
            matches!(LlmTarget::from_settings(&settings), Err(LlmError::Config(message)) if message.contains("API key"))
        );

        settings.providers.get_mut("openai").unwrap().api_key = Secret::new("k");
        let openai_target = LlmTarget::from_settings(&settings).unwrap();
        assert_eq!(
            openai_target.endpoint,
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(openai_target.max_tokens_field, "max_completion_tokens");

        settings.active_provider = "ollama".into();
        assert!(
            LlmTarget::from_settings(&settings).is_ok(),
            "local servers need no key"
        );

        settings.active_provider = "opencode-zen".into();
        settings.providers.get_mut("opencode-zen").unwrap().api_key = Secret::new("k");
        settings.providers.get_mut("opencode-zen").unwrap().model = "gemini-3.5-flash".into();
        assert!(matches!(
            LlmTarget::from_settings(&settings),
            Err(LlmError::Config(_))
        ));
        settings.providers.get_mut("opencode-zen").unwrap().model = "claude-sonnet-4-6".into();
        assert_eq!(
            LlmTarget::from_settings(&settings).unwrap().endpoint,
            "https://opencode.ai/zen/v1/messages"
        );
    }
}
