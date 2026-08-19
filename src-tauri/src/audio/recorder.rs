//! Microphone capture via `cpal`, ported from `src/vibetotext/recorder.py`.
//!
//! Mirrors the Python `AudioRecorder`: it captures **16 kHz mono f32** audio into
//! a growing buffer and periodically invokes an `on_level` callback with a 25-bar
//! waveform (computed by the sibling [`crate::audio::waveform::compute_bars`]).
//!
//! ## Sample-rate handling (Phase-2 carry-forward, plan §3 / risk #5)
//! The reference records at exactly `samplerate=16000, channels=1, dtype=float32`.
//! Some devices cannot honor 16 kHz. Rather than silently dropping such a device,
//! we open it at its **native** sample rate / channel count and resample to
//! 16 kHz mono ourselves (linear interpolation — adequate for speech → whisper).
//! Multi-channel input is downmixed to mono by averaging channels.
//!
//! ## Threading
//! cpal delivers audio on its own real-time callback thread. That thread pushes
//! resampled mono f32 into a shared `Arc<Mutex<Vec<f32>>>` (the full 16 kHz
//! buffer) and, when enough fresh samples have accumulated, computes the 25-bar
//! waveform and calls the user `on_level` callback. As in the Python callback,
//! the audio thread does no blocking I/O.
//!
//! ## RAII guard
//! `Recorder::start` returns a [`RecordingGuard`] that owns the live
//! `cpal::Stream` and the buffer. Dropping the guard stops the stream
//! (joining the callback thread) and discards any unclaimed audio. A panic
//! in the pipeline mid-recording therefore releases the OS audio handle
//! instead of leaking it. The normal path is
//! `guard.into_buffer()` which consumes the guard and returns the captured
//! 16 kHz mono f32 samples.
//!
//! ## Device index semantics (risk #5)
//! `start(device_index, …)` selects by **host input-device index** — the position
//! in `cpal::Host::input_devices()` — matching `list_audio_devices` enumeration.
//! `None` (or an out-of-range index) falls back to the system default input.
//!
//! Verified target: Windows (WASAPI). macOS (CoreAudio) / Linux (ALSA) are
//! written-but-unverified in this environment and guarded only by cpal's own
//! cross-platform abstraction.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

use crate::audio::waveform;

/// Target capture rate — matches `recorder.py` (`sample_rate=16000`).
const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Number of waveform bars — matches `recorder.py`'s `NUM_BARS = 25` and the
/// Phase-2 callback contract `Fn([f32; 25])`. Defined locally so this module does
/// not depend on the sibling waveform module re-exporting the constant; the value
/// must match `waveform::compute_bars`' return-array length.
const NUM_BARS: usize = 25;

/// Window of samples (at 16 kHz) handed to the waveform analyzer per update.
/// Matches `recorder.py`'s `FFT_SIZE = 512` so band-mapping/smoothing line up.
const WAVEFORM_WINDOW: usize = 512;

/// Owns the cpal `Stream` + the shared audio buffer for one recording session.
///
/// Returned by [`Recorder::start`]. The pipeline stores the guard in its state
/// while recording; on a clean stop it calls [`into_buffer`](Self::into_buffer)
/// to consume the guard and retrieve the audio. If the guard is dropped without
/// `into_buffer` (e.g. on a panic), the `Drop` impl stops the cpal stream
/// (joining the callback thread) and logs a warning so we don't lose track of
/// a forgotten recording session.
pub struct RecordingGuard {
    /// Live capture session; present until `into_buffer` consumes the guard.
    session: Option<Session>,
}

/// One active recording session: the cpal stream plus the shared buffer it fills.
struct Session {
    /// The cpal input stream. Dropping it stops capture and joins the callback.
    /// `Stream` is `!Send` on some backends, so the whole `RecordingGuard` is
    /// used from one thread; the *data* it produces is shared via `Arc<Mutex<…>>`.
    stream: Stream,
    /// Full captured audio, already resampled to 16 kHz mono f32.
    buffer: Arc<Mutex<Vec<f32>>>,
}

impl RecordingGuard {
    /// Stop the stream and return the captured 16 kHz mono f32 samples.
    ///
    /// Consumes the guard; calling it again is a type error. The cpal stream
    /// is dropped (joining the callback thread) so no callback can mutate the
    /// buffer after this point.
    pub fn into_buffer(mut self) -> Result<Vec<f32>> {
        let session = match self.session.take() {
            Some(s) => s,
            None => {
                // into_buffer can only be called once because the Option is
                // taken; the only path here is "impossible" (called after a
                // previous into_buffer), but stay loud rather than panic.
                anyhow::bail!("recording guard already consumed");
            }
        };

        // Dropping the stream stops capture and joins the callback thread, so no
        // callback can be mutating the buffer after this point.
        drop(session.stream);

        let audio = match Arc::try_unwrap(session.buffer) {
            Ok(mutex) => mutex
                .into_inner()
                .map_err(|_| anyhow!("audio buffer mutex poisoned"))?,
            // Should not happen (stream dropped → sole owner), but stay safe.
            Err(arc) => arc
                .lock()
                .map_err(|_| anyhow!("audio buffer mutex poisoned"))?
                .clone(),
        };

        let duration = audio.len() as f32 / TARGET_SAMPLE_RATE as f32;
        tracing::info!(
            samples = audio.len(),
            duration_s = duration,
            "stopped audio capture"
        );

        Ok(audio)
    }
}

impl Drop for RecordingGuard {
    fn drop(&mut self) {
        if self.session.is_some() {
            // The guard went out of scope without `into_buffer` being called.
            // That means the pipeline abandoned a recording (panic, early
            // return, etc.). Drop the stream now (joining the callback) and
            // warn so we notice if this becomes a regular pattern.
            tracing::warn!(
                "RecordingGuard dropped without into_buffer(); \
                 cpal stream released via Drop"
            );
            // Explicitly take to ensure the drop order is deterministic.
            self.session.take();
        }
    }
}

/// Microphone recorder. Stateless; the live `cpal::Stream` lives in a
/// [`RecordingGuard`] returned from [`start`](Recorder::start). The recorder
/// itself can be re-used to start successive sessions.
pub struct Recorder {
    _private: (),
}

impl Recorder {
    /// Create a new recorder. Stateless; safe to construct ad-hoc.
    pub fn new() -> Recorder {
        Recorder { _private: () }
    }

    /// Begin capturing from the selected input device and return a guard
    /// owning the live `cpal::Stream` and audio buffer.
    ///
    /// * `device_index` — host input-device index (see module docs). `None` or an
    ///   out-of-range value selects the system default input.
    /// * `on_level` — invoked periodically (roughly every [`WAVEFORM_WINDOW`]
    ///   16 kHz samples) with a freshly computed 25-bar waveform, suitable for
    ///   emitting the `waveform-levels[25]` event.
    ///
    /// The captured audio is resampled to 16 kHz mono f32 and accumulated; call
    /// [`RecordingGuard::into_buffer`] to consume the guard and retrieve the
    /// audio. Dropping the guard without `into_buffer` (e.g. on a panic)
    /// releases the cpal stream; a `tracing::warn!` is emitted so we notice.
    pub fn start<F>(
        &self,
        device_index: Option<i64>,
        device_name: Option<&str>,
        on_level: F,
    ) -> Result<RecordingGuard>
    where
        F: Fn([f32; NUM_BARS]) + Send + 'static,
    {
        let host = cpal::default_host();
        let reconciled_index =
            crate::audio::devices::resolve_input_device_index(device_index, device_name);
        let device = select_device(&host, reconciled_index)?;
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());

        // Pick an input config. Prefer the device default (its native rate);
        // we resample to 16 kHz ourselves rather than forcing the hardware.
        let default_cfg = device
            .default_input_config()
            .context("querying default input config")?;
        let sample_format = default_cfg.sample_format();
        let native_rate: u32 = default_cfg.sample_rate();
        let native_channels: u16 = default_cfg.channels();

        let stream_config = StreamConfig {
            channels: native_channels,
            sample_rate: native_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        tracing::info!(
            device = %device_name,
            native_rate,
            native_channels,
            ?sample_format,
            target_rate = TARGET_SAMPLE_RATE,
            "starting audio capture"
        );

        let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));

        // Per-stream resampling/analysis state, owned by the audio callback.
        let cb_state = CallbackState {
            buffer: Arc::clone(&buffer),
            on_level: Box::new(on_level),
            native_rate,
            native_channels,
            resample_pos: 0.0,
            resample_carry: None,
            pending_analysis: Vec::with_capacity(WAVEFORM_WINDOW * 2),
            analyzer: waveform::WaveformAnalyzer::new(),
        };

        let stream = build_stream(&device, &stream_config, sample_format, cb_state)?;
        stream.play().context("starting cpal stream")?;

        Ok(RecordingGuard {
            session: Some(Session { stream, buffer }),
        })
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder::new()
    }
}

/// Resolve a device from an optional host input-device index.
fn select_device(host: &cpal::Host, device_index: Option<i64>) -> Result<cpal::Device> {
    if let Some(idx) = device_index {
        if idx >= 0 {
            if let Ok(mut devices) = host.input_devices() {
                if let Some(dev) = devices.nth(idx as usize) {
                    return Ok(dev);
                }
                tracing::warn!(idx, "audio device index out of range; using default input");
            }
        } else {
            tracing::warn!(idx, "negative audio device index; using default input");
        }
    }

    host.default_input_device()
        .ok_or_else(|| anyhow!("no input audio device available"))
}

/// Mutable state owned by the cpal data callback. Not `Sync` is fine — cpal calls
/// the closure from a single dedicated thread.
struct CallbackState {
    /// Full 16 kHz mono buffer (shared with the `Recorder`).
    buffer: Arc<Mutex<Vec<f32>>>,
    /// User waveform callback.
    on_level: Box<dyn Fn([f32; NUM_BARS]) + Send + 'static>,
    /// Native input sample rate (Hz).
    native_rate: u32,
    /// Native input channel count.
    native_channels: u16,
    /// Fractional read position into the native-rate mono stream for linear
    /// resampling; carries across callbacks.
    resample_pos: f64,
    /// Last native-rate mono sample from the previous callback, so interpolation
    /// can bridge the callback boundary.
    resample_carry: Option<f32>,
    /// Freshly produced 16 kHz samples awaiting a waveform analysis window.
    pending_analysis: Vec<f32>,
    /// Stateful waveform analyzer (ports recorder.py's temporal smoothing
    /// `prev*0.7 + new*0.3` and the silent-decay `prev *= 0.7`).
    analyzer: waveform::WaveformAnalyzer,
}

impl CallbackState {
    /// Process one interleaved native-format block of f32 samples.
    fn process(&mut self, data: &[f32]) {
        // 1) Downmix interleaved channels to mono (average), like `flatten()` on
        //    a mono stream; for multi-channel we average to avoid amplitude bias.
        let ch = self.native_channels.max(1) as usize;
        let mono: Vec<f32> = if ch == 1 {
            data.to_vec()
        } else {
            data.chunks(ch)
                .map(|frame| frame.iter().copied().sum::<f32>() / ch as f32)
                .collect()
        };

        // 2) Resample native_rate -> 16 kHz with linear interpolation.
        let resampled = self.resample_to_target(&mono);
        if resampled.is_empty() {
            return;
        }

        // 3) Append to the full buffer.
        if let Ok(mut buf) = self.buffer.lock() {
            buf.extend_from_slice(&resampled);
        }

        // 4) Accumulate for waveform analysis; emit bars per full window.
        self.pending_analysis.extend_from_slice(&resampled);
        while self.pending_analysis.len() >= WAVEFORM_WINDOW {
            let window: Vec<f32> = self.pending_analysis.drain(..WAVEFORM_WINDOW).collect();
            let bars = self.analyzer.process(&window);
            (self.on_level)(bars);
        }
    }

    /// Linear-interpolation resampler from `native_rate` to [`TARGET_SAMPLE_RATE`].
    /// State (`resample_pos`, `resample_carry`) persists across calls so there are
    /// no clicks at callback boundaries.
    ///
    /// We treat the input as a virtual array `src = [carry?, mono...]`. `resample_pos`
    /// is the next fractional read position into `src`; after consuming this block we
    /// rebase it relative to the new carry (the block's last sample, virtual index 0
    /// of the next call).
    fn resample_to_target(&mut self, mono: &[f32]) -> Vec<f32> {
        if mono.is_empty() {
            return Vec::new();
        }

        // Fast path: already at the target rate — no interpolation needed.
        if self.native_rate == TARGET_SAMPLE_RATE {
            self.resample_carry = mono.last().copied();
            return mono.to_vec();
        }

        // Virtual source array. If we have a carry from the previous block, it is
        // index 0 and `mono` follows; otherwise `mono` starts at index 0.
        // `resample_pos` was left pointing into this virtual array on the last call.
        let carry = self.resample_carry;
        let offset = if carry.is_some() { 1 } else { 0 };
        let len = mono.len() + offset;

        let src = |i: usize| -> f32 {
            if let Some(c) = carry {
                if i == 0 {
                    c
                } else {
                    mono[i - 1]
                }
            } else {
                mono[i]
            }
        };

        let step = self.native_rate as f64 / TARGET_SAMPLE_RATE as f64;
        let mut out = Vec::with_capacity(((len as f64) / step).ceil() as usize + 1);

        let mut pos = self.resample_pos;
        // We can interpolate while `pos` has a right neighbor inside the array.
        while pos < (len - 1) as f64 {
            let i = pos.floor() as usize;
            let frac = (pos - i as f64) as f32;
            let a = src(i);
            let b = src(i + 1);
            out.push(a + (b - a) * frac);
            pos += step;
        }

        // Rebase `pos` relative to the next call's virtual array, whose index 0 is
        // this block's last sample (the new carry). The last real index of the
        // current array is `len - 1`, which becomes index 0 next time.
        self.resample_pos = pos - (len - 1) as f64;
        if self.resample_pos < 0.0 {
            self.resample_pos = 0.0;
        }
        self.resample_carry = mono.last().copied();

        out
    }
}

/// Build a cpal input stream for the device's native sample format, converting
/// every supported integer/float format to f32 before processing.
fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    mut state: CallbackState,
) -> Result<Stream> {
    let err_fn = |err: cpal::StreamError| {
        tracing::error!(error = %err, "audio stream error");
    };

    let stream = match format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| state.process(data),
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                let f: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                state.process(&f);
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| {
                let f: Vec<f32> = data
                    .iter()
                    .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                    .collect();
                state.process(&f);
            },
            err_fn,
            None,
        ),
        other => return Err(anyhow!("unsupported input sample format: {other:?}")),
    }
    .context("building cpal input stream")?;

    Ok(stream)
}
