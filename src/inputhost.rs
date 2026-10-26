// Remote control, host side (v0.11.0): turns the viewer's input packets into real keyboard/mouse input (SendInput)
// and virtual Xbox 360 pads (ViGEmBus), and sends force feedback back. The decisions (who may do what, when to let
// go of everything) are in inputlogic.rs; this file is the Windows glue. NOT tested on real Windows by the author.
//
// Input is injected globally (like Parsec does), not into one window: whatever has focus on the host gets it.
use crate::inputlogic::{self, Perms, Receiver, RumbleTx, Sink};
use crate::inputproto::{self, CtrlInfo, PadState, MAX_PADS};
use crate::net;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// Is the ViGEmBus driver there? (Opens and closes the driver's device.)
pub fn vigem_installed() -> bool {
    vigem_client::Client::connect().is_ok()
}

/// Where the user gets the driver.
pub const DRIVER_URL: &str = "https://github.com/nefarius/ViGEmBus/releases/latest";

/// Emergency key on the host: Ctrl+Alt+Shift+F12 takes control away at once.
pub const HOTKEY_TEXT: &str = "Ctrl+Alt+Shift+F12";

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key_input(code: u16, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if code & 0xFF00 == 0xE000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: code & 0xFF, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

struct Pad {
    target: vigem_client::XTarget,
}

/// What the stream shows, so a spot on the viewer's picture can be turned into a spot on this screen.
#[derive(Clone, Copy, Debug)]
pub enum Region {
    /// A monitor fills the picture: its rectangle on the desktop (left, top, width, height).
    Screen { canvas: (u32, u32), rect: (i32, i32, i32, i32) },
    /// One window letterboxed into a monitor-sized picture; the window's place is read when needed (it moves).
    Window { canvas: (u32, u32), content: (u32, u32), hwnd: isize },
}

/// The host's cursor is hidden: a game took the mouse, so only raw motion makes sense.
pub fn cursor_hidden() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};
    let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    unsafe { GetCursorInfo(&mut ci).is_ok() && ci.flags.0 & CURSOR_SHOWING.0 == 0 }
}

/// A window's visible rectangle on the desktop (without the invisible resize borders), in physical pixels.
fn window_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let h = HWND(hwnd as *mut core::ffi::c_void);
    let mut r = RECT::default();
    unsafe {
        let ok = DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut core::ffi::c_void, std::mem::size_of::<RECT>() as u32)
            .is_ok()
            || GetWindowRect(h, &mut r).is_ok();
        (ok && r.right > r.left && r.bottom > r.top).then_some((r.left, r.top, r.right - r.left, r.bottom - r.top))
    }
}

/// Where `region` sits on the desktop now (left, top, width, height): the monitor, or the window as it is.
pub fn region_rect(region: Region) -> Option<(i32, i32, i32, i32)> {
    match region {
        Region::Screen { rect, .. } => Some(rect),
        Region::Window { hwnd, .. } => window_rect(hwnd),
    }
}

/// The desktop point under spot `pos` (0..=65535 both ways) of the picture of `region`.
pub fn region_point(region: Region, pos: (u16, u16)) -> Option<(i32, i32)> {
    let target = region_rect(region)?;
    Some(match region {
        Region::Screen { canvas, .. } => crate::source::picture_to_screen(pos, canvas, None, target),
        Region::Window { canvas, content, .. } => crate::source::picture_to_screen(pos, canvas, Some(content), target),
    })
}

/// Puts the cursor on spot (x, y) of the picture (0..=65535 both ways) of `region`; returns the screen point.
pub fn place_cursor(region: Region, x: u16, y: u16) -> Option<(i32, i32)> {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};
    let (px, py) = region_point(region, (x, y))?;
    // Absolute SendInput speaks 0..65535 over the whole virtual desktop (all monitors, physical pixels).
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let nx = ((px - vx) as f64 * 65535.0 / (vw - 1) as f64).round() as i32;
    let ny = ((py - vy) as f64 * 65535.0 / (vh - 1) as f64).round() as i32;
    send(&[mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, nx, ny, 0)]);
    Some((px, py))
}

/// `abstest`: puts the cursor on the corners and the centre of `region` and reports where it really landed.
pub fn place_cursor_selftest(region: Region) -> Vec<((u16, u16), (i32, i32), (i32, i32))> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut out = Vec::new();
    for spot in [(0, 0), (65535, 0), (32768, 32768), (0, 65535), (65535, 65535)] {
        let Some(want) = place_cursor(region, spot.0, spot.1) else { continue };
        std::thread::sleep(std::time::Duration::from_millis(150));
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        out.push((spot, want, (p.x, p.y)));
    }
    out
}

/// The real thing the receiver drives: SendInput and virtual pads.
struct HostSink {
    client: Option<vigem_client::Client>,
    pads: [Option<Pad>; MAX_PADS],
    rumble: Arc<Mutex<RumbleTx>>,
    region: Arc<Mutex<Option<Region>>>,
}

impl HostSink {
    fn client(&mut self) -> Option<&vigem_client::Client> {
        if self.client.is_none() {
            self.client = vigem_client::Client::connect().ok();
        }
        self.client.as_ref()
    }
}

impl Sink for HostSink {
    fn key(&mut self, code: u16, down: bool) {
        send(&[key_input(code, down)]);
    }

    fn mouse_button(&mut self, b: u8, down: bool) {
        let (flag, data) = match (b, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, true) => (MOUSEEVENTF_XDOWN, 1),
            (3, false) => (MOUSEEVENTF_XUP, 1),
            (4, true) => (MOUSEEVENTF_XDOWN, 2),
            (4, false) => (MOUSEEVENTF_XUP, 2),
            _ => return,
        };
        send(&[mouse(flag, 0, 0, data)]);
    }

    fn mouse_move(&mut self, dx: i32, dy: i32) {
        send(&[mouse(MOUSEEVENTF_MOVE, dx, dy, 0)]);
    }

    fn mouse_abs(&mut self, x: u16, y: u16) -> bool {
        if cursor_hidden() {
            return false;
        }
        let Some(region) = *self.region.lock().unwrap() else { return false };
        place_cursor(region, x, y).is_some()
    }

    fn wheel(&mut self, v: i32, h: i32) {
        if v != 0 {
            send(&[mouse(MOUSEEVENTF_WHEEL, 0, 0, v as u32)]);
        }
        if h != 0 {
            send(&[mouse(MOUSEEVENTF_HWHEEL, 0, 0, h as u32)]);
        }
    }

    fn pad_plug(&mut self, idx: usize) -> bool {
        let Some(client) = self.client().and_then(|c| c.try_clone().ok()) else { return false };
        let mut target = vigem_client::XTarget::new(client, vigem_client::TargetId::XBOX360_WIRED);
        if target.plugin().is_err() {
            return false;
        }
        let _ = target.wait_ready();
        // Force feedback from the game: remember the motor speeds and let the host's tick thread send them.
        if let Ok(req) = target.request_notification() {
            let rumble = self.rumble.clone();
            req.spawn_thread(move |_, n| {
                if let Ok(mut r) = rumble.lock() {
                    r.set(idx, n.large_motor, n.small_motor);
                }
            });
        }
        self.pads[idx] = Some(Pad { target });
        true
    }

    fn pad_unplug(&mut self, idx: usize) {
        // Dropping the target unplugs it (and ends its notification thread).
        self.pads[idx] = None;
        if let Ok(mut r) = self.rumble.lock() {
            r.set(idx, 0, 0);
        }
    }

    fn pad_state(&mut self, idx: usize, st: &PadState) {
        if let Some(p) = &mut self.pads[idx] {
            let g = vigem_client::XGamepad {
                buttons: vigem_client::XButtons(st.buttons),
                left_trigger: st.lt,
                right_trigger: st.rt,
                thumb_lx: st.lx,
                thumb_ly: st.ly,
                thumb_rx: st.rx,
                thumb_ry: st.ry,
            };
            let _ = p.target.update(&g);
        }
    }
}

struct Inner {
    rx: Receiver,
    sink: HostSink,
}

/// The host's remote-control state: what the owner allowed, the receiver, the virtual pads.
pub struct InputHost {
    inner: Mutex<Inner>,
    rumble: Arc<Mutex<RumbleTx>>,
    /// The viewer announced (PT_CAPS) that it understands remote control.
    peer_input: AtomicBool,
    /// The emergency hotkey took control away since the owner last switched it on.
    revoked: AtomicBool,
    vigem: AtomicBool,
    region: Arc<Mutex<Option<Region>>>,
}

impl InputHost {
    pub fn new(perms: Perms) -> Arc<InputHost> {
        let rumble = Arc::new(Mutex::new(RumbleTx::new()));
        let region = Arc::new(Mutex::new(None));
        Arc::new(InputHost {
            inner: Mutex::new(Inner {
                rx: Receiver::new(perms),
                sink: HostSink { client: None, pads: Default::default(), rumble: rumble.clone(), region: region.clone() },
            }),
            region,
            rumble,
            peer_input: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            vigem: AtomicBool::new(vigem_installed()),
        })
    }

    /// What the stream shows right now (called for every frame: the window may have been resized or switched).
    /// The picture's place, shared (the laser pointer overlay reads it too).
    pub fn region_handle(&self) -> Arc<Mutex<Option<Region>>> {
        self.region.clone()
    }

    pub fn set_region(&self, r: Region) {
        *self.region.lock().unwrap() = Some(r);
    }

    pub fn set_peer_caps(&self, flags: u8) {
        self.peer_input.store(flags & net::CAP_INPUT != 0, Ordering::Relaxed);
    }

    /// A PT_INPUT packet from the connected viewer; applied right here, in the receiving thread.
    pub fn on_input(&self, body: &[u8]) {
        let Some(f) = inputproto::decode(body) else { return };
        let mut g = self.inner.lock().unwrap();
        let Inner { rx, sink } = &mut *g;
        rx.on_frame(Instant::now(), &f, sink);
    }

    pub fn set_perms(&self, p: Perms) {
        let mut g = self.inner.lock().unwrap();
        let Inner { rx, sink } = &mut *g;
        rx.set_perms(p, sink);
    }

    pub fn release_all(&self) {
        let mut g = self.inner.lock().unwrap();
        let Inner { rx, sink } = &mut *g;
        rx.release_all(sink);
    }

    fn ctrl_info(&self) -> CtrlInfo {
        let g = self.inner.lock().unwrap();
        let p = g.rx.perms();
        CtrlInfo { allowed: p.allowed, pad: p.pad, kbm: p.kbm, vigem: self.vigem.load(Ordering::Relaxed), pads: g.rx.plugged_count() as u8, relative: cursor_hidden() }
    }

    /// `CONTROL on=1 pad=1 kbm=1`
    fn apply_request(&self, line: &str) {
        let flag = |k: &str, default: bool| {
            line.split_whitespace().find_map(|w| w.strip_prefix(k)).map(|v| v == "1").unwrap_or(default)
        };
        let cur = self.inner.lock().unwrap().rx.perms();
        let p = Perms { allowed: flag("on=", cur.allowed), pad: flag("pad=", cur.pad), kbm: flag("kbm=", cur.kbm) };
        if p.allowed && !cur.allowed {
            self.revoked.store(false, Ordering::Relaxed);
        }
        self.set_perms(p);
    }

    /// The thread that keeps things safe and talks back: the 1 s silence rule, the emergency hotkey, the owner's
    /// switches from the GUI, the answer packets and force feedback. Stops (letting go of everything) when `alive` says no.
    pub fn spawn(self: &Arc<Self>, sock: Arc<UdpSocket>, peer: SocketAddr, token: u32, alive: impl Fn() -> bool + Send + 'static) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("input-host".into())
            .spawn(move || {
                unsafe {
                    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                }
                let mut out = Vec::with_capacity(32);
                let mut last_ctrl: Option<(CtrlInfo, Instant)> = None;
                let mut last_ui = String::new();
                let mut last_ui_t = Instant::now() - Duration::from_secs(10);
                let mut hot_down = false;
                let mut vigem_t = Instant::now();
                while alive() {
                    let now = Instant::now();
                    if let Some(l) = crate::ui::take_control_request() {
                        me.apply_request(&l);
                    }
                    // Emergency hotkey (edge triggered).
                    let down = hotkey_down();
                    if down && !hot_down {
                        let mut g = me.inner.lock().unwrap();
                        let Inner { rx, sink } = &mut *g;
                        let mut p = rx.perms();
                        if p.allowed {
                            p.allowed = false;
                            rx.set_perms(p, sink);
                            me.revoked.store(true, Ordering::Relaxed);
                            println!("Управление отключено сочетанием {HOTKEY_TEXT}.");
                        }
                    }
                    hot_down = down;
                    {
                        let mut g = me.inner.lock().unwrap();
                        let Inner { rx, sink } = &mut *g;
                        rx.tick(now, sink);
                    }
                    if !me.vigem.load(Ordering::Relaxed) && vigem_t.elapsed() > Duration::from_secs(3) {
                        vigem_t = Instant::now();
                        me.vigem.store(vigem_installed(), Ordering::Relaxed);
                    }
                    // Answer packets: only to viewers that said they understand them.
                    if me.peer_input.load(Ordering::Relaxed) {
                        let info = me.ctrl_info();
                        if last_ctrl.is_none_or(|(c, t)| c != info || now.duration_since(t) >= Duration::from_secs(1)) {
                            net::write_header(&mut out, net::PT_CTRL, token);
                            inputproto::encode_ctrl(&info, &mut out);
                            let _ = sock.send_to(&out, peer);
                            last_ctrl = Some((info, now));
                        }
                        let rs = me.rumble.lock().unwrap().poll(now);
                        for r in rs {
                            net::write_header(&mut out, net::PT_RUMBLE, token);
                            inputproto::encode_rumble(&r, &mut out);
                            let _ = sock.send_to(&out, peer);
                        }
                    }
                    // Status for the GUI: on change, and once a second so a late reader catches up.
                    let (info, active, holds) = {
                        let g = me.inner.lock().unwrap();
                        let p = g.rx.perms();
                        (
                            CtrlInfo { allowed: p.allowed, pad: p.pad, kbm: p.kbm, vigem: me.vigem.load(Ordering::Relaxed), pads: g.rx.plugged_count() as u8, relative: cursor_hidden() },
                            g.rx.controlling(now),
                            g.rx.holds_anything(),
                        )
                    };
                    let line = format!(
                        "on={} pad={} kbm={} vigem={} pads={} active={} revoked={}",
                        info.allowed as u8,
                        info.pad as u8,
                        info.kbm as u8,
                        info.vigem as u8,
                        info.pads,
                        active as u8,
                        me.revoked.load(Ordering::Relaxed) as u8
                    );
                    if line != last_ui || last_ui_t.elapsed() >= Duration::from_secs(1) {
                        crate::ui::ctrl_status(&line);
                        last_ui = line;
                        last_ui_t = Instant::now();
                    }
                    std::thread::sleep(inputlogic::host_pause(info.allowed, holds));
                }
                me.release_all();
            })
            .ok();
    }
}

fn key_is_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000 != 0 }
}

fn hotkey_down() -> bool {
    key_is_down(VK_CONTROL) && key_is_down(VK_MENU) && key_is_down(VK_SHIFT) && key_is_down(VK_F12)
}
