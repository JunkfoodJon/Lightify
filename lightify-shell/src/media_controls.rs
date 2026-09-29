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

pub fn set_art_url(url: &str) {
    if let Ok(mut g) = ART_URL.lock() {
        if *g != url {
            *g = url.to_string();
        }
    }
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
                let _ = weak.upgrade_in_event_loop(move |app| {
                    if app.get_playing() != want {
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
    pub fn sync(&self, app: &MainWindow) {
        let title = app.get_track_name().to_string();
        let artist = app.get_track_artist().to_string();
        // "—" + "Nothing playing" is the idle placeholder, not a track.
        let has_track = !(title.is_empty() || title == "\u{2014}");
        let status = if !has_track {
            MediaPlaybackStatus::Closed
        } else if app.get_playing() {
            MediaPlaybackStatus::Playing
        } else {
            MediaPlaybackStatus::Paused
        };
        let art = if has_track { ART_URL.lock().map(|g| g.clone()).unwrap_or_default() } else { String::new() };
        let next = Shown { title, artist, art, status: status.0 };
        let prev = self.shown.replace(next.clone());
        if prev == next {
            return;
        }
        if prev.status != next.status {
            let _ = self.smtc.SetPlaybackStatus(status);
        }
        if (prev.title, prev.artist, prev.art) != (next.title.clone(), next.artist.clone(), next.art.clone()) {
            let _ = self.update_display(&next, has_track);
        }
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
