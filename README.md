# ls50stream

Play your Mac's audio on a **KEF LS50 Wireless** (first generation) over the
network. ls50stream adds an output device, **KEF LS50W**, to macOS: pick it in
the sound menu and whatever you play comes out of the speakers.

## Features

- **Works with any app:** music, video, games, calls, system sounds.
- **No setup:** finds any first-generation LS50 Wireless on your network. The
  device appears while the speaker is on and disappears when it isn't.
- **Low delay:** about 0.7 s at 48 kHz, down to about 0.35 s at 192 kHz with
  the Mac on a network cable. On Wi-Fi, a small safety buffer is added
  automatically to prevent dropouts.
- **Lip sync:** reports the speaker's measured delay to macOS, so video players
  that support it (Safari, QuickTime, …) stay in sync.
- **Lossless:** 24-bit PCM at 48, 96 or 192 kHz.
- **Switches the speaker for you:** moves it to its network input when you
  start playing, and releases it after 30 s of silence. Short pauses resume
  instantly.
- **Steps aside:** if someone else starts playing on the speaker, your Mac's
  audio goes back to its previous output.
- **Volume and mute:** the Mac's volume keys control the device.
- Universal: Apple silicon and Intel, macOS 14 or later.

Only the first-generation LS50 Wireless is supported, not the LS50 Wireless II
or other KEF models.

## Install

Download `LS50Stream-<version>.pkg` from the
[latest release](https://github.com/kaihagseth/ls50stream/releases/latest) and
open it. It asks for an admin password and restarts system audio once. The
package is signed and notarized by Apple, and newer versions install over older
ones.

Then choose **KEF LS50W** as the output (the sound icon in the menu bar, or
System Settings → Sound).

### Settings

- **Sample rate:** set in Audio MIDI Setup (select KEF LS50W). The default,
  48 kHz, is the safe choice on Wi-Fi. With the Mac on a network cable, 96 or
  192 kHz lowers the delay.
- **Several LS50Ws:** it uses one of them. To choose which, add
  `--speaker <serial>` to `ProgramArguments` in
  `/Library/LaunchDaemons/io.github.kaihagseth.ls50stream.engine.plist`. The
  log lists the serial numbers of the speakers it finds.
- **Log:** `/Library/Logs/LS50Stream/ls50streamd.log`

### Uninstall

```sh
sudo "/Library/Application Support/LS50Stream/uninstall.sh"
```

## Build from source

You need the Xcode command line tools (`xcode-select --install`) and Rust
(<https://rustup.rs>):

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
git clone https://github.com/kaihagseth/ls50stream.git
cd ls50stream
scripts/build.sh                    # -> build/LS50Stream-<version>.pkg
open build/LS50Stream-*.pkg
```

Run the tests with `cd engine && cargo test`.

The project has two parts: `driver/` is the macOS audio device (a Core Audio
plug-in in C), and `engine/` is `ls50streamd`, a background service in Rust
that streams the device's audio to the speaker over DLNA/UPnP.

### Releases

`scripts/build.sh --release` signs the package with a Developer ID, notarizes
it and staples the ticket. `scripts/release.sh` does that and publishes it as
GitHub release `v<version>`; bump `version` in `engine/Cargo.toml` first.

## AI disclaimer

This app was built with the assistance of Claude Opus 5.5, an AI model by
Anthropic.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). Copyright (C) 2026 Kai Hagseth.

Not affiliated with or endorsed by KEF. "KEF" and "LS50" are trademarks of
their owner and are used here only to say which speaker this works with.
