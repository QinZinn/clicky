//! Microphone capture and PCM helpers.
//!
//! The capture thread converts whatever the sound card gives us (any sample
//! format, any rate, any channel count) into 16 kHz mono signed 16-bit
//! little-endian PCM, delivered in 100 ms chunks. That is what both speech
//! services we support want.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use tokio::sync::mpsc::UnboundedSender;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
/// 100 ms of audio at 16 kHz.
const CHUNK_SAMPLES: usize = 1_600;
/// AssemblyAI rejects chunks shorter than 50 ms, so the final partial chunk
/// is padded with silence up to this length.
const MINIMUM_FINAL_CHUNK_SAMPLES: usize = 800;

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Streaming linear-interpolation resampler. Keeps its position between
/// calls, so audio callbacks of any size produce a seamless output.
pub struct Resampler {
    /// Input samples consumed per output sample.
    step: f64,
    /// Read position relative to the start of the next buffer. Index 0 of that
    /// buffer is `previous_sample` (once we have one).
    position: f64,
    previous_sample: Option<f32>,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: f64::from(input_rate) / f64::from(output_rate),
            position: 0.0,
            previous_sample: None,
        }
    }

    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        // Prepend the last sample of the previous call so we can interpolate
        // across the boundary.
        let mut buffer: Vec<f32> = Vec::with_capacity(input.len() + 1);
        if let Some(previous) = self.previous_sample {
            buffer.push(previous);
        }
        buffer.extend_from_slice(input);

        let mut position = self.position;
        while position + 1.0 < buffer.len() as f64 {
            let index = position.floor() as usize;
            let fraction = (position - index as f64) as f32;
            output.push(buffer[index] * (1.0 - fraction) + buffer[index + 1] * fraction);
            position += self.step;
        }

        let last_index = buffer.len() - 1;
        self.previous_sample = Some(buffer[last_index]);
        self.position = position - last_index as f64;
    }
}

/// Averages interleaved channels into mono.
pub fn downmix_to_mono(interleaved: &[f32], channels: usize, mono: &mut Vec<f32>) {
    let channels = channels.max(1);
    mono.clear();
    for frame in interleaved.chunks(channels) {
        mono.push(frame.iter().sum::<f32>() / frame.len() as f32);
    }
}

/// Converts floating-point samples (-1.0..=1.0) to little-endian i16 bytes.
pub fn f32_to_pcm16_le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Root-mean-square loudness of a block of samples.
pub fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Wraps mono 16-bit little-endian PCM in a WAV container.
pub fn encode_wav_pcm16_mono(pcm_little_endian: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_length = pcm_little_endian.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm_little_endian.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_length).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_length.to_le_bytes());
    wav.extend_from_slice(pcm_little_endian);
    wav
}

/// Duration of mono 16 kHz PCM16 audio, in milliseconds.
pub fn pcm16_duration_ms(byte_count: usize) -> u64 {
    (byte_count as u64 / 2) * 1000 / u64::from(TARGET_SAMPLE_RATE)
}

/// Loudness shared between the audio thread and the UI (0.0..=1.0).
#[derive(Clone, Default)]
pub struct AudioLevel(Arc<AtomicU32>);

impl AudioLevel {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, level: f32) {
        self.0.store(level.to_bits(), Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum MicError {
    #[error(
        "No microphone was found. Connect one and check that Clicky is allowed to use it \
         in your system's privacy settings."
    )]
    NoDevice,
    #[error("Could not open the microphone: {0}")]
    Open(String),
}

struct CaptureState {
    channels: usize,
    resampler: Resampler,
    level: AudioLevel,
    chunk_sender: UnboundedSender<Vec<u8>>,
    pending_samples: Vec<f32>,
    mono_scratch: Vec<f32>,
    resampled_scratch: Vec<f32>,
}

impl CaptureState {
    fn handle_block(&mut self, interleaved: &[f32]) {
        downmix_to_mono(interleaved, self.channels, &mut self.mono_scratch);

        // Fast attack, slow decay so the meter is readable.
        let loudness = (rms_level(&self.mono_scratch) * 6.0).min(1.0);
        self.level.set(loudness.max(self.level.get() * 0.85));

        self.resampled_scratch.clear();
        self.resampler
            .process(&self.mono_scratch, &mut self.resampled_scratch);
        self.pending_samples
            .extend_from_slice(&self.resampled_scratch);

        while self.pending_samples.len() >= CHUNK_SAMPLES {
            let chunk: Vec<f32> = self.pending_samples.drain(..CHUNK_SAMPLES).collect();
            // The receiver going away just means the session ended.
            let _ = self.chunk_sender.send(f32_to_pcm16_le(&chunk));
        }
    }

    /// Sends whatever is left so the end of the last word is not clipped.
    fn flush(&mut self) {
        if self.pending_samples.is_empty() {
            return;
        }
        let missing = MINIMUM_FINAL_CHUNK_SAMPLES.saturating_sub(self.pending_samples.len());
        self.pending_samples
            .extend(std::iter::repeat_n(0.0, missing));
        let samples = std::mem::take(&mut self.pending_samples);
        let _ = self.chunk_sender.send(f32_to_pcm16_le(&samples));
        self.level.set(0.0);
    }
}

/// An open microphone. Audio flows until this is dropped.
pub struct MicCapture {
    stop_sender: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl MicCapture {
    /// Starts capturing from the default input device. PCM16 chunks are sent
    /// on `chunk_sender`; `level` is updated continuously for the UI meter.
    pub fn start(
        level: AudioLevel,
        chunk_sender: UnboundedSender<Vec<u8>>,
    ) -> Result<MicCapture, MicError> {
        let (ready_sender, ready_receiver) = mpsc::channel::<Result<(), MicError>>();
        let (stop_sender, stop_receiver) = mpsc::channel::<()>();

        // cpal streams are not Send, so the whole lifetime of the stream
        // lives on this one thread.
        let thread = std::thread::Builder::new()
            .name("clicky-microphone".into())
            .spawn(move || {
                match open_input_stream(level, chunk_sender) {
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                    }
                    Ok((stream, state)) => {
                        let _ = ready_sender.send(Ok(()));
                        // Returns when asked to stop, or when MicCapture is dropped.
                        let _ = stop_receiver.recv();
                        drop(stream); // no more callbacks after this
                        if let Ok(mut state) = state.lock() {
                            state.flush();
                        }
                    }
                }
            })
            .map_err(|error| MicError::Open(error.to_string()))?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(MicCapture {
                stop_sender: Some(stop_sender),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(MicError::Open(
                "the audio thread exited unexpectedly".into(),
            )),
        }
    }

    /// Stops the stream and waits until the last chunk has been sent.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.shut_down();
    }
}

fn open_input_stream(
    level: AudioLevel,
    chunk_sender: UnboundedSender<Vec<u8>>,
) -> Result<(cpal::Stream, Arc<Mutex<CaptureState>>), MicError> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or(MicError::NoDevice)?;
    let supported_config = device
        .default_input_config()
        .map_err(|error| MicError::Open(error.to_string()))?;

    let sample_format = supported_config.sample_format();
    let stream_config = supported_config.config();
    tracing::info!(
        "microphone: {} Hz, {} channel(s), {:?}",
        stream_config.sample_rate,
        stream_config.channels,
        sample_format
    );

    let state = Arc::new(Mutex::new(CaptureState {
        channels: usize::from(stream_config.channels),
        resampler: Resampler::new(stream_config.sample_rate, TARGET_SAMPLE_RATE),
        level,
        chunk_sender,
        pending_samples: Vec::with_capacity(CHUNK_SAMPLES * 2),
        mono_scratch: Vec::new(),
        resampled_scratch: Vec::new(),
    }));

    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, stream_config, &state),
        SampleFormat::I16 => build_stream::<i16>(&device, stream_config, &state),
        SampleFormat::U16 => build_stream::<u16>(&device, stream_config, &state),
        SampleFormat::I32 => build_stream::<i32>(&device, stream_config, &state),
        SampleFormat::U8 => build_stream::<u8>(&device, stream_config, &state),
        SampleFormat::I8 => build_stream::<i8>(&device, stream_config, &state),
        other => Err(MicError::Open(format!(
            "unsupported sample format {other:?}"
        ))),
    }?;

    stream
        .play()
        .map_err(|error| MicError::Open(error.to_string()))?;
    Ok((stream, state))
}

fn build_stream<T>(
    device: &cpal::Device,
    stream_config: cpal::StreamConfig,
    state: &Arc<Mutex<CaptureState>>,
) -> Result<cpal::Stream, MicError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let callback_state = Arc::clone(state);
    let mut converted_block: Vec<f32> = Vec::new();
    device
        .build_input_stream(
            stream_config,
            move |block: &[T], _: &cpal::InputCallbackInfo| {
                converted_block.clear();
                converted_block.extend(block.iter().map(|sample| f32::from_sample(*sample)));
                if let Ok(mut state) = callback_state.lock() {
                    state.handle_block(&converted_block);
                }
            },
            |error| tracing::warn!("microphone stream error: {error}"),
            None,
        )
        .map_err(|error| MicError::Open(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f32, sample_rate: u32, seconds: f32) -> Vec<f32> {
        (0..(sample_rate as f32 * seconds) as usize)
            .map(|index| {
                (2.0 * std::f32::consts::PI * frequency * index as f32 / sample_rate as f32).sin()
                    * 0.5
            })
            .collect()
    }

    fn zero_crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
            .count()
    }

    #[test]
    fn resampling_48k_to_16k_keeps_duration_and_pitch() {
        let input = sine(440.0, 48_000, 1.0);
        let mut resampler = Resampler::new(48_000, 16_000);
        let mut output = Vec::new();
        resampler.process(&input, &mut output);

        assert!(
            (output.len() as i64 - 16_000).abs() <= 2,
            "got {} samples",
            output.len()
        );
        // 440 Hz for one second is ~440 rising zero crossings at any sample rate.
        assert!((zero_crossings(&output) as i64 - 440).abs() <= 2);
    }

    #[test]
    fn resampling_in_small_blocks_matches_one_big_block() {
        let input = sine(300.0, 44_100, 0.5);
        let mut whole = Vec::new();
        Resampler::new(44_100, 16_000).process(&input, &mut whole);

        let mut pieces = Vec::new();
        let mut resampler = Resampler::new(44_100, 16_000);
        // Odd block sizes on purpose, like real audio callbacks.
        for block in input.chunks(441) {
            resampler.process(block, &mut pieces);
        }
        // An empty block (a callback with no frames) must be harmless.
        resampler.process(&[], &mut pieces);

        assert!((whole.len() as i64 - pieces.len() as i64).abs() <= 1);
        for (whole_sample, piecewise_sample) in whole.iter().zip(&pieces) {
            assert!(
                (whole_sample - piecewise_sample).abs() < 1e-3,
                "{whole_sample} vs {piecewise_sample}"
            );
        }
    }

    #[test]
    fn upsampling_also_works() {
        let mut output = Vec::new();
        Resampler::new(8_000, 16_000).process(&sine(200.0, 8_000, 0.5), &mut output);
        assert!((output.len() as i64 - 8_000).abs() <= 2);
    }

    #[test]
    fn stereo_is_averaged_to_mono() {
        let mut mono = Vec::new();
        downmix_to_mono(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2, &mut mono);
        assert_eq!(mono, vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn pcm16_conversion_clamps_and_is_little_endian() {
        let bytes = f32_to_pcm16_le(&[0.0, 1.0, -1.0, 2.0, -2.0]);
        let values: Vec<i16> = bytes
            .chunks(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(values, vec![0, i16::MAX, -i16::MAX, i16::MAX, -i16::MAX]);
    }

    #[test]
    fn wav_header_is_well_formed() {
        let pcm = f32_to_pcm16_le(&sine(440.0, 16_000, 0.1));
        let wav = encode_wav_pcm16_mono(&pcm, 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize,
            wav.len() - 8
        );
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize,
            pcm.len()
        );
        assert_eq!(wav.len(), 44 + pcm.len());
    }

    #[test]
    fn loudness_and_duration_helpers() {
        assert_eq!(rms_level(&[]), 0.0);
        assert!((rms_level(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
        assert_eq!(pcm16_duration_ms(32_000), 1_000);
        assert_eq!(pcm16_duration_ms(3_200), 100);
    }

    #[test]
    fn capture_state_chunks_and_flushes_the_tail() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut state = CaptureState {
            channels: 1,
            resampler: Resampler::new(16_000, 16_000),
            level: AudioLevel::default(),
            chunk_sender: sender,
            pending_samples: Vec::new(),
            mono_scratch: Vec::new(),
            resampled_scratch: Vec::new(),
        };
        // 250 ms of input -> two full 100 ms chunks and a 50 ms tail.
        state.handle_block(&sine(440.0, 16_000, 0.25));
        let first = receiver.try_recv().unwrap();
        let second = receiver.try_recv().unwrap();
        assert_eq!(first.len(), CHUNK_SAMPLES * 2);
        assert_eq!(second.len(), CHUNK_SAMPLES * 2);
        assert!(receiver.try_recv().is_err());
        assert!(state.level.get() > 0.0);

        state.flush();
        let tail = receiver.try_recv().unwrap();
        assert!(tail.len() >= MINIMUM_FINAL_CHUNK_SAMPLES * 2);
        assert_eq!(state.level.get(), 0.0);
    }
}
