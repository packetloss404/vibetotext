//! VibeToText application library.
//!
//! `main.rs` is a thin shim that calls [`run`]. Keeping the app logic in a lib
//! crate is the Tauri v2 convention and lets later phases reuse it from tests
//! and (eventually) mobile targets.
//!
//! Module ownership (Phase 0): this scaffold OWNS `state`. The `db`, `config`,
//! and `sentiment` modules are declared here but their files are authored by
//! sibling builders in this same phase — a full build only links once those
//! land (the Verify agent runs the authoritative build).

// Foundational modules (Phase 0).
pub mod config;
pub mod db;
pub mod sentiment;
pub mod state;

// Phase 2 modules (audio capture + transcription + model resolution). `audio`
// is wired here by the Phase-2 devices/wiring agent; `transcribe` and `models`
// bodies are authored by sibling builders — declared so the crate links once
// they land (the Verify agent runs the authoritative build).
mod audio;
mod models;
mod transcribe;

// Phase 3 modules (global hotkey chord listener + auto-paste + overlay window).
// This scaffold OWNS the declarations and the overlay window creation only; the
// hotkey/paste/overlay bodies are authored by sibling Phase-3 builders. They are
// intentionally unused until the Phase 4 pipeline wires the record/transcribe/
// paste flow — `unused` warnings here are expected. The overlay window itself is
// created at startup so the live waveform has a target as soon as Phase 4 lands.
mod hotkey;
#[cfg(desktop)]
mod overlay;
mod paste;

// Phase 4 modules (pipeline orchestrator + Gemini LLM client + greppy wrapper).
// This wiring agent OWNS only the declarations and the `pipeline::start` call in
// setup; the module *bodies* are authored by sibling Phase-4 builders. Declaring
// them here makes the previously-unused Phase 2/3 code (hotkey/paste/overlay/
// recorder/transcribe/models) live, so their `unused` warnings clear once the
// pipeline links.
mod greppy;
mod llm;
mod pipeline;

// Phase 5: feature-gated localhost HTTP transcription endpoint. Compiled in
// ONLY when the `local-api` cargo feature is enabled (off by default), so a
// default build contains none of this code or its `tiny_http` dependency.
#[cfg(feature = "local-api")]
mod local_api;

// Phase 1 integration surface.
pub mod commands;
pub mod events;
#[cfg(desktop)]
pub mod tray;

use state::AppState;
use tauri::Manager;

/// Initialize structured logging. Honors `RUST_LOG`; defaults to `info` for the
/// app and `warn` for noisy dependencies. Safe to call once at startup.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,vibetotext_lib=info,tao=warn,wry=warn"));

    let mut file_guard = None;
    let file_layer = dirs::home_dir().and_then(|home| {
        let log_dir = home.join(".vibetotext").join("logs");
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            eprintln!(
                "VibeToText could not create log directory {}: {e}",
                log_dir.display()
            );
            return None;
        }
        let appender = tracing_appender::rolling::daily(log_dir, "vibetotext.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        file_guard = Some(guard);
        Some(fmt::layer().with_ansi(false).with_writer(writer))
    });

    // `try_init` so repeated calls (e.g. in tests) don't panic. The file layer
    // remains available in GUI release builds where Windows hides the console.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(file_layer)
        .try_init();

    file_guard
}

/// Build, configure, and run the Tauri application.
///
/// Wires up:
/// - structured logging (`tracing`)
/// - `tauri-plugin-single-instance` (focus the existing window on relaunch)
/// - `tauri-plugin-window-state` (persist window geometry)
/// - the managed [`AppState`]
///
/// Tray scaffolding, commands, and per-OS hotkey/audio/paste wiring arrive in
/// later phases.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _log_guard = init_tracing();

    tauri::Builder::default()
        // Single-instance MUST be registered first per the plugin docs so its
        // argv/cwd handler can fire before the main window is created. When a
        // second instance launches, raise and focus the existing main window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        // Persist + restore window size/position across launches.
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(|app| {
            // Construct shared state and hand it to Tauri so command handlers
            // can resolve it with `State<AppState>`.
            let state = AppState::new()?;
            app.manage(state);

            match config::AppConfig::migrate_legacy_gemini_key() {
                Ok(true) => tracing::info!("migrated legacy Gemini key to private .env storage"),
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "could not migrate legacy Gemini key"),
            }

            // System tray (desktop only): app icon + Show/Quit menu.
            #[cfg(desktop)]
            tray::setup(app.handle())?;

            // Create the hidden, transparent, click-through waveform overlay
            // window up front (desktop only) so the live recording indicator has
            // a target the instant the Phase-4 pipeline starts emitting levels.
            // It stays hidden until the pipeline shows it on record start.
            #[cfg(desktop)]
            overlay::ensure(app.handle())?;

            // Start the capture pipeline (hotkey listener + record/transcribe/
            // paste orchestration). MUST run after `overlay::ensure` so the live
            // waveform has a target window the instant a recording begins. A
            // pipeline start failure (e.g. missing OS permission, no audio
            // device) is logged but MUST NOT crash app startup — the dashboard
            // and history still work without live capture.
            #[cfg(desktop)]
            if let Err(e) = pipeline::start(app.handle()) {
                tracing::error!(error = %e, "failed to start capture pipeline; \
                    dictation/hotkeys disabled (history + dashboard still work)");
            }

            // Feature-gated localhost HTTP transcription endpoint (Phase 5).
            // Bound to 127.0.0.1 loopback only. A start failure is logged but
            // MUST NOT crash app startup — the endpoint is an optional extra.
            #[cfg(feature = "local-api")]
            if let Err(e) = local_api::start(app.handle()) {
                tracing::error!(error = %e, "failed to start local transcription API");
            }

            tracing::info!("VibeToText backend initialized");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_entries,
            commands::get_statistics,
            commands::get_pipeline_status,
            commands::clear_history,
            commands::load_config,
            commands::list_audio_devices,
            commands::set_audio_device,
            commands::get_dictionary,
            commands::add_word,
            commands::remove_word,
            commands::set_whisper_model,
            commands::set_codebase_path,
            commands::get_codebase_path_status,
            commands::get_gemini_key_status,
            commands::set_gemini_api_key,
            commands::set_orb_position,
        ])
        .run(tauri::generate_context!())
        .expect("error while running VibeToText application");
}
