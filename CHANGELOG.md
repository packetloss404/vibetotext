# Changelog

## Unreleased

### Added
- **Tauri v2 rewrite** — Single cross-platform app (Rust backend + webview UI) under `src-tauri`, replacing the previous Python and multi-platform implementations
- **Local Whisper transcription** — `whisper-rs` bindings to whisper.cpp for on-device speech-to-text (CPU by default; optional Metal/CUDA/Vulkan GPU backends)
- **Audio capture** — `cpal` for cross-platform microphone input
- **Global push-to-talk hotkeys** — `rdev` global hotkey listener for modifier-chord push-to-talk modes

### Removed
- Python implementation retired — `llm.py`, the `google-generativeai`/`python-dotenv` dependencies, the standalone history app, and the `start-all.sh`/`stop-all.sh` startup scripts are gone, folded into the single Tauri app
