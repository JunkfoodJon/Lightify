//! Remembering the window between launches, and across the trip through maximize,
//! minimize and the mini player.
//!
//! One record (`Geom`): the **normal** window's rect (what un-maximizing and leaving
//! the mini player return to), whether it was maximized, and where the mini player
//! sat. It is captured from the window's own state after it settles (`capture`, run
//! by a short debounce on move/resize events and once on close), never from the
//! intermediate sizes a switch passes through, and written only when it changed.
//!
//! Units: the position is the outer top-left in **physical** screen pixels (what
//! Windows reports and accepts, monitor by monitor); the size is the inner size in
//! **logical** pixels, so a window keeps its look when it lands on a monitor with a
//! different scale.
//!
//! A restore is fitted to the screen as it is now: a window maximized on another
//! monitor than its normal rect comes back maximized there (`restore_rect`, from
//! `max_at`), and once the restored window has settled it is pulled inside its
//! monitor's work area — shrunk if it has to be — so a smaller monitor or the taskbar
//! no longer hides part of it (`fit_if_pending`).
//!
//! What is deliberately NOT recorded: a minimized window (Windows reports it at
//! -32000,-32000), a maximized window's size (it would become the "restore" size), the
//! mini player's size (fixed per layout), and anything smaller than the full window's
//! minimum (a frame caught mid-switch).

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The full window's minimum, in logical px (matches `MainWindow`'s `min-width`/`min-height`).
pub const MIN_W: f64 = 640.0;
pub const MIN_H: f64 = 460.0;
/// First launch, or a saved size that can't be used.
pub const DEFAULT_W: f64 = 980.0;
pub const DEFAULT_H: f64 = 660.0;
/// Anything wider/taller than this is a corrupt record, not a window.
const MAX_SIDE: f64 = 16384.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    /// Outer top-left, physical px.
    pub x: i32,
    pub y: i32,
    /// Inner size, logical px.
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Geom {
    #[serde(default)]
    pub normal: Option<Rect>,
    #[serde(default)]
    pub maximized: bool,
    /// The mini player's outer top-left, physical px.
    #[serde(default)]
    pub mini_pos: Option<(i32, i32)>,
    /// The maximized window's centre, physical px: which monitor it was maximized on
    /// (the normal rect may sit on another one).
    #[serde(default)]
    pub max_at: Option<(i32, i32)>,
}

/// A monitor's work area (the screen less the taskbar): left, top, right, bottom in
/// physical px.
pub type Area = (i32, i32, i32, i32);

static GEOM: Mutex<Option<Geom>> = Mutex::new(None);
/// What is on disk, so an unchanged record is never rewritten.
static WRITTEN: Mutex<Option<Geom>> = Mutex::new(None);

fn path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_window.json")
}

/// A record read from disk, with anything unusable dropped rather than applied.
fn sanitize(mut g: Geom) -> Geom {
    g.normal = g.normal.filter(|r| {
        r.w.is_finite() && r.h.is_finite() && r.w >= MIN_W - 1.0 && r.h >= MIN_H - 1.0 && r.w <= MAX_SIDE && r.h <= MAX_SIDE
    });
    g
}

/// The saved record (read once, then kept in memory).
pub fn get() -> Geom {
    let mut g = GEOM.lock().unwrap_or_else(|p| p.into_inner());
    if g.is_none() {
        let loaded = std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Geom>(&b).ok())
            .map(sanitize)
            .unwrap_or_default();
        *WRITTEN.lock().unwrap_or_else(|p| p.into_inner()) = Some(loaded.clone());
        *g = Some(loaded);
    }
    g.clone().unwrap_or_default()
}

fn update(f: impl FnOnce(&mut Geom)) {
    get();
    let mut g = GEOM.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(g) = g.as_mut() {
        f(g);
    }
}

/// Write the record if it differs from what is on disk.
pub fn save_if_changed() {
    let now = get();
    let mut written = WRITTEN.lock().unwrap_or_else(|p| p.into_inner());
    if written.as_ref() == Some(&now) {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&now) {
        if lightify_core::config::write_atomic(&path(), &bytes).is_ok() {
            *written = Some(now);
        }
    }
}

/// Record the window's settled state. `mini`: the mini player is showing, so only its
/// position counts.
pub fn capture(w: &slint::winit_030::winit::window::Window, mini: bool) {
    if w.is_minimized() == Some(true) {
        return;
    }
    // Full screen (the ASCII lava screensaver) is not where the window sits either: it
    // would store the screen as the normal rect and drop the maximized state.
    if w.fullscreen().is_some() {
        return;
    }
    let Ok(pos) = w.outer_position() else { return };
    // Windows parks a minimized window at -32000,-32000; `is_minimized` covers it,
    // this covers a backend that doesn't report minimized state.
    if pos.x <= -30000 || pos.y <= -30000 {
        return;
    }
    if mini {
        // Win+Up can maximize even the mini player; that is not a place it "sits".
        if !w.is_maximized() {
            update(|g| g.mini_pos = Some((pos.x, pos.y)));
        }
        return;
    }
    if w.is_maximized() {
        // The normal rect stays what it was: it is what un-maximizing returns to.
        let sz = w.outer_size();
        let at = (pos.x + sz.width as i32 / 2, pos.y + sz.height as i32 / 2);
        update(|g| {
            g.maximized = true;
            g.max_at = Some(at);
        });
        return;
    }
    let sf = w.scale_factor();
    let sz = w.inner_size();
    let (lw, lh) = ((sz.width as f64 / sf).round(), (sz.height as f64 / sf).round());
    if lw < MIN_W - 1.0 || lh < MIN_H - 1.0 || lw > MAX_SIDE || lh > MAX_SIDE {
        return;
    }
    update(|g| {
        g.maximized = false;
        g.normal = Some(Rect { x: pos.x, y: pos.y, w: lw, h: lh });
    });
}

/// Is a window whose outer top-left is at (x, y) (physical px) reachable — is the
/// part of its title bar you'd grab on some monitor? A monitor unplugged (or a
/// layout changed) since it was saved would otherwise put the window off-screen.
pub fn on_screen(x: i32, y: i32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
        // Two points along the top bar: the logo end and a little further in.
        [(x + 24, y + 12), (x + 160, y + 12)]
            .iter()
            .any(|&(px, py)| !unsafe { MonitorFromPoint(POINT { x: px, y: py }, MONITOR_DEFAULTTONULL) }.is_null())
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        true
    }
}

/// The normal rect to restore: as saved, unless the window was maximized on another
/// monitor — then the rect moves onto that one (keeping its place in the work area
/// as far as it fits), so the window maximizes there and un-maximizes there too.
pub fn restore_rect(g: &Geom) -> Option<Rect> {
    let r = g.normal?;
    #[cfg(windows)]
    if let Some((mx, my)) = g.max_at.filter(|_| g.maximized) {
        if let Some((to_mon, to)) = monitor_at(mx, my) {
            let from = monitor_at(r.x + 24, r.y + 12);
            if from.map(|f| f.0) != Some(to_mon) {
                let (x, y) = move_to_area(r.x, r.y, from.map_or(to, |f| f.1), to);
                return Some(Rect { x, y, ..r });
            }
        }
    }
    Some(r)
}

/// `(x, y)` keeps its offset from `from`'s top-left, now from `to`'s; clamped so at
/// least the title bar's start stays inside `to`.
pub fn move_to_area(x: i32, y: i32, from: Area, to: Area) -> (i32, i32) {
    let nx = to.0 + (x - from.0);
    let ny = to.1 + (y - from.1);
    (nx.clamp(to.0, (to.2 - 200).max(to.0)), ny.clamp(to.1, (to.3 - 100).max(to.1)))
}

/// `(x, y, w, h)` (physical px) inside `work`: shrunk to it first if larger, then
/// moved in.
pub fn fit_rect((x, y, w, h): (i32, i32, i32, i32), work: Area) -> (i32, i32, i32, i32) {
    let (l, t, r, b) = work;
    let w = w.min(r - l).max(1);
    let h = h.min(b - t).max(1);
    (x.clamp(l, r - w), y.clamp(t, b - h), w, h)
}

/// Set by a restore (`fit_next`), taken by the next settled capture.
static FIT_PENDING: AtomicBool = AtomicBool::new(false);

/// The window was just restored: fit it once it has settled (`fit_if_pending`).
pub fn fit_next() {
    FIT_PENDING.store(true, Ordering::SeqCst);
}

/// Whether the pending fit runs now: only on the full window in its normal state. A
/// window restored maximized (or minimized, full screen, the mini player) keeps the fit
/// pending until it first settles un-maximized, so the normal rect it returns to is
/// pulled inside the work area then.
fn take_fit(mini: bool, maximized: bool, minimized: bool, fullscreen: bool) -> bool {
    if mini || maximized || minimized || fullscreen {
        return false;
    }
    FIT_PENDING.swap(false, Ordering::SeqCst)
}

/// Once after a restore: pull the full (not maximized) window inside the work area of
/// the monitor it landed on. Run when the window has settled, so the monitor's own
/// scale is already applied to its size.
pub fn fit_if_pending(w: &slint::winit_030::winit::window::Window, mini: bool) {
    use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};
    if !take_fit(mini, w.is_maximized(), w.is_minimized() == Some(true), w.fullscreen().is_some()) {
        return;
    }
    let Ok(pos) = w.outer_position() else { return };
    let (outer, inner) = (w.outer_size(), w.inner_size());
    let (ow, oh) = (outer.width as i32, outer.height as i32);
    let Some(work) = work_area_near(pos.x + ow / 2, pos.y + oh / 2) else { return };
    let (x, y, fw, fh) = fit_rect((pos.x, pos.y, ow, oh), work);
    if (fw, fh) != (ow, oh) {
        let _ = w.request_inner_size(PhysicalSize::new(
            (inner.width as i32 - (ow - fw)).max(1) as u32,
            (inner.height as i32 - (oh - fh)).max(1) as u32,
        ));
    }
    if (x, y) != (pos.x, pos.y) {
        w.set_outer_position(PhysicalPosition::new(x, y));
    }
}

/// The monitor under (x, y) and its work area; `None` off every monitor.
#[cfg(windows)]
fn monitor_at(x: i32, y: i32) -> Option<(usize, Area)> {
    use windows_sys::Win32::Graphics::Gdi::MONITOR_DEFAULTTONULL;
    monitor_info(x, y, MONITOR_DEFAULTTONULL)
}

/// The work area of the monitor under (or nearest to) (x, y).
fn work_area_near(x: i32, y: i32) -> Option<Area> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST;
        monitor_info(x, y, MONITOR_DEFAULTTONEAREST).map(|m| m.1)
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        None
    }
}

#[cfg(windows)]
fn monitor_info(x: i32, y: i32, flags: windows_sys::Win32::Graphics::Gdi::MONITOR_FROM_FLAGS) -> Option<(usize, Area)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO};
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, flags);
        if mon.is_null() {
            return None;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi) == 0 {
            return None;
        }
        let r = mi.rcWork;
        Some((mon as usize, (r.left, r.top, r.right, r.bottom)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unusable_records_are_dropped_not_applied() {
        let bad = Geom {
            normal: Some(Rect { x: 0, y: 0, w: 200.0, h: 100.0 }),
            maximized: true,
            mini_pos: Some((5, 5)),
            max_at: None,
        };
        let s = sanitize(bad);
        assert_eq!(s.normal, None);
        assert!(s.maximized);
        assert_eq!(s.mini_pos, Some((5, 5)));
        let nan = Geom { normal: Some(Rect { x: 0, y: 0, w: f64::NAN, h: 700.0 }), ..Default::default() };
        assert_eq!(sanitize(nan).normal, None);
        let ok = Geom { normal: Some(Rect { x: -1200, y: 40, w: 1280.0, h: 800.0 }), ..Default::default() };
        assert_eq!(sanitize(ok.clone()), ok);
    }

    #[test]
    fn an_older_or_partial_file_still_loads() {
        let g: Geom = serde_json::from_str(r#"{"maximized":true}"#).unwrap();
        assert!(g.maximized && g.normal.is_none() && g.mini_pos.is_none() && g.max_at.is_none());
    }

    #[test]
    fn a_window_is_fitted_inside_the_work_area() {
        // A 1920x1040 work area (a 1080p screen less a 40 px taskbar).
        let work = (0, 0, 1920, 1040);
        // Already inside: untouched.
        assert_eq!(fit_rect((100, 80, 980, 660), work), (100, 80, 980, 660));
        // Hanging off the right and under the taskbar: moved in, same size.
        assert_eq!(fit_rect((1500, 700, 980, 660), work), (940, 380, 980, 660));
        // Saved on a bigger monitor: shrunk to the work area, then placed in it.
        assert_eq!(fit_rect((-50, -20, 2560, 1400), work), (0, 0, 1920, 1040));
        // A second monitor to the left (negative coordinates) works the same.
        assert_eq!(fit_rect((-2000, 10, 980, 660), (-1920, 0, 0, 1080)), (-1920, 10, 980, 660));
    }

    #[test]
    fn a_window_restored_maximized_is_fitted_once_it_is_un_maximized() {
        fit_next();
        // Restored maximized: nothing to fit yet, and the fit is not used up.
        assert!(!take_fit(false, true, false, false));
        assert!(!take_fit(false, false, true, false), "minimized: still waiting");
        assert!(!take_fit(true, false, false, false), "mini player: still waiting");
        // The first settle as a normal window fits it, once.
        assert!(take_fit(false, false, false, false));
        assert!(!take_fit(false, false, false, false));
    }

    #[test]
    fn a_rect_moves_onto_the_monitor_it_was_maximized_on() {
        let left = (-1920, 0, 0, 1040);
        let right = (0, 0, 2560, 1400);
        // Same offset into the other monitor's work area.
        assert_eq!(move_to_area(-1800, 100, left, right), (120, 100));
        // An offset the target can't take is clamped so the title bar stays on it.
        assert_eq!(move_to_area(2400, 1300, right, left), (-200, 940));
    }
}
