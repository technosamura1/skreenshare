// Laser pointer and reactions (v0.11.8): the viewer points at things and sends 😂 ❤️ ДА НЕТ; the host draws them
// on its own screen. Pure logic, no Windows calls (the drawing is in annotview.rs).
//
// Viewer -> host, PT_ANNOT: ver u8 (=1) | seq u16 | flags u8 (bit 0: laser on) | [x u16 | y u16 if laser] |
//   n u8 | n × [rseq u16 | kind u8]
// The laser is a snapshot (a lost packet is repaired by the next one; it goes out at least every 150 ms while on,
// and disappears on the host after 500 ms of silence). Each reaction rides in 3 packets in a row and the host
// keeps the numbers it has seen, so a loss neither drops it nor shows it twice. Older hosts ignore the type.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const VERSION: u8 = 1;
/// How long a reaction stays on screen.
pub const REACTION_LIFE: Duration = Duration::from_millis(2200);
/// The laser vanishes after this long without a packet (the viewer turned it off, or the link dropped).
pub const LASER_TIMEOUT: Duration = Duration::from_millis(500);
/// At most this many reactions on screen at once (a held key must not flood the host's screen).
pub const MAX_ACTIVE: usize = 6;
const SENDS: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reaction {
    Laugh = 1,
    Heart = 2,
    Yes = 3,
    No = 4,
}

impl Reaction {
    pub fn from_u8(v: u8) -> Option<Reaction> {
        Some(match v {
            1 => Reaction::Laugh,
            2 => Reaction::Heart,
            3 => Reaction::Yes,
            4 => Reaction::No,
            _ => return None,
        })
    }

    /// What is drawn (DirectWrite picks the colour emoji font for the emoji by itself).
    pub fn text(self) -> &'static str {
        match self {
            Reaction::Laugh => "😂",
            Reaction::Heart => "❤️",
            Reaction::Yes => "👍 ДА",
            Reaction::No => "👎 НЕТ",
        }
    }

    /// Words for the viewer's own confirmation line.
    pub fn name(self) -> &'static str {
        match self {
            Reaction::Laugh => "смех",
            Reaction::Heart => "сердечко",
            Reaction::Yes => "ДА",
            Reaction::No => "НЕТ",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub seq: u16,
    /// Where the viewer points on the picture, 0..=65535 both ways; None = laser off.
    pub laser: Option<(u16, u16)>,
    pub reactions: Vec<(u16, Reaction)>,
}

pub fn encode(f: &Frame, out: &mut Vec<u8>) {
    out.push(VERSION);
    out.extend_from_slice(&f.seq.to_le_bytes());
    out.push(f.laser.is_some() as u8);
    if let Some((x, y)) = f.laser {
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
    }
    let n = f.reactions.len().min(16);
    out.push(n as u8);
    for (s, r) in &f.reactions[..n] {
        out.extend_from_slice(&s.to_le_bytes());
        out.push(*r as u8);
    }
}

pub fn decode(b: &[u8]) -> Option<Frame> {
    let mut i = 0;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(i..i + n)?;
        i += n;
        Some(s)
    };
    if take(1)?[0] != VERSION {
        return None;
    }
    let seq = u16::from_le_bytes(take(2)?.try_into().ok()?);
    let flags = take(1)?[0];
    let laser = if flags & 1 != 0 {
        let p = take(4)?;
        Some((u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]])))
    } else {
        None
    };
    let n = take(1)?[0] as usize;
    if n > 16 {
        return None;
    }
    let mut reactions = Vec::with_capacity(n);
    for _ in 0..n {
        let p = take(3)?;
        reactions.push((u16::from_le_bytes([p[0], p[1]]), Reaction::from_u8(p[2])?));
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
    rseq: u16,
    pending: Vec<(u16, Reaction, u8)>,
    dirty: bool,
    last_send: Option<Instant>,
}

impl Sender {
    pub fn new(start: u16) -> Sender {
        Sender { seq: start, laser: None, rseq: start, pending: Vec::new(), dirty: false, last_send: None }
    }

    pub fn set_laser(&mut self, pos: Option<(u16, u16)>) {
        if self.laser != pos {
            // Turning it off must reach the host too: one more packet.
            self.laser = pos;
            self.dirty = true;
        }
    }

    pub fn react(&mut self, r: Reaction) {
        self.rseq = self.rseq.wrapping_add(1);
        self.pending.push((self.rseq, r, SENDS));
        self.dirty = true;
    }

    /// A packet to send now, if any: changes at most every 25 ms, the laser refreshed every 150 ms while on.
    pub fn poll(&mut self, now: Instant) -> Option<Frame> {
        let since = self.last_send.map(|t| now.saturating_duration_since(t));
        let due = since.is_none_or(|d| {
            (self.dirty || !self.pending.is_empty()) && d >= Duration::from_millis(25) || self.laser.is_some() && d >= Duration::from_millis(150)
        });
        if !due || !(self.dirty || !self.pending.is_empty() || self.laser.is_some()) {
            return None;
        }
        self.seq = self.seq.wrapping_add(1);
        let reactions = self.pending.iter().map(|(s, r, _)| (*s, *r)).collect();
        for p in &mut self.pending {
            p.2 -= 1;
        }
        self.pending.retain(|p| p.2 > 0);
        self.dirty = false;
        self.last_send = Some(now);
        Some(Frame { seq: self.seq, laser: self.laser, reactions })
    }
}

/// One reaction on the host's screen.
#[derive(Clone, Copy, Debug)]
pub struct Shown {
    pub kind: Reaction,
    pub born: Instant,
    /// Spot on the picture where it appears (the laser's, or a corner when there is none).
    pub at: (u16, u16),
    /// Small sideways offset so several at once don't sit on top of each other (-2..=2).
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
        for &(rs, kind) in &f.reactions {
            if self.seen.contains(&rs) {
                continue;
            }
            self.seen.push_back(rs);
            if self.seen.len() > 64 {
                self.seen.pop_front();
            }
            self.shown.retain(|s| now.saturating_duration_since(s.born) < REACTION_LIFE);
            if self.shown.len() >= MAX_ACTIVE {
                self.shown.remove(0);
            }
            self.lane = if self.lane >= 2 { -2 } else { self.lane + 1 };
            self.shown.push(Shown { kind, born: now, at: self.laser.map_or(CORNER, |l| l.0), lane: self.lane });
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn frames_round_trip_and_garbage_is_rejected() {
        for f in [
            Frame { seq: 7, laser: Some((1, 65535)), reactions: vec![(3, Reaction::Heart), (4, Reaction::No)] },
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
    }

    #[test]
    fn a_reaction_survives_two_lost_packets_and_shows_once() {
        let t = Instant::now();
        let mut s = Sender::new(10);
        let mut b = Board::default();
        s.react(Reaction::Laugh);
        let wire = |f: Frame| {
            let mut v = Vec::new();
            encode(&f, &mut v);
            decode(&v).unwrap()
        };
        let p1 = s.poll(t).unwrap();
        let p2 = s.poll(t + ms(30)).unwrap();
        let p3 = s.poll(t + ms(60)).unwrap();
        assert!(s.poll(t + ms(90)).is_none(), "nothing more to say");
        // the first two are lost
        let _ = (p1, p2);
        b.on_frame(t + ms(61), &wire(p3.clone()));
        b.on_frame(t + ms(62), &wire(p3));
        assert_eq!(b.reactions(t + ms(70)).len(), 1);
        assert!(b.reactions(t + ms(61) + REACTION_LIFE).is_empty(), "gone after its life");
    }

    #[test]
    fn laser_follows_refreshes_and_times_out() {
        let t = Instant::now();
        let mut s = Sender::new(0);
        let mut b = Board::default();
        s.set_laser(Some((100, 200)));
        b.on_frame(t, &s.poll(t).unwrap());
        assert_eq!(b.laser(t), Some((100, 200)));
        assert!(s.poll(t + ms(100)).is_none());
        let keep = s.poll(t + ms(150)).expect("refresh while on");
        b.on_frame(t + ms(150), &keep);
        assert_eq!(b.laser(t + ms(600)), Some((100, 200)));
        assert_eq!(b.laser(t + ms(700)), None, "silence: gone");
        // turning it off is sent and clears it at once
        s.set_laser(None);
        b.on_frame(t + ms(200), &s.poll(t + ms(200)).unwrap());
        assert_eq!(b.laser(t + ms(201)), None);
        // a reaction appears where the laser was
        s.set_laser(Some((9, 9)));
        s.react(Reaction::Yes);
        b.on_frame(t + ms(300), &s.poll(t + ms(300)).unwrap());
        assert_eq!(b.reactions(t + ms(300))[0].at, (9, 9));
    }

    #[test]
    fn spamming_keeps_at_most_six_on_screen() {
        let t = Instant::now();
        let mut s = Sender::new(0);
        let mut b = Board::default();
        for i in 0..20u64 {
            s.react(Reaction::Heart);
            if let Some(f) = s.poll(t + ms(30 * i)) {
                b.on_frame(t + ms(30 * i), &f);
            }
        }
        assert!(b.reactions(t + ms(600)).len() <= MAX_ACTIVE);
    }
}
