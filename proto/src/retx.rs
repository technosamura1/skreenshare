//! Sender side of loss recovery: remember the last few hundred milliseconds of video packets and re-send the
//! ones the receiver asks for (NACK), at a limited rate.

use crate::{FLAG_RETX, NACK_ALL, VIDEO_HEADER_LEN};
use std::collections::VecDeque;

struct Stored {
    frame_id: u32,
    t_ms: u64,
    pkts: Vec<Option<Vec<u8>>>,
    last_retx: Vec<Option<u64>>,
}

/// Ring of recent video datagrams (whole packets as sent), addressed by `(frame_id, idx)`.
pub struct RetxBuffer {
    window_ms: u64,
    frames: VecDeque<Stored>,
    pub bytes: usize,
}

impl RetxBuffer {
    pub fn new(window_ms: u64) -> Self {
        RetxBuffer { window_ms, frames: VecDeque::new(), bytes: 0 }
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }

    /// Remember a packet that was just sent. `count` = number of fragments of the frame.
    pub fn store(&mut self, now_ms: u64, frame_id: u32, idx: u16, count: u16, pkt: &[u8]) {
        while let Some(f) = self.frames.front() {
            if now_ms.saturating_sub(f.t_ms) > self.window_ms {
                let f = self.frames.pop_front().unwrap();
                self.bytes -= f.pkts.iter().flatten().map(|p| p.len()).sum::<usize>();
            } else {
                break;
            }
        }
        if self.frames.back().map(|f| f.frame_id) != Some(frame_id) {
            self.frames.push_back(Stored {
                frame_id,
                t_ms: now_ms,
                pkts: vec![None; count as usize],
                last_retx: vec![None; count as usize],
            });
        }
        let f = self.frames.back_mut().unwrap();
        if let Some(slot) = f.pkts.get_mut(idx as usize) {
            if slot.is_none() {
                self.bytes += pkt.len();
            }
            *slot = Some(pkt.to_vec());
        }
    }

    /// Appends the datagrams to re-send for one NACK item (a single fragment, or all of a frame for [`NACK_ALL`]).
    /// A packet already re-sent less than `min_gap_ms` ago is skipped (the earlier copy is probably still in flight).
    /// The copies carry [`FLAG_RETX`]. Unknown / too old packets are silently ignored.
    pub fn collect(&mut self, now_ms: u64, (frame_id, idx): (u32, u16), min_gap_ms: u64, out: &mut Vec<Vec<u8>>) {
        let Some(f) = self.frames.iter_mut().rev().find(|f| f.frame_id == frame_id) else { return };
        let range = if idx == NACK_ALL { 0..f.pkts.len() } else { (idx as usize)..(idx as usize + 1) };
        for i in range {
            let (Some(Some(p)), Some(last)) = (f.pkts.get(i), f.last_retx.get_mut(i)) else { continue };
            if last.is_some_and(|t| now_ms.saturating_sub(t) < min_gap_ms) {
                continue;
            }
            *last = Some(now_ms);
            let mut c = p.clone();
            c[VIDEO_HEADER_LEN - 1] |= FLAG_RETX;
            out.push(c);
        }
    }
}

/// Token bucket for re-sent bytes, so retransmissions can never eat the whole link.
pub struct RetxLimiter {
    tokens: f64,
    burst: f64,
    last_ms: u64,
}

impl RetxLimiter {
    pub fn new(burst_bytes: f64) -> Self {
        RetxLimiter { tokens: burst_bytes, burst: burst_bytes, last_ms: 0 }
    }

    /// May `bytes` go out now? `rate` is in bytes per second.
    pub fn allow(&mut self, now_ms: u64, bytes: usize, rate: f64) -> bool {
        let dt = now_ms.saturating_sub(self.last_ms) as f64 / 1000.0;
        self.last_ms = now_ms;
        self.tokens = (self.tokens + dt * rate).min(self.burst);
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_header, parse_video, write_video_header, VideoFrag};

    fn pkt(frame: u32, idx: u16, count: u16) -> Vec<u8> {
        let mut o = Vec::new();
        write_video_header(7, &VideoFrag { ts_ms: 0, frame_id: frame, idx, count, key: false }, &mut o);
        o.push(idx as u8);
        o
    }

    #[test]
    fn resends_only_what_was_asked_for_and_marks_it() {
        let mut b = RetxBuffer::new(300);
        for i in 0..3 {
            b.store(0, 5, i, 3, &pkt(5, i, 3));
        }
        let mut out = Vec::new();
        b.collect(10, (5, 1), 30, &mut out);
        assert_eq!(out.len(), 1);
        let (_, _, body) = parse_header(&out[0]).unwrap();
        let (f, payload) = parse_video(body).unwrap();
        assert_eq!((f.frame_id, f.idx, payload), (5, 1, &[1u8][..]));
        assert_eq!(out[0][VIDEO_HEADER_LEN - 1] & FLAG_RETX, FLAG_RETX);
        // asking again at once does not duplicate it; after the gap it does
        out.clear();
        b.collect(20, (5, 1), 30, &mut out);
        assert!(out.is_empty());
        b.collect(45, (5, 1), 30, &mut out);
        assert_eq!(out.len(), 1);
        // whole frame
        out.clear();
        b.collect(100, (5, NACK_ALL), 30, &mut out);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn old_frames_are_evicted_and_unknown_ignored() {
        let mut b = RetxBuffer::new(300);
        b.store(0, 1, 0, 1, &pkt(1, 0, 1));
        b.store(400, 2, 0, 1, &pkt(2, 0, 1));
        let mut out = Vec::new();
        b.collect(401, (1, 0), 30, &mut out);
        b.collect(401, (9, 0), 30, &mut out);
        b.collect(401, (2, 7), 30, &mut out);
        assert!(out.is_empty());
        assert_eq!(b.bytes, pkt(2, 0, 1).len());
        b.collect(401, (2, 0), 30, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn limiter_caps_the_rate() {
        let mut l = RetxLimiter::new(10_000.0);
        let rate = 100_000.0; // 100 kB/s
        let mut sent = 0;
        for ms in 0..1000u64 {
            while l.allow(ms, 1000, rate) {
                sent += 1000;
            }
        }
        // burst + one second of rate
        assert!((100_000..=111_000).contains(&sent), "{sent}");
    }
}
