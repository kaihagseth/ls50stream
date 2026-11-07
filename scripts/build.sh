#!/bin/bash
# Builds LS50Stream (audio driver + engine) as universal binaries and packages them:
#   scripts/build.sh  ->  build/LS50Stream-<version>.pkg (ad-hoc signed, for local use)
# Adapted from Limbo's build script.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/build"
VERSION=$(grep -m1 '^version' "$ROOT/engine/Cargo.toml" | cut -d'"' -f2)
MIN_MACOS=14.0
export PATH="$HOME/.cargo/bin:$PATH"

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
codesign --force --sign - "$OUT/stage/ls50streamd"

echo "==> Audio driver (LS50Stream.driver)"
DRIVER="$OUT/stage/LS50Stream.driver"
mkdir -p "$DRIVER/Contents/MacOS"
sed "s/__VERSION__/$VERSION/" "$ROOT/driver/Info.plist" > "$DRIVER/Contents/Info.plist"
clang -O2 -Wall -Wextra -Werror -fblocks -arch arm64 -arch x86_64 -mmacosx-version-min=$MIN_MACOS -bundle \
    -framework CoreAudio -framework CoreFoundation -o "$DRIVER/Contents/MacOS/LS50Stream" "$ROOT/driver/LS50StreamDriver.c"
codesign --force --sign - "$DRIVER"

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
pkgbuild --quiet --root "$PAYLOAD" --component-plist "$OUT/stage/components.plist" --scripts "$ROOT/packaging/scripts" \
    --identifier io.github.kaihagseth.ls50stream --version "$VERSION" --install-location / "$PKG"
echo "==> Built $PKG"
