//! On-disk config + token cache. Format-compatible with the shipped host
//! (same file names + fields), so the native shell reuses the SAME cached
//! Spotify session — no re-auth. Copied from the host verbatim on purpose.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const APP_NAME: &str = "Lightify";
const CONFIG_FILE: &str = "lightify_config.json";
const TOKEN_FILE: &str = ".lightify_cache";
/// How long to wait before re-reading a config that didn't parse.
const CONFIG_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

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

/// Read-modify-write the config. A file that stays unparseable is moved aside to
/// `lightify_config.json.corrupt` and the update carries on from defaults: refusing to
/// save used to strand a user at the last step of signing in, every time, with nothing
/// in the app able to clear it. `load_config` already reads such a file as defaults, so
/// nothing that was usable is lost, and the bytes are kept for recovery by hand.
pub fn update_config(dir: &Path, f: impl FnOnce(&mut Config)) -> Result<(), String> {
    let path = dir.join(CONFIG_FILE);
    let mut cfg = read_config_for_update(&path)?;
    f(&mut cfg);
    save_config(dir, &cfg)
}

fn read_config_for_update(path: &Path) -> Result<Config, String> {
    let parse = |bytes: &[u8]| serde_json::from_slice::<Config>(bytes).ok();
    let first = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    if let Some(cfg) = parse(&first) {
        return Ok(cfg);
    }
    // The shipped host writes this file in place, so one bad read can be a write in
    // progress. Setting a good file aside would lose the `client_id`: look once more.
    std::thread::sleep(CONFIG_SETTLE);
    if let Some(cfg) = std::fs::read(path).ok().as_deref().and_then(parse) {
        return Ok(cfg);
    }
    let mut aside = path.as_os_str().to_owned();
    aside.push(".corrupt");
    std::fs::rename(path, &aside)
        .map_err(|e| format!("{} is unreadable and couldn't be moved aside ({e}); not overwriting it", path.display()))?;
    eprintln!("[lightify] {} was unreadable; moved it aside and started from defaults", path.display());
    Ok(Config::default())
}

/// Persist the config file (same shape/location the shipped host reads, so a
/// choice made in either app carries over).
pub fn save_config(dir: &Path, cfg: &Config) -> Result<(), String> {
    let path = dir.join(CONFIG_FILE);
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    write_atomic(&path, text.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Save the token, logging a failure. Prefer [`save_token_checked`]: a token that never
/// reached the disk is lost at exit, and its refresh token may be the only valid one.
pub fn save_token(dir: &Path, token: &TokenInfo) {
    if let Err(e) = save_token_checked(dir, token) {
        eprintln!("[lightify] {e}");
    }
}

/// Save the token, saying why when it can't be. The message never holds the token.
pub fn save_token_checked(dir: &Path, token: &TokenInfo) -> Result<(), String> {
    let path = dir.join(TOKEN_FILE);
    let json = serde_json::to_string_pretty(token).map_err(|e| format!("encode the Spotify session: {e}"))?;
    write_atomic(&path, json.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Write via a sibling temp file + rename, so a reader (the shipped app shares these
/// files) or a crash mid-write never sees a truncated file. A torn config parses as
/// `Config::default()` and the next save would then persist an empty `client_id`;
/// a torn token cache signs the user out. `rename` replaces the target on Windows.
///
/// The bytes are flushed to disk before the rename: without that, a power cut can leave
/// the renamed file empty. Each call gets its own temp name, since threads of one
/// process writing the same file used to share one and could rename each other's half
/// of a write into place.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.{call}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let flushed = std::fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    flushed.and_then(|()| std::fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own under the system temp folder, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lightify-core-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn token(access: &str) -> TokenInfo {
        TokenInfo {
            access_token: access.into(),
            token_type: "Bearer".into(),
            expires_in: 3600,
            refresh_token: "r".into(),
            scope: "s".into(),
            expires_at: 1,
            cached_user_id: String::new(),
            cached_display_name: String::new(),
        }
    }

    #[test]
    fn a_corrupt_config_is_moved_aside_and_the_update_carries_on() {
        for (name, junk) in [("text", &b"{ not json"[..]), ("binary", &[0xff, 0xfe, 0x00][..]), ("empty", &b""[..])] {
            let dir = TempDir::new(&format!("corrupt-config-{name}"));
            let path = dir.0.join(CONFIG_FILE);
            std::fs::write(&path, junk).unwrap();
            update_config(&dir.0, |c| c.client_id = "abc".into()).unwrap();
            assert_eq!(load_config(&dir.0).client_id, "abc", "{name}");
            assert_eq!(std::fs::read(dir.0.join("lightify_config.json.corrupt")).unwrap(), junk, "{name}");
        }
    }

    #[test]
    fn update_config_keeps_what_a_good_file_holds() {
        let dir = TempDir::new("good-config");
        update_config(&dir.0, |c| c.client_id = "abc".into()).unwrap();
        update_config(&dir.0, |c| c.audio_output = "Speakers".into()).unwrap();
        let cfg = load_config(&dir.0);
        assert_eq!((cfg.client_id.as_str(), cfg.audio_output.as_str()), ("abc", "Speakers"));
        assert!(!dir.0.join("lightify_config.json.corrupt").exists());
    }

    #[test]
    fn a_failed_token_save_says_so_and_never_shows_the_token() {
        let dir = TempDir::new("token-save");
        save_token_checked(&dir.0, &token("first")).unwrap();
        assert_eq!(load_token(&dir.0).unwrap().access_token, "first");
        // A directory where the file belongs: the rename can't replace it.
        let blocked = TempDir::new("token-save-blocked");
        std::fs::create_dir(blocked.0.join(TOKEN_FILE)).unwrap();
        let e = save_token_checked(&blocked.0, &token("secret-value")).unwrap_err();
        assert!(!e.contains("secret-value"), "{e}");
        let left = std::fs::read_dir(&blocked.0).unwrap().count();
        assert_eq!(left, 1, "the temp file must not stay behind");
    }

    /// Threads of one process share a pid, which used to mean one temp file: a writer
    /// could rename another's half-written bytes into place.
    #[test]
    fn concurrent_writers_never_tear_the_file() {
        let dir = TempDir::new("write-atomic");
        let path = dir.0.join("shared.json");
        const SIZE: usize = 64 * 1024;
        let whole = |got: &[u8]| got.len() == SIZE && got.iter().all(|b| *b == got[0]);
        write_atomic(&path, &vec![b'0'; SIZE]).unwrap();
        let writing = std::sync::atomic::AtomicUsize::new(8);
        std::thread::scope(|scope| {
            for writer in 0u8..8 {
                let (path, writing) = (&path, &writing);
                scope.spawn(move || {
                    let body = vec![b'a' + writer; SIZE];
                    for _ in 0..100 {
                        // Windows can refuse a rename while another is in flight; the
                        // loser keeps the old file, which is the point. Tearing isn't.
                        let _ = write_atomic(path, &body);
                    }
                    writing.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                });
            }
            // What every reader of the shared file sees along the way. A read can fail
            // mid-rename on Windows; one that succeeds must hold one whole write.
            while writing.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                if let Ok(got) = std::fs::read(&path) {
                    assert!(whole(&got), "read a torn file: {} bytes", got.len());
                }
            }
        });
        assert!(whole(&std::fs::read(&path).unwrap()), "ended on a torn file");
        let left = std::fs::read_dir(&dir.0).unwrap().count();
        assert_eq!(left, 1, "temp files left behind");
    }
}
