//! Windows media controls (System Media Transport Controls).
//!
//! Puts the current track — title, artist, cover — in Windows' media flyout (the
//! volume/media overlay, Win11 quick settings, the lock screen), and takes
//! play/pause/next/previous back from it. Hardware media keys are routed by Windows
//! to the active SMTC session, so this also replaces the old global `RegisterHotKey`
//! grab, which took the keys away from every other player on the machine. If SMTC
//! can't be set up, the caller falls back to that grab.
//!
//! State flows one way: the UI's now-playing properties are the source of truth
//! (whatever path set them — a poll, an engine event, a click), and `app.slint`
//! fires `media-changed` whenever one of them changes. The update is only pushed
//! to Windows when something it shows actually differs.

use std::cell::RefCell;

use slint::ComponentHandle;
use windows::core::HSTRING;
use windows::Foundation::{TypedEventHandler, Uri};
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls, SystemMediaTransportControlsButton,
    SystemMediaTransportControlsButtonPressedEventArgs,
};
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;

use crate::{Cmd, MainWindow};

/// What Windows is currently showing, to skip redundant updates.
#[derive(Default, PartialEq, Clone)]
struct Shown {
    title: String,
    artist: String,
    art: String,
    status: i32,
}

pub struct MediaControls {
    smtc: SystemMediaTransportControls,
    shown: RefCell<Shown>,
}

/// Cover URL of the current track, set by the worker when it fetches art (the UI
/// only holds the decoded image). Read on the UI thread in `sync`.
static ART_URL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Returns whether it changed. The UI's own changes (title, artist, playing) fire
/// `media-changed`, but the worker's cover URL can't: when this is true and the title has
/// already gone up, the caller has to fire it, or Windows keeps the previous song's cover.
pub fn set_art_url(url: &str) -> bool {
    let Ok(mut g) = ART_URL.lock() else { return false };
    if *g == url {
        return false;
    }
    *g = url.to_string();
    true
}

pub fn hwnd_of(w: &slint::winit_030::winit::window::Window) -> Option<HWND> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match w.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

/// One line in `%APPDATA%\Lightify\lightify-shell-media.log` (release builds have no
/// console). Overwritten each launch; only written on setup, never per track.
pub fn note(msg: &str) {
    let path = lightify_core::config::data_dir().join("lightify-shell-media.log");
    let _ = std::fs::write(path, format!("{msg}\n"));
}

impl MediaControls {
    /// Attach to the main window's native window (see `WinitWindowAccessor::winit_window`).
    pub fn attach(
        window: &slint::winit_030::winit::window::Window,
        app: &MainWindow,
        tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    ) -> windows::core::Result<Self> {
        let hwnd = hwnd_of(window)
            .ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-2147467259), "no Win32 window handle"))?;
        let interop = windows::core::factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
        let smtc: SystemMediaTransportControls = unsafe { interop.GetForWindow(hwnd)? };
        smtc.SetIsEnabled(true)?;
        smtc.SetIsPlayEnabled(true)?;
        smtc.SetIsPauseEnabled(true)?;
        smtc.SetIsNextEnabled(true)?;
        smtc.SetIsPreviousEnabled(true)?;
        smtc.SetIsStopEnabled(true)?;
        smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed)?;
        smtc.DisplayUpdater()?.SetType(MediaPlaybackType::Music)?;

        // Runs on a WinRT thread-pool thread; the worker channel is thread-safe.
        let weak = app.as_weak();
        smtc.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            let Some(args) = args.as_ref() else { return Ok(()) };
            let button = args.Button()?;
            // Play and Pause are distinct buttons (a keyboard's single Play/Pause key
            // arrives as whichever one Windows thinks applies), so both are checked
            // against the real state instead of blindly toggling.
            let want_playing = match button {
                SystemMediaTransportControlsButton::Play => Some(true),
                SystemMediaTransportControlsButton::Pause | SystemMediaTransportControlsButton::Stop => Some(false),
                _ => None,
            };
            if let Some(want) = want_playing {
                let tx = tx.clone();
                // The player on screen: Local's song while it is the one "now"
                // (`mini-playing` follows it), else Spotify's. The worker routes the
                // toggle to whichever owns the transport.
                let _ = weak.upgrade_in_event_loop(move |app| {
                    if app.get_mini_playing() != want {
                        let _ = tx.send(Cmd::TogglePlay);
                    }
                });
            } else if button == SystemMediaTransportControlsButton::Next {
                let _ = tx.send(Cmd::Next);
            } else if button == SystemMediaTransportControlsButton::Previous {
                let _ = tx.send(Cmd::Prev);
            }
            Ok(())
        }))?;

        note("attached");
        Ok(Self { smtc, shown: RefCell::new(Shown { status: -1, ..Default::default() }) })
    }

    /// Mirror the UI's now-playing state into Windows. Cheap when nothing changed.
    /// It shows what the app's own player shows (`mini-*`): a Local song while it is the
    /// one "now", Spotify's otherwise.
    pub fn sync(&self, app: &MainWindow) {
        let title = app.get_mini_title().to_string();
        let artist = app.get_mini_artist().to_string();
        // "—" + "Nothing playing" is the idle placeholder, not a track.
        let has_track = !(title.is_empty() || title == "\u{2014}");
        let status = if !has_track {
            MediaPlaybackStatus::Closed
        } else if app.get_mini_playing() {
            MediaPlaybackStatus::Playing
        } else {
            MediaPlaybackStatus::Paused
        };
        // A Local cover is a decoded image, with no URL Windows could fetch.
        let art = if has_track && !app.get_local_present() {
            ART_URL.lock().map(|g| g.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        let next = Shown { title, artist, art, status: status.0 };
        let prev = self.shown.borrow().clone();
        if prev == next {
            return;
        }
        let status_ok = prev.status == next.status || self.smtc.SetPlaybackStatus(status).is_ok();
        let display_ok = (prev.title.as_str(), prev.artist.as_str(), prev.art.as_str())
            == (next.title.as_str(), next.artist.as_str(), next.art.as_str())
            || self.update_display(&next, has_track).is_ok();
        // Only what Windows took counts as shown: a failed update is tried again on the
        // next sync instead of being skipped as "already there" for good.
        *self.shown.borrow_mut() = committed(&prev, &next, status_ok, display_ok);
    }

    fn update_display(&self, s: &Shown, has_track: bool) -> windows::core::Result<()> {
        let du = self.smtc.DisplayUpdater()?;
        if !has_track {
            du.ClearAll()?;
            du.SetType(MediaPlaybackType::Music)?;
            return du.Update();
        }
        du.SetType(MediaPlaybackType::Music)?;
        let music = du.MusicProperties()?;
        music.SetTitle(&HSTRING::from(s.title.as_str()))?;
        music.SetArtist(&HSTRING::from(s.artist.as_str()))?;
        if s.art.starts_with("https://") {
            // Windows fetches and caches the cover itself.
            let uri = Uri::CreateUri(&HSTRING::from(s.art.as_str()))?;
            du.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(&uri)?)?;
        } else {
            du.SetThumbnail(None)?;
        }
        du.Update()
    }
}

/// What Windows shows after an update from `prev` towards `next`, given which of the two
/// calls (status, display) went through.
fn committed(prev: &Shown, next: &Shown, status_ok: bool, display_ok: bool) -> Shown {
    let mut out = prev.clone();
    if status_ok {
        out.status = next.status;
    }
    if display_ok {
        out.title = next.title.clone();
        out.artist = next.artist.clone();
        out.art = next.art.clone();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{committed, set_art_url, Shown};

    #[test]
    fn a_failed_update_is_not_taken_as_shown() {
        let prev = Shown { title: "A".into(), artist: "x".into(), art: String::new(), status: 4 };
        let next = Shown { title: "B".into(), artist: "y".into(), art: "https://i.scdn.co/image/b".into(), status: 3 };
        assert!(committed(&prev, &next, true, true) == next);
        // The display call failed: the next sync still sees a change and tries again.
        let half = committed(&prev, &next, true, false);
        assert!(half != next && half.title == "A" && half.status == 3);
        assert!(committed(&prev, &next, false, false) == prev);
    }

    #[test]
    fn a_new_cover_url_is_reported_once() {
        assert!(set_art_url("https://i.scdn.co/image/a"));
        assert!(!set_art_url("https://i.scdn.co/image/a"));
        assert!(set_art_url("https://i.scdn.co/image/b"));
        assert!(set_art_url(""));
    }
}
