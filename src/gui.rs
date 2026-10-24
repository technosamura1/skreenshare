// The only window: pick a projector, press "Транслировать".
use crate::capture::{self, MonitorInfo};
use crate::discovery::{Device, Discovery};
use crate::stream::{Phase, Settings, Stream};
use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, RichText, Stroke, Vec2};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

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
    mbps: u32,
}

const PRESETS: &[Preset] = &[
    Preset { name: "Full HD · 30 к/с · 10 Мбит/с — рекомендуется", w: 1920, h: 1080, fps: 30, mbps: 10 },
    Preset { name: "Full HD · 60 к/с · 15 Мбит/с — если проектор тянет", w: 1920, h: 1080, fps: 60, mbps: 15 },
    Preset { name: "HD 720p · 30 к/с · 6 Мбит/с — если лагает", w: 1280, h: 720, fps: 30, mbps: 6 },
    Preset { name: "HD 720p · 60 к/с · 8 Мбит/с", w: 1280, h: 720, fps: 60, mbps: 8 },
];

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Skreenshare проектор")
            .with_inner_size([560.0, 640.0])
            .with_min_inner_size([480.0, 560.0]),
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
    monitor: usize,
    audio: bool,
    manual_ip: String,
    manual_err: Option<String>,
    stream: Option<Stream>,
    /// Last status of the finished stream (so the error stays on screen).
    last: Option<crate::stream::Status>,
}

impl App {
    fn new() -> App {
        let monitors = capture::list_monitors().unwrap_or_default();
        let monitor = monitors.iter().position(|m| m.primary).unwrap_or(0);
        App {
            discovery: Discovery::start(),
            monitors,
            selected: None,
            preset: 0,
            monitor,
            audio: true,
            manual_ip: String::new(),
            manual_err: None,
            stream: None,
            last: None,
        }
    }

    fn start(&mut self, ctx: &egui::Context, dev: &Device) {
        let p = &PRESETS[self.preset];
        let settings = Settings {
            monitor: if self.monitors.is_empty() { None } else { Some(self.monitor) },
            fps: p.fps,
            bitrate_mbps: p.mbps,
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

            ui.label(RichText::new("Проекторы в сети").font(bold(16.0)));
            if devices.is_empty() {
                egui::Frame::new().fill(CARD).corner_radius(10).inner_margin(14).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Ищу проекторы... Откройте приложение «Skreenshare проектор» на проекторе.");
                    });
                });
            }
            for d in &devices {
                let sel = Some(d.ip) == self.selected;
                let fill = if sel { Color32::from_rgb(0x22, 0x2c, 0x4d) } else { CARD };
                let resp = egui::Frame::new()
                    .fill(fill)
                    .corner_radius(10)
                    .stroke(Stroke::new(1.0, if sel { ACCENT } else { Color32::TRANSPARENT }))
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&d.hello.name).font(bold(16.0)));
                            if d.hello.busy {
                                ui.label(RichText::new("● уже принимает").color(GREEN));
                            }
                        });
                        ui.label(
                            RichText::new(format!(
                                "{} · до {}×{}, {} к/с",
                                d.ip, d.hello.max_w, d.hello.max_h, d.hello.max_fps
                            ))
                            .color(MUTED)
                            .size(13.0),
                        );
                    })
                    .response
                    .interact(egui::Sense::click());
                if resp.clicked() && !streaming {
                    self.selected = Some(d.ip);
                }
            }
            ui.collapsing("Не находит проектор? Ввести IP вручную", |ui| {
                ui.label(RichText::new("IP написан на экране проектора в приложении").color(MUTED).size(13.0));
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
            ui.add_enabled_ui(!streaming, |ui| {
                ui.label(RichText::new("Качество").font(bold(16.0)));
                egui::ComboBox::from_id_salt("preset").width(ui.available_width()).selected_text(PRESETS[self.preset].name).show_ui(
                    ui,
                    |ui| {
                        for (i, p) in PRESETS.iter().enumerate() {
                            ui.selectable_value(&mut self.preset, i, p.name);
                        }
                    },
                );
                if self.monitors.len() > 1 {
                    ui.label(RichText::new("Какой экран показывать").font(bold(16.0)));
                    let name = |m: &MonitorInfo, i: usize| {
                        format!("Экран {} · {}×{}{}", i + 1, m.width, m.height, if m.primary { " (основной)" } else { "" })
                    };
                    egui::ComboBox::from_id_salt("monitor")
                        .width(ui.available_width())
                        .selected_text(name(&self.monitors[self.monitor.min(self.monitors.len() - 1)], self.monitor))
                        .show_ui(ui, |ui| {
                            for (i, m) in self.monitors.iter().enumerate() {
                                ui.selectable_value(&mut self.monitor, i, name(m, i));
                            }
                        });
                }
                ui.checkbox(&mut self.audio, "Передавать звук компьютера");
            });

            ui.add_space(8.0);
            let chosen = devices.iter().find(|d| Some(d.ip) == self.selected).cloned();
            if streaming {
                let b = egui::Button::new(RichText::new("Остановить").font(bold(18.0))).fill(RED).min_size(Vec2::new(ui.available_width(), 52.0));
                if ui.add(b).clicked() {
                    if let Some(s) = &self.stream {
                        s.stop();
                    }
                }
            } else {
                let b = egui::Button::new(RichText::new("Транслировать").font(bold(18.0)).color(Color32::WHITE))
                    .fill(ACCENT)
                    .min_size(Vec2::new(ui.available_width(), 52.0));
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
                if !st.audio.is_empty() {
                    ui.label(RichText::new(&st.audio).color(MUTED).size(13.0));
                }
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new("При первом запуске Windows спросит про доступ к сети — нажмите «Разрешить», иначе проектор не найдётся.")
                    .color(MUTED)
                    .size(12.0),
            );
        });
    }
}
