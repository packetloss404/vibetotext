//! System tray icon + menu.
//!
//! Builds a basic tray (app icon + a Show/Quit menu) and toggles the main window
//! from it — the cross-platform replacement for the three native apps' tray code
//! (parity checklist §8: "system tray"). Left-clicking the tray icon also shows
//! the window where the platform reports tray click events.
//!
//! Requires the `tray-icon` Cargo feature on the `tauri` crate (enabled in
//! `Cargo.toml`). Tauri only exposes `tauri::tray` on desktop targets, so the
//! whole module is desktop-gated by the caller.

use tauri::{
    menu::{Menu, MenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

/// Build and attach the system tray to the running app.
///
/// Menu: **Show** (raise + focus the main window) and **Quit** (exit the app).
/// Returns an error if the menu/tray cannot be constructed.
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .tooltip("VibeToText")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // A plain left click raises the window (common desktop convention).
            if let TrayIconEvent::Click { .. } = event {
                show_main_window(tray.app_handle());
            }
        });

    // Reuse the app's default window icon for the tray when available.
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder.build(app)?;
    Ok(())
}

/// Raise, un-minimize, and focus the main window. No-op if it is gone.
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
