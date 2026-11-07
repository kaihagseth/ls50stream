//! ls50streamd: streams the "KEF LS50W" audio device (LS50Stream.driver) to the speakers over DLNA.
//!
//! * Serves the driver's shared-memory ring over XPC and reads the device's output from it.
//! * Publishes the device only while a KEF LS50 Wireless answers SSDP searches.
//! * When audio starts flowing to the device, tells the speaker (UPnP AVTransport) to play our
//!   HTTP stream; after a while without audio, tells it to stop.
//! * Keeps the next "track" of the stream queued (SetNextAVTransportURI), since the speaker
//!   stops at the end of each WAV track, and restarts a player stuck in TRANSITIONING.

mod discovery;
mod http;
mod link;
mod monitor;
mod output;
mod renderer;
mod shm;
mod stream;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use log::{error, info, warn};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tokio::time::Instant;

use discovery::Speaker;
use shm::SharedAudio;

const DEFAULT_PORT: u16 = 8300;
/// Release the speaker after this long without audio.
const IDLE_RELEASE: Duration = Duration::from_secs(30);
/// If the speaker drops our stream while audio plays, ask it once to reconnect after this
/// long (e.g. a network hiccup, or our address changed). If it drops it again, or plays
/// something else, the Mac's audio goes back to its previous output (see HAND_BACK).
const RECONNECT_AFTER: Duration = Duration::from_secs(5);
/// After handing audio back, don't cast again for this long (the device keeps
/// running until macOS has moved to the other output). If switching the output
/// fails, the device is hidden this long instead, so macOS falls back by itself.
const HAND_BACK: Duration = Duration::from_secs(2);
/// How often to note the Mac's default output, to know where to hand audio back to.
const OUTPUT_CHECK: Duration = Duration::from_secs(1);
/// Buffer margin while the Mac reaches the speaker over Wi-Fi: covers the 1-2 s
/// stalls seen there (0 on a cable: the speaker's floor is enough).
const WIFI_MARGIN_MS: u64 = 1500;
/// Searches a present speaker may miss before the device is hidden.
const MISSES_BEFORE_GONE: u32 = 3;
/// How often to check the speaker's current/next track while it plays our stream.
const TRACK_CHECK: Duration = Duration::from_secs(2);
/// A player in TRANSITIONING this long is stuck (it then refuses Stop and drops streams).
const STUCK_AFTER: Duration = Duration::from_secs(10);

struct Args {
    port: u16,
    /// Serial numbers (UPnP serialNumber) to restrict to; empty: any LS50W.
    speakers: Vec<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        port: DEFAULT_PORT,
        speakers: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--port" => args.port = it.next().and_then(|p| p.parse().ok()).expect("--port <number>"),
            "--speaker" => args.speakers.push(it.next().expect("--speaker <serial>")),
            other => {
                eprintln!("usage: ls50streamd [--port N] [--speaker SERIAL]...\nunknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}

fn coreaudiod_uid() -> u32 {
    let name = std::ffi::CString::new("_coreaudiod").unwrap();
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() { 202 } else { unsafe { (*pw).pw_uid } }
}

/// Keeps `tx` up to date with the LS50W on the network. With several, it stays with the one it
/// has while that one answers, else takes the lowest serial number (pin one with `--speaker`).
async fn watch_speakers(only: Vec<String>, tx: watch::Sender<Option<Speaker>>) {
    let mut misses = 0;
    loop {
        let found = discovery::search(&only, Duration::from_millis(1500)).await;
        let current = tx.borrow().as_ref().map(|s| s.serial.clone());
        let pick = match found.iter().position(|s| Some(&s.serial) == current.as_ref()) {
            Some(i) => found.into_iter().nth(i),
            None => found.into_iter().next(),
        };
        match pick {
            Some(speaker) => {
                misses = 0;
                if tx.borrow().as_ref() != Some(&speaker) {
                    info!("Found {} ({}) at {}", speaker.name, speaker.serial, speaker.address.ip());
                    tx.send_replace(Some(speaker));
                }
            }
            None if tx.borrow().is_some() => {
                misses += 1;
                if misses >= MISSES_BEFORE_GONE {
                    info!("Speaker no longer answers; hiding the device");
                    tx.send_replace(None);
                }
            }
            None => {}
        }
        let pause = if tx.borrow().is_some() { 10 } else { 5 };
        tokio::time::sleep(Duration::from_secs(pause)).await;
    }
}

/// Our address on the speaker's network.
async fn local_ip_towards(speaker: &Speaker) -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    socket.connect(speaker.address).await.ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    let wanted = if args.speakers.is_empty() { "any KEF LS50 Wireless".to_string() } else { args.speakers.join(", ") };
    info!("ls50streamd {} starting; speakers: {wanted}", env!("CARGO_PKG_VERSION"));

    let shared = SharedAudio::create()?;
    shared.listen(shm::MACH_SERVICE, coreaudiod_uid())?;
    info!("Serving audio to the driver on mach service '{}'", shm::MACH_SERVICE);

    let live = stream::start(shared.clone());
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, args.port))).await?;
    let rate_source = shared.clone();
    tokio::spawn(http::serve(listener, live, Arc::new(move || rate_source.sample_rate())));

    let (speaker_tx, mut speaker_rx) = watch::channel(None);
    tokio::spawn(watch_speakers(args.speakers.clone(), speaker_tx));
    tokio::spawn(monitor::run(speaker_rx.clone(), shared.clone()));

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    // The rate the speaker was last told to play, while we hold it
    let mut casting: Option<u32> = None;
    let mut idle_since: Option<Instant> = None;
    let mut was_running = false;
    let mut retry_at = Instant::now();
    let mut driver_seen = false;
    let mut unheard_since: Option<Instant> = None;
    let mut link_wifi: Option<bool> = None;
    let mut reconnected = false;
    let mut next_check = Instant::now();
    let mut stuck_since: Option<Instant> = None;
    let mut hidden_until: Option<Instant> = None;
    let mut no_cast_until: Option<Instant> = None;
    // The last default output that wasn't ours
    let mut previous_output: Option<String> = None;
    let mut next_output_check = Instant::now();

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = speaker_rx.changed() => {}
            _ = shutdown_signal() => break,
        }
        if !driver_seen && shared.driver_attached() {
            driver_seen = true;
            info!("Audio driver attached");
        }
        let speaker = speaker_rx.borrow().clone();
        shared.set_device_hidden(speaker.is_none() || hidden_until.is_some_and(|t| Instant::now() < t));
        if Instant::now() >= next_output_check {
            next_output_check = Instant::now() + OUTPUT_CHECK;
            if let Ok(Some(uid)) = tokio::task::spawn_blocking(output::default_output).await {
                if uid != output::OUR_UID {
                    previous_output = Some(uid);
                }
            }
        }
        let Some(speaker) = speaker else {
            casting = None;
            was_running = false;
            continue;
        };

        let running = shared.io_running();
        let rate = shared.sample_rate();
        if running {
            idle_since = None;
            // Audio starts again after the speaker dropped our stream (e.g. someone
            // switched its input): take it back.
            let listeners = http::LISTENERS.load(Ordering::Relaxed);
            if !was_running {
                reconnected = false;
                if listeners == 0 {
                    casting = None;
                }
            }
            if casting.is_some() && listeners == 0 {
                let since = *unheard_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= RECONNECT_AFTER {
                    let foreign = renderer::media_info(&speaker)
                        .await
                        .is_ok_and(|(current, _)| !is_our_stream(&current, &args));
                    casting = None;
                    if foreign || reconnected {
                        info!(
                            "{} {}; handing audio back to the previous output",
                            speaker.name,
                            if foreign { "plays something else" } else { "dropped the stream again" }
                        );
                        no_cast_until = Some(Instant::now() + HAND_BACK);
                        let switched = match previous_output.clone() {
                            Some(uid) => tokio::task::spawn_blocking(move || output::set_default_output(&uid))
                                .await
                                .unwrap_or(false),
                            None => false,
                        };
                        if switched {
                            info!("Switched output to {}", previous_output.as_deref().unwrap_or("?"));
                        } else {
                            warn!("Could not switch the output back; hiding the device briefly");
                            hidden_until = no_cast_until;
                        }
                    } else {
                        info!("{} dropped the stream; asking it to reconnect", speaker.name);
                        reconnected = true;
                    }
                }
            } else {
                unheard_since = None;
            }
        } else if casting.is_some() {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= IDLE_RELEASE {
                info!("No audio for {} s; releasing {}", IDLE_RELEASE.as_secs(), speaker.name);
                if let Err(e) = renderer::stop(&speaker).await {
                    warn!("Could not stop {}: {e}", speaker.name);
                }
                casting = None;
                idle_since = None;
            }
        }
        was_running = running;

        if casting.is_some() && Instant::now() >= next_check {
            next_check = Instant::now() + TRACK_CHECK;
            keep_next_track_queued(&speaker, &args).await;
            match renderer::transport_state(&speaker).await.as_deref() {
                Ok("TRANSITIONING") => {
                    let since = *stuck_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= STUCK_AFTER {
                        warn!("{} is stuck in TRANSITIONING; restarting the stream", speaker.name);
                        casting = None;
                        stuck_since = None;
                    }
                }
                _ => stuck_since = None,
            }
        }

        let handing_back = no_cast_until.is_some_and(|t| Instant::now() < t);
        if running && rate != 0 && casting != Some(rate) && !handing_back && Instant::now() >= retry_at {
            let Some(ip) = local_ip_towards(&speaker).await else {
                warn!("No route to {}", speaker.address);
                retry_at = Instant::now() + Duration::from_secs(5);
                continue;
            };
            let interface = link::interface_with(ip);
            let wifi = interface.as_ref().is_some_and(|i| link::wifi_interfaces().contains(i));
            if link_wifi != Some(wifi) {
                link_wifi = Some(wifi);
                let margin = if wifi { WIFI_MARGIN_MS } else { 0 };
                info!(
                    "Reaching {} over {} ({}): {:.1} s buffer margin",
                    speaker.name,
                    if wifi { "Wi-Fi" } else { "a cable" },
                    interface.as_deref().unwrap_or("?"),
                    margin as f64 / 1000.0
                );
                http::MIN_MARGIN_MS.store(margin, Ordering::Relaxed);
                http::MARGIN_MS.store(margin, Ordering::Relaxed);
            }
            let url = format!("http://{ip}:{}{}", args.port, http::stream_path(rate, 0));
            info!("Asking {} to play {url}", speaker.name);
            match renderer::play(&speaker, &url, rate).await {
                Ok(()) => casting = Some(rate),
                Err(e) => {
                    error!("{} did not start: {e}", speaker.name);
                    retry_at = Instant::now() + Duration::from_secs(5);
                }
            }
        }
    }

    info!("Shutting down");
    shared.set_device_hidden(true);
    if casting.is_some() {
        if let Some(speaker) = speaker_rx.borrow().clone() {
            let _ = renderer::stop(&speaker).await;
        }
    }
    // Give the driver a moment to see the device go away
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// If the speaker plays track k of our stream and nothing is queued after it,
/// queues track k+1. (Queueing while the next slot is still taken replaces that
/// entry, and the speaker clears the slot when it moves on.)
async fn keep_next_track_queued(speaker: &Speaker, args: &Args) {
    let Ok((current, next)) = renderer::media_info(speaker).await else { return };
    if !next.is_empty() {
        return;
    }
    let Some((address, path)) = discovery::split_url(&current) else { return };
    let Some((rate, track)) = http::parse_path(&path) else { return };
    if address.port() != args.port {
        return;
    }
    let url = format!("http://{}{}", address, http::stream_path(rate, track + 1));
    match renderer::set_next(speaker, &url, rate).await {
        Ok(()) => info!("Queued track {} after track {track}", track + 1),
        Err(e) => warn!("Could not queue track {}: {e}", track + 1),
    }
}

/// Whether `url` is one of our stream's tracks.
fn is_our_stream(url: &str, args: &Args) -> bool {
    discovery::split_url(url).is_some_and(|(address, path)| {
        address.port() == args.port && http::parse_path(&path).is_some()
    })
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
