//! Application configuration: `~/.vibetotext/config.json`.
//!
//! Port of the Python `src/vibetotext/` config handling and the
//! windows-native `ConfigStore.cs` semantics, unified into one schema.
//!
//! ## Schema (snake_case keys)
//! - `audio_device_index`  — host device index (Python `sounddevice`/cpal). `Option<i64>`.
//! - `audio_device_name`   — human-readable device name (display + reconciliation).
//! - `audio_device_id`     — web/`navigator.mediaDevices` id (history-app renderer).
//! - `codebase_path`       — root for greppy/context search.
//! - `custom_dictionary`   — hot-reloaded whisper bias words.
//! - `whisper_model`       — ggml model name (default `"small"`).
//! - `orb_position`        — overlay anchor; EITHER a preset string
//!   (e.g. `"bottom-center"`) OR an object `{ "x": .., "y": .. }`, so it is kept
//!   as a raw `serde_json::Value`.
//! - `gemini_api_key`      — lowest-precedence source for the Gemini key.
//!
//! ## Unknown-key preservation (CRITICAL)
//! Other writers (older native apps, future fields) may add keys we don't model.
//! `#[serde(flatten)]` into an `extra: Map` round-trips them so a save() from
//! this app never drops another writer's keys — matching `ConfigStore.cs`'s
//! `_rawJson` behavior.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default whisper model when the config does not specify one.
pub const DEFAULT_WHISPER_MODEL: &str = "small";

/// Errors surfaced by config load/save.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not resolve the user home directory")]
    NoHomeDir,

    #[error("failed to read config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write config at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to create config directory at {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config at {path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to serialize config: {source}")]
    Serialize {
        #[source]
        source: serde_json::Error,
    },
}

/// The on-disk application configuration.
///
/// Known fields are typed; everything else is preserved verbatim in `extra` so
/// a round-trip (load -> save) never drops keys written by other tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Host audio device index (cpal/sounddevice). `None` => system default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device_index: Option<i64>,

    /// Human-readable device name (display + index reconciliation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device_name: Option<String>,

    /// Web `navigator.mediaDevices` device id (history-app renderer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device_id: Option<String>,

    /// Root path for greppy / code-context search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codebase_path: Option<String>,

    /// Hot-reloaded custom dictionary (whisper bias words).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_dictionary: Vec<String>,

    /// ggml whisper model name. Defaults to [`DEFAULT_WHISPER_MODEL`].
    #[serde(default = "default_whisper_model")]
    pub whisper_model: String,

    /// Overlay orb anchor. Either a preset string (`"bottom-center"`) or an
    /// object `{ "x": .., "y": .. }`; kept raw so both shapes round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orb_position: Option<Value>,

    /// Lowest-precedence Gemini API key source (see [`AppConfig::gemini_api_key`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_api_key: Option<String>,

    /// Unknown / future keys, preserved verbatim across save().
    ///
    /// `BTreeMap` keeps key order stable so saved files diff cleanly.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn default_whisper_model() -> String {
    DEFAULT_WHISPER_MODEL.to_string()
}

impl AppConfig {
    /// `~/.vibetotext/config.json`.
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(Self::data_dir()?.join("config.json"))
    }

    /// `~/.vibetotext`.
    fn data_dir() -> Result<PathBuf, ConfigError> {
        let home = dirs::home_dir().ok_or(ConfigError::NoHomeDir)?;
        Ok(home.join(".vibetotext"))
    }

    /// Load config from the canonical path.
    ///
    /// A missing file yields [`AppConfig::default`] (with `whisper_model` set to
    /// the default) — matching `ConfigStore.cs`, which simply returns without
    /// erroring when the file is absent.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::path()?)
    }

    /// Load config from a specific path (testable seam).
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                if contents.trim().is_empty() {
                    return Ok(Self::with_defaults());
                }
                serde_json::from_str(&contents).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::with_defaults()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Defaults as if loaded from an absent file (ensures `whisper_model` is set
    /// even though `Default` leaves the `String` empty).
    fn with_defaults() -> Self {
        Self {
            whisper_model: default_whisper_model(),
            ..Self::default()
        }
    }

    /// Persist config to the canonical path, creating `~/.vibetotext` if needed.
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    /// Persist config to a specific path (testable seam). Writes pretty JSON
    /// with a trailing newline, matching the C# `WriteIndented` output.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
            secure_directory(parent).map_err(|source| ConfigError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut json = serde_json::to_string_pretty(self)
            .map_err(|source| ConfigError::Serialize { source })?;
        json.push('\n');

        let mut file = atomic_write_file::AtomicWriteFile::open(path).map_err(|source| {
            ConfigError::Write {
                path: path.to_path_buf(),
                source,
            }
        })?;
        file.write_all(json.as_bytes())
            .map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        file.commit().map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                |source| ConfigError::Write {
                    path: path.to_path_buf(),
                    source,
                },
            )?;
        }

        Ok(())
    }

    /// Resolve the Gemini API key by precedence (per migration plan §3):
    /// `GEMINI_API_KEY` env -> `GOOGLE_API_KEY` env -> `~/.vibetotext/.env`
    /// -> `config.json` `gemini_api_key`.
    ///
    /// Empty / whitespace-only values are treated as unset so a stray empty env
    /// var doesn't shadow a real key further down the chain.
    pub fn gemini_api_key(&self) -> Option<String> {
        let dotenv = Self::data_dir().ok().map(|d| d.join(".env"));
        self.resolve_gemini_key(dotenv.as_deref())
    }

    /// Report where the effective Gemini key comes from without exposing it to
    /// the webview or logs.
    pub fn gemini_key_source(&self) -> Option<&'static str> {
        if env_nonempty("GEMINI_API_KEY").is_some() || env_nonempty("GOOGLE_API_KEY").is_some() {
            return Some("environment");
        }
        if Self::data_dir()
            .ok()
            .is_some_and(|dir| dotenv_lookup(&dir.join(".env")).is_some())
        {
            return Some("VibeToText settings");
        }
        self.gemini_api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|_| "legacy config")
    }

    /// Store or clear the key managed by VibeToText in its private `.env`
    /// file. Existing unrelated variables and comments are preserved.
    pub fn set_managed_gemini_api_key(key: Option<&str>) -> Result<(), ConfigError> {
        let dir = Self::data_dir()?;
        std::fs::create_dir_all(&dir).map_err(|source| ConfigError::CreateDir {
            path: dir.clone(),
            source,
        })?;
        secure_directory(&dir).map_err(|source| ConfigError::Write {
            path: dir.clone(),
            source,
        })?;

        let path = dir.join(".env");
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.clone(),
                    source,
                })
            }
        };
        let mut lines: Vec<String> = contents
            .lines()
            .filter(|line| !is_dotenv_assignment(line, "GEMINI_API_KEY"))
            .map(str::to_string)
            .collect();
        if let Some(key) = key {
            lines.push(format!("GEMINI_API_KEY={key}"));
        }
        let mut output = lines.join("\n");
        if !output.is_empty() {
            output.push('\n');
        }

        let mut file = atomic_write_file::AtomicWriteFile::open(&path).map_err(|source| {
            ConfigError::Write {
                path: path.clone(),
                source,
            }
        })?;
        file.write_all(output.as_bytes())
            .map_err(|source| ConfigError::Write {
                path: path.clone(),
                source,
            })?;
        file.commit().map_err(|source| ConfigError::Write {
            path: path.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
                |source| ConfigError::Write {
                    path: path.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }

    /// Move the deprecated `config.json` key into the private `.env` store.
    /// This runs before the pipeline starts, so no concurrent config reader can
    /// observe the two-file migration halfway through.
    pub fn migrate_legacy_gemini_key() -> Result<bool, ConfigError> {
        let mut config = Self::load()?;
        let Some(key) = config
            .gemini_api_key
            .take()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
        else {
            return Ok(false);
        };

        let dotenv_has_key = Self::data_dir()
            .ok()
            .is_some_and(|dir| dotenv_lookup(&dir.join(".env")).is_some());
        if !dotenv_has_key {
            Self::set_managed_gemini_api_key(Some(&key))?;
        }
        config.save()?;
        Ok(true)
    }

    /// Precedence resolution with an explicit `.env` path (testable seam).
    /// Env vars still take precedence over the file and config value.
    fn resolve_gemini_key(&self, dotenv_path: Option<&Path>) -> Option<String> {
        if let Some(k) = env_nonempty("GEMINI_API_KEY") {
            return Some(k);
        }
        if let Some(k) = env_nonempty("GOOGLE_API_KEY") {
            return Some(k);
        }
        if let Some(p) = dotenv_path {
            if let Some(k) = dotenv_lookup(p) {
                return Some(k);
            }
        }
        self.gemini_api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
}

fn is_dotenv_assignment(line: &str, expected_key: &str) -> bool {
    let line = line
        .trim()
        .strip_prefix("export ")
        .unwrap_or(line.trim())
        .trim_start();
    line.split_once('=')
        .is_some_and(|(key, _)| key.trim() == expected_key)
}

fn secure_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Read an env var, returning `None` for missing or blank values.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Minimal `.env` reader: scans for `GEMINI_API_KEY` then `GOOGLE_API_KEY`.
///
/// Supports `KEY=value`, optional `export ` prefix, surrounding single/double
/// quotes, `#` comments, and blank lines. We avoid a dotenv crate dependency to
/// keep this module self-contained and to mirror the narrow lookup the Python
/// app performs.
fn dotenv_lookup(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut found: BTreeMap<String, String> = BTreeMap::new();

    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        if !value.is_empty() {
            found.insert(key.to_string(), value);
        }
    }

    found
        .get("GEMINI_API_KEY")
        .or_else(|| found.get("GOOGLE_API_KEY"))
        .cloned()
}

/// Strip a single matching pair of surrounding quotes.
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that mutate process-wide env vars so they don't race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let unique = format!(
            "vibetotext_cfg_test_{}_{}_{}",
            std::process::id(),
            name,
            // monotonic-ish disambiguator
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(unique);
        p
    }

    #[test]
    fn defaults_when_file_absent() {
        let path = tmp_path("absent").join("config.json");
        assert!(!path.exists());

        let cfg = AppConfig::load_from(&path).expect("load absent");
        assert_eq!(cfg.whisper_model, DEFAULT_WHISPER_MODEL);
        assert_eq!(cfg.audio_device_index, None);
        assert!(cfg.custom_dictionary.is_empty());
        assert!(cfg.extra.is_empty());
    }

    #[test]
    fn recognizes_only_the_managed_dotenv_assignment() {
        assert!(is_dotenv_assignment(
            "GEMINI_API_KEY=value",
            "GEMINI_API_KEY"
        ));
        assert!(is_dotenv_assignment(
            " export GEMINI_API_KEY = value",
            "GEMINI_API_KEY"
        ));
        assert!(!is_dotenv_assignment(
            "GOOGLE_API_KEY=value",
            "GEMINI_API_KEY"
        ));
        assert!(!is_dotenv_assignment(
            "# GEMINI_API_KEY=value",
            "GEMINI_API_KEY"
        ));
    }

    #[test]
    fn roundtrip_preserves_unknown_key() {
        let dir = tmp_path("roundtrip");
        let path = dir.join("config.json");

        // A file written by some other tool: a key we don't model plus a known
        // one and a nested unknown object.
        let original = json!({
            "whisper_model": "medium",
            "custom_dictionary": ["kubectl", "rustfft"],
            "future_feature_flag": true,
            "some_other_writers_key": {"nested": [1, 2, 3]},
            "orb_position": {"x": 12, "y": 34}
        });
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        let cfg = AppConfig::load_from(&path).expect("load");
        assert_eq!(cfg.whisper_model, "medium");
        assert_eq!(cfg.custom_dictionary, vec!["kubectl", "rustfft"]);
        // Unknown keys captured in `extra`.
        assert_eq!(cfg.extra.get("future_feature_flag"), Some(&json!(true)));
        assert_eq!(
            cfg.extra.get("some_other_writers_key"),
            Some(&json!({"nested": [1, 2, 3]}))
        );
        // orb_position object shape preserved.
        assert_eq!(cfg.orb_position, Some(json!({"x": 12, "y": 34})));

        // Save, reload: unknown keys must survive.
        cfg.save_to(&path).expect("save");
        let reread: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reread["future_feature_flag"], json!(true));
        assert_eq!(
            reread["some_other_writers_key"],
            json!({"nested": [1, 2, 3]})
        );
        assert_eq!(reread["whisper_model"], json!("medium"));
        assert_eq!(reread["orb_position"], json!({"x": 12, "y": 34}));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orb_position_string_shape_roundtrips() {
        let dir = tmp_path("orbstr");
        let path = dir.join("config.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"orb_position":"bottom-center"}"#).unwrap();

        let cfg = AppConfig::load_from(&path).unwrap();
        assert_eq!(cfg.orb_position, Some(json!("bottom-center")));
        cfg.save_to(&path).unwrap();
        let reread: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reread["orb_position"], json!("bottom-center"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gemini_precedence_env_first() {
        let _g = env_guard();
        std::env::set_var("GEMINI_API_KEY", "from_gemini_env");
        std::env::set_var("GOOGLE_API_KEY", "from_google_env");

        let cfg = AppConfig {
            gemini_api_key: Some("from_config".to_string()),
            ..AppConfig::default()
        };
        assert_eq!(cfg.gemini_api_key().as_deref(), Some("from_gemini_env"));

        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("GOOGLE_API_KEY");
    }

    #[test]
    fn gemini_precedence_google_env_when_gemini_unset() {
        let _g = env_guard();
        std::env::remove_var("GEMINI_API_KEY");
        std::env::set_var("GOOGLE_API_KEY", "from_google_env");

        let cfg = AppConfig {
            gemini_api_key: Some("from_config".to_string()),
            ..AppConfig::default()
        };
        assert_eq!(cfg.gemini_api_key().as_deref(), Some("from_google_env"));

        std::env::remove_var("GOOGLE_API_KEY");
    }

    #[test]
    fn gemini_precedence_falls_back_to_config() {
        let _g = env_guard();
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("GOOGLE_API_KEY");

        // Use a definitely-absent .env so resolution falls through to config.
        let missing_env = tmp_path("no_env").join(".env");
        assert!(!missing_env.exists());

        let cfg = AppConfig {
            gemini_api_key: Some("  from_config  ".to_string()),
            ..AppConfig::default()
        };
        // Trimmed config value wins when nothing higher-precedence is set.
        assert_eq!(
            cfg.resolve_gemini_key(Some(&missing_env)).as_deref(),
            Some("from_config")
        );

        // Empty config key resolves to None.
        let empty = AppConfig {
            gemini_api_key: Some("   ".to_string()),
            ..AppConfig::default()
        };
        assert_eq!(empty.resolve_gemini_key(Some(&missing_env)), None);
        assert_eq!(empty.resolve_gemini_key(None), None);
    }

    #[test]
    fn gemini_precedence_dotenv_over_config() {
        let _g = env_guard();
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("GOOGLE_API_KEY");

        let dir = tmp_path("gemini_dotenv");
        std::fs::create_dir_all(&dir).unwrap();
        let env_path = dir.join(".env");
        std::fs::write(
            &env_path,
            "# comment\nexport GEMINI_API_KEY=\"from_dotenv\"\nGOOGLE_API_KEY=other\n",
        )
        .unwrap();

        let cfg = AppConfig {
            gemini_api_key: Some("from_config".to_string()),
            ..AppConfig::default()
        };
        // .env GEMINI_API_KEY beats config; GEMINI beats GOOGLE within the file.
        assert_eq!(
            cfg.resolve_gemini_key(Some(&env_path)).as_deref(),
            Some("from_dotenv")
        );

        // .env with only GOOGLE_API_KEY is used when GEMINI absent in file.
        std::fs::write(&env_path, "GOOGLE_API_KEY='google_only'\n").unwrap();
        assert_eq!(
            cfg.resolve_gemini_key(Some(&env_path)).as_deref(),
            Some("google_only")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
