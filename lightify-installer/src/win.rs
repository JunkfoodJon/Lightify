//! Thin Win32 helpers: registry, elevation, processes, shortcuts, known folders,
//! the folder picker and window styling. Everything returns plain Rust types.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, WAIT_OBJECT_0};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

pub fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hive {
    User,
    Machine,
}

impl Hive {
    fn raw(self) -> HKEY {
        match self {
            Hive::User => HKEY_CURRENT_USER,
            Hive::Machine => HKEY_LOCAL_MACHINE,
        }
    }
}

pub struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

impl Key {
    pub fn create(hive: Hive, sub: &str) -> Result<Key, String> {
        let mut h: HKEY = std::ptr::null_mut();
        let rc = unsafe {
            RegCreateKeyExW(
                hive.raw(),
                wide(sub).as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                std::ptr::null(),
                &mut h,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(format!("registry: cannot create {sub} (error {rc})"));
        }
        Ok(Key(h))
    }

    pub fn open(hive: Hive, sub: &str) -> Option<Key> {
        let mut h: HKEY = std::ptr::null_mut();
        let rc = unsafe { RegOpenKeyExW(hive.raw(), wide(sub).as_ptr(), 0, KEY_READ, &mut h) };
        (rc == 0).then_some(Key(h))
    }

    pub fn set_str(&self, name: &str, value: &str) -> Result<(), String> {
        let v = wide(value);
        let rc = unsafe {
            RegSetValueExW(self.0, wide(name).as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32)
        };
        if rc != 0 {
            return Err(format!("registry: cannot set {name} (error {rc})"));
        }
        Ok(())
    }

    pub fn set_dword(&self, name: &str, value: u32) -> Result<(), String> {
        let rc = unsafe {
            RegSetValueExW(self.0, wide(name).as_ptr(), 0, REG_DWORD, &value as *const u32 as *const u8, 4)
        };
        if rc != 0 {
            return Err(format!("registry: cannot set {name} (error {rc})"));
        }
        Ok(())
    }

    pub fn get_str(&self, name: &str) -> Option<String> {
        let wname = wide(name);
        let mut ty = 0u32;
        let mut len = 0u32;
        unsafe {
            if RegQueryValueExW(self.0, wname.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut len) != 0 {
                return None;
            }
            if ty != REG_SZ && ty != REG_EXPAND_SZ {
                return None;
            }
            let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
            let mut len2 = (buf.len() * 2) as u32;
            if RegQueryValueExW(self.0, wname.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut len2) != 0 {
                return None;
            }
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..n]))
        }
    }
}

pub fn delete_key_tree(hive: Hive, sub: &str) -> Result<(), String> {
    let w = wide(sub);
    unsafe {
        let rc = RegDeleteTreeW(hive.raw(), w.as_ptr());
        RegDeleteKeyW(hive.raw(), w.as_ptr());
        if rc != 0 && rc != 2 {
            return Err(format!("registry: cannot delete {sub} (error {rc})"));
        }
    }
    Ok(())
}

// ── Elevation ───────────────────────────────────────────────────────────────

pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut el as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        CloseHandle(token);
        ok != 0 && el.TokenIsElevated != 0
    }
}

/// Quote one argument for a Windows command line (CommandLineToArgvW rules).
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

pub enum Elevated {
    /// The elevated copy ran; its exit code.
    Exited(u32),
    /// Started, not waited for.
    Started,
    /// The user said no at the UAC prompt.
    Declined,
}

/// Run `exe args` through ShellExecute "runas" (the UAC prompt).
pub fn run_elevated(exe: &Path, args: &[String], wait: bool) -> Result<Elevated, String> {
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let params = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    let verb = wide("runas");
    let file = wide(exe);
    let wparams = wide(&params);
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = wparams.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(1223) {
                return Ok(Elevated::Declined);
            }
            return Err(lightify_i18n::tr!("could not start the elevated installer: {}", err));
        }
        if info.hProcess.is_null() {
            return Ok(Elevated::Started);
        }
        if !wait {
            CloseHandle(info.hProcess);
            return Ok(Elevated::Started);
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Ok(Elevated::Exited(code))
    }
}

// ── Processes ───────────────────────────────────────────────────────────────

/// Full image path of a running process.
pub fn process_path(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        (ok != 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len as usize])))
    }
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Is `p` inside (or equal to) `dir`? Case-insensitive, as Windows paths are.
pub fn path_within(p: &Path, dir: &Path) -> bool {
    let (p, d) = (norm(p), norm(dir));
    p == d || p.starts_with(&(d + "\\"))
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

/// Processes whose executable lives in `dir`, as `(pid, file name)`.
pub fn processes_in(dir: &Path) -> Vec<(u32, String)> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let me = std::process::id();
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            let pid = e.th32ProcessID;
            if pid != me && pid != 0 {
                if let Some(path) = process_path(pid) {
                    if path.parent().is_some_and(|p| same_path(p, dir)) {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        out.push((pid, name));
                    }
                }
            }
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    out
}

/// Wait for a process to exit. True if it has (or never existed).
pub fn wait_pid(pid: u32, timeout_ms: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return true;
        }
        let r = WaitForSingleObject(h, timeout_ms);
        CloseHandle(h);
        r == WAIT_OBJECT_0
    }
}

/// Ask a process to close by posting WM_CLOSE to its top-level windows (visible ones
/// first; if it has none visible - e.g. minimised to the tray - all of them).
pub fn close_windows_of(pid: u32) -> usize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE};
    struct Acc {
        pid: u32,
        visible: Vec<HWND>,
        hidden: Vec<HWND>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let acc = &mut *(lp as *mut Acc);
        let mut p = 0u32;
        GetWindowThreadProcessId(hwnd, &mut p);
        if p == acc.pid {
            if IsWindowVisible(hwnd) != 0 {
                acc.visible.push(hwnd);
            } else {
                acc.hidden.push(hwnd);
            }
        }
        1
    }
    let mut acc = Acc { pid, visible: Vec::new(), hidden: Vec::new() };
    unsafe {
        EnumWindows(Some(cb), &mut acc as *mut Acc as LPARAM);
        let targets = if acc.visible.is_empty() { &acc.hidden } else { &acc.visible };
        for &h in targets {
            PostMessageW(h, WM_CLOSE, 0, 0);
        }
        targets.len()
    }
}

/// Start a program detached from this one (no console, own process group).
pub fn spawn_detached(exe: &Path, args: &[&str], cwd: Option<&Path>) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    cmd.spawn().map(|_| ())
}

/// Delete `files`, then remove `dirs` if empty, a few seconds after this process
/// exits (a running exe can't delete itself, and the stage-1 uninstaller may still be
/// waiting on us). Uses a hidden cmd.exe.
pub fn delete_after_exit(files: &[PathBuf], dirs: &[PathBuf]) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let q = |p: &PathBuf| p.to_string_lossy().replace('"', "");
    let mut script = String::from("/d /c ping -n 4 127.0.0.1 >nul");
    for f in files {
        script.push_str(&format!(" & del /f /q \"{}\"", q(f)));
    }
    for d in dirs {
        script.push_str(&format!(" & rmdir \"{}\"", q(d)));
    }
    script.push_str(" 2>nul");
    let cmd = std::env::var_os("ComSpec").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    let _ = std::process::Command::new(cmd)
        .raw_arg(script)
        .creation_flags(CREATE_NO_WINDOW)
        .current_dir(std::env::temp_dir())
        .spawn();
}

// ── Known folders ───────────────────────────────────────────────────────────

fn known_folder(id: &GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    unsafe {
        let mut p: PWSTR = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p);
        let out = (hr >= 0).then(|| PathBuf::from(from_wide_ptr(p)));
        if !p.is_null() {
            CoTaskMemFree(p as *const _);
        }
        out.filter(|p| !p.as_os_str().is_empty())
    }
}

pub fn programs_dir(all_users: bool) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::{FOLDERID_CommonPrograms, FOLDERID_Programs};
    known_folder(if all_users { &FOLDERID_CommonPrograms } else { &FOLDERID_Programs })
}

pub fn desktop_dir(all_users: bool) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_PublicDesktop};
    known_folder(if all_users { &FOLDERID_PublicDesktop } else { &FOLDERID_Desktop })
}

pub fn program_files() -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles;
    known_folder(&FOLDERID_ProgramFiles)
}

// ── Shortcuts (IShellLink) ──────────────────────────────────────────────────

pub fn create_shortcut(lnk: &Path, target: &Path, args: &str, workdir: &Path, icon: &Path, desc: &str) -> Result<(), String> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    let fail = |what: &str, e: windows::core::Error| format!("shortcut {}: {what}: {e}", lnk.display());
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| fail("create", e))?;
        link.SetPath(&HSTRING::from(target.as_os_str())).map_err(|e| fail("path", e))?;
        if !args.is_empty() {
            link.SetArguments(&HSTRING::from(args)).map_err(|e| fail("arguments", e))?;
        }
        link.SetWorkingDirectory(&HSTRING::from(workdir.as_os_str())).map_err(|e| fail("workdir", e))?;
        link.SetIconLocation(&HSTRING::from(icon.as_os_str()), 0).map_err(|e| fail("icon", e))?;
        link.SetDescription(&HSTRING::from(desc)).map_err(|e| fail("description", e))?;
        let pf: IPersistFile = link.cast().map_err(|e| fail("persist", e))?;
        if let Some(dir) = lnk.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        pf.Save(&HSTRING::from(lnk.as_os_str()), true).map_err(|e| fail("save", e))?;
    }
    Ok(())
}

// ── Folder picker ───────────────────────────────────────────────────────────

/// The shell's folder browser (the one piece of OS UI left: browsing the file system).
/// Runs modally on the calling thread; call it from a worker thread with its own COM.
pub fn pick_folder(owner: Option<isize>, start: &Path) -> Option<PathBuf> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND as WHWND;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dlg.SetTitle(&HSTRING::from("Install Lightify to"));
        // Start in the nearest existing parent of the current path.
        let mut probe = Some(start);
        while let Some(p) = probe {
            if p.is_dir() {
                if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(p.as_os_str()), None) {
                    let _ = dlg.SetFolder(&item);
                }
                break;
            }
            probe = p.parent();
        }
        dlg.Show(owner.map(|h| WHWND(h as *mut _))).ok()?;
        let item = dlg.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        s.map(PathBuf::from)
    }
}

// ── Window ──────────────────────────────────────────────────────────────────

/// Windows 11: round the no-frame window's corners and colour its 1px border to the
/// theme's line colour. Harmless no-op on Windows 10.
pub fn style_window(hwnd: isize) {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE};
    const DWMWCP_ROUND: u32 = 2;
    let border: u32 = 0x0023_2323; // COLORREF 0x00BBGGRR
    unsafe {
        DwmSetWindowAttribute(hwnd as HWND, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &DWMWCP_ROUND as *const u32 as *const _, 4);
        DwmSetWindowAttribute(hwnd as HWND, DWMWA_BORDER_COLOR as u32, &border as *const u32 as *const _, 4);
    }
}

/// Top-left (physical px) that centres a `w`x`h` logical-px window on the primary
/// monitor's work area.
pub fn centered_position(w: f32, h: f32) -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut RECT as *mut _, 0) == 0 {
            return None;
        }
        let scale = GetDpiForSystem().max(96) as f32 / 96.0;
        let (pw, ph) = ((w * scale) as i32, (h * scale) as i32);
        Some((r.left + ((r.right - r.left) - pw) / 2, r.top + ((r.bottom - r.top) - ph) / 2))
    }
}

/// Expand `%VAR%` references (ExpandEnvironmentStrings semantics, done by hand).
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_argv_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("C:\\Program Files\\Lightify"), "\"C:\\Program Files\\Lightify\"");
        assert_eq!(quote_arg("C:\\Program Files\\Lightify\\"), "\"C:\\Program Files\\Lightify\\\\\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("LFY_TEST_VAR", "C:\\X");
        assert_eq!(expand_env("%LFY_TEST_VAR%\\Programs"), "C:\\X\\Programs");
        assert_eq!(expand_env("%NOPE_NOT_SET_9%\\a"), "%NOPE_NOT_SET_9%\\a");
        assert_eq!(expand_env("100%"), "100%");
    }

    #[test]
    fn path_containment() {
        assert!(path_within(Path::new("C:\\A\\b\\c.exe"), Path::new("c:\\a\\B")));
        assert!(path_within(Path::new("C:\\A\\b"), Path::new("C:\\A\\b\\")));
        assert!(!path_within(Path::new("C:\\A\\bc"), Path::new("C:\\A\\b")));
    }
}
