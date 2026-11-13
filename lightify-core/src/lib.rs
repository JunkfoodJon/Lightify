//! lightify-core — the Tauri-free seam.
//!
//! Loads the SAME cached Spotify session the shipped host writes (config +
//! `.lightify_cache`), refreshes the token on demand, and exposes the Web API
//! operations the native shell needs. The host will migrate onto this later.

pub mod auth;
pub mod beatport;
pub mod config;
pub mod model;
pub mod net;
pub mod ratelimit;
pub mod spotify;
pub mod update;

pub use beatport::BeatportTrack;
pub use model::{Album, Artist, Device, PlaybackState, Playlist, Track};
pub use spotify::TrackPage;

/// Successful "play this" requests (`me/player/play` with something to load) made by
/// any `Session` in this process.
static PLAY_REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many loads this process has asked Spotify for. A device that loads something
/// resets its shuffle/repeat (librespot's `handle_load` → `reset_options`) without
/// announcing it, so the shell watches this to know a load is on its way.
pub fn play_requests() -> u64 {
    PLAY_REQUESTS.load(std::sync::atomic::Ordering::SeqCst)
}

static LAST_PLAY_REQUEST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// How long ago the latest of [`play_requests`] was made.
pub fn last_play_request_age() -> Option<std::time::Duration> {
    LAST_PLAY_REQUEST.lock().ok().and_then(|g| g.map(|t| t.elapsed()))
}

/// `artists/{id}/top-tracks` answered 403/400 — it's forbidden for this client id, so
/// [`Session::artist_tracks`] goes straight to the discography from then on.
static TOP_TRACKS_FORBIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Append the songs of `batch` that `list` doesn't already hold. An artist's
/// discography repeats songs (the single, then the album it's on, then a compilation,
/// each with its own track id), so a song is one title — compared the way Beatport
/// matching compares them, so case/punctuation variants count as the same — not one id.
pub fn append_new_songs(list: &mut Vec<Track>, batch: Vec<Track>) {
    let mut seen: std::collections::HashSet<String> =
        list.iter().map(|t| beatport::normalize_match_text(&t.name)).collect();
    list.extend(batch.into_iter().filter(|t| seen.insert(beatport::normalize_match_text(&t.name))));
}

/// A refreshed token the disk wouldn't take, and why. It is the newest token anybody has,
/// and its refresh token may be the only valid one (Spotify can rotate them), so every
/// `Session` in the process uses it over the older one on disk, and writing it is tried
/// again until it lands or somebody else's newer token replaces it.
static UNSAVED_TOKEN: std::sync::Mutex<Option<(config::TokenInfo, String)>> = std::sync::Mutex::new(None);

/// Why the newest Spotify session couldn't be saved, while that's still so. Playback
/// carries on from memory, but closing the app now may mean signing in again.
pub fn token_save_problem() -> Option<String> {
    UNSAVED_TOKEN.lock().ok()?.as_ref().map(|(_, why)| why.clone())
}

/// Save a token a refresh just produced, holding on to it if the disk refuses.
fn store_refreshed_token(dir: &std::path::Path, token: &config::TokenInfo) {
    let outcome = config::save_token_checked(dir, token);
    if let Err(why) = &outcome {
        eprintln!("[lightify] {why}");
    }
    if let Ok(mut unsaved) = UNSAVED_TOKEN.lock() {
        *unsaved = outcome.err().map(|why| (token.clone(), why));
    }
}

/// Try again to save a token the disk refused. Returns the token while it still only
/// exists in memory.
fn retry_unsaved_token(dir: &std::path::Path) -> Option<config::TokenInfo> {
    let mut unsaved = UNSAVED_TOKEN.lock().ok()?;
    let (token, why) = unsaved.as_mut()?;
    // Someone refreshed since: theirs must not be overwritten with an older one.
    if config::load_token(dir).is_some_and(|disk| disk.expires_at >= token.expires_at) {
        *unsaved = None;
        return None;
    }
    match config::save_token_checked(dir, token) {
        Ok(()) => {
            *unsaved = None;
            None
        }
        Err(e) => {
            *why = e;
            Some(token.clone())
        }
    }
}

/// What a `Session` needing a new token should take from the stored one.
#[derive(Debug, PartialEq, Eq)]
enum StoredToken {
    /// Good as it is: somebody else already refreshed.
    Adopt,
    /// Expired, or just refused, but its refresh token is the newest one anyone was given.
    RefreshToken,
    /// Nothing: older than what this `Session` holds.
    Ignore,
}

fn stored_token_use(memory: &config::TokenInfo, stored: &config::TokenInfo, rejected: bool) -> StoredToken {
    if stored.expires_at < memory.expires_at {
        return StoredToken::Ignore;
    }
    // After a 401 only a DIFFERENT token is worth adopting; the refused one is not.
    if !stored.is_expired() && !(rejected && stored.access_token == memory.access_token) {
        return StoredToken::Adopt;
    }
    if stored.refresh_token.is_empty() {
        StoredToken::Ignore
    } else {
        StoredToken::RefreshToken
    }
}

/// The name the desktop engine registers with Spotify Connect. The phone's own engine
/// is "Lightify · <phone>", so it isn't this.
const DESKTOP_ENGINE_NAME: &str = "Lightify";

/// Which listed device a player call may aim at when it has no remembered target that
/// still works, and whether to transfer to it. No answer is a real answer: the request
/// then goes out unaddressed, and Spotify's "no active device" reaches the user, which
/// beats starting music on whatever happened to be first in the list.
///
/// 1. `prior`, the device already chosen, if it is still listed.
/// 2. The active device, untransferred. It outranks our own engine because a transfer
///    would take playback off a phone or speaker the user is listening to.
/// 3. With nothing active, so nothing to take away: the desktop engine, if there is
///    exactly one. Two (two PCs signed in) is a guess, so it's no answer.
fn pick_device<'a>(devices: &'a [Device], prior: Option<&str>) -> Option<(&'a Device, bool)> {
    if let Some(known) = prior.and_then(|id| devices.iter().find(|d| d.id == id)) {
        return Some((known, false));
    }
    if let Some(active) = devices.iter().find(|d| d.is_active) {
        return Some((active, false));
    }
    let mut engines = devices.iter().filter(|d| d.name.eq_ignore_ascii_case(DESKTOP_ENGINE_NAME));
    match (engines.next(), engines.next()) {
        (Some(engine), None) => Some((engine, true)),
        _ => None,
    }
}

fn note_play_request() {
    if let Ok(mut g) = LAST_PLAY_REQUEST.lock() {
        *g = Some(std::time::Instant::now());
    }
    PLAY_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// The full Beatport genre list (name, slug). Static — no network, no auth.
pub fn beatport_genres() -> Vec<(String, String)> {
    beatport::genre_list()
        .into_iter()
        .map(|(n, s)| (n.to_string(), s.to_string()))
        .collect()
}

use std::path::PathBuf;

/// One HTTP client for the whole process. `Session::load()` runs per background
/// task (the sidebar fetch alone fires every 1.5 s while the queue panel is open),
/// and each used to build its own client — a fresh connection pool, so a fresh TLS
/// handshake to api.spotify.com every time. Cloning shares the pool.
///
/// The timeouts are a correctness fix, not tuning: reqwest has none by default, and
/// the shell's worker awaits most calls inline, so one half-open connection used to
/// freeze every command (play/pause included) until the OS gave up on the socket.
fn shared_http() -> Result<reqwest::Client, String> {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(c) = HTTP.get() {
        return Ok(c.clone());
    }
    let c = reqwest::Client::builder()
        .user_agent("Lightify-shell/0.1")
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(20))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    Ok(HTTP.get_or_init(|| c).clone())
}

/// Results of a search across the four Spotify types.
#[derive(Default, Clone)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    /// Track id → a small album-cover URL (search-row thumbnails).
    pub track_thumbs: std::collections::HashMap<String, String>,
}

/// An authenticated Spotify session backed by the on-disk cache.
pub struct Session {
    http: reqwest::Client,
    data_dir: PathBuf,
    client_id: String,
    token: config::TokenInfo,
    /// The Connect device every player call is aimed at (normally Lightify's own
    /// engine). Mirrors the shipped host's `active_device_id`: without it, transport
    /// calls rely on Spotify's `is_active` flag, which lags by seconds and clears
    /// after idle — so "play" intermittently 404s with NO_ACTIVE_DEVICE even though
    /// our engine is registered and healthy.
    active_device: Option<String>,
}

impl Session {
    /// Load config + cached token from disk. Err if the user isn't signed in
    /// (no client_id or no token) — the caller should show a "not connected" UI.
    pub fn load() -> Result<Self, String> {
        let data_dir = config::data_dir();
        let cfg = config::load_config(&data_dir);
        if cfg.client_id.trim().is_empty() {
            return Err("No Spotify Client ID configured. Open Lightify to sign in.".into());
        }
        let token = config::load_token(&data_dir)
            .ok_or("No cached Spotify session. Open Lightify to sign in.")?;
        Ok(Self { http: shared_http()?, data_dir, client_id: cfg.client_id, token, active_device: None })
    }

    pub fn display_name(&self) -> &str {
        &self.token.cached_display_name
    }

    /// The account's Spotify user id (empty until the first profile read).
    pub fn user_id(&self) -> &str {
        &self.token.cached_user_id
    }

    /// The current Web-API access token. Handed to the bundled playback engine on
    /// stdin so it can call the Web API on the user's behalf; call `ensure_fresh`
    /// first if the token might be stale.
    pub fn access_token(&self) -> &str {
        &self.token.access_token
    }

    /// Fetch raw bytes from a URL (e.g. album-art images on scdn.co). Public asset,
    /// no auth header needed; reuses the session's HTTP client.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("fetch {url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("fetch {url} status: {e}"))?;
        Ok(resp.bytes().await.map_err(|e| format!("fetch {url} bytes: {e}"))?.to_vec())
    }

    /// Refresh the access token if it's expired, preserving the cached identity
    /// fields and writing the new token back to the shared cache.
    ///
    /// Several `Session`s live at once (the worker's, plus one per background task),
    /// and the shipped app shares the same cache file. So a refresh is single-flight
    /// across the process, and the on-disk token is re-read first: if someone else
    /// already refreshed, adopt theirs instead of spending another refresh — and,
    /// more importantly, instead of presenting a refresh token that a rotation may
    /// already have replaced.
    ///
    /// Failures: `spotify::is_token_revoked` is the only one that means "sign in again".
    pub async fn ensure_fresh(&mut self) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let unsaved = retry_unsaved_token(&self.data_dir);
        if !self.token.is_expired() && !spotify::TOKEN_REJECTED.load(Ordering::Relaxed) {
            return Ok(());
        }
        static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _single_flight = REFRESH.lock().await;
        // Cleared once, inside the single flight: one refresh answers every Session.
        let rejected = spotify::TOKEN_REJECTED.swap(false, Ordering::Relaxed);
        if let Some(stored) = unsaved.or_else(|| config::load_token(&self.data_dir)) {
            match stored_token_use(&self.token, &stored, rejected) {
                StoredToken::Adopt => {
                    self.adopt(stored);
                    return Ok(());
                }
                StoredToken::RefreshToken => self.token.refresh_token = stored.refresh_token,
                StoredToken::Ignore => {}
            }
        }
        let mut new = spotify::refresh_access_token(
            &self.http,
            &self.client_id,
            &self.token.refresh_token,
        )
        .await?;
        new.cached_user_id = self.token.cached_user_id.clone();
        new.cached_display_name = self.token.cached_display_name.clone();
        self.token = new;
        store_refreshed_token(&self.data_dir, &self.token);
        Ok(())
    }

    /// Take over a token another holder refreshed, keeping our identity fields when
    /// the other writer didn't carry them.
    fn adopt(&mut self, mut disk: config::TokenInfo) {
        if disk.cached_user_id.is_empty() {
            disk.cached_user_id = std::mem::take(&mut self.token.cached_user_id);
        }
        if disk.cached_display_name.is_empty() {
            disk.cached_display_name = std::mem::take(&mut self.token.cached_display_name);
        }
        self.token = disk;
    }

    fn access(&self) -> &str {
        &self.token.access_token
    }

    pub async fn playlists(&mut self) -> Result<Vec<Playlist>, String> {
        self.ensure_fresh().await?;
        spotify::get_playlists(&self.http, self.access()).await
    }

    /// Total number of the user's saved ("Liked Songs") tracks. Cheap: asks for
    /// one item and reads the `total`. Liked Songs is a pinned pseudo-playlist,
    /// not part of `me/playlists`.
    pub async fn saved_tracks_total(&mut self) -> Result<u32, String> {
        self.ensure_fresh().await?;
        let v = spotify::sp_get(&self.http, self.access(), "me/tracks?limit=1")
            .await?
            .ok_or("No saved-tracks response")?;
        Ok(v["total"].as_u64().unwrap_or(0) as u32)
    }

    pub async fn playback(&mut self) -> Result<Option<PlaybackState>, String> {
        self.ensure_fresh().await?;
        spotify::get_playback(&self.http, self.access()).await
    }

    /// A playlist's tracks, following pagination up to `cap` tracks.
    pub async fn playlist_tracks(&mut self, playlist_id: &str, cap: usize) -> Result<Vec<Track>, String> {
        self.ensure_fresh().await?;
        let (limit, mut offset) = (100u32, 0u32);
        let mut out: Vec<Track> = Vec::new();
        loop {
            let (batch, n) =
                spotify::get_playlist_tracks(&self.http, self.access(), playlist_id, limit, offset)
                    .await?;
            out.extend(batch);
            offset += limit;
            if n < limit as usize || out.len() >= cap {
                break;
            }
        }
        out.truncate(cap);
        Ok(out)
    }

    /// The user's saved ("Liked Songs") tracks, up to `cap`.
    /// Note: `me/tracks` caps `limit` at 50 (Spotify rejects 100 with "Invalid limit").
    pub async fn saved_tracks(&mut self, cap: usize) -> Result<Vec<Track>, String> {
        self.ensure_fresh().await?;
        let (limit, mut offset) = (50u32, 0u32);
        let mut out: Vec<Track> = Vec::new();
        loop {
            let (batch, n) =
                spotify::get_saved_tracks(&self.http, self.access(), limit, offset).await?;
            out.extend(batch);
            offset += limit;
            if n < limit as usize || out.len() >= cap {
                break;
            }
        }
        out.truncate(cap);
        Ok(out)
    }

    /// Play an explicit list of track URIs (Liked Songs / no context).
    pub async fn play_uris(&mut self, uris: &[String]) -> Result<(), String> {
        self.player_put("me/player/play", Some(serde_json::json!({ "uris": uris }))).await
    }

    /// Search across tracks, playlists, artists and albums.
    pub async fn search(&mut self, q: &str) -> Result<SearchResults, String> {
        self.ensure_fresh().await?;
        // One request for all four types (it used to be two: tracks/artists/albums,
        // then playlists).
        let general = spotify::search_general(&self.http, self.access(), q).await?;
        Ok(spotify::parse_search_results(&general))
    }

    /// One more page of a single search type (`track`/`artist`/`album`/`playlist`)
    /// at `offset`, plus Spotify's total for it.
    pub async fn search_page(&mut self, q: &str, kind: &str, offset: u32) -> Result<(SearchResults, u32), String> {
        self.ensure_fresh().await?;
        spotify::search_type_page(&self.http, self.access(), q, kind, offset).await
    }

    /// An album's tracks (for playing an album from search).
    pub async fn album_tracks(&mut self, album_id: &str) -> Result<Vec<Track>, String> {
        self.ensure_fresh().await?;
        spotify::get_album_tracks(&self.http, self.access(), album_id).await
    }

    /// An artist's tracks, for queueing a search hit whole: `artists/{id}/top-tracks`
    /// when Spotify serves it, otherwise the first releases of their discography
    /// ([`Self::artist_tracks_page`]), up to `ARTIST_QUEUE_TRACKS` distinct songs.
    ///
    /// The fallback used to be a track search for `artist:"<name>"`, filtered by
    /// artist id — but dev-mode search is fuzzy and capped (for "Spor" it reported 8
    /// hits in total, none of them by Spor), so most artists came back empty.
    /// `top-tracks` itself 403s for this client id; once it has, it isn't asked again
    /// this run (it would only cost a request per artist click).
    pub async fn artist_tracks(&mut self, artist_id: &str, _artist_name: &str) -> Result<Vec<Track>, String> {
        const ARTIST_QUEUE_TRACKS: usize = 50;
        const ARTIST_QUEUE_PAGES: u32 = 3;
        self.ensure_fresh().await?;
        if !TOP_TRACKS_FORBIDDEN.load(std::sync::atomic::Ordering::Relaxed) {
            let path = format!("artists/{artist_id}/top-tracks?market=from_token");
            match spotify::sp_get(&self.http, self.access(), &path).await {
                Ok(Some(v)) => {
                    let tracks: Vec<Track> = v["tracks"]
                        .as_array()
                        .map(|items| items.iter().filter_map(spotify::parse_track).collect())
                        .unwrap_or_default();
                    if !tracks.is_empty() {
                        return Ok(tracks);
                    }
                }
                Ok(None) => {}
                Err(e) if e.contains(" 403 ") || e.contains(" 400 ") => {
                    TOP_TRACKS_FORBIDDEN.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
        let mut out: Vec<Track> = Vec::new();
        let mut offset = 0u32;
        for _ in 0..ARTIST_QUEUE_PAGES {
            let page = self.artist_tracks_page(artist_id, offset).await?;
            append_new_songs(&mut out, page.tracks);
            offset += Self::ARTIST_RELEASE_PAGE;
            if out.len() >= ARTIST_QUEUE_TRACKS || offset >= page.total {
                break;
            }
        }
        out.truncate(ARTIST_QUEUE_TRACKS);
        Ok(out)
    }

    /// Releases per page of an artist's discography — the most `artists/{id}/albums`
    /// accepts for this client id.
    pub const ARTIST_RELEASE_PAGE: u32 = spotify::SEARCH_MAX_LIMIT;

    /// The tracks of the next [`Self::ARTIST_RELEASE_PAGE`] releases of an artist's
    /// discography at release `offset` — their albums and singles first, then what they
    /// appear on — keeping only tracks the artist is credited on. `total` is the
    /// artist's release count (what to page against), not a track count.
    ///
    /// Spotify's batch endpoints (`albums?ids=`) 403 for this client id, so each
    /// release is its own request; they go out a few at a time rather than in one
    /// burst, since the dev-mode quota is small and shared. A release that fails is
    /// skipped (the rest of the page is still useful) unless the failure is a rate
    /// limit, which is reported so the caller backs off.
    pub async fn artist_tracks_page(&mut self, artist_id: &str, offset: u32) -> Result<TrackPage, String> {
        use futures_util::StreamExt;
        const CONCURRENT_RELEASES: usize = 4;
        self.ensure_fresh().await?;
        let (releases, total) = spotify::get_artist_releases_page(
            &self.http, self.access(), artist_id, offset, Self::ARTIST_RELEASE_PAGE,
        )
        .await?;
        let (http, token) = (&self.http, self.access());
        let fetched: Vec<Result<Vec<Track>, String>> = futures_util::stream::iter(
            releases.iter().map(|id| spotify::get_album_tracks(http, token, id)),
        )
        .buffered(CONCURRENT_RELEASES)
        .collect()
        .await;
        let mut tracks = Vec::new();
        for r in fetched {
            match r {
                Ok(t) => tracks.extend(t.into_iter().filter(|t| t.artist_ids.iter().any(|id| id == artist_id))),
                Err(e) if ratelimit::is_rate_limited(&e) => return Err(e),
                Err(_) => {}
            }
        }
        let raw = tracks.len();
        Ok(TrackPage { tracks, total, partial: false, raw })
    }

    /// Play a context (playlist/album) starting at a specific track.
    pub async fn play_context(&mut self, context_uri: &str, offset_uri: &str) -> Result<(), String> {
        let body = serde_json::json!({ "context_uri": context_uri, "offset": { "uri": offset_uri } });
        self.player_put("me/player/play", Some(body)).await
    }

    pub async fn play(&mut self) -> Result<(), String> {
        self.player_put("me/player/play", None).await
    }

    /// Start `track_uri` at `position_ms` on our device — inside `context_uri` when
    /// that is a playlist or album (so what follows is that context), else on its own.
    /// One request: `me/player/play` takes the position in its body.
    pub async fn play_at(&mut self, context_uri: Option<&str>, track_uri: &str, position_ms: u64) -> Result<(), String> {
        let ctx = context_uri.filter(|c| c.starts_with("spotify:playlist:") || c.starts_with("spotify:album:"));
        let body = match ctx {
            Some(ctx) => serde_json::json!({ "context_uri": ctx, "offset": { "uri": track_uri }, "position_ms": position_ms }),
            None => serde_json::json!({ "uris": [track_uri], "position_ms": position_ms }),
        };
        self.player_put("me/player/play", Some(body)).await
    }

    /// Plays `uris` from the first one at `position_ms`, in one request (the position
    /// rides in `me/player/play`'s body, so no seek has to chase the load; BUGS-PLAN
    /// DM2-13).
    pub async fn play_uris_at(&mut self, uris: &[String], position_ms: u64) -> Result<(), String> {
        self.player_put("me/player/play", Some(serde_json::json!({ "uris": uris, "position_ms": position_ms }))).await
    }

    pub async fn pause(&mut self) -> Result<(), String> {
        self.player_put("me/player/pause", None).await
    }

    pub async fn next(&mut self) -> Result<(), String> {
        self.player_post("me/player/next").await
    }

    pub async fn previous(&mut self) -> Result<(), String> {
        self.player_post("me/player/previous").await
    }

    /// The user's up-next queue (upcoming tracks only).
    pub async fn queue(&mut self) -> Result<Vec<Track>, String> {
        self.ensure_fresh().await?;
        spotify::get_queue(&self.http, self.access()).await
    }

    /// Recently-played tracks, newest first (each carries `added_at` = played_at).
    pub async fn recently_played(&mut self, limit: u32) -> Result<Vec<Track>, String> {
        self.ensure_fresh().await?;
        spotify::get_recently_played(&self.http, self.access(), limit).await
    }

    pub async fn set_shuffle(&mut self, on: bool) -> Result<(), String> {
        self.player_put(&format!("me/player/shuffle?state={on}"), None).await
    }

    /// Set repeat mode: `off` | `context` | `track`.
    pub async fn set_repeat(&mut self, mode: &str) -> Result<(), String> {
        self.player_put(&format!("me/player/repeat?state={mode}"), None).await
    }

    /// Append a track URI to the up-next queue.
    pub async fn add_to_queue(&mut self, uri: &str) -> Result<(), String> {
        let enc = spotify::urlencode(uri);
        self.player_post(&format!("me/player/queue?uri={enc}")).await
    }

    pub async fn seek(&mut self, position_ms: u64) -> Result<(), String> {
        self.player_put(&format!("me/player/seek?position_ms={position_ms}"), None).await
    }

    pub async fn set_volume(&mut self, percent: u32) -> Result<(), String> {
        self.player_put(
            &format!("me/player/volume?volume_percent={}", percent.min(100)),
            None,
        )
        .await
    }

    /// Is this track in Liked Songs? (`me/tracks/contains`) — the like button's state.
    pub async fn is_track_saved(&mut self, track_id: &str) -> Result<bool, String> {
        self.ensure_fresh().await?;
        spotify::check_saved(&self.http, self.access(), track_id).await
    }

    /// Save / un-save a track in Liked Songs (`PUT`/`DELETE me/tracks`).
    pub async fn set_track_saved(&mut self, track_id: &str, saved: bool) -> Result<(), String> {
        self.ensure_fresh().await?;
        spotify::set_saved(&self.http, self.access(), track_id, saved).await
    }

    /// Start a context (playlist/album) from its first track — the right-click
    /// "Play" and double-click on a library row.
    pub async fn play_context_start(&mut self, context_uri: &str) -> Result<(), String> {
        let body = serde_json::json!({ "context_uri": context_uri, "offset": { "position": 0 } });
        self.player_put("me/player/play", Some(body)).await
    }

    /// Page size for a playlist page — the host's `PLAYLIST_PAGE_LIMIT`.
    pub const PLAYLIST_PAGE: u32 = 100;
    /// Page size for Liked Songs — the host's `LIKED_PAGE_LIMIT`. 50 is also the
    /// hard cap `me/tracks` accepts for this token.
    pub const LIKED_PAGE: u32 = 50;

    /// One page of a playlist, plus its total. Backs the infinite-scroll list.
    pub async fn playlist_tracks_page(
        &mut self,
        playlist_id: &str,
        offset: u32,
    ) -> Result<spotify::TrackPage, String> {
        self.ensure_fresh().await?;
        spotify::get_playlist_tracks_page(
            &self.http,
            self.access(),
            playlist_id,
            Self::PLAYLIST_PAGE,
            offset,
        )
        .await
    }

    /// Page size for playlist search hits — the host's `SEARCH_PLAYLIST_PAGE_LIMIT`.
    /// Spotify rejects more than 10 for this client id.
    pub const SEARCH_PAGE: u32 = 10;

    /// One page of playlist search hits, plus the total Spotify reports.
    pub async fn search_playlists_page(
        &mut self,
        q: &str,
        offset: u32,
    ) -> Result<(Vec<Playlist>, u32), String> {
        self.ensure_fresh().await?;
        spotify::search_playlists_page(&self.http, self.access(), q, Self::SEARCH_PAGE, offset).await
    }

    /// One page of Liked Songs, plus the total saved-track count.
    pub async fn saved_tracks_page(
        &mut self,
        offset: u32,
    ) -> Result<spotify::TrackPage, String> {
        self.ensure_fresh().await?;
        spotify::get_saved_tracks_page(&self.http, self.access(), Self::LIKED_PAGE, offset).await
    }

    /// Save an album to the library ("Save album" in a search hit's menu).
    pub async fn save_album(&mut self, album_id: &str) -> Result<(), String> {
        self.ensure_fresh().await?;
        spotify::save_album(&self.http, self.access(), album_id).await
    }

    /// Save a playlist to the library ("Save to library" in the row menu).
    pub async fn follow_playlist(&mut self, playlist_id: &str) -> Result<(), String> {
        self.ensure_fresh().await?;
        spotify::follow_playlist(&self.http, self.access(), playlist_id).await
    }

    /// Remove a playlist from the library ("Delete" in the row menu — Spotify has
    /// no hard delete, so this un-follows).
    pub async fn unfollow_playlist(&mut self, playlist_id: &str) -> Result<(), String> {
        self.ensure_fresh().await?;
        spotify::unfollow_playlist(&self.http, self.access(), playlist_id).await
    }

    /// Create a private playlist from a list of track URIs ("Create playlist from
    /// selection"). Mirrors the host's guard: without a cached user id the session
    /// isn't really authenticated.
    pub async fn create_playlist(
        &mut self,
        name: &str,
        uris: &[String],
    ) -> Result<Playlist, String> {
        if self.token.cached_user_id.is_empty() {
            return Err("Not authenticated (no user id)".to_string());
        }
        self.ensure_fresh().await?;
        spotify::create_playlist(&self.http, self.access(), name, uris).await
    }

    /// Available Spotify Connect devices.
    /// The account's Spotify plan as `me` reports it: "premium", "free" (or the
    /// legacy "open"). Needs `user-read-private`, which the sign-in asks for.
    pub async fn account_product(&mut self) -> Result<String, String> {
        self.ensure_fresh().await?;
        let me = spotify::sp_get(&self.http, self.access(), "me").await?.unwrap_or_default();
        Ok(me["product"].as_str().unwrap_or_default().to_string())
    }

    pub async fn devices(&mut self) -> Result<Vec<Device>, String> {
        self.ensure_fresh().await?;
        spotify::get_devices(&self.http, self.access()).await
    }

    /// The device player calls are currently aimed at, if one has been resolved.
    pub fn active_device(&self) -> Option<&str> {
        self.active_device.as_deref()
    }

    /// Remember a device without touching the API — used right after the engine
    /// registers, so the very first transport call is already addressed.
    pub fn remember_device(&mut self, device_id: &str) {
        if !device_id.is_empty() {
            self.active_device = Some(device_id.to_string());
        }
    }

    /// The device to aim at: the remembered one ([`Self::remember_device`], a transfer,
    /// or an earlier answer), else whatever [`pick_device`] will vouch for, cached.
    ///
    /// Keeps the host's `ensure_active_device_id` **transfer guard**: a device that
    /// Spotify already reports as active must NOT be transferred to again —
    /// `PUT me/player` answers a redundant transfer with a 500, which is exactly how
    /// adoption was failing. Unlike the host it never settles for any device that is
    /// merely listed, and a name containing "lightify" no longer outranks the active
    /// device: that moved playback off the phone, or onto a speaker nobody chose.
    pub async fn ensure_device(&mut self) -> Option<String> {
        if let Some(id) = self.active_device.clone() {
            return Some(id);
        }
        self.resolve_device(None).await
    }

    /// Re-resolve after a no-device error: look again, keeping the same device if it is
    /// still listed.
    async fn refresh_device(&mut self) -> Option<String> {
        let prior = self.active_device.take();
        self.resolve_device(prior).await
    }

    async fn resolve_device(&mut self, prior: Option<String>) -> Option<String> {
        let devices = match self.devices().await {
            Ok(devices) => devices,
            // Couldn't look (offline, rate limited): that says nothing about the device.
            Err(_) => {
                self.active_device = prior.clone();
                return prior;
            }
        };
        let (chosen, transfer) = pick_device(&devices, prior.as_deref())?;
        if transfer {
            // Best-effort: a failed transfer still leaves the id usable, because every
            // player call names the device explicitly from here on.
            let _ = spotify::transfer_playback(&self.http, self.access(), &chosen.id, false).await;
        }
        self.active_device = Some(chosen.id.clone());
        Some(chosen.id.clone())
    }

    /// A player PUT aimed at our device, with the host's one-shot re-resolve retry.
    async fn player_put(
        &mut self,
        base_path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(), String> {
        self.ensure_fresh().await?;
        let dev = self.ensure_device().await;
        let path = spotify::with_device_query(base_path, dev.as_deref());
        // A play with a body loads something new on the device (a bare play is just
        // "resume", which loads nothing).
        let loads = base_path == "me/player/play" && body.is_some();
        let first = match body.clone() {
            Some(b) => spotify::sp_put_body(&self.http, self.access(), &path, b).await,
            None => spotify::sp_put(&self.http, self.access(), &path).await,
        };
        let result = match first {
            Err(e) if spotify::is_no_device_error(&e) => {
                let dev2 = self.refresh_device().await;
                let path2 = spotify::with_device_query(base_path, dev2.as_deref());
                match body {
                    Some(b) => spotify::sp_put_body(&self.http, self.access(), &path2, b).await,
                    None => spotify::sp_put(&self.http, self.access(), &path2).await,
                }
            }
            other => other,
        };
        if result.is_ok() && loads {
            note_play_request();
        }
        result
    }

    /// A player POST aimed at our device, same retry.
    async fn player_post(&mut self, base_path: &str) -> Result<(), String> {
        self.ensure_fresh().await?;
        let dev = self.ensure_device().await;
        let path = spotify::with_device_query(base_path, dev.as_deref());
        match spotify::sp_post(&self.http, self.access(), &path).await {
            Err(e) if spotify::is_no_device_error(&e) => {
                let dev2 = self.refresh_device().await;
                let path2 = spotify::with_device_query(base_path, dev2.as_deref());
                spotify::sp_post(&self.http, self.access(), &path2).await
            }
            other => other,
        }
    }

    /// Transfer playback to a device (keeps the current play/pause state unless `play`).
    /// Move playback to `device_id`. Also records it as the device every later
    /// player call names, so a transfer and the transport that follows it agree.
    pub async fn transfer_playback(&mut self, device_id: &str, play: bool) -> Result<(), String> {
        self.ensure_fresh().await?;
        let r = spotify::transfer_playback(&self.http, self.access(), device_id, play).await;
        if r.is_ok() {
            self.remember_device(device_id);
        }
        r
    }

    /// Scrape a Beatport chart (`kind` = tracks | hype | releases). Public page,
    /// no Spotify auth needed.
    pub async fn beatport_chart(
        &self,
        genre_slug: &str,
        kind: &str,
    ) -> Result<Vec<BeatportTrack>, String> {
        beatport::fetch_chart(genre_slug, kind).await
    }

    /// Resolve a Beatport (name, artists) to the best-matching Spotify track,
    /// mirroring the host's scored search (accepts only score ≥ MATCH_THRESHOLD).
    pub async fn beatport_match(
        &mut self,
        name: &str,
        artists: &str,
    ) -> Result<Option<Track>, String> {
        Ok(self.beatport_match_with(name, artists, "").await?.0)
    }

    /// `beatport_match`, trying the recording's ISRC first when one is given (one
    /// exact search) before the fuzzy name searches, and saying how the answer was
    /// found — `MatchHow::Cached` cost no request at all, so a caller pacing itself
    /// against the rate budget doesn't need to wait after it.
    pub async fn beatport_match_with(
        &mut self,
        name: &str,
        artists: &str,
        isrc: &str,
    ) -> Result<(Option<Track>, MatchHow), String> {
        self.ensure_fresh().await?;
        let clean = beatport::clean_bp_name(name);
        let target_name = beatport::normalize_match_text(&clean);
        let target_artists = beatport::normalize_artist_list(artists);
        let key = format!("{target_name}|{}", target_artists.join(","));
        if !target_name.is_empty() {
            if let Some(hit) = bp_match_cache::get(&key) {
                return Ok((hit, MatchHow::Cached));
            }
        }
        let isrc = isrc.trim();
        let valid_isrc = isrc.len() == 12 && isrc.bytes().all(|b| b.is_ascii_alphanumeric());
        if valid_isrc {
            let q = format!("isrc:{isrc}");
            if let Some(t) = spotify::search_tracks(&self.http, self.access(), &q, 3)
                .await?
                .into_iter()
                .find(|t| t.is_playable && !t.uri.is_empty())
            {
                if !target_name.is_empty() {
                    bp_match_cache::put(&key, &Some(t.clone()));
                }
                return Ok((Some(t), MatchHow::Isrc));
            }
        }
        let found = self.beatport_match_by_name(name, artists).await?;
        Ok((found, if valid_isrc { MatchHow::IsrcMissed } else { MatchHow::Name }))
    }

    /// The fuzzy half of `beatport_match_with`: field-filtered, then plain, search.
    async fn beatport_match_by_name(
        &mut self,
        name: &str,
        artists: &str,
    ) -> Result<Option<Track>, String> {
        let clean = beatport::clean_bp_name(name);
        let target_name = beatport::normalize_match_text(&clean);
        if target_name.is_empty() {
            return Ok(None);
        }
        let target_artists = beatport::normalize_artist_list(artists);
        let key = format!("{target_name}|{}", target_artists.join(","));
        if let Some(hit) = bp_match_cache::get(&key) {
            return Ok(hit);
        }
        let first_artist = artists.split(',').next().unwrap_or(artists).trim();
        let queries = [
            format!("track:{clean} artist:{first_artist}"),
            format!("{clean} {first_artist}"),
        ];
        let (http, token) = (&self.http, self.access());
        let found = resolve_name_match(queries, &target_name, &target_artists, |q| async move {
            spotify::search_tracks(http, token, &q, 8).await
        })
        .await?;
        bp_match_cache::put(&key, &found);
        Ok(found)
    }
}

/// Run the name searches and score what they find. `Ok(None)` means Spotify answered and
/// nothing matched, the only "no match" fit to cache; a search that failed used to look
/// the same, so a rate limit or a dropped connection marked songs as unmatched for days.
///
/// `Err` when any search failed and no match came of the others. A rate limit or being
/// offline ends it there: the next search would only meet the same thing.
async fn resolve_name_match<F, Fut>(
    queries: [String; 2],
    target_name: &str,
    target_artists: &[String],
    mut search: F,
) -> Result<Option<Track>, String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Track>, String>>,
{
    let mut candidates: Vec<Track> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut failed = None;
    for q in queries {
        match search(q).await {
            Ok(items) => {
                for it in items {
                    if seen.insert(it.id.clone()) {
                        candidates.push(it);
                    }
                }
            }
            Err(e) if ratelimit::is_rate_limited(&e) || net::is_offline_error(&e) => return Err(e),
            Err(e) => failed = failed.or(Some(e)),
        }
        // Only a real match ends it. The field-filtered search is fuzzy, so its rows can
        // all be by other artists, and the plain search may still find the right one.
        if candidates
            .iter()
            .any(|t| beatport::score_beatport_track_match(target_name, target_artists, t) >= beatport::MATCH_THRESHOLD)
        {
            break;
        }
    }
    let best = candidates
        .into_iter()
        .map(|t| {
            let s = beatport::score_beatport_track_match(target_name, target_artists, &t);
            (s, t)
        })
        .max_by_key(|(s, _)| *s);
    match (best, failed) {
        (Some((s, t)), _) if s >= beatport::MATCH_THRESHOLD => Ok(Some(t)),
        (_, Some(e)) => Err(e),
        _ => Ok(None),
    }
}

/// How `Session::beatport_match_with` found its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchHow {
    /// From the on-disk match cache: no request was made.
    Cached,
    /// One exact `isrc:` search.
    Isrc,
    /// The ISRC search found nothing; the name searches were made as well.
    IsrcMissed,
    /// Name searches only (no ISRC to try).
    Name,
}

/// Beatport row → Spotify track answers, kept on disk. Every play of a chart used to
/// search Spotify once or twice for EVERY row again — 100–200 Web-API calls for a Top
/// 100 the user had already played. A match rarely changes, so a hit is kept for 90
/// days; a miss only for 7 (the track may reach Spotify later).
mod bp_match_cache {
    use crate::model::Track;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const HIT_DAYS: u64 = 90;
    const MISS_DAYS: u64 = 7;
    const MAX_ENTRIES: usize = 5000;
    /// Raised when what's stored can no longer be trusted; a file with any other version
    /// reads as empty, and every row re-resolves.
    /// - (no version field) Misses include failed searches, cached as "no match". Hits
    ///   include same-titled songs by other artists, accepted on the title alone.
    /// - 2: neither.
    /// - 3: artists matched by whole words only (DC8): a short artist name inside a longer
    ///   one ("ED" in "Fred again..") no longer counts, so v2's hits are checked again.
    const VERSION: u32 = 3;

    #[derive(Serialize, Deserialize, Clone)]
    struct Entry {
        track: Option<Track>,
        at: u64,
    }

    #[derive(Deserialize)]
    struct Disk {
        version: u32,
        entries: HashMap<String, Entry>,
    }

    #[derive(Serialize)]
    struct DiskOut<'a> {
        version: u32,
        entries: &'a HashMap<String, Entry>,
    }

    static CACHE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);
    /// One save at a time, each taking the map as it stands once its turn comes, so a
    /// slow writer can't put an older map over a newer one. Separate from `CACHE` so a
    /// lookup doesn't wait on the disk. Always taken before `CACHE`, never inside it.
    static SAVING: Mutex<()> = Mutex::new(());

    fn read_disk(bytes: &[u8]) -> HashMap<String, Entry> {
        serde_json::from_slice::<Disk>(bytes)
            .ok()
            .filter(|d| d.version == VERSION)
            .map(|d| d.entries)
            .unwrap_or_default()
    }

    fn write_disk(entries: &HashMap<String, Entry>) -> Option<Vec<u8>> {
        serde_json::to_vec(&DiskOut { version: VERSION, entries }).ok()
    }

    fn path() -> std::path::PathBuf {
        crate::config::data_dir().join("lightify_bp_matches.json")
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn fresh(e: &Entry, now: u64) -> bool {
        let days = if e.track.is_some() { HIT_DAYS } else { MISS_DAYS };
        now.saturating_sub(e.at) < days * 86_400
    }

    fn with<R>(f: impl FnOnce(&mut HashMap<String, Entry>) -> R) -> Option<R> {
        let mut g = CACHE.lock().ok()?;
        let map = g.get_or_insert_with(|| {
            std::fs::read(path()).map(|b| read_disk(&b)).unwrap_or_default()
        });
        Some(f(map))
    }

    /// `Some(answer)` when a fresh answer is cached (the answer itself may be "no
    /// match"); `None` when Spotify has to be asked.
    pub fn get(key: &str) -> Option<Option<Track>> {
        let now = now();
        with(|m| m.get(key).filter(|e| fresh(e, now)).map(|e| e.track.clone())).flatten()
    }

    pub fn put(key: &str, track: &Option<Track>) {
        let now = now();
        with(|m| {
            m.insert(key.to_string(), Entry { track: track.clone(), at: now });
            if m.len() > MAX_ENTRIES {
                m.retain(|_, e| fresh(e, now));
                while m.len() > MAX_ENTRIES {
                    let Some(oldest) = m.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) else { break };
                    m.remove(&oldest);
                }
            }
        });
        let Ok(_turn) = SAVING.lock() else { return };
        if let Some(bytes) = with(|m| write_disk(m)).flatten() {
            let _ = crate::config::write_atomic(&path(), &bytes);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn caches_from_before_the_version_are_discarded() {
            // The shape every earlier build wrote, holding a poisoned miss.
            let old = br#"{"strobe|deadmau5":{"track":null,"at":1800000000}}"#;
            assert!(read_disk(old).is_empty());
            assert!(read_disk(br#"{}"#).is_empty());
            assert!(read_disk(b"not json").is_empty());
            let other = br#"{"version":1,"entries":{"a":{"track":null,"at":1}}}"#;
            assert!(read_disk(other).is_empty());
        }

        #[test]
        fn this_version_reads_back() {
            let mut map = HashMap::new();
            map.insert("a|b".to_string(), Entry { track: None, at: 7 });
            let read = read_disk(&write_disk(&map).unwrap());
            assert_eq!(read.get("a|b").map(|e| e.at), Some(7));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, name: &str, artists: &str) -> Track {
        Track {
            id: id.into(),
            name: name.into(),
            artists: artists.into(),
            artist_ids: vec![],
            album: String::new(),
            album_id: String::new(),
            duration_ms: 0,
            uri: format!("spotify:track:{id}"),
            album_art: String::new(),
            added_at: None,
            is_playable: true,
        }
    }

    /// Runs the matcher against scripted search answers, returning its answer and how
    /// many searches it made.
    async fn resolve(script: Vec<Result<Vec<Track>, String>>) -> (Result<Option<Track>, String>, usize) {
        let asked = std::cell::Cell::new(0);
        let mut script = script.into_iter();
        let answer = resolve_name_match(
            ["q1".to_string(), "q2".to_string()],
            "strobe",
            &["deadmau5".to_string()],
            |_q| {
                asked.set(asked.get() + 1);
                let next = script.next().expect("more searches than scripted");
                async move { next }
            },
        )
        .await;
        (answer, asked.get())
    }

    #[tokio::test]
    async fn a_failed_search_is_never_a_miss() {
        let limited = ratelimit::rate_limited_error(5);
        let offline = format!("{}GET search: dns", net::OFFLINE_PREFIX);

        // A limit or an outage: straight back, without the second search.
        let (a, asked) = resolve(vec![Err(limited.clone())]).await;
        assert_eq!((a.unwrap_err(), asked), (limited, 1));
        let (a, asked) = resolve(vec![Err(offline.clone())]).await;
        assert_eq!((a.unwrap_err(), asked), (offline, 1));

        // Any other failure: the second search still runs, but its empty answer can't
        // turn the first one's failure into "no match".
        let (a, asked) = resolve(vec![Err("GET search 500 X: y".into()), Ok(vec![])]).await;
        assert_eq!((a.unwrap_err().as_str(), asked), ("GET search 500 X: y", 2));
        let (a, _) = resolve(vec![Ok(vec![]), Err("GET search 503 X: y".into())]).await;
        assert_eq!(a.unwrap_err(), "GET search 503 X: y");

        // Nor can a candidate that doesn't qualify.
        let wrong = track("w", "Strobe", "Some Cover Band");
        let (a, _) = resolve(vec![Err("GET search 500 X: y".into()), Ok(vec![wrong])]).await;
        assert!(a.is_err());
    }

    #[tokio::test]
    async fn answers_that_can_be_trusted_stay_answers() {
        // Both searches answered, nothing there: the real miss, and only this is cached.
        let (a, asked) = resolve(vec![Ok(vec![]), Ok(vec![])]).await;
        assert!(matches!(a, Ok(None)));
        assert_eq!(asked, 2);

        let wrong = track("w", "Strobe", "Some Cover Band");
        let (a, asked) = resolve(vec![Ok(vec![wrong.clone()]), Ok(vec![])]).await;
        assert!(matches!(a, Ok(None)), "the wrong artist isn't a match");
        assert_eq!(asked, 2);

        // Rows by other artists from the first search don't stop the second finding it.
        let hit = track("h", "Strobe", "deadmau5");
        let (a, asked) = resolve(vec![Ok(vec![wrong]), Ok(vec![hit.clone()])]).await;
        assert_eq!(a.unwrap().unwrap().id, "h");
        assert_eq!(asked, 2);

        let (a, asked) = resolve(vec![Ok(vec![hit.clone()])]).await;
        assert_eq!(a.unwrap().unwrap().id, "h");
        assert_eq!(asked, 1);

        // A real hit from the second search stands, whatever the first one did.
        let (a, _) = resolve(vec![Err("GET search 500 X: y".into()), Ok(vec![hit])]).await;
        assert_eq!(a.unwrap().unwrap().id, "h");
    }

    fn device(id: &str, name: &str, is_active: bool) -> Device {
        Device { id: id.into(), name: name.into(), is_active, kind: "Computer".into(), volume_percent: 50 }
    }

    fn picked(devices: &[Device], prior: Option<&str>) -> Option<(String, bool)> {
        pick_device(devices, prior).map(|(d, transfer)| (d.id.clone(), transfer))
    }

    #[test]
    fn nothing_transfers_to_a_device_nobody_chose() {
        // The bug: the first listed device, moved to.
        let speakers = [device("tv", "Living Room TV", false), device("car", "Car", false)];
        assert_eq!(picked(&speakers, None), None);
        assert_eq!(picked(&[], None), None);

        // The phone's engine isn't the desktop's, and it isn't ours to wake.
        assert_eq!(picked(&[device("phone", "Lightify · Pixel 9", false)], None), None);

        // Two desktops: a guess, so no answer.
        let two = [device("a", "Lightify", false), device("b", "Lightify", false)];
        assert_eq!(picked(&two, None), None);
    }

    #[test]
    fn the_active_device_keeps_playing_where_it_is() {
        // The bug: a name containing "lightify" outranked what was actually playing.
        let list = [device("desk", "Lightify", false), device("phone", "Pixel 9", true)];
        assert_eq!(picked(&list, None), Some(("phone".to_string(), false)));
    }

    #[test]
    fn the_desktop_engine_is_used_when_nothing_is_playing() {
        let list = [device("tv", "Living Room TV", false), device("desk", "Lightify", false)];
        assert_eq!(picked(&list, None), Some(("desk".to_string(), true)));
    }

    #[test]
    fn a_remembered_device_still_listed_wins_without_a_transfer() {
        let list = [device("desk", "Lightify", false), device("phone", "Pixel 9", true)];
        assert_eq!(picked(&list, Some("desk")), Some(("desk".to_string(), false)));
        // Gone from the list: choose again, as if there were none.
        assert_eq!(picked(&list, Some("gone")), Some(("phone".to_string(), false)));
    }

    fn token(access: &str, refresh: &str, expires_at: i64) -> config::TokenInfo {
        config::TokenInfo {
            access_token: access.into(),
            token_type: "Bearer".into(),
            expires_in: 3600,
            refresh_token: refresh.into(),
            scope: String::new(),
            expires_at,
            cached_user_id: String::new(),
            cached_display_name: String::new(),
        }
    }

    fn now() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    }

    #[test]
    fn an_older_stored_token_never_replaces_the_one_in_memory() {
        let (past, older_past, future) = (now() - 100, now() - 4000, now() + 3000);
        // The bug: a refresh whose save failed left the new refresh token only in memory,
        // and an hour later the disk's older one replaced it and was presented to Spotify.
        let memory = token("new", "rotated", past);
        let disk = token("old", "superseded", older_past);
        assert_eq!(stored_token_use(&memory, &disk, false), StoredToken::Ignore);
        // Also when a 401 is what brought us here, and the old one isn't even expired.
        let memory = token("new", "rotated", future + 100);
        assert_eq!(stored_token_use(&memory, &token("old", "superseded", future), true), StoredToken::Ignore);

        // What the disk is still for.
        let memory = token("mine", "r1", past);
        assert_eq!(stored_token_use(&memory, &token("theirs", "r2", future), false), StoredToken::Adopt);
        assert_eq!(stored_token_use(&memory, &token("theirs", "r2", past + 50), false), StoredToken::RefreshToken);
        assert_eq!(stored_token_use(&memory, &token("mine", "r1", past), false), StoredToken::RefreshToken);
        assert_eq!(stored_token_use(&memory, &token("theirs", "", past + 50), false), StoredToken::Ignore);

        // A 401: the refused token doesn't come back, somebody else's does.
        let memory = token("refused", "r1", future);
        assert_eq!(stored_token_use(&memory, &token("refused", "r1", future), true), StoredToken::RefreshToken);
        assert_eq!(stored_token_use(&memory, &token("theirs", "r2", future), true), StoredToken::Adopt);
    }

    /// The only test touching `UNSAVED_TOKEN`.
    #[test]
    fn a_token_the_disk_refused_is_kept_reported_and_saved_later() {
        let dir = std::env::temp_dir().join(format!("lightify-core-unsaved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A directory where the token file belongs makes the save fail.
        let blocker = dir.join(".lightify_cache");
        std::fs::create_dir_all(&blocker).unwrap();

        let newest = token("new", "rotated", now() + 3600);
        store_refreshed_token(&dir, &newest);
        let why = token_save_problem().expect("the failure is reported");
        assert!(!why.contains("rotated") && !why.contains("\"new\""), "{why}");

        // Still failing: every other Session is handed it instead of the disk's.
        assert_eq!(retry_unsaved_token(&dir).map(|t| t.refresh_token).as_deref(), Some("rotated"));
        assert!(token_save_problem().is_some());

        // The disk recovers: it lands, and the problem is over.
        std::fs::remove_dir(&blocker).unwrap();
        assert!(retry_unsaved_token(&dir).is_none());
        assert!(token_save_problem().is_none());
        assert_eq!(config::load_token(&dir).unwrap().refresh_token, "rotated");

        // Someone's newer token is never overwritten by a late retry.
        std::fs::remove_file(&blocker).unwrap();
        std::fs::create_dir(&blocker).unwrap();
        store_refreshed_token(&dir, &newest);
        std::fs::remove_dir(&blocker).unwrap();
        config::save_token_checked(&dir, &token("theirs", "r3", now() + 7200)).unwrap();
        assert!(retry_unsaved_token(&dir).is_none());
        assert!(token_save_problem().is_none());
        assert_eq!(config::load_token(&dir).unwrap().refresh_token, "r3");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
