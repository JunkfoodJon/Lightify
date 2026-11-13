//! Local player: plays files from the local library through the same audio output the
//! Spotify engine uses (`audio_output`), so device switching, the OS default following
//! and "Lightify owns its playback device" all apply to local playback too.
//!
//! CONTRACT (main.rs is written against exactly this API):
//! * One file at a time, decoded with symphonia and resampled to the output's rate, on
//!   a thread of its own. Never blocks the caller.
//! * While local audio is playing the output plays it and NOT the librespot ring (the
//!   worker also pauses Spotify first; the output must still never mix the two).
//! * Works whether or not the Spotify engine is running (the output is started on
//!   demand if the engine hasn't started it).
//! * Gapless: the file given to `set_next` is opened ahead of time and starts on the
//!   sample after the current one ends, then `Event::Track` reports it.
//!
//! Details beyond the contract:
//! * `play` clears any pending `set_next` (a new context); call `set_next` after it.
//! * After `Ended` or `Error` local playback is stopped: `active()` is false and the
//!   output is back with librespot. Both are followed by `Playing { playing: false }`,
//!   as is `stop`.
//!
//! How it fits together: `play` hands `audio_output::local_source()` (a ring of
//! 44.1 kHz stereo, the rate the output thread already converts from) to a new
//! generation, which cuts the previous audio at once, and starts a worker thread that
//! decodes, resamples and pushes into it. The output's device callback plays that
//! ring instead of librespot's while it is active and counts the frames it consumed,
//! which is what positions, the gapless `Track` and `Ended` are timed from.

use std::collections::VecDeque;
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use rubato::{FftFixedIn, Resampler};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use crate::audio_output::{self, LocalSource, SwitchableSink, LOCAL_RATE};
use lightify_i18n::tr;

#[derive(Debug, Clone)]
pub enum Event {
    /// A file started playing: the one `play` was given (`next: false`), or the
    /// gapless `set_next` file taking over (`next: true`; the same path can follow
    /// itself, e.g. a repeat or a queued copy).
    Track { path: PathBuf, duration_ms: u64, next: bool },
    /// Playback position, a few times a second while playing.
    Position { ms: u64 },
    Playing { playing: bool },
    /// The current file played to its end and no `set_next` file followed.
    Ended,
    /// The file couldn't be opened or decoded (playback stops). `path` is the file
    /// that failed: the one heard, or the `set_next` file it was to continue with.
    Error { path: PathBuf, msg: String },
}

/// Where events go (set once by main.rs, called from the player's own thread). Each
/// comes with the generation of the `play` it belongs to (what `play` returned);
/// none is sent for a generation once a newer `play` has started.
pub fn set_listener(f: impl Fn(u64, Event) + Send + Sync + 'static) {
    *lock(&core().listener) = Some(Arc::new(f));
}

/// Play `path` from `start_ms`, replacing whatever is playing. Returns the new
/// generation, which tags this playback's events.
pub fn play(path: &Path, start_ms: u64) -> u64 {
    Core::play(core(), path, start_ms)
}

/// The file to continue with, gaplessly, when the current one ends (`None` clears it).
pub fn set_next(path: Option<PathBuf>) {
    core().send(Cmd::SetNext(path));
}

pub fn pause() {
    core().pause();
}

pub fn resume() {
    core().resume();
}

/// Stop and release the output to the Spotify engine.
pub fn stop() {
    core().stop();
}

pub fn seek(ms: u64) {
    core().send(Cmd::Seek(ms));
}

/// 0.0 ..= 1.0, applied to local audio only.
pub fn set_volume(v: f32) {
    if v.is_finite() {
        core().src.set_volume(v);
    }
}

/// Is local audio the output's source right now (playing or paused, not stopped)?
pub fn active() -> bool {
    core().src.is_active()
}

// ── Player core (one global instance; tests make their own) ────────────────────

/// Output frames per second of everything pushed into the local source.
const OUT_RATE: u32 = LOCAL_RATE;
/// Worker poll interval while it has nothing to decode or is waiting for room.
const TICK: Duration = Duration::from_millis(10);
/// How often `Position` is reported while playing.
const POSITION_EVERY: Duration = Duration::from_millis(250);
/// Upper bound on chunks decoded per worker pass, so commands are never starved.
const CHUNKS_PER_PASS: usize = 16;
/// Input frames per resampler call.
const RS_CHUNK: usize = 1024;
/// Consecutive undecodable packets before a file is given up on.
const MAX_BAD_PACKETS: u32 = 100;
/// Input rates a file may have. Real audio sits well inside this (8 kHz .. 768 kHz);
/// a header outside it is broken, and resampling from it would plan absurdly large FFTs
/// (a rate of 1 Hz or 4 GHz means hundreds of MB per chunk).
const MIN_IN_RATE: u32 = 1_000;
const MAX_IN_RATE: u32 = 768_000;

type Listener = Arc<dyn Fn(u64, Event) + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

enum Cmd {
    Pause,
    Resume,
    Seek(u64),
    SetNext(Option<PathBuf>),
    Stop,
    /// A newer `play` took over; leave the source alone.
    Quit,
}

struct WorkerSlot {
    gen: u64,
    tx: Sender<Cmd>,
}

struct Core {
    src: Arc<LocalSource>,
    /// Bumped by every `play`.
    gen: AtomicU64,
    worker: Mutex<Option<WorkerSlot>>,
    listener: Mutex<Option<Listener>>,
    /// Held while a worker checks its generation and emits, and while `play` moves the
    /// generation on: so once `play` has returned, no older worker's event goes out.
    emit_gate: Mutex<()>,
    /// Drive a real output (start one when none is running). Off in tests.
    real_output: bool,
    /// The output this module started because the engine had none running.
    own_output: Mutex<Option<SwitchableSink>>,
}

fn core() -> &'static Arc<Core> {
    static CORE: OnceLock<Arc<Core>> = OnceLock::new();
    CORE.get_or_init(|| Arc::new(Core::new(Arc::clone(audio_output::local_source()), true)))
}

impl Core {
    fn new(src: Arc<LocalSource>, real_output: bool) -> Self {
        Self {
            src,
            gen: AtomicU64::new(0),
            worker: Mutex::new(None),
            listener: Mutex::new(None),
            emit_gate: Mutex::new(()),
            real_output,
            own_output: Mutex::new(None),
        }
    }

    /// Report `ev` for generation `gen`, unless a newer `play` has started (a replaced
    /// worker still running must never show its file while another one plays).
    fn emit(&self, gen: u64, ev: Event) {
        // Cloned out so the listener never runs under the listener lock. It does run
        // under the gate, which only `play` and other emits take.
        let l = lock(&self.listener).clone();
        let _gate = lock(&self.emit_gate);
        if self.gen.load(Ordering::SeqCst) != gen {
            return;
        }
        if let Some(l) = l {
            l(gen, ev);
        }
    }

    fn play(this: &Arc<Self>, path: &Path, start_ms: u64) -> u64 {
        let gen = {
            let _gate = lock(&this.emit_gate);
            this.gen.fetch_add(1, Ordering::SeqCst) + 1
        };
        // Cut whatever was playing now, not when the old worker notices.
        this.src.activate(gen);
        let (tx, rx) = mpsc::channel();
        let old = lock(&this.worker).replace(WorkerSlot { gen, tx });
        if let Some(old) = old {
            let _ = old.tx.send(Cmd::Quit);
        }
        let core = Arc::clone(this);
        let path_buf = path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("lightify-local-player".into())
            .spawn(move || worker_main(core, gen, rx, path_buf, start_ms));
        if let Err(e) = spawned {
            if this.finish(gen) {
                this.emit(gen, Event::Error { path: path.to_path_buf(), msg: tr!("could not start the player thread: {}", e) });
                this.emit(gen, Event::Playing { playing: false });
            }
        }
        gen
    }

    fn send(&self, cmd: Cmd) {
        if let Some(w) = lock(&self.worker).as_ref() {
            let _ = w.tx.send(cmd);
        }
    }

    fn pause(&self) {
        let w = lock(&self.worker);
        if let Some(w) = w.as_ref() {
            // Directly, so the device stops consuming on its very next callback.
            self.src.set_paused(true);
            let _ = w.tx.send(Cmd::Pause);
        }
    }

    fn resume(&self) {
        let w = lock(&self.worker);
        if let Some(w) = w.as_ref() {
            self.src.set_paused(false);
            let _ = w.tx.send(Cmd::Resume);
        }
    }

    fn stop(&self) {
        let w = lock(&self.worker).take();
        // Output back to librespot immediately; the worker winds down on its own.
        self.src.deactivate(None);
        if let Some(w) = w {
            let _ = w.tx.send(Cmd::Stop);
        }
    }

    /// Generation `gen` is done (ended, failed, stopped): release the source and the
    /// slot if it still owns them. Returns whether it did - false means a newer
    /// `play` or a `stop` already took over, and nothing should be reported.
    fn finish(&self, gen: u64) -> bool {
        let released = self.src.deactivate(Some(gen));
        let mut w = lock(&self.worker);
        if w.as_ref().is_some_and(|w| w.gen == gen) {
            *w = None;
        }
        released
    }

    /// Make sure some output is playing the local source (production only).
    fn ensure_output(&self) {
        if !self.real_output {
            return;
        }
        // The engine's sink took over from ours: ours only outputs silence now.
        let superseded = {
            let mut own = lock(&self.own_output);
            if own.as_ref().is_some_and(|o| !o.is_current()) { own.take() } else { None }
        };
        drop(superseded); // joins its thread (bounded: one output-thread poll)
        if audio_output::output_running() {
            return;
        }
        let device = match audio_output::last_target() {
            Some(t) => t,
            None => {
                let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
                crate::engine::resolve_output(&configured).ok().flatten()
            }
        };
        let log: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|m: String| eprintln!("local player: {m}"));
        let on_change: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_d: String| {});
        if let Some(out) = audio_output::start_if_none(device, log, on_change) {
            let old = lock(&self.own_output).replace(out);
            drop(old);
        }
    }

    /// Close the output this module started, once nothing plays locally any more.
    fn release_output(&self, gen: u64) {
        if self.gen.load(Ordering::SeqCst) != gen || self.src.is_active() {
            return; // a newer play is using it
        }
        let own = lock(&self.own_output).take();
        drop(own);
    }
}

// ── The worker ─────────────────────────────────────────────────────────────────

enum Exit {
    /// A newer `play` took over.
    Replaced,
    Stopped,
    Ended,
    Failed { path: PathBuf, msg: String },
}

fn worker_main(core: Arc<Core>, gen: u64, rx: Receiver<Cmd>, path: PathBuf, start_ms: u64) {
    // A decoder panicking on a hostile file must not leave the output stuck on silence.
    let exit = std::panic::catch_unwind(AssertUnwindSafe(|| run(&core, gen, &rx, path.clone(), start_ms)))
        .unwrap_or_else(|p| Exit::Failed { path, msg: crash_msg(&*p) });
    match exit {
        Exit::Replaced => {}
        Exit::Stopped => {
            core.emit(gen, Event::Playing { playing: false });
            core.release_output(gen);
        }
        Exit::Ended | Exit::Failed { .. } => {
            if core.finish(gen) {
                if let Exit::Failed { path, msg } = exit {
                    core.emit(gen, Event::Error { path, msg });
                } else {
                    core.emit(gen, Event::Ended);
                }
                core.emit(gen, Event::Playing { playing: false });
            } else {
                // `stop` got there first: report it as the stop it was (a newer `play`
                // taking over is filtered out by `emit`).
                core.emit(gen, Event::Playing { playing: false });
            }
            core.release_output(gen);
        }
    }
}

/// What to say about a decoder panic. symphonia's readers build a `TimeBase` from the
/// header's sample rate, which panics on 0.
fn crash_msg(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str));
    if text.is_some_and(|t| t.contains("TimeBase")) {
        tr!("the file reports a sample rate of 0 Hz")
    } else {
        tr!("the decoder crashed on this file")
    }
}

/// `Source::open`, with a decoder panic on a hostile file reported as that file's
/// error instead of taking down the worker (and with it the file being heard while
/// the next one is opened ahead of time).
fn open_guarded(path: &Path) -> Result<Source, String> {
    std::panic::catch_unwind(AssertUnwindSafe(|| Source::open(path))).unwrap_or_else(|p| Err(crash_msg(&*p)))
}

/// A stretch of the pushed stream that belongs to one file.
struct Seg {
    /// Stream frame (in `LocalSource` counter terms) where it starts.
    start: u64,
    path: PathBuf,
    duration_ms: u64,
    /// File position at `start`.
    base_ms: u64,
}

enum End {
    Ended,
    Failed { path: PathBuf, msg: String },
}

fn run(core: &Core, gen: u64, rx: &Receiver<Cmd>, path: PathBuf, start_ms: u64) -> Exit {
    core.ensure_output();
    let mut cur = match open_guarded(&path) {
        Ok(s) => s,
        Err(msg) => return Exit::Failed { path, msg },
    };
    let mut base_ms = 0;
    if start_ms > 0 {
        base_ms = cur.seek(start_ms).unwrap_or(0);
    }
    let src = &core.src;
    let mut segs: VecDeque<Seg> = VecDeque::new();
    segs.push_back(Seg { start: src.counters().0, path: path.clone(), duration_ms: cur.duration_ms, base_ms });
    core.emit(gen, Event::Track { path: path.clone(), duration_ms: cur.duration_ms, next: false });
    core.emit(gen, Event::Playing { playing: true });

    let mut next_path: Option<PathBuf> = None;
    let mut prepared: Option<(PathBuf, Result<Source, String>)> = None;
    // Decoded audio not yet taken by the ring, and how much of it was.
    let mut pending: Vec<f32> = Vec::new();
    let mut pending_off = 0usize;
    let mut done: Option<End> = None;
    let mut paused = false;
    let mut last_pos = Instant::now() - POSITION_EVERY;

    loop {
        // ── commands ──
        let mut first = true;
        loop {
            let cmd = if first {
                first = false;
                // Sleep only when there's nothing to decode right now.
                let idle = paused || done.is_some() || (pending_off < pending.len());
                match rx.recv_timeout(if idle { TICK } else { Duration::ZERO }) {
                    Ok(c) => c,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return Exit::Replaced,
                }
            } else {
                match rx.try_recv() {
                    Ok(c) => c,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Exit::Replaced,
                }
            };
            match cmd {
                Cmd::Quit => return Exit::Replaced,
                Cmd::Stop => return Exit::Stopped,
                Cmd::Pause => {
                    src.set_paused(true);
                    if !paused {
                        paused = true;
                        core.emit(gen, Event::Playing { playing: false });
                    }
                }
                Cmd::Resume => {
                    src.set_paused(false);
                    if paused {
                        paused = false;
                        core.emit(gen, Event::Playing { playing: true });
                    }
                }
                Cmd::SetNext(p) => {
                    // Already decoding it (the gapless switch happened in decode time but
                    // not yet in play time): a repeat of the same request, not a second copy.
                    let ahead = segs.len() > 1 && segs.back().map(|s| &s.path) == p.as_ref();
                    if !ahead {
                        // The file was fully decoded before this came (a short one is
                        // over in decode time before its own `Track` reaches the list),
                        // but it is still being heard: carry on into the new next file
                        // instead of ending. The finished source hands over to it.
                        if p.is_some() && matches!(done, Some(End::Ended)) {
                            done = None;
                        }
                        next_path = p;
                    }
                }
                Cmd::Seek(ms) => {
                    if segs.len() > 1 {
                        // Decoding has already moved on to the next file; go back to the one
                        // being heard, and queue the one after it again.
                        let heard = segs[0].path.clone();
                        next_path = Some(segs[1].path.clone());
                        prepared = None;
                        match open_guarded(&heard) {
                            Ok(s) => cur = s,
                            Err(msg) => return Exit::Failed { path: heard, msg },
                        }
                    }
                    src.clear(gen);
                    pending.clear();
                    pending_off = 0;
                    let seg = segs.pop_front().expect("segs is never empty");
                    segs.clear();
                    done = None;
                    let reached = match cur.seek(ms) {
                        Ok(at) => at,
                        Err(_) => {
                            // Beyond the end (or unseekable): the file is over from here.
                            cur.finish();
                            ms.min(seg.duration_ms)
                        }
                    };
                    segs.push_back(Seg { start: src.counters().0, base_ms: reached, ..seg });
                    core.emit(gen, Event::Position { ms: reached });
                    last_pos = Instant::now();
                }
            }
        }

        core.ensure_output();

        // Keep the prepared next file in step with `next_path` (opened ahead of time).
        let want = next_path.as_ref();
        if prepared.as_ref().map(|(p, _)| p) != want {
            prepared = want.map(|p| (p.clone(), open_guarded(p)));
        }

        // ── feed ──
        if !paused {
            for _ in 0..CHUNKS_PER_PASS {
                if pending_off < pending.len() {
                    match src.push(gen, &pending[pending_off..]) {
                        // The ring was taken away: by a newer `play`, or by `stop` (whose
                        // command may still be queued behind us).
                        None if core.gen.load(Ordering::SeqCst) == gen => return Exit::Stopped,
                        None => return Exit::Replaced,
                        Some(n) => pending_off += n * 2,
                    }
                    if pending_off < pending.len() {
                        break; // ring full
                    }
                }
                if done.is_some() {
                    break;
                }
                match std::panic::catch_unwind(AssertUnwindSafe(|| cur.next_chunk())).unwrap_or_else(|p| Err(crash_msg(&*p))) {
                    Ok(Some(chunk)) => {
                        pending = chunk;
                        pending_off = 0;
                    }
                    Ok(None) => {
                        // End of this file: gaplessly into the prepared one, if any.
                        let decoded = src.counters().0 + ((pending.len() - pending_off) / 2) as u64;
                        let opened = match (next_path.take(), prepared.take()) {
                            (Some(p), Some((pp, res))) if p == pp => Some((p, res)),
                            (Some(p), _) => {
                                let res = open_guarded(&p);
                                Some((p, res))
                            }
                            (None, _) => None,
                        };
                        match opened {
                            Some((p, Ok(s))) => {
                                segs.push_back(Seg { start: decoded, path: p, duration_ms: s.duration_ms, base_ms: 0 });
                                cur = s;
                            }
                            Some((p, Err(msg))) => done = Some(End::Failed { path: p, msg }),
                            None => done = Some(End::Ended),
                        }
                    }
                    Err(msg) => {
                        // Nothing playable at all: a broken file. One that fails part-way
                        // just ends there (and moves on to the next file, if any).
                        if !cur.produced_any {
                            done = Some(End::Failed { path: cur.path.clone(), msg });
                        } else {
                            cur.finish();
                        }
                    }
                }
            }
        }

        // ── what the device has actually played ──
        let (pushed, played) = src.counters();
        let decoded_end = pushed + ((pending.len() - pending_off) / 2) as u64;
        while segs.len() > 1 && played >= segs[1].start {
            segs.pop_front();
            // A file that produced no audio at all is skipped silently (the last one may
            // still be decoding its first chunk, so it counts as playing).
            let end = segs.get(1).map_or(decoded_end, |s| s.start);
            if end > segs[0].start || (segs.len() == 1 && done.is_none()) {
                core.emit(gen, Event::Track { path: segs[0].path.clone(), duration_ms: segs[0].duration_ms, next: true });
            }
        }
        if let Some(end) = &done {
            if pending_off >= pending.len() && played >= pushed {
                return match end {
                    End::Ended => Exit::Ended,
                    End::Failed { path, msg } => Exit::Failed { path: path.clone(), msg: msg.clone() },
                };
            }
        }
        if !paused && last_pos.elapsed() >= POSITION_EVERY {
            last_pos = Instant::now();
            let s = &segs[0];
            let mut ms = s.base_ms + played.saturating_sub(s.start) * 1000 / u64::from(OUT_RATE);
            if s.duration_ms > 0 {
                ms = ms.min(s.duration_ms);
            }
            core.emit(gen, Event::Position { ms });
        }
    }
}

// ── Decoding + resampling one file ─────────────────────────────────────────────

/// One open file, producing interleaved stereo f32 at `OUT_RATE`.
struct Source {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    duration_ms: u64,
    sbuf: Option<SampleBuffer<f32>>,
    /// Created on the first decoded buffer, at that buffer's rate.
    rs: Option<Rs>,
    /// Decoded input not yet through the resampler, per channel.
    stage: [Vec<f32>; 2],
    /// Input frames decoded / output frames returned since open or seek.
    fed: u64,
    emitted: u64,
    /// Leading resampler delay still to cut from the output.
    skip: usize,
    eof: bool,
    finished: bool,
    produced_any: bool,
    bad_packets: u32,
}

enum Rs {
    /// Already at `OUT_RATE`.
    Pass,
    Fft { r: Box<FftFixedIn<f32>>, in_rate: u32 },
}

impl Source {
    fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| tr!("could not open the file: {}", e))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let opts = FormatOptions { enable_gapless: true, ..Default::default() };
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &opts, &MetadataOptions::default())
            .map_err(|e| tr!("unsupported or unreadable audio file: {}", e))?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| tr!("the file has no audio track"))?;
        let params = track.codec_params.clone();
        let track_id = track.id;
        // Before any decoding: the decoder sizes its buffers from the header too.
        if let Some(rate) = params.sample_rate {
            check_rate(rate)?;
        }
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| tr!("unsupported audio codec: {}", e))?;
        let time_base = params.time_base.filter(|tb| tb.numer > 0 && tb.denom > 0);
        let duration_ms = match (params.n_frames, time_base, params.sample_rate) {
            (Some(n), Some(tb), _) => time_ms(tb.calc_time(n)),
            (Some(n), None, Some(rate)) if rate > 0 => n.saturating_mul(1000) / u64::from(rate),
            _ => 0,
        };
        Ok(Self {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id,
            time_base,
            duration_ms,
            sbuf: None,
            rs: None,
            stage: [Vec::new(), Vec::new()],
            fed: 0,
            emitted: 0,
            skip: 0,
            eof: false,
            finished: false,
            produced_any: false,
            bad_packets: 0,
        })
    }

    /// Jump to `ms` (coarse). Returns the position actually reached.
    fn seek(&mut self, ms: u64) -> Result<u64, String> {
        let time = Time::new(ms / 1000, (ms % 1000) as f64 / 1000.0);
        let to = SeekTo::Time { time, track_id: Some(self.track_id) };
        let res = self.format.seek(SeekMode::Coarse, to);
        self.decoder.reset();
        self.stage[0].clear();
        self.stage[1].clear();
        if let Some(Rs::Fft { r, .. }) = &mut self.rs {
            r.reset();
            self.skip = r.output_delay();
        }
        self.fed = 0;
        self.emitted = 0;
        self.eof = false;
        self.finished = false;
        let seeked = res.map_err(|e| tr!("seek failed: {}", e))?;
        Ok(match self.time_base {
            Some(tb) => time_ms(tb.calc_time(seeked.actual_ts)),
            None => ms,
        })
    }

    /// Stop producing audio (the rest of the file is skipped).
    fn finish(&mut self) {
        self.finished = true;
    }

    /// The next stretch of output, `None` at the end of the file.
    fn next_chunk(&mut self) -> Result<Option<Vec<f32>>, String> {
        loop {
            if self.finished {
                return Ok(None);
            }
            if self.eof {
                let tail = self.flush()?;
                self.finished = true;
                if !tail.is_empty() {
                    return Ok(Some(tail));
                }
                return Ok(None);
            }
            let out = self.resample_ready()?;
            if !out.is_empty() {
                self.produced_any = true;
                return Ok(Some(out));
            }
            if !self.decode_packet()? {
                self.eof = true;
            }
        }
    }

    /// Decode one packet into `stage`. False at the end of the stream.
    fn decode_packet(&mut self) -> Result<bool, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                // A chained stream changed its track list: treat as the end of this file.
                Err(SymError::ResetRequired) => return Ok(false),
                Err(e) => return Err(tr!("could not read the file: {}", e)),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    self.bad_packets = 0;
                    let spec = *buf.spec();
                    let channels = spec.channels.count();
                    if buf.frames() == 0 || channels == 0 {
                        continue;
                    }
                    let needed = buf.capacity() * channels;
                    let fits = self.sbuf.as_ref().is_some_and(|b| b.capacity() >= needed);
                    if !fits {
                        self.sbuf = Some(SampleBuffer::<f32>::new(buf.capacity() as u64, spec));
                    }
                    if self.rs.is_none() {
                        let rs = make_rs(spec.rate)?;
                        if let Rs::Fft { r, .. } = &rs {
                            self.skip = r.output_delay();
                        }
                        self.rs = Some(rs);
                    }
                    let Some(sb) = self.sbuf.as_mut() else { continue };
                    sb.copy_interleaved_ref(buf);
                    let samples = sb.samples();
                    for f in samples.chunks_exact(channels) {
                        // Mono -> both sides; more than two channels -> front left/right.
                        let l = f[0];
                        let r = if channels > 1 { f[1] } else { l };
                        self.stage[0].push(l);
                        self.stage[1].push(r);
                    }
                    self.fed += (samples.len() / channels) as u64;
                    return Ok(true);
                }
                Err(SymError::DecodeError(_)) => {
                    // A corrupt frame: skip it, but not forever.
                    self.bad_packets += 1;
                    if self.bad_packets > MAX_BAD_PACKETS {
                        return Err(tr!("too many undecodable frames"));
                    }
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(SymError::ResetRequired) => return Ok(false),
                Err(e) => return Err(tr!("could not decode the file: {}", e)),
            }
        }
    }

    /// Run whatever full resampler chunks `stage` holds.
    fn resample_ready(&mut self) -> Result<Vec<f32>, String> {
        let mut out = Vec::new();
        match &mut self.rs {
            None => {}
            Some(Rs::Pass) => {
                let n = self.stage[0].len();
                out.reserve(n * 2);
                for i in 0..n {
                    out.push(self.stage[0][i]);
                    out.push(self.stage[1][i]);
                }
                self.stage[0].clear();
                self.stage[1].clear();
                self.emitted += n as u64;
            }
            Some(Rs::Fft { r, .. }) => loop {
                let need = r.input_frames_next();
                if self.stage[0].len() < need {
                    break;
                }
                let res = r
                    .process(&[&self.stage[0][..need], &self.stage[1][..need]], None)
                    .map_err(|e| tr!("resampling failed: {}", e))?;
                self.stage[0].drain(..need);
                self.stage[1].drain(..need);
                append_out(&mut out, &res, &mut self.skip, &mut self.emitted, u64::MAX);
            },
        }
        Ok(out)
    }

    /// End of input: push the rest through, and the resampler's delayed tail, so the
    /// output is exactly as long as the input (that's what makes the next file gapless).
    fn flush(&mut self) -> Result<Vec<f32>, String> {
        let mut out = self.resample_ready()?;
        if let Some(Rs::Fft { r, in_rate }) = &mut self.rs {
            let rate = u64::from(*in_rate);
            let expected = (self.fed * u64::from(OUT_RATE) + rate / 2) / rate;
            if !self.stage[0].is_empty() {
                let res = r
                    .process_partial(Some(&[&self.stage[0][..], &self.stage[1][..]]), None)
                    .map_err(|e| tr!("resampling failed: {}", e))?;
                self.stage[0].clear();
                self.stage[1].clear();
                append_out(&mut out, &res, &mut self.skip, &mut self.emitted, expected);
            }
            let mut guard = 0;
            while self.emitted < expected && guard < 64 {
                guard += 1;
                let res = r
                    .process_partial::<&[f32]>(None, None)
                    .map_err(|e| tr!("resampling failed: {}", e))?;
                append_out(&mut out, &res, &mut self.skip, &mut self.emitted, expected);
            }
        }
        if !out.is_empty() {
            self.produced_any = true;
        }
        Ok(out)
    }
}

fn check_rate(rate: u32) -> Result<(), String> {
    if (MIN_IN_RATE..=MAX_IN_RATE).contains(&rate) {
        Ok(())
    } else {
        Err(tr!("the file reports a sample rate of {} Hz, which can\u{2019}t be played", rate))
    }
}

fn make_rs(rate: u32) -> Result<Rs, String> {
    if rate == OUT_RATE {
        return Ok(Rs::Pass);
    }
    // Checked again here: a decoded buffer's rate need not be the header's.
    check_rate(rate)?;
    FftFixedIn::<f32>::new(rate as usize, OUT_RATE as usize, RS_CHUNK, 2, 2)
        .map(|r| Rs::Fft { r: Box::new(r), in_rate: rate })
        .map_err(|e| tr!("cannot resample from {0} Hz: {1}", rate, e))
}

/// Interleave resampler output into `out`, cutting the leading `skip` frames and
/// never going past `limit` frames emitted in total.
fn append_out(out: &mut Vec<f32>, res: &[Vec<f32>], skip: &mut usize, emitted: &mut u64, limit: u64) {
    let n = res.first().map_or(0, Vec::len);
    let from = (*skip).min(n);
    *skip -= from;
    let room = limit.saturating_sub(*emitted);
    let to = n.min(from.saturating_add(usize::try_from(room).unwrap_or(usize::MAX)));
    for i in from..to {
        out.push(res[0][i]);
        out.push(res[1][i]);
    }
    *emitted += (to - from) as u64;
}

fn time_ms(t: Time) -> u64 {
    t.seconds.saturating_mul(1000) + (t.frac * 1000.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static FILE_NO: AtomicUsize = AtomicUsize::new(0);

    /// A temp file path unique to this test run.
    fn temp(name: &str) -> PathBuf {
        let n = FILE_NO.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("lightify-lp-test-{}-{n}-{name}", std::process::id()))
    }

    /// Write a 16-bit PCM WAV of `frames` frames; sample value from `f(frame, channel)`.
    fn write_wav(path: &Path, rate: u32, channels: u16, frames: usize, f: impl Fn(usize, usize) -> f32) {
        let data_len = frames * channels as usize * 2;
        let mut b = Vec::with_capacity(44 + data_len);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        // Wrapping: the tests also write absurd rates on purpose.
        b.extend_from_slice(&rate.wrapping_mul(u32::from(channels) * 2).to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data_len as u32).to_le_bytes());
        for i in 0..frames {
            for c in 0..channels as usize {
                let v = (f(i, c).clamp(-1.0, 1.0) * 32767.0) as i16;
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        std::fs::write(path, b).unwrap();
    }

    /// Write a 16-bit big-endian PCM AIFF (8 kHz) of `frames` frames, mono.
    fn write_aiff(path: &Path, frames: usize, f: impl Fn(usize) -> f32) {
        let data_len = frames * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"FORM");
        b.extend_from_slice(&((4 + 26 + 16 + data_len) as u32).to_be_bytes());
        b.extend_from_slice(b"AIFFCOMM");
        b.extend_from_slice(&18u32.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(frames as u32).to_be_bytes());
        b.extend_from_slice(&16u16.to_be_bytes());
        // 8000 Hz as an 80-bit extended float: 2^12 * 1.953125.
        b.extend_from_slice(&(16383u16 + 12).to_be_bytes());
        b.extend_from_slice(&(8000u64 << (63 - 12)).to_be_bytes());
        b.extend_from_slice(b"SSND");
        b.extend_from_slice(&((8 + data_len) as u32).to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        for i in 0..frames {
            b.extend_from_slice(&((f(i).clamp(-1.0, 1.0) * 32767.0) as i16).to_be_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn aiff_decodes() {
        let p = temp("tone.aiff");
        write_aiff(&p, 8000, |i| (i as f32 * 0.05).sin() * 0.5);
        let pcm = decode_all(&p);
        let _ = std::fs::remove_file(&p);
        assert!(!pcm.is_empty(), "AIFF produced no audio");
        assert!(pcm.iter().any(|v| v.abs() > 0.2), "AIFF decoded to silence");
    }

    fn decode_all(path: &Path) -> Vec<f32> {
        let mut s = Source::open(path).unwrap();
        let mut all = Vec::new();
        while let Some(c) = s.next_chunk().unwrap() {
            all.extend(c);
        }
        all
    }

    /// A player on its own source, with no device; events land in the returned vec.
    fn test_core() -> (Arc<Core>, Arc<Mutex<Vec<Event>>>) {
        let core = Arc::new(Core::new(Arc::new(LocalSource::new()), false));
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = Arc::clone(&events);
        *lock(&core.listener) = Some(Arc::new(move |_gen, e| lock(&ev).push(e)));
        (core, events)
    }

    fn track_events(ev: &[Event]) -> Vec<(PathBuf, bool)> {
        ev.iter()
            .filter_map(|e| if let Event::Track { path, next, .. } = e { Some((path.clone(), *next)) } else { None })
            .collect()
    }

    #[test]
    fn local_player_rejects_absurd_sample_rates_with_a_clear_message() {
        for rate in [0u32, 1, 4_000_000_000] {
            let p = temp(&format!("rate{rate}.wav"));
            write_wav(&p, rate, 1, 100, |_, _| 0.1);
            let err = open_guarded(&p).err().unwrap_or_else(|| panic!("{rate} Hz opened"));
            assert!(err.contains("sample rate of"), "{rate} Hz: {err}");
            // Through the player: an Error naming the file, and nothing stuck.
            let (core, events) = test_core();
            Core::play(&core, &p, 0);
            wait_for(&events, |e| matches!(e.last(), Some(Event::Playing { playing: false })));
            std::fs::remove_file(&p).ok();
            let ev = lock(&events).clone();
            assert!(matches!(&ev[0], Event::Error { path, msg } if *path == p && msg.contains("sample rate")), "{ev:?}");
            assert!(!core.src.is_active());
        }
        assert!(make_rs(4_000_000_000).is_err() && make_rs(0).is_err() && make_rs(48_000).is_ok());
    }

    #[test]
    fn local_player_a_broken_next_file_does_not_cut_the_current_one() {
        let a = temp("whole.wav");
        let bad = temp("next-rate0.wav");
        write_wav(&a, 44_100, 2, 13_230, |_, _| 0.1); // 0.3 s, no resampler
        write_wav(&bad, 0, 1, 100, |_, _| 0.5); // panics symphonia's reader
        let (core, events) = test_core();
        Core::play(&core, &a, 0);
        core.send(Cmd::SetNext(Some(bad.clone())));
        let got = consume_until(&core, &events, |e| e.iter().any(|e| matches!(e, Event::Playing { playing: false })));
        let ev = lock(&events).clone();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&bad).ok();
        assert_eq!(got.len() / 2, 13_230, "the current file plays to its end: {ev:?}");
        assert_eq!(track_events(&ev), vec![(a.clone(), false)]);
        assert!(ev.iter().any(|e| matches!(e, Event::Error { path, .. } if *path == bad)), "the next file is blamed: {ev:?}");
    }

    #[test]
    fn local_player_short_file_takes_a_next_file_set_after_it_was_decoded() {
        let short = temp("short.wav");
        let b = temp("after.wav");
        write_wav(&short, 44_100, 2, 4_410, |_, _| 0.1); // 0.1 s: decoded at once
        write_wav(&b, 44_100, 2, 4_410, |_, _| 0.6);
        let (core, events) = test_core();
        Core::play(&core, &short, 0);
        // All of it is in the ring and the worker has seen its end, nothing heard yet.
        wait_pushed(&core, 4_410);
        std::thread::sleep(Duration::from_millis(50));
        core.send(Cmd::SetNext(Some(b.clone())));
        let got = consume_until(&core, &events, |e| e.iter().any(|e| matches!(e, Event::Playing { playing: false })));
        let ev = lock(&events).clone();
        std::fs::remove_file(&short).ok();
        std::fs::remove_file(&b).ok();
        assert_eq!(track_events(&ev), vec![(short.clone(), false), (b.clone(), true)], "{ev:?}");
        assert!(ev.iter().any(|e| matches!(e, Event::Ended)));
        assert_eq!(got.len() / 2, 8_820, "both files, whole");
    }

    #[test]
    fn local_player_a_replaced_play_sends_nothing_after_the_new_one_started() {
        let a = temp("first.wav");
        let b = temp("second.wav");
        write_wav(&a, 48_000, 2, 48_000, |_, _| 0.1);
        write_wav(&b, 44_100, 2, 44_100, |_, _| 0.6);
        for _ in 0..20 {
            let core = Arc::new(Core::new(Arc::new(LocalSource::new()), false));
            let events: Arc<Mutex<Vec<(u64, Event)>>> = Arc::default();
            let ev = Arc::clone(&events);
            *lock(&core.listener) = Some(Arc::new(move |g, e| lock(&ev).push((g, e))));
            let ga = Core::play(&core, &a, 0);
            let gb = Core::play(&core, &b, 0);
            assert!(gb > ga);
            let after = lock(&events).len();
            wait_for_tagged(&events, |e| e.iter().any(|(g, e)| *g == gb && matches!(e, Event::Playing { playing: true })));
            std::thread::sleep(Duration::from_millis(20));
            core.stop();
            let ev = lock(&events).clone();
            assert!(ev[after..].iter().all(|(g, _)| *g == gb), "the replaced play spoke after: {ev:?}");
            let last_track = ev.iter().rev().find_map(|(_, e)| if let Event::Track { path, .. } = e { Some(path) } else { None });
            assert_eq!(last_track, Some(&b));
        }
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    fn wait_for_tagged(events: &Mutex<Vec<(u64, Event)>>, pred: impl Fn(&[(u64, Event)]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pred(&lock(events)) {
            assert!(Instant::now() < deadline, "timed out; events: {:?}", lock(events));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Pretend to be the device: pull frames until `stop_when` holds (10 s at most).
    fn consume_until(core: &Core, events: &Mutex<Vec<Event>>, stop_when: impl Fn(&[Event]) -> bool) -> Vec<f32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            if stop_when(&lock(events)) {
                return got;
            }
            got.extend(core.src.pull(4096));
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("timed out; events: {:?}", lock(events));
    }

    fn wait_for(events: &Mutex<Vec<Event>>, pred: impl Fn(&[Event]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pred(&lock(events)) {
            assert!(Instant::now() < deadline, "timed out; events: {:?}", lock(events));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_pushed(core: &Core, at_least: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while core.src.counters().0 < at_least {
            assert!(Instant::now() < deadline, "the worker never filled the ring");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn local_player_decodes_and_resamples_48k_mono_to_44k1_stereo() {
        let p = temp("ramp48.wav");
        // 1 s of a slow ramp 0 -> 0.8, so alignment (resampler delay trimmed) is checkable.
        write_wav(&p, 48_000, 1, 48_000, |i, _| i as f32 / 48_000.0 * 0.8);
        let s = Source::open(&p).unwrap();
        assert_eq!(s.duration_ms, 1000);
        drop(s);
        let out = decode_all(&p);
        std::fs::remove_file(&p).ok();
        assert_eq!(out.len() % 2, 0);
        let frames = out.len() / 2;
        assert_eq!(frames, 44_100, "1 s at 48 kHz must become exactly 1 s at 44.1 kHz");
        // Mono -> both channels.
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
        // Midpoint of the output ~ midpoint of the ramp (no leading delay left in).
        let mid = out[(frames / 2) * 2];
        assert!((mid - 0.4).abs() < 0.01, "mid sample {mid}");
    }

    #[test]
    fn local_player_passes_44k1_stereo_through_exactly() {
        let p = temp("st441.wav");
        write_wav(&p, 44_100, 2, 1000, |i, c| if c == 0 { 0.25 } else { -(i as f32) / 2000.0 });
        let out = decode_all(&p);
        std::fs::remove_file(&p).ok();
        assert_eq!(out.len(), 2000);
        assert!((out[0] - 0.25).abs() < 1e-3);
        assert!((out[2 * 999 + 1] + 999.0 / 2000.0).abs() < 1e-3);
    }

    #[test]
    fn local_player_gapless_next_emits_track_then_ended_after_playback() {
        let a = temp("a.wav");
        let b = temp("b.wav");
        write_wav(&a, 48_000, 1, 14_400, |_, _| 0.1); // 0.3 s
        write_wav(&b, 22_050, 2, 6_615, |_, _| 0.2); // 0.3 s
        let (core, events) = test_core();
        Core::play(&core, &a, 0);
        // Queued before the worker reads its first command, so it's in time.
        core.send(Cmd::SetNext(Some(b.clone())));
        let got = consume_until(&core, &events, |e| e.iter().any(|e| matches!(e, Event::Playing { playing: false })));
        let ev = lock(&events).clone();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();

        let tracks: Vec<&PathBuf> =
            ev.iter().filter_map(|e| if let Event::Track { path, .. } = e { Some(path) } else { None }).collect();
        assert_eq!(tracks, vec![&a, &b], "events: {ev:?}");
        let pos_b = ev.iter().position(|e| matches!(e, Event::Track { path, .. } if *path == b)).unwrap();
        let pos_end = ev.iter().position(|e| matches!(e, Event::Ended)).expect("Ended");
        assert!(pos_b < pos_end);
        assert!(!ev.iter().any(|e| matches!(e, Event::Error { .. })));
        // Every sample of both files was played before Ended: 0.3 s + 0.3 s exactly.
        assert_eq!(got.len() / 2, 13_230 + 13_230);
        // No gap: a's level switches to b's in one step at the boundary (give or take
        // the resamplers' edge ringing).
        let first_b = got.chunks(2).position(|f| f[0] > 0.15).unwrap();
        assert!((13_200..=13_260).contains(&first_b), "b starts at frame {first_b}");
        assert!(!core.src.is_active(), "Ended releases the output");
    }

    #[test]
    fn local_player_seek_drops_buffered_audio() {
        let p = temp("ramp2s.wav");
        // 2 s at 44.1 kHz (no resampler), value = position in seconds / 2.
        write_wav(&p, 44_100, 2, 88_200, |i, _| i as f32 / 88_200.0);
        let (core, events) = test_core();
        Core::play(&core, &p, 0);
        // Let the ring fill with audio from the start, without consuming any.
        wait_pushed(&core, audio_output::LOCAL_CAP_FRAMES as u64);
        core.send(Cmd::Seek(1000));
        wait_for(&events, |e| e.iter().any(|e| matches!(e, Event::Position { ms } if *ms >= 900)));
        std::thread::sleep(Duration::from_millis(100)); // let it refill
        let first = core.src.pull(10);
        core.stop();
        std::fs::remove_file(&p).ok();
        assert_eq!(first.len(), 20);
        // Old buffered audio (values near 0) is gone; playback resumes near 0.5 (1 s).
        assert!((first[0] - 0.5).abs() < 0.02, "first sample after seek {}", first[0]);
        let ev = lock(&events).clone();
        let reached = ev.iter().rev().find_map(|e| if let Event::Position { ms } = e { Some(*ms) } else { None });
        assert!(reached.is_some_and(|ms| (980..=1000).contains(&ms)), "events: {ev:?}");
    }

    #[test]
    fn local_player_bad_file_reports_error_and_stops() {
        let p = temp("broken.mp3");
        std::fs::write(&p, b"this is not audio at all, just some text pretending").unwrap();
        let (core, events) = test_core();
        Core::play(&core, &p, 0);
        wait_for(&events, |e| matches!(e.last(), Some(Event::Playing { playing: false })));
        std::fs::remove_file(&p).ok();
        let ev = lock(&events).clone();
        assert!(matches!(&ev[0], Event::Error { path, .. } if *path == p), "events: {ev:?}");
        assert!(!core.src.is_active());
        // A missing file too.
        lock(&events).clear();
        Core::play(&core, &temp("does-not-exist.flac"), 0);
        wait_for(&events, |e| e.iter().any(|e| matches!(e, Event::Error { .. })));
        assert!(!core.src.is_active());
    }

    #[test]
    fn local_player_pause_stops_consumption_and_resume_continues() {
        let p = temp("pause.wav");
        write_wav(&p, 44_100, 2, 44_100, |_, _| 0.3);
        let (core, events) = test_core();
        Core::play(&core, &p, 0);
        wait_pushed(&core, 1);
        core.pause();
        assert!(core.src.pull(100).is_empty(), "paused: the device takes nothing");
        wait_for(&events, |e| matches!(e.last(), Some(Event::Playing { playing: false })));
        core.resume();
        assert_eq!(core.src.pull(100).len(), 200);
        wait_for(&events, |e| matches!(e.last(), Some(Event::Playing { playing: true }) | Some(Event::Position { .. })));
        core.stop();
        assert!(!core.src.is_active());
        wait_for(&events, |e| matches!(e.last(), Some(Event::Playing { playing: false })));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn local_player_replacing_play_cuts_old_audio() {
        let a = temp("old.wav");
        let b = temp("new.wav");
        write_wav(&a, 44_100, 2, 44_100, |_, _| 0.1);
        write_wav(&b, 44_100, 2, 44_100, |_, _| 0.6);
        let (core, events) = test_core();
        Core::play(&core, &a, 0);
        wait_pushed(&core, 1);
        Core::play(&core, &b, 0);
        wait_for(&events, |e| e.iter().filter(|e| matches!(e, Event::Track { .. })).count() == 2);
        std::thread::sleep(Duration::from_millis(100));
        let got = core.src.pull(4096);
        core.stop();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        assert!(!got.is_empty());
        assert!(got.iter().all(|&s| (s - 0.6).abs() < 1e-3), "old file's audio leaked in");
    }
}
