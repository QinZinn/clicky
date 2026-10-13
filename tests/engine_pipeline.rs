//! Drives the real engine end to end. Only the hardware is faked (screen
//! capture, speakers); the LLM and ElevenLabs are local mock servers.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use clicky::audio::AudioLevel;
use clicky::config::{Secret, Settings, SttProvider, TtsProvider};
use clicky::engine::{
    Command, EngineDependencies, EventSink, ScreenCaptureFunction, UiEvent, spawn_engine,
};
use clicky::geometry::Rect;
use clicky::playback::SpeechPlayer;
use clicky::screen::{ScreenError, Screenshot};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

// ---------------------------------------------------------------------------
// Fakes and mocks
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakePlayer {
    played: Mutex<Vec<Vec<u8>>>,
    stop_calls: AtomicUsize,
    polls_remaining: AtomicUsize,
    playing: AtomicBool,
}

impl SpeechPlayer for FakePlayer {
    fn play(&self, audio: Vec<u8>) -> Result<(), String> {
        self.played.lock().unwrap().push(audio);
        self.polls_remaining.store(2, Ordering::SeqCst);
        self.playing.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        self.playing.store(false, Ordering::SeqCst);
    }
    fn is_playing(&self) -> bool {
        if !self.playing.load(Ordering::SeqCst) {
            return false;
        }
        // "Plays" for two polls, then finishes.
        if self.polls_remaining.load(Ordering::SeqCst) == 0 {
            self.playing.store(false, Ordering::SeqCst);
            return false;
        }
        self.polls_remaining.fetch_sub(1, Ordering::SeqCst);
        true
    }
}

#[derive(Clone)]
struct MockServerState {
    llm_requests: Arc<Mutex<Vec<Value>>>,
    llm_script: Arc<Mutex<Vec<LlmScript>>>,
    tts_requests: Arc<Mutex<Vec<Value>>>,
}

#[derive(Clone)]
enum LlmScript {
    Reply {
        chunks: Vec<&'static str>,
        delay_ms: u64,
    },
    Fail(u16, &'static str),
}

fn sse_reply(pieces: &[&str]) -> LlmScript {
    // Wrap each text piece into an OpenAI-style chunk.
    let leaked: Vec<&'static str> = pieces
        .iter()
        .map(|piece| {
            let event = format!(
                "data: {}\n\n",
                serde_json::json!({"choices": [{"delta": {"content": piece}}]})
            );
            &*Box::leak(event.into_boxed_str())
        })
        .chain(std::iter::once("data: [DONE]\n\n"))
        .collect();
    LlmScript::Reply {
        chunks: leaked,
        delay_ms: 2,
    }
}

async fn llm_handler(State(state): State<MockServerState>, body: String) -> Response {
    state
        .llm_requests
        .lock()
        .unwrap()
        .push(serde_json::from_str(&body).unwrap());
    let script = {
        let mut scripts = state.llm_script.lock().unwrap();
        if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        }
    };
    match script {
        LlmScript::Fail(status, body) => Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .body(Body::from(body))
            .unwrap(),
        LlmScript::Reply { chunks, delay_ms } => {
            let stream = futures_util::stream::iter(chunks).then(move |chunk| async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                Ok::<_, Infallible>(Bytes::from(chunk))
            });
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

async fn tts_handler(State(state): State<MockServerState>, body: String) -> Response {
    state
        .tts_requests
        .lock()
        .unwrap()
        .push(serde_json::from_str(&body).unwrap());
    Response::builder().body(Body::from("MP3-BYTES")).unwrap()
}

struct Mocks {
    base_url: String,
    state: MockServerState,
}

async fn start_mocks(script: Vec<LlmScript>) -> Mocks {
    let state = MockServerState {
        llm_requests: Arc::default(),
        llm_script: Arc::new(Mutex::new(script)),
        tts_requests: Arc::default(),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(llm_handler))
        .route("/v1/text-to-speech/{voice}", post(tts_handler))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mocks {
        base_url: format!("http://{address}"),
        state,
    }
}

fn screenshot(monitor_rect: Rect, is_cursor_screen: bool, label: &str) -> Screenshot {
    Screenshot {
        jpeg: vec![0xFF, 0xD8, 0xFF, 0xE0],
        label: label.to_string(),
        is_cursor_screen,
        monitor_rect,
        width_in_pixels: 500,
        height_in_pixels: 250,
    }
}

/// A monitor at (100, 50), 1000x500 points; its screenshot is 500x250 pixels.
fn one_screen() -> Vec<Screenshot> {
    vec![screenshot(
        Rect::new(100.0, 50.0, 1000.0, 500.0),
        true,
        "user's screen (image dimensions: 500x250 pixels)",
    )]
}

struct Harness {
    commands: UnboundedSender<Command>,
    events: Arc<Mutex<Vec<String>>>,
    point_requests: Arc<Mutex<Vec<clicky::engine::PointRequest>>>,
    player: Arc<FakePlayer>,
    capture_calls: Arc<AtomicUsize>,
    mocks: Mocks,
}

fn describe(event: &UiEvent) -> String {
    match event {
        UiEvent::StateChanged(state) => format!("state:{state:?}"),
        UiEvent::PartialTranscript(text) => format!("partial:{text}"),
        UiEvent::UserMessage(text) => format!("user:{text}"),
        UiEvent::AssistantStreaming(text) => format!("stream:{text}"),
        UiEvent::AssistantMessage(text) => format!("assistant:{text}"),
        UiEvent::Notice(text) => format!("notice:{text}"),
        UiEvent::Error(text) => format!("error:{text}"),
        UiEvent::PointAt(request) => {
            format!(
                "point:{},{}:{}",
                request.target.x, request.target.y, request.label
            )
        }
        UiEvent::Models {
            provider_id,
            result,
        } => {
            format!(
                "models:{provider_id}:{}",
                result.as_ref().map(|models| models.len()).unwrap_or(0)
            )
        }
    }
}

fn test_settings(mocks_base_url: &str) -> Settings {
    let mut settings = Settings::default();
    settings.active_provider = "custom".into();
    {
        let custom = settings.providers.get_mut("custom").unwrap();
        custom.base_url = format!("{mocks_base_url}/v1");
        custom.model = "test-model".into();
        custom.api_key = Secret::new("llm-key");
        custom.send_screenshots = true;
    }
    settings.tts.provider = TtsProvider::ElevenLabs;
    settings.tts.elevenlabs_api_key = Secret::new("xi-key");
    settings.tts.voice_id = "voice1".into();
    settings.stt.provider = SttProvider::Disabled;
    settings
}

async fn start_harness(
    script: Vec<LlmScript>,
    screens: Result<Vec<Screenshot>, &'static str>,
    configure: impl FnOnce(&mut Settings),
) -> Harness {
    let mocks = start_mocks(script).await;
    let mut settings = test_settings(&mocks.base_url);
    configure(&mut settings);

    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let point_requests: Arc<Mutex<Vec<clicky::engine::PointRequest>>> = Arc::default();
    let sink_events = events.clone();
    let sink_points = point_requests.clone();
    let sink: EventSink = Arc::new(move |event| {
        if let UiEvent::PointAt(request) = &event {
            sink_points.lock().unwrap().push(request.clone());
        }
        sink_events.lock().unwrap().push(describe(&event));
    });

    let capture_calls = Arc::new(AtomicUsize::new(0));
    let counter = capture_calls.clone();
    let capture_screens: ScreenCaptureFunction = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        screens
            .clone()
            .map_err(|message| ScreenError::Capture(message.to_string()))
    });
    let player = Arc::new(FakePlayer::default());
    let dependencies = EngineDependencies {
        capture_screens,
        player: player.clone(),
        tts_base_url: mocks.base_url.clone(),
    };
    let commands = spawn_engine(
        &tokio::runtime::Handle::current(),
        settings,
        dependencies,
        sink,
        AudioLevel::default(),
    );
    Harness {
        commands,
        events,
        point_requests,
        player,
        capture_calls,
        mocks,
    }
}

impl Harness {
    fn send_text(&self, text: &str) {
        self.commands.send(Command::SendText(text.into())).unwrap();
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    /// Waits until the event list contains `needle`; panics with the events on timeout.
    async fn wait_for(&self, needle: &str) {
        for _ in 0..400 {
            if self.events().iter().any(|event| event.contains(needle)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {needle:?}; events were: {:#?}",
            self.events()
        );
    }

    async fn wait_until_idle_after(&self, previous_idle_count: usize) {
        for _ in 0..600 {
            let idle_count = self
                .events()
                .iter()
                .filter(|event| *event == "state:Idle")
                .count();
            if idle_count > previous_idle_count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never became idle; events were: {:#?}", self.events());
    }

    fn idle_count(&self) -> usize {
        self.events()
            .iter()
            .filter(|event| *event == "state:Idle")
            .count()
    }

    fn llm_requests(&self) -> Vec<Value> {
        self.mocks.state.llm_requests.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_full_turn_sees_the_screen_points_and_speaks() {
    let harness = start_harness(
        vec![sse_reply(&[
            "see that ",
            "button. [POINT:250,",
            "125:save button]",
        ])],
        Ok(one_screen()),
        |_| {},
    )
    .await;

    harness.send_text("where is save?");
    harness.wait_until_idle_after(0).await;

    let events = harness.events();
    // Order matters: user, thinking, streaming..., final, point, speaking, idle.
    let position = |prefix: &str| {
        events
            .iter()
            .position(|event| event.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} in {events:#?}"))
    };
    assert_eq!(events[0], "user:where is save?");
    assert_eq!(events[1], "state:Thinking");
    assert!(position("stream:") < position("assistant:"));
    assert!(position("assistant:") < position("point:"));
    assert!(position("point:") < position("state:Speaking"));
    assert!(position("state:Speaking") < position("state:Idle"));

    // The raw tag never reaches the display text, even mid-stream.
    assert!(
        events
            .iter()
            .all(|event| !event.contains("[POINT") && !event.contains("POINT:")),
        "{events:#?}"
    );
    assert!(events.contains(&"stream:see that button.".to_string()));
    assert!(events.contains(&"assistant:see that button.".to_string()));

    // (250,125) in a 500x250 screenshot of a 1000x500 monitor at (100,50) -> (600,300).
    let points = harness.point_requests.lock().unwrap().clone();
    assert_eq!(points.len(), 1);
    assert_eq!((points[0].target.x, points[0].target.y), (600.0, 300.0));
    assert_eq!(points[0].label, "save button");
    assert_eq!(
        points[0].monitor_rect,
        Rect::new(100.0, 50.0, 1000.0, 500.0)
    );

    // The model received the screenshot, its label, the system prompt and the question.
    let request = &harness.llm_requests()[0];
    assert_eq!(request["model"], "test-model");
    let messages = request["messages"].as_array().unwrap();
    assert!(messages[0]["content"].as_str().unwrap().contains("clicky"));
    let content = &messages.last().unwrap()["content"];
    assert!(
        content[0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,")
    );
    assert_eq!(
        content[1]["text"],
        "user's screen (image dimensions: 500x250 pixels)"
    );
    assert_eq!(content[2]["text"], "where is save?");

    // The voice request carried the cleaned text, and the speaker got the audio.
    assert_eq!(
        harness.mocks.state.tts_requests.lock().unwrap()[0]["text"],
        "see that button."
    );
    assert_eq!(
        harness.player.played.lock().unwrap().as_slice(),
        &[b"MP3-BYTES".to_vec()]
    );
}

#[tokio::test]
async fn the_conversation_is_remembered_and_can_be_cleared() {
    let harness = start_harness(
        vec![
            sse_reply(&["first answer [POINT:none]"]),
            sse_reply(&["second answer"]),
        ],
        Ok(one_screen()),
        |settings| settings.tts.provider = TtsProvider::Off,
    )
    .await;

    harness.send_text("first question");
    harness.wait_until_idle_after(0).await;
    harness.send_text("second question");
    harness.wait_until_idle_after(1).await;

    let requests = harness.llm_requests();
    let second = requests[1]["messages"].as_array().unwrap();
    // system, previous user, previous assistant (tag stripped), current user.
    assert_eq!(second.len(), 4);
    assert_eq!(second[1]["content"], "first question");
    assert_eq!(second[2]["content"], "first answer");

    harness.commands.send(Command::ClearConversation).unwrap();
    harness.send_text("third question");
    harness.wait_until_idle_after(2).await;
    assert_eq!(
        harness.llm_requests()[2]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "history should be gone"
    );
}

#[tokio::test]
async fn history_is_capped_to_the_configured_number_of_exchanges() {
    let harness = start_harness(vec![sse_reply(&["ok"])], Ok(one_screen()), |settings| {
        settings.tts.provider = TtsProvider::Off;
        settings.history_exchanges = 2;
    })
    .await;
    for turn in 0..4 {
        harness.send_text(&format!("question {turn}"));
        harness.wait_until_idle_after(turn).await;
    }
    let last = harness.llm_requests().last().unwrap().clone();
    let messages = last["messages"].as_array().unwrap();
    // system + 2 remembered exchanges (4 messages) + current question.
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[1]["content"], "question 1");
}

#[tokio::test]
async fn no_screenshots_are_taken_when_they_are_switched_off() {
    let harness = start_harness(vec![sse_reply(&["hello"])], Ok(one_screen()), |settings| {
        settings.tts.provider = TtsProvider::Off;
        settings
            .providers
            .get_mut("custom")
            .unwrap()
            .send_screenshots = false;
    })
    .await;
    harness.send_text("hi");
    harness.wait_until_idle_after(0).await;

    assert_eq!(harness.capture_calls.load(Ordering::SeqCst), 0);
    assert!(!harness.llm_requests()[0].to_string().contains("image_url"));
}

#[tokio::test]
async fn a_failed_screen_capture_still_gets_an_answer_with_a_notice() {
    let harness = start_harness(
        vec![sse_reply(&["still here"])],
        Err("permission denied"),
        |settings| {
            settings.tts.provider = TtsProvider::Off;
        },
    )
    .await;
    harness.send_text("hi");
    harness.wait_until_idle_after(0).await;

    let events = harness.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("notice:") && event.contains("permission denied")),
        "{events:#?}"
    );
    assert!(events.contains(&"assistant:still here".to_string()));
    assert!(!harness.llm_requests()[0].to_string().contains("image_url"));
}

#[tokio::test]
async fn pointing_is_ignored_when_the_overlay_is_off_or_there_is_no_screenshot() {
    let harness = start_harness(
        vec![sse_reply(&["here [POINT:10,10:x]"])],
        Ok(one_screen()),
        |settings| {
            settings.tts.provider = TtsProvider::Off;
            settings.show_pointer_overlay = false;
        },
    )
    .await;
    harness.send_text("hi");
    harness.wait_until_idle_after(0).await;
    assert!(harness.point_requests.lock().unwrap().is_empty());
    assert!(harness.events().contains(&"assistant:here".to_string()));
}

#[tokio::test]
async fn the_screen_number_in_the_tag_selects_the_right_monitor() {
    let screens = vec![
        screenshot(Rect::new(0.0, 0.0, 1000.0, 500.0), true, "screen 1"),
        screenshot(Rect::new(1000.0, 0.0, 2000.0, 1000.0), false, "screen 2"),
    ];
    let harness = start_harness(
        vec![sse_reply(&["over there [POINT:250,125:terminal:screen2]"])],
        Ok(screens),
        |settings| {
            settings.tts.provider = TtsProvider::Off;
        },
    )
    .await;
    harness.send_text("where is the terminal?");
    harness.wait_until_idle_after(0).await;

    let points = harness.point_requests.lock().unwrap().clone();
    // Half way across screen 2: x = 1000 + 0.5*2000, y = 0.5*1000.
    assert_eq!((points[0].target.x, points[0].target.y), (2000.0, 500.0));
}

#[tokio::test]
async fn provider_errors_are_shown_and_do_not_pollute_the_history() {
    let harness = start_harness(
        vec![
            LlmScript::Fail(401, r#"{"error":{"message":"Invalid API key"}}"#),
            sse_reply(&["recovered"]),
        ],
        Ok(one_screen()),
        |settings| settings.tts.provider = TtsProvider::Off,
    )
    .await;
    harness.send_text("hello?");
    harness.wait_until_idle_after(0).await;

    let events = harness.events();
    assert!(
        events.iter().any(|event| event.starts_with("error:")
            && event.contains("401")
            && event.contains("Invalid API key")),
        "{events:#?}"
    );
    assert!(!events.iter().any(|event| event.contains("llm-key")));

    harness.send_text("again");
    harness.wait_until_idle_after(1).await;
    // The failed question was not remembered.
    assert_eq!(
        harness.llm_requests()[1]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn a_missing_api_key_explains_what_to_do_without_calling_the_network() {
    let harness = start_harness(vec![sse_reply(&["x"])], Ok(one_screen()), |settings| {
        settings.providers.get_mut("custom").unwrap().api_key = Secret::default();
        settings.active_provider = "openai".into();
    })
    .await;
    harness.send_text("hi");
    harness.wait_until_idle_after(0).await;
    let events = harness.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("error:") && event.contains("API key")),
        "{events:#?}"
    );
    assert!(harness.llm_requests().is_empty());
}

#[tokio::test]
async fn a_text_only_model_is_retried_without_the_screenshot() {
    let harness = start_harness(
        vec![
            LlmScript::Fail(
                400,
                r#"{"error":{"message":"image input is not supported"}}"#,
            ),
            sse_reply(&["text only answer [POINT:5,5:x]"]),
        ],
        Ok(one_screen()),
        |settings| settings.tts.provider = TtsProvider::Off,
    )
    .await;
    harness.send_text("hi");
    harness.wait_until_idle_after(0).await;

    let events = harness.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("notice:") && event.contains("does not accept images")),
        "{events:#?}"
    );
    assert!(events.contains(&"assistant:text only answer".to_string()));
    // No screenshot was sent, so pointing coordinates cannot be mapped and are dropped.
    assert!(harness.point_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn voice_failures_keep_the_text_reply_and_report_the_problem() {
    let harness = start_harness(vec![sse_reply(&["hello"])], Ok(one_screen()), |settings| {
        settings.tts.elevenlabs_api_key = Secret::default();
    })
    .await;
    harness.send_text("hi");
    harness.wait_until_idle_after(0).await;
    let events = harness.events();
    assert!(events.contains(&"assistant:hello".to_string()));
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("error:") && event.contains("ElevenLabs")),
        "{events:#?}"
    );
    assert!(harness.player.played.lock().unwrap().is_empty());
}

#[tokio::test]
async fn new_input_interrupts_the_previous_answer_and_speech() {
    let slow = LlmScript::Reply {
        chunks: vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"a very \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"long \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
            "data: [DONE]\n\n",
        ],
        delay_ms: 300,
    };
    let harness = start_harness(
        vec![slow, sse_reply(&["quick answer"])],
        Ok(one_screen()),
        |settings| {
            settings.tts.provider = TtsProvider::Off;
        },
    )
    .await;

    harness.send_text("slow question");
    harness.wait_for("stream:a very").await;
    let stops_before = harness.player.stop_calls.load(Ordering::SeqCst);
    harness.send_text("quick question");
    harness.wait_for("assistant:quick answer").await;

    assert!(
        harness.player.stop_calls.load(Ordering::SeqCst) > stops_before,
        "speech must be stopped"
    );
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !harness
            .events()
            .iter()
            .any(|event| event.starts_with("assistant:a very")),
        "the interrupted answer must not finish"
    );
}

#[tokio::test]
async fn cancel_returns_to_idle_and_stops_the_answer() {
    let slow = LlmScript::Reply {
        chunks: vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"thinking \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hard\"}}]}\n\n",
            "data: [DONE]\n\n",
        ],
        delay_ms: 400,
    };
    let harness = start_harness(vec![slow], Ok(one_screen()), |settings| {
        settings.tts.provider = TtsProvider::Off
    })
    .await;
    harness.send_text("q");
    harness.wait_for("stream:thinking").await;
    harness.commands.send(Command::Cancel).unwrap();
    harness.wait_until_idle_after(0).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !harness
            .events()
            .iter()
            .any(|event| event.starts_with("assistant:"))
    );
}

#[tokio::test]
async fn speech_input_that_is_switched_off_says_so() {
    let harness = start_harness(vec![sse_reply(&["x"])], Ok(one_screen()), |_| {}).await;
    harness.commands.send(Command::StartListening).unwrap();
    harness.wait_for("error:").await;
    let events = harness.events();
    assert!(
        events
            .iter()
            .any(|event| event.contains("Speech input is turned off")),
        "{events:#?}"
    );
    assert_eq!(harness.idle_count(), 1);
    // Releasing with nothing recording is harmless.
    harness.commands.send(Command::StopListening).unwrap();
}

#[tokio::test]
async fn updated_settings_apply_to_the_next_turn() {
    let harness = start_harness(vec![sse_reply(&["ok"])], Ok(one_screen()), |settings| {
        settings.tts.provider = TtsProvider::Off;
    })
    .await;
    let mut new_settings = test_settings(&harness.mocks.base_url);
    new_settings.tts.provider = TtsProvider::Off;
    new_settings.providers.get_mut("custom").unwrap().model = "renamed-model".into();
    harness
        .commands
        .send(Command::UpdateSettings(Box::new(new_settings)))
        .unwrap();

    harness.send_text("hi");
    harness.wait_until_idle_after(0).await;
    assert_eq!(harness.llm_requests()[0]["model"], "renamed-model");
}
