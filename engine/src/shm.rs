//! Shared-memory audio ring written by LS50Stream.driver. Layout: driver/ls50stream_shm.h.
//! Adapted from Limbo (src/audio/shm.rs).

use std::ffi::CString;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

pub const MACH_SERVICE: &str = "io.github.kaihagseth.ls50stream.audio";
pub const CHANNELS: usize = 2;
const MAGIC: u32 = 0x5453_4344;
const VERSION: u32 = 1;
pub const RING_FRAMES: u64 = 65536;
const SAMPLES_OFFSET: usize = 256;
const SIZE: usize = SAMPLES_OFFSET + RING_FRAMES as usize * CHANNELS * 4;

#[repr(C)]
struct Header {
    magic: u32,
    version: u32,
    channels: u32,
    ring_frames: u32,
    sample_rate: AtomicU32,
    io_running: AtomicU32,
    generation: AtomicU32,
    io_frames: AtomicU32,
    anchor_host_time: AtomicU64,
    host_ticks_per_frame: AtomicU64, // f64 bits
    write_end: AtomicU64,
    latency_frames: AtomicU32,
    attached: AtomicU32,
    device_hidden: AtomicU32,
    reserved: [u32; 23],
}
const _: () = assert!(std::mem::size_of::<Header>() == 160);

extern "C" {
    fn ls50stream_xpc_listen(service: *const libc::c_char, region: *mut libc::c_void, size: usize, allowed_uid: libc::uid_t) -> libc::c_int;
}

/// The mmap'ed region shared with the driver.
pub struct SharedAudio {
    base: *mut u8,
}

// The region is only accessed through atomics
unsafe impl Send for SharedAudio {}
unsafe impl Sync for SharedAudio {}

impl SharedAudio {
    pub fn create() -> std::io::Result<Arc<Self>> {
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANON, -1, 0)
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let header = base as *mut Header;
        unsafe {
            (*header).magic = MAGIC;
            (*header).version = VERSION;
            (*header).channels = CHANNELS as u32;
            (*header).ring_frames = RING_FRAMES as u32;
        }
        let shared = Arc::new(Self { base: base as *mut u8 });
        // Hidden until a speaker is found
        shared.set_device_hidden(true);
        Ok(shared)
    }

    /// Serve the region to the driver over XPC. `allowed_uid`: the peer uid besides root
    /// (coreaudiod's, or our own when developing with the test harness).
    pub fn listen(&self, service: &str, allowed_uid: u32) -> std::io::Result<()> {
        let service = CString::new(service)?;
        match unsafe { ls50stream_xpc_listen(service.as_ptr(), self.base as *mut libc::c_void, SIZE, allowed_uid) } {
            0 => Ok(()),
            err => Err(std::io::Error::other(format!("XPC listener failed ({err})"))),
        }
    }

    fn header(&self) -> &Header {
        unsafe { &*(self.base as *const Header) }
    }

    fn sample_bits(&self, index: usize) -> &AtomicU32 {
        debug_assert!(index < RING_FRAMES as usize * CHANNELS);
        unsafe { &*(self.base.add(SAMPLES_OFFSET) as *const AtomicU32).add(index) }
    }

    pub fn driver_attached(&self) -> bool {
        self.header().attached.load(Ordering::Acquire) != 0
    }

    pub fn io_running(&self) -> bool {
        self.header().io_running.load(Ordering::Acquire) != 0
    }

    /// The device's nominal rate (0 until the driver has attached).
    pub fn sample_rate(&self) -> u32 {
        self.header().sample_rate.load(Ordering::Relaxed)
    }

    /// Show or remove the audio device (the driver checks a few times a second).
    pub fn set_device_hidden(&self, hidden: bool) {
        self.header().device_hidden.store(hidden as u32, Ordering::Release);
    }

    pub fn latency_frames(&self) -> u32 {
        self.header().latency_frames.load(Ordering::Relaxed)
    }

    pub fn set_latency_frames(&self, frames: u32) {
        self.header().latency_frames.store(frames, Ordering::Relaxed);
    }

    /// Driver side of the protocol, for tests.
    #[cfg(test)]
    pub fn driver_write(&self, generation: u32, rate: u32, start: u64, frames: &[f32]) {
        let h = self.header();
        h.sample_rate.store(rate, Ordering::Relaxed);
        if h.generation.load(Ordering::Acquire) != generation {
            h.write_end.store(0, Ordering::Release);
            h.generation.store(generation, Ordering::Release);
        }
        h.io_running.store(1, Ordering::Release);
        for (i, frame) in frames.as_chunks::<CHANNELS>().0.iter().enumerate() {
            let base = ((start + i as u64) % RING_FRAMES) as usize * CHANNELS;
            for (ch, s) in frame.iter().enumerate() {
                self.sample_bits(base + ch).store(s.to_bits(), Ordering::Relaxed);
            }
        }
        h.write_end.store(start + (frames.len() / CHANNELS) as u64, Ordering::Release);
    }

    #[cfg(test)]
    pub fn driver_stop(&self) {
        self.header().io_running.store(0, Ordering::Release);
    }
}

impl Drop for SharedAudio {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, SIZE) };
    }
}

/// Reads the driver's output as it is written, or silence at the clock's pace
/// while the device is idle, so the stream to the speaker never stalls.
pub struct RingReader {
    shared: Arc<SharedAudio>,
    generation: u32,
    read_pos: u64,
    /// Frames of silence owed to keep real-time pace while idle (fractional).
    idle_debt: f64,
}

impl RingReader {
    pub fn new(shared: Arc<SharedAudio>) -> Self {
        Self { shared, generation: 0, read_pos: 0, idle_debt: 0.0 }
    }

    /// Appends the frames that became available since the last call to `out`
    /// (interleaved stereo float32). `elapsed_secs` is the wall time since the
    /// last call, used to produce silence while the device is idle.
    /// Returns the number of frames appended.
    pub fn read(&mut self, out: &mut Vec<f32>, elapsed_secs: f64, rate: u32) -> usize {
        let h = self.shared.header();
        let generation = h.generation.load(Ordering::Acquire);
        let running = h.io_running.load(Ordering::Acquire) != 0;
        let write_end = h.write_end.load(Ordering::Acquire);

        if !running || generation == 0 {
            self.idle_debt += elapsed_secs * rate as f64;
            // Never owe more than a quarter second (e.g. after a long stall)
            self.idle_debt = self.idle_debt.min(rate as f64 / 4.0);
            let frames = self.idle_debt as usize;
            self.idle_debt -= frames as f64;
            out.resize(out.len() + frames * CHANNELS, 0.0);
            return frames;
        }
        self.idle_debt = 0.0;

        if generation != self.generation {
            // A new timeline: start at what's being written now
            self.generation = generation;
            self.read_pos = write_end;
            return 0;
        }
        if write_end <= self.read_pos {
            return 0;
        }
        let mut available = write_end - self.read_pos;
        if available > RING_FRAMES / 2 {
            // We fell far behind (or the timeline jumped): skip to recent audio
            self.read_pos = write_end - RING_FRAMES / 8;
            available = RING_FRAMES / 8;
        }
        out.reserve(available as usize * CHANNELS);
        for pos in self.read_pos..write_end {
            let base = (pos % RING_FRAMES) as usize * CHANNELS;
            for ch in 0..CHANNELS {
                out.push(f32::from_bits(self.shared.sample_bits(base + ch).load(Ordering::Relaxed)));
            }
        }
        self.read_pos = write_end;
        available as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: u64, frames: usize) -> Vec<f32> {
        (0..frames * CHANNELS).map(|i| (start as usize * CHANNELS + i) as f32 / 1e6).collect()
    }

    #[test]
    fn follows_the_driver_from_the_current_position() {
        let shared = SharedAudio::create().unwrap();
        let mut reader = RingReader::new(shared.clone());
        let mut out = Vec::new();
        shared.driver_write(1, 192000, 0, &ramp(0, 512));
        // First sight of a timeline: nothing old is replayed
        assert_eq!(reader.read(&mut out, 0.0, 192000), 0);
        shared.driver_write(1, 192000, 512, &ramp(512, 256));
        assert_eq!(reader.read(&mut out, 0.001, 192000), 256);
        assert_eq!(out.len(), 512);
        assert_eq!(out[0], ramp(512, 1)[0]);
        assert_eq!(out[511], ramp(512, 256)[511]);
    }

    #[test]
    fn produces_silence_at_clock_pace_while_idle() {
        let shared = SharedAudio::create().unwrap();
        let mut reader = RingReader::new(shared.clone());
        let mut out = Vec::new();
        assert_eq!(reader.read(&mut out, 0.0105, 48000), 504);
        assert_eq!(reader.read(&mut out, 0.0105, 48000), 504);
        assert!(out.iter().all(|s| *s == 0.0));
        // Long stall: capped at a quarter second
        out.clear();
        assert_eq!(reader.read(&mut out, 10.0, 48000), 12000);
    }

    #[test]
    fn restarts_on_a_new_timeline_and_after_stopping() {
        let shared = SharedAudio::create().unwrap();
        let mut reader = RingReader::new(shared.clone());
        let mut out = Vec::new();
        shared.driver_write(1, 96000, 0, &ramp(0, 100));
        reader.read(&mut out, 0.0, 96000);
        shared.driver_stop();
        assert!(reader.read(&mut out, 0.01, 96000) > 0); // silence
        out.clear();
        shared.driver_write(2, 96000, 0, &ramp(0, 100));
        assert_eq!(reader.read(&mut out, 0.0, 96000), 0);
        shared.driver_write(2, 96000, 100, &ramp(100, 10));
        assert_eq!(reader.read(&mut out, 0.0, 96000), 10);
        assert_eq!(out[0], ramp(100, 1)[0]);
    }

    #[test]
    fn skips_ahead_when_far_behind() {
        let shared = SharedAudio::create().unwrap();
        let mut reader = RingReader::new(shared.clone());
        let mut out = Vec::new();
        shared.driver_write(1, 192000, 0, &ramp(0, 16));
        reader.read(&mut out, 0.0, 192000);
        shared.driver_write(1, 192000, 16, &ramp(16, 40000));
        let n = reader.read(&mut out, 0.0, 192000);
        assert_eq!(n as u64, RING_FRAMES / 8);
    }
}
