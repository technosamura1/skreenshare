// HDR -> SDR picture math (pure, no Windows APIs: also built and tested on Linux through tools/lossim).
//
// Why this exists. With "Use HDR" on, Windows composes the desktop in scRGB: linear light, BT.709 primaries,
// 1.0 = 80 nits, and "SDR white" (white of ordinary windows) sits at `SDR white level / 80`, e.g. 3.0 for the
// common 240-nit setting. Capturing that as 8-bit BGRA clips everything above 1.0, so a normal desktop comes
// out far too bright / washed out on the viewer's normal (SDR) screen. We capture the float picture instead and
// bring it back to SDR here: divide by the SDR white level (so ordinary white is 1.0 again), roll the HDR
// highlights above it off smoothly, and encode with the sRGB curve. Everything else in the program (encoder,
// network, viewer) stays 8-bit SDR BT.709 as before.
//
// The same curve is written in HLSL in hdr.rs (GPU path); `HLSL_CURVE_CONSTANTS` keeps the numbers in one place.

/// Windows' default SDR white level (80 nits = scRGB 1.0). Used when the real level can't be read.
pub const DEFAULT_SDR_WHITE_NITS: f32 = 80.0;
/// Lowest / highest SDR white level the Windows slider can produce, with a margin.
pub const MIN_SDR_WHITE_NITS: f32 = 40.0;
pub const MAX_SDR_WHITE_NITS: f32 = 1000.0;
/// Up to this value (SDR-relative linear light) the picture is passed through unchanged; above it highlights roll
/// off towards 1.0. 0.98 keeps ordinary white at 254/255 (measured with `hdrcheck` on a real HDR monitor: 0.92 gave
/// 250, a visibly grey "white" on pages and documents, which are most of what is streamed); HDR highlights still
/// fade into white smoothly instead of clipping with a hard edge.
pub const KNEE: f32 = 0.98;
/// Larger values are clamped first (HDR peaks are ~12.5 in these units; this only guards against garbage / inf).
pub const MAX_INPUT: f32 = 10_000.0;

/// What the host does with HDR. `Auto` is the default; the others exist only as a hidden switch (env var
/// SCREENSHARE_HDR = off | <nits>) for the rare PC where auto-detection is wrong.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HdrMode {
    Auto,
    Off,
    /// Force this SDR white level (nits) instead of asking Windows.
    White(f32),
}

pub fn parse_mode(v: Option<&str>) -> HdrMode {
    let Some(v) = v else { return HdrMode::Auto };
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "auto" | "on" | "1" => HdrMode::Auto,
        "off" | "0" | "no" | "false" => HdrMode::Off,
        n => match n.parse::<f32>() {
            Ok(x) if x.is_finite() && x > 0.0 => HdrMode::White(x.clamp(MIN_SDR_WHITE_NITS, MAX_SDR_WHITE_NITS)),
            _ => HdrMode::Auto,
        },
    }
}

/// The factor that turns scRGB (1.0 = 80 nits) into "SDR-relative" light (1.0 = the SDR white of the screen).
pub fn gain_for_white(nits: f32) -> f32 {
    let n = if nits.is_finite() { nits.clamp(MIN_SDR_WHITE_NITS, MAX_SDR_WHITE_NITS) } else { DEFAULT_SDR_WHITE_NITS };
    80.0 / n
}

/// Highlight roll-off of the biggest colour component `m` (SDR-relative linear light, >= 0): identity up to
/// `KNEE`, then a smooth, strictly rising curve (Reinhard tail) that approaches 1.0.
pub fn rolloff(m: f32) -> f32 {
    if !(m > KNEE) {
        return if m > 0.0 { m } else { 0.0 }; // also NaN -> 0
    }
    let m = m.min(MAX_INPUT);
    let t = (m - KNEE) / (1.0 - KNEE);
    1.0 - (1.0 - KNEE) / (1.0 + t) // = KNEE + (1-KNEE)*t/(1+t); Reinhard-style tail: slope 1 at the knee, never saturates abruptly
}

/// sRGB encoding of linear light 0..1.
pub fn srgb_oetf(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Inverse of `srgb_oetf` (used by tests and the CPU LUT checks).
#[allow(dead_code)]
pub fn srgb_eotf(e: f32) -> f32 {
    let e = e.clamp(0.0, 1.0);
    if e <= 0.040_45 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

/// SMPTE ST 2084 (PQ) -> linear light in scRGB units (1.0 = 80 nits; PQ 1.0 = 10000 nits = 125.0).
pub fn pq_to_scrgb(e: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let e = if e.is_finite() { e.clamp(0.0, 1.0) } else { 0.0 };
    let p = e.powf(1.0 / M2);
    let n = (p - C1).max(0.0);
    let d = C2 - C3 * p;
    let y = (n / d).powf(1.0 / M1); // 0..1 of 10000 nits
    y * 125.0
}

/// BT.2020 -> BT.709 primaries (linear light), for PQ pictures.
pub fn bt2020_to_bt709(c: [f32; 3]) -> [f32; 3] {
    [
        1.660_491 * c[0] - 0.587_641 * c[1] - 0.072_850 * c[2],
        -0.124_550 * c[0] + 1.132_900 * c[1] - 0.008_349 * c[2],
        -0.018_151 * c[0] - 0.100_579 * c[1] + 1.118_730 * c[2],
    ]
}

/// One pixel: linear scRGB (r, g, b) -> sRGB-encoded 8-bit (r, g, b). `gain` is `gain_for_white(...)`.
/// Hue-preserving: the roll-off scales all three components by the same factor (computed from the biggest one),
/// so bright colours don't turn grey. Negative components (wide-gamut colours outside sRGB) are clipped to 0.
/// NaN/inf never reach the output.
pub fn tonemap_pixel(r: f32, g: f32, b: f32, gain: f32) -> [u8; 3] {
    let f = |v: f32| if v.is_finite() { (v * gain).clamp(0.0, MAX_INPUT) } else if v > 0.0 { MAX_INPUT } else { 0.0 };
    let (r, g, b) = (f(r), f(g), f(b));
    let m = r.max(g).max(b);
    let s = if m > KNEE { rolloff(m) / m } else { 1.0 };
    let lut = srgb_lut();
    let q = |v: f32| lut[((v * s).clamp(0.0, 1.0) * (LUT_N - 1) as f32 + 0.5) as usize];
    [q(r), q(g), q(b)]
}

const LUT_N: usize = 16384;

/// Linear 0..1 -> sRGB code 0..255, so the CPU fallback does no `powf` per pixel.
fn srgb_lut() -> &'static [u8] {
    static LUT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    LUT.get_or_init(|| (0..LUT_N).map(|i| (srgb_oetf(i as f32 / (LUT_N - 1) as f32) * 255.0 + 0.5) as u8).collect())
}

/// IEEE half -> f32.
pub fn f16_to_f32(h: u16) -> f32 {
    let s = ((h >> 15) & 1) as u32;
    let e = ((h >> 10) & 0x1F) as u32;
    let m = (h & 0x3FF) as u32;
    let bits = if e == 0 {
        if m == 0 {
            s << 31
        } else {
            // subnormal half: normalise
            let mut e2 = 127 - 15 + 1;
            let mut m2 = m;
            while m2 & 0x400 == 0 {
                m2 <<= 1;
                e2 -= 1;
            }
            (s << 31) | ((e2 as u32) << 23) | ((m2 & 0x3FF) << 13)
        }
    } else if e == 31 {
        (s << 31) | (0xFF << 23) | (m << 13) // inf / NaN
    } else {
        (s << 31) | ((e + 127 - 15) << 23) | (m << 13)
    };
    f32::from_bits(bits)
}

/// CPU fallback, float16 RGBA rows (scRGB) -> BGRA8 rows. `src` has `pitch` bytes per row (8 bytes per pixel),
/// `out` is tightly packed `w*4` per row.
pub fn convert_f16_to_bgra(src: &[u8], pitch: usize, w: usize, h: usize, gain: f32, out: &mut [u8]) {
    for y in 0..h {
        let srow = &src[y * pitch..y * pitch + w * 8];
        let drow = &mut out[y * w * 4..(y + 1) * w * 4];
        for (s, d) in srow.chunks_exact(8).zip(drow.chunks_exact_mut(4)) {
            let c = |i: usize| f16_to_f32(u16::from_le_bytes([s[i * 2], s[i * 2 + 1]]));
            let [r, g, b] = tonemap_pixel(c(0), c(1), c(2), gain);
            d[0] = b;
            d[1] = g;
            d[2] = r;
            d[3] = 255;
        }
    }
}

/// CPU fallback for a PQ picture (R10G10B10A2, BT.2020): rows of u32 `r | g<<10 | b<<20 | a<<30`.
pub fn convert_pq10_to_bgra(src: &[u8], pitch: usize, w: usize, h: usize, gain: f32, out: &mut [u8]) {
    for y in 0..h {
        let srow = &src[y * pitch..y * pitch + w * 4];
        let drow = &mut out[y * w * 4..(y + 1) * w * 4];
        for (s, d) in srow.chunks_exact(4).zip(drow.chunks_exact_mut(4)) {
            let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            let c = |sh: u32| pq_to_scrgb(((v >> sh) & 0x3FF) as f32 / 1023.0);
            let [r, g, b] = bt2020_to_bt709([c(0), c(10), c(20)]);
            let [r, g, b] = tonemap_pixel(r, g, b, gain);
            d[0] = b;
            d[1] = g;
            d[2] = r;
            d[3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grey(v: f32, gain: f32) -> u8 {
        tonemap_pixel(v, v, v, gain)[0]
    }

    #[test]
    fn rolloff_is_monotonic_bounded_and_continuous() {
        let mut prev = -1.0f32;
        let mut x = 0.0f32;
        while x < 200.0 {
            let y = rolloff(x);
            assert!(y >= prev, "not monotonic at {x}: {y} < {prev}");
            assert!(y <= 1.0 && y.is_finite());
            prev = y;
            x += 0.003;
        }
        // identity below the knee, continuous at the knee, strictly rising above it
        assert_eq!(rolloff(0.5), 0.5);
        assert!((rolloff(KNEE) - KNEE).abs() < 1e-6);
        assert!(rolloff(KNEE + 1e-3) > KNEE);
        assert!(rolloff(2.0) < rolloff(3.0));
        // slope at the knee is 1 (no visible kink)
        let d = (rolloff(KNEE + 1e-4) - rolloff(KNEE)) / 1e-4;
        assert!((d - 1.0).abs() < 0.01, "slope {d}");
    }

    #[test]
    fn sdr_white_maps_to_white() {
        for nits in [80.0f32, 100.0, 203.0, 240.0, 480.0, 1000.0] {
            let sdr_white_scrgb = nits / 80.0;
            let v = grey(sdr_white_scrgb, gain_for_white(nits));
            assert!(v >= 250, "SDR white at {nits} nits became {v}");
        }
    }

    #[test]
    fn plain_sdr_content_is_unchanged_on_an_80_nit_desktop() {
        // a mid-grey and a colour below the knee: sRGB decode -> (scRGB) -> our encode must give the same codes
        for code in [0u8, 1, 10, 64, 128, 180, 200] {
            let l = srgb_eotf(code as f32 / 255.0);
            let o = tonemap_pixel(l, l, l, 1.0);
            assert!((o[0] as i32 - code as i32).abs() <= 1, "{code} -> {}", o[0]);
        }
    }

    #[test]
    fn bright_desktop_is_darkened_like_the_user_sees_it() {
        // The bug: SDR content of a 240-nit desktop is 3.0 in scRGB; clipped to 8 bit it is pure white.
        // Mid-grey sRGB 128 (linear 0.2158) is 0.647 in scRGB at 240 nits, and must come back as ~128.
        let l = srgb_eotf(128.0 / 255.0) * 3.0;
        let v = grey(l, gain_for_white(240.0));
        assert!((v as i32 - 128).abs() <= 1, "got {v}");
    }

    #[test]
    fn highlights_stay_distinguishable_and_hue_is_kept() {
        let g = gain_for_white(240.0);
        assert!(grey(3.0, g) >= grey(1.0, g));
        assert!(grey(6.0, g) >= grey(3.0, g));
        assert!(grey(30.0, g) >= grey(6.0, g));
        // a bright saturated orange keeps r > g > b
        let [r, gg, b] = tonemap_pixel(12.0, 6.0, 1.0, g);
        assert!(r > gg && gg > b, "{r} {gg} {b}");
    }

    #[test]
    fn no_nan_or_overflow() {
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30, 65504.0, 0.0, -0.0, -3.0] {
            for gain in [gain_for_white(80.0), gain_for_white(1000.0), 0.0, f32::NAN, 1e30] {
                let _ = tonemap_pixel(v, v, v, gain);
                let _ = tonemap_pixel(v, 1.0, -v, gain);
            }
        }
        assert_eq!(tonemap_pixel(f32::NAN, f32::NAN, f32::NAN, 1.0), [0, 0, 0]);
        assert_eq!(tonemap_pixel(f32::INFINITY, f32::INFINITY, f32::INFINITY, 1.0), [255, 255, 255]);
        assert_eq!(tonemap_pixel(-1.0, -1.0, -1.0, 1.0), [0, 0, 0]);
        assert!(rolloff(f32::INFINITY) <= 1.0 && rolloff(f32::INFINITY) > 0.99);
        assert!(rolloff(2.0) < rolloff(12.5), "peak HDR highlights must stay apart from moderate ones");
        assert_eq!(rolloff(f32::NAN), 0.0);
    }

    #[test]
    fn f16_decoding() {
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0x4000), 2.0);
        assert_eq!(f16_to_f32(0xC000), -2.0);
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        assert_eq!(f16_to_f32(0x7BFF), 65504.0);
        assert!(f16_to_f32(0x7C00).is_infinite());
        assert!(f16_to_f32(0x7E00).is_nan());
        assert!((f16_to_f32(0x0001) - 5.960_464_5e-8).abs() < 1e-12); // smallest subnormal
        assert!((f16_to_f32(0x0400) - 6.103_515_6e-5).abs() < 1e-10); // smallest normal
    }

    #[test]
    fn f16_picture_converts_with_the_same_math() {
        // 2x1 picture, pitch with padding: (1.0,1.0,1.0) and (3.0,0.5,0.0) at 240 nits
        let h = |v: f32| -> u16 {
            // exact for the values used here
            match v {
                x if x == 0.0 => 0x0000,
                x if x == 0.5 => 0x3800,
                x if x == 1.0 => 0x3C00,
                x if x == 3.0 => 0x4200,
                _ => unreachable!(),
            }
        };
        let px = [[1.0, 1.0, 1.0, 1.0], [3.0, 0.5, 0.0, 1.0]];
        let mut src = vec![0u8; 24];
        for (i, p) in px.iter().enumerate() {
            for (c, v) in p.iter().enumerate() {
                src[i * 8 + c * 2..][..2].copy_from_slice(&h(*v).to_le_bytes());
            }
        }
        let mut out = vec![0u8; 8];
        let g = gain_for_white(240.0);
        convert_f16_to_bgra(&src, 24, 2, 1, g, &mut out);
        let a = tonemap_pixel(1.0, 1.0, 1.0, g);
        assert_eq!(out[..4], [a[2], a[1], a[0], 255]);
        let b = tonemap_pixel(3.0, 0.5, 0.0, g);
        assert_eq!(out[4..], [b[2], b[1], b[0], 255]);
        // SDR white of a 240-nit desktop (3.0) is white
        assert!(b[0] >= 250);
    }

    #[test]
    fn pq_curve() {
        assert_eq!(pq_to_scrgb(0.0), 0.0);
        assert!((pq_to_scrgb(1.0) - 125.0).abs() < 0.01);
        // 203 nits reference white is PQ ~0.58 -> 2.54 scRGB
        let v = pq_to_scrgb(0.5807);
        assert!((v * 80.0 - 203.0).abs() < 3.0, "{} nits", v * 80.0);
        let mut prev = -1.0;
        for i in 0..=1023 {
            let y = pq_to_scrgb(i as f32 / 1023.0);
            assert!(y >= prev && y.is_finite());
            prev = y;
        }
        assert_eq!(pq_to_scrgb(f32::NAN), 0.0);
    }

    #[test]
    fn pq_picture_reference_white_is_white() {
        // 10-bit PQ code of 203 nits ~ 593 in all channels (BT.2020 white stays white in BT.709)
        let code = 593u32;
        let v = code | (code << 10) | (code << 20) | (3 << 30);
        let src = v.to_le_bytes();
        let mut out = [0u8; 4];
        convert_pq10_to_bgra(&src, 4, 1, 1, gain_for_white(203.0), &mut out);
        assert!(out[0] >= 248 && out[1] >= 248 && out[2] >= 248, "{out:?}");
        // black stays black
        let mut out = [9u8; 4];
        convert_pq10_to_bgra(&0u32.to_le_bytes(), 4, 1, 1, 1.0, &mut out);
        assert_eq!(out, [0, 0, 0, 255]);
    }

    #[test]
    fn gain_and_mode_parsing() {
        assert_eq!(gain_for_white(80.0), 1.0);
        assert_eq!(gain_for_white(240.0), 1.0 / 3.0);
        assert_eq!(gain_for_white(f32::NAN), 1.0);
        assert_eq!(gain_for_white(0.0), 2.0); // clamped to 40 nits
        assert_eq!(parse_mode(None), HdrMode::Auto);
        assert_eq!(parse_mode(Some("auto")), HdrMode::Auto);
        assert_eq!(parse_mode(Some("OFF")), HdrMode::Off);
        assert_eq!(parse_mode(Some("203")), HdrMode::White(203.0));
        assert_eq!(parse_mode(Some("5")), HdrMode::White(MIN_SDR_WHITE_NITS));
        assert_eq!(parse_mode(Some("zzz")), HdrMode::Auto);
    }
}
