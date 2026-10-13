//! Speech-to-text.
//!
//! A [`Recording`] owns the microphone and a background task that talks to a
//! transcription service. Two services are supported:
//!
//! * **AssemblyAI** real-time streaming (words appear while you talk).
//! * Any **OpenAI-compatible** `/audio/transcriptions` endpoint (the audio is
//!   uploaded when you let go).

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use crate::audio::{self, AudioLevel, MicCapture, MicError};
use crate::config::{Settings, SttProvider};
use crate::providers;

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("{0}")]
    NotConfigured(String),
    #[error(transparent)]
    Microphone(#[from] MicError),
    #[error("Speech recognition failed: {0}")]
    Service(String),
    #[error("Speech recognition was cancelled.")]
    Cancelled,
}

// ---------------------------------------------------------------------------
// AssemblyAI turn handling (pure logic)
// ---------------------------------------------------------------------------

/// Builds the full transcript out of AssemblyAI's per-turn messages.
///
/// A "turn" is one stretch of speech. While it is in progress the text keeps
/// changing; when it ends (or its formatted version arrives) it is stored.
/// Formatted text is never overwritten by unformatted text.
#[derive(Default, Debug)]
pub struct TranscriptAssembler {
    stored_turns: BTreeMap<i64, (String, bool)>,
    active_turn_order: Option<i64>,
    active_turn_text: String,
}

impl TranscriptAssembler {
    pub fn handle_turn(
        &mut self,
        turn_order: Option<i64>,
        transcript: &str,
        end_of_turn: bool,
        is_formatted: bool,
    ) {
        let transcript = transcript.trim();
        let turn_order = turn_order.or(self.active_turn_order).unwrap_or_else(|| {
            self.stored_turns
                .keys()
                .next_back()
                .map_or(0, |highest| highest + 1)
        });

        if end_of_turn || is_formatted {
            self.active_turn_order = None;
            self.active_turn_text.clear();
            if transcript.is_empty() {
                return;
            }
            if let Some((_, existing_is_formatted)) = self.stored_turns.get(&turn_order)
                && *existing_is_formatted
                && !is_formatted
            {
                return;
            }
            self.stored_turns
                .insert(turn_order, (transcript.to_string(), is_formatted));
        } else {
            self.active_turn_order = Some(turn_order);
            self.active_turn_text = transcript.to_string();
        }
    }

    pub fn compose(&self) -> String {
        let mut segments: Vec<&str> = self
            .stored_turns
            .values()
            .map(|(text, _)| text.as_str())
            .filter(|text| !text.is_empty())
            .collect();
        let active = self.active_turn_text.trim();
        if !active.is_empty() {
            segments.push(active);
        }
        segments.join(" ")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssemblyMessage {
    Begin,
    Turn {
        transcript: String,
        turn_order: Option<i64>,
        end_of_turn: bool,
        is_formatted: bool,
    },
    Termination,
    Error(String),
    Other,
}

pub fn parse_assembly_message(text: &str) -> Result<AssemblyMessage, serde_json::Error> {
    let value: Value = serde_json::from_str(text)?;
    let message_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    Ok(match message_type.as_str() {
        "begin" => AssemblyMessage::Begin,
        "turn" => AssemblyMessage::Turn {
            transcript: value
                .get("transcript")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            turn_order: value.get("turn_order").and_then(Value::as_i64),
            end_of_turn: value
                .get("end_of_turn")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_formatted: value
                .get("turn_is_formatted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "termination" => AssemblyMessage::Termination,
        "error" => AssemblyMessage::Error(
            value
                .get("error")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("AssemblyAI returned an error.")
                .to_string(),
        ),
        _ => AssemblyMessage::Other,
    })
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// Where the AssemblyAI client connects. Overridable for tests.
#[derive(Clone, Debug)]
pub struct AssemblyAiEndpoints {
    pub token_url: String,
    pub websocket_url: String,
}

impl Default for AssemblyAiEndpoints {
    fn default() -> Self {
        Self {
            token_url: "https://streaming.assemblyai.com/v3/token?expires_in_seconds=480".into(),
            websocket_url: "wss://streaming.assemblyai.com/v3/ws".into(),
        }
    }
}

/// How long to wait for AssemblyAI's last turn after the user lets go.
const FINAL_TRANSCRIPT_GRACE: Duration = Duration::from_millis(1_400);
/// After an unformatted end-of-turn, wait this long for the formatted text.
const FORMATTED_TEXT_GRACE: Duration = Duration::from_millis(500);
/// Give up if the service never accepts the connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// An in-progress push-to-talk recording.
pub struct Recording {
    microphone: Option<MicCapture>,
    finish_sender: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<String, SttError>>>,
    partial_transcript: watch::Receiver<String>,
}

impl Recording {
    /// Opens the microphone and starts talking to the configured service.
    /// Must be called from within a Tokio runtime.
    pub fn start(
        http: reqwest::Client,
        settings: &Settings,
        level: AudioLevel,
    ) -> Result<Recording, SttError> {
        Self::start_with_endpoints(http, settings, level, AssemblyAiEndpoints::default())
    }

    pub fn start_with_endpoints(
        http: reqwest::Client,
        settings: &Settings,
        level: AudioLevel,
        endpoints: AssemblyAiEndpoints,
    ) -> Result<Recording, SttError> {
        let (audio_sender, audio_receiver) = tokio::sync::mpsc::unbounded_channel();
        let microphone =
            Self::check_configuration_then_open_microphone(settings, level, audio_sender)?;
        Ok(Self::spawn_task(
            http,
            settings,
            endpoints,
            microphone,
            audio_receiver,
        ))
    }

    fn check_configuration_then_open_microphone(
        settings: &Settings,
        level: AudioLevel,
        audio_sender: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<MicCapture, SttError> {
        // Fail on missing configuration before the mic indicator lights up.
        match settings.stt.provider {
            SttProvider::Disabled => {
                return Err(SttError::NotConfigured(
                    "Speech input is turned off. Pick a speech service in Settings, or type your message."
                        .into(),
                ));
            }
            SttProvider::AssemblyAi if settings.assemblyai_api_key().is_empty() => {
                return Err(SttError::NotConfigured(
                    "Add your AssemblyAI API key in Settings to use speech input.".into(),
                ));
            }
            SttProvider::OpenAiCompatible if settings.whisper_api_key().is_empty() => {
                return Err(SttError::NotConfigured(
                    "Add an API key for the transcription service in Settings.".into(),
                ));
            }
            _ => {}
        }
        Ok(MicCapture::start(level, audio_sender)?)
    }

    fn spawn_task(
        http: reqwest::Client,
        settings: &Settings,
        endpoints: AssemblyAiEndpoints,
        microphone: MicCapture,
        audio_receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Recording {
        let (finish_sender, finish_receiver) = oneshot::channel();
        let (partial_sender, partial_receiver) = watch::channel(String::new());

        let task = match settings.stt.provider {
            SttProvider::AssemblyAi => {
                let api_key = settings.assemblyai_api_key();
                tokio::spawn(run_assemblyai_session(
                    http,
                    endpoints,
                    api_key,
                    audio_receiver,
                    finish_receiver,
                    partial_sender,
                ))
            }
            SttProvider::OpenAiCompatible => {
                let upload = WhisperUpload {
                    base_url: settings.stt.whisper_base_url.clone(),
                    api_key: settings.whisper_api_key(),
                    model: settings.stt.whisper_model.clone(),
                };
                tokio::spawn(run_whisper_upload(
                    http,
                    upload,
                    audio_receiver,
                    finish_receiver,
                ))
            }
            SttProvider::Disabled => unreachable!("rejected before the microphone was opened"),
        };

        Recording {
            microphone: Some(microphone),
            finish_sender: Some(finish_sender),
            task: Some(task),
            partial_transcript: partial_receiver,
        }
    }

    /// Live text while the user is still talking (AssemblyAI only).
    pub fn partial_transcript(&self) -> watch::Receiver<String> {
        self.partial_transcript.clone()
    }

    /// Stops the microphone and returns the final transcript.
    pub async fn finish(mut self) -> Result<String, SttError> {
        if let Some(microphone) = self.microphone.take() {
            // Joins the audio thread; it flushes the last chunk first.
            let _ = tokio::task::spawn_blocking(move || microphone.stop()).await;
        }
        if let Some(finish_sender) = self.finish_sender.take() {
            let _ = finish_sender.send(());
        }
        let task = self.task.take().expect("task is present until finish");
        task.await
            .map_err(|error| SttError::Service(format!("internal error: {error}")))?
    }

    /// Discards the recording.
    pub fn cancel(self) {
        // Drop does the work: it stops the mic and aborts the task.
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// AssemblyAI session
// ---------------------------------------------------------------------------

async fn fetch_assemblyai_token(
    http: &reqwest::Client,
    token_url: &str,
    api_key: &str,
) -> Result<String, SttError> {
    let response = http
        .get(token_url)
        .header("Authorization", api_key)
        .send()
        .await
        .map_err(|error| {
            SttError::Service(format!(
                "could not reach AssemblyAI: {}",
                error.without_url()
            ))
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    // AssemblyAI answers a bad key with HTTP 404 and {"detail":"Invalid API key"}
    // (verified against the live API), so go by the message as well as the status.
    let body_says_bad_key = body.to_lowercase().contains("invalid api key");
    if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
        || body_says_bad_key
    {
        return Err(SttError::Service(
            "AssemblyAI rejected the API key. Check it in Settings.".into(),
        ));
    }
    if !status.is_success() {
        return Err(SttError::Service(format!(
            "AssemblyAI token request failed (HTTP {}): {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        )));
    }
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| {
            SttError::Service("AssemblyAI returned an unexpected token response.".into())
        })
}

fn assemblyai_websocket_url(base: &str, token: &str) -> Result<String, SttError> {
    reqwest::Url::parse_with_params(
        base,
        [
            ("sample_rate", "16000"),
            ("encoding", "pcm_s16le"),
            ("format_turns", "true"),
            ("speech_model", "u3-rt-pro"),
            ("token", token),
        ],
    )
    .map(|url| url.to_string())
    .map_err(|error| SttError::Service(format!("bad AssemblyAI URL: {error}")))
}

/// Runs one AssemblyAI streaming session over caller-supplied channels: PCM16
/// chunks in, final transcript out. [`Recording`] wires this to the microphone;
/// tests wire it to a mock server.
pub async fn run_assemblyai_session(
    http: reqwest::Client,
    endpoints: AssemblyAiEndpoints,
    api_key: String,
    mut audio_receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    mut finish_receiver: oneshot::Receiver<()>,
    partial_sender: watch::Sender<String>,
) -> Result<String, SttError> {
    // Audio recorded while we connect piles up in `audio_receiver` and is
    // sent once the service says it is ready, so no words are lost.
    let token = fetch_assemblyai_token(&http, &endpoints.token_url, &api_key).await?;
    let url = assemblyai_websocket_url(&endpoints.websocket_url, &token)?;
    let (websocket, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
            .await
            .map_err(|_| SttError::Service("timed out connecting to AssemblyAI".into()))?
            .map_err(|error| {
                SttError::Service(format!("could not connect to AssemblyAI: {error}"))
            })?;
    let (mut sink, mut stream) = websocket.split();

    let mut assembler = TranscriptAssembler::default();
    let mut session_is_ready = false;
    let mut finish_requested = false;
    let mut force_endpoint_sent = false;
    let mut deadline: Option<Instant> =
        Some(Instant::now() + CONNECT_TIMEOUT + Duration::from_secs(60));
    let connect_deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut result: Option<Result<String, SttError>> = None;

    while result.is_none() {
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else {
                    // Server closed the socket.
                    let text = assembler.compose();
                    result = Some(if finish_requested && !text.is_empty() {
                        Ok(text)
                    } else {
                        Err(SttError::Service("the connection to AssemblyAI closed unexpectedly".into()))
                    });
                    continue;
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        let text = assembler.compose();
                        result = Some(if finish_requested && !text.is_empty() {
                            Ok(text) // deliver what we have
                        } else {
                            Err(SttError::Service(error.to_string()))
                        });
                        continue;
                    }
                };
                let Message::Text(text) = message else { continue };
                match parse_assembly_message(text.as_str()) {
                    Ok(AssemblyMessage::Begin) => session_is_ready = true,
                    Ok(AssemblyMessage::Turn { transcript, turn_order, end_of_turn, is_formatted }) => {
                        assembler.handle_turn(turn_order, &transcript, end_of_turn, is_formatted);
                        let _ = partial_sender.send(assembler.compose());
                        if force_endpoint_sent {
                            if is_formatted {
                                result = Some(Ok(assembler.compose()));
                            } else if end_of_turn {
                                // Formatted text follows immediately; give it a moment.
                                let formatted_deadline = Instant::now() + FORMATTED_TEXT_GRACE;
                                deadline = Some(deadline.map_or(formatted_deadline, |existing_deadline| existing_deadline.min(formatted_deadline)));
                            }
                        }
                    }
                    Ok(AssemblyMessage::Termination) => {
                        result = Some(Ok(assembler.compose()));
                    }
                    Ok(AssemblyMessage::Error(message)) => {
                        let text = assembler.compose();
                        result = Some(if finish_requested && !text.is_empty() {
                            Ok(text)
                        } else {
                            Err(SttError::Service(message))
                        });
                    }
                    Ok(AssemblyMessage::Other) => {}
                    Err(error) => tracing::debug!("ignoring unparseable AssemblyAI message: {error}"),
                }
            }
            chunk = audio_receiver.recv(), if session_is_ready && !force_endpoint_sent => {
                if let Some(chunk) = chunk
                    && let Err(error) = sink.send(Message::Binary(chunk.into())).await {
                        result = Some(Err(SttError::Service(error.to_string())));
                    }
            }
            finish = &mut finish_receiver, if !finish_requested => {
                if finish.is_err() {
                    result = Some(Err(SttError::Cancelled));
                } else {
                    finish_requested = true;
                }
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                let text = assembler.compose();
                result = Some(if finish_requested || !text.is_empty() {
                    Ok(text)
                } else {
                    Err(SttError::Service("AssemblyAI did not respond in time".into()))
                });
            }
        }

        if result.is_some() {
            break;
        }
        if !session_is_ready && Instant::now() > connect_deadline {
            result = Some(Err(SttError::Service(
                "AssemblyAI never confirmed the session".into(),
            )));
            break;
        }
        if finish_requested && session_is_ready && !force_endpoint_sent {
            // Everything recorded is already queued; flush it, then ask the
            // service to finalise the current turn.
            while let Ok(chunk) = audio_receiver.try_recv() {
                if sink.send(Message::Binary(chunk.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink
                .send(Message::text(json!({"type": "ForceEndpoint"}).to_string()))
                .await;
            force_endpoint_sent = true;
            deadline = Some(Instant::now() + FINAL_TRANSCRIPT_GRACE);
        }
    }

    let _ = sink
        .send(Message::text(json!({"type": "Terminate"}).to_string()))
        .await;
    let _ = sink.close().await;
    result.expect("loop only exits with a result")
}

// ---------------------------------------------------------------------------
// OpenAI-compatible upload
// ---------------------------------------------------------------------------

pub struct WhisperUpload {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Recordings shorter than this are treated as an accidental key press.
const MINIMUM_UPLOAD_DURATION_MS: u64 = 300;

/// Collects PCM16 chunks until `finish_receiver` fires, then uploads them as a
/// WAV file to `{base_url}/audio/transcriptions`.
pub async fn run_whisper_upload(
    http: reqwest::Client,
    upload: WhisperUpload,
    mut audio_receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    finish_receiver: oneshot::Receiver<()>,
) -> Result<String, SttError> {
    let mut recorded_pcm: Vec<u8> = Vec::new();
    let mut finish_receiver = finish_receiver;
    loop {
        tokio::select! {
            chunk = audio_receiver.recv() => match chunk {
                Some(chunk) => recorded_pcm.extend_from_slice(&chunk),
                None => break,
            },
            finish = &mut finish_receiver => {
                if finish.is_err() {
                    return Err(SttError::Cancelled);
                }
                while let Ok(chunk) = audio_receiver.try_recv() {
                    recorded_pcm.extend_from_slice(&chunk);
                }
                break;
            }
        }
    }

    if audio::pcm16_duration_ms(recorded_pcm.len()) < MINIMUM_UPLOAD_DURATION_MS {
        return Ok(String::new());
    }

    let wav = audio::encode_wav_pcm16_mono(&recorded_pcm, audio::TARGET_SAMPLE_RATE);
    let url = format!(
        "{}/audio/transcriptions",
        providers::normalize_base_url(upload.base_url.trim_end_matches("/audio/transcriptions"))
    );
    let file_part = reqwest::multipart::Part::bytes(wav)
        .file_name("speech.wav")
        .mime_str("audio/wav")
        .expect("static mime type is valid");
    let form = reqwest::multipart::Form::new()
        .text("model", upload.model.clone())
        .text("response_format", "json")
        .part("file", file_part);

    let mut request = http.post(&url).multipart(form);
    if !upload.api_key.is_empty() {
        request = request.bearer_auth(&upload.api_key);
    }
    let response = request.send().await.map_err(|error| {
        SttError::Service(format!(
            "could not reach the transcription service: {}",
            error.without_url()
        ))
    })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(SttError::Service(format!(
            "HTTP {}: {}",
            status.as_u16(),
            crate::llm::summarize_error_body(&body)
        )));
    }
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("text")
                .and_then(Value::as_str)
                .map(|text| text.trim().to_string())
        })
        .ok_or_else(|| {
            SttError::Service("unexpected response from the transcription service".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembler_joins_turns_in_order_and_shows_the_active_one() {
        let mut assembler = TranscriptAssembler::default();
        assembler.handle_turn(Some(0), "hello there", true, false);
        assembler.handle_turn(Some(1), "how are", false, false);
        assert_eq!(assembler.compose(), "hello there how are");
        assembler.handle_turn(Some(1), "how are you", true, true);
        assert_eq!(assembler.compose(), "hello there how are you");
    }

    #[test]
    fn formatted_text_is_never_replaced_by_unformatted() {
        let mut assembler = TranscriptAssembler::default();
        assembler.handle_turn(Some(0), "Hello, there.", true, true);
        assembler.handle_turn(Some(0), "hello there", true, false);
        assert_eq!(assembler.compose(), "Hello, there.");
    }

    #[test]
    fn formatted_text_replaces_unformatted_for_the_same_turn() {
        let mut assembler = TranscriptAssembler::default();
        assembler.handle_turn(Some(0), "hello there", true, false);
        assembler.handle_turn(Some(0), "Hello, there.", true, true);
        assert_eq!(assembler.compose(), "Hello, there.");
    }

    #[test]
    fn turns_without_an_order_get_sequential_ones() {
        let mut assembler = TranscriptAssembler::default();
        assembler.handle_turn(None, "one", true, false);
        assembler.handle_turn(None, "two", true, false);
        assert_eq!(assembler.compose(), "one two");
    }

    #[test]
    fn empty_final_turns_are_ignored() {
        let mut assembler = TranscriptAssembler::default();
        assembler.handle_turn(Some(0), "words", true, true);
        assembler.handle_turn(Some(1), "   ", true, true);
        assert_eq!(assembler.compose(), "words");
    }

    #[test]
    fn parses_assemblyai_messages() {
        assert_eq!(
            parse_assembly_message(r#"{"type":"Begin","id":"x"}"#).unwrap(),
            AssemblyMessage::Begin
        );
        assert_eq!(
            parse_assembly_message(
                r#"{"type":"Turn","transcript":"hi","turn_order":2,"end_of_turn":true,"turn_is_formatted":false}"#
            )
            .unwrap(),
            AssemblyMessage::Turn {
                transcript: "hi".into(),
                turn_order: Some(2),
                end_of_turn: true,
                is_formatted: false
            }
        );
        assert_eq!(
            parse_assembly_message(r#"{"type":"Termination"}"#).unwrap(),
            AssemblyMessage::Termination
        );
        assert_eq!(
            parse_assembly_message(r#"{"type":"Error","error":"bad audio"}"#).unwrap(),
            AssemblyMessage::Error("bad audio".into())
        );
        assert_eq!(
            parse_assembly_message(r#"{"type":"Whatever"}"#).unwrap(),
            AssemblyMessage::Other
        );
        assert!(parse_assembly_message("not json").is_err());
    }

    #[test]
    fn websocket_url_carries_audio_format_and_encoded_token() {
        let url =
            assemblyai_websocket_url("wss://streaming.assemblyai.com/v3/ws", "a b+c").unwrap();
        assert!(url.starts_with("wss://streaming.assemblyai.com/v3/ws?"));
        for expected in [
            "sample_rate=16000",
            "encoding=pcm_s16le",
            "format_turns=true",
            "speech_model=u3-rt-pro",
        ] {
            assert!(url.contains(expected), "{url}");
        }
        assert!(url.contains("token=a+b%2Bc"), "{url}");
    }
}
