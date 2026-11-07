#!/usr/bin/env bash
# Builds Capture.app: the capture tool in its own bundle, so macOS asks for
# system-audio permission for this tool (not for the terminal it's run from).
set -euo pipefail
cd "$(dirname "$0")"
APP=Capture.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
swiftc -O -swift-version 5 main.swift -o "$APP/Contents/MacOS/capture"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>io.github.kaihagseth.ls50stream.capture</string>
	<key>CFBundleName</key>
	<string>ls50stream capture</string>
	<key>CFBundleExecutable</key>
	<string>capture</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSAudioCaptureUsageDescription</key>
	<string>ls50stream streams your Mac's audio to your network speakers.</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$APP"
echo "built $APP"
