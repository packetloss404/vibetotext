# Changelog

## v2.0.0 — Architectural hardening + accessibility (2026-08-19)

Second major release. After the Tauri rewrite in v0.1.0, this release lands the
first round of post-launch review fixes, AI-slop cleanup, and the most
user-facing UX improvements to date. The product is back under its original
**VibeToText** name (was briefly PacketVoice during the Tauri migration, which
is fully reverted here).

### Added
- **ARIA tabs pattern** — full `role="tablist"` / `role="tab"` / `role="tabpanel"` + roving `tabindex` + arrow/Home/End navigation. New `src/tabs.js` helper wires both the main 9-tab tablist and the analytics hourly/yearly sub-tablist. Panels use the `hidden` attribute (not inline `style.display`) so screen readers and the roving focus contract honor the ARIA semantics.
- **Per-mode analytics** — `get_statistics(mode)` now mirrors `get_entries(mode)`: every metric (counts, avg WPM, time-saved, word frequency) filters to the selected mode. New `statistics_filters_by_mode` test covers it.
- **codebase_path status badge** — new `get_codebase_path_status` IPC surfaces `{ path, exists, readable, last_checked }`; the settings card renders a green "Path is valid" or red "Path no longer exists" indicator. Pipeline pre-checks the path and degrades gracefully with a `codebase_path_missing` status event instead of a confusing greppy error.
- **`phf::Set` stopword table** — 138-word English stopword list is now a compile-time perfect-hash table. Each `.contains()` is O(1) with no init cost or allocation.
- **macOS bundle inspection script** — `scripts/check-macos-bundle.sh` (run on a Mac after `cargo tauri build`) reports every dylib the binary links, flags any non-Apple dylib that would require `disable-library-validation`, and shows code-signing status. Exits 0/1 so the result is a yes/no for whether the entitlement can be dropped.
- **History-window defaults** — main window opens at 640×680 (min 480×440), a tighter default that matches the now-centered settings/transcribe panels.

### Changed
- **Shared whisper model** — the capture pipeline and the `local-api` HTTP endpoint now share a single `Arc<Transcriber>` on `AppState::Inner`. Saves ~3 GB RAM on `large-v3` (was loading the same ggml model twice). `AppState::ensure_transcriber(model)` is the single entry point; both `pipeline::Worker` and `local_api::start` clone from it.
- **`RecordingGuard` for cpal** — `Recorder::start` now returns a guard that RAII-owns the `cpal::Stream` and audio buffer. The pipeline stores the guard in `Worker.recording`; on stop it calls `into_buffer()` to consume and retrieve the samples. If the guard is dropped without `into_buffer` (panic, early return), the `Drop` impl releases the cpal stream and logs a warning — eliminates the previous foot-gun where a panic between start and stop would leak the OS audio handle.
- **A11y attribute cleanup** — `aria-labelledby` on `<select>`s, `<label for>`/`.sr-only` for unlabeled text inputs, `aria-live="polite"` on every status region, icon-only `dict-word-remove` button gets an `aria-label`, `keypress` → `keydown` (deprecated) on the dict input.

### Fixed
- **SQL `mode`+`LIMIT` ordering bug** — `get_entries` was applying `LIMIT` in SQL first, then the Rust-side mode filter dropped non-matching rows from the already-limited set, so "10 most recent cleanup entries" could return fewer than 10. Filter now goes into the SQL query; new `filter_mode_is_applied_before_limit` regression test.
- **Topic speed/mood tooltip rendered as literal text** — tooltip was going through `d3.text()` so the `<strong>`/`<br>` showed up as source. New `showTooltipHtml()` helper.
- **Stale analytics after `clear_history`** — `cachedAnalyticsData` was never nulled when entries went empty, so the resize handler re-rendered the pre-clear snapshot.
- **Tooltips stuck on tab switch** — the fragile `DOMContentLoaded` wrapper around the activity-tab handler (scripts at end of body → wrapper had a real race where the listener could be registered after the event fired).
- **Several panics and edge cases** — see the v0.1.0 → HEAD commit log for the full list: dead code removal, redundant `unsafe impl Send`, stdlib-sanity test that wasn't testing our code, AI-slop `once_cell_set` wrapper, dead CSS classes, debug `console.log`s, etc.

### Security
- Reviewed for any new surface area introduced by the refactors; no new attack surface beyond what v0.1.0 already established. The `~/.vibetotext/.env` Gemini key storage and 32 MiB localhost-API body cap from v0.1.0 are unchanged.
- macOS `disable-library-validation` entitlement under review via `scripts/check-macos-bundle.sh`. Whisper.cpp is statically linked (cmake `BUILD_SHARED_LIBS=0` default for `bundled` feature), so the entitlement is *likely* unused for our build; the script will confirm on a Mac run.

### Removed
- `db::entries::count`, `WaveformAnalyzer::reset`, stdlib-sanity greppy test, unused `CFStringCreateWithCString` FFI block + `OptionalExtension` import, verbose "unrelated printing key" comments, unit-struct `once_cell_set` wrapper, unused CSS classes (`.chart-line`/`.chart-area`/`.chart-dot`/`.entry-footer`/`.restart-btn`/`#sessions-today`), debug `console.log`s.

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
