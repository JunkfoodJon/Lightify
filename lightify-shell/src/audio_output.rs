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
//! * **Never block without a bound.** Every wait has a deadline, and when no stream is
//!   pulling audio the writer drops the oldest samples rather than waiting.
//! * **`stop` must never fail.** librespot calls `exit(1)` if it does.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Upper bound on letting the buffered tail play out when librespot stops the sink.
const DRAIN_MAX: Duration = Duration::from_millis(600);

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
    dead_since: Mutex<Option<Instant>>,
    /// librespot has started the sink (paused = output silence, keep the buffer).
    playing: AtomicBool,
    /// Ask the output thread to rebuild its stream on its next pass.
    reopen: AtomicBool,
    shutdown: AtomicBool,
    /// `Some(name)` = the user pinned an output; `None` = follow the OS default.
    target: Mutex<Option<String>>,
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
    if let Some(shared) = lock(active()).upgrade() {
        *lock(&shared.target) = device.filter(|d| !d.trim().is_empty());
        shared.reopen.store(true, Ordering::SeqCst);
    }
}

pub struct SwitchableSink {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    log: Logger,
}

impl SwitchableSink {
    /// `device`: the initial target — a pinned output name, or `None` for the OS
    /// default. `on_change` is told the device name every time output lands on one.
    pub fn new(device: Option<String>, log: Logger, on_change: OnChange) -> Self {
        let shared = Arc::new(Shared {
            ring: Mutex::new(VecDeque::with_capacity(RING_CAP)),
            drained: Condvar::new(),
            alive: AtomicBool::new(false),
            dead_since: Mutex::new(None),
            playing: AtomicBool::new(false),
            reopen: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            target: Mutex::new(device.filter(|d| !d.trim().is_empty())),
        });
        *lock(active()) = Arc::downgrade(&shared);

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
        Self { shared, thread, log }
    }
}

impl Drop for SwitchableSink {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.drained.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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
        let sh = &self.shared;
        let mut ring = lock(&sh.ring);
        while ring.len() + data.len() > RING_CAP {
            if sh.shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }
            if sh.alive.load(Ordering::SeqCst) {
                *lock(&sh.dead_since) = None;
            } else {
                let since = *lock(&sh.dead_since).get_or_insert_with(Instant::now);
                if since.elapsed() >= DEAD_WRITE_GRACE {
                    // No device is pulling audio (it vanished and none has opened yet).
                    // Waiting here is what used to hang the engine for good; drop the
                    // oldest audio instead so the player thread keeps moving.
                    let excess = (ring.len() + data.len()).saturating_sub(RING_CAP).min(ring.len());
                    ring.drain(..excess);
                    break;
                }
            }
            ring = sh.drained.wait_timeout(ring, Duration::from_millis(20)).unwrap_or_else(|p| p.into_inner()).0;
        }
        ring.extend(data.iter().copied());
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
    log(format!("audio output: playing on \"{name}\""));
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
    let data_shared = Arc::clone(shared);
    let err_shared = Arc::clone(shared);

    device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
            let playing = data_shared.playing.load(Ordering::Relaxed);
            let mut ring = lock(&data_shared.ring);
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
