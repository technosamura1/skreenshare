// Opus audio over UDP (PT_AUDIO3), the replacement for the hand-made mu-law redundancy of audiopkt.rs.
//
// Why: 2.5 ms datagrams with one-deep redundancy lose a whole "word end" whenever two datagrams in a row vanish
// (which is exactly what a video keyframe burst does to a small router queue), and the repair is a repeat of
// the last waveform. Here:
//   - 10 ms Opus frames, 48 kHz stereo, ~128 kbit/s: one datagram carries the newest frame and the two before it
//     (three-deep redundancy, ~400 kbit/s in total - 2.5% of a 15 Mbit/s stream);
//   - a hole that redundancy cannot fill (3+ datagrams in a row) is requested again (PT_ANACK) and waits for
//     the answer for a bounded time (the "deadline": about one RTT, 30..60 ms) before giving up;
//   - what is still missing is concealed by libopus' own PLC (pitch repetition, smooth fade), frame by frame.
// Pure logic (no sockets, no audio devices): used by the program and by tools/lossim tests.
use crate::net;
use crate::opus;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Interleaved stereo samples in one frame.
pub const FRAME_SAMPLES: usize = opus::FRAME * 2;
/// Frames carried by one datagram (the newest + this many minus one older ones).
pub const DEPTH: usize = 3;
/// Frames the host remembers for audio NACKs (1 s).
const HISTORY: usize = 100;
/// A jump this far ahead is a restart of the stream, not loss: resync instead of synthesizing seconds of PLC.
const MAX_FILL: u32 = 100;
pub const MIN_WAIT: Duration = Duration::from_millis(30);
pub const MAX_WAIT: Duration = Duration::from_millis(60);

/// Recent encoded frames, shared between the capture thread (pushes) and the control thread (answers NACKs).
#[derive(Clone, Default)]
pub struct History(Arc<Mutex<VecDeque<(u32, Vec<u8>)>>>);

fn write_datagram(out: &mut Vec<u8>, token: u32, seq: u32, frames: &[&[u8]]) {
    net::write_header(out, net::PT_AUDIO3, token);
    out.extend_from_slice(&seq.to_le_bytes());
    out.push(frames.len() as u8);
    for f in frames {
        out.extend_from_slice(&(f.len() as u16).to_le_bytes());
        out.extend_from_slice(f);
    }
}

impl History {
    fn push(&self, seq: u32, data: &[u8]) {
        let mut h = self.0.lock().unwrap();
        h.push_back((seq, data.to_vec()));
        while h.len() > HISTORY {
            h.pop_front();
        }
    }

    /// The datagram that carried (or would have carried) frame `seq` as its newest one, if still remembered.
    pub fn datagram(&self, token: u32, seq: u32, out: &mut Vec<u8>) -> bool {
        let h = self.0.lock().unwrap();
        let Some(pos) = h.iter().position(|(s, _)| *s == seq) else { return false };
        let from = pos.saturating_sub(DEPTH - 1);
        let frames: Vec<&[u8]> = h.range(from..=pos).rev().map(|(_, d)| d.as_slice()).collect();
        write_datagram(out, token, seq, &frames);
        true
    }
}

/// Host: PCM (48 kHz interleaved stereo) in, datagrams out.
pub struct Tx {
    token: u32,
    seq: Arc<AtomicU32>,
    enc: opus::Encoder,
    pending: Vec<i16>,
    recent: VecDeque<Vec<u8>>,
    pub history: History,
    scratch: Vec<u8>,
    pkt: Vec<u8>,
}

impl Tx {
    pub fn new(token: u32, seq: Arc<AtomicU32>, kbps: i32) -> anyhow::Result<Tx> {
        Ok(Tx {
            token,
            seq,
            enc: opus::Encoder::new(kbps, 5)?,
            pending: Vec::new(),
            recent: VecDeque::new(),
            history: History::default(),
            scratch: Vec::new(),
            pkt: Vec::with_capacity(1400),
        })
    }

    pub fn push(&mut self, samples: &[i16], mut send: impl FnMut(&[u8])) {
        self.pending.extend_from_slice(samples);
        let mut used = 0;
        while self.pending.len() - used >= FRAME_SAMPLES {
            if self.enc.encode(&self.pending[used..used + FRAME_SAMPLES], &mut self.scratch).is_err() {
                used += FRAME_SAMPLES;
                continue;
            }
            used += FRAME_SAMPLES;
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            self.history.push(seq, &self.scratch);
            self.recent.push_front(self.scratch.clone());
            self.recent.truncate(DEPTH);
            let frames: Vec<&[u8]> = self.recent.iter().map(|v| v.as_slice()).collect();
            write_datagram(&mut self.pkt, self.token, seq, &frames);
            send(&self.pkt);
        }
        self.pending.drain(..used);
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    /// Datagrams received.
    pub received: u64,
    /// Frames whose own datagram was lost but which arrived inside a later one (redundancy) or as a resend.
    pub recovered: u64,
    /// Frames nobody could supply: libopus concealed them.
    pub concealed: u64,
    pub late: u64,
    /// Frames we asked the host to resend.
    pub requested: u64,
    /// Datagrams missing between the first and last seen sequence numbers (lost on the network), for "потери: звук".
    pub lost_datagrams: u64,
    pub expected_datagrams: u64,
}

struct Held {
    data: Vec<u8>,
    /// Arrived as the newest frame of its own datagram (not as someone's redundant copy).
    direct: bool,
}

pub struct Rx {
    dec: opus::Decoder,
    next: Option<u32>,
    held: BTreeMap<u32, Held>,
    hole_since: Option<Instant>,
    /// seq -> (when last requested, times requested)
    asked: BTreeMap<u32, (Instant, u32)>,
    highest: Option<u32>,
    /// How long a hole that redundancy could not fill is waited for (set from the round trip time).
    pub wait: Duration,
    rtt: Duration,
    pub stats: Stats,
    pcm: Vec<i16>,
}

/// Parses `seq | n | n × (len u16 | data)` (the body after the 5-byte header).
pub fn parse(body: &[u8]) -> Option<(u32, Vec<&[u8]>)> {
    if body.len() < 5 {
        return None;
    }
    let seq = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let n = body[4] as usize;
    if n == 0 || n > 8 {
        return None;
    }
    let mut at = 5;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let len = u16::from_le_bytes(body.get(at..at + 2)?.try_into().unwrap()) as usize;
        at += 2;
        v.push(body.get(at..at + len)?);
        at += len;
    }
    Some((seq, v))
}

impl Rx {
    pub fn new() -> anyhow::Result<Rx> {
        Ok(Rx {
            dec: opus::Decoder::new()?,
            next: None,
            held: BTreeMap::new(),
            hole_since: None,
            asked: BTreeMap::new(),
            highest: None,
            wait: MIN_WAIT,
            rtt: Duration::from_millis(30),
            stats: Stats::default(),
            pcm: Vec::with_capacity(FRAME_SAMPLES),
        })
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
        self.wait = (rtt + Duration::from_millis(20)).clamp(MIN_WAIT, MAX_WAIT);
    }

    /// `body` is the datagram without its header. `sink` gets 48 kHz interleaved stereo in playback order.
    pub fn on_datagram(&mut self, now: Instant, body: &[u8], sink: &mut impl FnMut(&[i16], u32)) {
        let Some((seq, frames)) = parse(body) else { return };
        let expect = *self.next.get_or_insert(seq);
        self.stats.received += 1;
        match self.highest {
            Some(h) if seq > h => {
                self.stats.expected_datagrams += (seq - h) as u64;
                self.stats.lost_datagrams += (seq - h - 1) as u64;
                self.highest = Some(seq);
            }
            None => {
                self.stats.expected_datagrams += 1;
                self.highest = Some(seq);
            }
            _ => {}
        }
        if seq >= expect && seq - expect > MAX_FILL {
            // Restart of the stream (or a wild jump): start over rather than conceal seconds.
            self.held.clear();
            self.asked.clear();
            self.hole_since = None;
            self.next = Some(seq);
            self.highest = Some(seq);
        }
        let expect = self.next.unwrap();
        if seq < expect && !self.held.contains_key(&seq) && frames.len() > 0 {
            self.stats.late += 1;
        }
        for (i, f) in frames.iter().enumerate() {
            let s = match seq.checked_sub(i as u32) {
                Some(s) => s,
                None => continue,
            };
            if s < expect || self.held.contains_key(&s) {
                continue;
            }
            self.held.insert(s, Held { data: f.to_vec(), direct: i == 0 });
            self.asked.remove(&s);
        }
        self.drain(now, sink);
    }

    /// Time passes without new datagrams: give up on holes that waited long enough.
    pub fn tick(&mut self, now: Instant, sink: &mut impl FnMut(&[i16], u32)) {
        self.drain(now, sink);
    }

    fn decode(&mut self, data: Option<&[u8]>, sink: &mut impl FnMut(&[i16], u32)) {
        self.pcm.clear();
        if self.dec.decode(data, false, &mut self.pcm).is_ok() && !self.pcm.is_empty() {
            sink(&self.pcm, opus::RATE);
        }
    }

    fn drain(&mut self, now: Instant, sink: &mut impl FnMut(&[i16], u32)) {
        loop {
            let Some(next) = self.next else { return };
            if let Some(h) = self.held.remove(&next) {
                if !h.direct {
                    self.stats.recovered += 1;
                }
                self.decode(Some(&h.data), sink);
                self.next = Some(next + 1);
                self.hole_since = None;
                continue;
            }
            if self.held.is_empty() {
                self.hole_since = None;
                return;
            }
            // Something newer is waiting but `next` is missing.
            let since = *self.hole_since.get_or_insert(now);
            let overdue = now.duration_since(since) >= self.wait || self.held.len() >= 8;
            if !overdue {
                return;
            }
            self.stats.concealed += 1;
            self.decode(None, sink);
            self.asked.remove(&next);
            self.next = Some(next + 1);
            // consecutive missing frames of the same hole are overdue as well: hole_since stays
        }
    }

    /// Frames to request from the host now (call regularly, at least every few ms). Each frame is asked for at
    /// most twice, the second time after about one RTT without an answer.
    pub fn nacks(&mut self, now: Instant) -> Vec<u32> {
        let (Some(next), Some((&top, _))) = (self.next, self.held.last_key_value()) else { return Vec::new() };
        let mut out = Vec::new();
        for s in next..top {
            if self.held.contains_key(&s) {
                continue;
            }
            let retry = (self.rtt * 3 / 2).max(Duration::from_millis(15));
            match self.asked.get_mut(&s) {
                None => {
                    self.asked.insert(s, (now, 1));
                    out.push(s);
                }
                Some((t, n)) if *n < 2 && now.duration_since(*t) >= retry => {
                    *t = now;
                    *n += 1;
                    out.push(s);
                }
                _ => {}
            }
            if out.len() >= 8 {
                break;
            }
        }
        self.stats.requested += out.len() as u64;
        out
    }
}

pub fn anack_packet(token: u32, seqs: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(6 + seqs.len() * 4);
    net::write_header(&mut v, net::PT_ANACK, token);
    v.push(seqs.len().min(255) as u8);
    for s in seqs.iter().take(255) {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

pub fn parse_anack(body: &[u8]) -> Vec<u32> {
    let Some((&n, rest)) = body.split_first() else { return Vec::new() };
    rest.chunks_exact(4).take(n as usize).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, start: usize) -> Vec<i16> {
        (0..n).flat_map(|i| {
            let v = (((i + start) as f32 * 300.0 * std::f32::consts::TAU / 48000.0).sin() * 8000.0) as i16;
            [v, v]
        }).collect()
    }

    /// Encodes `n` frames, returns the datagram bodies (without header).
    fn make(n: usize) -> (Vec<Vec<u8>>, History) {
        let mut tx = Tx::new(7, Arc::new(AtomicU32::new(0)), 128).unwrap();
        let mut out = Vec::new();
        for k in 0..n {
            tx.push(&tone(opus::FRAME, k * opus::FRAME), |p| out.push(p[net::HEADER_LEN..].to_vec()));
        }
        (out, tx.history.clone())
    }

    struct Run {
        rx: Rx,
        out: Vec<i16>,
        t: Instant,
    }
    impl Run {
        fn new() -> Run {
            Run { rx: Rx::new().unwrap(), out: Vec::new(), t: Instant::now() }
        }
        fn feed(&mut self, body: &[u8]) {
            let out = &mut self.out;
            self.rx.on_datagram(self.t, body, &mut |s, r| {
                assert_eq!(r, 48_000);
                out.extend_from_slice(s)
            });
        }
        fn advance(&mut self, ms: u64) {
            self.t += Duration::from_millis(ms);
            let out = &mut self.out;
            self.rx.tick(self.t, &mut |s, _| out.extend_from_slice(s));
        }
    }

    fn rms(v: &[i16]) -> f64 {
        (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
    }

    #[test]
    fn datagram_is_small_and_in_order_stream_is_gap_free() {
        let (d, _) = make(50);
        assert_eq!(d.len(), 50);
        assert!(d.iter().all(|b| b.len() < 700), "max {}", d.iter().map(|b| b.len()).max().unwrap());
        let mut r = Run::new();
        for b in &d {
            r.feed(b);
            r.advance(10);
        }
        assert_eq!(r.out.len(), 50 * FRAME_SAMPLES);
        assert_eq!((r.rx.stats.recovered, r.rx.stats.concealed), (0, 0));
        assert!(rms(&r.out[40 * FRAME_SAMPLES..]) > 4000.0);
    }

    #[test]
    fn one_or_two_lost_datagrams_in_a_row_are_restored_exactly() {
        let (d, _) = make(60);
        let mut clean = Run::new();
        for b in &d {
            clean.feed(b);
        }
        for lost in [vec![20], vec![20, 21]] {
            let mut r = Run::new();
            for (i, b) in d.iter().enumerate() {
                if !lost.contains(&i) {
                    r.feed(b);
                }
                r.advance(10);
            }
            assert_eq!(r.rx.stats.concealed, 0, "{lost:?}");
            assert_eq!(r.rx.stats.recovered as usize, lost.len());
            assert_eq!(r.out, clean.out, "bit-exact: the redundant copies are the original packets");
        }
    }

    #[test]
    fn long_hole_is_requested_then_filled_by_the_resend_before_the_deadline() {
        let (d, hist) = make(60);
        let mut r = Run::new();
        r.rx.set_rtt(Duration::from_millis(20));
        let lost = [20usize, 21, 22, 23];
        for (i, b) in d.iter().enumerate().take(30) {
            if !lost.contains(&i) {
                r.feed(b);
            }
            r.advance(10);
            if i == 24 {
                // frames 20,21 are only inside datagrams 22/23 (lost): 20 and 21 are the hole; 22/23 too, 24 carries 22..24
                let want = r.rx.nacks(r.t);
                assert!(want.contains(&20) && want.contains(&21), "{want:?}");
                for s in want {
                    let mut p = Vec::new();
                    assert!(hist.datagram(7, s, &mut p));
                    r.feed(&p[net::HEADER_LEN..]);
                }
            }
        }
        assert_eq!(r.rx.stats.concealed, 0);
        assert!(r.rx.stats.requested >= 2);
        assert_eq!(r.out.len(), 30 * FRAME_SAMPLES);
    }

    #[test]
    fn unanswered_hole_is_concealed_after_the_wait_with_audible_signal() {
        let (d, _) = make(60);
        let mut r = Run::new();
        r.rx.set_rtt(Duration::from_millis(10));
        for (i, b) in d.iter().enumerate().take(40) {
            if !(20..=23).contains(&i) {
                r.feed(b);
            }
            r.advance(10);
        }
        assert_eq!(r.out.len(), 40 * FRAME_SAMPLES, "timeline keeps its length");
        assert!(r.rx.stats.concealed >= 1 && r.rx.stats.concealed <= 2, "{:?}", r.rx.stats);
        let hole = &r.out[20 * FRAME_SAMPLES..21 * FRAME_SAMPLES];
        assert!(rms(hole) > 1500.0, "PLC continues the tone, rms {}", rms(hole));
    }

    #[test]
    fn reordering_and_duplicates_are_harmless() {
        let (d, _) = make(30);
        let mut r = Run::new();
        let order = [0usize, 1, 3, 2, 4, 4, 5, 7, 6, 8, 9];
        for &i in &order {
            r.feed(&d[i]);
        }
        r.advance(100);
        assert_eq!(r.out.len(), 10 * FRAME_SAMPLES);
        assert_eq!(r.rx.stats.concealed, 0);
    }

    #[test]
    fn restart_with_a_far_jump_resyncs_without_filling() {
        let (d, _) = make(400);
        let mut r = Run::new();
        for b in &d[..10] {
            r.feed(b);
        }
        r.feed(&d[300]);
        r.advance(10);
        assert!(r.out.len() <= 12 * FRAME_SAMPLES);
        assert_eq!(r.rx.stats.concealed, 0);
    }

    #[test]
    fn anack_roundtrip() {
        let p = anack_packet(9, &[1, 70000, 5]);
        let (ty, body) = net::parse_header(&p, 9).unwrap();
        assert_eq!(ty, net::PT_ANACK);
        assert_eq!(parse_anack(body), vec![1, 70000, 5]);
        assert!(parse_anack(&[]).is_empty());
    }

    #[test]
    fn garbage_is_ignored() {
        let mut r = Run::new();
        r.feed(&[]);
        r.feed(&[1, 2, 3, 4, 5, 200, 1, 0]);
        r.feed(&[0, 0, 0, 0, 0]);
        assert!(r.out.is_empty());
    }
}
