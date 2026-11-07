#!/bin/bash
# Removes LS50Stream. Run with: sudo "/Library/Application Support/LS50Stream/uninstall.sh"
if [ "$(id -u)" != 0 ]; then
    echo "Please run with sudo." >&2
    exit 1
fi
launchctl bootout system/io.github.kaihagseth.ls50stream.engine 2>/dev/null
rm -f /Library/LaunchDaemons/io.github.kaihagseth.ls50stream.engine.plist /etc/newsyslog.d/io.github.kaihagseth.ls50stream.conf
rm -rf /Library/Audio/Plug-Ins/HAL/LS50Stream.driver "/Library/Application Support/LS50Stream" /Library/Logs/LS50Stream
pkgutil --forget io.github.kaihagseth.ls50stream >/dev/null 2>&1
launchctl kickstart -k system/com.apple.audio.coreaudiod 2>/dev/null || killall coreaudiod 2>/dev/null
echo "LS50Stream has been removed."
