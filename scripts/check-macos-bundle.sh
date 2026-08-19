#!/usr/bin/env bash
# VibeToText — macOS bundle inspection script
#
# Purpose: determine whether `com.apple.security.cs.disable-library-validation`
# is actually NEEDED by our app, or whether whisper.cpp is statically linked
# (in which case we can drop the entitlement for App Store readiness).
#
# Usage: run on a Mac with Xcode CLT installed, from the repo root:
#
#   cargo tauri build --target aarch64-apple-darwin
#   ./scripts/check-macos-bundle.sh
#
# Or, if the bundle already exists at src-tauri/target/release/bundle/.../VibeToText.app,
# just run the script.
#
# Exit codes:
#   0 — disable-library-validation is NOT required (safe to drop)
#   1 — disable-library-validation IS required (keep the entitlement)
#   2 — bundle not found (build first)

set -euo pipefail

# Locate the .app bundle under target/release/bundle/macos or target/release/bundle/dmg.
APP_PATH="$(find src-tauri/target -name 'VibeToText.app' -type d 2>/dev/null | head -1 || true)"
if [ -z "$APP_PATH" ]; then
    APP_PATH="$(find src-tauri/target -name 'VibeToText.app' -type d 2>/dev/null | head -1 || true)"
fi
if [ -z "$APP_PATH" ]; then
    echo "ERROR: VibeToText.app not found under src-tauri/target/."
    echo "Build it first: cargo tauri build"
    exit 2
fi

BIN="$APP_PATH/Contents/MacOS/VibeToText"
if [ ! -f "$BIN" ]; then
    echo "ERROR: $BIN not found"
    exit 2
fi

echo "============================================================"
echo "VibeToText bundle: $APP_PATH"
echo "============================================================"
echo

echo "## 1. Linked dylibs (otool -L)"
echo "------------------------------------------------------------"
otool -L "$BIN" | sed 's/^/  /'
echo

# Look for non-Apple, non-self, non-system dylibs.
# Apple frameworks live under /System/Library, /usr/lib, or have the @rpath from a
# signed Apple framework (e.g. /System/iOSSupport). Anything else is what
# disable-library-validation would need to allow.
echo "## 2. Non-Apple / non-system dylibs (would need disable-library-validation)"
echo "------------------------------------------------------------"
NEEDS_DISABLE=0
while IFS= read -r line; do
    # Skip the binary name line and lines that are clearly Apple/system.
    path=$(echo "$line" | awk '{print $1}')
    case "$path" in
        "$BIN") continue ;;
        /System/Library/*|/usr/lib/*|@executable_path/*|@rpath/*.dylib|@loader_path/*) continue ;;
        # Apple-bundled frameworks (linked via @rpath) — typically safe
        # but worth flagging the path explicitly.
        */VibeToText.app/Contents/*) continue ;;
        # @rpath expansions we couldn't determine; flag for review
        @rpath/*) echo "  ? $path" ; NEEDS_DISABLE=1; continue ;;
    esac
    # Anything that survived is a non-Apple dylib that needs validation off.
    echo "  $path"
    NEEDS_DISABLE=1
done < <(otool -L "$BIN" | tail -n +2)

# Also check any embedded frameworks folder.
FW_DIR="$APP_PATH/Contents/Frameworks"
if [ -d "$FW_DIR" ]; then
    echo
    echo "Embedded frameworks (Contents/Frameworks/):"
    ls -la "$FW_DIR" | tail -n +2 | sed 's/^/  /'
    # Each non-Apple dylib here is a candidate for needing library validation off
    # if it's loaded via @rpath at runtime.
    for fw in "$FW_DIR"/*.dylib "$FW_DIR"/*.framework; do
        [ -e "$fw" ] || continue
        name=$(basename "$fw")
        # Skip Apple's own frameworks if any (none expected, but be safe).
        if [ "${name#libswift}" != "$name" ] || [ "${name#libsystem}" != "$name" ]; then
            continue
        fi
        echo "  $name"
        NEEDS_DISABLE=1
    done
fi

echo
echo "## 3. Code signing status (codesign -dv)"
echo "------------------------------------------------------------"
codesign -dv "$BIN" 2>&1 | sed 's/^/  /' || true
echo

echo "## 4. Conclusion"
echo "------------------------------------------------------------"
if [ "$NEEDS_DISABLE" -eq 0 ]; then
    echo "  -> No non-Apple dylibs detected."
    echo "  -> The disable-library-validation entitlement is NOT used."
    echo "  -> Safe to drop entitlements.plist: com.apple.security.cs.disable-library-validation"
    echo "  -> And the corresponding comment in src-tauri/entitlements.plist."
    exit 0
else
    echo "  -> Non-Apple dylib(s) found above."
    echo "  -> disable-library-validation IS required (or you need to bundle+sign the dylib)."
    echo "  -> Two options to remove the entitlement:"
    echo "       (a) bundle whisper.cpp dylib into Contents/Frameworks and sign it with your Developer ID,"
    echo "       (b) link whisper.cpp statically (set BUILD_SHARED_LIBS=0 in the whisper.cpp cmake build)."
    exit 1
fi
