//! The v0.3.0 receiver logic, kept ONLY as a test oracle for the loss simulation (`sim.rs`): any frame that could
//! not be completed right away is "lost" and the decoder then needs a whole keyframe. This is the keyframe-request
//! loop that makes video crawl at 1 fps on a lossy Wi-Fi link. Not used in production code.
#![allow(dead_code)]

/// Receiver-side frame reassembly. Frames arrive as fragments, possibly reordered or missing; complete
/// frames are returned strictly in order, a frame that can never complete is reported as lost.
pub mod reassembly {
    use crate::VideoFrag;
    use std::collections::BTreeMap;

    pub struct Frame {
        pub frame_id: u32,
        pub ts_ms: u32,
        pub key: bool,
        pub data: Vec<u8>,
    }

    struct Partial {
        ts_ms: u32,
        key: bool,
        count: u16,
        got: u16,
        frags: Vec<Option<Vec<u8>>>,
    }

    #[derive(Default)]
    pub struct Reassembler {
        next_id: Option<u32>,
        pending: BTreeMap<u32, Partial>,
        pub lost: u32,
    }

    pub enum Out {
        Frame(Frame),
        /// One or more frames were skipped: the decoder needs a keyframe.
        Lost,
    }

    /// Max frames we keep waiting for before declaring the oldest one lost.
    const MAX_PENDING: usize = 4;

    impl Reassembler {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn reset(&mut self) {
            self.next_id = None;
            self.pending.clear();
        }

        pub fn push(&mut self, f: &VideoFrag, payload: &[u8], out: &mut Vec<Out>) {
            if f.count == 0 || f.idx >= f.count {
                return;
            }
            let next = *self.next_id.get_or_insert(f.frame_id);
            // Older than what we already delivered/skipped: ignore.
            if (f.frame_id.wrapping_sub(next) as i32) < 0 {
                return;
            }
            let p = self.pending.entry(f.frame_id).or_insert_with(|| Partial {
                ts_ms: f.ts_ms,
                key: f.key,
                count: f.count,
                got: 0,
                frags: vec![None; f.count as usize],
            });
            if p.count != f.count {
                return;
            }
            if p.frags[f.idx as usize].is_none() {
                p.frags[f.idx as usize] = Some(payload.to_vec());
                p.got += 1;
            }
            self.drain(out);
            // Too many half-finished frames waiting: give up on the oldest.
            while self.pending.len() > MAX_PENDING {
                let (&id, _) = self.pending.iter().next().unwrap();
                self.skip_to(id, out);
            }
        }

        /// Gives up on the oldest incomplete frame (called when it is clearly too late).
        pub fn give_up_oldest(&mut self, out: &mut Vec<Out>) {
            if let Some((&id, _)) = self.pending.iter().next() {
                self.skip_to(id, out);
            }
        }

        pub fn has_pending(&self) -> bool {
            !self.pending.is_empty()
        }

        fn skip_to(&mut self, id: u32, out: &mut Vec<Out>) {
            // Everything before `id` (and `id` itself) is abandoned.
            self.pending.remove(&id);
            self.next_id = Some(id.wrapping_add(1));
            self.lost += 1;
            out.push(Out::Lost);
            self.drain(out);
        }

        fn drain(&mut self, out: &mut Vec<Out>) {
            loop {
                let Some(next) = self.next_id else { return };
                // A complete frame ahead of a hole: the hole is lost, jump over it.
                let complete_id = self
                    .pending
                    .iter()
                    .find(|(_, p)| p.got == p.count)
                    .map(|(&id, _)| id);
                let Some(cid) = complete_id else { return };
                if cid != next {
                    // frames in [next, cid) never completed
                    let missing = cid.wrapping_sub(next);
                    self.pending.retain(|&id, _| id.wrapping_sub(next) >= missing);
                    self.lost += missing;
                    out.push(Out::Lost);
                    self.next_id = Some(cid);
                }
                let p = self.pending.remove(&cid).unwrap();
                let mut data = Vec::with_capacity(p.frags.iter().map(|f| f.as_ref().map_or(0, |v| v.len())).sum());
                for f in p.frags {
                    data.extend_from_slice(&f.unwrap());
                }
                out.push(Out::Frame(Frame { frame_id: cid, ts_ms: p.ts_ms, key: p.key, data }));
                self.next_id = Some(cid.wrapping_add(1));
            }
        }
    }
}

