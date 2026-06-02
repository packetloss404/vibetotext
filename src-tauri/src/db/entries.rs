//! Entry row type + read/write helpers.
//!
//! Port of `TranscriptionHistory.add_entry` / `get_entries` in
//! `src/vibetotext/history.py`. `add_entry` computes `word_count`, `wpm`, and
//! `sentiment` internally; reads return rows newest-first.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::{DbError, Scorer};

/// One transcription history row (the canonical `entries` schema, plan §6).
///
/// Serializes to camel/snake JSON matching the fields the frontend `renderer.js`
/// expects from the original SQLite `dict(row)` payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub text: String,
    pub mode: String,
    /// ISO-8601 timestamp string (stored verbatim, as Python did).
    pub timestamp: String,
    pub word_count: i64,
    pub duration_seconds: Option<f64>,
    /// Words per minute, rounded to an integer (NULL when no duration).
    pub wpm: Option<i64>,
    /// VADER compound score in `[-1.0, 1.0]` (NULL only on a transient
    /// pre-backfill read; populated for every row this layer writes).
    pub sentiment: Option<f64>,
}

/// Count whitespace-delimited words, matching Python `len(text.split())`.
fn word_count(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

/// WPM = round(word_count / (duration_seconds / 60)). NULL when there is no
/// positive duration (Python parity).
fn compute_wpm(word_count: i64, duration_seconds: Option<f64>) -> Option<i64> {
    match duration_seconds {
        Some(d) if d > 0.0 => {
            let minutes = d / 60.0;
            if minutes > 0.0 {
                Some((word_count as f64 / minutes).round() as i64)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Insert a new entry, computing derived fields. Returns the new row id.
///
/// Wrapped in an `IMMEDIATE` transaction so the write takes its lock up front
/// (matching Python's `isolation_level="IMMEDIATE"`).
pub(super) fn add_entry(
    conn: &mut Connection,
    scorer: Scorer,
    text: &str,
    mode: &str,
    timestamp: &str,
    duration_seconds: Option<f64>,
) -> Result<i64, DbError> {
    let wc = word_count(text);
    let wpm = compute_wpm(wc, duration_seconds);
    let sentiment = scorer(text);

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO entries (text, mode, timestamp, word_count, duration_seconds, wpm, sentiment)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![text, mode, timestamp, wc, duration_seconds, wpm, sentiment],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(id)
}

/// Map a `rusqlite::Row` to an [`Entry`].
fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get("id")?,
        text: row.get("text")?,
        mode: row.get("mode")?,
        timestamp: row.get("timestamp")?,
        word_count: row.get("word_count")?,
        duration_seconds: row.get("duration_seconds")?,
        wpm: row.get("wpm")?,
        sentiment: row.get("sentiment")?,
    })
}

/// Fetch entries newest-first, optionally capped at `limit`.
/// Port of `get_entries`.
pub(super) fn get_entries(conn: &Connection, limit: Option<u32>) -> Result<Vec<Entry>, DbError> {
    let entries = match limit {
        Some(n) => {
            let mut stmt =
                conn.prepare("SELECT * FROM entries ORDER BY timestamp DESC LIMIT ?1")?;
            let rows = stmt.query_map(params![n], row_to_entry)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        None => {
            let mut stmt = conn.prepare("SELECT * FROM entries ORDER BY timestamp DESC")?;
            let rows = stmt.query_map([], row_to_entry)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(entries)
}

/// Total row count — small helper used by callers/tests.
#[allow(dead_code)]
pub(super) fn count(conn: &Connection) -> Result<i64, DbError> {
    let c = conn
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpm_rounding_matches_python() {
        // 6 words / (30/60) = 12.0 -> 12.
        assert_eq!(compute_wpm(6, Some(30.0)), Some(12));
        // 10 words / (45/60 = 0.75) = 13.33 -> 13.
        assert_eq!(compute_wpm(10, Some(45.0)), Some(13));
        // No / zero / negative duration -> None.
        assert_eq!(compute_wpm(10, None), None);
        assert_eq!(compute_wpm(10, Some(0.0)), None);
        assert_eq!(compute_wpm(10, Some(-5.0)), None);
    }

    #[test]
    fn word_count_splits_on_whitespace() {
        assert_eq!(word_count("hello world"), 2);
        assert_eq!(word_count("  spaced   out  words "), 3);
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("single"), 1);
    }
}
