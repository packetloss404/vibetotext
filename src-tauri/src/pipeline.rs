//! Pipeline orchestrator — the actor/queue model that turns global push-to-talk
//! [`HotkeyEvent`]s into recordings, transcriptions, mode transforms, history
//! writes, and an auto-paste at the cursor.
//!
//! Port of `src/vibetotext/cli.py` (`on_start` / `on_stop`) using the exact
//! deadlock-free architecture of `recorder.py`'s `HotkeyListener`: a queue-based
//! actor model.
//!
//! ## Architecture (deadlock-free by construction)
//!
//! ```text
//!   rdev listener thread  --(HotkeyEvent)-->  mpsc channel  -->  WORKER thread
//!        (never blocks)                                          (FIFO consumer)
//! ```
//!
//! * [`hotkey::spawn`] runs the rdev `listen()` loop on **its own** thread and
//!   only *sends* [`HotkeyEvent`]s into an `mpsc` channel — it never blocks on
//!   recording/transcription.
//! * A single dedicated **worker** thread owns the `cpal` [`Recorder`] (which is
//!   `!Send` — it must live on exactly one thread) and a lazily-created
//!   [`Transcriber`], and processes events in FIFO order. Because the worker
//!   handles one event at a time, a `Start` always fully completes before the
//!   matching `Stop` is processed — no races, no shared locks on state.
//!
//! State machine (owned exclusively by the worker thread, mirroring
//! `recorder.py`'s `idle -> recording -> processing -> idle`):
//!
//! ```text
//!   IDLE      --Start(mode)-->  RECORDING   (overlay shown, cpal capturing)
//!   RECORDING --Stop(mode)-->   PROCESSING  (transcribe + mode transform + paste)
//!   PROCESSING --(done)-->      IDLE
//! ```
//!
//! ## Robustness
//!
//! Every stage logs via `tracing`. A failure in any stage aborts **only the
//! current utterance** — the worker catches it, logs it, restores UI state
//! (hides the overlay, emits `recording-state{false}`), and keeps running so the
//! next push-to-talk still works (parity with `cli.py`'s per-callback
//! try/except that logs to `vibetotext_crash.log` and continues).

use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::audio::recorder::Recorder;
use crate::config::AppConfig;
use crate::db::Db;
use crate::hotkey::{self, HotkeyEvent, Mode};
use crate::transcribe::Transcriber;
use crate::{events, greppy, llm, models, overlay, paste};

/// Payload for the `recording-state` event (plan §5: `{recording, mode}`).
/// `Clone` is required by Tauri's `Emitter::emit<S: Serialize + Clone>`.
#[derive(Serialize, Clone)]
struct RecordingState {
    recording: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<Mode>,
}

/// Payload for the `pipeline-status` event (plan §5: `{phase, mode}`).
#[derive(Serialize, Clone)]
struct PipelineStatus<'a> {
    phase: &'a str,
    mode: Mode,
}

/// Emit `recording-state{recording, mode}`. Errors are logged, never propagated:
/// a failed UI nudge must not abort the recording flow.
fn emit_recording_state(app: &tauri::AppHandle, recording: bool, mode: Option<Mode>) {
    if let Err(e) = app.emit(events::RECORDING_STATE, RecordingState { recording, mode }) {
        tracing::warn!(error = %e, "failed to emit recording-state event");
    }
}

/// Emit `pipeline-status{phase, mode}` for the current processing stage.
fn emit_pipeline_status(app: &tauri::AppHandle, phase: &str, mode: Mode) {
    if let Err(e) = app.emit(events::PIPELINE_STATUS, PipelineStatus { phase, mode }) {
        tracing::warn!(error = %e, "failed to emit pipeline-status event");
    }
}

/// Greppy code-context snippet limit for plain Transcribe mode (matches
/// `cli.py`'s `--context-limit` default of 5).
const CONTEXT_LIMIT: usize = 5;

/// Greppy file-attach limit for Greppy mode (matches `cli.py`'s `--greppy-limit`
/// default of 10).
const GREPPY_LIMIT: usize = 10;

/// Start the pipeline orchestrator.
///
/// Spawns the worker thread (owner of the `!Send` recorder + lazy transcriber)
/// and the hotkey listener thread, wires the listener to *send* every
/// [`HotkeyEvent`] into the worker's channel, and returns immediately. Both
/// threads run for the process lifetime.
///
/// This is the Phase-4 contract entry point called from `lib.rs`'s `setup`.
pub fn start(app: &tauri::AppHandle) -> Result<()> {
    let (tx, rx) = mpsc::channel::<HotkeyEvent>();

    // Resolve the history DB path once, up front, from managed state (falls back
    // to ~/.vibetotext/history.db if state isn't available for some reason).
    let db_path = app
        .try_state::<crate::state::AppState>()
        .map(|s| s.db_path())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".vibetotext")
                .join("history.db")
        });

    // Worker thread: sole owner of the recorder + transcriber, FIFO consumer.
    let worker_app = app.clone();
    thread::Builder::new()
        .name("vibetotext-pipeline".into())
        .spawn(move || {
            let mut worker = Worker::new(worker_app, db_path);
            // Warm the model (download + load) BEFORE the event loop so the user's
            // first hotkey isn't gated on a multi-hundred-MB download + model-load
            // freeze mid-recording. Best-effort; lazy resolution still covers it.
            worker.prewarm();
            worker.run(rx);
        })
        .context("failed to spawn pipeline worker thread")?;

    // Hotkey listener thread: rdev listen() on its own thread; only SENDS into
    // the channel, never blocks on recording/transcription.
    hotkey::spawn(move |evt| {
        if let Err(e) = tx.send(evt) {
            // The only way this fails is the worker (receiver) having gone away,
            // which means the process is shutting down — nothing actionable.
            tracing::debug!(error = %e, "pipeline channel closed; dropping hotkey event");
        }
    });

    tracing::info!("pipeline orchestrator started");
    Ok(())
}

/// The worker actor: owns the `!Send` recorder, a lazily-loaded transcriber, the
/// state machine, and the per-utterance processing. Lives on exactly one thread.
struct Worker {
    app: tauri::AppHandle,
    db_path: std::path::PathBuf,
    recorder: Recorder,
    /// Lazily created on first transcription (mirrors `cli.py` preloading vs. our
    /// download-on-first-run model resolution). Keyed implicitly by the model the
    /// config requested; we recreate it if the requested model changes.
    transcriber: Option<Transcriber>,
    /// The model name the current `transcriber` was built for, so we can detect a
    /// hot-applied model change and rebuild.
    transcriber_model: Option<String>,
    /// `Some(mode)` while a recording is active (RECORDING state). `None` is IDLE.
    /// During PROCESSING this is `None` (the recording has been stopped).
    active: Option<Active>,
}

/// The currently-active recording session metadata.
struct Active {
    /// Mode the recording was started in. Stop is driven by the HotkeyEvent's
    /// own mode, so this is kept as session metadata / for debugging.
    #[allow(dead_code)]
    mode: Mode,
    /// The config snapshot captured at Start, so Stop processes with exactly the
    /// settings that were in effect when recording began (hot-apply per-utterance).
    config: AppConfig,
}

impl Worker {
    fn new(app: tauri::AppHandle, db_path: std::path::PathBuf) -> Self {
        Self {
            app,
            db_path,
            recorder: Recorder::new(),
            transcriber: None,
            transcriber_model: None,
            active: None,
        }
    }

    /// Main FIFO loop. Blocks on the channel; processes one event fully before
    /// the next. A panic-free per-event design: each handler returns a `Result`
    /// and we log + recover on error, keeping the worker alive.
    fn run(&mut self, rx: mpsc::Receiver<HotkeyEvent>) {
        tracing::info!("pipeline worker loop started");
        for evt in rx {
            match evt {
                HotkeyEvent::Start(mode) => {
                    if let Err(e) = self.handle_start(mode) {
                        tracing::error!(?mode, error = %format!("{e:#}"), "pipeline Start failed");
                        // Best-effort UI cleanup so a failed Start doesn't leave
                        // the overlay stuck on screen.
                        self.abort_recording_ui();
                        self.active = None;
                    }
                }
                HotkeyEvent::Stop(mode) => {
                    if let Err(e) = self.handle_stop(mode) {
                        tracing::error!(?mode, error = %format!("{e:#}"), "pipeline Stop failed (utterance aborted)");
                        // Failure aborts THIS utterance only: clean up UI, drop
                        // any partial recording, and keep the worker running.
                        let _ = self.recorder.stop();
                        self.abort_recording_ui();
                    }
                    // Whether Stop succeeded or failed, we are back to IDLE.
                    self.active = None;
                }
            }
        }
        tracing::info!("pipeline worker loop ended (channel closed)");
    }

    /// Handle `Start(mode)` — RECORDING entry (or instant History handling).
    ///
    /// * `History` -> show/focus the main window and return (no recording).
    /// * else -> reload config (hot-apply), show overlay, emit
    ///   `recording-state{recording:true,mode}`, and start the recorder with an
    ///   `on_level` callback that emits `waveform-levels`.
    fn handle_start(&mut self, mode: Mode) -> Result<()> {
        // Reload config at each Start so mic / model / paths hot-apply.
        let config = AppConfig::load().context("reloading config at recording start")?;

        if mode == Mode::History {
            tracing::info!("History hotkey: showing main window");
            self.show_main_window();
            return Ok(());
        }

        // Defensive: if a previous recording somehow wasn't stopped, drop it so
        // recorder.start() doesn't error on an already-active session.
        if self.recorder.is_recording() {
            tracing::warn!("recorder still active on Start; dropping stale recording");
            let _ = self.recorder.stop();
        }

        tracing::info!(?mode, "recording start");
        overlay::show(&self.app).context("showing overlay")?;
        emit_recording_state(&self.app, true, Some(mode));

        // The waveform callback runs on cpal's audio thread; it must not block.
        // It only forwards the 25 bars to the frontend overlay via an event.
        let app_for_levels = self.app.clone();
        self.recorder
            .start(config.audio_device_index, move |bars| {
                events::emit_waveform_levels(&app_for_levels, bars);
            })
            .context("starting audio recorder")?;

        self.active = Some(Active { mode, config });
        Ok(())
    }

    /// Handle `Stop(mode)` — RECORDING -> PROCESSING -> IDLE.
    ///
    /// Stops the recorder, hides the overlay, transcribes, applies the
    /// mode-specific transform, writes history, and pastes at the cursor. Empty
    /// audio / empty transcription short-circuit cleanly (parity with `on_stop`).
    fn handle_stop(&mut self, mode: Mode) -> Result<()> {
        // History never records, so its Stop is a no-op (parity with cli.py).
        if mode == Mode::History {
            return Ok(());
        }

        // Pull the config snapshot captured at Start; if we somehow have no
        // active session (e.g. Start failed), reload fresh so Stop is still safe.
        let config = match self.active.take() {
            Some(a) => a.config,
            None => {
                tracing::warn!(?mode, "Stop with no active recording; reloading config");
                AppConfig::load().unwrap_or_default()
            }
        };

        // PROCESSING: stop capture, tear down the recording UI.
        let samples = self.recorder.stop().context("stopping audio recorder")?;
        let duration_seconds = samples.len() as f64 / 16_000.0;
        overlay::hide(&self.app).context("hiding overlay")?;
        emit_recording_state(&self.app, false, None);

        if samples.is_empty() {
            tracing::info!("no audio captured; nothing to transcribe");
            return Ok(());
        }

        // --- Transcribe -----------------------------------------------------
        // The very first use downloads + loads the model; surface a distinct
        // phase so the UI doesn't look frozen. Startup prewarm usually makes this
        // instant (the model is already resolved by the time the user records).
        emit_pipeline_status(&self.app, "preparing_model", mode);
        // Build/resolve the transcriber, then drop the &mut borrow before emitting
        // (which needs &self.app) and re-borrow it immutably for the transcription.
        self.ensure_transcriber(&config.whisper_model)?;
        emit_pipeline_status(&self.app, "transcribing", mode);
        let transcriber = self
            .transcriber
            .as_ref()
            .expect("transcriber ensured above");
        // `transcribe` already strips whisper artifacts/noise markers, so `raw`
        // is the cleaned transcription.
        let raw = transcriber
            .transcribe(&samples, &config.custom_dictionary)
            .context("whisper transcription failed")?;

        if raw.trim().is_empty() {
            tracing::info!("no speech detected (empty transcription)");
            return Ok(());
        }
        tracing::info!(chars = raw.len(), ?mode, "transcribed utterance");

        // --- Mode branch -> final text --------------------------------------
        let final_text = self.apply_mode(mode, &raw, &config);

        // --- History write (with VADER sentiment, computed inside Db) --------
        emit_pipeline_status(&self.app, "saving", mode);
        if let Err(e) = self.save_history(&raw, mode, duration_seconds) {
            // A history-write failure must not block the paste — log and proceed
            // so the user still gets their text (parity with cli.py ordering,
            // which is best-effort about history).
            tracing::error!(error = %format!("{e:#}"), "failed to write history entry");
        } else {
            events::emit_history_updated(&self.app);
        }

        // --- Paste at cursor ------------------------------------------------
        emit_pipeline_status(&self.app, "pasting", mode);
        paste::paste_at_cursor(&final_text).context("pasting at cursor")?;

        emit_pipeline_status(&self.app, "done", mode);
        tracing::info!(?mode, "utterance complete");
        Ok(())
    }

    /// Apply the per-mode transform to the raw transcription, producing the final
    /// text to paste. Every fallible branch degrades to `raw` on error/missing
    /// config (parity with `cli.py`'s "failed, using original").
    fn apply_mode(&self, mode: Mode, raw: &str, config: &AppConfig) -> String {
        match mode {
            // Transcribe: raw, optionally with greppy code-context appended.
            Mode::Transcribe => {
                if let Some(codebase) = config.codebase_path.as_deref() {
                    emit_pipeline_status(&self.app, "searching_context", mode);
                    // greppy collapses missing-binary / error / no-hits into None.
                    match greppy::code_context(raw, std::path::Path::new(codebase), CONTEXT_LIMIT) {
                        Some(ctx) if !ctx.is_empty() => format!("{raw}{ctx}"),
                        _ => raw.to_string(),
                    }
                } else {
                    raw.to_string()
                }
            }

            // Cleanup: Gemini refine, fall back to raw on missing key / error.
            Mode::Cleanup => {
                emit_pipeline_status(&self.app, "cleaning_up", mode);
                match config.gemini_api_key() {
                    Some(key) => match llm::cleanup_text(raw, &key) {
                        Ok(text) if !text.trim().is_empty() => text,
                        Ok(_) => {
                            tracing::warn!("cleanup returned empty; using raw text");
                            raw.to_string()
                        }
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "cleanup failed; using raw text");
                            raw.to_string()
                        }
                    },
                    None => {
                        tracing::warn!("no Gemini API key; cleanup falling back to raw text");
                        raw.to_string()
                    }
                }
            }

            // Plan: Gemini implementation plan, fall back to raw.
            Mode::Plan => {
                emit_pipeline_status(&self.app, "generating_plan", mode);
                match config.gemini_api_key() {
                    Some(key) => match llm::generate_plan(raw, &key) {
                        Ok(text) if !text.trim().is_empty() => text,
                        Ok(_) => {
                            tracing::warn!("plan returned empty; using raw text");
                            raw.to_string()
                        }
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "plan generation failed; using raw text");
                            raw.to_string()
                        }
                    },
                    None => {
                        tracing::warn!("no Gemini API key; plan falling back to raw text");
                        raw.to_string()
                    }
                }
            }

            // Greppy: attach relevant files; fall back to raw if no codebase /
            // no greppy binary / error.
            Mode::Greppy => {
                emit_pipeline_status(&self.app, "searching_files", mode);
                match config.codebase_path.as_deref() {
                    Some(codebase) => {
                        match greppy::greppy_files(raw, std::path::Path::new(codebase), GREPPY_LIMIT) {
                            Some(ctx) if !ctx.is_empty() => format!("{raw}{ctx}"),
                            _ => {
                                tracing::info!(
                                    "greppy found no files / binary absent; using raw text"
                                );
                                raw.to_string()
                            }
                        }
                    }
                    None => {
                        tracing::info!(
                            "no codebase_path configured; greppy falling back to raw text"
                        );
                        raw.to_string()
                    }
                }
            }

            // History never reaches here (handled before recording).
            Mode::History => raw.to_string(),
        }
    }

    /// Lazily create (or rebuild on model change) the whisper transcriber,
    /// resolving/downloading the ggml model on first use.
    fn ensure_transcriber(&mut self, model: &str) -> Result<&Transcriber> {
        let needs_rebuild =
            self.transcriber.is_none() || self.transcriber_model.as_deref() != Some(model);

        if needs_rebuild {
            tracing::info!(model, "resolving whisper model (download-on-first-run)");
            let model_path = models::resolve_or_download(model)
                .with_context(|| format!("resolving whisper model '{model}'"))?;
            let transcriber = Transcriber::new(&model_path)
                .with_context(|| format!("creating transcriber for model '{model}'"))?;
            self.transcriber = Some(transcriber);
            self.transcriber_model = Some(model.to_string());
        }

        Ok(self
            .transcriber
            .as_ref()
            .expect("transcriber created above"))
    }

    /// Best-effort startup warm-up: resolve/download + load the configured model
    /// up front (on the worker thread, before the event loop) so the first real
    /// utterance doesn't pay the download + load cost mid-recording. Failures are
    /// non-fatal — lazy `ensure_transcriber` retries on first use.
    fn prewarm(&mut self) {
        let model = AppConfig::load()
            .map(|c| c.whisper_model)
            .unwrap_or_else(|_| "small".to_string());
        tracing::info!(model, "prewarming whisper model (background, pre-first-use)");
        match self.ensure_transcriber(&model) {
            Ok(_) => tracing::info!("whisper model ready"),
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "model prewarm failed; will retry on first use"
            ),
        }
    }

    /// Write a history entry (mode lowercased, with computed sentiment/wpm done
    /// inside `Db::add_entry`). Opens the DB per-write to avoid holding a
    /// connection on the worker for the process lifetime; the `Db` layer uses WAL
    /// + a 30s busy timeout so this is safe alongside the command-layer reads.
    fn save_history(&self, text: &str, mode: Mode, duration_seconds: f64) -> Result<()> {
        let db = Db::open(&self.db_path).context("opening history database")?;
        let mode_str = mode_str_lowercase(mode);
        let timestamp = iso_now();
        db.add_entry(text, mode_str, &timestamp, Some(duration_seconds))
            .context("inserting history entry")?;
        Ok(())
    }

    /// Show + focus the main window (History hotkey behavior).
    fn show_main_window(&self) {
        if let Some(window) = self.app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        } else {
            tracing::warn!("History hotkey: no 'main' window to show");
        }
    }

    /// Best-effort restore of recording UI after a failure: hide the overlay and
    /// signal `recording-state{false}`. Infallible (each step swallows errors).
    fn abort_recording_ui(&self) {
        let _ = overlay::hide(&self.app);
        emit_recording_state(&self.app, false, None);
    }
}

/// Lowercase mode string for the history `mode` column (parity with the Python
/// mode strings: `"transcribe" | "cleanup" | "plan" | "greppy"`).
fn mode_str_lowercase(mode: Mode) -> &'static str {
    match mode {
        Mode::Transcribe => "transcribe",
        Mode::Cleanup => "cleanup",
        Mode::Plan => "plan",
        Mode::Greppy => "greppy",
        Mode::History => "history",
    }
}

/// Current time as an ISO-8601 UTC timestamp `YYYY-MM-DDTHH:MM:SS`.
///
/// Dependency-free (no `chrono`/`time` direct dep): derived from `SystemTime`
/// via the civil-date algorithm. Stored as a lexically-sortable string so the
/// `idx_timestamp(timestamp DESC)` index orders entries newest-first
/// (parity with `history.py`'s `datetime.now().isoformat()`).
fn iso_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;

    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}")
}

/// Convert days since the Unix epoch (1970-01-01) to a `(year, month, day)`
/// civil date. Howard Hinnant's well-known `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_strings_are_lowercase() {
        assert_eq!(mode_str_lowercase(Mode::Transcribe), "transcribe");
        assert_eq!(mode_str_lowercase(Mode::Cleanup), "cleanup");
        assert_eq!(mode_str_lowercase(Mode::Plan), "plan");
        assert_eq!(mode_str_lowercase(Mode::Greppy), "greppy");
    }

    #[test]
    fn iso_now_has_expected_shape() {
        let s = iso_now();
        // YYYY-MM-DDTHH:MM:SS == 19 chars.
        assert_eq!(s.len(), 19, "got {s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }

    #[test]
    fn civil_from_days_known_dates() {
        // Day 0 is 1970-01-01.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01 is 11017 days after epoch.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 2026-06-02 is 20606 days after epoch.
        assert_eq!(civil_from_days(20_606), (2026, 6, 2));
    }
}
