//! Feature-gated localhost HTTP transcription endpoint (plan §2/§4).
//!
//! Revives the dropped `src/vibetotext/socket_server.py` external transcription
//! API as a small, blocking, **localhost-only** HTTP server. External processes
//! (e.g. Jarvis) can POST audio and get a Whisper transcription back, reusing
//! this app's local model resolution.
//!
//! ## Security
//! The server binds to `127.0.0.1` (loopback) on a fixed port **only** — never
//! to `0.0.0.0` — so it is unreachable from other machines. This is the entire
//! security model: no auth, loopback isolation only. Do NOT change the bind
//! address to a non-loopback interface.
//!
//! ## Protocol (mirrors `socket_server.py`)
//! The Python server spoke newline-delimited JSON over a Unix socket; this is
//! the same payload shape over HTTP so existing callers port trivially:
//!
//! - Request:  `POST /transcribe` with body
//!   `{"audio_b64": "<base64 little-endian f32 samples>", "sample_rate": 16000}`
//! - Success:  `200` `{"text": "...", "duration_ms": 450}`
//! - Error:    non-2xx `{"error": "..."}`
//!
//! `audio_b64` is base64 of the raw little-endian `f32` PCM sample bytes, exactly
//! like the Python `np.frombuffer(b64decode(audio_b64), dtype=np.float32)` path.
//!
//! ## Model
//! This endpoint loads its **own** [`Transcriber`] from
//! [`models::resolve_or_download`] using the configured `whisper_model`. Sharing
//! the running pipeline's already-loaded in-memory model via an `Arc` is a later
//! optimization (it would avoid a second model load / RAM copy); for now the
//! endpoint is self-contained so it works even if the capture pipeline failed to
//! start.
#![cfg(feature = "local-api")]

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::models;
use crate::transcribe::Transcriber;

/// Loopback-only bind address. 127.0.0.1 (NOT 0.0.0.0) is the security boundary.
const BIND_ADDR: &str = "127.0.0.1:8765";

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
/// `_app` is accepted for symmetry with the other `start(app)` entry points and
/// so a future revision can pull live state (shared model, config watcher) off
/// the [`tauri::AppHandle`] without a signature change.
pub fn start(_app: &tauri::AppHandle) -> Result<()> {
    // Bind eagerly so a port conflict surfaces to the caller now, not later in
    // the worker thread where it could only be logged.
    let server = tiny_http::Server::http(BIND_ADDR)
        .map_err(|e| anyhow::anyhow!("failed to bind local API to {BIND_ADDR}: {e}"))?;

    // Resolve the configured model once, up front, so the (potentially slow,
    // first-run download) happens lazily inside the Transcriber but the model
    // *name* is read at startup. A config read failure falls back to the
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

    std::thread::Builder::new()
        .name("local-api".into())
        .spawn(move || {
            // Lazily-loaded, shared across requests. The whisper context inside
            // is itself Mutex-guarded (not reentrant), so concurrent requests
            // serialize on inference — acceptable for this low-traffic endpoint.
            let transcriber: std::sync::OnceLock<Arc<Transcriber>> = std::sync::OnceLock::new();

            tracing::info!(addr = BIND_ADDR, "local transcription API listening (loopback only)");

            for request in server.incoming_requests() {
                handle_request(request, &transcriber, &model_name, &custom_words);
            }
        })
        .context("failed to spawn local-api server thread")?;

    Ok(())
}

/// Handle one HTTP request: route, decode, transcribe, and respond.
fn handle_request(
    mut request: tiny_http::Request,
    transcriber: &std::sync::OnceLock<Arc<Transcriber>>,
    model_name: &str,
    custom_words: &[String],
) {
    use tiny_http::Method;

    // Only POST /transcribe is supported; everything else is rejected.
    // `method()` returns `&Method`, hence the `&Method::Post` pattern.
    let is_transcribe = matches!(request.method(), &Method::Post) && request.url() == "/transcribe";
    if !is_transcribe {
        respond_error(request, 404, "not found: POST /transcribe only");
        return;
    }

    let mut body = String::new();
    if let Err(e) = std::io::Read::read_to_string(request.as_reader(), &mut body) {
        respond_error(request, 400, &format!("failed to read request body: {e}"));
        return;
    }

    match transcribe_from_json(&body, transcriber, model_name, custom_words) {
        Ok(resp) => respond_json(request, 200, &resp),
        Err(e) => respond_error(request, 400, &e.to_string()),
    }
}

/// Pure core: parse the JSON request, decode audio, run transcription, and
/// build the response. Separated from the HTTP plumbing so it is unit-testable.
fn transcribe_from_json(
    body: &str,
    transcriber: &std::sync::OnceLock<Arc<Transcriber>>,
    model_name: &str,
    custom_words: &[String],
) -> Result<TranscribeResponse> {
    let req: TranscribeRequest =
        serde_json::from_str(body).context("invalid JSON request body")?;

    let samples = decode_f32_samples(&req.audio_b64).context("failed to decode audio_b64")?;
    if samples.is_empty() {
        anyhow::bail!("empty audio data");
    }

    // NOTE: whisper expects 16 kHz mono. The Python server passed sample_rate
    // through to its transcriber; our `Transcriber::transcribe` assumes 16 kHz,
    // so a non-16k rate is surfaced as an error rather than silently mis-decoded.
    // Resampling here is a later enhancement (parity with recorder.rs resampler).
    if req.sample_rate != 16_000 {
        anyhow::bail!(
            "unsupported sample_rate {}; only 16000 Hz mono f32 is supported",
            req.sample_rate
        );
    }

    // Lazy, fallible one-time model resolution. `OnceLock::get_or_init` can't
    // return a Result, so on the first request we resolve the transcriber
    // fallibly and only cache it on success; a model-resolve/load failure
    // surfaces as a normal error response instead of panicking the worker.
    let t = match transcriber.get() {
        Some(t) => t.clone(),
        None => {
            let t = init_transcriber(model_name)?;
            // Race-safe: if another thread initialized concurrently, keep theirs.
            let _ = transcriber.set(t);
            transcriber
                .get()
                .expect("transcriber set above")
                .clone()
        }
    };

    let start = std::time::Instant::now();
    let text = t
        .transcribe(&samples, custom_words)
        .context("transcription failed")?;
    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(TranscribeResponse { text, duration_ms })
}

/// Resolve the model (download-on-first-run) and build a [`Transcriber`].
fn init_transcriber(model_name: &str) -> Result<Arc<Transcriber>> {
    let path = models::resolve_or_download(model_name)
        .with_context(|| format!("failed to resolve whisper model '{model_name}'"))?;
    let t = Transcriber::new(&path)?;
    Ok(Arc::new(t))
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
    let json = serde_json::to_string(body).unwrap_or_else(|_| {
        r#"{"error":"failed to serialize response"}"#.to_string()
    });
    send(request, status, &json);
}

/// Respond with `{"error": message}` at the given HTTP status.
fn respond_error(request: tiny_http::Request, status: u16, message: &str) {
    let body = ErrorResponse {
        error: message.to_string(),
    };
    let json = serde_json::to_string(&body)
        .unwrap_or_else(|_| r#"{"error":"unknown error"}"#.to_string());
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

/// Minimal standard-alphabet base64 decoder (RFC 4648, `+`/`/`, optional `=`
/// padding). Self-contained so the `local-api` feature pulls in no extra crate
/// just to decode the audio payload. Whitespace is ignored.
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    /// Map a base64 character to its 6-bit value, or `None` if not a symbol.
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;

    for &b in input.as_bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = val(b).ok_or_else(|| anyhow::anyhow!("invalid base64 character"))? as u32;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }

    Ok(out)
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
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
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
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(v["error"], json!("empty audio data"));
        assert_eq!(v.as_object().unwrap().len(), 1);
    }

    #[test]
    fn empty_audio_is_an_error() {
        // Empty audio_b64 decodes to zero samples -> error (matches Python
        // "Empty audio data"). Uses a never-initialized transcriber cell; the
        // empty-input check fires before any model load.
        let cell = std::sync::OnceLock::new();
        let body = json!({ "audio_b64": "", "sample_rate": 16000 }).to_string();
        let err = transcribe_from_json(&body, &cell, "small", &[]).unwrap_err();
        assert!(err.to_string().contains("empty audio data"));
    }

    #[test]
    fn non_16k_sample_rate_is_rejected_before_model_load() {
        // A non-16k rate must error out before any model resolution/load, so a
        // never-initialized transcriber cell is fine here.
        let cell = std::sync::OnceLock::new();
        // One valid f32 sample so we pass the empty check and reach the rate check.
        let b64 = base64_encode(&1.0f32.to_le_bytes());
        let body = json!({ "audio_b64": b64, "sample_rate": 44100 }).to_string();
        let err = transcribe_from_json(&body, &cell, "small", &[]).unwrap_err();
        assert!(err.to_string().contains("unsupported sample_rate"));
    }
}
