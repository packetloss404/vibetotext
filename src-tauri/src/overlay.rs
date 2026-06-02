//! Floating waveform overlay window (Phase 3).
//!
//! Ports the behavior of the Python `src/vibetotext/ui.py`
//! `WaveformOverlayController`: a small, borderless, transparent, always-on-top
//! indicator that floats near the bottom-center of the screen and animates a
//! 25-bar waveform while recording.
//!
//! Architecture: instead of the Python IPC-file-to-subprocess scheme, this is a
//! second Tauri webview window (`overlay.html` / `overlay.js`). The Rust side
//! only manages the window lifecycle (create-once, show, hide); the pipeline
//! agent drives the visuals by emitting the `waveform-levels` (`number[25]`,
//! each `0..1`) and `recording-state` (`{recording, mode}`) events that the
//! overlay's JS subscribes to (plan §5 event contract).
//!
//! The window is created lazily and reused. It is:
//! - transparent + borderless (just the bars float, no chrome)
//! - always-on-top (visible over the focused app the user is dictating into)
//! - skip-taskbar + non-focusable (never steals focus from that app)
//! - click-through (`set_ignore_cursor_events(true)`) so clicks pass through to
//!   whatever is underneath
//! - initially hidden; the pipeline calls [`show`]/[`hide`] around a recording.
//!
//! NOTE (macOS transparency): on macOS, true webview transparency additionally
//! requires the private API to be enabled. The builder/wiring agent (Phase 5)
//! MUST set `app.macOSPrivateApi = true` in `tauri.conf.json` (or
//! `tauri::Builder::macos_private_api`) or this window will render with an
//! opaque background on macOS. Windows/Linux only need `.transparent(true)`.

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Stable label for the overlay window (used by every helper to look it up).
const OVERLAY_LABEL: &str = "overlay";

/// Logical size of the floating indicator, in DIPs. Mirrors the compact pill
/// dimensions of the Python overlay (`ui.py`): wide enough for 25 bars, short.
const OVERLAY_WIDTH: f64 = 280.0;
const OVERLAY_HEIGHT: f64 = 80.0;

/// Margin above the bottom screen edge for the bottom-center anchor (DIPs).
const BOTTOM_MARGIN: f64 = 120.0;

/// Create the overlay window if it does not already exist.
///
/// Idempotent: if the window is already present (looked up by [`OVERLAY_LABEL`])
/// this is a no-op and returns `Ok(())`. The created window starts **hidden**;
/// call [`show`] to reveal it.
pub fn ensure(app: &tauri::AppHandle) -> tauri::Result<()> {
    // Already created -> nothing to do.
    if app.get_webview_window(OVERLAY_LABEL).is_some() {
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        OVERLAY_LABEL,
        // `overlay.html` is bundled alongside the main frontend (app/src/).
        WebviewUrl::App("overlay.html".into()),
    )
    .title("VibeToText Overlay")
    .inner_size(OVERLAY_WIDTH, OVERLAY_HEIGHT)
    .transparent(true) // see macOS note in the module docs
    .decorations(false) // borderless: no title bar / frame
    .always_on_top(true) // float over the user's active app
    .skip_taskbar(true) // don't show up in the taskbar / app switcher
    .focused(false) // never steal focus from the app being dictated into
    .resizable(false)
    .shadow(false) // a shadow would draw an opaque-ish box around the pill
    .visible(false) // start hidden; pipeline calls show() on record start
    .build()?;

    // Click-through: pointer events fall through to whatever is underneath so
    // the floating bars never intercept clicks. Ported from the Python overlay
    // being a non-interactive HUD.
    let _ = window.set_ignore_cursor_events(true);

    // Anchor near bottom-center of the monitor the window landed on. Best-effort:
    // if monitor metrics are unavailable we leave it at the builder default.
    position_bottom_center(&window);

    Ok(())
}

/// Show the overlay (creating it first if needed).
pub fn show(app: &tauri::AppHandle) -> tauri::Result<()> {
    ensure(app)?;
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        // Re-anchor on each show in case the display layout changed since
        // creation, then reveal + keep it on top.
        position_bottom_center(&window);
        window.show()?;
        let _ = window.set_always_on_top(true);
    }
    Ok(())
}

/// Hide the overlay. No-op if it was never created.
pub fn hide(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        window.hide()?;
    }
    Ok(())
}

/// Position the window centered horizontally and near the bottom of the monitor
/// it currently sits on (falling back to the primary monitor). Best-effort:
/// monitor lookups can fail on some platforms/headless setups, in which case the
/// window keeps its builder-default position.
fn position_bottom_center(window: &tauri::WebviewWindow) {
    use tauri::{LogicalPosition, LogicalSize};

    let monitor = match window.current_monitor() {
        Ok(Some(m)) => Some(m),
        _ => window.primary_monitor().ok().flatten(),
    };

    let Some(monitor) = monitor else { return };

    let scale = monitor.scale_factor();
    let size: LogicalSize<f64> = monitor.size().to_logical(scale);
    let origin: LogicalPosition<f64> = monitor.position().to_logical(scale);

    let x = origin.x + (size.width - OVERLAY_WIDTH) / 2.0;
    let y = origin.y + size.height - OVERLAY_HEIGHT - BOTTOM_MARGIN;

    let _ = window.set_position(LogicalPosition::new(x, y));
}
