//! The brain of the app: turns input (a held shortcut or typed text) into a
//! reply, while telling the UI what is going on.
//!
//! One *turn* is: transcript → screenshots → model → pointing + history →
//! optional spoken reply. Everything that touches hardware (screen capture,
//! speakers) is injected, so the whole turn can be tested without any.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::audio::AudioLevel;
use crate::config::{Settings, TtsProvider};
use crate::geometry::{self, Point, Rect};
use crate::llm::{self, ChatImage, ChatInput, Exchange, LlmTarget, ModelInfo};
use crate::playback::SpeechPlayer;
use crate::pointing;
use crate::prompt::COMPANION_SYSTEM_PROMPT;
use crate::providers::{self, FormatRule, WireFormat};
use crate::screen::{self, ScreenError, Screenshot};
use crate::stt::Recording;
use crate::tts;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceState {
    Idle,
    /// The shortcut / button is held and the microphone is open.
    Listening,
    /// Waiting for the final transcript.
    Transcribing,
    /// Waiting for the model.
    Thinking,
    /// Reading the reply aloud.
    Speaking,
}

/// A request for the blue cursor to fly to a spot on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct PointRequest {
    /// Where to point, on the virtual desktop (logical points).
    pub target: Point,
    /// The monitor the target is on.
    pub monitor_rect: Rect,
    pub label: String,
}

#[derive(Debug)]
pub enum UiEvent {
    StateChanged(VoiceState),
    /// Live transcript while the user is talking.
    PartialTranscript(String),
    UserMessage(String),
    /// The reply so far (replaces any previous streaming text).
    AssistantStreaming(String),
    /// The finished reply, with pointing markup removed.
    AssistantMessage(String),
    Notice(String),
    Error(String),
    PointAt(PointRequest),
    Models {
        provider_id: String,
        result: Result<Vec<ModelInfo>, String>,
    },
}

#[derive(Debug)]
pub enum Command {
    StartListening,
    StopListening,
    SendText(String),
    /// Stop whatever is happening and go back to idle.
    Cancel,
    ClearConversation,
    UpdateSettings(Box<Settings>),
    RefreshModels {
        provider_id: String,
        base_url: String,
        api_key: String,
    },
}

pub type EventSink = Arc<dyn Fn(UiEvent) + Send + Sync>;
pub type ScreenCaptureFunction =
    Arc<dyn Fn() -> Result<Vec<Screenshot>, ScreenError> + Send + Sync>;

/// Everything the engine needs from the outside world.
#[derive(Clone)]
pub struct EngineDependencies {
    pub capture_screens: ScreenCaptureFunction,
    pub player: Arc<dyn SpeechPlayer>,
    pub tts_base_url: String,
}

impl EngineDependencies {
    /// The real thing: actual screen capture and speakers.
    pub fn real() -> Self {
        Self {
            capture_screens: Arc::new(screen::capture_all_screens),
            player: Arc::new(crate::playback::RodioSpeechPlayer::new()),
            tts_base_url: tts::ELEVENLABS_BASE_URL.to_string(),
        }
    }
}

/// Everything one turn needs. Cheap to clone.
#[derive(Clone)]
pub struct TurnContext {
    pub settings: Arc<Settings>,
    pub http: reqwest::Client,
    pub dependencies: EngineDependencies,
    pub events: EventSink,
    pub history: Arc<Mutex<Vec<Exchange>>>,
}

fn report_error(events: &EventSink, message: String) {
    events(UiEvent::Error(message));
    events(UiEvent::StateChanged(VoiceState::Idle));
}

/// Longest we wait for a spoken reply to finish before moving on.
const MAXIMUM_SPEECH_WAIT: Duration = Duration::from_secs(600);

/// Runs one full turn for `user_text`. Cancel it by aborting the task.
pub async fn run_turn(context: TurnContext, user_text: String) {
    let events = context.events.clone();
    events(UiEvent::UserMessage(user_text.clone()));
    events(UiEvent::StateChanged(VoiceState::Thinking));

    let settings = &context.settings;
    let target = match LlmTarget::from_settings(settings) {
        Ok(target) => target,
        Err(error) => return report_error(&events, error.to_string()),
    };
    let provider_config = settings.active_provider_config();

    // Screenshots. A failure here should not block a text-only answer.
    let mut screenshots: Vec<Screenshot> = Vec::new();
    if provider_config.send_screenshots {
        let capture = Arc::clone(&context.dependencies.capture_screens);
        match tokio::task::spawn_blocking(move || capture()).await {
            Ok(Ok(captured)) => screenshots = captured,
            Ok(Err(error)) => events(UiEvent::Notice(format!(
                "Answering without looking at your screen. {error}"
            ))),
            Err(_) => {}
        }
    }

    let images: Vec<ChatImage> = screenshots
        .iter()
        .map(|screenshot| ChatImage {
            jpeg: screenshot.jpeg.clone(),
            label: screenshot.label.clone(),
        })
        .collect();
    let history_snapshot: Vec<Exchange> = context.history.lock().expect("history lock").clone();
    let input = ChatInput {
        system_prompt: COMPANION_SYSTEM_PROMPT,
        history: &history_snapshot,
        user_text: &user_text,
        images: &images,
    };

    let mut reply_so_far = String::new();
    let streaming_events = events.clone();
    let outcome = llm::chat(
        &context.http,
        &target,
        &input,
        provider_config.send_screenshots,
        |piece| {
            reply_so_far.push_str(piece);
            streaming_events(UiEvent::AssistantStreaming(pointing::text_for_display(
                &reply_so_far,
            )));
        },
    )
    .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return report_error(&events, error.to_string()),
    };
    if outcome.screenshots_dropped {
        screenshots.clear();
        events(UiEvent::Notice(
            "This model does not accept images, so I answered without seeing your screen. \
             Turn off 'Send screenshots' for it in Settings to skip the retry."
                .into(),
        ));
    }

    let parsed = pointing::parse_pointing_coordinates(&outcome.text);
    let spoken_text = parsed.spoken_text.clone();

    {
        let mut history = context.history.lock().expect("history lock");
        history.push(Exchange {
            user: user_text,
            assistant: spoken_text.clone(),
        });
        let keep = settings.history_exchanges.max(1);
        if history.len() > keep {
            let excess = history.len() - keep;
            history.drain(..excess);
        }
    }
    events(UiEvent::AssistantMessage(spoken_text.clone()));

    // Point before speaking, so the cursor is already flying while the voice
    // is generated.
    if settings.show_pointer_overlay
        && let Some(coordinate) = parsed.coordinate
        && let Some(screenshot) = screen::pick_screenshot(&screenshots, parsed.screen_number)
    {
        let target = geometry::screenshot_point_to_desktop(
            coordinate,
            (screenshot.width_in_pixels, screenshot.height_in_pixels),
            screenshot.monitor_rect,
        );
        events(UiEvent::PointAt(PointRequest {
            target,
            monitor_rect: screenshot.monitor_rect,
            label: parsed.element_label.clone().unwrap_or_default(),
        }));
    }

    if settings.tts.provider == TtsProvider::ElevenLabs && !spoken_text.trim().is_empty() {
        speak(&context, &spoken_text).await;
    }
    events(UiEvent::StateChanged(VoiceState::Idle));
}

async fn speak(context: &TurnContext, text: &str) {
    let events = &context.events;
    let settings = &context.settings;
    let audio = match tts::synthesize(
        &context.http,
        &context.dependencies.tts_base_url,
        &settings.elevenlabs_api_key(),
        &settings.tts.voice_id,
        &settings.tts.model_id,
        text,
    )
    .await
    {
        Ok(audio) => audio,
        Err(error) => {
            events(UiEvent::Error(format!("Voice reply failed: {error}")));
            return;
        }
    };

    let player = Arc::clone(&context.dependencies.player);
    match tokio::task::spawn_blocking(move || player.play(audio)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            events(UiEvent::Error(format!("Could not play the reply: {error}")));
            return;
        }
        Err(_) => return,
    }

    events(UiEvent::StateChanged(VoiceState::Speaking));
    let started = tokio::time::Instant::now();
    while context.dependencies.player.is_playing() && started.elapsed() < MAXIMUM_SPEECH_WAIT {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// The command loop
// ---------------------------------------------------------------------------

struct Engine {
    settings: Arc<Settings>,
    http: reqwest::Client,
    dependencies: EngineDependencies,
    events: EventSink,
    audio_level: AudioLevel,
    history: Arc<Mutex<Vec<Exchange>>>,
    recording: Option<Recording>,
    turn_task: Option<JoinHandle<()>>,
}

/// Starts the engine on `runtime` and returns the channel to command it with.
pub fn spawn_engine(
    runtime: &tokio::runtime::Handle,
    settings: Settings,
    dependencies: EngineDependencies,
    events: EventSink,
    audio_level: AudioLevel,
) -> UnboundedSender<Command> {
    let (command_sender, command_receiver) = unbounded_channel();
    let engine = Engine {
        http: llm::build_http_client(&settings),
        settings: Arc::new(settings),
        dependencies,
        events,
        audio_level,
        history: Arc::new(Mutex::new(Vec::new())),
        recording: None,
        turn_task: None,
    };
    runtime.spawn(engine.run(command_receiver));
    command_sender
}

impl Engine {
    async fn run(mut self, mut commands: UnboundedReceiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::StartListening => self.start_listening().await,
                Command::StopListening => self.stop_listening(),
                Command::SendText(text) => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        self.interrupt();
                        self.start_turn(text);
                    }
                }
                Command::Cancel => {
                    self.interrupt();
                    (self.events)(UiEvent::PartialTranscript(String::new()));
                    (self.events)(UiEvent::StateChanged(VoiceState::Idle));
                }
                Command::ClearConversation => self.history.lock().expect("history lock").clear(),
                Command::UpdateSettings(settings) => {
                    self.http = llm::build_http_client(&settings);
                    self.settings = Arc::new(*settings);
                }
                Command::RefreshModels {
                    provider_id,
                    base_url,
                    api_key,
                } => {
                    self.refresh_models(provider_id, base_url, api_key);
                }
            }
        }
        self.interrupt();
    }

    /// Stops any recording, running turn and speech.
    fn interrupt(&mut self) {
        self.recording = None; // dropping stops the microphone and the upload
        if let Some(task) = self.turn_task.take() {
            task.abort();
        }
        self.dependencies.player.stop();
    }

    fn turn_context(&self) -> TurnContext {
        TurnContext {
            settings: Arc::clone(&self.settings),
            http: self.http.clone(),
            dependencies: self.dependencies.clone(),
            events: Arc::clone(&self.events),
            history: Arc::clone(&self.history),
        }
    }

    fn start_turn(&mut self, user_text: String) {
        self.turn_task = Some(tokio::spawn(run_turn(self.turn_context(), user_text)));
    }

    async fn start_listening(&mut self) {
        if self.recording.is_some() {
            return;
        }
        // Speaking again interrupts the previous answer, like talking over someone.
        self.interrupt();

        let http = self.http.clone();
        let settings = Arc::clone(&self.settings);
        let level = self.audio_level.clone();
        let started =
            tokio::task::spawn_blocking(move || Recording::start(http, &settings, level)).await;

        match started {
            Ok(Ok(recording)) => {
                let mut partial_transcripts = recording.partial_transcript();
                let events = Arc::clone(&self.events);
                tokio::spawn(async move {
                    while partial_transcripts.changed().await.is_ok() {
                        let text = partial_transcripts.borrow().clone();
                        events(UiEvent::PartialTranscript(text));
                    }
                });
                self.recording = Some(recording);
                (self.events)(UiEvent::StateChanged(VoiceState::Listening));
            }
            Ok(Err(error)) => report_error(&self.events, error.to_string()),
            Err(error) => report_error(&self.events, format!("internal error: {error}")),
        }
    }

    fn stop_listening(&mut self) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        (self.events)(UiEvent::StateChanged(VoiceState::Transcribing));

        let context = self.turn_context();
        self.turn_task = Some(tokio::spawn(async move {
            let events = Arc::clone(&context.events);
            let transcript = recording.finish().await;
            events(UiEvent::PartialTranscript(String::new()));
            match transcript {
                Ok(text) if !text.trim().is_empty() => {
                    run_turn(context, text.trim().to_string()).await
                }
                Ok(_) => {
                    events(UiEvent::Notice(
                        "I didn't catch that. Hold the button and try again.".into(),
                    ));
                    events(UiEvent::StateChanged(VoiceState::Idle));
                }
                Err(error) => report_error(&events, error.to_string()),
            }
        }));
    }

    fn refresh_models(&self, provider_id: String, base_url: String, api_key: String) {
        let http = self.http.clone();
        let events = Arc::clone(&self.events);
        // Anthropic authenticates with x-api-key; everyone else with a bearer token.
        let format = match providers::preset(&provider_id).map(|preset| preset.format_rule) {
            Some(FormatRule::Fixed(WireFormat::AnthropicMessages)) => WireFormat::AnthropicMessages,
            _ => WireFormat::OpenAiChat,
        };
        tokio::spawn(async move {
            let result = llm::list_models(&http, &base_url, &api_key, format)
                .await
                .map_err(|error| error.to_string());
            events(UiEvent::Models {
                provider_id,
                result,
            });
        });
    }
}
