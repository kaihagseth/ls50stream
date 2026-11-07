//! HTTP server for the speakers: GET /stream/<rate>/<track>.wav returns one
//! "track" of the live stream (see stream.rs). Track 0 starts a session; each
//! later track continues at exactly the byte where the previous one ended.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::time::Instant;

use crate::stream::{bytes_per_second, start_burst_bytes, start_burst_secs, track_bytes, wav_header, Chunk, Live};

/// Most audio we queue for a speaker (5 ms chunks): beyond this, after a network
/// stall, we drop the oldest instead of letting the delay grow for good.
const MAX_QUEUED_CHUNKS: usize = 100; // 0.5 s
/// How long a track request may wait for the previous track to finish.
const NEXT_TRACK_WAIT: Duration = Duration::from_secs(2 * 3600);

/// Number of speakers currently pulling the stream.
pub static LISTENERS: AtomicUsize = AtomicUsize::new(0);
/// Bytes sent in the current session (start burst included, all tracks), and
/// its rate: the delay monitor compares this with the speaker's play position.
pub static SENT_BYTES: AtomicU64 = AtomicU64::new(0);
pub static SENT_RATE: AtomicU32 = AtomicU32::new(0);
/// Incremented whenever track 0 starts a new session.
pub static SESSION: AtomicU64 = AtomicU64::new(0);
/// Length of each track in the current session, in ms of audio.
pub static TRACK_MS: AtomicU64 = AtomicU64::new(0);
/// Extra delay (ms) kept in the speaker's buffer as a margin against network
/// stalls; set by the delay monitor, used when a session starts.
pub static MARGIN_MS: AtomicU64 = AtomicU64::new(0);
/// Live audio to skip (ms) so the speaker's buffer drains back down; requested
/// by the delay monitor after the speaker ran dry and refilled.
pub static SKIP_MS: AtomicU64 = AtomicU64::new(0);
/// The margin never shrinks below this (set per link: 0 wired, more on Wi-Fi).
pub static MIN_MARGIN_MS: AtomicU64 = AtomicU64::new(0);

/// Where the next track of a session starts in the live stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Continuation {
    session: u64,
    rate: u32,
    track: u32,
    pos: u64,
}
static NEXT: Mutex<Option<Continuation>> = Mutex::new(None);

pub fn stream_path(rate: u32, track: u32) -> String {
    format!("/stream/{rate}/{track}.wav")
}

/// (rate, track) from a stream path.
pub fn parse_path(path: &str) -> Option<(u32, u32)> {
    let rest = path.strip_prefix("/stream/")?.strip_suffix(".wav")?;
    let (rate, track) = rest.split_once('/')?;
    Some((rate.parse().ok()?, track.parse().ok()?))
}

pub async fn serve(listener: TcpListener, live: Live, current_rate: Arc<dyn Fn() -> u32 + Send + Sync>) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let live = live.clone();
        let current_rate = current_rate.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, live, current_rate()).await {
                warn!("Stream to {peer} ended: {e}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, live: Live, rate: u32) -> std::io::Result<()> {
    let peer = stream.peer_addr()?;
    let request = read_head(&mut stream).await?;
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let wanted = parse_path(path);
    if (method != "GET" && method != "HEAD") || rate == 0 || wanted.map(|(r, _)| r) != Some(rate) {
        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let track = wanted.map_or(0, |(_, t)| t);

    let head = "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nCache-Control: no-cache\r\nConnection: close\r\n\
        transferMode.dlna.org: Streaming\r\n\
        contentFeatures.dlna.org: DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    if method == "HEAD" {
        return Ok(());
    }

    let data_bytes = track_bytes();
    // Track 0 starts a session; later tracks resume where the previous ended.
    let (session, resume_at) = if track == 0 {
        let session = SESSION.fetch_add(1, Ordering::Relaxed) + 1;
        SENT_BYTES.store(0, Ordering::Relaxed);
        SENT_RATE.store(rate, Ordering::Relaxed);
        TRACK_MS.store((data_bytes as f64 / bytes_per_second(rate) * 1000.0) as u64, Ordering::Relaxed);
        (session, None)
    } else {
        match wait_for_track(rate, track).await {
            Some(next) => (next.session, Some(next.pos)),
            None => {
                warn!("{peer} asked for track {track} of a session that is gone");
                return Ok(());
            }
        }
    };

    // No TCP_NODELAY: full-size segments matter on Wi-Fi, where many small
    // packets starved the speaker.
    stream.write_all(&wav_header(rate, data_bytes)).await?;
    let mut remaining = data_bytes as u64;
    let mut skip_until = Instant::now();
    if track == 0 {
        // Start burst: the speaker starts playing once it has this much, and
        // keeps whatever is queued, so send silence now and hold live audio back
        // until it has played, down to the floor plus the margin (stream.rs).
        let burst = (start_burst_bytes() as u64).min(remaining);
        stream.write_all(&vec![0u8; burst as usize]).await?;
        remaining -= burst;
        SENT_BYTES.fetch_add(burst, Ordering::Relaxed);
        let margin = MARGIN_MS.load(Ordering::Relaxed) as f64 / 1000.0;
        skip_until += Duration::from_secs_f64((start_burst_secs(rate) - margin).max(0.0));
        SKIP_MS.store(0, Ordering::Relaxed);
    }

    let (past, mut rx) = live.from(rate, resume_at.unwrap_or(u64::MAX));
    let mut past: VecDeque<Chunk> = if resume_at.is_some() { past.into() } else { VecDeque::new() };
    // Live position just after the last byte we took (sent or skipped)
    let mut cursor = resume_at;

    LISTENERS.fetch_add(1, Ordering::Relaxed);
    info!("Streaming {rate} Hz track {track} (session {session}) to {peer}");
    let mut dropped_secs = 0.0;
    let result: std::io::Result<()> = async {
        loop {
            let chunk = match past.pop_front() {
                Some(chunk) => chunk,
                None => match rx.recv().await {
                    Ok(chunk) => chunk,
                    Err(RecvError::Lagged(n)) => {
                        warn!("Stream to {peer} fell behind; skipped {n} chunks");
                        continue;
                    }
                    Err(RecvError::Closed) => return Ok(()),
                },
            };
            if chunk.rate != rate {
                info!("Device rate changed; ending the {rate} Hz stream to {peer}");
                return Ok(());
            }
            let Some(mut bytes) = after(&chunk, cursor) else { continue };

            // Draining (start burst, or after the speaker refilled): this audio
            // would only add delay
            let skip = SKIP_MS.swap(0, Ordering::Relaxed);
            if skip > 0 {
                skip_until = Instant::now() + Duration::from_millis(skip);
            }
            if Instant::now() < skip_until {
                cursor = Some(chunk.end());
                continue;
            }
            // After a stall, keep only the newest 0.5 s
            if past.is_empty() && rx.len() > MAX_QUEUED_CHUNKS {
                dropped_secs += bytes.len() as f64 / bytes_per_second(rate);
                cursor = Some(chunk.end());
                continue;
            }
            if dropped_secs >= 0.1 {
                warn!("Stream to {peer} stalled; dropped {dropped_secs:.2} s to keep the delay down");
                dropped_secs = 0.0;
            }

            // Send everything already queued in one write, up to the track's end
            let mut batch = Vec::with_capacity(bytes.len());
            let mut end = chunk.end();
            loop {
                let take = (remaining - batch.len() as u64).min(bytes.len() as u64) as usize;
                batch.extend_from_slice(&bytes[..take]);
                if take < bytes.len() {
                    end -= (bytes.len() - take) as u64; // the rest goes to the next track
                    break;
                }
                let next = match past.pop_front() {
                    Some(c) => c,
                    None => match rx.try_recv() {
                        Ok(c) => c,
                        Err(TryRecvError::Lagged(_)) | Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                    },
                };
                if next.rate != rate || next.pos != end {
                    past.push_front(next);
                    break;
                }
                end = next.end();
                bytes = next.bytes.clone();
            }
            stream.write_all(&batch).await?;
            remaining -= batch.len() as u64;
            SENT_BYTES.fetch_add(batch.len() as u64, Ordering::Relaxed);
            cursor = Some(end);

            if remaining == 0 {
                *NEXT.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(Continuation { session, rate, track: track + 1, pos: end });
                info!("Track {track} complete; track {} continues at byte {end}", track + 1);
                return Ok(());
            }
        }
    }
    .await;
    LISTENERS.fetch_sub(1, Ordering::Relaxed);
    info!("Stopped streaming track {track} to {peer}");
    result
}

/// The part of `chunk` at or after `cursor` (all of it without a cursor).
fn after(chunk: &Chunk, cursor: Option<u64>) -> Option<Arc<[u8]>> {
    match cursor {
        Some(c) if chunk.end() <= c => None,
        Some(c) if chunk.pos < c => Some(chunk.bytes[(c - chunk.pos) as usize..].into()),
        _ => Some(chunk.bytes.clone()),
    }
}

/// Waits until the previous track has ended (the speaker may ask early).
async fn wait_for_track(rate: u32, track: u32) -> Option<Continuation> {
    let deadline = Instant::now() + NEXT_TRACK_WAIT;
    let session = SESSION.load(Ordering::Relaxed);
    while Instant::now() < deadline && SESSION.load(Ordering::Relaxed) == session {
        let next = *NEXT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(next) = next.filter(|n| n.session == session && n.rate == rate && n.track == track) {
            return Some(next);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// Reads the request line and headers (up to the blank line).
async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 || head.len() > 16 * 1024 {
            return Err(std::io::Error::other("incomplete request"));
        }
        head.extend_from_slice(&buf[..n]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(stream_path(192000, 3), "/stream/192000/3.wav");
        assert_eq!(parse_path("/stream/192000/3.wav"), Some((192000, 3)));
        assert_eq!(parse_path("/stream/192000.wav"), None);
    }

    #[test]
    fn resumes_mid_chunk() {
        let chunk = Chunk { rate: 48000, pos: 600, bytes: (0u8..60).collect::<Vec<_>>().into() };
        assert_eq!(after(&chunk, None).unwrap().len(), 60);
        assert_eq!(&after(&chunk, Some(630)).unwrap()[..], &(30u8..60).collect::<Vec<_>>()[..]);
        assert!(after(&chunk, Some(660)).is_none());
    }
}
