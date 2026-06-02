//! ggml Whisper model resolution + download-on-first-run.
//!
//! Port of the model-download logic in `src/vibetotext/transcriber.py` and the
//! C# `WhisperTranscriber.EnsureModelAsync`. Models are stored, uniformly across
//! all platforms, at `~/.vibetotext/models/ggml-<model>.bin` (the ggml format
//! unifies the three old bindings — see plan §6). If the file is absent it is
//! downloaded from the canonical Hugging Face `ggerganov/whisper.cpp` repo.
//!
//! Downloads are **atomic**: bytes are written to a `*.downloading` temp file
//! and renamed into place only on success, so an interrupted download never
//! leaves a partial model that whisper would fail to load (matching the C# temp
//! + `File.Move` pattern).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Base URL for the canonical ggml Whisper models on Hugging Face.
const HF_GGML_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Resolve the local path to `ggml-<model>.bin`, downloading it on first run.
///
/// Returns `~/.vibetotext/models/ggml-<model>.bin`. If the file already exists it
/// is returned immediately; otherwise it is downloaded from the Hugging Face
/// ggml repo to a temp file and atomically renamed into place.
///
/// `model` is a size/name token such as `tiny`, `base`, `small`, `medium`,
/// `large-v3`, or `large-v3-turbo` — used verbatim in the `ggml-<model>.bin`
/// filename and URL.
pub fn resolve_or_download(model: &str) -> Result<PathBuf> {
    let models_dir = models_dir()?;
    fs::create_dir_all(&models_dir).with_context(|| {
        format!("failed to create models directory: {}", models_dir.display())
    })?;

    let file_name = format!("ggml-{model}.bin");
    let model_path = models_dir.join(&file_name);

    if model_path.is_file() {
        tracing::info!(model = %model, path = %model_path.display(), "whisper model present");
        return Ok(model_path);
    }

    let temp_path = models_dir.join(format!("{file_name}.downloading"));
    // Clean up any leftover partial download from a previous interrupted run.
    if temp_path.exists() {
        tracing::warn!(path = %temp_path.display(), "removing interrupted model download");
        let _ = fs::remove_file(&temp_path);
    }

    let url = format!("{HF_GGML_BASE}/{file_name}");
    tracing::info!(model = %model, %url, "downloading whisper model");

    download_to_file(&url, &temp_path).with_context(|| {
        // Best-effort cleanup so a failed download can be cleanly retried.
        let _ = fs::remove_file(&temp_path);
        format!("failed to download whisper model '{model}' from {url}")
    })?;

    // Atomic publish: rename the completed temp file into the final location.
    fs::rename(&temp_path, &model_path).with_context(|| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "failed to move downloaded model into place: {} -> {}",
            temp_path.display(),
            model_path.display()
        )
    })?;

    let size_mb = fs::metadata(&model_path).map(|m| m.len() / 1024 / 1024).unwrap_or(0);
    tracing::info!(model = %model, path = %model_path.display(), size_mb, "whisper model downloaded");

    Ok(model_path)
}

/// `~/.vibetotext/models`.
fn models_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".vibetotext").join("models"))
}

/// Download `url` into `dest` (blocking). The model files are large (tens of MB
/// to >1 GB), so the response is streamed to disk rather than buffered in RAM.
fn download_to_file(url: &str, dest: &std::path::Path) -> Result<()> {
    let mut resp = reqwest::blocking::Client::builder()
        // Bound connect, and cap the whole transfer at 30 min, so a dead/stalled
        // socket can't wedge the caller forever (the pipeline worker calls this
        // synchronously). 30 min is generous even for the ~1 GB large models on a
        // slow link, while still guaranteeing the worker eventually unblocks.
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .context("failed to build HTTP client")?
        .get(url)
        .send()
        .context("HTTP request failed")?;

    if !resp.status().is_success() {
        anyhow::bail!("download returned HTTP {}", resp.status());
    }

    let mut file = fs::File::create(dest)
        .with_context(|| format!("failed to create temp file: {}", dest.display()))?;
    std::io::copy(&mut resp, &mut file)
        .context("failed to stream model bytes to disk")?;
    file.sync_all().context("failed to flush model to disk")?;

    Ok(())
}
