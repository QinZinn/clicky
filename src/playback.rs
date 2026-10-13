//! Speaking replies aloud.
//!
//! The audio device lives on its own thread (the OS audio handles are not
//! `Send`) and is opened the first time something is played, so the app
//! starts fine on machines without speakers.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};

/// Something that can play a clip and report whether it is still playing.
/// The engine depends on this trait so tests can substitute a fake.
pub trait SpeechPlayer: Send + Sync {
    /// Starts playing encoded audio (MP3), replacing anything already playing.
    /// May block briefly while the audio device opens.
    fn play(&self, audio: Vec<u8>) -> Result<(), String>;
    fn stop(&self);
    fn is_playing(&self) -> bool;
}

enum Command {
    Play {
        audio: Vec<u8>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Stop,
}

pub struct RodioSpeechPlayer {
    commands: Mutex<mpsc::Sender<Command>>,
    is_playing: Arc<AtomicBool>,
}

impl RodioSpeechPlayer {
    pub fn new() -> Self {
        let (command_sender, command_receiver) = mpsc::channel::<Command>();
        let is_playing = Arc::new(AtomicBool::new(false));
        let thread_is_playing = Arc::clone(&is_playing);

        std::thread::Builder::new()
            .name("clicky-speaker".into())
            .spawn(move || run_audio_thread(command_receiver, thread_is_playing))
            .expect("spawning the audio thread");

        Self {
            commands: Mutex::new(command_sender),
            is_playing,
        }
    }
}

impl Default for RodioSpeechPlayer {
    fn default() -> Self {
        Self::new()
    }
}

fn run_audio_thread(commands: mpsc::Receiver<Command>, is_playing: Arc<AtomicBool>) {
    // Dropping the sink stops all sound, so it must outlive every Player.
    let mut sink: Option<MixerDeviceSink> = None;
    let mut player: Option<Player> = None;

    loop {
        match commands.recv_timeout(Duration::from_millis(50)) {
            Ok(Command::Play { audio, reply }) => {
                if let Some(previous) = player.take() {
                    previous.stop();
                }
                let result = start_playback(&mut sink, audio).map(|new_player| {
                    player = Some(new_player);
                });
                is_playing.store(result.is_ok(), Ordering::Relaxed);
                let _ = reply.send(result);
            }
            Ok(Command::Stop) => {
                if let Some(previous) = player.take() {
                    previous.stop();
                }
                is_playing.store(false, Ordering::Relaxed);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if player.as_ref().is_some_and(Player::empty) {
                    player = None;
                    is_playing.store(false, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn start_playback(sink: &mut Option<MixerDeviceSink>, audio: Vec<u8>) -> Result<Player, String> {
    if sink.is_none() {
        let opened = DeviceSinkBuilder::open_default_sink()
            .map_err(|error| format!("could not open the speakers: {error}"))?;
        *sink = Some(opened);
    }
    let sink = sink.as_ref().expect("opened above");
    let source = Decoder::new(Cursor::new(audio))
        .map_err(|error| format!("could not decode the reply audio: {error}"))?;
    let player = Player::connect_new(sink.mixer());
    player.append(source);
    Ok(player)
}

impl SpeechPlayer for RodioSpeechPlayer {
    fn play(&self, audio: Vec<u8>) -> Result<(), String> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.commands
            .lock()
            .map_err(|_| "audio thread poisoned".to_string())?
            .send(Command::Play {
                audio,
                reply: reply_sender,
            })
            .map_err(|_| "the audio thread has stopped".to_string())?;
        reply_receiver
            .recv()
            .map_err(|_| "the audio thread has stopped".to_string())?
    }

    fn stop(&self) {
        if let Ok(commands) = self.commands.lock() {
            let _ = commands.send(Command::Stop);
        }
        self.is_playing.store(false, Ordering::Relaxed);
    }

    fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::Relaxed)
    }
}
