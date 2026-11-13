//! Hot-swappable audio output for the embedded engine.
//!
//! This replaces librespot's rodio sink, which had two properties that together made
//! an output change cost 15+ seconds and could take the engine down:
//!
//! * **It is bound to one device for life.** `Player::new` takes the sink builder as
//!   an `FnOnce`, and rodio opens its stream on whichever device was current at that
//!   moment. When Windows reassigns the default output, the stream keeps playing to
//!   the old one. The only way to move it was to tear down the whole engine — a new
//!   librespot session, a new Connect device, Spotify listing it, a transfer — which
//!   is where the 15 seconds went.
//! * **It can hang forever.** Its `write` waits for its queue to drain
//!   (`while len > 26 { sleep(10ms) }`) and its `stop` calls `sleep_until_end`.
//!   Unplug the device and WASAPI stops pulling samples, the queue never drains, and
//!   librespot's player thread spins in that loop for good — the "crash".
//!
//! Here, librespot writes 44.1 kHz stereo into a shared ring, and a dedicated output
//! thread owns the actual `cpal` stream that plays from it. Switching device means
//! that thread drops one stream and opens another against the same ring: the session,
//! the Connect device and the player never notice, and nothing buffered is lost. The
//! thread polls the OS default every [`POLL`] and also reacts to the stream's own
//! error callback (device invalidated), so a change lands in well under a second.
//!
//! Two rules the rest of the module is built around:
//! * **Never block without a bound.** Every wait has a deadline. When no stream is
//!   pulling audio (or one stops pulling without saying so) the writer drops the oldest
//!   samples rather than wait for room, but still takes as long as the audio would have
//!   taken to play: librespot must not race through songs nobody hears, which would
//!   mark them played and spend an audio key on each.
//! * **`stop` must never fail.** librespot calls `exit(1)` if it does.
//!
//! **Local playback** (`local_player`) is a second source: a [`LocalSource`] ring of
//! the same 44.1 kHz stereo. While it is active, the current output (see
//! [`output_running`]) plays it *instead of* the librespot ring — never both — and
//! the librespot ring is left untouched until local playback stops. When the engine
//! isn't running, `local_player` starts an output of its own with
//! [`start_if_none`]; an engine sink created later always takes over from it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;

/// librespot always decodes to 44.1 kHz stereo; the output thread resamples to
/// whatever the device runs at (usually 48 kHz under WASAPI shared mode).
const SRC_RATE: f64 = 44_100.0;
const SRC_CHANNELS: usize = 2;

/// Buffered audio between the decoder and the device: ~300 ms. rodio's was ~500 ms;
/// shorter makes pause, seek and a device switch audibly snappier, and a desktop
/// decodes far faster than real time, so underruns are not a concern at this size.
const RING_CAP: usize = (SRC_RATE as usize * SRC_CHANNELS * 3) / 10;

/// How often the output thread re-checks which device it should be on. A change is
/// picked up within this, plus the ~20–80 ms WASAPI takes to open a stream.
const POLL: Duration = Duration::from_millis(100);

/// How long `write` keeps waiting for a stream that has gone away before it starts
/// discarding the oldest audio instead. Covers a normal device switch comfortably.
const DEAD_WRITE_GRACE: Duration = Duration::from_millis(400);

/// How long a stream that says it is alive may take nothing from a full ring before it
/// counts as stalled: it is reopened and `write` carries on as for a dead device. Normal
/// writes wait tens of milliseconds, so this only has to outlast a driver's hiccup.
const STALL_DEADLINE: Duration = Duration::from_secs(5);

/// The same for a device that has taken nothing since it opened. Laptop speakers waking
/// from power saving, Bluetooth and some USB devices take seconds to start pulling; a
/// short deadline reopened them before they ever did, again and again, and no sound
/// came out at all. A device that truly never starts still gets reopened, just later.
const FIRST_PULL_DEADLINE: Duration = Duration::from_secs(10);

/// How far unheard audio may run behind its clock and still catch up. A wake-up can land
/// ~15 ms late on Windows, longer than some packets, so the clock runs on from when the
/// last packet was due rather than from when it was let go. Past this it restarts: the
/// decoder was waiting for data, and a burst to make up for that would serve nobody.
const PACE_SLACK: Duration = Duration::from_millis(100);

/// Longest single wait inside `write`, so shutdown and a device coming back are noticed.
const WRITE_WAIT: Duration = Duration::from_millis(20);

/// How long dropping the sink waits for the output thread. A wedged driver can hold that
/// thread inside a stream call for good; the thread is then left behind, not waited for.
const OUTPUT_JOIN_WAIT: Duration = Duration::from_secs(1);

/// Upper bound on letting the buffered tail play out when librespot stops the sink.
const DRAIN_MAX: Duration = Duration::from_millis(600);

/// How long a stream that reports alive may take nothing before it counts as stalled.
fn stall_deadline(pulled_since_open: bool) -> Duration {
    if pulled_since_open { STALL_DEADLINE } else { FIRST_PULL_DEADLINE }
}

/// Why nothing can be heard, for the user: no output device opened. Set by the output
/// thread, taken (once per new reason) by the engine, cleared when a device goes live.
static TROUBLE: Mutex<Option<(String, bool)>> = Mutex::new(None);

fn note_trouble(why: String) {
    if let Ok(mut g) = TROUBLE.lock() {
        if g.as_ref().map(|(w, _)| w.as_str()) != Some(why.as_str()) {
            *g = Some((why, false));
        }
    }
}

/// A reason not yet reported that no audio can be heard, if there is one.
pub fn take_trouble() -> Option<String> {
    let mut g = TROUBLE.lock().ok()?;
    match g.as_mut() {
        Some((why, reported)) if !*reported => {
            *reported = true;
            Some(why.clone())
        }
        _ => None,
    }
}

/// How often to re-enumerate every output device, and only while a pinned device is
/// *not* the one playing (waiting for it to come back). Enumeration costs ~150 ms of
/// COM property reads on this hardware, so it must never run on every `POLL`: the
/// first version did exactly that while pinned, keeping this thread busy nonstop.
const PINNED_RESCAN: Duration = Duration::from_secs(1);

/// Consecutive failed opens of a pinned device that is still *listed* before it is
/// treated like a missing one: play on the OS default, and re-try the pin only on the
/// `PINNED_RESCAN` cadence. Without a limit, a device that enumerates but refuses to
/// open (held exclusively by another app, a broken driver) was retried every `POLL`
/// with a full scan each time — forever, and never falling back, so no audio at all.
const PINNED_OPEN_TRIES: u32 = 3;

/// Device enumerations performed, for `--probe-output` to confirm the steady state
/// does none. Diagnostic only.
static SCANS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn scan_count() -> u64 {
    SCANS.load(Ordering::Relaxed)
}

type Logger = Arc<dyn Fn(String) + Send + Sync>;
type OnChange = Arc<dyn Fn(String) + Send + Sync>;

struct Shared {
    /// Interleaved stereo f32 at 44.1 kHz.
    ring: Mutex<VecDeque<f32>>,
    /// Signalled whenever the device consumed audio (room freed / tail drained).
    drained: Condvar,
    /// A stream is currently open and pulling samples.
    alive: AtomicBool,
    /// When `write` first found no stream pulling, for `DEAD_WRITE_GRACE`. Shared
    /// (not per-`write`) so the grace is paid once per outage: as a local, every
    /// packet waited the full 400 ms again before dropping, throttling the player
    /// thread to a crawl for as long as no device was open. Cleared once alive.
    /// After the grace, `Pace` is what holds the player thread to real time.
    dead_since: Mutex<Option<Instant>>,
    /// librespot has started the sink (paused = output silence, keep the buffer).
    playing: AtomicBool,
    /// The current stream has taken audio from the ring at least once since it went
    /// live: only then can "taking nothing" mean stalled rather than still starting.
    pulled: AtomicBool,
    /// Ask the output thread to rebuild its stream on its next pass.
    reopen: AtomicBool,
    shutdown: AtomicBool,
    /// `Some(name)` = the user pinned an output; `None` = follow the OS default.
    target: Mutex<Option<String>>,
    /// Identity for [`OWNER`]: only the current output plays local audio.
    id: u64,
}

impl Shared {
    fn new(target: Option<String>) -> Self {
        Self {
            ring: Mutex::new(VecDeque::with_capacity(RING_CAP)),
            drained: Condvar::new(),
            alive: AtomicBool::new(false),
            dead_since: Mutex::new(None),
            playing: AtomicBool::new(false),
            pulled: AtomicBool::new(false),
            reopen: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            target: Mutex::new(target),
            id: NEXT_ID.fetch_add(1, Ordering::SeqCst),
        }
    }

    /// Put `data` in the ring, waiting for room as long as something is playing from it.
    /// With nothing to play it (`local` owns the output, the device is gone, or it has
    /// stalled) room is made by dropping the oldest audio, once `data` has taken its
    /// playing time on `pace`. Every path is bounded. Returns whether this call found the
    /// stream stalled and asked for it to be reopened.
    fn push(&self, data: &[f32], local: &LocalSource, pace: &mut Pace) -> bool {
        let mut stalled = false;
        let mut ring = lock(&self.ring);
        // The fill level last seen, and since when. Only this writer adds to the ring, so
        // a change means the device took audio: no callback has to report anything.
        let (mut seen_len, mut seen_at) = (ring.len(), Instant::now());
        let mut due: Option<Instant> = None;
        while ring.len() + data.len() > RING_CAP {
            if self.shutdown.load(Ordering::SeqCst) {
                return stalled;
            }
            let now = Instant::now();
            let unheard = if local.is_active() {
                // Local playback owns the device, so nothing drains this ring until it
                // stops (normally Spotify was paused first and we never get here).
                true
            } else if self.alive.load(Ordering::SeqCst) {
                *lock(&self.dead_since) = None;
                if ring.len() != seen_len {
                    (seen_len, seen_at) = (ring.len(), now);
                } else if now.duration_since(seen_at) >= stall_deadline(self.pulled.load(Ordering::Relaxed)) {
                    // Alive by every report, but taking nothing. The grace below was
                    // already served waiting here.
                    self.alive.store(false, Ordering::SeqCst);
                    self.reopen.store(true, Ordering::SeqCst);
                    *lock(&self.dead_since) = Some(seen_at);
                    stalled = true;
                }
                false
            } else {
                // No device is pulling audio (it vanished and none has opened yet).
                // Waiting for room here is what used to hang the engine for good.
                lock(&self.dead_since).get_or_insert(now).elapsed() >= DEAD_WRITE_GRACE
            };
            let mut wait = WRITE_WAIT;
            if unheard {
                let frames = data.len() / SRC_CHANNELS;
                let packet = Duration::from_secs_f64(frames as f64 / SRC_RATE);
                let left = due.get_or_insert_with(|| pace.next_due(now, packet)).saturating_duration_since(now);
                if left.is_zero() {
                    let excess = (ring.len() + data.len()).saturating_sub(RING_CAP).min(ring.len());
                    ring.drain(..excess);
                    break;
                }
                wait = wait.min(left);
            }
            ring = self.drained.wait_timeout(ring, wait).unwrap_or_else(|p| p.into_inner()).0;
        }
        ring.extend(data.iter().copied());
        stalled
    }
}

/// The clock for audio nobody hears, so it still takes as long as it would to play.
#[derive(Default)]
struct Pace {
    due: Option<Instant>,
}

impl Pace {
    /// When a packet of length `packet`, written at `now`, counts as played.
    fn next_due(&mut self, now: Instant, packet: Duration) -> Instant {
        let from = self.due.filter(|due| *due + PACE_SLACK >= now).unwrap_or(now);
        let due = from + packet;
        self.due = Some(due);
        due
    }
}

/// Ids handed to each `Shared`, and the id of the one registered in [`active`] (the
/// output that plays local audio). Any other output still alive at the same moment -
/// `local_player`'s own one, during the brief overlap after an engine sink took over -
/// outputs silence while local audio is active, so the two can never play it twice.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static OWNER: AtomicU64 = AtomicU64::new(0);

/// The last target any output was created with or switched to (`None` inside = the OS
/// default), so an output started for local playback lands where the user's audio was.
fn last_target_slot() -> &'static Mutex<Option<Option<String>>> {
    static LAST: OnceLock<Mutex<Option<Option<String>>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// `Some(target)` once any output has been created or `set_target` was called.
pub fn last_target() -> Option<Option<String>> {
    lock(last_target_slot()).clone()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock here would only mean a panic elsewhere; the data (audio samples,
    // a device name) is still perfectly usable, and refusing it would kill playback.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The sink currently wired into librespot, so `set_target` can reach it from the
/// engine's command loop. An engine restart replaces it; the old one unregisters by
/// simply dropping (this holds a `Weak`).
fn active() -> &'static Mutex<Weak<Shared>> {
    static ACTIVE: OnceLock<Mutex<Weak<Shared>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(Weak::new()))
}

/// Move playback to `device` (or back to following the OS default with `None`)
/// without touching the engine. Takes effect on the output thread's next pass.
pub fn set_target(device: Option<String>) {
    let device = device.filter(|d| !d.trim().is_empty());
    *lock(last_target_slot()) = Some(device.clone());
    if let Some(shared) = lock(active()).upgrade() {
        *lock(&shared.target) = device;
        shared.reopen.store(true, Ordering::SeqCst);
    }
}

/// Is any output (the engine's, or one started by [`start_if_none`]) alive?
pub fn output_running() -> bool {
    lock(active()).upgrade().is_some()
}

/// Start an output for local playback, but only when none is running: the engine's
/// sink, when there is one, is always the output. Checked and registered under one
/// lock, so this can never displace an engine sink that registered first; an engine
/// sink created afterwards displaces this one (see `OWNER`), and the caller drops it
/// once [`SwitchableSink::is_current`] turns false.
pub fn start_if_none(device: Option<String>, log: Logger, on_change: OnChange) -> Option<SwitchableSink> {
    let mut slot = lock(active());
    if slot.upgrade().is_some() {
        return None;
    }
    let (shared, sink) = SwitchableSink::spawn(device, log, on_change);
    *slot = Arc::downgrade(&shared);
    OWNER.store(shared.id, Ordering::SeqCst);
    Some(sink)
}

pub struct SwitchableSink {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    log: Logger,
    pace: Pace,
}

impl SwitchableSink {
    /// `device`: the initial target — a pinned output name, or `None` for the OS
    /// default. `on_change` is told the device name every time output lands on one.
    pub fn new(device: Option<String>, log: Logger, on_change: OnChange) -> Self {
        let (shared, sink) = Self::spawn(device, log, on_change);
        let mut slot = lock(active());
        *slot = Arc::downgrade(&shared);
        OWNER.store(shared.id, Ordering::SeqCst);
        drop(slot);
        sink
    }

    /// Is this the output that plays local audio (the one `set_target` reaches)?
    pub fn is_current(&self) -> bool {
        OWNER.load(Ordering::SeqCst) == self.shared.id
    }

    /// Build the shared state and start the output thread, without registering it.
    fn spawn(device: Option<String>, log: Logger, on_change: OnChange) -> (Arc<Shared>, Self) {
        let device = device.filter(|d| !d.trim().is_empty());
        *lock(last_target_slot()) = Some(device.clone());
        let shared = Arc::new(Shared::new(device));

        let thread = {
            let shared = Arc::clone(&shared);
            let log = Arc::clone(&log);
            std::thread::Builder::new()
                .name("lightify-audio-output".into())
                .spawn(move || output_thread(shared, log, on_change))
                .ok()
        };
        if thread.is_none() {
            log("audio output: could not start the output thread".into());
        }
        (Arc::clone(&shared), Self { shared, thread, log, pace: Pace::default() })
    }
}

impl Drop for SwitchableSink {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.drained.notify_all();
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + OUTPUT_JOIN_WAIT;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                (self.log)("audio output: the output thread is stuck in the driver; leaving it behind".into());
            }
        }
    }
}

impl Sink for SwitchableSink {
    fn start(&mut self) -> SinkResult<()> {
        self.shared.playing.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Let what is buffered play out (the rodio sink's `sleep_until_end`), but never
        // unbounded: with no live stream nothing drains, and that wait is exactly how the
        // old sink hung. Must return Ok regardless - librespot `exit(1)`s otherwise.
        let deadline = Instant::now() + DRAIN_MAX;
        let mut ring = lock(&self.shared.ring);
        while !ring.is_empty()
            && self.shared.alive.load(Ordering::SeqCst)
            // While local audio owns the output nothing drains this ring at all.
            && !local_source().is_active()
            && Instant::now() < deadline
            && !self.shared.shutdown.load(Ordering::SeqCst)
        {
            ring = self.shared.drained.wait_timeout(ring, Duration::from_millis(10)).unwrap_or_else(|p| p.into_inner()).0;
        }
        // Anything still here could not be played in time; holding it would only make
        // the next start begin with stale audio.
        ring.clear();
        drop(ring);
        self.shared.playing.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = match packet.samples() {
            Ok(s) => s,
            // Not a PCM packet (passthrough); nothing to play. An error here would make
            // librespot tear playback down over a packet we can simply skip.
            Err(e) => {
                (self.log)(format!("audio output: skipped a non-PCM packet: {e}"));
                return Ok(());
            }
        };
        let data = converter.f64_to_f32(samples);
        if self.shared.push(&data, local_source(), &mut self.pace) {
            (self.log)("audio output: the device stopped taking audio; reopening it".to_string());
        }
        Ok(())
    }
}

// ── The output thread ──────────────────────────────────────────────────────────

fn output_thread(shared: Arc<Shared>, log: Logger, on_change: OnChange) {
    let host = cpal::default_host();
    // (device name, the live stream). `cpal::Stream` is not `Send` on every host, so
    // it is created, played and dropped only on this thread.
    let mut current: Option<(String, cpal::Stream)> = None;
    let mut last_failure: Option<String> = None;
    // Names from the last full enumeration, and when it ran (see `PINNED_RESCAN`).
    let mut names: Vec<String> = Vec::new();
    let mut scanned_at: Option<Instant> = None;
    // Failed opens of the pinned device in a row (see `PINNED_OPEN_TRIES`), reset
    // whenever the pin itself changes so a fresh choice gets fresh tries.
    let mut pinned_failures: u32 = 0;
    let mut last_pinned: Option<String> = None;

    while !shared.shutdown.load(Ordering::SeqCst) {
        let forced = shared.reopen.swap(false, Ordering::SeqCst);
        let pinned = lock(&shared.target).clone();
        if pinned != last_pinned {
            pinned_failures = 0;
            last_pinned = pinned.clone();
        }
        let given_up = pinned_failures >= PINNED_OPEN_TRIES;
        // Set when a `PINNED_RESCAN` retry of a given-up pin is due this pass.
        let mut retry_pinned = false;
        // Devices from a scan made THIS pass, if one was: reused to open the chosen
        // device, so a switch costs one enumeration, not two.
        let mut fresh: Option<Vec<(String, cpal::Device)>> = None;
        let current_name = current.as_ref().map(|(n, _)| n.clone());
        // One cheap COM call - the only per-tick device query in the steady state.
        let default_name = host.default_output_device().and_then(|d| d.name().ok());

        let wanted_name = match &pinned {
            None => default_name.clone(),
            // Healthy on the pinned device: nothing to look up. If it goes away, the
            // stream's error callback sets `reopen` and we land in the arm below.
            Some(p) if !forced && current_name.as_deref() == Some(p.as_str())
                && shared.alive.load(Ordering::SeqCst) => Some(p.clone()),
            Some(p) => {
                let stale = scanned_at.map_or(true, |t| t.elapsed() >= PINNED_RESCAN);
                // Once given up, the retry `reopen` set by a failed open must not drive
                // a full scan every `POLL` - only the `PINNED_RESCAN` timer does.
                if (forced && !given_up) || stale {
                    fresh = Some(scan(&host));
                    names = fresh.iter().flatten().map(|(n, _)| n.clone()).collect();
                    scanned_at = Some(Instant::now());
                }
                let listed = names.iter().any(|n| n == p);
                retry_pinned = given_up && stale && listed;
                if listed && !given_up { Some(p.clone()) } else { default_name.clone() }
            }
        };
        let moved = current_name != wanted_name;

        if forced || moved {
            // Release the old device BEFORE opening the new one: two live streams would
            // both pull from the ring, each playing half the audio.
            if current.take().is_some() {
                shared.alive.store(false, Ordering::SeqCst);
            }
            let wanted = wanted_name
                .as_deref()
                .and_then(|n| resolve(&host, n, default_name.as_deref(), fresh.as_deref()));
            match wanted {
                Some(device) => match open_stream(&device, &shared) {
                    Ok((name, stream)) => {
                        if pinned.as_deref() == Some(name.as_str()) {
                            pinned_failures = 0;
                        }
                        went_live(&shared, &log, &on_change, &name);
                        last_failure = None;
                        current = Some((name, stream));
                    }
                    Err(e) => {
                        // Retry on the next pass rather than giving up: a device that is
                        // mid-switch in Windows often refuses the first open.
                        if last_failure.as_deref() != Some(e.as_str()) {
                            log(format!("audio output: {e}"));
                            note_trouble(lightify_i18n::tr!("No sound: the audio output wouldn\u{2019}t open \u{2014} {}", e));
                            last_failure = Some(e);
                        }
                        // ...but not forever on a pin: after `PINNED_OPEN_TRIES`, the next
                        // pass (still forced by the `reopen` below) lands on the default.
                        if pinned.is_some() && wanted_name == pinned {
                            pinned_failures += 1;
                            if pinned_failures == PINNED_OPEN_TRIES {
                                log(format!(
                                    "audio output: \"{}\" keeps refusing to open; using the OS default until it will",
                                    pinned.as_deref().unwrap_or_default()
                                ));
                            }
                        }
                        shared.reopen.store(true, Ordering::SeqCst);
                    }
                },
                None => {
                    if last_failure.as_deref() != Some("no output device") {
                        log("audio output: no output device available; waiting for one".into());
                        note_trouble(lightify_i18n::tr!("No sound: Windows reports no audio output device. Plug in or turn on speakers or headphones."));
                        last_failure = Some("no output device".into());
                    }
                }
            }
        }
        if retry_pinned {
            // The `PINNED_RESCAN` retry of a given-up pin. Built BEFORE letting go of the
            // fallback stream: a plain reopen would drop the working default first, so a
            // pin that still refuses would cut the music out once every rescan. A cpal
            // stream that is built but not yet played pulls nothing, so this still never
            // has two live streams on the ring.
            let p = pinned.as_deref().unwrap_or_default();
            let device = fresh.iter().flatten().find(|(n, _)| n == p).map(|(_, d)| d.clone());
            if let Some(Ok((name, stream))) = device.map(|d| build_stream(&d, &shared)) {
                if current.take().is_some() {
                    shared.alive.store(false, Ordering::SeqCst);
                }
                match stream.play() {
                    Ok(()) => {
                        pinned_failures = 0;
                        went_live(&shared, &log, &on_change, &name);
                        last_failure = None;
                        current = Some((name, stream));
                    }
                    // Still counts as given up: the forced pass lands back on the default.
                    Err(e) => {
                        log(format!("audio output: could not start \"{name}\": {e}"));
                        shared.reopen.store(true, Ordering::SeqCst);
                    }
                }
            }
            // A build error means it is still refusing: stay on the fallback, silently.
        }
        std::thread::sleep(POLL);
    }
    drop(current);
    shared.alive.store(false, Ordering::SeqCst);
}

/// A stream just opened on `name` and is pulling audio.
fn went_live(shared: &Shared, log: &Logger, on_change: &OnChange, name: &str) {
    *lock(&shared.dead_since) = None;
    shared.alive.store(true, Ordering::SeqCst);
    shared.drained.notify_all();
    shared.pulled.store(false, Ordering::SeqCst);
    log(format!("audio output: playing on \"{name}\""));
    if let Ok(mut g) = TROUBLE.lock() {
        *g = None;
    }
    on_change(name.to_string());
}

/// Every output device, with its name. The expensive call - see `PINNED_RESCAN`.
fn scan(host: &cpal::Host) -> Vec<(String, cpal::Device)> {
    SCANS.fetch_add(1, Ordering::Relaxed);
    host.output_devices()
        .map(|it| it.filter_map(|d| d.name().ok().map(|n| (n, d))).collect())
        .unwrap_or_default()
}

/// Turn a chosen name back into a device to open. Falling back to the OS default when
/// a pinned device is missing is what makes an unplugged headset behave like it does
/// in any other app, rather than stopping the music.
fn resolve(
    host: &cpal::Host,
    name: &str,
    default_name: Option<&str>,
    fresh: Option<&[(String, cpal::Device)]>,
) -> Option<cpal::Device> {
    if default_name == Some(name) {
        return host.default_output_device();
    }
    let found = match fresh {
        Some(devs) => devs.iter().find(|(n, _)| n == name).map(|(_, d)| d.clone()),
        None => scan(host).into_iter().find(|(n, _)| n == name).map(|(_, d)| d),
    };
    found.or_else(|| host.default_output_device())
}

fn open_stream(device: &cpal::Device, shared: &Arc<Shared>) -> Result<(String, cpal::Stream), String> {
    let (name, stream) = build_stream(device, shared)?;
    stream.play().map_err(|e| format!("could not start \"{name}\": {e}"))?;
    Ok((name, stream))
}

/// `open_stream` minus the `play()`: the device is opened (this is where one that
/// refuses fails) but pulls no audio until played.
fn build_stream(device: &cpal::Device, shared: &Arc<Shared>) -> Result<(String, cpal::Stream), String> {
    let name = device.name().unwrap_or_else(|_| "unknown device".into());
    let supported = device
        .default_output_config()
        .map_err(|e| format!("\"{name}\" has no usable output config: {e}"))?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(device, &config, shared),
        cpal::SampleFormat::I16 => build::<i16>(device, &config, shared),
        cpal::SampleFormat::I32 => build::<i32>(device, &config, shared),
        cpal::SampleFormat::U16 => build::<u16>(device, &config, shared),
        cpal::SampleFormat::F64 => build::<f64>(device, &config, shared),
        other => return Err(format!("\"{name}\" uses an unsupported sample format ({other:?})")),
    }
    .map_err(|e| format!("could not open \"{name}\": {e}"))?;
    Ok((name, stream))
}

/// Linear-interpolating resampler state, carried across device callbacks.
struct Resampler {
    /// Source frames advanced per output frame (44 100 / device rate).
    step: f64,
    /// Position between `cur` and `nxt`, in source frames.
    frac: f64,
    cur: [f32; 2],
    nxt: [f32; 2],
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, shared: &Arc<Shared>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = (config.channels as usize).max(1);
    let mut rs = Resampler {
        step: SRC_RATE / f64::from(config.sample_rate.0.max(1)),
        frac: 1.0,
        cur: [0.0; 2],
        nxt: [0.0; 2],
    };
    // Local playback gets its own interpolator state, so switching source never carries
    // one source's samples into the other.
    let mut lrs = Resampler { step: rs.step, frac: 1.0, cur: [0.0; 2], nxt: [0.0; 2] };
    let mut gain = 0.0f32;
    let data_shared = Arc::clone(shared);
    let err_shared = Arc::clone(shared);
    let local = Arc::clone(local_source());

    device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
            if local.is_active() {
                // Local audio owns the output: play it (only on the current output) and
                // leave the librespot ring untouched - the two are never mixed.
                let play = data_shared.id == OWNER.load(Ordering::Relaxed) && !local.is_paused();
                let target = local.volume();
                let mut buf = lock(&local.buf);
                let before = buf.ring.len();
                for frame in out.chunks_mut(channels) {
                    let (l, r) = if play { next_frame(&mut lrs, &mut buf.ring) } else { (0.0, 0.0) };
                    // ~10 ms smoothing, so a volume change doesn't click.
                    gain += (target - gain) * 0.002;
                    let (l, r) = (l * gain, r * gain);
                    write_frame(frame, l, r);
                }
                let consumed = (before - buf.ring.len()) / SRC_CHANNELS;
                buf.played += consumed as u64;
                return;
            }
            let playing = data_shared.playing.load(Ordering::Relaxed);
            let mut ring = lock(&data_shared.ring);
            if playing && !ring.is_empty() {
                data_shared.pulled.store(true, Ordering::Relaxed);
            }
            for frame in out.chunks_mut(channels) {
                let (l, r) = if playing { next_frame(&mut rs, &mut ring) } else { (0.0, 0.0) };
                if channels == 1 {
                    frame[0] = T::from_sample((l + r) * 0.5);
                } else {
                    frame[0] = T::from_sample(l);
                    frame[1] = T::from_sample(r);
                    for s in frame.iter_mut().skip(2) {
                        *s = T::from_sample(0.0f32);
                    }
                }
            }
            drop(ring);
            data_shared.drained.notify_all();
        },
        move |_err| {
            // Typically AUDCLNT_E_DEVICE_INVALIDATED: the device went away under us.
            // Flag it; the output thread rebuilds on the next pass (within `POLL`).
            err_shared.alive.store(false, Ordering::SeqCst);
            err_shared.reopen.store(true, Ordering::SeqCst);
            err_shared.drained.notify_all();
        },
        None,
    )
}

fn write_frame<T: SizedSample + FromSample<f32>>(frame: &mut [T], l: f32, r: f32) {
    if frame.len() == 1 {
        frame[0] = T::from_sample((l + r) * 0.5);
    } else {
        frame[0] = T::from_sample(l);
        frame[1] = T::from_sample(r);
        for s in frame.iter_mut().skip(2) {
            *s = T::from_sample(0.0f32);
        }
    }
}

// ── The local playback source ──────────────────────────────────────────────────

/// Sample rate of [`LocalSource`]: the same as the librespot ring, so the output
/// thread's per-device conversion serves both.
pub const LOCAL_RATE: u32 = SRC_RATE as u32;

/// Capacity of the local ring, in frames (~500 ms). Seek and `stop` drop it, pause
/// keeps it; its length is also how far ahead a gapless next file is decoded.
pub const LOCAL_CAP_FRAMES: usize = LOCAL_RATE as usize / 2;

/// The local player's ring and its play-position counters. Every write is tagged with
/// the generation of the `play` it belongs to, so a superseded decoder thread can
/// never sneak stale audio in after the ring was handed to the next one.
pub struct LocalSource {
    buf: Mutex<LocalBuf>,
    active: AtomicBool,
    paused: AtomicBool,
    /// f32 bits, 0.0..=1.0.
    volume: AtomicU32,
}

struct LocalBuf {
    /// Interleaved stereo f32 at `LOCAL_RATE`.
    ring: VecDeque<f32>,
    /// Generation owning the ring; 0 = nobody.
    gen: u64,
    /// Frames ever pushed / consumed by the device. `pushed - played` = buffered.
    pushed: u64,
    played: u64,
}

impl Default for LocalSource {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalSource {
    pub fn new() -> Self {
        Self {
            buf: Mutex::new(LocalBuf {
                ring: VecDeque::with_capacity(LOCAL_CAP_FRAMES * SRC_CHANNELS),
                gen: 0,
                pushed: 0,
                played: 0,
            }),
            active: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    /// Hand the ring to generation `gen` (dropping what's buffered) and make local
    /// audio the output's source.
    pub fn activate(&self, gen: u64) {
        let mut b = lock(&self.buf);
        b.ring.clear();
        b.pushed = b.played;
        b.gen = gen;
        self.paused.store(false, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
    }

    /// Give the output back to librespot, if `gen` (or any, with `None`) still owns
    /// the ring. Returns whether it did.
    pub fn deactivate(&self, gen: Option<u64>) -> bool {
        let mut b = lock(&self.buf);
        if b.gen == 0 || gen.is_some_and(|g| g != b.gen) {
            return false;
        }
        b.ring.clear();
        b.pushed = b.played;
        b.gen = 0;
        self.active.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        true
    }

    /// Drop everything buffered (seek). No-op for a stale `gen`.
    pub fn clear(&self, gen: u64) {
        let mut b = lock(&self.buf);
        if b.gen == gen {
            b.ring.clear();
            b.pushed = b.played;
        }
    }

    /// Append whole frames of interleaved stereo, as many as fit. Returns the frames
    /// taken, or `None` when `gen` no longer owns the ring. Never waits.
    pub fn push(&self, gen: u64, data: &[f32]) -> Option<usize> {
        let mut b = lock(&self.buf);
        if b.gen != gen || gen == 0 {
            return None;
        }
        let room = LOCAL_CAP_FRAMES.saturating_sub(b.ring.len() / SRC_CHANNELS);
        let n = room.min(data.len() / SRC_CHANNELS);
        b.ring.extend(data[..n * SRC_CHANNELS].iter().copied());
        b.pushed += n as u64;
        Some(n)
    }

    /// (frames pushed, frames played) since this source was created.
    pub fn counters(&self) -> (u64, u64) {
        let b = lock(&self.buf);
        (b.pushed, b.played)
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// What a device callback does, minus rate conversion and volume: take up to
    /// `max_frames` (nothing while paused or inactive). For tests without a device.
    #[cfg(test)]
    pub fn pull(&self, max_frames: usize) -> Vec<f32> {
        if !self.is_active() || self.is_paused() {
            return Vec::new();
        }
        let mut b = lock(&self.buf);
        let n = max_frames.min(b.ring.len() / SRC_CHANNELS);
        let out: Vec<f32> = b.ring.drain(..n * SRC_CHANNELS).collect();
        b.played += n as u64;
        out
    }
}

/// The one local source the real outputs play.
pub fn local_source() -> &'static Arc<LocalSource> {
    static LOCAL: OnceLock<Arc<LocalSource>> = OnceLock::new();
    LOCAL.get_or_init(|| Arc::new(LocalSource::new()))
}

/// One output frame, interpolated between source frames. On underrun it holds and
/// outputs silence rather than advancing, so no audio is skipped.
fn next_frame(rs: &mut Resampler, ring: &mut VecDeque<f32>) -> (f32, f32) {
    while rs.frac >= 1.0 {
        if ring.len() < SRC_CHANNELS {
            return (0.0, 0.0);
        }
        rs.cur = rs.nxt;
        rs.nxt = [ring.pop_front().unwrap_or(0.0), ring.pop_front().unwrap_or(0.0)];
        rs.frac -= 1.0;
    }
    let t = rs.frac as f32;
    let l = rs.cur[0] + (rs.nxt[0] - rs.cur[0]) * t;
    let r = rs.cur[1] + (rs.nxt[1] - rs.cur[1]) * t;
    rs.frac += rs.step;
    (l, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100 ms of audio, in samples.
    const PACKET: usize = (SRC_RATE as usize * SRC_CHANNELS) / 10;

    /// A ring that is full, with `alive` as given and no device: the tests are the device.
    fn full_ring(alive: bool) -> Arc<Shared> {
        let shared = Arc::new(Shared::new(None));
        shared.reopen.store(false, Ordering::SeqCst);
        shared.alive.store(alive, Ordering::SeqCst);
        lock(&shared.ring).extend(std::iter::repeat(0.25f32).take(RING_CAP));
        shared
    }

    /// Past the grace: the state after a device has been gone for a while.
    fn gone_for_a_while(shared: &Shared) {
        *lock(&shared.dead_since) = Instant::now().checked_sub(DEAD_WRITE_GRACE);
    }

    /// Writes `packets` of 100 ms each, returning how long that took.
    fn write_packets(shared: &Shared, local: &LocalSource, packets: usize) -> Duration {
        let (started, mut pace) = (Instant::now(), Pace::default());
        for n in 0..packets {
            let data = vec![n as f32; PACKET];
            shared.push(&data, local, &mut pace);
        }
        started.elapsed()
    }

    #[test]
    fn pace_runs_on_from_when_the_last_packet_was_due() {
        let (mut pace, t0, packet) = (Pace::default(), Instant::now(), Duration::from_millis(10));
        assert_eq!(pace.next_due(t0, packet), t0 + packet);
        // Woken 15 ms late, longer than a packet: the lost time is caught up.
        let late = t0 + packet + Duration::from_millis(15);
        assert_eq!(pace.next_due(late, packet), t0 + packet * 2);
    }

    #[test]
    fn pace_starts_over_after_a_real_gap() {
        let (mut pace, t0, packet) = (Pace::default(), Instant::now(), Duration::from_millis(10));
        pace.next_due(t0, packet);
        // The decoder was waiting for data: no burst to make up for it.
        let later = t0 + packet + PACE_SLACK + Duration::from_millis(1);
        assert_eq!(pace.next_due(later, packet), later + packet);
    }

    #[test]
    fn no_stream_holds_real_time_and_keeps_only_the_newest_audio() {
        let (shared, local) = (full_ring(false), LocalSource::new());
        gone_for_a_while(&shared);
        let took = write_packets(&shared, &local, 8);
        // 800 ms of audio. It took under 1 ms a packet before.
        assert!(took >= Duration::from_millis(790), "raced ahead: {took:?}");
        assert!(took < Duration::from_millis(2000), "fell behind: {took:?}");
        let ring = lock(&shared.ring);
        assert!(ring.len() <= RING_CAP);
        assert_eq!(ring.back(), Some(&7.0));
    }

    #[test]
    fn no_stream_serves_the_grace_once_then_keeps_moving() {
        let (shared, local) = (full_ring(false), LocalSource::new());
        // A device switch in progress: this first write waits for the grace, no longer.
        let took = write_packets(&shared, &local, 1);
        assert!(took >= DEAD_WRITE_GRACE, "gave up early: {took:?}");
        assert!(took < DEAD_WRITE_GRACE + Duration::from_millis(600), "{took:?}");
        // The outage is remembered: the next packet only takes its own playing time.
        let took = write_packets(&shared, &local, 1);
        assert!(took < Duration::from_millis(350), "paid the grace again: {took:?}");
    }

    /// Takes 100 ms of audio every 10 ms, a device far faster than any test needs.
    fn fake_device(shared: &Arc<Shared>) -> (JoinHandle<usize>, Arc<AtomicBool>) {
        let (shared, stop) = (Arc::clone(shared), Arc::new(AtomicBool::new(false)));
        let stop_it = Arc::clone(&stop);
        let device = std::thread::spawn(move || {
            let mut taken = 0;
            while !stop_it.load(Ordering::SeqCst) {
                {
                    let mut ring = lock(&shared.ring);
                    let n = ring.len().min(PACKET);
                    ring.drain(..n);
                    taken += n;
                }
                shared.drained.notify_all();
                std::thread::sleep(Duration::from_millis(10));
            }
            taken
        });
        (device, stop)
    }

    #[test]
    fn a_live_stream_is_waited_for_and_loses_nothing() {
        let (shared, local) = (full_ring(true), LocalSource::new());
        let (device, stop) = fake_device(&shared);
        let mut pace = Pace::default();
        let mut stalled = false;
        for _ in 0..40 {
            stalled |= shared.push(&vec![0.5f32; PACKET], &local, &mut pace);
        }
        stop.store(true, Ordering::SeqCst);
        let taken = device.join().unwrap();
        assert!(!stalled);
        assert!(!shared.reopen.load(Ordering::SeqCst));
        assert!(shared.alive.load(Ordering::SeqCst));
        assert_eq!(taken + lock(&shared.ring).len(), RING_CAP + 40 * PACKET);
    }

    #[test]
    fn a_stalled_stream_is_reopened_and_never_hangs_the_writer() {
        // A fake of the driver that can't be produced here: alive by every report, and
        // nothing ever leaves the ring.
        let (shared, local) = (full_ring(true), LocalSource::new());
        shared.pulled.store(true, Ordering::SeqCst);
        let (started, mut pace) = (Instant::now(), Pace::default());
        assert!(shared.push(&vec![1.0f32; PACKET], &local, &mut pace));
        let took = started.elapsed();
        assert!(took >= STALL_DEADLINE, "gave up early: {took:?}");
        assert!(took < STALL_DEADLINE + Duration::from_secs(1), "{took:?}");
        assert!(shared.reopen.load(Ordering::SeqCst), "the output thread is told to rebuild");
        assert!(!shared.alive.load(Ordering::SeqCst));

        // Until it has, the writer goes on as for a dead device, with no second grace.
        let took = write_packets(&shared, &local, 3);
        assert!(took < Duration::from_millis(900), "{took:?}");
        assert!(took >= Duration::from_millis(290), "raced ahead: {took:?}");
    }

    #[test]
    fn a_device_still_starting_is_given_time_before_it_counts_as_stalled() {
        // A device that has pulled audio before and stops: the short deadline.
        assert_eq!(stall_deadline(true), STALL_DEADLINE);
        // One that hasn't started yet (power saving, Bluetooth): longer, so it isn't
        // reopened before it ever plays, but still reopened if it never does.
        assert!(stall_deadline(false) > STALL_DEADLINE);
        assert!(stall_deadline(false) <= Duration::from_secs(10));
        // Opening a stream forgets the last one's pulling.
        let shared = Shared::new(None);
        shared.pulled.store(true, Ordering::SeqCst);
        let log: Logger = Arc::new(|_| {});
        let on_change: OnChange = Arc::new(|_| {});
        went_live(&shared, &log, &on_change, "test");
        assert!(!shared.pulled.load(Ordering::SeqCst));
    }

    #[test]
    fn shutdown_ends_a_wait_promptly() {
        for alive in [true, false] {
            let shared = full_ring(alive);
            let local = LocalSource::new();
            gone_for_a_while(&shared);
            let closer = {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    shared.shutdown.store(true, Ordering::SeqCst);
                })
            };
            // Far longer than the test should take, were shutdown missed.
            let huge = vec![0.0f32; (SRC_RATE as usize * SRC_CHANNELS) * 30];
            let started = Instant::now();
            shared.push(&huge, &local, &mut Pace::default());
            assert!(started.elapsed() < Duration::from_millis(500), "alive: {alive}");
            closer.join().unwrap();
        }
    }

    #[test]
    fn local_audio_owning_the_output_holds_real_time_too() {
        let (shared, local) = (full_ring(true), LocalSource::new());
        local.activate(1);
        let took = write_packets(&shared, &local, 5);
        assert!(took >= Duration::from_millis(490), "raced ahead: {took:?}");
        assert!(took < Duration::from_millis(1500), "{took:?}");
        assert!(!shared.reopen.load(Ordering::SeqCst), "the device isn't at fault");
    }

    fn ring_of(frames: &[(f32, f32)]) -> VecDeque<f32> {
        frames.iter().flat_map(|&(l, r)| [l, r]).collect()
    }

    #[test]
    fn same_rate_passes_samples_through() {
        let mut rs = Resampler { step: 1.0, frac: 1.0, cur: [0.0; 2], nxt: [0.0; 2] };
        let mut ring = ring_of(&[(0.1, 0.2), (0.3, 0.4), (0.5, 0.6)]);
        // First call primes cur/nxt, so output lags by one frame - which is the point
        // of an interpolating resampler, not a bug.
        let out: Vec<_> = (0..2).map(|_| next_frame(&mut rs, &mut ring)).collect();
        assert_eq!(out, vec![(0.0, 0.0), (0.1, 0.2)]);
    }

    #[test]
    fn upsampling_to_48k_consumes_source_at_the_right_rate() {
        // 44.1k -> 48k: 48 000 output frames must consume ~44 100 source frames.
        let mut rs = Resampler { step: SRC_RATE / 48_000.0, frac: 1.0, cur: [0.0; 2], nxt: [0.0; 2] };
        let src: Vec<(f32, f32)> = (0..50_000).map(|i| (i as f32, -(i as f32))).collect();
        let mut ring = ring_of(&src);
        let before = ring.len() / 2;
        for _ in 0..48_000 {
            next_frame(&mut rs, &mut ring);
        }
        let used = before - ring.len() / 2;
        assert!((44_099..=44_102).contains(&used), "consumed {used} source frames");
    }

    #[test]
    fn interpolates_between_frames() {
        let mut rs = Resampler { step: 0.5, frac: 1.0, cur: [0.0; 2], nxt: [0.0; 2] };
        let mut ring = ring_of(&[(0.0, 0.0), (1.0, -1.0), (2.0, -2.0)]);
        let out: Vec<_> = (0..5).map(|_| next_frame(&mut rs, &mut ring)).collect();
        // Half-steps: 0.0 -> 0.5 -> 1.0 -> 1.5 ... after the one-frame priming delay.
        assert_eq!(out[2], (0.0, 0.0));
        assert_eq!(out[3], (0.5, -0.5));
        assert_eq!(out[4], (1.0, -1.0));
    }

    #[test]
    fn underrun_outputs_silence_and_does_not_skip() {
        let mut rs = Resampler { step: 1.0, frac: 1.0, cur: [0.0; 2], nxt: [0.0; 2] };
        let mut ring = VecDeque::new();
        assert_eq!(next_frame(&mut rs, &mut ring), (0.0, 0.0));
        // Audio arriving late is still played from its start, not jumped past.
        ring.extend([0.7f32, 0.8]);
        ring.extend([0.9f32, 1.0]);
        next_frame(&mut rs, &mut ring);
        assert_eq!(next_frame(&mut rs, &mut ring), (0.7, 0.8));
    }
}
