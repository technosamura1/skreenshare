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

/// The real thing the receiver drives: SendInput and virtual pads.
struct HostSink {
    client: Option<vigem_client::Client>,
    pads: [Option<Pad>; MAX_PADS],
    rumble: Arc<Mutex<RumbleTx>>,
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
}

impl InputHost {
    pub fn new(perms: Perms) -> Arc<InputHost> {
        let rumble = Arc::new(Mutex::new(RumbleTx::new()));
        Arc::new(InputHost {
            inner: Mutex::new(Inner {
                rx: Receiver::new(perms),
                sink: HostSink { client: None, pads: Default::default(), rumble: rumble.clone() },
            }),
            rumble,
            peer_input: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            vigem: AtomicBool::new(vigem_installed()),
        })
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
        CtrlInfo { allowed: p.allowed, pad: p.pad, kbm: p.kbm, vigem: self.vigem.load(Ordering::Relaxed), pads: g.rx.plugged_count() as u8 }
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
                            CtrlInfo { allowed: p.allowed, pad: p.pad, kbm: p.kbm, vigem: me.vigem.load(Ordering::Relaxed), pads: g.rx.plugged_count() as u8 },
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
