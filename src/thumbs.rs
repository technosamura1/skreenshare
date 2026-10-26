// Thumbnails for the source picker (cards with a small picture of each window / monitor, like Discord's).
// Made with plain GDI: `PrintWindow(PW_RENDERFULLCONTENT)` for windows (works with the DWM-composed ones:
// browsers, Electron, UWP), a screen copy for monitors. Everything happens in ONE background thread that
// exists only while the picker is on screen: at most `MAX_PER_TICK` pictures per tick, each window at most
// every `REFRESH_MS`, pictures are tiny (`THUMB_W`x`THUMB_H`). The GUI thread only copies finished pixels.
// Windows that can't be read (minimized, GPU-only surfaces: black) simply get no picture: the card shows the
// app icon instead. Nothing here touches the capture/encode path.
use crate::guilogic as lg;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    /// Index in the monitor list.
    Monitor(usize),
    Window(isize),
}

/// What to take pictures of. A monitor comes with its rectangle on the desktop.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    Monitor { index: usize, left: i32, top: i32, w: u32, h: u32 },
    Window { hwnd: isize, minimized: bool },
}

impl Target {
    fn key(&self) -> Key {
        match self {
            Target::Monitor { index, .. } => Key::Monitor(*index),
            Target::Window { hwnd, .. } => Key::Window(*hwnd),
        }
    }
}

#[derive(Clone)]
pub struct Picture {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
    /// Grows with every new picture, so the GUI knows what it has already uploaded.
    pub version: u64,
}

#[derive(Default)]
struct Inner {
    targets: Vec<Target>,
    pics: HashMap<Key, Picture>,
    taken: HashMap<Key, u64>,
    icons: HashMap<isize, Option<Picture>>,
}

struct Shared {
    inner: Mutex<Inner>,
    touched: Mutex<Instant>,
    running: AtomicBool,
}

#[derive(Clone)]
pub struct Thumbs {
    sh: Arc<Shared>,
}

impl Thumbs {
    pub fn new() -> Thumbs {
        Thumbs {
            sh: Arc::new(Shared { inner: Mutex::new(Inner::default()), touched: Mutex::new(Instant::now()), running: AtomicBool::new(false) }),
        }
    }

    /// Called every frame the picker is visible: says what is on screen and keeps the worker alive.
    pub fn want(&self, targets: Vec<Target>) {
        {
            let mut i = self.sh.inner.lock().unwrap();
            let keys: Vec<Key> = targets.iter().map(|t| t.key()).collect();
            i.pics.retain(|k, _| keys.contains(k));
            i.taken.retain(|k, _| keys.contains(k));
            i.targets = targets;
        }
        *self.sh.touched.lock().unwrap() = Instant::now();
        if !self.sh.running.swap(true, Ordering::SeqCst) {
            let sh = self.sh.clone();
            std::thread::spawn(move || {
                worker(&sh);
                sh.running.store(false, Ordering::SeqCst);
            });
        }
    }

    /// Pictures newer than what the caller has (`seen`: key -> version).
    pub fn news(&self, seen: &HashMap<Key, u64>) -> Vec<(Key, Picture)> {
        let i = self.sh.inner.lock().unwrap();
        i.pics.iter().filter(|(k, p)| seen.get(k).is_none_or(|v| *v < p.version)).map(|(k, p)| (*k, p.clone())).collect()
    }

    /// App icons (by window) not yet uploaded: `seen` holds the HWNDs already taken.
    pub fn new_icons(&self, seen: &HashMap<isize, bool>) -> Vec<(isize, Option<Picture>)> {
        let i = self.sh.inner.lock().unwrap();
        i.icons.iter().filter(|(h, _)| !seen.contains_key(h)).map(|(h, p)| (*h, p.clone())).collect()
    }
}

fn now_ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

fn worker(sh: &Shared) {
    let t0 = Instant::now();
    let mut version = 0u64;
    loop {
        if sh.touched.lock().unwrap().elapsed() > Duration::from_millis(lg::IDLE_STOP_MS) {
            return;
        }
        let (targets, last): (Vec<Target>, Vec<Option<u64>>) = {
            let i = sh.inner.lock().unwrap();
            let last = i.targets.iter().map(|t| i.taken.get(&t.key()).copied()).collect();
            (i.targets.clone(), last)
        };
        let now = now_ms(t0);
        for idx in lg::pick_batch(&last, now, lg::REFRESH_MS, lg::MAX_PER_TICK) {
            let t = targets[idx];
            let pic = match t {
                Target::Monitor { left, top, w, h, .. } => grab_monitor(left, top, w, h),
                Target::Window { minimized: false, hwnd } => grab_window(hwnd),
                Target::Window { minimized: true, .. } => None,
            };
            let icon_needed = matches!(t, Target::Window { hwnd, .. } if !sh.inner.lock().unwrap().icons.contains_key(&hwnd));
            let icon = if icon_needed { if let Target::Window { hwnd, .. } = t { Some((hwnd, window_icon(hwnd))) } else { None } } else { None };
            let mut i = sh.inner.lock().unwrap();
            i.taken.insert(t.key(), now);
            if let Some(mut p) = pic {
                version += 1;
                p.version = version;
                i.pics.insert(t.key(), p);
            }
            if let Some((h, ic)) = icon {
                i.icons.insert(h, ic);
            }
        }
        std::thread::sleep(Duration::from_millis(lg::TICK_MS));
    }
}

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

fn bitmap_info(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Reads the pixels of a bitmap as opaque RGBA.
unsafe fn read_bitmap(dc: HDC, bmp: HBITMAP, w: u32, h: u32) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    let mut bi = bitmap_info(w as i32, h as i32);
    let n = unsafe { GetDIBits(dc, bmp, 0, h, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS) };
    if n == 0 {
        return None;
    }
    lg::bgra_to_opaque_rgba(&mut buf);
    Some(buf)
}

/// Copies `src` (a `sw`x`sh` area of `src_dc`) into a thumbnail-sized picture.
unsafe fn shrink(screen: HDC, src_dc: HDC, sx: i32, sy: i32, sw: i32, sh: i32, rop: ROP_CODE) -> Option<Picture> {
    let (tw, th) = lg::fit(sw as u32, sh as u32, lg::THUMB_W, lg::THUMB_H);
    unsafe {
        let dc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, tw as i32, th as i32);
        let old = SelectObject(dc, bmp.into());
        let _ = SetStretchBltMode(dc, HALFTONE);
        let _ = SetBrushOrgEx(dc, 0, 0, None);
        let ok = StretchBlt(dc, 0, 0, tw as i32, th as i32, Some(src_dc), sx, sy, sw, sh, rop).as_bool();
        let px = if ok { read_bitmap(dc, bmp, tw, th) } else { None };
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        px.filter(|p| !lg::all_black(p)).map(|rgba| Picture { w: tw as usize, h: th as usize, rgba, version: 0 })
    }
}

fn grab_monitor(left: i32, top: i32, w: u32, h: u32) -> Option<Picture> {
    if w == 0 || h == 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        // CAPTUREBLT: also layered windows (overlays, some browsers).
        let p = shrink(screen, screen, left, top, w as i32, h as i32, ROP_CODE(SRCCOPY.0 | 0x4000_0000));
        ReleaseDC(None, screen);
        p
    }
}

fn grab_window(h: isize) -> Option<Picture> {
    unsafe {
        let hw = hwnd(h);
        if !IsWindow(Some(hw)).as_bool() || IsIconic(hw).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(hw, &mut r).ok()?;
        let (w, hh) = (r.right - r.left, r.bottom - r.top);
        if w < 8 || hh < 8 {
            return None;
        }
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, hh);
        let old = SelectObject(dc, bmp.into());
        // 2 = PW_RENDERFULLCONTENT: asks DWM for the composed picture instead of an empty rectangle.
        let ok = PrintWindow(hw, dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        let pic = if ok { shrink(screen, dc, 0, 0, w, hh, SRCCOPY) } else { None };
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        pic
    }
}

/// The program's icon (32x32 RGBA), taken from the window itself: no extra shell calls.
fn window_icon(h: isize) -> Option<Picture> {
    unsafe {
        let hw = hwnd(h);
        let mut hicon = 0isize;
        for kind in [1usize, 2, 0] {
            // ICON_BIG, ICON_SMALL2, ICON_SMALL; a hung window must not hold us: 80 ms at most.
            let mut res = 0usize;
            let r = SendMessageTimeoutW(hw, WM_GETICON, WPARAM(kind), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, 80, Some(&mut res));
            if r.0 != 0 && res != 0 {
                hicon = res as isize;
                break;
            }
        }
        if hicon == 0 {
            for idx in [GCLP_HICON, GCLP_HICONSM] {
                let v = GetClassLongPtrW(hw, idx);
                if v != 0 {
                    hicon = v as isize;
                    break;
                }
            }
        }
        if hicon == 0 {
            return None;
        }
        render_icon(HICON(hicon as *mut core::ffi::c_void), 32)
    }
}

/// Draws an icon on black and on white; the difference gives the alpha (GDI drawing has no reliable alpha).
unsafe fn render_icon(icon: HICON, size: i32) -> Option<Picture> {
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bi = bitmap_info(size, size);
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0);
        let Ok(dib) = dib else {
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            return None;
        };
        let old = SelectObject(dc, dib.into());
        let n = (size * size) as usize * 4;
        let shot = |fill: u8| -> Vec<u8> {
            std::ptr::write_bytes(bits as *mut u8, fill, n);
            let _ = DrawIconEx(dc, 0, 0, icon, size, size, 0, None, DI_NORMAL);
            std::slice::from_raw_parts(bits as *const u8, n).to_vec()
        };
        let on_black = shot(0);
        let on_white = shot(255);
        SelectObject(dc, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        let mut out = vec![0u8; n];
        for i in (0..n).step_by(4) {
            // For a pixel with alpha a: black-bg = c*a, white-bg = c*a + 255*(1-a).
            let diff = (0..3).map(|c| on_white[i + c] as i32 - on_black[i + c] as i32).sum::<i32>() / 3;
            let a = (255 - diff).clamp(0, 255);
            if a == 0 {
                continue;
            }
            for c in 0..3 {
                out[i + 2 - c] = ((on_black[i + c] as i32 * 255) / a).clamp(0, 255) as u8;
            }
            out[i + 3] = a as u8;
        }
        Some(Picture { w: size as usize, h: size as usize, rgba: out, version: 0 })
    }
}
