//! Gemini LLM client (blocking reqwest), ported from `src/vibetotext/llm.py`.
//!
//! Phase 4 contract:
//! - [`cleanup_text`] — refine rambling transcription into a clean prompt.
//! - [`generate_plan`] — turn a voice description into a short implementation plan.
//!
//! Model id is **`gemini-3.5-flash`** (current GA per plan §3; the Python/macOS
//! reference used the `gemini-3-flash-preview` preview id and the old Windows app
//! used the now-shut-down `gemini-2.0-flash`).
//!
//! Both functions return `Err` on any HTTP or parse failure so the pipeline caller
//! can fall back to the raw transcribed text.

pub mod prompts;

use anyhow::{anyhow, Context};
use serde::Deserialize;
use std::time::Duration;

/// Gemini model id. GA / stable per plan §3.
const MODEL: &str = "gemini-3.5-flash";

/// Per-request timeout, matching the Python `request_options={"timeout": 30}`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// Generation parameters, ported verbatim from llm.py.
const CLEANUP_TEMPERATURE: f64 = 0.3;
const CLEANUP_MAX_OUTPUT_TOKENS: u32 = 2048;
const PLAN_TEMPERATURE: f64 = 0.4;
const PLAN_MAX_OUTPUT_TOKENS: u32 = 4096;

// ---------------------------------------------------------------------------
// Response shape (subset of the `generateContent` JSON we care about).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<Content>,
}

#[derive(Debug, Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
struct Part {
    #[serde(default)]
    text: Option<String>,
}

// ---------------------------------------------------------------------------
// Prompt + request body assembly.
// ---------------------------------------------------------------------------

/// Substitute the user's `text` into a `{text}`-templated prompt.
///
/// Mirrors Python's `PROMPT.format(text=text)`. Only the `{text}` placeholder is
/// replaced, so literal braces elsewhere in the template (e.g. the plan's
/// markdown fences) are preserved untouched.
fn assemble_prompt(template: &str, text: &str) -> String {
    template.replace("{text}", text)
}

/// Build the `generateContent` POST body as JSON.
fn request_body(prompt: &str, temperature: f64, max_output_tokens: u32) -> serde_json::Value {
    serde_json::json!({
        "contents": [{
            "parts": [{ "text": prompt }]
        }],
        "generationConfig": {
            "temperature": temperature,
            "maxOutputTokens": max_output_tokens,
        }
    })
}

/// Endpoint URL for the GA model, with the API key as a query param (matches the
/// REST convention used by the Google generative-language API).
fn endpoint(api_key: &str) -> String {
    format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{MODEL}:generateContent?key={api_key}"
    )
}

/// Extract `candidates[0].content.parts[0].text`, trimmed.
///
/// Returns `Err` when the response carries no usable text (no candidates, no
/// parts, empty/whitespace text) — the same "treat as failure, fall back to raw"
/// semantics as the Python `if response.text:` guard.
fn extract_text(body: &str) -> anyhow::Result<String> {
    let resp: GenerateContentResponse =
        serde_json::from_str(body).context("failed to parse Gemini response JSON")?;

    let text = resp
        .candidates
        .into_iter()
        .next()
        .and_then(|c| c.content)
        .and_then(|c| c.parts.into_iter().next())
        .and_then(|p| p.text)
        .ok_or_else(|| anyhow!("Gemini response contained no candidate text"))?;

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Gemini response text was empty"));
    }
    Ok(trimmed.to_string())
}

// ---------------------------------------------------------------------------
// Core request.
// ---------------------------------------------------------------------------

fn generate(
    text: &str,
    api_key: &str,
    template: &str,
    temperature: f64,
    max_output_tokens: u32,
) -> anyhow::Result<String> {
    if api_key.trim().is_empty() {
        return Err(anyhow!("no Gemini API key configured"));
    }

    let prompt = assemble_prompt(template, text);
    let body = request_body(&prompt, temperature, max_output_tokens);
    // Serialize manually (and set Content-Type) rather than using
    // `RequestBuilder::json`, which would require reqwest's `json` feature that
    // isn't enabled in Cargo.toml.
    let body_str = serde_json::to_string(&body).context("failed to serialize request body")?;

    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to build reqwest client")?;

    let response = client
        .post(endpoint(api_key))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body_str)
        .send()
        .context("Gemini request failed")?;

    let status = response.status();
    let payload = response
        .text()
        .context("failed to read Gemini response body")?;

    if !status.is_success() {
        // Include a short slice of the body for diagnostics without dumping a
        // huge payload into the logs.
        let snippet: String = payload.chars().take(500).collect();
        return Err(anyhow!("Gemini HTTP error {status}: {snippet}"));
    }

    extract_text(&payload)
}

// ---------------------------------------------------------------------------
// Public Phase 4 contract.
// ---------------------------------------------------------------------------

/// Clean up rambling transcribed `text` into a clear, refined prompt.
///
/// Port of `cleanup_text` in `llm.py` (temperature 0.3, maxOutputTokens 2048).
/// Returns `Err` on HTTP/parse failure; the caller falls back to the raw text.
pub fn cleanup_text(text: &str, api_key: &str) -> anyhow::Result<String> {
    generate(
        text,
        api_key,
        prompts::CLEANUP_PROMPT,
        CLEANUP_TEMPERATURE,
        CLEANUP_MAX_OUTPUT_TOKENS,
    )
}

/// Generate a short markdown implementation plan from rambling `text`.
///
/// Port of `generate_implementation_plan` in `llm.py` (temperature 0.4,
/// maxOutputTokens 4096). Returns `Err` on HTTP/parse failure; the caller falls
/// back to the raw text.
pub fn generate_plan(text: &str, api_key: &str) -> anyhow::Result<String> {
    generate(
        text,
        api_key,
        prompts::IMPLEMENTATION_PLAN_PROMPT,
        PLAN_TEMPERATURE,
        PLAN_MAX_OUTPUT_TOKENS,
    )
}

// ---------------------------------------------------------------------------
// Tests — prompt assembly + response parsing only. No network calls.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_prompt_substitutes_text() {
        let out = assemble_prompt("before {text} after", "HELLO");
        assert_eq!(out, "before HELLO after");
    }

    #[test]
    fn assemble_cleanup_includes_user_text_and_template_body() {
        let out = assemble_prompt(prompts::CLEANUP_PROMPT, "make a button red");
        assert!(out.contains("make a button red"));
        assert!(out.contains("expert prompt optimizer"));
        // The {text} placeholder must be fully consumed.
        assert!(!out.contains("{text}"));
    }

    #[test]
    fn assemble_plan_preserves_literal_braces_in_template() {
        let out = assemble_prompt(prompts::IMPLEMENTATION_PLAN_PROMPT, "add login");
        assert!(out.contains("add login"));
        assert!(!out.contains("{text}"));
        // Placeholders like `[Feature Name]` are literal and must survive.
        assert!(out.contains("[Feature Name]"));
        assert!(out.contains("Plan:"));
    }

    #[test]
    fn request_body_has_expected_shape() {
        let body = request_body("PROMPT", 0.3, 2048);
        assert_eq!(body["contents"][0]["parts"][0]["text"], "PROMPT");
        assert_eq!(body["generationConfig"]["temperature"], 0.3);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 2048);
    }

    #[test]
    fn endpoint_uses_ga_model_and_key() {
        let url = endpoint("SECRET_KEY");
        assert!(url.contains("gemini-3.5-flash:generateContent"));
        assert!(url.contains("key=SECRET_KEY"));
        assert!(url.starts_with(
            "https://generativelanguage.googleapis.com/v1beta/models/"
        ));
    }

    #[test]
    fn extract_text_parses_sample_candidates_json() {
        let sample = r#"{
            "candidates": [
                {
                    "content": {
                        "parts": [
                            { "text": "  Refined output here.  " }
                        ],
                        "role": "model"
                    },
                    "finishReason": "STOP"
                }
            ]
        }"#;
        let got = extract_text(sample).expect("should parse");
        assert_eq!(got, "Refined output here.");
    }

    #[test]
    fn extract_text_errors_on_empty_candidates() {
        let sample = r#"{ "candidates": [] }"#;
        assert!(extract_text(sample).is_err());
    }

    #[test]
    fn extract_text_errors_on_missing_content() {
        // Mirrors a safety-blocked response with a candidate but no content.
        let sample = r#"{ "candidates": [ { "finishReason": "SAFETY" } ] }"#;
        assert!(extract_text(sample).is_err());
    }

    #[test]
    fn extract_text_errors_on_whitespace_only_text() {
        let sample = r#"{
            "candidates": [
                { "content": { "parts": [ { "text": "   \n  " } ] } }
            ]
        }"#;
        assert!(extract_text(sample).is_err());
    }

    #[test]
    fn extract_text_errors_on_malformed_json() {
        assert!(extract_text("not json at all").is_err());
    }

    #[test]
    fn generate_errors_on_empty_api_key() {
        // Empty key short-circuits before any network call.
        assert!(cleanup_text("hi", "").is_err());
        assert!(generate_plan("hi", "   ").is_err());
    }
}
