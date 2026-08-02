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
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};

/// Base URL for the canonical ggml Whisper models on Hugging Face.
const HF_GGML_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
static MODEL_RESOLUTION_LOCK: Mutex<()> = Mutex::new(());

/// Model tokens published by the canonical whisper.cpp model repository.
pub const SUPPORTED_MODELS: &[&str] = &[
    "tiny",
    "base",
    "small",
    "medium",
    "large-v1",
    "large-v2",
    "large-v3",
    "large-v3-turbo",
];

#[derive(Clone, Copy)]
struct ModelMetadata {
    name: &'static str,
    size: u64,
    sha1: &'static str,
}

// Published artifact metadata from the canonical whisper.cpp model repository.
// Pinning both length and digest prevents a truncated, substituted, or corrupt
// model from being handed to whisper.cpp.
const MODEL_METADATA: &[ModelMetadata] = &[
    ModelMetadata {
        name: "tiny",
        size: 77_691_713,
        sha1: "bd577a113a864445d4c299885e0cb97d4ba92b5f",
    },
    ModelMetadata {
        name: "base",
        size: 147_951_465,
        sha1: "465707469ff3a37a2b9b8d8f89f2f99de7299dac",
    },
    ModelMetadata {
        name: "small",
        size: 487_601_967,
        sha1: "55356645c2b361a969dfd0ef2c5a50d530afd8d5",
    },
    ModelMetadata {
        name: "medium",
        size: 1_533_763_059,
        sha1: "fd9727b6e1217c2f614f9b698455c4ffd82463b4",
    },
    ModelMetadata {
        name: "large-v1",
        size: 3_094_623_691,
        sha1: "b1caaf735c4cc1429223d5a74f0f4d0b9b59a299",
    },
    ModelMetadata {
        name: "large-v2",
        size: 3_094_623_691,
        sha1: "0f4c8e34f21cf1a914c59d8b3ce882345ad349d6",
    },
    ModelMetadata {
        name: "large-v3",
        size: 3_095_033_483,
        sha1: "ad82bf6a9043ceed055076d0fd39f5f186ff8062",
    },
    ModelMetadata {
        name: "large-v3-turbo",
        size: 1_624_555_275,
        sha1: "4af2b29d7ec73d781377bfd1758ca957a807e941",
    },
];

fn metadata_for(model: &str) -> Result<ModelMetadata> {
    MODEL_METADATA
        .iter()
        .copied()
        .find(|metadata| metadata.name == model)
        .with_context(|| format!("missing integrity metadata for Whisper model '{model}'"))
}

/// Validate a configured model token and translate the legacy OpenAI Whisper
/// `large` name to whisper.cpp's current `large-v3` artifact.
pub fn normalize_model_name(model: &str) -> Result<&str> {
    let model = model.trim();
    let model = if model == "large" { "large-v3" } else { model };
    if SUPPORTED_MODELS.contains(&model) {
        Ok(model)
    } else {
        anyhow::bail!(
            "unsupported Whisper model '{model}'; choose one of: {}",
            SUPPORTED_MODELS.join(", ")
        )
    }
}

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
    let model = normalize_model_name(model)?;
    let _guard = MODEL_RESOLUTION_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Whisper model resolution lock poisoned"))?;
    let metadata = metadata_for(model)?;
    let models_dir = models_dir()?;
    fs::create_dir_all(&models_dir).with_context(|| {
        format!(
            "failed to create models directory: {}",
            models_dir.display()
        )
    })?;

    let file_name = format!("ggml-{model}.bin");
    let model_path = models_dir.join(&file_name);

    if model_path.is_file() {
        if verify_cached_model(&model_path, metadata)? {
            tracing::info!(model = %model, path = %model_path.display(), "verified whisper model present");
            return Ok(model_path);
        }

        tracing::warn!(model = %model, path = %model_path.display(), "removing invalid cached whisper model");
        fs::remove_file(&model_path)
            .with_context(|| format!("failed to remove invalid model: {}", model_path.display()))?;
        let _ = fs::remove_file(checksum_path(&model_path));
    }

    let temp_path = models_dir.join(format!("{file_name}.downloading"));
    // Clean up any leftover partial download from a previous interrupted run.
    if temp_path.exists() {
        tracing::warn!(path = %temp_path.display(), "removing interrupted model download");
        let _ = fs::remove_file(&temp_path);
    }

    let url = format!("{HF_GGML_BASE}/{file_name}");
    tracing::info!(model = %model, %url, "downloading whisper model");

    download_to_file(&url, &temp_path, metadata).with_context(|| {
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
    write_checksum_marker(&model_path, metadata)?;

    let size_mb = fs::metadata(&model_path)
        .map(|m| m.len() / 1024 / 1024)
        .unwrap_or(0);
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
fn download_to_file(url: &str, dest: &std::path::Path, metadata: ModelMetadata) -> Result<()> {
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

    if let Some(content_length) = resp.content_length() {
        if content_length != metadata.size {
            anyhow::bail!(
                "unexpected model size from server: expected {} bytes, got {content_length}",
                metadata.size
            );
        }
    }

    let mut file = fs::File::create(dest)
        .with_context(|| format!("failed to create temp file: {}", dest.display()))?;
    let mut hasher = Sha1::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = resp
            .read(&mut buffer)
            .context("failed to read model response")?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > metadata.size {
            anyhow::bail!(
                "model download exceeded expected size of {} bytes",
                metadata.size
            );
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .context("failed to stream model bytes to disk")?;
    }
    file.sync_all().context("failed to flush model to disk")?;

    if total != metadata.size {
        anyhow::bail!(
            "incomplete model download: expected {} bytes, got {total}",
            metadata.size
        );
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != metadata.sha1 {
        anyhow::bail!(
            "model checksum mismatch: expected {}, got {digest}",
            metadata.sha1
        );
    }

    Ok(())
}

fn checksum_path(model_path: &std::path::Path) -> PathBuf {
    let mut marker = model_path.as_os_str().to_owned();
    marker.push(".sha1");
    PathBuf::from(marker)
}

fn verify_cached_model(path: &std::path::Path, metadata: ModelMetadata) -> Result<bool> {
    if fs::metadata(path).map(|entry| entry.len()).unwrap_or(0) != metadata.size {
        return Ok(false);
    }

    let marker = checksum_path(path);
    if fs::read_to_string(&marker)
        .ok()
        .is_some_and(|digest| digest.trim() == metadata.sha1)
    {
        return Ok(true);
    }

    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open cached model: {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("failed to hash cached model")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let valid = format!("{:x}", hasher.finalize()) == metadata.sha1;
    if valid {
        write_checksum_marker(path, metadata)?;
    }
    Ok(valid)
}

fn write_checksum_marker(path: &std::path::Path, metadata: ModelMetadata) -> Result<()> {
    let marker = checksum_path(path);
    let mut file = atomic_write_file::AtomicWriteFile::open(&marker).with_context(|| {
        format!(
            "failed to create model checksum marker: {}",
            marker.display()
        )
    })?;
    file.write_all(metadata.sha1.as_bytes())
        .context("failed to write model checksum marker")?;
    file.commit()
        .context("failed to publish model checksum marker")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_published_model_names() {
        for model in SUPPORTED_MODELS {
            assert_eq!(normalize_model_name(model).unwrap(), *model);
        }
    }

    #[test]
    fn maps_legacy_large_to_large_v3() {
        assert_eq!(normalize_model_name("large").unwrap(), "large-v3");
    }

    #[test]
    fn rejects_paths_and_unknown_models() {
        assert!(normalize_model_name("../config").is_err());
        assert!(normalize_model_name("not-a-model").is_err());
    }

    #[test]
    fn every_supported_model_has_integrity_metadata() {
        for model in SUPPORTED_MODELS {
            let metadata = metadata_for(model).unwrap();
            assert!(metadata.size > 0);
            assert_eq!(metadata.sha1.len(), 40);
        }
    }
}
