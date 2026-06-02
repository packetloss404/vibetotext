// rdev modifier-only push-to-talk spike
// ======================================
// Proves that the four VibeToText push-to-talk chords can be driven entirely by
// global modifier-key state via `rdev::listen()`. Ports the chord table from the
// macOS `HotkeyManager.swift` reference (cmd==Meta on macOS, Win key on Windows).
//
// Chords (most-specific wins):
//   ctrl+shift            -> transcribe
//   alt+shift             -> cleanup
//   meta+alt+shift        -> greppy
//   meta+alt+'p'          -> plan
//
// On chord-complete: prints `START <mode>`.
// On release of any chord key: prints `STOP <mode> (held Xs)`.
// A 60s auto-cutoff stops an in-flight recording even if keys stay held.
//
// PLATFORM CAVEATS for rdev::listen() (the whole reason this is a go/no-go spike):
//   * Linux: X11 ONLY. There is no native Wayland backend; under a pure Wayland
//     session listen() either fails to grab events or returns an error. XWayland
//     is the practical fallback. The migration plan abstracts the listener behind
//     a trait so we can swap to `hotkey-listener` (evdev) for native Wayland later.
//   * macOS: listen() SILENTLY NO-OPS without Accessibility permission
//     (System Settings -> Privacy & Security -> Accessibility). No error is
//     returned; the callback simply never fires. The real app gates this with
//     AXIsProcessTrusted() before starting the listener.
//   * Windows: works via a low-level keyboard hook; no special permission needed.
//
// This file only needs to COMPILE in CI. Exercising it requires a real desktop
// session (X11 / permitted macOS / Windows), so `cargo run` here is a no-op until
// then.

use std::collections::HashSet;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rdev::{listen, Event, EventType, Key};

/// The non-printing modifiers we care about, with left/right variants normalized
/// to a single logical modifier (matching how the OS reports a "shift is down"
/// state regardless of which physical shift key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Meta,
}

/// Map a raw rdev `Key` to a logical `Modifier`, normalizing L/R variants.
/// Returns `None` for non-modifier keys.
fn modifier_of(key: Key) -> Option<Modifier> {
    match key {
        Key::ControlLeft | Key::ControlRight => Some(Modifier::Ctrl),
        Key::ShiftLeft | Key::ShiftRight => Some(Modifier::Shift),
        // `Alt` is left-alt; `AltGr` is right-alt. Both normalize to Alt so the
        // chord fires from either side of the keyboard.
        Key::Alt | Key::AltGr => Some(Modifier::Alt),
        Key::MetaLeft | Key::MetaRight => Some(Modifier::Meta),
        _ => None,
    }
}

/// A push-to-talk chord: a required modifier set plus an optional printing key.
/// `key == None` means a modifier-only chord (fires as soon as the modifiers
/// are all held); `key == Some(_)` means the printing key must also go down.
struct Chord {
    mode: &'static str,
    modifiers: &'static [Modifier],
    key: Option<Key>,
}

/// Chord table. Order does not matter for matching (we sort by specificity at
/// match time) but is kept consistent with the reference implementation.
const CHORDS: &[Chord] = &[
    Chord {
        mode: "plan",
        modifiers: &[Modifier::Meta, Modifier::Alt],
        key: Some(Key::KeyP),
    },
    Chord {
        mode: "greppy",
        modifiers: &[Modifier::Meta, Modifier::Alt, Modifier::Shift],
        key: None,
    },
    Chord {
        mode: "cleanup",
        modifiers: &[Modifier::Alt, Modifier::Shift],
        key: None,
    },
    Chord {
        mode: "transcribe",
        modifiers: &[Modifier::Ctrl, Modifier::Shift],
        key: None,
    },
];

/// "Specificity" of a chord = number of keys that must be held (modifiers + an
/// optional printing key). Longer chords are matched first so greppy (3 mods)
/// beats cleanup (2 mods) beats transcribe (2 mods), and plan (2 mods + key)
/// out-ranks the 2-mod-only chords.
fn specificity(c: &Chord) -> usize {
    c.modifiers.len() + if c.key.is_some() { 1 } else { 0 }
}

/// True if every required modifier of `chord` is currently held.
fn modifiers_satisfied(chord: &Chord, held: &HashSet<Modifier>) -> bool {
    chord.modifiers.iter().all(|m| held.contains(m))
}

/// Pick the most-specific chord whose modifiers are all held. For modifier-only
/// chords that's sufficient; for printing-key chords the key must also be down,
/// which is signalled via `pressed_key`.
fn match_chord<'a>(
    held: &HashSet<Modifier>,
    pressed_key: Option<Key>,
) -> Option<&'a Chord> {
    let mut best: Option<&Chord> = None;
    for c in CHORDS {
        if !modifiers_satisfied(c, held) {
            continue;
        }
        // Printing-key chords only fire when their key is the one just pressed
        // (or is being held). Modifier-only chords ignore the printing key.
        if let Some(k) = c.key {
            if pressed_key != Some(k) {
                continue;
            }
        }
        if best.map_or(true, |b| specificity(c) > specificity(b)) {
            best = Some(c);
        }
    }
    best
}

/// State of the currently-active recording, if any.
struct Active {
    mode: &'static str,
    started: Instant,
    /// The exact set of keys that must remain held; releasing ANY of them stops
    /// the recording. For modifier-only chords this is the modifier set; for
    /// printing-key chords it additionally includes the printing key.
    required_modifiers: Vec<Modifier>,
    required_key: Option<Key>,
}

const AUTO_CUTOFF: Duration = Duration::from_secs(60);

fn main() {
    println!("rdev push-to-talk spike. Chords:");
    println!("  [ctrl+shift]     = transcribe");
    println!("  [alt+shift]      = cleanup");
    println!("  [meta+alt+shift] = greppy");
    println!("  [meta+alt+'p']   = plan");
    println!("(60s auto-cutoff. Ctrl-C to quit.)");

    // rdev's listen() takes a callback and blocks forever on its own thread, so
    // we run it on a dedicated thread and funnel events into an mpsc channel.
    // This keeps the state machine single-threaded and lets us poll for the
    // auto-cutoff timer with a recv_timeout.
    let (tx, rx) = mpsc::channel::<Event>();
    thread::spawn(move || {
        // On macOS without Accessibility, and on native Wayland, this either
        // errors or never delivers events (see top-of-file caveats).
        if let Err(err) = listen(move |event| {
            // Best-effort: if the receiver is gone we just stop forwarding.
            let _ = tx.send(event);
        }) {
            eprintln!("rdev listen() failed: {:?}", err);
            eprintln!("Likely cause: native Wayland (X11 required) or missing macOS Accessibility permission.");
        }
    });

    let mut held: HashSet<Modifier> = HashSet::new();
    let mut active: Option<Active> = None;

    loop {
        // Poll with a timeout so the 60s auto-cutoff is enforced even when no
        // key events are arriving (keys held steady).
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => handle_event(event, &mut held, &mut active),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Auto-cutoff check.
        if let Some(a) = &active {
            if a.started.elapsed() >= AUTO_CUTOFF {
                println!(
                    "STOP {} (held {:.1}s) [auto-cutoff]",
                    a.mode,
                    a.started.elapsed().as_secs_f64()
                );
                active = None;
            }
        }
    }
}

fn handle_event(
    event: Event,
    held: &mut HashSet<Modifier>,
    active: &mut Option<Active>,
) {
    match event.event_type {
        EventType::KeyPress(key) => {
            // Update modifier state.
            if let Some(m) = modifier_of(key) {
                held.insert(m);
            }

            // Only attempt to start when nothing is active. The printing key
            // matters only for plan; for modifier-only chords we still want a
            // bare modifier press to trigger evaluation, so pass it through.
            if active.is_none() {
                let pressed_key = Some(key);
                if let Some(chord) = match_chord(held, pressed_key) {
                    println!("START {}", chord.mode);
                    *active = Some(Active {
                        mode: chord.mode,
                        started: Instant::now(),
                        required_modifiers: chord.modifiers.to_vec(),
                        required_key: chord.key,
                    });
                }
            }
        }
        EventType::KeyRelease(key) => {
            let released_mod = modifier_of(key);
            if let Some(m) = released_mod {
                held.remove(&m);
            }

            // If a recording is active, releasing ANY of its required keys stops it.
            if let Some(a) = active {
                let modifier_released = released_mod
                    .map(|m| a.required_modifiers.contains(&m))
                    .unwrap_or(false);
                let key_released = a.required_key == Some(key);

                if modifier_released || key_released {
                    println!(
                        "STOP {} (held {:.1}s)",
                        a.mode,
                        a.started.elapsed().as_secs_f64()
                    );
                    *active = None;
                }
            }
        }
        _ => {}
    }
}
