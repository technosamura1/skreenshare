// The player bar of the viewer window: a semi-transparent pill at the bottom that shows when the mouse moves and
// fades out ~3 s later. Mute + volume slider, network lamp, full screen, disconnect.
// The window is a plain pixel buffer (softbuffer), so everything is drawn by hand here: geometry, hit-testing and
// drawing are pure (no Windows calls) and unit-tested by tools/lossim.
use crate::volume::{clamp_level, percent, GLYPH, MAX_LEVEL};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Outside,
    /// On the bar but not on a control.
    Inside,
    Mute,
    /// On the slider: the level that spot means.
    Slider(f32),
    Fullscreen,
    Stop,
    /// The "Управление" switch (remote control).
    Control,
}

const PAD: i32 = 12;
const BTN: i32 = 40;
const GAP: i32 = 8;
const TEXT_W: i32 = 48; // "150%" in 3-unit digits
const LAMP_W: i32 = 26;
const SEP: i32 = 12;
const H: i32 = 56;
const BOTTOM_GAP: i32 = 28;
const MAX_W: i32 = 520;
const MIN_TRACK: i32 = 40;
/// Remote-control button: icon + count + label, or icon + count only in a narrow window.
const CTL_W: i32 = 150;
const CTL_W_COMPACT: i32 = 76;

/// Where the bar is and how big (integer UI scale `s`: 1 normal, 2 on 200% DPI).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub s: i32,
    /// Width of the remote-control button (0: no such button, an older host).
    pub ctl_w: i32,
}

impl Bar {
    pub fn new(ww: i32, wh: i32, s: i32) -> Bar {
        Bar::with_control(ww, wh, s, false)
    }

    /// `ctl`: the host understands remote control, so the bar has the "Управление" button too (and is wider).
    pub fn with_control(ww: i32, wh: i32, s: i32, ctl: bool) -> Bar {
        let s = s.clamp(1, 4);
        let full = MAX_W + SEP + CTL_W;
        let (maxw, ctl_w) = if !ctl {
            (MAX_W, 0)
        } else if ww >= (full + 16) * s {
            (full, CTL_W)
        } else {
            (MAX_W + SEP + CTL_W_COMPACT, CTL_W_COMPACT)
        };
        let w = (maxw * s).min(ww - 16 * s).max(1);
        let h = H * s;
        Bar { x: ((ww - w) / 2).max(0), y: (wh - h - BOTTOM_GAP * s).max(0), w, h, s, ctl_w }
    }

    /// The remote-control button (zero-sized when there is none). Sits left of the lamp.
    pub fn ctl_rect(&self) -> (i32, i32, i32, i32) {
        if self.ctl_w == 0 {
            return (0, 0, 0, 0);
        }
        let x = self.lamp_left() - (SEP + self.ctl_w) * self.s;
        (x, self.btn_y(), self.ctl_w * self.s, BTN * self.s)
    }

    fn btn_y(&self) -> i32 {
        self.y + (self.h - BTN * self.s) / 2
    }
    pub fn mute_rect(&self) -> (i32, i32, i32, i32) {
        (self.x + PAD * self.s, self.btn_y(), BTN * self.s, BTN * self.s)
    }
    pub fn stop_rect(&self) -> (i32, i32, i32, i32) {
        (self.x + self.w - (PAD + BTN) * self.s, self.btn_y(), BTN * self.s, BTN * self.s)
    }
    pub fn full_rect(&self) -> (i32, i32, i32, i32) {
        (self.x + self.w - (PAD + BTN + 6 + BTN) * self.s, self.btn_y(), BTN * self.s, BTN * self.s)
    }
    /// Centre of the lamp.
    pub fn lamp_center(&self) -> (i32, i32) {
        let (fx, _, _, _) = self.full_rect();
        (fx - (SEP + LAMP_W / 2 + SEP) * self.s, self.y + self.h / 2)
    }
    fn lamp_left(&self) -> i32 {
        self.lamp_center().0 - LAMP_W / 2 * self.s - SEP * self.s
    }
    /// Percent text position (left edge).
    fn text_x(&self) -> i32 {
        let left = if self.ctl_w > 0 { self.ctl_rect().0 - SEP * self.s } else { self.lamp_left() };
        left - (SEP + TEXT_W) * self.s
    }
    /// Track: x range and the y of its centre.
    pub fn track(&self) -> (i32, i32, i32) {
        let x0 = self.mute_rect().0 + (BTN + GAP) * self.s;
        let x1 = (self.text_x() - GAP * self.s).max(x0 + MIN_TRACK * self.s);
        (x0, x1, self.y + self.h / 2)
    }
    /// Level for a pointer x on the slider; snaps to 100% when close, whole percents otherwise.
    pub fn level_at(&self, px: i32) -> f32 {
        let (x0, x1, _) = self.track();
        let t = ((px - x0) as f32 / (x1 - x0).max(1) as f32).clamp(0.0, 1.0);
        let mut v = (t * MAX_LEVEL * 100.0).round() / 100.0;
        if (v - 1.0).abs() <= 0.03 {
            v = 1.0;
        }
        clamp_level(v)
    }
    pub fn hit(&self, px: i32, py: i32) -> Hit {
        if px < self.x || py < self.y || px >= self.x + self.w || py >= self.y + self.h {
            return Hit::Outside;
        }
        let inside = |r: (i32, i32, i32, i32)| px >= r.0 && px < r.0 + r.2 && py >= r.1 && py < r.1 + r.3;
        if inside(self.mute_rect()) {
            return Hit::Mute;
        }
        if inside(self.stop_rect()) {
            return Hit::Stop;
        }
        if inside(self.full_rect()) {
            return Hit::Fullscreen;
        }
        if self.ctl_w > 0 && inside(self.ctl_rect()) {
            return Hit::Control;
        }
        let (x0, x1, yc) = self.track();
        let pad = 10 * self.s;
        if px >= x0 - pad && px <= x1 + pad && (py - yc).abs() <= 16 * self.s {
            return Hit::Slider(self.level_at(px));
        }
        Hit::Inside
    }
}

/// What the bar shows.
#[derive(Clone, Copy, Debug)]
pub struct State {
    pub level: f32,
    pub muted: bool,
    /// Whether there is a sound output at all (without one the speaker and slider are dimmed and ignored).
    pub has_audio: bool,
    /// Network lamp: 0 good, 1 weak, 2 bad, None = not known yet.
    pub net: Option<u8>,
    pub fullscreen: bool,
    pub hover: Hit,
    /// 0..1, from `guilogic::bar_opacity`.
    pub opacity: f32,
    /// Seconds, for the lamp pulse.
    pub time: f32,
    /// Remote control (None: the host is older and has none).
    pub ctl: Option<Ctl>,
}

/// The remote-control button's state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctl {
    /// The switch is on.
    pub on: bool,
    /// The host's owner allows control right now.
    pub allowed: bool,
    /// Gamepads connected to this PC.
    pub pads: u8,
    /// The mouse is captured.
    pub captured: bool,
    /// What the host's owner allows (only meaningful while `allowed`): the gamepad, the mouse with the keyboard.
    pub pad_ok: bool,
    pub kbm_ok: bool,
}

impl Ctl {
    /// Text on the wide button: says which devices go to the host when only one (or none) is allowed.
    pub fn label(&self) -> &'static str {
        match (self.allowed, self.pad_ok, self.kbm_ok) {
            (true, true, false) => "Геймпад",
            (true, false, true) => "Мышь+клав.",
            (true, false, false) => "Нет доступа",
            _ => "Управление",
        }
    }
    /// A device the host switched off (shown struck through).
    fn pad_off(&self) -> bool {
        self.allowed && !self.pad_ok
    }
    fn kbm_off(&self) -> bool {
        self.allowed && !self.kbm_ok
    }
}

const BG: u32 = 0x00_0D_11_18;
const FG: u32 = 0x00_E8_EA_EE;
const DIM: u32 = 0x00_6A_6F_7B;
const ACCENT: u32 = 0x00_2F_86_F6;
const RED: u32 = 0x00_F0_5A_5A;
const GREEN: u32 = 0x00_3E_CF_8E;
const YELLOW: u32 = 0x00_F0_B4_3C;

struct Cv<'a> {
    buf: &'a mut [u32],
    w: i32,
    h: i32,
    /// Everything is multiplied by this (the fade).
    k: f32,
}

impl Cv<'_> {
    fn px(&mut self, x: i32, y: i32, color: u32, alpha: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let a = (alpha * self.k).clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let p = &mut self.buf[(y * self.w + x) as usize];
        let mix = |sh: u32| {
            let c = ((color >> sh) & 0xFF) as f32;
            let b = ((*p >> sh) & 0xFF) as f32;
            (((c * a + b * (1.0 - a)) + 0.5) as u32).min(255) << sh
        };
        *p = mix(16) | mix(8) | mix(0);
    }
    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32, alpha: f32) {
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                self.px(xx, yy, color, alpha);
            }
        }
    }
    /// Rounded rectangle, edges anti-aliased by coverage.
    fn round(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32, alpha: f32) {
        let r = r.min(w / 2).min(h / 2).max(0);
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let dx = if xx < x + r { x + r - xx } else if xx >= x + w - r { xx - (x + w - r - 1) } else { 0 };
                let dy = if yy < y + r { y + r - yy } else if yy >= y + h - r { yy - (y + h - r - 1) } else { 0 };
                let cov = if dx > 0 && dy > 0 {
                    let d = ((dx * dx + dy * dy) as f32).sqrt();
                    (r as f32 - d + 0.5).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                self.px(xx, yy, color, alpha * cov);
            }
        }
    }
    /// A 1-unit outline just inside the rounded rectangle's edge (anti-aliased), for the glassy border.
    fn outline(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, t: i32, color: u32, alpha: f32) {
        let cover = |xx: i32, yy: i32, x: i32, y: i32, w: i32, h: i32, r: i32| -> f32 {
            if xx < x || yy < y || xx >= x + w || yy >= y + h {
                return 0.0;
            }
            let dx = if xx < x + r { x + r - xx } else if xx >= x + w - r { xx - (x + w - r - 1) } else { 0 };
            let dy = if yy < y + r { y + r - yy } else if yy >= y + h - r { yy - (y + h - r - 1) } else { 0 };
            if dx > 0 && dy > 0 {
                (r as f32 - ((dx * dx + dy * dy) as f32).sqrt() + 0.5).clamp(0.0, 1.0)
            } else {
                1.0
            }
        };
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let out = cover(xx, yy, x, y, w, h, r);
                let inn = cover(xx, yy, x + t, y + t, w - 2 * t, h - 2 * t, (r - t).max(0));
                let c = out * (1.0 - inn);
                if c > 0.0 {
                    self.px(xx, yy, color, alpha * c);
                }
            }
        }
    }
    fn disc(&mut self, cx: i32, cy: i32, r: f32, color: u32, alpha: f32) {
        let ri = r.ceil() as i32 + 1;
        for y in cy - ri..=cy + ri {
            for x in cx - ri..=cx + ri {
                let d = (((x - cx) * (x - cx) + (y - cy) * (y - cy)) as f32).sqrt();
                let cov = (r - d + 0.5).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.px(x, y, color, alpha * cov);
                }
            }
        }
    }
    fn ring(&mut self, cx: i32, cy: i32, r: f32, thick: f32, color: u32, alpha: f32) {
        let ri = (r + thick).ceil() as i32 + 1;
        for y in cy - ri..=cy + ri {
            for x in cx - ri..=cx + ri {
                let d = (((x - cx) * (x - cx) + (y - cy) * (y - cy)) as f32).sqrt();
                let cov = (thick / 2.0 - (d - r).abs() + 0.5).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.px(x, y, color, alpha * cov);
                }
            }
        }
    }
    fn digits(&mut self, x: i32, y: i32, unit: i32, text: &str, color: u32, alpha: f32) {
        let mut cx = x;
        for c in text.chars() {
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
                        self.rect(cx + rx * unit, y + ry as i32 * unit, unit, unit, color, alpha);
                    }
                }
            }
            cx += 4 * unit;
        }
    }
}

/// A small gamepad: body, d-pad, four buttons. `x`,`y`: top-left of a 26x16 (unit `s`) box.
/// A thin diagonal bar from the bottom-left to the top-right of the box: "this one is off".
fn strike(c: &mut Cv, x: i32, y: i32, w: i32, h: i32, s: i32, col: u32) {
    let n = (w.max(h) / s.max(1)).max(2);
    for i in 0..n {
        c.rect(x + i * w / n, y + h - (i + 1) * h / n, (2 * s).max(2), (2 * s).max(2), col, 0.95);
    }
}

fn gamepad_icon(c: &mut Cv, x: i32, y: i32, s: i32, col: u32) {
    c.round(x, y + 2 * s, 26 * s, 12 * s, 6 * s, col, 1.0);
    // grips
    c.round(x + s, y + 8 * s, 7 * s, 8 * s, 3 * s, col, 1.0);
    c.round(x + 18 * s, y + 8 * s, 7 * s, 8 * s, 3 * s, col, 1.0);
    // cut-outs (what is drawn over the body is the bar's dark colour)
    let dark = 0x00_1B_20_2B;
    c.rect(x + 5 * s, y + 6 * s, 6 * s, 2 * s, dark, 0.9);
    c.rect(x + 7 * s, y + 4 * s, 2 * s, 6 * s, dark, 0.9);
    c.disc(x + 18 * s, y + 5 * s, 1.3 * s as f32, dark, 0.9);
    c.disc(x + 21 * s, y + 8 * s, 1.3 * s as f32, dark, 0.9);
    c.disc(x + 15 * s, y + 8 * s, 1.3 * s as f32, dark, 0.9);
    c.disc(x + 18 * s, y + 11 * s, 1.3 * s as f32, dark, 0.9);
}

/// Draws the bar into `buf` (`w`x`h` pixels, 0x00RRGGBB, tightly packed). Does nothing when fully faded.
pub fn draw(buf: &mut [u32], w: i32, h: i32, b: &Bar, st: &State) {
    if w <= 0 || h <= 0 || buf.len() < (w * h) as usize || st.opacity <= 0.0 {
        return;
    }
    let mut c = Cv { buf, w, h, k: st.opacity.clamp(0.0, 1.0) };
    let s = b.s;
    c.round(b.x, b.y, b.w, b.h, 18 * s, BG, 0.88);
    // glassy 1px border, brighter on the top edge
    c.outline(b.x, b.y, b.w, b.h, 18 * s, s.max(1), 0x00_8F_A6_C8, 0.22);
    c.rect(b.x + 18 * s, b.y + s.max(1), b.w - 36 * s, s.max(1), FG, 0.06);

    let hov = |c: &mut Cv, r: (i32, i32, i32, i32), on: bool| {
        if on {
            c.round(r.0, r.1, r.2, r.3, 12 * s, FG, 0.14);
        }
    };

    // mute + speaker
    let mr = b.mute_rect();
    hov(&mut c, mr, st.hover == Hit::Mute && st.has_audio);
    let u = s * 2;
    let ix = mr.0 + (mr.2 - 11 * u) / 2;
    let iy = mr.1 + (mr.3 - 10 * u) / 2;
    let dead = st.muted || !st.has_audio;
    let col = if dead { DIM } else { FG };
    c.rect(ix, iy + 3 * u, 3 * u, 4 * u, col, 1.0);
    for i in 0..4 {
        c.rect(ix + 3 * u + i * u, iy + (3 - i) * u, u, (4 + 2 * i) * u, col, 1.0);
    }
    if st.muted && st.has_audio {
        for i in 0..6 {
            c.rect(ix + (6 + i) * u, iy + (3 + i) * u, u, u, RED, 1.0);
            c.rect(ix + (6 + i) * u, iy + (8 - i) * u, u, u, RED, 1.0);
        }
    } else if st.has_audio {
        let bars = if st.level > 0.66 { 2 } else if st.level > 0.0 { 1 } else { 0 };
        for k in 0..bars {
            c.rect(ix + (8 + k * 2) * u, iy + (3 - k) * u, u, (4 + 2 * k) * u, col, 1.0);
        }
    }

    // slider
    let (x0, x1, yc) = b.track();
    let th = 4 * s;
    if !st.has_audio {
        // No sound output (the self-test, or no device): the speaker is struck through and the slider is just a flat
        // faint line without a knob or a percent, so it does not look like something that can be dragged.
        for i in 0..8 {
            c.rect(ix + (1 + i) * u, iy + (1 + i) * u, u, u, DIM, 1.0);
        }
        c.round(x0, yc - th / 2, x1 - x0, th, th / 2, DIM, 0.25);
        c.rect(b.text_x(), b.y + b.h / 2 - s, 10 * s, 2 * s, DIM, 1.0);
    } else {
        c.round(x0, yc - th / 2, x1 - x0, th, th / 2, DIM, 0.7);
        let frac = (clamp_level(st.level) / MAX_LEVEL).clamp(0.0, 1.0);
        let xk = x0 + ((x1 - x0) as f32 * frac) as i32;
        let fill = if dead { DIM } else { ACCENT };
        c.round(x0, yc - th / 2, (xk - x0).max(0), th, th / 2, fill, 1.0);
        let x100 = x0 + ((x1 - x0) as f32 / MAX_LEVEL) as i32;
        c.rect(x100, yc - 9 * s, s.max(1), 5 * s, FG, 0.55);
        let active = matches!(st.hover, Hit::Slider(_));
        c.disc(xk, yc, if active { 8.5 } else { 7.0 } * s as f32, FG, 1.0);
        c.disc(xk, yc, 3.0 * s as f32, fill, 1.0);
        let pct = format!("{}%", percent(st.level));
        c.digits(b.text_x(), b.y + (b.h - 15 * s) / 2, 3 * s, &pct, if dead { DIM } else { FG }, 1.0);
    }

    // remote control: [gamepad icon + count] and, in a wide bar, the label (the label itself is text, drawn by the
    // window code with the font; here only the shapes)
    if let Some(ct) = st.ctl {
        let r = b.ctl_rect();
        let live = ct.on && ct.allowed;
        let waiting = ct.on && !ct.allowed;
        if ct.on {
            c.round(r.0, r.1, r.2, r.3, 12 * s, if live { ACCENT } else { YELLOW }, if live { 0.85 } else { 0.30 });
        } else if st.hover == Hit::Control {
            c.round(r.0, r.1, r.2, r.3, 12 * s, FG, 0.14);
        } else {
            c.outline(r.0, r.1, r.2, r.3, 12 * s, s.max(1), FG, 0.20);
        }
        let col = if ct.on { 0x00_FF_FF_FF } else if ct.pads > 0 { FG } else { DIM };
        let pcol = if ct.pad_off() { DIM } else { col };
        let (px, py) = (r.0 + 10 * s, r.1 + (r.3 - 16 * s) / 2);
        gamepad_icon(&mut c, px, py, s, pcol);
        let count = format!("{}", ct.pads.min(9));
        c.digits(px + 30 * s, r.1 + (r.3 - 10 * s) / 2, 2 * s, &count, pcol, 1.0);
        if ct.pad_off() {
            strike(&mut c, px - 2 * s, py, 28 * s, 16 * s, s, RED);
        }
        let compact = b.ctl_w <= CTL_W_COMPACT;
        if compact {
            // No room for the label: a small mouse says whether mouse and keyboard are allowed (green = captured).
            let (mx, my) = (r.0 + r.2 - 22 * s, r.1 + (r.3 - 16 * s) / 2);
            let mcol = if ct.kbm_off() { DIM } else if ct.captured { GREEN } else { col };
            c.outline(mx, my, 10 * s, 16 * s, 5 * s, s.max(1), mcol, 1.0);
            c.rect(mx + 5 * s - s / 2, my + s, s.max(1), 6 * s, mcol, 1.0);
            if ct.kbm_off() {
                strike(&mut c, mx - 2 * s, my, 14 * s, 16 * s, s, RED);
            }
        } else if ct.captured {
            c.disc(r.0 + r.2 - 10 * s, r.1 + r.3 - 10 * s, 4.0 * s as f32, GREEN, 1.0); // the mouse is captured
        }
        if waiting {
            let q = 5.0 * s as f32;
            c.disc(r.0 + r.2 - 10 * s, r.1 + 10 * s, q, YELLOW, 0.5 + 0.5 * (st.time * 4.0).sin().abs());
        }
    }

    // network lamp: a dot in the level's colour with a soft pulsing ring
    let (lx, ly) = b.lamp_center();
    let lamp = match st.net {
        Some(0) => GREEN,
        Some(1) => YELLOW,
        Some(_) => RED,
        None => DIM,
    };
    let (period, strength) = match st.net {
        Some(0) => (3.0, 0.35),
        Some(1) => (1.6, 0.5),
        Some(_) => (0.9, 0.7),
        None => (3.0, 0.0),
    };
    let ph = (st.time / period).fract();
    c.ring(lx, ly, (7.0 + 7.0 * ph) * s as f32, 2.0 * s as f32, lamp, strength * (1.0 - ph));
    c.disc(lx, ly, 7.0 * s as f32, lamp, 1.0);
    // separators
    for sx in [b.lamp_left(), b.lamp_center().0 + (LAMP_W / 2 + SEP) * s - s] {
        c.rect(sx, b.y + 14 * s, s.max(1), b.h - 28 * s, FG, 0.14);
    }

    // full screen: four corner brackets (arms toward the centre = "enter", away from it = "leave")
    let fr = b.full_rect();
    hov(&mut c, fr, st.hover == Hit::Fullscreen);
    let (cx, cy) = (fr.0 + fr.2 / 2, fr.1 + fr.3 / 2);
    let (a, t, l) = (8 * s, 2 * s, 6 * s);
    for (sx, sy) in [(-1, -1), (1, -1), (-1, 1), (1, 1)] {
        let (corner, dir) = if st.fullscreen { (a - 3 * s, 1) } else { (a, -1) };
        let (px, py) = (cx + sx * corner, cy + sy * corner);
        let (dx, dy) = (dir * sx, dir * sy);
        c.rect(if dx > 0 { px - t / 2 } else { px - l + t / 2 }, py - t / 2, l, t, FG, 1.0);
        c.rect(px - t / 2, if dy > 0 { py - t / 2 } else { py - l + t / 2 }, t, l, FG, 1.0);
    }

    // stop / disconnect
    let sr = b.stop_rect();
    let hv = st.hover == Hit::Stop;
    c.round(sr.0, sr.1, sr.2, sr.3, 12 * s, RED, if hv { 0.95 } else { 0.75 });
    let q = 14 * s;
    c.round(sr.0 + (sr.2 - q) / 2, sr.1 + (sr.3 - q) / 2, q, q, 3 * s, 0x00_FF_FF_FF, 1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> State {
        State { level: 0.75, muted: false, has_audio: true, net: Some(0), fullscreen: false, hover: Hit::Outside, opacity: 1.0, time: 0.4, ctl: None }
    }

    #[test]
    fn bar_fits_in_any_reasonable_window() {
        for (ww, wh, s) in [(1280, 720, 1), (1920, 1080, 1), (3840, 2160, 2), (640, 360, 1), (420, 240, 1)] {
            let b = Bar::new(ww, wh, s);
            assert!(b.x >= 0 && b.y >= 0, "{b:?}");
            assert!(b.x + b.w <= ww && b.y + b.h <= wh, "{b:?} in {ww}x{wh}");
            let (x0, x1, _) = b.track();
            assert!(x1 > x0, "track has width in {ww}");
        }
    }

    #[test]
    fn controls_do_not_overlap_in_normal_windows() {
        for ww in [800, 1280, 1920] {
            let b = Bar::new(ww, 720, 1);
            let rs = [b.mute_rect(), b.full_rect(), b.stop_rect()];
            for (i, a) in rs.iter().enumerate() {
                for c in &rs[i + 1..] {
                    assert!(a.0 + a.2 <= c.0 || c.0 + c.2 <= a.0, "{a:?} vs {c:?}");
                }
            }
            let (_, tx1, _) = b.track();
            assert!(tx1 <= b.text_x(), "track ends before the percent text");
            assert!(b.text_x() + TEXT_W <= b.lamp_left(), "text before the lamp");
            assert!(b.lamp_center().0 < b.full_rect().0);
            assert!(b.full_rect().0 + b.full_rect().2 <= b.stop_rect().0);
        }
    }

    #[test]
    fn control_button_fits_and_is_hit() {
        for (ww, wh, sc) in [(1280, 720, 1), (1920, 1080, 1), (3840, 2160, 2), (900, 500, 1), (700, 400, 1)] {
            let b = Bar::with_control(ww, wh, sc, true);
            assert!(b.x >= 0 && b.x + b.w <= ww, "{b:?}");
            let c = b.ctl_rect();
            assert!(c.2 > 0);
            let (tx0, tx1, _) = b.track();
            assert!(tx1 <= b.text_x() || ww < 760, "track ends before the text in {ww}");
            assert!(c.0 + c.2 <= b.lamp_left(), "button before the lamp");
            assert!(b.text_x() + TEXT_W <= c.0, "percent text before the button in {ww}");
            assert!(tx0 < tx1);
            assert_eq!(b.hit(c.0 + c.2 / 2, c.1 + c.3 / 2), Hit::Control);
            let m = b.mute_rect();
            assert_eq!(b.hit(m.0 + 2, m.1 + 2), Hit::Mute);
        }
        // without the button nothing changes
        let a = Bar::new(1280, 720, 1);
        assert_eq!(a.ctl_w, 0);
        assert_eq!(a.w, MAX_W);
    }

    #[test]
    fn control_button_says_which_devices_are_allowed() {
        let c = |allowed, pad_ok, kbm_ok| Ctl { on: true, allowed, pads: 1, captured: false, pad_ok, kbm_ok };
        assert_eq!(c(true, true, true).label(), "Управление");
        assert_eq!(c(true, true, false).label(), "Геймпад");
        assert_eq!(c(true, false, true).label(), "Мышь+клав.");
        assert_eq!(c(true, false, false).label(), "Нет доступа");
        // not allowed at all (the host says nothing about devices yet): the plain label
        assert_eq!(c(false, false, false).label(), "Управление");
    }

    #[test]
    fn a_device_the_host_switched_off_is_drawn_differently() {
        for ww in [900, 640] {
            let (w, h) = (ww, 300);
            let b = Bar::with_control(w, h, 1, true);
            let base = vec![0x00_20_20_20u32; (w * h) as usize];
            let render = |pad_ok, kbm_ok| {
                let mut buf = base.clone();
                let ctl = Ctl { on: true, allowed: true, pads: 1, captured: false, pad_ok, kbm_ok };
                draw(&mut buf, w, h, &b, &State { ctl: Some(ctl), ..st() });
                buf
            };
            let both = render(true, true);
            let no_pad = render(false, true);
            let no_kbm = render(true, false);
            assert_ne!(both, no_pad, "pad off shows (width {ww})");
            if b.ctl_w <= CTL_W_COMPACT {
                assert_ne!(both, no_kbm, "the compact bar has a mouse glyph that changes");
            }
            assert_ne!(no_pad, no_kbm);
        }
    }

    #[test]
    fn control_button_draws_in_every_state() {
        let (w, h) = (900, 300);
        let b = Bar::with_control(w, h, 1, true);
        let base = vec![0x00_20_20_20u32; (w * h) as usize];
        for (on, allowed, pads, captured) in [(false, false, 0, false), (false, true, 2, false), (true, false, 1, false), (true, true, 3, true)] {
            let mut buf = base.clone();
            draw(&mut buf, w, h, &b, &State { ctl: Some(Ctl { on, allowed, pads, captured, pad_ok: true, kbm_ok: true }), ..st() });
            let r = b.ctl_rect();
            let mut changed = false;
            for y in r.1..r.1 + r.3 {
                for x in r.0..r.0 + r.2 {
                    changed |= buf[(y * w + x) as usize] != base[(y * w + x) as usize];
                }
            }
            assert!(changed, "button drawn for {on} {allowed} {pads}");
        }
    }

    #[test]
    fn hit_testing() {
        let b = Bar::new(1280, 720, 1);
        assert_eq!(b.hit(b.x - 1, b.y + 5), Hit::Outside);
        assert_eq!(b.hit(b.x + 2, b.y + 2), Hit::Inside);
        let m = b.mute_rect();
        assert_eq!(b.hit(m.0 + m.2 / 2, m.1 + m.3 / 2), Hit::Mute);
        let f = b.full_rect();
        assert_eq!(b.hit(f.0 + f.2 / 2, f.1 + f.3 / 2), Hit::Fullscreen);
        let s = b.stop_rect();
        assert_eq!(b.hit(s.0 + s.2 / 2, s.1 + s.3 / 2), Hit::Stop);
        let (x0, x1, yc) = b.track();
        assert_eq!(b.hit(x0, yc), Hit::Slider(0.0));
        assert_eq!(b.hit(x1, yc), Hit::Slider(MAX_LEVEL));
        let mid100 = x0 + ((x1 - x0) as f32 / MAX_LEVEL) as i32;
        assert_eq!(b.hit(mid100 + 1, yc), Hit::Slider(1.0));
        assert_eq!(b.level_at(-500), 0.0);
        assert_eq!(b.level_at(99_999), MAX_LEVEL);
    }

    /// Where the knob is drawn (x of its centre) for a level, the way `draw` places it.
    fn knob_x(b: &Bar, level: f32) -> i32 {
        let (x0, x1, _) = b.track();
        x0 + ((x1 - x0) as f32 * (clamp_level(level) / MAX_LEVEL)) as i32
    }

    #[test]
    fn slider_press_and_drag_follow_the_pointer() {
        for (ww, sc, ctl) in [(1280, 1, false), (1280, 1, true), (800, 1, true), (3840, 2, true)] {
            let b = Bar::with_control(ww, 720 * sc, sc, ctl);
            let (x0, x1, yc) = b.track();
            // a press anywhere along the track (and a little margin at the ends) is a slider hit
            let mut last = -1.0f32;
            for px in (x0..=x1).step_by(3) {
                let Hit::Slider(v) = b.hit(px, yc) else { panic!("no slider at {px} (ww {ww}, ctl {ctl})") };
                assert!(v >= last, "the level never goes back while moving right");
                assert!((0.0..=MAX_LEVEL).contains(&v));
                last = v;
            }
            assert!(matches!(b.hit(x0 - 1, yc), Hit::Slider(_)), "margin left of the track");
            assert!(matches!(b.hit(x1 + 10 * sc, yc), Hit::Slider(_)), "margin right of the track");
            // a drag leaves the bar sideways and up/down: the level keeps following x only, clamped at the ends
            assert_eq!(b.level_at(-1000), 0.0);
            assert_eq!(b.level_at(ww * sc + 1000), MAX_LEVEL);
            assert!(b.level_at((x0 + x1) / 2) > 0.7 && b.level_at((x0 + x1) / 2) < 0.8);
            // the knob is drawn where a press on it would put it back
            for lv in [0.0, 0.25, 0.5, 1.0, 1.25, 1.5] {
                let back = b.level_at(knob_x(&b, lv));
                assert!((back - lv).abs() < 0.03 + 2.0 / (x1 - x0) as f32 * MAX_LEVEL, "{lv} -> {back}");
            }
            // the track is never covered by another control
            for px in x0..=x1 {
                assert!(!matches!(b.hit(px, yc), Hit::Mute | Hit::Stop | Hit::Fullscreen | Hit::Control), "{px}");
            }
        }
    }

    #[test]
    fn slider_knob_is_drawn_with_sound_and_absent_without() {
        let (w, h) = (1280, 720);
        let b = Bar::new(w, h, 1);
        let (_, _, yc) = b.track();
        let at = |buf: &Vec<u32>, x: i32| buf[(yc * w + x) as usize];
        for lv in [0.3f32, 1.0, 1.4] {
            let mut buf = vec![0x00_20_20_20u32; (w * h) as usize];
            draw(&mut buf, w, h, &b, &State { level: lv, ..st() });
            assert_eq!(at(&buf, knob_x(&b, lv)), ACCENT, "knob centre at {lv}");
        }
        // no sound output: no knob and no accent fill anywhere, the percent is replaced by a dash
        let mut buf = vec![0x00_20_20_20u32; (w * h) as usize];
        draw(&mut buf, w, h, &b, &State { has_audio: false, ..st() });
        assert!(!buf.contains(&ACCENT), "no accent colour without sound");
        assert_ne!(at(&buf, knob_x(&b, 1.0)), FG, "no knob");
    }

    #[test]
    fn scaled_bar_geometry() {
        let b = Bar::new(3840, 2160, 2);
        assert_eq!(b.w, 1040);
        assert_eq!(b.h, 112);
        assert!(matches!(b.hit((b.track().0 + b.track().1) / 2, b.track().2), Hit::Slider(_)));
    }

    #[test]
    fn draw_paints_inside_the_bar_only_and_fades() {
        let (w, h) = (1280, 720);
        let base = 0x00_20_40_60u32;
        let mut buf = vec![base; (w * h) as usize];
        let b = Bar::new(w, h, 1);
        draw(&mut buf, w, h, &b, &st());
        assert_eq!(buf[0], base);
        assert_eq!(buf[(h - 1) as usize * w as usize + (w - 1) as usize], base);
        for y in 0..h {
            for x in 0..w {
                if buf[(y * w + x) as usize] != base {
                    assert!(x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h, "pixel outside the bar at {x},{y}");
                }
            }
        }
        let painted = buf.iter().filter(|&&v| v != base).count();
        assert!(painted > 8000, "painted {painted}");
        assert!(buf.contains(&GREEN), "green lamp");
        // fully faded: untouched
        let mut gone = vec![base; (w * h) as usize];
        draw(&mut gone, w, h, &b, &State { opacity: 0.0, ..st() });
        assert!(gone.iter().all(|&v| v == base));
        // half faded: the bar is lighter-touch than the full one
        let mut half = vec![base; (w * h) as usize];
        draw(&mut half, w, h, &b, &State { opacity: 0.5, ..st() });
        let i = ((b.y + 3) * w + b.x + w / 4) as usize;
        let d = |p: u32| (((p >> 16) & 0xff) as i32 - 0x20).abs();
        assert!(d(half[i]) < d(buf[i]) + 1);
    }

    #[test]
    fn lamp_colours_and_muted_speaker() {
        let (w, h) = (1280, 720);
        let b = Bar::new(w, h, 1);
        for (net, col) in [(Some(0u8), GREEN), (Some(1), YELLOW), (Some(2), RED)] {
            let mut buf = vec![0u32; (w * h) as usize];
            draw(&mut buf, w, h, &b, &State { net, ..st() });
            let (lx, ly) = b.lamp_center();
            assert_eq!(buf[(ly * w + lx) as usize], col);
        }
        let mut buf = vec![0u32; (w * h) as usize];
        draw(&mut buf, w, h, &b, &State { muted: true, ..st() });
        assert!(buf.contains(&RED));
        assert!(!buf.contains(&ACCENT), "muted slider is grey");
    }

    #[test]
    fn draw_survives_tiny_buffers_and_both_fullscreen_states() {
        for (w, h) in [(1, 1), (50, 20), (300, 100)] {
            let mut buf = vec![0u32; (w * h) as usize];
            let b = Bar::new(w, h, 1);
            for fs in [false, true] {
                for hover in [Hit::Outside, Hit::Mute, Hit::Fullscreen, Hit::Stop, Hit::Slider(1.0)] {
                    draw(&mut buf, w, h, &b, &State { fullscreen: fs, hover, ..st() });
                }
            }
        }
        draw(&mut [], 10, 10, &Bar::new(10, 10, 1), &st());
    }

    /// `BAR_DUMP=/path/prefix cargo test dump_preview` writes PPM pictures of the bar for a visual check.
    #[test]
    fn dump_preview() {
        let Ok(prefix) = std::env::var("BAR_DUMP") else { return };
        for (name, state, s, ww) in [
            ("a", st(), 1, 1000),
            ("b", State { muted: true, net: Some(2), fullscreen: true, hover: Hit::Stop, ..st() }, 1, 1000),
            ("d", State { has_audio: false, ..st() }, 1, 1000),
            ("c", State { net: Some(1), hover: Hit::Fullscreen, level: 1.3, ..st() }, 2, 1400),
        ] {
            let (w, h) = (ww, 160 * s);
            let mut buf: Vec<u32> = (0..w * h).map(|i| { let x = (i % w) as u32 * 255 / w as u32; 0x00_30_50_00 | (x << 16) | 0x40 }).collect();
            draw(&mut buf, w, h, &Bar::new(w, h, s), &state);
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            for p in buf { out.extend([(p >> 16) as u8, (p >> 8) as u8, p as u8]); }
            std::fs::write(format!("{prefix}{name}.ppm"), out).unwrap();
        }
    }
}
