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
    pub async fn ensure_fresh(&mut self) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        if !self.token.is_expired() && !spotify::TOKEN_REJECTED.load(Ordering::Relaxed) {
            return Ok(());
        }
        static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _single_flight = REFRESH.lock().await;
        // Cleared once, inside the single flight: one refresh answers every Session.
        let rejected = spotify::TOKEN_REJECTED.swap(false, Ordering::Relaxed);
        if let Some(disk) = config::load_token(&self.data_dir) {
            // After a 401 only a DIFFERENT token on disk is worth adopting (someone
            // else refreshed); the one that was just refused is not.
            if !disk.is_expired() && !(rejected && disk.access_token == self.token.access_token) {
                self.adopt(disk);
                return Ok(());
            }
            // Expired too, but its refresh token is the newest one anyone was given.
            if !disk.refresh_token.is_empty() {
                self.token.refresh_token = disk.refresh_token;
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
        config::save_token(&self.data_dir, &self.token);
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

    /// Resolve the device to aim at, preferring Lightify's own engine, and cache it.
    ///
    /// Ports the host's `ensure_active_device_id` **including its transfer guard**:
    /// a device that Spotify already reports as active must NOT be transferred to
    /// again — `PUT me/player` answers a redundant transfer with a 500, which is
    /// exactly how adoption was failing.
    pub async fn ensure_device(&mut self) -> Option<String> {
        if let Some(id) = self.active_device.clone() {
            return Some(id);
        }
        let devices = self.devices().await.unwrap_or_default();
        let chosen = devices
            .iter()
            .find(|d| d.name.to_ascii_lowercase().contains("lightify"))
            .or_else(|| devices.iter().find(|d| d.is_active))
            .or_else(|| devices.first())
            .cloned()?;
        if !chosen.is_active {
            // Best-effort: a failed transfer still leaves the id usable, because every
            // player call names the device explicitly from here on.
            let _ = spotify::transfer_playback(&self.http, self.access(), &chosen.id, false).await;
        }
        self.active_device = Some(chosen.id.clone());
        Some(chosen.id)
    }

    /// Re-resolve after a no-device error: drop the cached id and look again.
    async fn refresh_device(&mut self) -> Option<String> {
        self.active_device = None;
        self.ensure_device().await
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
        let mut candidates: Vec<Track> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for q in queries {
            if let Ok(items) = spotify::search_tracks(&self.http, self.access(), &q, 8).await {
                for it in items {
                    if seen.insert(it.id.clone()) {
                        candidates.push(it);
                    }
                }
            }
            if !candidates.is_empty() {
                break;
            }
        }
        let best = candidates
            .into_iter()
            .map(|t| {
                let s = beatport::score_beatport_track_match(&target_name, &target_artists, &t);
                (s, t)
            })
            .max_by_key(|(s, _)| *s);
        let found = match best {
            Some((s, t)) if s >= beatport::MATCH_THRESHOLD => Some(t),
            _ => None,
        };
        bp_match_cache::put(&key, &found);
        Ok(found)
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

    #[derive(Serialize, Deserialize, Clone)]
    struct Entry {
        track: Option<Track>,
        at: u64,
    }

    static CACHE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

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
            std::fs::read(path())
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default()
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
        let bytes = with(|m| {
            m.insert(key.to_string(), Entry { track: track.clone(), at: now });
            if m.len() > MAX_ENTRIES {
                m.retain(|_, e| fresh(e, now));
                while m.len() > MAX_ENTRIES {
                    let Some(oldest) = m.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) else { break };
                    m.remove(&oldest);
                }
            }
            serde_json::to_vec(&*m).ok()
        })
        .flatten();
        if let Some(bytes) = bytes {
            let _ = crate::config::write_atomic(&path(), &bytes);
        }
    }
}
