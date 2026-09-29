//! In-process playback engine — the same librespot wiring `lightify-audio` (the
//! standalone binary the shipped Tauri host still spawns as its own subprocess)
//! uses, embedded directly in this process instead. One `.exe`, not two: the
//! shell used to spawn `lightify-audio.exe` as a child process and talk to it
//! over stdin/stdout JSON lines; that required shipping a second binary next to
//! the first just for this to work at all. `librespot` is a normal Rust library,
//! so there was no real reason for the split here (the shipped host's OWN reasons
//! for keeping it a subprocess — sharing one engine binary across Tauri's IPC
//! model — don't apply to the shell). This module runs on a dedicated OS thread
//! with its own tokio runtime and talks to the rest of the shell purely through
//! in-memory channels; `engine.rs` still exposes the exact same public API
//! (`start`/`send`/`running`/`shutdown`, the same `Event` enum) so nothing else
//! in `main.rs` needed to change.
//!
//! A crash or fatal error in here must **never** take the whole app down with
//! it (the old subprocess could die on its own; a `std::process::exit` in a
//! background thread of this process would kill the UI too) — every fallible
//! step returns `Result` and propagates up to `run` as an `Event::Failed` +
//! `Event::Exited`, not a process exit.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use librespot_connect::{ConnectConfig, LoadRequest, LoadRequestOptions, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_metadata::audio::item::UniqueFields;
use librespot_oauth::OAuthClientBuilder;
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::config::PlayerConfig;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::{self, MixerConfig};
use librespot_playback::player::Player;
use serde::Deserialize;

use crate::engine::Event;

const DEVICE_NAME: &str = "Lightify";
// Blip-gate fail-safe: never keep a track muted longer than this (see TappedSink).
const GATE_MAX_MUTE_MS: u64 = 1500;

// Streaming auth (unchanged from lightify-audio — see the long comment there for
// why this can't use the Web-API token): a dedicated one-time OAuth against
// Spotify's keymaster client, cached afterward so later launches are silent.
const STREAM_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const STREAM_REDIRECT_URI: &str = "http://127.0.0.1:8898/login";
const STREAM_AUTH_MARKER: &str = ".lightify_stream_oauth_v1";
const STREAM_AUTH_TIMEOUT_SECS: u64 = 180;
/// Cap on resolving a station's tracks (see `Cmd::StationTracks`).
const STATION_RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Commands the UI thread can send in. Kept as the same tagged-JSON shape the
/// old stdin protocol used (`{"cmd":"repeat","mode":"track"}` etc.) purely so
/// every existing `engine::send(&serde_json::json!({...}).to_string())` call
/// site in `main.rs` keeps working unchanged — `engine::send` parses it into
/// this enum and posts it down an in-memory channel instead of a pipe.
#[derive(Deserialize)]
#[serde(tag = "cmd")]
pub enum Cmd {
    #[serde(rename = "play")] Play,
    #[serde(rename = "pause")] Pause,
    #[serde(rename = "next")] Next,
    #[serde(rename = "prev")] Prev,
    #[serde(rename = "seek")] Seek { position_ms: u32 },
    #[serde(rename = "volume")] Volume { percent: u16 },
    #[serde(rename = "shuffle")] Shuffle { enabled: bool },
    #[serde(rename = "repeat")] Repeat { #[serde(default)] mode: String },
    #[serde(rename = "stop")] Stop,
    #[serde(rename = "expect")] Expect { #[serde(default)] ids: Vec<String> },
    #[serde(rename = "autoplay")] Autoplay { enabled: bool },
    #[serde(rename = "station")] Station { context_uri: String },
    #[serde(rename = "stationtracks")] StationTracks { context_uri: String, #[serde(default)] seq: u64 },
    /// Empty the device queue.
    ///
    /// With `keep_uri` set, the current track is preserved: the whole context is
    /// replaced by that single track, resumed at `position_ms`. Without it, this is
    /// the hard disconnect+activate reset (what ending a station needs, where
    /// whatever plays next is loaded immediately afterwards anyway).
    #[serde(rename = "clearqueue")] ClearQueue {
        #[serde(default)] keep_uri: String,
        #[serde(default)] position_ms: u32,
        #[serde(default)] playing: bool,
    },
    /// Move audio to another output without restarting anything. Empty = follow the
    /// OS default.
    #[serde(rename = "output")] Output { #[serde(default)] device: String },
    #[serde(rename = "quit")] Quit,
}

/// A running engine. Sending `Quit` + waiting on `finished` is how the caller
/// (`engine::Engine`) tears it down — see `stop()`.
pub struct Handle {
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
    running: Arc<AtomicBool>,
    /// Set only as the engine thread's very last act (runtime dropped, Spirc task
    /// done). Deliberately NOT `running`: `command_loop` clears that the instant it
    /// sees `Quit`, so waiting on it returned before any teardown had happened.
    finished: Arc<AtomicBool>,
    /// Set by `stop()` before it sends `Quit`. `Quit` alone can't reach an engine
    /// still waiting on the streaming sign-in (nothing reads the command channel
    /// until the session is up), so a Restart or app close used to sit out the full
    /// sign-in timeout; the sign-in wait watches this instead.
    quit: Arc<AtomicBool>,
}

/// Wait this long for the engine thread to finish after `Quit` — longer than
/// `SPIRC_SHUTDOWN_WAIT` plus the ~100 ms the run loop takes to notice, so a clean
/// "device left" reaches Spotify, yet still a hard bound on app close.
const STOP_WAIT_MS: u64 = 3000;
/// How long `run` lets Spirc's own task finish after `shutdown()` (that task is
/// what actually tells Spotify the device left) before aborting it.
const SPIRC_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Flips `finished` when dropped. Declared first in the engine thread's closure so
/// it drops last — after the runtime — on every exit path, a panic included.
struct FinishedGuard(Arc<AtomicBool>);

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Ask the engine to quit, then wait briefly for it to actually stop (mirrors
    /// the old child-process teardown: a bounded wait, not a guarantee). Unlike a
    /// subprocess, a stuck thread can't be force-killed — but the whole app is
    /// exiting right behind this call regardless, so a bounded best-effort wait
    /// (long enough for a clean `spirc.shutdown()`, short enough to never hang
    /// app close) is the right tradeoff, not a `.join()` that could block forever.
    pub fn stop(&self) {
        self.quit.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Quit);
        for _ in 0..STOP_WAIT_MS / 50 {
            if self.finished.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Spawn the engine on its own OS thread with its own tokio runtime. `_token`
/// (the Spotify Web-API access token) is accepted only to keep `engine::start`'s
/// signature stable for its callers — it was always vestigial for the engine
/// itself (streaming auth uses the separate keymaster OAuth above, never the
/// Web-API token; the old subprocess protocol only carried it across as a
/// handshake), and there's no longer a process boundary to hand it across.
pub fn spawn<F>(cache_dir: PathBuf, device: Option<String>, _token: &str, on_event: F) -> Handle
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let running = Arc::new(AtomicBool::new(true));
    let running_for_thread = Arc::clone(&running);
    let finished = Arc::new(AtomicBool::new(false));
    let finished_for_thread = Arc::clone(&finished);
    let quit = Arc::new(AtomicBool::new(false));
    let quit_for_thread = Arc::clone(&quit);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    // Unsized to `Arc<dyn Fn(...)>` immediately: `run`/`command_loop` take the
    // trait-object form so they aren't generic over `F` themselves.
    let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);

    let spawned = std::thread::Builder::new()
        .name("lightify-audio-engine".into())
        .spawn(move || {
            let _finished = FinishedGuard(finished_for_thread);
            let log = Arc::new(Log::open());
            // Two workers, not one per CPU thread (the default — 20 here, most of the
            // process's threads). The engine's async side is session I/O + Spirc;
            // decoding runs on librespot's own player thread and track loads on
            // threads it spawns, which only borrow this runtime's handle. Still
            // multi-thread so a busy connection never stalls Spirc. Blocking-pool
            // threads are created on demand and retire after 10 s idle.
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(8)
                .thread_name("lightify-engine-rt")
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log.line(&format!("failed to start engine runtime: {e}"));
                    on_event(Event::Failed { msg: format!("Failed to start playback engine: {e}") });
                    running_for_thread.store(false, Ordering::Relaxed);
                    on_event(Event::Exited);
                    return;
                }
            };
            let state = Arc::new(SharedState::new(Arc::clone(&running_for_thread)));
            let result = rt.block_on(run(
                cache_dir,
                device,
                Arc::clone(&on_event),
                state,
                cmd_rx,
                Arc::clone(&log),
                Arc::clone(&quit_for_thread),
            ));
            if let Err(msg) = result {
                log.line(&format!("fatal: {msg}"));
                // A sign-in abandoned because we were asked to stop isn't a failure.
                if !quit_for_thread.load(Ordering::SeqCst) {
                    on_event(Event::Failed { msg });
                }
            }
            running_for_thread.store(false, Ordering::Relaxed);
            on_event(Event::Exited);
        });
    if spawned.is_err() {
        running.store(false, Ordering::Relaxed);
        finished.store(true, Ordering::SeqCst);
    }

    Handle { cmd_tx, running, finished, quit }
}

/// Diagnostics, written to the same file the old subprocess's stderr used to be
/// piped into (`%APPDATA%\Lightify\lightify-shell-audio.log`) — there's no
/// subprocess stderr to capture anymore, and this process runs under the
/// `windows` subsystem in release builds (no console at all), so without this
/// every one of these messages would simply vanish instead of landing somewhere
/// checkable after the fact.
struct Log(Mutex<Option<std::fs::File>>);

impl Log {
    fn open() -> Self {
        let path = lightify_core::config::data_dir().join("lightify-shell-audio.log");
        Self(Mutex::new(std::fs::File::create(path).ok()))
    }

    fn line(&self, msg: &str) {
        if let Ok(mut f) = self.0.lock() {
            if let Some(f) = f.as_mut() {
                let _ = writeln!(f, "{msg}");
            }
        }
    }
}

struct SharedState {
    running: Arc<AtomicBool>,
    // ── Blip gate (verbatim from lightify-audio) ──
    // The host supplies the set of sanctioned track ids for the arbitrary local
    // ('lightify') queue via the `expect` command. Any `TrackChanged` whose id is
    // absent is a stale interloper (e.g. a leftover Spotify user-queue item that
    // clips in between tracks) and its audio is muted to silence until the host
    // redirects playback. `None` = no expectation set → fail open (never gate).
    gate_expected: Mutex<Option<HashSet<String>>>,
    gate_muted: AtomicBool,
    gate_muted_since: Mutex<Option<std::time::Instant>>,
}

impl SharedState {
    fn new(running: Arc<AtomicBool>) -> Self {
        Self {
            running,
            gate_expected: Mutex::new(None),
            gate_muted: AtomicBool::new(false),
            gate_muted_since: Mutex::new(None),
        }
    }
}

struct TappedSink {
    inner: Box<dyn Sink>,
    state: Arc<SharedState>,
    log: Arc<Log>,
    seen_packets: u64,
}

// SAFETY: the concrete CPAL audio backend returned by audio_backend::find() is
// Send; the Sink trait object erases this bound, and this impl is sound given
// that constraint (verbatim rationale from lightify-audio).
unsafe impl Send for TappedSink {}

impl Sink for TappedSink {
    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let muted = if self.state.gate_muted.load(Ordering::Relaxed) {
            let expired = self
                .state
                .gate_muted_since
                .lock()
                .ok()
                .and_then(|g| *g)
                .map(|since| since.elapsed() > std::time::Duration::from_millis(GATE_MAX_MUTE_MS))
                .unwrap_or(false);
            if expired {
                self.state.gate_muted.store(false, Ordering::Relaxed);
                self.log.line("gate: mute fail-safe expired, letting audio through");
                false
            } else {
                true
            }
        } else {
            false
        };

        if muted {
            if let AudioPacket::Samples(samples) = packet {
                return self.inner.write(AudioPacket::Samples(vec![0.0f64; samples.len()]), converter);
            }
            return self.inner.write(packet, converter);
        }

        if let AudioPacket::Samples(ref samples) = packet {
            // Only ever the FIRST packet, never a recurring "every Nth" one — this
            // runs on librespot's real-time audio-feed path, and `self.log` is a raw,
            // unbuffered `std::fs::File` behind a `Mutex`: every `line()` call is a
            // blocking syscall taken right here. A one-off log at startup (confirming
            // the pipeline is alive at all — a silent backend with zero packets means
            // librespot never received play / never decoded the track) is a
            // negligible one-time cost; a write every ~1000 packets (roughly a
            // minute or two of audio at typical decode packet sizes) is a periodic
            // stall sitting directly in the path that has to keep up with playback in
            // real time, and disk I/O latency is exactly the kind of "usually fine,
            // occasionally not" cause that reads as a random pop/click every minute
            // or two rather than a deterministic one. Removed rather than made
            // non-blocking (`try_lock` alone wouldn't help - the write itself still
            // syscalls even when the lock is uncontended) since the ongoing
            // per-packet confirmation has no real diagnostic value this deep into
            // the engine's life that the one-time version doesn't already cover.
            if self.seen_packets == 0 {
                self.log.line(&format!("sink.write packet #0 samples={}", samples.len()));
            }
            self.seen_packets += 1;
        }
        self.inner.write(packet, converter)
    }

    fn start(&mut self) -> SinkResult<()> {
        self.log.line("sink.start()");
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.log.line("sink.stop()");
        self.inner.stop()
    }
}

/// Resolve librespot credentials for the streaming session (verbatim logic from
/// lightify-audio, `fail(...)` calls turned into `Result` propagation — see the
/// module doc for why a background thread here must never call
/// `std::process::exit`, which the original did).
async fn resolve_streaming_credentials(
    cache: &Cache,
    cache_dir: &Path,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
    log: &Log,
    quit: &AtomicBool,
) -> Result<Credentials, String> {
    let marker = cache_dir.join(STREAM_AUTH_MARKER);
    if marker.exists() {
        if let Some(c) = cache.credentials() {
            log.line("using cached streaming credentials");
            return Ok(c);
        }
    } else {
        let legacy = cache_dir.join("credentials.json");
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
            log.line("discarded legacy credentials (pre-streaming-oauth)");
        }
    }

    on_event(Event::NeedAuth {
        msg: "Complete the one-time Spotify sign-in in your browser to enable playback.".into(),
    });
    log.line(&format!("starting keymaster OAuth flow (redirect {STREAM_REDIRECT_URI})"));

    let client = OAuthClientBuilder::new(STREAM_CLIENT_ID, STREAM_REDIRECT_URI, vec!["streaming"])
        .open_in_browser()
        .build()
        .map_err(|e| format!("Failed to build streaming OAuth client: {e}"))?;

    // NOT `get_access_token_async` under the timeout: in librespot-oauth 0.8 that
    // future runs the callback listener's plain blocking `accept()` inline, so it
    // never yields and `tokio::time::timeout` never gets polled — an abandoned
    // sign-in hung this thread forever with port 8898 still bound, and every later
    // restart then failed to bind it. The sync variant on the blocking pool lets
    // the timer actually fire; the wake-up below then gets that thread unstuck.
    let mut fetch = tokio::task::spawn_blocking(move || client.get_access_token());
    // Waited in short slices so a `stop()` (Settings → Restart, app close) is seen
    // within a fraction of a second rather than after the whole sign-in timeout.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(STREAM_AUTH_TIMEOUT_SECS);
    let why = loop {
        match tokio::time::timeout(std::time::Duration::from_millis(250), &mut fetch).await {
            Ok(Ok(Ok(token))) => return Ok(Credentials::with_access_token(token.access_token)),
            Ok(Ok(Err(e))) => return Err(format!("Streaming sign-in failed: {e}")),
            Ok(Err(e)) => return Err(format!("Streaming sign-in failed: {e}")),
            Err(_) if quit.load(Ordering::SeqCst) => break "Streaming sign-in cancelled.",
            Err(_) if tokio::time::Instant::now() >= deadline => {
                break "Streaming sign-in timed out — restart playback to try again."
            }
            Err(_) => {}
        }
    };
    release_oauth_listener(log);
    // Brief, bounded: long enough for the listener to take the poke and close
    // (freeing the port before a restart tries to bind it). It also matters for
    // teardown — dropping the runtime waits on blocking tasks.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fetch).await;
    Err(why.to_string())
}

/// Unblock librespot-oauth's callback listener after a sign-in timeout. It has no
/// cancel: it sits in `accept()` until *something* connects, so connect to it with
/// a throwaway request. It accepts, finds no auth code, answers and returns an
/// error — dropping the listener and releasing `STREAM_REDIRECT_URI`'s port.
/// Harmless if nothing is listening any more (the connect just fails).
fn release_oauth_listener(log: &Log) {
    use std::net::{SocketAddr, TcpStream};
    // Must match the host:port of `STREAM_REDIRECT_URI`.
    let addr: SocketAddr = ([127, 0, 0, 1], 8898).into();
    match TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(1)) {
        Ok(mut s) => {
            let _ = s.write_all(b"GET /login HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            log.line("sign-in abandoned; released the OAuth callback listener");
        }
        Err(e) => log.line(&format!("sign-in abandoned; OAuth listener not reachable ({e})")),
    }
}

async fn run(
    cache_dir: PathBuf,
    device: Option<String>,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
    state: Arc<SharedState>,
    cmd_rx: std::sync::mpsc::Receiver<Cmd>,
    log: Arc<Log>,
    quit: Arc<AtomicBool>,
) -> Result<(), String> {
    // Autoplay is a RUNTIME toggle (`Some(false)` would pin it off forever), read
    // live via the "autoplay" user attribute the `autoplay` command flips. Forced
    // to "0" once the session is ready below so it starts OFF — Lightify owns
    // what plays next during normal playback; the blip gate mutes any stray
    // radio track there. Only turned on while a station is active.
    let session_config = SessionConfig { autoplay: None, ..SessionConfig::default() };
    log.line(&format!("session client_id={}", session_config.client_id));
    let cache = Cache::new(Some(&cache_dir), Some(&cache_dir), None, None)
        .map_err(|e| format!("Failed to create cache directory: {e}"))?;

    let credentials = resolve_streaming_credentials(&cache, &cache_dir, &on_event, &log, &quit).await?;
    let session = Session::new(session_config, Some(cache));

    // Periodic position reporting — without this librespot never emits
    // PositionChanged, so the UI's progress bar can only update from the 3s
    // Spotify poll or a one-shot Track event.
    let player_config = PlayerConfig {
        position_update_interval: Some(std::time::Duration::from_millis(500)),
        ..PlayerConfig::default()
    };
    // (No output format to pick here any more: `audio_output` asks each device for its
    // own native format and converts to it, instead of forcing S16 on every device.)
    log.line(&format!("requested output device={}", device.as_deref().unwrap_or("System Default")));

    // Output goes through `audio_output::SwitchableSink`, not librespot's rodio sink:
    // rodio binds to one device for the player's whole life (so any output change
    // meant restarting the engine - the 15 s) and can hang the player thread forever
    // when that device disappears. See that module's doc comment.
    let mixer_config = MixerConfig::default();
    let mixer_ctor = mixer::find(None).ok_or_else(|| "No mixer available".to_string())?;
    let mixer_instance = (mixer_ctor)(mixer_config).map_err(|e| format!("Failed to create mixer: {e}"))?;

    let device_for_sink = device.clone();
    let state_for_sink = Arc::clone(&state);
    let log_for_sink = Arc::clone(&log);

    let output_log: Arc<dyn Fn(String) + Send + Sync> = {
        let log = Arc::clone(&log);
        Arc::new(move |m: String| log.line(&m))
    };
    let output_changed: Arc<dyn Fn(String) + Send + Sync> = {
        let on_event = Arc::clone(&on_event);
        Arc::new(move |device: String| on_event(Event::OutputChanged { device }))
    };
    let player = Player::new(player_config, session.clone(), mixer_instance.get_soft_volume(), move || {
        Box::new(TappedSink {
            inner: Box::new(crate::audio_output::SwitchableSink::new(
                device_for_sink.clone(),
                output_log,
                output_changed,
            )),
            state: Arc::clone(&state_for_sink),
            log: Arc::clone(&log_for_sink),
            seen_packets: 0,
        })
    });
    let mut player_events = player.get_player_event_channel();

    let connect_config = ConnectConfig {
        name: DEVICE_NAME.into(),
        // librespot's Default sets initial_volume: Some(50), but Spirc treats
        // this as a u16 in 0..=65535 (50 ≈ 0.076%) — force ~70% so playback is
        // audible immediately on first Connect.
        initial_volume: (u16::MAX as u32 * 70 / 100) as u16,
        ..Default::default()
    };

    let (spirc, spirc_task) = match Spirc::new(connect_config, session.clone(), credentials, player.clone(), mixer_instance).await {
        Ok(result) => result,
        Err(e) => {
            // Cached credentials went stale — drop them plus the marker so the
            // next launch runs a fresh keymaster sign-in instead of failing here again.
            let _ = std::fs::remove_file(cache_dir.join("credentials.json"));
            let _ = std::fs::remove_file(cache_dir.join(STREAM_AUTH_MARKER));
            return Err(format!("Auth failed: {e}"));
        }
    };

    if let Err(e) = std::fs::write(cache_dir.join(STREAM_AUTH_MARKER), b"1") {
        log.line(&format!("warning: could not write streaming-auth marker: {e}"));
    }

    let mut spirc_handle = tokio::spawn(spirc_task);
    let spirc = Arc::new(spirc);

    let ready_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(8);
    loop {
        let user_data = session.user_data();
        let account_type = user_data.attributes.get("type").cloned().unwrap_or_default();
        let catalogue = user_data.attributes.get("catalogue").cloned().unwrap_or_default();
        if !user_data.country.is_empty() && (!account_type.is_empty() || !catalogue.is_empty()) {
            log.line(&format!("session ready country={} type={} catalogue={}", user_data.country, account_type, catalogue));
            break;
        }
        if tokio::time::Instant::now() >= ready_deadline {
            log.line(&format!("session readiness timeout country={} type={} catalogue={}", user_data.country, account_type, catalogue));
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    let device_id = session.device_id().to_string();
    on_event(Event::Ready { device_id });

    // Force autoplay OFF by default now that the session is up. The UI turns it
    // on only for stations; without this a Spotify-account autoplay=on setting
    // would leak radio into normal playback.
    session.set_user_attribute("autoplay", "0");

    let cmd_state = Arc::clone(&state);
    let cmd_spirc = Arc::clone(&spirc);
    let cmd_session = session.clone();
    let cmd_on_event = Arc::clone(&on_event);
    // Runs on a plain OS thread (blocking channel recv), outside the tokio
    // runtime proper — it carries a Handle for the one command that needs to
    // await (resolving a station context), same shape as the old stdin loop.
    let cmd_rt = tokio::runtime::Handle::current();
    let cmd_log = Arc::clone(&log);
    std::thread::spawn(move || command_loop(cmd_state, cmd_spirc, cmd_session, cmd_rt, cmd_rx, cmd_on_event, cmd_log));

    while state.running.load(Ordering::Relaxed) {
        tokio::select! {
            maybe_event = player_events.recv() => {
                use librespot_playback::player::PlayerEvent;
                let Some(event) = maybe_event else {
                    on_event(Event::Playing { playing: false });
                    break;
                };
                match event {
                    PlayerEvent::Playing { position_ms, .. } => {
                        on_event(Event::Playing { playing: true });
                        on_event(Event::Position { ms: position_ms as u64 });
                    }
                    PlayerEvent::Paused { position_ms, .. } => {
                        on_event(Event::Playing { playing: false });
                        on_event(Event::Position { ms: position_ms as u64 });
                    }
                    PlayerEvent::Stopped { .. } => {
                        on_event(Event::Playing { playing: false });
                        on_event(Event::Position { ms: 0 });
                    }
                    PlayerEvent::EndOfTrack { .. } => {
                        // Natural end of a track; the shell doesn't currently act on
                        // this distinctly from the Track/Playing events that follow
                        // it (or, on the final track, from Stopped) — same as before.
                    }
                    PlayerEvent::Unavailable { track_id, .. } => {
                        on_event(Event::Failed { msg: format!("track unavailable: {track_id}") });
                    }
                    PlayerEvent::Seeked { position_ms, .. }
                    | PlayerEvent::PositionCorrection { position_ms, .. }
                    | PlayerEvent::PositionChanged { position_ms, .. } => {
                        on_event(Event::Position { ms: position_ms as u64 });
                    }
                    // Shuffle/repeat as the device now has them — whether we asked, a
                    // remote asked (the Web API, a phone), or librespot changed them
                    // itself. The UI used to learn these only from the next poll.
                    PlayerEvent::ShuffleChanged { shuffle } => {
                        on_event(Event::Options { shuffle: Some(shuffle), repeat: None });
                    }
                    PlayerEvent::RepeatChanged { context, track } => {
                        let mode = if track { "track" } else if context { "context" } else { "off" };
                        on_event(Event::Options { shuffle: None, repeat: Some(mode.to_string()) });
                    }
                    PlayerEvent::VolumeChanged { volume } => {
                        let percent = ((volume as u32 * 100) / u16::MAX as u32) as u16;
                        on_event(Event::Volume { percent });
                    }
                    PlayerEvent::TrackChanged { audio_item } => {
                        let tid = audio_item.track_id.to_string(); // base62, matches Web-API ids
                        {
                            let allow = match &*state.gate_expected.lock().unwrap() {
                                None => true,
                                Some(set) => set.is_empty() || set.contains(&tid),
                            };
                            if allow {
                                state.gate_muted.store(false, Ordering::Relaxed);
                            } else {
                                state.gate_muted.store(true, Ordering::Relaxed);
                                *state.gate_muted_since.lock().unwrap() = Some(std::time::Instant::now());
                                log.line(&format!("gate: muting unsanctioned track {tid}"));
                            }
                        }
                        let artists = match &audio_item.unique_fields {
                            UniqueFields::Track { artists, .. } => {
                                artists.0.iter().map(|a| a.name.clone()).collect::<Vec<String>>().join(", ")
                            }
                            _ => String::new(),
                        };
                        on_event(Event::Track { name: audio_item.name.clone(), artists });
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
        }
    }

    let _ = spirc.shutdown();
    // `shutdown()` only queues the request; the Spirc task is what then sends
    // Spotify the "device left" update. Aborting it straight away (as this used
    // to) cancelled that every time, leaving a phantom "Lightify" device on the
    // account. Let it finish, bounded, and abort only if it overruns.
    if tokio::time::timeout(SPIRC_SHUTDOWN_WAIT, &mut spirc_handle).await.is_err() {
        log.line("spirc did not shut down in time; aborting it");
        spirc_handle.abort();
    }
    on_event(Event::Playing { playing: false });
    Ok(())
}

fn command_loop(
    state: Arc<SharedState>,
    spirc: Arc<Spirc>,
    session: Session,
    rt: tokio::runtime::Handle,
    cmd_rx: std::sync::mpsc::Receiver<Cmd>,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
    log: Arc<Log>,
) {
    for cmd in cmd_rx.iter() {
        match cmd {
            // Every spirc call below used to discard its Result with `let _ =` — if
            // the local Connect session wasn't actually the one Spotify considered
            // active (a stale device sharing our display name, a dropped connection,
            // ...), the command silently did nothing and left no trace anywhere. Now
            // logged like `station`/`clearqueue` already were, so a repeat of "next
            // did nothing" is at least visible in lightify-shell-audio.log afterward.
            Cmd::Play => { if let Err(e) = spirc.play() { log.line(&format!("play failed: {e}")); } }
            Cmd::Pause => { if let Err(e) = spirc.pause() { log.line(&format!("pause failed: {e}")); } }
            Cmd::Next => { if let Err(e) = spirc.next() { log.line(&format!("next failed: {e}")); } }
            Cmd::Prev => { if let Err(e) = spirc.prev() { log.line(&format!("prev failed: {e}")); } }
            Cmd::Seek { position_ms } => {
                if let Err(e) = spirc.set_position_ms(position_ms) {
                    log.line(&format!("seek failed: {e}"));
                }
            }
            Cmd::Volume { percent } => {
                let pct = percent.min(100) as u32;
                let v = ((pct * u16::MAX as u32) / 100) as u16;
                if let Err(e) = spirc.set_volume(v) {
                    log.line(&format!("volume failed: {e}"));
                }
            }
            Cmd::Shuffle { enabled } => {
                if let Err(e) = spirc.shuffle(enabled) {
                    log.line(&format!("shuffle failed: {e}"));
                }
            }
            Cmd::Repeat { mode } => {
                // Spotify's own model: repeat-one sits ON TOP of repeat-all (both flags
                // set). This used to send repeat-one with repeat-all *off* — and since
                // librespot clears repeat-one whenever you skip, one skip switched repeat
                // off altogether, where Spotify drops back to repeat-all.
                let r = match mode.as_str() {
                    "track" => spirc.repeat(true).and_then(|()| spirc.repeat_track(true)),
                    "context" => spirc.repeat_track(false).and_then(|()| spirc.repeat(true)),
                    _ => spirc.repeat_track(false).and_then(|()| spirc.repeat(false)),
                };
                if let Err(e) = r {
                    log.line(&format!("repeat({mode}) failed: {e}"));
                }
            }
            Cmd::Stop => { if let Err(e) = spirc.pause() { log.line(&format!("stop failed: {e}")); } }
            Cmd::Expect { ids } => {
                let set: HashSet<String> = ids.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                let fail_open = set.is_empty();
                *state.gate_expected.lock().unwrap() = if fail_open { None } else { Some(set) };
                if fail_open {
                    state.gate_muted.store(false, Ordering::Relaxed);
                }
            }
            Cmd::Autoplay { enabled } => {
                session.set_user_attribute("autoplay", if enabled { "1" } else { "0" });
                log.line(&format!("autoplay set to {enabled}"));
            }
            Cmd::Station { context_uri } => {
                // See lightify-audio's original comment (verbatim rationale): a
                // real station context, not `LoadContextOptions::Autoplay` (which
                // only continues an already-loaded context); disconnect+activate
                // first so `clear_next_tracks`'s user-queue exemption can't leave
                // duplicates/a skip-back loop behind.
                let station_uri = match context_uri.strip_prefix("spotify:") {
                    Some(rest) if !rest.starts_with("station:") => format!("spotify:station:{rest}"),
                    _ => context_uri.clone(),
                };
                log.line(&format!("station: loading {station_uri}"));
                if let Err(e) = spirc.disconnect(true) {
                    log.line(&format!("station: disconnect failed: {e}"));
                }
                if let Err(e) = spirc.activate() {
                    log.line(&format!("station: activate failed: {e}"));
                }
                let request = LoadRequest::from_context_uri(station_uri, LoadRequestOptions { start_playing: true, ..Default::default() });
                if let Err(e) = spirc.load(request) {
                    on_event(Event::Failed { msg: format!("Station failed: {e}") });
                }
            }
            Cmd::ClearQueue { keep_uri, position_ms, playing } => {
                if keep_uri.is_empty() {
                    // Hard reset. disconnect(true) pauses and tears the session's
                    // playback state down, so this MUST NOT be the path a user-facing
                    // "Clear" takes - it stops the music. Kept for ending a station,
                    // which loads something else on the very next line.
                    if let Err(e) = spirc.disconnect(true) {
                        log.line(&format!("clearqueue: disconnect failed: {e}"));
                    }
                    if let Err(e) = spirc.activate() {
                        log.line(&format!("clearqueue: activate failed: {e}"));
                    }
                    log.line("clearqueue: device queue reset (hard)");
                } else {
                    // Empty the queue WITHOUT losing the song: reset, then reload only
                    // the current track, resumed where it was.
                    //
                    // The reset has to be the real one. This used to be just the reload,
                    // on the belief that a one-track context leaves nothing to follow —
                    // but `load` goes through librespot's `clear_next_tracks`, which
                    // deliberately spares everything in the *user queue* (the
                    // `Queue`-provider tracks that `me/player/queue` adds, i.e. exactly
                    // what a station or "Add to queue" puts there). So Clear removed
                    // nothing that had been queued, a station's tracks all survived it,
                    // and the next station queued up behind them (found 2026-09-28 with
                    // `--selftest-station-clear`: 6 of 6 old tracks left after Clear).
                    // `disconnect` -> `became_inactive` -> `reset` is what actually
                    // empties `next_tracks`, and also drops the autoplay context so a
                    // finished station's radio can't carry over. Same three-step
                    // sequence as `Station` above, so it is the well-trodden path.
                    if let Err(e) = spirc.disconnect(true) {
                        log.line(&format!("clearqueue: disconnect failed: {e}"));
                    }
                    if let Err(e) = spirc.activate() {
                        log.line(&format!("clearqueue: activate failed: {e}"));
                    }
                    let request = LoadRequest::from_tracks(
                        vec![keep_uri.clone()],
                        LoadRequestOptions {
                            start_playing: playing,
                            seek_to: position_ms,
                            ..Default::default()
                        },
                    );
                    match spirc.load(request) {
                        Ok(()) => log.line(&format!(
                            "clearqueue: reset, kept {keep_uri} at {position_ms}ms (playing={playing})"
                        )),
                        Err(e) => log.line(&format!("clearqueue: keep-current load failed: {e}")),
                    }
                }
            }
            Cmd::StationTracks { context_uri, seq } => {
                let station_uri = match context_uri.strip_prefix("spotify:") {
                    Some(rest) if !rest.starts_with("station:") => format!("spotify:station:{rest}"),
                    _ => context_uri.clone(),
                };
                let sess = session.clone();
                // Bounded: this blocks the command thread, so a request that never
                // answers would otherwise leave every later command (pause, next,
                // quit included) stuck behind it for good.
                let resolved = rt.block_on(async move {
                    tokio::time::timeout(STATION_RESOLVE_TIMEOUT, sess.spclient().get_context(&station_uri)).await
                });
                let resolved = match resolved {
                    Ok(r) => r.map_err(|e| e.to_string()),
                    Err(_) => Err(format!("timed out after {}s", STATION_RESOLVE_TIMEOUT.as_secs())),
                };
                match resolved {
                    Ok(ctx) => {
                        let uris: Vec<String> = ctx
                            .pages
                            .into_iter()
                            .flat_map(|p| p.tracks)
                            .filter_map(|t| t.uri)
                            .filter(|u| u.starts_with("spotify:track:"))
                            .collect();
                        log.line(&format!("station: resolved {} tracks", uris.len()));
                        on_event(Event::StationTracks { uris, seq });
                    }
                    Err(e) => on_event(Event::Failed { msg: format!("Station resolve failed: {e}") }),
                }
            }
            Cmd::Output { device } => {
                log.line(&format!(
                    "output: switching live to {}",
                    if device.is_empty() { "the OS default" } else { device.as_str() }
                ));
                crate::audio_output::set_target(if device.is_empty() { None } else { Some(device) });
            }
            Cmd::Quit => {
                state.running.store(false, Ordering::Relaxed);
                let _ = spirc.shutdown();
                break;
            }
        }
    }
    state.running.store(false, Ordering::Relaxed);
}
