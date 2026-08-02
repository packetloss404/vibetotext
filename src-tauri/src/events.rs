//! Rust → frontend event names + emit helpers.
//!
//! The frontend (`renderer.js` → `api.js`) listens for these events instead of
//! the old Electron chokidar file-watch + 5s poll (migration plan §5). Phase 1
//! only needs `history-updated`; the recording / waveform / pipeline / permission
//! events land in later phases and are listed here as named constants now so all
//! emit sites share one source of truth.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};

/// Emitted (no payload) after every successful write to the history database, so
/// the dashboard refreshes immediately. Replaces the chokidar watch + poll.
pub const HISTORY_UPDATED: &str = "history-updated";

/// Recording-state change `{ recording: bool, mode: string }` (Phase 3+).
pub const RECORDING_STATE: &str = "recording-state";

/// 25 normalized waveform bar levels `[f32; 25]` (Phase 2+).
pub const WAVEFORM_LEVELS: &str = "waveform-levels";

/// Pipeline progress `{ phase: string, mode: string }` (Phase 4+).
pub const PIPELINE_STATUS: &str = "pipeline-status";

/// A platform permission is required `{ kind: string }` (Phase 3, macOS).
pub const PERMISSION_NEEDED: &str = "permission-needed";

/// Emit [`HISTORY_UPDATED`] to all webviews. No payload — the frontend re-pulls
/// `get_entries` / `get_statistics` on receipt.
///
/// Errors from `emit` are logged rather than propagated: a failed UI nudge must
/// never fail the database write that triggered it.
pub fn emit_history_updated<R: Runtime>(app: &AppHandle<R>) {
    if let Err(e) = app.emit(HISTORY_UPDATED, ()) {
        tracing::warn!(error = %e, "failed to emit history-updated event");
    }
}

/// Payload for [`RECORDING_STATE`]: the overlay + dashboard switch their visual
/// state on this. `mode` is the active capture mode (`transcribe`/`cleanup`/
/// `plan`/`greppy`) — empty when stopping is also acceptable for the frontend.
#[derive(Clone, Serialize)]
struct RecordingStatePayload<'a> {
    recording: bool,
    mode: &'a str,
}

/// Emit [`RECORDING_STATE`] `{ recording, mode }` to all webviews (Phase 3+).
///
/// Called by the pipeline on record start/stop so the overlay can show/hide and
/// the dashboard can reflect that a capture is in flight. Emit failures are
/// logged, never propagated — a UI nudge must not fail the pipeline.
pub fn emit_recording_state<R: Runtime>(app: &AppHandle<R>, recording: bool, mode: &str) {
    let payload = RecordingStatePayload { recording, mode };
    if let Err(e) = app.emit(RECORDING_STATE, payload) {
        tracing::warn!(error = %e, "failed to emit recording-state event");
    }
}

/// Emit [`WAVEFORM_LEVELS`] — the 25 normalized bar heights (`0.0..=1.0`) the
/// overlay animates (Phase 2+). Driven by the recorder/FFT at the UI frame rate,
/// so failures are logged and dropped rather than propagated.
pub fn emit_waveform_levels<R: Runtime>(app: &AppHandle<R>, levels: [f32; 25]) {
    if let Err(e) = app.emit(WAVEFORM_LEVELS, levels) {
        tracing::warn!(error = %e, "failed to emit waveform-levels event");
    }
}

/// Payload for [`PIPELINE_STATUS`]: coarse progress of the active capture so the
/// UI can surface "transcribing…", "cleaning up…", etc. `phase` is a free-form
/// stage label set by the pipeline; `mode` is the active capture mode.
#[derive(Clone, Serialize)]
struct PipelineStatusPayload<'a> {
    phase: &'a str,
    mode: &'a str,
}

/// Emit [`PIPELINE_STATUS`] `{ phase, mode }` to all webviews (Phase 4+).
///
/// The pipeline calls this as it moves through stages (e.g. `recording` →
/// `transcribing` → `cleaning` → `pasting` → `done`). Emit failures are logged,
/// never propagated.
pub fn emit_pipeline_status<R: Runtime>(app: &AppHandle<R>, phase: &str, mode: &str) {
    let payload = PipelineStatusPayload { phase, mode };
    if let Err(e) = app.emit(PIPELINE_STATUS, payload) {
        tracing::warn!(error = %e, "failed to emit pipeline-status event");
    }
}

#[derive(Clone, Serialize)]
struct PermissionNeededPayload<'a> {
    kind: &'a str,
}

pub fn emit_permission_needed<R: Runtime>(app: &AppHandle<R>, kind: &str) {
    if let Err(e) = app.emit(PERMISSION_NEEDED, PermissionNeededPayload { kind }) {
        tracing::warn!(error = %e, "failed to emit permission-needed event");
    }
}
