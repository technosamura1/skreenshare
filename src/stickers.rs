// Reaction stickers (v0.12.2): animated cybercats by @cybercats (https://ru.cybercats.store/), from their
// Telegram pack (t.me/addstickers/cybercats_stickers): the first 15 plus ДА, НЕТ and a laughing one.
// assets/stickers/cats.zst is made by tools/make-stickers.py from the pack's .webm files: every frame as a
// premultiplied BGRA square, 48 px (bar, picker) and 128 px (pointer, flying ones, the host's screen), with its
// duration. Unpacked once on first use. Pure (also built into tools/lossim with bar.rs).
use std::sync::OnceLock;
use std::time::Duration;

/// ДА, НЕТ, смех, сердечко: the bar's quick buttons and keys 1-4.
pub const FAVORITES: [usize; 4] = [15, 16, 17, 7];
const SIZES: [i32; 2] = [48, 128];

pub struct Img {
    pub size: i32,
    pub px: &'static [u8],
}

struct Sticker {
    /// Frame durations, ms.
    durs: Vec<u16>,
    total: u32,
    /// Where the frames of each size start in the unpacked data.
    at: [usize; 2],
}

struct Pack {
    raw: Vec<u8>,
    items: Vec<Sticker>,
}

fn parse(raw: Vec<u8>) -> Option<Pack> {
    if raw.get(..4)? != b"CAT1" {
        return None;
    }
    let n = *raw.get(4)? as usize;
    let mut p = 5;
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let k = *raw.get(p)? as usize;
        p += 1;
        let durs: Vec<u16> = (0..k).map(|i| raw.get(p + 2 * i..p + 2 * i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))).collect::<Option<_>>()?;
        p += 2 * k;
        let total = durs.iter().map(|&d| d as u32).sum::<u32>().max(1);
        items.push(Sticker { durs, total, at: [0, 0] });
    }
    for it in &mut items {
        for (s, size) in SIZES.iter().enumerate() {
            it.at[s] = p;
            p += it.durs.len() * (size * size * 4) as usize;
        }
    }
    (p <= raw.len() && n > 0).then_some(Pack { raw, items })
}

fn pack() -> &'static Pack {
    static P: OnceLock<Pack> = OnceLock::new();
    P.get_or_init(|| {
        let z = include_bytes!("../assets/stickers/cats.zst");
        let raw = zstd::bulk::decompress(z, 64 << 20).expect("stickers");
        parse(raw).expect("stickers")
    })
}

/// How many stickers there are.
pub fn count() -> usize {
    pack().items.len()
}

/// Words for the viewer's line (the favourites have names).
pub fn name(id: usize) -> &'static str {
    match FAVORITES.iter().position(|&f| f == id) {
        Some(0) => "ДА",
        Some(1) => "НЕТ",
        Some(2) => "смех",
        Some(3) => "сердечко",
        _ => "стикер",
    }
}

/// Sticker `id` at `t` into its animation (it loops), in the smaller stored size that is at least `want` px.
pub fn get(id: usize, want: i32, t: Duration) -> Img {
    let pk = pack();
    let it = &pk.items[id.min(pk.items.len() - 1)];
    let s = if want <= SIZES[0] { 0 } else { 1 };
    let mut ms = (t.as_millis() % it.total as u128) as u32;
    let mut f = 0;
    while f + 1 < it.durs.len() && ms >= it.durs[f] as u32 {
        ms -= it.durs[f] as u32;
        f += 1;
    }
    let len = (SIZES[s] * SIZES[s] * 4) as usize;
    let o = it.at[s] + f * len;
    Img { size: SIZES[s], px: &pk.raw[o..o + len] }
}

/// Draws `img` scaled to `size` px with its centre at (cx, cy) into a 0x00RRGGBB buffer, faded by `alpha`.
/// Shrinking averages a small box of source pixels, so 128 px frames stay smooth at pointer size.
pub fn blit(buf: &mut [u32], w: i32, h: i32, cx: i32, cy: i32, size: i32, img: &Img, alpha: f32) {
    if size <= 0 || alpha <= 0.0 {
        return;
    }
    let (x0, y0) = (cx - size / 2, cy - size / 2);
    let a = (alpha.clamp(0.0, 1.0) * 256.0) as u32;
    let step = (img.size / size).max(1); // source pixels per output pixel, each way
    for dy in 0..size {
        let y = y0 + dy;
        if y < 0 || y >= h {
            continue;
        }
        let sy = dy * img.size / size;
        for dx in 0..size {
            let x = x0 + dx;
            if x < 0 || x >= w {
                continue;
            }
            let sx = dx * img.size / size;
            let mut acc = [0u32; 4];
            let mut n = 0;
            for yy in sy..(sy + step).min(img.size) {
                for xx in sx..(sx + step).min(img.size) {
                    let o = ((yy * img.size + xx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += img.px[o + c] as u32;
                    }
                    n += 1;
                }
            }
            let [sb, sg, sr, sa] = acc.map(|v| v / n.max(1) * a >> 8);
            if sa == 0 {
                continue;
            }
            let d = &mut buf[(y * w + x) as usize];
            let inv = 255 - sa.min(255);
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
    fn the_pack_unpacks_and_every_frame_is_a_premultiplied_square() {
        assert_eq!(count(), 18);
        for id in 0..count() {
            for want in [1, 48, 60, 128, 400] {
                let img = get(id, want, Duration::ZERO);
                assert_eq!(img.px.len(), (img.size * img.size * 4) as usize);
                assert!(img.px.chunks(4).any(|p| p[3] > 200), "visible pixels");
                assert!(img.px.chunks(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]), "premultiplied");
            }
        }
        let see_through = (0..count()).filter(|&id| get(id, 128, Duration::ZERO).px.chunks(4).any(|p| p[3] == 0)).count();
        assert!(see_through >= count() - 3, "most stickers have a transparent background: {see_through}");
        assert_eq!([name(15), name(16), name(17), name(7), name(0)], ["ДА", "НЕТ", "смех", "сердечко", "стикер"]);
    }

    #[test]
    fn stickers_move_and_loop() {
        let moving = (0..count()).filter(|&id| get(id, 128, Duration::ZERO).px != get(id, 128, Duration::from_millis(170)).px).count();
        assert!(moving >= count() - 2, "{moving}");
        let total = Duration::from_millis(pack().items[7].total as u64);
        assert_eq!(get(7, 128, Duration::from_millis(30)).px, get(7, 128, total + Duration::from_millis(30)).px);
    }

    #[test]
    fn blit_stays_inside_and_covers_the_middle() {
        let (w, h) = (64, 40);
        let mut buf = vec![0x00_10_20_30u32; (w * h) as usize];
        let img = get(7, 128, Duration::ZERO);
        blit(&mut buf, w, h, 0, 0, 48, &img, 1.0); // mostly off the edge: must not panic
        blit(&mut buf, w, h, 32, 20, 36, &img, 1.0);
        assert!(buf.iter().any(|&p| p != 0x00_10_20_30));
    }
}
