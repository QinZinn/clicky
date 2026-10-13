use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use clicky::tts::{self, TtsError};
use serde_json::Value;
use tokio::net::TcpListener;

type Seen = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;

async fn start(status: u16, response_body: &'static [u8]) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let handler_seen = seen.clone();
    let app = Router::new().route(
        "/v1/text-to-speech/{voice_id}",
        post(
            move |Path(voice_id): Path<String>, headers: HeaderMap, body: String| {
                let handler_seen = handler_seen.clone();
                async move {
                    let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    handler_seen.lock().unwrap().push((voice_id, headers, json));
                    (StatusCode::from_u16(status).unwrap(), response_body)
                }
            },
        ),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), seen)
}

#[tokio::test]
async fn sends_the_voice_key_and_text_and_returns_the_audio() {
    let (base_url, seen) = start(200, b"ID3-fake-mp3-bytes").await;
    let audio = tts::synthesize(
        &reqwest::Client::new(),
        &base_url,
        "xi-key",
        "voice123",
        "eleven_flash_v2_5",
        "hello there",
    )
    .await
    .unwrap();
    assert_eq!(audio, b"ID3-fake-mp3-bytes");

    let seen = seen.lock().unwrap();
    let (voice_id, headers, body) = &seen[0];
    assert_eq!(voice_id, "voice123");
    assert_eq!(headers["xi-api-key"], "xi-key");
    assert_eq!(headers["accept"], "audio/mpeg");
    assert_eq!(body["text"], "hello there");
    assert_eq!(body["model_id"], "eleven_flash_v2_5");
}

#[tokio::test]
async fn quota_errors_show_the_providers_message() {
    let (base_url, _) = start(
        401,
        br#"{"detail":{"status":"quota_exceeded","message":"You have run out of credits."}}"#,
    )
    .await;
    let error = tts::synthesize(&reqwest::Client::new(), &base_url, "k", "v1", "m", "hi")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, TtsError::Http { status: 401, message } if message == "You have run out of credits.")
    );
    assert!(!error.to_string().contains("xi-key"));
}

#[tokio::test]
async fn bad_configuration_is_rejected_before_any_request() {
    let (base_url, seen) = start(200, b"x").await;
    let client = reqwest::Client::new();

    let no_key = tts::synthesize(&client, &base_url, "", "v1", "m", "hi")
        .await
        .unwrap_err();
    assert!(matches!(no_key, TtsError::NotConfigured(_)));

    let path_injection = tts::synthesize(&client, &base_url, "k", "../../admin", "m", "hi")
        .await
        .unwrap_err();
    assert!(matches!(path_injection, TtsError::NotConfigured(_)));

    assert!(
        seen.lock().unwrap().is_empty(),
        "no request may be sent for invalid configuration"
    );
}
