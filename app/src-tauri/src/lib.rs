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
mod transcribe;
mod models;

// Phase 3 modules (global hotkey chord listener + auto-paste + overlay window).
// This scaffold OWNS the declarations and the overlay window creation only; the
// hotkey/paste/overlay bodies are authored by sibling Phase-3 builders. They are
// intentionally unused until the Phase 4 pipeline wires the record/transcribe/
// paste flow — `unused` warnings here are expected. The overlay window itself is
// created at startup so the live waveform has a target as soon as Phase 4 lands.
mod hotkey;
mod paste;
#[cfg(desktop)]
mod overlay;

// Phase 1 integration surface.
pub mod commands;
pub mod events;
#[cfg(desktop)]
pub mod tray;

use state::AppState;
use tauri::Manager;

/// Initialize structured logging. Honors `RUST_LOG`; defaults to `info` for the
/// app and `warn` for noisy dependencies. Safe to call once at startup.
fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,vibetotext_lib=info,tao=warn,wry=warn"));

    // `try_init` so repeated calls (e.g. in tests) don't panic.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .try_init();
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
    init_tracing();

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

            // System tray (desktop only): app icon + Show/Quit menu.
            #[cfg(desktop)]
            tray::setup(app.handle())?;

            // Create the hidden, transparent, click-through waveform overlay
            // window up front (desktop only) so the live recording indicator has
            // a target the instant the Phase-4 pipeline starts emitting levels.
            // It stays hidden until the pipeline shows it on record start.
            #[cfg(desktop)]
            overlay::ensure(app.handle())?;

            tracing::info!("VibeToText backend initialized");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_entries,
            commands::get_statistics,
            commands::clear_history,
            commands::load_config,
            commands::save_config,
            commands::list_audio_devices,
            commands::set_audio_device,
            commands::get_dictionary,
            commands::add_word,
            commands::remove_word,
            commands::set_whisper_model,
            commands::set_orb_position,
        ])
        .run(tauri::generate_context!())
        .expect("error while running VibeToText application");
}
