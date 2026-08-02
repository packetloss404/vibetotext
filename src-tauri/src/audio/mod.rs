//! Audio capture + visualization (Phase 2).
//!
//! Submodules (per migration plan §4 target layout, `audio/{recorder,waveform,devices}.rs`):
//! - [`recorder`]: cpal input stream → 16 kHz mono f32 buffers (owns resampling for
//!   device-native rates; matches `src/vibetotext/recorder.py`). Authored by a sibling builder.
//! - [`waveform`]: rustfft band-mapping → 25 bars (visual parity with `recorder.py`).
//!   Authored by a sibling builder.
//! - [`devices`]: cpal input-device enumeration for the `list_audio_devices` command.
//!
//! This Phase-2 wiring agent owns only [`devices`]; `recorder` and `waveform` bodies
//! are owned by other builders. The module declarations are present so the crate links
//! once all three land (the Verify agent runs the authoritative build).

pub mod devices;
pub mod recorder;
pub mod waveform;
