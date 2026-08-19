//! SQLite history layer for VibeToText.
//!
//! Port of `src/vibetotext/history.py`. Owns the canonical `entries` table over
//! `~/.vibetotext/history.db`, with a `PRAGMA user_version`-gated migration that
//! reconciles the three legacy writers (Python, C#/WPF, Swift) onto one schema —
//! see the migration plan §6.
//!
//! ## Sentiment dependency
//!
//! VADER scoring lives in the sibling `crate::sentiment` module (owned by another
//! Phase 0 builder). To stay decoupled from that file during this parallel build,
//! every code path that needs a score takes a [`Scorer`] function. The public
//! [`Db`] API binds it to [`crate::sentiment::score`]; unit tests inject a stub so
//! this module's tests compile and run on their own.
//!
//! ## Concurrency
//!
//! Each [`Db`] owns a single `rusqlite::Connection` behind a `Mutex`. The
//! connection is opened in WAL mode with a 30s busy timeout, and every write runs
//! inside an `IMMEDIATE` transaction — mirroring the Python `isolation_level=
//! "IMMEDIATE"` + `timeout=30.0` settings so a concurrent reader/writer never sees
//! `database is locked`.

mod entries;
mod schema;
mod stats;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

pub use entries::Entry;
pub use stats::Statistics;

/// A function that maps text to a VADER compound score in `[-1.0, 1.0]`.
///
/// Production code passes [`crate::sentiment::score`]; tests pass a stub.
pub type Scorer = fn(&str) -> f64;

/// Default scorer: the real VADER port in `crate::sentiment`.
fn default_scorer(text: &str) -> f64 {
    crate::sentiment::score(text)
}

/// Errors surfaced by the database layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error touching the database directory or backup: {0}")]
    Io(#[from] std::io::Error),

    #[error("legacy history.json at {path} is malformed: {source}")]
    LegacyJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Handle to the transcription history database.
pub struct Db {
    conn: Mutex<Connection>,
    path: PathBuf,
    scorer: Scorer,
}

impl Db {
    /// Open (creating if needed) the history database at `path` and run all
    /// pending migrations. Uses the real [`crate::sentiment`] scorer.
    ///
    /// Mirrors `TranscriptionHistory.__init__`: ensures the parent directory
    /// exists, configures the connection, migrates, and imports legacy JSON.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        Self::open_with_scorer(path, default_scorer)
    }

    /// Like [`Db::open`] but with an injectable scorer (used by tests).
    pub fn open_with_scorer(path: impl AsRef<Path>, scorer: Scorer) -> Result<Self, DbError> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let conn = Self::configure_connection(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let db = Self {
            conn: Mutex::new(conn),
            path,
            scorer,
        };
        db.migrate()?;
        Ok(db)
    }

    /// Open a connection and apply the per-connection PRAGMAs that match the
    /// Python writer's semantics:
    /// - `busy_timeout = 30000` (30s, == Python `timeout=30.0`)
    /// - `journal_mode = WAL` (concurrent reads during writes)
    /// - `synchronous = NORMAL` (safe + fast under WAL)
    fn configure_connection(path: &Path) -> Result<Connection, DbError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(30_000))?;
        // `query_row` because WAL returns the new mode as a row.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    /// Path to the underlying database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run all pending schema migrations. See [`schema`] for the gory details.
    fn migrate(&self) -> Result<(), DbError> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        schema::migrate(&mut conn, self.scorer, &self.path)
    }

    /// Add a transcription entry. Computes `word_count`, `wpm`, and `sentiment`
    /// internally (port of `add_entry`). `timestamp` is an ISO-8601 string;
    /// callers that want "now" should pass the current time formatted as ISO.
    ///
    /// Returns the new row id.
    pub fn add_entry(
        &self,
        text: &str,
        mode: &str,
        timestamp: &str,
        duration_seconds: Option<f64>,
    ) -> Result<i64, DbError> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        entries::add_entry(
            &mut conn,
            self.scorer,
            text,
            mode,
            timestamp,
            duration_seconds,
        )
    }

    /// Fetch entries newest-first, optionally filtered by `mode` and capped at
    /// `limit`. The `mode` filter is applied in SQL so `limit` bounds the
    /// number of *filtered* rows.
    pub fn get_entries(
        &self,
        mode: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<Entry>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        entries::get_entries(&conn, mode, limit)
    }

    /// Compute aggregate statistics over history (port of `get_statistics`).
    ///
    /// `mode = Some("transcribe")` (etc.) restricts every metric to the matching
    /// rows; `None` aggregates over every mode. Mirrors [`get_entries`](Self::get_entries).
    pub fn get_statistics(&self, mode: Option<&str>) -> Result<Statistics, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        stats::get_statistics(&conn, mode)
    }

    /// Delete all history (port of `clear`).
    pub fn clear(&self) -> Result<(), DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute("DELETE FROM entries", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic stub scorer for tests: positive if "good" appears, negative
    /// if "bad" appears, else neutral. Lets tests assert backfill ran without
    /// depending on the real VADER module (owned by a sibling builder).
    pub(crate) fn stub_scorer(text: &str) -> f64 {
        let t = text.to_lowercase();
        if t.contains("good") {
            0.5
        } else if t.contains("bad") {
            -0.5
        } else {
            0.0
        }
    }

    fn temp_db_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("vibetotext_test_{tag}_{nanos}.db"));
        p
    }

    /// Best-effort cleanup of the db file plus WAL/SHM sidecars.
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    /// Create fresh -> insert -> read back, and confirm computed fields.
    #[test]
    fn fresh_db_insert_and_readback() {
        let path = temp_db_path("fresh");
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();

            // 6 words, 30s -> 12 wpm; "good" -> +0.5 sentiment.
            let id = db
                .add_entry(
                    "this is a really good transcription",
                    "transcribe",
                    "2026-06-02T10:00:00",
                    Some(30.0),
                )
                .unwrap();
            assert!(id > 0);

            let entries = db.get_entries(None, None).unwrap();
            assert_eq!(entries.len(), 1);
            let e = &entries[0];
            assert_eq!(e.text, "this is a really good transcription");
            assert_eq!(e.mode, "transcribe");
            assert_eq!(e.word_count, 6);
            assert_eq!(e.duration_seconds, Some(30.0));
            // 6 words / (30/60 min) = 12 wpm.
            assert_eq!(e.wpm, Some(12));
            assert_eq!(e.sentiment, Some(0.5));
        }
        cleanup(&path);
    }

    /// An entry with no duration must store NULL wpm/duration (Python parity).
    #[test]
    fn entry_without_duration_has_null_wpm() {
        let path = temp_db_path("noduration");
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            db.add_entry("plain text here", "cleanup", "2026-06-02T11:00:00", None)
                .unwrap();
            let e = &db.get_entries(None, None).unwrap()[0];
            assert_eq!(e.duration_seconds, None);
            assert_eq!(e.wpm, None);
            assert_eq!(e.word_count, 3);
        }
        cleanup(&path);
    }

    /// Newest-first ordering, the `limit` cap, and the `mode` filter.
    #[test]
    fn ordering_and_limit() {
        let path = temp_db_path("order");
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            db.add_entry("first", "transcribe", "2026-06-01T09:00:00", None)
                .unwrap();
            db.add_entry("second", "transcribe", "2026-06-02T09:00:00", None)
                .unwrap();
            db.add_entry("third", "transcribe", "2026-06-03T09:00:00", None)
                .unwrap();

            let all = db.get_entries(None, None).unwrap();
            assert_eq!(all.len(), 3);
            assert_eq!(all[0].text, "third"); // newest first
            assert_eq!(all[2].text, "first");

            let limited = db.get_entries(None, Some(2)).unwrap();
            assert_eq!(limited.len(), 2);
            assert_eq!(limited[0].text, "third");
        }
        cleanup(&path);
    }

    /// `mode` and `limit` compose: filtering happens in SQL so `limit` bounds
    /// the *filtered* rows, not the unfiltered ones. Regression for the
    /// frontend asking "10 most recent cleanup entries" and getting fewer than
    /// 10 because the most recent rows were other modes.
    #[test]
    fn filter_mode_is_applied_before_limit() {
        let path = temp_db_path("mode_limit");
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            // Interleave cleanup and transcribe; the cleanup rows are the 1st,
            // 3rd, 5th newest — so a "limit 2 cleanup" should skip past the
            // interleaved transcribe rows and return the 3rd and 5th newest.
            db.add_entry("cleanup-newest", "cleanup", "2026-06-05T10:00:00", None)
                .unwrap();
            db.add_entry(
                "transcribe-mid-1",
                "transcribe",
                "2026-06-04T10:00:00",
                None,
            )
            .unwrap();
            db.add_entry("cleanup-mid-2", "cleanup", "2026-06-03T10:00:00", None)
                .unwrap();
            db.add_entry(
                "transcribe-mid-2",
                "transcribe",
                "2026-06-02T10:00:00",
                None,
            )
            .unwrap();
            db.add_entry("cleanup-oldest", "cleanup", "2026-06-01T10:00:00", None)
                .unwrap();

            let top2_cleanup = db.get_entries(Some("cleanup"), Some(2)).unwrap();
            assert_eq!(top2_cleanup.len(), 2, "should return 2 cleanup rows");
            assert_eq!(top2_cleanup[0].text, "cleanup-newest");
            assert_eq!(top2_cleanup[1].text, "cleanup-mid-2");

            // No mode filter: same limit returns newest-of-any-mode.
            let top2_any = db.get_entries(None, Some(2)).unwrap();
            assert_eq!(top2_any[0].text, "cleanup-newest");
            assert_eq!(top2_any[1].text, "transcribe-mid-1");

            // Mode filter without limit: all matching rows in newest-first order.
            let all_cleanup = db.get_entries(Some("cleanup"), None).unwrap();
            assert_eq!(all_cleanup.len(), 3);
            assert_eq!(all_cleanup[0].text, "cleanup-newest");
            assert_eq!(all_cleanup[1].text, "cleanup-mid-2");
            assert_eq!(all_cleanup[2].text, "cleanup-oldest");
        }
        cleanup(&path);
    }

    /// Simulate a "native-written" DB (C#/Swift) that lacks the `sentiment`
    /// column and reports `user_version = 0`. After migration the column must
    /// exist and previously-null rows must be backfilled by the scorer.
    #[test]
    fn migrates_native_db_adding_and_backfilling_sentiment() {
        let path = temp_db_path("native");
        cleanup(&path);

        // 1. Hand-build a legacy schema WITHOUT a sentiment column and insert a
        //    row, exactly as a C#/Swift writer would have left it.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    text TEXT NOT NULL,
                    mode TEXT NOT NULL,
                    timestamp TEXT NOT NULL,
                    word_count INTEGER NOT NULL,
                    duration_seconds REAL,
                    wpm INTEGER
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO entries (text, mode, timestamp, word_count, duration_seconds, wpm)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    "this was a good day",
                    "transcribe",
                    "2026-05-30T08:00:00",
                    5,
                    Option::<f64>::None,
                    Option::<i64>::None
                ],
            )
            .unwrap();
            // user_version defaults to 0, matching all existing DBs.
            let uv: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(uv, 0, "fixture must look like an unmigrated native DB");
        }

        // 2. Open through our layer -> migration runs.
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();

            // Column now exists and the legacy row was backfilled.
            let entries = db.get_entries(None, None).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(
                entries[0].sentiment,
                Some(0.5),
                "stub scorer should have backfilled the 'good' row"
            );

            // user_version is bumped so a second open is a no-op.
            let conn = db.conn.lock().unwrap();
            let uv: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(uv, 1);

            // The one-time backup was created before the first migration.
            assert!(
                path.with_file_name("history.db.pre-tauri.bak").exists()
                    || backup_path_for(&path).exists()
            );
        }
        cleanup(&path);
        let _ = std::fs::remove_file(backup_path_for(&path));
    }

    /// Re-opening an already-migrated DB must not re-backfill or re-backup.
    #[test]
    fn reopen_is_idempotent() {
        let path = temp_db_path("idempotent");
        cleanup(&path);
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            db.add_entry(
                "hello good world",
                "transcribe",
                "2026-06-02T12:00:00",
                None,
            )
            .unwrap();
        }
        // Second open should succeed and preserve data.
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            assert_eq!(db.get_entries(None, None).unwrap().len(), 1);
        }
        cleanup(&path);
        let _ = std::fs::remove_file(backup_path_for(&path));
    }

    /// Legacy `history.json` is imported on first open then renamed to
    /// `.json.migrated`.
    #[test]
    fn imports_legacy_history_json() {
        let path = temp_db_path("json");
        cleanup(&path);
        // The JSON sits next to the db with the .json suffix (Python:
        // path.with_suffix(".json")).
        let json_path = path.with_extension("json");
        let json = r#"{
            "entries": [
                {"text": "imported good entry", "mode": "transcribe",
                 "timestamp": "2026-05-01T10:00:00", "word_count": 3,
                 "duration_seconds": 15.0, "wpm": 12},
                {"text": "second bad one", "mode": "cleanup",
                 "timestamp": "2026-05-02T10:00:00"}
            ]
        }"#;
        std::fs::write(&json_path, json).unwrap();

        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            let entries = db.get_entries(None, None).unwrap();
            assert_eq!(entries.len(), 2);
            // Newest first.
            assert_eq!(entries[0].text, "second bad one");
            // Missing word_count was derived from text (3 words).
            assert_eq!(entries[0].word_count, 3);
            // Sentiment backfilled for imported rows.
            assert_eq!(entries[0].sentiment, Some(-0.5));
            assert_eq!(entries[1].sentiment, Some(0.5));
        }

        // Original JSON renamed; .migrated created.
        assert!(!json_path.exists(), "history.json should be renamed away");
        assert!(json_path.with_extension("json.migrated").exists());

        cleanup(&path);
        let _ = std::fs::remove_file(json_path.with_extension("json.migrated"));
        let _ = std::fs::remove_file(backup_path_for(&path));
    }

    /// Statistics mirror history.py: sessions, words, avg wpm, time saved.
    #[test]
    fn statistics_match_python_semantics() {
        let path = temp_db_path("stats");
        cleanup(&path);
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            // 100 words in 60s -> 100 wpm.
            let text100 = "word ".repeat(100);
            db.add_entry(
                text100.trim(),
                "transcribe",
                "2026-06-01T10:00:00",
                Some(60.0),
            )
            .unwrap();
            // 40 words in 60s -> 40 wpm.
            let text40 = "alpha ".repeat(40);
            db.add_entry(
                text40.trim(),
                "transcribe",
                "2026-06-02T10:00:00",
                Some(60.0),
            )
            .unwrap();

            let s = db.get_statistics(None).unwrap();
            assert_eq!(s.total_sessions, 2);
            assert_eq!(s.total_words, 140);
            // avg of 100 and 40 = 70.
            assert_eq!(s.avg_wpm, 70);
            // time_to_type = 140 words / 40 wpm = 3.5 min.
            // time_dictating = 120s / 60 = 2.0 min. saved = 1.5 min.
            assert!((s.time_saved_minutes - 1.5).abs() < 1e-6);
            assert!((s.total_duration_seconds - 120.0).abs() < 1e-6);
        }
        cleanup(&path);
        let _ = std::fs::remove_file(backup_path_for(&path));
    }

    /// Empty DB returns the zeroed statistics shape (Python early-return).
    #[test]
    fn statistics_empty_db() {
        let path = temp_db_path("emptystats");
        cleanup(&path);
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            let s = db.get_statistics(None).unwrap();
            assert_eq!(s.total_sessions, 0);
            assert_eq!(s.total_words, 0);
            assert_eq!(s.avg_wpm, 0);
            assert_eq!(s.time_saved_minutes, 0.0);
            assert!(s.common_words.is_empty());
        }
        cleanup(&path);
        let _ = std::fs::remove_file(backup_path_for(&path));
    }

    /// `get_statistics(mode)` restricts every metric (counts, avg WPM, time
    /// saved, word frequency) to the matching rows.
    #[test]
    fn statistics_filters_by_mode() {
        let path = temp_db_path("statsbymode");
        cleanup(&path);
        {
            let db = Db::open_with_scorer(&path, stub_scorer).unwrap();
            let mut conn = db.conn.lock().expect("db mutex poisoned");

            // 1 transcribe entry (2 words) + 1 cleanup entry (3 words).
            let _ = super::entries::add_entry(
                &mut conn,
                stub_scorer,
                "alpha beta",
                "transcribe",
                "2026-08-19T00:00:00Z",
                Some(2.0),
            )
            .unwrap();
            let _ = super::entries::add_entry(
                &mut conn,
                stub_scorer,
                "gamma delta epsilon",
                "cleanup",
                "2026-08-19T00:00:01Z",
                Some(3.0),
            )
            .unwrap();
            drop(conn);

            let s_all = db.get_statistics(None).unwrap();
            assert_eq!(s_all.total_sessions, 2);
            assert_eq!(s_all.total_words, 5);

            let s_t = db.get_statistics(Some("transcribe")).unwrap();
            assert_eq!(s_t.total_sessions, 1);
            assert_eq!(s_t.total_words, 2);
            // Word frequency only sees transcribe text -> "alpha", "beta".
            let t_map: std::collections::HashMap<_, _> = s_t.common_words.iter().cloned().collect();
            assert_eq!(t_map.get("alpha"), Some(&1));
            assert_eq!(t_map.get("beta"), Some(&1));
            assert!(!t_map.contains_key("gamma"));

            let s_c = db.get_statistics(Some("cleanup")).unwrap();
            assert_eq!(s_c.total_sessions, 1);
            assert_eq!(s_c.total_words, 3);
            let c_map: std::collections::HashMap<_, _> = s_c.common_words.iter().cloned().collect();
            assert_eq!(c_map.get("gamma"), Some(&1));
            assert!(!c_map.contains_key("alpha"));
        }
        cleanup(&path);
        let _ = std::fs::remove_file(backup_path_for(&path));
    }

    /// Helper mirroring schema::backup_path so tests can clean up the backup.
    fn backup_path_for(path: &Path) -> PathBuf {
        let mut name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".pre-tauri.bak");
        path.with_file_name(name)
    }
}
