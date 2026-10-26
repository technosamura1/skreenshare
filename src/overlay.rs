// Text and small panels drawn into the viewer's pixel buffer (hints for remote control, the gamepad test).
// The bar itself (bar.rs) has no text; this uses the Inter font that ships with the program (assets/fonts, OFL).
use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use std::sync::OnceLock;

fn font() -> &'static FontRef<'static> {
    static F: OnceLock<FontRef<'static>> = OnceLock::new();
    F.get_or_init(|| FontRef::try_from_slice(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")).expect("font"))
}

pub struct Buf<'a> {
    pub px: &'a mut [u32],
    pub w: i32,
    pub h: i32,
}

impl Buf<'_> {
    pub fn blend(&mut self, x: i32, y: i32, color: u32, a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let p = &mut self.px[(y * self.w + x) as usize];
        let mix = |sh: u32| {
            let c = ((color >> sh) & 0xFF) as f32;
            let b = ((*p >> sh) & 0xFF) as f32;
            (((c * a + b * (1.0 - a)) + 0.5) as u32).min(255) << sh
        };
        *p = mix(16) | mix(8) | mix(0);
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32, a: f32) {
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                self.blend(xx, yy, color, a);
            }
        }
    }

    pub fn round(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32, a: f32) {
        let r = r.min(w / 2).min(h / 2).max(0);
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let dx = if xx < x + r { x + r - xx } else if xx >= x + w - r { xx - (x + w - r - 1) } else { 0 };
                let dy = if yy < y + r { y + r - yy } else if yy >= y + h - r { yy - (y + h - r - 1) } else { 0 };
                let cov = if dx > 0 && dy > 0 {
                    (r as f32 - ((dx * dx + dy * dy) as f32).sqrt() + 0.5).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                self.blend(xx, yy, color, a * cov);
            }
        }
    }

    pub fn disc(&mut self, cx: i32, cy: i32, r: f32, color: u32, a: f32) {
        let ri = r.ceil() as i32 + 1;
        for y in cy - ri..=cy + ri {
            for x in cx - ri..=cx + ri {
                let d = (((x - cx) * (x - cx) + (y - cy) * (y - cy)) as f32).sqrt();
                self.blend(x, y, color, a * (r - d + 0.5).clamp(0.0, 1.0));
            }
        }
    }

    pub fn ring(&mut self, cx: i32, cy: i32, r: f32, thick: f32, color: u32, a: f32) {
        let ri = (r + thick).ceil() as i32 + 1;
        for y in cy - ri..=cy + ri {
            for x in cx - ri..=cx + ri {
                let d = (((x - cx) * (x - cx) + (y - cy) * (y - cy)) as f32).sqrt();
                self.blend(x, y, color, a * (thick / 2.0 - (d - r).abs() + 0.5).clamp(0.0, 1.0));
            }
        }
    }

    /// Text with its top at `y`; returns the width drawn.
    pub fn text(&mut self, x: i32, y: i32, px: f32, text: &str, color: u32, a: f32) -> i32 {
        let f = font();
        let sc = f.as_scaled(PxScale::from(px));
        let mut cx = x as f32;
        let mut prev = None;
        for ch in text.chars() {
            let id = f.glyph_id(ch);
            if let Some(p) = prev {
                cx += sc.kern(p, id);
            }
            let g = id.with_scale_and_position(PxScale::from(px), point(cx, y as f32 + sc.ascent()));
            cx += sc.h_advance(id);
            prev = Some(id);
            if let Some(o) = f.outline_glyph(g) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| self.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, color, cov * a));
            }
        }
        (cx - x as f32).round() as i32
    }
}

pub fn text_width(px: f32, text: &str) -> i32 {
    let f = font();
    let sc = f.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let id = f.glyph_id(ch);
        if let Some(p) = prev {
            w += sc.kern(p, id);
        }
        w += sc.h_advance(id);
        prev = Some(id);
    }
    w.round() as i32
}

const BG: u32 = 0x00_0D_11_18;
const FG: u32 = 0x00_E8_EA_EE;
const DIM: u32 = 0x00_9A_A0_AD;
pub const ACCENT: u32 = 0x00_2F_86_F6;
const GREEN: u32 = 0x00_3E_CF_8E;

/// A rounded dark pill with one line of text, centred horizontally at `cx`, top at `y`.
pub fn hint(b: &mut Buf, cx: i32, y: i32, s: i32, text: &str, dot: Option<u32>, alpha: f32) {
    let px = 14.0 * s as f32;
    let tw = text_width(px, text);
    let pad = 16 * s;
    let dotw = if dot.is_some() { 18 * s } else { 0 };
    let w = (tw + 2 * pad + dotw).min(b.w - 8 * s);
    let h = 36 * s;
    let x = (cx - w / 2).max(4 * s);
    b.round(x, y, w, h, 18 * s, BG, 0.86 * alpha);
    let mut tx = x + pad;
    if let Some(c) = dot {
        b.disc(tx + 4 * s, y + h / 2, 4.5 * s as f32, c, alpha);
        tx += dotw;
    }
    b.text(tx, y + (h - (px * 1.2) as i32) / 2, px, text, FG, alpha);
}

/// The label inside the bar's "Управление" button (right of the icon and count).
pub fn bar_label(b: &mut Buf, rect: (i32, i32, i32, i32), s: i32, text: &str, color: u32) {
    let px = 13.0 * s as f32;
    let x = rect.0 + 10 * s + 30 * s + 14 * s + 6 * s;
    b.text(x, rect.1 + (rect.3 - (px * 1.2) as i32) / 2, px, text, color, 1.0);
}

/// Live state of the viewer's controllers, for the F2 panel: one row per controller.
pub struct PadRow {
    /// Virtual pad number on the host (1-based), None when the host does not get this controller.
    pub virt: Option<usize>,
    pub slot: usize,
    pub st: crate::inputproto::PadState,
}

pub fn pad_panel(b: &mut Buf, s: i32, rows: &[PadRow], control_note: &str, alpha: f32) {
    use crate::inputproto::btn;
    let w = 560 * s;
    let row_h = 118 * s;
    let h = (64 * s + row_h * rows.len().max(1) as i32 + 30 * s).min(b.h - 16 * s);
    let x = (b.w - w) / 2;
    let y = ((b.h - h) / 2).max(8 * s);
    b.round(x, y, w, h, 18 * s, BG, 0.92 * alpha);
    b.text(x + 20 * s, y + 14 * s, 17.0 * s as f32, "Тест геймпада", FG, alpha);
    let nw = text_width(12.0 * s as f32, "F2 — закрыть");
    b.text(x + w - 20 * s - nw, y + 18 * s, 12.0 * s as f32, "F2 — закрыть", DIM, alpha);
    if rows.is_empty() {
        b.text(x + 20 * s, y + 64 * s, 14.0 * s as f32, "Геймпад не найден. Подключите его и нажмите любую кнопку.", DIM, alpha);
        return;
    }
    for (i, r) in rows.iter().enumerate() {
        let ry = y + 52 * s + row_h * i as i32;
        let title = match r.virt {
            Some(v) => format!("Ваш геймпад {} → виртуальный геймпад {} у показывающего", r.slot + 1, v),
            None => format!("Ваш геймпад {} (не передаётся)", r.slot + 1),
        };
        b.text(x + 20 * s, ry, 13.0 * s as f32, &title, FG, alpha);
        // sticks
        let stick = |b: &mut Buf, cx: i32, cy: i32, sx: i16, sy: i16, pressed: bool| {
            let rad = 30 * s;
            b.disc(cx, cy, rad as f32, FG, 0.10 * alpha);
            b.ring(cx, cy, rad as f32, 1.5 * s as f32, FG, 0.35 * alpha);
            let dx = (sx as f32 / 32768.0 * rad as f32 * 0.8) as i32;
            let dy = (-(sy as f32) / 32768.0 * rad as f32 * 0.8) as i32;
            b.disc(cx + dx, cy + dy, 8.0 * s as f32, if pressed { GREEN } else { ACCENT }, alpha);
        };
        let cy = ry + 62 * s;
        stick(b, x + 50 * s, cy, r.st.lx, r.st.ly, r.st.buttons & btn::LTHUMB != 0);
        stick(b, x + 150 * s, cy, r.st.rx, r.st.ry, r.st.buttons & btn::RTHUMB != 0);
        // triggers
        for (k, v) in [r.st.lt, r.st.rt].into_iter().enumerate() {
            let tx = x + 205 * s + k as i32 * 22 * s;
            b.round(tx, ry + 26 * s, 14 * s, 70 * s, 4 * s, FG, 0.12 * alpha);
            let fh = (70 * s) * v as i32 / 255;
            b.round(tx, ry + 26 * s + 70 * s - fh, 14 * s, fh.max(0), 4 * s, ACCENT, alpha);
            b.text(tx - s, ry + 100 * s, 11.0 * s as f32, if k == 0 { "LT" } else { "RT" }, DIM, alpha);
        }
        // buttons
        let names: [(u16, &str); 14] = [
            (btn::A, "A"), (btn::B, "B"), (btn::X, "X"), (btn::Y, "Y"), (btn::LB, "LB"), (btn::RB, "RB"), (btn::BACK, "Back"),
            (btn::START, "Start"), (btn::UP, "↑"), (btn::DOWN, "↓"), (btn::LEFT, "←"), (btn::RIGHT, "→"), (btn::LTHUMB, "LS"), (btn::RTHUMB, "RS"),
        ];
        let (bx0, by0) = (x + 265 * s, ry + 26 * s);
        for (k, (mask, name)) in names.iter().enumerate() {
            let (col, row) = ((k % 7) as i32, (k / 7) as i32);
            let (rx, rr) = (bx0 + col * 40 * s, by0 + row * 34 * s);
            let on = r.st.buttons & mask != 0;
            b.round(rx, rr, 36 * s, 28 * s, 8 * s, if on { ACCENT } else { FG }, if on { alpha } else { 0.10 * alpha });
            let px = 12.0 * s as f32;
            let tw = text_width(px, name);
            b.text(rx + (36 * s - tw) / 2, rr + 6 * s, px, name, if on { 0x00_FF_FF_FF } else { DIM }, alpha);
        }
    }
    b.text(x + 20 * s, y + h - 26 * s, 12.0 * s as f32, control_note, DIM, alpha);
}
