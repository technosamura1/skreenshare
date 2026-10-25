// Video reassembly on the viewer side: fragments -> frames in order, with NACK-based recovery.
// Pure logic with an injected clock (no sockets, no Windows APIs), so tools/lossim can run it in a simulator.
use crate::net::MAX_PAYLOAD;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// A hole is only requested after it has stayed open this long: packets that merely arrive out of order
/// must not cause retransmissions.
pub const REORDER_DELAY: Duration = Duration::from_millis(4);
/// Request rounds per frame before the frame is given up on.
pub const MAX_ROUNDS: u8 = 8;
/// Give up on the frame we are waiting for after this long, no matter what.
pub const FRAME_DEADLINE: Duration = Duration::from_millis(1000);

struct Partial {
    /// 0 = placeholder for a frame we haven't seen a single packet of yet.
    count: u16,
    got: u16,
    key: bool,
    frags: Vec<Option<Vec<u8>>>,
    /// Which fragments we have asked for (to count recoveries).
    asked: Vec<bool>,
    first: Instant,
    last_rx: Instant,
    /// Since when this frame has had a hole (for the reorder delay).
    hole_since: Option<Instant>,
    last_nack: Option<Instant>,
    rounds: u8,
}

impl Partial {
    fn new(now: Instant, count: u16, key: bool) -> Partial {
        Partial {
            count,
            got: 0,
            key,
            frags: vec![None; count as usize],
            asked: vec![false; count as usize],
            first: now,
            last_rx: now,
            hole_since: None,
            last_nack: None,
            rounds: 0,
        }
    }
}

/// Counters for the status line and for the sender's bitrate control. All monotonically increasing.
#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    /// Fragments received (first copy of each).
    pub received: u64,
    /// Fragments found missing (first time) — raw network loss, before any repair.
    pub missed: u64,
    /// Fragments that arrived because we asked for them again.
    pub recovered: u64,
    /// Fragments still missing one round trip after being requested (request or resend lost too).
    pub residual: u64,
    /// Frames given up on: a keyframe is needed (last resort).
    pub gave_up: u64,
    /// NACK datagrams' worth of requests sent (fragment indices requested, counting repeats).
    pub requested: u64,
}

/// Puts fragments back together and hands out frames strictly in order.
/// Lost packets are asked for again (NACK); only if that fails does it wait for the next keyframe,
/// since later frames reference the lost one.
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    done: BTreeMap<u32, (Vec<u8>, bool)>,
    next: Option<u32>,
    newest: Option<u32>,
    pub waiting_key: bool,
    pub stats: Stats,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Reassembler {
            partial: BTreeMap::new(),
            done: BTreeMap::new(),
            next: None,
            newest: None,
            waiting_key: true,
            stats: Stats::default(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_fragment(
        &mut self,
        now: Instant,
        fid: u32,
        idx: u16,
        count: u16,
        key: bool,
        data: &[u8],
        out: &mut Vec<(Vec<u8>, bool)>,
    ) {
        if self.next.is_some_and(|n| fid < n) || self.done.contains_key(&fid) || count == 0 || idx >= count {
            return;
        }
        // Frames far older than the newest are stale (e.g. a late retransmit of something given up on).
        if self.newest.is_some_and(|nw| fid.saturating_add(120) < nw) {
            return;
        }
        self.newest = Some(self.newest.map_or(fid, |nw| nw.max(fid)));
        let p = self.partial.entry(fid).or_insert_with(|| Partial::new(now, count, key));
        if p.count == 0 {
            // Placeholder gets its real shape now.
            *p = Partial { first: p.first, last_nack: p.last_nack, rounds: p.rounds, ..Partial::new(now, count, key) };
        }
        if p.count != count || p.frags[idx as usize].is_some() {
            return;
        }
        p.frags[idx as usize] = Some(data.to_vec());
        p.got += 1;
        p.last_rx = now;
        if p.asked[idx as usize] {
            self.stats.recovered += 1;
        }
        self.stats.received += 1;
        if p.got == p.count {
            let p = self.partial.remove(&fid).unwrap();
            let mut buf = Vec::with_capacity(p.count as usize * MAX_PAYLOAD);
            for f in p.frags {
                buf.extend_from_slice(&f.unwrap());
            }
            self.done.insert(fid, (buf, p.key));
        }
        // Bound memory if the stream is badly broken.
        while self.partial.len() > 120 {
            self.partial.pop_first();
        }
        self.drain(out);
    }

    /// Which packets to ask for: (frame, fragment indices; empty = the whole frame).
    /// A request is repeated after a round trip if the packet still hasn't come.
    pub fn nacks(&mut self, now: Instant, rtt: Duration) -> Vec<(u32, Vec<u16>)> {
        let rtt = assume_rtt(rtt);
        let mut out = Vec::new();
        let Some(newest) = self.newest else { return out };
        let from = self.next.unwrap_or_else(|| self.partial.keys().next().copied().unwrap_or(newest));
        let from = from.max(newest.saturating_sub(120));
        if !self.waiting_key {
            // Frames we've never seen a packet of, while later ones are arriving: lost entirely.
            for fid in from..newest {
                if !self.done.contains_key(&fid) && !self.partial.contains_key(&fid) {
                    let mut ph = Partial::new(now, 0, false);
                    ph.hole_since = Some(now);
                    self.partial.insert(fid, ph);
                }
            }
        }
        let retry = rtt + Duration::from_millis(15);
        // If the packets of the newest frame stop coming for this long, its tail was lost too.
        let tail_idle = rtt + Duration::from_millis(20);
        for (&fid, p) in self.partial.range_mut(from..) {
            // While waiting for a keyframe only the keyframe (and what follows it) is worth repairing.
            if self.waiting_key && !p.key {
                continue;
            }
            if p.rounds >= MAX_ROUNDS || p.last_nack.is_some_and(|t| now.duration_since(t) < retry) {
                continue;
            }
            if p.count == 0 {
                if fid < newest && p.hole_since.is_some_and(|t| now.duration_since(t) >= REORDER_DELAY) {
                    out.push((fid, Vec::new()));
                    p.last_nack = Some(now);
                    p.rounds += 1;
                    self.stats.requested += 1;
                    if p.rounds == 1 {
                        self.stats.missed += 1; // size unknown: counted as one packet
                    } else {
                        self.stats.residual += 1;
                    }
                }
                continue;
            }
            // Packets go out in order, so a hole below the highest received index is a loss, not a packet
            // still on its way; once a later frame has started (or the frame went quiet) the tail counts too.
            let limit = if fid < newest || now.duration_since(p.last_rx) > tail_idle {
                p.count
            } else {
                p.frags.iter().rposition(|f| f.is_some()).map_or(0, |i| i as u16)
            };
            let missing: Vec<u16> = (0..limit).filter(|&i| p.frags[i as usize].is_none()).collect();
            if missing.is_empty() {
                p.hole_since = None;
                continue;
            }
            let since = *p.hole_since.get_or_insert(now);
            if now.duration_since(since) < REORDER_DELAY {
                continue;
            }
            if p.rounds == 0 {
                self.stats.missed += missing.len() as u64;
            } else {
                self.stats.residual += missing.len() as u64;
            }
            for &i in &missing {
                p.asked[i as usize] = true;
            }
            self.stats.requested += missing.len() as u64;
            out.push((fid, missing));
            p.last_nack = Some(now);
            p.rounds += 1;
        }
        out
    }

    /// A frame that can't be completed (repairs failed) must not stall the stream forever.
    pub fn check_timeout(&mut self, now: Instant, rtt: Duration, out: &mut Vec<(Vec<u8>, bool)>) {
        if self.waiting_key {
            return;
        }
        let rtt = assume_rtt(rtt);
        if let Some(n) = self.next {
            let gave_up = self.partial.get(&n).is_some_and(|p| {
                now.duration_since(p.first) > FRAME_DEADLINE
                    || (p.rounds >= MAX_ROUNDS
                        && p.last_nack.is_some_and(|t| now.duration_since(t) > rtt + Duration::from_millis(30)))
            });
            if gave_up {
                self.lose();
                self.drain(out);
            }
        }
    }

    /// The consumer could not keep up (decoder queue full): skip ahead to a keyframe.
    pub fn force_resync(&mut self) {
        if !self.waiting_key {
            self.lose();
        }
    }

    fn lose(&mut self) {
        self.waiting_key = true;
        self.stats.gave_up += 1;
    }

    fn drain(&mut self, out: &mut Vec<(Vec<u8>, bool)>) {
        loop {
            if self.waiting_key {
                let Some(k) = self.done.iter().find(|(_, (_, key))| *key).map(|(f, _)| *f) else {
                    // Keep only what could follow a keyframe that is still arriving.
                    match self.partial.iter().find(|(_, p)| p.key).map(|(f, _)| *f) {
                        Some(kp) => self.done.retain(|&f, _| f > kp),
                        None => self.done.clear(),
                    }
                    return;
                };
                self.done = self.done.split_off(&k);
                self.partial = self.partial.split_off(&k);
                self.next = Some(k);
                self.waiting_key = false;
            }
            let n = self.next.unwrap();
            if let Some(f) = self.done.remove(&n) {
                out.push(f);
                self.next = Some(n + 1);
                self.partial = self.partial.split_off(&(n + 1));
                continue;
            }
            // Missing frames get retransmitted; only if we're a whole second ahead is it hopeless.
            if self.done.last_key_value().is_some_and(|(&last, _)| last >= n + 60) {
                self.lose();
                continue;
            }
            return;
        }
    }
}

/// Until the first ping/pong answer there is no measurement: assume a slowish path, so that the very first
/// keyframe isn't re-requested before its repairs could possibly have arrived.
fn assume_rtt(rtt: Duration) -> Duration {
    if rtt.is_zero() {
        Duration::from_millis(100)
    } else {
        rtt
    }
}

/// Keyframe requests as a last resort: the first goes out at once, repeats back off
/// (0.3 s, 0.45 s, ... up to 3 s) and the backoff only relaxes after the stream has been stable for a while.
/// This is what prevents the "huge keyframe lost -> request another" loop.
pub struct KeyRequester {
    last: Option<Instant>,
    interval: Duration,
    stable_since: Option<Instant>,
    pub sent: u64,
}

impl KeyRequester {
    pub const BASE: Duration = Duration::from_millis(300);
    pub const MAX: Duration = Duration::from_secs(3);
    const RELAX_AFTER: Duration = Duration::from_secs(5);

    pub fn new() -> Self {
        KeyRequester { last: None, interval: Self::BASE, stable_since: None, sent: 0 }
    }

    /// Call periodically; true = send a keyframe request now.
    pub fn poll(&mut self, now: Instant, waiting_key: bool) -> bool {
        if !waiting_key {
            let since = *self.stable_since.get_or_insert(now);
            if now.duration_since(since) > Self::RELAX_AFTER {
                self.interval = Self::BASE;
            }
            return false;
        }
        self.stable_since = None;
        if self.last.is_some_and(|t| now.duration_since(t) < self.interval) {
            return false;
        }
        // Escalate only for repeats within a short time; a request after a long calm period starts afresh.
        self.last = Some(now);
        self.interval = (self.interval.mul_f64(1.5)).min(Self::MAX);
        self.sent += 1;
        true
    }
}

impl Default for KeyRequester {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(r: &mut Reassembler, t: Instant, fid: u32, key: bool, n: u16, out: &mut Vec<(Vec<u8>, bool)>) {
        for i in 0..n {
            r.on_fragment(t, fid, i, n, key, &[fid as u8], out);
        }
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn in_order_after_key() {
        let t = Instant::now();
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        feed(&mut r, t, 0, false, 2, &mut out); // non-key before any keyframe is useless
        assert!(out.is_empty());
        feed(&mut r, t, 1, true, 3, &mut out);
        feed(&mut r, t, 2, false, 2, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0].1);
        assert_eq!(out[0].0, vec![1, 1, 1]);
    }

    #[test]
    fn loss_waits_for_key() {
        let t = Instant::now();
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        feed(&mut r, t, 0, true, 1, &mut out);
        r.on_fragment(t, 1, 0, 2, false, &[1], &mut out); // frame 1 never completes
        for f in 2..62 {
            feed(&mut r, t, f, false, 1, &mut out);
        }
        // A whole second of newer frames and still no frame 1: give up, wait for a keyframe.
        assert_eq!(out.len(), 1);
        assert!(r.waiting_key);
        feed(&mut r, t, 62, false, 1, &mut out);
        feed(&mut r, t, 63, true, 1, &mut out);
        feed(&mut r, t, 64, false, 1, &mut out);
        assert_eq!(out.len(), 3);
        assert!(out[1].1);
        assert!(!r.waiting_key);
    }

    #[test]
    fn nacks_holes_and_missing_frames_then_recovers() {
        let t = Instant::now();
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        feed(&mut r, t, 0, true, 1, &mut out);
        r.on_fragment(t, 1, 0, 3, false, &[1], &mut out);
        r.on_fragment(t, 1, 2, 3, false, &[1], &mut out); // fragment 1 of frame 1 lost
        feed(&mut r, t, 3, false, 1, &mut out); // frame 2 lost entirely
        assert!(r.nacks(t, Duration::ZERO).is_empty(), "reorder delay: not yet");
        let t2 = t + ms(5);
        let mut n = r.nacks(t2, Duration::ZERO);
        n.sort();
        assert_eq!(n, vec![(1, vec![1]), (2, vec![])]);
        assert!(r.nacks(t2 + ms(5), Duration::ZERO).is_empty(), "no repeat before a round trip has passed");
        // Retransmissions arrive: everything comes out in order, no keyframe needed.
        r.on_fragment(t2 + ms(8), 1, 1, 3, false, &[1], &mut out);
        feed(&mut r, t2 + ms(8), 2, false, 2, &mut out);
        assert_eq!(out.len(), 4);
        assert!(!r.waiting_key);
        assert_eq!(r.stats.gave_up, 0);
        assert_eq!(r.stats.recovered, 1 + 0); // frame 2 was a whole-frame request (unknown shape)
    }

    #[test]
    fn lost_tail_of_quiet_frame_is_requested() {
        let t = Instant::now();
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        feed(&mut r, t, 0, true, 1, &mut out);
        r.on_fragment(t, 1, 0, 3, false, &[1], &mut out); // fragments 1, 2 never come, nothing newer follows
        assert!(r.nacks(t + ms(10), ms(16)).is_empty(), "still within a packet gap");
        assert!(r.nacks(t + ms(40), ms(16)).is_empty(), "the frame just went quiet: reorder delay starts");
        let n = r.nacks(t + ms(60), ms(16));
        assert_eq!(n, vec![(1, vec![1, 2])]);
    }

    #[test]
    fn gives_up_after_rounds_then_requests_back_off() {
        let t = Instant::now();
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        feed(&mut r, t, 0, true, 1, &mut out);
        r.on_fragment(t, 1, 0, 2, false, &[1], &mut out);
        feed(&mut r, t, 2, false, 1, &mut out);
        let mut now = t;
        for _ in 0..40 {
            now += ms(40);
            r.nacks(now, ms(16));
            r.check_timeout(now, ms(16), &mut out);
        }
        assert!(r.waiting_key);
        assert_eq!(r.stats.gave_up, 1);
        // Keyframe requests: first at once, then spaced out.
        let mut k = KeyRequester::new();
        let mut sent = 0;
        let mut now = t;
        for _ in 0..100 {
            now += ms(100);
            if k.poll(now, true) {
                sent += 1;
            }
        }
        assert!(sent <= 8, "10 s of waiting must not mean 33 requests like before (got {sent})");
        assert!(sent >= 3);
    }
}
