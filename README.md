# PacketVoice

Voice-to-text for developers: hold a hotkey, speak, and the cleaned-up text is
pasted at your cursor — with local Whisper transcription, AI cleanup/planning,
semantic code search, and a detailed analytics dashboard.

> **Lineage:** originally created as **VibeToText** by Dylan ([dyoburon](https://github.com/dyoburon));
> rewritten (single Tauri v2 app, Rust VADER port, native Windows/macOS builds) and continued
> as **PacketVoice** by Ian Walmsley ([packetloss404](https://github.com/packetloss404)) since 2026.
> MIT licensed; full commit history and authorship preserved.

A single cross-platform **Tauri v2** app (Rust backend + webview UI) for
**Windows, macOS, and Linux**. It replaces the four previous implementations
(Python CLI, Windows/WPF, macOS/Swift, Electron viewer); see
[`docs/tauri-migration-plan.md`](docs/tauri-migration-plan.md) for the why and how.

## Modes (push-to-talk)

Hold a modifier chord, speak, release. Each mode post-processes the transcription
differently:

| Hotkey | Mode | What it does |
|---|---|---|
| `Ctrl+Shift` | **Transcribe** | Raw speech → text (optionally with code context) |
| `Alt+Shift` | **Cleanup** | Gemini refines rambling into a clear prompt |
| `Cmd/Meta+Alt+P` | **Plan** | Gemini generates a structured implementation plan |
| `Cmd/Meta+Alt+Shift` | **Greppy** | Semantic code search injected as context |
| `Ctrl+Alt` | **History** | Open the History & Analytics window |

## Features

- **Local transcription** via `whisper-rs` (bundled whisper.cpp), with a
  technical-vocabulary prompt and a hot-reloadable custom dictionary.
- **AI cleanup & planning** via Google Gemini (`gemini-3.6-flash`). Optional —
  falls back to raw text when no API key is set.
- **Semantic code search** via the external [`greppy`](https://crates.io) CLI
  (optional; degrades gracefully when absent).
- **Auto-paste** at the cursor with a clipboard fallback.
- **Floating waveform overlay** while recording.
- **Analytics dashboard** (D3): streaks, WPM, activity heatmap, vocabulary,
  sentiment (VADER), filler words, and more.
- One shared SQLite history at `~/.vibetotext/history.db`.

## Architecture

```
src-tauri/   Rust backend (audio, whisper, hotkeys, paste, overlay, db, llm, greppy, pipeline)
src/         Webview frontend (D3 analytics dashboard + waveform overlay)
docs/        Migration plan + release guide
```

The Rust core owns all native work; the webview is the UI. State flows over Tauri
`invoke` commands and `emit` events (e.g. `history-updated` refreshes the
dashboard instantly). See the migration plan for the full module map and IPC
contract.

## Build & run

Prerequisites: Rust stable (≥ 1.94), [`cargo-tauri`](https://tauri.app) v2, a
C/C++ toolchain + CMake (for whisper.cpp), and the per-OS native deps listed in
[`docs/release.md`](docs/release.md).

```bash
cd src-tauri
cargo tauri dev      # run in development
cargo tauri build    # produce installers (msi/nsis, dmg, deb/appimage)
```

CPU-only by default. GPU backends are opt-in: `cargo tauri build --features metal|cuda|vulkan`.

## Configuration

- `~/.vibetotext/config.json` — audio device, whisper model, custom dictionary,
  codebase path (for greppy), overlay position.
- Gemini key resolution: `GEMINI_API_KEY` / `GOOGLE_API_KEY` env →
  `~/.vibetotext/.env` → legacy `config.json`. The Settings UI writes the
  private `.env` file without returning the key to the webview.
- **Models** download on first use to `~/.vibetotext/models/ggml-<model>.bin`
  (nothing is bundled); downloads are size- and checksum-verified before use.

The hidden `~/.vibetotext` path is retained as a compatibility data location so
existing PacketVoice/VibeToText history and models are not split during the rename.

The optional `local-api` feature requires `PACKETVOICE_LOCAL_API_TOKEN` and an
`Authorization: Bearer <token>` header. It remains disabled in normal builds.

## Platform notes

- **Linux:** X11 / XWayland is the supported path (global hotkeys via `rdev` are
  X11-only); native Wayland is a planned follow-up (swap the hotkey backend to
  `hotkey-listener`).
- **macOS:** grant **Accessibility** (global hotkeys + paste) and **Microphone**
  permissions on first run.

## Releasing

See [`docs/release.md`](docs/release.md) for bundling, code signing/notarization,
and the CI matrix (`.github/workflows/tauri-release.yml`).
