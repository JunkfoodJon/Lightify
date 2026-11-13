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

use librespot_connect::{CommandFailure, ConnectConfig, LoadRequest, LoadRequestOptions, PlayingTrack, Spirc};
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
use lightify_i18n::tr;
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
    /// `from` is the mode the device has now, when the sender knows it: only the
    /// flags that differ are then sent (see `repeat_calls`).
    #[serde(rename = "repeat")] Repeat { #[serde(default)] mode: String, #[serde(default)] from: String },
    #[serde(rename = "stop")] Stop,
    #[serde(rename = "expect")] Expect { #[serde(default)] ids: Vec<String> },
    #[serde(rename = "autoplay")] Autoplay { enabled: bool },
    #[serde(rename = "station")] Station { context_uri: String },
    /// `raw`: resolve `context_uri` as given rather than as a seed's station (the
    /// `--probe-station <context uri>` diagnostic).
    #[serde(rename = "stationtracks")] StationTracks { context_uri: String, #[serde(default)] seq: u64, #[serde(default)] raw: bool },
    /// Fetch `context_uri` ahead of a likely `loadcontext` (the list the user just
    /// opened), so the click that follows doesn't wait for it.
    #[serde(rename = "preparecontext")] PrepareContext { context_uri: String },
    /// Play a plain list of track uris from `index` — the local twin of the Web API's
    /// `me/player/play` with `uris`, without its practical length limit (the shell
    /// used to send at most 50, so a long Liked Songs run stopped after 50) or its
    /// round trip. `shuffle`/`repeat` ride along as load options: a load otherwise
    /// resets both on the device.
    /// Play `context_uri` from `track_uri`, resolved by the engine itself. For
    /// contexts the Web API won't start — Liked Songs (`spotify:user:<id>:collection`),
    /// which librespot resolves whole (thousands of tracks in one answer).
    #[serde(rename = "loadcontext")] LoadContext {
        context_uri: String,
        #[serde(default)] track_uri: String,
        #[serde(default)] position_ms: u32,
        #[serde(default)] shuffle: bool,
        #[serde(default)] repeat: String,
        /// A jump to a later row of the list already playing: songs removed from it by
        /// hand stay gone (`Spirc::keep_removed_on_next_load`, BUGS-PLAN QD5).
        #[serde(default)] keep_removed: bool,
    },
    /// Grow the list a `loadtracks` started, at its end, while it plays (the vendored
    /// librespot's `Spirc::append_tracks`). Dropped unless the playing list still
    /// holds `anchor`, so a late append never lands in something loaded since.
    #[serde(rename = "appendtracks")] AppendTracks { anchor: String, uris: Vec<String> },
    /// Make `uris` everything after the current track, which keeps playing (the vendored
    /// `Spirc::replace_next_tracks`): what a station does to the list it starts on.
    /// Hand-queued tracks survive, as on any load.
    #[serde(rename = "replacenext")] ReplaceNext { uris: Vec<String> },
    /// Add to the user queue on the device itself (the vendored `Spirc::add_to_queue`):
    /// the local twin of `POST me/player/queue`, without its one request per track.
    #[serde(rename = "addtoqueue")] AddToQueue { uris: Vec<String> },
    /// Drop one upcoming track (the `nth` occurrence of `uri`) and nothing else (the
    /// vendored `Spirc::remove_from_queue`); there is no Web-API way to do this.
    #[serde(rename = "removefromqueue")] RemoveFromQueue { uri: String, #[serde(default)] nth: usize },
    /// Play `uri` from up next now; the tracks before it are skipped unloaded and the list
    /// after it stays whole (the vendored `Spirc::skip_to`, BUGS-PLAN QD8).
    #[serde(rename = "skipto")] SkipTo { uri: String },
    #[serde(rename = "loadtracks")] LoadTracks {
        uris: Vec<String>,
        #[serde(default)] index: u32,
        #[serde(default)] position_ms: u32,
        #[serde(default)] shuffle: bool,
        #[serde(default)] repeat: String,
    },
    /// Empty the device queue.
    ///
    /// With `keep_uri` set, the current track is preserved: the whole context is
    /// replaced by that single track, resumed at `position_ms`. Without it, this is
    /// the hard disconnect+activate reset (what ending a station needs, where
    /// whatever plays next is loaded immediately afterwards anyway).
    ///
    /// `repeat` is the user's repeat mode. The reset wipes it on the device (and its
    /// "activate" reports that as a change), so when it's given the engine keeps the
    /// reset's repeat reports to itself and, on the keep path, puts the mode back.
    #[serde(rename = "clearqueue")] ClearQueue {
        #[serde(default)] keep_uri: String,
        #[serde(default)] position_ms: u32,
        #[serde(default)] playing: bool,
        #[serde(default)] repeat: String,
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
            install_librespot_log(&log);
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
    /// Fresh per app launch, appended to across engine restarts within it — a
    /// restart used to truncate the file and with it the record of why the previous
    /// engine stopped.
    fn open() -> Self {
        static OPENED: AtomicBool = AtomicBool::new(false);
        let path = lightify_core::config::data_dir().join("lightify-shell-audio.log");
        let file = if OPENED.swap(true, Ordering::SeqCst) {
            std::fs::OpenOptions::new().create(true).append(true).open(path)
        } else {
            std::fs::File::create(path)
        };
        Self(Mutex::new(file.ok()))
    }

    /// Each line carries the milliseconds since the app started, so how long a step
    /// took (a click's load to its first audio, say) can be read straight off the log.
    fn line(&self, msg: &str) {
        if let Ok(mut f) = self.0.lock() {
            if let Some(f) = f.as_mut() {
                let _ = writeln!(f, "[{:>9}ms] {msg}", log_epoch().elapsed().as_millis());
            }
        }
    }
}

/// librespot's last word on why a song wouldn't load, so the skip notice can say why
/// instead of only that it happened.
static LOAD_PROBLEM: Mutex<Option<(std::time::Instant, String)>> = Mutex::new(None);

fn note_load_problem(msg: &str) {
    let lower = msg.to_ascii_lowercase();
    let specific = ["load key", "audio key", "encrypted file", "audio item", "not available", "unavailable"]
        .iter()
        .any(|p| lower.contains(p));
    let generic = lower.contains("skipping to next track");
    if !specific && !generic {
        return;
    }
    if let Ok(mut g) = LOAD_PROBLEM.lock() {
        // The generic "skipping" line follows the specific one: keep the specific.
        let fresh_specific = g.as_ref().is_some_and(|(at, _)| at.elapsed() < std::time::Duration::from_secs(5));
        if specific || !fresh_specific {
            *g = Some((std::time::Instant::now(), msg.to_string()));
        }
    }
}

/// Why the last song failed, in words a user can act on (`None` when librespot said
/// nothing in the last half minute).
fn recent_load_problem() -> Option<String> {
    let g = LOAD_PROBLEM.lock().ok()?;
    let (at, msg) = g.as_ref()?;
    (at.elapsed() < std::time::Duration::from_secs(30)).then(|| load_problem_words(msg))
}

fn load_problem_words(msg: &str) -> String {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("load key") || lower.contains("audio key") {
        tr!("Spotify refused the song\u{2019}s key: this account may have asked for too many songs too fast")
    } else if lower.contains("encrypted file") {
        tr!("couldn\u{2019}t download it from Spotify\u{2019}s audio servers: check the network, or a firewall or antivirus blocking them")
    } else if lower.contains("not available") || lower.contains("unavailable") {
        tr!("it isn\u{2019}t available to this account or in this country")
    } else {
        msg.chars().take(140).collect()
    }
}

/// Songs that failed in a row with nothing playing in between: past this, every song is
/// failing the same way, so stop rather than spend an audio key on each one left.
const UNPLAYABLE_IN_A_ROW: usize = 3;

/// The zero of the log's timestamps: the first time anything asked for it.
fn log_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// librespot's own warnings and errors ("unexpected shutdown", a playlist page that
/// failed to resolve, ...) into the same log. No logger was ever installed, so those —
/// exactly what explains a playlist stopping early — went nowhere.
struct LibrespotLog;

static LIBRESPOT_LOG_SINK: Mutex<Option<Arc<Log>>> = Mutex::new(None);

impl log::Log for LibrespotLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn && m.target().starts_with("librespot")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let msg = r.args().to_string();
        // Said on every single load, by design (we activate before each one, and a
        // fresh load has no context metadata yet) — noise that would bury the rest.
        if msg.contains("will be ignored while already active") || msg.contains("couldn't load context info") {
            return;
        }
        note_load_problem(&msg);
        let sink = LIBRESPOT_LOG_SINK.lock().ok().and_then(|g| g.clone());
        if let Some(sink) = sink {
            sink.line(&format!("[{} {}] {msg}", r.level(), r.target()));
        }
    }

    fn flush(&self) {}
}

/// One line from the app into the engine's log (`lightify-shell-audio.log`), once the
/// engine has opened it — e.g. what RECENT counted (play_history.rs).
pub fn note(msg: &str) {
    let sink = LIBRESPOT_LOG_SINK.lock().ok().and_then(|g| g.clone());
    if let Some(sink) = sink {
        sink.line(msg);
    }
}

/// Point librespot's log at this engine's `Log` (installed once per process).
fn install_librespot_log(log: &Arc<Log>) {
    if let Ok(mut g) = LIBRESPOT_LOG_SINK.lock() {
        *g = Some(Arc::clone(log));
    }
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        if log::set_logger(&LibrespotLog).is_ok() {
            log::set_max_level(log::LevelFilter::Warn);
        }
    });
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
    /// Until then, librespot's repeat reports are not passed on: a queue clear's
    /// reset + activate announce "repeat off", which is the reset talking, not a
    /// change anyone made (see `Cmd::ClearQueue`'s `repeat`).
    repeat_quiet_until: Mutex<Option<std::time::Instant>>,
    /// The same for shuffle, set only when shuffle was on: see `keep_shuffle_report`.
    shuffle_quiet_until: Mutex<Option<std::time::Instant>>,
    /// Until then, a `SessionDisconnected` is our own `disconnect` (a queue clear's or a
    /// station start's reset), not another device taking over: no `Inactive` for it.
    own_disconnect_until: Mutex<Option<std::time::Instant>>,
}

impl SharedState {
    fn new(running: Arc<AtomicBool>) -> Self {
        Self {
            running,
            gate_expected: Mutex::new(None),
            gate_muted: AtomicBool::new(false),
            gate_muted_since: Mutex::new(None),
            repeat_quiet_until: Mutex::new(None),
            shuffle_quiet_until: Mutex::new(None),
            own_disconnect_until: Mutex::new(None),
        }
    }

    /// About to `disconnect` on purpose (see `own_disconnect_until`).
    fn note_own_disconnect(&self) {
        *self.own_disconnect_until.lock().unwrap() = Some(std::time::Instant::now() + CLEAR_REPEAT_QUIET);
    }

    fn own_disconnect(&self) -> bool {
        self.own_disconnect_until
            .lock()
            .unwrap()
            .is_some_and(|t| std::time::Instant::now() < t)
    }

    fn repeat_quiet(&self) -> bool {
        self.repeat_quiet_until
            .lock()
            .unwrap()
            .is_some_and(|t| std::time::Instant::now() < t)
    }

    fn shuffle_quiet(&self) -> bool {
        self.shuffle_quiet_until
            .lock()
            .unwrap()
            .is_some_and(|t| std::time::Instant::now() < t)
    }
}

/// Should a shuffle report reach the app? While a queue clear is under way, "off" is the
/// reset talking, not a change anyone made: the clear keeps shuffle on itself. A user's
/// own "off" ends the quiet first (`Cmd::Shuffle`).
fn keep_shuffle_report(quiet: bool, shuffle: bool) -> bool {
    shuffle || !quiet
}

/// Should a `SessionDisconnected` reach the app as `Inactive`? Not while it is our own
/// reset's disconnect (`SharedState::own_disconnect`).
fn keep_disconnect_report(own: bool) -> bool {
    !own
}

/// The load that keeps the current song through a queue clear. A load resets shuffle, so
/// when it was on it rides along. With shuffle on and no track named, a load picks at
/// random, hence the index, though there is only the one.
fn keep_current_request(keep_uri: String, position_ms: u32, playing: bool, shuffle: bool) -> LoadRequest {
    LoadRequest::from_tracks(
        vec![keep_uri],
        LoadRequestOptions {
            start_playing: playing,
            seek_to: position_ms,
            context_options: shuffle.then(|| librespot_connect::LoadContextOptions::Options(load_options(true, "off"))),
            playing_track: shuffle.then_some(PlayingTrack::Index(0)),
        },
    )
}

/// Used when nothing has been saved yet. librespot's Default is half volume.
const DEFAULT_VOLUME_PERCENT: u16 = 70;

/// The level to start at: the last one set, which librespot saves in the cache.
fn initial_volume(saved: Option<u16>) -> u16 {
    saved.unwrap_or_else(|| percent_to_volume(DEFAULT_VOLUME_PERCENT))
}

/// Spotify's 0-65535 from the app's 0-100, to the nearest step.
fn percent_to_volume(percent: u16) -> u16 {
    ((u32::from(percent.min(100)) * u32::from(u16::MAX) + 50) / 100) as u16
}

/// To the nearest percent, so what the app set comes back as it was set (50 used to
/// come back as 49).
fn volume_to_percent(volume: u16) -> u16 {
    ((u32::from(volume) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)) as u16
}

/// What the user is told about a command that was accepted and then didn't work.
fn failure_notice(failure: &CommandFailure) -> String {
    match failure {
        CommandFailure::Load { reason } => tr!("Couldn\u{2019}t play that \u{2014} {}", reason),
        CommandFailure::RemoveFromQueue { .. } => {
            tr!("That song wasn\u{2019}t in the queue any more, so nothing was removed")
        }
    }
}

/// How long a queue clear keeps the device's repeat reports quiet. Covers the reset,
/// the reload, and the Web-API play the app sends after a hard reset (≤ ~2.6 s).
const CLEAR_REPEAT_QUIET: std::time::Duration = std::time::Duration::from_secs(4);

/// The spirc calls that take the device from repeat mode `from` to `to`, in order.
/// Modes are Spotify's: "off", "context" (repeat all), "track" (repeat one, which
/// rides on repeat all). An empty or unknown `from` means "not known": both flags
/// are then set outright.
///
/// Only flags that actually change are touched, and that matters beyond tidiness:
/// librespot's `handle_set_repeat_context` rebuilds the whole up-next list from the
/// context *every* time `repeat(..)` is called, even with the value it already has —
/// and with shuffle on it looks the current track up in the unshuffled list but
/// rebuilds the shuffled one from that index. Going repeat-all → repeat-one used to
/// call `repeat(true)` again for nothing and pay that rebuild.
#[derive(Debug, PartialEq, Clone, Copy)]
enum RepeatCall {
    Context(bool),
    Track(bool),
}

fn repeat_calls(from: &str, to: &str) -> Vec<RepeatCall> {
    let flags = |m: &str| match m {
        "track" => Some((true, true)),
        "context" => Some((true, false)),
        "off" => Some((false, false)),
        _ => None,
    };
    let (ctx, track) = flags(to).unwrap_or((false, false));
    let mut calls = Vec::new();
    match flags(from) {
        Some((had_ctx, had_track)) => {
            // Repeat-one off before repeat-all changes, and repeat-all on before
            // repeat-one, so the device never sits in a mode nobody asked for.
            if had_track && !track {
                calls.push(RepeatCall::Track(false));
            }
            if had_ctx != ctx {
                calls.push(RepeatCall::Context(ctx));
            }
            if !had_track && track {
                calls.push(RepeatCall::Track(true));
            }
        }
        None => {
            if track {
                calls.push(RepeatCall::Context(true));
                calls.push(RepeatCall::Track(true));
            } else {
                calls.push(RepeatCall::Track(false));
                calls.push(RepeatCall::Context(ctx));
            }
        }
    }
    calls
}

fn apply_repeat(spirc: &Spirc, from: &str, to: &str) -> Result<(), librespot_core::Error> {
    for call in repeat_calls(from, to) {
        match call {
            RepeatCall::Context(on) => spirc.repeat(on)?,
            RepeatCall::Track(on) => spirc.repeat_track(on)?,
        }
    }
    Ok(())
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
            // Once per start, not per packet: the first audio of each track.
            if self.seen_packets == 0 {
                self.log.line(&format!("sink.write packet #0 samples={}", samples.len()));
            }
            self.seen_packets += 1;
        }
        self.inner.write(packet, converter)
    }

    fn start(&mut self) -> SinkResult<()> {
        self.log.line("sink.start()");
        self.seen_packets = 0;
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
    let saved_volume = cache.volume();

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
        initial_volume: initial_volume(saved_volume),
        ..Default::default()
    };

    let (spirc, spirc_task) = match Spirc::new(connect_config, session.clone(), credentials, player.clone(), mixer_instance).await {
        Ok(result) => result,
        Err(e) => {
            // Cached credentials went stale — drop them plus the marker so the
            // next launch runs a fresh keymaster sign-in instead of failing here again.
            // Only on a real login refusal: a network failure here (an engine restart
            // racing a connection that is still coming back) used to throw away good
            // credentials too and demand a browser sign-in for nothing.
            if e.kind == librespot_core::error::ErrorKind::PermissionDenied {
                let _ = std::fs::remove_file(cache_dir.join("credentials.json"));
                let _ = std::fs::remove_file(cache_dir.join(STREAM_AUTH_MARKER));
                return Err(format!("Auth failed: {e}"));
            }
            return Err(format!("Couldn't reach Spotify: {e}"));
        }
    };

    if let Err(e) = std::fs::write(cache_dir.join(STREAM_AUTH_MARKER), b"1") {
        log.line(&format!("warning: could not write streaming-auth marker: {e}"));
    }

    // `spirc.load` and friends only put a command on a channel; this is how one that was
    // accepted and then failed gets out of the Spirc task.
    {
        let (on_event, log) = (Arc::clone(&on_event), Arc::clone(&log));
        spirc.on_command_failed(move |failure| {
            log.line(&format!("command failed: {failure:?}"));
            on_event(Event::Notice { msg: failure_notice(&failure) });
        });
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

    {
        let user_data = session.user_data();
        on_event(Event::Account {
            username: session.username(),
            account_type: user_data.attributes.get("type").cloned().unwrap_or_default(),
            catalogue: user_data.attributes.get("catalogue").cloned().unwrap_or_default(),
        });
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

    // Why the loop below ended, when it wasn't asked to. librespot has no reconnect:
    // a network blip, sleep/wake or a dealer reconnect that fails invalidates the
    // session and ends Spirc's task, while the player (held here) stays up. The song
    // then plays out and nothing ever loads the next one: playback quietly stopped
    // partway through a playlist with the engine still reported "running". Ending the
    // engine instead reports `Exited`, so the shell's watchdog restarts it.
    let mut lost: Option<&str> = None;
    let mut spirc_done = false;
    // Songs that failed to load since anything last played (see `UNPLAYABLE_IN_A_ROW`).
    let mut failed_in_a_row: std::collections::HashSet<String> = std::collections::HashSet::new();
    while state.running.load(Ordering::Relaxed) {
        tokio::select! {
            _ = &mut spirc_handle => {
                spirc_done = true;
                lost = Some("Spotify connection ended");
                break;
            }
            maybe_event = player_events.recv() => {
                use librespot_playback::player::PlayerEvent;
                let Some(event) = maybe_event else {
                    on_event(Event::Playing { playing: false });
                    break;
                };
                match event {
                    PlayerEvent::Loading { position_ms, .. } => {
                        log.line(&format!("player: loading at {position_ms} ms"));
                    }
                    PlayerEvent::Playing { position_ms, .. } => {
                        failed_in_a_row.clear();
                        log.line(&format!("player: playing at {position_ms} ms"));
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
                        // Spirc skips it and carries on, and it may be only the next
                        // song's preload: nothing is wrong with the engine.
                        log.line(&format!("track unavailable: {track_id}"));
                        let why = recent_load_problem();
                        failed_in_a_row.insert(track_id.to_string());
                        let msg = if failed_in_a_row.len() >= UNPLAYABLE_IN_A_ROW {
                            // Every song is failing the same way: going on would only
                            // spend an audio key on each one left in the queue.
                            failed_in_a_row.clear();
                            let _ = spirc.pause();
                            log.line("stopped: songs keep failing to load");
                            match why {
                                Some(why) => tr!("Stopped: {} songs in a row couldn\u{2019}t be played \u{2014} {}", UNPLAYABLE_IN_A_ROW, why),
                                None => tr!("Stopped: {} songs in a row couldn\u{2019}t be played", UNPLAYABLE_IN_A_ROW),
                            }
                        } else {
                            match why {
                                Some(why) => tr!("Skipped a song Spotify can\u{2019}t play \u{2014} {}", why),
                                None => tr!("Skipped a song Spotify can\u{2019}t play"),
                            }
                        };
                        on_event(Event::Notice { msg });
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
                        if keep_shuffle_report(state.shuffle_quiet(), shuffle) {
                            on_event(Event::Options { shuffle: Some(shuffle), repeat: None });
                        }
                    }
                    PlayerEvent::RepeatChanged { context, track } => {
                        // A queue clear's reset reporting "off" isn't a change anyone
                        // made; the clear puts the user's mode back itself.
                        if !state.repeat_quiet() {
                            let mode = if track { "track" } else if context { "context" } else { "off" };
                            on_event(Event::Options { shuffle: None, repeat: Some(mode.to_string()) });
                        }
                    }
                    PlayerEvent::VolumeChanged { volume } => {
                        on_event(Event::Volume { percent: volume_to_percent(volume) });
                    }
                    // Spirc emits this when the cluster names another device active
                    // (`handle_disconnect`) — a handoff the shell would otherwise only
                    // learn about from its next Web-API poll.
                    // Our own reset (Clear Queue, a station start) disconnects and
                    // re-activates at once: nothing moved, so the shell must not drop
                    // what it shows as playing (the play button flickered).
                    PlayerEvent::SessionDisconnected { .. } => {
                        if keep_disconnect_report(state.own_disconnect()) {
                            on_event(Event::Inactive);
                        } else {
                            log.line("disconnect: our own reset, not a handoff");
                        }
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
                        let (artists, artist_ids, album) = match &audio_item.unique_fields {
                            UniqueFields::Track { artists, album, .. } => (
                                artists.0.iter().map(|a| a.name.clone()).collect::<Vec<String>>().join(", "),
                                artists.0.iter().filter_map(|a| a.id.to_id().ok()).collect::<Vec<String>>(),
                                album.clone(),
                            ),
                            _ => (String::new(), Vec::new(), String::new()),
                        };
                        // The 640 px LARGE cover is the one the Web API lists first (what
                        // `album_art` holds), so a later poll sees the same URL and does
                        // not fetch and redraw the cover a second time.
                        let cover = audio_item
                            .covers
                            .iter()
                            .find(|c| c.size == librespot_metadata::image::ImageSize::LARGE)
                            .or_else(|| audio_item.covers.first())
                            .map(|c| c.url.clone())
                            .unwrap_or_default();
                        on_event(Event::Track {
                            name: audio_item.name.clone(),
                            artists,
                            id: audio_item.track_id.to_id().unwrap_or_default(),
                            uri: audio_item.uri.clone(),
                            artist_ids,
                            album,
                            duration_ms: audio_item.duration_ms as u64,
                            cover,
                        });
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                // Said once per reason: a silent engine otherwise looks like a broken one.
                if let Some(why) = crate::audio_output::take_trouble() {
                    on_event(Event::Notice { msg: why });
                }
                if session.is_invalid() {
                    lost = Some("Spotify connection lost");
                    break;
                }
            }
        }
    }

    if !spirc_done {
        let _ = spirc.shutdown();
        // `shutdown()` only queues the request; the Spirc task is what then sends
        // Spotify the "device left" update. Aborting it straight away (as this used
        // to) cancelled that every time, leaving a phantom "Lightify" device on the
        // account. Let it finish, bounded, and abort only if it overruns.
        if tokio::time::timeout(SPIRC_SHUTDOWN_WAIT, &mut spirc_handle).await.is_err() {
            log.line("spirc did not shut down in time; aborting it");
            spirc_handle.abort();
        }
    }
    // The player outlives Spirc; with nothing driving it, stop it rather than let a
    // song play out on a device Spotify no longer knows about.
    if lost.is_some() {
        player.stop();
    } else {
        // Not on a lost connection: the shell reads "was it playing" off its last
        // state when `Exited` arrives, to resume after the restart.
        on_event(Event::Playing { playing: false });
    }
    match lost {
        // Only when nobody asked us to stop (Quit clears `running`).
        Some(why) if state.running.load(Ordering::Relaxed) => Err(why.to_string()),
        _ => Ok(()),
    }
}

/// The user's shuffle/repeat as load options — a load otherwise resets both.
fn load_options(shuffle: bool, repeat: &str) -> librespot_connect::Options {
    librespot_connect::Options {
        shuffle,
        // Spotify's model: repeat-one sits on top of repeat-all.
        repeat: repeat == "context" || repeat == "track",
        repeat_track: repeat == "track",
    }
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
                if let Err(e) = spirc.set_volume(percent_to_volume(percent)) {
                    log.line(&format!("volume failed: {e}"));
                }
            }
            Cmd::Shuffle { enabled } => {
                *state.shuffle_quiet_until.lock().unwrap() = None;
                if let Err(e) = spirc.shuffle(enabled) {
                    log.line(&format!("shuffle failed: {e}"));
                }
            }
            Cmd::Repeat { mode, from } => {
                // Spotify's own model: repeat-one sits ON TOP of repeat-all (both flags
                // set), so a skip out of repeat-one (librespot clears it) leaves
                // repeat-all, as in Spotify. Only the flags that change are sent.
                if let Err(e) = apply_repeat(&spirc, &from, &mode) {
                    log.line(&format!("repeat({from} -> {mode}) failed: {e}"));
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
                state.note_own_disconnect();
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
            Cmd::ClearQueue { keep_uri, position_ms, playing, repeat } => {
                if !repeat.is_empty() {
                    *state.repeat_quiet_until.lock().unwrap() = Some(std::time::Instant::now() + CLEAR_REPEAT_QUIET);
                }
                // The reset below wipes shuffle as well as repeat. Read before it goes out.
                let shuffle = spirc.shuffling();
                if shuffle {
                    *state.shuffle_quiet_until.lock().unwrap() = Some(std::time::Instant::now() + CLEAR_REPEAT_QUIET);
                }
                if keep_uri.is_empty() {
                    // Hard reset. disconnect(true) pauses and tears the session's
                    // playback state down, so this MUST NOT be the path a user-facing
                    // "Clear" takes - it stops the music. Kept for ending a station,
                    // which loads something else on the very next line.
                    state.note_own_disconnect();
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
                    state.note_own_disconnect();
                    if let Err(e) = spirc.disconnect(true) {
                        log.line(&format!("clearqueue: disconnect failed: {e}"));
                    }
                    if let Err(e) = spirc.activate() {
                        log.line(&format!("clearqueue: activate failed: {e}"));
                    }
                    match spirc.load(keep_current_request(keep_uri.clone(), position_ms, playing, shuffle)) {
                        Ok(()) => log.line(&format!(
                            "clearqueue: reset, kept {keep_uri} at {position_ms}ms (playing={playing})"
                        )),
                        Err(e) => log.line(&format!("clearqueue: keep-current load failed: {e}")),
                    }
                    // The reset and the load both leave repeat off (`reset_options`).
                    // (Shuffle rode along on the load: a change after it would reshuffle.)
                    // Clearing the queue isn't a reason to lose the user's repeat mode,
                    // so put it back. Spirc runs its commands in order, so this lands
                    // after the load.
                    if !repeat.is_empty() && repeat != "off" {
                        if let Err(e) = apply_repeat(&spirc, "off", &repeat) {
                            log.line(&format!("clearqueue: restoring repeat({repeat}) failed: {e}"));
                        }
                    }
                }
            }
            Cmd::LoadContext { context_uri, track_uri, position_ms, shuffle, repeat, keep_removed } => {
                if let Err(e) = spirc.activate() {
                    log.line(&format!("loadcontext: activate failed: {e}"));
                }
                if keep_removed {
                    if let Err(e) = spirc.keep_removed_on_next_load() {
                        log.line(&format!("loadcontext: keep removed failed: {e}"));
                    }
                }
                let request = LoadRequest::from_context_uri(
                    context_uri.clone(),
                    LoadRequestOptions {
                        start_playing: true,
                        seek_to: position_ms,
                        context_options: Some(librespot_connect::LoadContextOptions::Options(load_options(shuffle, &repeat))),
                        playing_track: (!track_uri.is_empty()).then(|| PlayingTrack::Uri(track_uri)),
                    },
                );
                match spirc.load(request) {
                    Ok(()) => log.line(&format!("loadcontext: {context_uri}")),
                    Err(e) => on_event(Event::Failed { msg: format!("Play failed: {e}") }),
                }
            }
            Cmd::AppendTracks { anchor, uris } => {
                let n = uris.len();
                match spirc.append_tracks(anchor, uris) {
                    Ok(()) => log.line(&format!("appendtracks: {n} tracks")),
                    Err(e) => log.line(&format!("appendtracks failed: {e}")),
                }
            }
            Cmd::ReplaceNext { uris } => {
                let n = uris.len();
                match spirc.replace_next_tracks(uris) {
                    Ok(()) => log.line(&format!("replacenext: {n} tracks")),
                    Err(e) => log.line(&format!("replacenext failed: {e}")),
                }
            }
            Cmd::SkipTo { uri } => match spirc.skip_to(uri.clone()) {
                Ok(()) => log.line(&format!("skipto: {uri}")),
                Err(e) => log.line(&format!("skipto failed: {e}")),
            },
            Cmd::RemoveFromQueue { uri, nth } => match spirc.remove_from_queue(uri, nth) {
                Ok(()) => log.line(&format!("removefromqueue: #{nth}")),
                Err(e) => log.line(&format!("removefromqueue failed: {e}")),
            },
            Cmd::AddToQueue { uris } => {
                let n = uris.len();
                match spirc.add_to_queue(uris) {
                    Ok(()) => log.line(&format!("addtoqueue: {n} tracks")),
                    Err(e) => log.line(&format!("addtoqueue failed: {e}")),
                }
            }
            Cmd::LoadTracks { uris, index, position_ms, shuffle, repeat } => {
                // Load is ignored by an inactive device; activate is a no-op on an
                // active one. Spirc runs both in order.
                if let Err(e) = spirc.activate() {
                    log.line(&format!("loadtracks: activate failed: {e}"));
                }
                let n = uris.len();
                let request = LoadRequest::from_tracks(
                    uris,
                    LoadRequestOptions {
                        start_playing: true,
                        seek_to: position_ms,
                        context_options: Some(librespot_connect::LoadContextOptions::Options(load_options(shuffle, &repeat))),
                        playing_track: Some(PlayingTrack::Index(index)),
                    },
                );
                match spirc.load(request) {
                    Ok(()) => log.line(&format!("loadtracks: {n} tracks from #{index}")),
                    Err(e) => on_event(Event::Failed { msg: format!("Play failed: {e}") }),
                }
            }
            Cmd::PrepareContext { context_uri } => {
                // In the background: the command thread must stay free for the click
                // this is preparing for. A load that arrives first just resolves the
                // context itself, as it would have anyway.
                let (sess, log) = (session.clone(), Arc::clone(&log));
                rt.spawn(async move {
                    let t = std::time::Instant::now();
                    match tokio::time::timeout(STATION_RESOLVE_TIMEOUT, librespot_connect::prepare_context(&sess, &context_uri)).await {
                        Ok(Ok(())) => log.line(&format!("prepared {context_uri} in {} ms", t.elapsed().as_millis())),
                        Ok(Err(e)) => log.line(&format!("prepare {context_uri} failed: {e}")),
                        Err(_) => log.line(&format!("prepare {context_uri} timed out")),
                    }
                });
            }
            Cmd::StationTracks { context_uri, seq, raw } => {
                let station_uri = match context_uri.strip_prefix("spotify:") {
                    Some(rest) if !raw && !rest.starts_with("station:") => format!("spotify:station:{rest}"),
                    _ => context_uri.clone(),
                };
                // In the background, like `PrepareContext`: resolving used to block this
                // thread for up to `STATION_RESOLVE_TIMEOUT`, so Pause/Next/seek/volume
                // waited behind it. Bounded all the same, so a request that never
                // answers can't hold a task forever.
                let (sess, log, on_event) = (session.clone(), Arc::clone(&log), Arc::clone(&on_event));
                rt.spawn(async move {
                    let resolved =
                        tokio::time::timeout(STATION_RESOLVE_TIMEOUT, sess.spclient().get_context(&station_uri)).await;
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
                });
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

#[cfg(test)]
mod tests {
    use super::{
        failure_notice, initial_volume, keep_current_request, keep_disconnect_report, keep_shuffle_report, load_problem_words, note_load_problem,
        percent_to_volume, recent_load_problem, repeat_calls, volume_to_percent,
        RepeatCall, DEFAULT_VOLUME_PERCENT,
    };
    use librespot_connect::{CommandFailure, LoadContextOptions, PlayingTrack};

    #[test]
    fn a_failed_load_says_why() {
        let failure = CommandFailure::Load { reason: "no such playlist".into() };
        assert!(failure_notice(&failure).contains("no such playlist"));
    }

    #[test]
    fn volume_percent_round_trips_at_every_step() {
        for percent in 0..=100 {
            assert_eq!(volume_to_percent(percent_to_volume(percent)), percent);
        }
        assert_eq!(percent_to_volume(0), 0);
        assert_eq!(percent_to_volume(100), u16::MAX);
        assert_eq!(percent_to_volume(250), u16::MAX, "out of range clamps");
        assert_eq!(volume_to_percent(u16::MAX), 100);
    }

    #[test]
    fn a_failed_load_is_explained_and_the_specific_reason_wins() {
        // The key refusal comes first, then librespot's generic "skipping" line.
        note_load_problem("Unable to load key, continuing without decryption: Service unavailable");
        note_load_problem("Skipping to next track, unable to load track <SpotifyId>: ()");
        let why = recent_load_problem().expect("a reason");
        assert!(why.contains("key"), "{why}");
        assert!(load_problem_words("Unable to load encrypted file: timed out").contains("audio servers"));
        assert!(load_problem_words("<x> is not available").contains("available"));
        // Lines that aren't about loading a song are not a reason.
        note_load_problem("Client specified an invalid argument");
        assert!(recent_load_problem().expect("still the key").contains("key"));
    }

    #[test]
    fn volume_starts_at_the_saved_level() {
        assert_eq!(initial_volume(Some(12_345)), 12_345);
        assert_eq!(initial_volume(Some(0)), 0, "muted is a level too");
        assert_eq!(volume_to_percent(initial_volume(None)), DEFAULT_VOLUME_PERCENT);
    }

    #[test]
    fn a_clear_holds_back_only_the_resets_shuffle_off() {
        assert!(!keep_shuffle_report(true, false), "the reset's off");
        assert!(keep_shuffle_report(true, true));
        assert!(keep_shuffle_report(false, false), "any other time, off is off");
        assert!(keep_shuffle_report(false, true));
    }

    #[test]
    fn our_own_reset_is_not_a_handoff() {
        use std::sync::{atomic::AtomicBool, Arc};
        let state = super::SharedState::new(Arc::new(AtomicBool::new(true)));
        assert!(keep_disconnect_report(state.own_disconnect()), "another device taking over is reported");
        state.note_own_disconnect();
        assert!(!keep_disconnect_report(state.own_disconnect()), "a clear's or station's reset is not");
    }

    #[test]
    fn keeping_the_song_through_a_clear_keeps_shuffle() {
        let on = keep_current_request("spotify:track:x".into(), 4200, true, true);
        assert!(on.start_playing);
        assert_eq!(on.seek_to, 4200);
        assert!(matches!(&on.context_options, Some(LoadContextOptions::Options(o)) if o.shuffle && !o.repeat && !o.repeat_track));
        assert!(matches!(on.playing_track, Some(PlayingTrack::Index(0))), "or a load picks at random");

        let off = keep_current_request("spotify:track:x".into(), 0, false, false);
        assert!(!off.start_playing);
        assert!(off.context_options.is_none());
        assert!(off.playing_track.is_none());
    }

    #[test]
    fn repeat_cycle_touches_only_the_flag_that_changes() {
        use RepeatCall::{Context, Track};
        // The button's cycle: off -> all -> one -> off.
        assert_eq!(repeat_calls("off", "context"), [Context(true)]);
        // All -> one must NOT call repeat(true) again: librespot rebuilds up-next on
        // every repeat-context call, even one that changes nothing.
        assert_eq!(repeat_calls("context", "track"), [Track(true)]);
        // One -> off: repeat-one off first, then repeat-all.
        assert_eq!(repeat_calls("track", "off"), [Track(false), Context(false)]);
        // Restoring after a load (device is off): repeat-all goes on under repeat-one.
        assert_eq!(repeat_calls("off", "track"), [Context(true), Track(true)]);
        assert_eq!(repeat_calls("track", "context"), [Track(false)]);
        assert!(repeat_calls("context", "context").is_empty());
        // Device state unknown: set both outright, as before.
        assert_eq!(repeat_calls("", "track"), [Context(true), Track(true)]);
        assert_eq!(repeat_calls("", "context"), [Track(false), Context(true)]);
        assert_eq!(repeat_calls("", "off"), [Track(false), Context(false)]);
    }
}
