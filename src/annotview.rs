// Laser pointer and reactions on the host's screen (v0.11.8). A see-through, click-through, always-on-top window
// over the streamed picture that the capture does not see (WDA_EXCLUDEFROMCAPTURE: the viewer draws their own
// dot, and a captured copy would come back late and doubled). Drawn with Direct2D (stickers.rs frames)
// into a bitmap handed to UpdateLayeredWindow; the window only covers what is drawn, and hides when idle.
use crate::annot::{self, Board};
use crate::inputhost::{self, Region};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct AnnotView {
    board: Arc<Mutex<Board>>,
    stop: Arc<AtomicBool>,
    /// The host's "show the viewer's pointer and reactions" switch.
    pub enabled: Arc<AtomicBool>,
    /// Set by the capture loop after checking exclusion for the selected backend.
    pub capture_safe: Arc<AtomicBool>,
    /// Acknowledgement from the draw thread that the overlay is hidden.
    pub paused: Arc<AtomicBool>,
    pub exclusion: Arc<Mutex<Option<crate::captureexclude::Exclusion>>>,
    got_any: AtomicBool,
}

impl AnnotView {
    /// `input`: the remote-control side, for the «the viewer asks for control» banner (v0.12.8).
    pub fn spawn(region: Arc<Mutex<Option<Region>>>, input: Option<Arc<inputhost::InputHost>>) -> AnnotView {
        let board = Arc::new(Mutex::new(Board::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let enabled = Arc::new(AtomicBool::new(load_enabled()));
        // Fail closed until the capture backend has been selected.
        let capture_safe = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let exclusion = Arc::new(Mutex::new(None));
        let ex = exclusion.clone();
        let (b, s, e, c, p) = (board.clone(), stop.clone(), enabled.clone(), capture_safe.clone(), paused.clone());
        std::thread::Builder::new()
            .name("annot-view".into())
            .spawn(move || {
                if let Err(err) = run(&b, &s, &e, &c, &p, &region, input.as_deref(), &ex) {
                    crate::net::conn_log(&format!("указка: окно не создано: {err}"));
                    c.store(false, Ordering::Release);
                    p.store(true, Ordering::Release);
                }
            })
            .ok();
        AnnotView { board, stop, enabled, capture_safe, paused, exclusion, got_any: AtomicBool::new(false) }
    }

    pub fn on_packet(&self, body: &[u8]) {
        if let Some(f) = annot::decode(body) {
            if !self.got_any.swap(true, Ordering::Relaxed) {
                crate::net::conn_log(&format!("указка/реакции: первый пакет от зрителя, включены={}, разрешён показ={}", self.enabled.load(Ordering::Relaxed), self.capture_safe.load(Ordering::Relaxed)));
            }
            if !self.enabled.load(Ordering::Relaxed) { return; }
            self.board.lock().unwrap().on_frame(Instant::now(), &f);
        }
    }
}

impl Drop for AnnotView {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// ini "show_pointer" (default on).
pub fn load_enabled() -> bool {
    crate::volume::read_settings().lines().all(|l| l.trim() != "show_pointer=0")
}

struct Gfx {
    rt: ID2D1DCRenderTarget,
    text: IDWriteTextFormat,
    dc: HDC,
    bmp: HBITMAP,
    cap: (i32, i32),
}

const PAD: i32 = 8;
const LASER_R: f32 = 16.0;
/// Sticker size on the host's screen, px.
const STICKER: f32 = 132.0;

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// The banner at the top of the streamed screen while the viewer asks for control: the question and two buttons,
/// so it can be answered right there (the same request is also a card in the screenshare window).
const BANNER: &str = "Зритель просит управление";
const BANNER_W: f32 = 760.0;
const BANNER_H: f32 = 54.0;
const BTN_W: f32 = 132.0;
const BTN_H: f32 = 38.0;
const BTN_GAP: f32 = 8.0;

/// The two buttons inside a banner whose left edge is at `left`: («Разрешить», «Отклонить») as (from, to) in x.
fn banner_buttons(left: f32) -> ((f32, f32), (f32, f32)) {
    let right = left + BANNER_W - BTN_GAP;
    ((right - 2.0 * BTN_W - BTN_GAP, right - BTN_W - BTN_GAP), (right - BTN_W, right))
}

/// Which button a click at `x` (from the banner's left edge) lands on: 1 = allow, 2 = deny, 0 = neither.
fn banner_hit(x: f32) -> u8 {
    let (allow, deny) = banner_buttons(0.0);
    if x >= allow.0 && x < allow.1 {
        1
    } else if x >= deny.0 && x < deny.1 {
        2
    } else {
        0
    }
}

/// The last click on the banner (see `banner_hit`), picked up by the drawing loop.
static BANNER_CLICK: AtomicU8 = AtomicU8::new(0);

/// The drawing window lets every click through; the banner's buttons need one that does not. This is an (almost)
/// invisible window laid exactly over the banner that only takes the clicks. It never becomes the active window.
unsafe extern "system" fn click_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_LBUTTONUP {
        let x = (l.0 & 0xFFFF) as i16 as f32;
        let hit = banner_hit(x);
        if hit != 0 {
            BANNER_CLICK.store(hit, Ordering::Relaxed);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn run(
    board: &Mutex<Board>,
    stop: &AtomicBool,
    enabled: &AtomicBool,
    capture_safe: &AtomicBool,
    paused: &AtomicBool,
    region: &Mutex<Option<Region>>,
    input: Option<&inputhost::InputHost>,
    exclusion: &Mutex<Option<crate::captureexclude::Exclusion>>,
) -> Result<()> {
    // Explicit screenshot/debug override. Otherwise the overlay stays hidden if Windows
    // does not actually confirm its exclusion affinity.
    let debug_visible = std::env::var("SS_ANNOT_VISIBLE").is_ok();
    let hwnd = unsafe {
        let inst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: w!("ScreenshareAnnot"), ..Default::default() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("ScreenshareAnnot"),
            w!("screenshare pointer"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        // SS_ANNOT_VISIBLE: screenshots must see the overlay, so it is not excluded at all (only then).
        let excluded = if debug_visible {
            crate::captureexclude::Exclusion { wgc: true, dda: true }
        } else {
            crate::captureexclude::apply(h)
        };
        *exclusion.lock().unwrap() = Some(excluded);
        h
    };
    let clicks = unsafe {
        let inst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(click_proc),
            hInstance: inst.into(),
            lpszClassName: w!("ScreenshareAnnotClicks"),
            hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let h = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("ScreenshareAnnotClicks"),
            w!("screenshare control request"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        // 1/255 opaque: takes clicks, cannot be seen.
        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 1, LWA_ALPHA);
        if !debug_visible {
            let _ = crate::captureexclude::apply(h);
        }
        h
    };
    let mut clicks_shown = false;
    let mut gfx: Option<Gfx> = None;
    let mut shown = false;
    let mut last_drawn: Option<(Option<(i32, i32)>, usize, bool)> = None;
    // The dot glides toward the newest spot (packets come unevenly over the internet); jumps far away snap.
    let mut glide: Option<(f32, f32)> = None;
    let mut last_frame = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        unsafe {
            let mut m = MSG::default();
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&m);
            }
        }
        // «Разрешить» / «Отклонить» pressed on the banner.
        match (BANNER_CLICK.swap(0, Ordering::Relaxed), input) {
            (1, Some(i)) => i.banner_answer(true),
            (2, Some(i)) => i.banner_answer(false),
            _ => {}
        }
        let now = Instant::now();
        let reg = *region.lock().unwrap();
        let asking = input.is_some_and(|i| i.asking(now));
        if !asking && clicks_shown {
            unsafe {
                let _ = ShowWindow(clicks, SW_HIDE);
            }
            clicks_shown = false;
        }
        let mut b = board.lock().unwrap();
        let can_draw = capture_safe.load(Ordering::Acquire);
        let busy = can_draw && ((enabled.load(Ordering::Relaxed) && b.busy(now)) || asking);
        let (Some(reg), true) = (reg, busy) else {
            drop(b);
            if shown {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                shown = false;
                last_drawn = None;
            }
            // Acknowledge only after the window has actually been hidden.
            if !capture_safe.load(Ordering::Acquire) {
                paused.store(true, Ordering::Release);
            }
            std::thread::sleep(Duration::from_millis(15));
            continue;
        };
        let target = enabled.load(Ordering::Relaxed).then(|| b.laser(now)).flatten()
            .and_then(|p| inputhost::region_point(reg, p));
        let dt = now.saturating_duration_since(last_frame).as_secs_f32().min(0.1);
        last_frame = now;
        glide = match (target, glide) {
            (None, _) => None,
            (Some((x, y)), Some((gx, gy))) if (x as f32 - gx).hypot(y as f32 - gy) < 400.0 => {
                let k = 1.0 - (-dt / 0.012).exp();
                Some((gx + (x as f32 - gx) * k, gy + (y as f32 - gy) * k))
            }
            (Some((x, y)), _) => Some((x as f32, y as f32)),
        };
        let laser = glide.map(|(x, y)| (x.round() as i32, y.round() as i32));
        // (sticker, centre, scale, opacity)
        let items: Vec<(usize, (f32, f32), f32, f32, Duration)> = b
            .reactions(now)
            .iter()
            .filter(|_| enabled.load(Ordering::Relaxed))
            .filter_map(|s| {
                let (x, y) = inputhost::region_point(reg, s.at)?;
                let age = now.saturating_duration_since(s.born);
                if s.sticker == annot::PULSE {
                    return Some((s.sticker as usize, (x as f32, y as f32), 1.0, 1.0, age));
                }
                let (rise, scale, alpha) = annot::motion(age);
                Some((s.sticker as usize, (x as f32 + s.lane as f32 * STICKER * 0.7, y as f32 - rise * STICKER), scale, alpha, age))
            })
            .collect();
        drop(b);
        // A still laser and nothing moving: nothing to redraw.
        let key = (laser, items.len(), asking);
        if items.is_empty() && last_drawn == Some(key) {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        last_drawn = Some(key);
        // What the window must cover.
        let mut bb = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        let mut grow = |x: f32, y: f32, r: f32| {
            bb = (bb.0.min(x - r), bb.1.min(y - r), bb.2.max(x + r), bb.3.max(y + r));
        };
        if let Some((x, y)) = laser {
            grow(x as f32, y as f32, LASER_R + 4.0);
        }
        for (i, (x, y), _, _, _) in &items {
            grow(*x, *y, if *i == annot::PULSE as usize { 84.0 } else { STICKER * 0.55 });
        }
        // Top centre of the streamed monitor / window.
        let banner = asking.then(|| inputhost::region_rect(reg)).flatten().map(|(rl, rt, rw, _)| (rl as f32 + rw as f32 / 2.0, rt as f32 + 80.0));
        if let Some((x, y)) = banner {
            grow(x - BANNER_W / 2.0 + BANNER_H / 2.0, y, BANNER_H / 2.0 + 2.0);
            grow(x + BANNER_W / 2.0 - BANNER_H / 2.0, y, BANNER_H / 2.0 + 2.0);
        }
        let (l, t) = (bb.0.floor() as i32 - PAD, bb.1.floor() as i32 - PAD);
        let (w, h) = ((bb.2.ceil() as i32 + PAD - l).max(1), (bb.3.ceil() as i32 + PAD - t).max(1));
        if gfx.as_ref().is_none_or(|g| g.cap.0 < w || g.cap.1 < h) {
            gfx = Some(make_gfx(w.max(256), h.max(256))?);
        }
        let g = gfx.as_ref().unwrap();
        draw(g, (l, t, w, h), laser, &items, banner)?;
        // Capture may have switched while drawing; let the next loop hide/ack instead
        // of revealing another overlay frame after it was disabled.
        if !capture_safe.load(Ordering::Acquire) {
            continue;
        }
        unsafe {
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
            let _ = UpdateLayeredWindow(
                hwnd,
                None,
                Some(&POINT { x: l, y: t }),
                Some(&SIZE { cx: w, cy: h }),
                Some(g.dc),
                Some(&POINT { x: 0, y: 0 }),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            if !shown {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                shown = true;
            }
            // Stay above windows that came to the top since.
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            // The click window sits exactly over the banner, above the drawing.
            if let Some((x, y)) = banner {
                let (bl, bt) = ((x - BANNER_W / 2.0).round() as i32, (y - BANNER_H / 2.0).round() as i32);
                let _ = SetWindowPos(clicks, Some(HWND_TOPMOST), bl, bt, BANNER_W as i32, BANNER_H as i32, SWP_NOACTIVATE | SWP_SHOWWINDOW);
                clicks_shown = true;
            }
        }
        // Next frame with the screen's own refresh (60/144 Hz): a plain sleep is 15.6 ms steps on Windows.
        if unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }.is_err() {
            std::thread::sleep(Duration::from_millis(4));
        }
    }
    unsafe {
        let _ = DestroyWindow(clicks);
        let _ = DestroyWindow(hwnd);
        if let Some(g) = gfx {
            let _ = DeleteDC(g.dc);
            let _ = DeleteObject(g.bmp.into());
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn make_gfx(w: i32, h: i32) -> Result<Gfx> {
    unsafe {
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let fmt = D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED };
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: fmt,
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let text = dw.CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 19.0, w!("ru-ru"))?;
        text.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        text.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        let dc = CreateCompatibleDC(None);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        SelectObject(dc, bmp.into());
        Ok(Gfx { rt, text, dc, bmp, cap: (w, h) })
    }
}

fn draw(g: &Gfx, (l, t, w, h): (i32, i32, i32, i32), laser: Option<(i32, i32)>, items: &[(usize, (f32, f32), f32, f32, Duration)], banner: Option<(f32, f32)>) -> Result<()> {
    let rt = &g.rt;
    unsafe {
        rt.BindDC(g.dc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
        rt.BeginDraw();
        rt.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
        let (ox, oy) = (l as f32, t as f32);
        let bp = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        for (i, (x, y), scale, alpha, age) in items {
            if *i == annot::PULSE as usize {
                // The viewer clicked with the laser: a soft glow and two rings spreading from the spot.
                let (x, y) = (x - ox, y - oy);
                let (rings, (dr, da)) = annot::pulse_rings(*age);
                let brush = rt.CreateSolidColorBrush(&color(1.0, 0.25, 0.2, da), None)?;
                let at = windows_numerics::Vector2 { X: x, Y: y };
                if da > 0.0 {
                    rt.FillEllipse(&D2D1_ELLIPSE { point: at, radiusX: dr, radiusY: dr }, &brush);
                }
                for (r, a) in rings {
                    if a > 0.0 {
                        brush.SetColor(&color(1.0, 0.35, 0.28, a));
                        rt.DrawEllipse(&D2D1_ELLIPSE { point: at, radiusX: r, radiusY: r }, &brush, 4.0, None);
                        brush.SetColor(&color(1.0, 0.75, 0.7, a * 0.6));
                        rt.DrawEllipse(&D2D1_ELLIPSE { point: at, radiusX: r, radiusY: r }, &brush, 1.5, None);
                    }
                }
                continue;
            }
            // The sticker moves: this moment's frame.
            let img = crate::stickers::get(*i, STICKER as i32, *age);
            let size = D2D_SIZE_U { width: img.size as u32, height: img.size as u32 };
            let bmp = rt.CreateBitmap(size, Some(img.px.as_ptr() as *const core::ffi::c_void), img.size as u32 * 4, &bp)?;
            let (x, y) = (x - ox, y - oy);
            let r = STICKER * scale / 2.0;
            let dst = D2D_RECT_F { left: x - r, top: y - r, right: x + r, bottom: y + r };
            rt.DrawBitmap(&bmp, Some(&dst), *alpha, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
        }
        if let Some((x, y)) = banner {
            let (x, y) = (x - ox, y - oy);
            // The launcher's dark card: #2b2d31 with a #3f4147 border, an accent and a neutral button.
            let brush = rt.CreateSolidColorBrush(&color(0.169, 0.176, 0.192, 0.97), None)?;
            let r = D2D_RECT_F { left: x - BANNER_W / 2.0, top: y - BANNER_H / 2.0, right: x + BANNER_W / 2.0, bottom: y + BANNER_H / 2.0 };
            let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: 12.0, radiusY: 12.0 };
            rt.FillRoundedRectangle(&rr, &brush);
            brush.SetColor(&color(0.247, 0.255, 0.278, 1.0));
            rt.DrawRoundedRectangle(&rr, &brush, 1.5, None);
            let (allow, deny) = banner_buttons(r.left);
            let button = |(from, to): (f32, f32)| D2D_RECT_F { left: from, top: y - BTN_H / 2.0, right: to, bottom: y + BTN_H / 2.0 };
            let (ra, rd) = (button(allow), button(deny));
            brush.SetColor(&color(0.290, 0.369, 0.878, 1.0));
            rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: ra, radiusX: 8.0, radiusY: 8.0 }, &brush);
            brush.SetColor(&color(0.251, 0.259, 0.286, 1.0));
            rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rd, radiusX: 8.0, radiusY: 8.0 }, &brush);
            brush.SetColor(&color(0.949, 0.953, 0.961, 1.0));
            let label = |text: &str, rect: &D2D_RECT_F| {
                let txt: Vec<u16> = text.encode_utf16().collect();
                rt.DrawText(&txt, &g.text, rect, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
            };
            label(BANNER, &D2D_RECT_F { left: r.left + 16.0, top: r.top, right: allow.0 - 16.0, bottom: r.bottom });
            brush.SetColor(&color(1.0, 1.0, 1.0, 1.0));
            label("Разрешить", &ra);
            label("Отклонить", &rd);
        }
        if let Some((x, y)) = laser {
            let (x, y) = (x as f32 - ox, y as f32 - oy);
            let brush = rt.CreateSolidColorBrush(&color(1.0, 0.0, 0.0, 1.0), None)?;
            for (r, c) in [(1.0, color(1.0, 0.1, 0.1, 0.22)), (0.62, color(1.0, 0.1, 0.1, 0.5)), (0.38, color(1.0, 0.15, 0.15, 1.0)), (0.14, color(1.0, 0.85, 0.85, 1.0))] {
                brush.SetColor(&c);
                rt.FillEllipse(&D2D1_ELLIPSE { point: windows_numerics::Vector2 { X: x, Y: y }, radiusX: LASER_R * r, radiusY: LASER_R * r }, &brush);
            }
        }
        rt.EndDraw(None, None)?;
    }
    Ok(())
}

/// `annottest`: shows the laser walking across `region`, every sticker once, then a burst of clicks (~7 s).
pub fn demo(region: Region) {
    let v = AnnotView::spawn(Arc::new(Mutex::new(Some(region))), None);
    v.enabled.store(true, Ordering::Relaxed);
    // No capture exists in this standalone visual demo.
    v.capture_safe.store(true, Ordering::Release);
    let mut s = annot::Sender::new(1);
    let t0 = Instant::now();
    let mut next_r = 0;
    let mut pulses = 0u64;
    while t0.elapsed() < Duration::from_secs(7) {
        let now = Instant::now();
        let f = t0.elapsed().as_secs_f32() / 7.0;
        let laser = ((8000.0 + f * 49000.0) as u16, (32768.0 + (f * 12.0).sin() * 12000.0) as u16);
        s.set_laser(Some(laser), now);
        // Laser clicks: the glow, once a second from 1.2 s.
        if t0.elapsed() > Duration::from_millis(1200 + 1000 * pulses) {
            s.pulse(laser, now);
            pulses += 1;
        }
        if next_r < 4 && t0.elapsed() > Duration::from_millis(600 + 700 * next_r as u64) {
            s.react(crate::stickers::FAVORITES[next_r] as u8, None);
            next_r += 1;
        } else if next_r >= 4 && next_r < 14 && t0.elapsed() > Duration::from_millis(3600 + 120 * (next_r - 4) as u64) {
            s.react((next_r % crate::stickers::count()) as u8, Some((20000 + 1500 * (next_r - 4) as u16, 20000)));
            next_r += 1;
        }
        if let Some(fr) = s.poll(now) {
            let mut b = Vec::new();
            annot::encode(&fr, &mut b);
            v.on_packet(&b);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(annot::REACTION_LIFE);
}

#[cfg(test)]
mod banner_tests {
    use super::*;

    #[test]
    fn banner_buttons_sit_inside_the_banner_and_take_their_own_clicks() {
        let (allow, deny) = banner_buttons(0.0);
        assert!(allow.0 > BANNER_W / 2.0, "the question has the left half and more");
        assert!(allow.1 <= deny.0 && deny.1 <= BANNER_W, "side by side, inside the banner");
        assert_eq!(banner_hit((allow.0 + allow.1) / 2.0), 1);
        assert_eq!(banner_hit((deny.0 + deny.1) / 2.0), 2);
        // the text, the gap between the buttons and the right margin answer nothing
        assert_eq!(banner_hit(100.0), 0);
        assert_eq!(banner_hit((allow.1 + deny.0) / 2.0), 0);
        assert_eq!(banner_hit(BANNER_W - 1.0), 0);
        assert!(BTN_H < BANNER_H);
    }
}
