# VibeToText → Single Tauri App: Migration Plan

**Status:** Approved, ready to execute. Work happens on branch `tauri-migration`; `main` keeps the
four existing apps runnable until the final decommission phase.

## 1. Goal

Collapse the four current implementations into **one cross-platform Tauri v2 app** (Rust backend +
webview frontend), eliminating the maintenance drift that produced the current inconsistencies
(Windows missing greppy/plan modes, mismatched Gemini model ids, a `sentiment` column that only the
Python writer creates).

### What gets deleted at the end
`src/vibetotext/` (Python CLI), `windows-native/` (C#/WPF), `macos-native/` (Swift), `history-app/`
(Electron shell — its UI code is *ported*, not deleted), `packaging/`, PyInstaller specs, helper
scripts, `pyproject.toml`.

## 2. Decisions (locked)

| Decision | Choice |
|---|---|
| Python CLI | **Killed** — the one Tauri app does everything. |
| Platforms | **Windows + macOS + Linux** (Linux = X11/XWayland first; native Wayland is a documented follow-up). |
| Frontend | **Reuse the Electron `history-app` web UI** (HTML/CSS/D3 `analytics.js`) near-verbatim now; possible rebuild later. |
| Sentiment | **Full VADER port now** (`sentiment.rs` + vendored lexicon) so historical charts don't drift. |
| GPU | **CPU-only MVP**; `metal`/`cuda`/`vulkan` cargo features scaffolded but off — opt-in builds post-MVP. |
| External socket API | **Revive** as a feature-gated **localhost HTTP** endpoint (`local_api.rs`) reusing the in-process whisper model. |

## 3. Research findings (2026-06)

- **Gemini model id:** use **`gemini-3.5-flash`** (GA/Stable, best price-performance for coding/agentic
  via `generateContent`). `gemini-2.0-flash` (old Windows app) is **shut down** as of 2026-06-01.
  `gemini-3-flash-preview` (Python/macOS) was a preview id. Key resolution order:
  `GEMINI_API_KEY` → `GOOGLE_API_KEY` env → `~/.vibetotext/.env` → `config.json` `gemini_api_key`.
- **Hotkeys:** `rdev` `listen` is **X11-only** and requires **macOS Accessibility** (matches our
  X11-first decision). Abstract the hotkey listener behind a trait; swap to **`hotkey-listener`**
  (evdev Linux + rdev macOS, maintained Jan 2026) when we add native Wayland.
- **Paste:** `enigo` for cross-platform input simulation; always copy-to-clipboard first so a paste
  failure degrades to manual paste + notification sound (port the fallback semantics from
  `src/vibetotext/output.py`).
- **Toolchain on this machine:** Rust 1.94, cargo-tauri 2.10 (v2), Node 24, cmake 4.3, gcc (MinGW),
  git ✓. Caveats: **no MSVC `cl`** (Tauri/whisper.cpp default to the `msvc` Windows target — may need
  MSVC Build Tools or the `gnu` target), and **no `~/.vibetotext`** yet (no local DB to test
  migrations against — generate fixtures). macOS/Linux builds and all runtime behavior (mic, global
  hotkeys, paste, overlay) **cannot be verified in this environment** — those need real per-OS CI/hardware.

## 4. Target layout

During migration the new app lives in **`app/`** (so it doesn't collide with the Python `src/`).
The decommission phase relocates it to the repo root.

```
app/
├─ src-tauri/
│  ├─ Cargo.toml                 # deps + GPU feature flags (off by default)
│  ├─ tauri.conf.json            # windows, bundle, tray, capabilities
│  ├─ capabilities/default.json
│  └─ src/
│     ├─ main.rs                 # builder, plugins, tray, single-instance, windows
│     ├─ state.rs                # AppState (Arc<…>), Pipeline handle
│     ├─ config.rs               # config.json load/save/watch; preserves unknown keys
│     ├─ db/{mod,schema,entries,stats}.rs   # rusqlite + PRAGMA user_version migrations
│     ├─ audio/{recorder,waveform,devices}.rs  # cpal + rustfft (waveform math matches recorder.py)
│     ├─ transcribe/{mod,prompt,artifacts}.rs  # whisper-rs + TECH_PROMPT + dictionary + filters
│     ├─ sentiment.rs            # VADER port (vendored lexicon)
│     ├─ llm/{mod,prompts}.rs    # reqwest Gemini client (gemini-3.5-flash)
│     ├─ greppy.rs               # std::process greppy CLI wrapper
│     ├─ paste.rs                # enigo + clipboard fallback
│     ├─ hotkey/{mod,permissions}.rs   # rdev behind a trait; macOS AXIsProcessTrusted
│     ├─ pipeline.rs             # orchestrator (port of cli.py on_start/on_stop, actor model)
│     ├─ models.rs               # ggml model resolution + download-on-first-run
│     ├─ overlay.rs              # transparent click-through waveform window
│     ├─ tray.rs / events.rs / commands.rs
│     └─ local_api.rs            # feature-gated localhost HTTP transcription endpoint
└─ src/                          # frontend, from history-app/
   ├─ index.html / styles.css / analytics.js / lib/d3.min.js   # near-verbatim
   ├─ renderer.js               # rewritten: Node/browser calls → invoke/listen
   └─ api.js                    # thin shim over invoke()
```

## 5. IPC + event contract

**Commands (frontend → Rust via `invoke`)** — one per current Node/browser coupling point in
`history-app/renderer.js`:
`get_entries{mode?,limit?}`, `get_statistics{mode?}`, `clear_history`, `load_config`,
`save_config(partial)` (preserve unknown keys), `list_audio_devices` (cpal, replaces
`navigator.mediaDevices`), `set_audio_device`, `get_dictionary`/`add_word`/`remove_word`,
`set_whisper_model` (hot-reload, no restart), `set_orb_position`, `request_accessibility` (macOS).
`restartEngine`/`restart.sh` is **deleted** (single process; settings hot-apply).

**Events (Rust → frontend via `emit`):**
`history-updated` (after each write — replaces chokidar watch + 5s poll), `recording-state{recording,mode}`,
`waveform-levels[25]`, `pipeline-status{phase,mode}`, `permission-needed{kind}`.

## 6. Canonical data model

**`entries`** (final): `id, text, mode, timestamp(ISO), word_count, duration_seconds, wpm, sentiment REAL`,
index `idx_timestamp(timestamp DESC)`.

**Migrations** (gate on `PRAGMA user_version`, which all existing DBs report as 0):
1. `CREATE TABLE IF NOT EXISTS entries(... sentiment REAL)` + index.
2. `ALTER TABLE entries ADD COLUMN sentiment REAL`, swallowing "duplicate column" (upgrades C#/Swift DBs).
3. Backfill `sentiment IS NULL` rows via the Rust VADER port.
4. Import legacy `history.json` if present, then rename to `.json.migrated`.
5. Back up `history.db` → `history.db.pre-tauri.bak` once before first migration; set `user_version = 1`.
   Connection: `busy_timeout=30000`, IMMEDIATE write txns, WAL.

**`config.json`:** union of all three readers, snake_case keys, **preserve unknown keys on save**.

**Models:** `~/.vibetotext/models/ggml-<model>.bin` (ggml unifies all three old bindings); download-on-first-run.

## 7. Execution: per-phase agent workflow

Each phase is run by the **same reusable workflow** (`.claude` workflow, invoked per phase):

1. **Scaffold/lead build** (1 agent) where the phase needs a foundation.
2. **Parallel builders** (up to 4 total incl. lead) — each owns **disjoint files** to avoid conflicts.
3. **Verify** (1 agent) — authoritative `cargo build` / `cargo check` / `npm` build; reports compile status.
4. **Peer review** (2 agents, parallel) — independent review of the phase diff (correctness + adheres to
   this plan + nothing silently dropped), structured verdict.
5. **Commit** — done from the main loop after the workflow returns and the build is verified (avoids
   parallel git races); only real, building progress is committed to `tauri-migration`.

> Environment honesty: builds are verified on **Windows only**; macOS/Linux compilation and all runtime
> behavior are deferred to per-OS CI/hardware. Code for those targets is written-but-unverified and is
> labeled as such in commit messages.

### Phase sequence

| Phase | Goal / demo gate | Builders (disjoint ownership) |
|---|---|---|
| **Spike** | Prove the 2 real risks: `rdev` modifier-only push-to-talk (Win/X11, macOS perm path) + `whisper-rs` CPU build & transcribe. **Hard go/no-go.** | scaffold; rdev spike; whisper-rs spike; cpal spike |
| **0** | Scaffold + `rusqlite` + canonical schema/migrations + **VADER port** + backfill + single-instance/window-state. Validated against fixture DBs. | db/schema+migrations; sentiment(VADER); config; state/main wiring |
| **1** | UI port: `analytics.js`/d3/HTML/CSS verbatim; `renderer.js`→`api.js`+invoke/listen. **Demo: dashboard live on real DB.** | api.js shim; renderer rewrite; commands (get_entries/stats/config); tray+window-state |
| **2** | Audio + transcription: cpal + rustfft waveform (match `recorder.py`), whisper-rs + TECH_PROMPT + dict + filters, model download. | recorder; waveform; transcribe+prompt+artifacts; models+devices |
| **3** | Hotkeys + paste + overlay: rdev chord state machine (port C#/Swift), enigo paste + fallback, transparent click-through overlay, macOS perms. | hotkey(rdev)+trait; permissions; paste(enigo); overlay |
| **4** | Pipeline + all 4 modes (transcribe/cleanup/plan/greppy — **closes Windows gap**); VADER on write; Gemini `gemini-3.5-flash`. | pipeline orchestrator; llm(gemini); greppy+context; mode glue/events |
| **5** | Build/sign/CI + `local_api.rs`: bundle targets, per-OS signing/notarization, new `release.yml`, feature-gated localhost endpoint. | tauri.conf/bundling; CI matrix; signing/notarization; local_api |
| **6** | Decommission: delete old trees + packaging + scripts + `pyproject.toml`, relocate `app/`→root, rewrite README. | (single sweep) |

**Finalization:** after Phase 6, **4 subagents** peer-review the whole result (architecture, parity vs
the §5/§8 checklist, security, build/release), findings addressed, then a final commit.

## 8. Parity checklist (nothing silently dropped)

push-to-talk chords · 4 modes + history · local whisper · TECH_PROMPT · custom dictionary (hot-reload) ·
artifact/noise filtering · waveform FFT→25 bars · transparent overlay + orb position · mic selection ·
auto-paste + clipboard fallback · Gemini cleanup/plan · greppy + code-context injection · VADER sentiment ·
SQLite history + stats · ~25 D3 charts · history list/filter/common-words · system tray · single-instance ·
window-state persist · event-driven refresh (replaces file-watch+poll) · 60s auto-cutoff · macOS
Accessibility flow · external transcription API (revived as localhost HTTP) · debug/crash logging (`tracing`).

Consciously dropped: Python CLI surface, `restart.sh`, chokidar polling, per-platform native UIs.

## 9. Risks

1. **`rdev` reliability + Wayland** (highest) — X11-first; trait abstraction for the `hotkey-listener` swap.
2. **whisper-rs native build per platform** (C++/CMake; MSVC vs MinGW on Windows) — CPU-only MVP de-risks.
3. **VADER fidelity** — vendor the lexicon + port the rule engine; validate compound scores vs Python.
4. **Waveform visual parity** — port `recorder.py` band-mapping/smoothing exactly; tune side-by-side.
5. **Mic device-index semantics** — reconcile `audio_device_index` (host index) vs `audio_device_id`
   (web) so existing configs keep selecting the right mic.
6. **Cross-OS/runtime unverifiable here** — rely on per-OS CI; label unverified code honestly.
