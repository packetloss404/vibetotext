//! Whisper artifact / noise-token filtering.
//!
//! Port of `Transcriber._filter_artifacts` from `src/vibetotext/transcriber.py`
//! (and the C# `WhisperTranscriber.FilterArtifacts`). Whisper occasionally emits
//! bracketed pseudo-tokens such as `[BLANK_AUDIO]`, `[silence]`, or `[inaudible]`
//! when there is little or no speech; these must be stripped before the text is
//! pasted or stored.

/// Bracketed artifact tokens to drop (matched case-insensitively, ignoring any
/// surrounding whitespace inside the brackets).
///
/// Superset of the Python/C# list (`end`, `blank_audio`, `silence`, `music`,
/// `applause`) plus the additional tokens called out for this port
/// (`inaudible`, `no_speech`).
const ARTIFACT_TOKENS: &[&str] = &[
    "end",
    "blank_audio",
    "silence",
    "music",
    "applause",
    "inaudible",
    "no_speech",
];

/// Remove bracketed Whisper artifacts and collapse the resulting whitespace.
///
/// Equivalent to the Python regex pipeline:
/// `re.sub(r'\[(?:end|blank_audio|silence|music|applause)\]', '', text, IGNORECASE)`
/// followed by `re.sub(r'\s+', ' ', text).strip()` — extended with the
/// `inaudible` / `no_speech` tokens and tolerant of inner whitespace
/// (e.g. `[ blank audio ]` / `[no speech]`).
pub fn filter(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < text.len() {
        if bytes[i] == b'[' {
            if let Some(close) = text[i + 1..].find(']') {
                let inner = &text[i + 1..i + 1 + close];
                if is_artifact(inner) {
                    // Skip the entire bracketed token.
                    i += 1 + close + 1;
                    continue;
                }
            }
        }
        // Not an artifact bracket: keep this character.
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    collapse_whitespace(&out)
}

/// Is the inner text of a `[...]` group one of the known artifact tokens?
///
/// Comparison is case-insensitive and ignores spaces/underscores so that both
/// `[BLANK_AUDIO]` and `[blank audio]` are recognized.
fn is_artifact(inner: &str) -> bool {
    let normalized: String = inner
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect();

    ARTIFACT_TOKENS
        .iter()
        .any(|tok| normalized == tok.replace('_', ""))
}

/// Collapse runs of whitespace into single spaces and trim the ends, matching
/// `re.sub(r'\s+', ' ', text).strip()`.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_blank_audio_case_insensitive() {
        assert_eq!(filter("[BLANK_AUDIO]"), "");
        assert_eq!(filter("[blank_audio]"), "");
        assert_eq!(filter("[Blank_Audio]"), "");
    }

    #[test]
    fn drops_each_known_artifact() {
        for tok in ["[end]", "[silence]", "[music]", "[applause]", "[inaudible]", "[no_speech]"] {
            assert_eq!(filter(tok), "", "token {tok} should be filtered");
        }
    }

    #[test]
    fn keeps_real_text_around_artifacts() {
        assert_eq!(
            filter("hello [silence] world"),
            "hello world"
        );
        assert_eq!(filter("ship it [blank_audio]"), "ship it");
        assert_eq!(filter("[silence] start of speech"), "start of speech");
    }

    #[test]
    fn collapses_whitespace_and_trims() {
        assert_eq!(filter("  hello   world  "), "hello world");
        assert_eq!(filter("a [silence]  b"), "a b");
    }

    #[test]
    fn tolerates_inner_whitespace_variants() {
        assert_eq!(filter("[ blank audio ]"), "");
        assert_eq!(filter("[no speech]"), "");
    }

    #[test]
    fn leaves_non_artifact_brackets_intact() {
        assert_eq!(filter("see [1] for details"), "see [1] for details");
        assert_eq!(filter("array[0] = x"), "array[0] = x");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(filter(""), "");
        assert_eq!(filter("   "), "");
    }
}
