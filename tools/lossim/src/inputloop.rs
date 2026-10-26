// Loopback test of the remote-control channel: the real packet codec and header over a real UDP socket on 127.0.0.1,
// with packets dropped, duplicated and reordered on the way. Time is virtual (1 ms per step), so it runs in a blink.
// What it proves: a lost packet never leaves a key or button stuck, the mouse does not drift, a silent viewer is let go of.
use crate::inputlogic::{Perms, Receiver, Sender, Sink, HOST_TIMEOUT};
use crate::inputproto::{self, btn, PadState};
use crate::net;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Host {
    keys: Vec<u16>,
    buttons: u8,
    mouse: (i64, i64),
    wheel: i64,
    plugged: [bool; 4],
    pad: [PadState; 4],
    typed: Vec<(u16, bool)>,
}

impl Sink for Host {
    fn key(&mut self, c: u16, down: bool) {
        if down {
            if !self.keys.contains(&c) {
                self.keys.push(c);
                self.typed.push((c, true));
            }
        } else {
            self.keys.retain(|&k| k != c);
            self.typed.push((c, false));
        }
    }
    fn mouse_button(&mut self, b: u8, down: bool) {
        if down {
            self.buttons |= 1 << b;
        } else {
            self.buttons &= !(1 << b);
        }
    }
    fn mouse_move(&mut self, dx: i32, dy: i32) {
        self.mouse.0 += dx as i64;
        self.mouse.1 += dy as i64;
    }
    fn wheel(&mut self, v: i32, _h: i32) {
        self.wheel += v as i64;
    }
    fn pad_plug(&mut self, i: usize) -> bool {
        self.plugged[i] = true;
        true
    }
    fn pad_unplug(&mut self, i: usize) {
        self.plugged[i] = false;
    }
    fn pad_state(&mut self, i: usize, st: &PadState) {
        self.pad[i] = *st;
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Rig {
    tx: UdpSocket,
    rx: UdpSocket,
    token: u32,
    sender: Sender,
    recv: Receiver,
    host: Host,
    now: Instant,
    rng: Rng,
    loss: f64,
    dup: f64,
    held_back: Option<Vec<u8>>,
    sent: u32,
    delivered: u32,
}

impl Rig {
    fn new(loss: f64, seed: u64) -> Rig {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_nonblocking(true).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx.local_addr().unwrap()).unwrap();
        Rig {
            tx,
            rx,
            token: 0xC0FFEE11,
            sender: Sender::new(100),
            recv: Receiver::new(Perms { allowed: true, pad: true, kbm: true }),
            host: Host::default(),
            now: Instant::now(),
            rng: Rng(seed),
            loss,
            dup: 0.03,
            held_back: None,
            sent: 0,
            delivered: 0,
        }
    }

    /// One virtual millisecond: the viewer polls, the "network" mangles, the host reads its socket.
    fn step(&mut self, viewer_alive: bool) {
        self.now += Duration::from_millis(1);
        let mut out = Vec::new();
        if viewer_alive {
            if let Some(f) = self.sender.poll(self.now) {
                net::write_header(&mut out, net::PT_INPUT, self.token);
                inputproto::encode(&f, &mut out);
                assert!(out.len() < 400, "input packets must stay small, got {}", out.len());
                self.sent += 1;
                let r = self.rng.next();
                if r >= self.loss {
                    // reorder: sometimes hold a packet back and send it after the next one
                    if self.rng.next() < 0.05 && self.held_back.is_none() {
                        self.held_back = Some(out.clone());
                    } else {
                        self.tx.send(&out).unwrap();
                        if let Some(h) = self.held_back.take() {
                            self.tx.send(&h).unwrap();
                        }
                        if self.rng.next() < self.dup {
                            self.tx.send(&out).unwrap();
                        }
                    }
                }
            }
        }
        let mut buf = [0u8; 1500];
        while let Ok((n, _)) = self.rx.recv_from(&mut buf) {
            if let Some((ty, body)) = net::parse_header(&buf[..n], self.token) {
                assert_eq!(ty, net::PT_INPUT);
                if let Some(f) = inputproto::decode(body) {
                    self.delivered += 1;
                    self.recv.on_frame(self.now, &f, &mut self.host);
                }
            }
        }
        self.recv.tick(self.now, &mut self.host);
    }

    fn run(&mut self, ms: u32, alive: bool) {
        for _ in 0..ms {
            self.step(alive);
        }
    }
}

#[test]
fn keys_and_buttons_are_never_stuck_over_a_lossy_udp_link() {
    for seed in 1..=20u64 {
        for loss in [0.0, 0.2, 0.4, 0.6] {
            let mut r = Rig::new(loss, seed * 7919 + (loss * 100.0) as u64);
            let mut want_mouse = (0i64, 0i64);
            // the first packet that arrives only sets the mouse's starting point (no jump on connect)
            r.run(2000, true);
            for round in 0..40 {
                // press W and the left button, move the mouse, release everything
                r.sender.key(0x11, true, false);
                r.sender.mouse_button(0, true);
                for _ in 0..10 {
                    r.sender.mouse_move(3, -2);
                    want_mouse.0 += 3;
                    want_mouse.1 -= 2;
                    r.run(5, true);
                }
                r.sender.key(0x11, false, false);
                r.sender.mouse_button(0, false);
                r.run(20 + (round % 3) * 10, true);
            }
            // a quiet moment (2 s = 20 refreshes): the refresh repairs whatever was lost
            r.run(2000, true);
            assert!(r.host.keys.is_empty(), "stuck key (seed {seed}, loss {loss}): {:?}", r.host.keys);
            assert_eq!(r.host.buttons, 0, "stuck mouse button (seed {seed}, loss {loss})");
            if loss <= 0.4 {
                // (at 60% loss ten refreshes in a row can vanish = 1 s of silence: the host then lets go and starts over,
                // so a mouse step may be lost - never a key)
                assert_eq!(r.host.mouse, want_mouse, "mouse drifted (seed {seed}, loss {loss})");
            }
            assert!(r.delivered > 0 && r.sent > r.delivered / 2);
        }
    }
}

#[test]
fn a_key_down_gets_through_a_bad_link_quickly_and_a_terrible_one_eventually() {
    let (mut fast, mut slow) = (0, 0);
    for seed in 1..=30u64 {
        // 30% loss: the numbered redundant events deliver it within ~30 ms
        let mut r = Rig::new(0.3, seed * 104729);
        r.run(20, true);
        r.sender.key(0x1E, true, false); // A
        r.run(40, true);
        fast += r.host.keys.contains(&0x1E) as u32;
        // 60% loss: the 100 ms refresh repairs it within a second at the latest
        let mut r = Rig::new(0.6, seed * 15485863);
        r.run(20, true);
        r.sender.key(0x1E, true, false);
        r.run(1000, true);
        slow += r.host.keys.contains(&0x1E) as u32;
    }
    assert!(fast >= 28, "30% loss, 40 ms: key arrived {fast}/30 times");
    assert_eq!(slow, 30, "60% loss, 1 s: key arrived {slow}/30 times");
}

#[test]
fn pads_follow_and_are_released_when_the_viewer_goes_silent() {
    let mut r = Rig::new(0.2, 42);
    let st = PadState { buttons: btn::A | btn::LB, lt: 200, rt: 0, lx: 12000, ly: -9000, rx: 0, ry: 30000 };
    r.sender.set_pad(0, Some(st));
    r.sender.key(0x2A, true, false); // Shift
    r.run(400, true);
    assert!(r.host.plugged[0], "virtual pad 1 plugged");
    assert_eq!(r.host.pad[0], st);
    assert!(r.host.keys.contains(&0x2A));
    // the viewer vanishes: no packets at all
    r.run(HOST_TIMEOUT.as_millis() as u32 - 100, false);
    assert!(r.host.plugged[0] && !r.host.keys.is_empty(), "not yet: the timeout is 1 s");
    r.run(300, false);
    assert!(r.host.keys.is_empty(), "key released after 1 s of silence");
    assert!(!r.host.plugged[0], "virtual pad unplugged after 1 s of silence");
    assert!(!r.recv.controlling(r.now));
}

#[test]
fn nothing_is_applied_when_control_is_not_allowed() {
    let mut r = Rig::new(0.0, 7);
    r.recv = Receiver::new(Perms { allowed: false, pad: true, kbm: true });
    r.sender.key(0x11, true, false);
    r.sender.mouse_button(0, true);
    r.sender.mouse_move(50, 50);
    r.sender.set_pad(0, Some(PadState { buttons: btn::A, ..Default::default() }));
    r.run(300, true);
    assert!(r.delivered > 0, "packets do arrive");
    assert!(r.host.keys.is_empty() && r.host.buttons == 0 && r.host.mouse == (0, 0) && !r.host.plugged[0]);
    // the owner allows only the pad: the keyboard stays out
    r.recv.set_perms(Perms { allowed: true, pad: true, kbm: false }, &mut r.host);
    r.run(300, true);
    assert!(r.host.plugged[0] && r.host.keys.is_empty() && r.host.mouse == (0, 0));
    // revoked while the key is held: everything is let go at once
    r.recv.set_perms(Perms { allowed: true, pad: true, kbm: true }, &mut r.host);
    r.run(300, true);
    assert!(r.host.keys.contains(&0x11));
    r.recv.set_perms(Perms::default(), &mut r.host);
    assert!(r.host.keys.is_empty() && r.host.buttons == 0 && !r.host.plugged[0]);
    r.run(300, true);
    assert!(r.host.keys.is_empty() && !r.host.plugged[0], "stays released while the viewer keeps sending");
}

#[test]
fn packets_of_another_session_or_garbage_are_ignored() {
    let mut r = Rig::new(0.0, 9);
    let evil = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut out = Vec::new();
    net::write_header(&mut out, net::PT_INPUT, 0xDEAD_BEEF);
    out.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
    evil.send_to(&out, r.rx.local_addr().unwrap()).unwrap();
    evil.send_to(&[0xA8, 1], r.rx.local_addr().unwrap()).unwrap();
    evil.send_to(&[], r.rx.local_addr().unwrap()).unwrap();
    r.run(50, false);
    assert_eq!(r.delivered, 0);
    assert!(r.host.keys.is_empty());
}
