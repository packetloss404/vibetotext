//! Shared application state, constructed once at startup and `.manage()`d on the
//! Tauri app so every command/event handler can reach it.
//!
//! Ownership note (Phase 0): the concrete `Db` / `Config` types are owned by
//! sibling builders (`db`, `config`). To keep this scaffold compiling and to
//! avoid stepping on those agents' files, `AppState` is intentionally minimal:
//! it currently carries only the resolved app data directory and the
//! foundational locks. Later phases extend it (Pipeline handle, hotkey
//! listener, recorder, whisper model, etc.) by adding fields here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::transcribe::Transcriber;

pub const PIPELINE_PREPARING: u8 = 0;
pub const PIPELINE_IDLE: u8 = 1;
pub const PIPELINE_RECORDING: u8 = 2;
pub const PIPELINE_PROCESSING: u8 = 3;

/// Errors surfaced while bringing up application state.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("could not resolve the user home directory")]
    NoHomeDir,

    #[error("failed to create app data directory at {path}: {source}")]
    CreateDataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Process-wide application state managed by Tauri.
///
/// All fields are cheaply cloneable handles (`Arc<…>`) so the struct itself can
/// be `Clone` and handed to background tasks. The `Mutex`-guarded slots will be
/// populated by later phases.
#[derive(Clone)]
pub struct AppState {
    /// Resolved data root, i.e. `~/.vibetotext`. All other paths (db, models,
    /// config.json, .env) hang off this directory.
    pub data_dir: Arc<PathBuf>,

    /// Inner mutable state. Phase 0 keeps this empty-but-present so later phases
    /// have a single, lock-once place to add the Pipeline handle and friends
    /// without changing the `manage()` call site.
    pub inner: Arc<Mutex<Inner>>,

    /// Lightweight runtime status shared with the hotkey admission gate and UI.
    pub pipeline_phase: Arc<AtomicU8>,
}

/// Mutable, lock-guarded portion of [`AppState`].
///
/// Holds the shared whisper model used by both the capture pipeline and the
/// `local-api` endpoint. Built lazily on first use, rebuilt when the user
/// changes the configured model name. The `Arc<Transcriber>` clone is cheap
/// (one refcount bump); inference serializes inside the `Transcriber`'s own
/// internal mutex (whisper.cpp is not reentrant).
#[derive(Default)]
pub struct Inner {
    /// Shared whisper model used by both the capture pipeline and the
    /// `local-api` HTTP endpoint. `None` until the first call to
    /// [`AppState::ensure_transcriber`].
    pub transcriber: Option<Arc<Transcriber>>,
    /// Name of the model the current `transcriber` was built for, so a config
    /// change can trigger a rebuild on the next call.
    pub transcriber_model: Option<String>,
}

impl AppState {
    /// Build application state.
    ///
    /// Resolves `~/.vibetotext`, ensures it exists, and constructs the empty
    /// inner state. The db/config builders consume `data_dir()` to locate
    /// `history.db` and `config.json`; wiring those in happens once their
    /// constructors exist (this scaffold purposefully does not reach into the
    /// `db`/`config` modules to avoid co-editing files another agent owns).
    pub fn new() -> Result<Self, StateError> {
        let data_dir = Self::resolve_data_dir()?;

        std::fs::create_dir_all(&data_dir).map_err(|source| StateError::CreateDataDir {
            path: data_dir.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |source| StateError::CreateDataDir {
                    path: data_dir.clone(),
                    source,
                },
            )?;
        }

        tracing::info!(data_dir = %data_dir.display(), "app data directory ready");

        Ok(Self {
            data_dir: Arc::new(data_dir),
            inner: Arc::new(Mutex::new(Inner::default())),
            pipeline_phase: Arc::new(AtomicU8::new(PIPELINE_PREPARING)),
        })
    }

    /// `~/.vibetotext` — the canonical data root from the migration plan.
    fn resolve_data_dir() -> Result<PathBuf, StateError> {
        let home = dirs::home_dir().ok_or(StateError::NoHomeDir)?;
        Ok(home.join(".vibetotext"))
    }

    /// Accessor for the resolved data directory.
    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// `~/.vibetotext/history.db`
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("history.db")
    }

    /// `~/.vibetotext/config.json`
    pub fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }

    /// `~/.vibetotext/models` — ggml whisper models live here.
    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("models")
    }

    pub fn pipeline_phase_name(&self) -> &'static str {
        match self.pipeline_phase.load(Ordering::Acquire) {
            PIPELINE_PREPARING => "preparing_model",
            PIPELINE_IDLE => "ready",
            PIPELINE_RECORDING => "recording",
            PIPELINE_PROCESSING => "processing",
            _ => "unknown",
        }
    }

    /// Snapshot of the currently-loaded shared transcriber, if any.
    ///
    /// Returns `None` until [`ensure_transcriber`](Self::ensure_transcriber) has
    /// been called at least once. The clone is cheap (refcount bump); the inner
    /// `Mutex` is released as soon as we hold the `Arc`.
    pub fn transcriber(&self) -> Option<Arc<Transcriber>> {
        self.inner
            .lock()
            .ok()
            .and_then(|guard| guard.transcriber.clone())
    }

    /// Get-or-create the shared whisper transcriber for `model`, rebuilding when
    /// the configured model name changes.
    ///
    /// The first call per model downloads the ggml file on demand and loads the
    /// `WhisperContext` (multi-second cost); subsequent calls are a mutex lock
    /// plus an `Option::clone` plus a return. Multiple callers race-safe via
    /// the AppState inner mutex; last writer wins, but every winner holds the
    /// same `Arc` because the model hasn't changed.
    pub fn ensure_transcriber(&self, model: &str) -> Result<Arc<Transcriber>> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("app state mutex poisoned"))?;
        if guard.transcriber.is_none() || guard.transcriber_model.as_deref() != Some(model) {
            let model_path = crate::models::resolve_or_download(model)
                .with_context(|| format!("resolving whisper model '{model}'"))?;
            let transcriber = Transcriber::new(&model_path)
                .with_context(|| format!("creating transcriber for model '{model}'"))?;
            guard.transcriber = Some(Arc::new(transcriber));
            guard.transcriber_model = Some(model.to_string());
        }
        Ok(guard
            .transcriber
            .as_ref()
            .expect("transcriber just set")
            .clone())
    }
}
