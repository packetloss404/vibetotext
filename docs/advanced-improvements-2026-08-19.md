# VibeToText — Advanced improvements design (2026-08-19)

Suggested by the code-review pass (commits `5a6317e` … `22a6c9e`). Research and design notes before implementation. No code is being changed yet.

| ID | Suggestion | Size | Group |
|----|---|---|---|
| A | Share whisper model between pipeline and `local-api` | medium-high | Big |
| B | Pass `mode` into `Db::get_statistics` | low | Quick win |
| C | `phf::Set` for stopwords in `db::stats` | low | Quick win |
| D | Re-validate `codebase_path` periodically | low-medium | Medium |
| E | `Drop` guard on `Recorder::start` | medium | Medium |
| F | Hoist `OnceLock<Transcriber>` to `AppState` | low | Quick win |
| G | Full ARIA tabs pattern | medium-high | Big |
| H | Drop `disable-library-validation` for App Store readiness | medium (might be a no-op) | Medium |
| I | Swap `rdev` for `keytap` (not `hotkey-listener`) | high | Big |

---

## A. Share whisper model between pipeline and `local-api`

**Current state.** `commands.rs` constructs a `Transcriber` and stashes it on `AppState`. `local_api.rs` constructs a *second* `Transcriber` on its own background thread, behind a `OnceLock<Arc<Transcriber>>`. Both load `~/.vibetotext/models/ggml-<m>.bin` independently → 2× RAM for `large-v3` (~3 GB). The endpoint doc-comment in `local_api.rs:28-33` already calls this out as a known issue.

**Design.** Move the canonical `Arc<Transcriber>` onto `AppState` (extending the existing `Inner` struct), have `local_api::start` clone it instead of building its own. The capture pipeline already owns the warm; the local-api endpoint just borrows the model.

Concrete steps:
1. `state.rs`: add `transcriber: Arc<Transcriber>` to `Inner` (default = `None`).
2. `lib.rs` `run()`: after building the Tauri app, resolve the model path from config, construct the `Transcriber`, warm it on a background thread (so app startup isn't blocked by multi-second model load), then `.manage()` it via `AppState.inner` mutator.
3. `local_api.rs:start`: drop the `OnceLock<Arc<Transcriber>>`; instead pull the shared `Arc<Transcriber>` off `AppHandle::state()` and clone it into the worker thread.
4. `transcribe/mod.rs`: add `pub fn model_name(&self) -> &str` accessor so the local-api can log which model it's serving.
5. Update tests if any reference the standalone transcriber construction.

**Thread-safety check.** The existing `Transcriber` has `ctx: Mutex<Option<WhisperContext>>` which serializes inferences. Sharing the `Arc<Transcriber>` is fine — the only contention is on `transcribe()` calls, and the capture pipeline + local-api are the only callers, with one transcription at a time anyway.

**Tradeoff / risk.** Model swap on config change gets slightly trickier: if the user changes `whisper_model` in settings, the shared `Arc<Transcriber>` is stale. Two options:
- **(a)** ignore the config change for the live process (model is loaded once at startup; user must restart to swap). Acceptable — current behavior is the same.
- **(b)** `RwLock<Arc<Transcriber>>` so a setter can atomically swap. Adds complexity, low value. Skip.

**Files touched:** `state.rs`, `lib.rs`, `local_api.rs`, `transcribe/mod.rs`. ~50-80 lines net.

**Commit shape.** One commit (`refactor: share whisper model between pipeline and local-api`).

---

## B. Pass `mode` into `Db::get_statistics`

**Current state.** `db::stats::get_statistics` ignores its `mode` parameter (renamed to `_mode` by the review pass). The frontend `analytics.js` only ever calls it with no mode filter, so this is a frontend-side addition, not a bug fix.

**Design.**
1. `db/stats.rs::get_statistics`: add a `WHERE mode = ?` clause to the SQL when `mode` is `Some`. The mode filter applies to the row count, totals, avg WPM, time-saved, and word-frequency text sources. The `Word_Frequency` already iterates all `entries.text`, so filter the rows before collecting.
2. `db/entries.rs::get_entries` already takes `mode` correctly (the review fixed this); mirror that pattern.
3. `commands.rs::get_statistics`: change signature to `get_statistics(mode: Option<String>) -> Result<Statistics, String>`. Underscore-prefix the unused param if frontend doesn't pass it yet.
4. `analytics.js` (or wherever stats are fetched): add a `mode` arg plumbed from the current tab.

**Tradeoff.** Tighter DB scan for filtered stats. With `idx` already on `mode` (added by the review's get_entries fix), the query is cheap. Word frequency is still O(rows) but bounded by the mode filter.

**Files touched:** `db/stats.rs`, `commands.rs`, `src/analytics.js`. ~15-30 lines net.

**Commit shape.** One commit (`feat: support per-mode statistics breakdown`).

---

## C. `phf::Set` for stopwords in `db::stats`

**Current state.** `word_frequency` does `STOPWORDS.contains(&word.as_str())` for every token. `STOPWORDS` is a `&[&str]` of 138 entries → linear scan, O(n×138) per stats call. Not a real bottleneck at <10k rows but trivially fixable.

**Design.** Switch to `phf::Set<&'static str>` (compile-time perfect-hash, ~3x faster than `HashSet` lookups, zero init cost, no runtime allocation). The migration is one-line: change the static type and the `.contains()` call signature.

Concrete steps:
1. Add `phf = "0.11"` to `Cargo.toml` dependencies.
2. `db/stats.rs`: replace the `static STOPWORDS: &[&str]` with a `static STOPWORDS: phf::Set<&'static str> = phf::set! { "a", "an", ... }`.
3. The call site already uses `.contains()` which works on both `&[&str]` and `phf::Set` — no other change needed.
4. Add a benchmark or perf test? Skip — premature.

**Tradeoff.** Adds a transitive dep (`phf` is well-maintained, zero runtime cost beyond what it replaces, MIT licensed). Recompile time goes up by ~1-2s for the proc-macro. Acceptable.

**Files touched:** `Cargo.toml`, `db/stats.rs`. ~5 lines net.

**Commit shape.** One commit (`perf: use phf::Set for stopword lookup`).

---

## D. Re-validate `codebase_path` periodically

**Current state.** `commands.rs::set_codebase_path` canonicalizes and saves the path at write time. Nothing re-checks the path on later reads. If the user deletes the directory, `greppy` invocations silently fail (the worker logs an error, frontend never sees it).

**Design.**
1. `commands.rs`: add a `get_codebase_path_status() -> { path: String, exists: bool, readable: bool, last_checked: u64 }` IPC command. Cheap — just `path.is_dir() && path.metadata().is_ok()`.
2. `config.rs`: add `last_validated_at: Option<u64>` to the codebase-path field's wrapper (or as a separate field on `AppConfig`). Update it whenever the path is set or read.
3. `pipeline.rs`: when greppy mode is invoked, check the status first; if `!exists`, log + return a user-facing "codebase path no longer valid" error instead of a generic greppy failure.
4. `src/index.html` + `src/renderer.js`: a small "codebase: valid ⚠️ / missing" indicator on the settings card and a settings-tab banner.

**Tradeoff.** New IPC surface (small). Frontend gets a useful signal. Pipeline error path becomes more useful.

**Files touched:** `commands.rs`, `config.rs`, `pipeline.rs`, `index.html`, `renderer.js`, `styles.css`. ~40-60 lines net.

**Commit shape.** One commit (`feat: surface codebase_path existence in UI and pipeline`).

---

## E. `Drop` guard on `Recorder::start`

**Current state.** `audio::recorder::Recorder::start` builds a cpal `Stream` and stores it in `self.session`. If the audio callback panics (or some downstream logic in `on_level` does), the stream could be left in `self.session` with no clean path to drop it. Currently `stop()` is the only cleanup. The Rust community pattern for "this must run on early exit" is `Drop`.

**Design.**
1. Create a `RecordingGuard` struct that holds the `cpal::Stream` and an `Arc<SharedBuffer>`. On `Drop`, it logs a warning (since by design `stop()` should have been called first; a dropped guard means the recorder was abandoned).
2. Change `Recorder::start` to return `RecordingGuard` instead of mutating `self`. The caller (`pipeline.rs`) stores the guard in its state, drops it on pipeline completion, and calls `guard.into_buffer()` to consume the captured audio.
3. `stop()` becomes "explicit early stop" — drops the guard from pipeline state, returns the buffer.

**Tradeoff / risk.** This is a structural refactor. The `Recorder` becomes stateless from the pipeline's perspective; pipeline state holds the guard. Touches `audio/mod.rs` (CallbackState already takes `Arc<SharedBuffer>`), `pipeline.rs`, and any test that constructs a `Recorder`. The audio callback still must be `Send + 'static` (unchanged).

**Why bother.** Right now, a panic in `on_level` would leave a zombie cpal stream. With the guard, dropping the guard (via panic unwind) drops the stream too. Robustness win.

**Files touched:** `audio/recorder.rs`, `audio/mod.rs`, `pipeline.rs`, tests. ~30-50 lines net.

**Commit shape.** One commit (`refactor: use Drop guard to ensure cpal stream is released on panic`).

---

## F. Hoist `OnceLock<Transcriber>` to `AppState`

**Current state.** `local_api.rs:125` has a per-thread `OnceLock<Arc<Transcriber>>`. The endpoint resolves `model_name` + `custom_words` once at startup. If a user changes `whisper_model` in settings, the endpoint keeps serving the old one. If a hot-reload of custom words is added later, the endpoint won't see it.

**Design.**
1. **Strongly coupled with A.** If A ships (shared model on `AppState`), this suggestion becomes trivial — just clone the `Arc<Transcriber>` from `AppState`. The `OnceLock<local_api>` itself can be deleted.
2. **Without A:** move the `OnceLock` to `AppState` so a future hot-swap (e.g., a new `set_whisper_model` command) can rebuild and re-`OnceLock::set(...)` the `Arc`. Need to swap to `Mutex<Option<Arc<Transcriber>>>` (or keep `OnceLock` and just rebuild the whole `AppState` on swap).

**Verdict.** If A ships, F is folded into A. If A is deferred, do F as a standalone — it's the same diff without the pipeline change. Low effort either way.

**Files touched.** See A; otherwise just `state.rs` + `local_api.rs`. ~10 lines net.

**Commit shape.** Folded into A, or standalone `refactor: hoist transcriber OnceLock to AppState` if A is deferred.

---

## G. Full ARIA tabs pattern

**Current state.** After the review pass, the main and activity tabs have `role="tablist"`, `role="tab"`, `aria-selected`. Missing: `role="tabpanel"`, `aria-controls` ↔ `aria-labelledby` pairing, roving `tabindex` (only the selected tab is 0), arrow-key navigation, `hidden` attribute on inactive panels.

**Design.** Two implementations considered:

**(a) Hand-rolled**, ~60 lines of JS. Roving tabindex manager, arrow-key handler, focus tracker. Matches WAI-ARIA APG exactly. Zero deps.

**(b) Library:** Radix UI Tabs / React Aria Tabs / Headless UI. Tauri uses vanilla HTML/JS, no React — so the right pick is a vanilla JS library, or hand-roll.

**Verdict.** Hand-roll. The codebase is vanilla JS, no framework, and the WAI-ARIA pattern is well-defined and small. A library adds weight and indirection for a single 9-tab instance.

**Concrete plan.**
1. `index.html`: add `id="tab-<mode>"` to each `<button class="tab">`, add `role="tab"`, `tabindex` (0 for active, -1 for others), `aria-controls="panel-<mode>"`. Add `id="panel-<mode>"` and `role="tabpanel"` to each corresponding panel section (`<div class="entries">`, `<div class="analytics-panel">`, etc.). Add `aria-labelledby="tab-<mode>"` to each panel.
2. The activity tabs (hourly/yearly) get the same treatment with their own tablist ID prefix.
3. `renderer.js` (or a small new `src/tabs.js`): a `setupTablist(tablistId)` function that:
   - Sets `aria-selected` and `tabindex` correctly on click
   - On `keydown`: ArrowLeft/Right moves focus (and switches tabs in automatic mode), Home/End jumps to ends
   - Toggles the `hidden` attribute on the corresponding panel
4. `.sr-only` style already exists (added in review commit `5d3d995`) for the focus indicator.
5. The focusable tab list must be a single tab stop — handled by roving tabindex.
6. Test by:
   - Tab into the list, focus lands on the active tab.
   - Arrow keys cycle through tabs; the panel switches.
   - Tab again, focus leaves the list.
   - Confirm with a screen reader (NVDA/VoiceOver) — out of automated test reach.

**Tradeoff.** ~80-100 lines net (HTML + JS). Standard pattern, low risk. The current click-only implementation is functional; this is an accessibility upgrade.

**Files touched:** `index.html`, `renderer.js` (or new `tabs.js` + script tag), minimal `styles.css` additions for visible focus. ~80-100 lines net.

**Commit shape.** One commit (`a11y: implement full ARIA tabs pattern (roving tabindex, keyboard nav, tabpanel roles)`).

---

## H. Drop `disable-library-validation` entitlement

**Current state.** `entitlements.plist:36` enables `com.apple.security.cs.disable-library-validation`. The verify worker flagged this as the main App Store blocker. Research question: do we *actually* need it?

**Research findings.**
- `whisper-rs 0.16` builds whisper.cpp from source via `cmake-build` (the Cargo dep uses `bundled` feature, not system). Default link mode is static — whisper.cpp is compiled into the VibeToText binary, not loaded as a dylib at runtime.
- We don't load any third-party plugins, frameworks, or external dylibs.
- WKWebView / wry / JSC JIT are Apple-signed, exempted by the default framework search policy.
- The `disable-library-validation` entitlement is therefore **unused for our specific build** — removing it should be a no-op.

**Design.** Verify the assumption empirically:
1. Build the macOS bundle: `cd src-tauri && cargo tauri build --target aarch64-apple-darwin` (or universal). Requires Xcode CLT on the build host.
2. Inspect the bundle: `otool -L VibeToText.app/Contents/MacOS/VibeToText` — list linked dylibs. Anything not signed by Apple or by us is what `disable-library-validation` is required for. Expected: only Apple system frameworks.
3. Inspect for any `@rpath` / `@executable_path` / `LC_LOAD_DYLIB` references that point outside `/System/Library/` and `/usr/lib/`.
4. If all linked dylibs are Apple-signed → entitlement is unused → remove it.
5. If something is unsigned → either bundle-and-sign it (preferred) or keep the entitlement.

**Tradeoff.** If the entitlement is genuinely unused, removing it is a strict App Store-readiness win. If it IS used, we'd need a different fix (bundle + sign whisper deps or link statically). The "is it actually used" question can only be answered on a Mac build host.

**Files touched:** `entitlements.plist`. ~3 lines net if removable, larger if not.

**Commit shape.** `chore(macos): drop unused disable-library-validation entitlement` (conditional on macOS verification).

**Caveat.** I (Mavis) can't run macOS builds from this Windows host. You'd need to run the verification on a Mac (or I can write the verification script for you to run).

---

## I. Swap `rdev` for `keytap`

**Current state.** `rdev 0.5.3` is pinned. Limitations documented in the search results above:
- No native Wayland on Linux (X11/XWayland only).
- macOS 14+ crashes on threaded callers (`TSMGetInputSourceProperty` on a background thread) — silent no-events or panic.
- No clean shutdown (`listen()` blocks forever; no Drop).

The `tauri-migration-plan.md §3` suggested swapping to `hotkey-listener` for Wayland, but research shows a better option has appeared.

**Why `keytap`, not `hotkey-listener`.** `keytap 0.4.0` (released April 2026) is cross-platform (mac + Windows + Linux X11 + Wayland), preserves left/right modifier identity, has clean `Drop`-based shutdown, detects macOS permission at startup instead of silently producing no events, and avoids the Sonoma main-thread crash. `hotkey-listener` is Linux+macOS only, no Windows support, collapses left/right modifiers.

**Design.** Two-phase: add the dependency and trait surface, port the chord state machine, then remove `rdev`.

**Phase 1: Trait seam.** The existing `hotkey/mod.rs` already has a `HotkeyBackend` trait (mentioned in the file's doc comment). Make the trait a real public type:
```rust
pub trait HotkeyBackend: Send {
    fn spawn_listener(self: Box<Self>, tx: mpsc::Sender<HotkeyEvent>) -> Result<JoinHandle>;
}
```
Both `RdevBackend` (current) and `KeytapBackend` (new) implement it. `lib.rs` selects via cfg: `if cfg!(feature = "keytap-backend")` → use keytap; else rdev.

**Phase 2: Port the state machine.** The chord state machine (`resolve_chord`, START-on-modifier-change, most-specific-wins, 60s auto-cutoff) is independent of the OS event source — it consumes `HotkeyEvent::Key{ key, direction }` and emits `HotkeyEvent::Start(mode) / Stop`. Porting just changes the event source from `rdev::Event` to `keytap::KeyEvent`.

**Phase 3: Config + permissions.** `keytap::is_accessibility_trusted()` (or equivalent) at startup replaces the current `hotkey/permissions.rs` AXIsProcessTrusted check on macOS. On Linux Wayland, the user needs `input` group membership — `keytap` returns a typed error; the UI banner already handles this.

**Tradeoff / risk.**
- New transitive dep (`keytap` — released April 2026, only 4 versions old, some risk of rough edges).
- Need to keep `rdev` as a fallback feature (default-build still works while keytap is being shaken out).
- macOS permission UX changes slightly.
- Big PR — probably 200-300 lines of diff.

**Files touched:** `Cargo.toml`, `hotkey/mod.rs`, new `hotkey/keytap_backend.rs`, `hotkey/permissions.rs` (macOS-specific path), `lib.rs`. ~200-300 lines net.

**Commit shape.** Three commits:
1. `feat: add keytap backend behind feature flag (default still rdev)`
2. `chore: keytap-backend on by default; rdev fallback behind feature flag`
3. (after a CI matrix run on all 3 OSes) `chore: drop rdev fallback feature`

Or fold into one commit if you want it atomic.

**Note on CI.** `keytap` needs verification on Linux Wayland (we don't have a CI runner for that), real macOS Sonoma, and Windows. The reviewer noted macOS/Linux are "written-but-unverified" in our env. This is a risk we should call out before the swap.

---

## Suggested implementation order

Quick wins first (B, C, F), then medium (D, E, H), then big (A, G, I). This is a 6-8 hour work block if all 9 ship.

If you want to defer or skip any, the ones with the best value/cost ratio are: **A, C, E, G, I**. The most skippable are **D, F, H** (D is a UX nicety, F is folded into A, H is a "is it actually needed" verification).

Want me to start with the quick wins (B, C, F)?
