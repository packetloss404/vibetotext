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

    // On Windows, cpal reports generic short names ("Headset", "Microphone") that
    // are ambiguous when several mics are present. Resolve the full WASAPI endpoint
    // labels ("Headset (PLT Focus)") by index — the WASAPI capture-endpoint order
    // matches cpal's input_devices() order. Empty on non-Windows / failure.
    let full_names = full_endpoint_names();

    devices
        .enumerate()
        .map(|(i, dev)| {
            let cpal_name = device_name(&dev);
            // is_default is matched on cpal's own naming (default_input_device
            // reports the same short name), before we overlay the full label.
            let is_default = default_name.as_deref() == Some(cpal_name.as_str());
            let name = full_names
                .get(i)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or(cpal_name);
            AudioDevice {
                index: i as i64,
                name,
                is_default,
            }
        })
        .collect()
}

/// Full friendly names of active WASAPI capture endpoints, in
/// `IMMDeviceCollection` order — which matches cpal's `input_devices()` order on
/// the WASAPI host, so the returned vec is index-aligned with the cpal list.
/// Returns an empty vec on any failure (callers fall back to cpal's short name).
#[cfg(windows)]
fn full_endpoint_names() -> Vec<String> {
    use windows::core::PWSTR;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::{
        eCapture, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };

    let mut out = Vec::new();
    // SAFETY: standard WASAPI device-enumeration COM sequence. Every fallible call
    // is checked and degrades to the cpal name; no raw pointer escapes the block.
    unsafe {
        // Ignore the result: a prior STA init on this thread returns RPC_E_CHANGED_MODE
        // but COM is still usable for CoCreateInstance below.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!("WASAPI enumerator unavailable: {e}");
                    return out;
                }
            };
        let collection = match enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("EnumAudioEndpoints failed: {e}");
                return out;
            }
        };
        let count = collection.GetCount().unwrap_or(0);
        for i in 0..count {
            let name = (|| -> Option<String> {
                let dev = collection.Item(i).ok()?;
                let store = dev.OpenPropertyStore(STGM_READ).ok()?;
                let pv = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
                let p: PWSTR = pv.Anonymous.Anonymous.Anonymous.pwszVal;
                if p.is_null() {
                    None
                } else {
                    p.to_string().ok()
                }
            })()
            .unwrap_or_default();
            out.push(name);
        }
    }
    out
}

#[cfg(not(windows))]
fn full_endpoint_names() -> Vec<String> {
    Vec::new()
}
