//! Receiver-side frame reassembly with loss recovery (v0.4).
//!
//! Video fragments are identified by `(frame_id, idx)`. Frames are handed out strictly in order. A missing
//! fragment is NOT a reason to ask for a new keyframe any more: after a short reorder delay [`Reassembler::poll`]
//! lists exactly the missing fragments (a NACK) and the sender re-sends only those. Only a frame that is still
//! incomplete after the latency budget is given up ([`Out::Lost`]); the caller then asks for a keyframe, under the
//! rules of [`crate::keyreq::KeyPolicy`].
//!
//! Time is passed in (`now_ms`, any monotonic millisecond clock), so the whole thing is deterministic and testable.
//! `android/.../Proto.kt` (`Reassembler`) mirrors this file.

use crate::{VideoFrag, NACK_ALL};
use std::collections::BTreeMap;

pub struct Frame {
    pub frame_id: u32,
    pub ts_ms: u32,
    pub key: bool,
    pub data: Vec<u8>,
}

pub enum Out {
    Frame(Frame),
    /// One or more frames were abandoned (could not be recovered in time): the decoder needs a keyframe.
    Lost,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// A hole is only reported after this long (packets may just be reordered).
    pub reorder_ms: u64,
    /// Ask again for a still-missing fragment after this long (about one round trip plus the sender's re-send gap).
    pub retry_ms: u64,
    pub max_tries: u8,
    /// A frame that blocks the stream for this long is given up. The latency we are willing to add.
    pub budget_ms: u64,
    /// Hard cap on half-finished frames kept.
    pub max_pending: usize,
    /// False for a sender that cannot re-send (v0.3): never NACK, give up quickly like before.
    pub nack: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { reorder_ms: 8, retry_ms: 40, max_tries: 6, budget_ms: 200, max_pending: 64, nack: true }
    }
}

impl Config {
    pub fn without_nack() -> Self {
        Config { nack: false, budget_ms: 40, ..Config::default() }
    }
}

struct Partial {
    ts_ms: u32,
    key: bool,
    count: u16,
    got: u16,
    frags: Vec<Option<Vec<u8>>>,
    first_ms: u64,
    last_ms: u64,
    /// Highest fragment index received so far (-1 = none).
    high: i32,
    /// When fragment i was found to be missing although a later one arrived (u64::MAX = not (yet) evident).
    evident_at: Vec<u64>,
    nack_at: Vec<u64>,
    tries: Vec<u8>,
}

impl Partial {
    fn new(f: &VideoFrag, now: u64) -> Self {
        let n = f.count as usize;
        Partial {
            ts_ms: f.ts_ms,
            key: f.key,
            count: f.count,
            got: 0,
            frags: vec![None; n],
            first_ms: now,
            last_ms: now,
            high: -1,
            evident_at: vec![u64::MAX; n],
            nack_at: vec![u64::MAX; n],
            tries: vec![0; n],
        }
    }
}

pub struct Reassembler {
    cfg: Config,
    next_id: Option<u32>,
    pending: BTreeMap<u32, Partial>,
    /// Frames of which not a single fragment has arrived: (last NACK time, tries).
    gap_nack: BTreeMap<u32, (u64, u8)>,
    /// Frames given up (unrecoverable).
    pub lost: u32,
    /// Fragments reported missing (each counted once, before any re-send).
    pub nacked_pkts: u32,
    /// Of those, how many showed up afterwards.
    pub recovered_pkts: u32,
    pub dup_pkts: u32,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(cfg: Config) -> Self {
        Reassembler {
            cfg,
            next_id: None,
            pending: BTreeMap::new(),
            gap_nack: BTreeMap::new(),
            lost: 0,
            nacked_pkts: 0,
            recovered_pkts: 0,
            dup_pkts: 0,
        }
    }

    pub fn reset(&mut self) {
        self.next_id = None;
        self.pending.clear();
        self.gap_nack.clear();
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// A keyframe is being received right now (so asking for another one would only add load).
    pub fn key_pending(&self) -> bool {
        self.pending.values().any(|p| p.key)
    }

    pub fn push(&mut self, now_ms: u64, f: &VideoFrag, payload: &[u8], out: &mut Vec<Out>) {
        if f.count == 0 || f.idx >= f.count {
            return;
        }
        let next = *self.next_id.get_or_insert(f.frame_id);
        // Older than what we already delivered/skipped: ignore.
        if (f.frame_id.wrapping_sub(next) as i32) < 0 {
            return;
        }
        if self.gap_nack.remove(&f.frame_id).is_some() {
            self.recovered_pkts += 1;
        }
        let p = self.pending.entry(f.frame_id).or_insert_with(|| Partial::new(f, now_ms));
        if p.count != f.count {
            return;
        }
        let i = f.idx as usize;
        if p.frags[i].is_some() {
            self.dup_pkts += 1;
            return;
        }
        p.frags[i] = Some(payload.to_vec());
        p.got += 1;
        p.last_ms = now_ms;
        if p.nack_at[i] != u64::MAX {
            self.recovered_pkts += 1;
        }
        if (f.idx as i32) > p.high {
            // everything between the old high mark and this fragment is now known to be missing (or reordered)
            for j in (p.high + 1) as usize..i {
                if p.frags[j].is_none() && p.evident_at[j] == u64::MAX {
                    p.evident_at[j] = now_ms;
                }
            }
            p.high = f.idx as i32;
        }
        self.drain(out);
        while self.pending.len() > self.cfg.max_pending {
            self.give_up_head(out);
        }
    }

    /// Call every few milliseconds. Appends the fragments to ask the sender for (`(frame_id, idx)`, idx = [`NACK_ALL`] for a
    /// whole frame) and gives up on frames that exceeded the latency budget.
    pub fn poll(&mut self, now_ms: u64, nacks: &mut Vec<(u32, u16)>, out: &mut Vec<Out>) {
        if self.next_id.is_none() || self.pending.is_empty() {
            return;
        }
        // 1. latency budget
        while let Some(head) = self.first_pending() {
            if now_ms.saturating_sub(self.pending[&head].first_ms) >= self.cfg.budget_ms {
                self.give_up_head(out);
            } else {
                break;
            }
        }
        if !self.cfg.nack {
            return;
        }
        let Some(head) = self.first_pending() else { return };
        let next = self.next_id.unwrap();
        // 2. frames of which nothing arrived (a whole frame lost, or all of its fragments)
        if head != next && now_ms.saturating_sub(self.pending[&head].first_ms) >= self.cfg.reorder_ms {
            let n = head.wrapping_sub(next).min(32);
            for k in 0..n {
                let id = next.wrapping_add(k);
                let e = self.gap_nack.entry(id).or_insert((0, 0));
                if e.1 < self.cfg.max_tries && (e.1 == 0 || now_ms.saturating_sub(e.0) >= self.cfg.retry_ms) {
                    if e.1 == 0 {
                        self.nacked_pkts += 1;
                    }
                    *e = (now_ms, e.1 + 1);
                    nacks.push((id, NACK_ALL));
                }
            }
        }
        // 3. holes inside frames we have started to receive
        for (&id, p) in self.pending.iter_mut() {
            if p.got == p.count {
                continue;
            }
            let tail_quiet = now_ms.saturating_sub(p.last_ms) >= self.cfg.reorder_ms;
            for i in 0..p.count as usize {
                if p.frags[i].is_some() || p.tries[i] >= self.cfg.max_tries {
                    continue;
                }
                let evident = if (i as i32) < p.high {
                    p.evident_at[i] != u64::MAX && now_ms.saturating_sub(p.evident_at[i]) >= self.cfg.reorder_ms
                } else {
                    tail_quiet
                };
                if !evident {
                    continue;
                }
                if p.tries[i] == 0 || now_ms.saturating_sub(p.nack_at[i]) >= self.cfg.retry_ms {
                    if p.tries[i] == 0 {
                        self.nacked_pkts += 1;
                    }
                    p.tries[i] += 1;
                    p.nack_at[i] = now_ms;
                    nacks.push((id, i as u16));
                }
            }
        }
    }

    /// Earliest frame (in stream order) we hold something of.
    fn first_pending(&self) -> Option<u32> {
        let next = self.next_id?;
        self.pending.keys().copied().min_by_key(|id| id.wrapping_sub(next))
    }

    /// The stream is blocked on a hole: abandon what blocks it (missing frames, or the first incomplete frame).
    fn give_up_head(&mut self, out: &mut Vec<Out>) {
        let (Some(next), Some(head)) = (self.next_id, self.first_pending()) else { return };
        if head != next {
            // frames [next, head) never showed up at all
            self.lost += head.wrapping_sub(next);
            self.next_id = Some(head);
        } else {
            self.pending.remove(&head);
            self.lost += 1;
            self.next_id = Some(head.wrapping_add(1));
        }
        out.push(Out::Lost);
        self.drain(out);
    }

    fn drain(&mut self, out: &mut Vec<Out>) {
        while let Some(next) = self.next_id {
            match self.pending.get(&next) {
                Some(p) if p.got == p.count => {}
                _ => break,
            }
            let p = self.pending.remove(&next).unwrap();
            let mut data = Vec::with_capacity(p.frags.iter().map(|f| f.as_ref().map_or(0, |v| v.len())).sum());
            for f in p.frags {
                data.extend_from_slice(&f.unwrap());
            }
            out.push(Out::Frame(Frame { frame_id: next, ts_ms: p.ts_ms, key: p.key, data }));
            self.next_id = Some(next.wrapping_add(1));
        }
        if let Some(next) = self.next_id {
            self.gap_nack.retain(|id, _| (id.wrapping_sub(next) as i32) >= 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(id: u32, idx: u16, count: u16, key: bool) -> VideoFrag {
        VideoFrag { ts_ms: id.wrapping_mul(33), frame_id: id, idx, count, key }
    }

    fn frames(out: &[Out]) -> Vec<(u32, Vec<u8>)> {
        out.iter()
            .filter_map(|o| match o {
                Out::Frame(f) => Some((f.frame_id, f.data.clone())),
                _ => None,
            })
            .collect()
    }

    fn lost(out: &[Out]) -> usize {
        out.iter().filter(|o| matches!(o, Out::Lost)).count()
    }

    #[test]
    fn reassemble_in_order_and_reordered() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(0, &frag(0, 1, 2, true), b"BB", &mut out);
        assert!(out.is_empty());
        r.push(1, &frag(0, 0, 2, true), b"AA", &mut out);
        r.push(2, &frag(1, 0, 1, false), b"C", &mut out);
        assert_eq!(frames(&out), vec![(0, b"AABB".to_vec()), (1, b"C".to_vec())]);
        assert_eq!(r.lost, 0);
        let mut n = Vec::new();
        r.poll(50, &mut n, &mut out);
        assert!(n.is_empty());
    }

    #[test]
    fn missing_fragment_is_nacked_then_recovered_without_loss() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        let mut n = Vec::new();
        r.push(0, &frag(0, 0, 1, true), b"K", &mut out);
        r.push(10, &frag(1, 0, 3, false), b"a", &mut out);
        r.push(11, &frag(1, 2, 3, false), b"c", &mut out); // idx 1 missing
        r.push(12, &frag(2, 0, 1, false), b"P", &mut out); // later frame completes but must wait
        assert_eq!(frames(&out).len(), 1);
        r.poll(15, &mut n, &mut out);
        assert!(n.is_empty(), "reorder delay not over: {n:?}");
        r.poll(20, &mut n, &mut out);
        assert_eq!(n, vec![(1, 1)]);
        n.clear();
        r.poll(30, &mut n, &mut out);
        assert!(n.is_empty(), "no re-ask before retry_ms");
        r.poll(62, &mut n, &mut out);
        assert_eq!(n, vec![(1, 1)]); // asked again: the first retransmission was lost too
        r.push(70, &frag(1, 1, 3, false), b"b", &mut out);
        assert_eq!(frames(&out).iter().map(|f| f.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!((r.lost, lost(&out)), (0, 0));
        assert_eq!((r.nacked_pkts, r.recovered_pkts), (1, 1));
    }

    #[test]
    fn lost_tail_fragment_found_by_silence() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        let mut n = Vec::new();
        r.push(0, &frag(0, 0, 3, true), b"a", &mut out);
        r.push(1, &frag(0, 1, 3, true), b"b", &mut out); // idx 2 never arrives, nothing follows
        r.poll(5, &mut n, &mut out);
        assert!(n.is_empty());
        r.poll(10, &mut n, &mut out);
        assert_eq!(n, vec![(0, 2)]);
    }

    #[test]
    fn whole_missing_frame_is_requested_as_all() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        let mut n = Vec::new();
        r.push(0, &frag(0, 0, 1, true), b"K", &mut out);
        r.push(10, &frag(2, 0, 1, false), b"P", &mut out); // frame 1 vanished completely
        r.poll(12, &mut n, &mut out);
        assert!(n.is_empty());
        r.poll(19, &mut n, &mut out);
        assert_eq!(n, vec![(1, NACK_ALL)]);
        r.push(25, &frag(1, 0, 2, false), b"x", &mut out); // first retransmitted fragment: now it is a normal partial
        r.push(25, &frag(1, 1, 2, false), b"y", &mut out);
        assert_eq!(frames(&out).iter().map(|f| f.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(r.lost, 0);
    }

    #[test]
    fn unrecoverable_frame_is_given_up_only_after_the_budget() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        let mut n = Vec::new();
        r.push(0, &frag(0, 0, 1, true), b"K", &mut out);
        r.push(10, &frag(1, 0, 2, false), b"x", &mut out); // second fragment never comes
        r.push(11, &frag(2, 0, 1, false), b"P", &mut out);
        r.poll(100, &mut n, &mut out);
        assert_eq!(lost(&out), 0);
        r.poll(209, &mut n, &mut out);
        assert_eq!(lost(&out), 0);
        r.poll(210, &mut n, &mut out);
        assert_eq!(lost(&out), 1);
        assert_eq!(r.lost, 1);
        // the stream continues with the frame behind the hole
        assert_eq!(frames(&out).iter().map(|f| f.0).collect::<Vec<_>>(), vec![0, 2]);
    }

    #[test]
    fn without_nack_gives_up_quickly_and_never_asks() {
        let mut r = Reassembler::with_config(Config::without_nack());
        let mut out = Vec::new();
        let mut n = Vec::new();
        r.push(0, &frag(0, 0, 2, true), b"x", &mut out);
        r.poll(30, &mut n, &mut out);
        assert!(n.is_empty() && lost(&out) == 0);
        r.poll(40, &mut n, &mut out);
        assert!(n.is_empty());
        assert_eq!(lost(&out), 1);
    }

    #[test]
    fn nack_retries_are_bounded() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        let mut n = Vec::new();
        r.push(0, &frag(0, 0, 2, true), b"a", &mut out);
        for t in (0..190).step_by(1) {
            r.poll(t, &mut n, &mut out);
        }
        assert_eq!(n.len(), 5, "{n:?}"); // 8, 48, 88, 128, 168 ms
    }

    #[test]
    fn duplicate_and_old_fragments_ignored() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(0, &frag(5, 0, 1, true), b"K", &mut out);
        r.push(0, &frag(5, 0, 1, true), b"K", &mut out);
        r.push(0, &frag(3, 0, 1, false), b"old", &mut out);
        assert_eq!(frames(&out).len(), 1);
    }

    #[test]
    fn frame_id_wraparound() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(0, &frag(u32::MAX, 0, 1, true), b"a", &mut out);
        r.push(0, &frag(0, 0, 1, false), b"b", &mut out);
        assert_eq!(frames(&out).len(), 2);
    }

    #[test]
    fn key_pending_tracks_a_keyframe_in_progress() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        assert!(!r.key_pending());
        r.push(0, &frag(0, 0, 2, true), b"a", &mut out);
        assert!(r.key_pending());
        r.push(1, &frag(0, 1, 2, true), b"b", &mut out);
        assert!(!r.key_pending());
    }
}
