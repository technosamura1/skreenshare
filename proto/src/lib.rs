//! Wire protocol between the Windows sender and the Android receiver.
//!
//! Everything is UDP, little-endian. The receiver listens on [`PORT`]; the sender sends START there and
//! then streams VIDEO/AUDIO from its own source port, the receiver answers (KEYREQ/PONG/REPORT/...) to
//! that source address. The Kotlin side (`android/.../Proto.kt`) mirrors this file byte for byte; the
//! golden-bytes tests in both places keep them in sync.

/// UDP port the receiver (projector) listens on: START, media, PROBE.
pub mod h264;

pub const PORT: u16 = 47800;
/// UDP port the sender listens on for receiver announcements (broadcast HELLO).
pub const DISCOVERY_PORT: u16 = 47801;
/// DNS-SD service type the receiver registers (NSD / mDNS).
pub const MDNS_TYPE: &str = "_skreenshare._udp.local.";
pub const VERSION: u8 = 1;
/// Max media bytes per datagram: stays under the Wi-Fi/Ethernet MTU with room for the headers.
pub const MAX_PAYLOAD: usize = 1200;

pub const MAGIC: u8 = 0xD7;
/// magic(1) type(1) session(4)
pub const HEADER_LEN: usize = 6;
/// header + ts_ms(4) frame_id(4) frag_idx(2) frag_cnt(2) flags(1)
pub const VIDEO_HEADER_LEN: usize = HEADER_LEN + 13;
/// header + seq(4) ts_ms(4)
pub const AUDIO_HEADER_LEN: usize = HEADER_LEN + 8;

pub const T_HELLO: u8 = 0x01;
pub const T_PROBE: u8 = 0x02;
pub const T_START: u8 = 0x03;
pub const T_START_ACK: u8 = 0x04;
pub const T_VIDEO: u8 = 0x10;
pub const T_AUDIO: u8 = 0x11;
pub const T_KEYREQ: u8 = 0x20;
pub const T_PING: u8 = 0x21;
pub const T_PONG: u8 = 0x22;
pub const T_BYE: u8 = 0x23;
pub const T_REPORT: u8 = 0x24;

pub const FLAG_KEY: u8 = 1;
pub const HELLO_BUSY: u8 = 1;

pub const VCODEC_H264: u8 = 1;

fn header(ty: u8, session: u32, out: &mut Vec<u8>) {
    out.clear();
    out.push(MAGIC);
    out.push(ty);
    out.extend_from_slice(&session.to_le_bytes());
}

/// (type, session, body) if this datagram is ours.
pub fn parse_header(pkt: &[u8]) -> Option<(u8, u32, &[u8])> {
    if pkt.len() < HEADER_LEN || pkt[0] != MAGIC {
        return None;
    }
    Some((pkt[1], u32::from_le_bytes(pkt[2..6].try_into().unwrap()), &pkt[HEADER_LEN..]))
}

/// What a receiver can decode; the sender never asks for more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub port: u16,
    pub max_w: u16,
    pub max_h: u16,
    pub max_fps: u8,
    pub busy: bool,
    pub name: String,
}

pub fn write_hello(h: &Hello, out: &mut Vec<u8>) {
    header(T_HELLO, 0, out);
    out.push(VERSION);
    out.extend_from_slice(&h.port.to_le_bytes());
    out.extend_from_slice(&h.max_w.to_le_bytes());
    out.extend_from_slice(&h.max_h.to_le_bytes());
    out.push(h.max_fps);
    out.push(if h.busy { HELLO_BUSY } else { 0 });
    let name = h.name.as_bytes();
    let n = name.len().min(60);
    out.push(n as u8);
    out.extend_from_slice(&name[..n]);
}

pub fn parse_hello(body: &[u8]) -> Option<Hello> {
    if body.len() < 10 || body[0] != VERSION {
        return None;
    }
    let n = body[9] as usize;
    let name = String::from_utf8_lossy(body.get(10..10 + n)?).into_owned();
    Some(Hello {
        port: u16::from_le_bytes([body[1], body[2]]),
        max_w: u16::from_le_bytes([body[3], body[4]]),
        max_h: u16::from_le_bytes([body[5], body[6]]),
        max_fps: body[7],
        busy: body[8] & HELLO_BUSY != 0,
        name,
    })
}

pub fn write_probe(reply_port: u16, out: &mut Vec<u8>) {
    header(T_PROBE, 0, out);
    out.extend_from_slice(&reply_port.to_le_bytes());
}

pub fn parse_probe(body: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(body.get(0..2)?.try_into().ok()?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub vcodec: u8,
    pub audio_rate: u32,
    pub audio_channels: u8,
}

pub fn write_start(session: u32, s: &Start, out: &mut Vec<u8>) {
    header(T_START, session, out);
    out.extend_from_slice(&s.width.to_le_bytes());
    out.extend_from_slice(&s.height.to_le_bytes());
    out.push(s.fps);
    out.push(s.vcodec);
    out.extend_from_slice(&s.audio_rate.to_le_bytes());
    out.push(s.audio_channels);
}

pub fn parse_start(body: &[u8]) -> Option<Start> {
    if body.len() < 11 {
        return None;
    }
    Some(Start {
        width: u16::from_le_bytes([body[0], body[1]]),
        height: u16::from_le_bytes([body[2], body[3]]),
        fps: body[4],
        vcodec: body[5],
        audio_rate: u32::from_le_bytes(body[6..10].try_into().unwrap()),
        audio_channels: body[10],
    })
}

pub fn write_simple(ty: u8, session: u32, out: &mut Vec<u8>) {
    header(ty, session, out);
}

pub fn write_ping(ty: u8, session: u32, t_ms: u32, out: &mut Vec<u8>) {
    header(ty, session, out);
    out.extend_from_slice(&t_ms.to_le_bytes());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrag {
    pub ts_ms: u32,
    pub frame_id: u32,
    pub idx: u16,
    pub count: u16,
    pub key: bool,
}

/// Writes the header only; the caller appends `chunk` (keeps one buffer per frame fragment loop).
pub fn write_video_header(session: u32, f: &VideoFrag, out: &mut Vec<u8>) {
    header(T_VIDEO, session, out);
    out.extend_from_slice(&f.ts_ms.to_le_bytes());
    out.extend_from_slice(&f.frame_id.to_le_bytes());
    out.extend_from_slice(&f.idx.to_le_bytes());
    out.extend_from_slice(&f.count.to_le_bytes());
    out.push(if f.key { FLAG_KEY } else { 0 });
}

pub fn parse_video(body: &[u8]) -> Option<(VideoFrag, &[u8])> {
    if body.len() < 13 {
        return None;
    }
    Some((
        VideoFrag {
            ts_ms: u32::from_le_bytes(body[0..4].try_into().unwrap()),
            frame_id: u32::from_le_bytes(body[4..8].try_into().unwrap()),
            idx: u16::from_le_bytes([body[8], body[9]]),
            count: u16::from_le_bytes([body[10], body[11]]),
            key: body[12] & FLAG_KEY != 0,
        },
        &body[13..],
    ))
}

pub fn write_audio_header(session: u32, seq: u32, ts_ms: u32, out: &mut Vec<u8>) {
    header(T_AUDIO, session, out);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&ts_ms.to_le_bytes());
}

pub fn parse_audio(body: &[u8]) -> Option<(u32, u32, &[u8])> {
    if body.len() < 8 {
        return None;
    }
    Some((
        u32::from_le_bytes(body[0..4].try_into().unwrap()),
        u32::from_le_bytes(body[4..8].try_into().unwrap()),
        &body[8..],
    ))
}

/// Receiver -> sender once a second: how it is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub decoded_fps: u16,
    pub lost_frames: u16,
    pub dropped_frames: u16,
}

pub fn write_report(session: u32, r: &Report, out: &mut Vec<u8>) {
    header(T_REPORT, session, out);
    out.extend_from_slice(&r.decoded_fps.to_le_bytes());
    out.extend_from_slice(&r.lost_frames.to_le_bytes());
    out.extend_from_slice(&r.dropped_frames.to_le_bytes());
}

pub fn parse_report(body: &[u8]) -> Option<Report> {
    if body.len() < 6 {
        return None;
    }
    Some(Report {
        decoded_fps: u16::from_le_bytes([body[0], body[1]]),
        lost_frames: u16::from_le_bytes([body[2], body[3]]),
        dropped_frames: u16::from_le_bytes([body[4], body[5]]),
    })
}

/// Splits one encoded access unit into datagram payload ranges (at least one, even for empty data).
pub fn fragment_count(len: usize) -> usize {
    len.div_ceil(MAX_PAYLOAD).max(1)
}

/// Receiver-side frame reassembly. Frames arrive as fragments, possibly reordered or missing; complete
/// frames are returned strictly in order, a frame that can never complete is reported as lost.
pub mod reassembly {
    use super::VideoFrag;
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

#[cfg(test)]
mod tests {
    use super::reassembly::*;
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // The same strings are asserted in android/app/src/test/.../ProtoTest.kt.
    #[test]
    fn golden_hello() {
        let mut o = Vec::new();
        write_hello(
            &Hello { port: PORT, max_w: 1920, max_h: 1080, max_fps: 60, busy: false, name: "Proj".into() },
            &mut o,
        );
        assert_eq!(hex(&o), "d70100000000" .to_string() + "01b8ba800738043c0004" + "50726f6a");
        let (ty, s, body) = parse_header(&o).unwrap();
        assert_eq!((ty, s), (T_HELLO, 0));
        assert_eq!(parse_hello(body).unwrap().name, "Proj");
    }

    #[test]
    fn golden_start() {
        let mut o = Vec::new();
        let s = Start { width: 1920, height: 1080, fps: 30, vcodec: 1, audio_rate: 48000, audio_channels: 2 };
        write_start(0x01020304, &s, &mut o);
        assert_eq!(hex(&o), "d703".to_string() + "04030201" + "8007" + "3804" + "1e" + "01" + "80bb0000" + "02");
        let (_, sess, body) = parse_header(&o).unwrap();
        assert_eq!(sess, 0x01020304);
        assert_eq!(parse_start(body).unwrap(), s);
    }

    #[test]
    fn golden_video_audio() {
        let mut o = Vec::new();
        write_video_header(7, &VideoFrag { ts_ms: 1000, frame_id: 5, idx: 1, count: 3, key: true }, &mut o);
        o.extend_from_slice(&[0xaa, 0xbb]);
        assert_eq!(hex(&o), "d710".to_string() + "07000000" + "e8030000" + "05000000" + "0100" + "0300" + "01" + "aabb");
        let (_, _, body) = parse_header(&o).unwrap();
        let (f, p) = parse_video(body).unwrap();
        assert_eq!((f.frame_id, f.idx, f.count, f.key, p), (5, 1, 3, true, &[0xaa, 0xbb][..]));

        write_audio_header(7, 9, 1234, &mut o);
        o.extend_from_slice(&[1, 2]);
        assert_eq!(hex(&o), "d711".to_string() + "07000000" + "09000000" + "d2040000" + "0102");
    }

    #[test]
    fn golden_report_ping() {
        let mut o = Vec::new();
        write_report(1, &Report { decoded_fps: 30, lost_frames: 2, dropped_frames: 1 }, &mut o);
        assert_eq!(hex(&o), "d724".to_string() + "01000000" + "1e00" + "0200" + "0100");
        write_ping(T_PING, 1, 0x0a0b0c0d, &mut o);
        assert_eq!(hex(&o), "d721".to_string() + "01000000" + "0d0c0b0a");
        write_simple(T_KEYREQ, 1, &mut o);
        assert_eq!(hex(&o), "d72001000000");
    }

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

    #[test]
    fn reassemble_in_order_and_reordered() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(&frag(0, 1, 2, true), b"BB", &mut out);
        assert!(out.is_empty());
        r.push(&frag(0, 0, 2, true), b"AA", &mut out);
        r.push(&frag(1, 0, 1, false), b"C", &mut out);
        assert_eq!(frames(&out), vec![(0, b"AABB".to_vec()), (1, b"C".to_vec())]);
        assert_eq!(r.lost, 0);
    }

    #[test]
    fn lost_fragment_is_skipped_when_next_frame_completes() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(&frag(0, 0, 1, true), b"K", &mut out);
        r.push(&frag(1, 0, 2, false), b"x", &mut out); // second fragment never arrives
        r.push(&frag(2, 0, 1, false), b"P", &mut out);
        assert!(out.iter().any(|o| matches!(o, Out::Lost)));
        assert_eq!(frames(&out), vec![(0, b"K".to_vec()), (2, b"P".to_vec())]);
        assert_eq!(r.lost, 1);
    }

    #[test]
    fn duplicate_and_old_fragments_ignored() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(&frag(5, 0, 1, true), b"K", &mut out);
        r.push(&frag(5, 0, 1, true), b"K", &mut out);
        r.push(&frag(3, 0, 1, false), b"old", &mut out);
        assert_eq!(frames(&out).len(), 1);
    }

    #[test]
    fn give_up_oldest_reports_loss() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(&frag(0, 0, 2, true), b"a", &mut out);
        assert!(r.has_pending());
        r.give_up_oldest(&mut out);
        assert!(matches!(out[0], Out::Lost));
        assert!(!r.has_pending());
        r.push(&frag(1, 0, 1, true), b"k", &mut out);
        assert_eq!(frames(&out), vec![(1, b"k".to_vec())]);
    }

    #[test]
    fn frame_id_wraparound() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        r.push(&frag(u32::MAX, 0, 1, true), b"a", &mut out);
        r.push(&frag(0, 0, 1, false), b"b", &mut out);
        assert_eq!(frames(&out).len(), 2);
    }

    #[test]
    fn fragments() {
        assert_eq!(fragment_count(0), 1);
        assert_eq!(fragment_count(1200), 1);
        assert_eq!(fragment_count(1201), 2);
    }
}
