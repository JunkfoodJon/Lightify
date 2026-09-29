//! On-disk config + token cache. Format-compatible with the shipped host
//! (same file names + fields), so the native shell reuses the SAME cached
//! Spotify session — no re-auth. Copied from the host verbatim on purpose.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const APP_NAME: &str = "Lightify";
const CONFIG_FILE: &str = "lightify_config.json";
const TOKEN_FILE: &str = ".lightify_cache";

/// Persistent app configuration (client_id, preferences).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub audio_output: String,
    #[serde(default)]
    pub download_path: String,
}

/// OAuth token info — matches the spotipy cache format for backward compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub refresh_token: String,
    pub scope: String,
    pub expires_at: i64,
    #[serde(default)]
    pub cached_user_id: String,
    #[serde(default)]
    pub cached_display_name: String,
}

impl TokenInfo {
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        now >= self.expires_at - 60 // 60-second buffer
    }
}

/// Resolve the writable data directory (same rules as the host).
pub fn data_dir() -> PathBuf {
    if let Ok(val) = std::env::var("LIGHTIFY_DATA_DIR") {
        let p = PathBuf::from(val.trim());
        if !p.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(&p);
            return p;
        }
    }
    if let Some(base) = dirs::data_dir() {
        let p = base.join(APP_NAME);
        let _ = std::fs::create_dir_all(&p);
        return p;
    }
    let p = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("."))
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".{}-data", APP_NAME.to_lowercase()));
    let _ = std::fs::create_dir_all(&p);
    p
}

pub fn load_config(dir: &Path) -> Config {
    let path = dir.join(CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn load_token(dir: &Path) -> Option<TokenInfo> {
    let path = dir.join(TOKEN_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read-modify-write the config. Unlike `load_config` (which treats an unreadable
/// file as defaults, fine for *reading* a preference), this refuses to save over a
/// file that exists but doesn't parse — saving defaults there would wipe the user's
/// `client_id` and sign them out of both apps.
pub fn update_config(dir: &Path, f: impl FnOnce(&mut Config)) -> Result<(), String> {
    let path = dir.join(CONFIG_FILE);
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} is unreadable ({e}); not overwriting it", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    f(&mut cfg);
    save_config(dir, &cfg)
}

/// Persist the config file (same shape/location the shipped host reads, so a
/// choice made in either app carries over).
pub fn save_config(dir: &Path, cfg: &Config) -> Result<(), String> {
    let path = dir.join(CONFIG_FILE);
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    write_atomic(&path, text.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn save_token(dir: &Path, token: &TokenInfo) {
    let path = dir.join(TOKEN_FILE);
    if let Ok(json) = serde_json::to_string_pretty(token) {
        let _ = write_atomic(&path, json.as_bytes());
    }
}

/// Write via a sibling temp file + rename, so a reader (the shipped app shares these
/// files) or a crash mid-write never sees a truncated file. A torn config parses as
/// `Config::default()` and the next save would then persist an empty `client_id`;
/// a torn token cache signs the user out. `rename` replaces the target on Windows.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
