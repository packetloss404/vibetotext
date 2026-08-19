# Changelog

## Unreleased

### Changed
- Reverted the product rename and restored **VibeToText** as the canonical name (was briefly PacketVoice during the Tauri migration). Bundle identifier `com.vibetotext.app`, binary `vibetotext`, env var `VIBETOTEXT_LOCAL_API_TOKEN`. The `~/.vibetotext` data directory is unchanged, so existing history, models, and config carry over.

## v0.1.0 — Tauri rewrite (was PacketVoice)

### Added
- **Tauri v2 rewrite** — Single cross-platform app (Rust backend + webview UI) under `src-tauri`, replacing the previous Python and multi-platform implementations
- **Local Whisper transcription** — `whisper-rs` bindings to whisper.cpp for on-device speech-to-text (CPU by default; optional Metal/CUDA/Vulkan GPU backends)
- **Audio capture** — `cpal` for cross-platform microphone input
- **Global push-to-talk hotkeys** — `rdev` global hotkey listener for modifier-chord push-to-talk modes
- **Operational visibility** — Persistent daily logs plus dashboard readiness, permission, recording, and pipeline-failure status
- **Settings controls** — Validated microphone/model/codebase selection, write-only Gemini key management, and confirmed history clearing

### Changed
- Renamed the product to PacketVoice (briefly) while keeping `~/.vibetotext` as the data directory for continuity
- Upgraded cleanup and planning to the stable `gemini-3.6-flash` model and removed deprecated sampling parameters
- Standardized timestamps as explicit UTC and calendar analytics as local-time buckets

### Security
- Added verified, size-bounded Whisper downloads and atomic/private config writes
- Escaped transcription-derived webview content and tightened the Content Security Policy
- Added timeout/output limits for Greppy and bearer authentication/body limits for the optional localhost API

### Removed
- Python implementation retired — `llm.py`, the `google-generativeai`/`python-dotenv` dependencies, the standalone history app, and the `start-all.sh`/`stop-all.sh` startup scripts are gone, folded into the single Tauri app
