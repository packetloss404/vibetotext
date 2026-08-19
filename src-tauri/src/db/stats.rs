//! Aggregate statistics over all history.
//!
//! Direct port of `TranscriptionHistory.get_statistics` in
//! `src/vibetotext/history.py`: total sessions/words, average WPM, "time saved"
//! at a 40 WPM typing baseline, plus common-word and longest-word frequency
//! analysis (stopwords filtered).

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::DbError;

/// Typing baseline used for the "time saved" calculation (Python: `typing_wpm`).
const TYPING_WPM: f64 = 40.0;

/// Aggregate statistics payload. Field names mirror the Python dict keys so the
/// frontend `analytics.js` consumes them unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Statistics {
    pub total_words: i64,
    pub total_sessions: i64,
    /// `(word, count)` pairs, most-common first, top 20.
    pub common_words: Vec<(String, i64)>,
    /// Longest unique words used, longest-first, top 20.
    pub longest_words: Vec<String>,
    pub avg_wpm: i64,
    pub time_saved_minutes: f64,
    pub total_duration_seconds: f64,
}

impl Statistics {
    /// The zeroed shape returned for an empty database (Python early-return).
    fn empty() -> Self {
        Self {
            total_words: 0,
            total_sessions: 0,
            common_words: Vec::new(),
            longest_words: Vec::new(),
            avg_wpm: 0,
            time_saved_minutes: 0.0,
            total_duration_seconds: 0.0,
        }
    }
}

/// Compute statistics over all history, optionally filtered by `mode`.
///
/// `mode = Some("transcribe")` (etc.) restricts the aggregate counts, avg WPM,
/// time-saved calculation, and the word-frequency text source to the matching
/// rows. `None` aggregates over every mode. The `entries.mode` index added by
/// the `get_entries` fix keeps the filtered aggregate cheap.
pub(super) fn get_statistics(conn: &Connection, mode: Option<&str>) -> Result<Statistics, DbError> {
    // Aggregate counts.
    let (total_sessions, total_words, total_duration): (i64, i64, f64) = if let Some(m) = mode {
        conn.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(word_count), 0),
                COALESCE(SUM(duration_seconds), 0)
             FROM entries WHERE mode = ?1",
            [m],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
    } else {
        conn.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(word_count), 0),
                COALESCE(SUM(duration_seconds), 0)
             FROM entries",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
    };

    if total_sessions == 0 {
        return Ok(Statistics::empty());
    }

    // Average WPM over rows that have one. AVG() returns NULL when none qualify.
    let avg_wpm_raw: Option<f64> = if let Some(m) = mode {
        conn.query_row(
            "SELECT AVG(wpm) FROM entries WHERE wpm IS NOT NULL AND mode = ?1",
            [m],
            |r| r.get(0),
        )?
    } else {
        conn.query_row(
            "SELECT AVG(wpm) FROM entries WHERE wpm IS NOT NULL",
            [],
            |r| r.get(0),
        )?
    };
    let avg_wpm = avg_wpm_raw.map(|v| v.round() as i64).unwrap_or(0);

    // Time saved: words-with-duration / 40wpm  minus  total dictation minutes.
    let words_with_duration: i64 = if let Some(m) = mode {
        conn.query_row(
            "SELECT COALESCE(SUM(word_count), 0) FROM entries
             WHERE duration_seconds IS NOT NULL AND mode = ?1",
            [m],
            |r| r.get(0),
        )?
    } else {
        conn.query_row(
            "SELECT COALESCE(SUM(word_count), 0) FROM entries WHERE duration_seconds IS NOT NULL",
            [],
            |r| r.get(0),
        )?
    };
    let time_to_type_minutes = words_with_duration as f64 / TYPING_WPM;
    let time_dictating_minutes = total_duration / 60.0;
    let time_saved_minutes = (time_to_type_minutes - time_dictating_minutes).max(0.0);

    // Word-frequency analysis over filtered text.
    let texts: Vec<String> = match mode {
        Some(m) => {
            let mut stmt = conn.prepare("SELECT text FROM entries WHERE mode = ?1")?;
            let rows = stmt.query_map([m], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        }
        None => {
            let mut stmt = conn.prepare("SELECT text FROM entries")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        }
    };

    let (common_words, longest_words) = word_frequency(&texts);

    Ok(Statistics {
        total_words,
        total_sessions,
        common_words,
        longest_words,
        avg_wpm,
        // Python: round(time_saved_minutes, 1) and round(total_duration, 1).
        time_saved_minutes: round1(time_saved_minutes),
        total_duration_seconds: round1(total_duration),
    })
}

/// Round to one decimal place, matching Python `round(x, 1)`.
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Tokenize, strip punctuation, drop stopwords/short words, then compute the
/// top-20 most common and top-20 longest unique words. Port of the Python
/// `Counter`/`most_common` block.
fn word_frequency(texts: &[String]) -> (Vec<(String, i64)>, Vec<String>) {
    // Characters Python strips via `w.strip(".,!?;:'\"()[]{}")`.
    const STRIP_CHARS: &[char] = &[
        '.', ',', '!', '?', ';', ':', '\'', '"', '(', ')', '[', ']', '{', '}',
    ];

    let mut counts: HashMap<String, i64> = HashMap::new();
    // Preserve first-seen insertion order so ties break like Python's Counter
    // (which is insertion-ordered for equal counts).
    let mut order: Vec<String> = Vec::new();

    for text in texts {
        for raw in text.to_lowercase().split_whitespace() {
            let word: String = raw.trim_matches(STRIP_CHARS).to_string();
            if word.is_empty() || word.chars().count() <= 2 || STOPWORDS.contains(&word.as_str()) {
                continue;
            }
            let entry = counts.entry(word.clone()).or_insert_with(|| {
                order.push(word.clone());
                0
            });
            *entry += 1;
        }
    }

    // most_common(20): sort by count desc, ties keep first-seen order.
    let mut indexed: Vec<(usize, &String)> = order.iter().enumerate().collect();
    indexed.sort_by(|a, b| {
        let ca = counts[a.1];
        let cb = counts[b.1];
        cb.cmp(&ca).then(a.0.cmp(&b.0))
    });
    let common_words: Vec<(String, i64)> = indexed
        .iter()
        .take(20)
        .map(|(_, w)| ((*w).clone(), counts[*w]))
        .collect();

    // Longest unique words, length desc, top 20. Python's sorted() is stable;
    // the iteration order of a set is arbitrary, so we break length ties by the
    // first-seen order for determinism (a documented, harmless divergence).
    let unique: HashSet<&String> = order.iter().collect();
    let mut uniq_vec: Vec<&String> = unique.into_iter().collect();
    // Stable secondary key: first-seen index.
    let first_seen: HashMap<&String, usize> =
        order.iter().enumerate().map(|(i, w)| (w, i)).collect();
    uniq_vec.sort_by(|a, b| {
        b.chars()
            .count()
            .cmp(&a.chars().count())
            .then(first_seen[a].cmp(&first_seen[b]))
    });
    let longest_words: Vec<String> = uniq_vec.iter().take(20).map(|w| (*w).clone()).collect();

    (common_words, longest_words)
}

/// English stopwords excluded from word-frequency analysis. Copied verbatim from
/// the Python `STOPWORDS` set in `history.py` (deduplicated where the Python
/// source had duplicates — `set` membership is identical).
static STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "but",
    "in",
    "on",
    "at",
    "to",
    "for",
    "of",
    "with",
    "by",
    "from",
    "as",
    "is",
    "was",
    "are",
    "were",
    "been",
    "be",
    "have",
    "has",
    "had",
    "do",
    "does",
    "did",
    "will",
    "would",
    "could",
    "should",
    "may",
    "might",
    "must",
    "shall",
    "can",
    "need",
    "dare",
    "ought",
    "used",
    "i",
    "you",
    "he",
    "she",
    "it",
    "we",
    "they",
    "me",
    "him",
    "her",
    "us",
    "them",
    "my",
    "your",
    "his",
    "its",
    "our",
    "their",
    "this",
    "that",
    "these",
    "those",
    "what",
    "which",
    "who",
    "whom",
    "whose",
    "where",
    "when",
    "why",
    "how",
    "all",
    "each",
    "every",
    "both",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "no",
    "nor",
    "not",
    "only",
    "own",
    "same",
    "so",
    "than",
    "too",
    "very",
    "just",
    "also",
    "now",
    "here",
    "there",
    "then",
    "once",
    "if",
    "because",
    "until",
    "while",
    "about",
    "into",
    "through",
    "during",
    "before",
    "after",
    "above",
    "below",
    "between",
    "under",
    "again",
    "further",
    "any",
    "up",
    "down",
    "out",
    "off",
    "over",
    "going",
    "gonna",
    "like",
    "okay",
    "ok",
    "yeah",
    "yes",
    "um",
    "uh",
    "ah",
    "oh",
    "well",
    "right",
    "actually",
    "basically",
    "really",
    "thing",
    "things",
    "something",
    "anything",
    "everything",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round1_matches_python() {
        assert_eq!(round1(1.55), 1.6);
        assert_eq!(round1(1.5), 1.5);
        assert_eq!(round1(0.0), 0.0);
        assert_eq!(round1(2.04), 2.0);
    }

    #[test]
    fn word_frequency_filters_stopwords_and_short_words() {
        let texts = vec![
            "the transcription transcription was good".to_string(),
            "Transcription, again! Project project project.".to_string(),
        ];
        let (common, _longest) = word_frequency(&texts);
        // "the", "was", "good"(>2 but...) -> "good" kept; stopwords "the","was",
        // "again" dropped; "transcription" appears 3x, "project" 3x.
        let map: std::collections::HashMap<_, _> = common.iter().cloned().collect();
        assert_eq!(map.get("transcription"), Some(&3));
        assert_eq!(map.get("project"), Some(&3));
        assert!(!map.contains_key("the"));
        assert!(!map.contains_key("was"));
        assert!(!map.contains_key("again"));
    }

    #[test]
    fn word_frequency_strips_punctuation() {
        let texts = vec!["hello! hello, (hello) \"hello\"".to_string()];
        let (common, _) = word_frequency(&texts);
        assert_eq!(common.len(), 1);
        assert_eq!(common[0], ("hello".to_string(), 4));
    }

    #[test]
    fn longest_words_sorted_by_length() {
        let texts = vec!["cat elephant dog hippopotamus bird".to_string()];
        let (_, longest) = word_frequency(&texts);
        assert_eq!(longest[0], "hippopotamus");
        assert_eq!(longest[1], "elephant");
    }
}
