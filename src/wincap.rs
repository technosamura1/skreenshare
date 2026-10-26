// Single-window capture with Windows Graphics Capture (Windows 10 1903+). Frames arrive on a free-threaded
// frame pool, are copied to CPU memory (BGRA) and handed out in the same way as `capture::Capturer`, so the
// rest of the pipeline (scale, encode, send) does not care where the picture comes from.
// WGC also draws the mouse cursor into the window picture for us.
// (Approach and code adapted from the projector project's wincap.rs.)
use crate::capture::Event;
use crate::source::WinEntry;
use anyhow::{anyhow, bail, Context, Result};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use windows::core::{factory, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::core::HSTRING;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::core::BOOL;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::Foundation::CloseHandle;
use windows::core::PWSTR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::*;

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

/// True when WGC can be used on this Windows version.
pub fn supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

pub fn is_alive(h: isize) -> bool {
    unsafe { IsWindow(Some(hwnd(h))).as_bool() }
}

pub fn title_of(h: isize) -> String {
    unsafe {
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd(h), &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// Full path of the program a process runs (None when it can't be opened, e.g. an elevated process).
fn exe_of(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// The process whose sound belongs to window `h`: its owner, except for Store (UWP) apps, whose frame window belongs
/// to ApplicationFrameHost.exe while the app itself (and its sound) lives in another process, found among the
/// frame's child windows. None when the window is gone.
pub fn audio_pid_of(h: isize) -> Option<u32> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd(h), Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let host = exe_of(pid).is_some_and(|e| e.to_ascii_lowercase().ends_with("applicationframehost.exe"));
        if host {
            struct Ctx {
                frame: u32,
                found: u32,
            }
            unsafe extern "system" fn cb(c: HWND, lp: LPARAM) -> BOOL {
                let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
                let mut p = 0u32;
                unsafe { GetWindowThreadProcessId(c, Some(&mut p)) };
                if p != 0 && p != ctx.frame {
                    ctx.found = p;
                    return BOOL(0);
                }
                BOOL(1)
            }
            let mut ctx = Ctx { frame: pid, found: 0 };
            let _ = EnumChildWindows(Some(hwnd(h)), Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
            if ctx.found != 0 {
                return Some(ctx.found);
            }
        }
        Some(pid)
    }
}

/// Top-level windows a person would call "a window": visible, titled, not a tool window / background app,
/// not ours (windows of this program, including a viewer window of the self-test, are skipped). Roughly what
/// Alt+Tab shows.
pub fn list_windows() -> Vec<WinEntry> {
    struct Ctx {
        own_pid: u32,
        own_exe: Option<String>,
        out: Vec<WinEntry>,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
        unsafe {
            if !IsWindowVisible(h).as_bool() {
                return BOOL(1);
            }
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            let owned = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.0.is_null());
            if ex & WS_EX_TOOLWINDOW.0 != 0 || (owned && ex & WS_EX_APPWINDOW.0 == 0) {
                return BOOL(1);
            }
            let mut cloaked = 0u32; // windows on other virtual desktops, hidden UWP shells, ...
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
            if cloaked != 0 {
                return BOOL(1);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid == ctx.own_pid {
                return BOOL(1);
            }
            if let Some(own) = &ctx.own_exe {
                if exe_of(pid).is_some_and(|e| e.eq_ignore_ascii_case(own)) {
                    return BOOL(1);
                }
            }
            let title = title_of(h.0 as isize);
            let title = title.trim().to_string();
            if title.is_empty() || title == "Program Manager" {
                return BOOL(1);
            }
            ctx.out.push(WinEntry { hwnd: h.0 as isize, title, minimized: IsIconic(h).as_bool() });
        }
        BOOL(1)
    }
    let own_pid = std::process::id();
    let mut ctx = Ctx { own_pid, own_exe: exe_of(own_pid), out: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

pub struct WindowCapturer {
    hwnd: isize,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    d3d_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// What became of the yellow capture border (see `BorderState`).
    pub border: BorderState,
    arrived: Arc<(Mutex<bool>, Condvar)>,
    token: i64,
    pool_size: SizeInt32,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Size of the window picture in `pixels` (it changes when the user resizes the window).
    pub width: u32,
    pub height: u32,
    pixels: Vec<u8>,
}

/// The yellow frame Windows draws around a captured window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderState {
    /// We asked Windows to leave it out (Windows 11 / build 20348+). Windows has the last word: for programs
    /// without a package it follows Settings -> Privacy -> Screenshot borders -> «desktop apps».
    Requested,
    /// This Windows has no such switch (Windows 10 before build 20348): the frame can't be removed.
    Unavailable,
}

/// Asks Windows not to draw the capture border: `IsBorderRequired = false`, only if the property exists
/// (`ApiInformation`), after a best-effort `GraphicsCaptureAccess.RequestAccessAsync(Borderless)`.
/// A program without a package can't declare the `graphicsCaptureWithoutBorder` capability, so that request may
/// be refused; it is harmless. The property is then set anyway: Windows 11 lets desktop apps hide the border
/// through its own setting, and ignores the request when that setting is off.
fn remove_border(session: &GraphicsCaptureSession) -> BorderState {
    let present = ApiInformation::IsPropertyPresent(
        &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
        &HSTRING::from("IsBorderRequired"),
    )
    .unwrap_or(false);
    if !present {
        return BorderState::Unavailable;
    }
    // The request can show a consent prompt or stall, so never wait for it more than a moment.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.join());
        let _ = tx.send(r.map(|s| s.0));
    });
    match rx.recv_timeout(Duration::from_millis(1500)) {
        Ok(Ok(st)) => println!("рамка захвата: доступ Borderless = {st}"),
        Ok(Err(e)) => println!("рамка захвата: доступ Borderless не выдан ({e}); пробую без него"),
        Err(_) => println!("рамка захвата: запрос доступа не ответил; пробую без него"),
    }
    match session.SetIsBorderRequired(false) {
        Ok(()) => BorderState::Requested,
        Err(_) => BorderState::Unavailable,
    }
}

impl WindowCapturer {
    pub fn new(h: isize) -> Result<WindowCapturer> {
        if !supported() {
            bail!("эта версия Windows не умеет захватывать отдельные окна (нужна Windows 10 1903 или новее)");
        }
        if !is_alive(h) {
            bail!("окно уже закрыто");
        }
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED); // already initialised on this thread is fine
            if IsIconic(hwnd(h)).as_bool() {
                let _ = ShowWindow(hwnd(h), SW_RESTORE); // a minimized window has no picture to capture
            }
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.ok_or_else(|| anyhow!("нет D3D11 устройства"))?;
            let context = context.ok_or_else(|| anyhow!("нет D3D11 контекста"))?;
            let dxgi: IDXGIDevice = device.cast()?;
            let d3d_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;

            let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem =
                interop.CreateForWindow(hwnd(h)).context("не удалось начать захват этого окна (оно защищено или недоступно)")?;
            let size = item.Size()?;
            let size = SizeInt32 { Width: size.Width.max(2), Height: size.Height.max(2) };
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
            let arrived = Arc::new((Mutex::new(false), Condvar::new()));
            let a2 = arrived.clone();
            let token = pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
                move |_, _| {
                    *a2.0.lock().unwrap() = true;
                    a2.1.notify_all();
                    Ok(())
                },
            ))?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            let border = remove_border(&session);
            session.StartCapture()?;
            Ok(WindowCapturer {
                hwnd: h,
                device,
                context,
                d3d_device,
                pool,
                session,
                border,
                arrived,
                token,
                pool_size: size,
                staging: None,
                width: size.Width as u32,
                height: size.Height as u32,
                pixels: vec![0; size.Width as usize * size.Height as usize * 4],
            })
        }
    }

    /// Waits up to `timeout_ms` for a new window picture.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Event> {
        if !is_alive(self.hwnd) {
            return Ok(Event::Closed);
        }
        let mut got = self.take_latest()?;
        if !got {
            let (lock, cv) = &*self.arrived;
            let g = lock.lock().unwrap();
            let (mut g, _) = cv.wait_timeout_while(g, Duration::from_millis(timeout_ms.min(100) as u64), |f| !*f).unwrap();
            *g = false;
            drop(g);
            got = self.take_latest()?;
        }
        Ok(if got { Event::Updated } else { Event::Timeout })
    }

    /// Takes the newest queued frame (dropping older ones). Returns whether there was one.
    fn take_latest(&mut self) -> Result<bool> {
        let mut last = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if let Some(old) = last.replace(f) {
                let _ = old.Close();
            }
        }
        let Some(frame) = last else { return Ok(false) };
        let r = self.copy_frame(&frame);
        let _ = frame.Close();
        r.map(|_| true)
    }

    fn copy_frame(&mut self, frame: &windows::Graphics::Capture::Direct3D11CaptureFrame) -> Result<()> {
        unsafe {
            let content = frame.ContentSize()?;
            let (cw, ch) = (content.Width.max(2) as u32, content.Height.max(2) as u32);
            // The window was resized: later frames must come in the new size.
            if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
                self.pool_size = SizeInt32 { Width: cw as i32, Height: ch as i32 };
                self.pool.Recreate(&self.d3d_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, self.pool_size)?;
            }
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let tex: ID3D11Texture2D = access.GetInterface()?;
            let mut d = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut d);
            if self.staging.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (d.Width, d.Height)) {
                d.Usage = D3D11_USAGE_STAGING;
                d.BindFlags = 0;
                d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                d.MiscFlags = 0;
                d.MipLevels = 1;
                d.ArraySize = 1;
                let mut st = None;
                self.device.CreateTexture2D(&d, None, Some(&mut st))?;
                self.staging = Some((st.ok_or_else(|| anyhow!("CreateTexture2D"))?, d.Width, d.Height));
            }
            let (st, tw, th) = self.staging.clone().unwrap();
            self.context.CopyResource(&st, &tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let src = std::slice::from_raw_parts(m.pData as *const u8, m.RowPitch as usize * th as usize);
            // Only the top-left `content` part of the texture is the window (the texture may still have the old size).
            let (w, h) = (cw.min(tw), ch.min(th));
            self.width = w;
            self.height = h;
            let row = w as usize * 4;
            self.pixels.resize(row * h as usize, 0);
            for y in 0..h as usize {
                self.pixels[y * row..(y + 1) * row].copy_from_slice(&src[y * m.RowPitch as usize..][..row]);
            }
            self.context.Unmap(&st, 0);
        }
        Ok(())
    }

    /// Whether the captured window still exists.
    pub fn alive(&self) -> bool {
        is_alive(self.hwnd)
    }

    /// The current window picture, tightly packed BGRA (`width`x`height`).
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

impl Drop for WindowCapturer {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

unsafe impl Send for WindowCapturer {}

