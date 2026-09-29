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

pub use beatport::BeatportTrack;
pub use model::{Album, Artist, Device, PlaybackState, Playlist, Track};
pub use spotify::TrackPage;

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
        if !self.token.is_expired() {
            return Ok(());
        }
        static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _single_flight = REFRESH.lock().await;
        if let Some(disk) = config::load_token(&self.data_dir) {
            if !disk.is_expired() {
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
            let batch =
                spotify::get_playlist_tracks(&self.http, self.access(), playlist_id, limit, offset)
                    .await?;
            let n = batch.len();
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
            let batch =
                spotify::get_saved_tracks(&self.http, self.access(), limit, offset).await?;
            let n = batch.len();
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
        // The general (track/artist/album) and playlist requests are independent, so
        // they go out together: one round trip of waiting instead of two.
        let (general, playlists) = tokio::join!(
            spotify::search_general(&self.http, self.access(), q),
            spotify::search_playlists(&self.http, self.access(), q),
        );
        let mut out = spotify::parse_search_results(&general?);
        if let Ok(p) = playlists {
            out.playlists = spotify::parse_search_results(&p).playlists;
        }
        Ok(out)
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

    /// An artist's tracks, for playing one from a search hit. `artists/{id}/top-tracks`
    /// is the "real" endpoint but 403s/400s under this dev-mode token — falls back to
    /// a plain track search scoped to the artist's name, keeping only hits whose own
    /// `artist_ids` include this artist. Search embeds full artist metadata per track,
    /// so this needs no elevated scope at all and is actually more precise than a name
    /// match would be (which is what the shipped host has to fall back to, since its
    /// own track parser doesn't carry artist ids through from a search response).
    pub async fn artist_tracks(&mut self, artist_id: &str, artist_name: &str) -> Result<Vec<Track>, String> {
        self.ensure_fresh().await?;
        let path = format!("artists/{artist_id}/top-tracks?market=from_token");
        match spotify::sp_get(&self.http, self.access(), &path).await {
            Ok(Some(v)) => Ok(v["tracks"]
                .as_array()
                .map(|items| items.iter().filter_map(spotify::parse_track).collect())
                .unwrap_or_default()),
            Ok(None) => Ok(Vec::new()),
            Err(e) if e.contains(" 403 ") || e.contains(" 400 ") => {
                let name = artist_name.replace('"', "");
                let name = name.trim();
                if name.is_empty() {
                    return Ok(Vec::new());
                }
                let q = format!("artist:\"{name}\"");
                // Paged at SEARCH_MAX_LIMIT (10) — this client id 400s past that on
                // ANY search request (see the constant's own doc). A handful of pages,
                // not the shipped host's full 90-offset sweep: this is an approximation
                // already (search hits, not real top-tracks), the API quota is shared
                // with the shipped host, and every extra page is one more request per
                // artist click.
                let mut seen = std::collections::HashSet::new();
                let mut matched = Vec::new();
                let mut offset = 0u32;
                for _ in 0..4 {
                    let (page, total) = spotify::search_tracks_page(
                        &self.http, self.access(), &q, spotify::SEARCH_MAX_LIMIT, offset,
                    ).await?;
                    if page.is_empty() {
                        break;
                    }
                    offset += page.len() as u32;
                    for t in page {
                        if t.artist_ids.iter().any(|id| id == artist_id) && seen.insert(t.id.clone()) {
                            matched.push(t);
                        }
                    }
                    if offset >= total {
                        break;
                    }
                }
                Ok(matched)
            }
            Err(e) => Err(e),
        }
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
        let first = match body.clone() {
            Some(b) => spotify::sp_put_body(&self.http, self.access(), &path, b).await,
            None => spotify::sp_put(&self.http, self.access(), &path).await,
        };
        match first {
            Err(e) if spotify::is_no_device_error(&e) => {
                let dev2 = self.refresh_device().await;
                let path2 = spotify::with_device_query(base_path, dev2.as_deref());
                match body {
                    Some(b) => spotify::sp_put_body(&self.http, self.access(), &path2, b).await,
                    None => spotify::sp_put(&self.http, self.access(), &path2).await,
                }
            }
            other => other,
        }
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
        self.ensure_fresh().await?;
        let clean = beatport::clean_bp_name(name);
        let target_name = beatport::normalize_match_text(&clean);
        if target_name.is_empty() {
            return Ok(None);
        }
        let target_artists = beatport::normalize_artist_list(artists);
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
        Ok(match best {
            Some((s, t)) if s >= beatport::MATCH_THRESHOLD => Some(t),
            _ => None,
        })
    }
}
