//! What Setup actually does: locate installs, copy the payload, write shortcuts and
//! the Apps-list entry, migrate an Inno Setup install, uninstall.
//!
//! Sandbox: with `LIGHTIFY_INSTALL_SANDBOX=<dir>` set, every location moves under that
//! directory (install dirs, Start menu, desktop, user data, updates) and the registry
//! entry becomes `...\Uninstall\Lightify-Sandbox` under HKCU. Inno detection and
//! elevation are off. That is how Setup is exercised end to end without touching a
//! real install.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use lightify_i18n::tr;
use lightify_installer::payload::{self, Header, PayloadReader};

use crate::win::{self, Hive, Key};

pub const APP_EXE: &str = "Lightify.exe";
pub const BRIDGE_EXE: &str = "lightify-onthespot.exe";
pub const FFMPEG_EXE: &str = "ffmpeg.exe";
pub const UNINSTALL_EXE: &str = "uninstall.exe";
/// Inno Setup's entry for the old installer (lightify-shell/installer/lightify.iss AppId).
const INNO_ID: &str = "{8F3C2A17-5D4E-4B9A-9E21-3C7A6B0D1E44}_is1";
const UNINSTALL_ROOT: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
/// Files an install can contain besides the payload (always removed on uninstall).
const KNOWN_FILES: &[&str] = &[APP_EXE, BRIDGE_EXE, FFMPEG_EXE, "LICENSE.md", UNINSTALL_EXE, "onthespot-web.log"];

pub fn sandbox() -> Option<PathBuf> {
    std::env::var_os("LIGHTIFY_INSTALL_SANDBOX").filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn own_subkey() -> String {
    format!(r"{UNINSTALL_ROOT}\{}", if sandbox().is_some() { "Lightify-Sandbox" } else { "Lightify" })
}

fn inno_subkey() -> String {
    format!(r"{UNINSTALL_ROOT}\{INNO_ID}")
}

pub fn hive_for(all_users: bool) -> Hive {
    if all_users && sandbox().is_none() {
        Hive::Machine
    } else {
        Hive::User
    }
}

/// Does installing for this scope need an elevated process?
pub fn needs_admin(all_users: bool) -> bool {
    all_users && sandbox().is_none() && !win::is_elevated()
}

/// The default install path as shown in the field (the per-user one in its
/// `%LOCALAPPDATA%` form, as in the design; it is expanded when used).
pub fn default_dir_display(all_users: bool) -> String {
    if let Some(sb) = sandbox() {
        return sb.join(if all_users { "app-all" } else { "app-user" }).to_string_lossy().into_owned();
    }
    if all_users {
        win::program_files()
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
            .join("Lightify")
            .to_string_lossy()
            .into_owned()
    } else {
        r"%LOCALAPPDATA%\Programs\Lightify".to_string()
    }
}

/// The path as typed → an absolute directory (env vars expanded, quotes trimmed).
pub fn resolve_dir(s: &str) -> PathBuf {
    let s = s.trim().trim_matches('"').trim();
    let mut p = PathBuf::from(win::expand_env(s));
    while p.as_os_str().len() > 3 && p.to_string_lossy().ends_with(['\\', '/']) {
        p = PathBuf::from(p.to_string_lossy().trim_end_matches(['\\', '/']).to_string());
    }
    p
}

/// Reject paths that are not a dedicated, absolute folder.
pub fn validate_dir(p: &Path) -> Result<(), String> {
    let s = p.to_string_lossy();
    let absolute = (s.len() >= 3 && s.as_bytes()[1] == b':' && (s.as_bytes()[2] == b'\\' || s.as_bytes()[2] == b'/'))
        || s.starts_with(r"\\");
    if !absolute || s.contains('%') {
        return Err(tr!("Choose a full folder path, like C:\\Programs\\Lightify."));
    }
    if p.parent().is_none() || s.len() <= 3 {
        return Err(tr!("Pick a folder, not the root of a drive."));
    }
    if let Some(windir) = std::env::var_os("SystemRoot") {
        if win::path_within(p, Path::new(&windir)) {
            return Err(tr!("Lightify can't be installed inside the Windows folder."));
        }
    }
    if s.contains(['<', '>', '|', '?', '*', '"']) {
        return Err(tr!("That folder name has characters Windows doesn't allow."));
    }
    Ok(())
}

/// Start menu folder (`...\Programs\Lightify`), as the Inno installer had it.
pub fn start_menu_dir(all_users: bool) -> Option<PathBuf> {
    if let Some(sb) = sandbox() {
        return Some(sb.join(if all_users { "startmenu-all" } else { "startmenu" }).join("Lightify"));
    }
    win::programs_dir(all_users).map(|p| p.join("Lightify"))
}

pub fn desktop_dir(all_users: bool) -> Option<PathBuf> {
    if let Some(sb) = sandbox() {
        return Some(sb.join(if all_users { "desktop-all" } else { "desktop" }));
    }
    win::desktop_dir(all_users)
}

/// Lightify's settings + cached sign-in (lightify-core config.rs `data_dir()`, default
/// location only) and the downloader bridge's state (`%APPDATA%\onthespot`).
fn user_data_dirs() -> Vec<PathBuf> {
    if let Some(sb) = sandbox() {
        return vec![sb.join("data").join("Lightify"), sb.join("data").join("onthespot")];
    }
    let mut v = Vec::new();
    if let Some(d) = dirs::data_dir() {
        v.push(d.join("Lightify"));
    }
    match std::env::var_os("APPDATA").filter(|a| !a.is_empty()) {
        Some(a) => v.push(PathBuf::from(a).join("onthespot")),
        None => {
            if let Some(c) = dirs::config_dir() {
                v.push(c.join("onthespot"));
            }
        }
    }
    v
}

/// Lightify's own data folder, where the language setting lives: the sandbox's, else
/// `LIGHTIFY_DATA_DIR`, else lightify-core's default (config.rs `data_dir()`).
pub fn app_data_dir() -> Option<PathBuf> {
    if let Some(sb) = sandbox() {
        return Some(sb.join("data").join("Lightify"));
    }
    std::env::var_os("LIGHTIFY_DATA_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("Lightify")))
}

/// Where lightify-core's updater keeps downloaded installers.
fn updates_dir() -> Option<PathBuf> {
    if let Some(sb) = sandbox() {
        return Some(sb.join("localdata").join("Lightify").join("updates"));
    }
    dirs::data_local_dir().map(|d| d.join("Lightify").join("updates"))
}

// ── Existing installs ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Existing {
    pub all_users: bool,
    pub dir: PathBuf,
    pub version: String,
    /// Set for an Inno Setup install: its `unins000.exe`.
    pub inno_uninstaller: Option<PathBuf>,
    pub files: Vec<String>,
}

fn trim_path(s: &str) -> PathBuf {
    resolve_dir(s)
}

/// This installer's own entry (per-user first, then all-users).
pub fn find_own() -> Option<Existing> {
    let hives: &[Hive] = if sandbox().is_some() { &[Hive::User] } else { &[Hive::User, Hive::Machine] };
    for &h in hives {
        if let Some(k) = Key::open(h, &own_subkey()) {
            let Some(dir) = k.get_str("InstallLocation").filter(|d| !d.trim().is_empty()) else { continue };
            return Some(Existing {
                all_users: h == Hive::Machine,
                dir: trim_path(&dir),
                version: k.get_str("DisplayVersion").unwrap_or_default(),
                inno_uninstaller: None,
                files: k
                    .get_str("LightifyFiles")
                    .map(|f| f.split(';').filter(|n| payload::valid_name(n)).map(str::to_string).collect())
                    .unwrap_or_default(),
            });
        }
    }
    None
}

/// An install made by the old Inno Setup installer.
pub fn find_inno() -> Option<Existing> {
    if sandbox().is_some() {
        return None;
    }
    for h in [Hive::User, Hive::Machine] {
        if let Some(k) = Key::open(h, &inno_subkey()) {
            let dir = k.get_str("InstallLocation").or_else(|| k.get_str("Inno Setup: App Path"))?;
            let un = k.get_str("UninstallString").map(|u| PathBuf::from(u.trim().trim_matches('"')));
            return Some(Existing {
                all_users: h == Hive::Machine,
                dir: trim_path(&dir),
                version: k.get_str("DisplayVersion").unwrap_or_default(),
                inno_uninstaller: un,
                files: Vec::new(),
            });
        }
    }
    None
}

pub fn find_existing() -> Option<Existing> {
    find_own().or_else(find_inno)
}

/// Lightify processes running from `dir` (the app itself; its bridge and ffmpeg go
/// when it does).
pub fn running_app(dir: &Path) -> Vec<u32> {
    win::processes_in(dir)
        .into_iter()
        .filter(|(_, n)| n.eq_ignore_ascii_case(APP_EXE))
        .map(|(p, _)| p)
        .collect()
}

/// Close Lightify the polite way (WM_CLOSE to its window) and wait for it to exit.
/// Then give its helper processes a moment to follow.
pub fn close_app(dir: &Path, timeout_ms: u32) -> bool {
    let pids = running_app(dir);
    for &p in &pids {
        win::close_windows_of(p);
    }
    let start = std::time::Instant::now();
    for &p in &pids {
        let left = (timeout_ms as u128).saturating_sub(start.elapsed().as_millis()) as u32;
        if !win::wait_pid(p, left) {
            return false;
        }
    }
    wait_helpers(dir, 5000);
    running_app(dir).is_empty()
}

/// Wait (bounded) for any other process still running out of `dir`.
pub fn wait_helpers(dir: &Path, timeout_ms: u32) {
    let start = std::time::Instant::now();
    for (p, _) in win::processes_in(dir) {
        let left = (timeout_ms as u128).saturating_sub(start.elapsed().as_millis()) as u32;
        win::wait_pid(p, left);
    }
}

pub fn launch_app(dir: &Path) -> Result<(), String> {
    let exe = dir.join(APP_EXE);
    win::spawn_detached(&exe, &[], Some(dir)).map_err(|e| tr!("could not start {}: {}", exe.display(), e))
}

// ── Install ─────────────────────────────────────────────────────────────────

pub struct Plan {
    pub dir: PathBuf,
    pub all_users: bool,
    pub desktop: bool,
    /// `--update`: replace files + refresh the entry; leave shortcuts alone.
    pub update: bool,
    /// An Inno install to remove first (its uninstaller keeps user config).
    pub migrate: Option<Existing>,
}

pub enum Ev {
    Step(usize, i32),
    Current(String),
    Progress(f32),
}

pub const STEP_APP: &str = "Lightify.exe";
// Shown translated (`steps_model`); matched here in English, so only marked.
pub const STEP_BRIDGE: &str = lightify_i18n::tr_noop!("Downloader bridge");
pub const STEP_FFMPEG: &str = "ffmpeg";
pub const STEP_FINAL: &str = lightify_i18n::tr_noop!("Shortcuts and uninstall entry");

/// The per-step list shown while installing, for what this payload carries.
pub fn step_labels(h: &Header) -> Vec<String> {
    let mut v = vec![STEP_APP.to_string()];
    if h.has(BRIDGE_EXE) {
        v.push(STEP_BRIDGE.into());
    }
    if h.has(FFMPEG_EXE) {
        v.push(STEP_FFMPEG.into());
    }
    v.push(STEP_FINAL.into());
    v
}

fn step_of(labels: &[String], file: &str) -> usize {
    let want = if file.eq_ignore_ascii_case(BRIDGE_EXE) {
        STEP_BRIDGE
    } else if file.eq_ignore_ascii_case(FFMPEG_EXE) {
        STEP_FFMPEG
    } else {
        STEP_APP
    };
    labels.iter().position(|l| l == want).unwrap_or(0)
}

/// Remove `*.lfy-old` / `*.lfy-new` leftovers from an earlier run.
fn clean_leftovers(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n.ends_with(".lfy-old") || n.ends_with(".lfy-new") || n.contains(".lfy-old-") {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

/// Put `tmp` in place of `target`. A target that can't be deleted (still running)
/// is renamed aside - Windows allows renaming a running exe - and removed next run.
fn swap_in(tmp: &Path, target: &Path) -> io::Result<()> {
    if target.exists() && fs::remove_file(target).is_err() {
        let name = target.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut i = 0;
        loop {
            let old = target.with_file_name(if i == 0 { format!("{name}.lfy-old") } else { format!("{name}.lfy-old-{i}") });
            if !old.exists() || fs::remove_file(&old).is_ok() {
                fs::rename(target, &old)?;
                let _ = fs::remove_file(&old);
                break;
            }
            i += 1;
            if i > 20 {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, tr!("{} is in use", name)));
            }
        }
    }
    fs::rename(tmp, target)
}

pub fn dir_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".lfy-write-test-{}", std::process::id()));
    let ok = File::create(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

/// Days since 1970-01-01 → (y, m, d) (Howard Hinnant's civil_from_days).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn today_yyyymmdd() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = civil((secs / 86_400) as i64);
    format!("{y:04}{m:02}{d:02}")
}

fn io_err(what: &Path, e: io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => tr!("{}: access denied", what.display()),
        _ => format!("{}: {e}", what.display()),
    }
}

/// Inno's uninstaller, silently. It removes its own files, shortcuts and entry and
/// keeps the user's config (which is what we want). Waits for its entry to go.
fn run_inno_uninstaller(ex: &Existing) -> Result<(), String> {
    let Some(un) = ex.inno_uninstaller.as_ref().filter(|u| u.is_file()) else { return Ok(()) };
    let status = std::process::Command::new(un)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .status()
        .map_err(|e| tr!("could not run the old uninstaller {}: {}", un.display(), e))?;
    if !status.success() {
        return Err(tr!("the old uninstaller {} failed ({})", un.display(), status));
    }
    // Inno's first stage re-runs itself from %TEMP%; wait for the real work to end.
    let hive = hive_for(ex.all_users);
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(60) {
        if Key::open(hive, &inno_subkey()).is_none() && !un.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Ok(())
}

pub fn install(plan: &Plan, exe: &Path, ev: &mut dyn FnMut(Ev)) -> Result<(), String> {
    let mut reader = PayloadReader::open(exe)
        .map_err(|e| tr!("this Setup file is damaged ({}) - download it again", e))?
        .ok_or_else(|| tr!("this copy of Setup has no files in it (it is the uninstaller) - download Setup from lightify.stream"))?;
    let header = reader.header.clone();
    let labels = step_labels(&header);
    let last = labels.len() - 1;
    let dir = &plan.dir;

    if let Some(old) = &plan.migrate {
        ev(Ev::Current(tr!("removing the previous installer")));
        run_inno_uninstaller(old)?;
    }

    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    clean_leftovers(dir);

    let stub = payload::stub_len(exe).map_err(|e| io_err(exe, e))?;
    let total = (header.total_size() + stub).max(1) as f64;
    let mut done = 0u64;
    let mut last_sent = -1.0f32;
    let mut cur_step = usize::MAX;
    let mut send_progress = |done: u64, ev: &mut dyn FnMut(Ev)| {
        let p = (done as f64 / total) as f32 * 0.97;
        if p - last_sent >= 0.004 || p >= 0.97 {
            last_sent = p;
            ev(Ev::Progress(p));
        }
    };

    while let Some(entry) = reader.peek().cloned() {
        let step = step_of(&labels, &entry.name);
        if step != cur_step {
            if cur_step != usize::MAX {
                ev(Ev::Step(cur_step, 2));
            }
            cur_step = step;
            ev(Ev::Step(step, 1));
        }
        ev(Ev::Current(tr!("copying {}", entry.name)));
        let target = dir.join(&entry.name);
        let tmp = dir.join(format!("{}.lfy-new", entry.name));
        let mut f = File::create(&tmp).map_err(|e| io_err(&tmp, e))?;
        let res = {
            let mut prog = |n: u64| {
                done += n;
                send_progress(done, ev);
            };
            reader.next_file(&mut f, &mut prog)
        };
        if let Err(e) = res.and_then(|_| f.flush()) {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(io_err(&target, e));
        }
        drop(f);
        swap_in(&tmp, &target).map_err(|e| io_err(&target, e))?;
    }
    if cur_step != usize::MAX {
        ev(Ev::Step(cur_step, 2));
    }

    // uninstall.exe = this stub without its payload.
    ev(Ev::Step(last, 1));
    ev(Ev::Current(tr!("writing {}", UNINSTALL_EXE)));
    let un = dir.join(UNINSTALL_EXE);
    let un_tmp = dir.join(format!("{UNINSTALL_EXE}.lfy-new"));
    payload::write_stub(exe, &un_tmp).map_err(|e| io_err(&un, e))?;
    swap_in(&un_tmp, &un).map_err(|e| io_err(&un, e))?;
    done += stub;
    send_progress(done, ev);

    if !plan.update {
        ev(Ev::Current(tr!("creating shortcuts")));
        write_shortcuts(plan)?;
    }
    ev(Ev::Current(tr!("registering with Windows")));
    let mut files: Vec<String> = header.files.iter().map(|f| f.name.clone()).collect();
    files.push(UNINSTALL_EXE.into());
    // Keep the names the version being replaced recorded: a file it shipped that this
    // one doesn't is still on disk, and uninstall only deletes names on this list (or
    // the built-in ones), so dropping it here orphaned the file — and the folder.
    if let Some(prev) = find_own().filter(|e| e.dir == plan.dir && e.all_users == plan.all_users) {
        for name in prev.files {
            if !files.contains(&name) {
                files.push(name);
            }
        }
    }
    write_entry(plan, &header.version, &files, header.total_size() + stub)?;
    ev(Ev::Step(last, 2));
    ev(Ev::Progress(1.0));
    ev(Ev::Current(tr!("done")));
    Ok(())
}

fn write_shortcuts(plan: &Plan) -> Result<(), String> {
    let dir = &plan.dir;
    let app = dir.join(APP_EXE);
    let sm = start_menu_dir(plan.all_users).ok_or_else(|| tr!("could not find the Start menu folder"))?;
    win::create_shortcut(&sm.join("Lightify.lnk"), &app, "", dir, &app, "Lightify")?;
    win::create_shortcut(&sm.join("Uninstall Lightify.lnk"), &dir.join(UNINSTALL_EXE), "--uninstall", dir, &app, "Uninstall Lightify")?;
    if plan.desktop {
        let desk = desktop_dir(plan.all_users).ok_or_else(|| tr!("could not find the desktop folder"))?;
        win::create_shortcut(&desk.join("Lightify.lnk"), &app, "", dir, &app, "Lightify")?;
    }
    Ok(())
}

fn write_entry(plan: &Plan, version: &str, files: &[String], bytes: u64) -> Result<(), String> {
    let dir = plan.dir.to_string_lossy();
    let k = Key::create(hive_for(plan.all_users), &own_subkey())?;
    let un = format!("\"{dir}\\{UNINSTALL_EXE}\"");
    k.set_str("DisplayName", "Lightify")?;
    k.set_str("DisplayVersion", version)?;
    k.set_str("Publisher", "Lightify")?;
    k.set_str("DisplayIcon", &format!("{dir}\\{APP_EXE}"))?;
    k.set_str("InstallLocation", &dir)?;
    k.set_str("UninstallString", &format!("{un} --uninstall"))?;
    k.set_str("QuietUninstallString", &format!("{un} --uninstall --silent"))?;
    k.set_str("URLInfoAbout", "https://lightify.stream")?;
    k.set_str("InstallDate", &today_yyyymmdd())?;
    k.set_dword("EstimatedSize", (bytes / 1024).min(u32::MAX as u64) as u32)?;
    k.set_dword("NoModify", 1)?;
    k.set_dword("NoRepair", 1)?;
    k.set_str("LightifyFiles", &files.join(";"))?;
    Ok(())
}

// ── Uninstall ───────────────────────────────────────────────────────────────

pub struct Removal {
    pub dir: PathBuf,
    pub all_users: bool,
    pub wipe: bool,
    /// Extra file names recorded at install time (registry `LightifyFiles`).
    pub files: Vec<String>,
}

pub fn removal_steps(wipe: bool) -> Vec<String> {
    let mut v = vec![lightify_i18n::tr_noop!("App files").to_string(), STEP_FINAL.to_string()];
    if wipe {
        v.push(lightify_i18n::tr_noop!("Settings and Spotify sign-in").into());
    }
    v
}

/// Does `dir` look like a Lightify install? (Guards against an uninstaller copied
/// somewhere odd deleting files that aren't ours.)
pub fn looks_installed(dir: &Path) -> bool {
    dir.join(APP_EXE).is_file() || dir.join(UNINSTALL_EXE).is_file() || find_own().is_some_and(|e| win::same_path(&e.dir, dir))
}

/// Paths a wipe must never delete: the user's download folders.
fn protected_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let dirs = user_data_dirs();
    if let Some(lfy) = dirs.first() {
        if let Ok(text) = fs::read_to_string(lfy.join("lightify_config.json")) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(p) = j.get("download_path").and_then(|v| v.as_str()).filter(|p| !p.trim().is_empty()) {
                    v.push(PathBuf::from(p.trim()));
                }
            }
        }
    }
    if let Some(ots) = dirs.get(1) {
        if let Ok(text) = fs::read_to_string(ots.join("otsconfig.json")) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&text) {
                for key in ["audio_download_path", "video_download_path"] {
                    if let Some(p) = j.get(key).and_then(|v| v.as_str()).filter(|p| !p.trim().is_empty()) {
                        v.push(PathBuf::from(p.trim()));
                    }
                }
            }
        }
    }
    if let Some(m) = dirs::audio_dir() {
        v.push(m.join("LightifyDownloads"));
    }
    v
}

/// Delete `dir` recursively, except any `keep` path inside it (and its parents).
/// Never follows links.
fn remove_tree_except(dir: &Path, keep: &[PathBuf]) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let inside: Vec<&PathBuf> = keep.iter().filter(|k| win::path_within(k, dir)).collect();
    if inside.iter().any(|k| win::same_path(k, dir)) {
        return Ok(());
    }
    if inside.is_empty() {
        return fs::remove_dir_all(dir);
    }
    for e in fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let ft = e.file_type()?;
        if ft.is_symlink() {
            let _ = fs::remove_file(&p).or_else(|_| fs::remove_dir(&p));
        } else if ft.is_dir() {
            remove_tree_except(&p, keep)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    let _ = fs::remove_dir(dir);
    Ok(())
}

/// Remove an install. Returns files that could not be deleted (in use).
pub fn uninstall(r: &Removal, ev: &mut dyn FnMut(Ev)) -> Result<Vec<String>, String> {
    let mut leftovers = Vec::new();
    let dir = &r.dir;
    ev(Ev::Step(0, 1));
    let mut names: Vec<String> = KNOWN_FILES.iter().map(|s| s.to_string()).collect();
    for f in &r.files {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(f)) {
            names.push(f.clone());
        }
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let l = n.to_lowercase();
            if (l.ends_with(".log") || l.contains(".lfy-old") || l.ends_with(".lfy-new")) && !names.contains(&n) {
                names.push(n);
            }
        }
    }
    let n_total = names.len().max(1) as f32;
    for (i, n) in names.iter().enumerate() {
        let p = dir.join(n);
        if p.is_file() {
            ev(Ev::Current(tr!("removing {}", n)));
            if fs::remove_file(&p).is_err() {
                leftovers.push(p.to_string_lossy().into_owned());
            }
        }
        ev(Ev::Progress(0.6 * (i + 1) as f32 / n_total));
    }
    let _ = fs::remove_dir(dir);
    ev(Ev::Step(0, 2));

    ev(Ev::Step(1, 1));
    ev(Ev::Current(tr!("removing shortcuts")));
    if let Some(sm) = start_menu_dir(r.all_users) {
        let _ = fs::remove_file(sm.join("Lightify.lnk"));
        let _ = fs::remove_file(sm.join("Uninstall Lightify.lnk"));
        let _ = fs::remove_dir(&sm);
    }
    if let Some(desk) = desktop_dir(r.all_users) {
        let lnk = desk.join("Lightify.lnk");
        if lnk.is_file() {
            let _ = fs::remove_file(lnk);
        }
    }
    ev(Ev::Current(tr!("removing the Apps entry")));
    win::delete_key_tree(hive_for(r.all_users), &own_subkey())?;
    if let Some(u) = updates_dir() {
        let _ = fs::remove_dir_all(&u);
        if let Some(parent) = u.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    ev(Ev::Progress(if r.wipe { 0.8 } else { 1.0 }));
    ev(Ev::Step(1, 2));

    if r.wipe {
        ev(Ev::Step(2, 1));
        let keep = protected_paths();
        for d in user_data_dirs() {
            ev(Ev::Current(tr!("removing {}", d.display())));
            if let Err(e) = remove_tree_except(&d, &keep) {
                leftovers.push(format!("{} ({e})", d.display()));
            }
        }
        ev(Ev::Step(2, 2));
    }
    ev(Ev::Progress(1.0));
    ev(Ev::Current(tr!("done")));
    Ok(leftovers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(20_754), (2026, 10, 28));
    }

    #[test]
    fn dir_validation() {
        assert!(validate_dir(Path::new(r"C:\Users\x\AppData\Local\Programs\Lightify")).is_ok());
        assert!(validate_dir(Path::new(r"\\server\share\Lightify")).is_ok());
        assert!(validate_dir(Path::new(r"C:\")).is_err());
        assert!(validate_dir(Path::new(r"relative\Lightify")).is_err());
        assert!(validate_dir(Path::new(r"%NOPE%\Lightify")).is_err());
        assert!(validate_dir(Path::new(r"C:\Windows\Lightify")).is_err());
    }

    #[test]
    fn resolve_trims_and_expands() {
        std::env::set_var("LFY_OPS_TEST", r"D:\Apps");
        assert_eq!(resolve_dir(r#" "%LFY_OPS_TEST%\Lightify\" "#), PathBuf::from(r"D:\Apps\Lightify"));
        assert_eq!(resolve_dir(r"C:\"), PathBuf::from(r"C:\"));
    }

    #[test]
    fn remove_tree_keeps_protected() {
        let d = std::env::temp_dir().join(format!("lfy-rm-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a").join("music")).unwrap();
        fs::create_dir_all(d.join("b")).unwrap();
        fs::write(d.join("a").join("music").join("song.mp3"), b"x").unwrap();
        fs::write(d.join("a").join("cfg.json"), b"x").unwrap();
        fs::write(d.join("b").join("x"), b"x").unwrap();
        remove_tree_except(&d, &[d.join("a").join("music")]).unwrap();
        assert!(d.join("a").join("music").join("song.mp3").is_file());
        assert!(!d.join("a").join("cfg.json").exists());
        assert!(!d.join("b").exists());
        remove_tree_except(&d, &[]).unwrap();
        assert!(!d.exists());
    }
}
