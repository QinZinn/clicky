//! End-to-end tests of the LLM client against a local mock provider.
//!
//! The mock records every request (headers + JSON body) and replies with
//! canned SSE, split into awkward chunks on purpose.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use clicky::config::Secret;
use clicky::llm::{self, ChatImage, ChatInput, Exchange, LlmError, LlmTarget};
use clicky::providers::WireFormat;
use futures_util::StreamExt;
use serde_json::{Value, json};

#[derive(Clone)]
struct RecordedRequest {
    headers: HeaderMap,
    body: Value,
}

/// A scripted reply: either an SSE body (sent in the given chunks) or a
/// plain error status + JSON body.
#[derive(Clone)]
enum Script {
    Sse(Vec<&'static str>),
    Error(u16, &'static str),
}

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    scripts: Arc<Mutex<Vec<Script>>>,
}

async fn handle_post(State(state): State<MockState>, headers: HeaderMap, body: String) -> Response {
    let body_json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    state.requests.lock().unwrap().push(RecordedRequest {
        headers,
        body: body_json,
    });

    let script = {
        let mut scripts = state.scripts.lock().unwrap();
        if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        }
    };
    match script {
        Script::Error(status, body) => Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap(),
        Script::Sse(chunks) => {
            let stream = futures_util::stream::iter(chunks).then(|chunk| async move {
                tokio::time::sleep(Duration::from_millis(3)).await;
                Ok::<_, Infallible>(Bytes::from(chunk))
            });
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

async fn handle_models() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"data": [
                {"id": "z/vision", "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "a/text", "architecture": {"input_modalities": ["text"]}}
            ]})
            .to_string(),
        ))
        .unwrap()
}

struct MockServer {
    base_url: String,
    state: MockState,
}

impl MockServer {
    async fn start(scripts: Vec<Script>) -> MockServer {
        let state = MockState {
            requests: Arc::new(Mutex::new(Vec::new())),
            scripts: Arc::new(Mutex::new(scripts)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(handle_post))
            .route("/v1/responses", post(handle_post))
            .route("/v1/messages", post(handle_post))
            .route("/v1/models", get(handle_models))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockServer {
            base_url: format!("http://{address}/v1"),
            state,
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().unwrap().clone()
    }

    fn target(&self, format: WireFormat) -> LlmTarget {
        LlmTarget {
            format,
            endpoint: clicky::providers::endpoint_url(&self.base_url, format),
            api_key: Secret::new("test-key"),
            model: "test-model".into(),
            max_output_tokens: 256,
            max_tokens_field: "max_tokens",
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn one_image() -> Vec<ChatImage> {
    vec![ChatImage {
        jpeg: vec![0xFF, 0xD8, 0xFF, 0xE0],
        label: "screen 1 (64x64 pixels)".into(),
    }]
}

fn input<'a>(images: &'a [ChatImage], history: &'a [Exchange]) -> ChatInput<'a> {
    ChatInput {
        system_prompt: "sys",
        history,
        user_text: "what's on screen?",
        images,
    }
}

#[tokio::test]
async fn openai_chat_streams_text_split_across_chunks() {
    // The second event is split mid-JSON and the emoji is split mid-character.
    let server = MockServer::start(vec![Script::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"del",
        "ta\":{\"content\":\"lo \"}}]}\n\n: keep-alive\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"wor",
        "ld\"}}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;

    let images = one_image();
    let history = [Exchange {
        user: "hi".into(),
        assistant: "hey".into(),
    }];
    let mut pieces: Vec<String> = Vec::new();
    let outcome = llm::chat(
        &client(),
        &server.target(WireFormat::OpenAiChat),
        &input(&images, &history),
        true,
        |piece| pieces.push(piece.to_string()),
    )
    .await
    .unwrap();

    assert_eq!(outcome.text, "hello world");
    assert!(!outcome.screenshots_dropped);
    assert_eq!(pieces.concat(), "hello world");
    assert!(
        pieces.len() >= 3,
        "text should arrive incrementally: {pieces:?}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.headers["authorization"], "Bearer test-key");
    assert_eq!(request.body["model"], "test-model");
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.body["max_tokens"], 256);
    let messages = request.body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["content"], "hi");
    let last_content = &messages.last().unwrap()["content"];
    assert!(
        last_content[0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,/9j/4A")
    );
}

#[tokio::test]
async fn anthropic_style_sends_both_headers_to_gateways_and_parses_deltas() {
    let server = MockServer::start(vec![Script::Sse(vec![
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"it is \"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"a cat\"}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ])])
    .await;

    let images = one_image();
    let outcome = llm::chat(
        &client(),
        &server.target(WireFormat::AnthropicMessages),
        &input(&images, &[]),
        true,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.text, "it is a cat");

    let request = &server.requests()[0];
    assert_eq!(request.headers["x-api-key"], "test-key");
    assert_eq!(request.headers["anthropic-version"], "2023-06-01");
    // 127.0.0.1 is a gateway, not api.anthropic.com, so bearer is sent too.
    assert_eq!(request.headers["authorization"], "Bearer test-key");
    assert_eq!(request.body["system"], "sys");
    assert_eq!(request.body["messages"][0]["content"][0]["type"], "image");
    assert_eq!(
        request.body["messages"][0]["content"][0]["source"]["media_type"],
        "image/jpeg"
    );
}

#[tokio::test]
async fn responses_style_streams_output_text_deltas() {
    let server = MockServer::start(vec![Script::Sse(vec![
        "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"over \"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"there\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
    ])])
    .await;

    let images = one_image();
    let outcome = llm::chat(
        &client(),
        &server.target(WireFormat::OpenAiResponses),
        &input(&images, &[]),
        true,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.text, "over there");

    let request = &server.requests()[0];
    assert_eq!(request.body["store"], false);
    assert_eq!(request.body["instructions"], "sys");
    assert_eq!(
        request.body["input"][0]["content"][0]["type"],
        "input_image"
    );
}

#[tokio::test]
async fn retries_without_screenshots_when_the_model_rejects_images() {
    let server = MockServer::start(vec![
        Script::Error(
            400,
            r#"{"error":{"message":"This model does not support image input"}}"#,
        ),
        Script::Sse(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"text only\"}}]}\n\ndata: [DONE]\n\n",
        ]),
    ])
    .await;

    let images = one_image();
    let outcome = llm::chat(
        &client(),
        &server.target(WireFormat::OpenAiChat),
        &input(&images, &[]),
        true,
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.text, "text only");
    assert!(outcome.screenshots_dropped);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body.to_string().contains("image_url"));
    assert!(!requests[1].body.to_string().contains("image_url"));
    assert_eq!(
        requests[1].body["messages"][1]["content"],
        "what's on screen?"
    );
}

#[tokio::test]
async fn auth_errors_are_not_retried_and_show_the_provider_message() {
    let server = MockServer::start(vec![Script::Error(
        401,
        r#"{"error":{"message":"Invalid API key"}}"#,
    )])
    .await;
    let images = one_image();
    let error = llm::chat(
        &client(),
        &server.target(WireFormat::OpenAiChat),
        &input(&images, &[]),
        true,
        |_| {},
    )
    .await
    .unwrap_err();

    assert!(
        matches!(&error, LlmError::Http { status: 401, body } if body == "Invalid API key"),
        "{error:?}"
    );
    assert_eq!(server.requests().len(), 1);
    // The key must never be echoed into the error text.
    assert!(!error.to_string().contains("test-key"));
}

#[tokio::test]
async fn screenshots_toggle_off_sends_no_images() {
    let server = MockServer::start(vec![Script::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;
    let images = one_image();
    llm::chat(
        &client(),
        &server.target(WireFormat::OpenAiChat),
        &input(&images, &[]),
        false,
        |_| {},
    )
    .await
    .unwrap();
    assert!(!server.requests()[0].body.to_string().contains("image_url"));
}

#[tokio::test]
async fn mid_stream_provider_errors_fail_the_request() {
    let server = MockServer::start(vec![Script::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"upstream exploded\"}}\n\n",
    ])])
    .await;
    let error = llm::chat(
        &client(),
        &server.target(WireFormat::OpenAiChat),
        &input(&[], &[]),
        false,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(error, LlmError::Stream(message) if message == "upstream exploded"));
}

#[tokio::test]
async fn an_empty_reply_is_reported_with_a_hint() {
    let server = MockServer::start(vec![Script::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking...\"}}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;
    let error = llm::chat(
        &client(),
        &server.target(WireFormat::OpenAiChat),
        &input(&[], &[]),
        false,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(error, LlmError::EmptyResponse));
    assert!(error.to_string().contains("Max output tokens"));
}

#[tokio::test]
async fn lists_models_with_vision_flags() {
    let server = MockServer::start(vec![Script::Sse(vec![])]).await;
    let models = llm::list_models(&client(), &server.base_url, "k", WireFormat::OpenAiChat)
        .await
        .unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "a/text");
    assert_eq!(models[0].accepts_images, Some(false));
    assert_eq!(models[1].accepts_images, Some(true));
}

#[tokio::test]
async fn connection_failures_do_not_leak_the_key() {
    // Nothing listens on port 1.
    let mut target = MockServer::start(vec![Script::Sse(vec![])])
        .await
        .target(WireFormat::OpenAiChat);
    target.endpoint = "http://127.0.0.1:1/v1/chat/completions".into();
    let error = llm::chat(&client(), &target, &input(&[], &[]), false, |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Network(_)));
    assert!(!error.to_string().contains("test-key"));
}
