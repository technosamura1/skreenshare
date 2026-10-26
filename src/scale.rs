// Stop-gap scaling for the viewer window: after the window is resized, the decoder thread needs a frame or two
// to deliver a picture of the new size. Until then the old picture is stretched (bilinear) to fit, instead of
// sitting in black bars or being cut off. Pure std, unit-tested by tools/lossim.

/// Size of a `pw`x`ph` picture fitted into `ww`x`wh` keeping the aspect ratio (up or down).
pub fn fit_size(pw: usize, ph: usize, ww: usize, wh: usize) -> (usize, usize) {
    if pw == 0 || ph == 0 || ww == 0 || wh == 0 {
        return (0, 0);
    }
    if (ww as u64) * (ph as u64) <= (wh as u64) * (pw as u64) {
        (ww, (((ph as u64) * (ww as u64) + pw as u64 / 2) / pw as u64).max(1) as usize)
    } else {
        ((((pw as u64) * (wh as u64) + ph as u64 / 2) / ph as u64).max(1) as usize, wh)
    }
}

/// True when the picture is already (within 2 px) the size it should have in this window.
pub fn matches_window(pw: usize, ph: usize, ww: usize, wh: usize) -> bool {
    let (tw, th) = fit_size(pw, ph, ww, wh);
    tw.abs_diff(pw) <= 2 && th.abs_diff(ph) <= 2 && pw <= ww && ph <= wh
}

#[inline]
fn lerp(a: u32, b: u32, f: u32) -> u32 {
    // f in 0..=256; R and B travel together in one word, G alone.
    let rb = ((a & 0x00FF_00FF) * (256 - f) + (b & 0x00FF_00FF) * f) >> 8 & 0x00FF_00FF;
    let g = (((a >> 8) & 0xFF) * (256 - f) + ((b >> 8) & 0xFF) * f) >> 8 & 0xFF;
    rb | (g << 8)
}

/// Clears `dst` (`ww`x`wh`) to black and draws `src` (`sw`x`sh`) fitted and centred, bilinear.
pub fn blit_fit(dst: &mut [u32], ww: usize, wh: usize, src: &[u32], sw: usize, sh: usize) {
    if dst.len() < ww * wh {
        return;
    }
    dst[..ww * wh].fill(0);
    if sw == 0 || sh == 0 || src.len() < sw * sh {
        return;
    }
    let (tw, th) = fit_size(sw, sh, ww, wh);
    if tw == 0 || th == 0 {
        return;
    }
    let (ox, oy) = ((ww - tw) / 2, (wh - th) / 2);
    // Per destination column: left source pixel and weight of the right one (pixel centres).
    let xs: Vec<(usize, u32)> = (0..tw)
        .map(|x| {
            let fx = ((x as f32 + 0.5) * sw as f32 / tw as f32 - 0.5).max(0.0);
            let x0 = (fx as usize).min(sw - 1);
            (x0, (((fx - x0 as f32) * 256.0) as u32).min(256))
        })
        .collect();
    for y in 0..th {
        let fy = ((y as f32 + 0.5) * sh as f32 / th as f32 - 0.5).max(0.0);
        let y0 = (fy as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = (((fy - y0 as f32) * 256.0) as u32).min(256);
        let (r0, r1) = (&src[y0 * sw..(y0 + 1) * sw], &src[y1 * sw..(y1 + 1) * sw]);
        let row = &mut dst[(oy + y) * ww + ox..(oy + y) * ww + ox + tw];
        for (d, &(x0, wx)) in row.iter_mut().zip(&xs) {
            let x1 = (x0 + 1).min(sw - 1);
            *d = lerp(lerp(r0[x0], r0[x1], wx), lerp(r1[x0], r1[x1], wx), wy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect() {
        assert_eq!(fit_size(1920, 1080, 960, 600), (960, 540));
        assert_eq!(fit_size(1920, 1080, 1000, 400), (711, 400));
        assert_eq!(fit_size(100, 100, 300, 200), (200, 200));
        assert_eq!(fit_size(0, 5, 10, 10), (0, 0));
        assert_eq!(fit_size(10, 10, 0, 10), (0, 0));
    }

    #[test]
    fn matches_window_tolerates_two_pixels() {
        assert!(matches_window(1280, 720, 1280, 720));
        assert!(matches_window(1279, 720, 1280, 720));
        assert!(matches_window(1280, 718, 1280, 720));
        assert!(!matches_window(1920, 1080, 1280, 720)); // picture bigger than the window
        assert!(!matches_window(640, 360, 1280, 720)); // window grew, picture not yet
        assert!(matches_window(1280, 700, 1280, 720)); // a different aspect is fine: it only has to be the fitted size
    }

    #[test]
    fn solid_colour_stays_solid_and_letterbox_is_black() {
        let src = vec![0x00_80_40_20u32; 64 * 36];
        let mut dst = vec![1u32; 200 * 200];
        blit_fit(&mut dst, 200, 200, &src, 64, 36);
        let (tw, th) = fit_size(64, 36, 200, 200);
        assert_eq!((tw, th), (200, 113));
        let (ox, oy) = (0, (200 - th) / 2);
        for y in 0..200 {
            for x in 0..200 {
                let inside = x >= ox && x < ox + tw && y >= oy && y < oy + th;
                let v = dst[y * 200 + x];
                if inside {
                    assert_eq!(v, 0x00_80_40_20, "at {x},{y}");
                } else {
                    assert_eq!(v, 0);
                }
            }
        }
    }

    #[test]
    fn same_size_is_a_copy() {
        let src: Vec<u32> = (0..40 * 30).map(|i| ((i as u32).wrapping_mul(2654435761)) & 0x00FF_FFFF).collect();
        let mut dst = vec![0u32; 40 * 30];
        blit_fit(&mut dst, 40, 30, &src, 40, 30);
        assert_eq!(dst, src);
    }

    #[test]
    fn downscale_averages_neighbours_and_never_goes_out_of_bounds() {
        // black | white halves, scaled to half width: the middle must be a grey, not a hard edge.
        let mut src = vec![0u32; 8 * 2];
        for y in 0..2 {
            for x in 4..8 {
                src[y * 8 + x] = 0x00FF_FFFF;
            }
        }
        let mut dst = vec![0u32; 4 * 2];
        blit_fit(&mut dst, 4, 2, &src, 8, 2);
        assert_eq!(dst[0], 0);
        assert_eq!(dst[3], 0x00FF_FFFF);
        for (sw, sh, ww, wh) in [(1, 1, 1, 1), (3, 7, 5, 2), (1920, 1080, 7, 3), (2, 2, 1000, 3), (5, 5, 0, 0)] {
            let src = vec![0x00_11_22_33u32; sw * sh];
            let mut dst = vec![0u32; ww * wh];
            blit_fit(&mut dst, ww, wh, &src, sw, sh);
        }
        blit_fit(&mut [], 10, 10, &[1], 1, 1); // short destination: nothing happens
    }
}
