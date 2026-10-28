// Remote control, viewer side (v0.11.0): collects the viewer's gamepads (XInput), keyboard and mouse and sends
// them to the host in small, numbered packets (inputlogic::Sender); applies the host's force feedback to the
// viewer's pad. Runs its own high-priority thread; the window thread only hands over events.
// NOT tested with real hardware by the author.
use crate::inputlogic::{self, assign_slots, RumbleRx, Sender};
use crate::inputproto::{self, CtrlInfo, PadState, MAX_PADS};
use crate::net;
use crate::xinput::XInput;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the host's PT_CTRL stays valid (it sends one every second).
const CTRL_TTL: Duration = Duration::from_secs(4);
/// A pad plugged in later is found within RESCAN. (Reading pace: inputlogic::pad_poll_gap.) Packets go out only on change plus the 100 ms refresh.
/// A pad plugged in later is found within this time (asking for missing pads is slow in XInput).
const RESCAN: Duration = Duration::from_millis(1000);

pub struct ViewInput {
    sender: Mutex<Sender>,
    /// The "Управление" switch in the bar.
    pub want: AtomicBool,
    /// The mouse is captured (set by the window thread; the thread uses it to know when keys count).
    pub captured: AtomicBool,
    ctrl: Mutex<Option<(CtrlInfo, Instant)>>,
    /// Live state of the viewer's own controllers (XInput slots 0..3), also for the test panel.
    pub pads: Mutex<[Option<PadState>; MAX_PADS]>,
    /// Viewer controller (XInput slot) behind each virtual pad on the host.
    slots: Mutex<[Option<usize>; MAX_PADS]>,
    rumble: Mutex<RumbleRx>,
    xi: Option<XInput>,
    /// The host took control away (shown once in the window).
    pub revoked_at: Mutex<Option<Instant>>,
    /// The host turned down our request for control (shown once in the window).
    pub denied_at: Mutex<Option<Instant>>,
    /// Laser pointer and reactions (work without remote control).
    annot: Mutex<crate::annot::Sender>,
}

impl ViewInput {
    pub fn new() -> Arc<ViewInput> {
        let start = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_micros() as u16)) | 1;
        Arc::new(ViewInput {
            sender: Mutex::new(Sender::new(start)),
            want: AtomicBool::new(false),
            captured: AtomicBool::new(false),
            ctrl: Mutex::new(None),
            pads: Mutex::new([None; MAX_PADS]),
            slots: Mutex::new([None; MAX_PADS]),
            rumble: Mutex::new(RumbleRx::new()),
            xi: XInput::load(),
            revoked_at: Mutex::new(None),
            denied_at: Mutex::new(None),
            annot: Mutex::new(crate::annot::Sender::new(start)),
        })
    }

    /// What the host last said about permissions (None: an older host, or silent for a while).
    pub fn host_info(&self) -> Option<CtrlInfo> {
        self.ctrl.lock().unwrap().filter(|(_, t)| t.elapsed() < CTRL_TTL).map(|(c, _)| c)
    }

    pub fn on_ctrl(&self, body: &[u8]) {
        let Some(c) = inputproto::decode_ctrl(body) else { return };
        let mut g = self.ctrl.lock().unwrap();
        if g.is_some_and(|(old, t)| old.allowed && !c.allowed && t.elapsed() < CTRL_TTL) && self.want.load(Ordering::Relaxed) {
            // The host took control away: let go of everything and show it.
            *self.revoked_at.lock().unwrap() = Some(Instant::now());
            self.want.store(false, Ordering::Relaxed);
        }
        if c.denied && self.want.load(Ordering::Relaxed) && !c.allowed {
            // The host said no: stop asking and show it.
            *self.denied_at.lock().unwrap() = Some(Instant::now());
            self.want.store(false, Ordering::Relaxed);
        }
        *g = Some((c, Instant::now()));
    }

    pub fn on_rumble(&self, body: &[u8]) {
        let Some(r) = inputproto::decode_rumble(body) else { return };
        if !self.pad_active() {
            return;
        }
        let set = self.rumble.lock().unwrap().on_packet(Instant::now(), &r);
        if let Some((idx, l, s)) = set {
            self.vibrate_virtual(idx, l, s);
        }
    }

    fn vibrate_virtual(&self, idx: usize, l: u8, s: u8) {
        let slot = self.slots.lock().unwrap()[idx];
        if let (Some(xi), Some(slot)) = (&self.xi, slot) {
            xi.vibrate(slot, l, s);
        }
    }

    /// Which viewer controller feeds which virtual pad (for the test panel).
    pub fn slots_view(&self) -> std::sync::MutexGuard<'_, [Option<usize>; MAX_PADS]> {
        self.slots.lock().unwrap()
    }

    pub fn kbm_active(&self) -> bool {
        self.want.load(Ordering::Relaxed) && self.host_info().is_some_and(|c| c.allowed && c.kbm)
    }

    pub fn pad_active(&self) -> bool {
        self.want.load(Ordering::Relaxed) && self.host_info().is_some_and(|c| c.allowed && c.pad)
    }

    /// Number of gamepads connected to this PC.
    pub fn pad_count(&self) -> usize {
        self.pads.lock().unwrap().iter().flatten().count()
    }

    pub fn key(&self, code: u16, down: bool, repeat: bool) {
        if down && !self.kbm_active() {
            return;
        }
        self.sender.lock().unwrap().key(code, down, repeat);
    }

    pub fn mouse_button(&self, b: u8, down: bool) {
        if down && !self.kbm_active() {
            return;
        }
        self.sender.lock().unwrap().mouse_button(b, down);
    }

    pub fn mouse_move(&self, dx: i32, dy: i32) {
        if self.kbm_active() {
            self.sender.lock().unwrap().mouse_move(dx, dy);
        }
    }

    /// The pointer's spot on the picture (0..=65535 both ways), None when it should not count.
    pub fn mouse_abs(&self, pos: Option<(u16, u16)>) {
        self.sender.lock().unwrap().mouse_abs(if self.kbm_active() { pos } else { None });
    }

    /// The host's cursor is hidden (a game holds the mouse): raw motion only, our pointer hidden too.
    pub fn host_relative(&self) -> bool {
        self.host_info().is_some_and(|c| c.relative)
    }

    pub fn wheel(&self, v: i32, h: i32) {
        if self.kbm_active() {
            self.sender.lock().unwrap().wheel(v, h);
        }
    }

    /// Keys and mouse buttons are let go (the mouse was released, the window lost focus...). Pads stay.
    pub fn release_kbm(&self) {
        self.sender.lock().unwrap().release_kbm();
    }

    /// Where the laser points on the picture (0..=65535 both ways); None = off.
    pub fn laser(&self, pos: Option<(u16, u16)>) {
        self.annot.lock().unwrap().set_laser(pos, Instant::now());
    }

    /// The laser is on and the mouse moved in the last 3 s (otherwise it is dark on both sides).
    pub fn laser_visible(&self) -> bool {
        self.annot.lock().unwrap().laser_visible(Instant::now()).is_some()
    }

    /// A sticker for the host's screen, at a spot of the picture (None: where the laser is, or a corner).
    pub fn react(&self, sticker: u8, at: Option<(u16, u16)>) {
        self.annot.lock().unwrap().react(sticker, at);
    }

    pub fn spawn(self: &Arc<Self>, sock: Arc<UdpSocket>, host: SocketAddr, token: u32, alive: impl Fn() -> bool + Send + 'static) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("input-view".into())
            .spawn(move || {
                unsafe {
                    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                }
                let mut out = Vec::with_capacity(512);
                let mut last_poll = Instant::now() - Duration::from_secs(1);
                let mut busy = false; // sending, or saying goodbye
                let mut last_scan = Instant::now() - RESCAN;
                let mut known = [false; MAX_PADS];
                let mut was_active = false;
                let mut linger_until: Option<Instant> = None;
                let mut last_ask: Option<Instant> = None;
                while alive() {
                    let now = Instant::now();
                    // 1. gamepads
                    if let Some(xi) = &me.xi {
                        if now.duration_since(last_poll) >= inputlogic::pad_poll_gap(busy) {
                            last_poll = now;
                            let scan = now.duration_since(last_scan) >= RESCAN;
                            if scan {
                                last_scan = now;
                            }
                            let mut st = [None; MAX_PADS];
                            for (i, s) in st.iter_mut().enumerate() {
                                if known[i] || scan {
                                    *s = xi.state(i);
                                    known[i] = s.is_some();
                                }
                            }
                            *me.pads.lock().unwrap() = st;
                            let slots = assign_slots([st[0].is_some(), st[1].is_some(), st[2].is_some(), st[3].is_some()]);
                            *me.slots.lock().unwrap() = slots;
                            let pad_on = me.pad_active();
                            let mut s = me.sender.lock().unwrap();
                            for (v, slot) in slots.iter().enumerate() {
                                s.set_pad(v, if pad_on { slot.and_then(|sl| st[sl]) } else { None });
                            }
                        }
                    }
                    // 2. switched on / off
                    let active = me.pad_active() || me.kbm_active();
                    if was_active && !active {
                        me.sender.lock().unwrap().release_all(true);
                        for i in me.rumble.lock().unwrap().stop_all() {
                            me.vibrate_virtual(i, 0, 0);
                        }
                        // Say it a few times: the host would notice by itself after 1 s of silence.
                        linger_until = Some(now + Duration::from_millis(300));
                    }
                    was_active = active;
                    // 3. send
                    let sending = active || linger_until.is_some_and(|t| now < t);
                    busy = sending;
                    if sending {
                        let f = me.sender.lock().unwrap().poll(now);
                        if let Some(f) = f {
                            net::write_header(&mut out, net::PT_INPUT, token);
                            inputproto::encode(&f, &mut out);
                            let _ = sock.send_to(&out, host);
                        }
                    }
                    let (a, pointing) = {
                        let mut s = me.annot.lock().unwrap();
                        (s.poll(now), s.laser_visible(now).is_some())
                    };
                    // The laser moves: wake often, like for remote control (the idle pause is ~15 ms).
                    busy |= pointing;
                    if let Some(a) = a {
                        net::write_header(&mut out, net::PT_ANNOT, token);
                        crate::annot::encode(&a, &mut out);
                        let _ = sock.send_to(&out, host);
                    }
                    // 4. asking the host for control (it shows the owner a notice), once a second while waiting
                    let asking = me.want.load(Ordering::Relaxed) && me.host_info().is_some_and(|c| !c.allowed && !c.denied);
                    if asking && last_ask.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) || !asking && last_ask.is_some() {
                        net::write_header(&mut out, net::PT_CTLREQ, token);
                        out.extend_from_slice(&[1, asking as u8]);
                        let _ = sock.send_to(&out, host);
                        last_ask = asking.then_some(now);
                    }
                    // 5. motors stop by themselves when the host goes quiet
                    for i in me.rumble.lock().unwrap().tick(now) {
                        me.vibrate_virtual(i, 0, 0);
                    }
                    std::thread::sleep(inputlogic::view_pause(busy));
                }
                me.sender.lock().unwrap().release_all(true);
                for i in me.rumble.lock().unwrap().stop_all() {
                    me.vibrate_virtual(i, 0, 0);
                }
            })
            .ok();
    }
}
