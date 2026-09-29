//! Main-window visibility, and the work the app skips while nobody can see it.
//!
//! * While minimized, periodic UI pushes (the 500 ms seek-bar clock) stand down —
//!   each one would wake the UI thread to lay out and render a frame nobody sees.
//! * Two seconds after minimizing, the process working set is trimmed. Windows then
//!   pages cold memory (decoded UI assets, startup-only allocations) out to the
//!   standby list; whatever is touched again on restore comes back from RAM, not disk,
//!   so restoring stays instant.
//!
//! The minimize is read straight from `WM_SIZE` (`SIZE_MINIMIZED`) through a window
//! subclass: winit 0.30 on Windows doesn't surface it as an event this app sees
//! (checked: no `Resized` reaches the Slint window-event filter on minimize).

use std::sync::atomic::{AtomicBool, Ordering};

static HIDDEN: AtomicBool = AtomicBool::new(false);

/// True while the main window is minimized. Safe to read from any thread.
pub fn hidden() -> bool {
    HIDDEN.load(Ordering::Relaxed)
}

/// Delay between minimize and the working-set trim: lets the minimize animation
/// finish, and skips the trim entirely for a quick minimize/restore.
#[cfg(windows)]
const TRIM_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(windows)]
thread_local! {
    static TRIM: slint::Timer = slint::Timer::default();
}

#[cfg(windows)]
unsafe extern "system" fn subclass_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _id: usize,
    _data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{SIZE_MINIMIZED, WM_SIZE};
    if msg == WM_SIZE {
        let minimized = wparam.0 as u32 == SIZE_MINIMIZED;
        let was = HIDDEN.swap(minimized, Ordering::Relaxed);
        if minimized && !was {
            TRIM.with(|t| {
                t.start(slint::TimerMode::SingleShot, TRIM_AFTER, || {
                    if hidden() {
                        trim_working_set();
                    }
                })
            });
        } else if !minimized {
            TRIM.with(|t| t.stop());
        }
    }
    unsafe { windows::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Watch the main window. Call once, on the UI thread, with its HWND.
#[cfg(windows)]
pub fn attach(hwnd: windows::Win32::Foundation::HWND) {
    unsafe {
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(hwnd, Some(subclass_proc), 0x4C54_5649, 0);
    }
}

#[cfg(windows)]
fn trim_working_set() {
    use windows_sys::Win32::System::Memory::SetProcessWorkingSetSizeEx;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // (SIZE_T)-1 for both bounds = "remove as many pages as possible".
    unsafe {
        SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0);
    }
}
