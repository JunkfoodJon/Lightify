//! Library cover thumbnails (UI-PLAN A2).
//!
//! Covers are fetched off the worker and UI threads, on one short-lived loader thread
//! per batch (at most 4 downloads in flight), shrunk to `SIZE`² — sharp at the 32px
//! the rows draw them, up to 200% scaling — and kept on disk as small PNGs keyed by
//! URL, so a relaunch reads them back without the network. Cover images come from
//! Spotify's image CDN, not the Web API, so they don't count against the rate gate.
//!
//! `slint::Image` can't cross threads, so pixels travel as `SharedPixelBuffer` and
//! become images on the UI thread, which keeps playlist id → image and fills the
//! `playlist-art` model (index-parallel to `playlists`) whenever rows are pushed.
//! Memory: 16 KB per cover (≈1.7 MB for a 107-playlist library).
//!
//! Search rows use the same loader keyed by URL instead of playlist id, at two
//! sizes (64px rows, 128px for the Top result card). The UI-thread URL cache is
//! bounded, and the disk cache is pruned at startup, so searching all day can't
//! grow either without limit.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::{MainWindow, Row, SearchRow};

/// Edge length the covers are stored and drawn from.
pub const SIZE: u32 = 64;
/// The search Top result card's cover.
pub const LARGE: u32 = 128;
/// URL-keyed covers kept on the UI thread before the cache is dropped and refilled.
const URL_CACHE_MAX: usize = 400;
/// Disk cache: prune the oldest files down to `DISK_KEEP` once over `DISK_MAX`.
const DISK_MAX: usize = 1500;
const DISK_KEEP: usize = 1200;
/// Concurrent downloads per batch.
const PARALLEL: usize = 4;

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

thread_local! {
    /// UI thread only: playlist id → cover.
    static THUMBS: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
    /// UI thread only: (url, size) → cover, for search rows.
    static BY_URL: RefCell<HashMap<(String, u32), Image>> = RefCell::new(HashMap::new());
}

/// (url, size) pairs already delivered or in flight for search rows.
static REQUESTED_URLS: Mutex<Option<HashSet<(String, u32)>>> = Mutex::new(None);

/// (playlist id, cover URL) pairs already delivered or in flight — a batch never
/// re-requests them. Keyed by the pair, not the URL alone: two playlists can share a
/// cover, and keyed by URL only the first of them ever got it.
static REQUESTED: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);

/// The playlists in `playlists` whose cover isn't delivered or in flight yet,
/// grouped by URL so a cover shared by several is fetched once. Marks them as
/// requested in `seen`.
fn new_requests<'a>(
    seen: &mut HashSet<(String, String)>,
    playlists: impl Iterator<Item = (&'a str, &'a str)>,
) -> Vec<(String, Vec<String>)> {
    let mut todo: Vec<(String, Vec<String>)> = Vec::new();
    for (id, url) in playlists {
        if url.is_empty() || !seen.insert((id.to_string(), url.to_string())) {
            continue;
        }
        match todo.iter_mut().find(|(u, _)| u == url) {
            Some((_, ids)) => ids.push(id.to_string()),
            None => todo.push((url.to_string(), vec![id.to_string()])),
        }
    }
    todo
}

/// A cover that failed to load may be asked for again by the next library refresh,
/// instead of staying blank until relaunch.
fn forget_requests(seen: &mut HashSet<(String, String)>, url: &str, ids: &[String]) {
    for id in ids {
        seen.remove(&(id.clone(), url.to_string()));
    }
}

fn cache_dir() -> PathBuf {
    lightify_core::config::data_dir().join("thumbs")
}

fn cache_path(url: &str, size: u32) -> PathBuf {
    cache_dir().join(format!("{:016x}.png", cache_key(url, size)))
}

/// FNV-1a over the URL (and the size when it isn't the default): the same name on every
/// build. `DefaultHasher` may change between Rust releases, and then every cover was
/// downloaded again (BUGS-PLAN DC20). The switch itself renames the cache once.
fn cache_key(url: &str, size: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(url.as_bytes());
    if size != SIZE {
        eat(&size.to_le_bytes());
    }
    h
}

/// Covers that came back but couldn't be decoded this run: not fetched again on every
/// refresh (DC20).
fn undecodable() -> &'static std::sync::Mutex<HashSet<String>> {
    static V: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    V.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

fn to_pixels(img: &image::RgbaImage) -> Pixels {
    SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height())
}

/// A cover from the disk cache, if present.
pub fn load_cached(url: &str) -> Option<Pixels> {
    load_cached_sized(url, SIZE)
}

fn load_cached_sized(url: &str, size: u32) -> Option<Pixels> {
    if url.is_empty() {
        return None;
    }
    let path = cache_path(url, size);
    let bytes = std::fs::read(&path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
    // Used now: pruning goes by age, and a cover read from the cache every launch (the
    // library's) was the oldest file and went first (DC20).
    if let Ok(f) = std::fs::File::options().write(true).open(&path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    Some(to_pixels(&img))
}

/// Download, shrink to `size`² (centre-cropped square) and cache one cover.
async fn fetch(client: &reqwest::Client, url: &str) -> Option<Pixels> {
    fetch_sized(client, url, SIZE).await
}

async fn fetch_sized(client: &reqwest::Client, url: &str, size: u32) -> Option<Pixels> {
    if let Some(px) = load_cached_sized(url, size) {
        return Some(px);
    }
    if undecodable().lock().unwrap_or_else(|p| p.into_inner()).contains(url) {
        return None;
    }
    let bytes = client.get(url).send().await.ok()?.error_for_status().ok()?.bytes().await.ok()?;
    let Ok(decoded) = image::load_from_memory(&bytes) else {
        undecodable().lock().unwrap_or_else(|p| p.into_inner()).insert(url.to_string());
        return None;
    };
    let img = decoded.resize_to_fill(size, size, image::imageops::FilterType::Triangle).into_rgba8();
    let _ = std::fs::create_dir_all(cache_dir());
    let mut png = Vec::new();
    if image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .is_ok()
    {
        let _ = lightify_core::config::write_atomic(&cache_path(url, size), &png);
    }
    Some(to_pixels(&img))
}

/// Make sure every playlist's cover is (being) loaded. Cheap to call on every
/// library refresh: already-requested URLs are skipped, and nothing is spawned when
/// there is nothing new.
pub fn request(weak: &slint::Weak<MainWindow>, playlists: &[lightify_core::Playlist]) {
    // A playlist whose cover was removed: the old one goes too, instead of staying for
    // the rest of the run (BUGS-PLAN DC20).
    let bare: Vec<String> = playlists.iter().filter(|p| p.image.is_empty()).map(|p| p.id.clone()).collect();
    if !bare.is_empty() {
        if let Ok(mut guard) = REQUESTED.lock() {
            if let Some(seen) = guard.as_mut() {
                seen.retain(|(id, _)| !bare.contains(id));
            }
        }
        let _ = weak.upgrade_in_event_loop(move |app| {
            let had: Vec<String> = THUMBS.with(|t| {
                let mut t = t.borrow_mut();
                bare.into_iter().filter(|id| t.remove(id).is_some()).collect()
            });
            if had.is_empty() {
                return;
            }
            let rows = app.get_playlists();
            let art = app.get_playlist_art();
            for i in 0..rows.row_count().min(art.row_count()) {
                if rows.row_data(i).is_some_and(|r| had.iter().any(|id| r.id.as_str() == id)) {
                    art.set_row_data(i, Image::default());
                }
            }
        });
    }
    let todo: Vec<(String, Vec<String>)> = {
        let Ok(mut guard) = REQUESTED.lock() else { return };
        let seen = guard.get_or_insert_with(HashSet::new);
        new_requests(seen, playlists.iter().map(|p| (p.id.as_str(), p.image.as_str())))
    };
    if todo.is_empty() {
        return;
    }
    let weak = weak.clone();
    let _ = std::thread::Builder::new().name("thumbs".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default();
            let mut set = tokio::task::JoinSet::new();
            let mut queue = todo.into_iter();
            loop {
                while set.len() < PARALLEL {
                    let Some((url, ids)) = queue.next() else { break };
                    let client = client.clone();
                    set.spawn(async move {
                        let px = fetch(&client, &url).await;
                        (url, ids, px)
                    });
                }
                let Some(done) = set.join_next().await else { break };
                match done {
                    Ok((_, ids, Some(px))) => deliver(&weak, ids, px),
                    Ok((url, ids, None)) => {
                        if let Ok(mut g) = REQUESTED.lock() {
                            if let Some(seen) = g.as_mut() {
                                forget_requests(seen, &url, &ids);
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        });
    });
}

fn deliver(weak: &slint::Weak<MainWindow>, ids: Vec<String>, px: Pixels) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        let img = Image::from_rgba8(px);
        THUMBS.with(|t| {
            let mut t = t.borrow_mut();
            for id in &ids {
                t.insert(id.clone(), img.clone());
            }
        });
        // Patch just the rows showing these playlists.
        let rows = app.get_playlists();
        let art = app.get_playlist_art();
        for i in 0..rows.row_count().min(art.row_count()) {
            if rows.row_data(i).is_some_and(|r| ids.iter().any(|id| r.id.as_str() == id)) {
                art.set_row_data(i, img.clone());
            }
        }
    });
}

/// UI thread: the `playlist-art` model for a freshly pushed row list.
pub fn art_model(rows: &ModelRc<Row>) -> ModelRc<Image> {
    let art: Vec<Image> = THUMBS.with(|t| {
        let t = t.borrow();
        (0..rows.row_count())
            .map(|i| rows.row_data(i).and_then(|r| t.get(r.id.as_str()).cloned()).unwrap_or_default())
            .collect()
    });
    ModelRc::from(Rc::new(VecModel::from(art)))
}

/// Headless renders: seed the UI-thread cache from disk for these playlists.
pub fn seed_from_disk(playlists: &[lightify_core::Playlist]) {
    THUMBS.with(|t| {
        let mut t = t.borrow_mut();
        for p in playlists {
            if let Some(px) = load_cached(&p.image) {
                t.insert(p.id.clone(), Image::from_rgba8(px));
            }
        }
    });
}

/// Headless renders (`--shot-live`): fill the disk cache for these playlists first,
/// so the render shows real covers. The interactive app uses `request` instead.
pub async fn prefetch(playlists: &[lightify_core::Playlist]) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let mut set = tokio::task::JoinSet::new();
    let mut queue = playlists.iter().filter(|p| !p.image.is_empty()).map(|p| p.image.clone());
    loop {
        while set.len() < PARALLEL {
            let Some(url) = queue.next() else { break };
            let client = client.clone();
            set.spawn(async move { fetch(&client, &url).await.is_some() });
        }
        if set.join_next().await.is_none() {
            break;
        }
    }
}

// ── Search rows: URL-keyed covers ────────────────────────────────────────────

/// Cover size a search row draws at.
pub fn search_size(kind: &str) -> u32 {
    if kind == "top" { LARGE } else { SIZE }
}

/// Load the covers these search rows show. Already-requested ones are skipped.
pub fn request_search(weak: &slint::Weak<MainWindow>, rows: &[SearchRow]) {
    let todo: Vec<(String, u32)> = {
        let Ok(mut guard) = REQUESTED_URLS.lock() else { return };
        let seen = guard.get_or_insert_with(HashSet::new);
        rows.iter()
            .filter(|r| !r.art.is_empty())
            .map(|r| (r.art.to_string(), search_size(&r.kind)))
            .filter(|k| seen.insert(k.clone()))
            .collect()
    };
    if todo.is_empty() {
        return;
    }
    let weak = weak.clone();
    let _ = std::thread::Builder::new().name("thumbs-search".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default();
            let mut set = tokio::task::JoinSet::new();
            let mut queue = todo.into_iter();
            loop {
                while set.len() < PARALLEL {
                    let Some((url, size)) = queue.next() else { break };
                    let client = client.clone();
                    set.spawn(async move {
                        let px = fetch_sized(&client, &url, size).await;
                        (url, size, px)
                    });
                }
                let Some(done) = set.join_next().await else { break };
                match done {
                    Ok((url, size, Some(px))) => deliver_url(&weak, url, size, px),
                    // A failed cover may be asked for again next time it's shown.
                    Ok((url, size, None)) => {
                        if let Ok(mut g) = REQUESTED_URLS.lock() {
                            if let Some(seen) = g.as_mut() {
                                seen.remove(&(url, size));
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        });
    });
}

fn deliver_url(weak: &slint::Weak<MainWindow>, url: String, size: u32, px: Pixels) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        let img = Image::from_rgba8(px);
        BY_URL.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() >= URL_CACHE_MAX {
                // Rows on screen keep their images alive through the model; this
                // only forgets covers of results that are long gone.
                m.clear();
                if let Ok(mut g) = REQUESTED_URLS.lock() {
                    *g = None;
                }
            }
            m.insert((url.clone(), size), img.clone());
        });
        let rows = app.get_search_results();
        let art = app.get_search_art();
        for i in 0..rows.row_count().min(art.row_count()) {
            if let Some(r) = rows.row_data(i) {
                if r.art.as_str() == url && search_size(&r.kind) == size {
                    art.set_row_data(i, img.clone());
                }
            }
        }
    });
}

/// UI thread: the cover for one search row, if loaded.
pub fn search_art(row: &SearchRow) -> Image {
    if row.art.is_empty() {
        return Image::default();
    }
    BY_URL.with(|m| m.borrow().get(&(row.art.to_string(), search_size(&row.kind))).cloned().unwrap_or_default())
}

/// Headless renders: fetch these rows' covers now and seed the UI-thread cache.
pub async fn prefetch_search(rows: &[SearchRow]) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    for r in rows.iter().filter(|r| !r.art.is_empty()) {
        let size = search_size(&r.kind);
        let _ = fetch_sized(&client, &r.art, size).await;
    }
}

/// Headless renders: covers for these rows from the disk cache.
pub fn seed_search_from_disk(rows: &[SearchRow]) {
    BY_URL.with(|m| {
        let mut m = m.borrow_mut();
        for r in rows.iter().filter(|r| !r.art.is_empty()) {
            let size = search_size(&r.kind);
            if let Some(px) = load_cached_sized(&r.art, size) {
                m.insert((r.art.to_string(), size), Image::from_rgba8(px));
            }
        }
    });
}

/// Keep the disk cache bounded: once it holds more than `DISK_MAX` covers, delete
/// the oldest (by modification time) down to `DISK_KEEP`. Runs off the UI thread.
pub fn prune_disk_cache() {
    let _ = std::thread::Builder::new().name("thumbs-prune".into()).spawn(|| {
        let Ok(dir) = std::fs::read_dir(cache_dir()) else { return };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = dir
            .filter_map(|e| e.ok())
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        if files.len() <= DISK_MAX {
            return;
        }
        files.sort();
        let excess = files.len() - DISK_KEEP;
        for (_, path) in files.into_iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // D14: a cover shared by two playlists reaches both, fetched once.
    /// DC20: the cache name is the same on every build (FNV-1a, not `DefaultHasher`).
    #[test]
    fn cover_cache_names_are_stable() {
        assert_eq!(cache_key("", SIZE), 0xcbf2_9ce4_8422_2325, "FNV-1a's offset basis");
        assert_eq!(cache_key("a", SIZE), 0xaf63_dc4c_8601_ec8c, "FNV-1a 64 of \"a\"");
        assert_ne!(cache_key("a", SIZE), cache_key("a", LARGE));
    }

    #[test]
    fn shared_cover_reaches_every_playlist() {
        let mut seen = HashSet::new();
        let todo = new_requests(&mut seen, [("a", "u1"), ("b", "u1"), ("c", "u2"), ("d", "")].into_iter());
        assert_eq!(todo, vec![("u1".to_string(), vec!["a".to_string(), "b".to_string()]), ("u2".to_string(), vec!["c".to_string()])]);
        // A later refresh only asks for what's new, including a new playlist on a known cover.
        let again = new_requests(&mut seen, [("a", "u1"), ("e", "u1")].into_iter());
        assert_eq!(again, vec![("u1".to_string(), vec!["e".to_string()])]);
    }

    // D15: a failed cover is asked for again by the next refresh.
    #[test]
    fn failed_cover_is_requested_again() {
        let mut seen = HashSet::new();
        let todo = new_requests(&mut seen, [("a", "u1"), ("b", "u1")].into_iter());
        assert!(new_requests(&mut seen, [("a", "u1")].into_iter()).is_empty());
        forget_requests(&mut seen, &todo[0].0, &todo[0].1);
        assert_eq!(new_requests(&mut seen, [("a", "u1"), ("b", "u1")].into_iter()), todo);
    }
}
