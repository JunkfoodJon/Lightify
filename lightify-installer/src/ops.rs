//! What Setup actually does: locate installs, copy the payload, write shortcuts and
//! the Apps-list entry, migrate an Inno Setup install, uninstall.
//!
//! Sandbox: with `LIGHTIFY_INSTALL_SANDBOX=<dir>` set, every location moves under that
//! directory (install dirs, Start menu, desktop, user data, updates) and the registry
//! entries become `...\Uninstall\Lightify-Sandbox` (per-user) and `Lightify-Sandbox-All`
//! (all-users), both under HKCU. Inno detection and elevation are off. That is how Setup
//! is exercised end to end without touching a real install.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

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

/// The entry's key for a scope, opened in `hive_for` the same scope. A real install has
/// one name in two hives; a sandbox has one hive, so two names.
fn own_subkey(all_users: bool) -> String {
    let name = match (sandbox().is_some(), all_users) {
        (false, _) => "Lightify",
        (true, false) => "Lightify-Sandbox",
        (true, true) => "Lightify-Sandbox-All",
    };
    format!(r"{UNINSTALL_ROOT}\{name}")
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

/// Is `dir` free for Lightify to use? Missing or empty, or holding nothing but what an
/// install of ours leaves there. Setup overwrites its file names and uninstall deletes
/// them, so a folder with anything else in it is somebody else's.
fn folder_is_dedicated(dir: &Path) -> bool {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return e.kind() == io::ErrorKind::NotFound,
    };
    let names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_lowercase())
        .filter(|n| !is_setup_leftover(n))
        .collect();
    let ours = |n: &String| KNOWN_FILES.iter().any(|k| k.eq_ignore_ascii_case(n)) || n.starts_with("unins000.");
    names.is_empty()
        || (names.iter().all(ours)
            && names.iter().any(|n| n.eq_ignore_ascii_case(APP_EXE) || n.eq_ignore_ascii_case(UNINSTALL_EXE)))
}

/// `validate_dir`, plus: may Setup put files there? An install this PC already knows
/// about keeps its folder, whatever else has ended up in it.
pub fn validate_target(p: &Path) -> Result<(), String> {
    validate_dir(p)?;
    if find_own_at(p).is_some() || find_inno().is_some_and(|e| same_folder(&e.dir, p)) || folder_is_dedicated(p) {
        return Ok(());
    }
    Err(tr!("That folder has other files in it. Pick an empty one, or add \\Lightify to the end."))
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

/// A user's own folders that an uninstall acts on. They belong to whoever started it, and
/// an elevated stage 2 may be running as another admin (over-the-shoulder UAC), whose
/// profile is not that user's: so stage 1 resolves them and hands them over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserDirs {
    /// Settings + cached sign-in (lightify-core config.rs `data_dir()`, default location only).
    pub data: Option<PathBuf>,
    /// The downloader bridge's state (`%APPDATA%\onthespot`).
    pub bridge: Option<PathBuf>,
    /// Where lightify-core's updater keeps downloaded installers.
    pub updates: Option<PathBuf>,
    /// The Music folder (the default download folder is inside it).
    pub music: Option<PathBuf>,
}

impl UserDirs {
    /// This process's own user.
    pub fn current() -> UserDirs {
        if let Some(sb) = sandbox() {
            return UserDirs {
                data: Some(sb.join("data").join("Lightify")),
                bridge: Some(sb.join("data").join("onthespot")),
                updates: Some(sb.join("localdata").join("Lightify").join("updates")),
                music: Some(sb.join("music")),
            };
        }
        let roaming = std::env::var_os("APPDATA").filter(|a| !a.is_empty()).map(PathBuf::from).or_else(dirs::config_dir);
        UserDirs {
            data: dirs::data_dir().map(|d| d.join("Lightify")),
            bridge: roaming.map(|a| a.join("onthespot")),
            updates: dirs::data_local_dir().map(|d| d.join("Lightify").join("updates")),
            music: dirs::audio_dir(),
        }
    }

    /// Stage 2's command-line form.
    pub fn to_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        for (flag, dir) in [("--user-data", &self.data), ("--user-bridge", &self.bridge), ("--user-updates", &self.updates), ("--user-music", &self.music)] {
            if let Some(d) = dir {
                a.push(flag.to_string());
                a.push(d.to_string_lossy().into_owned());
            }
        }
        a
    }

    /// What stage 2 acts on: the folders it was handed (this copy's own user if none, which
    /// only a hand-started stage 2 has). It may be elevated and it deletes these trees, so
    /// each must be the folder it claims to be; one that isn't is left alone.
    pub fn handed_over(self) -> UserDirs {
        if self == UserDirs::default() {
            return UserDirs::current();
        }
        UserDirs {
            data: self.data.filter(|d| is_folder_named(d, "Lightify")),
            bridge: self.bridge.filter(|d| is_folder_named(d, "onthespot")),
            updates: self.updates.filter(|d| is_folder_named(d, "updates") && d.parent().is_some_and(|p| is_folder_named(p, "Lightify"))),
            music: self.music.filter(|d| is_plain_folder(d)),
        }
    }
}

fn is_plain_folder(p: &Path) -> bool {
    validate_dir(p).is_ok() && !p.components().any(|c| matches!(c, Component::ParentDir))
}

fn is_folder_named(p: &Path, name: &str) -> bool {
    is_plain_folder(p) && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name))
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

/// The same folder, however it is spelled (case, trailing slash, 8.3 name, junction)?
fn same_folder(a: &Path, b: &Path) -> bool {
    win::same_path(a, b) || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// This installer's entry for one scope.
fn read_own(all_users: bool) -> Option<Existing> {
    let k = Key::open(hive_for(all_users), &own_subkey(all_users))?;
    let dir = k.get_str("InstallLocation").filter(|d| !d.trim().is_empty())?;
    Some(Existing {
        all_users,
        dir: trim_path(&dir),
        version: k.get_str("DisplayVersion").unwrap_or_default(),
        inno_uninstaller: None,
        files: k
            .get_str("LightifyFiles")
            .map(|f| f.split(';').filter(|n| payload::valid_name(n)).map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

/// This installer's own entry (per-user first, then all-users).
pub fn find_own() -> Option<Existing> {
    read_own(false).or_else(|| read_own(true))
}

/// The entry, in either scope, that names `dir`. With both a per-user and an all-users
/// install on the PC, this is how an uninstaller finds its own.
pub fn find_own_at(dir: &Path) -> Option<Existing> {
    [false, true].into_iter().filter_map(read_own).find(|e| same_folder(&e.dir, dir))
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

/// Where an install into `dir` replaces something: `dir` itself, and the folder this
/// scope's Lightify (or an old Inno install) is in now, if that is somewhere else. Lightify
/// must not be left running from any of them.
pub fn install_folders(dir: &Path, all_users: bool) -> Vec<PathBuf> {
    let mut v = vec![dir.to_path_buf()];
    for e in read_own(all_users).into_iter().chain(find_inno()) {
        if !v.iter().any(|d| same_folder(d, &e.dir)) {
            v.push(e.dir);
        }
    }
    v
}

pub fn app_running_in(folders: &[PathBuf]) -> bool {
    folders.iter().any(|d| !running_app(d).is_empty())
}

pub fn close_apps_in(folders: &[PathBuf], timeout_ms: u32) -> bool {
    folders.iter().all(|d| close_app(d, timeout_ms))
}

/// Lightify never runs as administrator: an elevated Setup (whether it was handed over to
/// or started with "Run as administrator") starts it through the desktop shell instead,
/// which runs it as the signed-in user with that user's environment.
pub fn launch_app(dir: &Path) -> Result<(), String> {
    let exe = dir.join(APP_EXE);
    if sandbox().is_none() && win::is_elevated() {
        return win::run_through_shell(&exe, "", dir).map_err(|e| tr!("could not start {}: {}", exe.display(), e));
    }
    win::spawn_detached(&exe, &[], Some(dir)).map_err(|e| tr!("could not start {}: {}", exe.display(), e))
}

// ── Install ─────────────────────────────────────────────────────────────────

pub struct Plan {
    pub dir: PathBuf,
    pub all_users: bool,
    pub desktop: bool,
    /// `--update`: replace files + refresh the entry; leave shortcuts alone.
    pub update: bool,
    /// An Inno install to remove before the swap (its uninstaller keeps user config).
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

/// A `*.lfy-old` / `*.lfy-new` file (lowercase name), which only Setup makes.
fn is_setup_leftover(lower_name: &str) -> bool {
    lower_name.ends_with(".lfy-old") || lower_name.ends_with(".lfy-new") || lower_name.contains(".lfy-old-")
}

/// Remove `*.lfy-old` / `*.lfy-new` leftovers from an earlier run.
fn clean_leftovers(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if is_setup_leftover(&e.file_name().to_string_lossy().to_lowercase()) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

/// Rename `target` to a free `<name>.lfy-old[-n]` beside it. Windows allows that for a
/// running exe, and an aside file is removed by the next run if it can't be yet.
fn set_aside(target: &Path) -> io::Result<Option<PathBuf>> {
    if !target.exists() {
        return Ok(None);
    }
    let name = target.file_name().unwrap_or_default().to_string_lossy().into_owned();
    for i in 0..=20 {
        let old = target.with_file_name(if i == 0 { format!("{name}.lfy-old") } else { format!("{name}.lfy-old-{i}") });
        if !old.exists() || fs::remove_file(&old).is_ok() {
            fs::rename(target, &old)?;
            return Ok(Some(old));
        }
    }
    Err(io::Error::new(io::ErrorKind::PermissionDenied, tr!("{} is in use", name)))
}

/// One file moved into place, and the file it replaced until the install has succeeded.
struct Swapped {
    target: PathBuf,
    aside: Option<PathBuf>,
}

/// Move every staged `(temp, target)` into place, recording each in `swapped` as it goes.
fn swap_all(staged: &[(PathBuf, PathBuf)], swapped: &mut Vec<Swapped>) -> Result<(), String> {
    for (tmp, target) in staged {
        let aside = set_aside(target).map_err(|e| io_err(target, e))?;
        if let Err(e) = fs::rename(tmp, target) {
            if let Some(a) = &aside {
                let _ = fs::rename(a, target);
            }
            return Err(io_err(target, e));
        }
        swapped.push(Swapped { target: target.clone(), aside });
    }
    Ok(())
}

/// Put back what `swap_all` replaced, and take away what it added.
fn undo_swaps(swapped: &[Swapped]) {
    for s in swapped.iter().rev() {
        if fs::remove_file(&s.target).is_err() {
            let _ = set_aside(&s.target);
        }
        if let Some(a) = &s.aside {
            let _ = fs::rename(a, &s.target);
        }
    }
}

/// Throw away a failed attempt's temp files, and the folder if that was all it held.
fn discard_staged(dir: &Path, staged: &[(PathBuf, PathBuf)]) {
    for (tmp, _) in staged {
        let _ = fs::remove_file(tmp);
    }
    let _ = fs::remove_dir(dir);
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

/// Unpack the payload and the uninstaller beside their targets, recording each file in
/// `staged` before it is written. Nothing already installed is touched.
fn stage_files(
    reader: &mut PayloadReader,
    exe: &Path,
    dir: &Path,
    labels: &[String],
    ev: &mut dyn FnMut(Ev),
    staged: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), String> {
    let stub = payload::stub_len(exe).map_err(|e| io_err(exe, e))?;
    let total = (reader.header.total_size() + stub).max(1) as f64;
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
        let step = step_of(labels, &entry.name);
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
        staged.push((tmp.clone(), target.clone()));
        let mut f = File::create(&tmp).map_err(|e| io_err(&tmp, e))?;
        let res = {
            let mut prog = |n: u64| {
                done += n;
                send_progress(done, ev);
            };
            reader.next_file(&mut f, &mut prog)
        };
        res.and_then(|_| f.flush()).map_err(|e| io_err(&target, e))?;
    }
    if cur_step != usize::MAX {
        ev(Ev::Step(cur_step, 2));
    }

    // uninstall.exe = this stub without its payload.
    ev(Ev::Step(labels.len() - 1, 1));
    ev(Ev::Current(tr!("writing {}", UNINSTALL_EXE)));
    let un = dir.join(UNINSTALL_EXE);
    let un_tmp = dir.join(format!("{UNINSTALL_EXE}.lfy-new"));
    staged.push((un_tmp.clone(), un.clone()));
    payload::write_stub(exe, &un_tmp).map_err(|e| io_err(&un, e))?;
    done += stub;
    send_progress(done, ev);
    Ok(())
}

/// All or nothing. The payload is unpacked and checked beside what is installed, then
/// swapped in; whatever fails after that puts the old files, shortcuts and entry back, so
/// a failed update leaves the old version working and a failed first install leaves
/// nothing behind. (An Inno install's own uninstaller can't be undone: it runs last before
/// the swap, so only a failure of the swap itself comes after it.)
pub fn install(plan: &Plan, exe: &Path, ev: &mut dyn FnMut(Ev)) -> Result<(), String> {
    let mut reader = PayloadReader::open(exe)
        .map_err(|e| tr!("this Setup file is damaged ({}) - download it again", e))?
        .ok_or_else(|| tr!("this copy of Setup has no files in it (it is the uninstaller) - download Setup from lightify.stream"))?;
    let header = reader.header.clone();
    let labels = step_labels(&header);
    let last = labels.len() - 1;
    let dir = &plan.dir;

    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    clean_leftovers(dir);

    let mut staged = Vec::new();
    let mut staging = stage_files(&mut reader, exe, dir, &labels, ev, &mut staged);
    if let (Ok(()), Some(old)) = (&staging, &plan.migrate) {
        ev(Ev::Current(tr!("removing the previous installer")));
        staging = run_inno_uninstaller(old);
    }
    if let Err(e) = staging {
        discard_staged(dir, &staged);
        return Err(e);
    }

    let prior = read_own(plan.all_users);
    let moved = prior.as_ref().filter(|e| !same_folder(&e.dir, dir));
    let mut swapped = Vec::new();
    let mut result = swap_all(&staged, &mut swapped);
    if result.is_ok() {
        result = register(plan, &header, exe, prior.as_ref(), moved, ev);
    }
    if let Err(e) = result {
        undo_swaps(&swapped);
        undo_registration(plan, prior.as_ref(), moved);
        discard_staged(dir, &staged);
        return Err(e);
    }
    for a in swapped.iter().filter_map(|s| s.aside.as_ref()) {
        let _ = fs::remove_file(a);
    }

    if let Some(old) = moved {
        ev(Ev::Current(tr!("removing the old copy")));
        remove_old_copy(old);
    }
    ev(Ev::Step(last, 2));
    ev(Ev::Progress(1.0));
    ev(Ev::Current(tr!("done")));
    Ok(())
}

/// The shortcuts and the Apps entry for a plan whose files are in place.
fn register(plan: &Plan, header: &Header, exe: &Path, prior: Option<&Existing>, moved: Option<&Existing>, ev: &mut dyn FnMut(Ev)) -> Result<(), String> {
    if !plan.update {
        ev(Ev::Current(tr!("creating shortcuts")));
        // A desktop shortcut the move would leave pointing at nothing follows the app.
        let desktop = plan.desktop || (moved.is_some() && has_desktop_shortcut(plan.all_users));
        write_shortcuts(&plan.dir, plan.all_users, desktop)?;
    }
    ev(Ev::Current(tr!("registering with Windows")));
    let mut files: Vec<String> = header.files.iter().map(|f| f.name.clone()).collect();
    files.push(UNINSTALL_EXE.into());
    // Keep the names the version being replaced recorded: a file it shipped that this
    // one doesn't is still on disk, and uninstall only deletes names on this list (or
    // the built-in ones), so dropping it here orphaned the file — and the folder.
    if let Some(prev) = prior.filter(|e| same_folder(&e.dir, &plan.dir)) {
        for name in &prev.files {
            if !files.contains(name) {
                files.push(name.clone());
            }
        }
    }
    let stub = payload::stub_len(exe).map_err(|e| io_err(exe, e))?;
    write_entry(plan.all_users, &plan.dir, &header.version, &files, Some(header.total_size() + stub))
}

/// Back to the shortcuts and entry from before a failed install.
fn undo_registration(plan: &Plan, prior: Option<&Existing>, moved: Option<&Existing>) {
    if !plan.update {
        match (prior, moved) {
            (None, _) => remove_shortcuts(plan.all_users),
            (_, Some(old)) => {
                let _ = write_shortcuts(&old.dir, plan.all_users, has_desktop_shortcut(plan.all_users));
            }
            _ => {}
        }
    }
    match prior {
        None => {
            let _ = win::delete_key_tree(hive_for(plan.all_users), &own_subkey(plan.all_users));
        }
        Some(p) => {
            let _ = write_entry(p.all_users, &p.dir, &p.version, &p.files, None);
        }
    }
}

/// After a successful move, the old folder's copy goes: it would be a second Lightify
/// nothing updates, whose uninstaller the user could still run.
fn remove_old_copy(old: &Existing) {
    if validate_dir(&old.dir).is_err() {
        return;
    }
    let left = remove_app_files(&old.dir, &old.files, &mut |_| {}, 0.0);
    if !left.is_empty() {
        crate::log(&format!("move: removing after exit {left:?}"));
        win::delete_after_exit(&left, std::slice::from_ref(&old.dir));
    }
}

fn has_desktop_shortcut(all_users: bool) -> bool {
    desktop_dir(all_users).is_some_and(|d| d.join("Lightify.lnk").is_file())
}

fn write_shortcuts(dir: &Path, all_users: bool, desktop: bool) -> Result<(), String> {
    let app = dir.join(APP_EXE);
    let sm = start_menu_dir(all_users).ok_or_else(|| tr!("could not find the Start menu folder"))?;
    win::create_shortcut(&sm.join("Lightify.lnk"), &app, "", dir, &app, "Lightify")?;
    win::create_shortcut(&sm.join("Uninstall Lightify.lnk"), &dir.join(UNINSTALL_EXE), "--uninstall", dir, &app, "Uninstall Lightify")?;
    if desktop {
        let desk = desktop_dir(all_users).ok_or_else(|| tr!("could not find the desktop folder"))?;
        win::create_shortcut(&desk.join("Lightify.lnk"), &app, "", dir, &app, "Lightify")?;
    }
    Ok(())
}

fn remove_shortcuts(all_users: bool) {
    if let Some(sm) = start_menu_dir(all_users) {
        let _ = fs::remove_file(sm.join("Lightify.lnk"));
        let _ = fs::remove_file(sm.join("Uninstall Lightify.lnk"));
        let _ = fs::remove_dir(&sm);
    }
    if let Some(desk) = desktop_dir(all_users) {
        let lnk = desk.join("Lightify.lnk");
        if lnk.is_file() {
            let _ = fs::remove_file(lnk);
        }
    }
}

/// `bytes` None (putting an old entry back) leaves the size and date as they are.
fn write_entry(all_users: bool, install_dir: &Path, version: &str, files: &[String], bytes: Option<u64>) -> Result<(), String> {
    let dir = install_dir.to_string_lossy();
    let k = Key::create(hive_for(all_users), &own_subkey(all_users))?;
    let un = format!("\"{dir}\\{UNINSTALL_EXE}\"");
    k.set_str("DisplayName", "Lightify")?;
    k.set_str("DisplayVersion", version)?;
    k.set_str("Publisher", "Lightify")?;
    k.set_str("DisplayIcon", &format!("{dir}\\{APP_EXE}"))?;
    k.set_str("InstallLocation", &dir)?;
    k.set_str("UninstallString", &format!("{un} --uninstall"))?;
    k.set_str("QuietUninstallString", &format!("{un} --uninstall --silent"))?;
    k.set_str("URLInfoAbout", "https://lightify.stream")?;
    if let Some(bytes) = bytes {
        k.set_str("InstallDate", &today_yyyymmdd())?;
        k.set_dword("EstimatedSize", (bytes / 1024).min(u32::MAX as u64) as u32)?;
    }
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
    /// The folders of the user who started the uninstall.
    pub user: UserDirs,
}

/// How an uninstall went, when it could run at all.
#[derive(Debug, Default)]
pub struct Removed {
    /// App files still in use: plain paths, to delete once the process has exited.
    pub leftovers: Vec<PathBuf>,
    /// Settings folders a wipe could not delete (each with why), to tell the user about.
    pub not_wiped: Vec<String>,
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
    dir.join(APP_EXE).is_file() || dir.join(UNINSTALL_EXE).is_file() || find_own_at(dir).is_some()
}

/// Paths a wipe must never delete: that user's download folders.
fn protected_paths(user: &UserDirs) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(lfy) = &user.data {
        if let Ok(text) = fs::read_to_string(lfy.join("lightify_config.json")) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(p) = j.get("download_path").and_then(|v| v.as_str()).filter(|p| !p.trim().is_empty()) {
                    v.push(PathBuf::from(p.trim()));
                }
            }
        }
    }
    if let Some(ots) = &user.bridge {
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
    if let Some(m) = &user.music {
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

/// Delete an install's own files from `dir` (the built-in names, `recorded`, Setup's temp
/// files) and the folder if that empties it. Nothing else: the folder may hold other
/// things, and `*.log` isn't ours to take (ours is the one on the built-in list). Returns
/// the files still in use. `progress` is the share of `Ev::Progress` this covers.
fn remove_app_files(dir: &Path, recorded: &[String], ev: &mut dyn FnMut(Ev), progress: f32) -> Vec<PathBuf> {
    let mut names: Vec<String> = KNOWN_FILES.iter().map(|s| s.to_string()).collect();
    for f in recorded {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(f)) {
            names.push(f.clone());
        }
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if is_setup_leftover(&n.to_lowercase()) && !names.contains(&n) {
                names.push(n);
            }
        }
    }
    let mut leftovers = Vec::new();
    let n_total = names.len().max(1) as f32;
    for (i, n) in names.iter().enumerate() {
        let p = dir.join(n);
        if p.is_file() {
            ev(Ev::Current(tr!("removing {}", n)));
            if fs::remove_file(&p).is_err() {
                leftovers.push(p);
            }
        }
        ev(Ev::Progress(progress * (i + 1) as f32 / n_total));
    }
    let _ = fs::remove_dir(dir);
    leftovers
}

/// Does an entry whose `InstallLocation` is `claimed` belong to the install in `dir`? An
/// entry naming another folder is another install's, along with its shortcuts; a missing
/// or blank one claims nothing, so an orphan can still clean up after itself.
fn entry_is_ours(claimed: Option<&str>, dir: &Path) -> bool {
    match claimed.filter(|c| !c.trim().is_empty()) {
        Some(c) => same_folder(&trim_path(c), dir),
        None => true,
    }
}

/// Remove an install. The shortcuts, Apps entry and update cache go only if the entry
/// doesn't name a different folder.
pub fn uninstall(r: &Removal, ev: &mut dyn FnMut(Ev)) -> Result<Removed, String> {
    let dir = &r.dir;
    ev(Ev::Step(0, 1));
    let leftovers = remove_app_files(dir, &r.files, ev, 0.6);
    ev(Ev::Step(0, 2));

    ev(Ev::Step(1, 1));
    let claimed = Key::open(hive_for(r.all_users), &own_subkey(r.all_users)).and_then(|k| k.get_str("InstallLocation"));
    if entry_is_ours(claimed.as_deref(), dir) {
        ev(Ev::Current(tr!("removing shortcuts")));
        remove_shortcuts(r.all_users);
        ev(Ev::Current(tr!("removing the Apps entry")));
        win::delete_key_tree(hive_for(r.all_users), &own_subkey(r.all_users))?;
        if let Some(u) = &r.user.updates {
            let _ = fs::remove_dir_all(u);
            if let Some(parent) = u.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    ev(Ev::Progress(if r.wipe { 0.8 } else { 1.0 }));
    ev(Ev::Step(1, 2));

    let mut not_wiped = Vec::new();
    if r.wipe {
        ev(Ev::Step(2, 1));
        let keep = protected_paths(&r.user);
        for d in [&r.user.data, &r.user.bridge].into_iter().flatten() {
            ev(Ev::Current(tr!("removing {}", d.display())));
            if let Err(e) = remove_tree_except(d, &keep) {
                not_wiped.push(format!("{} ({e})", d.display()));
            }
        }
        ev(Ev::Step(2, 2));
    }
    ev(Ev::Progress(1.0));
    ev(Ev::Current(tr!("done")));
    Ok(Removed { leftovers, not_wiped })
}

/// The sentence for settings a wipe could not delete.
pub fn not_wiped_text(not_wiped: &[String]) -> String {
    tr!("Lightify was removed, but these settings could not be deleted. Close whatever is using them, then delete them yourself:\n{}", not_wiped.join("\n"))
}

/// Tests that set `LIGHTIFY_INSTALL_SANDBOX` hold this: without it, one test clearing the
/// variable would send another's install or uninstall at the real system.
#[cfg(test)]
pub static SANDBOX_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    // ── Whole installs and uninstalls, in the sandbox ───────────────────────────
    //
    // The sandbox's "all users" is a second key under HKCU and folders of its own, so these
    // prove the logic, not HKLM, UAC or a second Windows account.

    /// Sets the sandbox for one test and takes everything away again, its registry keys too.
    struct Sandbox {
        root: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Sandbox {
        fn new(tag: &str) -> Sandbox {
            let lock = SANDBOX_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            let root = std::env::temp_dir().join(format!("lfy-ops-sandbox-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            std::env::set_var("LIGHTIFY_INSTALL_SANDBOX", &root);
            let sb = Sandbox { root, _lock: lock };
            sb.clear_entries();
            sb
        }

        fn clear_entries(&self) {
            assert!(sandbox().is_some(), "these tests must never reach the real entry");
            for all_users in [false, true] {
                win::delete_key_tree(Hive::User, &own_subkey(all_users)).unwrap();
            }
        }

        /// A Setup whose files say which version they are.
        fn setup(&self, version: &str) -> PathBuf {
            let src = self.root.join(format!("src-{version}"));
            fs::create_dir_all(&src).unwrap();
            let mut files = Vec::new();
            for name in ["LICENSE.md", APP_EXE, BRIDGE_EXE, FFMPEG_EXE] {
                let f = src.join(name);
                fs::write(&f, format!("{name} {version}")).unwrap();
                files.push((name.to_string(), f));
            }
            let stub = src.join("stub.exe");
            fs::write(&stub, format!("stub {version}")).unwrap();
            let out = self.root.join(format!("setup-{version}.exe"));
            payload::write_payload(&stub, &out, version, &files, 1, 1).unwrap();
            out
        }

        /// That Setup, with its payload damaged past the point the first files unpack.
        fn damaged_setup(&self, version: &str) -> PathBuf {
            let src = self.root.join(format!("src-bad-{version}"));
            fs::create_dir_all(&src).unwrap();
            // Noise, so the stream is long and the flipped byte lands in the last file.
            let noise: Vec<u8> = (0..400_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
            let mut files = Vec::new();
            for name in ["LICENSE.md", APP_EXE, FFMPEG_EXE] {
                let f = src.join(name);
                fs::write(&f, if name == FFMPEG_EXE { noise.clone() } else { format!("{name} {version}").into_bytes() }).unwrap();
                files.push((name.to_string(), f));
            }
            let stub = src.join("stub.exe");
            fs::write(&stub, "stub").unwrap();
            let out = self.root.join(format!("setup-bad-{version}.exe"));
            payload::write_payload(&stub, &out, version, &files, 1, 1).unwrap();
            let mut bytes = fs::read(&out).unwrap();
            let (offset, len) = payload::locate(&out).unwrap().unwrap();
            let at = (offset + len * 3 / 4) as usize;
            bytes[at] ^= 0x55;
            fs::write(&out, bytes).unwrap();
            out
        }

        fn install_to(&self, setup: &Path, dir: &Path, all_users: bool, desktop: bool) -> Result<(), String> {
            let plan = Plan { dir: dir.to_path_buf(), all_users, desktop, update: false, migrate: None };
            install(&plan, setup, &mut |_| {})
        }

        fn removal(&self, dir: &Path, all_users: bool, wipe: bool) -> Removal {
            Removal { dir: dir.to_path_buf(), all_users, wipe, files: Vec::new(), user: UserDirs::current() }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            self.clear_entries();
            std::env::remove_var("LIGHTIFY_INSTALL_SANDBOX");
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_default()
    }

    fn no_temp_files(dir: &Path) -> bool {
        fs::read_dir(dir).map_or(true, |rd| rd.flatten().all(|e| !e.file_name().to_string_lossy().contains(".lfy-")))
    }

    #[test]
    fn install_then_update_then_uninstall() {
        let sb = Sandbox::new("happy");
        let dir = sb.root.join("app");
        sb.install_to(&sb.setup("1.0.0"), &dir, false, true).unwrap();
        assert_eq!(read(&dir.join(APP_EXE)), "Lightify.exe 1.0.0");
        assert_eq!(read(&dir.join(UNINSTALL_EXE)), "stub 1.0.0");
        assert!(no_temp_files(&dir));
        assert!(start_menu_dir(false).unwrap().join("Lightify.lnk").is_file());
        assert!(has_desktop_shortcut(false));
        let e = find_own().unwrap();
        assert_eq!((e.version.as_str(), e.all_users, same_folder(&e.dir, &dir)), ("1.0.0", false, true));

        let plan = Plan { dir: dir.clone(), all_users: false, desktop: false, update: true, migrate: None };
        install(&plan, &sb.setup("2.0.0"), &mut |_| {}).unwrap();
        assert_eq!(read(&dir.join(APP_EXE)), "Lightify.exe 2.0.0");
        assert!(no_temp_files(&dir));
        assert_eq!(find_own().unwrap().version, "2.0.0");

        let done = uninstall(&sb.removal(&dir, false, false), &mut |_| {}).unwrap();
        assert!(done.leftovers.is_empty() && done.not_wiped.is_empty());
        assert!(!dir.exists());
        assert!(find_own().is_none());
        assert!(!start_menu_dir(false).unwrap().exists());
        assert!(!has_desktop_shortcut(false));
    }

    // R6: nothing half done, whichever step fails.
    #[test]
    fn failed_first_install_leaves_nothing() {
        let sb = Sandbox::new("fresh-fail");
        let dir = sb.root.join("app");
        assert!(sb.install_to(&sb.damaged_setup("1.0.0"), &dir, false, false).is_err());
        assert!(!dir.exists(), "a damaged payload leaves no folder");
        assert!(find_own().is_none());

        // Files all fine, then the Start menu can't be written: the last step fails.
        fs::write(start_menu_dir(false).unwrap().parent().unwrap(), "in the way").unwrap();
        assert!(sb.install_to(&sb.setup("1.0.0"), &dir, false, false).is_err());
        assert!(!dir.exists(), "copied files are taken away again");
        assert!(find_own().is_none(), "no Apps entry");
    }

    #[test]
    fn failed_update_keeps_the_old_version() {
        use std::os::windows::fs::OpenOptionsExt;
        let sb = Sandbox::new("update-fail");
        let dir = sb.root.join("app");
        sb.install_to(&sb.setup("1.0.0"), &dir, false, false).unwrap();
        let unchanged = |what: &str| {
            for name in ["LICENSE.md", APP_EXE, BRIDGE_EXE, FFMPEG_EXE] {
                assert_eq!(read(&dir.join(name)), format!("{name} 1.0.0"), "{what}: {name}");
            }
            assert_eq!(read(&dir.join(UNINSTALL_EXE)), "stub 1.0.0", "{what}");
            assert!(no_temp_files(&dir), "{what}");
            assert_eq!(find_own().unwrap().version, "1.0.0", "{what}");
        };
        let plan = Plan { dir: dir.clone(), all_users: false, desktop: false, update: true, migrate: None };

        // Damaged payload: fails while unpacking, before anything is touched.
        assert!(install(&plan, &sb.damaged_setup("2.0.0"), &mut |_| {}).is_err());
        unchanged("damaged payload");

        // A file that can't be moved (locked, no sharing): earlier swaps are undone.
        let setup2 = sb.setup("2.0.0");
        {
            let _hold = fs::OpenOptions::new().read(true).share_mode(0).open(dir.join(FFMPEG_EXE)).unwrap();
            assert!(install(&plan, &setup2, &mut |_| {}).is_err());
        }
        unchanged("locked file");

        // And once it is free, the same Setup goes through.
        install(&plan, &setup2, &mut |_| {}).unwrap();
        assert_eq!(read(&dir.join(FFMPEG_EXE)), "ffmpeg.exe 2.0.0");
    }

    // R3: a reinstall somewhere else moves the install.
    #[test]
    fn reinstall_elsewhere_moves_the_install() {
        let sb = Sandbox::new("move");
        let (old, new) = (sb.root.join("old"), sb.root.join("new"));
        sb.install_to(&sb.setup("1.0.0"), &old, false, true).unwrap();
        fs::write(old.join("my-notes.txt"), "mine").unwrap();
        let folders = install_folders(&new, false);
        assert_eq!(folders.len(), 2, "a running app is looked for in both");
        assert!(same_folder(&folders[1], &old));
        assert_eq!(install_folders(&old, false).len(), 1);

        sb.install_to(&sb.setup("2.0.0"), &new, false, false).unwrap();
        assert_eq!(read(&new.join(APP_EXE)), "Lightify.exe 2.0.0");
        assert!(same_folder(&find_own().unwrap().dir, &new));
        for name in [APP_EXE, BRIDGE_EXE, FFMPEG_EXE, "LICENSE.md", UNINSTALL_EXE] {
            assert!(!old.join(name).exists(), "{name} left behind");
        }
        assert_eq!(read(&old.join("my-notes.txt")), "mine", "only our own files go");
        assert!(has_desktop_shortcut(false), "the desktop shortcut it had is kept");

        // Same folder, spelled differently, is not a move.
        let spelled = PathBuf::from(new.to_string_lossy().to_uppercase());
        sb.install_to(&sb.setup("3.0.0"), &spelled, false, false).unwrap();
        assert_eq!(read(&new.join(APP_EXE)), "Lightify.exe 3.0.0");
    }

    #[test]
    fn failed_move_keeps_the_old_install() {
        let sb = Sandbox::new("move-fail");
        let (old, new) = (sb.root.join("old"), sb.root.join("new"));
        sb.install_to(&sb.setup("1.0.0"), &old, false, false).unwrap();
        assert!(sb.install_to(&sb.damaged_setup("2.0.0"), &new, false, false).is_err());
        assert!(!new.exists());
        assert_eq!(read(&old.join(APP_EXE)), "Lightify.exe 1.0.0");
        assert!(same_folder(&find_own().unwrap().dir, &old));
    }

    // R3: an orphan from before that fix must leave the real install's entries alone.
    #[test]
    fn an_orphans_uninstaller_leaves_the_real_install_alone() {
        let sb = Sandbox::new("orphan");
        let (orphan, real) = (sb.root.join("orphan"), sb.root.join("real"));
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join(APP_EXE), "old").unwrap();
        fs::write(orphan.join(UNINSTALL_EXE), "old").unwrap();
        sb.install_to(&sb.setup("2.0.0"), &real, false, true).unwrap();
        let updates = UserDirs::current().updates.unwrap();
        fs::create_dir_all(&updates).unwrap();

        uninstall(&sb.removal(&orphan, false, false), &mut |_| {}).unwrap();
        assert!(!orphan.exists());
        assert!(same_folder(&find_own().unwrap().dir, &real), "the entry stays");
        assert!(start_menu_dir(false).unwrap().join("Lightify.lnk").is_file(), "the shortcuts stay");
        assert!(has_desktop_shortcut(false));
        assert!(updates.is_dir(), "the update cache stays");

        // The real one's own uninstaller still takes it all.
        uninstall(&sb.removal(&real, false, false), &mut |_| {}).unwrap();
        assert!(find_own().is_none());
        assert!(!start_menu_dir(false).unwrap().exists());
    }

    #[test]
    fn entry_claims() {
        let d = Path::new(r"C:\Apps\Lightify");
        assert!(entry_is_ours(None, d));
        assert!(entry_is_ours(Some("  "), d));
        assert!(entry_is_ours(Some(r"c:\apps\lightify\"), d));
        assert!(!entry_is_ours(Some(r"D:\Other\Lightify"), d));
    }

    // R4: per-user and all-users installs side by side, matched by folder.
    #[test]
    fn two_scopes_are_kept_apart() {
        let sb = Sandbox::new("scopes");
        let (mine, machine) = (sb.root.join("mine"), sb.root.join("machine"));
        let setup = sb.setup("1.0.0");
        sb.install_to(&setup, &mine, false, false).unwrap();
        sb.install_to(&setup, &machine, true, false).unwrap();
        assert!(!find_own_at(&mine).unwrap().all_users);
        assert!(find_own_at(&machine).unwrap().all_users, "found in its own scope, though per-user comes first");
        assert!(find_own_at(&sb.root.join("elsewhere")).is_none());
        assert!(mine.is_dir() && machine.is_dir(), "two scopes are not a move");

        // Uninstalling the all-users one as if per-user (what the old lookup fell back to):
        // the per-user entry and shortcuts must survive it.
        uninstall(&sb.removal(&machine, false, false), &mut |_| {}).unwrap();
        assert!(same_folder(&read_own(false).unwrap().dir, &mine));
        assert!(start_menu_dir(false).unwrap().join("Lightify.lnk").is_file());

        // Its proper scope takes its own entry and shortcuts, and nothing of the other's.
        uninstall(&sb.removal(&machine, true, false), &mut |_| {}).unwrap();
        assert!(read_own(true).is_none());
        assert!(!start_menu_dir(true).unwrap().exists());
        assert!(same_folder(&read_own(false).unwrap().dir, &mine));
    }

    // R2: a wipe acts on the folders it was handed, not on this process's own profile.
    #[test]
    fn wipe_uses_the_handed_over_user() {
        let sb = Sandbox::new("wipe-user");
        let dir = sb.root.join("app");
        sb.install_to(&sb.setup("1.0.0"), &dir, true, false).unwrap();

        // This process's profile stands for the admin elevating; `user` for who asked.
        let admin = UserDirs::current();
        let profile = sb.root.join("pat");
        let user = UserDirs {
            data: Some(profile.join("Roaming").join("Lightify")),
            bridge: Some(profile.join("Roaming").join("onthespot")),
            updates: Some(profile.join("Local").join("Lightify").join("updates")),
            music: Some(profile.join("Music")),
        };
        let songs = profile.join("Roaming").join("Lightify").join("songs");
        let ots_songs = profile.join("Roaming").join("onthespot").join("downloads");
        let default_songs = profile.join("Music").join("LightifyDownloads");
        for d in [&songs, &ots_songs, &default_songs, user.updates.as_ref().unwrap(), admin.data.as_ref().unwrap(), admin.bridge.as_ref().unwrap(), admin.updates.as_ref().unwrap()] {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("file.txt"), "x").unwrap();
        }
        let data = user.data.as_ref().unwrap();
        let cfg = serde_json::json!({ "download_path": songs.to_string_lossy() }).to_string();
        fs::write(data.join("lightify_config.json"), cfg).unwrap();
        fs::write(data.join(".lightify_cache"), "sign-in").unwrap();
        let ots = serde_json::json!({ "audio_download_path": ots_songs.to_string_lossy() }).to_string();
        fs::write(user.bridge.as_ref().unwrap().join("otsconfig.json"), ots).unwrap();
        // The admin's config names a folder that protects nothing of Pat's.
        fs::write(admin.data.as_ref().unwrap().join("lightify_config.json"), r#"{"download_path":"C:\\admin-music"}"#).unwrap();

        let r = Removal { dir: dir.clone(), all_users: true, wipe: true, files: Vec::new(), user: user.clone() };
        let done = uninstall(&r, &mut |_| {}).unwrap();
        assert!(done.not_wiped.is_empty(), "{:?}", done.not_wiped);

        assert!(!data.join(".lightify_cache").exists(), "the asking user's sign-in goes");
        assert!(songs.join("file.txt").is_file(), "their configured download folder stays");
        assert!(ots_songs.join("file.txt").is_file(), "so does the bridge's");
        assert!(default_songs.join("file.txt").is_file(), "and the default one, in their Music");
        assert!(!user.updates.as_ref().unwrap().exists(), "their update cache goes");

        assert!(admin.data.as_ref().unwrap().join("file.txt").is_file(), "the admin's settings are not touched");
        assert!(admin.bridge.as_ref().unwrap().join("file.txt").is_file());
        assert!(admin.updates.as_ref().unwrap().join("file.txt").is_file());
    }

    // R2: stage 2 deletes these trees, possibly elevated, so it checks what it was handed.
    #[test]
    fn handed_over_folders_are_checked() {
        let good = UserDirs {
            data: Some(PathBuf::from(r"C:\Users\pat\AppData\Roaming\Lightify")),
            bridge: Some(PathBuf::from(r"C:\Users\pat\AppData\Roaming\onthespot")),
            updates: Some(PathBuf::from(r"C:\Users\pat\AppData\Local\Lightify\updates")),
            music: Some(PathBuf::from(r"C:\Users\pat\Music")),
        };
        assert_eq!(good.clone().handed_over(), good);
        let bad = UserDirs {
            data: Some(PathBuf::from(r"C:\Users\pat\Documents")),
            bridge: Some(PathBuf::from(r"C:\Users\pat\..\..\onthespot")),
            updates: Some(PathBuf::from(r"C:\Users\pat\updates")),
            music: Some(PathBuf::from("relative")),
        };
        let checked = bad.handed_over();
        assert_eq!(checked, UserDirs { data: None, bridge: None, updates: None, music: None });
    }

    // R7: a wipe that falls short says so, and its report never reaches the file list.
    #[test]
    fn failed_wipe_is_reported_and_keeps_paths_clean() {
        use std::os::windows::fs::OpenOptionsExt;
        let sb = Sandbox::new("wipe-fail");
        let dir = sb.root.join("app");
        sb.install_to(&sb.setup("1.0.0"), &dir, false, false).unwrap();
        let user = UserDirs::current();
        let data = user.data.clone().unwrap();
        fs::create_dir_all(&data).unwrap();
        let locked = data.join(".lightify_cache");
        fs::write(&locked, "sign-in").unwrap();
        let _hold = fs::OpenOptions::new().read(true).share_mode(0).open(&locked).unwrap();
        // An app file in use is the other kind of leftover.
        let in_use = dir.join(FFMPEG_EXE);
        let _hold2 = fs::OpenOptions::new().read(true).share_mode(0).open(&in_use).unwrap();

        let done = uninstall(&sb.removal(&dir, false, true), &mut |_| {}).unwrap();
        assert_eq!(done.not_wiped.len(), 1, "{:?}", done.not_wiped);
        assert!(done.not_wiped[0].contains(&data.display().to_string()));
        assert!(not_wiped_text(&done.not_wiped).contains(&done.not_wiped[0]));
        assert_eq!(done.leftovers, vec![in_use], "only real paths go to the delete list");
        assert!(find_own().is_none(), "the rest of the uninstall still happened");
    }

    // R8: a folder that isn't Lightify's alone is refused, and uninstall only takes its own.
    #[test]
    fn shared_folders_are_refused() {
        let sb = Sandbox::new("dedicated");
        let tools = sb.root.join("Tools");
        assert!(validate_target(&tools).is_ok(), "missing is fine");
        fs::create_dir_all(&tools).unwrap();
        assert!(validate_target(&tools).is_ok(), "empty is fine");
        fs::write(tools.join("ffmpeg.exe"), "someone else's").unwrap();
        assert!(validate_target(&tools).is_err(), "one of our names, but not an install of ours");
        fs::write(tools.join("other.exe"), "x").unwrap();
        assert!(validate_target(tools.parent().unwrap()).is_err());

        // A file where the folder should be.
        let file = sb.root.join("a-file");
        fs::write(&file, "x").unwrap();
        assert!(validate_target(&file).is_err());

        // What an install leaves is ours, with or without an entry.
        let leftover = sb.root.join("leftover");
        fs::create_dir_all(&leftover).unwrap();
        for n in [APP_EXE, BRIDGE_EXE, UNINSTALL_EXE, "ffmpeg.exe.lfy-old"] {
            fs::write(leftover.join(n), "x").unwrap();
        }
        assert!(validate_target(&leftover).is_ok());

        // A registered install keeps its folder even with other things in it.
        let dir = sb.root.join("app");
        sb.install_to(&sb.setup("1.0.0"), &dir, false, false).unwrap();
        fs::write(dir.join("my-notes.txt"), "mine").unwrap();
        fs::write(dir.join("mine.log"), "mine").unwrap();
        fs::write(dir.join("onthespot-web.log"), "ours").unwrap();
        assert!(validate_target(&dir).is_ok());

        uninstall(&sb.removal(&dir, false, false), &mut |_| {}).unwrap();
        assert_eq!(read(&dir.join("my-notes.txt")), "mine");
        assert_eq!(read(&dir.join("mine.log")), "mine", "a log that isn't ours stays");
        assert!(!dir.join("onthespot-web.log").exists(), "the one on the list goes");
    }
}
