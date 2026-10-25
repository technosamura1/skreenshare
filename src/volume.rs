// Viewer volume: the level/mute state shared with the audio callback, the small overlay that a right click opens
// (geometry, hit-testing and drawing into a plain 0x00RRGGBB buffer), and the tiny settings file helpers.
// Pure std, no Windows calls, so it is unit-tested on any OS (tools/lossim includes this file too).
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Highest level: 150% (a bit of boost for quiet streams; the output is clamped, so it can only distort, not blow up).
pub const MAX_LEVEL: f32 = 1.5;
/// Level at start when nothing is saved.
pub const DEFAULT_LEVEL: f32 = 1.0;

/// Level and mute, shared between the window thread (sets) and the audio callback (reads).
pub struct VolumeCtl {
    bits: AtomicU32,
    muted: AtomicBool,
}

impl VolumeCtl {
    pub fn new(level: f32, muted: bool) -> VolumeCtl {
        VolumeCtl { bits: AtomicU32::new(clamp_level(level).to_bits()), muted: AtomicBool::new(muted) }
    }
    pub fn level(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn set_level(&self, v: f32) {
        self.bits.store(clamp_level(v).to_bits(), Ordering::Relaxed);
    }
    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }
    pub fn toggle_mute(&self) {
        self.muted.store(!self.muted(), Ordering::Relaxed);
    }
    /// The factor the audio callback multiplies samples by.
    pub fn gain(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            self.level()
        }
    }
}

pub fn clamp_level(v: f32) -> f32 {
    if v.is_nan() {
        DEFAULT_LEVEL
    } else {
        v.clamp(0.0, MAX_LEVEL)
    }
}

/// Applies `gain` to interleaved samples, moving smoothly from `*cur` to `gain` over the buffer (no clicks when the
/// slider jumps), and clamps to [-1, 1] (the 150% boost must never wrap or overflow into the device).
pub fn apply_gain(out: &mut [f32], channels: usize, cur: &mut f32, gain: f32) {
    let ch = channels.max(1);
    let frames = (out.len() / ch).max(1);
    let start = *cur;
    if (start - gain).abs() < 1e-4 {
        *cur = gain;
        if (gain - 1.0).abs() < 1e-4 {
            return; // 100%: leave the samples exactly as they are
        }
        for s in out.iter_mut() {
            *s = (*s * gain).clamp(-1.0, 1.0);
        }
        return;
    }
    for (i, f) in out.chunks_mut(ch).enumerate() {
        let g = start + (gain - start) * ((i + 1) as f32 / frames as f32);
        for s in f.iter_mut() {
            *s = (*s * g).clamp(-1.0, 1.0);
        }
    }
    *cur = gain;
}

// ---------------------------------------------------------------- settings file (key=value lines)

/// Value of `key` in an ini-like text.
pub fn ini_get<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then_some(v.trim())
    })
}

/// The text with `key` set to `value` (other lines are kept, so the GUI and the viewer don't wipe each other's settings).
pub fn ini_set(text: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut done = false;
    for l in text.lines() {
        if l.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
            if !done {
                out.push_str(&format!("{key}={value}\n"));
                done = true;
            }
        } else if !l.trim().is_empty() {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !done {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

/// Saved level (0..1.5) and mute from the settings text.
pub fn load(text: &str) -> (f32, bool) {
    let level = ini_get(text, "volume").and_then(|v| v.parse::<f32>().ok()).map_or(DEFAULT_LEVEL, |p| clamp_level(p / 100.0));
    let muted = ini_get(text, "muted") == Some("1");
    (level, muted)
}

pub fn percent(level: f32) -> u32 {
    (clamp_level(level) * 100.0).round() as u32
}


/// Tiny settings file shared by the GUI and the viewer: %LOCALAPPDATA%\\screenshare\\gui.ini
pub fn settings_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::PathBuf::from(base).join("screenshare").join("gui.ini"))
}

pub fn read_settings() -> String {
    settings_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default()
}

/// Sets one key in the settings file, keeping the others. Errors are ignored: settings are a convenience.
pub fn write_setting(key: &str, value: &str) {
    if let Some(p) = settings_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, ini_set(&read_settings(), key, value));
    }
}

/// Saves the level and mute for the next start.
pub fn save(v: &VolumeCtl) {
    let text = ini_set(&read_settings(), "volume", &percent(v.level()).to_string());
    let text = ini_set(&text, "muted", if v.muted() { "1" } else { "0" });
    if let Some(p) = settings_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, text);
    }
}

// ---------------------------------------------------------------- overlay geometry

const PANEL_W: i32 = 300;
const PANEL_H: i32 = 72;
const MUTE_X: i32 = 14;
const MUTE_SIZE: i32 = 44;
const TRACK_X0: i32 = 74;
const TRACK_X1: i32 = 234;
const TEXT_X: i32 = 244;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Outside,
    /// On the panel but not on a control.
    Inside,
    Mute,
    /// On the slider: the level that spot means.
    Slider(f32),
}

/// Where the panel is: top-left corner and the integer UI scale (1 on a normal screen, 2 on 200% DPI, ...).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Panel {
    pub x: i32,
    pub y: i32,
    pub s: i32,
}

impl Panel {
    /// Panel for a click at (`cx`, `cy`): just below-right of the cursor, pushed back inside the window.
    pub fn place(ww: i32, wh: i32, cx: i32, cy: i32, s: i32) -> Panel {
        let s = s.clamp(1, 4);
        let (w, h) = (PANEL_W * s, PANEL_H * s);
        let x = (cx + 8 * s).min(ww - w).max(0);
        let y = (cy + 8 * s).min(wh - h).max(0);
        Panel { x, y, s }
    }
    pub fn size(&self) -> (i32, i32) {
        (PANEL_W * self.s, PANEL_H * self.s)
    }
    fn mute_rect(&self) -> (i32, i32, i32, i32) {
        let s = self.s;
        (self.x + MUTE_X * s, self.y + (PANEL_H - MUTE_SIZE) / 2 * s, MUTE_SIZE * s, MUTE_SIZE * s)
    }
    /// Track: x range and the y of its centre.
    fn track(&self) -> (i32, i32, i32) {
        (self.x + TRACK_X0 * self.s, self.x + TRACK_X1 * self.s, self.y + PANEL_H / 2 * self.s)
    }
    /// Level for a pointer x on the slider; snaps to 100% when close, to whole percents otherwise.
    pub fn level_at(&self, px: i32) -> f32 {
        let (x0, x1, _) = self.track();
        let t = ((px - x0) as f32 / (x1 - x0) as f32).clamp(0.0, 1.0);
        let mut v = (t * MAX_LEVEL * 100.0).round() / 100.0;
        if (v - 1.0).abs() <= 0.03 {
            v = 1.0;
        }
        clamp_level(v)
    }
    pub fn hit(&self, px: i32, py: i32) -> Hit {
        let (w, h) = self.size();
        if px < self.x || py < self.y || px >= self.x + w || py >= self.y + h {
            return Hit::Outside;
        }
        let (mx, my, mw, mh) = self.mute_rect();
        if px >= mx && px < mx + mw && py >= my && py < my + mh {
            return Hit::Mute;
        }
        let (x0, x1, yc) = self.track();
        let pad = 10 * self.s;
        if px >= x0 - pad && px <= x1 + pad && (py - yc).abs() <= 16 * self.s {
            return Hit::Slider(self.level_at(px));
        }
        Hit::Inside
    }
}

// ---------------------------------------------------------------- drawing

const GLYPH: [[u8; 5]; 11] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
    [0b101, 0b001, 0b010, 0b100, 0b101], // %
];

struct Canvas<'a> {
    buf: &'a mut [u32],
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let (x0, y0, x1, y1) = (x.max(0), y.max(0), (x + w).min(self.w), (y + h).min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for yy in y0..y1 {
            let row = &mut self.buf[(yy * self.w + x0) as usize..(yy * self.w + x1) as usize];
            row.fill(color);
        }
    }
    /// Rectangle blended over what is there (`alpha` 0..=255 of `color`).
    fn blend(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32, alpha: u32) {
        let (x0, y0, x1, y1) = (x.max(0), y.max(0), (x + w).min(self.w), (y + h).min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for yy in y0..y1 {
            for p in &mut self.buf[(yy * self.w + x0) as usize..(yy * self.w + x1) as usize] {
                let mix = |sh: u32| {
                    let a = (color >> sh) & 0xFF;
                    let b = (*p >> sh) & 0xFF;
                    ((a * alpha + b * (255 - alpha)) / 255) << sh
                };
                *p = mix(16) | mix(8) | mix(0);
            }
        }
    }
    fn text(&mut self, x: i32, y: i32, unit: i32, s: &str, color: u32) {
        let mut cx = x;
        for c in s.chars() {
            let g = match c {
                '0'..='9' => c as usize - '0' as usize,
                '%' => 10,
                _ => {
                    cx += 4 * unit;
                    continue;
                }
            };
            for (ry, row) in GLYPH[g].iter().enumerate() {
                for rx in 0..3 {
                    if row >> (2 - rx) & 1 == 1 {
                        self.rect(cx + rx * unit, y + ry as i32 * unit, unit, unit, color);
                    }
                }
            }
            cx += 4 * unit;
        }
    }
}

const BG: u32 = 0x00_14_16_1B;
const FG: u32 = 0x00_E8_EA_EE;
const DIM: u32 = 0x00_5A_5F_6B;
const ACCENT: u32 = 0x00_4C_8D_FF;
const RED: u32 = 0x00_FF_5A_5A;

/// Draws the volume panel into `buf` (`w`x`h` pixels, 0x00RRGGBB, tightly packed).
pub fn draw_panel(buf: &mut [u32], w: i32, h: i32, p: &Panel, level: f32, muted: bool) {
    if w <= 0 || h <= 0 || buf.len() < (w * h) as usize {
        return;
    }
    let mut c = Canvas { buf, w, h };
    let s = p.s;
    let (pw, ph) = p.size();
    c.blend(p.x, p.y, pw, ph, BG, 235);
    // border
    for (x, y, rw, rh) in [(p.x, p.y, pw, s), (p.x, p.y + ph - s, pw, s), (p.x, p.y, s, ph), (p.x + pw - s, p.y, s, ph)] {
        c.blend(x, y, rw, rh, FG, 60);
    }
    // speaker (mute button)
    let (mx, my, mw, mh) = p.mute_rect();
    c.blend(mx, my, mw, mh, FG, 25);
    let u = s * 2; // icon unit
    let ix = mx + (mw - 11 * u) / 2;
    let iy = my + (mh - 10 * u) / 2;
    let col = if muted { DIM } else { FG };
    c.rect(ix, iy + 3 * u, 3 * u, 4 * u, col); // box
    for i in 0..4 {
        // cone widening to the right
        c.rect(ix + 3 * u + i * u, iy + (3 - i) * u, u, (4 + 2 * i) * u, col);
    }
    if muted {
        for i in 0..6 {
            let (x, y) = (ix + (6 + i) * u, iy + (2 + i + 1) * u);
            c.rect(x, y, u, u, RED);
            c.rect(x, iy + (8 - i) * u, u, u, RED);
        }
    } else {
        let bars = if level > 0.66 { 2 } else if level > 0.0 { 1 } else { 0 };
        for b in 0..bars {
            c.rect(ix + (8 + b * 2) * u, iy + (3 - b) * u, u, (4 + 2 * b) * u, col);
        }
    }
    // slider
    let (x0, x1, yc) = p.track();
    let th = 4 * s;
    c.rect(x0, yc - th / 2, x1 - x0, th, DIM);
    let frac = (clamp_level(level) / MAX_LEVEL).clamp(0.0, 1.0);
    let xk = x0 + ((x1 - x0) as f32 * frac) as i32;
    let fill_col = if muted { DIM } else { ACCENT };
    c.rect(x0, yc - th / 2, xk - x0, th, fill_col);
    let x100 = x0 + ((x1 - x0) as f32 / MAX_LEVEL) as i32;
    c.rect(x100 - s / 2, yc - 9 * s, s.max(1), 5 * s, FG); // tick at 100%
    let kr = 7 * s;
    c.rect(xk - kr / 2 - s, yc - kr - s / 2, kr + 2 * s, 2 * kr + s, FG);
    // text
    let pct = format!("{}%", percent(level));
    c.text(p.x + TEXT_X * s, p.y + (PANEL_H - 15) / 2 * s, 3 * s, &pct, if muted { DIM } else { FG });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_and_mute() {
        let v = VolumeCtl::new(0.8, false);
        assert!((v.gain() - 0.8).abs() < 1e-6);
        v.toggle_mute();
        assert_eq!(v.gain(), 0.0);
        v.toggle_mute();
        v.set_level(9.0);
        assert_eq!(v.level(), MAX_LEVEL);
        v.set_level(f32::NAN);
        assert_eq!(v.level(), DEFAULT_LEVEL);
        v.set_level(-1.0);
        assert_eq!(v.level(), 0.0);
    }

    #[test]
    fn apply_gain_unity_is_untouched_and_boost_is_clamped() {
        let mut a = [0.5f32, -0.5, 0.9, -0.9];
        let mut cur = 1.0;
        apply_gain(&mut a, 2, &mut cur, 1.0);
        assert_eq!(a, [0.5, -0.5, 0.9, -0.9]);
        let mut b = [0.5f32, -0.5, 0.9, -0.9];
        let mut cur = 1.5;
        apply_gain(&mut b, 2, &mut cur, 1.5);
        assert_eq!(b, [0.75, -0.75, 1.0, -1.0]);
    }

    #[test]
    fn apply_gain_ramps_to_target_without_a_jump() {
        let mut a = vec![1.0f32; 480 * 2];
        let mut cur = 1.0;
        apply_gain(&mut a, 2, &mut cur, 0.0);
        assert_eq!(cur, 0.0);
        assert!(a[0] > 0.99, "first sample barely changes: {}", a[0]);
        assert_eq!(a[a.len() - 1], 0.0);
        assert!(a.windows(2).all(|w| w[1] <= w[0] + 1e-6), "monotone fade");
        // both channels of a frame get the same gain
        assert_eq!(a[100], a[101]);
    }

    #[test]
    fn mute_to_unmute_ramps_up() {
        let mut a = vec![1.0f32; 200];
        let mut cur = 0.0;
        apply_gain(&mut a, 1, &mut cur, 1.0);
        assert!(a[0] < 0.05 && a[199] > 0.99);
    }

    #[test]
    fn ini_keeps_other_lines() {
        let t = "preview=0\n";
        let t = ini_set(t, "volume", "70");
        assert_eq!(t, "preview=0\nvolume=70\n");
        let t = ini_set(&t, "volume", "90");
        let t = ini_set(&t, "muted", "1");
        assert_eq!(t, "preview=0\nvolume=90\nmuted=1\n");
        assert_eq!(ini_get(&t, "preview"), Some("0"));
        assert_eq!(ini_get(&t, "nope"), None);
        // duplicated key collapses to one
        assert_eq!(ini_set("volume=1\nvolume=2\n", "volume", "3"), "volume=3\n");
    }

    #[test]
    fn load_defaults_and_values() {
        assert_eq!(load(""), (1.0, false));
        assert_eq!(load("volume=70\nmuted=1\n"), (0.7, true));
        assert_eq!(load("volume=999\n").0, MAX_LEVEL);
        assert_eq!(load("volume=abc\n").0, DEFAULT_LEVEL);
        assert_eq!(load("volume=0\n"), (0.0, false));
    }

    #[test]
    fn panel_stays_inside_window() {
        for (ww, wh, cx, cy, s) in [(1280, 720, 1270, 710, 1), (1280, 720, 0, 0, 1), (200, 50, 100, 20, 1), (3840, 2160, 3800, 2100, 2)] {
            let p = Panel::place(ww, wh, cx, cy, s);
            let (w, h) = p.size();
            assert!(p.x >= 0 && p.y >= 0);
            if ww >= w && wh >= h {
                assert!(p.x + w <= ww && p.y + h <= wh, "{p:?} in {ww}x{wh}");
            }
        }
    }

    #[test]
    fn hit_testing() {
        let p = Panel::place(1280, 720, 100, 100, 1);
        assert_eq!(p.hit(p.x - 1, p.y), Hit::Outside);
        assert_eq!(p.hit(p.x + 300, p.y + 10), Hit::Outside);
        let (mx, my, mw, mh) = p.mute_rect();
        assert_eq!(p.hit(mx + mw / 2, my + mh / 2), Hit::Mute);
        let (x0, x1, yc) = p.track();
        assert_eq!(p.hit(x0, yc), Hit::Slider(0.0));
        assert_eq!(p.hit(x1, yc), Hit::Slider(MAX_LEVEL));
        assert_eq!(p.hit(x0 - 5, yc), Hit::Slider(0.0)); // slightly off the end still counts
        let mid100 = x0 + ((x1 - x0) as f32 / MAX_LEVEL) as i32;
        assert_eq!(p.hit(mid100 + 1, yc), Hit::Slider(1.0)); // snaps to 100%
        assert_eq!(p.hit(p.x + 5, p.y + 5), Hit::Inside);
        // drag outside the panel still maps to a valid level
        assert_eq!(p.level_at(-500), 0.0);
        assert_eq!(p.level_at(5000), MAX_LEVEL);
    }

    #[test]
    fn scaled_panel_hit_testing() {
        let p = Panel::place(3840, 2160, 500, 500, 2);
        let (x0, x1, yc) = p.track();
        assert_eq!(x1 - x0, 320);
        assert!(matches!(p.hit((x0 + x1) / 2, yc), Hit::Slider(v) if (v - 0.75).abs() < 0.02));
    }

    #[test]
    fn draw_marks_the_panel_and_stays_in_bounds() {
        let (w, h) = (400, 200);
        let mut buf = vec![0x00_20_40_60u32; (w * h) as usize];
        let p = Panel::place(w, h, 10, 10, 1);
        draw_panel(&mut buf, w, h, &p, 0.75, false);
        let outside = buf[(h - 1) as usize * w as usize + (w - 1) as usize];
        assert_eq!(outside, 0x00_20_40_60, "far corner untouched");
        let changed = buf.iter().filter(|&&v| v != 0x00_20_40_60).count();
        assert!(changed > 5000, "panel painted: {changed}");
        // the filled part of the slider uses the accent colour, the muted one does not
        assert!(buf.iter().any(|&v| v == ACCENT));
        let mut muted = vec![0x00_20_40_60u32; (w * h) as usize];
        draw_panel(&mut muted, w, h, &p, 0.75, true);
        assert!(!muted.iter().any(|&v| v == ACCENT));
        assert!(muted.iter().any(|&v| v == RED), "muted speaker is crossed out");
    }

    #[test]
    fn draw_survives_tiny_and_edge_cases() {
        for (w, h) in [(1, 1), (50, 20), (300, 72)] {
            let mut buf = vec![0u32; (w * h) as usize];
            let p = Panel::place(w, h, w, h, 1);
            for lvl in [0.0, 1.0, 1.5] {
                draw_panel(&mut buf, w, h, &p, lvl, false);
                draw_panel(&mut buf, w, h, &p, lvl, true);
            }
        }
        draw_panel(&mut [], 10, 10, &Panel { x: 0, y: 0, s: 1 }, 1.0, false); // short buffer: nothing happens
    }

    #[test]
    fn percent_text() {
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(1.5), 150);
        assert_eq!(percent(0.0), 0);
        assert_eq!(percent(0.755), 76);
    }
}
