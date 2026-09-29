//! Native playback engine bridge — Lightify plays audio **itself**.
//!
//! The engine (librespot, wired up in `audio_engine.rs`) registers a Spotify
//! Connect endpoint named "Lightify" **in this same process** — one `.exe`, not
//! two. Starting it is what makes this a music player rather than a remote
//! control: without it the Web API has no device to target and every transport
//! call fails with "no active device", which reads to the user as "go open
//! Spotify" — exactly the dependency Lightify exists to avoid.
//!
//! (Until 2026-09-11 this spawned a separate `lightify-audio.exe` subprocess and
//! spoke JSON lines over its stdin/stdout — the shipped Tauri host still does
//! that, and `lightify-audio/` remains a real binary for its sake. The shell no
//! longer needs a second executable to ship: `librespot` is a normal Rust
//! library, so `audio_engine.rs` embeds the same wiring directly and runs it on
//! its own OS thread. This module's public API — `start`/`send`/`running`/
//! `shutdown`, the `Event` enum — is unchanged by that, so nothing else in
//! `main.rs` needed to change.)
//!
//! It authenticates from its own cached streaming credentials in
//! `<data_dir>/lightify-audio-cache` (Spotify's streaming stack needs keymaster
//! credentials that the Web-API token cannot provide — see the long comment in
//! `lightify-audio/src/main.rs`, which this was ported from). That cache is
//! **shared with the shipped Tauri host**, so if the user has ever signed in
//! there, this is silent: no browser, no second sign-in, and no Spotify app.
//!
//! Commands still travel as the same JSON-tagged strings the old subprocess
//! protocol used (`{"cmd":"repeat","mode":"track"}` etc.) — every call site in
//! `main.rs` already builds them that way, so `send()` keeps that shape and just
//! parses it into `audio_engine::Cmd` instead of writing it to a pipe.

use std::path::{Path, PathBuf};

use crate::audio_engine;

/// An engine notification, delivered straight from `audio_engine`'s own thread
/// via the `on_event` callback (no serialization involved any more — this used
/// to mirror `lightify-audio`'s `OutEvent` wire enum when the engine was a
/// separate process talking JSON over a pipe; the shape stuck around because
/// every UI-side match on it is unchanged, but nothing here is parsed today).
#[derive(Debug, Clone)]
pub enum Event {
    /// The Connect device registered; `device_id` is what `me/player` can target.
    Ready { device_id: String },
    Playing { playing: bool },
    Track { name: String, artists: String },
    Position { ms: u64 },
    Volume { percent: u16 },
    /// Shuffle and/or repeat ("off" | "context" | "track") as the device now has them.
    Options { shuffle: Option<bool>, repeat: Option<String> },
    /// The one-time keymaster sign-in is required (the engine opens the browser).
    NeedAuth { msg: String },
    Failed { msg: String },
    /// A seed's radio, resolved to plain track uris. Nothing was loaded or played —
    /// the host decides whether to queue them behind what's already on, or start them.
    ///
    /// `seq` is the epoch the request was made under (echoed back untouched) so the
    /// shell can tell an answer to the station the user still wants from one to a
    /// station they have since abandoned. See `STATION_EPOCH` in `main.rs`.
    StationTracks { uris: Vec<String>, seq: u64 },
    /// Audio is now coming out of this device. Fires on start and on every live
    /// switch (OS default changed, device unplugged, user picked another) — none of
    /// which restart the engine any more; see `audio_output`.
    OutputChanged { device: String },
    /// The engine thread has exited (cleanly or after a fatal error).
    Exited,
}

/// The engine's credential/volume cache — deliberately the **same** directory the
/// shipped host uses, so a sign-in done in either app serves both.
pub fn cache_dir() -> PathBuf {
    lightify_core::config::data_dir().join("lightify-audio-cache")
}

/// Historically "locate the `lightify-audio` subprocess binary" — the engine is
/// compiled into this executable now (see the module doc), so there is nothing
/// to locate. Kept as `Option<PathBuf>` returning this process's own path so the
/// handful of diagnostic call sites that print/branch on "binary found" (the
/// settings panel, `--probe-engine`, the self-tests) keep working unchanged.
pub fn find_binary() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// The machine's audio output devices, default first, deduped. Same enumeration the
/// shipped host does — the shell needs it because a device name that no longer exists
/// is not a soft error downstream: librespot's rodio backend calls `.unwrap()` on the
/// lookup and takes the whole engine down with `DeviceNotAvailable`, leaving the app
/// with no Connect device and every transport call 404-ing.
pub fn audio_outputs() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    let mut names: Vec<String> = match host.output_devices() {
        Ok(it) => it.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names.dedup();
    if !default_name.is_empty() {
        names.retain(|n| n != &default_name);
        names.insert(0, default_name);
    }
    names
}

/// What to actually pass as `--device`, given the configured preference.
///
/// `Ok(None)` means "let the engine pick the system default". `Err(name)` means the
/// configured device is gone (unplugged headphones, a disconnected interface) — the
/// caller should fall back to the default and tell the user why, never hand the name
/// to the engine.
pub fn resolve_output(configured: &str) -> Result<Option<String>, String> {
    let want = configured.trim();
    if want.is_empty() || want.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    let outputs = audio_outputs();
    if outputs.iter().any(|n| n == want) {
        Ok(Some(want.to_string()))
    } else {
        Err(want.to_string())
    }
}

/// True when a previous streaming sign-in is cached, so `spawn` will be silent.
/// (`STREAM_AUTH_MARKER` in `lightify-audio` — its absence means the engine will
/// open a browser once.)
pub fn has_cached_login() -> bool {
    let dir = cache_dir();
    dir.join(".lightify_stream_oauth_v1").exists() && dir.join("credentials.json").exists()
}

/// The one engine this process owns. Global (rather than worker-local) for one
/// reason: the Slint event loop returns straight to `main` on quit, which never
/// unwinds the worker thread — so `main` must be able to stop the child itself and
/// not leave a stray "Lightify" device advertised on the user's account.
static ENGINE: std::sync::OnceLock<std::sync::Mutex<Option<Engine>>> = std::sync::OnceLock::new();

fn slot() -> &'static std::sync::Mutex<Option<Engine>> {
    ENGINE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Which `start` call the current engine belongs to. Bumped by every `start` (and
/// by `shutdown`) BEFORE the previous engine is stopped, so that engine's own
/// teardown events — its `Exited` above all — arrive already stale. Without this,
/// Settings → Restart had the old engine's `Exited` land after the new engine was
/// up, and `main.rs`'s `Exited` handler read it as a crash and restarted again,
/// whose replacement's `Exited` did the same: a potential endless restart loop.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Spawn the engine and park it in the global slot, replacing any previous one.
/// `bin` is accepted only for call-site compatibility (see `find_binary`) and
/// ignored — there is no separate binary to launch any more.
pub fn start<F>(_bin: &Path, token: &str, audio_output: Option<&str>, on_event: F) -> Result<(), String>
where
    F: Fn(Event) + Send + Sync + 'static,
{
    use std::sync::atomic::Ordering;
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    // Every event from a superseded engine is dropped, not just `Exited`/`Ready`:
    // its `Playing { false }`, `Failed`, etc. describe an engine the UI no longer
    // has, and would only overwrite what the new one reports.
    let on_event = move |ev: Event| {
        if GENERATION.load(Ordering::SeqCst) == generation {
            on_event(ev);
        }
    };
    // Stop the old engine BEFORE spawning the new one. Overlapping them registered
    // two "Lightify" Connect devices at once, and an old engine still waiting on the
    // streaming sign-in held its callback port, so the new one's sign-in couldn't
    // bind it. Taken out under the lock but stopped after releasing it: `stop()`
    // waits up to a few seconds for a clean Spotify disconnect, and `send`/`running`
    // (called from the UI side) would block on this lock that long.
    let old = match slot().lock() {
        Ok(mut g) => g.take(),
        Err(_) => None,
    };
    if let Some(mut o) = old {
        o.stop();
    }
    let e = Engine::spawn(token, audio_output, on_event);
    let raced = match slot().lock() {
        Ok(mut g) => g.replace(e),
        Err(_) => None,
    };
    drop(raced); // only if another `start` slipped in between; dropping stops it
    Ok(())
}

/// Forward a JSON command line to the running engine; no-op when it isn't
/// running or the line doesn't parse (the latter would indicate a bug at the
/// call site — every caller in `main.rs` builds these with `serde_json::json!`).
pub fn send(line: &str) {
    let Ok(cmd) = serde_json::from_str::<audio_engine::Cmd>(line) else { return };
    if let Ok(mut g) = slot().lock() {
        if let Some(e) = g.as_mut() {
            e.send(cmd);
        }
    }
}

pub fn running() -> bool {
    match slot().lock() {
        Ok(mut g) => g.as_mut().map(|e| e.is_running()).unwrap_or(false),
        Err(_) => false,
    }
}

/// Stop the engine. Safe to call when nothing is running; call it on the way out.
pub fn shutdown() {
    // A deliberate stop, not a crash: retire its generation first so its `Exited`
    // can't reach `main.rs`'s restart-on-exit handling (see `GENERATION`).
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let old = match slot().lock() {
        Ok(mut g) => g.take(),
        Err(_) => None,
    };
    if let Some(mut e) = old {
        e.stop();
    }
}

/// The one engine this process owns, wrapping the in-process handle from
/// `audio_engine`. Dropping it stops the engine.
pub struct Engine {
    handle: audio_engine::Handle,
}

impl Engine {
    /// Start the engine on its own thread. `token` is accepted for call-site
    /// compatibility (every caller passes `session.access_token()`) but unused —
    /// it was always vestigial here even in the subprocess days (streaming auth
    /// uses the separate keymaster OAuth in `audio_engine.rs`, never the Web-API
    /// token); there is no longer a process boundary to hand it across either way.
    pub fn spawn<F>(token: &str, audio_output: Option<&str>, on_event: F) -> Self
    where
        F: Fn(Event) + Send + Sync + 'static,
    {
        let device = audio_output.and_then(|d| {
            let d = d.trim();
            (!d.is_empty() && !d.eq_ignore_ascii_case("default")).then(|| d.to_string())
        });
        let handle = audio_engine::spawn(cache_dir(), device, token, on_event);
        Engine { handle }
    }

    /// Forward a parsed command. Used for the controls the Web API can't
    /// reliably apply to the local device — repeat above all, since librespot's
    /// spirc owns track advancement.
    pub fn send(&mut self, cmd: audio_engine::Cmd) {
        self.handle.send(cmd);
    }

    pub fn is_running(&mut self) -> bool {
        self.handle.is_running()
    }

    /// Ask the engine to quit, then make sure it is gone (bounded wait — see
    /// `audio_engine::Handle::stop` for why this can't be an unbounded join).
    pub fn stop(&mut self) {
        self.handle.stop();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_command_wire_format() {
        // The exact JSON shapes `main.rs` sends via `engine::send` — this is the
        // wire format callers depend on, now parsed straight into `audio_engine::Cmd`
        // instead of being written to a pipe.
        assert!(matches!(
            serde_json::from_str::<audio_engine::Cmd>(r#"{"cmd":"repeat","mode":"track"}"#),
            Ok(audio_engine::Cmd::Repeat { ref mode }) if mode == "track"
        ));
        assert!(matches!(
            serde_json::from_str::<audio_engine::Cmd>(r#"{"cmd":"autoplay","enabled":true}"#),
            Ok(audio_engine::Cmd::Autoplay { enabled: true })
        ));
        // A station request carries its epoch; one without (older callers) is 0.
        assert!(matches!(
            serde_json::from_str::<audio_engine::Cmd>(r#"{"cmd":"stationtracks","context_uri":"spotify:track:x"}"#),
            Ok(audio_engine::Cmd::StationTracks { seq: 0, .. })
        ));
        assert!(matches!(
            serde_json::from_str::<audio_engine::Cmd>(
                r#"{"cmd":"stationtracks","context_uri":"spotify:track:x","seq":7}"#
            ),
            Ok(audio_engine::Cmd::StationTracks { seq: 7, .. })
        ));
        // Bare clearqueue = the hard reset (what ending a station wants).
        assert!(matches!(
            serde_json::from_str::<audio_engine::Cmd>(r#"{"cmd":"clearqueue"}"#),
            Ok(audio_engine::Cmd::ClearQueue { ref keep_uri, .. }) if keep_uri.is_empty()
        ));
        // ...and with a track to keep, the variant that empties the queue WITHOUT
        // stopping what is playing. The defaults matter: a missing field here would
        // silently fall back to the hard reset and skip the user's track.
        assert!(matches!(
            serde_json::from_str::<audio_engine::Cmd>(
                r#"{"cmd":"clearqueue","keep_uri":"spotify:track:x","position_ms":4200,"playing":true}"#
            ),
            Ok(audio_engine::Cmd::ClearQueue { ref keep_uri, position_ms: 4200, playing: true })
                if keep_uri == "spotify:track:x"
        ));
        assert!(matches!(
            serde_json::from_str::<audio_engine::Cmd>(r#"{"cmd":"expect","ids":["a","b"]}"#),
            Ok(audio_engine::Cmd::Expect { ref ids }) if ids == &["a".to_string(), "b".to_string()]
        ));
        assert!(serde_json::from_str::<audio_engine::Cmd>("not json").is_err());
        assert!(serde_json::from_str::<audio_engine::Cmd>(r#"{"cmd":"whatever"}"#).is_err());
    }
}
