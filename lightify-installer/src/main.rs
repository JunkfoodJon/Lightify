//! Lightify-Setup.exe - Lightify's own installer, updater and uninstaller.
//!
//! One themed Slint window (no Windows wizard chrome), three modes:
//!
//!   (no args)                              setup wizard: Welcome → Installing → Done
//!   --update --wait-pid <pid> --relaunch   small "Updating Lightify" window, no prompts
//!   --uninstall                            "Remove Lightify?" (uninstall.exe runs this)
//!
//! Options: --silent (no UI; with --all-users / --dir <path> / --desktop / --launch for
//! setup, --wipe for uninstall). Internal: --elevated, --from-temp, --parent-pid.
//! Self-check: --shot <page> <out.png> [scale] renders a page to PNG headlessly.
//!
//! The payload (app files) is appended to this exe by lightify-pack; see payload.rs.
//! A diagnostic log goes to %TEMP%\Lightify-Setup.log.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod ops;
mod win;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use lightify_installer::payload::{self, PayloadReader};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

const EXIT_OK: i32 = 0;
const EXIT_ERR: i32 = 1;
const EXIT_CANCEL: i32 = 2;
/// Elevated setup finished and the user asked to launch: the non-elevated parent
/// starts Lightify, so it never runs as administrator.
const EXIT_LAUNCH: i32 = 10;

// ── Args ────────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct Args {
    update: bool,
    wait_pid: Option<u32>,
    relaunch: bool,
    uninstall: bool,
    from_temp: bool,
    parent_pid: Option<u32>,
    dir: Option<String>,
    silent: bool,
    all_users: bool,
    desktop: bool,
    launch: bool,
    wipe: bool,
    elevated: bool,
    shot: Option<(String, String, f32)>,
}

fn parse_args() -> Args {
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut a = Args::default();
    let mut i = 0;
    while i < v.len() {
        let next = |i: usize| v.get(i + 1).cloned();
        match v[i].to_ascii_lowercase().as_str() {
            "--update" => a.update = true,
            "--wait-pid" => {
                a.wait_pid = next(i).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--relaunch" => a.relaunch = true,
            "--uninstall" => a.uninstall = true,
            "--from-temp" => a.from_temp = true,
            "--parent-pid" => {
                a.parent_pid = next(i).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--dir" => {
                a.dir = next(i);
                i += 1;
            }
            "--silent" | "/silent" | "/verysilent" => a.silent = true,
            "--all-users" => a.all_users = true,
            "--desktop" => a.desktop = true,
            "--launch" => a.launch = true,
            "--wipe" => a.wipe = true,
            "--elevated" => a.elevated = true,
            "--shot" => {
                let page = next(i).unwrap_or_else(|| "welcome".into());
                let out = v.get(i + 2).cloned().unwrap_or_else(|| "setup-shot.png".into());
                let scale = v.get(i + 3).and_then(|s| s.parse().ok()).unwrap_or(1.0);
                a.shot = Some((page, out, scale));
                i += 3;
            }
            _ => {}
        }
        i += 1;
    }
    a
}

// ── Log ─────────────────────────────────────────────────────────────────────

fn log(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("Lightify-Setup.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{secs}] pid {} {msg}", std::process::id());
    }
}

fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("Lightify-Setup.exe"))
}

/// The app version this stub was built next to (shown before a payload is read).
const STUB_VERSION: &str = env!("LIGHTIFY_APP_VERSION");

// ── Licence text → lines ────────────────────────────────────────────────────

fn strip_inline(s: &str) -> String {
    let mut out = s.replace("**", "").replace("__", "").replace('`', "");
    // [text](url) → text (url)
    while let Some(open) = out.find('[') {
        let Some(mid) = out[open..].find("](").map(|m| open + m) else { break };
        let Some(close) = out[mid..].find(')').map(|c| mid + c) else { break };
        let text = out[open + 1..mid].to_string();
        let url = out[mid + 2..close].to_string();
        let rep = if url.starts_with("http") && url != text { format!("{text} ({url})") } else { text };
        out.replace_range(open..=close, &rep);
    }
    out
}

fn licence_lines(md: &str) -> Vec<LicLine> {
    let mut out = Vec::new();
    let mut para = String::new();
    let flush = |para: &mut String, out: &mut Vec<LicLine>| {
        if !para.trim().is_empty() {
            out.push(LicLine { kind: 0, text: strip_inline(para.trim()).into() });
        }
        para.clear();
    };
    for raw in md.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("---") || t.starts_with("***") {
            flush(&mut para, &mut out);
        } else if let Some(h) = t.strip_prefix("# ") {
            flush(&mut para, &mut out);
            out.push(LicLine { kind: 1, text: strip_inline(h).into() });
        } else if t.starts_with("##") {
            flush(&mut para, &mut out);
            out.push(LicLine { kind: 2, text: strip_inline(t.trim_start_matches('#').trim()).into() });
        } else if let Some(b) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ ")) {
            flush(&mut para, &mut out);
            out.push(LicLine { kind: 3, text: strip_inline(b).into() });
        } else {
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(t.trim_start_matches('>').trim());
        }
    }
    flush(&mut para, &mut out);
    out
}

struct PayloadInfo {
    header: payload::Header,
    licence: String,
}

fn read_payload_info(exe: &Path) -> Result<Option<PayloadInfo>, String> {
    let Some(mut r) = PayloadReader::open(exe).map_err(|e| format!("this Setup file is damaged ({e}) - download it again"))? else {
        return Ok(None);
    };
    let header = r.header.clone();
    let mut licence = String::new();
    if r.peek().is_some_and(|f| f.name.eq_ignore_ascii_case(payload::LICENSE_NAME)) {
        if let Ok(Some((_, bytes))) = r.next_to_vec() {
            licence = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    Ok(Some(PayloadInfo { header, licence }))
}

// ── Window plumbing ─────────────────────────────────────────────────────────

fn hwnd_of(ui: &InstallerWindow) -> Option<isize> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    ui.window()
        .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        })
        .flatten()
}

fn wire_chrome(ui: &InstallerWindow) {
    let weak = ui.as_weak();
    ui.on_start_drag(move || {
        use slint::winit_030::WinitWindowAccessor;
        if let Some(ui) = weak.upgrade() {
            ui.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });
    // Alt+F4 / taskbar "Close window" / WM_CLOSE: same as the title-strip X (which
    // ignores it mid-install). Slint's default would hide the window and leave the
    // event loop running invisibly (run_event_loop_until_quit).
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_close_window();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let weak = ui.as_weak();
    ui.on_minimize(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });
}

/// Centre on the primary work area, show, then round the corners (the native
/// window only exists once the event loop has created it).
fn show(ui: &InstallerWindow) {
    let (w, h) = if ui.get_mode() == "update" { (420.0, 172.0) } else { (600.0, 440.0) };
    if let Some((x, y)) = win::centered_position(w, h) {
        ui.window().set_position(slint::PhysicalPosition::new(x, y));
    }
    let _ = ui.show();
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(1), move || {
        if let Some(ui) = weak.upgrade() {
            if let Some(h) = hwnd_of(&ui) {
                win::style_window(h);
            }
        }
    });
}

fn quit() {
    let _ = slint::quit_event_loop();
}

fn steps_model(labels: &[String]) -> ModelRc<Step> {
    ModelRc::new(VecModel::from(labels.iter().map(|l| Step { label: l.into(), state: 0 }).collect::<Vec<_>>()))
}

fn apply_ev(ui: &InstallerWindow, ev: ops::Ev) {
    match ev {
        ops::Ev::Step(i, s) => {
            let m = ui.get_steps();
            if let Some(mut row) = m.row_data(i) {
                row.state = s;
                m.set_row_data(i, row);
            }
        }
        ops::Ev::Current(s) => ui.set_current(s.into()),
        ops::Ev::Progress(p) => ui.set_progress(p),
    }
}

/// An event sink for a worker thread that forwards to the window.
fn forwarder(weak: slint::Weak<InstallerWindow>) -> impl FnMut(ops::Ev) {
    move |ev| {
        let _ = weak.upgrade_in_event_loop(move |ui| apply_ev(&ui, ev));
    }
}

fn show_error(ui: &InstallerWindow, title: &str, text: &str, can_back: bool) {
    log(&format!("error: {title}: {text}"));
    ui.set_error_title(title.into());
    ui.set_error_text(text.into());
    ui.set_error_can_back(can_back);
    ui.set_page("error".into());
}

// ── Setup (install) ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct Choice {
    dir: PathBuf,
    all_users: bool,
    desktop: bool,
}

fn plan_for(c: &Choice) -> ops::Plan {
    ops::Plan { dir: c.dir.clone(), all_users: c.all_users, desktop: c.desktop, update: false, migrate: ops::find_inno() }
}

fn run_install(args: Args) -> i32 {
    let exe = current_exe();
    let info = match read_payload_info(&exe) {
        Ok(Some(i)) => i,
        // A bare stub run without arguments is uninstall.exe opened directly.
        Ok(None) => return run_uninstall(Args { uninstall: true, ..args }),
        Err(e) => {
            log(&e);
            return if args.silent { EXIT_ERR } else { error_window("install", STUB_VERSION, "Setup can't start", &e) };
        }
    };
    let version = info.header.version.clone();
    let existing = ops::find_existing();
    log(&format!("setup {version}; existing: {:?}", existing.as_ref().map(|e| (&e.dir, e.all_users, &e.version, e.inno_uninstaller.is_some()))));

    let all_users = existing.as_ref().map(|e| e.all_users).unwrap_or(args.all_users);
    let dir_text = args
        .dir
        .clone()
        .or_else(|| existing.as_ref().map(|e| e.dir.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ops::default_dir_display(all_users));

    if args.silent {
        return silent_install(&args, &exe, Choice { dir: ops::resolve_dir(&dir_text), all_users, desktop: args.desktop });
    }

    let ui = InstallerWindow::new().expect("window");
    wire_chrome(&ui);
    ui.set_mode("install".into());
    ui.set_version(version.as_str().into());
    ui.set_for_all(all_users);
    ui.set_scope_locked(existing.is_some() || args.elevated);
    ui.set_install_path(dir_text.as_str().into());
    ui.set_desktop(args.desktop);
    ui.set_licence(ModelRc::new(VecModel::from(licence_lines(&info.licence))));
    ui.set_steps(steps_model(&ops::step_labels(&info.header)));

    let exit = Rc::new(RefCell::new(EXIT_CANCEL));
    let pending: Rc<RefCell<Option<Choice>>> = Rc::new(RefCell::new(None));
    let labels = ops::step_labels(&info.header);

    // Start copying (page → installing).
    let start_install = {
        let weak = ui.as_weak();
        let exe = exe.clone();
        let labels = labels.clone();
        Rc::new(move |c: Choice| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_steps(steps_model(&labels));
            ui.set_progress(0.0);
            ui.set_current("preparing".into());
            ui.set_page("installing".into());
            let weak = ui.as_weak();
            let exe = exe.clone();
            std::thread::spawn(move || {
                log(&format!("install to {} (all users: {})", c.dir.display(), c.all_users));
                let mut fwd = forwarder(weak.clone());
                let res = ops::install(&plan_for(&c), &exe, &mut fwd);
                let _ = weak.upgrade_in_event_loop(move |ui| match res {
                    Ok(()) => {
                        log("install ok");
                        ui.set_page("done".into());
                    }
                    Err(e) => show_error(&ui, "Setup couldn't finish", &e, false),
                });
            });
        })
    };

    // Before copying: make sure Lightify isn't running from there.
    let begin = {
        let weak = ui.as_weak();
        let pending = pending.clone();
        let start_install = start_install.clone();
        Rc::new(move |c: Choice| {
            let Some(ui) = weak.upgrade() else { return };
            if !ops::running_app(&c.dir).is_empty() {
                ui.set_closeapp_note(
                    "Lightify is open. Setup can close it for you \u{2014} your settings are kept, and you can reopen it when setup finishes."
                        .into(),
                );
                ui.set_page("closeapp".into());
                *pending.borrow_mut() = Some(c);
                return;
            }
            start_install(c);
        })
    };

    {
        let weak = ui.as_weak();
        ui.on_scope_changed(move |all| {
            let Some(ui) = weak.upgrade() else { return };
            // Follow the scope only while the path is still a default one.
            let cur = ui.get_install_path().to_string();
            if cur == ops::default_dir_display(!all) || cur.is_empty() {
                ui.set_install_path(ops::default_dir_display(all).into());
            }
            ui.set_welcome_note("".into());
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_change_path(move || {
            let Some(ui) = weak.upgrade() else { return };
            let start = ops::resolve_dir(&ui.get_install_path());
            let owner = hwnd_of(&ui);
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                if let Some(mut p) = win::pick_folder(owner, &start) {
                    if !p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("Lightify")) {
                        p.push("Lightify");
                    }
                    let s: SharedString = p.to_string_lossy().as_ref().into();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        ui.set_install_path(s);
                        ui.set_welcome_note("".into());
                    });
                }
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_show_licence(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_page("licence".into());
            }
        });
        let weak = ui.as_weak();
        ui.on_hide_licence(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_page("welcome".into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let exe = exe.clone();
        let begin = begin.clone();
        let exit = exit.clone();
        ui.on_install(move || {
            let Some(ui) = weak.upgrade() else { return };
            let dir = ops::resolve_dir(&ui.get_install_path());
            if let Err(m) = ops::validate_dir(&dir) {
                ui.set_welcome_note(m.into());
                return;
            }
            ui.set_welcome_note("".into());
            let c = Choice { dir, all_users: ui.get_for_all(), desktop: ui.get_desktop() };
            if !ops::needs_admin(c.all_users) {
                if !ops::dir_writable(&c.dir) {
                    ui.set_welcome_note(
                        "Setup can't write to that folder. Pick another one, or install for everyone on this PC.".into(),
                    );
                    return;
                }
                begin(c);
                return;
            }
            // "Everyone on this PC": hand over to an elevated copy, which shows the
            // rest of the wizard. This (unelevated) copy launches Lightify after.
            let mut a = vec!["--elevated".to_string(), "--all-users".into(), "--dir".into(), c.dir.to_string_lossy().into_owned()];
            if c.desktop {
                a.push("--desktop".into());
            }
            let _ = ui.hide();
            let weak = ui.as_weak();
            let exe = exe.clone();
            let exit = exit.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(win::run_elevated(&exe, &a, true));
            });
            // Poll the hand-off from the UI thread (the result decides what to show).
            let timer = Rc::new(slint::Timer::default());
            let t2 = timer.clone();
            let dir = c.dir.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
                let Ok(res) = rx.try_recv() else { return };
                t2.stop();
                let Some(ui) = weak.upgrade() else { return };
                match res {
                    Ok(win::Elevated::Exited(code)) => {
                        log(&format!("elevated setup exited {code}"));
                        if code == EXIT_LAUNCH as u32 {
                            let _ = ops::launch_app(&dir);
                            *exit.borrow_mut() = EXIT_OK;
                        } else {
                            *exit.borrow_mut() = code as i32;
                        }
                        quit();
                    }
                    Ok(win::Elevated::Started) => {
                        *exit.borrow_mut() = EXIT_OK;
                        quit();
                    }
                    Ok(win::Elevated::Declined) => {
                        show(&ui);
                        ui.set_welcome_note("Installing for everyone needs administrator approval.".into());
                    }
                    Err(e) => {
                        show(&ui);
                        ui.set_welcome_note(e.into());
                    }
                }
            });
            std::mem::forget(timer);
        });
    }
    {
        let weak = ui.as_weak();
        let pending = pending.clone();
        let start_install = start_install.clone();
        ui.on_close_app(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(c) = pending.borrow().clone() else { return };
            ui.set_closing(true);
            let weak = ui.as_weak();
            let (tx, rx) = std::sync::mpsc::channel();
            let dir = c.dir.clone();
            std::thread::spawn(move || {
                let _ = tx.send(ops::close_app(&dir, 15_000));
            });
            let start_install = start_install.clone();
            let timer = Rc::new(slint::Timer::default());
            let t2 = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
                let Ok(closed) = rx.try_recv() else { return };
                t2.stop();
                let Some(ui) = weak.upgrade() else { return };
                ui.set_closing(false);
                if closed {
                    start_install(c.clone());
                } else {
                    ui.set_closeapp_note(
                        "Lightify is still open. Quit it from its tray icon (right-click \u{2192} Quit), then press Close Lightify again."
                            .into(),
                    );
                }
            });
            std::mem::forget(timer);
        });
        let weak = ui.as_weak();
        ui.on_cancel_close_app(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_page("welcome".into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let exit1 = exit.clone();
        let elevated = args.elevated;
        ui.on_finish(move || {
            let exit = &exit1;
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_page() == "done" {
                *exit.borrow_mut() = EXIT_OK;
                if ui.get_launch() {
                    if elevated {
                        *exit.borrow_mut() = EXIT_LAUNCH;
                    } else if let Err(e) = ops::launch_app(&ops::resolve_dir(&ui.get_install_path())) {
                        log(&e);
                    }
                }
            } else if ui.get_page() == "error" {
                *exit.borrow_mut() = EXIT_ERR;
            }
            quit();
        });
        let weak = ui.as_weak();
        let exit2 = exit.clone();
        ui.on_close_window(move || {
            let Some(ui) = weak.upgrade() else { return };
            let page = ui.get_page();
            if page == "installing" || ui.get_closing() {
                return;
            }
            *exit2.borrow_mut() = match page.as_str() {
                "done" => EXIT_OK,
                "error" => EXIT_ERR,
                _ => EXIT_CANCEL,
            };
            quit();
        });
        let weak = ui.as_weak();
        ui.on_error_back(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_page("welcome".into());
            }
        });
    }

    if args.elevated {
        // Handed over from the unelevated wizard: the choices are made, go.
        ui.set_accepted(true);
        let c = Choice { dir: ops::resolve_dir(&dir_text), all_users: true, desktop: args.desktop };
        begin(c);
    }
    show(&ui);
    let _ = slint::run_event_loop_until_quit();
    let _ = ui.hide();
    let code = *exit.borrow();
    code
}

fn silent_install(args: &Args, exe: &Path, c: Choice) -> i32 {
    if let Err(e) = ops::validate_dir(&c.dir) {
        log(&e);
        return EXIT_ERR;
    }
    if ops::needs_admin(c.all_users) {
        let mut a = vec!["--silent".to_string(), "--elevated".into(), "--all-users".into(), "--dir".into(), c.dir.to_string_lossy().into_owned()];
        if c.desktop {
            a.push("--desktop".into());
        }
        return match win::run_elevated(exe, &a, true) {
            Ok(win::Elevated::Exited(0)) => {
                if args.launch {
                    let _ = ops::launch_app(&c.dir);
                }
                EXIT_OK
            }
            Ok(win::Elevated::Exited(code)) => code as i32,
            Ok(win::Elevated::Started) => EXIT_OK,
            Ok(win::Elevated::Declined) => EXIT_CANCEL,
            Err(e) => {
                log(&e);
                EXIT_ERR
            }
        };
    }
    if !ops::running_app(&c.dir).is_empty() && !ops::close_app(&c.dir, 15_000) {
        log("Lightify is running and did not close");
        return EXIT_ERR;
    }
    match ops::install(&plan_for(&c), exe, &mut |_| {}) {
        Ok(()) => {
            log(&format!("silent install ok: {}", c.dir.display()));
            if args.launch && !args.elevated {
                let _ = ops::launch_app(&c.dir);
            }
            EXIT_OK
        }
        Err(e) => {
            log(&format!("silent install failed: {e}"));
            EXIT_ERR
        }
    }
}

/// A standalone themed error (Setup can't even start).
fn error_window(mode: &str, version: &str, title: &str, text: &str) -> i32 {
    let ui = InstallerWindow::new().expect("window");
    wire_chrome(&ui);
    ui.set_mode(if mode == "update" { "install".into() } else { mode.into() });
    ui.set_version(version.into());
    show_error(&ui, title, text, false);
    ui.on_finish(quit);
    ui.on_close_window(quit);
    show(&ui);
    let _ = slint::run_event_loop_until_quit();
    EXIT_ERR
}

// ── Update ──────────────────────────────────────────────────────────────────

fn run_update(args: Args) -> i32 {
    let exe = current_exe();
    let info = match read_payload_info(&exe) {
        Ok(Some(i)) => i,
        Ok(None) => {
            log("update: no payload");
            return EXIT_ERR;
        }
        Err(e) => {
            log(&format!("update: {e}"));
            return EXIT_ERR;
        }
    };
    let new_version = info.header.version.clone();
    let own = ops::find_own();
    let inno = if own.is_none() { ops::find_inno() } else { None };
    let Some(existing) = own.clone().or_else(|| inno.clone()) else {
        log("update: no install found");
        return if args.silent {
            EXIT_ERR
        } else {
            error_window("install", &new_version, "Lightify isn't installed", "There is no Lightify install on this PC to update. Download Setup from lightify.stream.")
        };
    };
    log(&format!("update {} -> {new_version} in {}", existing.version, existing.dir.display()));

    let blocked = !args.elevated
        && ops::sandbox().is_none()
        && !win::is_elevated()
        && (existing.all_users || !ops::dir_writable(&existing.dir));
    if blocked {
        let mut a = vec!["--update".to_string(), "--elevated".into()];
        if let Some(p) = args.wait_pid {
            a.push("--wait-pid".into());
            a.push(p.to_string());
        }
        if args.silent {
            a.push("--silent".into());
        }
        let code = match win::run_elevated(&exe, &a, true) {
            Ok(win::Elevated::Exited(c)) => c as i32,
            Ok(win::Elevated::Started) => EXIT_OK,
            Ok(win::Elevated::Declined) => EXIT_CANCEL,
            Err(e) => {
                log(&e);
                EXIT_ERR
            }
        };
        if args.relaunch {
            if let Some(pid) = args.wait_pid {
                win::wait_pid(pid, 30_000);
            }
            let _ = ops::launch_app(&existing.dir);
        }
        return code;
    }

    // Migrating from Inno: its uninstaller takes the shortcuts with it, so recreate
    // them (desktop too, if it had one).
    let migrating = own.is_none();
    let had_desktop = migrating && ops::desktop_dir(existing.all_users).is_some_and(|d| d.join("Lightify.lnk").is_file());
    let plan = ops::Plan {
        dir: existing.dir.clone(),
        all_users: existing.all_users,
        desktop: had_desktop,
        update: !migrating,
        migrate: inno,
    };
    let relaunch = args.relaunch && !args.elevated;
    let dir = existing.dir.clone();
    let wait_pid = args.wait_pid;

    let work = move |ev: &mut dyn FnMut(ops::Ev)| -> Result<(), String> {
        if let Some(pid) = wait_pid {
            if !win::wait_pid(pid, 30_000) {
                log("update: the app did not exit within 30 s; continuing");
            }
        }
        if !ops::running_app(&dir).is_empty() && !ops::close_app(&dir, 15_000) {
            log("update: another Lightify is still running; replacing around it");
        }
        ops::wait_helpers(&dir, 5_000);
        ops::install(&plan, &current_exe(), ev)
    };

    if args.silent {
        let res = work(&mut |_| {});
        if let Err(e) = &res {
            log(&format!("update failed: {e}"));
        }
        if relaunch {
            let _ = ops::launch_app(&existing.dir);
        }
        return if res.is_ok() { EXIT_OK } else { EXIT_ERR };
    }

    let ui = InstallerWindow::new().expect("window");
    wire_chrome(&ui);
    ui.set_mode("update".into());
    ui.set_page("update".into());
    ui.set_version(new_version.as_str().into());
    ui.set_update_from(if existing.version.is_empty() { "installed".into() } else { existing.version.as_str().into() });
    ui.set_update_to(new_version.as_str().into());
    ui.set_steps(steps_model(&ops::step_labels(&info.header)));
    ui.on_close_window(|| {});
    ui.on_finish(|| {});
    let exit = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(EXIT_OK));
    {
        let weak = ui.as_weak();
        let exit = exit.clone();
        let app_dir = existing.dir.clone();
        std::thread::spawn(move || {
            let mut fwd = forwarder(weak.clone());
            let res = work(&mut fwd);
            match &res {
                Ok(()) => {
                    log("update ok");
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => {
                    exit.store(EXIT_ERR, std::sync::atomic::Ordering::SeqCst);
                    let msg = e.clone();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        log(&format!("update failed: {msg}"));
                        ui.set_error_text(msg.into());
                        ui.set_page("error".into());
                    });
                    std::thread::sleep(Duration::from_secs(6));
                }
            }
            if relaunch {
                if let Err(e) = ops::launch_app(&app_dir) {
                    log(&e);
                }
            }
            let _ = slint::invoke_from_event_loop(quit);
        });
    }
    show(&ui);
    let _ = slint::run_event_loop_until_quit();
    let _ = ui.hide();
    exit.load(std::sync::atomic::Ordering::SeqCst)
}

// ── Uninstall ───────────────────────────────────────────────────────────────

fn run_uninstall(args: Args) -> i32 {
    let exe = current_exe();
    if !args.from_temp {
        // Stage 1 (uninstall.exe in the install dir): copy ourselves to %TEMP% and run
        // from there, so the install dir - this exe included - can be deleted.
        let dir = args
            .dir
            .as_deref()
            .map(ops::resolve_dir)
            .or_else(|| exe.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let own = ops::find_own().filter(|e| win::same_path(&e.dir, &dir));
        let all_users = own.as_ref().map(|e| e.all_users).unwrap_or(args.all_users);
        let temp = std::env::temp_dir().join(format!("Lightify-uninstall-{}.exe", std::process::id()));
        if let Err(e) = payload::write_stub(&exe, &temp) {
            log(&format!("uninstall: copy to temp failed: {e}"));
            return EXIT_ERR;
        }
        let mut a = vec![
            "--uninstall".to_string(),
            "--from-temp".into(),
            "--dir".into(),
            dir.to_string_lossy().into_owned(),
            "--parent-pid".into(),
            std::process::id().to_string(),
        ];
        if all_users {
            a.push("--all-users".into());
        }
        if args.silent {
            a.push("--silent".into());
        }
        if args.wipe {
            a.push("--wipe".into());
        }
        log(&format!("uninstall stage 1: {} (all users: {all_users})", dir.display()));
        if ops::needs_admin(all_users) {
            return match win::run_elevated(&temp, &a, args.silent) {
                Ok(win::Elevated::Declined) => {
                    let _ = std::fs::remove_file(&temp);
                    EXIT_CANCEL
                }
                Ok(win::Elevated::Exited(c)) => c as i32,
                Ok(_) => EXIT_OK,
                Err(e) => {
                    log(&e);
                    let _ = std::fs::remove_file(&temp);
                    EXIT_ERR
                }
            };
        }
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        if args.silent {
            // Unattended callers wait for the result.
            return std::process::Command::new(&temp)
                .args(&refs)
                .status()
                .map(|s| s.code().unwrap_or(EXIT_ERR))
                .unwrap_or(EXIT_ERR);
        }
        return match win::spawn_detached(&temp, &refs, Some(&std::env::temp_dir())) {
            Ok(()) => EXIT_OK,
            Err(e) => {
                log(&format!("uninstall: {e}"));
                EXIT_ERR
            }
        };
    }

    // Stage 2: running from %TEMP%. Delete this copy once we are done, and anything
    // in the install dir that was still in use (a silent caller's stage 1 waits on
    // us, so its uninstall.exe can only go after we exit).
    let left: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
    let code = uninstall_from_temp(&args, &left);
    let mut files = vec![exe];
    files.extend(left.borrow().iter().cloned());
    let dirs: Vec<PathBuf> = args.dir.as_deref().map(ops::resolve_dir).filter(|_| code == EXIT_OK).into_iter().collect();
    win::delete_after_exit(&files, &dirs);
    code
}

fn uninstall_from_temp(args: &Args, left_out: &Rc<RefCell<Vec<PathBuf>>>) -> i32 {
    // Interactive: stage 1 exits right after starting us. Silent: it waits for us.
    if let (Some(p), false) = (args.parent_pid, args.silent) {
        win::wait_pid(p, 10_000);
    }
    let Some(dir) = args.dir.as_deref().map(ops::resolve_dir) else { return EXIT_ERR };
    let own = ops::find_own().filter(|e| win::same_path(&e.dir, &dir));
    let all_users = own.as_ref().map(|e| e.all_users).unwrap_or(args.all_users);
    let version = own.as_ref().map(|e| e.version.clone()).filter(|v| !v.is_empty()).unwrap_or_else(|| STUB_VERSION.into());
    let files = own.as_ref().map(|e| e.files.clone()).unwrap_or_default();
    log(&format!("uninstall {} (all users {all_users}, version {version})", dir.display()));

    if !ops::looks_installed(&dir) || ops::validate_dir(&dir).is_err() {
        let msg = format!("There is no Lightify install in {}.", dir.display());
        log(&msg);
        return if args.silent { EXIT_ERR } else { error_window("uninstall", &version, "Nothing to remove", &msg) };
    }

    let make_removal = |wipe: bool| ops::Removal { dir: dir.clone(), all_users, wipe, files: files.clone() };

    if args.silent {
        if !ops::running_app(&dir).is_empty() && !ops::close_app(&dir, 15_000) {
            log("uninstall: Lightify did not close");
            return EXIT_ERR;
        }
        ops::wait_helpers(&dir, 5_000);
        return match ops::uninstall(&make_removal(args.wipe), &mut |_| {}) {
            Ok(left) => {
                if !left.is_empty() {
                    log(&format!("uninstall: removing after exit {left:?}"));
                }
                left_out.borrow_mut().extend(left.into_iter().map(PathBuf::from));
                EXIT_OK
            }
            Err(e) => {
                log(&format!("uninstall failed: {e}"));
                EXIT_ERR
            }
        };
    }

    let ui = InstallerWindow::new().expect("window");
    wire_chrome(&ui);
    ui.set_mode("uninstall".into());
    ui.set_page("confirm".into());
    ui.set_version(version.as_str().into());
    ui.set_wipe(args.wipe);
    let exit = Rc::new(RefCell::new(EXIT_CANCEL));

    let start_removal = {
        let weak = ui.as_weak();
        let dir = dir.clone();
        let files = files.clone();
        Rc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            let wipe = ui.get_wipe();
            ui.set_steps(steps_model(&ops::removal_steps(wipe)));
            ui.set_progress(0.0);
            ui.set_current("".into());
            ui.set_page("removing".into());
            let weak = ui.as_weak();
            let r = ops::Removal { dir: dir.clone(), all_users, wipe, files: files.clone() };
            std::thread::spawn(move || {
                ops::wait_helpers(&r.dir, 5_000);
                let mut fwd = forwarder(weak.clone());
                let res = ops::uninstall(&r, &mut fwd);
                if let Ok(left) = &res {
                    if !left.is_empty() {
                        log(&format!("uninstall: removing after exit {left:?}"));
                    }
                    if let Ok(mut g) = LEFTOVERS.lock() {
                        g.extend(left.iter().map(PathBuf::from));
                    }
                }
                let _ = weak.upgrade_in_event_loop(move |ui| match res {
                    Ok(_) => {
                        ui.set_page("removed".into());
                    }
                    Err(e) => show_error(&ui, "Lightify wasn't fully removed", &e, false),
                });
            });
        })
    };
    {
        let weak = ui.as_weak();
        let dir = dir.clone();
        let start_removal = start_removal.clone();
        ui.on_remove(move || {
            let Some(ui) = weak.upgrade() else { return };
            if !ops::running_app(&dir).is_empty() {
                ui.set_closeapp_note("Lightify is open. Setup can close it for you, then remove it.".into());
                ui.set_page("closeapp".into());
                return;
            }
            start_removal();
        });
    }
    {
        let weak = ui.as_weak();
        let dir = dir.clone();
        let start_removal = start_removal.clone();
        ui.on_close_app(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_closing(true);
            let (tx, rx) = std::sync::mpsc::channel();
            let d = dir.clone();
            std::thread::spawn(move || {
                let _ = tx.send(ops::close_app(&d, 15_000));
            });
            let weak = ui.as_weak();
            let start_removal = start_removal.clone();
            let timer = Rc::new(slint::Timer::default());
            let t2 = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
                let Ok(closed) = rx.try_recv() else { return };
                t2.stop();
                let Some(ui) = weak.upgrade() else { return };
                ui.set_closing(false);
                if closed {
                    start_removal();
                } else {
                    ui.set_closeapp_note(
                        "Lightify is still open. Quit it from its tray icon (right-click \u{2192} Quit), then press Close Lightify again."
                            .into(),
                    );
                }
            });
            std::mem::forget(timer);
        });
        let weak = ui.as_weak();
        ui.on_cancel_close_app(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_page("confirm".into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let exit1 = exit.clone();
        ui.on_finish(move || {
            if let Some(ui) = weak.upgrade() {
                *exit1.borrow_mut() = if ui.get_page() == "removed" { EXIT_OK } else { EXIT_ERR };
            }
            quit();
        });
        let weak = ui.as_weak();
        let exit = exit.clone();
        ui.on_close_window(move || {
            let Some(ui) = weak.upgrade() else { return };
            let page = ui.get_page();
            if page == "removing" || ui.get_closing() {
                return;
            }
            *exit.borrow_mut() = match page.as_str() {
                "removed" => EXIT_OK,
                "error" => EXIT_ERR,
                _ => EXIT_CANCEL,
            };
            quit();
        });
    }
    show(&ui);
    let _ = slint::run_event_loop_until_quit();
    let _ = ui.hide();
    if let Ok(g) = LEFTOVERS.lock() {
        left_out.borrow_mut().extend(g.iter().cloned());
    }
    let code = *exit.borrow();
    code
}

/// Files the interactive uninstall could not delete (removed after exit).
static LEFTOVERS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

// ── Headless render (self-check) ────────────────────────────────────────────

fn render_shot(page: &str, out: &str, scale: f32) -> i32 {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter, WindowEvent};

    struct ShotPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl Platform for ShotPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(ShotPlatform { window: window.clone() })).expect("set_platform");

    let exe = current_exe();
    let info = read_payload_info(&exe).ok().flatten();
    let version = info.as_ref().map(|i| i.header.version.clone()).unwrap_or_else(|| STUB_VERSION.into());
    let ui = InstallerWindow::new().expect("window");
    ui.set_version(version.as_str().into());
    ui.set_install_path(ops::default_dir_display(false).into());
    let lic = info
        .as_ref()
        .map(|i| i.licence.clone())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "# Licence\n\nNo payload in this build; this is placeholder text.".into());
    ui.set_licence(ModelRc::new(VecModel::from(licence_lines(&lic))));
    let step = |l: &str, s: i32| Step { label: l.into(), state: s };
    match page {
        "welcome" => ui.set_page("welcome".into()),
        "welcome-all" => {
            ui.set_page("welcome".into());
            ui.set_for_all(true);
            ui.set_install_path(r"C:\Program Files\Lightify".into());
            ui.set_desktop(true);
            ui.set_accepted(true);
        }
        "welcome-note" => {
            ui.set_page("welcome".into());
            ui.set_install_path("C:\\".into());
            ui.set_accepted(true);
            ui.set_welcome_note("Pick a folder, not the root of a drive.".into());
        }
        "licence" => ui.set_page("licence".into()),
        "closeapp" => {
            ui.set_page("closeapp".into());
            ui.set_closeapp_note(
                "Lightify is open. Setup can close it for you \u{2014} your settings are kept, and you can reopen it when setup finishes."
                    .into(),
            );
        }
        "installing" => {
            ui.set_page("installing".into());
            ui.set_progress(0.64);
            ui.set_current("copying lightify-onthespot.exe".into());
            ui.set_steps(ModelRc::new(VecModel::from(vec![
                step("Lightify.exe", 2),
                step("Downloader bridge", 1),
                step("ffmpeg", 0),
                step("Shortcuts and uninstall entry", 0),
            ])));
        }
        "done" => ui.set_page("done".into()),
        "error" => {
            ui.set_error_title("Setup couldn't finish".into());
            ui.set_error_text(r"C:\Program Files\Lightify\Lightify.exe: access denied".into());
            ui.set_error_can_back(true);
            ui.set_page("error".into());
        }
        "confirm" => {
            ui.set_mode("uninstall".into());
            ui.set_page("confirm".into());
        }
        "removing" => {
            ui.set_mode("uninstall".into());
            ui.set_page("removing".into());
            ui.set_progress(0.4);
            ui.set_current("removing ffmpeg.exe".into());
            ui.set_steps(ModelRc::new(VecModel::from(vec![
                step("App files", 1),
                step("Shortcuts and uninstall entry", 0),
                step("Settings and Spotify sign-in", 0),
            ])));
        }
        "removed" => {
            ui.set_mode("uninstall".into());
            ui.set_page("removed".into());
        }
        "update" | "update-error" => {
            ui.set_mode("update".into());
            ui.set_page(if page == "update" { "update".into() } else { "error".into() });
            ui.set_update_from("2.2.4".into());
            ui.set_update_to(version.as_str().into());
            ui.set_progress(0.82);
            ui.set_error_text(r"Lightify.exe: access denied".into());
        }
        other => {
            eprintln!("unknown page {other}");
            return EXIT_ERR;
        }
    }
    let (lw, lh) = if ui.get_mode() == "update" { (420.0, 172.0) } else { (600.0, 440.0) };
    let (w, h) = ((lw * scale).round() as u32, (lh * scale).round() as u32);
    window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
    window.set_size(slint::PhysicalSize::new(w, h));
    let _ = ui.show();
    slint::platform::update_timers_and_animations();
    // Let the 150-200 ms colour/width animations settle so the frame is final.
    std::thread::sleep(Duration::from_millis(400));
    slint::platform::update_timers_and_animations();
    let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w as usize);
    });
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in &buffer {
        rgb.extend_from_slice(&[px.red, px.green, px.blue]);
    }
    let file = match std::fs::File::create(out) {
        Ok(f) => f,
        Err(e) => {
            log(&format!("shot: {out}: {e}"));
            return EXIT_ERR;
        }
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    EXIT_OK
}

fn main() {
    let args = parse_args();
    if let Some((page, out, scale)) = args.shot.clone() {
        std::process::exit(render_shot(&page, &out, scale.clamp(0.5, 4.0)));
    }
    let code = if args.uninstall {
        run_uninstall(args)
    } else if args.update {
        run_update(args)
    } else {
        run_install(args)
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn licence_markdown_to_lines() {
        let md = "# Lightify licence\n\nSome **bold** text\nthat wraps.\n\n## Third party\n\n- ffmpeg: [LGPL](https://ffmpeg.org)\n- `librespot`\n";
        let l = licence_lines(md);
        let k: Vec<(i32, String)> = l.iter().map(|x| (x.kind, x.text.to_string())).collect();
        assert_eq!(
            k,
            vec![
                (1, "Lightify licence".into()),
                (0, "Some bold text that wraps.".into()),
                (2, "Third party".into()),
                (3, "ffmpeg: LGPL (https://ffmpeg.org)".into()),
                (3, "librespot".into()),
            ]
        );
    }
}
