//! Tests of the speech-to-text sessions against local mock servers: a mock
//! AssemblyAI (token endpoint + websocket) and a mock Whisper upload endpoint.

// The websocket handshake callback's signature is dictated by tungstenite.
#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use clicky::stt::{
    AssemblyAiEndpoints, SttError, WhisperUpload, run_assemblyai_session, run_whisper_upload,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// What the mock AssemblyAI should do.
#[derive(Clone)]
struct Behaviour {
    /// Delay before sending `Begin`.
    begin_delay: Duration,
    /// Send the formatted turn after `ForceEndpoint`?
    send_formatted_turn: bool,
    /// Immediately send an error instead of working.
    fail_with_error: Option<&'static str>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            begin_delay: Duration::ZERO,
            send_formatted_turn: true,
            fail_with_error: None,
        }
    }
}

#[derive(Default)]
struct Observed {
    websocket_uri: Option<String>,
    audio_bytes: usize,
    force_endpoint_seen: bool,
    terminate_seen: bool,
    token_authorization: Option<String>,
}

struct MockAssemblyAi {
    endpoints: AssemblyAiEndpoints,
    observed: Arc<Mutex<Observed>>,
}

async fn start_mock_assemblyai(behaviour: Behaviour, token_status: u16) -> MockAssemblyAi {
    let observed = Arc::new(Mutex::new(Observed::default()));

    // Token endpoint.
    let token_observed = observed.clone();
    let app = Router::new().route(
        "/token",
        get(move |headers: HeaderMap| {
            let token_observed = token_observed.clone();
            async move {
                token_observed.lock().unwrap().token_authorization = headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap().to_string());
                if token_status == 200 {
                    (StatusCode::OK, json!({"token": "tok-123"}).to_string())
                } else {
                    (
                        StatusCode::from_u16(token_status).unwrap(),
                        "nope".to_string(),
                    )
                }
            }
        }),
    );
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_address = http_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(http_listener, app).await.unwrap() });

    // Websocket endpoint.
    let websocket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_address = websocket_listener.local_addr().unwrap();
    let websocket_observed = observed.clone();
    tokio::spawn(async move {
        let (tcp, _) = websocket_listener.accept().await.unwrap();
        let uri_observed = websocket_observed.clone();
        let websocket = tokio_tungstenite::accept_hdr_async(
            tcp,
            move |request: &Request, response: Response| {
                uri_observed.lock().unwrap().websocket_uri = Some(request.uri().to_string());
                Ok(response)
            },
        )
        .await
        .unwrap();
        let (mut sink, mut stream) = websocket.split();

        if let Some(message) = behaviour.fail_with_error {
            let _ = sink
                .send(Message::text(
                    json!({"type": "Error", "error": message}).to_string(),
                ))
                .await;
            return;
        }

        tokio::time::sleep(behaviour.begin_delay).await;
        sink.send(Message::text(
            json!({"type": "Begin", "id": "s1"}).to_string(),
        ))
        .await
        .unwrap();

        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Binary(bytes) => {
                    websocket_observed.lock().unwrap().audio_bytes += bytes.len();
                    // A partial result while talking.
                    let _ = sink
                        .send(Message::text(
                            json!({"type": "Turn", "turn_order": 0, "transcript": "hello wor",
                                   "end_of_turn": false, "turn_is_formatted": false})
                            .to_string(),
                        ))
                        .await;
                }
                Message::Text(text) => {
                    let value: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                    match value["type"].as_str().unwrap() {
                        "ForceEndpoint" => {
                            websocket_observed.lock().unwrap().force_endpoint_seen = true;
                            sink.send(Message::text(
                                json!({"type": "Turn", "turn_order": 0, "transcript": "hello world",
                                       "end_of_turn": true, "turn_is_formatted": false})
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                            if behaviour.send_formatted_turn {
                                tokio::time::sleep(Duration::from_millis(30)).await;
                                sink.send(Message::text(
                                    json!({"type": "Turn", "turn_order": 0, "transcript": "Hello, world.",
                                           "end_of_turn": true, "turn_is_formatted": true})
                                    .to_string(),
                                ))
                                .await
                                .unwrap();
                            }
                        }
                        "Terminate" => {
                            websocket_observed.lock().unwrap().terminate_seen = true;
                            let _ = sink
                                .send(Message::text(json!({"type": "Termination"}).to_string()))
                                .await;
                            return;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    });

    MockAssemblyAi {
        endpoints: AssemblyAiEndpoints {
            token_url: format!("http://{http_address}/token"),
            websocket_url: format!("ws://{websocket_address}/v3/ws"),
        },
        observed,
    }
}

struct Session {
    audio: mpsc::UnboundedSender<Vec<u8>>,
    finish: oneshot::Sender<()>,
    partial: watch::Receiver<String>,
    task: tokio::task::JoinHandle<Result<String, SttError>>,
}

fn start_session(mock: &MockAssemblyAi) -> Session {
    let (audio, audio_receiver) = mpsc::unbounded_channel();
    let (finish, finish_receiver) = oneshot::channel();
    let (partial_sender, partial) = watch::channel(String::new());
    let task = tokio::spawn(run_assemblyai_session(
        reqwest::Client::new(),
        mock.endpoints.clone(),
        "my-assemblyai-key".into(),
        audio_receiver,
        finish_receiver,
        partial_sender,
    ));
    Session {
        audio,
        finish,
        partial,
        task,
    }
}

#[tokio::test]
async fn streams_audio_and_returns_the_formatted_transcript() {
    let mock = start_mock_assemblyai(Behaviour::default(), 200).await;
    let session = start_session(&mock);

    for _ in 0..5 {
        session.audio.send(vec![0u8; 3200]).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        *session.partial.borrow(),
        "hello wor",
        "live text should be exposed while talking"
    );

    session.finish.send(()).unwrap();
    let transcript = session.task.await.unwrap().unwrap();
    assert_eq!(transcript, "Hello, world.");

    let observed = mock.observed.lock().unwrap();
    assert_eq!(
        observed.token_authorization.as_deref(),
        Some("my-assemblyai-key")
    );
    let uri = observed.websocket_uri.clone().unwrap();
    for expected in [
        "token=tok-123",
        "sample_rate=16000",
        "encoding=pcm_s16le",
        "format_turns=true",
    ] {
        assert!(uri.contains(expected), "{uri}");
    }
    assert_eq!(observed.audio_bytes, 5 * 3200);
    assert!(observed.force_endpoint_seen);
}

#[tokio::test]
async fn audio_recorded_before_the_session_is_ready_is_not_lost() {
    // Begin arrives 250 ms late; all audio and the finish signal are queued
    // long before that.
    let mock = start_mock_assemblyai(
        Behaviour {
            begin_delay: Duration::from_millis(250),
            ..Behaviour::default()
        },
        200,
    )
    .await;
    let session = start_session(&mock);
    for _ in 0..3 {
        session.audio.send(vec![1u8; 3200]).unwrap();
    }
    session.finish.send(()).unwrap();

    let transcript = session.task.await.unwrap().unwrap();
    assert_eq!(transcript, "Hello, world.");
    assert_eq!(mock.observed.lock().unwrap().audio_bytes, 3 * 3200);
}

#[tokio::test]
async fn falls_back_to_unformatted_text_when_the_formatted_turn_never_comes() {
    let mock = start_mock_assemblyai(
        Behaviour {
            send_formatted_turn: false,
            ..Behaviour::default()
        },
        200,
    )
    .await;
    let session = start_session(&mock);
    session.audio.send(vec![0u8; 3200]).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    session.finish.send(()).unwrap();
    let transcript = session.task.await.unwrap().unwrap();
    assert_eq!(transcript, "hello world");
    assert!(
        started.elapsed() < Duration::from_millis(1_300),
        "should not wait the whole grace period"
    );
}

#[tokio::test]
async fn a_rejected_api_key_gives_an_actionable_error() {
    let mock = start_mock_assemblyai(Behaviour::default(), 401).await;
    let session = start_session(&mock);
    let error = session.task.await.unwrap().unwrap_err();
    assert!(
        error.to_string().contains("rejected the API key"),
        "{error}"
    );
    assert!(!error.to_string().contains("my-assemblyai-key"));
}

#[tokio::test]
async fn service_errors_before_finish_are_reported() {
    let mock = start_mock_assemblyai(
        Behaviour {
            fail_with_error: Some("Insufficient funds"),
            ..Behaviour::default()
        },
        200,
    )
    .await;
    let session = start_session(&mock);
    let error = session.task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("Insufficient funds"), "{error}");
}

#[tokio::test]
async fn dropping_the_finish_sender_cancels_the_session() {
    let mock = start_mock_assemblyai(Behaviour::default(), 200).await;
    let session = start_session(&mock);
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(session.finish);
    let error = session.task.await.unwrap().unwrap_err();
    assert!(matches!(error, SttError::Cancelled));
}

// ---------------------------------------------------------------------------
// Whisper-style upload
// ---------------------------------------------------------------------------

/// Every upload the mock Whisper endpoint received: headers and raw body.
type ReceivedUploads = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;

async fn start_mock_whisper(received: ReceivedUploads) -> String {
    let app = Router::new().route(
        "/v1/audio/transcriptions",
        post(move |headers: HeaderMap, body: Bytes| {
            let received = received.clone();
            async move {
                received.lock().unwrap().push((headers, body.to_vec()));
                json!({"text": "  transcribed words "}).to_string()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}/v1")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
async fn uploads_a_wav_file_and_returns_the_text() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let base_url = start_mock_whisper(received.clone()).await;

    let (audio, audio_receiver) = mpsc::unbounded_channel();
    let (finish, finish_receiver) = oneshot::channel();
    let task = tokio::spawn(run_whisper_upload(
        reqwest::Client::new(),
        WhisperUpload {
            base_url,
            api_key: "whisper-key".into(),
            model: "whisper-1".into(),
        },
        audio_receiver,
        finish_receiver,
    ));
    // One second of audio.
    for _ in 0..10 {
        audio.send(vec![0u8; 3200]).unwrap();
    }
    finish.send(()).unwrap();

    assert_eq!(task.await.unwrap().unwrap(), "transcribed words");

    let received = received.lock().unwrap();
    assert_eq!(received.len(), 1);
    let (headers, body) = &received[0];
    assert_eq!(headers["authorization"], "Bearer whisper-key");
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("multipart/form-data")
    );
    assert!(contains(body, b"name=\"model\""));
    assert!(contains(body, b"whisper-1"));
    assert!(contains(body, b"filename=\"speech.wav\""));
    assert!(contains(body, b"RIFF"));
}

#[tokio::test]
async fn very_short_recordings_are_not_uploaded() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let base_url = start_mock_whisper(received.clone()).await;

    let (audio, audio_receiver) = mpsc::unbounded_channel();
    let (finish, finish_receiver) = oneshot::channel();
    let task = tokio::spawn(run_whisper_upload(
        reqwest::Client::new(),
        WhisperUpload {
            base_url,
            api_key: String::new(),
            model: "whisper-1".into(),
        },
        audio_receiver,
        finish_receiver,
    ));
    audio.send(vec![0u8; 3200]).unwrap(); // 100 ms: an accidental tap
    finish.send(()).unwrap();

    assert_eq!(task.await.unwrap().unwrap(), "");
    assert!(received.lock().unwrap().is_empty());
}
