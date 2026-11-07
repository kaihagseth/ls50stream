//! Measures the real delay while streaming: audio sent minus the position the
//! speaker reports playing (KEF display stream, port 10100: `timep`, whole
//! seconds since the stream started). Every 20 position updates the median is
//! logged and reported to macOS as the device latency, so video players keep
//! lip sync.
//!
//! It also keeps the delay low: if the speaker runs dry (e.g. a Wi-Fi stall) it
//! waits to refill its whole start-up buffer, and the delay jumps by seconds.
//! The monitor then keeps a larger margin, and skips live audio to drain back,
//! but at most every few minutes: skipping starves the speaker, which on a
//! stall-prone link can make it run dry again. After a long calm spell the
//! margin shrinks again, never below the link's minimum.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use std::time::Duration;

use log::{debug, info};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::http::{LISTENERS, MARGIN_MS, MIN_MARGIN_MS, SENT_BYTES, SENT_RATE, SESSION, SKIP_MS, TRACK_MS};
use crate::shm::SharedAudio;
use crate::stream::{bytes_per_second, speaker_delay_secs, POSITION_TO_SOUND_SECS};

const DISPLAY_PORT: u16 = 10100;
const REPORT_EVERY: usize = 20;
/// Smaller changes aren't passed on, so players don't re-sync for nothing.
const LATENCY_UPDATE_SECS: f64 = 0.03;
/// Recent position updates (one per second) judged together.
const WINDOW: usize = 5;
/// Updates ignored after a stream starts or a skip, while the buffer settles.
const SETTLE: usize = 8;
/// Delay this far above target means the speaker ran dry and refilled.
const REFILL_DETECT_SECS: f64 = 0.4;
const MARGIN_STEP_SECS: f64 = 0.5;
const MARGIN_MAX_SECS: f64 = 3.0;
/// Shrink the margin (by a quarter step) after this long without a refill.
const CALM_SPELL: Duration = Duration::from_secs(600);
/// At most one skip-to-recover per this long.
const SKIP_INTERVAL: Duration = Duration::from_secs(180);
/// Never skip more than this at once (a safety net against a bad measurement).
const MAX_SKIP_SECS: f64 = 5.0;

/// Runs forever; measures while a speaker is pulling our stream.
/// Measurement state; kept across reconnects to the speaker (our listener count
/// dips to 0 for a moment at each track switch).
struct Tracker {
    session: u64,
    /// The speaker's position restarts at each track: earlier tracks' length
    earlier_tracks_secs: f64,
    last_position: Option<u32>,
    delays: Vec<f64>,
    recent: VecDeque<f64>,
    settle: usize,
    last_change: Instant,
    last_skip: Option<Instant>,
}

pub async fn run(speaker: tokio::sync::watch::Receiver<Option<crate::discovery::Speaker>>, shared: Arc<SharedAudio>) {
    let mut t = Tracker {
        session: SESSION.load(Ordering::Relaxed),
        earlier_tracks_secs: 0.0,
        last_position: None,
        delays: Vec::new(),
        recent: VecDeque::new(),
        settle: SETTLE,
        last_change: Instant::now(),
        last_skip: None,
    };
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if LISTENERS.load(Ordering::Relaxed) == 0 {
            continue;
        }
        let Some(address) = speaker.borrow().as_ref().map(|s| SocketAddr::new(s.address.ip(), DISPLAY_PORT)) else {
            continue;
        };
        if let Err(e) = measure(address, &shared, &mut t).await {
            debug!("Delay monitor: {e}");
        }
    }
}

async fn measure(address: SocketAddr, shared: &SharedAudio, t: &mut Tracker) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(address).await?;
    let mut buf = String::new();
    let mut read = [0u8; 16 * 1024];
    let mut idle_since: Option<Instant> = None;
    loop {
        // Stop after the stream has really ended (not just between two tracks)
        if LISTENERS.load(Ordering::Relaxed) > 0 {
            idle_since = None;
        } else if idle_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
            return Ok(());
        }
        let n = match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut read)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return Ok(()),
            Ok(Ok(n)) => n,
            Err(_) => continue,
        };
        buf.push_str(&String::from_utf8_lossy(&read[..n]));
        while let Some(position) = take_position(&mut buf) {
            // Each change of the whole-second position is a precise moment:
            // the speaker has just played `position` seconds of the stream.
            if Some(position) == t.last_position || position == 0 {
                continue;
            }
            let current = SESSION.load(Ordering::Relaxed);
            if current != t.session {
                // A new stream started
                t.session = current;
                t.earlier_tracks_secs = 0.0;
                t.recent.clear();
                t.settle = SETTLE;
            } else if t.last_position.is_some_and(|last| position < last) {
                // The next track of the same stream
                t.earlier_tracks_secs += TRACK_MS.load(Ordering::Relaxed) as f64 / 1000.0;
                t.recent.clear();
                t.settle = SETTLE;
            }
            t.last_position = Some(position);
            let rate = SENT_RATE.load(Ordering::Relaxed);
            if rate == 0 {
                continue;
            }
            let sent_secs = SENT_BYTES.load(Ordering::Relaxed) as f64 / bytes_per_second(rate);
            let delay = sent_secs - (t.earlier_tracks_secs + position as f64);
            t.delays.push(delay);
            if t.settle > 0 {
                t.settle -= 1;
            } else {
                t.recent.push_back(delay);
                if t.recent.len() > WINDOW {
                    t.recent.pop_front();
                }
                if t.recent.len() == WINDOW && adjust(rate, &t.recent, &mut t.last_change, &mut t.last_skip) {
                    t.recent.clear();
                    t.settle = SETTLE;
                }
            }
            if t.delays.len() >= REPORT_EVERY {
                t.delays.sort_by(f64::total_cmp);
                let median = t.delays[t.delays.len() / 2];
                info!("Speaker delay: median {median:.2} s (range {:.2}–{:.2} s) at {rate} Hz", t.delays[0], t.delays[t.delays.len() - 1]);
                t.delays.clear();
                let reported = shared.latency_frames() as f64 / rate as f64;
                let latency = median + POSITION_TO_SOUND_SECS;
                if median > 0.0 && (latency - reported).abs() > LATENCY_UPDATE_SECS {
                    info!("Reporting {latency:.2} s latency to macOS (was {reported:.2} s)");
                    shared.set_latency_frames((latency * rate as f64) as u32);
                }
            }
        }
        if buf.len() > 64 * 1024 {
            buf.clear();
        }
    }
}

/// Keeps the speaker's buffer at its floor plus a margin. Returns true if it
/// changed something the next measurements need to settle from.
fn adjust(rate: u32, recent: &VecDeque<f64>, last_change: &mut Instant, last_skip: &mut Option<Instant>) -> bool {
    let mut sorted: Vec<f64> = recent.iter().copied().collect();
    sorted.sort_by(f64::total_cmp);
    let delay = sorted[sorted.len() / 2];
    let floor = speaker_delay_secs(rate);
    let margin = MARGIN_MS.load(Ordering::Relaxed) as f64 / 1000.0;
    let min_margin = MIN_MARGIN_MS.load(Ordering::Relaxed) as f64 / 1000.0;

    if delay > floor + margin + REFILL_DETECT_SECS {
        let new_margin = (margin + MARGIN_STEP_SECS).min(MARGIN_MAX_SECS);
        MARGIN_MS.store((new_margin * 1000.0) as u64, Ordering::Relaxed);
        *last_change = Instant::now();
        if last_skip.is_some_and(|t| t.elapsed() < SKIP_INTERVAL) {
            info!("Speaker refilled its buffer again (delay {delay:.2} s); margin now {new_margin:.2} s, not skipping");
            return false;
        }
        let skip = (delay - (floor + new_margin)).min(MAX_SKIP_SECS);
        info!("Speaker refilled its buffer (delay {delay:.2} s); skipping {skip:.2} s, margin now {new_margin:.2} s");
        SKIP_MS.store((skip * 1000.0) as u64, Ordering::Relaxed);
        *last_skip = Some(Instant::now());
        return true;
    }
    if margin > min_margin && last_change.elapsed() >= CALM_SPELL {
        let new_margin = (margin - MARGIN_STEP_SECS / 2.0).max(min_margin);
        info!("No refills for {} min; margin now {new_margin:.2} s", CALM_SPELL.as_secs() / 60);
        MARGIN_MS.store((new_margin * 1000.0) as u64, Ordering::Relaxed);
        SKIP_MS.store(((margin - new_margin) * 1000.0) as u64, Ordering::Relaxed);
        *last_change = Instant::now();
        return true;
    }
    false
}

/// Removes everything up to and including the next complete `play` fragment
/// from `buf`, returning its `timep` value if it has one.
fn take_position(buf: &mut String) -> Option<u32> {
    loop {
        let start = buf.find("<update id=\"play\">").or_else(|| buf.find("<view id=\"play\">"))?;
        let close = if buf[start..].starts_with("<update") { "</update>" } else { "</view>" };
        let end = start + buf[start..].find(close)? + close.len();
        let fragment = buf[start..end].to_string();
        buf.drain(..end);
        if let Some(position) = timep(&fragment) {
            return Some(position);
        }
    }
}

fn timep(fragment: &str) -> Option<u32> {
    let at = fragment.find("id=\"timep\"")?;
    let rest = &fragment[at..];
    let value = &rest[rest.find('>')? + 1..];
    value[..value.find('<')?].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_from_a_refill_and_widens_the_margin() {
        MARGIN_MS.store(0, Ordering::Relaxed);
        SKIP_MS.store(0, Ordering::Relaxed);
        let (mut last_change, mut last_skip) = (Instant::now(), None);
        // At target (48 kHz floor 0.74 s): nothing to do
        let calm: VecDeque<f64> = [0.75, 0.74, 0.76, 0.75, 0.74].into();
        assert!(!adjust(48000, &calm, &mut last_change, &mut last_skip));
        // The Wi-Fi case: ran dry and refilled to ~3.9 s
        let refilled: VecDeque<f64> = [3.89, 3.92, 3.92, 3.90, 3.91].into();
        assert!(adjust(48000, &refilled, &mut last_change, &mut last_skip));
        assert_eq!(MARGIN_MS.load(Ordering::Relaxed), 500);
        let skip = SKIP_MS.swap(0, Ordering::Relaxed);
        assert!((2650..=2680).contains(&skip), "{skip}"); // 3.91 - (0.74 + 0.5)
        // Another refill right away: widen the margin, but don't skip again
        assert!(!adjust(48000, &refilled, &mut last_change, &mut last_skip));
        assert_eq!(MARGIN_MS.load(Ordering::Relaxed), 1000);
        assert_eq!(SKIP_MS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn extracts_positions_from_play_updates() {
        let mut buf = String::from(
            "<view id=\"status\"><icon id=\"play\">play:4</icon></view>\
             <update id=\"play\">\r\n  <text id=\"scrid\">100</text>\r\n  <value id=\"timep\" min=\"0\" max=\"65535\">12</value>\r\n</update>\
             <update id=\"play\"><value id=\"buflvl\" min=\"0\" max=\"100\">50</value></update>\
             <update id=\"play\"><value id=\"timep\" min=\"0\" max=\"65535\">13</value></update><update id=\"pl",
        );
        assert_eq!(take_position(&mut buf), Some(12));
        assert_eq!(take_position(&mut buf), Some(13));
        assert_eq!(take_position(&mut buf), None);
        assert_eq!(buf, "<update id=\"pl");
    }
}
