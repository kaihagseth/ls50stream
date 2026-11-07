#!/bin/bash
# Builds a signed, notarized package and publishes it as a GitHub release:
#   scripts/release.sh   ->  tag v<version> + release with LS50Stream-<version>.pkg (and .sha256)
# Bump `version` in engine/Cargo.toml and commit first. Needs `gh` (logged in) and what
# `scripts/build.sh --release` needs.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERSION=$(grep -m1 '^version' "$ROOT/engine/Cargo.toml" | cut -d'"' -f2)
TAG="v$VERSION"
cd "$ROOT"

[ -z "$(git status --porcelain)" ] || { echo "Commit or stash your changes first." >&2; exit 1; }
git fetch --quiet --tags origin
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "Push main first (HEAD is not origin/main)." >&2; exit 1; }
if gh release view "$TAG" >/dev/null 2>&1; then
    echo "Release $TAG already exists; bump the version in engine/Cargo.toml." >&2
    exit 1
fi

scripts/build.sh --release
PKG="build/LS50Stream-$VERSION.pkg"

NOTES=$(cat <<EOF
Signed and notarized installer for macOS 14 or later (Apple silicon and Intel).

1. Download **LS50Stream-$VERSION.pkg** and open it (asks for an admin password; restarts system audio once).
2. Choose **KEF LS50W** as the sound output. It appears while a first-generation KEF LS50 Wireless is on and on the same network.

Upgrading installs over the previous version. To remove: \`sudo "/Library/Application Support/LS50Stream/uninstall.sh"\`.

SHA-256: \`$(cut -d' ' -f1 "$PKG.sha256")\`
EOF
)
gh release create "$TAG" "$PKG" "$PKG.sha256" --target main --title "ls50stream $VERSION" --notes "$NOTES"
