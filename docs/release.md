# VibeToText Release Guide

How to build, sign, and ship the Tauri app. Reflects the actual config in
`src-tauri/` (`Cargo.toml`, `tauri.conf.json`). For the design rationale see
[`tauri-migration-plan.md`](./tauri-migration-plan.md) §5–§9.

> Environment honesty: builds are authoritatively verified on **Windows only**.
> macOS and Linux build/sign/notarize steps below are written-but-unverified and
> need real per-OS CI/hardware to confirm.

## 1. Prerequisites

### All platforms

- **Rust stable >= 1.94** (the repo builds on 1.94.1). The crate pins
  `rusqlite 0.39` (`libsqlite3-sys 0.37`) on purpose: `rusqlite 0.40` pulls
  `libsqlite3-sys 0.38`, whose `build.rs` uses `cfg_select!` and requires
  Rust **>= 1.95**. Do **not** bump rusqlite until the toolchain moves to 1.95.
  Install via `rustup toolchain install stable`.
- **cargo-tauri** (v2): `cargo install tauri-cli --version "^2"` (developed
  against cargo-tauri 2.10). Invoke as `cargo tauri ...`.
- **Node 24** + npm (the frontend in `src/` is served as `frontendDist`).
- **CMake** (>= 3.x) and a C/C++ compiler — required to build `whisper-rs`
  (bundled whisper.cpp) and the bundled SQLite.

### Per-OS native toolchain

- **Windows:** Visual Studio Build Tools 2022 with the MSVC C++ workload
  (VC 14.44+). `cc`/`cmake` auto-discover MSVC even when `cl` is not on PATH, so
  `whisper-rs` builds clean on the default `x86_64-pc-windows-msvc` target — no
  MinGW/`gnu` workaround is needed.
- **macOS:** Xcode Command Line Tools (`xcode-select --install`) for the
  toolchain, plus a Developer ID for signing (see §4).
- **Linux:** WebKitGTK + GTK + AppIndicator + related dev packages. On
  Debian/Ubuntu:
  ```sh
  sudo apt-get install -y \
    libwebkit2gtk-4.1-dev libgtk-3-dev \
    libayatana-appindicator3-dev librsvg2-dev \
    build-essential cmake pkg-config
  ```
  (X11/XWayland is the supported path — `rdev` global hotkeys are X11-only;
  native Wayland is a documented follow-up.)

## 2. Build locally (per OS)

From `src-tauri/`:

```sh
cargo tauri build
```

This is **CPU-only by default** (the GPU backends are scaffolded but off). The
default cargo feature set is `["rdev-backend"]` (real global-hotkey listener).

### GPU opt-in

Enable exactly one GPU backend at build time (forwards to the matching
`whisper-rs` feature):

```sh
cargo tauri build --features metal    # macOS Metal
cargo tauri build --features cuda     # NVIDIA CUDA
cargo tauri build --features vulkan   # Vulkan
```

> GPU builds are post-MVP and not verified here; they require the platform GPU
> SDK (CUDA toolkit / Vulkan SDK) to be installed.

### Optional local-api build

The localhost HTTP transcription endpoint (`local_api.rs`) is feature-gated and
**off by default**. Build it in with:

```sh
cargo tauri build --features local-api
```

## 3. Bundle outputs (per OS)

`bundle.targets` in `tauri.conf.json` is the explicit array
`["nsis","msi","app","dmg","deb","appimage"]`; `cargo tauri build` produces the
targets the host OS supports. Outputs land under
`src-tauri/target/release/bundle/`:

| OS | Artifacts |
|---|---|
| Windows | `.msi` (WiX) and `.exe` (NSIS) |
| macOS | `.app` and `.dmg` |
| Linux | `.deb` and `.AppImage` |

Product name is **VibeToText**, identifier `com.vibetotext.app`, version from
`tauri.conf.json` (keep it in sync with `Cargo.toml`).

## 4. Code signing & notarization

> Unsigned bundles run but trip OS gatekeeping (SmartScreen / Gatekeeper).
> Sign release artifacts before distribution.

### macOS (Developer ID + notarization)

1. Install a **Developer ID Application** certificate into the keychain (CI:
   import the `.p12` via the secrets in §5).
2. `src-tauri/entitlements.plist` is committed and referenced from
   `tauri.conf.json` `bundle.macOS.entitlements`. It grants microphone access
   (`com.apple.security.device.audio-input`) and the hardened-runtime entries
   notarization requires. The macOS microphone TCC prompt also needs a usage
   string: `src-tauri/Info.plist` provides `NSMicrophoneUsageDescription`
   (Tauri merges `src-tauri/Info.plist` into the bundle). Accessibility (global
   hotkeys + synthetic paste) is a runtime TCC grant via `AXIsProcessTrusted`,
   not an entitlement.
3. Tauri signs the `.app`/`.dmg` during `cargo tauri build` when
   `APPLE_SIGNING_IDENTITY` (+ keychain) is present.
4. Notarize and staple:
   ```sh
   xcrun notarytool submit VibeToText_*.dmg \
     --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
     --password "$APPLE_APP_SPECIFIC_PASSWORD" --wait
   xcrun stapler staple VibeToText_*.dmg
   ```

### Windows (Authenticode)

Sign the `.msi` and NSIS `.exe` with `signtool` (Authenticode cert):

```bat
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 ^
  /f cert.pfx /p %CERT_PASSWORD% "VibeToText_*.msi"
```

A code-signing cert with established reputation reduces **SmartScreen** warnings;
new certs may still warn until reputation builds.

### Linux (optional)

`.deb`/`.AppImage` need no signing to run. Optionally detach-sign artifacts and
publish a `.sig` + public key:

```sh
gpg --armor --detach-sign VibeToText_*.AppImage
```

## 5. CI secrets

The release CI workflow (`.github/workflows/tauri-release.yml`) expects these
GitHub Actions secrets:

| Secret | Purpose |
|---|---|
| `APPLE_CERTIFICATE` | base64 Developer ID `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password for the `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: … (TEAMID)` |
| `APPLE_ID` | Apple account for notarization |
| `APPLE_TEAM_ID` | Apple Developer Team ID |
| `APPLE_APP_SPECIFIC_PASSWORD` | app-specific password for `notarytool` |
| `KEYCHAIN_PASSWORD` | transient CI keychain password |
| `WINDOWS_CERTIFICATE` | base64 Authenticode `.pfx` |
| `WINDOWS_CERTIFICATE_PASSWORD` | password for the `.pfx` |
| `GPG_PRIVATE_KEY` / `GPG_PASSPHRASE` | optional Linux signing |

`GITHUB_TOKEN` is provided automatically (the release job needs
`permissions: contents: write` to upload assets).

> Note: `.github/workflows/tauri-release.yml` is the Tauri build matrix that
> consumes the secrets above. The legacy `release.yml` (Python/PyInstaller) is
> removed in the Phase 6 decommission.

## 6. Models (download-on-first-run)

**No Whisper model is bundled.** On first transcription the app resolves
`~/.vibetotext/models/ggml-<model>.bin`; if missing it streams the file from the
canonical Hugging Face repo
`ggerganov/whisper.cpp` (`…/resolve/main/ggml-<model>.bin`) to a
`*.downloading` temp file and atomically renames it into place, so an
interrupted download never leaves a corrupt model. `<model>` is a size token such
as `tiny`, `base`, `small`, `medium`, `large-v3`, or `large-v3-turbo`. This keeps
installers small and identical across platforms; users need network access on
first run.

## 7. Release checklist

1. Bump `version` in **both** `src-tauri/Cargo.toml` and
   `src-tauri/tauri.conf.json`.
2. Build per OS (§2); for macOS/Windows sign + (macOS) notarize (§4).
3. Verify bundles install and the app launches (first-run downloads a model).
4. Tag `vX.Y.Z` and push — CI builds the matrix and attaches the bundles to the
   GitHub Release.
