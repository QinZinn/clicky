//! A fake AI provider for trying Clicky without any API keys.
//!
//!   cargo run --example mock_provider            # listens on 127.0.0.1:8787
//!
//! Then in Clicky: Settings > AI model > "Custom (OpenAI-compatible)", base URL
//! `http://127.0.0.1:8787/v1`, any model name, any key. Replies stream word by
//! word and point at the middle of your screen, so you can see the whole
//! pipeline (screenshot, streaming, blue cursor) work.
//!
//! It also plays the other roles of the docs lookup, so Settings > Docs works
//! against it too: choose the SearXNG search service with address
//! `http://127.0.0.1:8787`, then turn on "Look up docs". The mock model plans a
//! search, a mock SearXNG lists a mock docs page, and the answer says it used the
//! docs. (Clicky refuses to fetch pages that are not on the public internet, as it
//! should, and this mock's page is not, so the docs here come from the search
//! snippet: the same fallback Clicky uses for pages it cannot read.)
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

/// The text of the last user message (the last text part when it has several).
fn last_user_text(request: &Value) -> String {
    let content = request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .map_or(Value::Null, |message| message["content"].clone());
    match content {
        Value::String(text) => text,
        Value::Array(parts) => parts
            .iter()
            .rev()
            .find_map(|part| part["text"].as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Streams `text` back word by word, in the OpenAI chat-completions format.
fn stream_text(text: String) -> Response {
    let words: Vec<String> = text.split_inclusive(' ').map(str::to_string).collect();
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

async fn chat(body: String) -> Response {
    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let system_prompt = request["messages"][0]["content"].as_str().unwrap_or("");

    // The docs planner asks for "SEARCH: <query>" or "NONE".
    if system_prompt.contains("needs a lookup in real documentation") {
        let reply = if last_user_text(&request).to_lowercase().contains("capital") {
            "NONE"
        } else {
            "SEARCH: mock app auto layout wrap"
        };
        return stream_text(reply.to_string());
    }

    let used_docs = last_user_text(&request).contains("REFERENCE-DOCS");
    let reply = std::env::var("MOCK_REPLY").unwrap_or_else(|_| {
        if used_docs {
            "according to the docs, open the auto layout panel and turn on wrap, so here is the spot."
                .to_string()
        } else {
            "this is a mock reply, so here is the middle of your screen.".to_string()
        }
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
    stream_text(format!("{reply}{tag}"))
}

/// A mock SearXNG: one result, a documentation page that is not on the public internet.
async fn searxng() -> Response {
    let address = std::env::var("MOCK_ADDRESS").unwrap_or_else(|_| "127.0.0.1:8787".into());
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"results": [{
                "title": "Mock App: Guide to auto layout",
                "url": format!("https://docs.mock-app.example/guide/auto-layout?served-by={address}"),
                "content": "Open the auto layout panel and turn on wrap to let items flow onto new rows."
            }]})
            .to_string(),
        ))
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
        .route("/v1/models", get(models))
        .route("/search", get(searxng));
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .expect("binding the mock port");
    println!("mock provider listening on http://{address}/v1");
    axum::serve(listener, app).await.unwrap();
}
