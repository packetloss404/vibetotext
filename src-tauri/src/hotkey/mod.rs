//! Global push-to-talk hotkey listener.
//!
//! This module owns the chord state machine that turns global keyboard events
//! into [`HotkeyEvent::Start`] / [`HotkeyEvent::Stop`] for the four (well, five)
//! VibeToText [`Mode`]s. The actual OS event source sits behind the
//! [`HotkeyBackend`] trait so we can swap the X11/macOS [`RdevBackend`] for an
//! evdev-based `hotkey-listener` backend when we add native Wayland (plan §3/§9,
//! risk #1).
//!
//! ## Chords (most-specific wins)
//!
//! | Chord                | Mode         |
//! |----------------------|--------------|
//! | `ctrl+shift`         | Transcribe   |
//! | `alt+shift`          | Cleanup      |
//! | `meta+alt+shift`     | Greppy       |
//! | `meta+alt+KeyP`      | Plan         |
//! | `ctrl+alt`           | History      |
//!
//! ## Design notes (spike carry-forwards, plan §3)
//!
//! * **(a) Start on modifier-state change, not arbitrary keypress.** A stray
//!   printing key pressed while modifiers are held must not spuriously START a
//!   modifier-only chord. The state machine therefore re-evaluates START only
//!   when a *modifier* goes down, or when a *chord-relevant printing key*
//!   (`KeyP`) goes down — never on an unrelated character key. [`resolve_chord`]
//!   itself stays pure and total.
//! * **(b) Plan(`meta+alt+P`) and Greppy(`meta+alt+shift`) share `meta+alt`.**
//!   Because at most one recording is active at a time and any required-key
//!   release STOPs it, the state machine cannot double-fire when transitioning
//!   between the two: leaving one chord stops it before the other can start.
//! * **(c) 60s auto-cutoff** stops an in-flight recording even if keys stay
//!   held.
//! * **(d) Most-specific chord wins** (greppy beats cleanup/transcribe; plan
//!   out-ranks the 2-mod-only chords) — encoded in [`resolve_chord`]'s scoring.

pub mod permissions;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

/// When set, the rdev backend ignores ALL key events. Auto-paste raises this
/// around its synthetic Ctrl/Cmd+V so the global listener doesn't see its own
/// injected keystrokes as a fresh chord (the self-trigger feedback loop that
/// produced spurious 0-sample recordings right after a paste).
pub static SUPPRESS_HOTKEYS: AtomicBool = AtomicBool::new(false);

/// RAII guard that suppresses hotkey processing for its lifetime. Wrap synthetic
/// input injection in one (see `paste::simulate_paste`). Suppression is released
/// when the guard drops.
pub struct SuppressGuard;

impl SuppressGuard {
    pub fn new() -> Self {
        SUPPRESS_HOTKEYS.store(true, Ordering::SeqCst);
        SuppressGuard
    }
}

impl Drop for SuppressGuard {
    fn drop(&mut self) {
        SUPPRESS_HOTKEYS.store(false, Ordering::SeqCst);
    }
}

/// Push-to-talk modes. `snake_case` over serde so the wire/event form matches
/// the rest of the IPC contract (plan §5) and the legacy mode strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Transcribe,
    Cleanup,
    Plan,
    Greppy,
    History,
}

/// Emitted by the listener as chords begin and end. The pipeline turns a
/// `Start` into "begin recording in this mode" and a `Stop` into "finish &
/// process" (plan §4).
///
/// Externally tagged (serde default) so the inner [`Mode`] — which serializes
/// to a bare string — round-trips cleanly: an internally-tagged representation
/// would reject a newtype variant wrapping a non-map value at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyEvent {
    Start(Mode),
    Stop(Mode),
}

/// The non-printing modifiers we track, with left/right variants normalized to a
/// single logical modifier (the OS reports "shift is down" regardless of which
/// physical shift key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Meta,
}

/// Printing keys that participate in a chord. We only care about the ones used
/// by a chord (currently just `P` for Plan); everything else is "some other
/// printing key" and is irrelevant to chord resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrintingKey {
    P,
}

/// 60-second auto-cutoff: a held chord is force-stopped after this long.
pub const AUTO_CUTOFF: Duration = Duration::from_secs(60);

/// Pure chord resolver: given the currently-held modifiers and the
/// chord-relevant printing key that is down (if any), return the [`Mode`] that
/// should be active, or `None` if no chord matches.
///
/// "Most-specific wins" is implemented by scoring each candidate chord by the
/// number of keys it requires and keeping the highest score. This makes:
///
/// * greppy (`meta+alt+shift`, 3) beat cleanup (`alt+shift`, 2) and transcribe
///   (`ctrl+shift`, 2) when all of meta/alt/shift are held,
/// * plan (`meta+alt`+`P`, 3) win over any 2-mod chord while `P` is down,
/// * `meta+alt` *alone* (no shift, no `P`) resolve to `None` — it is only a
///   shared prefix, never a chord on its own.
///
/// The function is **total and side-effect-free** so it can be unit-tested
/// exhaustively and reused by either backend.
pub fn resolve_chord(mods: &HashSet<Modifier>, printing_key: Option<PrintingKey>) -> Option<Mode> {
    use Modifier::*;

    let has = |m: Modifier| mods.contains(&m);

    // (chord modes, predicate that it is fully satisfied, specificity score)
    //
    // Specificity = number of keys that must be held. Modifier-only chords score
    // by their modifier count; Plan additionally counts its printing key.
    let mut best: Option<(Mode, u32)> = None;
    let mut consider = |mode: Mode, satisfied: bool, score: u32| {
        if satisfied {
            match best {
                Some((_, b)) if b >= score => {}
                _ => best = Some((mode, score)),
            }
        }
    };

    // Plan: meta+alt+P (score 3). Requires the printing key to be down.
    consider(
        Mode::Plan,
        has(Meta) && has(Alt) && printing_key == Some(PrintingKey::P),
        3,
    );
    // Greppy: meta+alt+shift (score 3).
    consider(Mode::Greppy, has(Meta) && has(Alt) && has(Shift), 3);
    // Cleanup: alt+shift (score 2).
    consider(Mode::Cleanup, has(Alt) && has(Shift), 2);
    // Transcribe: ctrl+shift (score 2).
    consider(Mode::Transcribe, has(Ctrl) && has(Shift), 2);
    // History: ctrl+alt (score 2).
    consider(Mode::History, has(Ctrl) && has(Alt), 2);

    best.map(|(mode, _)| mode)
}

/// Abstraction over the OS keyboard-event source. Implemented by
/// [`RdevBackend`] today; a Wayland/evdev backend can be dropped in later
/// without touching [`spawn`] or the pipeline (plan §3/§9, risk #1).
///
/// `run` consumes the backend (it blocks for the process lifetime) and pushes
/// resolved [`HotkeyEvent`]s into `sink`.
pub trait HotkeyBackend {
    fn run(self: Box<Self>, sink: Box<dyn Fn(HotkeyEvent) + Send>);
}

/// The currently-active recording, if any.
struct Active {
    mode: Mode,
    started: Instant,
}

/// The shared chord state machine. Both backends funnel their normalized
/// key-state transitions through here so the START/STOP/auto-cutoff semantics
/// (and the carry-forward fixes) live in exactly one place.
///
/// Backends call:
/// * [`StateMachine::on_modifier_change`] when a modifier goes down/up,
/// * [`StateMachine::on_printing_key`] when a chord-relevant printing key goes
///   down/up,
/// * [`StateMachine::tick`] periodically to enforce the auto-cutoff.
pub struct StateMachine {
    held: HashSet<Modifier>,
    printing: Option<PrintingKey>,
    active: Option<Active>,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            held: HashSet::new(),
            printing: None,
            active: None,
        }
    }

    /// A modifier transitioned. `down == true` for press, `false` for release.
    /// Returns any emitted event(s) — at most a STOP followed by a START when a
    /// transition both ends one chord and begins another (carry-forward (b)).
    pub fn on_modifier_change(&mut self, m: Modifier, down: bool) -> Vec<HotkeyEvent> {
        if down {
            self.held.insert(m);
        } else {
            self.held.remove(&m);
        }
        self.reevaluate()
    }

    /// A chord-relevant printing key transitioned. Unrelated printing keys must
    /// NOT be routed here (carry-forward (a)): the backend only forwards keys it
    /// maps to a [`PrintingKey`].
    pub fn on_printing_key(&mut self, k: PrintingKey, down: bool) -> Vec<HotkeyEvent> {
        if down {
            self.printing = Some(k);
        } else if self.printing == Some(k) {
            self.printing = None;
        }
        self.reevaluate()
    }

    /// Re-derive the desired active mode from current key state and reconcile it
    /// against what is actually active, emitting STOP/START as needed.
    ///
    /// Reconciliation logic (single active recording invariant):
    /// * desired == active.mode  -> nothing (still holding the same chord),
    /// * desired != active.mode  -> STOP old, then START new if desired is Some,
    /// * desired Some, none active -> START,
    /// * desired None, active     -> STOP.
    ///
    /// Because we STOP before any START, the Plan<->Greppy transition over the
    /// shared `meta+alt` prefix cannot double-fire (carry-forward (b)).
    fn reevaluate(&mut self) -> Vec<HotkeyEvent> {
        let desired = resolve_chord(&self.held, self.printing);
        let mut out = Vec::new();

        match (&self.active, desired) {
            (Some(a), Some(d)) if a.mode == d => {
                // Same chord still held; no event.
            }
            (Some(a), Some(d)) => {
                out.push(HotkeyEvent::Stop(a.mode));
                out.push(HotkeyEvent::Start(d));
                self.active = Some(Active {
                    mode: d,
                    started: Instant::now(),
                });
            }
            (Some(a), None) => {
                out.push(HotkeyEvent::Stop(a.mode));
                self.active = None;
            }
            (None, Some(d)) => {
                out.push(HotkeyEvent::Start(d));
                self.active = Some(Active {
                    mode: d,
                    started: Instant::now(),
                });
            }
            (None, None) => {}
        }

        out
    }

    /// Enforce the 60s auto-cutoff. Returns a STOP if the active chord has been
    /// held past [`AUTO_CUTOFF`]. The chord stays "held" in key state, so it
    /// will NOT immediately re-START: a fresh START requires releasing and
    /// re-pressing (matching the spike's behavior).
    pub fn tick(&mut self) -> Option<HotkeyEvent> {
        if let Some(a) = &self.active {
            if a.started.elapsed() >= AUTO_CUTOFF {
                let mode = a.mode;
                self.active = None;
                return Some(HotkeyEvent::Stop(mode));
            }
        }
        None
    }

    /// Test/integration hook: is a recording currently active, and in which mode.
    #[allow(dead_code)]
    pub fn active_mode(&self) -> Option<Mode> {
        self.active.as_ref().map(|a| a.mode)
    }
}

/// The `rdev`-backed listener (X11 on Linux, low-level hook on Windows, AX on
/// macOS). `rdev` is X11-only on Linux and silently no-ops on macOS without
/// Accessibility permission — see [`permissions`] and plan §3/§9.
///
/// The `rdev` event-source wiring is compiled only when the `rdev` dependency is
/// present (added to `Cargo.toml` by the wiring builder). The trait, state
/// machine, and pure resolver above compile and test without it.
pub struct RdevBackend {
    _private: (),
}

impl Default for RdevBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RdevBackend {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(feature = "rdev-backend")]
mod rdev_impl {
    use super::*;
    use rdev::{listen, Event, EventType, Key};
    use std::sync::mpsc;

    /// Map a raw rdev `Key` to a logical [`Modifier`], normalizing L/R variants.
    fn modifier_of(key: Key) -> Option<Modifier> {
        match key {
            Key::ControlLeft | Key::ControlRight => Some(Modifier::Ctrl),
            Key::ShiftLeft | Key::ShiftRight => Some(Modifier::Shift),
            // `Alt` == left-alt, `AltGr` == right-alt; both normalize to Alt so
            // the chord fires from either side of the keyboard.
            Key::Alt | Key::AltGr => Some(Modifier::Alt),
            Key::MetaLeft | Key::MetaRight => Some(Modifier::Meta),
            _ => None,
        }
    }

    /// Map a raw rdev `Key` to a chord-relevant [`PrintingKey`], if any. Returns
    /// `None` for every key that no chord uses — those must never reach the
    /// state machine's printing-key path (carry-forward (a)).
    fn printing_of(key: Key) -> Option<PrintingKey> {
        match key {
            Key::KeyP => Some(PrintingKey::P),
            _ => None,
        }
    }

    impl HotkeyBackend for RdevBackend {
        fn run(self: Box<Self>, sink: Box<dyn Fn(HotkeyEvent) + Send>) {
            // rdev's listen() blocks forever on its own thread, so we funnel its
            // events into a channel and run the state machine here, polling with
            // a timeout to enforce the auto-cutoff while keys stay held.
            let (tx, rx) = mpsc::channel::<Event>();
            thread::spawn(move || {
                tracing::info!("installing global hotkey hook (rdev listen)");
                if let Err(err) = listen(move |event| {
                    let _ = tx.send(event);
                }) {
                    tracing::error!(
                        ?err,
                        "rdev listen() failed (native Wayland needs X11, or macOS \
                         Accessibility not granted)"
                    );
                }
            });

            let mut sm = StateMachine::new();
            loop {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(event) => {
                        // Ignore events while suppressed (our own auto-paste
                        // injecting synthetic keys); tick() below still runs.
                        let events = if SUPPRESS_HOTKEYS.load(Ordering::SeqCst) {
                            Vec::new()
                        } else {
                            match event.event_type {
                            EventType::KeyPress(key) => {
                                if let Some(m) = modifier_of(key) {
                                    sm.on_modifier_change(m, true)
                                } else if let Some(p) = printing_of(key) {
                                    sm.on_printing_key(p, true)
                                } else {
                                    // Unrelated printing key while modifiers are
                                    // held: deliberately NOT routed (no spurious
                                    // START) — carry-forward (a).
                                    Vec::new()
                                }
                            }
                            EventType::KeyRelease(key) => {
                                if let Some(m) = modifier_of(key) {
                                    sm.on_modifier_change(m, false)
                                } else if let Some(p) = printing_of(key) {
                                    sm.on_printing_key(p, false)
                                } else {
                                    Vec::new()
                                }
                            }
                            _ => Vec::new(),
                            }
                        };
                        for e in events {
                            sink(e);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }

                if let Some(e) = sm.tick() {
                    sink(e);
                }
            }
        }
    }
}

// Fallback `HotkeyBackend` impl for when the rdev event source is not compiled
// in (e.g. the `rdev` dep / `rdev-backend` feature is absent). It still
// satisfies the trait so `spawn` type-checks; it just parks the thread. The
// wiring builder enables `rdev-backend` once `rdev` is in `Cargo.toml`.
#[cfg(not(feature = "rdev-backend"))]
impl HotkeyBackend for RdevBackend {
    fn run(self: Box<Self>, _sink: Box<dyn Fn(HotkeyEvent) + Send>) {
        tracing::warn!(
            "RdevBackend compiled without the `rdev-backend` feature; global \
             hotkeys are disabled. Enable the feature (and the rdev dependency) \
             to activate push-to-talk."
        );
    }
}

/// Start the global hotkey listener on a dedicated thread, invoking `on_event`
/// for every [`HotkeyEvent`]. Returns immediately; the listener runs for the
/// process lifetime.
///
/// On macOS this first checks [`permissions::ensure_accessibility`]; if not
/// trusted it logs and still spawns the backend (so the OS prompt is shown and
/// the listener begins delivering events once permission is granted).
pub fn spawn(on_event: impl Fn(HotkeyEvent) + Send + 'static) {
    if !permissions::ensure_accessibility() {
        tracing::warn!(
            "Accessibility permission not yet granted; global hotkeys will be \
             inert until the user grants it (macOS)."
        );
    }

    let backend: Box<dyn HotkeyBackend + Send> = Box::new(RdevBackend::new());
    thread::spawn(move || {
        backend.run(Box::new(on_event));
    });
}

// `RdevBackend` must be `Send` for `spawn` to move it onto a thread.
unsafe impl Send for RdevBackend {}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(list: &[Modifier]) -> HashSet<Modifier> {
        list.iter().copied().collect()
    }

    // --- resolve_chord: each chord resolves to its mode --------------------

    #[test]
    fn transcribe_ctrl_shift() {
        assert_eq!(
            resolve_chord(&mods(&[Modifier::Ctrl, Modifier::Shift]), None),
            Some(Mode::Transcribe)
        );
    }

    #[test]
    fn cleanup_alt_shift() {
        assert_eq!(
            resolve_chord(&mods(&[Modifier::Alt, Modifier::Shift]), None),
            Some(Mode::Cleanup)
        );
    }

    #[test]
    fn greppy_meta_alt_shift() {
        assert_eq!(
            resolve_chord(
                &mods(&[Modifier::Meta, Modifier::Alt, Modifier::Shift]),
                None
            ),
            Some(Mode::Greppy)
        );
    }

    #[test]
    fn plan_meta_alt_p() {
        assert_eq!(
            resolve_chord(
                &mods(&[Modifier::Meta, Modifier::Alt]),
                Some(PrintingKey::P)
            ),
            Some(Mode::Plan)
        );
    }

    #[test]
    fn history_ctrl_alt() {
        assert_eq!(
            resolve_chord(&mods(&[Modifier::Ctrl, Modifier::Alt]), None),
            Some(Mode::History)
        );
    }

    // --- most-specific wins ------------------------------------------------

    #[test]
    fn greppy_beats_cleanup_and_transcribe() {
        // meta+alt+shift holds; alt+shift (cleanup) is also satisfied, but the
        // 3-key greppy chord must win.
        let m = mods(&[Modifier::Meta, Modifier::Alt, Modifier::Shift]);
        assert_eq!(resolve_chord(&m, None), Some(Mode::Greppy));
    }

    #[test]
    fn greppy_beats_transcribe_when_ctrl_also_held() {
        // Everything held: ctrl+shift (transcribe), alt+shift (cleanup),
        // ctrl+alt (history) and meta+alt+shift (greppy) all match — greppy (3)
        // is most specific.
        let m = mods(&[
            Modifier::Ctrl,
            Modifier::Meta,
            Modifier::Alt,
            Modifier::Shift,
        ]);
        assert_eq!(resolve_chord(&m, None), Some(Mode::Greppy));
    }

    #[test]
    fn plan_beats_two_mod_chords() {
        // meta+alt+P with shift also down: greppy (3) and plan (3) both match.
        // Tie at score 3 — verify it resolves deterministically to one mode and
        // never panics. (Plan is evaluated first, so it wins the tie.)
        let m = mods(&[Modifier::Meta, Modifier::Alt, Modifier::Shift]);
        let r = resolve_chord(&m, Some(PrintingKey::P));
        assert!(matches!(r, Some(Mode::Plan) | Some(Mode::Greppy)));
    }

    #[test]
    fn plan_without_shift_is_plan_not_greppy() {
        let m = mods(&[Modifier::Meta, Modifier::Alt]);
        assert_eq!(resolve_chord(&m, Some(PrintingKey::P)), Some(Mode::Plan));
    }

    // --- meta+alt alone resolves nothing (shared prefix) -------------------

    #[test]
    fn meta_alt_alone_is_nothing() {
        assert_eq!(
            resolve_chord(&mods(&[Modifier::Meta, Modifier::Alt]), None),
            None
        );
    }

    #[test]
    fn no_modifiers_is_nothing() {
        assert_eq!(resolve_chord(&mods(&[]), None), None);
        assert_eq!(resolve_chord(&mods(&[]), Some(PrintingKey::P)), None);
    }

    #[test]
    fn single_modifier_is_nothing() {
        assert_eq!(resolve_chord(&mods(&[Modifier::Shift]), None), None);
        assert_eq!(resolve_chord(&mods(&[Modifier::Ctrl]), None), None);
        assert_eq!(resolve_chord(&mods(&[Modifier::Meta]), None), None);
    }

    #[test]
    fn p_without_meta_alt_is_nothing() {
        // A stray P with only one of the prefix modifiers must not fire Plan.
        assert_eq!(
            resolve_chord(&mods(&[Modifier::Meta]), Some(PrintingKey::P)),
            None
        );
        assert_eq!(
            resolve_chord(&mods(&[Modifier::Alt]), Some(PrintingKey::P)),
            None
        );
    }

    // --- state machine: start on modifier change, not stray key (cf a) -----

    #[test]
    fn modifier_only_chord_starts_on_modifier_press() {
        let mut sm = StateMachine::new();
        assert!(sm.on_modifier_change(Modifier::Ctrl, true).is_empty());
        let ev = sm.on_modifier_change(Modifier::Shift, true);
        assert_eq!(ev, vec![HotkeyEvent::Start(Mode::Transcribe)]);
        assert_eq!(sm.active_mode(), Some(Mode::Transcribe));
    }

    #[test]
    fn stray_printing_key_does_not_start_modifier_chord() {
        // Hold ctrl+shift -> transcribe starts. A subsequent unrelated printing
        // key never reaches on_printing_key (backend filters it), so the only
        // way it could affect us is via on_printing_key — and even routing the
        // *known* printing key P here must NOT start a new chord nor stop the
        // active one (P is irrelevant to transcribe).
        let mut sm = StateMachine::new();
        sm.on_modifier_change(Modifier::Ctrl, true);
        sm.on_modifier_change(Modifier::Shift, true);
        assert_eq!(sm.active_mode(), Some(Mode::Transcribe));

        // Pressing P while ctrl+shift held: resolve_chord still yields
        // Transcribe (P only matters with meta+alt), so no event.
        let ev = sm.on_printing_key(PrintingKey::P, true);
        assert!(ev.is_empty());
        assert_eq!(sm.active_mode(), Some(Mode::Transcribe));
    }

    #[test]
    fn releasing_a_required_modifier_stops() {
        let mut sm = StateMachine::new();
        sm.on_modifier_change(Modifier::Ctrl, true);
        sm.on_modifier_change(Modifier::Shift, true);
        let ev = sm.on_modifier_change(Modifier::Shift, false);
        assert_eq!(ev, vec![HotkeyEvent::Stop(Mode::Transcribe)]);
        assert_eq!(sm.active_mode(), None);
    }

    // --- transition cases: plan <-> greppy over shared meta+alt (cf b) -----

    #[test]
    fn plan_to_greppy_transition_no_double_fire() {
        let mut sm = StateMachine::new();
        // meta+alt+P -> Plan
        sm.on_modifier_change(Modifier::Meta, true);
        sm.on_modifier_change(Modifier::Alt, true);
        let ev = sm.on_printing_key(PrintingKey::P, true);
        assert_eq!(ev, vec![HotkeyEvent::Start(Mode::Plan)]);

        // Release P (Plan ends), press Shift (Greppy begins). Each transition is
        // a clean single event; no double START while meta+alt stays held.
        let ev = sm.on_printing_key(PrintingKey::P, false);
        assert_eq!(ev, vec![HotkeyEvent::Stop(Mode::Plan)]);
        assert_eq!(sm.active_mode(), None);

        let ev = sm.on_modifier_change(Modifier::Shift, true);
        assert_eq!(ev, vec![HotkeyEvent::Start(Mode::Greppy)]);
        assert_eq!(sm.active_mode(), Some(Mode::Greppy));
    }

    #[test]
    fn greppy_to_plan_simultaneous_transition_stops_then_starts() {
        // Edge case: from greppy (meta+alt+shift) the user presses P *before*
        // releasing shift. Now meta+alt+shift+P all held -> resolve tie favors
        // Plan. The machine must STOP greppy then START plan in one step (single
        // active invariant), never leave both "running".
        let mut sm = StateMachine::new();
        sm.on_modifier_change(Modifier::Meta, true);
        sm.on_modifier_change(Modifier::Alt, true);
        sm.on_modifier_change(Modifier::Shift, true);
        assert_eq!(sm.active_mode(), Some(Mode::Greppy));

        let ev = sm.on_printing_key(PrintingKey::P, true);
        assert_eq!(
            ev,
            vec![HotkeyEvent::Stop(Mode::Greppy), HotkeyEvent::Start(Mode::Plan)]
        );
        assert_eq!(sm.active_mode(), Some(Mode::Plan));
    }

    #[test]
    fn holding_same_chord_does_not_refire() {
        // Adding an irrelevant extra modifier that does not change the resolved
        // mode must not re-fire START. ctrl+shift = transcribe; adding meta
        // keeps transcribe as the most-specific *fully-satisfied* chord? No:
        // meta+ctrl+shift still resolves to transcribe (no chord needs exactly
        // those three), so no new event.
        let mut sm = StateMachine::new();
        sm.on_modifier_change(Modifier::Ctrl, true);
        sm.on_modifier_change(Modifier::Shift, true);
        assert_eq!(sm.active_mode(), Some(Mode::Transcribe));
        let ev = sm.on_modifier_change(Modifier::Meta, true);
        assert!(ev.is_empty(), "no refire when resolved mode is unchanged");
        assert_eq!(sm.active_mode(), Some(Mode::Transcribe));
    }

    // --- serde wire form ---------------------------------------------------

    #[test]
    fn mode_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&Mode::Greppy).unwrap(), "\"greppy\"");
        assert_eq!(
            serde_json::to_string(&Mode::Transcribe).unwrap(),
            "\"transcribe\""
        );
    }

    #[test]
    fn hotkey_event_serializes_externally_tagged() {
        let s = serde_json::to_string(&HotkeyEvent::Start(Mode::Plan)).unwrap();
        assert_eq!(s, r#"{"start":"plan"}"#);
        let s = serde_json::to_string(&HotkeyEvent::Stop(Mode::Greppy)).unwrap();
        assert_eq!(s, r#"{"stop":"greppy"}"#);
    }
}
