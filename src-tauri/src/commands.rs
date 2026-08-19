//! Tauri command surface (frontend → Rust via `invoke`).
//!
//! Phase 1 contract (migration plan §5). Each command maps one coupling point in
//! the old Electron `history-app/renderer.js` (which reached straight into
//! `better-sqlite3` / `fs`) onto an IPC call.
//!
//! ## Error handling
//! Commands return `Result<T, String>`: the `Err` string is what `invoke()`'s
//! promise rejects with on the JS side. The internal `DbError` / `ConfigError`
//! enums are stringified at this boundary so the frontend gets a readable message
//! without leaking Rust types across IPC.
//!
//! ## State access
//! History commands open the [`Db`] on demand from
//! [`AppState::db_path`](crate::state::AppState::db_path). `Db::open` runs the
//! idempotent, `user_version`-gated migration, so per-call opens are cheap after
//! the first and need no shared-mutable handle. Config commands load/mutate/save
//! [`AppConfig`] straight from disk (the contract's "simplest, no
//! shared-mutable-state" path) — and because [`AppConfig`] preserves unknown keys
//! via `#[serde(flatten)]`, a focused setter never clobbers other writers' keys.

use serde_json::Value;
use std::sync::{Mutex, MutexGuard};
use tauri::State;

use crate::audio::devices::{list_input_devices, AudioDevice};
use crate::config::AppConfig;
use crate::db::{Db, Entry, Statistics};
use crate::state::AppState;

static CONFIG_MUTATION_LOCK: Mutex<()> = Mutex::new(());

fn lock_config_mutation() -> Result<MutexGuard<'static, ()>, String> {
    CONFIG_MUTATION_LOCK
        .lock()
        .map_err(|_| "config mutation lock poisoned".to_string())
}

fn cfg_for_frontend(cfg: &AppConfig) -> Result<Value, String> {
    let mut value = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    if let Value::Object(ref mut map) = value {
        // A key stored in config.json must never be reflected into the webview.
        // Environment/.env keys were already private; keep the lowest-precedence
        // config source private as well.
        map.remove("gemini_api_key");
    }
    Ok(value)
}

/// Open the history database from the managed app-data dir.
///
/// `Db::open` is idempotent (migrations gate on `PRAGMA user_version`), so it is
/// safe to call once per command. Errors are stringified for the IPC boundary.
fn open_db(state: &State<'_, AppState>) -> Result<Db, String> {
    Db::open(state.db_path()).map_err(|e| e.to_string())
}

/// Load config from `~/.vibetotext/config.json`, stringifying errors for IPC.
fn load_cfg() -> Result<AppConfig, String> {
    AppConfig::load().map_err(|e| e.to_string())
}

/// Persist config, stringifying errors for IPC.
fn save_cfg(cfg: &AppConfig) -> Result<(), String> {
    cfg.save().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// History (db-backed)
// ---------------------------------------------------------------------------

/// Fetch history entries newest-first.
///
/// Replaces the renderer's direct `SELECT * FROM entries ORDER BY timestamp DESC`.
/// `mode` filters by transcription mode (`transcribe`/`cleanup`/`plan`/`greppy`);
/// `None` or `"all"` returns every mode. `limit` caps the row count of the
/// *filtered* set (the filter is applied in SQL so a request for the 10 most
/// recent "cleanup" entries really returns up to 10, not "up to 10 of any mode
/// then filtered").
#[tauri::command]
pub fn get_entries(
    state: State<'_, AppState>,
    mode: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Entry>, String> {
    let db = open_db(&state)?;
    let mode_filter = match mode.as_deref() {
        None | Some("all") | Some("") => None,
        Some(m) => Some(m),
    };
    db.get_entries(mode_filter, limit)
        .map_err(|e| e.to_string())
}

/// Aggregate statistics over all history (sessions, words, avg WPM, time saved,
/// common/longest words). Replaces the renderer's in-JS stat computation.
///
/// `mode` is accepted for forward-compatibility/parity with `get_entries`; the
/// Phase 0 `Db::get_statistics` aggregates over all rows, so a per-mode breakdown
/// is a documented later refinement (the frontend currently requests "all").
#[tauri::command]
pub fn get_statistics(
    state: State<'_, AppState>,
    _mode: Option<String>,
) -> Result<Statistics, String> {
    let db = open_db(&state)?;
    db.get_statistics().map_err(|e| e.to_string())
}

/// Return the current pipeline readiness/admission state so a newly-loaded
/// dashboard cannot miss the startup status event.
#[tauri::command]
pub fn get_pipeline_status(state: State<'_, AppState>) -> String {
    state.pipeline_phase_name().to_string()
}

/// Delete all history rows. Replaces a renderer that had no real clear path.
#[tauri::command]
pub fn clear_history(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let db = open_db(&state)?;
    db.clear().map_err(|e| e.to_string())?;
    crate::events::emit_history_updated(&app);
    Ok(())
}

// ---------------------------------------------------------------------------
// Config (disk-backed, unknown-key-preserving)
// ---------------------------------------------------------------------------

/// Load the non-secret config as a JSON object. The legacy Gemini key field is
/// stripped before the value crosses into the webview.
#[tauri::command]
pub fn load_config() -> Result<Value, String> {
    let cfg = load_cfg()?;
    cfg_for_frontend(&cfg)
}

/// Enumerate input audio devices via cpal's default host.
///
/// Replaces the old Electron `navigator.mediaDevices.enumerateDevices()`. Returns
/// `[{ index, name, is_default }, ...]` where `index` is the host enumeration index
/// stored in `config.audio_device_index` (risk #5: distinct from the web
/// `audio_device_id`; the recorder reconciles the two). Infallible at the cpal layer
/// (a device-less host yields an empty list), but kept `Result` for IPC-contract
/// stability.
#[tauri::command]
pub fn list_audio_devices() -> Result<Vec<AudioDevice>, String> {
    Ok(list_input_devices())
}

/// Persist the selected microphone (host index + display name). Replaces the
/// renderer's `handleMicChange` config write.
#[tauri::command]
pub fn set_audio_device(
    index: Option<i64>,
    name: Option<String>,
    id: Option<String>,
) -> Result<Value, String> {
    let (canonical_index, canonical_name) = match index {
        Some(index) if index >= 0 => {
            let devices = list_input_devices();
            let supplied_name = name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty());
            let selected = devices
                .iter()
                .find(|device| {
                    device.index == index
                        && match supplied_name {
                            Some(name) => device.name == name,
                            None => true,
                        }
                })
                .or_else(|| {
                    supplied_name.and_then(|name| devices.iter().find(|device| device.name == name))
                })
                .ok_or_else(|| "selected audio device is no longer available".to_string())?;
            (Some(selected.index), selected.name.clone())
        }
        Some(_) => return Err("audio device index cannot be negative".to_string()),
        None => (None, "System default".to_string()),
    };
    if id.as_deref().is_some_and(|id| id.len() > 512) {
        return Err("audio device id is too long".to_string());
    }
    let _guard = lock_config_mutation()?;
    let mut cfg = load_cfg()?;
    cfg.audio_device_index = canonical_index;
    cfg.audio_device_name = Some(canonical_name);
    cfg.audio_device_id = id;
    save_cfg(&cfg)?;
    cfg_for_frontend(&cfg)
}

/// Return the custom whisper-bias dictionary words. Replaces the renderer's
/// `loadDictionary`.
#[tauri::command]
pub fn get_dictionary() -> Result<Vec<String>, String> {
    Ok(load_cfg()?.custom_dictionary)
}

/// Add a word to the custom dictionary (idempotent; no duplicates). Replaces the
/// renderer's `addDictionaryWord`. Returns the updated word list.
#[tauri::command]
pub fn add_word(word: String) -> Result<Vec<String>, String> {
    let word = word.trim().to_string();
    if word.is_empty() {
        return Err("cannot add an empty word".to_string());
    }
    if word.chars().count() > 100 {
        return Err("dictionary entries are limited to 100 characters".to_string());
    }
    let _guard = lock_config_mutation()?;
    let mut cfg = load_cfg()?;
    if !cfg.custom_dictionary.iter().any(|w| w == &word) {
        if cfg.custom_dictionary.len() >= 500 {
            return Err("custom dictionary is limited to 500 entries".to_string());
        }
        cfg.custom_dictionary.push(word);
        save_cfg(&cfg)?;
    }
    Ok(cfg.custom_dictionary)
}

/// Remove a word from the custom dictionary. Replaces the renderer's
/// `dict-word-remove` handler. Returns the updated word list.
#[tauri::command]
pub fn remove_word(word: String) -> Result<Vec<String>, String> {
    let _guard = lock_config_mutation()?;
    let mut cfg = load_cfg()?;
    let before = cfg.custom_dictionary.len();
    cfg.custom_dictionary.retain(|w| w != &word);
    if cfg.custom_dictionary.len() != before {
        save_cfg(&cfg)?;
    }
    Ok(cfg.custom_dictionary)
}

/// Set the ggml whisper model name. Replaces the renderer's `saveWhisperModel`.
///
/// Phase 1 persists the choice; the hot-reload (no restart) wiring lands with the
/// transcribe module in Phase 2.
#[tauri::command]
pub fn set_whisper_model(model: String) -> Result<Value, String> {
    let _guard = lock_config_mutation()?;
    let model = crate::models::normalize_model_name(&model)
        .map_err(|e| e.to_string())?
        .to_string();
    let mut cfg = load_cfg()?;
    cfg.whisper_model = model;
    save_cfg(&cfg)?;
    cfg_for_frontend(&cfg)
}

/// Set the overlay orb anchor — either a preset string (`"bottom-center"`) or an
/// object `{ "x": .., "y": .. }`. Replaces the renderer's `saveOrbPreset` /
/// `saveOrbCustom`. The value is stored raw so both shapes round-trip.
#[tauri::command]
pub fn set_orb_position(position: Value) -> Result<Value, String> {
    let valid = match &position {
        Value::String(preset) => [
            "top-left",
            "top-right",
            "bottom-left",
            "bottom-right",
            "bottom-center",
        ]
        .contains(&preset.as_str()),
        Value::Object(coords) if coords.len() == 2 => {
            let x = coords.get("x").and_then(Value::as_i64);
            let y = coords.get("y").and_then(Value::as_i64);
            x.zip(y).is_some_and(|(x, y)| {
                (-100_000..=100_000).contains(&x) && (-100_000..=100_000).contains(&y)
            })
        }
        _ => false,
    };
    if !valid {
        return Err("invalid orb position".to_string());
    }
    let _guard = lock_config_mutation()?;
    let mut cfg = load_cfg()?;
    cfg.orb_position = Some(position);
    save_cfg(&cfg)?;
    cfg_for_frontend(&cfg)
}

/// Validate and persist the codebase root used by greppy/context search.
#[tauri::command]
pub fn set_codebase_path(path: Option<String>) -> Result<Value, String> {
    let _guard = lock_config_mutation()?;
    let path = path
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());
    let canonical = match path {
        Some(path) => {
            let canonical = std::fs::canonicalize(&path)
                .map_err(|e| format!("codebase path is not accessible: {e}"))?;
            if !canonical.is_dir() {
                return Err("codebase path must be a directory".to_string());
            }
            Some(canonical.to_string_lossy().into_owned())
        }
        None => None,
    };
    let mut cfg = load_cfg()?;
    cfg.codebase_path = canonical;
    save_cfg(&cfg)?;
    cfg_for_frontend(&cfg)
}

/// Return only the source of the effective Gemini key, never the secret itself.
#[tauri::command]
pub fn get_gemini_key_status() -> Result<Option<String>, String> {
    Ok(load_cfg()?.gemini_key_source().map(str::to_string))
}

/// Store or clear the private Gemini key used by cleanup/plan modes.
#[tauri::command]
pub fn set_gemini_api_key(key: Option<String>) -> Result<Option<String>, String> {
    let key = key
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty());
    if let Some(key) = key.as_deref() {
        if key.len() > 512 || key.chars().any(char::is_whitespace) {
            return Err(
                "Gemini API key must be at most 512 characters with no whitespace".to_string(),
            );
        }
    }
    let _guard = lock_config_mutation()?;
    AppConfig::set_managed_gemini_api_key(key.as_deref()).map_err(|e| e.to_string())?;
    let mut cfg = load_cfg()?;
    if cfg.gemini_api_key.take().is_some() {
        save_cfg(&cfg)?;
    }
    Ok(load_cfg()?.gemini_key_source().map(str::to_string))
}
