//! What Lightify itself played, for the RECENT panel.
//!
//! Spotify's recently-played list (`me/player/recently-played`) never hears about plays
//! through Lightify's own engine: librespot doesn't report playback to Spotify (librespot
//! 0.8 has no play-reporting at all). So the panel showed only what *other* devices had
//! played — checked 2026-10-31: after a day of listening in Lightify, the newest entry was
//! a day old. Lightify keeps its own list of what its engine played, and the panel shows
//! the two merged, newest first.
//!
//! A song is added **the moment it starts playing**, skips included (owner's call — no
//! waiting for a 30-second "play" as Spotify counts one), driven by our engine's own
//! events: a track loads, playing / paused. A track loaded paused (a resume offer) is
//! added when it first plays. Not from Web-API reads: those come only every 2 minutes
//! while our engine plays (the first version used them and recorded nothing). Local mode
//! keeps its own history the same way (`local_ui`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use lightify_core::Track;

/// The most plays kept on disk, and the most the panel shows.
const KEEP: usize = 100;
const SHOW: usize = 50;
/// A play of ours and Spotify's report of the same track this close together are one play.
const SAME_PLAY_SECS: i64 = 300;

/// Now, as Spotify writes `played_at` (UTC, RFC 3339), which `parse_rfc3339_utc` reads.
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    iso_from_unix(secs)
}

pub fn iso_from_unix(secs: i64) -> String {
    // civil_from_days (Howard Hinnant): the inverse of `parse_rfc3339_utc`'s day count.
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A history file, for a read-then-write: `Ok(None)` when there is none, `Err` when it
/// is there but can't be read now (it must not be written over: it holds plays that
/// would be lost). One that reads but doesn't parse is moved aside to `<name>.corrupt`
/// and counts as none (as the local library's state files do).
pub fn read_file<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> std::io::Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Ok(Some(v)),
        Err(_) => {
            let mut aside = path.as_os_str().to_owned();
            aside.push(".corrupt");
            std::fs::rename(path, &aside)?;
            Ok(None)
        }
    }
}

/// How long after a failed read the history file is tried again.
const RETRY_READ: Duration = Duration::from_secs(5);

struct State {
    /// Where the history lives: the data folder, or (tests) a throwaway file.
    file: Option<std::path::PathBuf>,
    /// The file has been read (or there was none). Until then plays are kept in memory
    /// only, and go on top of what it holds once it reads.
    loaded: bool,
    /// When reading it last failed.
    failed_at: Option<Instant>,
    /// Newest first; `added_at` is when it was played.
    plays: Vec<Track>,
    /// The track our engine has loaded, and how long it has been listened to.
    listen: Option<Listen>,
    /// Our engine is playing (not paused or stopped).
    playing: bool,
    changed: bool,
}

struct Listen {
    track: Track,
    loaded_at: Instant,
    /// Already in the history (it has played).
    added: bool,
}

/// A second "this track loaded" for the same track this soon is the same play (librespot
/// can name a track twice while loading it); later, it is a repeat.
const SAME_LOAD: Duration = Duration::from_secs(10);

static STATE: Mutex<State> = Mutex::new(State {
    file: None,
    loaded: false,
    failed_at: None,
    plays: Vec::new(),
    listen: None,
    playing: false,
    changed: false,
});

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    st.file.get_or_insert_with(|| lightify_core::config::data_dir().join("lightify_shell_history.json"));
    load(&mut st, Instant::now());
    f(&mut st)
}

/// Read the history file if it hasn't been (at most every `RETRY_READ` while it can't
/// be). Plays recorded meanwhile stay on top and are saved with what it held.
fn load(st: &mut State, now: Instant) {
    if st.loaded || st.failed_at.is_some_and(|t| now.saturating_duration_since(t) < RETRY_READ) {
        return;
    }
    let Some(file) = st.file.clone() else { return };
    match read_file::<Vec<Track>>(&file) {
        Ok(saved) => {
            st.loaded = true;
            st.failed_at = None;
            let had = !st.plays.is_empty();
            st.plays.extend(saved.unwrap_or_default());
            st.plays.truncate(KEEP);
            if had {
                st.changed = true;
                save(st);
            }
        }
        Err(e) => {
            if st.failed_at.is_none() {
                note(&format!("can't read the history file ({e}); not saving over it until it reads"));
            }
            st.failed_at = Some(now);
        }
    }
}

fn save(st: &State) {
    if !st.loaded {
        return;
    }
    if let (Some(file), Ok(b)) = (st.file.as_ref(), serde_json::to_vec(&st.plays)) {
        let _ = lightify_core::config::write_atomic(file, &b);
    }
}

/// Our engine loaded `t` (`engine::Event::Track`): added now if it's playing.
pub fn engine_track(t: Track) {
    with(|st| engine_track_at(st, t, Instant::now()))
}

/// Our engine started or stopped playing (`engine::Event::Playing`).
pub fn engine_playing(playing: bool) {
    with(|st| engine_playing_at(st, playing))
}

/// Playback left our engine (another device took over, or it failed).
pub fn engine_stopped() {
    with(|st| {
        st.playing = false;
        st.listen = None;
    })
}

fn engine_track_at(st: &mut State, t: Track, now: Instant) {
    if let Some(l) = st.listen.as_mut().filter(|l| l.track.uri == t.uri && now.saturating_duration_since(l.loaded_at) < SAME_LOAD) {
        l.track = t;
        return;
    }
    st.listen = Some(Listen { track: t, loaded_at: now, added: false });
    if st.playing {
        add_current(st);
    } else if let Some(l) = &st.listen {
        note(&format!("loaded \u{201c}{}\u{201d} paused \u{2014} added when it plays", l.track.name));
    }
}

/// The loaded track has started playing: into the history (once per load).
fn add_current(st: &mut State) {
    let Some(l) = st.listen.as_mut().filter(|l| !l.added) else { return };
    l.added = true;
    let t = l.track.clone();
    if st.plays.first().is_some_and(|p| p.uri == t.uri && p.added_at.as_deref().is_some_and(|at| continues(at, t.duration_ms))) {
        note(&format!("\u{201c}{}\u{201d} resumed \u{2014} already at the top", t.name));
        return;
    }
    note(&format!("added \u{201c}{}\u{201d}", t.name));
    record(st, t);
}

/// The same song at the top of a Recent list, added `at`, playing again now: is that the
/// same listen going on (Lightify reopened and resumed it) rather than a new play? Yes
/// while under 90 % of the song's length has passed — a repeat once it finished (which
/// lands about one length later) is a play.
pub fn continues(at: &str, dur_ms: u64) -> bool {
    let Some(then) = crate::parse_rfc3339_utc(at) else { return false };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    continues_at(then, now, dur_ms)
}

fn continues_at(then: i64, now: i64, dur_ms: u64) -> bool {
    let length = if dur_ms > 0 { (dur_ms / 1000) as i64 } else { 600 };
    (0..length * 9 / 10).contains(&(now - then))
}

/// What RECENT decided, into the engine's log with the time — so a play that didn't
/// show can be explained from the log alone.
fn note(msg: &str) {
    #[cfg(not(test))]
    crate::audio_engine::note(&format!("[recent {}] {msg}", now_iso()));
    #[cfg(test)]
    let _ = msg;
}

fn engine_playing_at(st: &mut State, playing: bool) {
    st.playing = playing;
    if playing {
        add_current(st);
    }
}

fn record(st: &mut State, mut t: Track) {
    t.added_at = Some(now_iso());
    st.plays.insert(0, t);
    st.plays.truncate(KEEP);
    st.changed = true;
    save(st);
}

/// Did the history gain a play since the last call?
pub fn take_changed() -> bool {
    with(|st| std::mem::take(&mut st.changed))
}

/// The RECENT panel's list: our plays and Spotify's (`spotify`, newest first), merged.
pub fn merged(spotify: &[Track]) -> Vec<Track> {
    with(|st| merge(&st.plays, spotify))
}

fn merge(ours: &[Track], spotify: &[Track]) -> Vec<Track> {
    let at = |t: &Track| t.added_at.as_deref().and_then(crate::parse_rfc3339_utc).unwrap_or(0);
    let mut all: Vec<Track> = ours.to_vec();
    for s in spotify {
        let dup = ours.iter().any(|o| o.uri == s.uri && (at(o) - at(s)).abs() <= SAME_PLAY_SECS);
        if !dup {
            all.push(s.clone());
        }
    }
    // Stable, so plays in the same second keep their order.
    all.sort_by_key(|t| std::cmp::Reverse(at(t)));
    all.truncate(SHOW);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(uri: &str, at: &str) -> Track {
        Track {
            id: uri.to_string(),
            name: uri.to_string(),
            artists: String::new(),
            artist_ids: Vec::new(),
            album: String::new(),
            album_id: String::new(),
            duration_ms: 200_000,
            uri: uri.to_string(),
            album_art: String::new(),
            added_at: Some(at.to_string()),
            is_playable: true,
        }
    }

    #[test]
    fn merging_sorts_dedupes_and_caps() {
        let ours = vec![track("t:new", "2026-10-31T12:00:00Z"), track("t:dup", "2026-10-31T11:00:00Z")];
        let spotify = vec![
            track("t:phone", "2026-10-31T11:30:00.123Z"),
            track("t:dup", "2026-10-31T11:02:00.000Z"),
            track("t:old", "2026-10-30T09:00:00.000Z"),
        ];
        let m: Vec<String> = merge(&ours, &spotify).into_iter().map(|t| t.uri).collect();
        assert_eq!(m, ["t:new", "t:phone", "t:dup", "t:old"]);
        let many: Vec<Track> = (0..80).map(|i| track(&format!("t:{i}"), "2026-10-31T10:00:00Z")).collect();
        assert_eq!(merge(&many, &[]).len(), SHOW);
    }

    /// The whole path, as the engine reports it: events in, history on disk and the
    /// panel's list out. A throwaway file, nothing process-wide; time is simulated.
    #[test]
    fn adds_each_song_the_moment_it_plays() {
        let dir = std::env::temp_dir().join(format!("lightify-history-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = State { file: Some(dir.join("lightify_shell_history.json")), loaded: true, failed_at: None, plays: Vec::new(), listen: None, playing: false, changed: false };
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let uris = |st: &State| st.plays.iter().map(|t| t.uri.clone()).collect::<Vec<_>>();

        // Loaded paused (a resume offer): not yet; it goes in when it plays.
        engine_track_at(&mut st, track("t:a", ""), at(0));
        assert!(uris(&st).is_empty());
        engine_playing_at(&mut st, true);
        assert_eq!(uris(&st), ["t:a"], "in as soon as it plays");
        // The same load named twice, and pause / play: still one entry.
        engine_track_at(&mut st, track("t:a", ""), at(1));
        engine_playing_at(&mut st, false);
        engine_playing_at(&mut st, true);
        assert_eq!(uris(&st), ["t:a"]);
        // Skipping through: every song that plays goes in at once, however briefly.
        engine_track_at(&mut st, track("t:b", ""), at(20));
        engine_track_at(&mut st, track("t:c", ""), at(21));
        assert_eq!(uris(&st), ["t:c", "t:b", "t:a"]);
        // Lightify reopened and resumed C (at the top, added just now): the same listen.
        engine_track_at(&mut st, track("t:c", ""), at(300));
        assert_eq!(uris(&st), ["t:c", "t:b", "t:a"], "a resume isn't a second play");
        // Repeat: C again once a whole song-length has passed is another play.
        st.plays[0].added_at = Some(iso_from_unix(crate::parse_rfc3339_utc(&now_iso()).unwrap() - 200));
        engine_track_at(&mut st, track("t:x", ""), at(600));
        engine_track_at(&mut st, track("t:c", ""), at(900));
        assert_eq!(uris(&st), ["t:c", "t:x", "t:c", "t:b", "t:a"], "after other songs, C is a new play");
        st.plays.remove(0);
        st.plays.remove(0);
        engine_track_at(&mut st, track("t:c", ""), at(1200));
        assert_eq!(uris(&st), ["t:c", "t:c", "t:b", "t:a"], "repeat after it finished");
        // Another device took over: what loads next, paused, waits until it plays.
        st.playing = false;
        st.listen = None;
        engine_track_at(&mut st, track("t:d", ""), at(400));
        assert_eq!(uris(&st).len(), 4);

        assert!(st.changed);
        let on_disk: Vec<Track> = serde_json::from_slice(&std::fs::read(dir.join("lightify_shell_history.json")).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 4);
        assert!(on_disk[0].added_at.as_deref().and_then(crate::parse_rfc3339_utc).is_some(), "stamped with when");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_history_file_that_cant_be_read_is_not_written_over() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("lightify-history-locked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lightify_shell_history.json");
        let old = vec![track("t:old1", "2026-10-30T09:00:00Z"), track("t:old2", "2026-10-30T08:00:00Z")];
        std::fs::write(&file, serde_json::to_vec(&old).unwrap()).unwrap();
        let before = std::fs::read(&file).unwrap();
        let mut st = State { file: Some(file.clone()), loaded: false, failed_at: None, plays: Vec::new(), listen: None, playing: true, changed: false };

        // Locked (another program, a backup tool): not read, and a play isn't saved over it.
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();
        let t0 = Instant::now();
        load(&mut st, t0);
        assert!(!st.loaded && st.failed_at.is_some());
        engine_track_at(&mut st, track("t:new", ""), t0);
        drop(lock);
        assert_eq!(std::fs::read(&file).unwrap(), before, "the file is as it was");
        assert_eq!(st.plays.len(), 1, "the play is kept meanwhile");

        // Not tried again at once; after the wait it reads, the play goes on top, saved.
        load(&mut st, t0 + Duration::from_secs(1));
        assert!(!st.loaded);
        load(&mut st, t0 + RETRY_READ);
        assert!(st.loaded);
        let uris = |v: &[Track]| v.iter().map(|t| t.uri.clone()).collect::<Vec<_>>();
        assert_eq!(uris(&st.plays), ["t:new", "t:old1", "t:old2"]);
        let on_disk: Vec<Track> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(uris(&on_disk), ["t:new", "t:old1", "t:old2"]);

        // One that doesn't parse is set aside, not lost.
        std::fs::write(&file, b"{ not json").unwrap();
        assert!(read_file::<Vec<Track>>(&file).unwrap().is_none());
        assert_eq!(std::fs::read(dir.join("lightify_shell_history.json.corrupt")).unwrap(), b"{ not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resume_continues_a_repeat_does_not() {
        // A 200 s song added at 1000.
        assert!(continues_at(1_000, 1_005, 200_000), "reopened 5 s later");
        assert!(continues_at(1_000, 1_170, 200_000), "170 s in: still the same listen");
        assert!(!continues_at(1_000, 1_195, 200_000), "about one length later: a repeat");
        assert!(!continues_at(1_000, 5_000, 200_000), "much later");
        assert!(!continues_at(1_000, 990, 200_000), "clock went backwards: not a resume");
        assert!(continues_at(1_000, 1_300, 0), "unknown length: 10 min window");
        assert!(!continues("not a date", 200_000));
    }

    #[test]
    fn known_dates_format_and_parse_back() {
        for (secs, iso) in [(0, "1970-01-01T00:00:00Z"), (951_782_400, "2000-02-29T00:00:00Z"), (1_793_473_200, "2026-10-31T19:00:00Z")] {
            assert_eq!(iso_from_unix(secs), iso);
            assert_eq!(crate::parse_rfc3339_utc(iso), Some(secs));
        }
    }

    #[test]
    fn timestamps_round_trip() {
        let s = now_iso();
        let parsed = crate::parse_rfc3339_utc(&s).expect("our own format parses");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((parsed - now).abs() <= 2, "{s} -> {parsed} vs {now}");
    }
}
