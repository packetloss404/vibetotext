//! Rust → frontend event names + emit helpers.
//!
//! The frontend (`renderer.js` → `api.js`) listens for these events instead of
//! the old Electron chokidar file-watch + 5s poll (migration plan §5). Phase 1
//! only needs `history-updated`; the recording / waveform / pipeline / permission
//! events land in later phases and are listed here as named constants now so all
//! emit sites share one source of truth.

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
