//! Taskbar thumbnail toolbar: Previous · Play/Pause · Next under the window's taskbar
//! hover preview (`ITaskbarList3::ThumbBarAddButtons`).
//!
//! The buttons report clicks as `WM_COMMAND` / `THBN_CLICKED` to the window, which
//! winit owns — so the window is subclassed (`SetWindowSubclass`) to catch those, plus
//! the "TaskbarButtonCreated" broadcast, after which the toolbar has to be added again
//! (Explorer restarted, or the button didn't exist yet on the first try).
//!
//! Glyphs are the same Material shapes the in-app transport uses, rasterised here at
//! the small-icon size for the window's DPI, in a colour that suits the taskbar theme.
//! Everything lives on the UI thread (the window's thread), hence `thread_local!`.

use std::cell::{Cell, RefCell};

use windows::core::{w, BOOL};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    DefSubclassProc, ITaskbarList3, SetWindowSubclass, TaskbarList, THBF_ENABLED, THBN_CLICKED, THB_FLAGS, THB_ICON,
    THB_TOOLTIP, THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, RegisterWindowMessageW, HICON, ICONINFO, SM_CXSMICON, WM_COMMAND,
};

use crate::Cmd;

const ID_PREV: u32 = 1;
const ID_PLAY: u32 = 2;
const ID_NEXT: u32 = 3;

struct State {
    list: ITaskbarList3,
    hwnd: HWND,
    tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    prev: HICON,
    play: HICON,
    pause: HICON,
    next: HICON,
    playing: Cell<bool>,
    created_msg: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

// ── Glyphs ───────────────────────────────────────────────────────────────────

/// A shape in the 24×24 icon space the app's SVG paths use.
enum Shape {
    Rect(f32, f32, f32, f32),
    Tri([(f32, f32); 3]),
}

fn inside(shape: &Shape, x: f32, y: f32) -> bool {
    match *shape {
        Shape::Rect(x0, y0, x1, y1) => x >= x0 && x < x1 && y >= y0 && y < y1,
        Shape::Tri([a, b, c]) => {
            let s = |p: (f32, f32), q: (f32, f32)| (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0);
            let (d0, d1, d2) = (s(a, b), s(b, c), s(c, a));
            (d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0) || (d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0)
        }
    }
}

// Same geometry as app.slint: prev "M6 6h2v12H6zm3.5 6l8.5 6V6z", play "M8 5v14l11-7z",
// pause "M6 19h4V5H6v14zm8-14v14h4V5h-4z", next "M6 18l8.5-6L6 6v12zM16 6v12h2V6h-2z".
fn glyph_prev() -> Vec<Shape> {
    vec![Shape::Rect(6.0, 6.0, 8.0, 18.0), Shape::Tri([(9.5, 12.0), (18.0, 18.0), (18.0, 6.0)])]
}
fn glyph_play() -> Vec<Shape> {
    vec![Shape::Tri([(8.0, 5.0), (8.0, 19.0), (19.0, 12.0)])]
}
fn glyph_pause() -> Vec<Shape> {
    vec![Shape::Rect(6.0, 5.0, 10.0, 19.0), Shape::Rect(14.0, 5.0, 18.0, 19.0)]
}
fn glyph_next() -> Vec<Shape> {
    vec![Shape::Tri([(6.0, 18.0), (14.5, 12.0), (6.0, 6.0)]), Shape::Rect(16.0, 6.0, 18.0, 18.0)]
}

/// Rasterise with 4×4 supersampling into a 32-bit icon (straight alpha, as icons use).
unsafe fn make_icon(shapes: &[Shape], size: i32, rgb: (u8, u8, u8)) -> windows::core::Result<HICON> {
    const SS: i32 = 4;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let color = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)? };
    let px = unsafe { std::slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize) };
    let scale = 24.0 / size as f32;
    for y in 0..size {
        for x in 0..size {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    if shapes.iter().any(|s| inside(s, u, v)) {
                        hits += 1;
                    }
                }
            }
            let i = ((y * size + x) * 4) as usize;
            px[i] = rgb.2;
            px[i + 1] = rgb.1;
            px[i + 2] = rgb.0;
            px[i + 3] = (hits * 255 / (SS * SS)) as u8;
        }
    }
    // With a 32-bit colour bitmap the mask is ignored for drawing, but it must exist.
    let zeros = vec![0u8; (((size + 15) / 16 * 2) * size) as usize];
    let mask = unsafe { CreateBitmap(size, size, 1, 1, Some(zeros.as_ptr() as *const _)) };
    let info = ICONINFO { fIcon: BOOL(1), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = unsafe { CreateIconIndirect(&info) };
    unsafe {
        let _ = DeleteObject(HGDIOBJ(mask.0));
        let _ = DeleteObject(HGDIOBJ(color.0));
    }
    icon
}

/// Taskbar (and its thumbnail flyout) follows the *system* theme, not the app theme.
fn taskbar_is_light() -> bool {
    let mut value: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut len),
        )
    };
    r.is_ok() && value == 1
}

// ── Toolbar ──────────────────────────────────────────────────────────────────

fn tip(text: &str) -> [u16; 260] {
    let mut buf = [0u16; 260];
    for (i, c) in text.encode_utf16().take(259).enumerate() {
        buf[i] = c;
    }
    buf
}

fn button(id: u32, icon: HICON, text: &str) -> THUMBBUTTON {
    THUMBBUTTON {
        dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
        iId: id,
        iBitmap: 0,
        hIcon: icon,
        szTip: tip(text),
        dwFlags: THBF_ENABLED,
    }
}

impl State {
    fn play_button(&self) -> THUMBBUTTON {
        if self.playing.get() {
            button(ID_PLAY, self.pause, "Pause")
        } else {
            button(ID_PLAY, self.play, "Play")
        }
    }

    fn add(&self) -> windows::core::Result<()> {
        let buttons = [button(ID_PREV, self.prev, "Previous"), self.play_button(), button(ID_NEXT, self.next, "Next")];
        unsafe { self.list.ThumbBarAddButtons(self.hwnd, &buttons) }
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let handled = STATE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else { return false };
        if msg == WM_COMMAND && ((wparam.0 >> 16) & 0xffff) as u32 == THBN_CLICKED {
            let cmd = match (wparam.0 & 0xffff) as u32 {
                ID_PREV => Cmd::Prev,
                ID_PLAY => Cmd::TogglePlay,
                ID_NEXT => Cmd::Next,
                _ => return false,
            };
            let _ = st.tx.send(cmd);
            return true;
        }
        if msg == st.created_msg {
            // First creation or an Explorer restart: the (new) button has no toolbar.
            unsafe {
                let _ = st.list.HrInit();
            }
            let _ = st.add();
        }
        false
    });
    if handled {
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Install the toolbar on `hwnd`. Must run on the window's own (UI) thread.
pub fn attach(hwnd: HWND, tx: tokio::sync::mpsc::UnboundedSender<Cmd>) -> windows::core::Result<()> {
    unsafe {
        // Already initialised by winit (OLE) in the usual case; S_FALSE / changed-mode
        // results are fine — only the apartment has to exist.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let list: ITaskbarList3 = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)?;
        list.HrInit()?;
        let dpi = GetDpiForWindow(hwnd).max(96);
        let size = GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16);
        let rgb = if taskbar_is_light() { (0x20, 0x20, 0x20) } else { (0xff, 0xff, 0xff) };
        let st = State {
            list,
            hwnd,
            tx,
            prev: make_icon(&glyph_prev(), size, rgb)?,
            play: make_icon(&glyph_play(), size, rgb)?,
            pause: make_icon(&glyph_pause(), size, rgb)?,
            next: make_icon(&glyph_next(), size, rgb)?,
            playing: Cell::new(false),
            created_msg: RegisterWindowMessageW(w!("TaskbarButtonCreated")),
        };
        // If the taskbar button already exists this succeeds now; if not, the
        // TaskbarButtonCreated message will add it.
        let _ = st.add();
        STATE.with(|s| *s.borrow_mut() = Some(st));
        if !SetWindowSubclass(hwnd, Some(subclass_proc), 0x4C54_4659, 0).as_bool() {
            STATE.with(|s| *s.borrow_mut() = None);
            return Err(windows::core::Error::from_thread());
        }
    }
    Ok(())
}

/// Flip the middle button between Play and Pause. No-op when unchanged.
pub fn set_playing(playing: bool) {
    STATE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else { return };
        if st.playing.replace(playing) != playing {
            let b = [st.play_button()];
            unsafe {
                let _ = st.list.ThumbBarUpdateButtons(st.hwnd, &b);
            }
        }
    });
}
