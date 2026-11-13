//! One Lightify per user session.
//!
//! A second launch used to start a second process — and with it a second "Lightify"
//! Spotify Connect device on the account. Now the first instance owns a named mutex;
//! a later launch finds it taken, signals the first instance's named event (which
//! restores and focuses the window, even from the tray), and exits — unless that
//! instance's window has already closed and it is shutting down, in which case the
//! later launch waits for it to exit and then runs instead (`claim`).
//!
//! Only the interactive app takes part: `--shot*`, `--probe*` and `--selftest*` all
//! return from `main` before this runs, so they keep working next to a running app.
//! Setting `LIGHTIFY_DATA_DIR` gives an instance of its own (the names are keyed on
//! it), so a test profile can still run beside the real one.

use slint::ComponentHandle;

use crate::MainWindow;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, ResetEvent, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
    SYNCHRONIZATION_SYNCHRONIZE as SYNCHRONIZE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

/// Held for the whole run by the first instance. The OS releases the mutex when
/// the process exits, however it exits, so a crash never locks out the next launch.
pub struct Guard {
    _mutex: HANDLE,
    /// Manual-reset, set once the window is gone and shutdown has begun (`closing`).
    closing: HANDLE,
}

impl Guard {
    /// The event loop has ended: from now on a later launch can't be brought forward
    /// here, so it waits for this process to exit and then runs itself.
    pub fn closing(&self) {
        if !self.closing.is_null() {
            unsafe { SetEvent(self.closing) };
        }
    }
}

/// How long a launch waits for an instance that is shutting down, and how often it
/// looks. Shutdown stops the engine and the downloader bridge — a few seconds at most.
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(100);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Object-name suffix: empty for the normal profile, a hash of the data dir otherwise.
fn scope() -> String {
    match std::env::var_os("LIGHTIFY_DATA_DIR") {
        Some(d) if !d.is_empty() => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            d.to_string_lossy().to_lowercase().hash(&mut h);
            format!(".{:016x}", h.finish())
        }
        _ => String::new(),
    }
}

/// Claim the single-instance slot. `Some` = we are the first instance (keep the
/// guard alive); `None` = another instance was already running and has been asked
/// to come to the front, so the caller should exit.
///
/// If the mutex can't be created at all, this errs on the side of running.
///
/// An instance that is already shutting down (`Guard::closing`) can't come forward, so
/// signalling it would make this launch simply vanish: it is waited out instead (up
/// to `SHUTDOWN_WAIT`), and this launch then becomes the first instance.
pub fn claim() -> Option<Guard> {
    let scope = scope();
    let mutex_name = wide(&format!("Local\\Lightify.SingleInstance{scope}"));
    let event_name = wide(&format!("Local\\Lightify.Activate{scope}"));
    let closing_name = wide(&format!("Local\\Lightify.Closing{scope}"));
    unsafe {
        let mut mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
        if mutex.is_null() {
            return Some(Guard { _mutex: mutex, closing: std::ptr::null_mut() });
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let closing = OpenEventW(SYNCHRONIZE, 0, closing_name.as_ptr());
            let shutting_down = !closing.is_null() && WaitForSingleObject(closing, 0) == WAIT_OBJECT_0;
            if !closing.is_null() {
                // Our own handle would keep the old instance's event alive (and set).
                CloseHandle(closing);
            }
            if shutting_down {
                let start = std::time::Instant::now();
                loop {
                    // Our handle keeps the mutex object alive too: let go, then retry.
                    CloseHandle(mutex);
                    if start.elapsed() > SHUTDOWN_WAIT {
                        return None;
                    }
                    std::thread::sleep(SHUTDOWN_POLL);
                    mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
                    if mutex.is_null() {
                        return None;
                    }
                    if GetLastError() != ERROR_ALREADY_EXISTS {
                        return Some(Guard { _mutex: mutex, closing: closing_event(&closing_name) });
                    }
                }
            }
            let ev = OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr());
            if !ev.is_null() {
                // This process was just launched by the user, so it holds the
                // foreground right; hand it on so the running window can take focus
                // instead of only flashing in the taskbar.
                AllowSetForegroundWindow(ASFW_ANY);
                SetEvent(ev);
                CloseHandle(ev);
            }
            CloseHandle(mutex);
            return None;
        }
        Some(Guard { _mutex: mutex, closing: closing_event(&closing_name) })
    }
}

/// The first instance's "shutting down" flag, created clear.
fn closing_event(name: &[u16]) -> HANDLE {
    unsafe {
        // Manual-reset: once set it stays set for every later launch that looks.
        let ev = CreateEventW(std::ptr::null(), 1, 0, name.as_ptr());
        if !ev.is_null() {
            // Only this process can hold it (we own the mutex), but be sure it starts clear.
            ResetEvent(ev);
        }
        ev
    }
}

/// First instance: wait for later launches' signals and bring the window forward.
/// One parked thread blocked in `WaitForSingleObject` — no polling, no wake-ups.
pub fn listen(weak: slint::Weak<MainWindow>) {
    let event_name = wide(&format!("Local\\Lightify.Activate{}", scope()));
    // Auto-reset: each SetEvent wakes the wait exactly once.
    let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
    if ev.is_null() {
        return;
    }
    let ev = ev as usize; // HANDLE is a raw pointer; carry it across threads as an integer
    let _ = std::thread::Builder::new()
        .name("single-instance".into())
        .stack_size(64 * 1024)
        .spawn(move || loop {
            if unsafe { WaitForSingleObject(ev as HANDLE, INFINITE) } != 0 {
                return;
            }
            let _ = weak.upgrade_in_event_loop(|app| {
                crate::tray::restore(&app);
                app.window().request_redraw();
            });
        });
}
