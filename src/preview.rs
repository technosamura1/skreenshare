// Self-preview ("ты показываешь это"): the host process downsizes the frame it just encoded to a micro picture
// and sends it to the GUI as one `@PREVIEW w h <base64 of zstd(RGB)>` line. This file is the pure part (no
// Windows, no zstd): sizes, pixel packing, base64, the line format, the rate gate and the "GUI is looking"
// heartbeat. Tested on Linux through tools/lossim.
use crate::source::letterbox_rect;

/// The micro preview box, in pixels.
pub const BOX_W: u32 = 320;
pub const BOX_H: u32 = 180;
/// The GUI refuses pictures bigger than this (a broken line must not allocate megabytes).
const MAX_SIDE: u32 = 1024;

/// How big the preview of a `cw`x`ch` stream canvas is: the canvas aspect ratio fitted into 320x180 (even sides).
/// The canvas already contains the letterbox bars of a window, so this is exactly what the viewer gets.
pub fn preview_size(cw: u32, ch: u32) -> (u32, u32) {
    let (_, _, w, h) = letterbox_rect(cw, ch, BOX_W, BOX_H);
    (w, h)
}

/// BGRA rows (`stride` bytes apart) -> tightly packed RGB.
pub fn pack_rgb(bgra: &[u8], w: u32, h: u32, stride: usize, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(w as usize * h as usize * 3);
    for y in 0..h as usize {
        for px in bgra[y * stride..][..w as usize * 4].chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(B64[(n >> 18) as usize & 63] as char);
        s.push(B64[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    s
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().as_bytes();
    if s.len() % 4 != 0 {
        return None;
    }
    let val = |c: u8| B64.iter().position(|&b| b == c).map(|p| p as u32);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (i, q) in s.chunks(4).enumerate() {
        let last = i == s.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (j, &c) in q.iter().enumerate() {
            n = n << 6 | if j >= 4 - pad { 0 } else { val(c)? };
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The IPC line for one picture; `packed` is the compressed RGB.
pub fn encode_line(w: u32, h: u32, packed: &[u8]) -> String {
    format!("@PREVIEW {w} {h} {}", b64_encode(packed))
}

/// Parses what follows `@PREVIEW `: `(w, h, compressed RGB)`. None for anything malformed or oversized.
pub fn parse_line(rest: &str) -> Option<(u32, u32, Vec<u8>)> {
    let mut it = rest.splitn(3, ' ');
    let w: u32 = it.next()?.parse().ok()?;
    let h: u32 = it.next()?.parse().ok()?;
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    Some((w, h, b64_decode(it.next()?)?))
}

/// Size of the uncompressed RGB picture (what the decompressor may produce at most).
pub fn rgb_len(w: u32, h: u32) -> usize {
    w as usize * h as usize * 3
}

/// Decides when the next preview picture is made: at most `base_ms` apart, and rarer when producing one is
/// slow (the work should stay a small share of the frame loop whatever the machine).
pub struct Gate {
    base_ms: u64,
    next_ms: u64,
}

impl Gate {
    /// 150 ms ≈ 6.7 pictures per second.
    pub fn new() -> Gate {
        Gate { base_ms: 150, next_ms: 0 }
    }

    pub fn due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_ms
    }

    /// A picture was made at `now_ms` and cost `cost_us` microseconds: keep the cost under ~4% of the time.
    pub fn done(&mut self, now_ms: u64, cost_us: u64) {
        self.next_ms = now_ms + self.base_ms.max(cost_us * 25 / 1000);
    }
}

/// The GUI says "I'm looking" with a heartbeat line every ~1.5 s while its window is visible. No heartbeat for
/// `TTL_MS` (window minimized or hidden, GUI frozen or gone) = no pictures are made at all.
pub const TTL_MS: u64 = 3500;

/// `last` is the time of the latest heartbeat as `ms + 1` (0 = none yet).
pub fn heartbeat_active(last: u64, now_ms: u64) -> bool {
    last != 0 && now_ms + 1 < last + TTL_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_keep_aspect_inside_the_box() {
        assert_eq!(preview_size(1920, 1080), (320, 180));
        assert_eq!(preview_size(3840, 2160), (320, 180));
        let (w, h) = preview_size(1920, 1200); // 16:10
        assert_eq!(h, 180);
        assert!(w < 320 && w % 2 == 0 && (w as f64 / h as f64 - 1.6).abs() < 0.03, "{w}x{h}");
        let (w, h) = preview_size(1080, 1920); // portrait monitor
        assert_eq!(h, 180);
        assert!(w <= 320 && w >= 2);
        for &(cw, ch) in &[(2u32, 2u32), (5000, 100), (100, 5000), (1366, 768)] {
            let (w, h) = preview_size(cw, ch);
            assert!(w >= 2 && h >= 2 && w <= BOX_W && h <= BOX_H && w % 2 == 0 && h % 2 == 0);
        }
    }

    #[test]
    fn packs_bgra_to_rgb_and_skips_stride_padding() {
        // 2x2, stride 12 (4 bytes of padding per row)
        let mut src = vec![0u8; 24];
        for (i, px) in [[1u8, 2, 3], [4, 5, 6], [7, 8, 9], [10, 11, 12]].iter().enumerate() {
            let o = (i / 2) * 12 + (i % 2) * 4;
            src[o..o + 4].copy_from_slice(&[px[0], px[1], px[2], 255]); // B G R A
        }
        src[8..12].fill(99);
        let mut out = vec![1, 2, 3];
        pack_rgb(&src, 2, 2, 12, &mut out);
        assert_eq!(out, [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    }

    #[test]
    fn base64_roundtrip_all_remainders_and_known_values() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        for n in 0..40usize {
            let d: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&d)).unwrap(), d, "len {n}");
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(b64_decode(&b64_encode(&all)).unwrap(), all);
    }

    #[test]
    fn base64_rejects_garbage() {
        assert!(b64_decode("abc").is_none()); // length
        assert!(b64_decode("ab!d").is_none());
        assert!(b64_decode("Zg==Zg==").is_none()); // padding in the middle
        assert!(b64_decode("Z===").is_none());
    }

    #[test]
    fn line_roundtrip() {
        let payload: Vec<u8> = (0..200).map(|i| (i * 7) as u8).collect();
        let l = encode_line(320, 180, &payload);
        assert!(l.starts_with("@PREVIEW 320 180 ") && !l.contains('\n'));
        let (w, h, p) = parse_line(l.strip_prefix("@PREVIEW ").unwrap()).unwrap();
        assert_eq!((w, h, p), (320, 180, payload));
    }

    #[test]
    fn malformed_lines_are_ignored() {
        assert!(parse_line("").is_none());
        assert!(parse_line("320 180").is_none());
        assert!(parse_line("0 180 AAAA").is_none());
        assert!(parse_line("320 x AAAA").is_none());
        assert!(parse_line("99999 180 AAAA").is_none());
        assert!(parse_line("320 180 AA!A").is_none());
        assert_eq!(rgb_len(320, 180), 172_800);
    }

    #[test]
    fn gate_limits_rate_and_backs_off_when_slow() {
        let mut g = Gate::new();
        assert!(g.due(0));
        g.done(1000, 2_000); // cheap: 2 ms
        assert!(!g.due(1100) && g.due(1150));
        g.done(2000, 20_000); // slow: 20 ms -> 500 ms apart
        assert!(!g.due(2400) && g.due(2500));
        // never faster than ~6.7/s however fast it is
        g.done(3000, 0);
        assert!(!g.due(3149));
    }

    #[test]
    fn heartbeat_expires() {
        assert!(!heartbeat_active(0, 0)); // no heartbeat ever
        let last = 5000 + 1;
        assert!(heartbeat_active(last, 5000));
        assert!(heartbeat_active(last, 5000 + TTL_MS - 1));
        assert!(!heartbeat_active(last, 5000 + TTL_MS));
        assert!(heartbeat_active(1, 0)); // a beat at t=0 counts
    }
}
