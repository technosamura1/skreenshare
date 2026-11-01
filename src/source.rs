// What to show: the whole screen, one monitor or one window. Pure logic (no Windows calls), so it is unit-tested
// on any OS (tools/lossim includes this file too): the menu, the command line for the host process, and the
// geometry of fitting a window into the fixed stream canvas.

/// One entry of the "Что показывать" menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The whole screen (the primary monitor).
    Screen,
    /// A particular monitor (index in the monitor list).
    Monitor(usize),
    /// One window, by its HWND.
    Window(isize),
}

/// A top-level window as the menu needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinEntry {
    pub hwnd: isize,
    pub title: String,
    pub minimized: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub choice: Choice,
    pub label: String,
}

/// Menu: "Весь экран" first (default), each monitor when there are several, then each window.
/// `monitors` are ready-made labels (size, adapter, ...).
pub fn build_menu(monitors: &[String], windows: &[WinEntry]) -> Vec<MenuItem> {
    let mut m = vec![MenuItem { choice: Choice::Screen, label: "Весь экран".into() }];
    if monitors.len() > 1 {
        for (i, l) in monitors.iter().enumerate() {
            m.push(MenuItem { choice: Choice::Monitor(i), label: format!("Монитор {}: {l}", i + 1) });
        }
    }
    for w in windows {
        m.push(MenuItem { choice: Choice::Window(w.hwnd), label: window_label(w) });
    }
    m
}

pub fn window_label(w: &WinEntry) -> String {
    let t = ellipsize(&w.title, 60);
    if w.minimized {
        format!("Окно: {t} (свёрнуто)")
    } else {
        format!("Окно: {t}")
    }
}

/// Added to the status when this Windows can't hide the yellow capture border (no `IsBorderRequired`).
pub const BORDER_NOTE: &str = ". Жёлтую рамку Windows тут не убрать (нужна Windows 11) — для трансляции без рамки выберите «Весь экран»";

/// The label under the self-preview: "Окно: <заголовок>" or "Весь экран". Same wording as in the menu.
pub fn source_label(window_title: Option<&str>) -> String {
    match window_title {
        Some(t) => format!("Окно: {}", ellipsize(t, 60)),
        None => "Весь экран".into(),
    }
}

/// `host` arguments for a choice. `primary` is the index of the primary monitor: windows are streamed on a
/// canvas of that monitor's size, and "the whole screen" fallback is that monitor too.
pub fn host_args(choice: Choice, primary: usize) -> Vec<String> {
    match choice {
        Choice::Screen => vec!["--monitor".into(), primary.to_string()],
        Choice::Monitor(i) => vec!["--monitor".into(), i.to_string()],
        Choice::Window(h) => vec!["--monitor".into(), primary.to_string(), "--window".into(), h.to_string()],
    }
}

/// A request to change what is streamed while the stream is running (GUI -> host, one stdin line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceReq {
    /// A monitor by its index (the "whole screen" is the primary monitor's index).
    Monitor(usize),
    Window(isize),
}

impl SourceReq {
    /// The GUI's menu choice as a request; `primary` is the index of the primary monitor.
    pub fn from_choice(c: Choice, primary: usize) -> SourceReq {
        match c {
            Choice::Screen => SourceReq::Monitor(primary),
            Choice::Monitor(i) => SourceReq::Monitor(i),
            Choice::Window(h) => SourceReq::Window(h),
        }
    }

    /// `SOURCE monitor 1` / `SOURCE window 65814`
    pub fn to_line(self) -> String {
        match self {
            SourceReq::Monitor(i) => format!("SOURCE monitor {i}"),
            SourceReq::Window(h) => format!("SOURCE window {h}"),
        }
    }

    pub fn parse(line: &str) -> Option<SourceReq> {
        let mut it = line.split_whitespace();
        if it.next()? != "SOURCE" {
            return None;
        }
        let kind = it.next()?;
        let v = it.next()?;
        if it.next().is_some() {
            return None;
        }
        match kind {
            "monitor" => v.parse().ok().map(SourceReq::Monitor),
            "window" => v.parse().ok().filter(|&h| h != 0).map(SourceReq::Window),
            _ => None,
        }
    }
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

/// Where a `sw`x`sh` picture goes inside an `ow`x`oh` canvas: scaled to fit (up or down) keeping its aspect
/// ratio and centred, i.e. letterboxed. Returns `(x, y, w, h)`; sizes and offsets are even (size at least 2),
/// so the result can go straight into a YUV 4:2:0 encoder input.
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

/// Where a spot of the streamed picture lands on the host's screen (remote control by position).
/// `pos`: 0..=65535 both ways over the whole picture (the canvas). `content`: the window's own size when a window
/// is letterboxed into the canvas (None: a monitor fills it). `target`: that content's rectangle on the host's
/// screen (left, top, width, height). A spot on the black bars goes to the nearest edge of the window.
pub fn picture_to_screen(pos: (u16, u16), canvas: (u32, u32), content: Option<(u32, u32)>, target: (i32, i32, i32, i32)) -> (i32, i32) {
    let (cw, ch) = (canvas.0.max(1) as f64, canvas.1.max(1) as f64);
    let (u, v) = (pos.0 as f64 / 65535.0 * cw, pos.1 as f64 / 65535.0 * ch);
    let (fx, fy) = match content {
        None => (u / cw, v / ch),
        Some((sw, sh)) => {
            let (x, y, fw, fh) = letterbox_rect(sw, sh, canvas.0, canvas.1);
            ((u - x as f64) / fw.max(1) as f64, (v - y as f64) / fh.max(1) as f64)
        }
    };
    let (fx, fy) = (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0));
    let (l, t, w, h) = target;
    (l + (fx * (w - 1).max(0) as f64).round() as i32, t + (fy * (h - 1).max(0) as f64).round() as i32)
}

/// Draws `src` (`sw`x`sh` BGRA, tight rows) into the `ow`x`oh` canvas `dst` (rows of `stride` bytes): scaled to
/// fit and centred, bars black. `scale(src, sw, sh, out, fw, fh)` resizes into a tight `fw`x`fh` buffer
/// (only called when the size really differs). A window picture of any size (the user drags its edge) lands in
/// the same fixed canvas.
pub fn draw_letterboxed(
    src: &[u8],
    (sw, sh): (u32, u32),
    dst: &mut [u8],
    stride: usize,
    (ow, oh): (u32, u32),
    scratch: &mut Vec<u8>,
    mut scale: impl FnMut(&[u8], u32, u32, &mut [u8], u32, u32) -> Result<(), String>,
) -> Result<(), String> {
    if src.len() < sw as usize * sh as usize * 4 {
        return Err("кадр окна меньше заявленного размера".into());
    }
    let (x, y, fw, fh) = letterbox_rect(sw, sh, ow, oh);
    let (x, y, fw, fh) = (x as usize, y as usize, fw as usize, fh as usize);
    let (ow, oh) = (ow as usize, oh as usize);
    if dst.len() < stride * (oh - 1) + ow * 4 {
        return Err("буфер кодера слишком мал".into());
    }
    // Common 60/144 FPS monitor path: same-size BGRA is a direct copy.
    // Avoid clearing and re-touching every row when there are no black bars.
    if (sw as usize, sh as usize) == (ow, oh) && (fw, fh) == (ow, oh) {
        let bytes = ow * 4;
        if stride == bytes {
            dst[..bytes * oh].copy_from_slice(&src[..bytes * oh]);
        } else {
            for row in 0..oh {
                dst[row * stride..row * stride + bytes]
                    .copy_from_slice(&src[row * bytes..(row + 1) * bytes]);
            }
        }
        return Ok(());
    }
    for row in 0..oh {
        let line = &mut dst[row * stride..row * stride + ow * 4];
        if row < y || row >= y + fh {
            line.fill(0);
        } else {
            line[..x * 4].fill(0);
            line[(x + fw) * 4..].fill(0);
        }
    }
    if (fw, fh) == (sw as usize, sh as usize) {
        for row in 0..fh {
            dst[(y + row) * stride + x * 4..][..fw * 4].copy_from_slice(&src[row * sw as usize * 4..][..fw * 4]);
        }
    } else {
        scratch.resize(fw * fh * 4, 0);
        scale(src, sw, sh, scratch, fw as u32, fh as u32)?;
        for row in 0..fh {
            dst[(y + row) * stride + x * 4..][..fw * 4].copy_from_slice(&scratch[row * fw * 4..][..fw * 4]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_spots_map_to_the_host_screen() {
        // Whole monitor 2560x1440 at (0,0): the centre of the picture is the centre of the screen.
        assert_eq!(picture_to_screen((32768, 32768), (2560, 1440), None, (0, 0, 2560, 1440)), (1280, 720));
        assert_eq!(picture_to_screen((0, 0), (2560, 1440), None, (-1080, 100, 1080, 1920)), (-1080, 100));
        assert_eq!(picture_to_screen((65535, 65535), (2560, 1440), None, (0, 0, 2560, 1440)), (2559, 1439));
        // A 1000x1000 window at (300, 200) letterboxed into a 2560x1440 canvas: it fills 1440x1440 in the middle
        // (x 560..2000). The canvas centre is the window centre; its left bar edge is the window's left edge.
        let w = Some((1000, 1000));
        assert_eq!(picture_to_screen((32768, 32768), (2560, 1440), w, (300, 200, 1000, 1000)), (800, 700));
        let left_edge = (560.0 / 2560.0 * 65535.0) as u16;
        assert_eq!(picture_to_screen((left_edge, 0), (2560, 1440), w, (300, 200, 1000, 1000)).0, 300);
        // A click on the black bar goes to the nearest window edge, not somewhere else on the screen.
        assert_eq!(picture_to_screen((100, 32768), (2560, 1440), w, (300, 200, 1000, 1000)), (300, 700));
    }

    #[test]
    fn source_request_round_trips() {
        for r in [SourceReq::Monitor(0), SourceReq::Monitor(2), SourceReq::Window(65814), SourceReq::Window(-5)] {
            assert_eq!(SourceReq::parse(&r.to_line()), Some(r));
        }
        assert_eq!(SourceReq::parse("  SOURCE   window  12 \r\n"), Some(SourceReq::Window(12)));
        for bad in ["", "STOP", "SOURCE", "SOURCE monitor", "SOURCE monitor x", "SOURCE window 0", "SOURCE screen 1", "SOURCE monitor 1 2", "source monitor 1"] {
            assert_eq!(SourceReq::parse(bad), None, "{bad:?}");
        }
        assert_eq!(SourceReq::from_choice(Choice::Screen, 1), SourceReq::Monitor(1));
        assert_eq!(SourceReq::from_choice(Choice::Window(7), 1), SourceReq::Window(7));
    }

    #[test]
    fn identical_monitor_frame_copies_without_scaling() {
        let input: Vec<u8> = (0u8..64).collect(); // 4x4 BGRA
        let mut dst = vec![255; 4 * 4 * 4];
        let mut scratch = Vec::new();
        draw_letterboxed(&input, (4, 4), &mut dst, 16, (4, 4), &mut scratch,
            |_, _, _, _, _, _| panic!("matching canvas must not scale")).unwrap();
        assert_eq!(dst, input);
        assert!(scratch.is_empty());
    }

    #[test]
    fn matching_monitor_frame_respects_padded_encoder_stride() {
        let input: Vec<u8> = (0u8..32).collect(); // 4x2 BGRA
        let mut dst = vec![255; 40];
        let mut scratch = Vec::new();
        draw_letterboxed(&input, (4, 2), &mut dst, 24, (4, 2), &mut scratch,
            |_, _, _, _, _, _| panic!("matching canvas must not scale")).unwrap();
        assert_eq!(&dst[..16], &input[..16]);
        assert_eq!(&dst[24..40], &input[16..32]);
        assert_eq!(&dst[16..24], &[255; 8]);
    }

    #[test]
    fn same_aspect_fills_canvas() {
        assert_eq!(letterbox_rect(1280, 720, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(letterbox_rect(3840, 2160, 1920, 1080), (0, 0, 1920, 1080));
    }

    #[test]
    fn narrow_window_gets_side_bars() {
        let (x, y, w, h) = letterbox_rect(800, 600, 1920, 1080);
        assert_eq!((y, h, w, x), (0, 1080, 1440, 240));
    }

    #[test]
    fn wide_window_gets_top_bottom_bars() {
        let (x, y, w, h) = letterbox_rect(2000, 500, 1920, 1080);
        assert_eq!((x, w, h, y), (0, 1920, 480, 300));
    }

    #[test]
    fn portrait_window() {
        let (x, y, w, h) = letterbox_rect(600, 1000, 1920, 1080);
        assert_eq!((y, h, w, x), (0, 1080, 648, 636));
    }

    #[test]
    fn result_always_fits_and_is_even() {
        for &(sw, sh) in &[(1, 1), (3, 7), (333, 777), (1920, 1080), (5000, 40), (40, 5000), (1281, 721), (2, 2)] {
            for &(ow, oh) in &[(1920u32, 1080u32), (1280, 720), (640, 360), (2560, 1440)] {
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

    fn win(h: isize, t: &str, min: bool) -> WinEntry {
        WinEntry { hwnd: h, title: t.into(), minimized: min }
    }

    #[test]
    fn menu_default_is_whole_screen_and_single_monitor_is_not_listed() {
        let m = build_menu(&["1920×1080".into()], &[win(7, "Блокнот", false)]);
        assert_eq!(m[0], MenuItem { choice: Choice::Screen, label: "Весь экран".into() });
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].choice, Choice::Window(7));
        assert_eq!(m[1].label, "Окно: Блокнот");
    }

    #[test]
    fn menu_lists_each_monitor_when_several_and_marks_minimized() {
        let m = build_menu(&["A".into(), "B".into()], &[win(1, "x", true), win(2, "y", false)]);
        let labels: Vec<_> = m.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Весь экран", "Монитор 1: A", "Монитор 2: B", "Окно: x (свёрнуто)", "Окно: y"]);
        assert_eq!(m[2].choice, Choice::Monitor(1));
    }

    #[test]
    fn long_titles_are_shortened_in_the_menu() {
        let l = window_label(&win(1, &"я".repeat(200), false));
        assert_eq!(l.chars().count(), "Окно: ".chars().count() + 60);
        assert!(l.ends_with('…'));
    }

    #[test]
    fn border_note_is_short_and_mentions_the_way_out() {
        assert!(BORDER_NOTE.contains("Windows 11") && BORDER_NOTE.contains("Весь экран"));
        assert!(BORDER_NOTE.chars().count() < 120);
    }

    #[test]
    fn preview_label_names_the_source() {
        assert_eq!(source_label(None), "Весь экран");
        assert_eq!(source_label(Some("  Блокнот \t — a.txt ")), "Окно: Блокнот — a.txt");
        assert_eq!(source_label(Some(&"x".repeat(100))).chars().count(), "Окно: ".chars().count() + 60);
        // the same text as the menu entry for that window
        let w = WinEntry { hwnd: 1, title: "Игра".into(), minimized: false };
        assert_eq!(source_label(Some(&w.title)), window_label(&w));
    }

    #[test]
    fn host_args_for_each_choice() {
        assert_eq!(host_args(Choice::Screen, 1), ["--monitor", "1"]);
        assert_eq!(host_args(Choice::Monitor(0), 1), ["--monitor", "0"]);
        assert_eq!(host_args(Choice::Window(-5), 1), ["--monitor", "1", "--window", "-5"]);
    }

    /// Nearest-neighbour stand-in for swscale.
    fn nn(src: &[u8], sw: u32, sh: u32, out: &mut [u8], dw: u32, dh: u32) -> Result<(), String> {
        for y in 0..dh as usize {
            for x in 0..dw as usize {
                let (sx, sy) = (x * sw as usize / dw as usize, y * sh as usize / dh as usize);
                out[(y * dw as usize + x) * 4..][..4].copy_from_slice(&src[(sy * sw as usize + sx) * 4..][..4]);
            }
        }
        Ok(())
    }

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w * h * 4) as usize]
    }

    #[test]
    fn letterboxed_window_has_black_bars_and_fills_the_picture_area() {
        let (ow, oh, stride) = (64u32, 36u32, 64 * 4 + 16);
        let mut dst = vec![9u8; stride * oh as usize];
        let mut scratch = Vec::new();
        // 4:3 window into 16:9: bars left and right.
        draw_letterboxed(&solid(40, 30, 200), (40, 30), &mut dst, stride, (ow, oh), &mut scratch, nn).unwrap();
        let (x, y, w, h) = letterbox_rect(40, 30, ow, oh);
        for row in 0..oh as usize {
            for col in 0..ow as usize {
                let inside = (col as u32) >= x && (col as u32) < x + w && (row as u32) >= y && (row as u32) < y + h;
                let px = dst[row * stride + col * 4];
                assert_eq!(px, if inside { 200 } else { 0 }, "pixel {col},{row}");
            }
        }
        // stride padding untouched
        assert_eq!(dst[ow as usize * 4], 9);
    }

    #[test]
    fn same_size_skips_the_scaler() {
        let mut dst = vec![0u8; 8 * 4 * 6];
        let mut scratch = Vec::new();
        let mut called = false;
        draw_letterboxed(&solid(8, 6, 77), (8, 6), &mut dst, 32, (8, 6), &mut scratch, |_, _, _, _, _, _| {
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(!called);
        assert!(dst.iter().all(|&b| b == 77));
    }

    #[test]
    fn resizing_the_window_keeps_the_canvas_size() {
        // The picture size changes from frame to frame; the canvas buffer never does and is fully rewritten.
        let (ow, oh) = (32u32, 18u32);
        let mut dst = vec![5u8; (ow * oh * 4) as usize];
        let mut scratch = Vec::new();
        for &(w, h) in &[(100u32, 50u32), (50, 100), (7, 7), (640, 360), (2, 2)] {
            draw_letterboxed(&solid(w, h, 100), (w, h), &mut dst, ow as usize * 4, (ow, oh), &mut scratch, nn).unwrap();
            assert_eq!(dst.len(), (ow * oh * 4) as usize);
            assert!(dst.iter().all(|&b| b == 0 || b == 100), "{w}x{h}: stale pixels left");
            let (x, y, fw, fh) = letterbox_rect(w, h, ow, oh);
            assert_eq!(dst[((y + fh / 2) * ow + x + fw / 2) as usize * 4], 100);
        }
    }

    #[test]
    fn short_or_missing_source_is_an_error_not_a_panic() {
        let mut dst = vec![0u8; 16 * 16 * 4];
        let mut scratch = Vec::new();
        assert!(draw_letterboxed(&solid(4, 4, 1)[..10], (4, 4), &mut dst, 64, (16, 16), &mut scratch, nn).is_err());
        assert!(draw_letterboxed(&solid(4, 4, 1), (4, 4), &mut dst[..100], 64, (16, 16), &mut scratch, nn).is_err());
    }
}
