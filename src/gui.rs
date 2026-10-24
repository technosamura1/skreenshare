// The only window: one big "Транслировать" button. Everything else is under "Дополнительно".
use crate::capture::{self, MonitorInfo, Source};
use crate::discovery::{Device, Discovery};
use crate::stream::{Phase, Settings, Stream};
use crate::wincap::{self, WindowInfo};
use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, RichText, Stroke, Vec2};
use std::net::Ipv4Addr;
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

struct Preset {
    name: &'static str,
    w: u32,
    h: u32,
    fps: u32,
}

/// The first one is the default: Full HD at 60 frames. The bitrate adapts to the Wi-Fi on its own.
const PRESETS: &[Preset] = &[
    Preset { name: "Full HD · 60 к/с (по умолчанию)", w: 1920, h: 1080, fps: 60 },
    Preset { name: "Full HD · 30 к/с — запасной вариант, если лагает", w: 1920, h: 1080, fps: 30 },
    Preset { name: "HD 720p · 60 к/с", w: 1280, h: 720, fps: 60 },
    Preset { name: "HD 720p · 30 к/с — самый лёгкий", w: 1280, h: 720, fps: 30 },
];

/// Default bitrate: where 1080p60 starts; with auto-adjust it is also the ceiling. Selectable up to 80.
const DEFAULT_MAX_MBPS: u32 = 30;

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Skreenshare проектор")
            .with_inner_size([520.0, 560.0])
            .with_min_inner_size([460.0, 480.0]),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Skreenshare проектор",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new()))
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
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
        v.widgets.active.weak_bg_fill = ACCENT;
        s.spacing.item_spacing = Vec2::new(10.0, 10.0);
        s.spacing.button_padding = Vec2::new(14.0, 8.0);
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

struct App {
    discovery: Discovery,
    monitors: Vec<MonitorInfo>,
    selected: Option<Ipv4Addr>,
    preset: usize,
    /// "Что показывать": the whole screen (default), a monitor, or one window.
    source: Source,
    windows: Vec<WindowInfo>,
    windows_at: Instant,
    audio: bool,
    max_mbps: u32,
    adaptive: bool,
    av_offset_ms: i32,
    manual_ip: String,
    manual_err: Option<String>,
    stream: Option<Stream>,
    /// Last status of the finished stream (so the error stays on screen).
    last: Option<crate::stream::Status>,
}

impl App {
    fn new() -> App {
        let monitors = capture::list_monitors().unwrap_or_default();
        App {
            discovery: Discovery::start(),
            monitors,
            selected: None,
            preset: 0,
            source: Source::Monitor(None),
            windows: Vec::new(),
            windows_at: Instant::now(),
            audio: true,
            max_mbps: DEFAULT_MAX_MBPS,
            adaptive: true,
            av_offset_ms: 0,
            manual_ip: String::new(),
            manual_err: None,
            stream: None,
            last: None,
        }
    }

    fn start(&mut self, ctx: &egui::Context, dev: &Device) {
        let p = &PRESETS[self.preset];
        let settings = Settings {
            source: self.source.clone(),
            fps: p.fps,
            bitrate_mbps: self.max_mbps,
            adaptive: self.adaptive,
            av_offset_ms: self.av_offset_ms,
            max_w: p.w,
            max_h: p.h,
            audio: self.audio,
        };
        let limit = (dev.hello.max_w as u32, dev.hello.max_h as u32, dev.hello.max_fps as u32);
        let ctx2 = ctx.clone();
        self.last = None;
        self.stream = Some(Stream::start(dev.addr(), limit, settings, move || ctx2.request_repaint()));
    }
}

impl App {
    /// "Что показывать": whole screen (default) / each monitor / each open window. The window list is read
    /// again every time the menu is opened (and once a second while it stays open).
    fn source_picker(&mut self, ui: &mut egui::Ui, streaming: bool) {
        let monitor_name = |m: &MonitorInfo, i: usize| format!("Экран {} · {}×{}{}", i + 1, m.width, m.height, if m.primary { " (основной)" } else { "" });
        let selected = match &self.source {
            Source::Monitor(None) => "Весь экран".to_string(),
            Source::Monitor(Some(i)) => self.monitors.get(*i).map(|m| monitor_name(m, *i)).unwrap_or_else(|| "Весь экран".into()),
            Source::Window { title, .. } => format!("Окно: {}", proto::geom::ellipsize(title, 48)),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Что показывать").font(bold(14.0)));
            ui.add_enabled_ui(!streaming, |ui| {
                let resp = egui::ComboBox::from_id_salt("source").width(ui.available_width()).selected_text(selected).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.source, Source::Monitor(None), "Весь экран");
                    if self.monitors.len() > 1 {
                        for (i, m) in self.monitors.iter().enumerate() {
                            ui.selectable_value(&mut self.source, Source::Monitor(Some(i)), monitor_name(m, i));
                        }
                    }
                    ui.separator();
                    if self.windows.is_empty() {
                        ui.label(RichText::new("Нет открытых окон").color(MUTED));
                    }
                    for w in &self.windows {
                        let label = format!("{}{}", proto::geom::ellipsize(&w.title, 60), if w.minimized { " (свёрнуто)" } else { "" });
                        let value = Source::Window { hwnd: w.hwnd, title: w.title.clone() };
                        // The same window is the same choice even if its title changed since it was picked.
                        let sel = matches!(&self.source, Source::Window { hwnd, .. } if *hwnd == w.hwnd);
                        if ui.selectable_label(sel, label).clicked() {
                            self.source = value;
                        }
                    }
                });
                let open = resp.inner.is_some();
                let clicked = resp.response.clicked();
                if clicked || (open && self.windows_at.elapsed() >= Duration::from_secs(1)) {
                    self.windows = wincap::list_windows();
                    self.windows_at = Instant::now();
                }
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        ctx.request_repaint_after(Duration::from_millis(500)); // device list refresh
        let devices = self.discovery.devices();
        if self.selected.is_none() || !devices.iter().any(|d| Some(d.ip) == self.selected) && self.stream.is_none() {
            self.selected = devices.first().map(|d| d.ip); // one projector around: nothing to choose
        }
        if self.stream.as_ref().is_some_and(|s| s.is_finished()) {
            self.last = self.stream.as_ref().map(|s| s.status());
            self.stream = None;
        }
        let streaming = self.stream.is_some();

        egui::CentralPanel::no_frame().frame(egui::Frame::new().fill(BG).inner_margin(20)).show(ui, |ui| {
            ui.label(RichText::new("Skreenshare проектор").font(bold(24.0)));
            ui.label(RichText::new("Экран и звук компьютера — на проектор по Wi-Fi, без кодов").color(MUTED));
            ui.add_space(6.0);

            let chosen = devices.iter().find(|d| Some(d.ip) == self.selected).cloned();

            // 0 projectors: say what to do. 1: just show it (nothing to choose). 2+: pick one (first is preselected).
            if devices.is_empty() {
                egui::Frame::new().fill(CARD).corner_radius(10).inner_margin(14).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Ищу проектор... Откройте приложение «Skreenshare проектор» на проекторе.");
                    });
                });
            }
            for d in &devices {
                let sel = Some(d.ip) == self.selected;
                let many = devices.len() > 1;
                let fill = if sel && many { Color32::from_rgb(0x22, 0x2c, 0x4d) } else { CARD };
                let resp = egui::Frame::new()
                    .fill(fill)
                    .corner_radius(10)
                    .stroke(Stroke::new(1.0, if sel && many { ACCENT } else { Color32::TRANSPARENT }))
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("●").color(GREEN));
                            ui.label(RichText::new(&d.hello.name).font(bold(16.0)));
                            if d.hello.busy {
                                ui.label(RichText::new("уже принимает").color(MUTED));
                            }
                        });
                        ui.label(
                            RichText::new(format!("{} · до {}×{}, {} к/с", d.ip, d.hello.max_w, d.hello.max_h, d.hello.max_fps))
                                .color(MUTED)
                                .size(13.0),
                        );
                    })
                    .response
                    .interact(egui::Sense::click());
                if resp.clicked() && !streaming && many {
                    self.selected = Some(d.ip);
                }
            }

            ui.add_space(8.0);
            self.source_picker(ui, streaming);
            if streaming {
                let b = egui::Button::new(RichText::new("Остановить").font(bold(26.0)).color(Color32::WHITE))
                    .fill(RED)
                    .min_size(Vec2::new(ui.available_width(), 96.0));
                if ui.add(b).clicked() {
                    if let Some(s) = &self.stream {
                        s.stop();
                    }
                }
            } else {
                let b = egui::Button::new(RichText::new("Транслировать").font(bold(26.0)).color(Color32::WHITE))
                    .fill(ACCENT)
                    .min_size(Vec2::new(ui.available_width(), 96.0));
                if ui.add_enabled(chosen.is_some(), b).clicked() {
                    if let Some(d) = &chosen {
                        self.start(ctx, d);
                    }
                }
            }

            ui.add_space(4.0);
            let st = self.stream.as_ref().map(|s| s.status()).or_else(|| self.last.clone());
            if let Some(st) = st {
                if let Some(e) = &st.error {
                    ui.label(RichText::new(e).color(RED));
                } else {
                    let color = if st.phase == Phase::Streaming { GREEN } else { TEXT };
                    ui.label(RichText::new(&st.text).color(color).font(bold(15.0)));
                }
                if !st.stats.is_empty() && st.phase == Phase::Streaming {
                    ui.label(RichText::new(&st.stats).color(MUTED).size(13.0));
                }
                if !st.notice.is_empty() {
                    ui.label(RichText::new(&st.notice).color(Color32::from_rgb(0xf2, 0xb8, 0x4b)).size(14.0));
                }
                if !st.audio.is_empty() {
                    ui.label(RichText::new(&st.audio).color(MUTED).size(13.0));
                }
            }
            ui.add_space(6.0);
            ui.collapsing(RichText::new("Дополнительно").font(bold(15.0)), |ui| {
                ui.add_enabled_ui(!streaming, |ui| {
                    ui.label(RichText::new("Качество").font(bold(14.0)));
                    egui::ComboBox::from_id_salt("preset").width(ui.available_width()).selected_text(PRESETS[self.preset].name).show_ui(
                        ui,
                        |ui| {
                            for (i, p) in PRESETS.iter().enumerate() {
                                ui.selectable_value(&mut self.preset, i, p.name);
                            }
                        },
                    );
                    ui.checkbox(&mut self.adaptive, "Подстраивать битрейт под Wi-Fi автоматически (рекомендуется)");
                    ui.horizontal(|ui| {
                        ui.add(egui::Slider::new(&mut self.max_mbps, 6..=80).suffix(" Мбит/с"));
                        ui.label(if self.adaptive { "старт и максимум" } else { "постоянный битрейт" });
                    });
                    ui.checkbox(&mut self.audio, "Передавать звук компьютера");
                    ui.label(RichText::new("Звук и картинка не совпадают? Сдвиг звука (мс, «+» — звук позже):").size(13.0).color(MUTED));
                    ui.add(egui::Slider::new(&mut self.av_offset_ms, -300..=300).suffix(" мс"));
                    if ui.small_button("Сбросить настройки").clicked() {
                        self.preset = 0;
                        self.source = Source::Monitor(None);
                        self.max_mbps = DEFAULT_MAX_MBPS;
                        self.adaptive = true;
                        self.audio = true;
                        self.av_offset_ms = 0;
                    }
                });
                ui.label(RichText::new("Не находит проектор? Введите его IP (он написан на экране проектора):").size(13.0).color(MUTED));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.manual_ip).hint_text("192.168.1.50").desired_width(180.0));
                    if ui.button("Добавить").clicked() {
                        match self.manual_ip.trim().parse::<Ipv4Addr>() {
                            Ok(ip) => {
                                self.discovery.add_manual(ip);
                                self.manual_err = None;
                            }
                            Err(_) => self.manual_err = Some("Это не похоже на IP-адрес".into()),
                        }
                    }
                });
                if let Some(e) = &self.manual_err {
                    ui.label(RichText::new(e).color(RED));
                }
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new("При первом запуске Windows спросит про доступ к сети — нажмите «Разрешить», иначе проектор не найдётся. Для лучшей картинки: ПК — кабелем к роутеру, проектор — поближе к роутеру.")
                    .color(MUTED)
                    .size(12.0),
            );
        });
    }
}
