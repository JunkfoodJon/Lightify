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

fn load_icon() -> Option<Icon> {
    // The exe's own icon resource (id 1, see build.rs) at the tray's size, so Windows
    // picks the hand-tuned small cut out of lightify.ico (16–32 px) instead of
    // shrinking the 256 px artwork.
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
        let px = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
        if let Ok(icon) = Icon::from_resource(1, Some((px, px))) {
            return Some(icon);
        }
    }
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

    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Lightify")
        .with_icon(load_icon()?)
        .build()
        .ok()?;

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
