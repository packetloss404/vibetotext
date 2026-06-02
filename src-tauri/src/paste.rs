//! Auto-paste at the cursor.
//!
//! Phase 3 port of `src/vibetotext/output.py` (`paste_at_cursor` + per-OS
//! `simulate_paste_*`) and the Windows-native `PasteService.cs`. The single
//! public entry point is [`paste_at_cursor`].
//!
//! ## Strategy (mirrors the references)
//! 1. **Clipboard first.** Copy the text to the system clipboard via `arboard`.
//!    This is the load-bearing step: even if the synthetic keystroke fails, the
//!    user can paste manually. (`output.py` copies before pasting; `PasteService`
//!    sets the clipboard before `SendInput`.)
//! 2. **Release held modifiers + settle.** The user is (probably) still holding
//!    the push-to-talk hotkey chord (e.g. `meta+alt`). If those modifiers are
//!    down when we send Ctrl/Cmd+V, the OS sees a corrupted chord and the paste
//!    misfires. `PasteService.cs` explicitly releases L/R Shift/Ctrl/Alt then
//!    waits 250ms; `output.py` sleeps 50-100ms. We best-effort release the paste
//!    modifier and sleep ~80ms.
//! 3. **Synthetic paste.** `Cmd+V` on macOS, `Ctrl+V` on Windows/Linux, via
//!    `enigo`.
//! 4. **Clipboard-first fallback.** If the synthetic paste fails, the text is
//!    *already on the clipboard*, so this is a soft failure: log a warning,
//!    best-effort notify the user, and return `Ok(())` — do **not** hard-error.
//!    (Matches `output.py`, which falls back to a notification sound, and
//!    `PasteService`, which beeps on exception.)
//!
//! ## Wayland caveat (plan §7 follow-up — TODO)
//! `enigo`'s synthetic input is X11-oriented (it talks XTEST). Under a native
//! Wayland session, synthetic key injection is typically blocked by the
//! compositor and the paste step will silently no-op (the clipboard copy still
//! works, so the user can paste manually). The documented follow-up is to
//! detect Wayland (`XDG_SESSION_TYPE=wayland` / `WAYLAND_DISPLAY`) and shell out
//! to `wtype` for the keystroke and `wl-copy` for the clipboard — exactly the
//! fallback `output.py` already implements for Linux. Tracked as a Phase 7 TODO;
//! not implemented here (the X11/XWayland path is the MVP per plan §2).

use anyhow::{Context, Result};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

/// Time to wait after releasing modifiers before sending the paste chord, so a
/// still-held hotkey chord doesn't corrupt the synthetic Ctrl/Cmd+V.
///
/// `output.py` uses 50-100ms; `PasteService.cs` uses 250ms. 80ms is a balance:
/// long enough for the modifier key-up to propagate, short enough to feel
/// instant after the user releases the chord.
const MODIFIER_SETTLE_MS: u64 = 80;

/// The platform modifier key for the paste chord: `Cmd` on macOS, `Ctrl`
/// elsewhere. Keeping this in one helper means the rest of the flow is OS-agnostic.
#[cfg(target_os = "macos")]
fn paste_modifier() -> Key {
    Key::Meta // Command key on macOS.
}

#[cfg(not(target_os = "macos"))]
fn paste_modifier() -> Key {
    Key::Control
}

/// Copy `text` to the system clipboard and simulate a paste keystroke at the
/// cursor.
///
/// Returns `Ok(())` when the text reaches the clipboard, *even if the synthetic
/// paste itself fails* — in that case the text is still on the clipboard for a
/// manual paste (clipboard-first semantics). The only hard error is failure to
/// access the clipboard at all, which means there is nothing for the user to
/// fall back to.
pub fn paste_at_cursor(text: &str) -> Result<()> {
    // Don't clobber the clipboard with empty/whitespace-only output (parity with
    // `output.py`, which skips empty text outright).
    if text.trim().is_empty() {
        tracing::debug!("paste_at_cursor: skipping empty/whitespace-only text");
        return Ok(());
    }

    // (1) Clipboard first — the load-bearing step. If this fails there is no
    // fallback, so it is the one genuinely hard error.
    let mut clipboard =
        arboard::Clipboard::new().context("failed to open the system clipboard")?;
    clipboard
        .set_text(text)
        .context("failed to write text to the system clipboard")?;
    tracing::info!(chars = text.len(), "copied transcription to clipboard");

    // (2) + (3) Synthetic paste. Any failure here is *soft*: the text is already
    // on the clipboard, so we degrade to a manual paste rather than erroring.
    match simulate_paste() {
        Ok(()) => {
            tracing::info!("auto-paste succeeded");
        }
        Err(e) => {
            // (4) Clipboard-first fallback: warn, best-effort notify, return Ok.
            tracing::warn!(
                error = %e,
                "auto-paste failed; text remains on the clipboard for manual paste"
            );
            notify_manual_paste_needed();
        }
    }

    Ok(())
}

/// Release any held paste modifier, settle, then send the OS paste chord via
/// `enigo`. Returns `Err` if `enigo` could not be initialized or the keystroke
/// could not be sent — the caller treats that as a soft, clipboard-backed failure.
fn simulate_paste() -> Result<()> {
    let modifier = paste_modifier();

    let mut enigo =
        Enigo::new(&Settings::default()).context("failed to initialize enigo input simulator")?;

    // (2) Briefly release the held modifier so a still-held hotkey chord doesn't
    // fuse with our synthetic chord. `PasteService.cs` releases every modifier
    // (L/R Shift/Ctrl/Alt) explicitly; with `enigo` we best-effort release the
    // paste modifier itself. A Release on an already-up key is a harmless no-op,
    // so we ignore its result.
    let _ = enigo.key(modifier, Direction::Release);

    // Wait for the key-up to propagate before we press the chord (parity with
    // both references' post-release sleep).
    std::thread::sleep(std::time::Duration::from_millis(MODIFIER_SETTLE_MS));

    // (3) Send the paste chord: hold modifier, click 'v', release modifier.
    enigo
        .key(modifier, Direction::Press)
        .context("failed to press paste modifier")?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .context("failed to send 'v' keystroke")?;
    enigo
        .key(modifier, Direction::Release)
        .context("failed to release paste modifier")?;

    Ok(())
}

/// Best-effort user notification that an auto-paste failed and the text is
/// waiting on the clipboard for a manual paste.
///
/// The Python/`C#` references play a system beep here. We don't have an audio
/// dependency wired in this module, so we emit a tracing event that the pipeline
/// layer can surface (e.g. via a Tauri notification or tray balloon). This is
/// intentionally infallible — a failed notification must never escalate a soft
/// paste failure into a hard error.
fn notify_manual_paste_needed() {
    // The pipeline/event layer (Phase 4) can subscribe to this target to raise a
    // user-facing toast. Kept dependency-free so paste.rs stays self-contained.
    tracing::warn!(
        target: "vibetotext::user_notification",
        "transcription copied to clipboard — auto-paste failed, paste manually (Ctrl/Cmd+V)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_a_noop_and_succeeds() {
        // Whitespace-only / empty input must not touch the clipboard or error.
        // (No clipboard/display needed, so this is safe in headless CI.)
        assert!(paste_at_cursor("").is_ok());
        assert!(paste_at_cursor("   \n\t ").is_ok());
    }

    #[test]
    fn paste_modifier_is_platform_correct() {
        let m = paste_modifier();
        #[cfg(target_os = "macos")]
        assert!(matches!(m, Key::Meta));
        #[cfg(not(target_os = "macos"))]
        assert!(matches!(m, Key::Control));
    }
}
