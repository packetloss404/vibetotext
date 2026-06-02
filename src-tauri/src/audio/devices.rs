//! Input audio device enumeration (Phase 2).
//!
//! Backs the `list_audio_devices` command (migration plan §5), replacing the old
//! Electron `navigator.mediaDevices.enumerateDevices()`. Uses cpal's default host
//! `input_devices()` iterator — the same 0.17 API exercised in
//! `app/spikes/cpal_spike/src/main.rs`.
//!
//! ## Device-index semantics (risk #5)
//! The returned `index` is the **host enumeration index** (position in
//! `host.input_devices()`), which is what `config.audio_device_index` stores. This is
//! distinct from the web `audio_device_id`; the recorder reconciles the two when it
//! opens a stream. The index is best-effort stable for a given device set but can
//! shift if devices are added/removed — callers should also match on `name`.

use serde::Serialize;

use cpal::traits::{DeviceTrait, HostTrait};

/// A selectable input (capture) device.
#[derive(Debug, Clone, Serialize)]
pub struct AudioDevice {
    /// Host enumeration index (position in `host.input_devices()`); maps to
    /// `config.audio_device_index`.
    pub index: i64,
    /// Human-readable device name.
    pub name: String,
    /// Whether this is the host's default input device.
    pub is_default: bool,
}

/// Best-effort human-readable device name via cpal 0.17's `description()` API
/// (matches the cpal spike). Falls back to a placeholder if the backend can't
/// report a name.
fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

/// Enumerate the default host's input devices.
///
/// Returns an empty vec (never panics) if the host can't enumerate — a headless /
/// device-less box is a valid runtime state, mirroring the spike's graceful path.
pub fn list_input_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();

    // Name of the default input device, used to flag the matching entry.
    let default_name = host.default_input_device().map(|d| device_name(&d));

    let devices = match host.input_devices() {
        Ok(devices) => devices,
        Err(e) => {
            tracing::warn!("could not enumerate input devices: {e}");
            return Vec::new();
        }
    };

    devices
        .enumerate()
        .map(|(i, dev)| {
            let name = device_name(&dev);
            let is_default = default_name.as_deref() == Some(name.as_str());
            AudioDevice {
                index: i as i64,
                name,
                is_default,
            }
        })
        .collect()
}
