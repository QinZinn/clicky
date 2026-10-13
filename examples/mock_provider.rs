//! A fake AI provider for trying Clicky without any API keys.
//!
//!   cargo run --example mock_provider            # listens on 127.0.0.1:8787
//!
//! Then in Clicky: Settings > AI model > "Custom (OpenAI-compatible)", base URL
//! `http://127.0.0.1:8787/v1`, any model name, any key. Replies stream word by
//! word and point at the middle of your screen, so you can see the whole
//! pipeline (screenshot, streaming, blue cursor) work.
//!
//! Set `MOCK_REPLY` to change the reply text, and `MOCK_POINT=0.25,0.3` to
//! choose where it points (fractions of the screen width and height).

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde_json::{Value, json};

/// Finds "image dimensions: 1280x720 pixels" in the request, as written by Clicky.
fn screenshot_size(request: &Value) -> Option<(u32, u32)> {
    let text = request.to_string();
    let after = text.split("image dimensions: ").nth(1)?;
    let (width, rest) = after.split_once('x')?;
    let height: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((width.parse().ok()?, height.parse().ok()?))
}

async fn chat(body: String) -> Response {
    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let reply = std::env::var("MOCK_REPLY").unwrap_or_else(|_| {
        "this is a mock reply, so here is the middle of your screen.".to_string()
    });
    // Where to point, as fractions of the screenshot (default: the middle).
    let (fraction_x, fraction_y) = std::env::var("MOCK_POINT")
        .ok()
        .and_then(|value| {
            let (x_text, y_text) = value.split_once(',')?;
            Some((
                x_text.trim().parse::<f32>().ok()?,
                y_text.trim().parse::<f32>().ok()?,
            ))
        })
        .unwrap_or((0.5, 0.5));
    let tag = match screenshot_size(&request) {
        Some((width, height)) => format!(
            " [POINT:{},{}:the target]",
            (width as f32 * fraction_x) as u32,
            (height as f32 * fraction_y) as u32
        ),
        None => " [POINT:none]".to_string(),
    };

    let words: Vec<String> = format!("{reply}{tag}")
        .split_inclusive(' ')
        .map(str::to_string)
        .collect();
    let events = words
        .into_iter()
        .map(|word| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"content": word}}]})
            )
        })
        .chain(std::iter::once("data: [DONE]\n\n".to_string()));
    let stream = futures_util::stream::iter(events).then(|event| async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        Ok::<_, Infallible>(Bytes::from(event))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn models() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"data": [
                {"id": "mock-vision", "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "mock-text-only", "architecture": {"input_modalities": ["text"]}}
            ]})
            .to_string(),
        ))
        .unwrap()
}

#[tokio::main]
async fn main() {
    let address = std::env::var("MOCK_ADDRESS").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models));
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .expect("binding the mock port");
    println!("mock provider listening on http://{address}/v1");
    axum::serve(listener, app).await.unwrap();
}
