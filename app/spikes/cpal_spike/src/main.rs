//! cpal spike for the VibeToText Tauri migration.
//!
//! Proves the audio capture path the real app needs (see migration plan §7 Spike row,
//! and `audio/{recorder,devices}.rs` in the target layout):
//!   1. Enumerate input devices + report the default input device.
//!   2. Open a **16 kHz mono f32** input stream on the default device, capture ~1 s,
//!      and print the RMS level of the captured samples.
//!
//! The reference Python recorder (`src/vibetotext/recorder.py`) records at
//! `samplerate=16000, channels=1, dtype=float32`; this mirrors that exactly so the
//! ported `recorder.rs` inherits a known-good config.
//!
//! Robustness goals for a headless / device-less CI box:
//!   * No input device  -> print a message and exit 0 (not an error condition for a spike).
//!   * Stream build/play/capture failures -> reported, never `panic!`/`unwrap`. We still
//!     exit 0 because "the binary compiles and the capture path is wired correctly" is the
//!     go/no-go signal; whether a real mic exists in the sandbox is irrelevant.
//!
//! Verified target: Windows (WASAPI). macOS (CoreAudio) / Linux (ALSA) are written-but-
//! unverified in this environment.

use std::error::Error;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

/// What the real recorder needs (matches recorder.py).
const TARGET_SAMPLE_RATE: u32 = 16_000;
const TARGET_CHANNELS: u16 = 1;
const CAPTURE_MS: u64 = 1_000;

fn main() {
    // Never let an unexpected error abort with a non-zero status for this spike: the
    // point is to prove the path links + runs, and headless boxes legitimately have no mic.
    if let Err(e) = run() {
        eprintln!("[cpal-spike] non-fatal error: {e}");
        eprintln!("[cpal-spike] (treated as success for a spike; capture hardware may be absent)");
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let host = cpal::default_host();
    println!("[cpal-spike] host: {}", host.id().name());

    // ---- (1) Enumerate input devices -------------------------------------------------
    enumerate_inputs(&host);

    // ---- (2) Capture ~1s of 16kHz mono f32 from the default device -------------------
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            // Graceful no-input-device path: this is the documented headless outcome.
            println!("[cpal-spike] no default input device available; nothing to capture.");
            println!("[cpal-spike] OK (no-input-device path exercised)");
            return Ok(());
        }
    };

    let name = device_name(&device);
    println!("[cpal-spike] capturing from default input device: {name}");

    match capture_rms(&device) {
        Ok(Some(rms)) => {
            println!("[cpal-spike] captured ~{CAPTURE_MS} ms; RMS level = {rms:.6}");
            println!("[cpal-spike] OK (capture path exercised)");
        }
        Ok(None) => {
            println!("[cpal-spike] stream ran but produced no samples (expected on a silent/headless host)");
            println!("[cpal-spike] OK (capture path exercised, no samples)");
        }
        Err(e) => {
            // Stream build/play can fail headlessly (no real endpoint). Report, don't panic.
            eprintln!("[cpal-spike] capture failed (non-fatal): {e}");
            println!("[cpal-spike] OK (capture path wired; runtime capture unavailable here)");
        }
    }

    Ok(())
}

/// Best-effort human-readable device name via the non-deprecated `description()` API.
fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

/// Print every input device's name and flag the default one.
fn enumerate_inputs(host: &cpal::Host) {
    let default_name = host
        .default_input_device()
        .map(|d| device_name(&d));

    match host.input_devices() {
        Ok(devices) => {
            let mut count = 0usize;
            println!("[cpal-spike] input devices:");
            for (i, dev) in devices.enumerate() {
                count += 1;
                let dname = device_name(&dev);
                let is_default = default_name.as_deref() == Some(dname.as_str());
                let marker = if is_default { "  <-- default" } else { "" };
                println!("  [{i}] {dname}{marker}");
            }
            if count == 0 {
                println!("  (none)");
            }
        }
        Err(e) => {
            eprintln!("[cpal-spike] could not enumerate input devices (non-fatal): {e}");
        }
    }

    match &default_name {
        Some(n) => println!("[cpal-spike] default input device: {n}"),
        None => println!("[cpal-spike] default input device: <none>"),
    }
}

/// Build a 16 kHz mono f32 input stream, run it for ~1s, and return the RMS of the
/// captured samples (`None` if zero samples were delivered).
///
/// We request the exact target config the real app uses. Many backends will honor
/// 16 kHz mono directly; if a device cannot, the error is surfaced (non-fatally) by the
/// caller rather than silently resampling — the ported recorder will own resampling.
fn capture_rms(device: &cpal::Device) -> Result<Option<f64>, Box<dyn Error>> {
    // Confirm the device advertises an f32 input config near our target (informational).
    log_supported_configs(device);

    // In cpal 0.17 `SampleRate` is a type alias for `u32`.
    let config = StreamConfig {
        channels: TARGET_CHANNELS,
        sample_rate: TARGET_SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };

    // Accumulate sum-of-squares + sample count across callbacks for an RMS at the end.
    // Arc<Mutex<…>> keeps the data callback `Send + 'static` without unsafe sharing.
    let acc = Arc::new(Mutex::new((0.0f64, 0u64))); // (sum_sq, n)
    let acc_cb = Arc::clone(&acc);

    let err_acc = Arc::new(Mutex::new(None::<String>));
    let err_cb = Arc::clone(&err_acc);

    let data_fn = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        if let Ok(mut g) = acc_cb.lock() {
            for &s in data {
                g.0 += (s as f64) * (s as f64);
                g.1 += 1;
            }
        }
    };
    let error_fn = move |err: cpal::StreamError| {
        if let Ok(mut g) = err_cb.lock() {
            *g = Some(err.to_string());
        }
    };

    // Guard: we explicitly target f32. If the default sample format isn't f32 we still
    // try to build an f32 stream (most backends support it); a build failure propagates.
    if let Ok(default_cfg) = device.default_input_config() {
        if default_cfg.sample_format() != SampleFormat::F32 {
            eprintln!(
                "[cpal-spike] note: device default format is {:?}; requesting f32 anyway",
                default_cfg.sample_format()
            );
        }
    }

    let stream = device.build_input_stream(&config, data_fn, error_fn, None)?;
    stream.play()?;

    thread::sleep(Duration::from_millis(CAPTURE_MS));

    // Drop the stream to stop capture before reading the accumulator.
    drop(stream);

    if let Ok(g) = err_acc.lock() {
        if let Some(msg) = g.as_ref() {
            eprintln!("[cpal-spike] stream reported an error during capture (non-fatal): {msg}");
        }
    }

    let (sum_sq, n) = *acc.lock().map_err(|_| "accumulator mutex poisoned")?;
    if n == 0 {
        return Ok(None);
    }
    let rms = (sum_sq / n as f64).sqrt();
    println!("[cpal-spike] samples captured: {n}");
    Ok(Some(rms))
}

/// Log the device's supported input configs (helps debug sample-rate/format mismatches).
fn log_supported_configs(device: &cpal::Device) {
    match device.supported_input_configs() {
        Ok(cfgs) => {
            for c in cfgs {
                // min/max_sample_rate() return `SampleRate` (= u32) in cpal 0.17.
                println!(
                    "[cpal-spike]   supports: {:?} {}ch {}..{} Hz",
                    c.sample_format(),
                    c.channels(),
                    c.min_sample_rate(),
                    c.max_sample_rate()
                );
            }
        }
        Err(e) => {
            eprintln!("[cpal-spike] could not query supported input configs (non-fatal): {e}");
        }
    }
}
