//! RustRig GUI — 獨立電吉他即時效果處理 app。
//!
//! 視覺對標 AudioSFX 海報：黑底、紫→洋紅霓虹、LED 旋鈕、發光播放鍵。
//! 音訊引擎跑在獨立執行緒（rustrig-audio），GUI 與 RT 之間全部走
//! lock-free（SharedParam / MeterHandle / atomic 計數器），互不阻塞。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod widgets;

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Stroke};
use rustrig_audio::{AudioBackend, LatencyInfo, RunningStream, StreamConfig, WasapiShared};
use rustrig_dsp::{Chain, Gain, MeterHandle, PeakMeter, SharedParam};
use widgets as w;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([470.0, 790.0])
            .with_min_inner_size([440.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RustRig",
        options,
        Box::new(|cc| Ok(Box::new(RigApp::new(cc)))),
    )
}

/// 佔位旋鈕（之後 P2/P3 點亮成真效果）。
struct GhostKnob {
    label: &'static str,
    accent: Color32,
    value: f32,
}

struct RigApp {
    stream: Option<Box<dyn RunningStream>>,
    latency: Option<LatencyInfo>,
    error: Option<String>,

    /// 線性音量（1.0 = 0 dB），GUI 端鏡像；真值在 SharedParam
    vol_lin: f32,
    volume: SharedParam,
    meter: MeterHandle,

    /// 峰值表顯示值（dB，含下落 ballistics）
    disp_db: f32,
    clip_until: Option<Instant>,

    ghosts: Vec<GhostKnob>,
}

impl RigApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx);
        Self {
            stream: None,
            latency: None,
            error: None,
            vol_lin: 1.0,
            volume: SharedParam::new(1.0),
            meter: MeterHandle::new(),
            disp_db: -80.0,
            clip_until: None,
            ghosts: vec![
                GhostKnob { label: "GATE", accent: w::PINK, value: 0.3 },
                GhostKnob { label: "DRIVE", accent: w::AMBER, value: 0.5 },
                GhostKnob { label: "TONE", accent: w::CYAN, value: 0.6 },
                GhostKnob { label: "REVERB", accent: w::GREEN, value: 0.25 },
            ],
        }
    }

    fn start(&mut self) {
        let mut chain = Chain::new();
        chain.push(Box::new(Gain::new(self.volume.clone())));
        chain.push(Box::new(PeakMeter::new(self.meter.clone())));
        match WasapiShared::open(StreamConfig::default())
            .and_then(|b| b.run(Box::new(chain)))
        {
            Ok(s) => {
                self.latency = Some(s.latency());
                self.stream = Some(s);
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn stop(&mut self) {
        self.stream = None; // drop = 停止串流、join 音訊執行緒
        self.latency = None;
        self.disp_db = -80.0;
    }

    fn running(&self) -> bool {
        self.stream.is_some()
    }
}

impl eframe::App for RigApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 引擎活性監看：執行緒死掉（拔裝置等）要立即反映，不能默默裝沒事
        if let Some(s) = &self.stream {
            if !s.is_alive() {
                self.error = Some("音訊執行緒已停止（裝置被拔除或驅動錯誤）".into());
                self.stop();
            }
        }

        // 峰值表 ballistics：瞬間上衝、30 dB/s 下落
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        if self.running() {
            let peak = self.meter.take_peak();
            let peak_db = if peak > 1e-5 { 20.0 * peak.log10() } else { -80.0 };
            self.disp_db = (self.disp_db - 30.0 * dt).max(peak_db);
            if peak >= 0.999 {
                self.clip_until = Some(Instant::now() + Duration::from_secs(1));
            }
        }
        let clip = self.clip_until.is_some_and(|t| Instant::now() < t);

        ctx.request_repaint_after(Duration::from_millis(if self.running() { 33 } else { 250 }));

        egui::Frame::new()
            .fill(w::BG)
            .inner_margin(Margin::same(22))
            .show(ui, |ui| {
                // ── 標題 ──
                ui.vertical_centered(|ui| {
                    let mut job = egui::text::LayoutJob::default();
                    let f = FontId::proportional(40.0);
                    job.append("Rust", 0.0, egui::TextFormat {
                        font_id: f.clone(),
                        color: w::PURPLE,
                        ..Default::default()
                    });
                    job.append("Rig", 0.0, egui::TextFormat {
                        font_id: f,
                        color: w::MAGENTA,
                        ..Default::default()
                    });
                    ui.label(job);
                });
                w::divider_title(ui, "吉 他 即 時 效 果", 56.0);
                ui.add_space(14.0);

                // ── 狀態卡 ──
                status_card(ui, self);
                if let Some(err) = &self.error {
                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(format!("⚠ {err}")).color(w::RED).size(11.0));
                    });
                }
                ui.add_space(12.0);

                // ── 主面板：channel strip + 旋鈕區 ──
                panel_frame().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        if w::channel_strip(ui, &mut self.vol_lin, self.disp_db, clip) {
                            self.volume.set(self.vol_lin);
                        }
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.vertical(|ui| {
                            ui.add_space(18.0);
                            ui.horizontal(|ui| {
                                for g in &mut self.ghosts[..2] {
                                    w::knob(ui, g.label, &mut g.value, 0.0, 1.0, 0.5, g.accent, false, &|v| {
                                        format!("{:.0}%", v * 100.0)
                                    });
                                }
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                for g in &mut self.ghosts[2..] {
                                    w::knob(ui, g.label, &mut g.value, 0.0, 1.0, 0.5, g.accent, false, &|v| {
                                        format!("{:.0}%", v * 100.0)
                                    });
                                }
                            });
                            ui.add_space(4.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("效果鏈 P1-P3 點亮").color(w::FAINT).size(9.0));
                            });
                        });
                    });
                });
                ui.add_space(18.0);

                // ── 播放鍵 ──
                ui.vertical_centered(|ui| {
                    let resp = w::play_button(ui, self.running());
                    if resp.clicked() {
                        if self.running() {
                            self.stop();
                        } else {
                            self.start();
                        }
                    }
                    ui.add_space(6.0);
                    let (txt, col) = if self.running() {
                        ("直通中 — 彈彈看", w::MAGENTA)
                    } else {
                        ("點擊啟動引擎", w::DIM)
                    };
                    ui.label(RichText::new(txt).color(col).size(11.0));
                });

                // ── footer ──
                ui.add_space(10.0);
                w::divider_title(ui, "Isan · 13soul", 44.0);
            });
    }
}

fn panel_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(w::PANEL)
        .stroke(Stroke::new(1.0, w::PANEL_EDGE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
}

fn status_card(ui: &mut egui::Ui, app: &RigApp) {
    panel_frame().show(ui, |ui| {
        ui.horizontal(|ui| {
            let (dot, label, col) = if app.running() {
                ("●", "運轉中", w::GREEN)
            } else {
                ("●", "待機", w::FAINT)
            };
            ui.label(RichText::new(dot).color(col).size(12.0));
            ui.label(RichText::new(label).color(w::TEXT).size(12.0));
            ui.add_space(12.0);
            ui.label(RichText::new("WASAPI-Shared").color(w::DIM).size(11.0).monospace());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let stat = |v: String| RichText::new(v).color(w::TEXT).size(11.0).monospace();
                let cap = |v: &str| RichText::new(v.to_owned()).color(w::FAINT).size(10.0);
                match (&app.latency, app.stream.as_ref()) {
                    (Some(lat), Some(s)) => {
                        ui.label(stat(format!("{}", s.xrun_count())));
                        ui.label(cap("xrun"));
                        ui.add_space(8.0);
                        ui.label(stat(format!("{:.1}ms", lat.buffer_ms())));
                        ui.label(cap("延遲"));
                        ui.add_space(8.0);
                        ui.label(stat(format!("{}Hz", lat.sample_rate)));
                        ui.label(cap("取樣率"));
                    }
                    _ => {
                        ui.label(RichText::new("— Hz · — ms · xrun —").color(w::FAINT).size(11.0).monospace());
                    }
                }
            });
        });
    });
}

/// egui 內建字型沒有 CJK 字元，掛上微軟正黑體（系統內建）。
fn install_cjk_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read("C:/Windows/Fonts/msjh.ttc") {
        fonts.font_data.insert(
            "msjh".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().push("msjh".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = w::BG;
    v.window_fill = w::BG;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, w::PANEL_EDGE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, w::TEXT);
    v.selection.bg_fill = w::with_alpha(w::PURPLE, 80);
    ctx.set_visuals(v);
}
