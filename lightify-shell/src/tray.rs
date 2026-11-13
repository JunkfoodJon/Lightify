//! Notification-area (system tray) icon.
//!
//! Three different things on Windows are all called "the app icon" and none of them
//! implies the others:
//!
//! * the **.exe** icon (Explorer, a pinned shortcut, Alt-Tab) — a Win32 resource,
//!   embedded by `build.rs`;
//! * the **window/taskbar** icon — `Window { icon: … }` in `app.slint`;
//! * the **tray** icon — this module.
//!
//! The tray entry is present for the whole run. Left-click restores and focuses the
//! window; the context menu offers the same plus Quit. The shipped app's
//! hide-to-tray button (`#btn-tray`) is still absent — this gives the icon and the
//! restore path without inventing a UI control the original places elsewhere.

use slint::ComponentHandle;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::MainWindow;

/// Same artwork the window uses, decoded at startup — the fallback when the exe's
/// icon resource can't be loaded (and the only source off Windows).
const ICON_PNG: &[u8] = include_bytes!("../assets/lightify-icon.png");

/// Live tray state. The `TrayIcon` must be kept alive — dropping it removes the
/// icon from the notification area — so `main` holds this until the event loop ends.
pub struct Tray {
    _icon: TrayIcon,
}

impl Drop for Tray {
    /// `rescale`'s copy goes first, so dropping this still removes the icon at once.
    fn drop(&mut self) {
        let _ = ICON.try_with(|i| i.borrow_mut().take());
    }
}

/// The tray's icon size in px: the small-icon size at the DPI of the primary monitor,
/// whose taskbar holds the notification area — read now, not the DPI the process
/// started with, so a scale changed since is followed.
fn tray_px() -> u32 {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI};
        use windows::Win32::UI::WindowsAndMessaging::SM_CXSMICON;
        let (mut x, mut y) = (96u32, 96u32);
        unsafe {
            let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y);
            GetSystemMetricsForDpi(SM_CXSMICON, x.max(96)).max(16) as u32
        }
    }
    #[cfg(not(windows))]
    16
}

fn load_icon(px: u32) -> Option<Icon> {
    // The exe's own icon resource (id 1, see build.rs) at the tray's size, so Windows
    // picks the hand-tuned small cut out of lightify.ico (16–32 px) instead of
    // shrinking the 256 px artwork.
    #[cfg(windows)]
    {
        if let Ok(icon) = Icon::from_resource(1, Some((px, px))) {
            return Some(icon);
        }
    }
    #[cfg(not(windows))]
    let _ = px;
    let img = image::load_from_memory(ICON_PNG).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Bring the window back from minimised/behind and give it focus.
pub fn restore(app: &MainWindow) {
    use slint::winit_030::WinitWindowAccessor;
    app.window().with_winit_window(|w| {
        w.set_minimized(false);
        w.focus_window();
    });
    app.window().show().ok();
}

thread_local! {
    /// The menu's items, kept so a language switch can say them again (`relabel`). The
    /// tray belongs to the UI thread, and so do these.
    static ITEMS: std::cell::RefCell<Option<(MenuItem, MenuItem)>> = const { std::cell::RefCell::new(None) };
    /// The icon in the tray and the size it was drawn at, for `rescale`.
    static ICON: std::cell::RefCell<Option<(TrayIcon, u32)>> = const { std::cell::RefCell::new(None) };
}

/// The primary monitor's scale may have changed (the main window saw a DPI, display or
/// settings change): draw the icon again at the size that scale wants, so it isn't
/// stretched from the old one. No-op when the size is the same.
pub fn rescale() {
    ICON.with(|i| {
        let mut i = i.borrow_mut();
        let Some((icon, at)) = i.as_mut() else { return };
        let px = tray_px();
        if px != *at {
            if let Some(new) = load_icon(px) {
                if icon.set_icon(Some(new)).is_ok() {
                    *at = px;
                }
            }
        }
    });
}

/// The menu's two items, in the active language.
fn menu_items() -> (MenuItem, MenuItem) {
    (
        MenuItem::new(lightify_i18n::tr!("Show Lightify"), true, None),
        MenuItem::new(lightify_i18n::tr!("Quit"), true, None),
    )
}

/// After a language switch: the menu in the new language. A headless run (a self-test)
/// has no tray icon, so it gets the items alone, to read back with `labels`.
pub fn relabel() {
    ITEMS.with(|i| {
        let mut i = i.borrow_mut();
        match i.as_ref() {
            Some((show, quit)) => {
                show.set_text(lightify_i18n::tr!("Show Lightify"));
                quit.set_text(lightify_i18n::tr!("Quit"));
            }
            None => *i = Some(menu_items()),
        }
    });
}

/// What the menu says now (for `--selftest-language`).
pub fn labels() -> Vec<String> {
    ITEMS.with(|i| i.borrow().as_ref().map(|(s, q)| vec![s.text(), q.text()]).unwrap_or_default())
}

/// Install the tray icon. Returns `None` (and leaves the app perfectly usable) if
/// the shell refuses the icon — a missing tray is not worth failing startup over.
///
/// Must be called on the UI thread: on Windows the tray icon belongs to the thread
/// that created it, and that thread has to pump messages — which is exactly what
/// Slint's event loop does.
pub fn install(weak: slint::Weak<MainWindow>) -> Option<Tray> {
    let menu = Menu::new();
    let (show, quit) = menu_items();
    menu.append_items(&[&show, &PredefinedMenuItem::separator(), &quit]).ok()?;
    let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
    ITEMS.with(|i| *i.borrow_mut() = Some((show, quit)));

    let px = tray_px();
    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        // tray-icon opens the menu on a left click too by default; here a left click
        // only restores the window (below), and the menu is the right button's.
        .with_menu_on_left_click(false)
        .with_tooltip("Lightify")
        .with_icon(load_icon(px)?)
        .build()
        .ok()?;
    ICON.with(|i| *i.borrow_mut() = Some((icon.clone(), px)));

    // Both crates deliver events outside winit. They used to be drained by a 150 ms
    // UI-thread timer — ~7 wake-ups a second for the whole run, tray clicked or not.
    // Handlers instead run only when something happens; they hop onto the UI thread
    // via `upgrade_in_event_loop` (which is also what makes calling them from the
    // tray's own window procedure safe).
    {
        let weak = weak.clone();
        TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
            // Left button *up*, so a click that opened the context menu with the
            // right button doesn't also raise the window.
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                let _ = weak.upgrade_in_event_loop(|app| restore(&app));
            }
        }));
    }
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        if ev.id == show_id {
            let _ = weak.upgrade_in_event_loop(|app| restore(&app));
        } else if ev.id == quit_id {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        }
    }));

    Some(Tray { _icon: icon })
}
