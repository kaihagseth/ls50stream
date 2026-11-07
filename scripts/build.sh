#!/bin/bash
# Builds LS50Stream (audio driver + engine) as universal binaries and packages them:
#   scripts/build.sh            ->  build/LS50Stream-<version>.pkg, ad-hoc signed (for your own Mac)
#   scripts/build.sh --release  ->  the same, signed with your Developer ID, notarized and stapled
#                                   (installs on any Mac without Gatekeeper warnings)
# --release uses the "Developer ID Application" and "Developer ID Installer" identities in your
# keychain (override with DEVELOPER_ID_APP / DEVELOPER_ID_INSTALLER) and the notarytool keychain
# profile NOTARY_PROFILE (default "ls50stream"), created once with:
#   xcrun notarytool store-credentials ls50stream --apple-id <email> --team-id <team id>
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/build"
VERSION=$(grep -m1 '^version' "$ROOT/engine/Cargo.toml" | cut -d'"' -f2)
MIN_MACOS=14.0
export PATH="$HOME/.cargo/bin:$PATH"
# Keep local paths (home folder, checkout) out of the binaries' panic messages
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~ --remap-path-prefix=$ROOT=ls50stream"

RELEASE=0
case "${1:-}" in
    "") ;;
    --release) RELEASE=1 ;;
    *) echo "usage: scripts/build.sh [--release]" >&2; exit 2 ;;
esac

# Finds the first identity in the keychain whose name contains $2 (policy $1).
find_identity() {
    security find-identity -v -p "$1" | grep -F "$2" | head -1 | sed -E 's/^ *[0-9]+\) ([0-9A-F]{40}) "(.*)"$/\2/'
}

if [ $RELEASE = 1 ]; then
    NOTARY_PROFILE=${NOTARY_PROFILE:-ls50stream}
    APP_ID=${DEVELOPER_ID_APP:-$(find_identity codesigning "Developer ID Application:")}
    INSTALLER_ID=${DEVELOPER_ID_INSTALLER:-$(find_identity basic "Developer ID Installer:")}
    [ -n "$APP_ID" ] || { echo "No \"Developer ID Application\" certificate in the keychain." >&2; exit 1; }
    [ -n "$INSTALLER_ID" ] || { echo "No \"Developer ID Installer\" certificate in the keychain." >&2; exit 1; }
    if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
        echo "No working notarytool keychain profile \"$NOTARY_PROFILE\". Create it once with:" >&2
        echo "  xcrun notarytool store-credentials $NOTARY_PROFILE --apple-id <email> --team-id <team id>" >&2
        exit 1
    fi
    echo "==> Signing as: $APP_ID / $INSTALLER_ID"
    SIGN=(codesign --force --timestamp --options runtime --sign "$APP_ID")
else
    SIGN=(codesign --force --sign -)
fi

echo "==> LS50Stream $VERSION"
rm -rf "$OUT/stage"
mkdir -p "$OUT/stage"

echo "==> Engine (ls50streamd)"
export MACOSX_DEPLOYMENT_TARGET=$MIN_MACOS
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --quiet --manifest-path "$ROOT/engine/Cargo.toml" --target $target
done
lipo -create -output "$OUT/stage/ls50streamd" \
    "$ROOT/engine/target/aarch64-apple-darwin/release/ls50streamd" "$ROOT/engine/target/x86_64-apple-darwin/release/ls50streamd"
"${SIGN[@]}" --identifier io.github.kaihagseth.ls50stream.engine "$OUT/stage/ls50streamd"

echo "==> Audio driver (LS50Stream.driver)"
DRIVER="$OUT/stage/LS50Stream.driver"
mkdir -p "$DRIVER/Contents/MacOS"
sed "s/__VERSION__/$VERSION/" "$ROOT/driver/Info.plist" > "$DRIVER/Contents/Info.plist"
clang -O2 -Wall -Wextra -Werror -fblocks -arch arm64 -arch x86_64 -mmacosx-version-min=$MIN_MACOS -bundle \
    -framework CoreAudio -framework CoreFoundation -o "$DRIVER/Contents/MacOS/LS50Stream" "$ROOT/driver/LS50StreamDriver.c"
"${SIGN[@]}" "$DRIVER"

echo "==> Package"
PAYLOAD="$OUT/stage/payload"
mkdir -p "$PAYLOAD/Library/Audio/Plug-Ins/HAL" "$PAYLOAD/Library/Application Support/LS50Stream" "$PAYLOAD/Library/LaunchDaemons"
cp -R "$DRIVER" "$PAYLOAD/Library/Audio/Plug-Ins/HAL/"
cp "$OUT/stage/ls50streamd" "$ROOT/packaging/uninstall.sh" "$PAYLOAD/Library/Application Support/LS50Stream/"
cp "$ROOT/packaging/io.github.kaihagseth.ls50stream.engine.plist" "$PAYLOAD/Library/LaunchDaemons/"
export COPYFILE_DISABLE=1
xattr -cr "$PAYLOAD"
pkgbuild --analyze --root "$PAYLOAD" "$OUT/stage/components.plist" >/dev/null
i=0
while plutil -extract "$i" xml1 -o /dev/null "$OUT/stage/components.plist" 2>/dev/null; do
    plutil -replace "$i.BundleIsRelocatable" -bool NO "$OUT/stage/components.plist"
    i=$((i + 1))
done
PKG="$OUT/LS50Stream-$VERSION.pkg"
rm -f "$PKG"
PKG_SIGN=()
[ $RELEASE = 1 ] && PKG_SIGN=(--sign "$INSTALLER_ID" --timestamp)
pkgbuild --quiet --root "$PAYLOAD" --component-plist "$OUT/stage/components.plist" --scripts "$ROOT/packaging/scripts" \
    --identifier io.github.kaihagseth.ls50stream --version "$VERSION" --install-location / ${PKG_SIGN[@]+"${PKG_SIGN[@]}"} "$PKG"

if [ $RELEASE = 1 ]; then
    echo "==> Notarizing (usually a few minutes)"
    xcrun notarytool submit "$PKG" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json > "$OUT/stage/notary.json" || true
    STATUS=$(plutil -extract status raw -o - "$OUT/stage/notary.json" 2>/dev/null || echo "unknown")
    if [ "$STATUS" != "Accepted" ]; then
        cat "$OUT/stage/notary.json" >&2
        ID=$(plutil -extract id raw -o - "$OUT/stage/notary.json" 2>/dev/null || true)
        [ -n "$ID" ] && xcrun notarytool log "$ID" --keychain-profile "$NOTARY_PROFILE" >&2
        echo "Notarization failed: $STATUS" >&2
        exit 1
    fi
    xcrun stapler staple -q "$PKG"
    spctl --assess --type install -vv "$PKG"
    (cd "$OUT" && shasum -a 256 "$(basename "$PKG")" > "$(basename "$PKG").sha256")
fi
echo "==> Built $PKG"
