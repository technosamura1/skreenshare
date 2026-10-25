//! Wire protocol between the Windows sender and the Android receiver.
//!
//! Everything is UDP, little-endian. The receiver listens on [`PORT`]; the sender sends START there and
//! then streams VIDEO/AUDIO from its own source port, the receiver answers (KEYREQ/PONG/REPORT/...) to
//! that source address. The Kotlin side (`android/.../Proto.kt`) mirrors this file byte for byte; the
//! golden-bytes tests in both places keep them in sync.

/// UDP port the receiver (projector) listens on: START, media, PROBE.
pub mod audiorx;
pub mod geom;
pub mod h264;
pub mod keyreq;
pub mod loss;
pub mod rate;
pub mod reassembly;
pub mod retx;

#[cfg(test)]
mod legacy;
#[cfg(test)]
mod sim;

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
/// v0.4: audio with a copy of the previous chunk (negotiated with [`FEAT_AUDIO_RED`]).
pub const T_AUDIO_RED: u8 = 0x12;
/// v0.5: Opus audio, 10 ms frames, each datagram carries the newest frame and a few before it
/// (negotiated with [`FEAT_OPUS`], only for 48 kHz stereo).
pub const T_AUDIO_OPUS: u8 = 0x13;
pub const T_KEYREQ: u8 = 0x20;
pub const T_PING: u8 = 0x21;
pub const T_PONG: u8 = 0x22;
pub const T_BYE: u8 = 0x23;
pub const T_REPORT: u8 = 0x24;
/// v0.4: receiver -> sender, list of missing video packets (negotiated with [`FEAT_NACK`]).
pub const T_NACK: u8 = 0x25;

pub const FLAG_KEY: u8 = 1;
/// Set on a re-sent video packet (old receivers only look at FLAG_KEY and ignore it).
pub const FLAG_RETX: u8 = 2;
/// `Start::features` (what the sender can do) and the START_ACK body (what the receiver agreed to).
pub const FEAT_NACK: u8 = 1;
pub const FEAT_AUDIO_RED: u8 = 2;
/// v0.5: the receiver can decode Opus (it ran a decode self-test). Without it audio stays raw PCM with redundancy.
pub const FEAT_OPUS: u8 = 4;
/// Length of one Opus frame, ms (and so the timestamp step between frames of one datagram).
pub const OPUS_FRAME_MS: u32 = 10;
/// Frames per Opus datagram (the newest + this many minus one older ones): covers a burst of 3 lost datagrams in a row.
pub const OPUS_DEPTH: usize = 4;
/// NACK item `idx` meaning "every fragment of this frame" (the receiver has seen none of it).
pub const NACK_ALL: u16 = 0xFFFF;
/// Items per NACK datagram (6 bytes each).
pub const MAX_NACK_ITEMS: usize = 150;
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
    /// 1 = audio timestamps are capture times on the same clock (same origin) as the video timestamps, so the
    /// receiver can lip-sync. 0 = (v0.1 sender) timestamps of audio are not comparable to video.
    pub clock_sync: u8,
    /// v0.4 capability bits ([`FEAT_NACK`], [`FEAT_AUDIO_RED`]). Absent (0) from older senders.
    pub features: u8,
}

pub fn write_start(session: u32, s: &Start, out: &mut Vec<u8>) {
    header(T_START, session, out);
    out.extend_from_slice(&s.width.to_le_bytes());
    out.extend_from_slice(&s.height.to_le_bytes());
    out.push(s.fps);
    out.push(s.vcodec);
    out.extend_from_slice(&s.audio_rate.to_le_bytes());
    out.push(s.audio_channels);
    out.push(s.clock_sync);
    out.push(s.features);
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
        clock_sync: body.get(11).copied().unwrap_or(0),
        features: body.get(12).copied().unwrap_or(0),
    })
}

/// START_ACK: header + (v0.4) the feature bits the receiver will use. Older receivers send no body = 0.
pub fn write_start_ack(session: u32, features: u8, out: &mut Vec<u8>) {
    header(T_START_ACK, session, out);
    out.push(features);
}

pub fn parse_start_ack(body: &[u8]) -> u8 {
    body.first().copied().unwrap_or(0)
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
    /// v0.4: video packets the receiver found missing in the last second (before any retransmission).
    pub nack_pkts: u16,
    /// v0.4: of those, how many arrived after all (retransmitted).
    pub recovered_pkts: u16,
    /// v0.5: video frames fully received (before decoding) in the last second; 0 from older receivers.
    pub received_fps: u16,
    /// v0.5: frames actually put on the screen in the last second; 0 from older receivers.
    pub shown_fps: u16,
}

pub fn write_report(session: u32, r: &Report, out: &mut Vec<u8>) {
    header(T_REPORT, session, out);
    out.extend_from_slice(&r.decoded_fps.to_le_bytes());
    out.extend_from_slice(&r.lost_frames.to_le_bytes());
    out.extend_from_slice(&r.dropped_frames.to_le_bytes());
    out.extend_from_slice(&r.nack_pkts.to_le_bytes());
    out.extend_from_slice(&r.recovered_pkts.to_le_bytes());
    out.extend_from_slice(&r.received_fps.to_le_bytes());
    out.extend_from_slice(&r.shown_fps.to_le_bytes());
}

pub fn parse_report(body: &[u8]) -> Option<Report> {
    if body.len() < 6 {
        return None;
    }
    Some(Report {
        decoded_fps: u16::from_le_bytes([body[0], body[1]]),
        lost_frames: u16::from_le_bytes([body[2], body[3]]),
        dropped_frames: u16::from_le_bytes([body[4], body[5]]),
        nack_pkts: if body.len() >= 8 { u16::from_le_bytes([body[6], body[7]]) } else { 0 },
        recovered_pkts: if body.len() >= 10 { u16::from_le_bytes([body[8], body[9]]) } else { 0 },
        received_fps: if body.len() >= 12 { u16::from_le_bytes([body[10], body[11]]) } else { 0 },
        shown_fps: if body.len() >= 14 { u16::from_le_bytes([body[12], body[13]]) } else { 0 },
    })
}

/// NACK: header + count(1) + count x (frame_id u32, idx u16). Extra items beyond [`MAX_NACK_ITEMS`] are not written.
pub fn write_nack(session: u32, items: &[(u32, u16)], out: &mut Vec<u8>) {
    header(T_NACK, session, out);
    let n = items.len().min(MAX_NACK_ITEMS);
    out.push(n as u8);
    for &(frame, idx) in &items[..n] {
        out.extend_from_slice(&frame.to_le_bytes());
        out.extend_from_slice(&idx.to_le_bytes());
    }
}

pub fn parse_nack(body: &[u8]) -> Vec<(u32, u16)> {
    let Some(&n) = body.first() else { return Vec::new() };
    body[1..]
        .chunks_exact(6)
        .take(n as usize)
        .map(|c| (u32::from_le_bytes(c[0..4].try_into().unwrap()), u16::from_le_bytes([c[4], c[5]])))
        .collect()
}

/// Audio with redundancy: header + seq(4) ts_ms(4) cur_len(2), then [previous chunk (0 or cur_len bytes)][current chunk].
/// `ts_ms` is the capture time of the CURRENT chunk; the previous one ends where it starts.
pub const AUDIO_RED_HEADER_LEN: usize = HEADER_LEN + 10;

pub fn write_audio_red(session: u32, seq: u32, ts_ms: u32, prev: &[u8], cur: &[u8], out: &mut Vec<u8>) {
    header(T_AUDIO_RED, session, out);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&ts_ms.to_le_bytes());
    out.extend_from_slice(&(cur.len() as u16).to_le_bytes());
    out.extend_from_slice(prev);
    out.extend_from_slice(cur);
}

/// (seq, ts_ms, previous chunk (maybe empty), current chunk)
pub fn parse_audio_red(body: &[u8]) -> Option<(u32, u32, &[u8], &[u8])> {
    if body.len() < 10 {
        return None;
    }
    let cur_len = u16::from_le_bytes([body[8], body[9]]) as usize;
    let data = &body[10..];
    if cur_len == 0 || data.len() < cur_len || (data.len() != cur_len && data.len() != 2 * cur_len) {
        return None;
    }
    let (prev, cur) = data.split_at(data.len() - cur_len);
    Some((
        u32::from_le_bytes(body[0..4].try_into().unwrap()),
        u32::from_le_bytes(body[4..8].try_into().unwrap()),
        prev,
        cur,
    ))
}

/// Opus audio: header + seq(4) ts_ms(4) n(1) + n x (len u16, Opus packet). Frame 0 is the newest (sequence `seq`,
/// starts at `ts_ms`); frame k is `seq - k` and starts `k * OPUS_FRAME_MS` earlier.
pub fn write_audio_opus(session: u32, seq: u32, ts_ms: u32, frames: &[&[u8]], out: &mut Vec<u8>) {
    header(T_AUDIO_OPUS, session, out);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&ts_ms.to_le_bytes());
    out.push(frames.len() as u8);
    for f in frames {
        out.extend_from_slice(&(f.len() as u16).to_le_bytes());
        out.extend_from_slice(f);
    }
}

/// (seq, ts_ms, frames newest first)
pub fn parse_audio_opus(body: &[u8]) -> Option<(u32, u32, Vec<&[u8]>)> {
    if body.len() < 9 {
        return None;
    }
    let n = body[8] as usize;
    if n == 0 || n > 8 {
        return None;
    }
    let mut at = 9;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let len = u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?) as usize;
        at += 2;
        v.push(body.get(at..at + len)?);
        at += len;
    }
    Some((u32::from_le_bytes(body[0..4].try_into().unwrap()), u32::from_le_bytes(body[4..8].try_into().unwrap()), v))
}

/// Splits one encoded access unit into datagram payload ranges (at least one, even for empty data).
pub fn fragment_count(len: usize) -> usize {
    len.div_ceil(MAX_PAYLOAD).max(1)
}

#[cfg(test)]
mod tests {
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
        let s = Start { width: 1920, height: 1080, fps: 30, vcodec: 1, audio_rate: 48000, audio_channels: 2, clock_sync: 1, features: FEAT_NACK | FEAT_AUDIO_RED };
        write_start(0x01020304, &s, &mut o);
        assert_eq!(hex(&o), "d703".to_string() + "04030201" + "8007" + "3804" + "1e" + "01" + "80bb0000" + "02" + "01" + "03");
        let (_, sess, body) = parse_header(&o).unwrap();
        assert_eq!(sess, 0x01020304);
        assert_eq!(parse_start(body).unwrap(), s);
    }

    #[test]
    fn start_from_v01_sender_has_no_clock_sync() {
        // 11-byte START as sent by v0.1.0: still understood, but flagged as not lip-sync capable.
        let body = [0x80, 0x07, 0x38, 0x04, 0x1e, 0x01, 0x80, 0xbb, 0x00, 0x00, 0x02];
        let s = parse_start(&body).unwrap();
        assert_eq!((s.width, s.audio_rate, s.clock_sync, s.features), (1920, 48000, 0, 0));
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
        write_report(1, &Report { decoded_fps: 30, lost_frames: 2, dropped_frames: 1, nack_pkts: 5, recovered_pkts: 4, received_fps: 31, shown_fps: 29 }, &mut o);
        assert_eq!(hex(&o), "d724".to_string() + "01000000" + "1e00" + "0200" + "0100" + "0500" + "0400" + "1f00" + "1d00");
        let r = parse_report(&o[HEADER_LEN..]).unwrap();
        assert_eq!((r.received_fps, r.shown_fps), (31, 29));
        // a v0.4 receiver's 10-byte report has no fps counters
        let v04 = parse_report(&o[HEADER_LEN..HEADER_LEN + 10]).unwrap();
        assert_eq!((v04.recovered_pkts, v04.received_fps, v04.shown_fps), (4, 0, 0));
        assert_eq!(parse_report(&o[HEADER_LEN..]).unwrap().recovered_pkts, 4);
        write_ping(T_PING, 1, 0x0a0b0c0d, &mut o);
        assert_eq!(hex(&o), "d721".to_string() + "01000000" + "0d0c0b0a");
        // a v0.3 receiver's 6-byte report is still understood
        let old = parse_report(&[30, 0, 2, 0, 1, 0]).unwrap();
        assert_eq!((old.decoded_fps, old.lost_frames, old.nack_pkts, old.recovered_pkts), (30, 2, 0, 0));
        write_simple(T_KEYREQ, 1, &mut o);
        assert_eq!(hex(&o), "d72001000000");
    }

    #[test]
    fn golden_audio_opus() {
        let mut o = Vec::new();
        write_audio_opus(7, 9, 1234, &[&[1, 2, 3], &[4]], &mut o);
        assert_eq!(hex(&o), "d713".to_string() + "07000000" + "09000000" + "d2040000" + "02" + "0300" + "010203" + "0100" + "04");
        let (_, _, body) = parse_header(&o).unwrap();
        let (seq, ts, f) = parse_audio_opus(body).unwrap();
        assert_eq!((seq, ts, f), (9, 1234, vec![&[1u8, 2, 3][..], &[4u8][..]]));
        // truncated / empty list / absurd count are rejected, never panic
        assert!(parse_audio_opus(&body[..body.len() - 1]).is_none());
        assert!(parse_audio_opus(&body[..8]).is_none());
        let mut zero = body.to_vec();
        zero[8] = 0;
        assert!(parse_audio_opus(&zero).is_none());
        assert_eq!(FEAT_NACK | FEAT_AUDIO_RED | FEAT_OPUS, 7);
    }

    #[test]
    fn fragments() {
        assert_eq!(fragment_count(0), 1);
        assert_eq!(fragment_count(1200), 1);
        assert_eq!(fragment_count(1201), 2);
    }

    #[test]
    fn golden_start_ack_nack_audio_red() {
        let mut o = Vec::new();
        write_start_ack(1, FEAT_NACK | FEAT_AUDIO_RED, &mut o);
        assert_eq!(hex(&o), "d704".to_string() + "01000000" + "03");
        assert_eq!(parse_start_ack(&o[HEADER_LEN..]), 3);
        assert_eq!(parse_start_ack(&[]), 0); // v0.3 receiver: empty ACK = no new features

        write_nack(1, &[(5, 1), (0x01020304, NACK_ALL)], &mut o);
        assert_eq!(hex(&o), "d725".to_string() + "01000000" + "02" + "050000000100" + "04030201ffff");
        assert_eq!(parse_nack(&o[HEADER_LEN..]), vec![(5, 1), (0x01020304, NACK_ALL)]);
        // a truncated list never panics and yields only complete items
        assert_eq!(parse_nack(&o[HEADER_LEN..o.len() - 2]), vec![(5, 1)]);
        let many: Vec<(u32, u16)> = (0..400).map(|i| (i, 0)).collect();
        write_nack(1, &many, &mut o);
        assert_eq!(parse_nack(&o[HEADER_LEN..]).len(), MAX_NACK_ITEMS);
        assert!(o.len() <= MAX_PAYLOAD);

        write_audio_red(7, 9, 1234, &[1, 2], &[3, 4], &mut o);
        assert_eq!(hex(&o), "d712".to_string() + "07000000" + "09000000" + "d2040000" + "0200" + "0102" + "0304");
        let (_, _, body) = parse_header(&o).unwrap();
        assert_eq!(parse_audio_red(body).unwrap(), (9, 1234, &[1u8, 2][..], &[3u8, 4][..]));
        write_audio_red(7, 0, 0, &[], &[3, 4], &mut o);
        let (_, _, body) = parse_header(&o).unwrap();
        assert_eq!(parse_audio_red(body).unwrap().2.len(), 0);
        assert!(parse_audio_red(&body[..body.len() - 1]).is_none());
        // inconsistent length is rejected
        let mut bad = body.to_vec();
        bad.push(0);
        assert!(parse_audio_red(&bad).is_none());
    }
}
