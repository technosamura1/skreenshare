// Launcher window (egui). Streaming itself runs in child processes (`host --ipc` / `client --ipc`),
// which keeps the fast viewer path untouched and lets the GUI stop them cleanly.
use crate::audiosel;
use crate::capture;
use crate::netq;
use crate::easy;
use crate::guilogic::{self as lg, Step, STEPS};
use crate::preview;
use crate::source::{self, Choice, WinEntry};
use crate::thumbs::{Key, Target, Thumbs};
use crate::wincap;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Sense, Stroke, Vec2,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- look: palette (dark / light x accent), animations

/// Every colour the launcher paints with. One is active per thread (the UI thread) and `apply_theme` swaps it,
/// so the whole window changes at once when the theme or accent is switched.
#[derive(Clone, Copy)]
struct Palette {
    light: bool,
    /// Glass (Mica) is on: the big surfaces are translucent so the backdrop shines through.
    #[allow(dead_code)]
    glass: bool,
    anim: bool,
    accent: Color32,
    bg: Color32,
    card: Color32,
    card_hover: Color32,
    field: Color32,
    text: Color32,
    muted: Color32,
    green: Color32,
    yellow: Color32,
    red: Color32,
    /// Neutral big button ("Смотреть").
    tile2: Color32,
    /// Fill of a standard widget under the pointer.
    widget_hover: Color32,
}

const fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

impl Palette {
    fn make(look: lg::Look, glass: bool) -> Palette {
        use lg::Accent;
        let accent = match (look.light, look.accent) {
            (false, Accent::Blue) => rgb(0x4f7cff),
            (false, Accent::Purple) => rgb(0x8b6bff),
            (false, Accent::Green) => rgb(0x1fa36a),
            (true, Accent::Blue) => rgb(0x2f5fe6),
            (true, Accent::Purple) => rgb(0x6a45e0),
            (true, Accent::Green) => rgb(0x14875a),
        };
        let mut p = if look.light {
            Palette {
                light: true,
                glass,
                anim: look.anim,
                accent,
                bg: rgb(0xf2f3f7),
                card: rgb(0xffffff),
                card_hover: rgb(0xe6e9f1),
                field: rgb(0xe9ebf2),
                text: rgb(0x1b1d24),
                muted: rgb(0x5b6172),
                green: rgb(0x148a55),
                yellow: rgb(0x9a6400),
                red: rgb(0xc42b2b),
                tile2: rgb(0xdfe3ee),
                widget_hover: rgb(0xd8dce8),
            }
        } else {
            Palette {
                light: false,
                glass,
                anim: look.anim,
                accent,
                bg: rgb(0x121318),
                card: rgb(0x1d1f27),
                card_hover: rgb(0x252833),
                field: rgb(0x0d0e12),
                text: rgb(0xe8eaf0),
                muted: rgb(0x8b90a0),
                green: rgb(0x3ecf8e),
                yellow: rgb(0xf0b43c),
                red: rgb(0xef5b5b),
                tile2: rgb(0x2b2f3f),
                widget_hover: rgb(0x2e3240),
            }
        };
        if glass {
            let a = |c: Color32, al: u8| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), al);
            p.bg = Color32::TRANSPARENT;
            p.card = a(p.card, 178);
            p.card_hover = a(p.card_hover, 205);
            p.field = a(p.field, 200);
            p.tile2 = a(p.tile2, 205);
        }
        p
    }
}

thread_local! {
    static PAL: std::cell::Cell<Palette> = std::cell::Cell::new(Palette::make(lg::Look::default(), false));
}

fn pal() -> Palette {
    PAL.with(|p| p.get())
}
fn c_bg() -> Color32 {
    pal().bg
}
fn c_card() -> Color32 {
    pal().card
}
fn c_card_hover() -> Color32 {
    pal().card_hover
}
fn c_field() -> Color32 {
    pal().field
}
fn c_text() -> Color32 {
    pal().text
}
fn c_muted() -> Color32 {
    pal().muted
}
fn c_accent() -> Color32 {
    pal().accent
}
fn c_green() -> Color32 {
    pal().green
}
fn c_yellow() -> Color32 {
    pal().yellow
}
fn c_red() -> Color32 {
    pal().red
}
fn c_tile2() -> Color32 {
    pal().tile2
}

/// Animation length in seconds, or 0 when animations are switched off.
fn anim_secs(s: f32) -> f32 {
    if pal().anim {
        s
    } else {
        0.0
    }
}

fn luminance(c: Color32) -> f32 {
    let f = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

/// Readable text colour on a button fill: white when it has enough contrast, near-black otherwise.
/// (Translucent glass fills are judged by their colour; they sit on a dark or light backdrop either way.)
fn on_fill(fill: Color32) -> Color32 {
    if (1.05) / (luminance(fill) + 0.05) >= 3.0 {
        Color32::WHITE
    } else {
        rgb(0x10131a)
    }
}

/// A fill nudged towards white (dark fills) or black (light fills) by `t` (0..1): the hover highlight.
fn hot(fill: Color32, t: f32) -> Color32 {
    let to = if luminance(fill) > 0.45 { Color32::BLACK } else { Color32::WHITE };
    fill.lerp_to_gamma(to, 0.11 * t)
}

/// 0..1 hover progress of a widget (instant when animations are off).
fn hover_t(ctx: &egui::Context, resp: &egui::Response) -> f32 {
    ctx.animate_bool_with_time(resp.id.with("hv"), resp.hovered(), anim_secs(0.14))
}

/// Applies the palette to egui's style. Called at start and whenever the look changes.
fn apply_theme(ctx: &egui::Context, look: lg::Look, glass: bool) {
    let pl = Palette::make(look, glass);
    PAL.with(|p| p.set(pl));
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.visuals = if look.light { egui::Visuals::light() } else { egui::Visuals::dark() };
        let v = &mut s.visuals;
        v.panel_fill = pl.bg;
        v.window_fill = if glass { rgb(0x1d1f27) } else { pl.card };
        v.extreme_bg_color = pl.field;
        v.override_text_color = Some(pl.text);
        v.selection.bg_fill = pl.accent;
        v.selection.stroke = Stroke::new(1.0, on_fill(pl.accent));
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.inactive.weak_bg_fill = pl.card_hover;
        v.widgets.inactive.bg_fill = pl.card_hover;
        v.widgets.hovered.weak_bg_fill = pl.widget_hover;
        v.widgets.hovered.bg_fill = pl.widget_hover;
        v.widgets.hovered.bg_stroke = Stroke::NONE; // no outlines: hover shows by the fill
        v.widgets.active.weak_bg_fill = pl.accent;
        s.animation_time = if look.anim { 0.12 } else { 0.0 };
        s.spacing.item_spacing = Vec2::new(10.0, 10.0);
        s.spacing.button_padding = Vec2::new(14.0, 8.0);
        s.spacing.slider_width = 260.0;
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(15.0));
        s.text_styles.insert(Button, FontId::proportional(15.0));
        s.text_styles.insert(Small, FontId::proportional(13.0));
        s.text_styles.insert(Monospace, FontId::monospace(15.0));
    });
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn run() -> eframe::Result {
    let look = lg::Look::from_ini(&crate::volume::read_settings());
    // Glass is decided once per start: a transparent window cannot be switched on later.
    let glass = look.glass && crate::glass::supported();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenshare")
            .with_inner_size([540.0, 660.0])
            .with_min_inner_size([480.0, 560.0])
            .with_transparent(glass)
            .with_icon(Arc::new(egui::IconData {
                rgba: crate::icon::rgba().to_vec(),
                width: crate::icon::SIZE,
                height: crate::icon::SIZE,
            })),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "screenshare",
        options,
        Box::new(move |cc| {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            setup_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, look, glass);
            let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
                _ => 0,
            };
            crate::glass::apply(hwnd, !look.light, glass);
            Ok(Box::new(GuiApp::new(&cc.egui_ctx, look, glass, hwnd)))
        }),
    )
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, file) in [("segoe", "segoeui.ttf"), ("segoe-sb", "seguisb.ttf")] {
        if let Ok(bytes) = std::fs::read(format!("C:\\Windows\\Fonts\\{file}")) {
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
        }
    }
    if fonts.font_data.contains_key("segoe") {
        fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "segoe".into());
    }
    let mut bold = vec![];
    if fonts.font_data.contains_key("segoe-sb") {
        bold.push("segoe-sb".to_string());
    }
    bold.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(FontFamily::Name("bold".into()), bold);
    ctx.set_fonts(fonts);

}

fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

// ---------------------------------------------------------------- child processes

struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl Proc {
    fn spawn(ctx: &egui::Context, args: &[String]) -> std::io::Result<Proc> {
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .args(args)
            .arg("--ipc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let (tx, rx) = mpsc::channel();
        fn pump(r: impl Read + Send + 'static, tx: mpsc::Sender<String>, ctx: egui::Context, eof: bool) {
            std::thread::spawn(move || {
                for line in BufReader::new(r).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                    ctx.request_repaint();
                }
                if eof {
                    let _ = tx.send("@EXIT".into());
                    ctx.request_repaint();
                }
            });
        }
        pump(child.stdout.take().unwrap(), tx.clone(), ctx.clone(), true);
        pump(child.stderr.take().unwrap(), tx, ctx.clone(), false);
        Ok(Proc { stdin: child.stdin.take(), child, rx })
    }

    fn send(&mut self, line: &str) {
        if let Some(s) = &mut self.stdin {
            let _ = writeln!(s, "{line}");
            let _ = s.flush();
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Ask politely (the child then says goodbye to the peer), then make sure it's gone.
        self.send("STOP");
        self.stdin = None;
        for _ in 0..20 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
    }
}

struct Session {
    proc: Proc,
    /// This side shows the screen (`host`), not watches it (`client`).
    is_host: bool,
    started: Instant,
    connected_at: Option<Instant>,
    status: String,
    code: Option<String>,
    connected: bool,
    /// Short room code of the simple pairing (beta), when this session uses it.
    room: Option<String>,
    stats: String,
    /// Viewer diagnostics (`@DIAG`): decoder type, timings, queue, jitter.
    diag: String,
    audio: String,
    /// Video bitrate now and how it is chosen (`@RATE`): "ramp" (Авто is probing), "auto" or "fixed".
    rate: Option<(f64, String)>,
    /// Network lamp (`@NET`): level, short text, numbers for the tooltip.
    net: Option<(netq::Level, String, String)>,
    /// Encoder in use and, if the video card was skipped, the reason.
    encoder: String,
    enc_warn: String,
    /// What is shown (window / screen) and whether that is a fallback warning.
    source: String,
    source_warn: bool,
    /// Self-preview: the newest `@PREVIEW` payload not decoded yet, the picture and its caption.
    preview_raw: Option<String>,
    preview_tex: Option<egui::TextureHandle>,
    preview_label: String,
    /// When the last "I'm looking" heartbeat went to the host.
    preview_beat: Option<Instant>,
    bad_code: Option<String>,
    error: Option<String>,
    ended: bool,
    /// The last 60 seconds of bitrate / ping / loss (`@GRAPH`).
    graph: lg::Graph,
    /// Sound cues already played for this session.
    cue_conn: bool,
    cue_end: bool,
}

impl Session {
    fn start(ctx: &egui::Context, args: Vec<String>) -> Result<Session, String> {
        let proc = Proc::spawn(ctx, &args).map_err(|e| format!("не удалось запустить: {e}"))?;
        Ok(Session {
            is_host: args.first().is_some_and(|a| a == "host"),
            started: Instant::now(),
            connected_at: None,
            proc,
            status: "Запускаю...".into(),
            code: None,
            connected: false,
            room: None,
            stats: String::new(),
            diag: String::new(),
            audio: String::new(),
            rate: None,
            net: None,
            encoder: String::new(),
            enc_warn: String::new(),
            source: String::new(),
            source_warn: false,
            preview_raw: None,
            preview_tex: None,
            preview_label: String::new(),
            preview_beat: None,
            bad_code: None,
            error: None,
            ended: false,
            graph: lg::Graph::default(),
            cue_conn: false,
            cue_end: false,
        })
    }

    fn poll(&mut self) {
        while let Ok(l) = self.proc.rx.try_recv() {
            let l = l.trim().to_string();
            if let Some(c) = l.strip_prefix("@CODE ") {
                self.code = Some(c.to_string());
            } else if l == "@CONNECTED" {
                self.connected = true;
                self.connected_at = Some(Instant::now());
                self.bad_code = None;
            } else if let Some(s) = l.strip_prefix("@STATS ") {
                self.stats = s.to_string();
            } else if let Some(d) = l.strip_prefix("@DIAG ") {
                self.diag = d.to_string();
            } else if let Some(a) = l.strip_prefix("@ENCODER ") {
                self.encoder = a.to_string();
            } else if let Some(a) = l.strip_prefix("@ENCWARN ") {
                self.enc_warn = a.to_string();
            } else if let Some(a) = l.strip_prefix("@AUDIO ") {
                self.audio = a.to_string();
            } else if let Some(r) = l.strip_prefix("@RATE ") {
                if let Some((v, m)) = r.split_once('|') {
                    self.rate = v.trim().parse().ok().map(|v| (v, m.trim().to_string()));
                }
            } else if let Some(n) = l.strip_prefix("@NET ") {
                self.net = netq::decode(n);
            } else if let Some(g) = l.strip_prefix("@GRAPH ") {
                if let Some(p) = lg::parse_graph(g) {
                    self.graph.push(p);
                }
            } else if let Some(p) = l.strip_prefix("@PREVIEW ") {
                self.preview_raw = Some(p.to_string()); // only the newest picture matters
            } else if let Some(t) = l.strip_prefix("@PVLABEL ") {
                self.preview_label = t.to_string();
            } else if let Some(a) = l.strip_prefix("@SOURCEWARN ") {
                self.source = a.to_string();
                self.source_warn = true;
            } else if let Some(a) = l.strip_prefix("@SOURCE ") {
                self.source = a.to_string();
                self.source_warn = false;
            } else if let Some(e) = l.strip_prefix("@BADCODE ") {
                self.bad_code = Some(e.to_string());
            } else if let Some(e) = l.strip_prefix("@ERROR ") {
                self.error = Some(e.to_string());
            } else if l == "@EXIT" {
                self.ended = true;
            } else if !l.is_empty() && !l.starts_with('@') {
                self.status = l;
            }
        }
    }

    fn active(&self) -> bool {
        !self.ended
    }

    /// Tells the host the preview is being looked at (it makes no pictures without these), every ~1.5 s.
    fn preview_heartbeat(&mut self) {
        if self.preview_beat.is_none_or(|t| t.elapsed() >= Duration::from_millis(1500)) {
            self.proc.send("PREVIEW");
            self.preview_beat = Some(Instant::now());
        }
    }

    /// Decodes the newest picture into the texture.
    fn update_preview(&mut self, ctx: &egui::Context) {
        let Some(raw) = self.preview_raw.take() else { return };
        let Some((w, h, packed)) = preview::parse_line(&raw) else { return };
        let Ok(rgb) = zstd::bulk::decompress(&packed, preview::rgb_len(w, h)) else { return };
        if rgb.len() != preview::rgb_len(w, h) {
            return;
        }
        let img = egui::ColorImage::from_rgb([w as usize, h as usize], &rgb);
        match &mut self.preview_tex {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => self.preview_tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
        }
    }
}

fn load_show_preview() -> bool {
    crate::volume::ini_get(&crate::volume::read_settings(), "preview") != Some("0")
}

fn save_show_preview(on: bool) {
    crate::volume::write_setting("preview", if on { "1" } else { "0" });
}

/// Keeps the preview of `s` (the process that streams) alive. Only while the switch is on and this window is
/// visible does anything happen: the heartbeat is what makes the host produce pictures at all, so with the
/// switch off (or minimized) the host does no scaling, no compression and writes nothing to the pipe.
fn drive_preview(ctx: &egui::Context, on: bool, s: &mut Session) {
    if !s.connected || s.ended {
        return;
    }
    let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
    if on && !minimized {
        s.preview_heartbeat();
        ctx.request_repaint_after(Duration::from_millis(1000)); // keep the heartbeat going on a still screen
        s.update_preview(ctx);
    } else {
        s.preview_raw = None; // drop anything still in flight; the texture stays for when it comes back
    }
}

/// What the little bar over the preview asked for.
#[derive(PartialEq)]
enum PvAction {
    None,
    ChangeSource,
    Stop,
}

/// A pulsing network lamp: a solid dot and a ring that grows and fades; faster when the link is worse.
fn lamp(ui: &egui::Ui, c: egui::Pos2, r: f32, level: Option<netq::Level>, alpha: f32) {
    let col = match level {
        Some(l) => level_color(l),
        None => c_muted(),
    };
    let p = ui.painter();
    if let (true, Some(l)) = (pal().anim, level) {
        let period = match l {
            netq::Level::Good => 2.6,
            netq::Level::Weak => 1.5,
            _ => 0.9,
        };
        let ph = ((ui.input(|i| i.time) / period) % 1.0) as f32;
        let a = (0.55 * (1.0 - ph) * alpha * 255.0) as u8;
        p.circle_stroke(c, r + ph * r * 1.1, Stroke::new(2.0, Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), a)));
        let focused = ui.input(|i| i.focused);
        ui.ctx().request_repaint_after(Duration::from_millis(if focused { 60 } else { 250 }));
    }
    p.circle_filled(c, r, col.gamma_multiply(alpha));
}

/// Under the "Остановить" button: the switch and, when on, the live thumbnail (no frame, no background) with
/// what it shows. Over the picture a small translucent bar (lamp, "Сменить окно", "Стоп") shows while the mouse
/// moves over it and fades out ~3 s after the last movement.
fn preview_block(ui: &mut egui::Ui, on: &mut bool, s: &Session, can_change: bool) -> PvAction {
    let mut action = PvAction::None;
    ui.add_space(10.0);
    if ui.checkbox(on, "Показывать превью").changed() {
        save_show_preview(*on);
    }
    if !*on || !s.connected || s.ended {
        return action;
    }
    ui.add_space(4.0);
    match &s.preview_tex {
        Some(t) => {
            let scale = (ui.available_width() / preview::BOX_W as f32).min(1.0);
            let (rect, _) = ui.allocate_exact_size(t.size_vec2() * scale, Sense::hover());
            ui.painter().image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            action = preview_bar(ui, rect, s, can_change);
        }
        None => {
            ui.label(RichText::new("превью загружается…").color(c_muted()).size(13.0));
        }
    }
    if !s.preview_label.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(&s.preview_label).color(if s.source_warn { c_red() } else { c_text() }));
    }
    action
}

fn preview_bar(ui: &mut egui::Ui, rect: egui::Rect, s: &Session, can_change: bool) -> PvAction {
    use egui::{pos2, Rect};
    let ctx = ui.ctx().clone();
    let now = ctx.input(|i| i.time);
    #[allow(deprecated)]
    let id = egui::Id::new("pv-touch");
    let last: f64 = ctx.data_mut(|d| *d.get_temp_mut_or_insert_with(id, || now));
    let hovered = ctx.pointer_hover_pos().is_some_and(|p| rect.contains(p));
    let moved = hovered && ctx.input(|i| i.pointer.delta() != Vec2::ZERO || i.pointer.any_pressed());
    let last = if moved { now } else { last };
    ctx.data_mut(|d| d.insert_temp(id, last));
    let idle = ((now - last).max(0.0) * 1000.0) as u128;
    let mut op = lg::bar_opacity(idle);
    if !pal().anim && op > 0.0 {
        op = 1.0;
    }
    if op <= 0.0 {
        return PvAction::None;
    }
    ctx.request_repaint_after(Duration::from_millis(33));
    let h = 38.0;
    let bar = Rect::from_min_size(pos2(rect.left() + 8.0, rect.bottom() - h - 8.0), Vec2::new(rect.width() - 16.0, h));
    if bar.width() < 150.0 {
        return PvAction::None;
    }
    let p = ui.painter().clone();
    let white = |a: f32| Color32::from_white_alpha((a * op * 255.0) as u8);
    p.rect_filled(bar, 10.0, Color32::from_black_alpha((0.62 * op * 255.0) as u8));
    lamp(ui, pos2(bar.left() + 20.0, bar.center().y), 5.0, s.net.as_ref().map(|n| n.0), op);
    let mut action = PvAction::None;
    let mut right = bar.right() - 8.0;
    let mut button = |ui: &mut egui::Ui, label: &str, w: f32, fill: Color32, tag: &str| -> bool {
        let r = Rect::from_min_size(pos2(right - w, bar.center().y - 14.0), Vec2::new(w, 28.0));
        right = r.left() - 8.0;
        let resp = ui.interact(r, id.with(tag), Sense::click());
        let t = hover_t(&ctx, &resp);
        let f = fill.lerp_to_gamma(Color32::WHITE, 0.16 * t);
        p.rect_filled(r, 8.0, f.gamma_multiply(op));
        p.text(r.center(), Align2::CENTER_CENTER, label, FontId::proportional(13.0), Color32::WHITE.gamma_multiply(op));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    };
    if button(ui, "Стоп", 64.0, c_red(), "stop") {
        action = PvAction::Stop;
    }
    if can_change && button(ui, "Сменить окно", 112.0, Color32::from_white_alpha(46), "chg") {
        action = PvAction::ChangeSource;
    }
    // The network text takes what the buttons leave; when it does not fit, only the lamp stays.
    let net_text = s.net.as_ref().map(|n| n.1.as_str()).unwrap_or("сеть…");
    let g = p.layout_no_wrap(net_text.to_string(), FontId::proportional(13.0), white(0.92));
    if bar.left() + 36.0 + g.size().x <= right {
        p.galley(pos2(bar.left() + 36.0, bar.center().y - g.size().y / 2.0), g, white(0.92));
    }
    action
}

// ---------------------------------------------------------------- app

#[derive(PartialEq)]
enum Page {
    /// First start only: three tips.
    Welcome,
    Home,
    Host,
    Join,
    Test,
}

/// How long a new picture takes to replace the old one on a card.
const THUMB_FADE_MS: u128 = 350;

/// One card's picture. A refresh makes a new texture and keeps the old one for the crossfade, so the card never
/// flashes and never shows a half-updated picture.
struct Thumb {
    tex: egui::TextureHandle,
    version: u64,
    prev: Option<egui::TextureHandle>,
    since: Instant,
}

impl Thumb {
    /// 0..1 progress of the fade-in of `tex` over `prev` (or over the empty card).
    fn fade(&self) -> f32 {
        lg::progress(self.since.elapsed().as_millis(), (anim_secs(THUMB_FADE_MS as f32 / 1000.0) * 1000.0) as u128)
    }
}

/// The picker's pictures: the background worker plus the egui textures made from what it produced.
struct ThumbStore {
    worker: Thumbs,
    tex: HashMap<Key, Thumb>,
    icons: HashMap<isize, Option<egui::TextureHandle>>,
    last_list: Instant,
}

impl ThumbStore {
    fn new() -> ThumbStore {
        ThumbStore { worker: Thumbs::new(), tex: HashMap::new(), icons: HashMap::new(), last_list: Instant::now() }
    }

    /// Tells the worker what is on screen and uploads whatever it finished since the last frame.
    fn sync(&mut self, ctx: &egui::Context, targets: Vec<Target>, keys: &[Key]) {
        self.worker.want(targets);
        let seen: HashMap<Key, u64> = self.tex.iter().map(|(k, t)| (*k, t.version)).collect();
        for (k, p) in self.worker.news(&seen) {
            let img = egui::ColorImage::from_rgba_unmultiplied([p.w, p.h], &p.rgba);
            let t = ctx.load_texture(format!("th{k:?}"), img, egui::TextureOptions::LINEAR);
            match self.tex.get_mut(&k) {
                Some(th) => {
                    th.prev = Some(std::mem::replace(&mut th.tex, t));
                    th.version = p.version;
                    th.since = Instant::now();
                }
                None => {
                    self.tex.insert(k, Thumb { tex: t, version: p.version, prev: None, since: Instant::now() });
                }
            }
        }
        for th in self.tex.values_mut() {
            if th.prev.is_some() && th.fade() >= 1.0 {
                th.prev = None; // fade done: free the old picture
            }
        }
        let iseen: HashMap<isize, bool> = self.icons.keys().map(|k| (*k, true)).collect();
        for (h, p) in self.worker.new_icons(&iseen) {
            let t = p.map(|p| {
                ctx.load_texture(format!("ic{h}"), egui::ColorImage::from_rgba_unmultiplied([p.w, p.h], &p.rgba), egui::TextureOptions::LINEAR)
            });
            self.icons.insert(h, t);
        }
        self.tex.retain(|k, _| keys.contains(k));
        // The picker is alive: look again soon so new pictures appear, but never faster than the worker makes them.
        ctx.request_repaint_after(Duration::from_millis(700));
    }
}

/// One card of the picker.
struct Pick {
    choice: Choice,
    key: Key,
    title: String,
    sub: String,
    hwnd: Option<isize>,
    minimized: bool,
}

/// The cards: "Весь экран" first, each monitor when there are several, then the windows.
fn build_picks(monitors: &[String], rects: &[(i32, i32, u32, u32)], primary: usize, windows: &[WinEntry]) -> Vec<Pick> {
    let size = |i: usize| rects.get(i).map(|r| format!("{}×{}", r.2, r.3)).unwrap_or_default();
    let mut v = vec![Pick {
        choice: Choice::Screen,
        key: Key::Monitor(primary),
        title: "Весь экран".into(),
        sub: size(primary),
        hwnd: None,
        minimized: false,
    }];
    if monitors.len() > 1 {
        for i in 0..monitors.len() {
            v.push(Pick { choice: Choice::Monitor(i), key: Key::Monitor(i), title: format!("Монитор {}", i + 1), sub: size(i), hwnd: None, minimized: false });
        }
    }
    for w in windows {
        v.push(Pick {
            choice: Choice::Window(w.hwnd),
            key: Key::Window(w.hwnd),
            title: w.title.clone(),
            sub: String::new(),
            hwnd: Some(w.hwnd),
            minimized: w.minimized,
        });
    }
    v
}

struct GuiApp {
    page: Page,
    monitors: Vec<String>,
    /// Position and size of every monitor on the desktop (for the picker's pictures).
    rects: Vec<(i32, i32, u32, u32)>,
    /// Index of the primary monitor ("Весь экран"; also the canvas size when one window is shown).
    primary: usize,
    /// "Что показывать": the choice, its text (kept in case the window vanished from the list) and the windows.
    choice: Choice,
    choice_label: String,
    windows: Vec<WinEntry>,
    thumbs: ThumbStore,
    /// Mbit/s ceiling of the chosen preset; 0 = Авто (the default).
    bitrate: u32,
    audio: bool,
    /// "Звук только из этого окна" and "Не передавать звук Discord".
    audio_prefs: audiosel::AudioPrefs,
    fullscreen: bool,
    host: Option<Session>,
    join: Option<Session>,
    test: Option<(Session, Session, bool, bool)>,
    show_preview: bool,
    /// Look from gui.ini; `glass` is what this window was started with (cannot change while running).
    look: lg::Look,
    glass: bool,
    glass_ok: bool,
    hwnd: isize,
    logo: egui::TextureHandle,
    /// Page transition: which view is on screen and since when.
    view_key: u32,
    view_since: Instant,
    /// The preview bar's "Сменить окно": open the picker below and scroll to it.
    pick_open_req: bool,
    pick_scroll: bool,
    #[allow(dead_code)]
    pick_anchor: Option<egui::Rect>,
    /// Host option: short room code instead of two long codes (beta).
    easy: bool,
    input: String,
    copied_at: Option<Instant>,
    start_error: Option<String>,
}

impl GuiApp {
    fn new(ctx: &egui::Context, look: lg::Look, glass: bool, hwnd: isize) -> GuiApp {
        let mons = capture::list_monitors().unwrap_or_default();
        let monitor = mons.iter().position(|m| m.primary).unwrap_or(0);
        let monitors = mons
            .iter()
            .map(|m| {
                format!("{}×{} · {}{}", m.width, m.height, m.adapter, if m.primary { " · основной" } else { "" })
            })
            .collect();
        let first_run = crate::volume::ini_get(&crate::volume::read_settings(), "welcome") != Some("1");
        let logo = ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([crate::icon::SIZE as usize; 2], crate::icon::rgba()),
            egui::TextureOptions::LINEAR,
        );
        GuiApp {
            page: if first_run { Page::Welcome } else { Page::Home },
            monitors,
            rects: mons.iter().map(|m| (m.left, m.top, m.width, m.height)).collect(),
            primary: monitor,
            choice: Choice::Screen,
            choice_label: "Весь экран".into(),
            windows: wincap::list_windows(),
            thumbs: ThumbStore::new(),
            bitrate: 0,
            audio: true,
            audio_prefs: audiosel::AudioPrefs::default(),
            fullscreen: true,
            host: None,
            join: None,
            test: None,
            show_preview: load_show_preview(),
            look,
            glass,
            glass_ok: crate::glass::supported(),
            hwnd,
            logo,
            view_key: 0,
            view_since: Instant::now(),
            pick_open_req: false,
            pick_scroll: false,
            pick_anchor: None,
            easy: false,
            input: String::new(),
            copied_at: None,
            start_error: None,
        }
    }

    fn go_home(&mut self) {
        self.host = None;
        self.join = None;
        self.test = None;
        self.input.clear();
        self.start_error = None;
        self.page = Page::Home;
    }

    fn host_args(&self, sub: &str) -> Vec<String> {
        let mut a = vec![sub.to_string()];
        a.extend(source::host_args(self.choice, self.primary));
        if self.bitrate == 0 {
            a.push("--auto-bitrate".into());
        } else {
            a.extend(["--bitrate".to_string(), self.bitrate.to_string()]);
        }
        if !self.audio {
            a.push("--no-audio".into());
        }
        if !self.audio_prefs.window_only {
            a.push("--audio-all".into());
        }
        if !self.audio_prefs.exclude_voice {
            a.push("--keep-voice".into());
        }
        a
    }

    /// "Начать показ" (also "Повторить" after a failure: a short code is made anew, the old one is spent).
    fn start_host(&mut self, ctx: &egui::Context) {
        let mut args = self.host_args("host");
        let room = self.easy.then(easy::new_room_code);
        if let Some(r) = &room {
            args.extend(["--room".to_string(), r.clone()]);
        }
        self.host = None;
        match Session::start(ctx, args) {
            Ok(mut s) => {
                s.room = room;
                self.host = Some(s);
                self.start_error = None;
            }
            Err(e) => self.start_error = Some(e),
        }
    }

    /// "Подключиться" with the code in the input (also "Повторить").
    fn start_join(&mut self, ctx: &egui::Context) {
        let code = self.input.trim().to_string();
        let mut args = vec!["client".to_string()];
        if !self.fullscreen {
            args.push("--windowed".into());
        }
        // A short room code needs no pasting at all: the client fetches the offer itself.
        let room = easy::normalize(&code);
        if let Some(r) = &room {
            args.extend(["--room".to_string(), r.clone()]);
        }
        self.join = None;
        match Session::start(ctx, args) {
            Ok(mut s) => {
                if room.is_some() {
                    s.room = room;
                    s.status = "Ищу трансляцию по короткому коду...".into();
                } else {
                    s.proc.send(&code);
                }
                self.join = Some(s);
                self.start_error = None;
            }
            Err(e) => self.start_error = Some(e),
        }
    }
}

/// Which cue (if any) a session's change of state calls for. Sessions dropped by the user ("Остановить") are
/// gone before they can end, so a stop you asked for is silent.
fn cue_for(s: &mut Session) -> Option<crate::cues::Cue> {
    use crate::cues::Cue;
    let mut out = None;
    if s.connected && !s.cue_conn {
        s.cue_conn = true;
        out = Some(Cue::Connect);
    }
    if s.ended && !s.cue_end {
        s.cue_end = true;
        if s.error.is_some() {
            out = Some(Cue::Error);
        } else if s.connected {
            out = Some(Cue::Disconnect);
        }
    }
    out
}

impl GuiApp {
    fn set_look(&mut self, ctx: &egui::Context, look: lg::Look) {
        let theme_changed = look.light != self.look.light;
        self.look = look;
        apply_theme(ctx, look, self.glass);
        let b = |v: bool| if v { "1" } else { "0" };
        crate::volume::write_setting("theme", if look.light { "light" } else { "dark" });
        crate::volume::write_setting("accent", look.accent.key());
        crate::volume::write_setting("anim", b(look.anim));
        crate::volume::write_setting("sounds", b(look.sounds));
        crate::volume::write_setting("glass", b(look.glass));
        if theme_changed {
            crate::glass::apply(self.hwnd, !look.light, self.glass);
        }
        ctx.request_repaint();
    }

    /// Plays the cues the sessions earned this frame (never in the self-test, never a "connected" on the sharing
    /// side: see cues.rs).
    fn play_cues(&mut self) {
        for s in [&mut self.host, &mut self.join].into_iter().flatten() {
            if let Some(c) = cue_for(s) {
                if self.look.sounds && crate::cues::allowed(c, s.is_host, false) {
                    #[cfg(windows)]
                    crate::cues::play(c);
                }
            }
        }
    }

    /// Identifies what is on screen, so a change of it starts a transition.
    fn view_id(&self) -> u32 {
        let sub = |s: &Option<Session>| match s {
            None => 0,
            Some(s) if s.ended => 3,
            Some(s) if s.connected => 2,
            Some(_) => 1,
        };
        match self.page {
            Page::Welcome => 1,
            Page::Home => 2,
            Page::Host => 10 + sub(&self.host),
            Page::Join => 20 + sub(&self.join),
            Page::Test => 30 + self.test.is_some() as u32,
        }
    }
}

impl eframe::App for GuiApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.glass {
            [0.0; 4]
        } else {
            c_bg().to_normalized_gamma_f32()
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        for s in [&mut self.host, &mut self.join].into_iter().flatten() {
            s.poll();
        }
        if let Some((h, c, sent_h, sent_c)) = &mut self.test {
            h.poll();
            c.poll();
            // Self-test: we play the friend and carry the codes across ourselves.
            if let (false, Some(code)) = (*sent_h, &h.code) {
                c.proc.send(code);
                *sent_h = true;
            }
            if let (false, Some(code)) = (*sent_c, &c.code) {
                h.proc.send(code);
                *sent_c = true;
            }
        }
        self.play_cues();
        let busy = self.host.as_ref().is_some_and(|s| s.active())
            || self.join.as_ref().is_some_and(|s| s.active())
            || self.test.is_some();
        if busy {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        let vid = self.view_id();
        if vid != self.view_key {
            self.view_key = vid;
            self.view_since = Instant::now();
        }
        let tr = lg::ease_out(lg::progress(self.view_since.elapsed().as_millis(), (anim_secs(0.28) * 1000.0) as u128));

        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(c_bg()).inner_margin(Margin::symmetric(28, 24)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    if tr < 1.0 {
                        // Page change: fade in and rise a few pixels. Repaints only while it runs.
                        ui.set_opacity(tr);
                        ui.add_space((1.0 - tr) * 16.0);
                        ui.ctx().request_repaint();
                    }
                    match self.page {
                        Page::Welcome => self.welcome(ui),
                        Page::Home => self.home(ui),
                        Page::Host => self.host_page(ui),
                        Page::Join => self.join_page(ui),
                        Page::Test => self.test_page(ui),
                    }
                });
            });
    }
}

/// A pill you can switch on (selected = accent fill), with an optional colour dot. Hover and selection are smooth.
fn chip(ui: &mut egui::Ui, label: &str, on: bool, swatch: Option<Color32>) -> bool {
    let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(15.0), c_text());
    let dot = if swatch.is_some() { 20.0 } else { 0.0 };
    let size = Vec2::new(galley.size().x + 28.0 + dot, 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let t = hover_t(ui.ctx(), &resp);
    let st = ui.ctx().animate_bool_with_time(resp.id.with("on"), on, anim_secs(0.15));
    let base = c_card_hover();
    let fill = hot(base.lerp_to_gamma(c_accent(), st), t);
    let fg = if st > 0.5 { on_fill(c_accent()) } else { c_text() };
    let p = ui.painter();
    p.rect_filled(rect, 10.0, fill);
    let mut x = rect.left() + 14.0;
    if let Some(c) = swatch {
        let ring = if st > 0.5 { on_fill(c_accent()) } else { c_muted() };
        p.circle_filled(egui::pos2(x + 6.0, rect.center().y), 6.5, ring);
        p.circle_filled(egui::pos2(x + 6.0, rect.center().y), 5.0, c);
        x += dot;
    }
    p.galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp.clicked()
}

/// One sparkline: title and current value on top, the last 60 s below with a round upper bound.
/// `vals` is oldest first; None = no sample (a gap in the line).
fn spark(ui: &mut egui::Ui, title: &str, now: &str, unit: &str, color: Color32, vals: &[Option<f32>], floor: f32) {
    use egui::{pos2, Rect, Shape};
    let max = lg::nice_max(vals.iter().flatten().cloned().fold(0.0, f32::max), floor);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(bold(14.0)));
        ui.label(RichText::new(format!("{now} {unit}")).color(color).size(14.0));
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 54.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 8.0, c_field());
    let plot = rect.shrink2(Vec2::new(8.0, 6.0));
    p.line_segment([plot.left_center(), plot.right_center()], Stroke::new(1.0, c_muted().gamma_multiply(0.22)));
    let n = vals.len();
    let dx = plot.width() / (lg::GRAPH_LEN as f32 - 1.0);
    let xy = |i: usize, v: f32| pos2(plot.right() - (n - 1 - i) as f32 * dx, plot.bottom() - (v / max).clamp(0.0, 1.0) * plot.height());
    for i in 1..n {
        if let (Some(a), Some(b)) = (vals[i - 1], vals[i]) {
            let (pa, pb) = (xy(i - 1, a), xy(i, b));
            p.add(Shape::convex_polygon(
                vec![pa, pb, pos2(pb.x, plot.bottom()), pos2(pa.x, plot.bottom())],
                color.gamma_multiply(0.16),
                Stroke::NONE,
            ));
            p.line_segment([pa, pb], Stroke::new(2.0, color));
        }
    }
    p.text(
        rect.left_top() + Vec2::new(10.0, 3.0),
        Align2::LEFT_TOP,
        format!("{} {unit}", if max >= 10.0 { format!("{max:.0}") } else { format!("{max}") }),
        FontId::proportional(11.0),
        c_muted(),
    );
    let _ = Rect::NOTHING;
}

/// "Качество за последние 60 секунд": bitrate, ping, loss.
fn graph_panel(ui: &mut egui::Ui, g: &lg::Graph, host: bool) {
    ui.add_space(6.0);
    ui.label(RichText::new("Последние 60 секунд").font(bold(15.0)));
    if g.pts.len() < 2 {
        ui.label(RichText::new("График появится через пару секунд.").color(c_muted()).size(13.0));
        return;
    }
    let last = *g.pts.back().unwrap();
    let rate: Vec<Option<f32>> = g.pts.iter().map(|p| Some(p.mbps)).collect();
    let ping: Vec<Option<f32>> = g.pts.iter().map(|p| p.rtt_ms).collect();
    let loss: Vec<Option<f32>> = g.pts.iter().map(|p| Some(p.loss_pct)).collect();
    spark(ui, if host { "Отправка" } else { "Приём" }, &format!("{:.1}", last.mbps), "Мбит/с", c_accent(), &rate, 5.0);
    spark(ui, "Пинг", &last.rtt_ms.map_or("—".to_string(), |r| format!("{r:.0}")), "мс", c_yellow(), &ping, 50.0);
    spark(ui, "Потери", &format!("{:.1}", last.loss_pct), "%", c_red(), &loss, 2.0);
}

// ---------------------------------------------------------------- icons (drawn, so no font is needed)

#[derive(Clone, Copy, PartialEq)]
enum Ico {
    Monitor,
    Eye,
    Play,
    Stop,
    Check,
    Cross,
    Warn,
    Window,
    Retry,
    Home,
    Wifi,
}

fn draw_ico(p: &egui::Painter, ico: Ico, c: egui::Pos2, size: f32, col: Color32) {
    use egui::{pos2, Rect, Shape};
    let s = size / 2.0;
    let st = Stroke::new((size / 11.0).max(1.6), col);
    let pt = |x: f32, y: f32| pos2(c.x + x * s, c.y + y * s);
    let line = |pts: Vec<egui::Pos2>| Shape::line(pts, st);
    match ico {
        Ico::Monitor => {
            p.rect_stroke(Rect::from_min_max(pt(-1.0, -0.8), pt(1.0, 0.45)), 3.0, st, egui::StrokeKind::Inside);
            p.add(line(vec![pt(0.0, 0.45), pt(0.0, 0.85)]));
            p.add(line(vec![pt(-0.5, 0.88), pt(0.5, 0.88)]));
        }
        Ico::Window => {
            p.rect_stroke(Rect::from_min_max(pt(-1.0, -0.8), pt(1.0, 0.8)), 3.0, st, egui::StrokeKind::Inside);
            p.add(line(vec![pt(-1.0, -0.35), pt(1.0, -0.35)]));
        }
        Ico::Eye => {
            let n = 14;
            let up: Vec<_> = (0..=n).map(|i| { let x = -1.0 + 2.0 * i as f32 / n as f32; pt(x, -0.62 * (1.0 - x * x)) }).collect();
            let dn: Vec<_> = (0..=n).map(|i| { let x = -1.0 + 2.0 * i as f32 / n as f32; pt(x, 0.62 * (1.0 - x * x)) }).collect();
            p.add(line(up));
            p.add(line(dn));
            p.circle_filled(c, s * 0.3, col);
        }
        Ico::Play => {
            p.add(Shape::convex_polygon(vec![pt(-0.55, -0.8), pt(0.85, 0.0), pt(-0.55, 0.8)], col, Stroke::NONE));
        }
        Ico::Stop => {
            p.rect_filled(Rect::from_min_max(pt(-0.7, -0.7), pt(0.7, 0.7)), 2.5, col);
        }
        Ico::Check => {
            p.add(line(vec![pt(-0.7, 0.05), pt(-0.2, 0.55), pt(0.75, -0.5)]));
        }
        Ico::Cross => {
            p.add(line(vec![pt(-0.6, -0.6), pt(0.6, 0.6)]));
            p.add(line(vec![pt(-0.6, 0.6), pt(0.6, -0.6)]));
        }
        Ico::Warn => {
            p.add(Shape::closed_line(vec![pt(0.0, -0.9), pt(0.95, 0.75), pt(-0.95, 0.75)], st));
            p.add(line(vec![pt(0.0, -0.3), pt(0.0, 0.25)]));
            p.circle_filled(pt(0.0, 0.5), s * 0.09 + 0.6, col);
        }
        Ico::Retry => {
            let pts: Vec<_> = (0..=18)
                .map(|i| {
                    let a = (-30.0 + 280.0 * i as f32 / 18.0).to_radians();
                    pt(0.8 * a.cos(), 0.8 * a.sin())
                })
                .collect();
            let end = *pts.last().unwrap();
            p.add(line(pts));
            p.add(Shape::convex_polygon(vec![pt(0.8 * (250f32).to_radians().cos() - 0.05, 0.8 * (250f32).to_radians().sin() - 0.55), pt(0.8 * (250f32).to_radians().cos() + 0.5, 0.8 * (250f32).to_radians().sin() + 0.1), end + egui::vec2(-0.1 * s, 0.0)], col, Stroke::NONE));
        }
        Ico::Home => {
            p.add(Shape::closed_line(vec![pt(-0.85, 0.0), pt(0.0, -0.8), pt(0.85, 0.0), pt(0.85, 0.8), pt(-0.85, 0.8)], st));
        }
        Ico::Wifi => {
            for (i, r) in [0.35f32, 0.7, 1.05].into_iter().enumerate() {
                let pts: Vec<_> = (0..=10)
                    .map(|k| {
                        let a = (-135.0 + 90.0 * k as f32 / 10.0).to_radians();
                        pt(r * a.cos(), 0.7 + r * a.sin())
                    })
                    .collect();
                let _ = i;
                p.add(line(pts));
            }
            p.circle_filled(pt(0.0, 0.7), s * 0.12 + 0.5, col);
        }
    }
}

// ---------------------------------------------------------------- widgets

/// The two big buttons of the first screen.
fn tile_button(ui: &mut egui::Ui, ico: Ico, title: &str, subtitle: &str, fill: Color32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 112.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let t = hover_t(ui.ctx(), &resp);
    let mut color = hot(fill, t);
    if resp.is_pointer_button_down_on() {
        color = fill.gamma_multiply(0.82);
    }
    let fg = on_fill(fill);
    let p = ui.painter();
    p.rect_filled(rect, 18.0, color);
    let c = rect.left_center() + Vec2::new(52.0, 0.0);
    p.circle_filled(c, 32.0 + 1.5 * t, fg.gamma_multiply(0.14 + 0.06 * t));
    draw_ico(p, ico, c, 34.0, fg);
    p.text(rect.left_center() + Vec2::new(104.0, -15.0), Align2::LEFT_CENTER, title, bold(24.0), fg);
    p.text(rect.left_center() + Vec2::new(104.0, 17.0), Align2::LEFT_CENTER, subtitle, FontId::proportional(14.0), fg.gamma_multiply(0.82));
    resp.clicked()
}

fn big_button(ui: &mut egui::Ui, text: &str, fill: Color32, enabled: bool) -> bool {
    big_button_i(ui, None, text, fill, enabled)
}

fn big_button_i(ui: &mut egui::Ui, ico: Option<Ico>, text: &str, fill: Color32, enabled: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), sense);
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    let t = if enabled { hover_t(ui.ctx(), &resp) } else { 0.0 };
    let mut color = hot(fill, t);
    if !enabled {
        color = fill.gamma_multiply(0.35);
    } else if resp.is_pointer_button_down_on() {
        color = fill.gamma_multiply(0.8);
    }
    let p = ui.painter();
    p.rect_filled(rect, 14.0, color);
    let text_color = if enabled { on_fill(fill) } else { on_fill(fill).gamma_multiply(0.45) };
    let galley = p.layout_no_wrap(text.to_string(), bold(17.0), text_color);
    let icon_w = if ico.is_some() { 26.0 } else { 0.0 };
    let total = galley.size().x + icon_w;
    let x0 = rect.center().x - total / 2.0;
    if let Some(i) = ico {
        draw_ico(p, i, egui::pos2(x0 + 9.0, rect.center().y), 18.0, text_color);
    }
    p.galley(egui::pos2(x0 + icon_w, rect.center().y - galley.size().y / 2.0), galley, text_color);
    enabled && resp.clicked()
}

fn back_link(ui: &mut egui::Ui) -> bool {
    let r = ui
        .add(egui::Label::new(RichText::new("‹  Назад").color(c_muted()).size(16.0)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let t = hover_t(ui.ctx(), &r);
    if t > 0.0 {
        // hover: the text brightens (drawn over the muted one)
        ui.painter().text(r.rect.left_center(), Align2::LEFT_CENTER, "‹  Назад", FontId::proportional(16.0), c_text().gamma_multiply(t));
    }
    r.clicked()
}

fn title(ui: &mut egui::Ui, t: &str, sub: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(t).font(bold(28.0)));
    ui.label(RichText::new(sub).color(c_muted()));
    ui.add_space(12.0);
}

fn section(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(c_card())
        .corner_radius(16)
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(4.0);
}

/// A collapsible block ("Дополнительно", "Подробности"): closed by default.
fn fold<R>(ui: &mut egui::Ui, name: &str, add: impl FnOnce(&mut egui::Ui) -> R) {
    egui::CollapsingHeader::new(RichText::new(name).font(bold(15.0)).color(c_muted())).default_open(false).show(ui, |ui| {
        add(ui);
    });
}

fn step(ui: &mut egui::Ui, n: u32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 12.0, c_accent());
        ui.painter().text(r.center(), Align2::CENTER_CENTER, n.to_string(), bold(13.0), on_fill(c_accent()));
        ui.label(RichText::new(text).font(bold(16.0)));
    });
}

fn level_color(level: netq::Level) -> Color32 {
    match level {
        netq::Level::Good => c_green(),
        netq::Level::Weak => c_yellow(),
        netq::Level::Bad => c_red(),
    }
}

/// The one-glance status: a coloured round icon (the network lamp of 0.7.7), a headline and a short text.
/// The numbers behind it are in the tooltip; the whole long line lives in "Подробности".
fn status_head(ui: &mut egui::Ui, s: &Session, headline: &str) {
    let (color, short, tip, ico) = match &s.net {
        Some((level, text, details)) => {
            let ico = match level {
                netq::Level::Good => Ico::Check,
                netq::Level::Weak => Ico::Warn,
                netq::Level::Bad => Ico::Cross,
            };
            (level_color(*level), text.clone(), if details.is_empty() { "Пока нет данных".to_string() } else { details.clone() }, ico)
        }
        None => (c_muted(), "Проверяю сеть…".to_string(), "Пока нет данных".to_string(), Ico::Wifi),
    };
    let r = ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 23.0, color.gamma_multiply(0.28));
        ui.painter().circle_stroke(r.center(), 23.0, Stroke::new(2.0, color));
        draw_ico(ui.painter(), ico, r.center(), 22.0, color);
        // The lamp breathes: a ring grows out of it and fades; the worse the network, the faster.
        if let (true, Some((level, _, _))) = (pal().anim, &s.net) {
            let period = match level {
                netq::Level::Good => 2.6,
                netq::Level::Weak => 1.5,
                netq::Level::Bad => 0.9,
            };
            let ph = ((ui.input(|i| i.time) / period) % 1.0) as f32;
            let a = (0.5 * (1.0 - ph) * 255.0) as u8;
            ui.painter().circle_stroke(r.center(), 23.0 + ph * 8.0, Stroke::new(2.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)));
            let focused = ui.input(|i| i.focused);
            ui.ctx().request_repaint_after(Duration::from_millis(if focused { 60 } else { 250 }));
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(headline).font(bold(20.0)));
            ui.label(RichText::new(short).color(color).size(15.0));
        });
    });
    r.response.on_hover_text(tip);
}

fn warn_line(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
        draw_ico(ui.painter(), Ico::Warn, r.center() + Vec2::new(0.0, 3.0), 15.0, c_red());
        ui.add(egui::Label::new(RichText::new(text).color(c_red())).wrap());
    });
}

/// Host: what "Авто" chose.
fn rate_line(ui: &mut egui::Ui, s: &Session) {
    let Some((mbps, mode)) = &s.rate else { return };
    let text = match mode.as_str() {
        "ramp" => format!("Авто: подбираю скорость… сейчас {mbps:.0} Мбит/с"),
        "auto" => format!("Авто: выбрано {mbps:.0} Мбит/с (меняется, если сеть ухудшится)"),
        _ => return,
    };
    ui.label(RichText::new(text).color(c_text()));
}

fn code_box(ui: &mut egui::Ui, code: &str, copied_at: &mut Option<Instant>) {
    egui::Frame::new().fill(c_field()).corner_radius(12).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(RichText::new(code).monospace().size(16.0).color(c_text())).wrap());
    });
    let copied = copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    let label = if copied { "Скопировано" } else { "Скопировать код" };
    if big_button_i(ui, copied.then_some(Ico::Check), label, if copied { c_green() } else { c_accent() }, true) {
        ui.ctx().copy_text(code.to_string());
        *copied_at = Some(Instant::now());
    }
    if copied {
        ui.ctx().request_repaint_after(Duration::from_millis(300));
    }
}

/// The old two-code flow on the host page: my code, then the friend's answer.
fn classic_host_codes(ui: &mut egui::Ui, s: &mut Session, input: &mut String, copied_at: &mut Option<Instant>) {
    section(ui, |ui| {
        step(ui, 1, "Передайте зрителю этот код");
        match &s.code {
            Some(c) => code_box(ui, c, copied_at),
            None => spinner_line(ui, &s.status),
        }
    });
    if s.code.is_some() {
        section(ui, |ui| {
            step(ui, 2, "Вставьте код ответа от зрителя");
            code_input(ui, input, "C...");
            if let Some(e) = &s.bad_code {
                error_text(ui, e);
            }
            let sent = s.status.starts_with("Соединяюсь");
            if sent && s.bad_code.is_none() {
                spinner_line(ui, "Соединяюсь...");
            } else if big_button(ui, "Подключить", c_accent(), !input.trim().is_empty()) {
                s.bad_code = None;
                s.proc.send(input.trim());
            }
        });
    }
}

/// The short room code, large, with a copy button.
fn code_box_big(ui: &mut egui::Ui, code: &str, copied_at: &mut Option<Instant>) {
    egui::Frame::new().fill(c_field()).corner_radius(12).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(code).monospace().size(34.0).color(c_text()));
        });
    });
    let copied = copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    if big_button_i(ui, copied.then_some(Ico::Check), if copied { "Скопировано" } else { "Скопировать код" }, if copied { c_green() } else { c_accent() }, true) {
        ui.ctx().copy_text(code.to_string());
        *copied_at = Some(Instant::now());
    }
    if copied {
        ui.ctx().request_repaint_after(Duration::from_millis(300));
    }
}

fn code_input(ui: &mut egui::Ui, input: &mut String, hint: &str) {
    ui.add(
        egui::TextEdit::singleline(input)
            .hint_text(hint)
            .font(FontId::monospace(15.0))
            .margin(Margin::symmetric(12, 10))
            .desired_width(f32::INFINITY),
    );
}

fn fps_hint(ui: &mut egui::Ui) {
    ui.label(
        RichText::new("Кадр отправляется только когда картинка меняется, поэтому на спокойном экране кадров/с меньше 60 — это нормально.")
            .color(c_muted())
            .size(12.5),
    );
}

fn error_text(ui: &mut egui::Ui, e: &str) {
    ui.label(RichText::new(e).color(c_red()));
}

fn spinner_line(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(16.0).color(c_accent()));
        ui.label(RichText::new(text).color(c_muted()));
    });
}

/// The animated connection screen: a spinner, the current step in words and the whole path as a list.
fn connect_card(ui: &mut egui::Ui, s: &Session) {
    let cur = lg::conn_step(s.is_host, &s.status, s.code.is_some(), s.connected, !s.stats.is_empty());
    section(ui, |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(44.0).color(c_accent()));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(cur.label()).font(bold(22.0)));
                ui.label(RichText::new(cur.hint(s.is_host, s.room.is_some())).color(c_muted()));
            });
        });
        ui.add_space(6.0);
        for st in STEPS {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                let p = ui.painter();
                if st < cur || (st == cur && st == Step::Picture) {
                    p.circle_filled(r.center(), 10.0, c_green());
                    draw_ico(p, Ico::Check, r.center(), 12.0, on_fill(c_green()));
                } else if st == cur {
                    p.circle_filled(r.center(), 10.0, c_accent().gamma_multiply(0.35));
                    let t = ui.input(|i| i.time) as f32;
                    let pulse = 0.5 + 0.5 * (t * 4.0).sin();
                    p.circle_filled(r.center(), 4.0 + 2.5 * pulse, c_accent());
                } else {
                    p.circle_stroke(r.center(), 9.5, Stroke::new(1.5, c_muted().gamma_multiply(0.7)));
                }
                let color = if st <= cur { c_text() } else { c_muted() };
                let font = if st == cur { bold(15.0) } else { FontId::proportional(15.0) };
                ui.label(RichText::new(st.label()).font(font).color(color));
            });
        }
        if !s.status.is_empty() && !s.connected {
            ui.add_space(2.0);
            ui.label(RichText::new(&s.status).color(c_muted()).size(12.5));
        }
        if !s.connected && s.started.elapsed() > Duration::from_secs(45) {
            ui.label(
                RichText::new("Идёт долго. Программа на другой стороне должна оставаться открытой; файрвол может мешать.")
                    .color(c_yellow())
                    .size(13.0),
            );
        }
    });
    ui.ctx().request_repaint_after(Duration::from_millis(50));
}

enum ErrAction {
    Retry,
    Home,
}

/// A failure in plain words with "Повторить"; the raw text is under "Подробности".
fn error_card(ui: &mut egui::Ui, raw: Option<&str>, retry_label: &str) -> Option<ErrAction> {
    let (title, hint) = lg::friendly_error(raw.unwrap_or("остановлено"));
    section(ui, |ui| {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 23.0, c_red().gamma_multiply(0.28));
            ui.painter().circle_stroke(r.center(), 23.0, Stroke::new(2.0, c_red()));
            draw_ico(ui.painter(), Ico::Cross, r.center(), 22.0, c_red());
            ui.add(egui::Label::new(RichText::new(title).font(bold(20.0))).wrap());
        });
        ui.label(RichText::new(hint).color(c_text()));
        if let Some(raw) = raw {
            fold(ui, "Подробности", |ui| {
                ui.add(egui::Label::new(RichText::new(raw).color(c_muted()).size(12.5)).wrap());
            });
        }
    });
    let mut act = None;
    if big_button_i(ui, Some(Ico::Retry), retry_label, c_accent(), true) {
        act = Some(ErrAction::Retry);
    }
    if big_button_i(ui, Some(Ico::Home), "На главную", c_card_hover(), true) {
        act = Some(ErrAction::Home);
    }
    act
}

/// The picker: a grid of cards, each with a small picture of the window / monitor, its app icon and title.
/// Returns what was clicked. Used before the start and, with the same cards, to switch while streaming.
fn source_picker(
    ui: &mut egui::Ui,
    ts: &mut ThumbStore,
    windows: &mut Vec<WinEntry>,
    monitors: &[String],
    rects: &[(i32, i32, u32, u32)],
    primary: usize,
    selected: Choice,
    max_height: f32,
) -> Option<Choice> {
    if ts.last_list.elapsed() > Duration::from_secs(3) {
        *windows = wincap::list_windows(); // new windows appear, closed ones go
        ts.last_list = Instant::now();
    }
    let picks = build_picks(monitors, rects, primary, windows);
    let targets: Vec<Target> = picks
        .iter()
        .filter_map(|p| match (p.key, p.hwnd) {
            (Key::Monitor(i), _) => rects.get(i).map(|r| Target::Monitor { index: i, left: r.0, top: r.1, w: r.2, h: r.3 }),
            (Key::Window(h), _) => Some(Target::Window { hwnd: h, minimized: p.minimized }),
        })
        .collect();
    let keys: Vec<Key> = picks.iter().map(|p| p.key).collect();
    ts.sync(ui.ctx(), targets, &keys);

    let gap = ui.spacing().item_spacing.x;
    let mut clicked = None;
    egui::ScrollArea::vertical().max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
        let card_w = ((ui.available_width() - gap - 8.0) / 2.0).floor();
        for row in picks.chunks(2) {
            ui.horizontal(|ui| {
                for p in row {
                    if pick_card(ui, ts, p, card_w, p.choice == selected) {
                        clicked = Some(p.choice);
                    }
                }
            });
        }
    });
    clicked
}

fn pick_card(ui: &mut egui::Ui, ts: &ThumbStore, p: &Pick, w: f32, selected: bool) -> bool {
    let pad = 8.0;
    let th_h = ((w - 2.0 * pad) * 9.0 / 16.0).round();
    let h = th_h + pad * 2.0 + 28.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(&p.title);
    let paint = ui.painter();
    // Hover and selection change the fill smoothly instead of snapping.
    let ht = hover_t(ui.ctx(), &resp);
    let st = ui.ctx().animate_bool_with_time(resp.id.with("sel"), selected, anim_secs(0.18));
    let sel_fill = c_card().lerp_to_gamma(c_accent(), if pal().light { 0.15 } else { 0.28 });
    let fill = c_card().lerp_to_gamma(c_card_hover(), ht).lerp_to_gamma(sel_fill, st);
    paint.rect_filled(rect, 14.0, fill);
    if st > 0.0 {
        paint.rect_stroke(rect, 14.0, Stroke::new(2.0, c_accent().gamma_multiply(st)), egui::StrokeKind::Inside);
    }
    let th = egui::Rect::from_min_size(rect.min + Vec2::splat(pad), Vec2::new(w - 2.0 * pad, th_h));
    paint.rect_filled(th, 8.0, c_field());
    let icon_tex = p.hwnd.and_then(|h| ts.icons.get(&h)).and_then(|t| t.as_ref());
    let fade = ts.tex.get(&p.key).map(|t| t.fade()).unwrap_or(1.0);
    // The empty card (program icon) shows until the first picture has faded in over it.
    if fade < 1.0 || !ts.tex.contains_key(&p.key) {
        match (icon_tex, p.hwnd) {
            (Some(t), _) => {
                let r = egui::Rect::from_center_size(th.center(), Vec2::splat(40.0));
                paint.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            (None, Some(_)) => draw_ico(paint, Ico::Window, th.center(), 34.0, c_muted()),
            (None, None) => draw_ico(paint, Ico::Monitor, th.center(), 38.0, c_muted()),
        }
    }
    if let Some(t) = ts.tex.get(&p.key) {
        // Letterbox inside the 16:9 area, keeping the window's own aspect. A refresh: the old picture stays
        // underneath and the new one fades in over it.
        let draw = |tex: &egui::TextureHandle, a: f32| {
            let tv = tex.size_vec2();
            let k = (th.width() / tv.x).min(th.height() / tv.y);
            let r = egui::Rect::from_center_size(th.center(), tv * k);
            paint.image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE.gamma_multiply(a));
        };
        if let Some(prev) = &t.prev {
            draw(prev, 1.0);
        }
        draw(&t.tex, lg::ease_out(fade));
        if fade < 1.0 {
            ui.ctx().request_repaint_after(Duration::from_millis(16));
        }
    }
    if p.minimized {
        paint.text(th.center_bottom() - Vec2::new(0.0, 6.0), Align2::CENTER_BOTTOM, "свёрнуто", FontId::proportional(12.0), c_muted());
    }
    if selected {
        let c = th.right_top() + Vec2::new(-15.0, 15.0);
        paint.circle_filled(c, 11.0, c_accent());
        draw_ico(paint, Ico::Check, c, 13.0, on_fill(c_accent()));
    }
    // Line under the picture: the program's icon (or a drawn one) and the title.
    let y = th.bottom() + 6.0 + 11.0;
    let mut x = rect.left() + pad;
    let small = Vec2::splat(18.0);
    match (icon_tex, p.hwnd) {
        (Some(t), _) => {
            let r = egui::Rect::from_center_size(egui::pos2(x + 9.0, y), small);
            paint.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        (None, Some(_)) => draw_ico(paint, Ico::Window, egui::pos2(x + 9.0, y), 15.0, c_muted()),
        (None, None) => draw_ico(paint, Ico::Monitor, egui::pos2(x + 9.0, y), 16.0, c_muted()),
    }
    x += 26.0;
    let title = if p.sub.is_empty() { p.title.clone() } else { format!("{} · {}", p.title, p.sub) };
    let mut job = egui::text::LayoutJob::simple_singleline(title, FontId::proportional(14.0), c_text());
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - pad - x).max(10.0));
    let galley = ui.painter().layout_job(job);
    let gh = galley.size().y;
    ui.painter().galley(egui::pos2(x, y - gh / 2.0), galley, c_text());
    resp.clicked()
}

impl GuiApp {
    /// A click on a card: remembers the choice and its label (same wording as the old menu).
    fn choose(&mut self, c: Choice) {
        self.choice = c;
        let items = source::build_menu(&self.monitors, &self.windows);
        if let Some(it) = items.iter().find(|i| i.choice == c) {
            self.choice_label = it.label.clone();
        }
    }

    /// "Что показывать" before the start.
    fn pick_source(&mut self, ui: &mut egui::Ui) {
        if self.monitors.is_empty() {
            error_text(ui, "Мониторы не найдены");
            return;
        }
        let sel = self.choice;
        let (mut windows, monitors, rects, primary) = (std::mem::take(&mut self.windows), self.monitors.clone(), self.rects.clone(), self.primary);
        let picked = source_picker(ui, &mut self.thumbs, &mut windows, &monitors, &rects, primary, sel, 300.0);
        self.windows = windows;
        if let Some(c) = picked {
            self.choose(c);
        }
        if matches!(self.choice, Choice::Window(_)) {
            ui.label(
                RichText::new("Зрители увидят только это окно. Если окно закроется — будет показан весь экран.").color(c_muted()).size(13.0),
            );
        }
    }

    /// "Дополнительно": quality, audio, the short-code option. Defaults need no touching.
    fn advanced(&mut self, ui: &mut egui::Ui, with_audio: bool, with_easy: bool) {
        ui.label(RichText::new("Качество").font(bold(15.0)));
        ui.horizontal_wrapped(|ui| {
            for (name, v) in [("Авто", 0), ("Стандарт · 30 Мбит/с", 30), ("Высокое качество · 60 Мбит/с", 60)] {
                let on = self.bitrate == v;
                if chip(ui, name, on, None) {
                    self.bitrate = v;
                }
            }
        });
        ui.label(
            RichText::new(
                "«Авто» в начале сама подбирает скорость: стартует с 20 Мбит/с и поднимает до 60, пока нет потерь. \
                 «Стандарт» и «Высокое» — потолок: если интернет не успевает, программа сама снижает качество, а потом возвращает.",
            )
            .color(c_muted())
            .size(13.0),
        );
        if with_audio {
            ui.add_space(6.0);
            ui.label(RichText::new("Звук").font(bold(15.0)));
            ui.checkbox(&mut self.audio, "Передавать звук компьютера");
            if self.audio {
                if matches!(self.choice, Choice::Window(_)) {
                    ui.checkbox(&mut self.audio_prefs.window_only, "Звук только из этого окна");
                }
                ui.checkbox(&mut self.audio_prefs.exclude_voice, "Не передавать звук Discord").on_hover_text("Иначе зритель услышит себя с эхом");
            }
        }
        if with_easy {
            ui.add_space(6.0);
            ui.label(RichText::new("Подключение").font(bold(15.0)));
            ui.checkbox(&mut self.easy, "Простое подключение (beta): один короткий код вместо двух длинных");
            if self.easy {
                ui.label(
                    RichText::new("Коды обмениваются через публичный сервер ntfy.sh в зашифрованном виде. Обычные коды тоже останутся — на случай, если не сработает.")
                        .color(c_muted())
                        .size(12.5),
                );
            }
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), Vec2::splat(56.0))));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("screenshare").font(bold(32.0)));
                ui.label(RichText::new("Показ экрана напрямую, без серверов").color(c_muted()).size(15.0));
            });
        });
        ui.add_space(26.0);
        if tile_button(ui, Ico::Monitor, "Показать экран", "Зритель увидит экран и услышит звук", c_accent()) {
            self.windows = wincap::list_windows();
            self.thumbs.last_list = Instant::now();
            self.page = Page::Host;
        }
        ui.add_space(4.0);
        if tile_button(ui, Ico::Eye, "Смотреть", "Понадобится код трансляции", c_tile2()) {
            self.page = Page::Join;
        }
        ui.add_space(18.0);
        fold(ui, "Дополнительно", |ui| {
            if big_button_i(ui, Some(Ico::Play), "Проверить на этом ПК", c_card_hover(), true) {
                self.windows = wincap::list_windows();
                self.thumbs.last_list = Instant::now();
                self.page = Page::Test;
            }
            ui.label(RichText::new("Вы увидите то же, что увидит зритель. Качество, звук и порт настраиваются на странице показа.").color(c_muted()).size(13.0));
            ui.add_space(8.0);
            self.look_settings(ui);
            ui.add_space(6.0);
            ui.label(RichText::new(format!("версия {}", env!("CARGO_PKG_VERSION"))).color(c_muted()).size(12.0));
        });
    }

    /// "Оформление": theme, accent, animations, sounds, glass. Saved to gui.ini at once.
    fn look_settings(&mut self, ui: &mut egui::Ui) {
        let mut look = self.look;
        ui.label(RichText::new("Оформление").font(bold(15.0)));
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Тема").color(c_muted()));
            if chip(ui, "Тёмная", !look.light, None) {
                look.light = false;
            }
            if chip(ui, "Светлая", look.light, None) {
                look.light = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Цвет").color(c_muted()));
            for a in lg::Accent::ALL {
                let sw = Palette::make(lg::Look { accent: a, glass: false, ..look }, false).accent;
                if chip(ui, a.label(), look.accent == a, Some(sw)) {
                    look.accent = a;
                }
            }
        });
        ui.checkbox(&mut look.anim, "Плавные анимации");
        ui.checkbox(&mut look.sounds, "Звуковые сигналы: подключение, отключение, ошибка").on_hover_text(
            "Тихие короткие звуки. Пока идёт показ экрана, они молчат (звук компьютера уходит в трансляцию) — отключение и ошибки звучат уже после остановки.",
        );
        let r = ui.add_enabled(self.glass_ok, egui::Checkbox::new(
                &mut look.glass,
                if self.glass_ok { "Стекло Windows 11 (после перезапуска)" } else { "Стекло — только Windows 11" },
            ));
        if !self.glass_ok {
            r.on_disabled_hover_text("Нужна Windows 11 (сборка 22621 и новее). Здесь окно остаётся плоским.");
        }
        if look != self.look {
            let ctx = ui.ctx().clone();
            self.set_look(&ctx, look);
        }
    }

    /// First start: three short tips; "Поехали" remembers it in gui.ini.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), Vec2::splat(64.0))));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Добро пожаловать").font(bold(28.0)));
                ui.label(RichText::new("Три коротких подсказки").color(c_muted()).size(15.0));
            });
        });
        ui.add_space(14.0);
        let tips = [
            (Ico::Monitor, "Покажите экран", "Нажмите «Показать экран», выберите окно или весь экран и передайте зрителю код."),
            (Ico::Eye, "Смотрите трансляцию", "Зрителю нужно нажать «Смотреть» и вставить ваш код. Видео и звук идут напрямую, без серверов."),
            (Ico::Wifi, "Следите за лампочкой", "Зелёная — всё хорошо, жёлтая — сеть слабая, красная — картинка будет терять качество. В окне просмотра: F11 — полный экран, колесо мыши — громкость."),
        ];
        for (ico, head, text) in tips {
            section(ui, |ui| {
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 22.0, c_accent());
                    draw_ico(ui.painter(), ico, r.center(), 22.0, on_fill(c_accent()));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.label(RichText::new(head).font(bold(18.0)));
                        ui.add(egui::Label::new(RichText::new(text).color(c_muted())).wrap());
                    });
                });
            });
        }
        ui.add_space(10.0);
        if big_button(ui, "Поехали", c_accent(), true) {
            crate::volume::write_setting("welcome", "1");
            self.page = Page::Home;
        }
    }

    fn host_page(&mut self, ui: &mut egui::Ui) {
        let running = self.host.as_ref().is_some_and(|s| s.active());
        if !running && back_link(ui) {
            self.go_home();
            return;
        }
        let live = self.host.as_ref().is_some_and(|s| s.connected && s.active());
        title(
            ui,
            "Показать экран",
            if live { "Ваш экран транслируется" } else { "Выберите, что показать, и нажмите «Начать»" },
        );

        let Some(s) = &mut self.host else {
            section(ui, |ui| {
                ui.label(RichText::new("Что показывать").font(bold(15.0)));
                self.pick_source(ui);
            });
            if let Some(e) = &self.start_error {
                error_text(ui, e);
            }
            if big_button_i(ui, Some(Ico::Play), "Начать показ", c_accent(), !self.monitors.is_empty()) {
                let ctx = ui.ctx().clone();
                self.start_host(&ctx);
            }
            ui.add_space(6.0);
            fold(ui, "Дополнительно", |ui| self.advanced(ui, true, true));
            return;
        };

        if s.connected && !s.ended {
            // The picture/numbers of the first second are not in yet: still the connection screen.
            if s.stats.is_empty() && s.connected_at.is_some_and(|t| t.elapsed() < Duration::from_secs(6)) {
                connect_card(ui, s);
                return;
            }
            // The preview is made only while this window is visible and the switch is on.
            drive_preview(ui.ctx(), self.show_preview, s);
            let s = &mut *s;
            section(ui, |ui| {
                status_head(ui, s, "Идёт трансляция");
                if !s.source.is_empty() && s.source_warn {
                    warn_line(ui, &s.source);
                }
                if !s.enc_warn.is_empty() {
                    warn_line(ui, &format!("Видеокарта не используется: {}", s.enc_warn));
                }
                if s.audio.starts_with("Звук выключен") || s.audio.starts_with("Звук не") {
                    warn_line(ui, &s.audio);
                }
                fold(ui, "Подробности", |ui| {
                    rate_line(ui, s);
                    ui.label(RichText::new(if s.stats.is_empty() { "собираю статистику..." } else { &s.stats }).color(c_muted()));
                    if !s.encoder.is_empty() {
                        ui.label(RichText::new(&s.encoder).color(c_text()));
                    }
                    if !s.audio.is_empty() {
                        ui.label(RichText::new(&s.audio).color(c_text()));
                    }
                    if !s.source.is_empty() && !s.source_warn {
                        ui.label(RichText::new(&s.source).color(c_text()));
                    }
                    fps_hint(ui);
                    graph_panel(ui, &s.graph, true);
                });
            });
            // Switching without stopping: the same cards as at the start.
            let mut picked = None;
            if !self.monitors.is_empty() {
                section(ui, |ui| {
                    let shown = if s.preview_label.is_empty() { self.choice_label.clone() } else { s.preview_label.clone() };
                    let mut hdr = egui::CollapsingHeader::new(RichText::new("Сменить окно или экран").font(bold(16.0)))
                        .id_salt("swpick")
                        .default_open(false);
                    if std::mem::take(&mut self.pick_open_req) {
                        hdr = hdr.open(Some(true)); // asked for by the bar over the preview
                    }
                    let r = hdr.show(ui, |ui| {
                        let mut windows = std::mem::take(&mut self.windows);
                        picked = source_picker(ui, &mut self.thumbs, &mut windows, &self.monitors, &self.rects, self.primary, self.choice, 280.0);
                        self.windows = windows;
                        ui.label(RichText::new("Можно менять, не останавливая показ.").color(c_muted()).size(12.5));
                    });
                    if std::mem::take(&mut self.pick_scroll) {
                        ui.scroll_to_rect(r.header_response.rect, Some(egui::Align::TOP));
                    }
                    ui.label(RichText::new(format!("Сейчас: {shown}")).color(c_muted()).size(13.0));
                });
            }
            if let Some(c) = picked {
                self.choice = c;
                if let Some(it) = source::build_menu(&self.monitors, &self.windows).iter().find(|i| i.choice == c) {
                    self.choice_label = it.label.clone();
                }
                s.proc.send(&source::SourceReq::from_choice(c, self.primary).to_line());
            }
            if self.audio {
                let before = self.audio_prefs;
                fold(ui, "Дополнительно", |ui| {
                    ui.label(RichText::new("Звук").font(bold(15.0)));
                    if matches!(self.choice, Choice::Window(_)) {
                        ui.checkbox(&mut self.audio_prefs.window_only, "Звук только из этого окна");
                    }
                    ui.checkbox(&mut self.audio_prefs.exclude_voice, "Не передавать звук Discord").on_hover_text("Иначе зритель услышит себя с эхом");
                });
                if self.audio_prefs != before {
                    s.proc.send(&self.audio_prefs.line());
                }
            }
            if big_button_i(ui, Some(Ico::Stop), "Остановить", c_red(), true) {
                self.go_home();
                return;
            }
            let mut act = PvAction::None;
            if let Some(s) = &self.host {
                act = preview_block(ui, &mut self.show_preview, s, !self.monitors.is_empty());
            }
            match act {
                PvAction::Stop => self.go_home(),
                PvAction::ChangeSource => {
                    self.pick_open_req = true;
                    self.pick_scroll = true;
                    ui.ctx().request_repaint();
                }
                PvAction::None => {}
            }
            return;
        }
        if s.ended {
            let (was, raw) = (s.connected, s.error.clone());
            if was && raw.is_none() {
                section(ui, |ui| {
                    ui.label(RichText::new("Трансляция завершена").font(bold(20.0)));
                    ui.label(RichText::new(&s.status).color(c_muted()));
                });
                if big_button_i(ui, Some(Ico::Retry), "Показать снова", c_accent(), true) {
                    let ctx = ui.ctx().clone();
                    self.start_host(&ctx);
                } else if big_button_i(ui, Some(Ico::Home), "На главную", c_card_hover(), true) {
                    self.go_home();
                }
                return;
            }
            let raw = raw.or_else(|| Some(s.status.clone()).filter(|t| !t.is_empty()));
            match error_card(ui, raw.as_deref(), "Повторить") {
                Some(ErrAction::Retry) => {
                    let ctx = ui.ctx().clone();
                    self.start_host(&ctx);
                }
                Some(ErrAction::Home) => self.go_home(),
                None => {}
            }
            return;
        }

        if let Some(room) = s.room.clone() {
            section(ui, |ui| {
                step(ui, 1, "Передайте зрителю этот код");
                code_box_big(ui, &room, &mut self.copied_at);
                ui.label(
                    RichText::new("Зритель вводит его в «Смотреть» — больше ничего пересылать не нужно. Код живёт 15 минут.")
                        .color(c_muted())
                        .size(13.0),
                );
            });
            connect_card(ui, s);
            if s.code.is_some() {
                egui::CollapsingHeader::new(RichText::new("Не подключается? Обычный способ с длинными кодами").color(c_muted())).show(ui, |ui| {
                    classic_host_codes(ui, s, &mut self.input, &mut self.copied_at);
                });
            }
        } else {
            classic_host_codes(ui, s, &mut self.input, &mut self.copied_at);
            connect_card(ui, s);
        }
        if big_button_i(ui, Some(Ico::Cross), "Отмена", c_card_hover(), true) {
            self.go_home();
        }
    }

    fn join_page(&mut self, ui: &mut egui::Ui) {
        let running = self.join.as_ref().is_some_and(|s| s.active());
        if !running && back_link(ui) {
            self.go_home();
            return;
        }
        let live = self.join.as_ref().is_some_and(|s| s.connected && s.active());
        title(ui, "Смотреть", if live { "Вы смотрите трансляцию" } else { "Нужен код от показывающего: он нажимает «Показать экран» и присылает код" });

        if let Some(s) = &mut self.join {
            if s.connected && !s.ended {
                if s.stats.is_empty() && s.connected_at.is_some_and(|t| t.elapsed() < Duration::from_secs(6)) {
                    connect_card(ui, s);
                    return;
                }
                section(ui, |ui| {
                    status_head(ui, s, "Идёт просмотр");
                    ui.label(RichText::new("Окно просмотра открыто. F11 или двойной клик — полный экран.").color(c_muted()).size(14.0));
                    fold(ui, "Подробности", |ui| {
                        ui.label(RichText::new(if s.stats.is_empty() { "собираю статистику..." } else { &s.stats }).color(c_muted()));
                        if !s.diag.is_empty() {
                            ui.label(RichText::new(&s.diag).color(c_muted()).size(12.0));
                        }
                        ui.label(RichText::new("Журнал: %LOCALAPPDATA%\\screenshare\\viewer.log").color(c_muted()).size(12.0));
                        graph_panel(ui, &s.graph, false);
                    });
                });
                if big_button_i(ui, Some(Ico::Stop), "Отключиться", c_red(), true) {
                    self.go_home();
                }
                return;
            }
            if s.ended {
                let (was, raw) = (s.connected, s.error.clone());
                if was && raw.is_none() {
                    section(ui, |ui| {
                        ui.label(RichText::new("Просмотр завершён").font(bold(20.0)));
                        ui.label(RichText::new(&s.status).color(c_muted()));
                    });
                    if big_button_i(ui, Some(Ico::Retry), "Подключиться снова", c_accent(), true) {
                        let ctx = ui.ctx().clone();
                        self.start_join(&ctx);
                    } else if big_button_i(ui, Some(Ico::Home), "На главную", c_card_hover(), true) {
                        self.go_home();
                    }
                    return;
                }
                let raw = raw.or_else(|| Some(s.status.clone()).filter(|t| !t.is_empty()));
                match error_card(ui, raw.as_deref(), "Повторить") {
                    Some(ErrAction::Retry) => {
                        let ctx = ui.ctx().clone();
                        self.start_join(&ctx);
                    }
                    Some(ErrAction::Home) => self.go_home(),
                    None => {}
                }
                return;
            }
        }

        if let Some(s) = self.join.as_ref().filter(|s| s.room.is_some()) {
            section(ui, |ui| {
                step(ui, 1, &format!("Код {}", s.room.as_deref().unwrap_or("")));
                ui.label(
                    RichText::new("Окно «Показать экран» на другой стороне должно оставаться открытым. Если не выходит дольше минуты — отмените и используйте обычный длинный код.")
                        .color(c_muted())
                        .size(13.0),
                );
            });
            connect_card(ui, s);
            if big_button_i(ui, Some(Ico::Cross), "Отмена", c_card_hover(), true) {
                self.go_home();
            }
            return;
        }
        let have_reply = self.join.as_ref().is_some_and(|s| s.code.is_some());
        if !have_reply {
            section(ui, |ui| {
                step(ui, 1, "Введите код трансляции");
                code_input(ui, &mut self.input, "короткий код (K7QM-X2PD, beta) или длинный H...");
                if let Some(e) = self.join.as_ref().and_then(|s| s.bad_code.as_ref()) {
                    error_text(ui, e);
                }
                if let Some(e) = &self.start_error {
                    error_text(ui, e);
                }
                if self.join.as_ref().is_some_and(|s| s.bad_code.is_none()) {
                    spinner_line(ui, "Проверяю код...");
                } else if big_button_i(ui, Some(Ico::Eye), "Смотреть", c_accent(), !self.input.trim().is_empty()) {
                    if let Some(s) = &mut self.join {
                        let code = self.input.trim().to_string();
                        s.bad_code = None;
                        s.proc.send(&code);
                    } else {
                        let ctx = ui.ctx().clone();
                        self.start_join(&ctx);
                    }
                }
            });
            if self.join.is_none() {
                fold(ui, "Дополнительно", |ui| {
                    ui.checkbox(&mut self.fullscreen, "Открыть на весь экран");
                });
            }
        } else {
            let s = self.join.as_mut().unwrap();
            section(ui, |ui| {
                step(ui, 2, "Передайте показывающему код ответа");
                code_box(ui, s.code.as_ref().unwrap(), &mut self.copied_at);
            });
        }
        if let Some(s) = &self.join {
            // A bad code is just a typo: stay on the input, no connection card for it.
            if have_reply || s.bad_code.is_none() {
                connect_card(ui, s);
            }
            if big_button_i(ui, Some(Ico::Cross), "Отмена", c_card_hover(), true) {
                self.go_home();
            }
        }
    }

    fn test_page(&mut self, ui: &mut egui::Ui) {
        if self.test.is_none() && back_link(ui) {
            self.go_home();
            return;
        }
        title(ui, "Проверить на этом ПК", "Показ и просмотр на одном ПК — как увидит зритель");

        if let Some((h, _, _, _)) = &mut self.test {
            drive_preview(ui.ctx(), self.show_preview, h); // the streaming side makes the pictures
        }
        let Some((h, c, _, _)) = &self.test else {
            section(ui, |ui| {
                ui.label(RichText::new("Что показывать").font(bold(15.0)));
                self.pick_source(ui);
            });
            section(ui, |ui| {
                ui.label(RichText::new("Совет").font(bold(15.0)));
                ui.label(
                    RichText::new(
                        "Окно просмотра откроется отдельно. Если показывать тот же монитор, на котором это окно, \
                         получится «зеркало в зеркале» — это нормально. Нагляднее показывать второй монитор. \
                         Звук в проверке не передаётся, чтобы не было эха.",
                    )
                    .color(c_muted())
                    .size(13.0),
                );
            });
            if let Some(e) = &self.start_error {
                error_text(ui, e);
            }
            if big_button_i(ui, Some(Ico::Play), "Запустить проверку", c_accent(), !self.monitors.is_empty()) {
                let mut host_args = self.host_args("host");
                host_args.push("--no-audio".into());
                let started = Session::start(ui.ctx(), host_args).and_then(|h| {
                    Session::start(ui.ctx(), vec!["client".into(), "--no-audio".into(), "--windowed".into()]).map(|c| (h, c))
                });
                match started {
                    Ok((h, c)) => self.test = Some((h, c, false, false)),
                    Err(e) => self.start_error = Some(e),
                }
            }
            ui.add_space(6.0);
            fold(ui, "Дополнительно", |ui| self.advanced(ui, false, false));
            return;
        };

        // One short line per side; the numbers are in "Подробности".
        let line = |ui: &mut egui::Ui, name: &str, s: &Session| {
            let (color, text) = if let Some(e) = &s.error {
                (c_red(), lg::friendly_error(e).0.to_string())
            } else if s.ended {
                (c_muted(), "остановлено".to_string())
            } else if s.connected && !s.stats.is_empty() {
                match &s.net {
                    Some((l, t, _)) => (level_color(*l), t.clone()),
                    None => (c_green(), "работает".to_string()),
                }
            } else {
                (c_accent(), lg::conn_step(s.is_host, &s.status, s.code.is_some(), s.connected, !s.stats.is_empty()).label().to_string())
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 6.0, color);
                ui.label(RichText::new(name).font(bold(16.0)));
                ui.label(RichText::new(text).color(color));
            });
        };
        section(ui, |ui| {
            line(ui, "Трансляция", h);
            line(ui, "Просмотр", c);
            if !(h.ended || c.ended) && !(h.connected && c.connected && !h.stats.is_empty()) {
                spinner_line(ui, "Соединяю две половины на этом компьютере…");
            }
            fold(ui, "Подробности", |ui| {
                for (name, s) in [("Трансляция", h), ("Просмотр", c)] {
                    ui.label(RichText::new(name).font(bold(14.0)));
                    if let Some(e) = &s.error {
                        error_text(ui, e);
                    }
                    ui.label(RichText::new(if s.stats.is_empty() { &s.status } else { &s.stats }).color(c_muted()));
                }
                if !c.diag.is_empty() {
                    ui.label(RichText::new(&c.diag).color(c_muted()).size(12.0));
                }
                fps_hint(ui);
            });
        });
        if h.connected && c.connected {
            ui.label(RichText::new("Окно просмотра открыто отдельно. F11 или двойной клик — полный экран.").color(c_muted()));
        }
        if big_button_i(ui, Some(Ico::Stop), "Остановить", c_red(), true) {
            self.go_home();
            return;
        }
        let mut stop_test = false;
        if let Some((h, _, _, _)) = &self.test {
            stop_test = preview_block(ui, &mut self.show_preview, h, false) == PvAction::Stop;
        }
        if stop_test {
            self.go_home();
        }
    }
}
