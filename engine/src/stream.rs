//! The audio stream: reads the device's output from shared memory and publishes
//! it as 24-bit little-endian PCM chunks for the HTTP server.
//!
//! The speaker waits for ~1 MB before it starts playing, and whatever is queued
//! then stays queued. So each stream starts with that much silence at once, then
//! holds back live audio until the speaker has played it, leaving only the
//! speaker's internal floor (measured over Ethernet, 2026-10-11): 0.33 s at
//! 192 kHz, 0.76 s at 96 kHz, 0.74 s at 48 kHz (vs. 0.5, 2.25 and 3.5 s
//! without). 24-bit is the deepest format the LS50W accepts (32-bit is refused).
//!
//! The speaker honours the WAV header's data size exactly (it hangs up after
//! it; 0 is refused), so the endless stream is served as consecutive "tracks"
//! of at most ~4 GB, chained gaplessly with SetNextAVTransportURI. Chunks carry
//! their position in the live stream, and the last few seconds are kept, so
//! each track can resume at exactly the byte where the previous one ended.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::info;
use tokio::sync::broadcast;

use crate::shm::{RingReader, SharedAudio, CHANNELS};

pub const BITS: u32 = 24;
const BYTES_PER_SAMPLE: usize = 3;
/// What the speaker wants buffered before it starts playing, in bytes.
const START_BUFFER_BYTES: f64 = 1_040_000.0;
const TICK: Duration = Duration::from_millis(5);

/// Recent chunks kept for a track that starts a moment after the previous ended.
const HISTORY: Duration = Duration::from_secs(3);
const WAV_HEADER_BYTES: u32 = 68;

/// One piece of the stream, at `rate`.
#[derive(Clone)]
pub struct Chunk {
    pub rate: u32,
    /// Byte offset of `bytes` in the live stream (restarts at 0 on a rate change).
    pub pos: u64,
    pub bytes: Arc<[u8]>,
}

impl Chunk {
    pub fn end(&self) -> u64 {
        self.pos + self.bytes.len() as u64
    }
}

/// The live stream: new chunks as they come, plus the last few seconds.
#[derive(Clone)]
pub struct Live {
    pub tx: broadcast::Sender<Chunk>,
    history: Arc<Mutex<VecDeque<Chunk>>>,
}

impl Live {
    /// Chunks from byte `from` on, at `rate`: what history still has, and a
    /// receiver for the rest (subscribed first, so nothing falls in between;
    /// the caller skips chunks it already got from history).
    pub fn from(&self, rate: u32, from: u64) -> (Vec<Chunk>, broadcast::Receiver<Chunk>) {
        let rx = self.tx.subscribe();
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let past = history.iter().filter(|c| c.rate == rate && c.end() > from).cloned().collect();
        (past, rx)
    }
}

/// Header of a WAV track holding `data_bytes` of audio, WAVE_FORMAT_EXTENSIBLE
/// as ffmpeg writes it.
pub fn wav_header(rate: u32, data_bytes: u32) -> Vec<u8> {
    let channels = CHANNELS as u16;
    let block_align = channels as u32 * BYTES_PER_SAMPLE as u32;
    let mut h = Vec::with_capacity(68);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(data_bytes + (WAV_HEADER_BYTES - 8)).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&40u32.to_le_bytes());
    h.extend_from_slice(&0xFFFEu16.to_le_bytes()); // WAVE_FORMAT_EXTENSIBLE
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * block_align).to_le_bytes());
    h.extend_from_slice(&(block_align as u16).to_le_bytes());
    h.extend_from_slice(&(BITS as u16).to_le_bytes());
    h.extend_from_slice(&22u16.to_le_bytes()); // extension size
    h.extend_from_slice(&(BITS as u16).to_le_bytes()); // valid bits
    h.extend_from_slice(&3u32.to_le_bytes()); // front left | front right
    // KSDATAFORMAT_SUBTYPE_PCM
    h.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]);
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_bytes.to_le_bytes());
    debug_assert_eq!(h.len(), WAV_HEADER_BYTES as usize);
    h
}

/// Audio bytes per track: as much as a WAV header can describe (~1 h at
/// 192 kHz, ~4 h at 48 kHz).
pub fn track_bytes() -> u32 {
    let frame = (CHANNELS * BYTES_PER_SAMPLE) as u32;
    (u32::MAX - WAV_HEADER_BYTES) / frame * frame
}

/// float32 samples -> signed 24-bit little-endian
pub fn to_s24le(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * BYTES_PER_SAMPLE);
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    }
}

pub fn bytes_per_second(rate: u32) -> f64 {
    rate as f64 * (CHANNELS * BYTES_PER_SAMPLE) as f64
}

/// Silence sent at the start of each stream, so the speaker starts at once.
pub fn start_burst_bytes() -> usize {
    let frame = CHANNELS * BYTES_PER_SAMPLE;
    (START_BUFFER_BYTES as usize / frame) * frame
}

/// How long the start burst plays: live audio is held back this long.
pub fn start_burst_secs(rate: u32) -> f64 {
    start_burst_bytes() as f64 / bytes_per_second(rate)
}

/// Steady delay from sending audio to the speaker playing it, after the start
/// burst has drained: the starting value reported to macOS until the delay
/// monitor has measured the real one (192/48 kHz from test streams, 96 kHz from
/// the device in use; Ethernet).
pub fn speaker_delay_secs(rate: u32) -> f64 {
    match rate {
        r if r >= 192_000 => 0.33,
        r if r >= 96_000 => 0.46,
        _ => 0.74,
    }
}

/// Sound leaves the speakers this long after the speaker reports it as played
/// (its `timep` position): judged by lip sync in Safari, which trusts the
/// device latency (2026-10-11: sound ~4 frames at 60 fps late without it).
pub const POSITION_TO_SOUND_SECS: f64 = 0.07;

/// The delay between audio entering the device and leaving the speakers, in frames.
pub fn latency_frames(rate: u32) -> u32 {
    ((speaker_delay_secs(rate) + POSITION_TO_SOUND_SECS) * rate as f64) as u32
}

/// Starts the producer thread.
pub fn start(shared: Arc<SharedAudio>) -> Live {
    // ~5 s of 5 ms chunks to absorb Wi-Fi stalls; slower readers lose the oldest
    let (tx, _) = broadcast::channel(1000);
    let live = Live { tx, history: Arc::new(Mutex::new(VecDeque::new())) };
    let producer = live.clone();
    std::thread::Builder::new()
        .name("stream".into())
        .spawn(move || produce(shared, producer))
        .expect("spawn stream thread");
    live
}

fn produce(shared: Arc<SharedAudio>, live: Live) {
    let mut reader = RingReader::new(shared.clone());
    let mut samples = Vec::new();
    let mut last = Instant::now();
    let mut last_rate = 0;
    let mut pos = 0u64;
    loop {
        std::thread::sleep(TICK);
        let now = Instant::now();
        let elapsed = now.duration_since(last).as_secs_f64();
        last = now;

        let rate = shared.sample_rate();
        if rate == 0 {
            continue; // driver not attached yet
        }
        if rate != last_rate {
            info!("Device rate {rate} Hz: streaming 24-bit PCM, {:.2} s speaker delay", latency_frames(rate) as f64 / rate as f64);
            shared.set_latency_frames(latency_frames(rate));
            last_rate = rate;
            pos = 0;
        }

        samples.clear();
        if reader.read(&mut samples, elapsed, rate) == 0 {
            continue;
        }
        let mut bytes = Vec::new();
        to_s24le(&samples, &mut bytes);
        let chunk = Chunk { rate, pos, bytes: bytes.into() };
        pos = chunk.end();
        {
            let mut history = live.history.lock().unwrap_or_else(|e| e.into_inner());
            history.push_back(chunk.clone());
            let keep = (bytes_per_second(rate) * HISTORY.as_secs_f64()) as u64;
            while history.front().is_some_and(|c| c.rate != rate || c.end() + keep < pos) {
                history.pop_front();
            }
        }
        // No receivers is fine: nobody is listening right now
        let _ = live.tx.send(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_ffmpeg() {
        // ffmpeg -f lavfi -i anullsrc=r=192000:cl=stereo -acodec pcm_s24le -f wav pipe:1 (first 60 bytes)
        let ffmpeg: [u8; 60] = [
            0x52, 0x49, 0x46, 0x46, 0xff, 0xff, 0xff, 0xff, 0x57, 0x41, 0x56, 0x45, 0x66, 0x6d, 0x74, 0x20,
            0x28, 0x00, 0x00, 0x00, 0xfe, 0xff, 0x02, 0x00, 0x00, 0xee, 0x02, 0x00, 0x00, 0x94, 0x11, 0x00,
            0x06, 0x00, 0x18, 0x00, 0x16, 0x00, 0x18, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        // ffmpeg's piped header claims the maximum size, like our largest track
        let h = wav_header(192000, u32::MAX - 60);
        assert_eq!(&h[..60], &ffmpeg);
        assert_eq!(&h[60..64], b"data");
        let h = wav_header(48000, 2_880_000);
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 2_880_060);
        assert_eq!(u32::from_le_bytes(h[64..68].try_into().unwrap()), 2_880_000);
    }

    #[test]
    fn track_sizes() {
        // Whole frames, as much as fits: ~3,728 s at 192 kHz
        let max = track_bytes();
        assert_eq!(max % 6, 0);
        assert!(max as u64 + 68 <= u32::MAX as u64);
        assert!((max as f64 / bytes_per_second(192000) - 3728.0).abs() < 1.0);
    }

    #[test]
    fn converts_to_24_bit() {
        let mut out = Vec::new();
        to_s24le(&[0.0, 1.0, -1.0, 2.0, 0.5], &mut out);
        assert_eq!(out, [0, 0, 0, 0xff, 0xff, 0x7f, 0x01, 0x00, 0x80, 0xff, 0xff, 0x7f, 0x00, 0x00, 0x40]);
    }

    #[test]
    fn start_burst_and_latency() {
        // ~1 MB of whole frames: 0.9 s at 192 kHz, 1.8 s at 96, 3.6 s at 48
        for (rate, secs) in [(192000, 0.9), (96000, 1.8), (48000, 3.6)] {
            assert_eq!(start_burst_bytes() % 6, 0);
            assert!((start_burst_secs(rate) - secs).abs() < 0.02, "{rate}");
        }
        assert_eq!(latency_frames(192000), 76800); // 0.40 s
        assert_eq!(latency_frames(96000), 50880); // 0.53 s
    }
}
