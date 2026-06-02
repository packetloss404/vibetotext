//! whisper-rs CPU spike for the VibeToText Tauri migration.
//!
//! GO/NO-GO PURPOSE: prove that `whisper-rs` (and its bundled whisper.cpp C/C++
//! sources, compiled via cc/cmake) actually BUILDS and LINKS on this toolchain
//! (Windows + MinGW gcc, no MSVC `cl`) and can run a full CPU transcription.
//! The native C++ build is the genuine risk here (see plan §9 risk #2); the Rust
//! glue below is deliberately small.
//!
//! GPU INTENTIONALLY OFF: this is the CPU-only MVP. The `metal` / `cuda` /
//! `vulkan` cargo features of `whisper-rs` are NOT enabled (Cargo.toml pulls the
//! crate with default features only), and `WhisperContextParameters::use_gpu` is
//! left at its CPU default. GPU acceleration is a deliberate post-MVP opt-in.
//!
//! Usage:
//!     whisper_spike <ggml-model.bin> <audio-16k-mono-f32.wav>
//!
//! - argv[1] = path to a ggml `.bin` whisper model (e.g. ggml-base.en.bin)
//! - argv[2] = path to a 16 kHz mono 32-bit float WAV file
//!
//! If arguments are missing or the files don't exist, it prints a clear usage
//! message and exits 0 — the spike must still COMPILE and LINK even when it
//! can't be run end-to-end (no model/audio available in CI).

use std::path::Path;
use std::process::ExitCode;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const TARGET_SAMPLE_RATE: u32 = 16_000;

fn usage(program: &str) {
    eprintln!(
        "usage: {program} <ggml-model.bin> <audio-16k-mono-f32.wav>\n\n\
         Arguments:\n\
         \x20 <ggml-model.bin>           path to a ggml whisper model (CPU build)\n\
         \x20 <audio-16k-mono-f32.wav>   16 kHz, mono, 32-bit float WAV to transcribe\n\n\
         This is a build/link + transcription smoke test for whisper-rs.\n\
         GPU (metal/cuda/vulkan) is intentionally OFF for the CPU-only MVP."
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let program = args
        .first()
        .map(String::as_str)
        .unwrap_or("whisper_spike");

    // Missing args -> usage + exit 0 (must still build/link without inputs).
    let (model_path, audio_path) = match (args.get(1), args.get(2)) {
        (Some(m), Some(a)) => (m.clone(), a.clone()),
        _ => {
            usage(program);
            return ExitCode::SUCCESS;
        }
    };

    // Missing files -> usage + exit 0.
    if !Path::new(&model_path).is_file() {
        eprintln!("error: model file not found: {model_path}\n");
        usage(program);
        return ExitCode::SUCCESS;
    }
    if !Path::new(&audio_path).is_file() {
        eprintln!("error: audio file not found: {audio_path}\n");
        usage(program);
        return ExitCode::SUCCESS;
    }

    match run(&model_path, &audio_path) {
        Ok(text) => {
            // The joined transcript is the spike's payload on stdout.
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("transcription failed: {e}");
            // Real runtime failure (bad model / bad audio): non-zero so the
            // orchestrator can tell a true failure from the no-args smoke path.
            ExitCode::FAILURE
        }
    }
}

/// Load the model, create state/params, run full CPU transcription, and return
/// the joined segment text.
fn run(model_path: &str, audio_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let audio = read_wav_mono_f32_16k(audio_path)?;

    // CPU-only: WhisperContextParameters::default() leaves use_gpu at its
    // CPU default; no GPU cargo feature is enabled in Cargo.toml.
    let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())?;
    let mut state = ctx.create_state()?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(num_cpu_threads());
    params.set_translate(false);
    params.set_language(Some("en"));
    // Keep stdout clean — the joined transcript is the only thing we print.
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    state.full(params, &audio)?;

    let n_segments = state.full_n_segments();
    let mut out = String::new();
    for i in 0..n_segments {
        if let Some(segment) = state.get_segment(i) {
            out.push_str(segment.to_str_lossy()?.trim());
            out.push(' ');
        }
    }

    Ok(out.trim().to_string())
}

fn num_cpu_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(1)
        .max(1)
}

/// Minimal WAV reader (std-only, so the spike adds no extra Cargo deps).
///
/// Accepts a 16 kHz, mono, 32-bit IEEE-float (WAVE_FORMAT_IEEE_FLOAT, tag 3)
/// PCM WAV — exactly the format the pipeline feeds whisper (`recorder.py`
/// produces f32 mono @ 16 kHz). This is intentionally strict: the spike's job
/// is to feed whisper a buffer it accepts, not to be a general audio decoder.
fn read_wav_mono_f32_16k(path: &str) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }

    let mut audio_format: u16 = 0;
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut data: Option<&[u8]> = None;

    // Walk the chunk list starting after the 12-byte RIFF header.
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(chunk_size)
            .filter(|&e| e <= bytes.len())
            .ok_or("WAV chunk size exceeds file length")?;
        let body = &bytes[body_start..body_end];

        match chunk_id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err("fmt chunk too small".into());
                }
                audio_format = u16::from_le_bytes([body[0], body[1]]);
                channels = u16::from_le_bytes([body[2], body[3]]);
                sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                bits_per_sample = u16::from_le_bytes([body[14], body[15]]);
            }
            b"data" => {
                data = Some(body);
            }
            _ => {}
        }

        // Chunks are word-aligned: bodies of odd length are padded by one byte.
        pos = body_end + (chunk_size & 1);
    }

    // WAVE_FORMAT_IEEE_FLOAT = 3; WAVE_FORMAT_EXTENSIBLE = 0xFFFE may also wrap
    // float, but we keep the spike strict to the format the recorder emits.
    if audio_format != 3 {
        return Err(format!(
            "expected 32-bit float WAV (format tag 3), got format tag {audio_format}"
        )
        .into());
    }
    if bits_per_sample != 32 {
        return Err(format!("expected 32 bits/sample, got {bits_per_sample}").into());
    }
    if channels != 1 {
        return Err(format!("expected mono (1 channel), got {channels} channels").into());
    }
    if sample_rate != TARGET_SAMPLE_RATE {
        return Err(format!(
            "expected {TARGET_SAMPLE_RATE} Hz, got {sample_rate} Hz"
        )
        .into());
    }

    let data = data.ok_or("WAV has no data chunk")?;
    if data.len() % 4 != 0 {
        return Err("data chunk length is not a multiple of 4 (f32)".into());
    }

    let samples = data
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok(samples)
}
