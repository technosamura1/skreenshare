// Audio packets with loss protection: every datagram carries its own samples (16-bit) plus a compact
// copy (8-bit mu-law) of the previous datagram. One lost datagram is rebuilt from the next one; a longer
// hole is filled with a faded repeat + silence of exactly the missing length, so the playback timeline
// never shifts and the jitter buffer doesn't "jerk".
// Pure logic (no sockets / audio devices): shared by the real program and tools/lossim.
use crate::net;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Samples per datagram (interleaved stereo): 120 frames = 2.5 ms.
pub const AUDIO_CHUNK: usize = 240;
/// A hole longer than this many datagrams (0.5 s) is not "loss" but a restart of the stream: don't fill it.
const MAX_FILL: u32 = 200;
/// Fade after a concealed hole, in samples (interleaved): ~1 ms.
const FADE_IN: usize = 96;

pub fn lin2ulaw(s: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let mut x = s as i32;
    let sign = if x < 0 {
        x = -x;
        0x80
    } else {
        0
    };
    x = x.min(CLIP) + BIAS;
    let mut exp = 7;
    let mut mask = 0x4000;
    while exp > 0 && x & mask == 0 {
        exp -= 1;
        mask >>= 1;
    }
    let man = (x >> (exp + 3)) & 0x0F;
    !((sign | (exp << 4) | man) as u8)
}

pub fn ulaw2lin(u: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let u = !u as i32;
    let exp = (u >> 4) & 7;
    let man = u & 0x0F;
    let s = ((((man << 3) + BIAS) << exp) - BIAS) as i16;
    if u & 0x80 != 0 {
        -s
    } else {
        s
    }
}

/// Packetizer on the host. `seq` is shared so numbering continues if the capture is restarted.
pub struct AudioTx {
    token: u32,
    seq: Arc<AtomicU32>,
    pending: Vec<i16>,
    prev: Vec<u8>,
    pkt: Vec<u8>,
}

impl AudioTx {
    pub fn new(token: u32, seq: Arc<AtomicU32>) -> Self {
        AudioTx { token, seq, pending: Vec::new(), prev: Vec::new(), pkt: Vec::with_capacity(1100) }
    }

    /// Feed captured samples; `send` gets every finished datagram.
    pub fn push(&mut self, samples: &[i16], rate: u32, mut send: impl FnMut(&[u8])) {
        self.pending.extend_from_slice(samples);
        let mut used = 0;
        while self.pending.len() - used >= AUDIO_CHUNK {
            let cur = &self.pending[used..used + AUDIO_CHUNK];
            used += AUDIO_CHUNK;
            net::write_header(&mut self.pkt, net::PT_AUDIO2, self.token);
            self.pkt.extend_from_slice(&self.seq.fetch_add(1, Ordering::Relaxed).to_le_bytes());
            self.pkt.extend_from_slice(&rate.to_le_bytes());
            self.pkt.extend_from_slice(&(AUDIO_CHUNK as u16).to_le_bytes());
            for s in cur {
                self.pkt.extend_from_slice(&s.to_le_bytes());
            }
            self.pkt.extend_from_slice(&self.prev);
            self.prev.clear();
            self.prev.extend(cur.iter().map(|&s| lin2ulaw(s)));
            send(&self.pkt);
        }
        self.pending.drain(..used);
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct AudioStats {
    pub received: u64,
    /// Lost datagrams rebuilt from the next one's copy.
    pub recovered: u64,
    /// Lost datagrams replaced by a faded repeat / silence of the right length.
    pub concealed: u64,
    /// Late or duplicate datagrams dropped.
    pub late: u64,
}

/// Depacketizer on the viewer.
///
/// Small reorder window: a datagram that arrives ahead of its predecessor is held for up to `REORDER`
/// following datagrams (7.5 ms); only then is the hole declared lost and rebuilt/concealed.
pub struct AudioRx {
    next: Option<u32>,
    held: BTreeMap<u32, Vec<u8>>,
    last: Vec<i16>,
    pub stats: AudioStats,
}

impl Default for AudioRx {
    fn default() -> Self {
        Self::new()
    }
}

struct Parsed<'a> {
    rate: u32,
    n: usize,
    cur: &'a [u8],
    prev: &'a [u8],
}

fn parse(body: &[u8]) -> Option<(u32, Parsed<'_>)> {
    if body.len() < 10 {
        return None;
    }
    let seq = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
    let n = u16::from_le_bytes([body[8], body[9]]) as usize;
    let cur = body.get(10..10 + n * 2)?;
    Some((seq, Parsed { rate, n, cur, prev: &body[10 + n * 2..] }))
}

/// Held datagrams allowed before a hole is declared lost.
const REORDER: usize = 3;

impl AudioRx {
    pub fn new() -> Self {
        AudioRx { next: None, held: BTreeMap::new(), last: Vec::new(), stats: AudioStats::default() }
    }

    /// `body` is the datagram without the 5-byte header: seq u32 | rate u32 | n u16 | n samples | n mu-law bytes.
    /// `sink(samples, rate)` receives the audio in playback order, gap-free.
    pub fn on_packet(&mut self, body: &[u8], mut sink: impl FnMut(&[i16], u32)) {
        let Some((seq, _)) = parse(body) else { return };
        let expect = *self.next.get_or_insert(seq);
        if seq < expect || self.held.contains_key(&seq) {
            self.stats.late += 1; // late or duplicate: that slot was already filled
            return;
        }
        if seq - expect > MAX_FILL {
            // Not loss but a restart of the stream (or a wild jump): resync, don't synthesize seconds of silence.
            self.held.clear();
            self.next = Some(seq);
        }
        self.held.insert(seq, body.to_vec());
        self.drain(false, &mut sink);
    }

    /// No audio for a while: don't keep the last few datagrams hostage, release them (holes get filled).
    pub fn flush(&mut self, mut sink: impl FnMut(&[i16], u32)) {
        self.drain(true, &mut sink);
    }

    fn drain(&mut self, all: bool, sink: &mut impl FnMut(&[i16], u32)) {
        loop {
            let Some(next) = self.next else { return };
            let Some((&first, _)) = self.held.first_key_value() else { return };
            if first != next && !all && self.held.len() <= REORDER {
                return; // wait: the missing one may still be on its way
            }
            let body = self.held.remove(&first).unwrap();
            self.emit(next, first, &body, sink);
        }
    }

    fn emit(&mut self, expect: u32, seq: u32, body: &[u8], sink: &mut impl FnMut(&[i16], u32)) {
        let (_, p) = parse(body).unwrap();
        let mut cur: Vec<i16> = p.cur.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        let mut fade_in = false;
        if seq > expect {
            let missing = (seq - expect) as usize;
            let have_prev = p.prev.len() == p.n && p.n > 0;
            let concealed = missing - have_prev as usize;
            for k in 0..concealed {
                let mut c = vec![0i16; p.n];
                if k == 0 && self.last.len() == p.n {
                    // Repeat what we just played, fading to silence: no click at the seam.
                    for (i, v) in c.iter_mut().enumerate() {
                        *v = (self.last[i] as f32 * (1.0 - i as f32 / p.n as f32)) as i16;
                    }
                }
                sink(&c, p.rate);
                self.stats.concealed += 1;
            }
            if have_prev {
                let mut pv: Vec<i16> = p.prev.iter().map(|&b| ulaw2lin(b)).collect();
                if concealed > 0 {
                    fade(&mut pv);
                }
                sink(&pv, p.rate);
                self.last = pv;
                self.stats.recovered += 1;
            } else {
                fade_in = concealed > 0;
            }
        }
        if fade_in {
            fade(&mut cur);
        }
        sink(&cur, p.rate);
        self.last = cur;
        self.next = Some(seq.wrapping_add(1));
        self.stats.received += 1;
    }
}

fn fade(v: &mut [i16]) {
    let m = FADE_IN.min(v.len());
    for (i, s) in v[..m].iter_mut().enumerate() {
        *s = (*s as f32 * i as f32 / m as f32) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulaw_roundtrip_is_close() {
        for s in (-32768i32..32768).step_by(7) {
            let s = s as i16;
            let r = ulaw2lin(lin2ulaw(s)) as i32;
            let err = (r - s as i32).abs();
            assert!(err <= 700 && err as f64 <= (s as i32).abs() as f64 * 0.04 + 40.0, "{s} -> {r}");
        }
    }

    fn stream(n_chunks: usize) -> Vec<i16> {
        (0..n_chunks * AUDIO_CHUNK / 2)
            .flat_map(|i| {
                let v = ((i as f32 * 0.05).sin() * 12000.0) as i16;
                [v, v]
            })
            .collect()
    }

    fn packets(n_chunks: usize) -> Vec<Vec<u8>> {
        let mut tx = AudioTx::new(1, Arc::new(AtomicU32::new(0)));
        let mut out = Vec::new();
        tx.push(&stream(n_chunks), 48000, |p| out.push(p[net::HEADER_LEN..].to_vec()));
        out
    }

    #[test]
    fn one_lost_packet_is_rebuilt_and_timeline_keeps_length() {
        let pk = packets(20);
        let mut rx = AudioRx::new();
        let mut got: Vec<i16> = Vec::new();
        for (i, p) in pk.iter().enumerate() {
            if i == 7 {
                continue;
            }
            rx.on_packet(p, |s, _| got.extend_from_slice(s));
        }
        rx.flush(|s, _| got.extend_from_slice(s));
        assert_eq!(got.len(), 20 * AUDIO_CHUNK);
        assert_eq!(rx.stats.recovered, 1);
        assert_eq!(rx.stats.concealed, 0);
        let src = stream(20);
        let err: i32 = (7 * AUDIO_CHUNK..8 * AUDIO_CHUNK).map(|i| (got[i] as i32 - src[i] as i32).abs()).max().unwrap();
        assert!(err < 700, "rebuilt chunk is close to the original (max err {err})");
        assert_eq!(got[..7 * AUDIO_CHUNK], src[..7 * AUDIO_CHUNK]);
    }

    #[test]
    fn longer_hole_is_filled_to_exact_length() {
        let pk = packets(40);
        let mut rx = AudioRx::new();
        let mut got: Vec<i16> = Vec::new();
        for (i, p) in pk.iter().enumerate() {
            if (10..14).contains(&i) {
                continue;
            }
            rx.on_packet(p, |s, _| got.extend_from_slice(s));
        }
        rx.flush(|s, _| got.extend_from_slice(s));
        assert_eq!(got.len(), 40 * AUDIO_CHUNK);
        assert_eq!(rx.stats.recovered, 1);
        assert_eq!(rx.stats.concealed, 3);
        // The concealed part decays, no jump back to full level at the seam.
        let seam = 10 * AUDIO_CHUNK;
        assert!(got[seam + AUDIO_CHUNK - 1].abs() < 400);
    }

    #[test]
    fn duplicates_and_reordered_are_dropped() {
        let pk = packets(5);
        let mut rx = AudioRx::new();
        let mut got = 0usize;
        for i in [0usize, 1, 1, 2, 0, 3, 4] {
            rx.on_packet(&pk[i], |s, _| got += s.len());
        }
        assert_eq!(got, 5 * AUDIO_CHUNK);
        assert_eq!(rx.stats.late, 2);
    }

    #[test]
    fn reordered_packets_are_played_in_order_without_concealment() {
        let pk = packets(12);
        let mut rx = AudioRx::new();
        let mut got: Vec<i16> = Vec::new();
        for i in [0usize, 2, 1, 3, 5, 6, 4, 7, 9, 8, 10, 11] {
            rx.on_packet(&pk[i], |s, _| got.extend_from_slice(s));
        }
        assert_eq!(got.len(), 12 * AUDIO_CHUNK);
        assert_eq!(rx.stats.concealed + rx.stats.recovered, 0);
        assert_eq!(got, stream(12));
    }
}
