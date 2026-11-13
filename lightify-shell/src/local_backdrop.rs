//! The Local view's backdrop: a wall of random ASCII text (each character seeded per
//! cell, re-rolled now and then) with a slow lava-lamp fade morphing over it. Soft blobs drift,
//! merge and split; each character brightens as one passes over it, and the text in a
//! blob's hot core churns (re-rolls several times a second), so it looks animated.
//!
//! Rendered here into one transparent image per frame (JetBrains Mono, the UI's mono
//! face, rasterised once per DPI scale) at ~15 fps, and only while local audio plays
//! in Local mode (frames are skipped while minimised). It fades in and out over ~0.7 s
//! and stops its timer once faded out, so an idle or paused player costs nothing.
//!
//! In the now-playing view the cover's colour glow is drawn into the frame too (from a
//! cached layer) and the view's own gradient is hidden meanwhile: the software renderer
//! would otherwise re-evaluate that large radial gradient under every frame.
//!
//! The same wall runs behind the Spotify now-playing pane while Spotify plays, and as a
//! full-screen screensaver (right-click the lava). Settings › ascii lava turns the two
//! backdrops off; the screensaver stays available.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};

use crate::{MainWindow, Pal};

const FONT: &[u8] = include_bytes!("../ui/fonts/JetBrainsMono-Regular.ttf");
/// Logical font size of the wall; the cell is one advance wide, 1.45 lines high.
const FONT_PX: f32 = 11.0;
const FRAME: Duration = Duration::from_millis(66);
const FADE_S: f32 = 0.7;
/// Every character shows at least `base`; a blob's core reaches `base + peak`.
#[derive(Clone, Copy)]
struct Levels {
    base: f32,
    peak: f32,
}
/// Behind the views: quiet, so the controls stay readable (the cover's glow under it
/// does the rest).
const BACKDROP: Levels = Levels { base: 0.05, peak: 0.27 };
/// The screensaver has nothing in front of it and no glow under it: the wall stays
/// about as dim, the lava itself comes up so it is what you see. At the backdrop's
/// levels its brightest characters reached 21 % of white.
const SAVER: Levels = Levels { base: 0.07, peak: 0.75 };
/// The wall's alphabet, by weight: mostly letters and digits, light punctuation, and
/// gaps so it reads as text rather than a solid block. Heavy glyphs (@ # %) are kept
/// out; they would stand out as dots of ink everywhere.
const WALL: &[(&[u8], u32)] = &[
    (b"abcdefghijklmnopqrstuvwxyz", 34),
    (b"ABCDEFGHIJKLMNOPQRSTUVWXYZ", 8),
    (b"0123456789", 14),
    (b".,:;'`-_~^\"", 14),
    (b"/\\|()[]{}<>+=*!?", 16),
    (b" ", 14),
];
/// How often a character re-rolls (per second): a slow shimmer outside the lava,
/// up to CHURN in a blob's core.
const REROLL: f32 = 0.0675;
const CHURN: f32 = 5.25;
/// Where the now-playing glow ends, as a share of its half-width, and its falloff:
/// fitted to the software renderer's own `@radial-gradient(circle, c 0%, transparent
/// 60%)` (it reaches past 60 % of the half-width and fades quadratically): 0.6 levels
/// of 255 RMS over the glow beside and above the cover.
const GLOW_STOP: f32 = 0.795;
fn glow_falloff(s: f32) -> f32 {
    (1.0 - s) * (1.0 - s)
}

struct Glyph {
    /// Offset of the coverage box from the cell's top-left, in physical px.
    ox: i32,
    oy: i32,
    w: usize,
    h: usize,
    cov: Vec<u8>,
}

struct Atlas {
    scale: f32,
    cw: usize,
    ch: usize,
    /// Printable ASCII, 0x20..0x7f.
    glyphs: Vec<Option<Glyph>>,
}

fn atlas(scale: f32) -> Atlas {
    let font = FontRef::try_from_slice(FONT).expect("bundled JetBrains Mono");
    let sf = font.as_scaled(PxScale::from(FONT_PX * scale));
    let cw = sf.h_advance(font.glyph_id('M')).round().max(1.0) as usize;
    let ch = (FONT_PX * scale * 1.45).round().max(1.0) as usize;
    let baseline = (ch as f32 - (sf.ascent() - sf.descent())) / 2.0 + sf.ascent();
    let glyphs = (0x20u8..0x7f)
        .map(|c| {
            let mut g = sf.scaled_glyph(c as char);
            g.position = point(0.0, baseline);
            let og = font.outline_glyph(g)?;
            let b = og.px_bounds();
            let (w, h) = (b.width() as usize, b.height() as usize);
            let mut cov = vec![0u8; w * h];
            og.draw(|x, y, v| {
                if let Some(p) = cov.get_mut(y as usize * w + x as usize) {
                    *p = (v.clamp(0.0, 1.0) * 255.0) as u8;
                }
            });
            Some(Glyph { ox: b.min.x as i32, oy: b.min.y as i32, w, h, cov })
        })
        .collect();
    Atlas { scale, cw, ch, glyphs }
}

/// Where a frame goes: the Local panel, the Spotify now-playing pane, or the
/// full-screen screensaver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Surface {
    Local = 0,
    Native = 1,
    Saver = 2,
}

#[derive(Default)]
struct Bg {
    atlas: Option<Atlas>,
    started: Option<Instant>,
    last: Option<Instant>,
    /// Fade per surface, 0..1.
    gain: [f32; 3],
    /// A frame is up on that surface (so fading out ends by clearing it).
    shown: [bool; 3],
    /// A fresh wall each run of the app.
    seed: Option<u32>,
    /// The now-playing glow, premultiplied, for (w, h, cx, cy, d, rgba) — Local, Native.
    glow: [Option<((usize, usize, i32, i32, i32, u32), Vec<Rgba8Pixel>)>; 2],
    /// The window was maximized when the screensaver took it full screen.
    saver_was_max: bool,
}

thread_local! {
    static BG: RefCell<Bg> = RefCell::default();
    static TIMER: slint::Timer = slint::Timer::default();
}

/// Settings › ascii lava, on unless switched off (written whole, `write_atomic`).
fn setting_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_lava.txt")
}

/// Wires the wake-up (the UI calls `local-bg-wake` when playback, the mode, the cover
/// or the setting changes, which (re)starts the frame timer if it is stopped), the
/// setting, and the screensaver's way out.
pub fn install(app: &MainWindow) {
    app.set_lava_on(std::fs::read_to_string(setting_path()).map(|s| s.trim() != "off").unwrap_or(true));
    app.on_set_lava_on(|on| {
        let _ = lightify_core::config::write_atomic(&setting_path(), if on { "on" } else { "off" }.as_bytes());
    });
    let weak = app.as_weak();
    app.on_lava_saver_end(move || {
        if let Some(a) = weak.upgrade() {
            end_saver(&a);
        }
    });
    let weak = app.as_weak();
    app.on_local_bg_wake(move || wake(&weak));
    wake(&app.as_weak());
}

/// The screensaver: the window goes full screen on its monitor with only the wall in
/// it (the backdrop menu's one item).
pub fn start_saver(app: &MainWindow) {
    if app.get_lava_saver() || app.get_mini_mode() != "off" {
        return;
    }
    BG.with(|bg| {
        let mut bg = bg.borrow_mut();
        bg.saver_was_max = app.window().is_maximized();
        bg.gain[Surface::Saver as usize] = 0.0;
    });
    app.set_ctx_open(false);
    app.window().set_fullscreen(true);
    app.set_lava_saver(true);
    wake(&app.as_weak());
}

/// Any click or key: back to the window as it was.
fn end_saver(app: &MainWindow) {
    if !app.get_lava_saver() {
        return;
    }
    let was_max = BG.with(|bg| {
        let mut bg = bg.borrow_mut();
        bg.gain[Surface::Saver as usize] = 0.0;
        bg.shown[Surface::Saver as usize] = false;
        bg.saver_was_max
    });
    app.set_lava_saver(false);
    app.set_saver_bg(Image::default());
    app.window().set_fullscreen(false);
    if was_max && !app.window().is_maximized() {
        app.window().set_maximized(true);
    }
    wake(&app.as_weak());
}

fn wake(weak: &slint::Weak<MainWindow>) {
    TIMER.with(|t| {
        if t.running() {
            return;
        }
        let weak = weak.clone();
        t.start(slint::TimerMode::Repeated, FRAME, move || tick(&weak));
    });
}

/// Which surfaces want the lava now: Local while local audio plays in Local mode and
/// the view has a backdrop up (the track list with rows in it, or the now-playing
/// view; an empty list shows none), the Spotify pane while Spotify plays (cover not
/// expanded) — both only with the setting on and not in the mini player, which shows
/// neither view — and the screensaver while it is up. (`mini-mode` and
/// `local-bg-shown` changes wake the timer, so the lava comes back with its view.)
fn wanted(app: &MainWindow) -> [bool; 3] {
    let on = app.get_lava_on() && app.get_mini_mode() == "off";
    let local = app.get_local_mode();
    [
        on && local && app.get_local_now_playing() && app.get_local_bg_shown(),
        on && !local && app.get_playing() && !app.get_art_full(),
        app.get_lava_saver(),
    ]
}

fn tick(weak: &slint::Weak<MainWindow>) {
    let Some(app) = weak.upgrade() else {
        TIMER.with(|t| t.stop());
        return;
    };
    let want = wanted(&app);
    // Minimised: skip frames but keep the timer, so restoring needs no wake-up.
    if want.iter().any(|&w| w) && app.window().is_minimized() {
        return;
    }
    let (cleared, idle) = BG.with(|bg| {
        let mut bg = bg.borrow_mut();
        let now = Instant::now();
        let dt = bg.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0).min(0.25);
        bg.last = Some(now);
        let step = dt / FADE_S;
        let t = bg.started.get_or_insert(now).elapsed().as_secs_f32();
        let saver = want[Surface::Saver as usize];
        let mut cleared = Vec::new();
        for s in [Surface::Local, Surface::Native, Surface::Saver] {
            let i = s as usize;
            // Under the screensaver the views are covered: hold their fade, draw nothing.
            if saver && s != Surface::Saver {
                continue;
            }
            bg.gain[i] = (bg.gain[i] + if want[i] { step } else { -step }).clamp(0.0, 1.0);
            if bg.gain[i] > 0.0 {
                let gain = bg.gain[i];
                draw(&mut bg, &app, s, t, gain);
                bg.shown[i] = true;
            } else if bg.shown[i] {
                bg.shown[i] = false;
                cleared.push(s);
            }
        }
        let idle = !want.iter().any(|&w| w) && bg.gain.iter().all(|&g| g <= 0.0);
        if idle {
            bg.last = None;
        }
        (cleared, idle)
    });
    for s in cleared {
        clear(&app, s);
    }
    if idle {
        TIMER.with(|t| t.stop());
    }
}

fn clear(app: &MainWindow, s: Surface) {
    match s {
        Surface::Local => {
            app.set_local_bg(Image::default());
            app.set_local_glow_baked(false);
        }
        Surface::Native => {
            app.set_np_bg(Image::default());
            app.set_np_glow_baked(false);
        }
        Surface::Saver => app.set_saver_bg(Image::default()),
    }
}

/// `--shot-local` / `--shot-lava` `<variant>@<seconds>`: one full-strength frame at that
/// time, on the surface the shot shows.
pub fn shot_frame(app: &MainWindow, t: f32) {
    let s = if app.get_lava_saver() {
        Surface::Saver
    } else if app.get_local_mode() {
        Surface::Local
    } else {
        Surface::Native
    };
    BG.with(|bg| {
        let mut bg = bg.borrow_mut();
        let start = Instant::now();
        draw(&mut bg, app, s, t, 1.0);
        eprintln!("{s:?} backdrop frame {:.2} ms", start.elapsed().as_secs_f64() * 1000.0);
    });
}

fn draw(bg: &mut Bg, app: &MainWindow, s: Surface, t: f32, gain: f32) {
    let scale = app.window().scale_factor();
    let (w, h) = match s {
        Surface::Local => ((app.get_local_bg_w() * scale) as usize, (app.get_local_bg_h() * scale) as usize),
        Surface::Native => ((app.get_np_bg_w() * scale) as usize, (app.get_np_bg_h() * scale) as usize),
        Surface::Saver => {
            let size = app.window().size();
            (size.width as usize, size.height as usize)
        }
    };
    if w < 8 || h < 8 {
        return;
    }
    if bg.atlas.as_ref().map(|a| a.scale) != Some(scale) {
        bg.atlas = Some(atlas(scale));
    }
    let seed = *bg.seed.get_or_insert_with(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(7)
    });
    let accent = rgb(app.global::<Pal>().get_accent());
    // The cover's colour: the ink fades to it down the wall, and the glow is drawn in it.
    let cover = match s {
        Surface::Local => app.get_local_now_glow(),
        Surface::Native => app.get_glow(),
        Surface::Saver if app.get_local_mode() => app.get_local_now_glow(),
        Surface::Saver => app.get_glow(),
    };
    // The now-playing views keep the centre column calm; the screensaver is all lava.
    let calm = match s {
        Surface::Local => app.get_local_now_open(),
        Surface::Native => true,
        Surface::Saver => false,
    };
    let geom = match s {
        Surface::Local => Some((app.get_local_glow_x(), app.get_local_glow_y(), app.get_local_glow_d())),
        Surface::Native => Some((app.get_np_glow_x(), app.get_np_glow_y(), app.get_np_glow_d())),
        Surface::Saver => None,
    };
    let under = match geom {
        Some((gx, gy, gd)) if calm && gd * scale > 1.0 => {
            let (gx, gy, gd) = (gx * scale, gy * scale, gd * scale);
            let key = (w, h, gx as i32, gy as i32, gd as i32, u32::from_be_bytes([cover.red(), cover.green(), cover.blue(), cover.alpha()]));
            let slot = &mut bg.glow[s as usize];
            if slot.as_ref().map(|g| g.0) != Some(key) {
                *slot = Some((key, glow_layer(w, h, gx, gy, gd, cover)));
            }
            slot.as_ref().map(|g| g.1.as_slice())
        }
        _ => None,
    };
    let baked = under.is_some();
    // The screensaver runs on its own stretch of the clock, so it isn't the view's frame.
    let (t, levels) = if s == Surface::Saver { (t + 97.0, SAVER) } else { (t, BACKDROP) };
    let buf = render(bg.atlas.as_ref().unwrap(), seed, w, h, t, gain, levels, accent, rgb(cover), calm, under);
    let img = Image::from_rgba8_premultiplied(buf);
    match s {
        Surface::Local => {
            app.set_local_bg(img);
            app.set_local_glow_baked(baked);
        }
        Surface::Native => {
            app.set_np_bg(img);
            app.set_np_glow_baked(baked);
        }
        Surface::Saver => app.set_saver_bg(img),
    }
}

/// The now-playing view's glow (`@radial-gradient(circle, glow 0%, transparent 60%)`
/// on a `d`-wide square centred at (cx, cy)), premultiplied, in physical px.
fn glow_layer(w: usize, h: usize, cx: f32, cy: f32, d: f32, c: slint::Color) -> Vec<Rgba8Pixel> {
    let mut px = vec![Rgba8Pixel { r: 0, g: 0, b: 0, a: 0 }; w * h];
    let reach = d / 2.0 * GLOW_STOP;
    let a0 = c.alpha() as f32 / 255.0;
    let (y0, y1) = ((cy - reach).max(0.0) as usize, ((cy + reach).ceil() as usize).min(h));
    let (x0, x1) = ((cx - reach).max(0.0) as usize, ((cx + reach).ceil() as usize).min(w));
    for y in y0..y1 {
        let dy = y as f32 + 0.5 - cy;
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let s = (dx * dx + dy * dy).sqrt() / reach;
            if s >= 1.0 {
                continue;
            }
            let a = a0 * glow_falloff(s);
            px[y * w + x] = Rgba8Pixel {
                r: (c.red() as f32 * a) as u8,
                g: (c.green() as f32 * a) as u8,
                b: (c.blue() as f32 * a) as u8,
                a: (a * 255.0) as u8,
            };
        }
    }
    px
}

fn rgb(c: slint::Color) -> [f32; 3] {
    [c.red() as f32, c.green() as f32, c.blue() as f32]
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The lava field at (x, y) — y in 0..1 down the panel, x in the same units across
/// it — at time t. Above ~1 is inside a blob.
fn field(x: f32, y: f32, aspect: f32, t: f32) -> f32 {
    // A slow warp so blob edges wobble and pinch instead of staying round.
    let wx = x + 0.035 * (y * 7.0 + t * 0.55).sin() + 0.02 * (y * 13.0 - t * 0.8).sin();
    let wy = y + 0.035 * (x * 6.0 + t * 0.45).sin();
    let mut f = 0.0;
    for i in 0..7 {
        let k = i as f32;
        // Mostly vertical travel, like a lava lamp; each blob on its own slow beat.
        let bx = aspect * (0.5 + 0.44 * (t * (0.07 + 0.013 * k) + k * 2.1).sin());
        let by = 0.5 + 0.62 * (t * (0.11 + 0.017 * k) + k * 1.3).sin();
        let r = 0.11 + 0.035 * (t * 0.23 + k * 0.9).sin() + 0.02 * (k * 1.7).sin();
        let (dx, dy) = (wx - bx, wy - by);
        f += r * r / (dx * dx + dy * dy + 1e-4);
    }
    f
}

/// The wall's character for a cell and roll.
fn wall_char(seed: u32, c: u32, r: u32, roll: u32) -> u8 {
    let total: u32 = WALL.iter().map(|(_, w)| w).sum();
    let h = hash(c ^ seed, r.wrapping_add(seed.rotate_left(13)), roll);
    let mut pick = h % total;
    for (set, wt) in WALL {
        if pick < *wt {
            return set[(h / total) as usize % set.len()];
        }
        pick -= wt;
    }
    b' '
}

fn hash(a: u32, b: u32, c: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9e37_79b1) ^ b.wrapping_mul(0x85eb_ca77) ^ c.wrapping_mul(0xc2b2_ae3d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^ (h >> 12)
}

#[allow(clippy::too_many_arguments)]
/// `calm_centre`: the now-playing view — the centre column (cover, title, controls)
/// keeps the wall but only a third of the lava, so the controls stay clean. `under`: a
/// premultiplied layer the text is drawn over (the glow). The result is premultiplied.
fn render(
    a: &Atlas,
    seed: u32,
    w: usize,
    h: usize,
    t: f32,
    gain: f32,
    lv: Levels,
    accent: [f32; 3],
    glow: [f32; 3],
    calm_centre: bool,
    under: Option<&[Rgba8Pixel]>,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
    let px = buf.make_mut_slice();
    if let Some(u) = under.filter(|u| u.len() == px.len()) {
        px.copy_from_slice(u);
    }
    let (cols, rows) = (w / a.cw + 1, h / a.ch + 1);
    let aspect = w as f32 / h as f32;
    // Soft edges: the wall fades out over the last ~40 px top and bottom, ~24 px sides.
    let (edge_y, edge_x) = (a.ch as f32 * 2.5, a.cw as f32 * 3.5);
    for r in 0..rows {
        let y = (r * a.ch + a.ch / 2) as f32 / h as f32;
        for c in 0..cols {
            let x = (c * a.cw + a.cw / 2) as f32 / h as f32;
            let f = field(x, y, aspect, t);
            let mut v = smoothstep(0.55, 1.6, f);
            if calm_centre {
                let off = (c as f32 * a.cw as f32 / w as f32 - 0.5).abs();
                v *= 0.33 + 0.67 * smoothstep(0.2, 0.36, off);
            }
            let (cx, cy) = ((c * a.cw) as f32, (r * a.ch) as f32);
            let edge = smoothstep(0.0, edge_y, cy.min(h as f32 - cy - a.ch as f32))
                * smoothstep(0.0, edge_x, cx.min(w as f32 - cx - a.cw as f32));
            let alpha = gain * edge * (lv.base + lv.peak * v);
            if alpha * 255.0 < 2.0 {
                continue;
            }
            // Each cell re-rolls on its own clock, offset by its own phase; in the
            // lava's core the clock runs fast, so the text there churns.
            let phase = (hash(c as u32, r as u32, seed) % 1000) as f32 / 1000.0;
            let roll = if v > 0.6 {
                let rate = CHURN * (0.35 + 0.65 * smoothstep(0.6, 1.0, v)) * (0.7 + 0.6 * phase);
                0x8000_0000 | (t * rate + phase * 7.0) as u32
            } else {
                (t * REROLL * (0.6 + 0.8 * phase) + phase * 7.0) as u32
            };
            let ch = wall_char(seed, c as u32, r as u32, roll);
            if ch == b' ' {
                continue;
            }
            let Some(Some(g)) = a.glyphs.get(ch.wrapping_sub(0x20) as usize) else { continue };
            // Accent at the top fading to the cover's colour at the bottom, lifted
            // towards white in the cores.
            let lift = v * 0.15;
            let col: [u8; 3] = std::array::from_fn(|k| {
                let base = accent[k] + (glow[k] - accent[k]) * y.clamp(0.0, 1.0) * 0.7;
                (base + (255.0 - base) * lift) as u8
            });
            let (x0, y0) = ((c * a.cw) as i32 + g.ox, (r * a.ch) as i32 + g.oy);
            for gy in 0..g.h {
                let py = y0 + gy as i32;
                if py < 0 || py >= h as i32 {
                    continue;
                }
                let row = &g.cov[gy * g.w..(gy + 1) * g.w];
                for (gx, &cv) in row.iter().enumerate() {
                    let pxx = x0 + gx as i32;
                    if cv == 0 || pxx < 0 || pxx >= w as i32 {
                        continue;
                    }
                    // Source-over, premultiplied.
                    let sa = cv as f32 * alpha / 255.0;
                    let keep = 1.0 - sa;
                    let p = &mut px[py as usize * w + pxx as usize];
                    *p = Rgba8Pixel {
                        r: (col[0] as f32 * sa + p.r as f32 * keep) as u8,
                        g: (col[1] as f32 * sa + p.g as f32 * keep) as u8,
                        b: (col[2] as f32 * sa + p.b as f32 * keep) as u8,
                        a: (255.0 * sa + p.a as f32 * keep) as u8,
                    };
                }
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_is_varied_and_does_not_repeat() {
        let line = |r: u32| (0..200).map(|c| wall_char(42, c, r, 0)).collect::<Vec<u8>>();
        let (a, b) = (line(0), line(1));
        assert_ne!(a, b, "lines differ");
        // No short period: no 6-char chunk recurs within a line.
        let most = a.windows(6).map(|w| a.windows(6).filter(|x| *x == w).count()).max().unwrap();
        assert!(most <= 1, "a chunk repeats {most} times");
        let distinct = a.iter().collect::<std::collections::BTreeSet<_>>().len();
        assert!(distinct > 40, "only {distinct} distinct characters");
        assert!(a.iter().all(|c| c.is_ascii_graphic() || *c == b' '));
        // The same roll is stable; a new roll changes most characters.
        assert_eq!(line(0), a);
        let rolled = (0..200).filter(|&c| wall_char(42, c, 0, 1) != a[c as usize]).count();
        assert!(rolled > 150, "only {rolled} changed on a re-roll");
    }

    #[test]
    fn frame_has_faint_wall_and_bright_blobs() {
        let a = atlas(1.0);
        let buf = render(&a, 42, 400, 300, 3.0, 1.0, BACKDROP, [52.0, 211.0, 153.0], [200.0, 80.0, 40.0], false, None);
        let alphas: Vec<u8> = buf.as_slice().iter().map(|p| p.a).collect();
        let max = *alphas.iter().max().unwrap();
        let lit = alphas.iter().filter(|&&a| a > 0).count();
        assert!(max as f32 > 255.0 * (BACKDROP.base + BACKDROP.peak * 0.7), "a blob core is bright (max {max})");
        assert!(lit > 400 * 300 / 20, "the wall covers the panel ({lit} lit px)");
        // Faded out: nothing drawn.
        let none = render(&a, 42, 400, 300, 3.0, 0.0, BACKDROP, [0.0; 3], [0.0; 3], false, None);
        assert!(none.as_slice().iter().all(|p| p.a == 0));
    }
}
