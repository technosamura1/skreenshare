// Laser pointer and reactions (v0.11.8, stickers placed by click v0.12.1, any sticker of the pack v0.12.2): the
// viewer points at things and sends animated cybercats stickers (stickers.rs); the host draws them on its own
// screen. Pure logic, no Windows calls (the drawing is in annotview.rs).
//
// Viewer -> host, PT_ANNOT: ver u8 (=1) | seq u16 | flags u8 (bit 0: laser on) | [x u16 | y u16 if laser] |
//   n u8 | n × [rseq u16 | kind u8 | [x u16 | y u16]]   kind: bit 7 = a spot follows, bit 6 = sticker number in
//   bits 0-5 (0.12.2); without bit 6, 1..4 = the old laugh / heart / yes / no.
// The laser is a snapshot (a lost packet is repaired by the next one; it goes out at least every 150 ms while on,
// and disappears on the host after 500 ms of silence); it switches itself off after 3 s without the mouse
// moving. Each reaction rides in 3 packets in a row and the host keeps the numbers it has seen, so a loss neither
// drops it nor shows it twice. Older hosts ignore the type (0.12.0 drops a frame with a placed reaction).
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const VERSION: u8 = 1;
/// How long a reaction stays on screen.
pub const REACTION_LIFE: Duration = Duration::from_millis(2200);
/// The laser vanishes after this long without a packet (the viewer turned it off, or the link dropped).
pub const LASER_TIMEOUT: Duration = Duration::from_millis(500);
/// ... and after this long without the viewer's mouse moving.
pub const LASER_IDLE: Duration = Duration::from_secs(3);
/// At most this many reactions on screen at once (spamming clicks must not bury the host's screen).
pub const MAX_ACTIVE: usize = 16;
const SENDS: u8 = 3;
const PLACED: u8 = 0x80;
const MAX_PER_PACKET: usize = 24;

const STICKER: u8 = 0x40;

/// The sticker an older viewer (0.12.0-0.12.1) meant by its reaction 1..4.
fn legacy(k: u8) -> Option<u8> {
    let f = crate::stickers::FAVORITES;
    Some(match k {
        1 => f[2], // laugh
        2 => f[3], // heart
        3 => f[0], // yes
        4 => f[1], // no
        _ => return None,
    } as u8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sent {
    pub rseq: u16,
    /// Sticker number (stickers.rs).
    pub sticker: u8,
    /// Spot on the picture (0..=65535 both ways) where the viewer clicked; None = where the laser is / a corner.
    pub at: Option<(u16, u16)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub seq: u16,
    /// Where the viewer points on the picture, 0..=65535 both ways; None = laser off.
    pub laser: Option<(u16, u16)>,
    pub reactions: Vec<Sent>,
}

pub fn encode(f: &Frame, out: &mut Vec<u8>) {
    out.push(VERSION);
    out.extend_from_slice(&f.seq.to_le_bytes());
    out.push(f.laser.is_some() as u8);
    if let Some((x, y)) = f.laser {
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
    }
    let n = f.reactions.len().min(MAX_PER_PACKET);
    out.push(n as u8);
    for r in &f.reactions[..n] {
        out.extend_from_slice(&r.rseq.to_le_bytes());
        out.push(STICKER | (r.sticker & 0x3F) | if r.at.is_some() { PLACED } else { 0 });
        if let Some((x, y)) = r.at {
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
    }
}

pub fn decode(b: &[u8]) -> Option<Frame> {
    let mut i = 0;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(i..i + n)?;
        i += n;
        Some(s)
    };
    let point = |p: &[u8]| (u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]]));
    if take(1)?[0] != VERSION {
        return None;
    }
    let seq = u16::from_le_bytes(take(2)?.try_into().ok()?);
    let flags = take(1)?[0];
    let laser = if flags & 1 != 0 { Some(point(take(4)?)) } else { None };
    let n = take(1)?[0] as usize;
    if n > MAX_PER_PACKET {
        return None;
    }
    let mut reactions = Vec::with_capacity(n);
    for _ in 0..n {
        let p = take(3)?;
        let (rseq, k) = (u16::from_le_bytes([p[0], p[1]]), p[2]);
        let at = if k & PLACED != 0 { Some(point(take(4)?)) } else { None };
        let sticker = if k & STICKER != 0 { k & 0x3F } else { legacy(k & !PLACED)? };
        reactions.push(Sent { rseq, sticker, at });
    }
    Some(Frame { seq, laser, reactions })
}

fn newer(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 0x8000
}

/// Viewer side: what to send and when.
pub struct Sender {
    seq: u16,
    laser: Option<(u16, u16)>,
    /// When the laser last moved (it goes dark LASER_IDLE later).
    moved: Option<Instant>,
    /// The laser as the host last heard it.
    sent_laser: Option<(u16, u16)>,
    rseq: u16,
    pending: Vec<(Sent, u8)>,
    last_send: Option<Instant>,
}

impl Sender {
    pub fn new(start: u16) -> Sender {
        Sender { seq: start, laser: None, moved: None, sent_laser: None, rseq: start, pending: Vec::new(), last_send: None }
    }

    pub fn set_laser(&mut self, pos: Option<(u16, u16)>, now: Instant) {
        if self.laser != pos {
            self.laser = pos;
            self.moved = Some(now);
        }
    }

    /// The laser is on and the mouse moved lately: shown (and sent).
    pub fn laser_visible(&self, now: Instant) -> Option<(u16, u16)> {
        self.laser.filter(|_| self.moved.is_some_and(|m| now.saturating_duration_since(m) < LASER_IDLE))
    }

    pub fn react(&mut self, sticker: u8, at: Option<(u16, u16)>) {
        self.rseq = self.rseq.wrapping_add(1);
        self.pending.push((Sent { rseq: self.rseq, sticker, at }, SENDS));
    }

    /// A packet to send now, if any: changes at most every 6 ms (smooth on 144 Hz screens; the packets are
    /// ~10 bytes), the laser refreshed every 150 ms while on.
    pub fn poll(&mut self, now: Instant) -> Option<Frame> {
        let laser = self.laser_visible(now);
        let changed = laser != self.sent_laser || !self.pending.is_empty();
        let since = self.last_send.map(|t| now.saturating_duration_since(t));
        let due = match since {
            None => changed || laser.is_some(),
            Some(d) => changed && d >= Duration::from_millis(6) || laser.is_some() && d >= Duration::from_millis(150),
        };
        if !due {
            return None;
        }
        self.seq = self.seq.wrapping_add(1);
        let reactions = self.pending.iter().take(MAX_PER_PACKET).map(|p| p.0).collect();
        for p in self.pending.iter_mut().take(MAX_PER_PACKET) {
            p.1 -= 1;
        }
        self.pending.retain(|p| p.1 > 0);
        self.sent_laser = laser;
        self.last_send = Some(now);
        Some(Frame { seq: self.seq, laser, reactions })
    }
}

/// One reaction on screen.
#[derive(Clone, Copy, Debug)]
pub struct Shown {
    pub sticker: u8,
    pub born: Instant,
    /// Spot on the picture where it appears.
    pub at: (u16, u16),
    /// Sideways offset (-2..=2) for ones without a spot of their own, so several don't sit on each other.
    pub lane: i8,
}

/// Host side: the current laser and the reactions to draw.
#[derive(Default)]
pub struct Board {
    last_seq: Option<u16>,
    laser: Option<((u16, u16), Instant)>,
    last_rx: Option<Instant>,
    seen: VecDeque<u16>,
    shown: Vec<Shown>,
    lane: i8,
}

/// Where a reaction appears when the viewer isn't pointing: lower right, above the taskbar.
const CORNER: (u16, u16) = (55_000, 52_000);

impl Board {
    pub fn on_frame(&mut self, now: Instant, f: &Frame) {
        if self.last_seq.is_some_and(|l| !newer(f.seq, l)) {
            return; // old or repeated packet
        }
        self.last_seq = Some(f.seq);
        self.last_rx = Some(now);
        self.laser = f.laser.map(|p| (p, now));
        for r in &f.reactions {
            if self.seen.contains(&r.rseq) {
                continue;
            }
            self.seen.push_back(r.rseq);
            if self.seen.len() > 128 {
                self.seen.pop_front();
            }
            self.add(now, r.sticker, r.at.or(self.laser.map(|l| l.0)));
        }
    }

    /// A reaction to show (`at` None: the corner, spread over lanes).
    pub fn add(&mut self, now: Instant, sticker: u8, at: Option<(u16, u16)>) {
        self.shown.retain(|s| now.saturating_duration_since(s.born) < REACTION_LIFE);
        if self.shown.len() >= MAX_ACTIVE {
            self.shown.remove(0);
        }
        let lane = if at.is_some() {
            0
        } else {
            self.lane = if self.lane >= 2 { -2 } else { self.lane + 1 };
            self.lane
        };
        self.shown.push(Shown { sticker, born: now, at: at.unwrap_or(CORNER), lane });
    }

    pub fn laser(&self, now: Instant) -> Option<(u16, u16)> {
        let last = self.last_rx?;
        (now.saturating_duration_since(last) < LASER_TIMEOUT).then_some(())?;
        self.laser.map(|l| l.0)
    }

    pub fn reactions(&mut self, now: Instant) -> &[Shown] {
        self.shown.retain(|s| now.saturating_duration_since(s.born) < REACTION_LIFE);
        &self.shown
    }

    /// Anything to draw at all (the overlay window hides otherwise).
    pub fn busy(&mut self, now: Instant) -> bool {
        self.laser(now).is_some() || !self.reactions(now).is_empty()
    }
}

/// Where a reaction is `age` into its life: rises from its spot, pops in over 0.15 s, fades over the last 0.4 s.
/// Returns (rise in sticker sizes, scale, opacity).
pub fn motion(age: Duration) -> (f32, f32, f32) {
    let t = age.as_secs_f32();
    let life = REACTION_LIFE.as_secs_f32();
    let scale = 0.4 + 0.6 * (t / 0.15).clamp(0.0, 1.0);
    let alpha = ((life - t) / 0.4).clamp(0.0, 1.0);
    (0.45 * t, scale, alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn wire(f: Frame) -> Frame {
        let mut v = Vec::new();
        encode(&f, &mut v);
        decode(&v).unwrap()
    }

    #[test]
    fn frames_round_trip_and_garbage_is_rejected() {
        let s = |rseq, sticker, at| Sent { rseq, sticker, at };
        for f in [
            Frame { seq: 7, laser: Some((1, 65535)), reactions: vec![s(3, 7, None), s(4, 63, Some((5, 6)))] },
            Frame { seq: 65535, laser: None, reactions: vec![] },
        ] {
            let mut b = Vec::new();
            encode(&f, &mut b);
            assert_eq!(decode(&b), Some(f.clone()));
            for cut in 0..b.len() {
                assert_eq!(decode(&b[..cut]), None);
            }
        }
        assert_eq!(decode(&[1, 0, 0, 0, 1, 0, 0, 9]), None, "unknown reaction");
        assert_eq!(decode(&[2, 0, 0, 0, 0]), None, "other version");
        // 0.12.0-0.12.1 viewers (reactions 1..4) still read fine
        assert_eq!(decode(&[1, 1, 0, 0, 1, 2, 0, 2]).unwrap().reactions, vec![s(2, crate::stickers::FAVORITES[3] as u8, None)]);
    }

    #[test]
    fn a_reaction_survives_two_lost_packets_and_shows_once_at_its_spot() {
        let t = Instant::now();
        let mut s = Sender::new(10);
        let mut b = Board::default();
        s.react(17, Some((100, 200)));
        let _lost = (s.poll(t).unwrap(), s.poll(t + ms(30)).unwrap());
        let p3 = s.poll(t + ms(60)).unwrap();
        assert!(s.poll(t + ms(90)).is_none(), "nothing more to say");
        b.on_frame(t + ms(61), &wire(p3.clone()));
        b.on_frame(t + ms(62), &wire(p3));
        let r = b.reactions(t + ms(70));
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].at, r[0].lane), ((100, 200), 0));
        assert!(b.reactions(t + ms(61) + REACTION_LIFE).is_empty(), "gone after its life");
    }

    #[test]
    fn laser_follows_refreshes_and_times_out() {
        let t = Instant::now();
        let mut s = Sender::new(0);
        let mut b = Board::default();
        s.set_laser(Some((100, 200)), t);
        b.on_frame(t, &s.poll(t).unwrap());
        assert_eq!(b.laser(t), Some((100, 200)));
        assert!(s.poll(t + ms(100)).is_none());
        let keep = s.poll(t + ms(150)).expect("refresh while on");
        b.on_frame(t + ms(150), &keep);
        assert_eq!(b.laser(t + ms(600)), Some((100, 200)));
        assert_eq!(b.laser(t + ms(700)), None, "silence: gone");
        // turning it off is sent and clears it at once
        s.set_laser(None, t + ms(200));
        b.on_frame(t + ms(200), &s.poll(t + ms(200)).unwrap());
        assert_eq!(b.laser(t + ms(201)), None);
        // a reaction without a spot appears where the laser is
        s.set_laser(Some((9, 9)), t + ms(300));
        s.react(15, None);
        b.on_frame(t + ms(300), &s.poll(t + ms(300)).unwrap());
        assert_eq!(b.reactions(t + ms(300))[0].at, (9, 9));
    }

    #[test]
    fn a_still_mouse_turns_the_laser_off_and_moving_brings_it_back() {
        let t = Instant::now();
        let mut s = Sender::new(0);
        let mut b = Board::default();
        s.set_laser(Some((5, 5)), t);
        let mut now = t;
        while now < t + LASER_IDLE + ms(400) {
            if let Some(f) = s.poll(now) {
                b.on_frame(now, &f);
            }
            now += ms(10);
        }
        assert_eq!(s.laser_visible(now), None);
        assert_eq!(b.laser(now), None, "the host was told, not just left to time out");
        // the same spot again is not a move; another one is
        s.set_laser(Some((5, 5)), now);
        assert_eq!(s.laser_visible(now), None);
        s.set_laser(Some((6, 5)), now);
        b.on_frame(now, &s.poll(now).unwrap());
        assert_eq!(b.laser(now), Some((6, 5)));
    }

    #[test]
    fn spamming_keeps_the_screen_bounded() {
        let t = Instant::now();
        let mut s = Sender::new(0);
        let mut b = Board::default();
        for i in 0..60u64 {
            s.react(7, Some((i as u16, 0)));
            if let Some(f) = s.poll(t + ms(30 * i)) {
                b.on_frame(t + ms(30 * i), &f);
            }
        }
        let n = b.reactions(t + ms(1800)).len();
        assert!(n > 4 && n <= MAX_ACTIVE, "{n}");
    }
}
