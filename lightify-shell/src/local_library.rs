//! Local library: music files on this computer — which folders are watched, what is in
//! them (tags, duration), the on-disk cache that makes rescans incremental, local
//! playlists, and "find music on this PC".
//!
//! CONTRACT (the UI and worker in main.rs are written against exactly this API):
//! everything here is synchronous and may be slow (disk); callers run it off the UI
//! thread. Persistent state lives in `lightify_core::config::data_dir()`:
//!   lightify_shell_local_folders.json   — watched folders + loose files
//!   lightify_shell_local_index.json     — the scan cache (path → tags, size, mtime)
//!   lightify_shell_local_playlists.json — local playlists

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One audio file in the library.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LocalTrack {
    pub path: PathBuf,
    /// From tags; the file stem when there is no title tag.
    pub title: String,
    /// From tags; "Unknown artist" when absent.
    pub artist: String,
    /// From tags; "Unknown album" when absent.
    pub album: String,
    /// Album artist when tagged, else `artist`.
    pub album_artist: String,
    /// From tags; empty when absent.
    pub genre: String,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub duration_ms: u64,
    /// When this file first entered the library (unix seconds) — "Recently added".
    pub added_at: u64,
}

/// What the library is made of.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sources {
    /// Folders scanned recursively.
    pub folders: Vec<PathBuf>,
    /// Individual files added outside those folders (a drop, "Add files…").
    pub files: Vec<PathBuf>,
}

/// A local playlist (ordered paths; a path may repeat).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LocalPlaylist {
    pub id: String,
    pub name: String,
    pub tracks: Vec<PathBuf>,
}

/// A folder "Find music on this PC" suggests.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Short label ("Music", "Downloads", "OneDrive — Music", "D:\\Audio").
    pub name: String,
    pub path: PathBuf,
    /// Audio files found under it (counting may stop at a cap; see `capped`).
    pub count: usize,
    pub capped: bool,
    /// Already one of `Sources::folders` (or inside one).
    pub already_added: bool,
}

/// Result of adding dropped / picked paths.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddResult {
    pub audio_files: usize,
    pub folders: usize,
    /// Paths that are neither a folder nor an audio file (skipped).
    pub skipped: usize,
}

// ── State files ─────────────────────────────────────────────────────────────

const FOLDERS_FILE: &str = "lightify_shell_local_folders.json";
const INDEX_FILE: &str = "lightify_shell_local_index.json";
const PLAYLISTS_FILE: &str = "lightify_shell_local_playlists.json";

const UNKNOWN_ARTIST: &str = "Unknown artist";
const UNKNOWN_ALBUM: &str = "Unknown album";

/// Serialises read-modify-write of the state files (the UI and the worker may both
/// edit sources or playlists).
static STATE_LOCK: Mutex<()> = Mutex::new(());

fn state_lock() -> std::sync::MutexGuard<'static, ()> {
    STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
thread_local! {
    /// Tests point the state files here so they never touch the real data dir.
    static TEST_STATE_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    /// Stand-in for the downloader's folder in tests (`None` = no such folder).
    static TEST_DOWNLOAD_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn state_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(d) = TEST_STATE_DIR.with(|d| d.borrow().clone()) {
            return d;
        }
        // A test that didn't set a dir still must not write into the user's data.
        let d = std::env::temp_dir().join(format!("lightify-local-library-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        d
    }
    #[cfg(not(test))]
    {
        lightify_core::config::data_dir()
    }
}

/// The downloader's folder (configured `download_path`, else its default), resolved
/// exactly the way the downloader does it.
fn downloader_folder() -> Option<PathBuf> {
    #[cfg(test)]
    {
        TEST_DOWNLOAD_DIR.with(|d| d.borrow().clone())
    }
    #[cfg(not(test))]
    {
        let p = crate::downloader::download_path();
        (!p.trim().is_empty()).then(|| PathBuf::from(p))
    }
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    let bytes = fs::read(state_dir().join(name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json<T: serde::Serialize>(name: &str, value: &T, pretty: bool) {
    let bytes = if pretty { serde_json::to_vec_pretty(value) } else { serde_json::to_vec(value) };
    if let Ok(bytes) = bytes {
        let _ = lightify_core::config::write_atomic(&state_dir().join(name), &bytes);
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ── Paths ───────────────────────────────────────────────────────────────────

/// Path components for comparison (case-insensitive on Windows, like the file system).
fn comps(p: &Path) -> Vec<String> {
    p.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            if cfg!(windows) {
                s.to_lowercase()
            } else {
                s.into_owned()
            }
        })
        .collect()
}

/// `child` is `parent` or somewhere below it.
fn same_or_inside(child: &Path, parent: &Path) -> bool {
    let (c, p) = (comps(child), comps(parent));
    !p.is_empty() && c.len() >= p.len() && c[..p.len()] == p[..]
}

fn inside_any(path: &Path, folders: &[PathBuf]) -> bool {
    folders.iter().any(|f| same_or_inside(path, f))
}

/// Cache / dedup key for a file path.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── Formats ─────────────────────────────────────────────────────────────────

/// Is this a file the local player can play (by extension — exactly the formats the
/// decoder build supports)?
pub fn is_audio(path: &Path) -> bool {
    // symphonia is built with mp1/mp2/mp3, flac, vorbis+ogg, wav/aiff/caf with pcm and
    // adpcm, and isomp4 with aac and alac (plus the ADTS reader for raw .aac). There is
    // no opus or wma decoder, so .opus / .wma are not playable.
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_ascii_lowercase().as_str(),
            "mp3" | "mp2" | "mp1" | "flac" | "ogg" | "oga" | "wav" | "aif" | "aiff" | "aifc" | "caf" | "m4a" | "m4b" | "aac"
        ),
        None => false,
    }
}

// ── Sources ─────────────────────────────────────────────────────────────────

/// The library's sources. First run: the downloader's folder
/// (`lightify_core::config` `download_path`, or the default downloads folder the
/// downloader uses) when it exists.
pub fn sources() -> Sources {
    if let Some(s) = read_json::<Sources>(FOLDERS_FILE) {
        return s;
    }
    let mut s = Sources::default();
    if let Some(dl) = downloader_folder() {
        if dl.is_dir() {
            s.folders.push(dl);
        }
    }
    s
}

pub fn set_sources(s: &Sources) {
    let _guard = state_lock();
    write_json(FOLDERS_FILE, s, true);
}

/// Add dropped / picked paths: folders join `Sources::folders` (unless already inside
/// one), audio files outside every folder join `Sources::files`; everything else is
/// counted as skipped. Persists. Does not scan — call `scan` afterwards.
pub fn add_paths(paths: &[PathBuf]) -> AddResult {
    let _guard = state_lock();
    let mut s = sources();
    let mut r = AddResult::default();
    for p in paths {
        let p = absolute(p);
        let Ok(meta) = fs::metadata(&p) else {
            r.skipped += 1;
            continue;
        };
        if meta.is_dir() {
            r.folders += 1;
            if inside_any(&p, &s.folders) {
                continue;
            }
            // A new parent makes folders and loose files below it redundant.
            s.folders.retain(|f| !same_or_inside(f, &p));
            s.files.retain(|f| !same_or_inside(f, &p));
            s.folders.push(p);
        } else if meta.is_file() && is_audio(&p) {
            r.audio_files += 1;
            let key = path_key(&p);
            if inside_any(&p, &s.folders) || s.files.iter().any(|f| path_key(f) == key) {
                continue;
            }
            s.files.push(p);
        } else {
            r.skipped += 1;
        }
    }
    write_json(FOLDERS_FILE, &s, true);
    r
}

/// Stop watching a folder (its tracks leave the library on the next scan; files are
/// never touched on disk).
pub fn remove_folder(path: &Path) {
    let _guard = state_lock();
    let mut s = sources();
    let target = comps(path);
    s.folders.retain(|f| comps(f) != target);
    write_json(FOLDERS_FILE, &s, true);
}

// ── Walking ─────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn attrs(meta: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}

#[cfg(not(windows))]
fn attrs(_meta: &fs::Metadata) -> u32 {
    0
}

const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x40000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

/// A cloud placeholder (OneDrive "online-only"): opening it would download it.
fn is_cloud_only(meta: &fs::Metadata) -> bool {
    attrs(meta) & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0
}

enum WalkEnd {
    Complete,
    /// The visitor asked to stop, or the deadline passed.
    Stopped,
}

/// Depth-first walk of `root` calling `on_file` for each audio file (with its
/// metadata, following file links). Skips dot-names and hidden/system folders; follows
/// folder links/junctions but never enters the same real folder twice (`visited`
/// holds canonical paths), so link loops end. `on_file` returns `false` to stop.
fn walk_audio(
    root: &Path,
    visited: &mut HashSet<PathBuf>,
    deadline: Option<Instant>,
    on_file: &mut dyn FnMut(PathBuf, &fs::Metadata) -> bool,
) -> WalkEnd {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return WalkEnd::Stopped;
        }
        let canon = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !visited.insert(canon) {
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut subdirs = Vec::new();
        for entry in rd.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_symlink() {
                // Link or junction: look at the target.
                match fs::metadata(&path) {
                    Ok(m) if m.is_dir() => {
                        if let Ok(own) = entry.metadata() {
                            if attrs(&own) & (ATTR_HIDDEN | ATTR_SYSTEM) != 0 {
                                continue;
                            }
                        }
                        subdirs.push(path);
                    }
                    Ok(m) if m.is_file() && is_audio(&path) => {
                        if !on_file(path, &m) {
                            return WalkEnd::Stopped;
                        }
                    }
                    _ => {}
                }
            } else if ft.is_dir() {
                let Ok(meta) = entry.metadata() else { continue };
                if attrs(&meta) & (ATTR_HIDDEN | ATTR_SYSTEM) != 0 {
                    continue;
                }
                subdirs.push(path);
            } else if ft.is_file() && is_audio(&path) {
                let Ok(meta) = entry.metadata() else { continue };
                if !on_file(path, &meta) {
                    return WalkEnd::Stopped;
                }
            }
        }
        // Reverse so the stack pops them in directory order.
        stack.extend(subdirs.into_iter().rev());
    }
    WalkEnd::Complete
}

// ── Tags ────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Tags {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    track_no: Option<u32>,
    disc_no: Option<u32>,
    duration_ms: u64,
}

fn probe(path: &Path) -> Option<symphonia::core::probe::ProbeResult> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let fmt_opts = FormatOptions { enable_gapless: false, ..Default::default() };
    symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &MetadataOptions::default())
        .ok()
}

fn clean(v: &symphonia::core::meta::Value) -> Option<String> {
    let s = v.to_string();
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!s.is_empty()).then(|| s.to_string())
}

/// "3", "03", "3/12" → 3. Zero counts as absent.
fn leading_number(s: &str) -> Option<u32> {
    let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok().filter(|n| *n > 0)
}

fn apply_tags(out: &mut Tags, rev: &symphonia::core::meta::MetadataRevision) {
    use symphonia::core::meta::StandardTagKey as K;
    for tag in rev.tags() {
        let Some(key) = tag.std_key else { continue };
        let Some(val) = clean(&tag.value) else { continue };
        let slot = match key {
            K::TrackTitle => &mut out.title,
            K::Artist => &mut out.artist,
            K::Album => &mut out.album,
            K::AlbumArtist => &mut out.album_artist,
            K::Genre => &mut out.genre,
            K::TrackNumber => {
                if out.track_no.is_none() {
                    out.track_no = leading_number(&val);
                }
                continue;
            }
            K::DiscNumber => {
                if out.disc_no.is_none() {
                    out.disc_no = leading_number(&val);
                }
                continue;
            }
            _ => continue,
        };
        // First source wins (ID3v2 ahead of container tags).
        if slot.is_none() {
            *slot = Some(val);
        }
    }
}

/// Tags + duration from headers only (no audio is decoded). `None` when the file
/// can't be opened or isn't a format symphonia recognises.
fn read_tags(path: &Path) -> Option<Tags> {
    let mut probed = probe(path)?;
    let mut tags = Tags::default();
    if let Some(mut m) = probed.metadata.get() {
        if let Some(rev) = m.skip_to_latest() {
            apply_tags(&mut tags, rev);
        }
    }
    if let Some(rev) = probed.format.metadata().skip_to_latest() {
        apply_tags(&mut tags, rev);
    }
    let track = probed.format.default_track().or_else(|| probed.format.tracks().first());
    if let Some(track) = track {
        let p = &track.codec_params;
        if let Some(n) = p.n_frames {
            tags.duration_ms = if let Some(tb) = p.time_base {
                let t = tb.calc_time(n);
                t.seconds.saturating_mul(1000) + (t.frac * 1000.0) as u64
            } else if let Some(sr) = p.sample_rate.filter(|r| *r > 0) {
                n.saturating_mul(1000) / u64::from(sr)
            } else {
                0
            };
        }
    }
    Some(tags)
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn track_from(path: &Path, tags: Tags, added_at: u64) -> LocalTrack {
    let artist = tags.artist.unwrap_or_else(|| UNKNOWN_ARTIST.to_string());
    LocalTrack {
        path: path.to_path_buf(),
        title: tags.title.unwrap_or_else(|| stem(path)),
        album: tags.album.unwrap_or_else(|| UNKNOWN_ALBUM.to_string()),
        album_artist: tags.album_artist.unwrap_or_else(|| artist.clone()),
        artist,
        genre: tags.genre.unwrap_or_default(),
        track_no: tags.track_no,
        disc_no: tags.disc_no,
        duration_ms: tags.duration_ms,
        added_at,
    }
}

// ── Scan cache ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    track: LocalTrack,
    size: u64,
    /// Modification time, nanoseconds since the unix epoch.
    mtime: u64,
    /// Not opened yet (a cloud-only placeholder): re-read once it is local.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    partial: bool,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct IndexFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: Vec<CacheEntry>,
}

fn load_index() -> Vec<CacheEntry> {
    read_json::<IndexFile>(INDEX_FILE).map(|i| i.entries).unwrap_or_default()
}

fn mtime_nanos(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn sort_tracks(tracks: &mut [LocalTrack]) {
    tracks.sort_by_cached_key(|t| {
        (
            t.album_artist.to_lowercase(),
            t.album.to_lowercase(),
            t.disc_no.unwrap_or(1),
            t.track_no.unwrap_or(0),
            t.title.to_lowercase(),
            path_key(&t.path),
        )
    });
}

/// The library as last scanned, straight from the cache (instant; empty on first run).
pub fn cached() -> Vec<LocalTrack> {
    let mut tracks: Vec<LocalTrack> = load_index().into_iter().map(|e| e.track).collect();
    sort_tracks(&mut tracks);
    tracks
}

/// A file that needs opening during a scan.
struct Job {
    path: PathBuf,
    size: u64,
    mtime: u64,
    added_at: u64,
    cloud_only: bool,
}

fn run_job(job: &Job) -> CacheEntry {
    let tags = if job.cloud_only {
        None
    } else {
        // A malformed file must not take the scan down with it.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_tags(&job.path))).ok().flatten()
    };
    CacheEntry {
        track: track_from(&job.path, tags.unwrap_or_default(), job.added_at),
        size: job.size,
        mtime: job.mtime,
        partial: job.cloud_only,
    }
}

/// Opens every job's file (in parallel when there are many) and returns the entries;
/// `tick` runs on the calling thread once per finished job.
fn run_jobs(jobs: &[Job], tick: &mut dyn FnMut()) -> Vec<CacheEntry> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 8);
    if threads <= 1 || jobs.len() < 16 {
        return jobs
            .iter()
            .map(|j| {
                let e = run_job(j);
                tick();
                e
            })
            .collect();
    }
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..threads.min(jobs.len()) {
            let tx = tx.clone();
            let next = &next;
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(i) else { break };
                if tx.send(run_job(job)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        let mut out = Vec::with_capacity(jobs.len());
        for e in rx {
            out.push(e);
            tick();
        }
        out
    })
}

/// How often `scan` reports progress (files).
const PROGRESS_EVERY: usize = 100;

/// `scan`, also returning how many files were opened (for tests).
fn scan_counted(progress: &dyn Fn(usize)) -> (Vec<LocalTrack>, usize) {
    let src = sources();
    let mut old: HashMap<String, CacheEntry> =
        load_index().into_iter().map(|e| (path_key(&e.track.path), e)).collect();
    let now = now_secs();

    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<CacheEntry> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    let mut done = 0usize;
    let mut tick = || {
        done += 1;
        if done % PROGRESS_EVERY == 0 {
            progress(done);
        }
    };

    let mut consider = |path: PathBuf, meta: &fs::Metadata, tick: &mut dyn FnMut()| {
        let key = path_key(&path);
        if !seen.insert(key.clone()) {
            return;
        }
        let size = meta.len();
        let mtime = mtime_nanos(meta);
        let cloud_only = is_cloud_only(meta);
        if let Some(e) = old.remove(&key) {
            let unchanged = e.size == size && e.mtime == mtime && !(e.partial && !cloud_only);
            if unchanged {
                kept.push(e);
                tick();
                return;
            }
            jobs.push(Job { path, size, mtime, added_at: e.track.added_at, cloud_only });
        } else {
            jobs.push(Job { path, size, mtime, added_at: now, cloud_only });
        }
    };

    let mut visited = HashSet::new();
    for folder in &src.folders {
        walk_audio(folder, &mut visited, None, &mut |p, m| {
            consider(p, m, &mut tick);
            true
        });
    }
    for file in &src.files {
        if let Ok(meta) = fs::metadata(file) {
            if meta.is_file() && is_audio(file) {
                consider(file.clone(), &meta, &mut tick);
            }
        }
    }

    let opened = jobs.iter().filter(|j| !j.cloud_only).count();
    let fresh = run_jobs(&jobs, &mut tick);
    kept.extend(fresh);
    progress(done);

    kept.sort_by_cached_key(|e| path_key(&e.track.path));
    let mut tracks: Vec<LocalTrack> = kept.iter().map(|e| e.track.clone()).collect();
    sort_tracks(&mut tracks);
    write_json(INDEX_FILE, &IndexFile { version: 1, entries: kept }, false);
    (tracks, opened)
}

/// Walk every source and return the library. Incremental: a file whose size and mtime
/// match the cache keeps its cached tags and `added_at` without being opened. Updates
/// the cache. `progress(done)` is called every so often with files looked at so far.
/// Sorted by album artist, album, disc, track number, title.
pub fn scan(progress: &dyn Fn(usize)) -> Vec<LocalTrack> {
    scan_counted(progress).0
}

// ── Cover art ───────────────────────────────────────────────────────────────

fn embedded_cover(path: &Path) -> Option<Vec<u8>> {
    use symphonia::core::meta::{StandardVisualKey, Visual};
    let mut probed = probe(path)?;
    let mut visuals: Vec<Visual> = Vec::new();
    if let Some(mut m) = probed.metadata.get() {
        if let Some(rev) = m.skip_to_latest() {
            visuals.extend(rev.visuals().iter().cloned());
        }
    }
    if let Some(rev) = probed.format.metadata().skip_to_latest() {
        visuals.extend(rev.visuals().iter().cloned());
    }
    let pick = visuals
        .iter()
        .find(|v| v.usage == Some(StandardVisualKey::FrontCover) && !v.data.is_empty())
        .or_else(|| visuals.iter().find(|v| !v.data.is_empty()))?;
    Some(pick.data.to_vec())
}

fn folder_cover(path: &Path) -> Option<Vec<u8>> {
    const STEMS: [&str; 3] = ["cover", "folder", "front"];
    const EXTS: [&str; 3] = ["jpg", "jpeg", "png"];
    let dir = path.parent()?;
    let mut best: Option<(usize, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        let (Some(s), Some(e)) = (p.file_stem(), p.extension()) else { continue };
        let (s, e) = (s.to_string_lossy().to_lowercase(), e.to_string_lossy().to_lowercase());
        let (Some(si), Some(ei)) = (STEMS.iter().position(|x| *x == s), EXTS.iter().position(|x| *x == e))
        else {
            continue;
        };
        let rank = si * EXTS.len() + ei;
        if best.as_ref().is_none_or(|(r, _)| rank < *r) && p.is_file() {
            best = Some((rank, p));
        }
    }
    fs::read(best?.1).ok().filter(|b| !b.is_empty())
}

/// Encoded cover image for a track: embedded art first, else cover/folder/front
/// .jpg/.png beside it. `None` when there is none.
pub fn cover(path: &Path) -> Option<Vec<u8>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| embedded_cover(path)))
        .ok()
        .flatten()
        .or_else(|| folder_cover(path))
}

// ── Find music on this PC ───────────────────────────────────────────────────

/// Stop counting a candidate folder after this many audio files.
const CANDIDATE_CAP: usize = 5000;
/// Total time `find_candidates` may spend counting.
const CANDIDATE_BUDGET: Duration = Duration::from_secs(4);

#[cfg(windows)]
fn fixed_drive_roots() -> Vec<PathBuf> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    const DRIVE_FIXED: u32 = 3;
    // SAFETY: no arguments; returns a bitmask of drive letters.
    let mask = unsafe { GetLogicalDrives() };
    (0u8..26)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let root = format!("{}:\\", (b'A' + i) as char);
            let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
            let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };
            (kind == DRIVE_FIXED).then(|| PathBuf::from(root))
        })
        .collect()
}

#[cfg(not(windows))]
fn fixed_drive_roots() -> Vec<PathBuf> {
    Vec::new()
}

fn candidate_folders() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    if let Some(d) = dirs::audio_dir() {
        out.push(("Music".into(), d));
    }
    if let Some(d) = dirs::download_dir() {
        out.push(("Downloads".into(), d));
    }
    if let Some(d) = dirs::desktop_dir() {
        out.push(("Desktop".into(), d));
    }
    let onedrive = std::env::var_os("OneDrive")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|h| h.join("OneDrive")));
    if let Some(od) = onedrive {
        out.push(("OneDrive — Music".into(), od.join("Music")));
    }
    if let Some(d) = downloader_folder() {
        out.push(("Lightify downloads".into(), d));
    }
    for root in fixed_drive_roots() {
        for sub in ["Music", "Audio"] {
            let p = root.join(sub);
            out.push((p.to_string_lossy().into_owned(), p));
        }
    }
    // Same folder reached two ways (e.g. Music redirected into OneDrive): keep the first.
    let mut seen = HashSet::new();
    out.retain(|(_, p)| seen.insert(comps(p)) && p.is_dir());
    out
}

/// Folders worth suggesting, with how many audio files each holds: the user's Music,
/// Downloads, Desktop, OneDrive\Music, the downloader's folder, and a Music/Audio
/// folder at the root of other fixed drives. Bounded: stops counting a folder at a
/// cap and never takes more than a few seconds in all. Folders with no audio left out.
pub fn find_candidates() -> Vec<Candidate> {
    find_candidates_in(candidate_folders(), CANDIDATE_BUDGET, CANDIDATE_CAP)
}

fn find_candidates_in(folders: Vec<(String, PathBuf)>, budget: Duration, cap: usize) -> Vec<Candidate> {
    let start = Instant::now();
    let added = sources().folders;
    let total = folders.len();
    let mut out = Vec::new();
    for (i, (name, path)) in folders.into_iter().enumerate() {
        // Equal share of what's left, so an early huge folder can't starve the rest.
        let remaining = budget.saturating_sub(start.elapsed());
        let deadline = Instant::now() + remaining / (total - i) as u32;
        let mut count = 0usize;
        let mut visited = HashSet::new();
        let end = walk_audio(&path, &mut visited, Some(deadline), &mut |_, _| {
            count += 1;
            count < cap
        });
        let capped = matches!(end, WalkEnd::Stopped);
        if count == 0 {
            continue;
        }
        out.push(Candidate {
            already_added: inside_any(&path, &added),
            name,
            path,
            count,
            capped,
        });
    }
    out
}

// ── Playlists ───────────────────────────────────────────────────────────────

fn load_playlists() -> Vec<LocalPlaylist> {
    read_json(PLAYLISTS_FILE).unwrap_or_default()
}

fn edit_playlists<R>(f: impl FnOnce(&mut Vec<LocalPlaylist>) -> R) -> R {
    let _guard = state_lock();
    let mut all = load_playlists();
    let r = f(&mut all);
    write_json(PLAYLISTS_FILE, &all, true);
    r
}

pub fn playlists() -> Vec<LocalPlaylist> {
    load_playlists()
}

/// Create a playlist; returns its id.
pub fn create_playlist(name: &str, tracks: &[PathBuf]) -> String {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    edit_playlists(|all| {
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let mut id;
        loop {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed) & 0xfff;
            id = format!("lp{millis:x}{seq:03x}");
            if !all.iter().any(|p| p.id == id) {
                break;
            }
        }
        all.push(LocalPlaylist { id: id.clone(), name: name.to_string(), tracks: tracks.to_vec() });
        id
    })
}

pub fn add_to_playlist(id: &str, tracks: &[PathBuf]) {
    edit_playlists(|all| {
        if let Some(p) = all.iter_mut().find(|p| p.id == id) {
            p.tracks.extend_from_slice(tracks);
        }
    })
}

/// Remove the entries at these indices (positions in `LocalPlaylist::tracks`).
pub fn remove_from_playlist(id: &str, indices: &[usize]) {
    let drop: HashSet<usize> = indices.iter().copied().collect();
    edit_playlists(|all| {
        if let Some(p) = all.iter_mut().find(|p| p.id == id) {
            let mut i = 0;
            p.tracks.retain(|_| {
                let keep = !drop.contains(&i);
                i += 1;
                keep
            });
        }
    })
}

pub fn rename_playlist(id: &str, name: &str) {
    edit_playlists(|all| {
        if let Some(p) = all.iter_mut().find(|p| p.id == id) {
            p.name = name.to_string();
        }
    })
}

pub fn delete_playlist(id: &str) {
    edit_playlists(|all| all.retain(|p| p.id != id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp dir that is also this thread's state dir; removed on drop.
    struct Env {
        root: PathBuf,
    }

    impl Env {
        fn new(tag: &str) -> Env {
            static N: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "lightify-ll-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("state")).unwrap();
            TEST_STATE_DIR.with(|d| *d.borrow_mut() = Some(root.join("state")));
            TEST_DOWNLOAD_DIR.with(|d| *d.borrow_mut() = None);
            Env { root }
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.root.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            TEST_STATE_DIR.with(|d| *d.borrow_mut() = None);
            TEST_DOWNLOAD_DIR.with(|d| *d.borrow_mut() = None);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// A tiny PCM WAV (8 kHz mono 16-bit, `samples` frames) with an optional RIFF INFO
    /// title/artist.
    fn wav(path: &Path, samples: u32, title: Option<&str>, artist: Option<&str>) {
        fn sub(id: &[u8; 4], text: &str) -> Vec<u8> {
            let mut data = text.as_bytes().to_vec();
            data.push(0);
            let mut v = id.to_vec();
            v.extend_from_slice(&(data.len() as u32).to_le_bytes());
            v.extend_from_slice(&data);
            if data.len() % 2 == 1 {
                v.push(0);
            }
            v
        }
        let mut body = b"WAVE".to_vec();
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // PCM
        body.extend_from_slice(&1u16.to_le_bytes()); // mono
        body.extend_from_slice(&8000u32.to_le_bytes());
        body.extend_from_slice(&16000u32.to_le_bytes()); // byte rate
        body.extend_from_slice(&2u16.to_le_bytes()); // block align
        body.extend_from_slice(&16u16.to_le_bytes());
        if title.is_some() || artist.is_some() {
            let mut info = b"INFO".to_vec();
            if let Some(t) = title {
                info.extend(sub(b"INAM", t));
            }
            if let Some(a) = artist {
                info.extend(sub(b"IART", a));
            }
            body.extend_from_slice(b"LIST");
            body.extend_from_slice(&(info.len() as u32).to_le_bytes());
            body.extend(info);
        }
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(samples * 2).to_le_bytes());
        for i in 0..samples {
            body.extend_from_slice(&((i as i16).wrapping_mul(64)).to_le_bytes());
        }
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend(body);
        fs::write(path, file).unwrap();
    }

    #[test]
    fn is_audio_matches_decoder_formats() {
        for ok in ["a.mp3", "b.FLAC", "c.ogg", "d.oga", "e.wav", "f.m4a", "g.aac", "x/y/Z.Mp3", "h.aiff", "i.AIF", "j.aifc", "k.mp2", "l.caf", "m.m4b"] {
            assert!(is_audio(Path::new(ok)), "{ok}");
        }
        for no in ["a.opus", "b.txt", "c.jpg", "mp3", "d.mp3.part", "e", "f.wma", "g.mp4"] {
            assert!(!is_audio(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn add_paths_classifies_and_dedups() {
        let env = Env::new("add");
        let music = env.dir("Music");
        let album = env.dir("Music/Album");
        let loose_dir = env.dir("Elsewhere");
        let inside = album.join("in.wav");
        let loose = loose_dir.join("loose.wav");
        let text = loose_dir.join("notes.txt");
        wav(&inside, 10, None, None);
        wav(&loose, 10, None, None);
        fs::write(&text, "hi").unwrap();

        let r = add_paths(&[music.clone(), loose.clone(), text.clone(), env.root.join("missing")]);
        assert_eq!(r, AddResult { audio_files: 1, folders: 1, skipped: 2 });
        let s = sources();
        assert_eq!(s.folders, vec![absolute(&music)]);
        assert_eq!(s.files, vec![absolute(&loose)]);

        // A file and a subfolder inside a watched folder, and the same loose file again:
        // recognised but not duplicated.
        let r = add_paths(&[inside.clone(), album.clone(), loose.clone()]);
        assert_eq!(r, AddResult { audio_files: 2, folders: 1, skipped: 0 });
        assert_eq!(sources(), s);

        // Adding a parent folder absorbs the folders and loose files below it.
        let r = add_paths(&[env.root.clone()]);
        assert_eq!(r.folders, 1);
        let s = sources();
        assert_eq!(s.folders, vec![absolute(&env.root)]);
        assert!(s.files.is_empty());

        remove_folder(&env.root);
        assert!(sources().folders.is_empty());
    }

    #[test]
    fn sources_round_trip_and_first_run_default() {
        let env = Env::new("src");
        // First run with no downloader folder: empty.
        assert_eq!(sources(), Sources::default());
        // First run with a downloader folder that exists: it is the library.
        let dl = env.dir("Downloads/LightifyDownloads");
        TEST_DOWNLOAD_DIR.with(|d| *d.borrow_mut() = Some(dl.clone()));
        assert_eq!(sources().folders, vec![dl.clone()]);

        let s = Sources {
            folders: vec![env.root.join("A"), env.root.join("B")],
            files: vec![env.root.join("x.mp3")],
        };
        set_sources(&s);
        assert_eq!(sources(), s);
        // Once saved, the downloader default no longer applies.
        set_sources(&Sources::default());
        assert_eq!(sources(), Sources::default());
    }

    #[test]
    fn scan_reads_tags_and_is_incremental() {
        let env = Env::new("scan");
        let music = env.dir("Music");
        let sub = env.dir("Music/Artist/Album");
        let a = sub.join("01 First.wav");
        let b = music.join("untagged.wav");
        let c = env.dir("Music/.hidden").join("skip.wav");
        wav(&a, 8000, Some("First Song"), Some("Some Artist"));
        wav(&b, 4000, None, None);
        wav(&c, 10, None, None);
        fs::write(music.join("broken.mp3"), b"not really an mp3").unwrap();
        fs::write(music.join("notes.txt"), b"not audio").unwrap();
        add_paths(&[music.clone()]);

        let calls = std::cell::Cell::new(0usize);
        let (tracks, opened) = scan_counted(&|_| calls.set(calls.get() + 1));
        assert!(calls.get() >= 1);
        assert_eq!(opened, 3);
        assert_eq!(tracks.len(), 3, "{tracks:#?}");
        let ta = tracks.iter().find(|t| t.path == a).unwrap();
        assert_eq!(ta.title, "First Song");
        assert_eq!(ta.artist, "Some Artist");
        assert_eq!(ta.album_artist, "Some Artist");
        assert_eq!(ta.album, UNKNOWN_ALBUM);
        assert_eq!(ta.duration_ms, 1000);
        let tb = tracks.iter().find(|t| t.path == b).unwrap();
        assert_eq!(tb.title, "untagged");
        assert_eq!(tb.artist, UNKNOWN_ARTIST);
        assert_eq!(tb.duration_ms, 500);
        // Unreadable file kept with its stem.
        let broken = tracks.iter().find(|t| t.title == "broken").unwrap();
        assert_eq!((broken.artist.as_str(), broken.duration_ms), (UNKNOWN_ARTIST, 0));
        assert_eq!(cached(), tracks);

        // Backdate added_at in the cache to tell kept entries from fresh ones.
        let mut idx: IndexFile = read_json(INDEX_FILE).unwrap();
        for e in &mut idx.entries {
            e.track.added_at = 42;
        }
        write_json(INDEX_FILE, &idx, false);

        // Nothing changed: nothing reopened, added_at kept.
        let (again, opened) = scan_counted(&|_| {});
        assert_eq!(opened, 0);
        assert_eq!(again.len(), 3);
        assert!(again.iter().all(|t| t.added_at == 42));

        // A new file is opened (and only it); a deleted file drops out.
        let d = sub.join("02 Second.wav");
        wav(&d, 800, Some("Second Song"), Some("Some Artist"));
        fs::remove_file(&b).unwrap();
        let (third, opened) = scan_counted(&|_| {});
        assert_eq!(opened, 1);
        assert_eq!(third.len(), 3);
        assert!(third.iter().all(|t| t.path != b));
        let td = third.iter().find(|t| t.path == d).unwrap();
        assert!(td.added_at > 42);
        assert!(third.iter().filter(|t| t.path != d).all(|t| t.added_at == 42));

        // Removing the folder empties the library on the next scan.
        remove_folder(&music);
        assert!(scan(&|_| {}).is_empty());
        assert!(cached().is_empty());
    }

    #[test]
    fn scan_sorts_and_includes_loose_files() {
        let env = Env::new("sort");
        let dir = env.dir("Loose");
        let z = dir.join("z.wav");
        let a = dir.join("a.wav");
        wav(&z, 10, Some("Zed"), Some("Alpha"));
        wav(&a, 10, Some("Ay"), Some("Beta"));
        add_paths(&[z.clone(), a.clone()]);
        let t = scan(&|_| {});
        let titles: Vec<&str> = t.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Zed", "Ay"]); // by album artist: Alpha before Beta
    }

    #[test]
    fn cover_falls_back_to_folder_image() {
        let env = Env::new("cover");
        let dir = env.dir("Album");
        let t = dir.join("t.wav");
        wav(&t, 10, None, None);
        assert_eq!(cover(&t), None);
        fs::write(dir.join("Folder.png"), b"png-bytes").unwrap();
        fs::write(dir.join("cover.jpg"), b"jpg-bytes").unwrap();
        assert_eq!(cover(&t).as_deref(), Some(&b"jpg-bytes"[..]));
    }

    #[test]
    fn candidates_count_cap_and_skip_empty() {
        let env = Env::new("cand");
        let full = env.dir("Full");
        let empty = env.dir("Empty");
        for i in 0..5 {
            wav(&full.join(format!("{i}.wav")), 10, None, None);
        }
        let c = find_candidates_in(
            vec![("Full".into(), full.clone()), ("Empty".into(), empty)],
            Duration::from_secs(2),
            3,
        );
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].count, c[0].capped, c[0].already_added), (3, true, false));
        add_paths(&[full.clone()]);
        let c = find_candidates_in(vec![("Full".into(), full)], Duration::from_secs(2), 100);
        assert_eq!((c[0].count, c[0].capped, c[0].already_added), (5, false, true));
    }

    #[test]
    fn find_candidates_is_bounded() {
        let _env = Env::new("findall");
        let t = Instant::now();
        let c = find_candidates();
        assert!(t.elapsed() < CANDIDATE_BUDGET + Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(c.iter().all(|c| c.count > 0 && c.count <= CANDIDATE_CAP));
    }

    #[test]
    fn playlist_crud() {
        let _env = Env::new("pl");
        let p = |s: &str| PathBuf::from(s);
        let id = create_playlist("Mix", &[p("a"), p("b")]);
        let id2 = create_playlist("Other", &[]);
        assert_ne!(id, id2);
        add_to_playlist(&id, &[p("c"), p("a"), p("d")]);
        assert_eq!(playlists()[0].tracks, [p("a"), p("b"), p("c"), p("a"), p("d")]);
        // Indices, not paths: only the second "a" goes; out-of-range ignored.
        remove_from_playlist(&id, &[3, 1, 99]);
        assert_eq!(playlists()[0].tracks, [p("a"), p("c"), p("d")]);
        rename_playlist(&id, "Renamed");
        assert_eq!(playlists()[0].name, "Renamed");
        delete_playlist(&id);
        let all = playlists();
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].id.as_str(), all[0].name.as_str()), (id2.as_str(), "Other"));
        // Unknown ids are no-ops.
        add_to_playlist("nope", &[p("x")]);
        delete_playlist("nope");
        assert_eq!(playlists(), all);
    }
}
