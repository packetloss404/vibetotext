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

// Foundational modules. `db`, `config`, and `sentiment` are owned by other
// Phase 0 builders; only declared here.
pub mod config;
pub mod db;
pub mod sentiment;
pub mod state;

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
            // (added in later phases) can resolve it with `State<AppState>`.
            let state = AppState::new()?;
            app.manage(state);

            tracing::info!("VibeToText backend initialized");
            Ok(())
        })
        // Phase 1+ registers IPC commands here via `.invoke_handler(...)`.
        .run(tauri::generate_context!())
        .expect("error while running VibeToText application");
}
