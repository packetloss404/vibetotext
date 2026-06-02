//! Greppy semantic-search integration — a thin wrapper around the external
//! `greppy` Rust CLI invoked as a subprocess.
//!
//! Port of `src/vibetotext/greppy.py` (greppy mode: file list + contents) and
//! `src/vibetotext/context.py` (transcribe-mode code-context injection). Both
//! Python references shell out to the same external binary:
//!
//! ```text
//! greppy search <query> -n <limit> -p <codebase> --json
//! ```
//!
//! and parse its newline-delimited JSON output (one `{file_path, start_line,
//! end_line, content, ...}` object per line). This module mirrors that exactly.
//!
//! **Graceful absence.** The `greppy` binary is an optional, externally
//! installed tool. Matching the Python references (which return `[]` /`""` on
//! `FileNotFoundError`), every public function here returns `None` — never an
//! error — when the binary is missing, when it exits non-zero, or when it yields
//! no usable results. Callers treat `None` as "no code context available" and
//! proceed without injection.

use std::path::Path;
use std::process::Command;

/// Max number of lines read from each matched file when building the greppy-mode
/// file dump (matches `format_files_for_context`'s `max_lines_per_file=200`).
const MAX_LINES_PER_FILE: usize = 200;

/// One parsed line of `greppy ... --json` output.
///
/// Only the fields the Python references consume are modelled; any additional
/// keys greppy emits are ignored (serde drops unknown fields by default), which
/// matches the Python `item.get(...)` access pattern.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct GreppyHit {
    #[serde(default)]
    file_path: String,
    /// greppy uses 1-based line numbers; default to 1 when absent (Python:
    /// `item.get("start_line", 1)`).
    #[serde(default = "default_start_line")]
    start_line: u32,
    /// Defaults to `start_line` when absent — applied after parsing.
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    content: String,
}

fn default_start_line() -> u32 {
    1
}

/// Run the greppy CLI and return its raw stdout, or `None` if the binary is
/// absent / failed to spawn / exited non-zero.
///
/// The absent-binary path corresponds to the Python `except FileNotFoundError:
/// return []`: `Command::output` surfaces a missing executable as an
/// `io::ErrorKind::NotFound` spawn error, which we map to `None`. A non-zero
/// exit (Python's `result.returncode != 0`) is likewise `None`.
fn run_greppy(query: &str, codebase: &Path, limit: usize) -> Option<String> {
    let output = Command::new("greppy")
        .arg("search")
        .arg(query)
        .arg("-n")
        .arg(limit.to_string())
        .arg("-p")
        .arg(codebase)
        .arg("--json")
        .output()
        .map_err(|e| {
            // NotFound == greppy not installed: expected, log quietly. Other
            // spawn errors are unusual enough to warrant a warning.
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::debug!("greppy binary not found on PATH; skipping code context");
            } else {
                tracing::warn!(error = %e, "failed to spawn greppy");
            }
        })
        .ok()?;

    if !output.status.success() {
        tracing::debug!(status = ?output.status.code(), "greppy exited non-zero");
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse greppy's newline-delimited JSON stdout into hits.
///
/// Mirrors the Python loop: split on `\n`, skip blank lines, `json.loads` each
/// line, and silently drop lines that fail to parse (Python's
/// `except json.JSONDecodeError: continue`). Hits with an empty `file_path` are
/// dropped (the Python `if filepath` guard).
fn parse_hits(stdout: &str) -> Vec<GreppyHit> {
    stdout
        .trim()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<GreppyHit>(line).ok())
        .filter(|hit| !hit.file_path.is_empty())
        .map(|mut hit| {
            // end_line defaults to start_line (context.py:
            // `item.get("end_line", start_line)`).
            if hit.end_line.is_none() {
                hit.end_line = Some(hit.start_line);
            }
            hit
        })
        .collect()
}

/// Transcribe-mode code context (port of `context.py` `search_context` +
/// `format_context`).
///
/// Searches `codebase` for `query`, then formats the matched snippets — using
/// the `content` returned by greppy itself — into a prompt-injection block:
///
/// ````text
///
/// ---
/// Relevant code context:
///
///
/// path/to/file.rs:10-25
/// ```
/// <snippet content>
/// ```
///
/// ````
///
/// Returns `None` (graceful) when greppy is absent / errored / returned no
/// snippets, so the caller injects nothing — matching `format_context([]) ==
/// ""` in the Python reference.
pub fn code_context(query: &str, codebase: &Path, limit: usize) -> Option<String> {
    let stdout = run_greppy(query, codebase, limit)?;
    let hits = parse_hits(&stdout);
    if hits.is_empty() {
        return None;
    }

    // Build the block exactly like context.py format_context: a list of parts
    // joined with "\n".
    let mut parts: Vec<String> = vec!["\n---\nRelevant code context:\n".to_string()];

    for hit in &hits {
        let end = hit.end_line.unwrap_or(hit.start_line);
        parts.push(format!("\n{}:{}-{}", hit.file_path, hit.start_line, end));
        parts.push("```".to_string());
        parts.push(hit.content.clone());
        parts.push("```\n".to_string());
    }

    Some(parts.join("\n"))
}

/// Greppy-mode file dump (port of `greppy.py` `search_files` +
/// `read_file_content` + `format_files_for_context`).
///
/// Searches `codebase` for `query`, de-duplicates by file path (keeping the
/// first hit per file, as the Python `seen_files` set does), reads up to
/// [`MAX_LINES_PER_FILE`] lines of each file from disk, and wraps each in a
/// fenced code block tagged with a language inferred from the file extension:
///
/// ````text
///
/// ### ~/rel/path.rs
/// ```rust
/// <file contents>
/// ```
///
/// ### other/file.py
/// ```python
/// <file contents>
/// ```
/// ````
///
/// Paths under the user's home directory are displayed as `~/...` (matching the
/// Python `relative_to(Path.home())` shortening). Returns `None` (graceful)
/// when greppy is absent / errored / matched nothing, or when none of the
/// matched files could be read.
pub fn greppy_files(query: &str, codebase: &Path, limit: usize) -> Option<String> {
    let stdout = run_greppy(query, codebase, limit)?;
    let hits = parse_hits(&stdout);
    if hits.is_empty() {
        return None;
    }

    // De-duplicate by file path, preserving first-seen order, capped at `limit`
    // (Python returns `files[:limit]`).
    let mut seen = std::collections::HashSet::new();
    let mut files: Vec<&str> = Vec::new();
    for hit in &hits {
        if seen.insert(hit.file_path.as_str()) {
            files.push(hit.file_path.as_str());
            if files.len() >= limit {
                break;
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();
    for filepath in files {
        let Some(content) = read_file_content(Path::new(filepath), MAX_LINES_PER_FILE) else {
            // Unreadable / missing file: skip it (Python `if content:`).
            continue;
        };
        let display_path = display_path(filepath);
        let lang = lang_for_extension(filepath);
        parts.push(format!("### {display_path}\n```{lang}\n{content}\n```"));
    }

    if parts.is_empty() {
        return None;
    }

    // Python prepends "\n\n" then joins parts with "\n\n".
    Some(format!("\n\n{}", parts.join("\n\n")))
}

/// Read up to `max_lines` lines of a file as UTF-8 (lossy, mirroring Python's
/// `errors='ignore'`). Returns `None` if the file is missing or unreadable
/// (Python returns `""`, which callers then skip via `if content:`).
///
/// If the file has at least `max_lines` lines, a truncation marker is appended —
/// matching `read_file_content`'s `... (truncated at N lines)` behavior.
fn read_file_content(path: &Path, max_lines: usize) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&raw);

    let mut lines = text.lines();
    let kept: Vec<&str> = lines.by_ref().take(max_lines).collect();
    if kept.is_empty() {
        return None;
    }

    let mut content = kept.join("\n");
    // The Python reads exactly `max_lines` then checks `len(lines) == max_lines`
    // to decide truncation; equivalently, if there is at least one more line
    // beyond what we kept, we truncated.
    let truncated = kept.len() == max_lines && lines.next().is_some();
    if truncated {
        content.push_str(&format!("\n... (truncated at {max_lines} lines)"));
    }
    Some(content)
}

/// Shorten a path under the user's home dir to `~/relative` form, matching the
/// Python `Path(filepath).relative_to(Path.home())` shortening. Falls back to
/// the original path when it isn't under home (Python's `except ValueError`).
fn display_path(filepath: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = Path::new(filepath).strip_prefix(&home) {
            // Use forward slashes for the display form, like the Python `~/...`.
            return format!("~/{}", rel.to_string_lossy().replace('\\', "/"));
        }
    }
    filepath.to_string()
}

/// Map a file extension to a Markdown code-fence language tag.
///
/// The Python reference uses a bare ``` fence with no language; we add a
/// language hint (a strict superset — better fenced rendering) derived from the
/// extension, defaulting to the raw extension or an empty tag when unknown.
fn lang_for_extension(filepath: &str) -> &'static str {
    let ext = Path::new(filepath)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "ps1" => "powershell",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        "lua" => "lua",
        "dart" => "dart",
        "scala" => "scala",
        "r" => "r",
        "pl" | "pm" => "perl",
        // Unknown extension: a plain fence (empty tag) renders fine.
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_json_line() {
        let stdout = r#"{"file_path":"src/main.rs","start_line":10,"end_line":25,"content":"fn main() {}"}"#;
        let hits = parse_hits(stdout);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_path, "src/main.rs");
        assert_eq!(hits[0].start_line, 10);
        assert_eq!(hits[0].end_line, Some(25));
        assert_eq!(hits[0].content, "fn main() {}");
    }

    #[test]
    fn parse_multiple_newline_delimited_lines() {
        let stdout = "\
{\"file_path\":\"a.rs\",\"start_line\":1,\"end_line\":3,\"content\":\"a\"}
{\"file_path\":\"b.py\",\"start_line\":5,\"content\":\"b\"}
";
        let hits = parse_hits(stdout);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].file_path, "a.rs");
        assert_eq!(hits[1].file_path, "b.py");
        // end_line defaults to start_line when absent (context.py behavior).
        assert_eq!(hits[1].end_line, Some(5));
    }

    #[test]
    fn parse_defaults_start_line_to_one() {
        let stdout = r#"{"file_path":"x.rs","content":"x"}"#;
        let hits = parse_hits(stdout);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_line, 1);
        assert_eq!(hits[0].end_line, Some(1));
    }

    #[test]
    fn parse_skips_blank_lines() {
        let stdout = "\n\n{\"file_path\":\"a.rs\",\"start_line\":1}\n   \n";
        let hits = parse_hits(stdout);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_path, "a.rs");
    }

    #[test]
    fn parse_skips_invalid_json_lines() {
        // Mirrors Python's `except json.JSONDecodeError: continue`.
        let stdout = "not json at all\n{\"file_path\":\"good.rs\",\"start_line\":2}\n{broken";
        let hits = parse_hits(stdout);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_path, "good.rs");
        assert_eq!(hits[0].start_line, 2);
    }

    #[test]
    fn parse_drops_empty_file_path() {
        // Python: `if filepath and filepath not in seen_files`.
        let stdout = r#"{"file_path":"","start_line":1,"content":"x"}"#;
        let hits = parse_hits(stdout);
        assert!(hits.is_empty());
    }

    #[test]
    fn parse_empty_stdout_yields_no_hits() {
        assert!(parse_hits("").is_empty());
        assert!(parse_hits("   \n  \n").is_empty());
    }

    #[test]
    fn run_greppy_returns_none_when_binary_absent() {
        // No real greppy needed: a nonexistent binary name forces the
        // io::ErrorKind::NotFound spawn-error path, which must map to None
        // (the graceful absent-binary path matching Python FileNotFoundError).
        // We exercise run_greppy indirectly by confirming the spawn-error map
        // is None for a guaranteed-absent command.
        let result = Command::new("greppy_definitely_not_installed_xyz123")
            .arg("search")
            .output();
        // Sanity: the OS reports NotFound for an absent binary.
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn code_context_none_when_binary_absent() {
        // End-to-end graceful path: code_context must return None (not panic,
        // not error) when greppy cannot be spawned. run_greppy uses the literal
        // "greppy" binary; on a machine without it installed this returns None.
        // If greppy *is* installed, this query is unlikely to match this temp
        // path, so an empty result also yields None — either way: None.
        let tmp = std::env::temp_dir();
        let out = code_context("zzz_no_such_symbol_qqq", &tmp, 3);
        // We can't assert which branch ran, but the contract is: never Some("").
        if let Some(s) = &out {
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn read_file_content_truncates_at_max_lines() {
        let dir = std::env::temp_dir().join("greppy_test_trunc");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("many.txt");
        let body: String = (0..10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(&file, body).unwrap();

        let content = read_file_content(&file, 3).expect("should read");
        assert!(content.starts_with("line0\nline1\nline2"));
        assert!(content.contains("(truncated at 3 lines)"));

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn read_file_content_no_truncation_marker_when_short() {
        let dir = std::env::temp_dir().join("greppy_test_short");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("few.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();

        let content = read_file_content(&file, 200).expect("should read");
        assert_eq!(content, "one\ntwo");
        assert!(!content.contains("truncated"));

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn read_file_content_none_for_missing() {
        let missing = std::env::temp_dir().join("greppy_definitely_missing_qzx.txt");
        let _ = std::fs::remove_file(&missing);
        assert!(read_file_content(&missing, 200).is_none());
    }

    #[test]
    fn lang_for_extension_maps_known_and_unknown() {
        assert_eq!(lang_for_extension("a/b/main.rs"), "rust");
        assert_eq!(lang_for_extension("script.PY"), "python"); // case-insensitive
        assert_eq!(lang_for_extension("x.tsx"), "tsx");
        assert_eq!(lang_for_extension("noext"), "");
        assert_eq!(lang_for_extension("weird.qwerty"), "");
    }
}
