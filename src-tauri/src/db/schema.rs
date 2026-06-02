//! Schema definition + `PRAGMA user_version`-gated migrations.
//!
//! Port of the `_ensure_storage` / `_backfill_sentiment` / `_migrate_from_json`
//! logic in `src/vibetotext/history.py`, generalized per migration plan §6 so it
//! safely upgrades databases written by any of the three legacy apps:
//!
//! - **Python** already has the `sentiment` column (it created it).
//! - **C# / Swift** wrote the table *without* `sentiment`.
//!
//! All existing DBs report `PRAGMA user_version = 0`, so version 0 -> 1 performs
//! the full reconciliation:
//!
//! 0. (once, before any change) back up `history.db` -> `history.db.pre-tauri.bak`.
//! 1. `CREATE TABLE IF NOT EXISTS entries(...)` + `idx_timestamp`.
//! 2. `ALTER TABLE entries ADD COLUMN sentiment REAL`, swallowing the
//!    "duplicate column" error for DBs that already have it.
//! 3. Backfill `sentiment IS NULL` rows via the injected [`Scorer`].
//! 4. Import a legacy sibling `history.json` (if present) then rename it to
//!    `.json.migrated`.
//! 5. `PRAGMA user_version = 1`.
//!
//! Steps 1-5 run inside a single `IMMEDIATE` transaction so a crash mid-migration
//! leaves the DB at version 0 and the migration simply re-runs next launch.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{DbError, Scorer};

/// Latest schema version this build knows how to produce.
const TARGET_VERSION: i64 = 1;

/// SQL for the canonical `entries` table (migration plan §6).
const CREATE_ENTRIES: &str = "
    CREATE TABLE IF NOT EXISTS entries (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        text TEXT NOT NULL,
        mode TEXT NOT NULL,
        timestamp TEXT NOT NULL,
        word_count INTEGER NOT NULL,
        duration_seconds REAL,
        wpm INTEGER,
        sentiment REAL
    )
";

const CREATE_INDEX: &str = "
    CREATE INDEX IF NOT EXISTS idx_timestamp ON entries(timestamp DESC)
";

/// `history.db` -> `history.db.pre-tauri.bak`. Appends to the *full* filename so
/// the suffix survives regardless of the base name (matches plan wording and
/// keeps the original extension intact).
pub(super) fn backup_path(db_path: &Path) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".pre-tauri.bak");
    db_path.with_file_name(name)
}

/// Run pending migrations to bring the DB to [`TARGET_VERSION`].
///
/// `db_path` is needed for the one-time backup and to locate the sibling
/// `history.json`.
pub(super) fn migrate(
    conn: &mut Connection,
    scorer: Scorer,
    db_path: &Path,
) -> Result<(), DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    if version >= TARGET_VERSION {
        return Ok(());
    }

    // --- Step 0: one-time backup BEFORE the first migration (version 0 only). ---
    //
    // Only back up a DB file that already has content; a brand-new empty file we
    // just created has nothing worth preserving (Python never backed up at all —
    // this is a plan §6 safety addition for the native-DB upgrade path).
    if version == 0 {
        maybe_backup(conn, db_path)?;
    }

    // --- Steps 1-5 in one IMMEDIATE transaction. ---
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    // Step 1: canonical table + index.
    tx.execute_batch(&format!("{CREATE_ENTRIES};{CREATE_INDEX};"))?;

    // Step 2: ensure the sentiment column exists, swallowing the duplicate-column
    // error raised on DBs (or the table we just created) that already have it.
    match tx.execute("ALTER TABLE entries ADD COLUMN sentiment REAL", []) {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
            if msg.contains("duplicate column name") =>
        {
            // Already present — fine.
        }
        Err(e) => return Err(e.into()),
    }

    // Step 4 (before backfill so imported rows also get scored): import legacy
    // JSON. Done inside the txn so a failure rolls the whole thing back.
    let imported_json = import_legacy_json(&tx, db_path)?;

    // Step 3: backfill any NULL sentiment (legacy native rows + freshly imported).
    backfill_sentiment(&tx, scorer)?;

    // Step 5: stamp the version.
    tx.pragma_update(None, "user_version", TARGET_VERSION)?;

    tx.commit()?;

    // Rename the imported JSON only after the txn commits (so we never lose the
    // source file if the DB rolled back) AND only if we actually imported it —
    // never rename a user's history.json when import was skipped.
    if imported_json {
        finalize_legacy_json(db_path)?;
    }

    Ok(())
}

/// Back up the db file iff it already contains an `entries` table with rows.
fn maybe_backup(conn: &Connection, db_path: &Path) -> Result<(), DbError> {
    let has_entries: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='entries'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    if !has_entries {
        return Ok(());
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    if count == 0 {
        return Ok(());
    }

    let dst = backup_path(db_path);
    if dst.exists() {
        // A previous run already backed up; don't clobber it.
        return Ok(());
    }

    // Checkpoint WAL first so the copied main file is self-consistent.
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
    std::fs::copy(db_path, &dst)?;
    tracing::info!(backup = %dst.display(), "backed up history.db before migration");
    Ok(())
}

/// Score every row whose `sentiment` is NULL using the injected scorer.
/// Port of `_backfill_sentiment`.
fn backfill_sentiment(conn: &Connection, scorer: Scorer) -> Result<(), DbError> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, text FROM entries WHERE sentiment IS NULL")?;
        let mapped = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        mapped.collect::<Result<_, _>>()?
    };

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(count = rows.len(), "backfilling VADER sentiment");
    for (id, text) in rows {
        let score = scorer(&text);
        conn.execute(
            "UPDATE entries SET sentiment = ?1 WHERE id = ?2",
            rusqlite::params![score, id],
        )?;
    }
    Ok(())
}

/// Sibling legacy JSON path: `history.db` -> `history.json`
/// (Python: `self.path.with_suffix(".json")`).
fn legacy_json_path(db_path: &Path) -> PathBuf {
    db_path.with_extension("json")
}

/// Import legacy `history.json` if present AND the table is currently empty
/// (port of `_migrate_from_json`: it bails when entries already exist).
/// Returns `true` only if rows were actually imported, so the caller renames the
/// source file (via [`finalize_legacy_json`]) ONLY then — never clobbering a
/// user's history.json when import was skipped. The rename happens after commit.
fn import_legacy_json(conn: &Connection, db_path: &Path) -> Result<bool, DbError> {
    let json_path = legacy_json_path(db_path);
    if !json_path.exists() {
        return Ok(false);
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    if count > 0 {
        // Don't migrate twice; leave the JSON in place (Python returns early and
        // does NOT rename in this case).
        return Ok(false);
    }

    let raw = std::fs::read_to_string(&json_path)?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| DbError::LegacyJson {
            path: json_path.clone(),
            source,
        })?;

    let entries = parsed
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if entries.is_empty() {
        return Ok(false);
    }

    tracing::info!(count = entries.len(), "migrating entries from history.json");

    for entry in entries {
        let text = entry
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mode = entry
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("transcribe")
            .to_string();
        // Python falls back to datetime.now().isoformat(); we keep it simple and
        // require legacy entries to carry a timestamp, defaulting to empty-safe
        // value only if absent.
        let timestamp = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(now_iso);
        let word_count = entry
            .get("word_count")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| text.split_whitespace().count() as i64);
        let duration_seconds = entry.get("duration_seconds").and_then(|v| v.as_f64());
        let wpm = entry.get("wpm").and_then(|v| v.as_i64());

        // sentiment left NULL on purpose: the backfill step scores it next.
        conn.execute(
            "INSERT INTO entries (text, mode, timestamp, word_count, duration_seconds, wpm)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![text, mode, timestamp, word_count, duration_seconds, wpm],
        )?;
    }

    Ok(true)
}

/// After the migration transaction commits, rename the imported JSON so it is
/// not re-imported. Mirrors the Python rename to `.json.migrated`. No-op if the
/// file is gone (e.g. it was never present or already renamed).
fn finalize_legacy_json(db_path: &Path) -> Result<(), DbError> {
    let json_path = legacy_json_path(db_path);
    if !json_path.exists() {
        return Ok(());
    }
    // Only rename if we actually imported (table now non-empty was the gate);
    // since import only ran on an empty table, the presence of the file here +
    // a non-version-0 stamp means we either imported or the file had no usable
    // entries. Renaming is safe either way: it documents that we processed it.
    let migrated = json_path.with_extension("json.migrated");
    // Avoid clobbering an existing .migrated from a prior partial run.
    if migrated.exists() {
        let _ = std::fs::remove_file(&json_path);
    } else {
        std::fs::rename(&json_path, &migrated)?;
    }
    tracing::info!(renamed = %migrated.display(), "legacy history.json migrated");
    Ok(())
}

/// Current time as an ISO-8601 string. Avoids a chrono dependency in the db
/// crate; only used as a last-resort default for legacy rows missing a
/// timestamp.
fn now_iso() -> String {
    // Fallback timestamp for legacy JSON rows missing one. Emit a real UTC
    // ISO-8601 string (matching the Python reference's datetime.now().isoformat())
    // so it sorts correctly in the timestamp-DESC index. Legacy rows virtually
    // always carry a timestamp, so this is a last resort.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // days since 1970-01-01 -> civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}
