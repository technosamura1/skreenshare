// Launcher window (egui). Streaming itself runs in child processes (`host --ipc` / `client --ipc`),
// which keeps the fast viewer path untouched and lets the GUI stop them cleanly.
use crate::capture;
use crate::easy;
use crate::preview;
use crate::source::{self, Choice, WinEntry};
use crate::wincap;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Sense, Stroke, Vec2,
};
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
    status: String,
    code: Option<String>,
    connected: bool,
    /// Short room code of the simple pairing (beta), when this session uses it.
    room: Option<String>,
    stats: String,
    /// Viewer diagnostics (`@DIAG`): decoder type, timings, queue, jitter.
    diag: String,
    audio: String,
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
            proc,
            status: "Запускаю...".into(),
            code: None,
            connected: false,
            room: None,
            stats: String::new(),
            diag: String::new(),
            audio: String::new(),
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

struct GuiApp {
    page: Page,
    monitors: Vec<String>,
    /// Index of the primary monitor ("Весь экран"; also the canvas size when one window is shown).
    primary: usize,
    /// "Что показывать": the choice, its text (kept in case the window vanished from the list) and the windows.
    choice: Choice,
    choice_label: String,
    windows: Vec<WinEntry>,
    bitrate: u32,
    audio: bool,
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
            primary: monitor,
            choice: Choice::Screen,
            choice_label: "Весь экран".into(),
            windows: wincap::list_windows(),
            bitrate: 15,
            audio: true,
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
        a.extend(["--bitrate".to_string(), self.bitrate.to_string()]);
        if !self.audio {
            a.push("--no-audio".into());
        }
        a
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

// ---------------------------------------------------------------- widgets

fn card_button(ui: &mut egui::Ui, title: &str, subtitle: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 82.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter();
    p.rect_filled(rect, 14.0, if resp.hovered() { CARD_HOVER } else { CARD });
    p.text(rect.left_top() + Vec2::new(22.0, 18.0), Align2::LEFT_TOP, title, bold(19.0), TEXT);
    p.text(rect.left_top() + Vec2::new(22.0, 48.0), Align2::LEFT_TOP, subtitle, FontId::proportional(14.0), MUTED);
    p.text(rect.right_center() - Vec2::new(22.0, 2.0), Align2::RIGHT_CENTER, "›", FontId::proportional(30.0), MUTED);
    resp.clicked()
}

fn big_button(ui: &mut egui::Ui, text: &str, fill: Color32, enabled: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), sense);
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
    p.rect_filled(rect, 10.0, color);
    let text_color = if enabled { Color32::WHITE } else { Color32::from_white_alpha(110) };
    p.text(rect.center(), Align2::CENTER_CENTER, text, bold(16.0), text_color);
    enabled && resp.clicked()
}

fn back_link(ui: &mut egui::Ui) -> bool {
    ui.add(egui::Label::new(RichText::new("‹  Назад").color(MUTED).size(15.0)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn title(ui: &mut egui::Ui, t: &str, sub: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(t).font(bold(26.0)));
    ui.label(RichText::new(sub).color(MUTED));
    ui.add_space(12.0);
}

fn section(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(14)
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(4.0);
}

fn step(ui: &mut egui::Ui, n: u32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 12.0, ACCENT);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, n.to_string(), bold(13.0), Color32::WHITE);
        ui.label(RichText::new(text).font(bold(16.0)));
    });
}

fn status_dot(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 6.0, color);
        ui.label(RichText::new(text).font(bold(17.0)));
    });
}

fn code_box(ui: &mut egui::Ui, code: &str, copied_at: &mut Option<Instant>) {
    egui::Frame::new().fill(FIELD).corner_radius(10).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(RichText::new(code).monospace().size(16.0).color(TEXT)).wrap());
    });
    let copied = copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    let label = if copied { "Скопировано ✓" } else { "Скопировать код" };
    if big_button(ui, label, if copied { GREEN } else { ACCENT }, true) {
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
    egui::Frame::new().fill(FIELD).corner_radius(10).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(code).monospace().size(34.0).color(TEXT));
        });
    });
    let copied = copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    if big_button(ui, if copied { "Скопировано ✓" } else { "Скопировать код" }, if copied { GREEN } else { ACCENT }, true) {
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

impl GuiApp {
    fn settings(&mut self, ui: &mut egui::Ui, with_audio: bool) {
        section(ui, |ui| {
            ui.label(RichText::new("Что показывать").font(bold(15.0)));
            if self.monitors.is_empty() {
                error_text(ui, "Мониторы не найдены");
            } else {
                let items = source::build_menu(&self.monitors, &self.windows);
                let sel = items.iter().find(|i| i.choice == self.choice).map_or_else(|| self.choice_label.clone(), |i| i.label.clone());
                let r = egui::ComboBox::from_id_salt("source").width(ui.available_width()).selected_text(sel).show_ui(ui, |ui| {
                    for it in &items {
                        if ui.selectable_label(self.choice == it.choice, &it.label).clicked() {
                            self.choice = it.choice;
                            self.choice_label = it.label.clone();
                        }
                    }
                });
                if r.response.clicked() {
                    // The list of windows is refreshed every time the dropdown is opened.
                    self.windows = wincap::list_windows();
                    ui.ctx().request_repaint();
                }
                if matches!(self.choice, Choice::Window(_)) {
                    ui.label(
                        RichText::new(
                            "Друг увидит только это окно (картинка вписывается в кадр с чёрными полосами). \
                             Если окно закроется — покажу весь экран.",
                        )
                        .color(MUTED)
                        .size(13.0),
                    );
                }
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Качество").font(bold(15.0)));
            ui.horizontal(|ui| {
                for (name, v) in [("Для друга по интернету · 15", 15), ("Хороший интернет · 40", 40), ("Одна сеть · 60", 60)] {
                    if ui.selectable_label(self.bitrate == v, name).clicked() {
                        self.bitrate = v;
                    }
                }
            });
            ui.add(egui::Slider::new(&mut self.bitrate, 8..=80).suffix(" Мбит/с"));
            ui.label(
                RichText::new(
                    "Это потолок: программа сама снижает качество, если у друга не успевает интернет. \
                     Если у друга лагает — поставь 10–15. 40 и больше — только когда у обоих быстрый интернет.",
                )
                .color(MUTED)
                .size(13.0),
            );
            if with_audio {
                ui.add_space(8.0);
                ui.checkbox(&mut self.audio, "Передавать звук компьютера");
            }
        });
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.label(RichText::new("screenshare").font(bold(34.0)));
        ui.label(RichText::new("Экран другу — напрямую, без серверов и регистрации").color(MUTED).size(15.0));
        ui.add_space(26.0);
        if card_button(ui, "Показать мой экран", "Весь экран или одно окно — и звук компьютера") {
            self.windows = wincap::list_windows();
            self.page = Page::Host;
        }
        if card_button(ui, "Смотреть экран друга", "Понадобится код, который пришлёт друг") {
            self.page = Page::Join;
        }
        if card_button(ui, "Проверить на этом ПК", "Увидишь то же самое, что увидит друг") {
            self.windows = wincap::list_windows();
            self.page = Page::Test;
        }
        ui.add_space(18.0);
        ui.label(RichText::new(format!("версия {}", env!("CARGO_PKG_VERSION"))).color(MUTED).size(12.0));
    }

    fn host_page(&mut self, ui: &mut egui::Ui) {
        let running = self.host.as_ref().is_some_and(|s| s.active());
        if !running && back_link(ui) {
            self.go_home();
            return;
        }
        title(ui, "Показать мой экран", "Друг подключится по коду — без настроек роутера");

        let Some(s) = &mut self.host else {
            self.settings(ui, true);
            if let Some(e) = &self.start_error {
                error_text(ui, e);
            }
            ui.checkbox(&mut self.easy, "Простое подключение (beta): один короткий код вместо двух длинных");
            if self.easy {
                ui.label(
                    RichText::new("Коды обмениваются через публичный сервер ntfy.sh в зашифрованном виде. Обычные коды тоже останутся — на случай, если не сработает.")
                        .color(MUTED)
                        .size(12.5),
                );
            }
            if big_button(ui, "Начать", ACCENT, !self.monitors.is_empty()) {
                let mut args = self.host_args("host");
                let room = self.easy.then(easy::new_room_code);
                if let Some(r) = &room {
                    args.extend(["--room".to_string(), r.clone()]);
                }
                match Session::start(ui.ctx(), args) {
                    Ok(mut s) => {
                        s.room = room;
                        self.host = Some(s);
                    }
                    Err(e) => self.start_error = Some(e),
                }
            }
            return;
        };

        if s.connected && !s.ended {
            // The preview is made only while this window is visible and the switch is on.
            drive_preview(ui.ctx(), self.show_preview, s);
            let s = &*s;
            section(ui, |ui| {
                status_dot(ui, GREEN, "Идёт трансляция");
                ui.label(RichText::new(if s.stats.is_empty() { "собираю статистику..." } else { &s.stats }).color(MUTED));
                if !s.encoder.is_empty() {
                    ui.label(RichText::new(&s.encoder).color(TEXT));
                }
                if !s.enc_warn.is_empty() {
                    ui.label(RichText::new(format!("Видеокарта не используется: {}", s.enc_warn)).color(RED));
                }
                ui.add_space(6.0);
                if !s.source.is_empty() && s.source_warn {
                    ui.label(RichText::new(&s.source).color(if s.source_warn { RED } else { TEXT }));
                }
                if !s.audio.is_empty() {
                    let warn = s.audio.starts_with("Звук выключен") || s.audio.starts_with("Звук не");
                    ui.label(RichText::new(&s.audio).color(if warn { RED } else { TEXT }));
                }
                fps_hint(ui);
            });
            if big_button(ui, "Остановить", RED, true) {
                self.go_home();
                return;
            }
            if let Some(s) = &self.host {
                preview_block(ui, &mut self.show_preview, s);
            }
            return;
        }
        if s.ended {
            section(ui, |ui| {
                status_dot(ui, if s.error.is_some() { RED } else { MUTED }, "Трансляция завершена");
                if let Some(e) = &s.error {
                    error_text(ui, e);
                } else {
                    ui.label(RichText::new(&s.status).color(MUTED));
                }
            });
            if big_button(ui, "На главную", CARD_HOVER, true) {
                self.go_home();
            }
            return;
        }

        if let Some(room) = s.room.clone() {
            section(ui, |ui| {
                step(ui, 1, "Назови другу этот код");
                code_box_big(ui, &room, &mut self.copied_at);
                ui.label(
                    RichText::new("Друг вводит его в «Смотреть экран друга» — больше ничего пересылать не нужно. Код живёт 15 минут.")
                        .color(MUTED)
                        .size(13.0),
                );
                spinner_line(ui, &s.status);
            });
            if s.code.is_some() {
                egui::CollapsingHeader::new("Не подключается? Обычный способ с длинными кодами").show(ui, |ui| {
                    classic_host_codes(ui, s, &mut self.input, &mut self.copied_at);
                });
            }
        } else {
            classic_host_codes(ui, s, &mut self.input, &mut self.copied_at);
        }
        if big_button(ui, "Отмена", CARD_HOVER, true) {
            self.go_home();
        }
    }

    fn join_page(&mut self, ui: &mut egui::Ui) {
        let running = self.join.as_ref().is_some_and(|s| s.active());
        if !running && back_link(ui) {
            self.go_home();
            return;
        }
        title(ui, "Смотреть экран друга", "Попроси друга нажать «Показать мой экран» и прислать код");

        if let Some(s) = &mut self.join {
            if s.connected && !s.ended {
                section(ui, |ui| {
                    status_dot(ui, GREEN, "Подключено");
                    ui.label("Окно просмотра открыто. F11 или двойной клик — переключить полный экран.");
                    ui.label(RichText::new(if s.stats.is_empty() { "собираю статистику..." } else { &s.stats }).color(MUTED));
                    if !s.diag.is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new(&s.diag).color(MUTED).size(12.0));
                        ui.label(RichText::new("Журнал: %LOCALAPPDATA%\\screenshare\\viewer.log").color(MUTED).size(12.0));
                    }
                });
                if big_button(ui, "Отключиться", RED, true) {
                    self.go_home();
                }
                return;
            }
            if s.ended {
                section(ui, |ui| {
                    status_dot(ui, if s.error.is_some() { RED } else { MUTED }, "Отключено");
                    match &s.error {
                        Some(e) => error_text(ui, e),
                        None => {
                            ui.label(RichText::new(&s.status).color(MUTED));
                        }
                    }
                });
                if big_button(ui, "На главную", CARD_HOVER, true) {
                    self.go_home();
                }
                return;
            }
        }

        if let Some(s) = self.join.as_ref().filter(|s| s.room.is_some()) {
            section(ui, |ui| {
                step(ui, 1, &format!("Код {} — подключаюсь к другу", s.room.as_deref().unwrap_or("")));
                spinner_line(ui, if s.code.is_some() { "Нашёл друга, соединяюсь..." } else { &s.status });
                ui.label(
                    RichText::new("Друг должен держать окно «Показать мой экран» открытым. Если не выходит дольше минуты — отмени и используй обычный длинный код.")
                        .color(MUTED)
                        .size(13.0),
                );
            });
            if big_button(ui, "Отмена", CARD_HOVER, true) {
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
                ui.checkbox(&mut self.fullscreen, "Открыть на весь экран");
                if self.join.as_ref().is_some_and(|s| s.bad_code.is_none()) {
                    spinner_line(ui, "Проверяю код...");
                } else if big_button(ui, "Подключиться", ACCENT, !self.input.trim().is_empty()) {
                    let code = self.input.trim().to_string();
                    if let Some(s) = &mut self.join {
                        s.bad_code = None;
                        s.proc.send(&code);
                    } else {
                        let mut args = vec!["client".to_string()];
                        if !self.fullscreen {
                            args.push("--windowed".into());
                        }
                        // A short room code needs no pasting at all: the client fetches the offer itself.
                        let room = easy::normalize(&code);
                        if let Some(r) = &room {
                            args.extend(["--room".to_string(), r.clone()]);
                        }
                        match Session::start(ui.ctx(), args) {
                            Ok(mut s) => {
                                if room.is_some() {
                                    s.room = room;
                                    s.status = "Ищу друга по короткому коду...".into();
                                } else {
                                    s.proc.send(&code);
                                }
                                self.join = Some(s);
                            }
                            Err(e) => self.start_error = Some(e),
                        }
                    }
                }
            });
        } else {
            let s = self.join.as_mut().unwrap();
            section(ui, |ui| {
                step(ui, 2, "Отправь другу код ответа");
                code_box(ui, s.code.as_ref().unwrap(), &mut self.copied_at);
                spinner_line(ui, "Жду, пока друг вставит этот код...");
            });
        }
        if self.join.is_some() && big_button(ui, "Отмена", CARD_HOVER, true) {
            self.go_home();
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
            self.settings(ui, false);
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
            if big_button(ui, "Запустить проверку", ACCENT, !self.monitors.is_empty()) {
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
            return;
        };

        let line = |ui: &mut egui::Ui, name: &str, s: &Session| {
            let (color, text) = if let Some(e) = &s.error {
                (RED, e.clone())
            } else if s.ended {
                (MUTED, "остановлено".to_string())
            } else if s.connected {
                (GREEN, if s.stats.is_empty() { "работает".to_string() } else { s.stats.clone() })
            } else {
                (ACCENT, s.status.clone())
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 6.0, color);
                ui.label(RichText::new(name).font(bold(15.0)));
            });
            ui.label(RichText::new(text).color(MUTED));
        };
        section(ui, |ui| {
            line(ui, "Трансляция", h);
            ui.add_space(6.0);
            line(ui, "Просмотр", c);
            if !c.diag.is_empty() {
                ui.label(RichText::new(&c.diag).color(MUTED).size(12.0));
            }
            fps_hint(ui);
        });
        if h.connected && c.connected {
            ui.label(RichText::new("Окно просмотра открыто отдельно. F11 или двойной клик — полный экран.").color(MUTED));
        }
        if big_button(ui, "Остановить", RED, true) {
            self.go_home();
            return;
        }
        if let Some((h, _, _, _)) = &self.test {
            preview_block(ui, &mut self.show_preview, h);
        }
    }
}
