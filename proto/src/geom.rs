//! Picture geometry helpers shared by the sender (kept in `proto` so they are unit-tested on any OS).

/// Where a `sw`x`sh` picture goes inside an `ow`x`oh` canvas: scaled to fit (up or down) keeping its
/// aspect ratio and centred, i.e. letterboxed. Returns `(x, y, w, h)`; the size is even (and at least 2)
/// so it can be scaled straight into a YUV 4:2:0 encoder input, the offsets are even too.
pub fn letterbox_rect(sw: u32, sh: u32, ow: u32, oh: u32) -> (u32, u32, u32, u32) {
    if sw == 0 || sh == 0 || ow < 2 || oh < 2 {
        return (0, 0, ow, oh);
    }
    let s = (ow as f64 / sw as f64).min(oh as f64 / sh as f64);
    let even = |v: f64, max: u32| (((v.round() as u32).max(2)) & !1).min(max & !1);
    let fw = even(sw as f64 * s, ow);
    let fh = even(sh as f64 * s, oh);
    let x = ((ow - fw) / 2) & !1;
    let y = ((oh - fh) / 2) & !1;
    (x, y, fw, fh)
}

/// Shortens a window title for a menu: whitespace is collapsed, long titles end with an ellipsis.
pub fn ellipsize(title: &str, max_chars: usize) -> String {
    let t = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max_chars {
        t
    } else {
        let mut s: String = t.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_aspect_fills_canvas() {
        assert_eq!(letterbox_rect(1280, 720, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(letterbox_rect(3840, 2160, 1920, 1080), (0, 0, 1920, 1080));
    }

    #[test]
    fn narrow_window_gets_side_bars() {
        // 4:3 window on a 16:9 canvas: full height, bars left and right.
        let (x, y, w, h) = letterbox_rect(800, 600, 1920, 1080);
        assert_eq!((y, h), (0, 1080));
        assert_eq!(w, 1440);
        assert_eq!(x, 240);
    }

    #[test]
    fn wide_window_gets_top_bottom_bars() {
        let (x, y, w, h) = letterbox_rect(2000, 500, 1920, 1080);
        assert_eq!((x, w), (0, 1920));
        assert_eq!(h, 480);
        assert_eq!(y, 300);
    }

    #[test]
    fn portrait_window() {
        let (x, y, w, h) = letterbox_rect(600, 1000, 1920, 1080);
        assert_eq!((y, h), (0, 1080));
        assert_eq!(w, 648);
        assert_eq!(x, 636);
    }

    #[test]
    fn result_always_fits_and_is_even() {
        for &(sw, sh) in &[(1, 1), (3, 7), (333, 777), (1920, 1080), (5000, 40), (40, 5000), (1281, 721)] {
            for &(ow, oh) in &[(1920u32, 1080u32), (1280, 720), (640, 360)] {
                let (x, y, w, h) = letterbox_rect(sw, sh, ow, oh);
                assert!(w >= 2 && h >= 2 && w % 2 == 0 && h % 2 == 0, "{sw}x{sh} -> {w}x{h}");
                assert!(x % 2 == 0 && y % 2 == 0);
                assert!(x + w <= ow && y + h <= oh, "{sw}x{sh} in {ow}x{oh}: {x},{y},{w},{h}");
            }
        }
    }

    #[test]
    fn degenerate_input_is_whole_canvas() {
        assert_eq!(letterbox_rect(0, 100, 1920, 1080), (0, 0, 1920, 1080));
    }

    #[test]
    fn ellipsize_titles() {
        assert_eq!(ellipsize("  a   b\tc ", 20), "a b c");
        assert_eq!(ellipsize("абвгдеёжзий", 5), "абвг…");
        assert_eq!(ellipsize("short", 5), "short");
    }
}
