// Host side of the video stream: packetizer, history of recent packets for retransmission, pacing.
// Pure logic with an injected clock, shared by the real sender thread and tools/lossim.
use crate::net::{self, MAX_PAYLOAD};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many recent frames we keep for retransmission (~4 s at 60 fps) ...
const HISTORY_FRAMES: usize = 240;
/// ... but never more than this many bytes.
const HISTORY_BYTES: usize = 48 << 20;
/// A packet that was just resent is not resent again for this long (duplicate requests).
const RESEND_DEDUPE: Duration = Duration::from_millis(8);
/// Retransmissions may use at most this share of the pacer rate (the rest is for new frames).
const RESEND_SHARE: f64 = 0.25;

struct Sent {
    pkt: Vec<u8>,
    last_resend: Option<Instant>,
}

struct HistFrame {
    fid: u32,
    pkts: Vec<Sent>,
}

/// Keyframe requests on the host: served at most once per cooldown (a keyframe is hundreds of packets;
/// answering every request at once would make the congestion that caused it worse).
pub struct KeyGate {
    pending: bool,
    last: Instant,
}

impl KeyGate {
    pub const COOLDOWN: Duration = Duration::from_millis(500);

    pub fn new(now: Instant) -> Self {
        KeyGate { pending: true, last: now.checked_sub(Duration::from_secs(1)).unwrap_or(now) }
    }

    pub fn request(&mut self) {
        self.pending = true;
    }

    /// A keyframe was encoded for another reason (start, new encoder): counts as an answer to pending requests.
    pub fn note_key(&mut self, now: Instant) {
        self.pending = false;
        self.last = now;
    }

    /// True = encode a keyframe now.
    pub fn take(&mut self, now: Instant) -> bool {
        if self.pending && now.duration_since(self.last) >= Self::COOLDOWN {
            self.pending = false;
            self.last = now;
            true
        } else {
            false
        }
    }
}

pub enum Poll {
    /// Send this datagram now.
    Send(Vec<u8>),
    /// Something is queued but the pacer wants us to wait this long.
    Wait(Duration),
    /// Nothing queued.
    Idle,
}

/// Token bucket: sends no faster than `rate` bytes/s, so bursts (keyframes) don't overflow the router
/// queue on the way — that would turn into exactly the packet loss we're trying to avoid.
struct Pacer {
    tokens: f64,
    last: Instant,
}

impl Pacer {
    /// Back-to-back packets allowed after a quiet moment. 48 (62 KB, the old value) overflowed small Wi-Fi/ISP queues
    /// and made the loss we then had to repair; ~12 packets is ~1 ms at 100 Mbit/s.
    const BURST: f64 = 12.0 * 1300.0;

    fn new(now: Instant) -> Pacer {
        Pacer { tokens: Self::BURST, last: now }
    }

    /// Ok = tokens taken; Err(d) = come back in `d`.
    fn take(&mut self, now: Instant, need: usize, rate: f64) -> Result<(), Duration> {
        let need = need as f64;
        self.tokens = (self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * rate).min(Self::BURST);
        self.last = now;
        if self.tokens >= need {
            self.tokens -= need;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((need - self.tokens) / rate))
        }
    }
}

pub struct Sender {
    token: u32,
    history: VecDeque<HistFrame>,
    hist_bytes: usize,
    /// Retransmissions waiting to go out (served before new frames).
    retx_q: VecDeque<Vec<u8>>,
    /// New frame packets waiting to go out.
    new_q: VecDeque<Vec<u8>>,
    pacer: Pacer,
    /// Budget for resends, in bytes (refilled in proportion to the pacer rate).
    resend_tokens: f64,
    resend_last: Instant,
    pub resent: u64,
    /// Requests we did not serve (budget, duplicates, frame no longer in history).
    pub refused: u64,
}

impl Sender {
    pub fn new(token: u32, now: Instant) -> Sender {
        Sender {
            token,
            history: VecDeque::new(),
            hist_bytes: 0,
            retx_q: VecDeque::new(),
            new_q: VecDeque::new(),
            pacer: Pacer::new(now),
            resend_tokens: 64.0 * 1300.0,
            resend_last: now,
            resent: 0,
            refused: 0,
        }
    }

    /// True while the previous frame is still being sent: the caller should not hand over the next one yet.
    pub fn busy(&self) -> bool {
        !self.new_q.is_empty()
    }

    #[allow(dead_code)]
    pub fn queued(&self) -> usize {
        self.new_q.len() + self.retx_q.len()
    }

    /// Splits an encoded frame into datagrams, queues them and remembers them for retransmission.
    pub fn push_frame(&mut self, fid: u32, key: bool, data: &[u8]) {
        let count = data.len().div_ceil(MAX_PAYLOAD).max(1);
        let mut pkts = Vec::with_capacity(count);
        for i in 0..count {
            let chunk = &data[(i * MAX_PAYLOAD).min(data.len())..((i + 1) * MAX_PAYLOAD).min(data.len())];
            let mut pkt = Vec::with_capacity(net::VIDEO_HEADER_LEN + chunk.len());
            net::write_header(&mut pkt, net::PT_VIDEO, self.token);
            pkt.extend_from_slice(&fid.to_le_bytes());
            pkt.extend_from_slice(&(i as u16).to_le_bytes());
            pkt.extend_from_slice(&(count as u16).to_le_bytes());
            pkt.push(key as u8);
            pkt.extend_from_slice(chunk);
            self.hist_bytes += pkt.len();
            self.new_q.push_back(pkt.clone());
            pkts.push(Sent { pkt, last_resend: None });
        }
        self.history.push_back(HistFrame { fid, pkts });
        while self.history.len() > HISTORY_FRAMES || (self.hist_bytes > HISTORY_BYTES && self.history.len() > 1) {
            if let Some(f) = self.history.pop_front() {
                self.hist_bytes -= f.pkts.iter().map(|p| p.pkt.len()).sum::<usize>();
            }
        }
    }

    /// The client asks for these fragments again (empty list = the whole frame).
    pub fn on_nack(&mut self, now: Instant, fid: u32, idxs: &[u16]) {
        let Some(f) = self.history.iter_mut().find(|f| f.fid == fid) else {
            self.refused += 1;
            return;
        };
        let all: Vec<u16>;
        let list = if idxs.is_empty() {
            all = (0..f.pkts.len() as u16).collect();
            &all
        } else {
            idxs
        };
        for &i in list {
            let Some(s) = f.pkts.get_mut(i as usize) else { continue };
            if s.last_resend.is_some_and(|t| now.saturating_duration_since(t) < RESEND_DEDUPE) {
                self.refused += 1;
                continue;
            }
            // Safety valve against a flood of requests.
            if self.retx_q.len() > 4000 {
                self.refused += 1;
                continue;
            }
            s.last_resend = Some(now);
            self.retx_q.push_back(s.pkt.clone());
        }
    }

    /// Next datagram to send at `now`, given the pacer rate in bytes/s.
    pub fn poll(&mut self, now: Instant, rate: f64) -> Poll {
        // Resend budget refills with time.
        self.resend_tokens =
            (self.resend_tokens + now.saturating_duration_since(self.resend_last).as_secs_f64() * rate * RESEND_SHARE)
                .min(128.0 * 1300.0);
        self.resend_last = now;
        // Retransmissions first: the viewer is waiting on them (as long as the budget allows).
        if let Some(p) = self.retx_q.front() {
            let need = p.len() as f64;
            if self.resend_tokens >= need || self.new_q.is_empty() && self.resend_tokens >= 0.0 {
                match self.pacer.take(now, p.len(), rate) {
                    Ok(()) => {
                        self.resend_tokens -= need;
                        self.resent += 1;
                        return Poll::Send(self.retx_q.pop_front().unwrap());
                    }
                    Err(d) => return Poll::Wait(d),
                }
            }
        }
        if let Some(p) = self.new_q.front() {
            return match self.pacer.take(now, p.len(), rate) {
                Ok(()) => Poll::Send(self.new_q.pop_front().unwrap()),
                Err(d) => Poll::Wait(d),
            };
        }
        if self.retx_q.is_empty() {
            Poll::Idle
        } else {
            // Over the resend budget with nothing else to do: wait for tokens.
            Poll::Wait(Duration::from_millis(1))
        }
    }
}

/// NACK datagrams for one frame (a long list is split so each fits one datagram).
pub fn nack_packets(token: u32, fid: u32, idxs: &[u16]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let chunks: Vec<&[u16]> = if idxs.is_empty() { vec![&[][..]] } else { idxs.chunks(500).collect() };
    for c in chunks {
        let mut p = Vec::with_capacity(11 + c.len() * 2);
        net::write_header(&mut p, net::PT_NACK, token);
        p.extend_from_slice(&fid.to_le_bytes());
        p.extend_from_slice(&(c.len() as u16).to_le_bytes());
        for i in c {
            p.extend_from_slice(&i.to_le_bytes());
        }
        out.push(p);
    }
    out
}

/// Body of a PT_NACK datagram (without the 5-byte header) -> (frame, fragment indices).
pub fn parse_nack(body: &[u8]) -> Option<(u32, Vec<u16>)> {
    if body.len() < 6 {
        return None;
    }
    let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let n = u16::from_le_bytes([body[4], body[5]]) as usize;
    let list = body.get(6..6 + n * 2)?;
    Some((fid, list.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()))
}

/// Pacer speed for a given video bitrate: room for keyframe bursts and retransmissions, within the cap.
pub fn pacer_rate(bitrate_mbps: f64, max_send_mbps: u32) -> u64 {
    ((bitrate_mbps * 1.6).max(10.0).min(max_send_mbps as f64) * 125_000.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(s: &mut Sender, mut now: Instant, rate: f64) -> (Vec<Vec<u8>>, Instant) {
        let mut out = Vec::new();
        loop {
            match s.poll(now, rate) {
                Poll::Send(p) => out.push(p),
                Poll::Wait(d) => now += d,
                Poll::Idle => return (out, now),
            }
        }
    }

    #[test]
    fn packetizes_and_resends_only_requested() {
        let t = Instant::now();
        let mut s = Sender::new(7, t);
        let data = vec![9u8; MAX_PAYLOAD * 3 + 10];
        s.push_frame(5, false, &data);
        let (first, now) = drain(&mut s, t, 1e7);
        assert_eq!(first.len(), 4);
        s.on_nack(now, 5, &[1, 3]);
        s.on_nack(now, 5, &[1]); // duplicate within the dedupe window
        let (re, now) = drain(&mut s, now, 1e7);
        assert_eq!(re.len(), 2);
        assert_eq!(re[0], first[1]);
        assert_eq!(re[1], first[3]);
        assert_eq!(s.refused, 1);
        // Whole-frame request, after the dedupe window.
        s.on_nack(now + Duration::from_millis(20), 5, &[]);
        let (re, _) = drain(&mut s, now + Duration::from_millis(20), 1e7);
        assert_eq!(re.len(), 4);
        // Unknown frame: ignored.
        s.on_nack(now, 99, &[0]);
    }

    #[test]
    fn retransmissions_overtake_queued_new_frame() {
        let t = Instant::now();
        let mut s = Sender::new(7, t);
        s.push_frame(1, true, &vec![1u8; MAX_PAYLOAD * 2]);
        let (_, now) = drain(&mut s, t, 1e7);
        s.push_frame(2, false, &vec![2u8; MAX_PAYLOAD * 2]);
        s.on_nack(now, 1, &[0]);
        match s.poll(now, 1e7) {
            Poll::Send(p) => assert_eq!(p[HEADER_FID], 1),
            _ => panic!("expected the resend first"),
        }
    }
    const HEADER_FID: usize = net::HEADER_LEN; // low byte of frame_id
}
