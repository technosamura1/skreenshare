// Reaction stickers (v0.12.1): cybercats by @cybercats (https://ru.cybercats.store/), from their public sticker
// pack. Premultiplied BGRA squares made by tools from the pack's 512 px WebP files (cropped, centred, Lanczos):
// 160 px for the host's overlay, 96 and 48 px for the viewer's bar and pointer. Order = the bar and keys 1-4:
// ДА, НЕТ, смех, сердечко. Pure (also built into tools/lossim with bar.rs).

pub const COUNT: usize = 4;

pub struct Img {
    pub size: i32,
    pub px: &'static [u8],
}

macro_rules! set {
    ($n:literal) => {
        [
            include_bytes!(concat!("../assets/stickers/", $n, "_48.bgra")).as_slice(),
            include_bytes!(concat!("../assets/stickers/", $n, "_96.bgra")).as_slice(),
            include_bytes!(concat!("../assets/stickers/", $n, "_160.bgra")).as_slice(),
        ]
    };
}

static ALL: [[&[u8]; 3]; COUNT] = [set!("yes"), set!("no"), set!("laugh"), set!("heart")];
const SIZES: [i32; 3] = [48, 96, 160];

/// Sticker `i` in the smallest stored size that is at least `want` pixels (or the biggest).
pub fn get(i: usize, want: i32) -> Img {
    let k = SIZES.iter().position(|&s| s >= want).unwrap_or(SIZES.len() - 1);
    Img { size: SIZES[k], px: ALL[i.min(COUNT - 1)][k] }
}

/// Draws `img` scaled to `size` px with its centre at (cx, cy) into a 0x00RRGGBB buffer, faded by `alpha`.
/// Nearest-neighbour over a box of source pixels (good enough for shrinking a little).
pub fn blit(buf: &mut [u32], w: i32, h: i32, cx: i32, cy: i32, size: i32, img: &Img, alpha: f32) {
    if size <= 0 || alpha <= 0.0 {
        return;
    }
    let (x0, y0) = (cx - size / 2, cy - size / 2);
    let a = (alpha.clamp(0.0, 1.0) * 256.0) as u32;
    for dy in 0..size {
        let y = y0 + dy;
        if y < 0 || y >= h {
            continue;
        }
        let sy = (dy * img.size / size) as usize;
        for dx in 0..size {
            let x = x0 + dx;
            if x < 0 || x >= w {
                continue;
            }
            let sx = (dx * img.size / size) as usize;
            let o = (sy * img.size as usize + sx) * 4;
            let sa = img.px[o + 3] as u32 * a >> 8;
            if sa == 0 {
                continue;
            }
            let (sb, sg, sr) = (img.px[o] as u32 * a >> 8, img.px[o + 1] as u32 * a >> 8, img.px[o + 2] as u32 * a >> 8);
            let d = &mut buf[(y * w + x) as usize];
            let inv = 255 - sa;
            let r = sr + ((*d >> 16 & 0xFF) * inv) / 255;
            let g = sg + ((*d >> 8 & 0xFF) * inv) / 255;
            let b = sb + ((*d & 0xFF) * inv) / 255;
            *d = r.min(255) << 16 | g.min(255) << 8 | b.min(255);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sticker_is_a_full_square_with_something_in_it() {
        for i in 0..COUNT {
            for want in [1, 48, 60, 96, 120, 160, 400] {
                let img = get(i, want);
                assert_eq!(img.px.len(), (img.size * img.size * 4) as usize);
                assert!(img.size >= want.min(160));
                assert!(img.px.chunks(4).any(|p| p[3] == 255), "opaque pixels");
                assert!(img.px.chunks(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]), "premultiplied");
            }
        }
    }

    #[test]
    fn blit_stays_inside_and_covers_the_middle() {
        let (w, h) = (64, 40);
        let mut buf = vec![0x00_10_20_30u32; (w * h) as usize];
        let img = get(3, 48);
        blit(&mut buf, w, h, 0, 0, 48, &img, 1.0); // mostly off the edge: must not panic
        blit(&mut buf, w, h, 32, 20, 36, &img, 1.0);
        assert!(buf.iter().any(|&p| p != 0x00_10_20_30));
    }
}
