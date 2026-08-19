//! Feature-gated localhost HTTP transcription endpoint (plan §2/§4).
//!
//! Revives the dropped `src/vibetotext/socket_server.py` external transcription
//! API as a small, blocking, **localhost-only** HTTP server. External processes
//! (e.g. Jarvis) can POST audio and get a Whisper transcription back, reusing
//! this app's local model resolution.
//!
//! ## Security
//! The server binds to `127.0.0.1` (loopback) on a fixed port **only** — never
//! to `0.0.0.0` — and requires a bearer token from
//! `VIBETOTEXT_LOCAL_API_TOKEN`. Do NOT change the bind address to a
//! non-loopback interface.
//!
//! ## Protocol (mirrors `socket_server.py`)
//! The Python server spoke newline-delimited JSON over a Unix socket; this is
//! the same payload shape over HTTP so existing callers port trivially:
//!
//! - Request:  `POST /transcribe` with body
//!   `{"audio_b64": "<base64 little-endian f32 samples>", "sample_rate": 16000}`
//!   and `Authorization: Bearer <VIBETOTEXT_LOCAL_API_TOKEN>`.
//! - Success:  `200` `{"text": "...", "duration_ms": 450}`
//! - Error:    non-2xx `{"error": "..."}`
//!
//! `audio_b64` is base64 of the raw little-endian `f32` PCM sample bytes, exactly
//! like the Python `np.frombuffer(b64decode(audio_b64), dtype=np.float32)` path.
//!
//! ## Model
//! The endpoint borrows the **shared** [`Transcriber`] from
//! [`crate::state::AppState`], so the same in-memory model instance serves the
//! capture pipeline and the HTTP endpoint. The first request triggers the
//! model download + load (multi-second cost); subsequent requests are
//! refcount-bump fast. The endpoint returns `503 Service Unavailable` if the
//! model has not finished loading — clients can retry.
#![cfg(feature = "local-api")]

use std::io::Read;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::config::AppConfig;
use crate::state::AppState;
use crate::transcribe::Transcriber;

/// Loopback-only bind address. Never expose this service on a non-loopback interface.
const BIND_ADDR: &str = "127.0.0.1:8765";
const MAX_REQUEST_BODY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_AUDIO_SAMPLES: usize = 16_000 * 60 * 5;

/// Request body for `POST /transcribe` (mirrors `socket_server.py`'s payload).
#[derive(Debug, Deserialize)]
struct TranscribeRequest {
    /// Base64 of raw little-endian `f32` PCM samples (mono).
    audio_b64: String,
    /// Sample rate of the supplied audio, in Hz. Defaults to 16 kHz (whisper's
    /// native rate) when omitted, matching the Python default.
    #[serde(default = "default_sample_rate")]
    sample_rate: u32,
}

fn default_sample_rate() -> u32 {
    16_000
}

/// Success response for `POST /transcribe`.
#[derive(Debug, Serialize)]
struct TranscribeResponse {
    /// The transcribed text.
    text: String,
    /// Wall-clock inference time in milliseconds (matches Python `duration_ms`).
    duration_ms: u64,
}

/// Error response body (any non-2xx status).
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

/// Start the localhost transcription server in a background daemon thread.
///
/// Binds to [`BIND_ADDR`] (loopback only) and serves `POST /transcribe`. The
/// server thread is detached; it lives for the duration of the process. A bind
/// failure (e.g. port already in use) is returned to the caller, which logs it
/// without aborting app startup.
///
/// The endpoint shares the capture pipeline's whisper `Transcriber` via
/// [`AppState`], so the same in-memory model instance serves both. If the model
/// has not finished loading (download + first-use load), requests get
/// `503 Service Unavailable` until it is.
pub fn start(app: &tauri::AppHandle) -> Result<()> {
    let token = std::env::var("VIBETOTEXT_LOCAL_API_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .context("VIBETOTEXT_LOCAL_API_TOKEN must be set when the local-api feature is enabled")?;
    // Bind eagerly so a port conflict surfaces to the caller now, not later in
    // the worker thread where it could only be logged.
    let server = tiny_http::Server::http(BIND_ADDR)
        .map_err(|e| anyhow::anyhow!("failed to bind local API to {BIND_ADDR}: {e}"))?;

    // Resolve the configured model name once, up front, so the (potentially
    // slow, first-run download) happens lazily inside the Transcriber but the
    // model *name* is read at startup. A config read failure falls back to the
    // default model rather than disabling the endpoint.
    let model_name = AppConfig::load()
        .map(|c| c.whisper_model)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "local API: config load failed; using default model");
            crate::config::DEFAULT_WHISPER_MODEL.to_string()
        });

    // Custom dictionary words bias whisper, same as the capture pipeline. Read
    // once at startup (hot-reload is a later optimization for this endpoint).
    let custom_words: Vec<String> = AppConfig::load()
        .map(|c| c.custom_dictionary)
        .unwrap_or_default();

    // Clone the `AppState` handle so the worker thread can borrow the shared
    // `Transcriber` (built lazily by either the capture pipeline or the first
    // HTTP request) instead of loading its own.
    let state: AppState = app.state::<AppState>().inner().clone();

    std::thread::Builder::new()
        .name("vibetotext-local-api".into())
        .spawn(move || {
            tracing::info!(
                addr = BIND_ADDR,
                "local transcription API listening (loopback only, shared model)"
            );

            for request in server.incoming_requests() {
                handle_request(request, &state, &model_name, &custom_words, &token);
            }
        })
        .context("failed to spawn local-api server thread")?;

    Ok(())
}

/// Handle one HTTP request: route, decode, transcribe, and respond.
fn handle_request(
    mut request: tiny_http::Request,
    state: &AppState,
    model_name: &str,
    custom_words: &[String],
    token: &str,
) {
    use tiny_http::Method;

    // Only POST /transcribe is supported; everything else is rejected.
    // `method()` returns `&Method`, hence the `&Method::Post` pattern.
    let is_transcribe = matches!(request.method(), &Method::Post) && request.url() == "/transcribe";
    if !is_transcribe {
        respond_error(request, 404, "not found: POST /transcribe only");
        return;
    }

    let expected = format!("Bearer {token}");
    let authorized = request
        .headers()
        .iter()
        .any(|header| header.field.equiv("Authorization") && header.value.as_str() == expected);
    if !authorized {
        respond_error(request, 401, "missing or invalid bearer token");
        return;
    }

    let mut body = String::new();
    let read_result = request
        .as_reader()
        .take(MAX_REQUEST_BODY_BYTES + 1)
        .read_to_string(&mut body);
    if let Err(e) = read_result {
        respond_error(request, 400, &format!("failed to read request body: {e}"));
        return;
    }
    if body.len() as u64 > MAX_REQUEST_BODY_BYTES {
        respond_error(request, 413, "request body exceeds 32 MiB limit");
        return;
    }

    match transcribe_from_json(&body, state, model_name, custom_words) {
        Ok(resp) => respond_json(request, 200, &resp),
        Err(TranscribeError::BadRequest(e)) => respond_error(request, 400, &e.to_string()),
        Err(TranscribeError::ServerError(e)) => {
            respond_error(request, 500, &format!("transcription failed: {e}"))
        }
    }
}

/// Errors that `transcribe_from_json` can return, mapped to HTTP status codes by
/// [`handle_request`]. `BadRequest` → 400 (client sent something malformed),
/// `ServerError` → 500 (model or inference failure on our side). Splitting this
/// way keeps the HTTP layer thin and the testable core (`transcribe_from_json`)
/// unaware of status codes.
enum TranscribeError {
    BadRequest(anyhow::Error),
    ServerError(anyhow::Error),
}

impl std::fmt::Display for TranscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscribeError::BadRequest(e) => write!(f, "{e}"),
            TranscribeError::ServerError(e) => write!(f, "{e}"),
        }
    }
}

/// Pure core: parse the JSON request, decode audio, run transcription, and
/// build the response. Separated from the HTTP plumbing so it is unit-testable.
fn transcribe_from_json(
    body: &str,
    state: &AppState,
    model_name: &str,
    custom_words: &[String],
) -> Result<TranscribeResponse, TranscribeError> {
    let req: TranscribeRequest = serde_json::from_str(body).map_err(|e| {
        TranscribeError::BadRequest(anyhow::anyhow!("invalid JSON request body: {e}"))
    })?;

    let samples = decode_f32_samples(&req.audio_b64)
        .map_err(|e| TranscribeError::BadRequest(e.context("failed to decode audio_b64")))?;
    if samples.is_empty() {
        return Err(TranscribeError::BadRequest(anyhow::anyhow!(
            "empty audio data"
        )));
    }
    if samples.len() > MAX_AUDIO_SAMPLES {
        return Err(TranscribeError::BadRequest(anyhow::anyhow!(
            "audio exceeds the five-minute request limit"
        )));
    }
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err(TranscribeError::BadRequest(anyhow::anyhow!(
            "audio contains non-finite samples"
        )));
    }

    // NOTE: whisper expects 16 kHz mono. The Python server passed sample_rate
    // through to its transcriber; our `Transcriber::transcribe` assumes 16 kHz,
    // so a non-16k rate is surfaced as an error rather than silently mis-decoded.
    // Resampling here is a later enhancement (parity with recorder.rs resampler).
    if req.sample_rate != 16_000 {
        return Err(TranscribeError::BadRequest(anyhow::anyhow!(
            "unsupported sample_rate {}; only 16000 Hz mono f32 is supported",
            req.sample_rate
        )));
    }

    // Resolve the shared Transcriber via the AppState. The first caller (capture
    // pipeline prewarm or a concurrent local-api request) pays the model-load
    // cost; everyone else gets a refcount-bump clone.
    let t: Arc<Transcriber> = state
        .ensure_transcriber(model_name)
        .map_err(|e| TranscribeError::ServerError(e.context("whisper model not loaded")))?;

    let start = std::time::Instant::now();
    let text = t
        .transcribe(&samples, custom_words)
        .map_err(|e| TranscribeError::ServerError(e.context("transcription failed")))?;
    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(TranscribeResponse { text, duration_ms })
}

/// Decode base64 of raw little-endian `f32` PCM bytes into samples.
///
/// Mirrors the Python `np.frombuffer(b64decode(audio_b64), dtype=np.float32)`.
/// A byte length that is not a multiple of 4 is an error (truncated/corrupt
/// payload) rather than silently dropping a trailing partial sample.
fn decode_f32_samples(audio_b64: &str) -> Result<Vec<f32>> {
    let bytes = base64_decode(audio_b64).context("invalid base64")?;
    if bytes.len() % 4 != 0 {
        anyhow::bail!(
            "audio byte length {} is not a multiple of 4 (not f32-aligned)",
            bytes.len()
        );
    }
    let samples = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok(samples)
}

/// Serialize `body` to JSON and respond with the given HTTP status.
fn respond_json<T: Serialize>(request: tiny_http::Request, status: u16, body: &T) {
    let json = serde_json::to_string(body)
        .unwrap_or_else(|_| r#"{"error":"failed to serialize response"}"#.to_string());
    send(request, status, &json);
}

/// Respond with `{"error": message}` at the given HTTP status.
fn respond_error(request: tiny_http::Request, status: u16, message: &str) {
    let body = ErrorResponse {
        error: message.to_string(),
    };
    let json =
        serde_json::to_string(&body).unwrap_or_else(|_| r#"{"error":"unknown error"}"#.to_string());
    send(request, status, &json);
}

/// Send a JSON HTTP response, logging (not propagating) any write failure.
fn send(request: tiny_http::Request, status: u16, json: &str) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header is valid");
    let response = tiny_http::Response::from_string(json)
        .with_status_code(status)
        .with_header(header);
    if let Err(e) = request.respond(response) {
        tracing::warn!(error = %e, "local API: failed to send response");
    }
}

/// Strict standard-alphabet base64 decoder (RFC 4648, `+`/`/`, optional `=`
/// padding). Whitespace is ignored for newline-delimited clients.
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    use base64::Engine;

    let compact: String = input
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    STANDARD
        .decode(&compact)
        .or_else(|_| STANDARD_NO_PAD.decode(&compact))
        .context("invalid standard base64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Reference base64 encoder for building test fixtures (standard alphabet).
    fn base64_encode(data: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
            out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn base64_roundtrip_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");

        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_decode_ignores_whitespace_and_rejects_garbage() {
        assert_eq!(base64_decode("Zm9v\nYm Fy").unwrap(), b"foobar");
        assert!(base64_decode("not*valid").is_err());
    }

    #[test]
    fn decode_f32_samples_roundtrips_little_endian() {
        let samples = [0.0f32, 1.0, -1.0, 0.5, -0.25];
        let mut bytes = Vec::new();
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let b64 = base64_encode(&bytes);

        let decoded = decode_f32_samples(&b64).unwrap();
        assert_eq!(decoded, samples);
    }

    #[test]
    fn decode_f32_samples_rejects_misaligned_length() {
        // 5 bytes -> not a multiple of 4.
        let b64 = base64_encode(&[1u8, 2, 3, 4, 5]);
        assert!(decode_f32_samples(&b64).is_err());
    }

    #[test]
    fn request_json_shape_parses_with_defaults() {
        // Full shape (mirrors socket_server.py request payload).
        let body = json!({
            "audio_b64": "Zm9v",
            "sample_rate": 16000
        })
        .to_string();
        let req: TranscribeRequest = serde_json::from_str(&body).unwrap();
        assert_eq!(req.audio_b64, "Zm9v");
        assert_eq!(req.sample_rate, 16_000);

        // sample_rate omitted -> defaults to 16 kHz (Python default).
        let body = json!({ "audio_b64": "Zm9v" }).to_string();
        let req: TranscribeRequest = serde_json::from_str(&body).unwrap();
        assert_eq!(req.sample_rate, 16_000);
    }

    #[test]
    fn request_json_rejects_missing_audio() {
        let body = json!({ "sample_rate": 16000 }).to_string();
        assert!(serde_json::from_str::<TranscribeRequest>(&body).is_err());
    }

    #[test]
    fn success_response_json_shape() {
        let resp = TranscribeResponse {
            text: "hello world".to_string(),
            duration_ms: 450,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(v["text"], json!("hello world"));
        assert_eq!(v["duration_ms"], json!(450));
        // No stray keys.
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn error_response_json_shape() {
        let resp = ErrorResponse {
            error: "empty audio data".to_string(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(v["error"], json!("empty audio data"));
        assert_eq!(v.as_object().unwrap().len(), 1);
    }

    #[test]
    fn empty_audio_is_an_error() {
        // Empty audio_b64 decodes to zero samples -> error. The empty-input
        // check fires before any model load, so we use a fresh AppState whose
        // transcriber is still None.
        let state = AppState::new().expect("app state");
        let body = json!({ "audio_b64": "", "sample_rate": 16000 }).to_string();
        let err = transcribe_from_json(&body, &state, "small", &[]).unwrap_err();
        assert!(matches!(err, TranscribeError::BadRequest(_)));
        let msg = format!("{err}");
        assert!(msg.contains("empty audio data"), "got: {msg}");
    }

    #[test]
    fn non_16k_sample_rate_is_rejected_before_model_load() {
        // A non-16k rate must error out before any model resolution/load, so a
        // never-initialized AppState is fine here.
        let state = AppState::new().expect("app state");
        // One valid f32 sample so we pass the empty check and reach the rate check.
        let b64 = base64_encode(&1.0f32.to_le_bytes());
        let body = json!({ "audio_b64": b64, "sample_rate": 44100 }).to_string();
        let err = transcribe_from_json(&body, &state, "small", &[]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unsupported sample_rate"), "got: {msg}");
    }

    #[test]
    fn non_finite_samples_are_rejected_before_model_load() {
        let state = AppState::new().expect("app state");
        let b64 = base64_encode(&f32::NAN.to_le_bytes());
        let body = json!({ "audio_b64": b64, "sample_rate": 16000 }).to_string();
        let err = transcribe_from_json(&body, &state, "small", &[]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("non-finite"), "got: {msg}");
    }
}
