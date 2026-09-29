//! Spotify Web API: token refresh, response parsers (copied from the host so
//! behavior matches), and the HTTP calls the shell needs.

use serde_json::Value;

use crate::model::{Album, Artist, Device, PlaybackState, Playlist, Track};
use crate::config::TokenInfo;
use crate::ratelimit;

const BASE: &str = "https://api.spotify.com/v1";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SCOPES_FALLBACK: &str = "user-read-playback-state user-modify-playback-state \
user-read-currently-playing user-library-read playlist-read-private streaming";
/// Hard ceiling for ANY `/search` request under this app's client id. Spotify's own
/// docs say 50, but this client id 400s ("Invalid limit") past 10 — verified live
/// against track/artist/album/playlist searches by the shipped host (see its own
/// `SPOTIFY_SEARCH_MAX_LIMIT`, `lightify-tauri/src-tauri/src/main.rs`), which shares
/// the same client id (`lightify_config.json`'s `client_id` — checked identical).
/// Depth has to come from offset paging, never a bigger `limit`.
pub const SEARCH_MAX_LIMIT: u32 = 10;

// ── Parsers (mirror host spotify.rs) ─────────────────────────────────────────

pub fn parse_track(v: &Value) -> Option<Track> {
    let id = v["id"].as_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    let name = v["name"].as_str().unwrap_or("Unknown").to_string();
    let artists = v["artists"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let artist_ids = v["artists"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["id"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let album = v["album"]["name"].as_str().unwrap_or("").to_string();
    let album_id = v["album"]["id"].as_str().unwrap_or("").to_string();
    let duration_ms = v["duration_ms"].as_u64().unwrap_or(0);
    let uri = v["uri"].as_str().unwrap_or("").to_string();
    let album_art = v["album"]["images"]
        .as_array()
        .and_then(|images| images.first())
        .and_then(|image| image["url"].as_str())
        .unwrap_or("")
        .to_string();
    let is_playable = v["is_playable"].as_bool().unwrap_or(true);
    Some(Track {
        id,
        name,
        artists,
        artist_ids,
        album,
        album_id,
        duration_ms,
        uri,
        album_art,
        added_at: None,
        is_playable,
    })
}

pub fn parse_playlist(v: &Value) -> Option<Playlist> {
    let id = v["id"].as_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    let name = v["name"].as_str().unwrap_or("Untitled").to_string();
    let tracks = v["tracks"]["total"]
        .as_u64()
        .or_else(|| v["items"]["total"].as_u64())
        .or_else(|| v["tracks"]["items"].as_array().map(|a| a.len() as u64))
        .or_else(|| v["items"].as_array().map(|a| a.len() as u64))
        .unwrap_or(0) as u32;
    let owner = v["owner"]["display_name"]
        .as_str()
        .or_else(|| v["owner"]["id"].as_str())
        .unwrap_or("")
        .to_string();
    let uri = v["uri"].as_str().unwrap_or("").to_string();
    let image = pick_thumb(&v["images"]);
    Some(Playlist { id, name, tracks, owner, uri, image })
}

/// From a Spotify `images` array, the smallest image that is still at least 60px
/// wide (sizes can be null for user-uploaded covers — treated as large enough).
/// Library rows draw covers at ~32px, so this avoids pulling 640px mosaics.
pub fn pick_thumb(images: &Value) -> String {
    let Some(list) = images.as_array() else { return String::new() };
    list.iter()
        .filter_map(|i| Some((i["url"].as_str()?, i["width"].as_u64().unwrap_or(u64::MAX))))
        .filter(|(_, w)| *w >= 60)
        .min_by_key(|(_, w)| *w)
        .or_else(|| list.first().and_then(|i| Some((i["url"].as_str()?, 0))))
        .map(|(u, _)| u.to_string())
        .unwrap_or_default()
}

pub fn parse_artist(v: &Value) -> Option<Artist> {
    let id = v["id"].as_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    let name = v["name"].as_str().unwrap_or("Unknown").to_string();
    let followers = v["followers"]["total"].as_u64().unwrap_or(0) as u32;
    let uri = v["uri"].as_str().unwrap_or("").to_string();
    let image = v["images"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i["url"].as_str())
        .unwrap_or("")
        .to_string();
    let thumb = pick_thumb(&v["images"]);
    Some(Artist { id, name, followers, uri, image, thumb })
}

pub fn parse_album(v: &Value) -> Option<Album> {
    let id = v["id"].as_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    let name = v["name"].as_str().unwrap_or("Untitled").to_string();
    let artists = v["artists"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let tracks = v["total_tracks"].as_u64().unwrap_or(0) as u32;
    let uri = v["uri"].as_str().unwrap_or("").to_string();
    let image = v["images"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i["url"].as_str())
        .unwrap_or("")
        .to_string();
    let thumb = pick_thumb(&v["images"]);
    Some(Album { id, name, artists, tracks, uri, image, thumb })
}

pub fn parse_playback(v: &Value) -> PlaybackState {
    let track = parse_track(&v["item"]);
    PlaybackState {
        is_playing: v["is_playing"].as_bool().unwrap_or(false),
        progress_ms: v["progress_ms"].as_u64().unwrap_or(0),
        duration_ms: track
            .as_ref()
            .map(|t| t.duration_ms)
            .unwrap_or_else(|| v["item"]["duration_ms"].as_u64().unwrap_or(0)),
        shuffle_state: v["shuffle_state"].as_bool().unwrap_or(false),
        repeat_state: v["repeat_state"].as_str().unwrap_or("off").to_string(),
        volume_percent: v["device"]["volume_percent"].as_u64().unwrap_or(100) as u32,
        device_name: v["device"]["name"].as_str().unwrap_or("").to_string(),
        device_id: v["device"]["id"].as_str().unwrap_or("").to_string(),
        track,
        context_uri: v["context"]["uri"].as_str().map(|s| s.to_string()),
    }
}

// ── Auth ─────────────────────────────────────────────────────────────────────

/// Refresh an access token using a refresh token (PKCE public client).
pub async fn refresh_access_token(
    http: &reqwest::Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenInfo, String> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let resp = http
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Token refresh HTTP error: {e}"))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Token refresh failed: {body}"));
    }
    let mut body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Token refresh parse error: {e}"))?;
    // Spotify may not return a new refresh_token — preserve the old one.
    if body.get("refresh_token").and_then(|v| v.as_str()).is_none() {
        body["refresh_token"] = Value::String(refresh_token.to_string());
    }
    parse_token_response(&body)
}

fn parse_token_response(body: &Value) -> Result<TokenInfo, String> {
    let access_token = body["access_token"].as_str().ok_or("Missing access_token")?.to_string();
    let refresh_token = body["refresh_token"].as_str().ok_or("Missing refresh_token")?.to_string();
    let expires_in = body["expires_in"].as_i64().unwrap_or(3600);
    let scope = body["scope"].as_str().unwrap_or(SCOPES_FALLBACK).to_string();
    let token_type = body["token_type"].as_str().unwrap_or("Bearer").to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Ok(TokenInfo {
        access_token,
        token_type,
        expires_in,
        refresh_token,
        scope,
        expires_at: now + expires_in,
        cached_user_id: String::new(),
        cached_display_name: String::new(),
    })
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

/// Append `?device_id=` (or `&device_id=`) to a player API path — the shipped
/// host's `with_device_query`.
///
/// Why every player call needs this: `me/player/play` and friends act on whatever
/// Spotify currently considers the *active* device, and that flag is both laggy and
/// easily lost (it clears after idle, or when anything else touches the account).
/// Naming the device explicitly is what makes a transport call land on Lightify's
/// own engine instead of 404-ing with NO_ACTIVE_DEVICE.
// NOTE: the untargeted player helpers that used to live here (play_uris,
// play_context, play_context_start, set_shuffle, set_repeat, add_to_queue) were
// removed on 2026-09-12. Each built a `me/player/...` request with no `device_id`,
// which is exactly how playback broke: Spotify then acts on whatever it currently
// considers "active", a flag that lags by seconds and clears after idle. Player
// requests are built by `Session::player_put` / `player_post` now, which attach
// `?device_id=` and re-resolve once on a no-device error. Don't add another one here.

pub fn with_device_query(path: &str, device_id: Option<&str>) -> String {
    match device_id {
        Some(id) if !id.is_empty() => {
            let sep = if path.contains('?') { '&' } else { '?' };
            format!("{path}{sep}device_id={id}")
        }
        _ => path.to_string(),
    }
}

/// Does this error mean "Spotify has no device to play on"? Ported from the host's
/// `is_no_device_error`, including its 404 catch-all — Spotify reports a stale device
/// id as a plain 404 rather than a typed error.
pub fn is_no_device_error(e: &str) -> bool {
    e.contains("NO_ACTIVE_DEVICE")
        || e.contains("Device not found")
        || e.contains(" 404")
        || e.contains("404 Not Found")
}

fn url_for(path: &str) -> String {
    if path.starts_with("http") {
        path.to_string()
    } else {
        format!("{BASE}/{}", path.trim_start_matches('/'))
    }
}

/// Build the error string for a non-success response. A 429 is rendered as a
/// recognizable "Rate limited by Spotify — retry in {n}s" carrying the server's
/// `Retry-After` (defaulting to 5s) so the caller can back off instead of hammering
/// the endpoint (which only prolongs the limit). Consumes the response.
async fn api_error(verb: &str, path: &str, resp: reqwest::Response) -> String {
    let status = resp.status();
    if status.as_u16() == 429 {
        let retry = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        // Starts the process-wide cool-down, so every other requester (other
        // `Session`s, background tasks) stands down too instead of re-tripping it.
        let secs = ratelimit::note_429(retry);
        return ratelimit::rate_limited_error(secs);
    }
    let body = resp.text().await.unwrap_or_default();
    format!("{verb} {path} {status}: {body}")
}

pub use ratelimit::is_rate_limited;

/// Send one Web-API request through the process-wide rate gate: fails fast (no
/// network) during a cool-down, and clears the 429 streak on any other answer.
/// Every `api.spotify.com` call made with the user's token must go through here.
async fn send_api(
    verb: &str,
    path: &str,
    req: reqwest::RequestBuilder,
) -> Result<reqwest::Response, String> {
    ratelimit::admit()?;
    let resp = req.send().await.map_err(|e| {
        if crate::net::is_connectivity(&e) {
            crate::net::note_down();
            format!("{}{verb} {path}: {e}", crate::net::OFFLINE_PREFIX)
        } else {
            format!("{verb} {path} error: {e}")
        }
    })?;
    // Any response at all — even an error status — means Spotify is reachable.
    crate::net::note_up();
    if resp.status().as_u16() != 429 {
        ratelimit::note_ok();
    }
    Ok(resp)
}

/// GET returning parsed JSON. `None` on 204 (No Content — e.g. no active device).
pub async fn sp_get(
    http: &reqwest::Client,
    token: &str,
    path: &str,
) -> Result<Option<Value>, String> {
    let resp = send_api("GET", path, http.get(url_for(path)).bearer_auth(token)).await?;
    if resp.status().as_u16() == 204 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(api_error("GET", path, resp).await);
    }
    let v = resp.json::<Value>().await.map_err(|e| format!("GET {path} parse: {e}"))?;
    Ok(Some(v))
}

/// PUT with no body (transport commands: play/pause/seek/volume/…).
pub async fn sp_put(http: &reqwest::Client, token: &str, path: &str) -> Result<(), String> {
    let req = http.put(url_for(path)).bearer_auth(token).header("Content-Length", "0");
    let resp = send_api("PUT", path, req).await?;
    if !resp.status().is_success() && resp.status().as_u16() != 204 {
        return Err(api_error("PUT", path, resp).await);
    }
    Ok(())
}

/// PUT with a JSON body (start playback with uris / context).
pub async fn sp_put_body(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    body: Value,
) -> Result<(), String> {
    let resp = send_api("PUT", path, http.put(url_for(path)).bearer_auth(token).json(&body)).await?;
    if !resp.status().is_success() && resp.status().as_u16() != 204 {
        return Err(api_error("PUT", path, resp).await);
    }
    Ok(())
}

/// POST with no body (next/previous).
pub async fn sp_post(http: &reqwest::Client, token: &str, path: &str) -> Result<(), String> {
    let req = http.post(url_for(path)).bearer_auth(token).header("Content-Length", "0");
    let resp = send_api("POST", path, req).await?;
    if !resp.status().is_success() && resp.status().as_u16() != 204 {
        return Err(api_error("POST", path, resp).await);
    }
    Ok(())
}

/// POST with a JSON body, returning the response JSON (creating a playlist).
pub async fn sp_post_body(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    body: Value,
) -> Result<Option<Value>, String> {
    let resp = send_api("POST", path, http.post(url_for(path)).bearer_auth(token).json(&body)).await?;
    if resp.status().as_u16() == 204 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(api_error("POST", path, resp).await);
    }
    let v = resp.json::<Value>().await.map_err(|e| format!("POST {path} parse: {e}"))?;
    Ok(Some(v))
}

/// DELETE with no body (un-save a track: `me/tracks?ids=`).
pub async fn sp_delete(http: &reqwest::Client, token: &str, path: &str) -> Result<(), String> {
    let req = http.delete(url_for(path)).bearer_auth(token).header("Content-Length", "0");
    let resp = send_api("DELETE", path, req).await?;
    if !resp.status().is_success() && resp.status().as_u16() != 204 {
        return Err(api_error("DELETE", path, resp).await);
    }
    Ok(())
}

/// All of the user's playlists (follows pagination).
pub async fn get_playlists(http: &reqwest::Client, token: &str) -> Result<Vec<Playlist>, String> {
    let mut out = Vec::new();
    let mut next = Some("me/playlists?limit=50".to_string());
    while let Some(path) = next {
        let v = match sp_get(http, token, &path).await? {
            Some(v) => v,
            None => break,
        };
        if let Some(items) = v["items"].as_array() {
            for it in items {
                if let Some(pl) = parse_playlist(it) {
                    out.push(pl);
                }
            }
        }
        next = v["next"].as_str().map(|s| s.to_string());
    }
    Ok(out)
}

/// Current playback state. `None` when nothing is active (204).
pub async fn get_playback(
    http: &reqwest::Client,
    token: &str,
) -> Result<Option<PlaybackState>, String> {
    match sp_get(http, token, "me/player").await? {
        Some(v) => Ok(Some(parse_playback(&v))),
        None => Ok(None),
    }
}

/// Track objects are wrapped inside each page item. `me/tracks` and the legacy
/// `/playlists/{id}/tracks` use `track`; the current `/playlists/{id}/items` uses
/// `item` (generic, since playlists can also hold episodes). Handle both.
fn parse_track_items(v: &Value) -> Vec<Track> {
    v["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|it| {
                    let wrapped = if it["track"].is_object() { &it["track"] } else { &it["item"] };
                    let mut t = parse_track(wrapped)?;
                    // The page item (not the track) carries when it was added to the
                    // playlist / saved — the key the library "Recent" sort orders by.
                    t.added_at = it["added_at"].as_str().map(|s| s.to_string());
                    Some(t)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One page of a playlist's tracks, mirroring the host's fallback chain:
///   1. user token `/playlists/{id}/items` (works for owned playlists)
///   2. user token `/playlists/{id}` full object (tracks under `tracks.items`),
///      but only when `tracks` isn't null (dev-mode returns null for non-owned)
///   3. anonymous Spotify web-player token (bypasses dev-mode limits for public playlists)
/// (The host also has an HTML-scrape last resort; deferred — see PARITY.md.)
pub async fn get_playlist_tracks(
    http: &reqwest::Client,
    token: &str,
    playlist_id: &str,
    limit: u32,
    offset: u32,
) -> Result<Vec<Track>, String> {
    // 1. user token /items — authoritative for owned playlists (return even if empty=end).
    let items_path =
        format!("playlists/{playlist_id}/items?limit={limit}&offset={offset}&market=from_token");
    match sp_get(http, token, &items_path).await {
        Ok(Some(v)) => return Ok(parse_track_items(&v)),
        Err(e) if is_rate_limited(&e) => return Err(e),
        _ => {}
    }
    // 2. user token full playlist — authoritative only when tracks is non-null.
    let full_path =
        format!("playlists/{playlist_id}?limit={limit}&offset={offset}&market=from_token");
    match sp_get(http, token, &full_path).await {
        Ok(Some(v)) if !v["tracks"].is_null() => return Ok(parse_track_items(&v["tracks"])),
        Err(e) if is_rate_limited(&e) => return Err(e),
        _ => {}
    }
    // 3. anonymous web-player token (public / editorial playlists).
    get_playlist_tracks_anon(http, playlist_id, limit, offset).await
}

/// Set once the anonymous web-player token endpoint refuses us. Spotify deprecated
/// it (401/403 for everyone), so without this every non-owned playlist page paid for
/// a guaranteed-to-fail round trip, every time.
static ANON_TOKEN_DEAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Fetch a short-lived anonymous Spotify web-player access token.
async fn get_anon_token(http: &reqwest::Client) -> Result<String, String> {
    use std::sync::atomic::Ordering;
    if ANON_TOKEN_DEAD.load(Ordering::Relaxed) {
        return Err("anon token: endpoint unavailable (deprecated)".to_string());
    }
    let resp = http
        .get("https://open.spotify.com/get_access_token?reason=transport&productType=web_player")
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 Lightify/2.0")
        .send()
        .await
        .map_err(|e| format!("anon token: {e}"))?;
    if matches!(resp.status().as_u16(), 401 | 403) {
        ANON_TOKEN_DEAD.store(true, Ordering::Relaxed);
    }
    let v: Value = resp
        .error_for_status()
        .map_err(|e| format!("anon token status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("anon token json: {e}"))?;
    v["accessToken"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "no accessToken in web-player response".to_string())
}

/// Playlist tracks via the anonymous web-player token (no user market).
pub async fn get_playlist_tracks_anon(
    http: &reqwest::Client,
    playlist_id: &str,
    limit: u32,
    offset: u32,
) -> Result<Vec<Track>, String> {
    let anon = get_anon_token(http).await?;
    let v: Value = http
        .get(format!(
            "https://api.spotify.com/v1/playlists/{playlist_id}/items?limit={limit}&offset={offset}"
        ))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {anon}"))
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 Lightify/2.0")
        .send()
        .await
        .map_err(|e| format!("anon items: {e}"))?
        .error_for_status()
        .map_err(|e| format!("anon items status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("anon items json: {e}"))?;
    Ok(parse_track_items(&v))
}

/// One page of the user's saved ("Liked Songs") tracks.
/// One page of saved tracks, plus Spotify's reported `total` so the UI can show
/// "N / TOTAL" and know when to stop. `me/tracks` caps `limit` at 50 for this
/// dev-mode token (100 answers "Invalid limit").
pub async fn get_saved_tracks_page(
    http: &reqwest::Client,
    token: &str,
    limit: u32,
    offset: u32,
) -> Result<TrackPage, String> {
    let path = format!("me/tracks?limit={limit}&offset={offset}&market=from_token");
    match sp_get(http, token, &path).await? {
        Some(v) => {
            let total = v["total"].as_u64().unwrap_or(0) as u32;
            Ok(TrackPage::api(parse_track_items(&v), total))
        }
        None => Ok(TrackPage::api(vec![], 0)),
    }
}

/// One page of a track list, plus how it was obtained. `partial` marks rows that
/// came from the public embed page instead of the Web API: those lists stop at the
/// ~100 tracks the embed exposes, so the UI must say so rather than implying the
/// playlist is that short.
#[derive(Debug, Clone)]
pub struct TrackPage {
    pub tracks: Vec<Track>,
    pub total: u32,
    pub partial: bool,
}

impl TrackPage {
    fn api(tracks: Vec<Track>, total: u32) -> Self {
        Self { tracks, total, partial: false }
    }
    fn embed(tracks: Vec<Track>, total: u32) -> Self {
        Self { tracks, total, partial: true }
    }
}

/// One track row out of Spotify's embed bootstrap. The embed payload is much
/// thinner than the Web API's (no album, no artist ids, no `added_at`), so only the
/// fields the track list actually renders are filled in.
fn parse_embed_track(entry: &Value) -> Option<Track> {
    if entry["entityType"].as_str().unwrap_or("track") != "track" {
        return None;
    }
    let uri = entry["uri"].as_str()?.to_string();
    let id = uri.rsplit(':').next().unwrap_or_default().to_string();
    if id.is_empty() {
        return None;
    }
    Some(Track {
        id,
        name: entry["title"].as_str().unwrap_or("Unknown").to_string(),
        // The embed separates artists with non-breaking spaces.
        artists: entry["subtitle"].as_str().unwrap_or("").replace('\u{00a0}', " "),
        artist_ids: Vec::new(),
        album: String::new(),
        album_id: String::new(),
        duration_ms: entry["duration"].as_u64().unwrap_or(0),
        uri,
        album_art: String::new(),
        added_at: None,
        is_playable: true,
    })
}

/// Last-resort track list for a playlist the Web API refuses. Spotify's **public
/// embed page** ships the track list in its `__NEXT_DATA__` bootstrap, so a plain
/// unauthenticated GET reaches playlists that `/items` 403s for — every playlist
/// this dev-mode token doesn't own. Ports the host's
/// `fetch_spotify_embed_playlist_page`; no auth and, unlike the Beatport "verify"
/// flow, no WebView, so it is portable to the shell.
///
/// The embed exposes only the first ~100 tracks, so the page it returns is flagged
/// partial and the caller says so rather than implying the list is complete.
pub async fn get_playlist_tracks_embed(
    http: &reqwest::Client,
    playlist_id: &str,
    limit: u32,
    offset: u32,
) -> Result<(Vec<Track>, u32), String> {
    let url = format!("https://open.spotify.com/embed/playlist/{playlist_id}");
    let html = http
        .get(&url)
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 Lightify/2.0")
        .send()
        .await
        .map_err(|e| format!("Spotify embed playlist: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Spotify embed playlist status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Spotify embed playlist html: {e}"))?;

    let re = regex::Regex::new(r#"(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>"#)
        .map_err(|e| format!("embed regex: {e}"))?;
    let raw = re
        .captures(&html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .ok_or_else(|| "Spotify embed playlist missing bootstrap data".to_string())?;
    let payload: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Spotify embed playlist bootstrap json: {e}"))?;

    let list = payload["props"]["pageProps"]["state"]["data"]["entity"]["trackList"]
        .as_array()
        .ok_or_else(|| "Spotify embed playlist missing trackList".to_string())?;

    let total = list.len() as u32;
    let start = offset.min(total) as usize;
    let end = offset.saturating_add(limit).min(total) as usize;
    let tracks = list[start..end].iter().filter_map(parse_embed_track).collect();
    Ok((tracks, total))
}

/// One page of a playlist's tracks + its `total`. Same three-way fallback as
/// `get_playlist_tracks` (user `/items` → full playlist → anonymous web token);
/// only the first two can report a total, so the fallback reports 0 and the caller
/// falls back to "stop when a short page arrives".
pub async fn get_playlist_tracks_page(
    http: &reqwest::Client,
    token: &str,
    playlist_id: &str,
    limit: u32,
    offset: u32,
) -> Result<TrackPage, String> {
    let items_path =
        format!("playlists/{playlist_id}/items?limit={limit}&offset={offset}&market=from_token");
    // A rate limit is NOT "this endpoint refuses this playlist": falling through to
    // the next rung on a 429 used to spend two more Web-API requests into the same
    // limit, then scrape the embed and show its partial ~100 rows as if that were
    // the playlist. Surface the limit instead so the caller backs off.
    match sp_get(http, token, &items_path).await {
        Ok(Some(v)) => {
            let total = v["total"].as_u64().unwrap_or(0) as u32;
            return Ok(TrackPage::api(parse_track_items(&v), total));
        }
        Err(e) if is_rate_limited(&e) => return Err(e),
        _ => {}
    }
    let full_path =
        format!("playlists/{playlist_id}?limit={limit}&offset={offset}&market=from_token");
    match sp_get(http, token, &full_path).await {
        Ok(Some(v)) if !v["tracks"].is_null() => {
            let total = v["tracks"]["total"].as_u64().unwrap_or(0) as u32;
            return Ok(TrackPage::api(parse_track_items(&v["tracks"]), total));
        }
        Err(e) if is_rate_limited(&e) => return Err(e),
        _ => {}
    }
    // The anonymous web-player token is deprecated (403 for everyone, the shipped
    // app included), so the public embed page is the real last resort.
    match get_playlist_tracks_embed(http, playlist_id, limit, offset).await {
        Ok((tracks, total)) if !tracks.is_empty() => Ok(TrackPage::embed(tracks, total)),
        embed => {
            // Try the old anonymous web-player token too, even though Spotify
            // deprecated it (403 for everyone, the shipped app included) — if it ever
            // works again it carries richer rows than the embed.
            if let Ok(items) = get_playlist_tracks_anon(http, playlist_id, limit, offset).await {
                if !items.is_empty() {
                    return Ok(TrackPage::api(items, 0));
                }
            }
            embed.map(|(tracks, total)| TrackPage::embed(tracks, total))
        }
    }
}

pub async fn get_saved_tracks(
    http: &reqwest::Client,
    token: &str,
    limit: u32,
    offset: u32,
) -> Result<Vec<Track>, String> {
    let path = format!("me/tracks?limit={limit}&offset={offset}&market=from_token");
    match sp_get(http, token, &path).await? {
        Some(v) => Ok(parse_track_items(&v)),
        None => Ok(vec![]),
    }
}

/// Search tracks/artists/albums in one request (matches the host's general search).
pub async fn search_general(http: &reqwest::Client, token: &str, q: &str) -> Result<Value, String> {
    let req = http
        .get(format!("{BASE}/search"))
        .bearer_auth(token)
        .query(&[("q", q), ("type", "track,artist,album"), ("limit", "10"), ("market", "from_token")]);
    let resp = send_api("GET", "search", req).await?;
    // Through `api_error`, not a hand-rolled message: a 429 here used to come back as
    // "search 429 Too Many Requests: …", which the shell's backoff never recognised.
    if !resp.status().is_success() {
        return Err(api_error("GET", "search", resp).await);
    }
    resp.json().await.map_err(|e| format!("search json: {e}"))
}

/// Track-only search with an explicit query (for Beatport → Spotify matching,
/// which uses `track:`/`artist:` field filters). Returns parsed tracks.
///
/// `limit` used to be clamped to 50, not `SEARCH_MAX_LIMIT` (10) — harmless for its
/// one caller (which already passes 8), but a live 400 waiting for the next one.
pub async fn search_tracks(
    http: &reqwest::Client,
    token: &str,
    q: &str,
    limit: u32,
) -> Result<Vec<Track>, String> {
    let (tracks, _total) = search_tracks_page(http, token, q, limit, 0).await?;
    Ok(tracks)
}

/// Like `search_tracks`, but with an offset — depth for a track-only search has to
/// come from paging (see `SEARCH_MAX_LIMIT`), not a bigger `limit`. Returns the page
/// plus Spotify's reported total, so a caller can keep paging until it has enough.
pub async fn search_tracks_page(
    http: &reqwest::Client,
    token: &str,
    q: &str,
    limit: u32,
    offset: u32,
) -> Result<(Vec<Track>, u32), String> {
    let limit = limit.clamp(1, SEARCH_MAX_LIMIT).to_string();
    let offset_s = offset.to_string();
    let req = http.get(format!("{BASE}/search")).bearer_auth(token).query(&[
        ("q", q),
        ("type", "track"),
        ("limit", &limit),
        ("offset", &offset_s),
        ("market", "from_token"),
    ]);
    let resp = send_api("GET", "search", req).await?;
    if !resp.status().is_success() {
        return Err(api_error("GET", "search", resp).await);
    }
    let v: Value = resp.json().await.map_err(|e| format!("tsearch json: {e}"))?;
    let total = v["tracks"]["total"].as_u64().unwrap_or(0) as u32;
    let tracks = v["tracks"]["items"]
        .as_array()
        .map(|a| a.iter().filter_map(parse_track).collect())
        .unwrap_or_default();
    Ok((tracks, total))
}

/// One page of a single-type search (`track` | `artist` | `album` | `playlist`),
/// parsed, plus Spotify's reported total for that type — how every search filter
/// pages deeper than the first 10 (see `SEARCH_MAX_LIMIT`). Playlist search takes no
/// market, like the dedicated playlist request below.
pub async fn search_type_page(
    http: &reqwest::Client,
    token: &str,
    q: &str,
    kind: &str,
    offset: u32,
) -> Result<(crate::SearchResults, u32), String> {
    let limit = SEARCH_MAX_LIMIT.to_string();
    let offset_s = offset.to_string();
    let mut params = vec![("q", q), ("type", kind), ("limit", limit.as_str()), ("offset", offset_s.as_str())];
    if kind != "playlist" {
        params.push(("market", "from_token"));
    }
    let req = http.get(format!("{BASE}/search")).bearer_auth(token).query(&params);
    let resp = send_api("GET", "search", req).await?;
    if !resp.status().is_success() {
        return Err(api_error("GET", "search", resp).await);
    }
    let v: Value = resp.json().await.map_err(|e| format!("search page json: {e}"))?;
    let key = format!("{kind}s");
    let total = v[&key]["total"].as_u64().unwrap_or(0) as u32;
    Ok((parse_search_results(&v), total))
}

/// Parse whichever of `tracks` / `artists` / `albums` / `playlists` a search
/// response carries, with a small cover URL per track (search rows draw thumbnails).
pub fn parse_search_results(v: &Value) -> crate::SearchResults {
    let items = |k: &str| v[k]["items"].as_array().cloned().unwrap_or_default();
    let mut out = crate::SearchResults::default();
    for t in items("tracks") {
        if let Some(track) = parse_track(&t) {
            let thumb = pick_thumb(&t["album"]["images"]);
            if !thumb.is_empty() {
                out.track_thumbs.insert(track.id.clone(), thumb);
            }
            out.tracks.push(track);
        }
    }
    out.artists = items("artists").iter().filter_map(parse_artist).collect();
    out.albums = items("albums").iter().filter_map(parse_album).collect();
    out.playlists = items("playlists").iter().filter_map(parse_playlist).collect();
    out
}

/// Playlist search is a separate request (no market — playlist search ignores it).
/// One page of playlist search hits + Spotify's reported total. The original pages
/// search results **only** under the PLAYLISTS filter (`searchResultsPagerIsActive`),
/// so this is the only search endpoint that needs an offset.
/// `limit` must stay <= 10: this client id rejects more with "Invalid limit".
pub async fn search_playlists_page(
    http: &reqwest::Client,
    token: &str,
    q: &str,
    limit: u32,
    offset: u32,
) -> Result<(Vec<Playlist>, u32), String> {
    let req = http.get(format!("{BASE}/search")).bearer_auth(token).query(&[
        ("q", q),
        ("type", "playlist"),
        ("limit", &limit.to_string()),
        ("offset", &offset.to_string()),
    ]);
    let resp = send_api("GET", "search", req).await?;
    if !resp.status().is_success() {
        return Err(api_error("GET", "search", resp).await);
    }
    let v: Value = resp.json().await.map_err(|e| format!("psearch parse: {e}"))?;
    let total = v["playlists"]["total"].as_u64().unwrap_or(0) as u32;
    let items = v["playlists"]["items"]
        .as_array()
        .map(|a| a.iter().filter_map(parse_playlist).collect())
        .unwrap_or_default();
    Ok((items, total))
}

pub async fn search_playlists(http: &reqwest::Client, token: &str, q: &str) -> Result<Value, String> {
    let req = http
        .get(format!("{BASE}/search"))
        .bearer_auth(token)
        .query(&[("q", q), ("type", "playlist"), ("limit", "10")]);
    let resp = send_api("GET", "search", req).await?;
    if !resp.status().is_success() {
        return Err(api_error("GET", "search", resp).await);
    }
    resp.json().await.map_err(|e| format!("psearch json: {e}"))
}

/// An album's tracks via `albums/{id}` (simplified items lack an `album` field, so we
/// inject the album's art/name — like the host's `parse_album_tracks`).
pub async fn get_album_tracks(
    http: &reqwest::Client,
    token: &str,
    album_id: &str,
) -> Result<Vec<Track>, String> {
    let v = sp_get(http, token, &format!("albums/{album_id}?market=from_token"))
        .await?
        .ok_or("no album response")?;
    let album_art = v["images"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i["url"].as_str())
        .unwrap_or("")
        .to_string();
    let album_name = v["name"].as_str().unwrap_or("").to_string();
    let album_id = v["id"].as_str().unwrap_or("").to_string();
    Ok(v["tracks"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|t| {
                    let id = t["id"].as_str()?.to_string();
                    if id.is_empty() {
                        return None;
                    }
                    Some(Track {
                        id,
                        name: t["name"].as_str().unwrap_or("Unknown").to_string(),
                        artists: t["artists"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default(),
                        artist_ids: t["artists"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|x| x["id"].as_str().map(|s| s.to_string())).collect())
                            .unwrap_or_default(),
                        album: album_name.clone(),
                        album_id: album_id.clone(),
                        duration_ms: t["duration_ms"].as_u64().unwrap_or(0),
                        uri: t["uri"].as_str().unwrap_or("").to_string(),
                        album_art: album_art.clone(),
                        added_at: None,
                        is_playable: t["is_playable"].as_bool().unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Available Spotify Connect devices (`me/player/devices`).
pub async fn get_devices(http: &reqwest::Client, token: &str) -> Result<Vec<Device>, String> {
    match sp_get(http, token, "me/player/devices").await? {
        Some(v) => Ok(v["devices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| {
                        let id = d["id"].as_str()?.to_string();
                        Some(Device {
                            id,
                            name: d["name"].as_str().unwrap_or("Unknown").to_string(),
                            is_active: d["is_active"].as_bool().unwrap_or(false),
                            kind: d["type"].as_str().unwrap_or("").to_string(),
                            volume_percent: d["volume_percent"].as_u64().unwrap_or(0) as u32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()),
        None => Ok(vec![]),
    }
}

/// Transfer playback to a device (`PUT me/player`). `play` resumes immediately.
pub async fn transfer_playback(
    http: &reqwest::Client,
    token: &str,
    device_id: &str,
    play: bool,
) -> Result<(), String> {
    let body = serde_json::json!({ "device_ids": [device_id], "play": play });
    sp_put_body(http, token, "me/player", body).await
}

/// The user's up-next playback queue (`me/player/queue`). Returns the upcoming
/// items (not the currently-playing head), mirroring what the original sidebar shows.
pub async fn get_queue(http: &reqwest::Client, token: &str) -> Result<Vec<Track>, String> {
    match sp_get(http, token, "me/player/queue").await? {
        Some(v) => Ok(v["queue"]
            .as_array()
            .map(|a| a.iter().filter_map(parse_track).collect())
            .unwrap_or_default()),
        None => Ok(vec![]),
    }
}

/// Recently-played tracks (`me/player/recently-played`), newest first. Each item
/// carries a `played_at` timestamp, kept in `Track.added_at` for the relative time.
pub async fn get_recently_played(
    http: &reqwest::Client,
    token: &str,
    limit: u32,
) -> Result<Vec<Track>, String> {
    let path = format!("me/player/recently-played?limit={}", limit.min(50));
    match sp_get(http, token, &path).await? {
        Some(v) => Ok(v["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|it| {
                        let mut t = parse_track(&it["track"])?;
                        t.added_at = it["played_at"].as_str().map(|s| s.to_string());
                        Some(t)
                    })
                    .collect()
            })
            .unwrap_or_default()),
        None => Ok(vec![]),
    }
}

/// Toggle shuffle (`me/player/shuffle?state=`).

/// Set repeat mode (`me/player/repeat?state=`): `off` | `context` | `track`.

/// Append a track URI to the up-next queue (`me/player/queue?uri=`).

/// Minimal percent-encoding for the characters that appear in a Spotify URI query
/// value (`spotify:track:<id>` → the colons must be escaped).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Start playback of an explicit list of track URIs (used for Liked Songs / no context).

/// Start playback of a context (playlist/album) at a specific track — so "next"
/// continues through the context, like the original.

/// Play a context from its first track (`{"context_uri": …, "offset": {"position": 0}}`).
/// The host's `playPlaylistFromStart` fetches page 0 and starts on that track; asking
/// Spotify for position 0 is the same thing without the extra round trip.

/// Create a private playlist and fill it with `uris` — the host's
/// `cmd_create_playlist`. Note the **`items`** endpoint, not `tracks`: this
/// dev-mode app sees the newer playlist API (the same reason reading a playlist
/// wraps each entry under `item`).
pub async fn create_playlist(
    http: &reqwest::Client,
    token: &str,
    name: &str,
    uris: &[String],
) -> Result<Playlist, String> {
    let created = sp_post_body(
        http,
        token,
        "me/playlists",
        serde_json::json!({ "name": name, "public": false }),
    )
    .await?
    .ok_or_else(|| "Create playlist returned no body".to_string())?;
    let id = created["id"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return Err("Failed to get playlist ID".to_string());
    }
    // Spotify caps an add at 100 URIs per call.
    for chunk in uris.chunks(100) {
        sp_post_body(
            http,
            token,
            &format!("playlists/{id}/items"),
            serde_json::json!({ "uris": chunk }),
        )
        .await?;
    }
    Ok(Playlist {
        name: created["name"].as_str().unwrap_or(name).to_string(),
        id,
        tracks: uris.len() as u32,
        owner: created["owner"]["display_name"]
            .as_str()
            .or_else(|| created["owner"]["id"].as_str())
            .unwrap_or("")
            .to_string(),
        uri: created["uri"].as_str().unwrap_or("").to_string(),
        image: pick_thumb(&created["images"]),
    })
}

// ── Playlist library (follow / unfollow) ─────────────────────────────────────

/// Save a playlist into the user's library — `PUT playlists/{id}/followers`
/// (the host's `cmd_follow_playlist`).
/// Save an album to the library - `PUT me/albums?ids=` (the host's `cmd_save_album`).
pub async fn save_album(
    http: &reqwest::Client,
    token: &str,
    album_id: &str,
) -> Result<(), String> {
    sp_put(http, token, &format!("me/albums?ids={album_id}")).await
}

pub async fn follow_playlist(
    http: &reqwest::Client,
    token: &str,
    playlist_id: &str,
) -> Result<(), String> {
    sp_put(http, token, &format!("playlists/{playlist_id}/followers")).await
}

/// Remove a playlist from the user's library — `DELETE playlists/{id}/followers`
/// (the host's `cmd_unfollow_playlist`). Spotify has no hard delete: "delete
/// playlist" means un-following it, for owned playlists too.
pub async fn unfollow_playlist(
    http: &reqwest::Client,
    token: &str,
    playlist_id: &str,
) -> Result<(), String> {
    sp_delete(http, token, &format!("playlists/{playlist_id}/followers")).await
}

// ── Liked Songs (saved tracks) ───────────────────────────────────────────────

/// Is this track in the user's Liked Songs? Uses the same endpoint as the host's
/// `cmd_check_liked`: `me/library/contains?uris=`. NOT `me/tracks/contains?ids=`,
/// which **403s** under this dev-mode token even though `me/tracks` itself works.
pub async fn check_saved(http: &reqwest::Client, token: &str, track_id: &str) -> Result<bool, String> {
    if track_id.is_empty() {
        return Ok(false);
    }
    let uri = urlencode(&format!("spotify:track:{track_id}"));
    let path = format!("me/library/contains?uris={uri}");
    match sp_get(http, token, &path).await? {
        Some(v) => Ok(v.as_array().and_then(|a| a.first()).and_then(|b| b.as_bool()).unwrap_or(false)),
        None => Ok(false),
    }
}

/// Save (`PUT`) or un-save (`DELETE`) a track in Liked Songs, via the host's
/// endpoint: `me/library?uris=`. Needs `user-library-modify`.
pub async fn set_saved(
    http: &reqwest::Client,
    token: &str,
    track_id: &str,
    saved: bool,
) -> Result<(), String> {
    if track_id.is_empty() {
        return Err("No track".into());
    }
    let uri = urlencode(&format!("spotify:track:{track_id}"));
    let path = format!("me/library?uris={uri}");
    if saved {
        sp_put(http, token, &path).await
    } else {
        sp_delete(http, token, &path).await
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_playlist, pick_thumb};
    use serde_json::json;

    #[test]
    fn thumb_prefers_smallest_that_is_at_least_60px() {
        let imgs = json!([
            { "url": "big", "width": 640 },
            { "url": "mid", "width": 300 },
            { "url": "small", "width": 60 },
        ]);
        assert_eq!(pick_thumb(&imgs), "small");
        let imgs = json!([{ "url": "big", "width": 640 }, { "url": "tiny", "width": 32 }]);
        assert_eq!(pick_thumb(&imgs), "big");
        // User-uploaded covers come with null sizes.
        assert_eq!(pick_thumb(&json!([{ "url": "custom", "width": null }])), "custom");
        assert_eq!(pick_thumb(&json!(null)), "");
        assert_eq!(pick_thumb(&json!([])), "");
    }

    #[test]
    fn playlist_carries_its_cover() {
        let p = parse_playlist(&json!({
            "id": "abc", "name": "Mix", "uri": "spotify:playlist:abc",
            "tracks": { "total": 3 }, "owner": { "display_name": "Jo" },
            "images": [{ "url": "u640", "width": 640 }, { "url": "u60", "width": 60 }]
        }))
        .unwrap();
        assert_eq!(p.image, "u60");
        // Cached libraries from before the field existed still load.
        let old: crate::Playlist =
            serde_json::from_str(r#"{"id":"a","name":"n","tracks":1,"owner":"o","uri":"u"}"#).unwrap();
        assert_eq!(old.image, "");
    }
}
