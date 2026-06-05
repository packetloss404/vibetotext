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
use tauri::State;

use crate::audio::devices::{list_input_devices, AudioDevice};
use crate::config::AppConfig;
use crate::db::{Db, Entry, Statistics};
use crate::state::AppState;

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
/// `None` or `"all"` returns every mode. `limit` caps the row count.
#[tauri::command]
pub fn get_entries(
    state: State<'_, AppState>,
    mode: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Entry>, String> {
    let db = open_db(&state)?;
    let entries = db.get_entries(limit).map_err(|e| e.to_string())?;

    let entries = match mode.as_deref() {
        None | Some("all") | Some("") => entries,
        Some(m) => entries.into_iter().filter(|e| e.mode == m).collect(),
    };
    Ok(entries)
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
    mode: Option<String>,
) -> Result<Statistics, String> {
    let _ = mode; // see doc comment: all-mode aggregate for now.
    let db = open_db(&state)?;
    db.get_statistics().map_err(|e| e.to_string())
}

/// Delete all history rows. Replaces a renderer that had no real clear path.
#[tauri::command]
pub fn clear_history(state: State<'_, AppState>) -> Result<(), String> {
    let db = open_db(&state)?;
    db.clear().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Config (disk-backed, unknown-key-preserving)
// ---------------------------------------------------------------------------

/// Load the full config as a JSON object so the frontend sees every key
/// (including unknown/future ones preserved in `extra`). Replaces the renderer's
/// `fs.readFileSync(CONFIG_PATH)`.
#[tauri::command]
pub fn load_config() -> Result<Value, String> {
    let cfg = load_cfg()?;
    serde_json::to_value(&cfg).map_err(|e| e.to_string())
}

/// Merge a partial config object into the on-disk config and save, preserving
/// unknown keys. Replaces the renderer's read-modify-`fs.writeFileSync` cycle.
///
/// The incoming object is shallow-merged onto the current file: keys present in
/// `partial` overwrite, everything else is retained. Round-tripping through
/// [`AppConfig`] keeps known fields typed and stashes anything else in `extra`,
/// so no writer's keys are dropped.
#[tauri::command]
pub fn save_config(config: Value) -> Result<Value, String> {
    let Value::Object(updates) = config else {
        return Err("save_config expects a JSON object".to_string());
    };

    // Start from the current on-disk config as a raw object so we shallow-merge
    // over *all* keys (known + unknown) uniformly.
    let current = load_cfg()?;
    let mut merged = match serde_json::to_value(&current).map_err(|e| e.to_string())? {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    for (k, v) in updates {
        merged.insert(k, v);
    }

    let cfg: AppConfig =
        serde_json::from_value(Value::Object(merged)).map_err(|e| e.to_string())?;
    save_cfg(&cfg)?;
    serde_json::to_value(&cfg).map_err(|e| e.to_string())
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
    let mut cfg = load_cfg()?;
    cfg.audio_device_index = index;
    cfg.audio_device_name = name;
    cfg.audio_device_id = id;
    save_cfg(&cfg)?;
    serde_json::to_value(&cfg).map_err(|e| e.to_string())
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
    let mut cfg = load_cfg()?;
    if !cfg.custom_dictionary.iter().any(|w| w == &word) {
        cfg.custom_dictionary.push(word);
        save_cfg(&cfg)?;
    }
    Ok(cfg.custom_dictionary)
}

/// Remove a word from the custom dictionary. Replaces the renderer's
/// `dict-word-remove` handler. Returns the updated word list.
#[tauri::command]
pub fn remove_word(word: String) -> Result<Vec<String>, String> {
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
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("whisper model name cannot be empty".to_string());
    }
    let mut cfg = load_cfg()?;
    cfg.whisper_model = model;
    save_cfg(&cfg)?;
    serde_json::to_value(&cfg).map_err(|e| e.to_string())
}

/// Set the overlay orb anchor — either a preset string (`"bottom-center"`) or an
/// object `{ "x": .., "y": .. }`. Replaces the renderer's `saveOrbPreset` /
/// `saveOrbCustom`. The value is stored raw so both shapes round-trip.
#[tauri::command]
pub fn set_orb_position(position: Value) -> Result<Value, String> {
    let mut cfg = load_cfg()?;
    cfg.orb_position = Some(position);
    save_cfg(&cfg)?;
    serde_json::to_value(&cfg).map_err(|e| e.to_string())
}
