# Intel Mac .dmg — not yet available

**Status**: Blocked on CI infrastructure (2026-08-19)

## What's missing

`VibeToText_2.0.0_x64.dmg` — Intel Mac (x86_64) installer. The other 5
release assets ship in v2.0.0 (aarch64 .dmg, Windows nsis+msi, Linux
deb+appimage).

## Why

The only GitHub-hosted runner that produces Intel Mac binaries is
`macos-13`. We've attempted to use it on v2.0.0 twice:

| Run | Outcome |
|---|---|
| `32305600275` | macos-13 queued 30+ min, no runner, cancelled |
| `32308533228` | macos-13 queued 33 min, no runner, cancelled |

GitHub has deprecated the `macos-13` runner image. The runner pool is
small and the deprecation makes it actively shrinking. We can't
reliably ship a CI-built Intel .dmg through GitHub Actions today.

Rosetta does **not** help here: it lets Apple Silicon run x86_64
binaries, not the other way around. Intel Macs cannot run aarch64
binaries at all, so the aarch64 .dmg is not a substitute for Intel
users.

## Workarounds for Intel Mac users (v2.0.0)

Until this is resolved, Intel Mac users can build locally:

```sh
git clone https://github.com/packetloss404/vibetotext
cd vibetotext
git checkout v2.0.0
cargo install tauri-cli --version '^2.0'
cd src-tauri
cargo tauri build --target x86_64-apple-darwin --bundles dmg
```

The output is `src-tauri/target/release/bundle/dmg/VibeToText_2.0.0_x64.dmg`.

## How to unblock

In rough order of effort:

1. **Self-hosted Intel Mac runner** — add a Mac mini (or similar) to
   the repo as a self-hosted runner, label it `intel-mac`, then add
   the leg back to the release matrix using that label. This is the
   long-term fix and unblocks both the .dmg and macOS code signing.
2. **Third-party macOS CI** — services like Cirrus CI or MacStadium
   can host Intel Mac runners. Wire one in as an external CI
   provider. More setup than option 1 but no hardware to maintain.
3. **Drop Intel Mac support** — document the limitation in the
   README. Apple Silicon is now the dominant Mac architecture; the
   share of Intel Macs in active use is small and shrinking.

## Tracking

- This doc: `docs/intel-mac-todo.md`
- Workflow comment: `.github/workflows/tauri-release.yml` (release
  matrix, near the macos-14 leg)
- Last attempt: 2026-08-19
