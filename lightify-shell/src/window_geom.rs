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
//! What is deliberately NOT recorded: a minimized window (Windows reports it at
//! -32000,-32000), a maximized window's size (it would become the "restore" size), the
//! mini player's size (fixed per layout), and anything smaller than the full window's
//! minimum (a frame caught mid-switch).

use serde::{Deserialize, Serialize};
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
}

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
        update(|g| g.maximized = true);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unusable_records_are_dropped_not_applied() {
        let bad = Geom {
            normal: Some(Rect { x: 0, y: 0, w: 200.0, h: 100.0 }),
            maximized: true,
            mini_pos: Some((5, 5)),
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
        assert!(g.maximized && g.normal.is_none() && g.mini_pos.is_none());
    }
}
