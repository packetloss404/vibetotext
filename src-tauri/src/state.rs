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
use std::sync::{Arc, Mutex};

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
}

/// Mutable, lock-guarded portion of [`AppState`].
///
/// Deliberately empty in Phase 0. Future fields (examples, not yet wired):
/// `pipeline: Option<PipelineHandle>`, `current_model: Option<String>`,
/// `recording: bool`.
#[derive(Default)]
pub struct Inner {}

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

        tracing::info!(data_dir = %data_dir.display(), "app data directory ready");

        Ok(Self {
            data_dir: Arc::new(data_dir),
            inner: Arc::new(Mutex::new(Inner::default())),
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
}
