//! Data models shared with the host (field-compatible subset for the shell).

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub artists: String,
    pub artist_ids: Vec<String>,
    pub album: String,
    pub album_id: String,
    pub duration_ms: u64,
    pub uri: String,
    pub album_art: String,
    pub added_at: Option<String>,
    #[serde(default = "default_true")]
    pub is_playable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub tracks: u32,
    pub owner: String,
    pub uri: String,
    /// Cover image URL, the smallest size Spotify offers that is still ≥ 60px
    /// (library thumbnails). Empty when the playlist has no cover.
    #[serde(default)]
    pub image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub followers: u32,
    pub uri: String,
    pub image: String,
    /// Smallest image >= 60px (search-row thumbnail); `image` is the largest.
    #[serde(default)]
    pub thumb: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artists: String,
    pub tracks: u32,
    pub uri: String,
    pub image: String,
    /// Smallest image >= 60px (search-row thumbnail); `image` is the largest.
    #[serde(default)]
    pub thumb: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub is_active: bool,
    pub kind: String, // Spotify's `type` (Computer / Smartphone / Speaker / …)
    pub volume_percent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub progress_ms: u64,
    pub duration_ms: u64,
    pub shuffle_state: bool,
    pub repeat_state: String,
    pub volume_percent: u32,
    pub device_name: String,
    pub device_id: String,
    pub track: Option<Track>,
    /// The playback context URI (`spotify:playlist:…`, `spotify:album:…`, …) when
    /// playback was started from one. `None` for a bare `uris` play. Drives the
    /// library "now playing here" row highlight.
    #[serde(default)]
    pub context_uri: Option<String>,
}
