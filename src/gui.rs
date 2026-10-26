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

const BG: Color32 = Color32::from_rgb(0x12, 0x13, 0x18);
const CARD: Color32 = Color32::from_rgb(0x1d, 0x1f, 0x27);
const CARD_HOVER: Color32 = Color32::from_rgb(0x25, 0x28, 0x33);
const FIELD: Color32 = Color32::from_rgb(0x0d, 0x0e, 0x12);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xf0);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x90, 0xa0);
const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x7c, 0xff);
const GREEN: Color32 = Color32::from_rgb(0x3e, 0xcf, 0x8e);
const YELLOW: Color32 = Color32::from_rgb(0xf0, 0xb4, 0x3c);
const RED: Color32 = Color32::from_rgb(0xef, 0x5b, 0x5b);

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenshare")
            .with_inner_size([540.0, 660.0])
            .with_min_inner_size([480.0, 560.0]),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "screenshare",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(GuiApp::new()))
        }),
    )
}

fn setup_style(ctx: &egui::Context) {
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

    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = FIELD;
        v.override_text_color = Some(TEXT);
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, TEXT);
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.inactive.weak_bg_fill = CARD_HOVER;
        v.widgets.inactive.bg_fill = CARD_HOVER;
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2e, 0x32, 0x40);
        v.widgets.hovered.bg_stroke = Stroke::NONE; // no outlines: hover shows by the fill
        v.widgets.active.weak_bg_fill = ACCENT;
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

/// Under the "Остановить" button: the switch and, when on, the live thumbnail (no frame, no background) with
/// what it shows.
fn preview_block(ui: &mut egui::Ui, on: &mut bool, s: &Session) {
    ui.add_space(10.0);
    if ui.checkbox(on, "Показывать превью").changed() {
        save_show_preview(*on);
    }
    if !*on || !s.connected || s.ended {
        return;
    }
    ui.add_space(4.0);
    match &s.preview_tex {
        Some(t) => {
            let scale = (ui.available_width() / preview::BOX_W as f32).min(1.0);
            let (rect, _) = ui.allocate_exact_size(t.size_vec2() * scale, Sense::hover());
            ui.painter().image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            ui.label(RichText::new("превью загружается…").color(MUTED).size(13.0));
        }
    }
    if !s.preview_label.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(&s.preview_label).color(if s.source_warn { RED } else { TEXT }));
    }
}

// ---------------------------------------------------------------- app

#[derive(PartialEq)]
enum Page {
    Home,
    Host,
    Join,
    Test,
}

/// The picker's pictures: the background worker plus the egui textures made from what it produced.
struct ThumbStore {
    worker: Thumbs,
    tex: HashMap<Key, (egui::TextureHandle, u64)>,
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
        let seen: HashMap<Key, u64> = self.tex.iter().map(|(k, (_, v))| (*k, *v)).collect();
        for (k, p) in self.worker.news(&seen) {
            let img = egui::ColorImage::from_rgba_unmultiplied([p.w, p.h], &p.rgba);
            match self.tex.get_mut(&k) {
                Some((t, v)) => {
                    t.set(img, egui::TextureOptions::LINEAR);
                    *v = p.version;
                }
                None => {
                    let t = ctx.load_texture(format!("th{k:?}"), img, egui::TextureOptions::LINEAR);
                    self.tex.insert(k, (t, p.version));
                }
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
    /// Host option: short room code instead of two long codes (beta).
    easy: bool,
    input: String,
    copied_at: Option<Instant>,
    start_error: Option<String>,
}

impl GuiApp {
    fn new() -> GuiApp {
        let mons = capture::list_monitors().unwrap_or_default();
        let monitor = mons.iter().position(|m| m.primary).unwrap_or(0);
        let monitors = mons
            .iter()
            .map(|m| {
                format!("{}×{} · {}{}", m.width, m.height, m.adapter, if m.primary { " · основной" } else { "" })
            })
            .collect();
        GuiApp {
            page: Page::Home,
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
                    s.status = "Ищу друга по короткому коду...".into();
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

impl eframe::App for GuiApp {
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
        let busy = self.host.as_ref().is_some_and(|s| s.active())
            || self.join.as_ref().is_some_and(|s| s.active())
            || self.test.is_some();
        if busy {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(28, 24)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.page {
                    Page::Home => self.home(ui),
                    Page::Host => self.host_page(ui),
                    Page::Join => self.join_page(ui),
                    Page::Test => self.test_page(ui),
                });
            });
    }
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
    let mut color = fill;
    if resp.is_pointer_button_down_on() {
        color = fill.gamma_multiply(0.82);
    } else if resp.hovered() {
        color = fill.lerp_to_gamma(Color32::WHITE, 0.10);
    }
    let p = ui.painter();
    p.rect_filled(rect, 18.0, color);
    let c = rect.left_center() + Vec2::new(52.0, 0.0);
    p.circle_filled(c, 32.0, Color32::from_white_alpha(34));
    draw_ico(p, ico, c, 34.0, Color32::WHITE);
    p.text(rect.left_center() + Vec2::new(104.0, -15.0), Align2::LEFT_CENTER, title, bold(24.0), Color32::WHITE);
    p.text(rect.left_center() + Vec2::new(104.0, 17.0), Align2::LEFT_CENTER, subtitle, FontId::proportional(14.0), Color32::from_white_alpha(205));
    resp.clicked()
}

fn big_button(ui: &mut egui::Ui, text: &str, fill: Color32, enabled: bool) -> bool {
    big_button_i(ui, None, text, fill, enabled)
}

fn big_button_i(ui: &mut egui::Ui, ico: Option<Ico>, text: &str, fill: Color32, enabled: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), sense);
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    let mut color = fill;
    if !enabled {
        color = fill.gamma_multiply(0.35);
    } else if resp.is_pointer_button_down_on() {
        color = fill.gamma_multiply(0.8);
    } else if resp.hovered() {
        color = fill.lerp_to_gamma(Color32::WHITE, 0.12);
    }
    let p = ui.painter();
    p.rect_filled(rect, 14.0, color);
    let text_color = if enabled { Color32::WHITE } else { Color32::from_white_alpha(110) };
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
    ui.add(egui::Label::new(RichText::new("‹  Назад").color(MUTED).size(16.0)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn title(ui: &mut egui::Ui, t: &str, sub: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(t).font(bold(28.0)));
    ui.label(RichText::new(sub).color(MUTED));
    ui.add_space(12.0);
}

fn section(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD)
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
    egui::CollapsingHeader::new(RichText::new(name).font(bold(15.0)).color(MUTED)).default_open(false).show(ui, |ui| {
        add(ui);
    });
}

fn step(ui: &mut egui::Ui, n: u32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 12.0, ACCENT);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, n.to_string(), bold(13.0), Color32::WHITE);
        ui.label(RichText::new(text).font(bold(16.0)));
    });
}

fn level_color(level: netq::Level) -> Color32 {
    match level {
        netq::Level::Good => GREEN,
        netq::Level::Weak => YELLOW,
        netq::Level::Bad => RED,
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
        None => (MUTED, "Проверяю сеть…".to_string(), "Пока нет данных".to_string(), Ico::Wifi),
    };
    let r = ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 23.0, color.gamma_multiply(0.28));
        ui.painter().circle_stroke(r.center(), 23.0, Stroke::new(2.0, color));
        draw_ico(ui.painter(), ico, r.center(), 22.0, color);
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
        draw_ico(ui.painter(), Ico::Warn, r.center() + Vec2::new(0.0, 3.0), 15.0, RED);
        ui.add(egui::Label::new(RichText::new(text).color(RED)).wrap());
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
    ui.label(RichText::new(text).color(TEXT));
}

fn code_box(ui: &mut egui::Ui, code: &str, copied_at: &mut Option<Instant>) {
    egui::Frame::new().fill(FIELD).corner_radius(12).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(RichText::new(code).monospace().size(16.0).color(TEXT)).wrap());
    });
    let copied = copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    let label = if copied { "Скопировано" } else { "Скопировать код" };
    if big_button_i(ui, copied.then_some(Ico::Check), label, if copied { GREEN } else { ACCENT }, true) {
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
        step(ui, 1, "Отправь другу этот код");
        match &s.code {
            Some(c) => code_box(ui, c, copied_at),
            None => spinner_line(ui, &s.status),
        }
    });
    if s.code.is_some() {
        section(ui, |ui| {
            step(ui, 2, "Вставь код ответа от друга");
            code_input(ui, input, "C...");
            if let Some(e) = &s.bad_code {
                error_text(ui, e);
            }
            let sent = s.status.starts_with("Соединяюсь");
            if sent && s.bad_code.is_none() {
                spinner_line(ui, "Соединяюсь...");
            } else if big_button(ui, "Подключить", ACCENT, !input.trim().is_empty()) {
                s.bad_code = None;
                s.proc.send(input.trim());
            }
        });
    }
}

/// The short room code, large, with a copy button.
fn code_box_big(ui: &mut egui::Ui, code: &str, copied_at: &mut Option<Instant>) {
    egui::Frame::new().fill(FIELD).corner_radius(12).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(code).monospace().size(34.0).color(TEXT));
        });
    });
    let copied = copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    if big_button_i(ui, copied.then_some(Ico::Check), if copied { "Скопировано" } else { "Скопировать код" }, if copied { GREEN } else { ACCENT }, true) {
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
            .color(MUTED)
            .size(12.5),
    );
}

fn error_text(ui: &mut egui::Ui, e: &str) {
    ui.label(RichText::new(e).color(RED));
}

fn spinner_line(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
        ui.label(RichText::new(text).color(MUTED));
    });
}

/// The animated connection screen: a spinner, the current step in words and the whole path as a list.
fn connect_card(ui: &mut egui::Ui, s: &Session) {
    let cur = lg::conn_step(s.is_host, &s.status, s.code.is_some(), s.connected, !s.stats.is_empty());
    section(ui, |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(44.0).color(ACCENT));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(cur.label()).font(bold(22.0)));
                ui.label(RichText::new(cur.hint(s.is_host, s.room.is_some())).color(MUTED));
            });
        });
        ui.add_space(6.0);
        for st in STEPS {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                let p = ui.painter();
                if st < cur || (st == cur && st == Step::Picture) {
                    p.circle_filled(r.center(), 10.0, GREEN);
                    draw_ico(p, Ico::Check, r.center(), 12.0, BG);
                } else if st == cur {
                    p.circle_filled(r.center(), 10.0, ACCENT.gamma_multiply(0.35));
                    let t = ui.input(|i| i.time) as f32;
                    let pulse = 0.5 + 0.5 * (t * 4.0).sin();
                    p.circle_filled(r.center(), 4.0 + 2.5 * pulse, ACCENT);
                } else {
                    p.circle_stroke(r.center(), 9.5, Stroke::new(1.5, MUTED.gamma_multiply(0.7)));
                }
                let color = if st <= cur { TEXT } else { MUTED };
                let font = if st == cur { bold(15.0) } else { FontId::proportional(15.0) };
                ui.label(RichText::new(st.label()).font(font).color(color));
            });
        }
        if !s.status.is_empty() && !s.connected {
            ui.add_space(2.0);
            ui.label(RichText::new(&s.status).color(MUTED).size(12.5));
        }
        if !s.connected && s.started.elapsed() > Duration::from_secs(45) {
            ui.label(
                RichText::new("Идёт долго. Друг должен держать свою программу открытой; файрвол может мешать.")
                    .color(YELLOW)
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
            ui.painter().circle_filled(r.center(), 23.0, RED.gamma_multiply(0.28));
            ui.painter().circle_stroke(r.center(), 23.0, Stroke::new(2.0, RED));
            draw_ico(ui.painter(), Ico::Cross, r.center(), 22.0, RED);
            ui.add(egui::Label::new(RichText::new(title).font(bold(20.0))).wrap());
        });
        ui.label(RichText::new(hint).color(TEXT));
        if let Some(raw) = raw {
            fold(ui, "Подробности", |ui| {
                ui.add(egui::Label::new(RichText::new(raw).color(MUTED).size(12.5)).wrap());
            });
        }
    });
    let mut act = None;
    if big_button_i(ui, Some(Ico::Retry), retry_label, ACCENT, true) {
        act = Some(ErrAction::Retry);
    }
    if big_button_i(ui, Some(Ico::Home), "На главную", CARD_HOVER, true) {
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
    let fill = if selected {
        Color32::from_rgb(0x1f, 0x2c, 0x55)
    } else if resp.hovered() {
        CARD_HOVER
    } else {
        CARD
    };
    paint.rect_filled(rect, 14.0, fill);
    if selected {
        paint.rect_stroke(rect, 14.0, Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);
    }
    let th = egui::Rect::from_min_size(rect.min + Vec2::splat(pad), Vec2::new(w - 2.0 * pad, th_h));
    paint.rect_filled(th, 8.0, FIELD);
    let icon_tex = p.hwnd.and_then(|h| ts.icons.get(&h)).and_then(|t| t.as_ref());
    match ts.tex.get(&p.key) {
        Some((t, _)) => {
            // Letterbox inside the 16:9 area, keeping the window's own aspect.
            let tv = t.size_vec2();
            let k = (th.width() / tv.x).min(th.height() / tv.y);
            let r = egui::Rect::from_center_size(th.center(), tv * k);
            paint.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => match (icon_tex, p.hwnd) {
            (Some(t), _) => {
                let r = egui::Rect::from_center_size(th.center(), Vec2::splat(40.0));
                paint.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            (None, Some(_)) => draw_ico(paint, Ico::Window, th.center(), 34.0, MUTED),
            (None, None) => draw_ico(paint, Ico::Monitor, th.center(), 38.0, MUTED),
        },
    }
    if p.minimized {
        paint.text(th.center_bottom() - Vec2::new(0.0, 6.0), Align2::CENTER_BOTTOM, "свёрнуто", FontId::proportional(12.0), MUTED);
    }
    if selected {
        let c = th.right_top() + Vec2::new(-15.0, 15.0);
        paint.circle_filled(c, 11.0, ACCENT);
        draw_ico(paint, Ico::Check, c, 13.0, Color32::WHITE);
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
        (None, Some(_)) => draw_ico(paint, Ico::Window, egui::pos2(x + 9.0, y), 15.0, MUTED),
        (None, None) => draw_ico(paint, Ico::Monitor, egui::pos2(x + 9.0, y), 16.0, MUTED),
    }
    x += 26.0;
    let title = if p.sub.is_empty() { p.title.clone() } else { format!("{} · {}", p.title, p.sub) };
    let mut job = egui::text::LayoutJob::simple_singleline(title, FontId::proportional(14.0), if selected { Color32::WHITE } else { TEXT });
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - pad - x).max(10.0));
    let galley = ui.painter().layout_job(job);
    let gh = galley.size().y;
    ui.painter().galley(egui::pos2(x, y - gh / 2.0), galley, TEXT);
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
                RichText::new("Друг увидит только это окно. Если окно закроется — покажу весь экран.").color(MUTED).size(13.0),
            );
        }
    }

    /// "Дополнительно": quality, audio, the short-code option. Defaults need no touching.
    fn advanced(&mut self, ui: &mut egui::Ui, with_audio: bool, with_easy: bool) {
        ui.label(RichText::new("Качество").font(bold(15.0)));
        ui.horizontal_wrapped(|ui| {
            for (name, v) in [("Авто", 0), ("Стандарт · 30 Мбит/с", 30), ("Высокое качество · 60 Мбит/с", 60)] {
                let on = self.bitrate == v;
                let b = egui::Button::new(RichText::new(name).color(if on { Color32::WHITE } else { TEXT }))
                    .fill(if on { ACCENT } else { FIELD })
                    .corner_radius(10.0)
                    .min_size(egui::vec2(0.0, 30.0));
                if ui.add(b).clicked() {
                    self.bitrate = v;
                }
            }
        });
        ui.label(
            RichText::new(
                "«Авто» в начале сама подбирает скорость: стартует с 20 Мбит/с и поднимает до 60, пока нет потерь. \
                 «Стандарт» и «Высокое» — потолок: если интернет не успевает, программа сама снижает качество, а потом возвращает.",
            )
            .color(MUTED)
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
                ui.checkbox(&mut self.audio_prefs.exclude_voice, "Не передавать звук Discord").on_hover_text("Иначе друг услышит сам себя эхом");
            }
        }
        if with_easy {
            ui.add_space(6.0);
            ui.label(RichText::new("Подключение").font(bold(15.0)));
            ui.checkbox(&mut self.easy, "Простое подключение (beta): один короткий код вместо двух длинных");
            if self.easy {
                ui.label(
                    RichText::new("Коды обмениваются через публичный сервер ntfy.sh в зашифрованном виде. Обычные коды тоже останутся — на случай, если не сработает.")
                        .color(MUTED)
                        .size(12.5),
                );
            }
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
            ui.painter().rect_filled(r, 15.0, ACCENT);
            draw_ico(ui.painter(), Ico::Monitor, r.center(), 28.0, Color32::WHITE);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("screenshare").font(bold(32.0)));
                ui.label(RichText::new("Экран другу — напрямую, без серверов").color(MUTED).size(15.0));
            });
        });
        ui.add_space(26.0);
        if tile_button(ui, Ico::Monitor, "Показать экран", "Друг увидит экран и услышит звук", ACCENT) {
            self.windows = wincap::list_windows();
            self.thumbs.last_list = Instant::now();
            self.page = Page::Host;
        }
        ui.add_space(4.0);
        if tile_button(ui, Ico::Eye, "Смотреть", "Понадобится код друга", Color32::from_rgb(0x2b, 0x2f, 0x3f)) {
            self.page = Page::Join;
        }
        ui.add_space(18.0);
        fold(ui, "Дополнительно", |ui| {
            if big_button_i(ui, Some(Ico::Play), "Проверить на этом ПК", CARD_HOVER, true) {
                self.windows = wincap::list_windows();
                self.thumbs.last_list = Instant::now();
                self.page = Page::Test;
            }
            ui.label(RichText::new("Увидишь то же самое, что увидит друг. Качество, звук и порт настраиваются на странице показа.").color(MUTED).size(13.0));
            ui.label(RichText::new(format!("версия {}", env!("CARGO_PKG_VERSION"))).color(MUTED).size(12.0));
        });
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
            if live { "Друг видит твой экран" } else { "Выбери, что показать другу, и нажми «Начать»" },
        );

        let Some(s) = &mut self.host else {
            section(ui, |ui| {
                ui.label(RichText::new("Что показывать").font(bold(15.0)));
                self.pick_source(ui);
            });
            if let Some(e) = &self.start_error {
                error_text(ui, e);
            }
            if big_button_i(ui, Some(Ico::Play), "Начать показ", ACCENT, !self.monitors.is_empty()) {
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
                    ui.label(RichText::new(if s.stats.is_empty() { "собираю статистику..." } else { &s.stats }).color(MUTED));
                    if !s.encoder.is_empty() {
                        ui.label(RichText::new(&s.encoder).color(TEXT));
                    }
                    if !s.audio.is_empty() {
                        ui.label(RichText::new(&s.audio).color(TEXT));
                    }
                    if !s.source.is_empty() && !s.source_warn {
                        ui.label(RichText::new(&s.source).color(TEXT));
                    }
                    fps_hint(ui);
                });
            });
            // Switching without stopping: the same cards as at the start.
            let mut picked = None;
            if !self.monitors.is_empty() {
                section(ui, |ui| {
                    let shown = if s.preview_label.is_empty() { self.choice_label.clone() } else { s.preview_label.clone() };
                    egui::CollapsingHeader::new(RichText::new("Сменить окно или экран").font(bold(16.0))).default_open(false).show(ui, |ui| {
                        let mut windows = std::mem::take(&mut self.windows);
                        picked = source_picker(ui, &mut self.thumbs, &mut windows, &self.monitors, &self.rects, self.primary, self.choice, 280.0);
                        self.windows = windows;
                        ui.label(RichText::new("Можно менять, не останавливая показ.").color(MUTED).size(12.5));
                    });
                    ui.label(RichText::new(format!("Сейчас: {shown}")).color(MUTED).size(13.0));
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
                    ui.checkbox(&mut self.audio_prefs.exclude_voice, "Не передавать звук Discord").on_hover_text("Иначе друг услышит сам себя эхом");
                });
                if self.audio_prefs != before {
                    s.proc.send(&self.audio_prefs.line());
                }
            }
            if big_button_i(ui, Some(Ico::Stop), "Остановить", RED, true) {
                self.go_home();
                return;
            }
            if let Some(s) = &self.host {
                preview_block(ui, &mut self.show_preview, s);
            }
            return;
        }
        if s.ended {
            let (was, raw) = (s.connected, s.error.clone());
            if was && raw.is_none() {
                section(ui, |ui| {
                    ui.label(RichText::new("Трансляция завершена").font(bold(20.0)));
                    ui.label(RichText::new(&s.status).color(MUTED));
                });
                if big_button_i(ui, Some(Ico::Retry), "Показать снова", ACCENT, true) {
                    let ctx = ui.ctx().clone();
                    self.start_host(&ctx);
                } else if big_button_i(ui, Some(Ico::Home), "На главную", CARD_HOVER, true) {
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
                step(ui, 1, "Назови другу этот код");
                code_box_big(ui, &room, &mut self.copied_at);
                ui.label(
                    RichText::new("Друг вводит его в «Смотреть» — больше ничего пересылать не нужно. Код живёт 15 минут.")
                        .color(MUTED)
                        .size(13.0),
                );
            });
            connect_card(ui, s);
            if s.code.is_some() {
                egui::CollapsingHeader::new(RichText::new("Не подключается? Обычный способ с длинными кодами").color(MUTED)).show(ui, |ui| {
                    classic_host_codes(ui, s, &mut self.input, &mut self.copied_at);
                });
            }
        } else {
            classic_host_codes(ui, s, &mut self.input, &mut self.copied_at);
            connect_card(ui, s);
        }
        if big_button_i(ui, Some(Ico::Cross), "Отмена", CARD_HOVER, true) {
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
        title(ui, "Смотреть", if live { "Ты смотришь экран друга" } else { "Попроси друга нажать «Показать экран» и прислать код" });

        if let Some(s) = &mut self.join {
            if s.connected && !s.ended {
                if s.stats.is_empty() && s.connected_at.is_some_and(|t| t.elapsed() < Duration::from_secs(6)) {
                    connect_card(ui, s);
                    return;
                }
                section(ui, |ui| {
                    status_head(ui, s, "Идёт просмотр");
                    ui.label(RichText::new("Окно просмотра открыто. F11 или двойной клик — полный экран.").color(MUTED).size(14.0));
                    fold(ui, "Подробности", |ui| {
                        ui.label(RichText::new(if s.stats.is_empty() { "собираю статистику..." } else { &s.stats }).color(MUTED));
                        if !s.diag.is_empty() {
                            ui.label(RichText::new(&s.diag).color(MUTED).size(12.0));
                        }
                        ui.label(RichText::new("Журнал: %LOCALAPPDATA%\\screenshare\\viewer.log").color(MUTED).size(12.0));
                    });
                });
                if big_button_i(ui, Some(Ico::Stop), "Отключиться", RED, true) {
                    self.go_home();
                }
                return;
            }
            if s.ended {
                let (was, raw) = (s.connected, s.error.clone());
                if was && raw.is_none() {
                    section(ui, |ui| {
                        ui.label(RichText::new("Просмотр завершён").font(bold(20.0)));
                        ui.label(RichText::new(&s.status).color(MUTED));
                    });
                    if big_button_i(ui, Some(Ico::Retry), "Подключиться снова", ACCENT, true) {
                        let ctx = ui.ctx().clone();
                        self.start_join(&ctx);
                    } else if big_button_i(ui, Some(Ico::Home), "На главную", CARD_HOVER, true) {
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
                    RichText::new("Друг должен держать окно «Показать экран» открытым. Если не выходит дольше минуты — отмени и используй обычный длинный код.")
                        .color(MUTED)
                        .size(13.0),
                );
            });
            connect_card(ui, s);
            if big_button_i(ui, Some(Ico::Cross), "Отмена", CARD_HOVER, true) {
                self.go_home();
            }
            return;
        }
        let have_reply = self.join.as_ref().is_some_and(|s| s.code.is_some());
        if !have_reply {
            section(ui, |ui| {
                step(ui, 1, "Введи код друга");
                code_input(ui, &mut self.input, "короткий код (K7QM-X2PD, beta) или длинный H...");
                if let Some(e) = self.join.as_ref().and_then(|s| s.bad_code.as_ref()) {
                    error_text(ui, e);
                }
                if let Some(e) = &self.start_error {
                    error_text(ui, e);
                }
                if self.join.as_ref().is_some_and(|s| s.bad_code.is_none()) {
                    spinner_line(ui, "Проверяю код...");
                } else if big_button_i(ui, Some(Ico::Eye), "Смотреть", ACCENT, !self.input.trim().is_empty()) {
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
                step(ui, 2, "Отправь другу код ответа");
                code_box(ui, s.code.as_ref().unwrap(), &mut self.copied_at);
            });
        }
        if let Some(s) = &self.join {
            // A bad code is just a typo: stay on the input, no connection card for it.
            if have_reply || s.bad_code.is_none() {
                connect_card(ui, s);
            }
            if big_button_i(ui, Some(Ico::Cross), "Отмена", CARD_HOVER, true) {
                self.go_home();
            }
        }
    }

    fn test_page(&mut self, ui: &mut egui::Ui) {
        if self.test.is_none() && back_link(ui) {
            self.go_home();
            return;
        }
        title(ui, "Проверить на этом ПК", "Трансляция и просмотр на одном компьютере — как увидит друг");

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
                    .color(MUTED)
                    .size(13.0),
                );
            });
            if let Some(e) = &self.start_error {
                error_text(ui, e);
            }
            if big_button_i(ui, Some(Ico::Play), "Запустить проверку", ACCENT, !self.monitors.is_empty()) {
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
                (RED, lg::friendly_error(e).0.to_string())
            } else if s.ended {
                (MUTED, "остановлено".to_string())
            } else if s.connected && !s.stats.is_empty() {
                match &s.net {
                    Some((l, t, _)) => (level_color(*l), t.clone()),
                    None => (GREEN, "работает".to_string()),
                }
            } else {
                (ACCENT, lg::conn_step(s.is_host, &s.status, s.code.is_some(), s.connected, !s.stats.is_empty()).label().to_string())
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
                    ui.label(RichText::new(if s.stats.is_empty() { &s.status } else { &s.stats }).color(MUTED));
                }
                if !c.diag.is_empty() {
                    ui.label(RichText::new(&c.diag).color(MUTED).size(12.0));
                }
                fps_hint(ui);
            });
        });
        if h.connected && c.connected {
            ui.label(RichText::new("Окно просмотра открыто отдельно. F11 или двойной клик — полный экран.").color(MUTED));
        }
        if big_button_i(ui, Some(Ico::Stop), "Остановить", RED, true) {
            self.go_home();
            return;
        }
        if let Some((h, _, _, _)) = &self.test {
            preview_block(ui, &mut self.show_preview, h);
        }
    }
}
