// Remote control (v0.11.0): the logic on both sides, without any Windows calls (so tools/lossim can test it).
//
//   Sender   (viewer): collects keys, mouse and pad states, decides when a packet goes out and what it repeats.
//   Receiver (host):   filters old/duplicate packets, applies permissions, turns state into key/button/pad
//                      transitions for a `Sink` (SendInput + virtual pads in the real program), and lets go of
//                      everything on silence, on revoke and on disconnect.
//   RumbleTx / RumbleRx: force feedback host -> viewer.
use crate::inputproto::{newer, Ev, EvKind, Frame, Kbm, PadState, Rumble, MAX_PADS};
use std::time::{Duration, Instant};

/// Packets are not sent closer than this (a 1000 Hz mouse is folded into 2 ms steps).
pub const MIN_GAP: Duration = Duration::from_millis(2);
/// Events are repeated this far apart, up to EVENT_SENDS times in all.
pub const RESEND_GAP: Duration = Duration::from_millis(8);
pub const EVENT_SENDS: u8 = 3;
/// Full state at least this often even if nothing changed.
pub const REFRESH: Duration = Duration::from_millis(100);
/// The host lets go of everything after this long without a valid packet.
pub const HOST_TIMEOUT: Duration = Duration::from_millis(1000);
/// The viewer stops the motors by itself after this long without a rumble packet.
pub const RUMBLE_TIMEOUT: Duration = Duration::from_millis(600);

pub struct Sender {
    seq: u16,
    ev_seq: u16,
    buttons: u8,
    keys: Vec<u16>,
    dx: i32,
    dy: i32,
    wv: i32,
    wh: i32,
    pads: [Option<PadState>; MAX_PADS],
    /// (event, sends left, last sent)
    events: Vec<(Ev, u8, Option<Instant>)>,
    dirty: bool,
    last_send: Option<Instant>,
}

impl Sender {
    pub fn new(start_seq: u16) -> Sender {
        Sender {
            seq: start_seq,
            ev_seq: 0,
            buttons: 0,
            keys: Vec::new(),
            dx: 0,
            dy: 0,
            wv: 0,
            wh: 0,
            pads: [None; MAX_PADS],
            events: Vec::new(),
            dirty: true,
            last_send: None,
        }
    }

    fn push_event(&mut self, kind: EvKind, code: u16) {
        self.ev_seq = self.ev_seq.wrapping_add(1);
        self.events.push((Ev { seq: self.ev_seq, kind, code }, EVENT_SENDS, None));
        self.dirty = true;
    }

    /// A key went down (`repeat`: the viewer's own keyboard auto-repeat, passed on as another press).
    pub fn key(&mut self, code: u16, down: bool, repeat: bool) {
        if down {
            if self.keys.contains(&code) {
                if repeat {
                    self.push_event(EvKind::KeyDown, code);
                }
                return;
            }
            if self.keys.len() < crate::inputproto::MAX_KEYS {
                self.keys.push(code);
            }
            self.push_event(EvKind::KeyDown, code);
        } else if let Some(i) = self.keys.iter().position(|&c| c == code) {
            self.keys.remove(i);
            self.push_event(EvKind::KeyUp, code);
        }
    }

    pub fn mouse_button(&mut self, b: u8, down: bool) {
        if b > 4 {
            return;
        }
        let bit = 1u8 << b;
        if down == (self.buttons & bit != 0) {
            return;
        }
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        self.push_event(if down { EvKind::MouseDown } else { EvKind::MouseUp }, b as u16);
    }

    pub fn mouse_move(&mut self, dx: i32, dy: i32) {
        if dx != 0 || dy != 0 {
            self.dx = self.dx.wrapping_add(dx);
            self.dy = self.dy.wrapping_add(dy);
            self.dirty = true;
        }
    }

    /// Wheel in 1/120 notch units.
    pub fn wheel(&mut self, v: i32, h: i32) {
        if v != 0 || h != 0 {
            self.wv = self.wv.wrapping_add(v);
            self.wh = self.wh.wrapping_add(h);
            self.dirty = true;
        }
    }

    /// The state of virtual pad `idx`, or None when it is not connected.
    pub fn set_pad(&mut self, idx: usize, st: Option<PadState>) {
        if idx < MAX_PADS && self.pads[idx] != st {
            self.pads[idx] = st;
            self.dirty = true;
        }
    }

    #[allow(dead_code)]
    pub fn pad_count(&self) -> usize {
        self.pads.iter().flatten().count()
    }

    /// Everything the viewer holds down is let go (queued as events too, so the host hears it even if the
    /// snapshot is lost). Pads stay connected with neutral sticks unless `drop_pads`.
    pub fn release_all(&mut self, drop_pads: bool) {
        for c in std::mem::take(&mut self.keys) {
            self.push_event(EvKind::KeyUp, c);
        }
        for b in 0..5u8 {
            self.mouse_button(b, false);
        }
        for p in self.pads.iter_mut() {
            if drop_pads {
                *p = None;
            } else if p.is_some() {
                *p = Some(PadState::default());
            }
        }
        self.dirty = true;
    }

    /// Keys and mouse buttons are let go; pads are left alone.
    pub fn release_kbm(&mut self) {
        for c in std::mem::take(&mut self.keys) {
            self.push_event(EvKind::KeyUp, c);
        }
        for b in 0..5u8 {
            self.mouse_button(b, false);
        }
    }

    #[allow(dead_code)]
    pub fn holds_anything(&self) -> bool {
        !self.keys.is_empty() || self.buttons != 0 || self.pads.iter().flatten().any(|p| *p != PadState::default())
    }

    /// The next packet to send, if one is due.
    pub fn poll(&mut self, now: Instant) -> Option<Frame> {
        let gap = self.last_send.map(|t| now.saturating_duration_since(t));
        let due = match gap {
            None => true,
            Some(g) => {
                (self.dirty && g >= MIN_GAP)
                    || (self.events.iter().any(|(_, left, _)| *left > 0) && g >= RESEND_GAP)
                    || g >= REFRESH
            }
        };
        if !due {
            return None;
        }
        self.seq = self.seq.wrapping_add(1);
        let mut mask = 0u8;
        let mut pads = Vec::new();
        for (i, p) in self.pads.iter().enumerate() {
            if let Some(p) = p {
                mask |= 1 << i;
                pads.push((i as u8, *p));
            }
        }
        let mut evs = Vec::new();
        for (e, left, last) in self.events.iter_mut() {
            evs.push(*e);
            if last.is_none_or(|t| now.saturating_duration_since(t) >= RESEND_GAP / 2) {
                *left = left.saturating_sub(1);
                *last = Some(now);
            }
        }
        self.events.retain(|(_, left, _)| *left > 0);
        self.dirty = false;
        self.last_send = Some(now);
        Some(Frame {
            seq: self.seq,
            pad_mask: mask,
            pads,
            kbm: Some(Kbm { buttons: self.buttons, dx: self.dx, dy: self.dy, wheel_v: self.wv, wheel_h: self.wh, keys: self.keys.clone(), events: evs }),
        })
    }
}

/// What the host does with the input (SendInput and ViGEm in the real program).
pub trait Sink {
    /// Scan code (0xE000 | code for extended keys). A second `down` for a held key is a typematic repeat.
    fn key(&mut self, code: u16, down: bool);
    fn mouse_button(&mut self, b: u8, down: bool);
    fn mouse_move(&mut self, dx: i32, dy: i32);
    fn wheel(&mut self, v: i32, h: i32);
    /// Plug virtual pad `idx` in; false = it could not be (no driver).
    fn pad_plug(&mut self, idx: usize) -> bool;
    fn pad_unplug(&mut self, idx: usize);
    fn pad_state(&mut self, idx: usize, st: &PadState);
}

/// What the host's owner allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Perms {
    pub allowed: bool,
    pub pad: bool,
    pub kbm: bool,
}

impl Perms {
    pub fn kbm_ok(&self) -> bool {
        self.allowed && self.kbm
    }
    pub fn pad_ok(&self) -> bool {
        self.allowed && self.pad
    }
}

pub struct Receiver {
    perms: Perms,
    last_seq: Option<u16>,
    last_ev: Option<u16>,
    base: Option<(i32, i32, i32, i32)>,
    held_keys: Vec<u16>,
    held_buttons: u8,
    plugged: [bool; MAX_PADS],
    pad_last: [Option<PadState>; MAX_PADS],
    plug_retry: [Option<Instant>; MAX_PADS],
    last_rx: Option<Instant>,
    /// The last accepted packet while control was allowed (for the "viewer is controlling" indicator).
    last_active: Option<Instant>,
}

impl Receiver {
    pub fn new(perms: Perms) -> Receiver {
        Receiver {
            perms,
            last_seq: None,
            last_ev: None,
            base: None,
            held_keys: Vec::new(),
            held_buttons: 0,
            plugged: [false; MAX_PADS],
            pad_last: [None; MAX_PADS],
            plug_retry: [None; MAX_PADS],
            last_rx: None,
            last_active: None,
        }
    }

    pub fn perms(&self) -> Perms {
        self.perms
    }

    /// True while an accepted packet arrived within the timeout and control is allowed.
    pub fn controlling(&self, now: Instant) -> bool {
        self.perms.allowed && self.last_active.is_some_and(|t| now.saturating_duration_since(t) < HOST_TIMEOUT)
    }

    pub fn plugged_count(&self) -> usize {
        self.plugged.iter().filter(|p| **p).count()
    }

    pub fn holds_anything(&self) -> bool {
        !self.held_keys.is_empty() || self.held_buttons != 0 || self.plugged.iter().any(|p| *p)
    }

    fn release_kbm(&mut self, sink: &mut dyn Sink) {
        for c in std::mem::take(&mut self.held_keys) {
            sink.key(c, false);
        }
        for b in 0..5u8 {
            if self.held_buttons & (1 << b) != 0 {
                sink.mouse_button(b, false);
            }
        }
        self.held_buttons = 0;
        self.base = None;
    }

    fn release_pads(&mut self, sink: &mut dyn Sink) {
        for i in 0..MAX_PADS {
            if self.plugged[i] {
                sink.pad_state(i, &PadState::default());
                sink.pad_unplug(i);
            }
            self.plugged[i] = false;
            self.pad_last[i] = None;
            self.plug_retry[i] = None;
        }
    }

    /// Lets go of everything: keys, buttons, pads (unplugged) and forgets the packet numbers.
    pub fn release_all(&mut self, sink: &mut dyn Sink) {
        self.release_kbm(sink);
        self.release_pads(sink);
        self.last_seq = None;
        self.last_ev = None;
        self.last_rx = None;
        self.last_active = None;
    }

    /// The owner changed what is allowed; whatever was taken away is let go of on the spot.
    pub fn set_perms(&mut self, p: Perms, sink: &mut dyn Sink) {
        self.perms = p;
        if !p.kbm_ok() {
            self.release_kbm(sink);
        }
        if !p.pad_ok() {
            self.release_pads(sink);
        }
        if !p.allowed {
            self.last_active = None;
        }
    }

    /// Call often (every 10-50 ms): silence for HOST_TIMEOUT lets go of everything. True when it did.
    pub fn tick(&mut self, now: Instant, sink: &mut dyn Sink) -> bool {
        if let Some(t) = self.last_rx {
            if now.saturating_duration_since(t) >= HOST_TIMEOUT {
                let had = self.holds_anything();
                self.release_all(sink);
                return had;
            }
        }
        false
    }

    /// One decoded packet from the connected viewer. True if it was accepted (new, not a duplicate).
    pub fn on_frame(&mut self, now: Instant, f: &Frame, sink: &mut dyn Sink) -> bool {
        if let Some(l) = self.last_seq {
            if !newer(f.seq, l) {
                return false;
            }
        }
        self.last_seq = Some(f.seq);
        self.last_rx = Some(now);
        if self.perms.allowed {
            self.last_active = Some(now);
        }

        // Keyboard and mouse.
        if let Some(k) = &f.kbm {
            if self.perms.kbm_ok() {
                self.apply_kbm(k, sink);
            } else {
                // Not allowed: nothing is injected, but remember the numbers so allowing it later starts clean.
                self.base = Some((k.dx, k.dy, k.wheel_v, k.wheel_h));
                if let Some(m) = k.events.iter().map(|e| e.seq).reduce(|a, b| if newer(b, a) { b } else { a }) {
                    self.last_ev = Some(m);
                }
            }
        }

        // Pads.
        for i in 0..MAX_PADS {
            let want = f.pad_mask & (1 << i) != 0 && self.perms.pad_ok();
            if want {
                if !self.plugged[i] {
                    let retry_ok = self.plug_retry[i].is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(1));
                    if retry_ok {
                        if sink.pad_plug(i) {
                            self.plugged[i] = true;
                            self.plug_retry[i] = None;
                        } else {
                            self.plug_retry[i] = Some(now);
                        }
                    }
                }
                if self.plugged[i] {
                    let st = f.pads.iter().find(|(j, _)| *j as usize == i).map(|(_, s)| *s).unwrap_or_default();
                    if self.pad_last[i] != Some(st) {
                        sink.pad_state(i, &st);
                        self.pad_last[i] = Some(st);
                    }
                }
            } else if self.plugged[i] {
                sink.pad_state(i, &PadState::default());
                sink.pad_unplug(i);
                self.plugged[i] = false;
                self.pad_last[i] = None;
            }
        }
        true
    }

    fn apply_kbm(&mut self, k: &Kbm, sink: &mut dyn Sink) {
        // Numbered events first, oldest first, each only once.
        let mut evs: Vec<&Ev> = k.events.iter().filter(|e| self.last_ev.is_none_or(|l| newer(e.seq, l))).collect();
        evs.sort_by(|a, b| if a.seq == b.seq { std::cmp::Ordering::Equal } else if newer(a.seq, b.seq) { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less });
        for e in &evs {
            match e.kind {
                EvKind::KeyDown => {
                    // Second press of a held key: the viewer's auto-repeat.
                    if !self.held_keys.contains(&e.code) {
                        self.held_keys.push(e.code);
                    }
                    sink.key(e.code, true);
                }
                EvKind::KeyUp => {
                    if let Some(i) = self.held_keys.iter().position(|&c| c == e.code) {
                        self.held_keys.remove(i);
                        sink.key(e.code, false);
                    }
                }
                EvKind::MouseDown if e.code <= 4 => {
                    let bit = 1u8 << e.code;
                    if self.held_buttons & bit == 0 {
                        self.held_buttons |= bit;
                        sink.mouse_button(e.code as u8, true);
                    }
                }
                EvKind::MouseUp if e.code <= 4 => {
                    let bit = 1u8 << e.code;
                    if self.held_buttons & bit != 0 {
                        self.held_buttons &= !bit;
                        sink.mouse_button(e.code as u8, false);
                    }
                }
                _ => {}
            }
            self.last_ev = Some(e.seq);
        }
        // Then the snapshot: what is held on the host must be what the viewer says is held.
        let stale: Vec<u16> = self.held_keys.iter().copied().filter(|c| !k.keys.contains(c)).collect();
        for c in stale {
            self.held_keys.retain(|&x| x != c);
            sink.key(c, false);
        }
        for &c in &k.keys {
            if !self.held_keys.contains(&c) {
                self.held_keys.push(c);
                sink.key(c, true);
            }
        }
        for b in 0..5u8 {
            let bit = 1u8 << b;
            let want = k.buttons & bit != 0;
            let have = self.held_buttons & bit != 0;
            if want != have {
                if want {
                    self.held_buttons |= bit;
                } else {
                    self.held_buttons &= !bit;
                }
                sink.mouse_button(b, want);
            }
        }
        // Motion: running totals, the host applies what is new.
        let now = (k.dx, k.dy, k.wheel_v, k.wheel_h);
        if let Some(b) = self.base {
            let (dx, dy) = (now.0.wrapping_sub(b.0), now.1.wrapping_sub(b.1));
            let (wv, wh) = (now.2.wrapping_sub(b.2), now.3.wrapping_sub(b.3));
            if dx != 0 || dy != 0 {
                sink.mouse_move(dx, dy);
            }
            if wv != 0 || wh != 0 {
                sink.wheel(wv, wh);
            }
        }
        self.base = Some(now);
    }
}

/// Host side of force feedback: sends changes a few times, and keeps telling while a motor runs.
pub struct RumbleTx {
    seq: u16,
    cur: [(u8, u8); MAX_PADS],
    /// Repeats still to send after a change.
    left: [u8; MAX_PADS],
    last: [Option<Instant>; MAX_PADS],
}

/// How often the viewer's input thread looks around: every millisecond only while it sends something (or says
/// goodbye); otherwise 10 ms, because nothing can be pressed and the gamepad list is only for the bar.
pub fn view_pause(sending: bool) -> Duration {
    if sending { Duration::from_millis(1) } else { Duration::from_millis(10) }
}

/// The gamepads are read every 4 ms while they go to the host (250 Hz), every 50 ms otherwise.
pub fn pad_poll_gap(sending: bool) -> Duration {
    if sending { Duration::from_millis(4) } else { Duration::from_millis(50) }
}

/// The host's input thread: 8 ms while control is allowed or something is still held, 25 ms when it is off.
pub fn host_pause(allowed: bool, holds: bool) -> Duration {
    if allowed || holds { Duration::from_millis(8) } else { Duration::from_millis(25) }
}

pub const RUMBLE_REFRESH: Duration = Duration::from_millis(200);
pub const RUMBLE_REPEAT_GAP: Duration = Duration::from_millis(15);

impl RumbleTx {
    pub fn new() -> RumbleTx {
        RumbleTx { seq: 0, cur: [(0, 0); MAX_PADS], left: [0; MAX_PADS], last: [None; MAX_PADS] }
    }

    pub fn set(&mut self, idx: usize, large: u8, small: u8) {
        if idx < MAX_PADS && self.cur[idx] != (large, small) {
            self.cur[idx] = (large, small);
            self.left[idx] = 3;
            self.last[idx] = None;
        }
    }

    pub fn poll(&mut self, now: Instant) -> Vec<Rumble> {
        let mut out = Vec::new();
        for i in 0..MAX_PADS {
            let gap = self.last[i].map(|t| now.saturating_duration_since(t));
            let due = if self.left[i] > 0 {
                gap.is_none_or(|g| g >= RUMBLE_REPEAT_GAP)
            } else {
                self.cur[i] != (0, 0) && gap.is_some_and(|g| g >= RUMBLE_REFRESH)
            };
            if due {
                self.seq = self.seq.wrapping_add(1);
                self.left[i] = self.left[i].saturating_sub(1);
                self.last[i] = Some(now);
                out.push(Rumble { seq: self.seq, idx: i as u8, large: self.cur[i].0, small: self.cur[i].1 });
            }
        }
        out
    }
}

impl Default for RumbleTx {
    fn default() -> Self {
        Self::new()
    }
}

/// Viewer side: applies rumble packets (newest wins) and stops the motors by itself when they go quiet.
pub struct RumbleRx {
    seq: [Option<u16>; MAX_PADS],
    cur: [(u8, u8); MAX_PADS],
    last: [Option<Instant>; MAX_PADS],
}

impl RumbleRx {
    pub fn new() -> RumbleRx {
        RumbleRx { seq: [None; MAX_PADS], cur: [(0, 0); MAX_PADS], last: [None; MAX_PADS] }
    }

    /// Some((idx, large, small)) when the motors must be set to something new.
    pub fn on_packet(&mut self, now: Instant, r: &Rumble) -> Option<(usize, u8, u8)> {
        let i = r.idx as usize;
        if i >= MAX_PADS {
            return None;
        }
        if let Some(s) = self.seq[i] {
            if !newer(r.seq, s) {
                return None;
            }
        }
        self.seq[i] = Some(r.seq);
        self.last[i] = Some(now);
        if self.cur[i] != (r.large, r.small) {
            self.cur[i] = (r.large, r.small);
            return Some((i, r.large, r.small));
        }
        None
    }

    pub fn tick(&mut self, now: Instant) -> Vec<usize> {
        let mut stop = Vec::new();
        for i in 0..MAX_PADS {
            if self.cur[i] != (0, 0) && self.last[i].is_some_and(|t| now.saturating_duration_since(t) >= RUMBLE_TIMEOUT) {
                self.cur[i] = (0, 0);
                stop.push(i);
            }
        }
        stop
    }

    /// Control was switched off: everything stops now.
    pub fn stop_all(&mut self) -> Vec<usize> {
        let mut v = Vec::new();
        for i in 0..MAX_PADS {
            if self.cur[i] != (0, 0) {
                v.push(i);
            }
            self.cur[i] = (0, 0);
            self.seq[i] = None;
        }
        v
    }
}

impl Default for RumbleRx {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps the viewer's physical XInput slots (0..3, whichever are connected) onto virtual pads 0.. in order,
/// so one connected pad is always "pad 1" on the host no matter which slot Windows gave it.
pub fn assign_slots(connected: [bool; 4]) -> [Option<usize>; MAX_PADS] {
    let mut out = [None; MAX_PADS];
    let mut n = 0;
    for (slot, c) in connected.iter().enumerate() {
        if *c {
            out[n] = Some(slot);
            n += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inputproto::{btn, decode, encode};

    #[derive(Default)]
    struct Log {
        ev: Vec<String>,
        /// Real state of the host as the log sees it.
        keys: Vec<u16>,
        buttons: u8,
        plugged: [bool; 4],
        pad: [PadState; 4],
        mouse: (i32, i32),
        wheel: (i32, i32),
        no_driver: bool,
    }

    impl Sink for Log {
        fn key(&mut self, c: u16, down: bool) {
            self.ev.push(format!("key {c:x} {down}"));
            if down {
                if !self.keys.contains(&c) {
                    self.keys.push(c);
                }
            } else {
                self.keys.retain(|&k| k != c);
            }
        }
        fn mouse_button(&mut self, b: u8, down: bool) {
            self.ev.push(format!("btn {b} {down}"));
            if down {
                self.buttons |= 1 << b;
            } else {
                self.buttons &= !(1 << b);
            }
        }
        fn mouse_move(&mut self, dx: i32, dy: i32) {
            self.mouse.0 += dx;
            self.mouse.1 += dy;
        }
        fn wheel(&mut self, v: i32, h: i32) {
            self.wheel.0 += v;
            self.wheel.1 += h;
        }
        fn pad_plug(&mut self, i: usize) -> bool {
            if self.no_driver {
                return false;
            }
            self.plugged[i] = true;
            self.ev.push(format!("plug {i}"));
            true
        }
        fn pad_unplug(&mut self, i: usize) {
            self.plugged[i] = false;
            self.ev.push(format!("unplug {i}"));
        }
        fn pad_state(&mut self, i: usize, st: &PadState) {
            self.pad[i] = *st;
        }
    }

    const ALL: Perms = Perms { allowed: true, pad: true, kbm: true };

    fn t0() -> Instant {
        Instant::now()
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Wire round trip so the tests also cover the codec.
    fn wire(f: &Frame) -> Frame {
        let mut b = Vec::new();
        encode(f, &mut b);
        decode(&b).unwrap()
    }

    #[test]
    fn first_poll_sends_then_waits_for_change_or_refresh() {
        let t = t0();
        let mut s = Sender::new(0);
        assert!(s.poll(t).is_some());
        assert!(s.poll(t + ms(1)).is_none());
        assert!(s.poll(t + ms(50)).is_none());
        assert!(s.poll(t + ms(100)).is_some(), "refresh every 100 ms");
        s.mouse_move(1, 0);
        assert!(s.poll(t + ms(101)).is_none(), "min gap");
        assert!(s.poll(t + ms(103)).is_some());
    }

    #[test]
    fn key_tap_reaches_host_and_does_not_stick() {
        let t = t0();
        let mut s = Sender::new(7);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x1E, true, false);
        s.key(0x1E, false, false);
        let f = s.poll(t + ms(10)).unwrap();
        assert!(r.on_frame(t + ms(10), &wire(&f), &mut log));
        assert_eq!(log.ev, vec!["key 1e true", "key 1e false"]);
        assert!(log.keys.is_empty());
    }

    #[test]
    fn lost_packets_cannot_leave_a_key_stuck() {
        // Every pattern of loss over the packets after the release: the key must always end up released.
        for loss in 0u32..64 {
            let t = t0();
            let mut s = Sender::new(0);
            let mut r = Receiver::new(ALL);
            let mut log = Log::default();
            let mut now = t;
            let deliver = |s: &mut Sender, now: Instant, r: &mut Receiver, log: &mut Log, i: u32, lossy: bool| {
                if let Some(f) = s.poll(now) {
                    if !(lossy && loss & (1 << (i % 6)) != 0) {
                        r.on_frame(now, &wire(&f), log);
                    }
                }
            };
            deliver(&mut s, now, &mut r, &mut log, 0, false);
            s.key(0x11, true, false);
            now += ms(10);
            deliver(&mut s, now, &mut r, &mut log, 1, false);
            assert_eq!(log.keys, vec![0x11]);
            s.key(0x11, false, false);
            // The release is sent through a lossy channel (3 event copies + refreshes).
            for i in 0..6 {
                now += ms(10);
                deliver(&mut s, now, &mut r, &mut log, i, true);
            }
            // Even if all six were lost, the refresh over a clean channel repairs it.
            now += ms(100);
            deliver(&mut s, now, &mut r, &mut log, 0, false);
            assert!(log.keys.is_empty(), "loss pattern {loss:06b}");
        }
    }

    #[test]
    fn lost_key_down_is_repaired_by_the_snapshot() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x1F, true, false);
        let _lost = s.poll(t + ms(10)).unwrap();
        // A later snapshot with the key still held (events may have expired by then).
        let f = s.poll(t + ms(150)).unwrap();
        r.on_frame(t + ms(150), &wire(&f), &mut log);
        assert_eq!(log.keys, vec![0x1F]);
    }

    #[test]
    fn duplicates_and_reordered_packets_are_ignored() {
        let t = t0();
        let mut s = Sender::new(65530);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x1E, true, false);
        let a = s.poll(t + ms(10)).unwrap();
        s.key(0x1E, false, false);
        let b = s.poll(t + ms(20)).unwrap();
        assert!(r.on_frame(t + ms(20), &wire(&b), &mut log) || true);
        // The older packet arrives late: the key must not be pressed again.
        let before = log.ev.len();
        assert!(!r.on_frame(t + ms(21), &wire(&a), &mut log));
        assert_eq!(log.ev.len(), before);
        assert!(!r.on_frame(t + ms(22), &wire(&b), &mut log), "duplicate");
        assert!(log.keys.is_empty());
    }

    #[test]
    fn sequence_numbers_wrap() {
        let t = t0();
        let mut s = Sender::new(65534);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        let mut now = t;
        for i in 0..6 {
            let f = s.poll(now).unwrap();
            assert!(r.on_frame(now, &wire(&f), &mut log), "packet {i}");
            now += ms(100);
        }
    }

    #[test]
    fn key_repeat_is_forwarded_as_repeated_presses() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x20, true, false);
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        s.key(0x20, true, true);
        let f = s.poll(t + ms(500)).unwrap();
        r.on_frame(t + ms(500), &wire(&f), &mut log);
        assert_eq!(log.ev, vec!["key 20 true", "key 20 true"]);
        assert_eq!(log.keys, vec![0x20]);
    }

    #[test]
    fn events_are_repeated_three_times_then_dropped() {
        let t = t0();
        let mut s = Sender::new(0);
        s.poll(t);
        s.key(1, true, false);
        let mut seen = 0;
        let mut now = t;
        for _ in 0..40 {
            now += ms(10);
            if let Some(f) = s.poll(now) {
                if !f.kbm.as_ref().unwrap().events.is_empty() {
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, EVENT_SENDS as usize);
    }

    #[test]
    fn mouse_motion_is_totals_and_survives_loss() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        let f0 = s.poll(t).unwrap();
        r.on_frame(t, &wire(&f0), &mut log);
        s.mouse_move(5, -3);
        let _lost = s.poll(t + ms(5)).unwrap();
        s.mouse_move(2, 2);
        s.wheel(120, 0);
        let f = s.poll(t + ms(10)).unwrap();
        r.on_frame(t + ms(10), &wire(&f), &mut log);
        assert_eq!(log.mouse, (7, -1));
        assert_eq!(log.wheel, (120, 0));
        // Nothing is applied twice by a refresh.
        let f = s.poll(t + ms(110)).unwrap();
        r.on_frame(t + ms(110), &wire(&f), &mut log);
        assert_eq!(log.mouse, (7, -1));
    }

    #[test]
    fn mouse_totals_wrap_around() {
        let t = t0();
        let mut s = Sender::new(0);
        s.dx = i32::MAX - 1;
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        let f = s.poll(t).unwrap();
        r.on_frame(t, &wire(&f), &mut log);
        s.mouse_move(5, 0);
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert_eq!(log.mouse, (5, 0));
    }

    #[test]
    fn mouse_buttons_follow_events_and_snapshot() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.mouse_button(0, true);
        s.mouse_button(1, true);
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert_eq!(log.buttons, 0b11);
        s.mouse_button(0, false);
        let f = s.poll(t + ms(10)).unwrap();
        r.on_frame(t + ms(10), &wire(&f), &mut log);
        assert_eq!(log.buttons, 0b10);
        s.mouse_button(9, true); // invalid button: nothing
        assert_eq!(s.buttons, 0b10);
    }

    #[test]
    fn silence_releases_everything_after_one_second() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x11, true, false);
        s.mouse_button(0, true);
        s.set_pad(0, Some(PadState { buttons: btn::A, rt: 255, ..Default::default() }));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert_eq!(log.keys, vec![0x11]);
        assert_eq!(log.buttons, 1);
        assert!(log.plugged[0]);
        assert!(!r.tick(t + ms(900), &mut log));
        assert_eq!(log.keys, vec![0x11], "not yet");
        assert!(r.tick(t + ms(1006), &mut log));
        assert!(log.keys.is_empty());
        assert_eq!(log.buttons, 0);
        assert!(!log.plugged[0]);
        assert_eq!(log.pad[0], PadState::default());
        assert!(!r.holds_anything());
        assert!(!r.tick(t + ms(5000), &mut log), "nothing more to release");
    }

    #[test]
    fn a_viewer_that_starts_over_is_accepted_after_silence() {
        let t = t0();
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        let mut s = Sender::new(40000);
        let f = s.poll(t).unwrap();
        assert!(r.on_frame(t, &wire(&f), &mut log));
        r.tick(t + ms(1100), &mut log);
        let mut s2 = Sender::new(5);
        let f = s2.poll(t + ms(1200)).unwrap();
        assert!(r.on_frame(t + ms(1200), &wire(&f), &mut log));
    }

    #[test]
    fn nothing_is_injected_when_not_allowed() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(Perms::default());
        let mut log = Log::default();
        s.poll(t);
        s.key(0x11, true, false);
        s.mouse_button(0, true);
        s.mouse_move(10, 10);
        s.set_pad(0, Some(PadState { buttons: btn::A, ..Default::default() }));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert!(log.ev.is_empty());
        assert_eq!(log.mouse, (0, 0));
        assert!(!log.plugged[0]);
        assert!(!r.controlling(t + ms(6)));
    }

    #[test]
    fn each_device_has_its_own_switch() {
        let t = t0();
        let mk = |p: Perms| {
            let mut s = Sender::new(0);
            let mut r = Receiver::new(p);
            let mut log = Log::default();
            s.poll(t);
            s.key(0x11, true, false);
            s.set_pad(1, Some(PadState { buttons: btn::B, ..Default::default() }));
            let f = s.poll(t + ms(5)).unwrap();
            r.on_frame(t + ms(5), &wire(&f), &mut log);
            log
        };
        let only_pad = mk(Perms { allowed: true, pad: true, kbm: false });
        assert!(only_pad.keys.is_empty() && only_pad.plugged[1]);
        let only_kbm = mk(Perms { allowed: true, pad: false, kbm: true });
        assert_eq!(only_kbm.keys, vec![0x11]);
        assert!(!only_kbm.plugged[1]);
        let master_off = mk(Perms { allowed: false, pad: true, kbm: true });
        assert!(master_off.keys.is_empty() && !master_off.plugged[1]);
    }

    #[test]
    fn revoking_lets_go_of_everything_at_once() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x11, true, false);
        s.mouse_button(2, true);
        s.set_pad(0, Some(PadState { lx: 30000, ..Default::default() }));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert!(r.controlling(t + ms(6)));
        r.set_perms(Perms { allowed: false, pad: true, kbm: true }, &mut log);
        assert!(log.keys.is_empty());
        assert_eq!(log.buttons, 0);
        assert!(!log.plugged[0]);
        assert!(!r.controlling(t + ms(6)));
        // The viewer keeps sending: still nothing.
        s.key(0x12, true, false);
        let f = s.poll(t + ms(120)).unwrap();
        r.on_frame(t + ms(120), &wire(&f), &mut log);
        assert!(log.keys.is_empty());
        // Allowed again: held state applies from the next snapshot.
        r.set_perms(ALL, &mut log);
        let f = s.poll(t + ms(230)).unwrap();
        r.on_frame(t + ms(230), &wire(&f), &mut log);
        assert_eq!(log.keys, vec![0x11, 0x12], "what the viewer still holds applies again");
        assert!(log.plugged[0]);
    }

    #[test]
    fn revoking_only_the_pad_keeps_the_keyboard() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x11, true, false);
        s.set_pad(0, Some(PadState::default()));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        r.set_perms(Perms { allowed: true, pad: false, kbm: true }, &mut log);
        assert_eq!(log.keys, vec![0x11]);
        assert!(!log.plugged[0]);
    }

    #[test]
    fn revoking_only_mouse_and_keyboard_keeps_the_pad_and_ignores_their_packets() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x11, true, false);
        s.mouse_button(1, true);
        s.set_pad(0, Some(PadState { buttons: btn::A, ..Default::default() }));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert_eq!(log.keys, vec![0x11]);
        assert!(log.plugged[0] && log.buttons == 2);
        // The owner unticks "Мышь и клавиатура": held keys and buttons are let go on the spot, the pad stays.
        r.set_perms(Perms { allowed: true, pad: true, kbm: false }, &mut log);
        assert!(log.keys.is_empty());
        assert_eq!(log.buttons, 0);
        assert!(log.plugged[0], "the pad is not touched");
        assert_eq!(log.pad[0].buttons, btn::A);
        // The viewer keeps typing and moving: nothing is injected, the pad still follows.
        s.key(0x12, true, false);
        s.mouse_move(40, 40);
        s.set_pad(0, Some(PadState { buttons: btn::B, ..Default::default() }));
        let f = s.poll(t + ms(120)).unwrap();
        r.on_frame(t + ms(120), &wire(&f), &mut log);
        assert!(log.keys.is_empty());
        assert_eq!(log.mouse, (0, 0));
        assert_eq!(log.pad[0].buttons, btn::B);
        // Ticked again: what is held applies from the next snapshot, and no jump of the mouse.
        r.set_perms(ALL, &mut log);
        let f = s.poll(t + ms(230)).unwrap();
        r.on_frame(t + ms(230), &wire(&f), &mut log);
        assert_eq!(log.keys, vec![0x11, 0x12]);
        assert_eq!(log.mouse, (0, 0));
    }

    #[test]
    fn pad_packets_are_ignored_while_the_pad_is_off_and_keys_still_work() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(Perms { allowed: true, pad: false, kbm: true });
        let mut log = Log::default();
        s.poll(t);
        s.set_pad(0, Some(PadState { buttons: btn::A, ..Default::default() }));
        s.set_pad(1, Some(PadState::default()));
        s.key(0x1E, true, false);
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert!(!log.plugged[0] && !log.plugged[1], "no virtual pad appears");
        assert!(!log.ev.iter().any(|e| e.starts_with("plug")));
        assert_eq!(log.keys, vec![0x1E]);
        // Switched on later: the pad appears from the next packet; switched off again: unplugged at once.
        r.set_perms(ALL, &mut log);
        let f = s.poll(t + ms(120)).unwrap();
        r.on_frame(t + ms(120), &wire(&f), &mut log);
        assert!(log.plugged[0] && log.plugged[1]);
        r.set_perms(Perms { allowed: true, pad: false, kbm: true }, &mut log);
        assert!(!log.plugged[0] && !log.plugged[1]);
        assert_eq!(log.keys, vec![0x1E], "the keyboard is untouched");
        assert_eq!(r.plugged_count(), 0);
    }

    #[test]
    fn idle_threads_sleep_longer_and_busy_ones_keep_their_pace() {
        assert_eq!(view_pause(true), Duration::from_millis(1));
        assert_eq!(pad_poll_gap(true), Duration::from_millis(4));
        assert!(view_pause(false) >= Duration::from_millis(10));
        assert!(pad_poll_gap(false) >= Duration::from_millis(50));
        assert_eq!(host_pause(true, false), Duration::from_millis(8));
        assert_eq!(host_pause(false, true), Duration::from_millis(8), "a held key is let go promptly");
        assert!(host_pause(false, false) > Duration::from_millis(8));
        // the idle pace still lets the host see a peer's first packet within a few frames
        assert!(host_pause(false, false) < Duration::from_millis(50));
    }

    #[test]
    fn no_motion_jump_after_being_allowed_late() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(Perms::default());
        let mut log = Log::default();
        s.poll(t);
        s.mouse_move(500, 500);
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        r.set_perms(ALL, &mut log);
        s.mouse_move(1, 1);
        let f = s.poll(t + ms(10)).unwrap();
        r.on_frame(t + ms(10), &wire(&f), &mut log);
        assert_eq!(log.mouse, (1, 1));
    }

    #[test]
    fn pads_plug_update_and_unplug_by_the_mask() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.set_pad(0, Some(PadState::default()));
        s.set_pad(1, Some(PadState { buttons: btn::X, lt: 9, ..Default::default() }));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert_eq!(r.plugged_count(), 2);
        assert_eq!(log.pad[1].buttons, btn::X);
        assert_eq!(log.pad[1].lt, 9);
        s.set_pad(1, None);
        let f = s.poll(t + ms(10)).unwrap();
        r.on_frame(t + ms(10), &wire(&f), &mut log);
        assert_eq!(r.plugged_count(), 1);
        assert!(!log.plugged[1] && log.plugged[0]);
    }

    #[test]
    fn missing_driver_is_retried_once_a_second() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log { no_driver: true, ..Default::default() };
        s.poll(t);
        s.set_pad(0, Some(PadState::default()));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert_eq!(r.plugged_count(), 0);
        log.no_driver = false; // the driver got installed
        let f = s.poll(t + ms(200)).unwrap();
        r.on_frame(t + ms(200), &wire(&f), &mut log);
        assert_eq!(r.plugged_count(), 0, "not before a second has passed");
        let f = s.poll(t + ms(1100)).unwrap();
        r.on_frame(t + ms(1100), &wire(&f), &mut log);
        assert_eq!(r.plugged_count(), 1);
    }

    #[test]
    fn release_all_on_the_sender_queues_key_ups() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x11, true, false);
        s.mouse_button(0, true);
        s.set_pad(0, Some(PadState { buttons: btn::A, ..Default::default() }));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        assert!(s.holds_anything());
        s.release_all(false);
        assert!(!s.holds_anything());
        let f = s.poll(t + ms(10)).unwrap();
        r.on_frame(t + ms(10), &wire(&f), &mut log);
        assert!(log.keys.is_empty());
        assert_eq!(log.buttons, 0);
        assert_eq!(log.pad[0], PadState::default());
        assert!(log.plugged[0], "pad stays plugged, neutral");
        s.release_all(true);
        let f = s.poll(t + ms(20)).unwrap();
        r.on_frame(t + ms(20), &wire(&f), &mut log);
        assert!(!log.plugged[0]);
    }

    #[test]
    fn release_kbm_keeps_the_pad_as_it_is() {
        let t = t0();
        let mut s = Sender::new(0);
        let mut r = Receiver::new(ALL);
        let mut log = Log::default();
        s.poll(t);
        s.key(0x1E, true, false);
        s.mouse_button(1, true);
        s.set_pad(0, Some(PadState { buttons: btn::B, ..Default::default() }));
        let f = s.poll(t + ms(5)).unwrap();
        r.on_frame(t + ms(5), &wire(&f), &mut log);
        s.release_kbm();
        let f = s.poll(t + ms(10)).unwrap();
        r.on_frame(t + ms(10), &wire(&f), &mut log);
        assert!(log.keys.is_empty());
        assert_eq!(log.buttons, 0);
        assert_eq!(log.pad[0].buttons, btn::B, "the pad button is still held");
    }

    #[test]
    fn rumble_is_sent_several_times_and_refreshed_while_running() {
        let t = t0();
        let mut tx = RumbleTx::new();
        assert!(tx.poll(t).is_empty());
        tx.set(0, 200, 50);
        let mut got = Vec::new();
        let mut now = t;
        for _ in 0..100 {
            got.extend(tx.poll(now));
            now += ms(10);
        }
        // 3 repeats right after the change, then ~every 200 ms while running (1 s => about 5 more).
        assert!(got.len() >= 7 && got.len() <= 11, "{}", got.len());
        assert!(got.iter().all(|r| r.idx == 0 && r.large == 200 && r.small == 50));
        tx.set(0, 0, 0);
        let mut stop = Vec::new();
        for _ in 0..10 {
            stop.extend(tx.poll(now));
            now += ms(10);
        }
        assert_eq!(stop.len(), 3);
        assert!(tx.poll(now + ms(1000)).is_empty(), "silent once stopped");
    }

    #[test]
    fn rumble_rx_newest_wins_and_stops_by_itself() {
        let t = t0();
        let mut rx = RumbleRx::new();
        let a = Rumble { seq: 10, idx: 1, large: 255, small: 0 };
        assert_eq!(rx.on_packet(t, &a), Some((1, 255, 0)));
        assert_eq!(rx.on_packet(t, &a), None, "duplicate");
        let old = Rumble { seq: 9, idx: 1, large: 0, small: 0 };
        assert_eq!(rx.on_packet(t, &old), None, "older packet cannot stop a newer one");
        assert!(rx.tick(t + ms(500)).is_empty());
        assert_eq!(rx.tick(t + ms(700)), vec![1]);
        assert!(rx.tick(t + ms(900)).is_empty());
        let b = Rumble { seq: 11, idx: 1, large: 10, small: 10 };
        assert_eq!(rx.on_packet(t + ms(1000), &b), Some((1, 10, 10)));
        assert_eq!(rx.stop_all(), vec![1]);
        assert_eq!(rx.on_packet(t, &Rumble { seq: 1, idx: 9, large: 1, small: 1 }), None);
    }

    #[test]
    fn slots_are_packed_in_order() {
        assert_eq!(assign_slots([false, false, false, false]), [None; 4]);
        assert_eq!(assign_slots([false, true, false, true]), [Some(1), Some(3), None, None]);
        assert_eq!(assign_slots([true, true, true, true]), [Some(0), Some(1), Some(2), Some(3)]);
    }
}
