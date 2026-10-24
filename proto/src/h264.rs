//! H.264 helpers for the low-latency path.
//!
//! Weak hardware decoders (cheap projectors, TV boxes) hold back several frames before showing them unless
//! the stream's SPS says, in its VUI `bitstream_restriction`, that no frame is ever reordered
//! (`max_num_reorder_frames = 0`). NVENC does not always write that, so the sender rewrites the SPS of
//! every keyframe. Nothing else in the stream is touched.

struct BitReader<'a> {
    d: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    fn new(d: &'a [u8]) -> Self {
        BitReader { d, pos: 0 }
    }
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        Some(((1u64 << zeros) - 1) as u32 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        if self.pos > self.d.len() * 8 {
            None
        } else {
            Some(())
        }
    }
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        if self.nbits % 8 == 0 {
            self.out.push(0);
        }
        if b != 0 {
            *self.out.last_mut().unwrap() |= 0x80 >> (self.nbits % 8);
        }
        self.nbits += 1;
    }
    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros(); // number of bits in x
        for _ in 0..len - 1 {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit(((x >> i) & 1) as u32);
        }
    }
    /// Copies `n` bits starting at bit `from` of `src`.
    fn copy(&mut self, src: &[u8], from: usize, n: usize) {
        for i in from..from + n {
            self.bit(((src[i / 8] >> (7 - i % 8)) & 1) as u32);
        }
    }
    fn finish_rbsp(mut self) -> Vec<u8> {
        self.bit(1);
        while self.nbits % 8 != 0 {
            self.bit(0);
        }
        self.out
    }
}

fn unescape(nal_body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_body.len());
    let mut zeros = 0;
    for &b in nal_body {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let cnt = r.ue()? + 1;
    r.skip(8)?;
    for _ in 0..cnt {
        r.ue()?;
        r.ue()?;
        r.bit()?;
    }
    r.skip(20)
}

/// What we need to know about an SPS: where the VUI starts and which reorder settings it has.
struct SpsInfo {
    /// Bit position of `vui_parameters_present_flag`.
    vui_flag_pos: usize,
    vui_present: bool,
    /// Bit position of `bitstream_restriction_flag` (only if the VUI is present).
    restriction_flag_pos: usize,
    restriction_present: bool,
    max_num_ref_frames: u32,
    /// Existing (num_reorder_frames, max_dec_frame_buffering).
    reorder: Option<(u32, u32)>,
}

fn parse_sps(rbsp: &[u8]) -> Option<SpsInfo> {
    // rbsp[0] is the NAL header byte.
    let mut r = BitReader::new(&rbsp[1..]);
    let profile = r.bits(8)?;
    r.skip(16)?; // constraint flags + level
    r.ue()?; // sps id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            for i in 0..(if chroma != 3 { 8 } else { 12 }) {
                if r.bit()? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.bit()?; // gaps_in_frame_num_allowed
    r.ue()?;
    r.ue()?;
    if r.bit()? == 0 {
        r.bit()?; // mb_adaptive_frame_field
    }
    r.bit()?; // direct_8x8_inference
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let vui_flag_pos = r.pos;
    let vui_present = r.bit()? == 1;
    let mut info = SpsInfo {
        vui_flag_pos,
        vui_present,
        restriction_flag_pos: 0,
        restriction_present: false,
        max_num_ref_frames,
        reorder: None,
    };
    if !vui_present {
        return Some(info);
    }
    if r.bit()? == 1 {
        if r.bits(8)? == 255 {
            r.skip(32)?;
        }
    }
    if r.bit()? == 1 {
        r.bit()?;
    }
    if r.bit()? == 1 {
        r.skip(4)?;
        if r.bit()? == 1 {
            r.skip(24)?;
        }
    }
    if r.bit()? == 1 {
        r.ue()?;
        r.ue()?;
    }
    if r.bit()? == 1 {
        r.skip(65)?;
    }
    let nal_hrd = r.bit()? == 1;
    if nal_hrd {
        skip_hrd(&mut r)?;
    }
    let vcl_hrd = r.bit()? == 1;
    if vcl_hrd {
        skip_hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.bit()?;
    }
    r.bit()?; // pic_struct_present
    info.restriction_flag_pos = r.pos;
    if r.bit()? == 1 {
        info.restriction_present = true;
        r.bit()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        let reorder = r.ue()?;
        let dec = r.ue()?;
        info.reorder = Some((reorder, dec));
    }
    Some(info)
}

/// Returns the NAL (header byte included, no start code, escaped) with a zero-reorder bitstream restriction,
/// or `None` if the SPS can't be parsed (the caller then sends the original).
fn patch_sps_nal(nal: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(nal);
    let info = parse_sps(&rbsp)?;
    let want_dec = info.max_num_ref_frames.max(1);
    if info.reorder == Some((0, want_dec)) {
        return Some(nal.to_vec());
    }
    let body = &rbsp[1..];
    let mut w = BitWriter::default();
    w.copy(body, 0, info.vui_flag_pos);
    w.bit(1); // vui_parameters_present_flag
    if info.vui_present {
        let from = info.vui_flag_pos + 1;
        w.copy(body, from, info.restriction_flag_pos - from);
    } else {
        for _ in 0..8 {
            w.bit(0); // aspect, overscan, video signal, chroma loc, timing, nal hrd, vcl hrd, pic_struct
        }
    }
    w.bit(1); // bitstream_restriction_flag
    w.bit(1); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom
    w.ue(0); // max_bits_per_mb_denom
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(want_dec); // max_dec_frame_buffering
    let mut out = vec![rbsp[0]];
    escape(&w.finish_rbsp(), &mut out);
    Some(out)
}

/// Splits an Annex-B buffer into (start_code_len, nal_range) pairs.
fn split_annexb(d: &[u8]) -> Vec<(usize, std::ops::Range<usize>)> {
    let mut starts = Vec::new(); // (position of start code, its length)
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            let (p, l) = if i > 0 && d[i - 1] == 0 { (i - 1, 4) } else { (i, 3) };
            starts.push((p, l));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &(p, l)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map(|s| s.0).unwrap_or(d.len());
        out.push((l, p + l..end));
    }
    out
}

/// Rewrites the SPS NAL units in one Annex-B access unit; returns it unchanged if there is none.
pub fn patch_access_unit(au: &[u8]) -> Vec<u8> {
    let nals = split_annexb(au);
    if !nals.iter().any(|(_, r)| au.get(r.start).is_some_and(|b| b & 0x1f == 7)) {
        return au.to_vec();
    }
    let mut out = Vec::with_capacity(au.len() + 16);
    for (sc, r) in nals {
        out.extend_from_slice(&[0, 0, 0, 1][4 - sc.max(3)..]);
        let nal = &au[r];
        if !nal.is_empty() && nal[0] & 0x1f == 7 {
            match patch_sps_nal(nal) {
                Some(p) => out.extend_from_slice(&p),
                None => out.extend_from_slice(nal),
            }
        } else {
            out.extend_from_slice(nal);
        }
    }
    out
}

/// (max_num_reorder_frames, max_dec_frame_buffering) of the first SPS in the access unit, if it has them.
pub fn sps_reorder(au: &[u8]) -> Option<(u32, u32)> {
    for (_, r) in split_annexb(au) {
        let nal = &au[r];
        if !nal.is_empty() && nal[0] & 0x1f == 7 {
            return parse_sps(&unescape(nal))?.reorder;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline-ish SPS without VUI, built by hand: profile 77 (main), level 4.1, 1920x1088.
    fn make_sps(vui: bool) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(77, 8);
        w.bits(0, 8);
        w.bits(41, 8);
        w.ue(0); // sps id
        w.ue(0); // log2_max_frame_num-4
        w.ue(2); // poc type 2
        w.ue(1); // refs
        w.bit(0);
        w.ue(119); // width mbs - 1
        w.ue(67); // height - 1
        w.bit(1); // frame_mbs_only
        w.bit(1); // direct 8x8
        w.bit(0); // cropping
        if vui {
            w.bit(1);
            w.bit(0); // aspect
            w.bit(0); // overscan
            w.bit(1); // video signal
            w.bits(5, 3);
            w.bit(0);
            w.bit(1);
            w.bits(1, 8);
            w.bits(1, 8);
            w.bits(1, 8);
            w.bit(0); // chroma loc
            w.bit(0); // timing
            w.bit(0);
            w.bit(0);
            w.bit(0); // pic struct
            w.bit(1); // restriction present, with reorder = 2
            w.bit(1);
            w.ue(0);
            w.ue(0);
            w.ue(16);
            w.ue(16);
            w.ue(2);
            w.ue(4);
        } else {
            w.bit(0);
        }
        let mut nal = vec![0x67];
        escape(&w.finish_rbsp(), &mut nal);
        nal
    }

    fn au(sps: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(sps);
        v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x38, 0x80]);
        v.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10]);
        v
    }

    #[test]
    fn adds_restriction_without_vui() {
        let a = au(&make_sps(false));
        assert_eq!(sps_reorder(&a), None);
        let p = patch_access_unit(&a);
        assert_eq!(sps_reorder(&p), Some((0, 1)));
        // PPS and IDR slice are byte-identical, start codes preserved.
        assert!(p.ends_with(&[0, 0, 0, 1, 0x68, 0xce, 0x38, 0x80, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10]));
    }

    #[test]
    fn replaces_existing_restriction() {
        let a = au(&make_sps(true));
        assert_eq!(sps_reorder(&a), Some((2, 4)));
        let p = patch_access_unit(&a);
        assert_eq!(sps_reorder(&p), Some((0, 1)));
        // Idempotent.
        assert_eq!(patch_access_unit(&p), p);
    }

    #[test]
    fn keeps_non_sps_data() {
        let d = [0, 0, 0, 1, 0x41, 1, 2, 3, 4];
        assert_eq!(patch_access_unit(&d), d.to_vec());
    }

    #[test]
    fn exp_golomb_roundtrip() {
        let mut w = BitWriter::default();
        for v in [0u32, 1, 2, 3, 7, 8, 255, 65535, 1_000_000] {
            w.ue(v);
        }
        let b = w.finish_rbsp();
        let mut r = BitReader::new(&b);
        for v in [0u32, 1, 2, 3, 7, 8, 255, 65535, 1_000_000] {
            assert_eq!(r.ue(), Some(v));
        }
    }

    /// Real encoder output (x264 via the ffmpeg CLI) still decodes identically after patching.
    #[test]
    fn real_stream_decodes_the_same() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("skp-h264-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.h264");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=2", "-c:v", "libx264"])
            .args(["-profile:v", "high", "-bf", "0", "-g", "30", "-pix_fmt", "yuv420p", "-f", "h264"])
            .arg(&src)
            .status();
        let Ok(ok) = ok else { return }; // no ffmpeg on this machine: skip
        if !ok.success() {
            return;
        }
        let data = std::fs::read(&src).unwrap();
        let patched = patch_access_unit(&data); // the whole stream: only the SPS differs
        assert_eq!(sps_reorder(&patched), Some((0, (sps_refs(&data)).max(1))));
        let dst = dir.join("b.h264");
        std::fs::write(&dst, &patched).unwrap();
        let md5 = |f: &std::path::Path| {
            let o = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(f)
                .args(["-f", "md5", "-"])
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8(o.stdout).unwrap()
        };
        assert_eq!(md5(&src), md5(&dst));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sps_refs(au: &[u8]) -> u32 {
        for (_, r) in split_annexb(au) {
            let nal = &au[r];
            if !nal.is_empty() && nal[0] & 0x1f == 7 {
                return parse_sps(&unescape(nal)).unwrap().max_num_ref_frames;
            }
        }
        0
    }
}
