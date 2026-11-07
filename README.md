# ls50stream

Play your Mac's audio on a **KEF LS50 Wireless** (first generation) over the
network, with low delay. ls50stream adds an output device, **"KEF LS50W"**, to
macOS: pick it in the sound menu and whatever you play comes out of the
speakers, with lip sync for video in apps that honour device latency (Safari,
QuickTime, …).

- Works with any first-generation LS50 Wireless on your network; no setup.
- Not the LS50 Wireless II or other KEF models (yet).
- macOS 14 or later, Apple silicon or Intel.

Not affiliated with or endorsed by KEF. "KEF" and "LS50" are trademarks of
their owner and are used here only to say which speaker this works with.

## Install

Download `LS50Stream-<version>.pkg` from the
[latest release](https://github.com/kaihagseth/ls50stream/releases/latest) and
open it (asks for an admin password; restarts system audio once). It is signed
and notarized by Apple. Newer versions install over older ones.

### Or build it from source

You need the Xcode command line tools (`xcode-select --install`) and Rust
(<https://rustup.rs>), with both Mac targets:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
git clone https://github.com/kaihagseth/ls50stream.git
cd ls50stream
scripts/build.sh                    # -> build/LS50Stream-<version>.pkg
open build/LS50Stream-*.pkg         # asks for an admin password; restarts system audio once
```

Then choose **KEF LS50W** as the output (menu bar sound icon, or System
Settings → Sound). It appears while the speaker is on and on the same network.
The sample rate (48, 96 or 192 kHz; default 48) is set in Audio MIDI Setup.
48 kHz is the safe choice on Wi-Fi. With the Mac on a network cable, 96 or
192 kHz lowers the delay (measured on a cable: about 0.7 s at 48 kHz, 0.45 s at
96, 0.35 s at 192; on Wi-Fi a safety margin adds up to 1.5 s).

To remove it: `sudo "/Library/Application Support/LS50Stream/uninstall.sh"`.

If you have several LS50Ws, it uses one of them (the same one while it stays
on). To pick one, add `--speaker <serial>` to `ProgramArguments` in
`/Library/LaunchDaemons/io.github.kaihagseth.ls50stream.engine.plist`; the
serial numbers of the speakers it sees are in the log,
`/Library/Logs/LS50Stream/ls50streamd.log`.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). Copyright (C) 2026 Kai Hagseth.

## How it works

```
Mac apps -> "KEF LS50W" device (LS50Stream.driver, inside coreaudiod)
         -> shared-memory ring (XPC, mach service io.github.kaihagseth.ls50stream.audio)
         -> ls50streamd (root LaunchDaemon, Rust)
         -> HTTP: endless 24-bit WAV at the device's rate (port 8300)
         -> the speaker (told to play it via UPnP AVTransport)
```

- **Rates:** 48, 96 or 192 kHz (default 48 since 0.5, as 192 stutters on
  Wi-Fi), set in Audio MIDI Setup. The stream uses the device's rate
  unchanged; a higher rate means less delay (the speaker buffers ~1 MB):
  ~0.9 s at 192 kHz, ~1.8 s at 96, ~3.6 s at 48. The device reports this delay
  to macOS, so video players can keep lip sync.
- **Wi-Fi vs. wired (2026-10-11):** with the Mac wired, 192 kHz (~9.2 Mbit/s)
  plays smoothly. With the Mac on Wi-Fi the stream crosses the air twice, and
  192 kHz stuttered a little even with 0.1.1's larger writes. 96 kHz
  (~4.6 Mbit/s) was smooth. ls50streamd 0.1.0 also sent many small packets
  (`TCP_NODELAY`, one write per 5 ms), which starved the speaker on Wi-Fi;
  0.1.1 batches writes and buffers 5 s.
- **Low delay (0.2.x):** each stream starts with ~1 MB of silence (the
  speaker's start-up buffer), then live audio is held back until the speaker
  has played it. Measured over Ethernet: 0.33–0.41 s at 192 kHz, 0.46 s at
  96 kHz, ~0.74 s at 48 kHz (was 0.5 / 2.25 / 3.5 s). 32-bit samples are refused.
- **Lip sync:** ls50streamd measures the delay live (sent audio vs. the speaker's
  play position on port 10100), logs it every 20 s, and reports it to macOS
  plus 0.07 s (position → sound, judged in Safari). Safari honours it. Firefox
  doesn't use the device latency the same way and plays audio early.
- **Wi-Fi margin (0.3):** drained to the bare floor, the speaker can run dry
  in a Wi-Fi stall; it then waits to refill its whole start-up buffer and the
  delay jumps (seen: 0.4 s -> 3.9 s at 48 kHz). The delay monitor detects
  that, skips live audio to drain back, and adds 0.25 s of margin (max 2 s);
  after 5 calm minutes the margin shrinks by 0.25 s. 0.3.1: the margin
  starts at 1.5 s when the Mac reaches the speaker over Wi-Fi (0 on a
  cable), grows in 0.5 s steps, skips at most every 3 min (skipping starved
  the speaker into more refills), and shrinks after 10 calm minutes.
- **Endless stream as chained tracks (0.4):** the speaker honours the WAV
  header's data size exactly (a 10 s header: it hung up after 10 s; 0 bytes:
  refused), so one WAV can't run forever (max ~1 h at 192 kHz, ~4 h at 48).
  The stream is served as consecutive tracks `/stream/<rate>/<n>.wav` of
  ~4 GB each; each resumes at exactly the byte the previous ended (a 3 s
  replay buffer covers the reconnect), and the next is queued with
  `SetNextAVTransportURI` once the speaker's next slot is empty (queueing
  earlier replaces the pending entry). Verified with 20 s tracks
  (`--segment-secs 20`): seamless switches, delay unchanged across them.
  A player stuck in `TRANSITIONING` (it then refuses `Stop` and drops new
  streams) is detected after 10 s and restarted; loading a fresh stream
  recovers it.
- **Handing back (0.4.2):** if the speaker stops pulling our stream while the
  Mac plays audio, and it now plays something else (another controller, a
  playlist), or drops us a second time after one reconnect, ls50streamd switches
  the Mac's default output back to the last one that wasn't ours (it notes the
  default output every second). If that fails it hides the device for 2 s so
  macOS falls back by itself. A plain drop of our own stream (network hiccup,
  Mac switching Wi-Fi/cable) is still retried once first.
- **Visibility:** the device only exists while a KEF LS50 Wireless answers
  SSDP searches (every 5–10 s; hidden after 3 misses). Any first-generation
  LS50W is used (UPnP `manufacturer` `KEF`, `modelDescription`
  `LS50 Wireless`; the LS50 Wireless II and other KEFs are ignored). With
  several on the network, ls50streamd stays with the one it has while it answers,
  else takes the lowest serial number; `--speaker <serial>` (repeatable) limits
  it to those speakers.
- **Starting the speaker:** when audio starts flowing to the device (it is
  selected and something plays), ls50streamd sends `SetAVTransportURI` + `Play`;
  the speaker switches itself to its network input. It is asked again after a
  rate change, or if it dropped our stream (e.g. someone switched its input).
  After 30 s without audio ls50streamd sends `Stop`. While idle it streams
  silence, so pauses resume instantly.
- Device volume/mute scale the signal digitally; the speaker's
  own volume is separate.

## Build and install (details)

`scripts/build.sh --release` signs with your Developer ID, notarizes and staples
the package; `scripts/release.sh` does that and publishes it as a GitHub
release `v<version>` (bump `version` in `engine/Cargo.toml` first).

```sh
scripts/build.sh                         # -> build/LS50Stream-<version>.pkg
open build/LS50Stream-*.pkg              # admin password; restarts system audio once
sudo "/Library/Application Support/LS50Stream/uninstall.sh"   # remove
```

Log: `/Library/Logs/LS50Stream/ls50streamd.log`.

## Development (no root)

```sh
(cd engine && cargo test && cargo build --release)
clang -O2 -fblocks -arch arm64 -bundle -framework CoreAudio -framework CoreFoundation \
    -o build/LS50Stream driver/LS50StreamDriver.c
clang -O2 -o build/harness driver/test/harness.c -framework CoreAudio -framework CoreFoundation
# Run ls50streamd as a user LaunchAgent with MachServices io.github.kaihagseth.ls50stream.audio.dev and
# args --dev --mach-service io.github.kaihagseth.ls50stream.audio.dev --port 8301, then:
LS50STREAM_MACH_SERVICE=io.github.kaihagseth.ls50stream.audio.dev build/harness build/LS50Stream 25 song.f32 192000
```

The harness loads the driver outside coreaudiod, checks its properties and
rate switching, waits for the device to be published, and plays raw float32
stereo (`ffmpeg -i song.mp3 -ar 192000 -ac 2 -f f32le song.f32`) through it.
Verified 2026-10-11: all checks pass, the LS50W played the stream, and the
speaker was released after 30 s idle. (The speaker opens one probe connection,
drops it and reconnects; that is normal.)

## Background: feasibility tests

`prototype/stream_test.py` serves an endless, real-time-paced WAV
(ffmpeg `-re` decoding a file, standing in for live capture), sends
`SetAVTransportURI` + `Play` to the speaker's AVTransport (port 8080), and
watches the speaker's own play state on port 10100.

Results against the LS50W:
- Accepts an endless `audio/wav` stream (no length) with
  `DLNA.ORG_PN=WAV`, `transferMode: Streaming`. It fetched the URL 0.7 s
  after Play, with no Range header (User-Agent `WinampMPEG/2.8`).
- Switches its input to Wi-Fi by itself when playback starts.
- 25–35 s of continuous real-time streaming per run, with no stalls or
  reconnects, including at 192 kHz/24-bit (~9.2 Mbit/s over Wi-Fi).
- **The speaker buffers a fixed amount of data (~0.7–1 MB), not a fixed
  time**, so a higher-bitrate stream means less delay:

  | Stream (WAV, stereo) | Data rate | First byte → playing | Steady delay |
  |---|---|---|---|
  | 44.1 kHz / 16-bit | 176 KB/s | 4.1 s | ~4 s |
  | 96 kHz / 24-bit | 576 KB/s | 1.8 s | ~1.7 s |
  | 192 kHz / 24-bit | 1.15 MB/s | 0.9 s | ~0.85 s |

  Delays are from the speaker's own reported position (10100 `timep`), not
  measured acoustically. Compressed formats (FLAC/MP3) would make the delay
  *worse*, so send uncompressed PCM, upsampled to 192 kHz/24-bit.

### Live capture prototype (superseded by the driver)

`prototype/capture/` is a Swift tool that captures the Mac's mixed output
with a Core Audio process tap (`CATapDescription`, global stereo tap, private
aggregate device) and writes float32 stereo to stdout. While it runs it mutes
the Mac's own output (`mutedWhenTapped`).

- It must run as its own app bundle (`build.sh` → `Capture.app`, launched
  with `open --stdout <fifo>`). Run directly from a terminal, macOS checks
  the *terminal app's* permission and silently delivers silence.
- First launch asks for "System Audio Recording". The ad-hoc signature
  changes on every rebuild, so macOS may ask again.
- `stream_test.py --capture --rate 192000 --bits 24` streams it: ffmpeg
  resamples 48 kHz → 192 kHz/24-bit WAV. The speaker played 0.75 s after the
  first byte, and listening felt "faster than expected". No reconnects or
  stalls over the first 100 s.


