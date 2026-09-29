//! OnTheSpot downloader bridge — ported from the shipped Tauri host
//! (`lightify-tauri/src-tauri/src/main.rs`, the `onthespot_*` / `cmd_*_download*`
//! family) so the native shell can actually download instead of showing the
//! "full app only" placeholder it used to.
//!
//! Shape of the thing: OnTheSpot ships a small Flask web app. Lightify bundles it
//! as a standalone `lightify-onthespot(.exe)` and drives it over loopback HTTP on
//! a fixed port. Every call therefore goes: make sure the bridge process is up →
//! `GET /login` for a session cookie → hit the real endpoint with that cookie.
//! The bridge's own on-disk state lives in `ONTHESPOTDIR`, which is set to the
//! *same* directory the shipped host uses, so a downloader account connected in
//! either app serves both.
//!
//! **Not everything the bridge does is loopback-only.** `start_login` (below) asks
//! it to sign in as its own Spotify Connect device ("OnTheSpot", picked from
//! Spotify's device list on another device) — that handshake is real LAN traffic:
//! the vendored OnTheSpot login code opens a listener on `0.0.0.0` on a random
//! port and broadcasts over mDNS so Spotify can find it, which is what actually
//! triggers a Windows Firewall prompt (the Flask control API this module drives
//! never does; it's 127.0.0.1 end to end). That prompt's default "Allow access"
//! dialog only checks **Private networks** — a Public-profile connection (common
//! on an unfamiliar/test network) silently drops the handshake even after the user
//! clicks Allow, and the bridge's own wait for it has no timeout, so the login
//! flag can get stuck "in progress" forever with no way to notice from the
//! outside. `start_login`'s `LOGIN_STUCK_AFTER` backstop below is what breaks that
//! — see its doc comment.
//!
//! Deliberately self-contained: `main.rs` only ever calls the `pub async fn`s
//! here, and the process handle / last snapshot live in this module's statics
//! rather than being threaded through the worker loop. That lets every command
//! run on its own `tokio::spawn` (the bridge's cold start is seconds, and the
//! worker's command loop must not block on it).

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Loopback port the bundled bridge listens on — same value the shipped host
/// uses, so the two apps reuse one running bridge instead of fighting over it.
const PORT: u16 = 57321;
/// How long a cold start may take before we give up on it. The packaged bridge is
/// a PyInstaller one-file build that unpacks its whole Python runtime to a temp
/// dir before Flask even imports — on a slow disk, or with Defender scanning every
/// extracted file, that alone blows well past 10s. The ready loop only waits this
/// long while the process is still alive; one that exits fails immediately.
const READY_TIMEOUT: Duration = Duration::from_secs(45);
const READY_POLL_DELAY_MS: u64 = 250;
/// How many bridge log lines to keep for the "didn't come up" error message.
const LOG_RING: usize = 200;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The bridge's view of the downloader's *own* Spotify account, which is separate
/// from the Web-API OAuth the rest of the app uses (OnTheSpot needs a librespot
/// session of its own to pull audio).
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub spotify_ready: bool,
    pub login_in_progress: bool,
    pub message: String,
    pub error: String,
    pub configured_accounts: u32,
}

impl Status {
    /// The one-line summary the settings panel shows — ports `downloaderStatusText`
    /// (`app.js:4753`).
    pub fn summary(&self) -> String {
        if self.spotify_ready {
            return "Spotify downloader connected".to_string();
        }
        if self.login_in_progress {
            if !self.message.is_empty() {
                return self.message.clone();
            }
            return "Waiting for the downloader sign-in\u{2026}".to_string();
        }
        if !self.error.is_empty() {
            return self.error.clone();
        }
        if self.configured_accounts > 0 {
            "Spotify downloader account needs reconnecting".to_string()
        } else {
            "Spotify downloader is not connected".to_string()
        }
    }
}

/// One row of the bridge's download queue.
#[derive(Debug, Clone, Default)]
pub struct Item {
    pub local_id: String,
    pub name: String,
    pub by: String,
    pub service: String,
    pub status: String,
    pub progress: u32,
    pub file_path: String,
    pub error: String,
}

impl Item {
    /// The statuses that mean "nothing is going to happen to this row any more".
    /// Mirrors the lists `app.js` checks in `buildDownloadItemMenu` /
    /// `updateSidebarClearButton`.
    pub fn finished(&self) -> bool {
        matches!(
            self.status.as_str(),
            "Downloaded" | "Already Exists" | "Cancelled" | "Unavailable" | "Deleted"
        )
    }

    /// A finished row whose file is actually on disk.
    pub fn on_disk(&self) -> bool {
        !self.file_path.is_empty() && matches!(self.status.as_str(), "Downloaded" | "Already Exists")
    }

    pub fn failed(&self) -> bool {
        !self.error.is_empty() || matches!(self.status.as_str(), "Failed" | "Unavailable")
    }

    /// `status • artist • 42%` — ports `renderSidebarDownloads`' summary line.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(if self.status.is_empty() { "Waiting".to_string() } else { self.status.clone() });
        let by = if self.by.is_empty() { self.service.clone() } else { self.by.clone() };
        if !by.is_empty() {
            parts.push(by);
        }
        if self.progress > 0 {
            parts.push(format!("{}%", self.progress));
        }
        parts.join(" \u{2022} ")
    }
}

// ── Process state ───────────────────────────────────────────────────────────

struct Bridge {
    child: Option<Child>,
    started_at: Option<Instant>,
    /// The download root we last pushed into the bridge's settings; `None` forces
    /// a re-sync on the next call.
    synced_path: Option<String>,
}

fn bridge() -> &'static Mutex<Bridge> {
    static BRIDGE: OnceLock<Mutex<Bridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| Mutex::new(Bridge { child: None, started_at: None, synced_path: None }))
}

fn log_ring() -> &'static Arc<Mutex<VecDeque<String>>> {
    static LOG: OnceLock<Arc<Mutex<VecDeque<String>>>> = OnceLock::new();
    LOG.get_or_init(|| Arc::new(Mutex::new(VecDeque::new())))
}

/// The most recent queue snapshot, kept here so the worker can map a clicked row
/// back to its `local_id` without owning the list.
/// Serializes cold starts. Every downloader command runs on its own task, so two
/// of them arriving together (opening Settings and the Downloads panel at the same
/// moment, say) would both observe "no child yet" and both spawn a bridge — one of
/// them orphaned, still holding port 57321. Only `ensure` takes this.
fn start_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn snapshot_cache() -> &'static Mutex<Vec<Item>> {
    static SNAP: OnceLock<Mutex<Vec<Item>>> = OnceLock::new();
    SNAP.get_or_init(|| Mutex::new(Vec::new()))
}

/// The last queue snapshot pushed to the UI. Row indices in the downloads sidebar
/// index into this.
pub fn last_snapshot() -> Vec<Item> {
    snapshot_cache().lock().unwrap().clone()
}

fn stop_child_locked(b: &mut Bridge) {
    b.started_at = None;
    b.synced_path = None;
    if let Some(mut child) = b.child.take() {
        // Graceful first (Windows). The PyInstaller bootloader we spawned deletes its
        // unpacked runtime (%TEMP%\_MEIxxxxx, ~78 MB) when its server child exits —
        // but only if the bootloader itself is still alive to do it. Force-killing the
        // whole tree skipped that, and every bridge stop leaked one of those folders
        // (47 of them, 1.1 GB, found on the dev machine 2026-09-26). So: end the
        // server (and anything it spawned, e.g. an ffmpeg mid-convert) and give the
        // bootloader a moment to clean up and exit on its own; the tree-kill below is
        // only the fallback.
        #[cfg(windows)]
        {
            for pid in proc::descendants(child.id()) {
                proc::terminate(pid);
            }
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        // The packaged bridge is a PyInstaller one-file build: the process we spawned
        // is only the bootloader, and it re-execs the real server as a child of its
        // own. Killing just our handle therefore leaves that grandchild alive, still
        // holding port 57321 — which is what the shipped host does, and why a stray
        // bridge can outlive it. Kill the whole tree instead.
        // Unix: the child was put in its own process group at spawn, so signalling
        // the negative pid takes the bootloader and the server it re-execs together.
        #[cfg(unix)]
        {
            let mut killer = Command::new("kill");
            killer
                .arg("-KILL")
                .arg(format!("-{}", child.id()))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = killer.status();
        }
        #[cfg(windows)]
        {
            let mut killer = Command::new("taskkill");
            killer
                .arg("/PID")
                .arg(child.id().to_string())
                .arg("/T")
                .arg("/F")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            killer.creation_flags(CREATE_NO_WINDOW);
            let _ = killer.status();
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Kill the bridge process on app exit. Safe to call when nothing was started.
pub fn shutdown() {
    let mut b = bridge().lock().unwrap();
    stop_child_locked(&mut b);
}

// ── HTTP plumbing ───────────────────────────────────────────────────────────

fn root_url() -> String {
    format!("http://127.0.0.1:{PORT}")
}

fn http() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("Lightify/2.0")
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| format!("Failed to build OnTheSpot client: {e}"))
        })
        .clone()
}

/// `GET /login` both wakes the Flask session and hands back the cookie every other
/// endpoint needs. The bridge issues a fresh one per call; there is nothing to cache.
async fn session_cookie(client: &reqwest::Client) -> Result<String, String> {
    let response = client
        .get(format!("{}/login", root_url()))
        .send()
        .await
        .map_err(|e| format!("OnTheSpot login failed: {e}"))?;
    if !(response.status().is_success() || response.status().is_redirection()) {
        return Err(format!("OnTheSpot login failed with status {}", response.status()));
    }
    let cookie = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|raw| raw.split(';').next())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if cookie.is_empty() {
        return Err("OnTheSpot login did not return a session cookie".to_string());
    }
    Ok(cookie)
}

async fn ping() -> bool {
    let Ok(client) = http() else { return false };
    client
        .get(format!("{}/login", root_url()))
        .send()
        .await
        .map(|r| r.status().is_success() || r.status().is_redirection())
        .unwrap_or(false)
}

/// An authenticated client + cookie pair, with the bridge known to be up. Every
/// call the UI makes comes through here, so this is also what marks the downloader
/// as "in use" for the idle shutdown.
async fn ready() -> Result<(reqwest::Client, String), String> {
    touch();
    ensure().await?;
    let client = http()?;
    let cookie = session_cookie(&client).await?;
    Ok((client, cookie))
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ── Download path ───────────────────────────────────────────────────────────

fn default_download_path() -> String {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
        .map(|d| d.join("LightifyDownloads").to_string_lossy().into_owned())
        .unwrap_or_else(|| "LightifyDownloads".to_string())
}

/// The configured download root, falling back to `~/Music/LightifyDownloads`.
/// Read straight from the shared config file, which is also what the shipped host
/// writes — set it in either app and both follow.
pub fn download_path() -> String {
    let cfg = lightify_core::config::load_config(&lightify_core::config::data_dir());
    let trimmed = cfg.download_path.trim();
    if trimmed.is_empty() {
        default_download_path()
    } else {
        trimmed.to_string()
    }
}

/// Persist a new download root and push it into a live bridge. Ports
/// `cmd_set_download_path`.
pub async fn set_download_path(path: &str) -> Result<String, String> {
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err("Download path cannot be empty".to_string());
    }
    std::fs::create_dir_all(&path).map_err(|e| format!("Failed to create download path {path}: {e}"))?;
    let dir = lightify_core::config::data_dir();
    lightify_core::config::update_config(&dir, |cfg| cfg.download_path = path.clone())?;
    bridge().lock().unwrap().synced_path = None;
    // Only touch the bridge if it's already live — setting a path shouldn't cold-start it.
    if ping().await {
        sync_download_path(true).await?;
    }
    Ok(path)
}

fn settings_payload(path: &str) -> serde_json::Value {
    serde_json::json!({
        "audio_download_path": path,
        // Keep the legacy key in sync as an informational field for existing configs.
        "download_location": path,
    })
}

async fn sync_download_path(force: bool) -> Result<String, String> {
    let path = download_path();

    if !force {
        let already = bridge().lock().unwrap().synced_path.as_deref() == Some(path.as_str());
        if already {
            return Ok(path);
        }
    }

    std::fs::create_dir_all(&path)
        .map_err(|e| format!("Failed to create download directory {path}: {e}"))?;

    let client = http()?;
    let cookie = session_cookie(&client).await?;
    client
        .post(format!("{}/api/update_settings", root_url()))
        .header(reqwest::header::COOKIE, cookie)
        .json(&settings_payload(&path))
        .send()
        .await
        .map_err(|e| format!("Failed to sync OnTheSpot download path: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Failed to sync OnTheSpot download path: {e}"))?;

    bridge().lock().unwrap().synced_path = Some(path.clone());
    Ok(path)
}

// ── Locating the bridge + its runtime dir ───────────────────────────────────

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.as_os_str().is_empty() && !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn find_path_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|value| {
        std::env::split_paths(&value).map(|dir| dir.join(name)).find(|p| p.is_file())
    })
}

/// The Lightify checkout this build was made from, if we're running inside one.
/// Used to find the bridge exe that `lightify-tauri` builds, and (last resort) the
/// OnTheSpot Python sources.
fn dev_workspace_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(explicit) = std::env::var_os("LIGHTIFY_DEV_WORKSPACE") {
        push_unique(&mut candidates, PathBuf::from(explicit));
    }
    if let Ok(exe_path) = std::env::current_exe() {
        for ancestor in exe_path.ancestors().skip(1) {
            push_unique(&mut candidates, ancestor.to_path_buf());
        }
    }
    if cfg!(debug_assertions) {
        if let Ok(cwd) = std::env::current_dir() {
            for ancestor in cwd.ancestors() {
                push_unique(&mut candidates, ancestor.to_path_buf());
            }
        }
    }

    candidates.into_iter().find(|root| {
        root.join("lightify-shell").join("Cargo.toml").is_file()
            && root.join("lightify-core").join("Cargo.toml").is_file()
    })
}

fn bridge_exe_name() -> &'static str {
    if cfg!(windows) { "lightify-onthespot.exe" } else { "lightify-onthespot" }
}

/// Where the packaged bridge might be. Installed builds ship it next to (or in
/// `resources/` beside) this executable; a dev checkout has whatever
/// `lightify-tauri` last built.
fn find_bridge() -> Option<PathBuf> {
    let name = bridge_exe_name();

    if let Some(explicit) = std::env::var_os("LIGHTIFY_ONTHESPOT_BIN") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            for candidate in [exe_dir.join(name), exe_dir.join("resources").join(name)] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    if let Some(root) = dev_workspace_root() {
        let tauri_target = root.join("lightify-tauri").join("src-tauri").join("target");
        for candidate in [
            tauri_target.join("pyinstaller").join("release").join("dist").join(name),
            tauri_target.join("release").join(name),
            tauri_target.join("debug").join(name),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    find_path_binary(name)
}

fn find_ffmpeg() -> Option<PathBuf> {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            for candidate in [exe_dir.join(name), exe_dir.join("resources").join(name)] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    if let Some(root) = dev_workspace_root() {
        let candidate = root
            .join("lightify-tauri")
            .join("src-tauri")
            .join("target")
            .join("release")
            .join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    find_path_binary(name)
}

/// The bridge's own state directory. Points at the exact path OnTheSpot picks by
/// default (`%APPDATA%/onthespot`), which is what the shipped host sets too — so a
/// downloader account connected there is already connected here.
fn runtime_dir(data_dir: &Path) -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let path = PathBuf::from(appdata).join("onthespot");
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("onthespot");
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    data_dir.join("onthespot")
}

fn dev_source_dir(root: &Path) -> PathBuf {
    root.join("lightify-tauri")
        .join("docs")
        .join("onthespot-master")
        .join("onthespot-master")
        .join("src")
}

fn dev_python(root: Option<&Path>) -> PathBuf {
    if let Some(root) = root {
        let venv = if cfg!(windows) {
            root.join(".venv").join("Scripts").join("python.exe")
        } else {
            root.join(".venv").join("bin").join("python")
        };
        if venv.is_file() {
            return venv;
        }
    }
    if cfg!(windows) {
        if let Some(path) = find_path_binary("python.exe")
            .or_else(|| find_path_binary("python"))
            .or_else(|| find_path_binary("py.exe"))
            .or_else(|| find_path_binary("py"))
        {
            return path;
        }
    } else if let Some(path) = find_path_binary("python3").or_else(|| find_path_binary("python")) {
        return path;
    }
    PathBuf::from("python")
}

/// Tee one of the bridge's pipes into the log file and the in-memory ring buffer.
///
/// This thread must keep draining until EOF no matter what the bridge writes: if
/// it stops, the pipe buffer fills, the bridge's next log write blocks while
/// holding Python's logging lock, and every request thread that logs wedges
/// behind it — the bridge deadlocks. So lines are read as raw bytes and decoded
/// lossily rather than via `lines()`, which returns an error (and ended this loop)
/// on the first non-UTF-8 line — exactly what Python on Windows emits by default
/// when stdout is a pipe (cp1252, e.g. a track named "Café"). The spawn also sets
/// `PYTHONUTF8`/`PYTHONIOENCODING` so the text is normally UTF-8 to begin with;
/// the lossy decode is the backstop for anything that still isn't.
fn capture<R: std::io::Read + Send + 'static>(reader: R, log_path: PathBuf, prefix: &'static str) {
    let ring = Arc::clone(log_ring());
    std::thread::spawn(move || {
        use std::io::Write;
        let mut reader = std::io::BufReader::new(reader);
        let mut sink = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                // Interrupted is retryable; anything else means the pipe is gone.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\n', '\r']);
            let formatted = format!("[{prefix}] {line}");
            if let Some(file) = sink.as_mut() {
                let _ = writeln!(file, "{formatted}");
                let _ = file.flush();
            }
            let mut recent = ring.lock().unwrap();
            if recent.len() >= LOG_RING {
                recent.pop_front();
            }
            recent.push_back(formatted);
        }
    });
}

fn recent_log(max_lines: usize) -> Vec<String> {
    let recent = log_ring().lock().unwrap();
    recent.iter().skip(recent.len().saturating_sub(max_lines)).cloned().collect()
}

/// Bring the bridge up (or confirm it already is) and make sure its download root
/// matches ours. Cold start takes seconds — every caller runs off the worker's
/// command loop for exactly this reason.
async fn ensure() -> Result<(), String> {
    if ping().await {
        sync_download_path(false).await?;
        return Ok(());
    }

    // One cold start at a time, then re-check: whoever waited here usually finds the
    // bridge already up and takes the fast path above.
    let _gate = start_gate().lock().await;
    if ping().await {
        sync_download_path(false).await?;
        return Ok(());
    }

    let should_spawn = {
        let mut b = bridge().lock().unwrap();
        if let Some(child) = b.child.as_mut() {
            let dead = !matches!(child.try_wait(), Ok(None));
            // "Stale" means a cold start that never came up. `started_at` is cleared
            // the moment the bridge first answers (see the ready loop below), so a
            // bridge that has been serving — and is just slow to answer two pings
            // while it's busy downloading — is never killed from here.
            let stale = b.started_at.map(|t| t.elapsed() >= READY_TIMEOUT).unwrap_or(false);
            if dead || stale {
                stop_child_locked(&mut b);
            }
        }
        b.child.is_none()
    };

    if should_spawn {
        log_ring().lock().unwrap().clear();
        let data_dir = lightify_core::config::data_dir();
        let log_path = data_dir.join("onthespot-web.log");
        let _ = std::fs::File::create(&log_path);
        let ots_dir = runtime_dir(&data_dir);
        std::fs::create_dir_all(&ots_dir)
            .map_err(|e| format!("Failed to create OnTheSpot data dir {}: {e}", ots_dir.display()))?;
        // Before this start: faster defaults (the bridge reads them at startup), and
        // the list of runtime folders earlier bridges leaked — taken now, so the one
        // this start unpacks can never be on it.
        tune_bridge_config(&ots_dir);
        let stale = stale_runtime_dirs();

        let mut command;
        let what;
        if let Some(exe) = find_bridge() {
            what = exe.display().to_string();
            command = Command::new(&exe);
            command.arg("--host").arg("127.0.0.1").arg("--port").arg(PORT.to_string());
        } else {
            // Dev checkouts without a built bridge: run OnTheSpot straight from source.
            let root = dev_workspace_root();
            let source = root
                .as_deref()
                .map(dev_source_dir)
                .filter(|p| p.is_dir())
                .ok_or_else(|| {
                    "Downloader bridge not found. Build lightify-onthespot, or set \
                     LIGHTIFY_ONTHESPOT_BIN to its path."
                        .to_string()
                })?;
            let python = dev_python(root.as_deref());
            what = python.display().to_string();
            command = Command::new(&python);
            command
                .arg("-m")
                .arg("onthespot.web")
                .arg("--host")
                .arg("127.0.0.1")
                .arg("--port")
                .arg(PORT.to_string())
                .env("PYTHONPATH", source.as_os_str());
            if let Some(root) = root.as_deref() {
                command.current_dir(root);
            }
        }

        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Own process group, so shutdown can signal the whole tree at once (see
        // `stop_child_locked`) instead of orphaning the server behind its bootloader.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command.env("ONTHESPOTDIR", &ots_dir);
        // Python on Windows writes the ANSI code page (cp1252) to a pipe, not UTF-8,
        // so any non-ASCII track/artist name in the bridge's logs would arrive here
        // as invalid UTF-8. Force UTF-8 for both the interpreter's I/O and its
        // default text encoding; `capture` still decodes lossily as a backstop.
        command.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
        if let Some(ffmpeg) = find_ffmpeg() {
            command.env("FFMPEG_PATH", ffmpeg);
        }
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = command
            .spawn()
            .map_err(|e| format!("Failed to start the downloader bridge ({what}): {e}"))?;
        if let Some(stdout) = child.stdout.take() {
            capture(stdout, log_path.clone(), "stdout");
        }
        if let Some(stderr) = child.stderr.take() {
            capture(stderr, log_path, "stderr");
        }

        let mut b = bridge().lock().unwrap();
        b.started_at = Some(Instant::now());
        b.child = Some(child);
        drop(b);
        remove_dirs_in_background(stale);
        spawn_idle_monitor();
    }

    // Wait as long as the process is alive (up to `READY_TIMEOUT`), but give up at
    // once if it exits — a crash on startup shouldn't cost the user 45 seconds.
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if ping().await {
            // Up and answering: this is no longer a cold start, so `ensure`'s
            // stale-start check must not apply to it from here on.
            bridge().lock().unwrap().started_at = None;
            sync_download_path(true).await?;
            return Ok(());
        }
        let alive = bridge()
            .lock()
            .unwrap()
            .child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if !alive || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(READY_POLL_DELAY_MS)).await;
    }

    let log_path = lightify_core::config::data_dir().join("onthespot-web.log");
    shutdown();
    let tail = recent_log(12);
    let detail = if tail.is_empty() {
        format!(" See {} for bridge logs.", log_path.display())
    } else {
        format!(
            " Last bridge log lines:\n{}\nSee {} for the full bridge log.",
            tail.join("\n"),
            log_path.display()
        )
    };
    Err(format!("Downloader bridge did not become ready at {}.{}", root_url(), detail))
}

// ── URLs ────────────────────────────────────────────────────────────────────

/// Build the open.spotify.com URL the bridge parses. `kind` is one of the Spotify
/// item types, or `"liked"` for the saved-tracks collection.
pub fn spotify_url(kind: &str, id: &str) -> Result<String, String> {
    if kind == "liked" {
        return Ok("https://open.spotify.com/collection/tracks".to_string());
    }
    if id.is_empty() {
        return Err("Missing Spotify id".to_string());
    }
    Ok(format!("https://open.spotify.com/{kind}/{id}"))
}

/// Reject anything that isn't a Spotify item URL before it reaches the bridge —
/// ports `normalize_download_url`.
fn normalize_url(input: &str) -> Result<String, String> {
    let parsed = url::Url::parse(input).map_err(|_| "Invalid download URL".to_string())?;
    if parsed.scheme() != "https" {
        return Err("Download URL must use https".to_string());
    }
    if parsed.host_str().unwrap_or_default().to_ascii_lowercase() != "open.spotify.com" {
        return Err("Only open.spotify.com download URLs are supported".to_string());
    }
    let segments = parsed
        .path_segments()
        .map(|parts| parts.filter(|part| !part.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    if segments.as_slice() == ["collection", "tracks"] {
        return Ok("https://open.spotify.com/collection/tracks".to_string());
    }
    if segments.len() < 2 {
        return Err("Unsupported Spotify download URL".to_string());
    }
    let (kind, id) = (segments[0], segments[1]);
    match kind {
        "track" | "playlist" | "album" | "artist" | "show" | "episode" => {
            if id.is_empty() || id.len() > 128 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(format!("Invalid {kind} id"));
            }
            Ok(format!("https://open.spotify.com/{kind}/{id}"))
        }
        _ => Err(format!("Unsupported Spotify download URL: {kind}")),
    }
}

fn check_local_id(local_id: &str) -> Result<String, String> {
    let trimmed = local_id.trim();
    if trimmed.is_empty() {
        return Err("Missing download item id".to_string());
    }
    if trimmed.len() > 512 {
        return Err("Download item id is too long".to_string());
    }
    Ok(trimmed.to_string())
}

// ── Commands ────────────────────────────────────────────────────────────────

fn status_from_json(value: &serde_json::Value) -> Status {
    Status {
        spotify_ready: value["spotify_ready"].as_bool().unwrap_or(false),
        login_in_progress: value["spotify_login_in_progress"].as_bool().unwrap_or(false),
        message: value["spotify_login_message"].as_str().unwrap_or_default().to_string(),
        error: value["spotify_login_error"].as_str().unwrap_or_default().to_string(),
        configured_accounts: value["spotify_configured_accounts"].as_u64().unwrap_or(0) as u32,
    }
}

pub async fn status() -> Result<Status, String> {
    let (client, cookie) = ready().await?;
    let value: serde_json::Value = client
        .get(format!("{}/api/lightify/status", root_url()))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| format!("Failed to check downloader status: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Failed to check downloader status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse downloader status: {e}"))?;
    let status = status_from_json(&value);
    // Any status showing no login in flight ends the tracked attempt — it finished
    // (or failed) on its own. Without this the timestamp from a login that
    // *succeeded* lingers, and a later Connect would read as "stuck" and restart a
    // perfectly healthy bridge, dropping its download queue.
    if !status.login_in_progress {
        *login_started_at().lock().unwrap() = None;
    }
    Ok(status)
}

/// When the current login attempt started (`None` once it's finished, one way or
/// another). The bridge's own Zeroconf wait for the user to pick "OnTheSpot" in
/// Spotify's device list has **no timeout** — if that handshake never completes
/// (most commonly: Windows' firewall prompt only checked "Private networks" by
/// default, while the active network profile is Public, so the broadcast never
/// reaches Spotify at all), the bridge's `spotify_login_in_progress` flag never
/// clears. `enqueue` below only (re)starts a login when `!spotify_ready`, and the
/// bridge itself refuses to start a *second* attempt while one is already "in
/// progress" — so every retry after that point is a silent no-op, forever, even
/// across relaunching Lightify (the bridge is a separate OS process that survives
/// unless explicitly killed). This tracks how long the current attempt has been
/// running so `start_login` can notice and break it.
fn login_started_at() -> &'static Mutex<Option<Instant>> {
    static V: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(None))
}
/// Long enough for a real handshake (the user has to switch to their phone or
/// desktop, open Spotify, and tap "OnTheSpot" in the device list) — short enough
/// that a genuinely wedged attempt doesn't leave every future retry, for the rest
/// of the session, doing nothing.
const LOGIN_STUCK_AFTER: Duration = Duration::from_secs(150);

/// Kick off the downloader's own Spotify sign-in. OnTheSpot advertises itself on
/// the local network; the user picks it in Spotify's device list to hand it
/// credentials — which is why the follow-up message says exactly that.
pub async fn start_login() -> Result<Status, String> {
    let stuck = login_started_at().lock().unwrap().is_some_and(|t| t.elapsed() >= LOGIN_STUCK_AFTER);
    if stuck {
        // Our timestamp alone isn't proof: the attempt may have completed since we
        // last looked. Only a bridge that *still* reports a login in progress is
        // wedged — `status` clears the slot itself when it isn't.
        match status().await {
            Ok(st) if st.login_in_progress => {
                // Nothing short of a fresh process can clear the bridge's own stuck
                // flag — there is no "cancel" endpoint, because the vendored login
                // code has no way to interrupt its own wait. Restarting is exactly
                // what a user pressing "retry" wants here, not another no-op POST
                // to an already-wedged bridge.
                //
                // But we can only restart a bridge we spawned. One already running
                // when we started (the shipped app's, or an earlier session's —
                // they share port 57321) has no handle here, so `shutdown` would
                // silently do nothing and the POST below would be the same no-op.
                // Say so instead of pretending to retry.
                if bridge().lock().unwrap().child.is_none() {
                    return Err("The downloader sign-in is stuck, and its bridge was started by \
                                another Lightify instance, so it can't be restarted from here. \
                                Close the other Lightify app (or end lightify-onthespot in Task \
                                Manager), then retry."
                        .to_string());
                }
                shutdown();
                *login_started_at().lock().unwrap() = None;
            }
            Ok(_) => {}
            // Couldn't reach the bridge at all, so there's no attempt left to
            // attribute the old timestamp to; `ready` below reports the real error
            // (or cold-starts a fresh bridge, whose login gets its own timestamp).
            Err(_) => *login_started_at().lock().unwrap() = None,
        }
    }
    let (client, cookie) = ready().await?;
    let value: serde_json::Value = client
        .post(format!("{}/api/lightify/start_spotify_login", root_url()))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| format!("Failed to start downloader login: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Failed to start downloader login: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse downloader login status: {e}"))?;
    let status = status_from_json(&value);
    {
        let mut slot = login_started_at().lock().unwrap();
        *slot = if status.login_in_progress { Some(slot.unwrap_or_else(Instant::now)) } else { None };
    }
    Ok(status)
}

/// Queue a download. Errors with the "connect the downloader first" message (and
/// starts that flow) when the bridge has no usable Spotify account yet.
pub async fn enqueue(url: &str) -> Result<String, String> {
    let normalized = normalize_url(url)?;
    let st = status().await?;
    if !st.spotify_ready {
        let _ = start_login().await?;
        return Err("Downloader setup started \u{2014} open Spotify, pick OnTheSpot in the \
                    device list, then retry Download."
            .to_string());
    }
    let (client, cookie) = ready().await?;
    client
        .post(format!("{}/api/parse_url/{}", root_url(), url_encode(&normalized)))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| format!("Failed to enqueue download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Failed to enqueue download: {e}"))?;
    Ok(normalized)
}

fn text_field(o: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    o.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

fn u32_field(o: &serde_json::Map<String, serde_json::Value>, key: &str) -> u32 {
    o.get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|n| n as u64)))
        .unwrap_or(0) as u32
}

/// Read the bridge's whole queue. The result is cached for row→`local_id` lookups.
pub async fn snapshot() -> Result<Vec<Item>, String> {
    let (client, cookie) = ready().await?;
    let value: serde_json::Value = client
        .get(format!("{}/api/download_queue", root_url()))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch downloads: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Failed to fetch downloads: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse downloads queue: {e}"))?;

    let mut items: Vec<Item> = value
        .as_object()
        .map(|object| {
            object
                .values()
                .filter_map(|v| v.as_object())
                .map(|o| Item {
                    local_id: text_field(o, "local_id"),
                    name: text_field(o, "item_name"),
                    by: text_field(o, "item_by"),
                    service: text_field(o, "item_service"),
                    status: text_field(o, "item_status"),
                    progress: u32_field(o, "progress"),
                    file_path: text_field(o, "file_path"),
                    error: text_field(o, "error"),
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| a.local_id.cmp(&b.local_id));
    *snapshot_cache().lock().unwrap() = items.clone();
    Ok(items)
}

async fn post_empty(path: String, what: &str) -> Result<(), String> {
    let (client, cookie) = ready().await?;
    client
        .post(format!("{}{}", root_url(), path))
        .header(reqwest::header::COOKIE, cookie)
        .header("Content-Length", "0")
        .send()
        .await
        .map_err(|e| format!("Failed to {what}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Failed to {what}: {e}"))?;
    Ok(())
}

pub async fn clear_finished() -> Result<(), String> {
    post_empty("/api/clear_items".to_string(), "clear downloads").await
}

pub async fn retry(local_id: &str) -> Result<(), String> {
    let id = check_local_id(local_id)?;
    post_empty(format!("/api/retry/{}", url_encode(&id)), "retry download").await
}

pub async fn cancel(local_id: &str) -> Result<(), String> {
    let id = check_local_id(local_id)?;
    post_empty(format!("/api/cancel/{}", url_encode(&id)), "cancel download").await
}

pub async fn delete(local_id: &str) -> Result<(), String> {
    let id = check_local_id(local_id)?;
    let (client, cookie) = ready().await?;
    client
        .delete(format!("{}/api/delete/{}", root_url(), url_encode(&id)))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| format!("Failed to delete downloaded file: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Failed to delete downloaded file: {e}"))?;
    Ok(())
}

/// Reveal a finished download (or the download root, given an empty path) in the
/// OS file manager. No bridge involvement — this is purely local.
///
/// Only the download root is ever created. A row's `file_path` can point at a file
/// that has since been moved or deleted — creating that path would leave behind an
/// empty *folder* named e.g. `Song.mp3` — so a missing path instead opens its
/// nearest ancestor that still exists, falling back to the download root.
pub fn open_folder(path: &str) -> Result<(), String> {
    let root = || {
        let root = PathBuf::from(download_path());
        let _ = std::fs::create_dir_all(&root);
        root
    };
    let dir: PathBuf = if path.is_empty() {
        root()
    } else {
        let p = Path::new(path);
        if p.is_dir() {
            p.to_path_buf()
        } else {
            // A file (its folder), or a path that no longer exists (the closest
            // folder of it that does).
            p.ancestors()
                .skip(1)
                .find(|a| !a.as_os_str().is_empty() && a.is_dir())
                .map(Path::to_path_buf)
                .unwrap_or_else(root)
        }
    };

    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("explorer");
        c.arg(&dir);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(&dir);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(&dir);
        c
    };

    // explorer.exe exits non-zero even on success, so only the spawn is checked.
    command.spawn().map(|_| ()).map_err(|e| format!("Failed to open folder: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_supported_spotify_urls() {
        assert_eq!(
            normalize_url("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=x").unwrap(),
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT"
        );
        assert_eq!(
            normalize_url("https://open.spotify.com/collection/tracks").unwrap(),
            "https://open.spotify.com/collection/tracks"
        );
    }

    #[test]
    fn rejects_everything_else() {
        assert!(normalize_url("http://open.spotify.com/track/abc").is_err());
        assert!(normalize_url("https://example.com/track/abc").is_err());
        assert!(normalize_url("https://open.spotify.com/user/bob").is_err());
        assert!(normalize_url("https://open.spotify.com/track/bad-id!").is_err());
        assert!(normalize_url("not a url").is_err());
    }

    #[test]
    fn builds_item_urls() {
        assert_eq!(spotify_url("liked", "").unwrap(), "https://open.spotify.com/collection/tracks");
        assert_eq!(spotify_url("track", "abc").unwrap(), "https://open.spotify.com/track/abc");
        assert!(spotify_url("track", "").is_err());
    }

    #[test]
    fn summarizes_queue_rows() {
        let item = Item {
            status: "Downloading".into(),
            by: "Rage Against The Machine".into(),
            progress: 42,
            ..Default::default()
        };
        assert_eq!(item.summary(), "Downloading \u{2022} Rage Against The Machine \u{2022} 42%");
        assert!(!item.finished());

        let waiting = Item::default();
        assert_eq!(waiting.summary(), "Waiting");

        let done = Item {
            status: "Downloaded".into(),
            file_path: "C:/music/x.ogg".into(),
            ..Default::default()
        };
        assert!(done.finished() && done.on_disk());
    }

    #[test]
    fn status_summary_covers_each_state() {
        let ready = Status { spotify_ready: true, ..Default::default() };
        assert_eq!(ready.summary(), "Spotify downloader connected");

        let stale = Status { configured_accounts: 1, ..Default::default() };
        assert_eq!(stale.summary(), "Spotify downloader account needs reconnecting");

        let fresh = Status::default();
        assert_eq!(fresh.summary(), "Spotify downloader is not connected");
    }
}

// ── Speed: OnTheSpot's defaults are one download at a time ───────────────────

/// OnTheSpot ships with 1 download worker, a 3 s pause after every track and 50 KB
/// stream reads. Those values are only replaced while they are still the defaults —
/// a value changed in OnTheSpot's own settings is the user's and stays. Workers are
/// created when the bridge starts, so this runs just before a start.
const BRIDGE_TUNING: [(&str, i64, i64); 4] = [
    // (key, OnTheSpot default, Lightify value)
    ("maximum_download_workers", 1, 3),
    ("maximum_queue_workers", 1, 2),
    ("download_delay", 3, 1),
    ("download_chunk_size", 50_000, 262_144),
];

fn tune_bridge_config(ots_dir: &Path) {
    let path = ots_dir.join("otsconfig.json");
    // No file yet: the bridge writes its defaults on first start; the next start
    // tunes them.
    let Ok(bytes) = std::fs::read(&path) else { return };
    let Ok(mut cfg) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return };
    let Some(obj) = cfg.as_object_mut() else { return };
    let mut changed = false;
    for (key, default, tuned) in BRIDGE_TUNING {
        if obj.get(key).and_then(|v| v.as_i64()) == Some(default) {
            obj.insert(key.to_string(), serde_json::json!(tuned));
            changed = true;
        }
    }
    if changed {
        if let Ok(out) = serde_json::to_vec_pretty(&cfg) {
            let _ = lightify_core::config::write_atomic(&path, &out);
        }
    }
}

// ── Idle shutdown ───────────────────────────────────────────────────────────

/// Stop the bridge after this long with nothing in the app using the downloader and
/// nothing downloading. It starts again on demand (a cold start, as at launch).
const IDLE_AFTER: Duration = Duration::from_secs(5 * 60);
const IDLE_CHECK: Duration = Duration::from_secs(30);

/// `IDLE_AFTER`/`IDLE_CHECK`, overridable for testing (`LIGHTIFY_DOWNLOADER_IDLE_SECS`;
/// the check then runs every second).
fn idle_timing() -> (Duration, Duration) {
    match std::env::var("LIGHTIFY_DOWNLOADER_IDLE_SECS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(secs) => (Duration::from_secs(secs), Duration::from_secs(1)),
        None => (IDLE_AFTER, IDLE_CHECK),
    }
}

/// Is a bridge process that we started still running?
pub fn running() -> bool {
    bridge().lock().unwrap().child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
}

fn last_interest() -> &'static Mutex<Instant> {
    static V: OnceLock<Mutex<Instant>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(Instant::now()))
}

fn touch() {
    *last_interest().lock().unwrap() = Instant::now();
}

/// Is anything still queued or in progress? `None` = the bridge didn't answer.
/// Deliberately not through `ready()`: asking must not count as using it.
async fn queue_busy() -> Option<bool> {
    let client = http().ok()?;
    let cookie = session_cookie(&client).await.ok()?;
    let value: serde_json::Value = client
        .get(format!("{}/api/download_queue", root_url()))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let busy = value.as_object()?.values().filter_map(|v| v.as_object()).any(|o| {
        let status = text_field(o, "item_status");
        !matches!(
            status.as_str(),
            "Downloaded" | "Already Exists" | "Cancelled" | "Unavailable" | "Deleted" | "Failed"
        )
    });
    Some(busy)
}

/// One watcher per bridge we started: every `IDLE_CHECK` it looks for
/// `IDLE_AFTER` without use, no sign-in in flight and an idle queue, then stops the
/// bridge (gracefully — see `stop_child_locked`) and exits. It also exits when the
/// bridge goes away by other means.
fn spawn_idle_monitor() {
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    touch();
    let spawned = std::thread::Builder::new()
        .name("downloader-idle".into())
        .stack_size(256 * 1024)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok();
            let (idle_after, idle_check) = idle_timing();
            loop {
                std::thread::sleep(idle_check);
                let alive = bridge()
                    .lock()
                    .unwrap()
                    .child
                    .as_mut()
                    .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
                if !alive {
                    break;
                }
                if last_interest().lock().unwrap().elapsed() < idle_after {
                    continue;
                }
                if login_started_at().lock().unwrap().is_some() {
                    continue;
                }
                let Some(rt) = rt.as_ref() else { break };
                if rt.block_on(queue_busy()) == Some(false) {
                    append_log(&format!("stopping the downloader bridge after {}s idle", idle_after.as_secs()));
                    shutdown();
                    break;
                }
            }
            RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    if spawned.is_err() {
        RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn append_log(line: &str) {
    use std::io::Write;
    let path = lightify_core::config::data_dir().join("onthespot-web.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
        let _ = writeln!(f, "[lightify] {line}");
    }
}

// ── Leaked runtime folders ──────────────────────────────────────────────────

/// `%TEMP%\_MEI*` folders unpacked by earlier bridges (they hold an `onthespot/`
/// package directory — other PyInstaller apps' folders are never touched). Only
/// collected while no bridge process is running at all, so no live bridge — ours
/// or the shipped app's — can lose files it is using.
fn stale_runtime_dirs() -> Vec<PathBuf> {
    #[cfg(windows)]
    if proc::any_named(bridge_exe_name()) {
        return Vec::new();
    }
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else { return Vec::new() };
    dir.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("_MEI"))
                && p.join("onthespot").is_dir()
        })
        .collect()
}

fn remove_dirs_in_background(dirs: Vec<PathBuf>) {
    if dirs.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("downloader-tmp-clean".into()).spawn(move || {
        let n = dirs.len();
        for d in dirs {
            let _ = std::fs::remove_dir_all(d);
        }
        append_log(&format!("removed {n} runtime folder(s) left behind by earlier bridge runs"));
    });
}

// ── Windows process helpers ─────────────────────────────────────────────────

#[cfg(windows)]
mod proc {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    /// (pid, parent pid, exe name) for every process.
    fn processes() -> Vec<(u32, u32, String)> {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut e) != 0 {
                loop {
                    let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                    out.push((e.th32ProcessID, e.th32ParentProcessID, name));
                    if Process32NextW(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        out
    }

    /// Every descendant of `root`, deepest first (so a server dies after anything
    /// it spawned, never leaving an orphan behind).
    pub fn descendants(root: u32) -> Vec<u32> {
        let all = processes();
        let mut found = Vec::new();
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for &(pid, ppid, _) in &all {
                if ppid == parent && pid != root && !found.contains(&pid) {
                    found.push(pid);
                    frontier.push(pid);
                }
            }
        }
        found.reverse();
        found
    }

    pub fn terminate(pid: u32) {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    pub fn any_named(exe: &str) -> bool {
        processes().iter().any(|(_, _, n)| n.eq_ignore_ascii_case(exe))
    }
}
