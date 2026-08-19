//! Local Whisper transcription via `whisper-rs` (CPU-only MVP).
//!
//! Port of `src/vibetotext/transcriber.py` (`Transcriber`) and the C#
//! `WhisperTranscriber`. Uses `whisper-rs` 0.16 against bundled whisper.cpp,
//! compiled CPU-only — no `metal` / `cuda` / `vulkan` cargo features and
//! `WhisperContextParameters::use_gpu` left at its CPU default (see plan §2 GPU
//! decision and the working reference at `app/spikes/whisper_spike/src/main.rs`).
//!
//! The whisper C backend is **not reentrant**, so the loaded context lives
//! behind a `Mutex`; only one transcription runs at a time (mirroring the
//! Python `threading.Lock`). The context is loaded lazily on first use.

pub mod artifacts;
pub mod prompt;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Local Whisper transcriber holding a lazily-loaded, non-reentrant context.
pub struct Transcriber {
    /// Path to the ggml `.bin` model the context is (or will be) loaded from.
    model_path: PathBuf,
    /// The loaded whisper context, guarded because whisper.cpp is not reentrant.
    /// `None` until the first `transcribe` call loads it (lazy init).
    ctx: Mutex<Option<WhisperContext>>,
}

impl Transcriber {
    /// Create a transcriber for the given ggml model path.
    ///
    /// The model file is **not** loaded here — loading is deferred to the first
    /// [`transcribe`](Self::transcribe) call (matching the Python lazy `model`
    /// property). `new` only validates that the path exists so callers get an
    /// early, clear error instead of a deferred one.
    pub fn new(model_path: &Path) -> Result<Transcriber> {
        if !model_path.is_file() {
            anyhow::bail!("whisper model not found: {}", model_path.display());
        }
        Ok(Transcriber {
            model_path: model_path.to_path_buf(),
            ctx: Mutex::new(None),
        })
    }

    /// Force the (otherwise lazy) `WhisperContext` load now, so the first
    /// `transcribe()` doesn't pay the multi-second model-load cost mid-utterance.
    /// Used by the pipeline's startup prewarm. Idempotent.
    pub fn warm(&self) -> Result<()> {
        let mut guard = self
            .ctx
            .lock()
            .map_err(|_| anyhow::anyhow!("whisper context mutex poisoned"))?;
        self.ensure_loaded(&mut guard)?;
        Ok(())
    }

    /// Transcribe 16 kHz mono f32 samples to text.
    ///
    /// Sets `initial_prompt = build_prompt(custom_words)` (TECH_PROMPT plus the
    /// optional custom dictionary), runs CPU inference, joins the segments, and
    /// filters Whisper artifacts. Returns an empty string for empty input,
    /// matching `Transcriber.transcribe`.
    ///
    /// Whisper is not reentrant: the context mutex serializes calls, so a second
    /// concurrent transcription waits rather than corrupting state.
    pub fn transcribe(&self, samples: &[f32], custom_words: &[String]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }

        // Hold the lock for the whole inference: the whisper state derived from
        // the context must not be used from two threads at once.
        let mut guard = self
            .ctx
            .lock()
            .map_err(|_| anyhow::anyhow!("whisper context mutex poisoned"))?;
        let ctx = self.ensure_loaded(&mut guard)?;

        let mut state = ctx
            .create_state()
            .context("failed to create whisper state")?;

        let initial_prompt = prompt::build_prompt(custom_words);

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(num_cpu_threads());
        params.set_translate(false);
        params.set_language(Some("en"));
        params.set_initial_prompt(&initial_prompt);
        // Keep the C backend quiet — we collect text via the segment API.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        state
            .full(params, samples)
            .context("whisper inference (state.full) failed")?;

        let n_segments = state.full_n_segments();
        let mut joined = String::new();
        for i in 0..n_segments {
            if let Some(segment) = state.get_segment(i) {
                let text = segment
                    .to_str_lossy()
                    .context("failed to decode whisper segment text")?;
                if !joined.is_empty() {
                    joined.push(' ');
                }
                joined.push_str(text.trim());
            }
        }

        Ok(artifacts::filter(&joined))
    }

    /// Load the `WhisperContext` if it hasn't been loaded yet, and return a
    /// borrow of the loaded context. Caller must already hold the `ctx` mutex
    /// guard so the borrow is race-free. The `WhisperContext` is reentrant-safe
    /// to load but not to use from multiple threads, which is why the mutex
    /// guards the whole call.
    fn ensure_loaded<'g>(
        &self,
        guard: &'g mut Option<WhisperContext>,
    ) -> Result<&'g WhisperContext> {
        if guard.is_none() {
            let ctx = WhisperContext::new_with_params(
                &self.model_path,
                WhisperContextParameters::default(),
            )
            .with_context(|| {
                format!(
                    "failed to load whisper model: {}",
                    self.model_path.display()
                )
            })?;
            *guard = Some(ctx);
        }
        Ok(guard.as_ref().expect("just loaded above"))
    }
}

/// Number of CPU threads to use for inference (at least 1), matching the spike
/// and the C# `Math.Max(1, Environment.ProcessorCount)`.
fn num_cpu_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(1)
        .max(1)
}
