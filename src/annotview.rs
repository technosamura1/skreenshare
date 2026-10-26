// Laser pointer and reactions on the host's screen (v0.11.8). A see-through, click-through, always-on-top window
// over the streamed picture that the capture does not see (WDA_EXCLUDEFROMCAPTURE: the viewer draws their own
// dot, and a captured copy would come back late and doubled). Drawn with Direct2D + DirectWrite (colour emoji)
// into a bitmap handed to UpdateLayeredWindow; the window only covers what is drawn, and hides when idle.
use crate::annot::{self, Board, Reaction};
use crate::inputhost::{self, Region};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F};
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
    got_any: AtomicBool,
}

impl AnnotView {
    pub fn spawn(region: Arc<Mutex<Option<Region>>>) -> AnnotView {
        let board = Arc::new(Mutex::new(Board::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let enabled = Arc::new(AtomicBool::new(load_enabled()));
        let (b, s, e) = (board.clone(), stop.clone(), enabled.clone());
        std::thread::Builder::new()
            .name("annot-view".into())
            .spawn(move || {
                if let Err(err) = run(&b, &s, &e, &region) {
                    crate::net::conn_log(&format!("указка: окно не создано: {err}"));
                }
            })
            .ok();
        AnnotView { board, stop, enabled, got_any: AtomicBool::new(false) }
    }

    pub fn on_packet(&self, body: &[u8]) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if let Some(f) = annot::decode(body) {
            if !self.got_any.swap(true, Ordering::Relaxed) {
                crate::net::conn_log("указка/реакции: первый пакет от зрителя");
            }
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
    emoji: IDWriteTextFormat,
    label: IDWriteTextFormat,
    dc: HDC,
    bmp: HBITMAP,
    cap: (i32, i32),
}

const PAD: i32 = 8;
const LASER_R: f32 = 16.0;
const EMOJI: f32 = 64.0;
const PILL: (f32, f32) = (190.0, 72.0);

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn run(board: &Mutex<Board>, stop: &AtomicBool, enabled: &AtomicBool, region: &Mutex<Option<Region>>) -> Result<()> {
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
        // Not in the stream (Windows 10 2004+); on older systems the viewer just sees it a second time.
        // SS_ANNOT_VISIBLE=1: let screenshots see it (for checking the drawing).
        if std::env::var("SS_ANNOT_VISIBLE").is_err() {
            let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
        }
        h
    };
    let mut gfx: Option<Gfx> = None;
    let mut shown = false;
    let mut last_drawn: Option<(Option<(i32, i32)>, usize)> = None;
    while !stop.load(Ordering::Relaxed) {
        unsafe {
            let mut m = MSG::default();
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&m);
            }
        }
        let now = Instant::now();
        let reg = *region.lock().unwrap();
        let mut b = board.lock().unwrap();
        let busy = enabled.load(Ordering::Relaxed) && b.busy(now);
        let (Some(reg), true) = (reg, busy) else {
            drop(b);
            if shown {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                shown = false;
                last_drawn = None;
            }
            std::thread::sleep(Duration::from_millis(15));
            continue;
        };
        let laser = b.laser(now).and_then(|p| inputhost::region_point(reg, p));
        let items: Vec<(Reaction, (f32, f32), f32)> = b
            .reactions(now)
            .iter()
            .filter_map(|s| {
                let (x, y) = inputhost::region_point(reg, s.at)?;
                let t = now.saturating_duration_since(s.born).as_secs_f32();
                Some((s.kind, (x as f32 + s.lane as f32 * 46.0, y as f32 - 40.0 - t * 70.0), t))
            })
            .collect();
        drop(b);
        // A still laser and nothing moving: nothing to redraw.
        let key = (laser, items.len());
        if items.is_empty() && last_drawn == Some(key) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        last_drawn = Some(key);
        // What the window must cover.
        let mut bb = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        let mut grow = |x: f32, y: f32, rx: f32, ry: f32| {
            bb = (bb.0.min(x - rx), bb.1.min(y - ry), bb.2.max(x + rx), bb.3.max(y + ry));
        };
        if let Some((x, y)) = laser {
            grow(x as f32, y as f32, LASER_R + 4.0, LASER_R + 4.0);
        }
        for (k, (x, y), _) in &items {
            match k {
                Reaction::Laugh | Reaction::Heart => grow(*x, *y, EMOJI * 0.8, EMOJI * 0.8),
                _ => grow(*x, *y, PILL.0 * 0.6, PILL.1 * 0.6),
            }
        }
        let (l, t) = (bb.0.floor() as i32 - PAD, bb.1.floor() as i32 - PAD);
        let (w, h) = ((bb.2.ceil() as i32 + PAD - l).max(1), (bb.3.ceil() as i32 + PAD - t).max(1));
        if gfx.as_ref().is_none_or(|g| g.cap.0 < w || g.cap.1 < h) {
            gfx = Some(make_gfx(w.max(256), h.max(256))?);
        }
        let g = gfx.as_ref().unwrap();
        draw(g, (l, t, w, h), laser, &items)?;
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
        }
        std::thread::sleep(Duration::from_millis(14));
    }
    unsafe {
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
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let fmt = |size: f32| -> Result<IDWriteTextFormat> {
            let f = dw.CreateTextFormat(w!("Segoe UI Emoji"), None, DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("ru-ru"))?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            Ok(f)
        };
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
        Ok(Gfx { rt, emoji: fmt(EMOJI)?, label: fmt(34.0)?, dc, bmp, cap: (w, h) })
    }
}

fn draw(g: &Gfx, (l, t, w, h): (i32, i32, i32, i32), laser: Option<(i32, i32)>, items: &[(Reaction, (f32, f32), f32)]) -> Result<()> {
    let rt = &g.rt;
    unsafe {
        rt.BindDC(g.dc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
        rt.BeginDraw();
        rt.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
        let (ox, oy) = (l as f32, t as f32);
        let brush = rt.CreateSolidColorBrush(&color(1.0, 0.0, 0.0, 1.0), None)?;
        let dot = |x: f32, y: f32, r: f32, c: D2D1_COLOR_F| {
            brush.SetColor(&c);
            rt.FillEllipse(&D2D1_ELLIPSE { point: windows_numerics::Vector2 { X: x, Y: y }, radiusX: r, radiusY: r }, &brush);
        };
        for (kind, (x, y), age) in items {
            let (x, y) = (x - ox, y - oy);
            // Pops in over 0.15 s, fades in the last 0.4 s.
            let life = annot::REACTION_LIFE.as_secs_f32();
            let a = ((life - age) / 0.4).clamp(0.0, 1.0);
            let s = (age / 0.15).clamp(0.3, 1.0);
            rt.SetTransform(&windows_numerics::Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: x - s * x, M32: y - s * y });
            match kind {
                Reaction::Laugh | Reaction::Heart => {
                    dot(x, y, EMOJI * 0.62, color(0.0, 0.0, 0.0, 0.35 * a));
                    let r = EMOJI * 0.75;
                    // Colour glyphs ignore the brush's alpha; a layer fades them.
                    let layer = rt.CreateLayer(None)?;
                    rt.PushLayer(&layer_params(a), &layer);
                    brush.SetColor(&color(1.0, 1.0, 1.0, 1.0));
                    let txt: Vec<u16> = kind.text().encode_utf16().collect();
                    rt.DrawText(&txt, &g.emoji, &D2D_RECT_F { left: x - r, top: y - r, right: x + r, bottom: y + r }, &brush, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, DWRITE_MEASURING_MODE_NATURAL);
                    rt.PopLayer();
                }
                _ => {
                    let (pw, ph) = PILL;
                    let bg = if *kind == Reaction::Yes { color(0.13, 0.62, 0.30, 0.95 * a) } else { color(0.82, 0.16, 0.16, 0.95 * a) };
                    brush.SetColor(&bg);
                    let rr = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: x - pw / 2.0, top: y - ph / 2.0, right: x + pw / 2.0, bottom: y + ph / 2.0 }, radiusX: ph / 2.0, radiusY: ph / 2.0 };
                    rt.FillRoundedRectangle(&rr, &brush);
                    let layer = rt.CreateLayer(None)?;
                    rt.PushLayer(&layer_params(a), &layer);
                    brush.SetColor(&color(1.0, 1.0, 1.0, 1.0));
                    let txt: Vec<u16> = kind.text().encode_utf16().collect();
                    rt.DrawText(&txt, &g.label, &rr.rect, &brush, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, DWRITE_MEASURING_MODE_NATURAL);
                    rt.PopLayer();
                }
            }
            rt.SetTransform(&windows_numerics::Matrix3x2::identity());
        }
        if let Some((x, y)) = laser {
            let (x, y) = (x as f32 - ox, y as f32 - oy);
            dot(x, y, LASER_R, color(1.0, 0.1, 0.1, 0.22));
            dot(x, y, LASER_R * 0.62, color(1.0, 0.1, 0.1, 0.5));
            dot(x, y, LASER_R * 0.38, color(1.0, 0.15, 0.15, 1.0));
            dot(x, y, LASER_R * 0.14, color(1.0, 0.85, 0.85, 1.0));
        }
        rt.EndDraw(None, None)?;
    }
    Ok(())
}

fn layer_params(opacity: f32) -> D2D1_LAYER_PARAMETERS {
    D2D1_LAYER_PARAMETERS {
        contentBounds: D2D_RECT_F { left: -1e6, top: -1e6, right: 1e6, bottom: 1e6 },
        maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
        maskTransform: windows_numerics::Matrix3x2::identity(),
        opacity,
        layerOptions: D2D1_LAYER_OPTIONS_NONE,
        ..Default::default()
    }
}

/// `annottest`: shows the laser walking across `region` and every reaction once (to look at, ~6 s).
pub fn demo(region: Region) {
    let v = AnnotView::spawn(Arc::new(Mutex::new(Some(region))));
    v.enabled.store(true, Ordering::Relaxed);
    let mut s = annot::Sender::new(1);
    let t0 = Instant::now();
    let mut next_r = 0;
    let all = [Reaction::Laugh, Reaction::Heart, Reaction::Yes, Reaction::No];
    while t0.elapsed() < Duration::from_secs(6) {
        let f = t0.elapsed().as_secs_f32() / 6.0;
        s.set_laser(Some(((8000.0 + f * 49000.0) as u16, (32768.0 + (f * 12.0).sin() * 12000.0) as u16)));
        if next_r < all.len() && t0.elapsed() > Duration::from_millis(800 + 900 * next_r as u64) {
            s.react(all[next_r]);
            next_r += 1;
        }
        if let Some(fr) = s.poll(Instant::now()) {
            let mut b = Vec::new();
            annot::encode(&fr, &mut b);
            v.on_packet(&b);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(annot::REACTION_LIFE);
}
