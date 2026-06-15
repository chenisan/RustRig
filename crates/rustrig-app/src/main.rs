//! RustRig GUI — 獨立電吉他即時效果處理 app。
//!
//! 視覺對標 AudioSFX 海報：黑底、紫→洋紅霓虹、LED 旋鈕、發光播放鍵。
//! 音訊引擎跑在獨立執行緒（rustrig-audio），GUI 與 RT 之間全部走
//! lock-free（SharedParam / MeterHandle / atomic 計數器），互不阻塞。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod widgets;

use std::time::{Duration, Instant};

use eframe::egui::{self, CornerRadius, FontId, Margin, RichText, Stroke};
use rustrig_audio::{
    BackendKind, DeviceLists, LatencyInfo, RunningStream, StreamConfig, open_stream,
};
use rustrig_dsp::{
    CabIr, Chain, Compressor, Delay, Drive, Gain, Gate, MeterHandle, Metronome, Nam, PeakMeter,
    PitchHandle, Reverb, SharedParam, Tuner,
};
use widgets as w;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 1085.0])
            .with_min_inner_size([450.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RustRig",
        options,
        Box::new(|cc| Ok(Box::new(RigApp::new(cc)))),
    )
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

    // ── 輸入增益（前級，鏈最前面）──
    input_db_v: f32,
    input_gain: SharedParam,

    // ── 調音器（輸入增益後、閘前；唯讀）──
    tuner_on: bool,
    tuner_on_p: SharedParam,
    pitch: PitchHandle,

    // ── 雜訊閘（破音前）──
    gate_v: f32, // 0..1 強度
    gate_amt: SharedParam,
    gate_on_p: SharedParam,
    gate_on: bool,

    // ── 壓縮（閘後、破音前）──
    comp_v: f32,       // 0..1 壓縮量
    makeup_db_v: f32,  // 補償 dB
    comp_amt: SharedParam,
    comp_makeup: SharedParam,
    comp_on_p: SharedParam,
    comp_on: bool,

    // ── 破音 ──
    drive_db_v: f32,
    tone_norm: f32, // 0..1 → 500..5000 Hz（對數）
    drive_db: SharedParam,
    tone_hz: SharedParam,
    drive_on_p: SharedParam,
    drive_on: bool,

    // ── NAM 擴大機（破音後、cab 前）──
    nam_on_p: SharedParam,
    nam_on: bool,
    /// .nam 檔內容（已驗證）；換模型需重建 chain
    nam_json: Option<String>,
    nam_name: Option<String>,
    /// 模型訓練取樣率（提示與引擎取樣率是否相符）
    nam_sr: Option<f64>,

    // ── IR cab ──
    cab_on_p: SharedParam,
    cab_on: bool,
    /// (樣本, 檔案取樣率)；換 IR 需重建 chain（運轉中會自動重啟引擎）
    ir: Option<(Vec<f32>, u32)>,
    ir_name: Option<String>,

    // ── 裝置選擇 ──
    devices: DeviceLists,
    /// None = 系統預設
    sel_capture: Option<String>,
    sel_render: Option<String>,
    /// 音訊後端（共享 / 獨佔 / ASIO）
    backend: BackendKind,
    /// 可用的 ASIO 驅動清單（未以 --features asio 編譯時為空）
    asio_drivers: Vec<String>,
    /// 選定的 ASIO 驅動（None = 第一個可用）
    sel_asio_driver: Option<String>,

    // ── 延遲（cab 後、殘響前）──
    delay_time_v: f32, // ms（手動模式）
    delay_fb_v: f32,   // 回授 0..0.95
    delay_mix_v: f32,  // 濕量 0..1
    delay_time_ms: SharedParam, // 實際餵 DSP 的 ms（手動 or BPM 換算）
    delay_fb: SharedParam,
    delay_mix: SharedParam,
    delay_on_p: SharedParam,
    delay_on: bool,
    delay_sync: bool,   // 開 → 延遲時間吃 BPM
    delay_div: NoteDiv, // 同步音符

    // ── 殘響（delay 後）──
    reverb_v: f32, // 0..1 濕量
    reverb_mix: SharedParam,
    reverb_on_p: SharedParam,
    reverb_on: bool,

    // ── 全域拍速（delay sync + 節拍器共用）──
    bpm_v: f32,
    bpm: SharedParam,
    tap_times: Vec<Instant>,

    // ── 節拍器（音量後）──
    metro_level_v: f32,
    metro_level: SharedParam,
    metro_on_p: SharedParam,
    metro_on: bool,

    // ── 開啟時的「關於 / 版權」視窗 ──
    show_about: bool,
    about_dont_show: bool,
}

/// ComboBox 顯示用：依選擇的 ID 找名稱。
fn device_label(list: &[rustrig_audio::DeviceInfo], sel: &Option<String>) -> String {
    match sel {
        None => "系統預設".into(),
        Some(id) => list
            .iter()
            .find(|d| &d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "（裝置已移除）".into()),
    }
}

/// 0..1 → 500..5000 Hz（一個 decade 的對數刻度，落在 cab 也聽得到的頻段）
fn tone_norm_to_hz(norm: f32) -> f32 {
    500.0 * 10f32.powf(norm.clamp(0.0, 1.0))
}

/// 基頻 Hz → (音名含八度, 與標準音差的 cents)。hz<=0 回 ("—", 0)。
fn hz_to_note(hz: f32) -> (String, f32) {
    if hz <= 0.0 {
        return ("—".into(), 0.0);
    }
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let midi = 69.0 + 12.0 * (hz / 440.0).log2();
    let nearest = midi.round();
    let cents = (midi - nearest) * 100.0;
    let idx = (((nearest as i32) % 12) + 12) % 12;
    let octave = (nearest as i32) / 12 - 1;
    (format!("{}{}", NAMES[idx as usize], octave), cents)
}

/// delay 同步音符。factor × 四分音符時值 = 延遲時間。
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
enum NoteDiv {
    Quarter,      // ♩
    DottedEighth, // ♪.
    Eighth,       // ♪
    Triplet,      // ♩3（四分三連音 = 2/3 四分）
}

impl NoteDiv {
    const ALL: [NoteDiv; 4] = [
        NoteDiv::Quarter,
        NoteDiv::DottedEighth,
        NoteDiv::Eighth,
        NoteDiv::Triplet,
    ];
    fn factor(self) -> f32 {
        match self {
            NoteDiv::Quarter => 1.0,
            NoteDiv::DottedEighth => 0.75,
            NoteDiv::Eighth => 0.5,
            NoteDiv::Triplet => 2.0 / 3.0,
        }
    }
    fn label(self) -> &'static str {
        match self {
            NoteDiv::Quarter => "♩ 1/4",
            NoteDiv::DottedEighth => "♪. 1/8.",
            NoteDiv::Eighth => "♪ 1/8",
            NoteDiv::Triplet => "♩3 1/4T",
        }
    }
}

/// 可存讀的效果鏈狀態（所有旋鈕 / 開關 / 拍速）。**不含** .nam / IR 檔，只存參數；
/// 載入後既有的 AMP/CAB 模型沿用，只套用開關狀態。
#[derive(serde::Serialize, serde::Deserialize)]
struct Preset {
    input_db: f32,
    volume: f32, // 線性
    gate_amt: f32,
    gate_on: bool,
    comp: f32,
    makeup_db: f32,
    comp_on: bool,
    drive_db: f32,
    tone_norm: f32,
    drive_on: bool,
    nam_on: bool,
    cab_on: bool,
    delay_time: f32,
    delay_fb: f32,
    delay_mix: f32,
    delay_on: bool,
    delay_sync: bool,
    delay_div: NoteDiv,
    reverb: f32,
    reverb_on: bool,
    bpm: f32,
    metro_level: f32,
    metro_on: bool,
}

#[inline]
fn b2f(b: bool) -> f32 {
    if b { 1.0 } else { 0.0 }
}

/// 預設目錄 %APPDATA%\RustRig\presets（取不到 APPDATA 時退回當前目錄）。
fn preset_dir() -> std::path::PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(|a| std::path::PathBuf::from(a).join("RustRig").join("presets"))
        .unwrap_or_else(|| std::path::PathBuf::from("presets"));
    let _ = std::fs::create_dir_all(&base);
    base
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
            input_db_v: 0.0,
            input_gain: SharedParam::new(1.0),
            tuner_on: false,
            tuner_on_p: SharedParam::new(0.0),
            pitch: PitchHandle::new(),
            gate_v: 0.0,
            gate_amt: SharedParam::new(0.0),
            gate_on_p: SharedParam::new(0.0),
            gate_on: false,
            comp_v: 0.4,
            makeup_db_v: 0.0,
            comp_amt: SharedParam::new(0.4),
            comp_makeup: SharedParam::new(0.0),
            comp_on_p: SharedParam::new(0.0),
            comp_on: false,
            drive_db_v: 18.0,
            tone_norm: 0.65,
            drive_db: SharedParam::new(18.0),
            tone_hz: SharedParam::new(tone_norm_to_hz(0.65)),
            drive_on_p: SharedParam::new(1.0),
            drive_on: true,
            nam_on_p: SharedParam::new(0.0),
            nam_on: false,
            nam_json: None,
            nam_name: None,
            nam_sr: None,
            cab_on_p: SharedParam::new(1.0),
            cab_on: true,
            ir: None,
            ir_name: None,
            devices: rustrig_audio::enumerate().unwrap_or_default(),
            sel_capture: None,
            sel_render: None,
            backend: BackendKind::WasapiShared,
            asio_drivers: rustrig_audio::asio_driver_names(),
            sel_asio_driver: None,
            delay_time_v: 350.0,
            delay_fb_v: 0.35,
            delay_mix_v: 0.3,
            delay_time_ms: SharedParam::new(350.0),
            delay_fb: SharedParam::new(0.35),
            delay_mix: SharedParam::new(0.3),
            delay_on_p: SharedParam::new(0.0),
            delay_on: false,
            delay_sync: false,
            delay_div: NoteDiv::Eighth,
            reverb_v: 0.3,
            reverb_mix: SharedParam::new(0.3),
            reverb_on_p: SharedParam::new(0.0),
            reverb_on: false,
            bpm_v: 120.0,
            bpm: SharedParam::new(120.0),
            tap_times: Vec::new(),
            metro_level_v: 0.6,
            metro_level: SharedParam::new(0.6),
            metro_on_p: SharedParam::new(0.0),
            metro_on: false,
            // 第一次開啟才顯示；勾過「不再顯示」後標記檔存在 → 不再彈
            show_about: !about_seen(),
            about_dont_show: true,
        }
    }

    fn start(&mut self) {
        let mut chain = Chain::new();
        // 訊號鏈：輸入增益 → 調音器 → 閘 → 壓縮 → 破音 → NAM 音箱 → cab → 延遲 → 殘響 → 音量 → 節拍器 → 峰值表
        chain.push(Box::new(Gain::new(self.input_gain.clone())));
        chain.push(Box::new(Tuner::new(
            self.tuner_on_p.clone(),
            self.pitch.clone(),
        )));
        chain.push(Box::new(Gate::new(
            self.gate_amt.clone(),
            self.gate_on_p.clone(),
        )));
        chain.push(Box::new(Compressor::new(
            self.comp_amt.clone(),
            self.comp_makeup.clone(),
            self.comp_on_p.clone(),
        )));
        chain.push(Box::new(Drive::new(
            self.drive_db.clone(),
            self.tone_hz.clone(),
            self.drive_on_p.clone(),
        )));
        if let Some(json) = &self.nam_json {
            chain.push(Box::new(Nam::new(json.clone(), self.nam_on_p.clone())));
        }
        if let Some((raw, sr)) = &self.ir {
            chain.push(Box::new(CabIr::new(raw.clone(), *sr, self.cab_on_p.clone())));
        }
        chain.push(Box::new(Delay::new(
            self.delay_time_ms.clone(),
            self.delay_fb.clone(),
            self.delay_mix.clone(),
            self.delay_on_p.clone(),
        )));
        chain.push(Box::new(Reverb::new(
            self.reverb_mix.clone(),
            self.reverb_on_p.clone(),
        )));
        chain.push(Box::new(Gain::new(self.volume.clone())));
        chain.push(Box::new(Metronome::new(
            self.bpm.clone(),
            self.metro_level.clone(),
            self.metro_on_p.clone(),
        )));
        chain.push(Box::new(PeakMeter::new(self.meter.clone())));
        // NAM 開啟 → 目標 48kHz（對上模型）；關閉 → 44.1kHz。僅 ASIO 後端會據此
        // 實際切換驅動取樣率；WASAPI 取樣率由 Windows 裝置設定決定，程式改不動。
        let config = StreamConfig {
            sample_rate: if self.nam_on && self.nam_json.is_some() {
                48_000
            } else {
                44_100
            },
            capture_id: self.sel_capture.clone(),
            render_id: self.sel_render.clone(),
            asio_driver: self.sel_asio_driver.clone(),
            ..Default::default()
        };
        match open_stream(self.backend, config, Box::new(chain)) {
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

    /// 換 IR / 重建 chain 用：運轉中就無縫重啟。
    fn restart_if_running(&mut self) {
        if self.running() {
            self.stop();
            self.start();
        }
    }

    /// 目前生效的延遲時間（ms）：同步時吃 BPM，否則用手動值。
    fn delay_effective_ms(&self) -> f32 {
        if self.delay_sync {
            let quarter = 60_000.0 / self.bpm_v.clamp(40.0, 240.0);
            (quarter * self.delay_div.factor()).clamp(1.0, 2000.0)
        } else {
            self.delay_time_v
        }
    }

    /// Tap tempo：用最近數次點擊的平均間隔估 BPM（間隔 >2s 視為重新數）。
    fn tap(&mut self) {
        let now = Instant::now();
        if let Some(&last) = self.tap_times.last()
            && now.duration_since(last).as_secs_f32() > 2.0
        {
            self.tap_times.clear();
        }
        self.tap_times.push(now);
        let n = self.tap_times.len();
        if n > 4 {
            self.tap_times.drain(0..n - 4);
        }
        if self.tap_times.len() >= 2 {
            let span = self
                .tap_times
                .last()
                .unwrap()
                .duration_since(self.tap_times[0])
                .as_secs_f32();
            let intervals = (self.tap_times.len() - 1) as f32;
            if span > 1e-3 {
                self.bpm_v = (60.0 * intervals / span).clamp(40.0, 240.0);
            }
        }
    }

    /// 調音器卡：LED 開關 + 大音名 + cents 指針條 + Hz/cents 讀數。
    fn tuner_card(&mut self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                if w::led_toggle(ui, "TUNER", self.tuner_on, w::CYAN).clicked() {
                    self.tuner_on = !self.tuner_on;
                    self.tuner_on_p.set(if self.tuner_on { 1.0 } else { 0.0 });
                }
                ui.add_space(8.0);

                let hz = if self.tuner_on { self.pitch.read() } else { 0.0 };
                let (name, cents) = hz_to_note(hz);
                let in_tune = hz > 0.0 && cents.abs() < 5.0;

                // 大音名
                let name_col = if hz <= 0.0 {
                    w::DIM
                } else if in_tune {
                    w::GREEN
                } else {
                    w::TEXT
                };
                ui.label(RichText::new(name).color(name_col).size(26.0).monospace());
                ui.add_space(8.0);

                // cents 指針條
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(150.0, 28.0), egui::Sense::hover());
                let painter = ui.painter().clone();
                let cy = rect.center().y;
                let left = rect.left() + 6.0;
                let right = rect.right() - 6.0;
                let cx = (left + right) / 2.0;
                let halfspan = (right - left) / 2.0;
                painter.line_segment(
                    [egui::pos2(left, cy), egui::pos2(right, cy)],
                    Stroke::new(1.0, w::FAINT),
                );
                for f in [-1.0f32, -0.5, 0.5, 1.0] {
                    let x = cx + f * halfspan;
                    painter.line_segment(
                        [egui::pos2(x, cy - 4.0), egui::pos2(x, cy + 4.0)],
                        Stroke::new(1.0, w::FAINT),
                    );
                }
                // 中央準點（綠）
                painter.line_segment(
                    [egui::pos2(cx, cy - 9.0), egui::pos2(cx, cy + 9.0)],
                    Stroke::new(1.5, w::GREEN),
                );
                // 指針
                if hz > 0.0 {
                    let frac = (cents / 50.0).clamp(-1.0, 1.0);
                    let x = cx + frac * halfspan;
                    let col = if in_tune { w::GREEN } else { w::AMBER };
                    painter.circle_filled(egui::pos2(x, cy - 9.0), 4.0, col);
                    painter.circle_filled(egui::pos2(x, cy - 9.0), 7.0, w::with_alpha(col, 60));
                }

                // 讀數
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if hz > 0.0 {
                        ui.label(
                            RichText::new(format!("{cents:+.0}¢"))
                                .color(if in_tune { w::GREEN } else { w::DIM })
                                .size(11.0)
                                .monospace(),
                        );
                        ui.label(
                            RichText::new(format!("{hz:.1} Hz"))
                                .color(w::DIM)
                                .size(11.0)
                                .monospace(),
                        );
                    } else {
                        ui.label(
                            RichText::new(if self.tuner_on { "彈一個音…" } else { "關閉" })
                                .color(w::FAINT)
                                .size(10.0),
                        );
                    }
                });
            });
        });
    }

    /// 節拍 / 練習卡：節拍器開關 + BPM + Tap + 音量。BPM 與 delay sync 共用。
    fn tempo_card(&mut self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                if w::led_toggle(ui, "節拍器", self.metro_on, w::VIOLET).clicked() {
                    self.metro_on = !self.metro_on;
                    self.metro_on_p.set(if self.metro_on { 1.0 } else { 0.0 });
                }
                ui.add_space(10.0);
                ui.label(RichText::new("BPM").color(w::DIM).size(11.0));
                ui.add(
                    egui::DragValue::new(&mut self.bpm_v)
                        .range(40.0..=240.0)
                        .speed(0.5)
                        .fixed_decimals(0),
                )
                .on_hover_text("拖曳或輸入拍速（delay 同步也用這個）");
                ui.add_space(8.0);
                if ui
                    .button(RichText::new("TAP").size(11.0).color(w::VIOLET))
                    .on_hover_text("照節奏連點 → 自動抓拍速")
                    .clicked()
                {
                    self.tap();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.metro_level_v, 0.0..=1.0)
                            .show_value(false),
                    )
                    .on_hover_text("節拍器音量");
                    ui.label(RichText::new("音量").color(w::DIM).size(10.0));
                });
            });
        });
    }

    /// 裝置選擇卡：輸入／輸出 ComboBox + 重新整理。換裝置即時生效。
    fn device_card(&mut self, ui: &mut egui::Ui) {
        let mut device_changed = false;
        panel_frame().show(ui, |ui| {
            let combo_w = ui.available_width() - 86.0;

            // ── 後端引擎（共享 / 獨佔 / ASIO）──
            ui.horizontal(|ui| {
                ui.label(RichText::new("引擎").color(w::DIM).size(10.5))
                    .on_hover_text("ASIO 最低延遲（需 ASIO 驅動）；獨佔次低但獨佔裝置；共享相容性最好但延遲高");
                let before = self.backend;
                egui::ComboBox::from_id_salt("backend")
                    .width(combo_w)
                    .selected_text(
                        RichText::new(self.backend.label()).size(10.5).color(w::TEXT),
                    )
                    .show_ui(ui, |ui| {
                        for k in BackendKind::ALL {
                            ui.selectable_value(&mut self.backend, k, k.label());
                        }
                    });
                if self.backend != before {
                    device_changed = true;
                }
            });

            if self.backend == BackendKind::Asio {
                // ── ASIO 驅動選擇（ASIO 自有清單，非 MMDevice）──
                ui.horizontal(|ui| {
                    ui.label(RichText::new("驅動").color(w::DIM).size(10.5));
                    let before = self.sel_asio_driver.clone();
                    let sel_text = self
                        .sel_asio_driver
                        .clone()
                        .unwrap_or_else(|| "第一個可用".into());
                    egui::ComboBox::from_id_salt("asio_driver")
                        .width(combo_w)
                        .selected_text(RichText::new(sel_text).size(10.5).color(w::TEXT))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.sel_asio_driver, None, "第一個可用");
                            for name in &self.asio_drivers {
                                ui.selectable_value(
                                    &mut self.sel_asio_driver,
                                    Some(name.clone()),
                                    name,
                                );
                            }
                        });
                    if self.sel_asio_driver != before {
                        device_changed = true;
                    }
                });
                if self.asio_drivers.is_empty() {
                    ui.label(
                        RichText::new("找不到 ASIO 驅動（需以 --features asio 編譯並安裝驅動）")
                            .color(w::FAINT)
                            .size(9.0),
                    );
                }
            } else {
                // ── WASAPI 輸入／輸出裝置 ──
                for (label, sel, list) in [
                    ("輸入", &mut self.sel_capture, &self.devices.capture),
                    ("輸出", &mut self.sel_render, &self.devices.render),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).color(w::DIM).size(10.5));
                        let before = sel.clone();
                        egui::ComboBox::from_id_salt(label)
                            .width(combo_w)
                            .selected_text(
                                RichText::new(device_label(list, sel)).size(10.5).color(w::TEXT),
                            )
                            .show_ui(ui, |ui| {
                                ui.selectable_value(sel, None, "系統預設");
                                for d in list {
                                    let name = if d.is_default {
                                        format!("{}（預設）", d.name)
                                    } else {
                                        d.name.clone()
                                    };
                                    ui.selectable_value(sel, Some(d.id.clone()), name);
                                }
                            });
                        if *sel != before {
                            device_changed = true;
                        }
                    });
                }
            }

            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("⟳ 重新整理").size(9.5))
                    .on_hover_text("重新掃描音訊裝置 / ASIO 驅動")
                    .clicked()
                {
                    match rustrig_audio::enumerate() {
                        Ok(d) => self.devices = d,
                        Err(e) => self.error = Some(format!("裝置列舉失敗：{e}")),
                    }
                    self.asio_drivers = rustrig_audio::asio_driver_names();
                }
                let hint = if self.backend == BackendKind::Asio {
                    "ASIO buffer 大小在驅動控制台設定"
                } else {
                    "輸入輸出建議用同一台介面（共用時鐘）"
                };
                ui.label(RichText::new(hint).color(w::FAINT).size(9.0));
            });
        });
        if device_changed {
            self.restart_if_running();
        }
    }

    /// 找預設 .nam：先看執行檔旁的 models/，再看當前目錄的 models/（dev）。取第一顆。
    fn find_default_nam(&self) -> Option<std::path::PathBuf> {
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(d) = exe.parent()
        {
            dirs.push(d.join("models"));
        }
        dirs.push(std::path::PathBuf::from("models"));
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("nam") {
                    return Some(p);
                }
            }
        }
        None
    }

    /// 自動載入預設 .nam（開 AMP 時用）。成功回 true。
    fn load_default_nam(&mut self) -> bool {
        let Some(path) = self.find_default_nam() else {
            return false;
        };
        let Ok(json) = std::fs::read_to_string(&path) else {
            return false;
        };
        match rustrig_dsp::nam::validate(&json) {
            Ok(info) => {
                self.nam_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                self.nam_sr = Some(info.sample_rate);
                self.nam_json = Some(json);
                self.nam_on = true;
                self.nam_on_p.set(1.0);
                self.error = None;
                self.restart_if_running();
                true
            }
            Err(_) => false,
        }
    }

    fn pick_nam(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("NAM 模型", &["nam"])
            .pick_file()
        else {
            return;
        };
        let json = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("NAM 讀檔失敗：{e}"));
                return;
            }
        };
        match rustrig_dsp::nam::validate(&json) {
            Ok(info) => {
                self.nam_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                self.nam_sr = Some(info.sample_rate);
                self.nam_json = Some(json);
                self.nam_on = true;
                self.nam_on_p.set(1.0);
                self.error = None;
                self.restart_if_running();
            }
            Err(e) => self.error = Some(format!("NAM 載入失敗：{e}")),
        }
    }

    fn pick_ir(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("脈衝響應 WAV", &["wav"])
            .pick_file()
        else {
            return;
        };
        match load_ir_wav(&path) {
            Ok((samples, sr)) => {
                self.ir = Some((samples, sr));
                self.ir_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                self.cab_on = true;
                self.cab_on_p.set(1.0);
                self.error = None;
                self.restart_if_running();
            }
            Err(e) => self.error = Some(format!("IR 載入失敗：{e}")),
        }
    }

    /// 蒐集目前狀態成 Preset。
    fn to_preset(&self) -> Preset {
        Preset {
            input_db: self.input_db_v,
            volume: self.vol_lin,
            gate_amt: self.gate_v,
            gate_on: self.gate_on,
            comp: self.comp_v,
            makeup_db: self.makeup_db_v,
            comp_on: self.comp_on,
            drive_db: self.drive_db_v,
            tone_norm: self.tone_norm,
            drive_on: self.drive_on,
            nam_on: self.nam_on,
            cab_on: self.cab_on,
            delay_time: self.delay_time_v,
            delay_fb: self.delay_fb_v,
            delay_mix: self.delay_mix_v,
            delay_on: self.delay_on,
            delay_sync: self.delay_sync,
            delay_div: self.delay_div,
            reverb: self.reverb_v,
            reverb_on: self.reverb_on,
            bpm: self.bpm_v,
            metro_level: self.metro_level_v,
            metro_on: self.metro_on,
        }
    }

    /// 套用 Preset：同步 mirror 欄位 + SharedParam，必要時無縫重啟引擎。
    fn apply_preset(&mut self, p: Preset) {
        self.input_db_v = p.input_db;
        self.input_gain.set(w::db_to_gain(p.input_db));
        self.vol_lin = p.volume;
        self.volume.set(p.volume);

        self.gate_v = p.gate_amt;
        self.gate_amt.set(p.gate_amt);
        self.gate_on = p.gate_on;
        self.gate_on_p.set(b2f(p.gate_on));

        self.comp_v = p.comp;
        self.comp_amt.set(p.comp);
        self.makeup_db_v = p.makeup_db;
        self.comp_makeup.set(p.makeup_db);
        self.comp_on = p.comp_on;
        self.comp_on_p.set(b2f(p.comp_on));

        self.drive_db_v = p.drive_db;
        self.drive_db.set(p.drive_db);
        self.tone_norm = p.tone_norm;
        self.tone_hz.set(tone_norm_to_hz(p.tone_norm));
        self.drive_on = p.drive_on;
        self.drive_on_p.set(b2f(p.drive_on));

        // AMP 只在已載模型時生效（取樣率切換靠重啟）
        self.nam_on = p.nam_on && self.nam_json.is_some();
        self.nam_on_p.set(b2f(self.nam_on));
        self.cab_on = p.cab_on;
        self.cab_on_p.set(b2f(p.cab_on));

        self.delay_time_v = p.delay_time;
        self.delay_fb_v = p.delay_fb;
        self.delay_fb.set(p.delay_fb);
        self.delay_mix_v = p.delay_mix;
        self.delay_mix.set(p.delay_mix);
        self.delay_on = p.delay_on;
        self.delay_on_p.set(b2f(p.delay_on));
        self.delay_sync = p.delay_sync;
        self.delay_div = p.delay_div;

        self.reverb_v = p.reverb;
        self.reverb_mix.set(p.reverb);
        self.reverb_on = p.reverb_on;
        self.reverb_on_p.set(b2f(p.reverb_on));

        self.bpm_v = p.bpm.clamp(40.0, 240.0);
        self.bpm.set(self.bpm_v);
        self.metro_level_v = p.metro_level;
        self.metro_level.set(p.metro_level);
        self.metro_on = p.metro_on;
        self.metro_on_p.set(b2f(p.metro_on));

        // 重建 chain（套用取樣率切換 + 確保開關結構一致）
        self.restart_if_running();
    }

    fn save_preset(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("RustRig 預設", &["json"])
            .set_directory(preset_dir())
            .set_file_name("preset.json")
            .save_file()
        else {
            return;
        };
        match serde_json::to_string_pretty(&self.to_preset()) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&path, s) {
                    self.error = Some(format!("預設儲存失敗：{e}"));
                } else {
                    self.error = None;
                }
            }
            Err(e) => self.error = Some(format!("預設序列化失敗：{e}")),
        }
    }

    fn load_preset(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("RustRig 預設", &["json"])
            .set_directory(preset_dir())
            .pick_file()
        else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Preset>(&s) {
                Ok(p) => {
                    self.apply_preset(p);
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("預設解析失敗：{e}")),
            },
            Err(e) => self.error = Some(format!("預設讀取失敗：{e}")),
        }
    }
}

/// 讀 IR wav：取第 0 聲道，int 格式正規化到 ±1.0。
fn load_ir_wav(path: &std::path::Path) -> Result<(Vec<f32>, u32), String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    let ch = spec.channels.max(1) as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .step_by(ch)
            .map(|s| s.unwrap_or(0.0))
            .collect(),
        hound::SampleFormat::Int => {
            let norm = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .step_by(ch)
                .map(|s| s.unwrap_or(0) as f32 * norm)
                .collect()
        }
    };
    if samples.is_empty() {
        return Err("檔案沒有樣本".into());
    }
    Ok((samples, spec.sample_rate))
}

impl eframe::App for RigApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 引擎活性監看：執行緒死掉（拔裝置等）要立即反映，不能默默裝沒事
        if let Some(s) = &self.stream
            && !s.is_alive()
        {
            self.error = Some("音訊執行緒已停止（裝置被拔除或驅動錯誤）".into());
            self.stop();
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

        // 每幀把拍速 / 生效延遲時間 / 節拍器音量推給 RT（sync 隨 BPM 即時連動）
        self.bpm.set(self.bpm_v);
        self.delay_time_ms.set(self.delay_effective_ms());
        self.metro_level.set(self.metro_level_v);

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
                ui.add_space(8.0);

                // ── 裝置選擇卡 ──
                self.device_card(ui);
                ui.add_space(8.0);

                // ── 調音器卡 ──
                self.tuner_card(ui);
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
                            ui.add_space(10.0);
                            // ── 第一排：輸入增益 + 破音雙旋鈕（live）──
                            ui.horizontal(|ui| {
                                if w::knob(ui, "INPUT", &mut self.input_db_v, -24.0, 24.0, 0.0, w::VIOLET, true, &|v| {
                                    format!("{v:+.0}dB")
                                }) {
                                    self.input_gain.set(w::db_to_gain(self.input_db_v));
                                }
                                if w::knob(ui, "DRIVE", &mut self.drive_db_v, 0.0, 40.0, 18.0, w::AMBER, true, &|v| {
                                    format!("{v:.0}dB")
                                }) {
                                    self.drive_db.set(self.drive_db_v);
                                }
                                if w::knob(ui, "TONE", &mut self.tone_norm, 0.0, 1.0, 0.65, w::CYAN, true, &|v| {
                                    let hz = tone_norm_to_hz(v);
                                    if hz >= 1000.0 { format!("{:.1}k", hz / 1000.0) } else { format!("{hz:.0}Hz") }
                                }) {
                                    self.tone_hz.set(tone_norm_to_hz(self.tone_norm));
                                }
                            });
                            // ── 第二排：閘 / 壓縮 / 補償（live）──
                            ui.horizontal(|ui| {
                                if w::knob(ui, "GATE", &mut self.gate_v, 0.0, 1.0, 0.0, w::PINK, true, &|v| {
                                    format!("{:.0}%", v * 100.0)
                                }) {
                                    self.gate_amt.set(self.gate_v);
                                }
                                if w::knob(ui, "COMP", &mut self.comp_v, 0.0, 1.0, 0.4, w::YELLOW, true, &|v| {
                                    format!("{:.0}%", v * 100.0)
                                }) {
                                    self.comp_amt.set(self.comp_v);
                                }
                                if w::knob(ui, "LEVEL", &mut self.makeup_db_v, 0.0, 18.0, 0.0, w::YELLOW, true, &|v| {
                                    format!("{v:+.0}dB")
                                }) {
                                    self.comp_makeup.set(self.makeup_db_v);
                                }
                            });
                            // ── 第三排：延遲（TIME 同步時 ghost）+ 殘響 ──
                            ui.horizontal(|ui| {
                                // TIME：同步開啟時改吃 BPM，旋鈕變 ghost；實際 ms 每幀推送
                                let _ = w::knob(ui, "DELAY", &mut self.delay_time_v, 40.0, 1000.0, 350.0, w::TEAL, !self.delay_sync, &|v| {
                                    if v >= 1000.0 { format!("{:.2}s", v / 1000.0) } else { format!("{v:.0}ms") }
                                });
                                if w::knob(ui, "FB", &mut self.delay_fb_v, 0.0, 0.95, 0.35, w::TEAL, true, &|v| {
                                    format!("{:.0}%", v * 100.0)
                                }) {
                                    self.delay_fb.set(self.delay_fb_v);
                                }
                                if w::knob(ui, "REVERB", &mut self.reverb_v, 0.0, 1.0, 0.3, w::GREEN, true, &|v| {
                                    format!("{:.0}%", v * 100.0)
                                }) {
                                    self.reverb_mix.set(self.reverb_v);
                                }
                            });
                            // ── 延遲同步列：MIX + SYNC + 音符 ──
                            ui.horizontal(|ui| {
                                ui.add_space(2.0);
                                ui.label(RichText::new("D.MIX").color(w::TEAL).size(9.5));
                                if ui
                                    .add(egui::Slider::new(&mut self.delay_mix_v, 0.0..=1.0).show_value(false))
                                    .on_hover_text("延遲濕量")
                                    .changed()
                                {
                                    self.delay_mix.set(self.delay_mix_v);
                                }
                                ui.add_space(6.0);
                                if w::led_toggle(ui, "SYNC", self.delay_sync, w::TEAL).clicked() {
                                    self.delay_sync = !self.delay_sync;
                                }
                                ui.add_enabled_ui(self.delay_sync, |ui| {
                                    egui::ComboBox::from_id_salt("delay_div")
                                        .width(72.0)
                                        .selected_text(RichText::new(self.delay_div.label()).size(10.0).color(w::TEXT))
                                        .show_ui(ui, |ui| {
                                            for d in NoteDiv::ALL {
                                                ui.selectable_value(&mut self.delay_div, d, d.label());
                                            }
                                        });
                                });
                                ui.label(RichText::new(format!("{:.0}ms", self.delay_effective_ms())).color(w::FAINT).size(9.5));
                            });
                            ui.add_space(6.0);
                            // ── 開關列（訊號順序：閘 → 破音 → 音箱 → cab → 殘響）──
                            ui.horizontal_wrapped(|ui| {
                                ui.add_space(2.0);
                                if w::led_toggle(ui, "GATE", self.gate_on, w::PINK).clicked() {
                                    self.gate_on = !self.gate_on;
                                    self.gate_on_p.set(if self.gate_on { 1.0 } else { 0.0 });
                                }
                                ui.add_space(4.0);
                                if w::led_toggle(ui, "COMP", self.comp_on, w::YELLOW).clicked() {
                                    self.comp_on = !self.comp_on;
                                    self.comp_on_p.set(if self.comp_on { 1.0 } else { 0.0 });
                                }
                                ui.add_space(4.0);
                                if w::led_toggle(ui, "DRIVE", self.drive_on, w::AMBER).clicked() {
                                    self.drive_on = !self.drive_on;
                                    self.drive_on_p.set(if self.drive_on { 1.0 } else { 0.0 });
                                }
                                ui.add_space(4.0);
                                if w::led_toggle(ui, "AMP", self.nam_on, w::PURPLE).clicked() {
                                    if self.nam_json.is_none() {
                                        // 還沒載模型 → 自動載入 models/ 內的預設 .nam
                                        if !self.load_default_nam() {
                                            self.error = Some(
                                                "找不到預設 .nam（把模型放進 models/，或用下方「載入 .nam…」）"
                                                    .into(),
                                            );
                                        }
                                    } else {
                                        self.nam_on = !self.nam_on;
                                        self.nam_on_p.set(if self.nam_on { 1.0 } else { 0.0 });
                                        // 重啟以套用取樣率切換（ASIO：48k↔44.1k）
                                        self.restart_if_running();
                                    }
                                }
                                ui.add_space(4.0);
                                if w::led_toggle(ui, "CAB", self.cab_on, w::MAGENTA).clicked() {
                                    self.cab_on = !self.cab_on;
                                    self.cab_on_p.set(if self.cab_on { 1.0 } else { 0.0 });
                                }
                                ui.add_space(4.0);
                                if w::led_toggle(ui, "DELAY", self.delay_on, w::TEAL).clicked() {
                                    self.delay_on = !self.delay_on;
                                    self.delay_on_p.set(if self.delay_on { 1.0 } else { 0.0 });
                                }
                                ui.add_space(4.0);
                                if w::led_toggle(ui, "REVERB", self.reverb_on, w::GREEN).clicked() {
                                    self.reverb_on = !self.reverb_on;
                                    self.reverb_on_p.set(if self.reverb_on { 1.0 } else { 0.0 });
                                }
                            });
                            ui.add_space(8.0);
                            // ── NAM 音箱載入列 ──
                            ui.horizontal(|ui| {
                                ui.add_space(2.0);
                                if ui
                                    .button(RichText::new("載入 .nam…").size(10.5))
                                    .on_hover_text("選一個 NAM 擴大機模型 .nam")
                                    .clicked()
                                {
                                    self.pick_nam();
                                }
                                let (name, col) = match &self.nam_name {
                                    Some(n) => (n.as_str(), w::PURPLE),
                                    None => ("未載入（AMP 不作用）", w::FAINT),
                                };
                                ui.label(RichText::new(name).color(col).size(9.5));
                            });
                            // 取樣率不符提示
                            if let Some(msr) = self.nam_sr
                                && let Some(lat) = &self.latency
                                && (msr as u32).abs_diff(lat.sample_rate) > 1
                            {
                                ui.horizontal(|ui| {
                                    ui.add_space(2.0);
                                    ui.label(
                                        RichText::new(format!(
                                            "⚠ 介面 {}Hz ≠ 模型 {:.0}Hz，音色偏移，建議介面設 {:.0}Hz",
                                            lat.sample_rate, msr, msr
                                        ))
                                        .color(w::AMBER)
                                        .size(9.0),
                                    );
                                });
                            }
                            ui.add_space(8.0);
                            // ── IR 載入列 ──
                            ui.horizontal(|ui| {
                                ui.add_space(2.0);
                                if ui
                                    .button(RichText::new("載入 IR…").size(10.5))
                                    .on_hover_text("選一個喇叭箱體脈衝響應 .wav")
                                    .clicked()
                                {
                                    self.pick_ir();
                                }
                                let (name, col) = match &self.ir_name {
                                    Some(n) => (n.as_str(), w::CYAN),
                                    None => ("未載入（CAB 不作用）", w::FAINT),
                                };
                                ui.label(RichText::new(name).color(col).size(9.5));
                            });
                        });
                    });
                });
                ui.add_space(12.0);

                // ── 節拍 / 練習卡 ──
                self.tempo_card(ui);
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

                // ── 預設存讀 ──
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("儲存預設").size(10.5).color(w::VIOLET))
                            .on_hover_text("把目前所有旋鈕 / 開關 / 拍速存成 .json")
                            .clicked()
                        {
                            self.save_preset();
                        }
                        ui.add_space(6.0);
                        if ui
                            .button(RichText::new("載入預設").size(10.5).color(w::VIOLET))
                            .on_hover_text("讀取 .json 預設（不含 .nam / IR 檔，沿用目前已載入的）")
                            .clicked()
                        {
                            self.load_preset();
                        }
                    });
                });

                // ── footer ──
                ui.add_space(10.0);
                w::divider_title(ui, "Isan · 13soul", 44.0);
                ui.add_space(4.0);
                ui.vertical_centered(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("關於 · 版權").color(w::FAINT).size(9.5),
                            )
                            .frame(false),
                        )
                        .on_hover_text("重新開啟關於 / 版權視窗")
                        .clicked()
                    {
                        self.show_about = true;
                    }
                });
            });

        // ── 開啟時的「關於 / 版權」視窗（疊在最上層）──
        if self.show_about {
            about_modal(&ctx, self);
        }
    }
}

/// 作者連結（與 README / NOTICE 同一組）。
const ABOUT_LINKS: [(&str, &str); 5] = [
    ("官方網站", "https://www.poofone.com.tw/"),
    ("Threads · @isan1314558", "https://www.threads.com/@isan1314558"),
    ("YouTube · @13Neosoul", "https://www.youtube.com/@13Neosoul"),
    ("Instagram · @isan1314558", "https://www.instagram.com/isan1314558/"),
    ("Facebook 社團 · AI 工具討論", "https://www.facebook.com/groups/26340062805675868"),
];

/// 「不再顯示」標記檔路徑（%APPDATA%\RustRig\about_seen）。
fn about_marker_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA")
        .map(|a| std::path::PathBuf::from(a).join("RustRig").join("about_seen"))
}

fn about_seen() -> bool {
    about_marker_path().is_some_and(|p| p.exists())
}

fn mark_about_seen() {
    if let Some(p) = about_marker_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, b"1");
    }
}

/// 開啟時的「關於 / 版權」modal：字標 + 簡介 + 署名 + 授權聲明 + 作者連結。
/// 比照 AudioSFX 的啟動關於頁。點「進入」、modal 外或 Esc 皆關閉。
fn about_modal(ctx: &egui::Context, app: &mut RigApp) {
    let resp = egui::Modal::new(egui::Id::new("about_modal")).show(ctx, |ui| {
        ui.set_width(380.0);

        // ── 字標 + 版本 ──
        ui.vertical_centered(|ui| {
            let mut job = egui::text::LayoutJob::default();
            let f = FontId::proportional(32.0);
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
            ui.label(
                RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                    .color(w::FAINT)
                    .size(10.0)
                    .monospace(),
            );
        });
        ui.add_space(12.0);

        // ── 簡介 ──
        ui.label(
            RichText::new(
                "用 Rust 寫的 Windows 獨立電吉他即時效果處理器——低延遲音訊引擎 + 破音 / 箱體 IR / 雜訊閘 / 殘響，對標 Neural DSP 的 standalone 模式。",
            )
            .color(w::DIM)
            .size(11.0),
        );
        ui.add_space(10.0);

        // ── 署名 ──
        ui.label(RichText::new("設計製作 · Isan（13soul）").color(w::PURPLE).size(13.0));
        ui.label(
            RichText::new("全端設計工程師 · 影像及音樂創作人")
                .color(w::DIM)
                .size(10.0),
        );
        ui.add_space(10.0);

        // ── 授權聲明 ──
        ui.label(
            RichText::new(
                "本程式以 Apache-2.0 釋出。ASIO 為 Steinberg 商標，本程式不附 ASIO SDK；NAM 模型與箱體 IR 版權屬各自權利人，不隨附、不轉散。第三方授權見 NOTICE.md。",
            )
            .color(w::FAINT)
            .size(9.0),
        );
        ui.add_space(12.0);

        // ── 作者連結 ──
        ui.label(RichText::new("連結").color(w::DIM).size(10.0));
        ui.add_space(4.0);
        for (label, url) in ABOUT_LINKS {
            ui.hyperlink_to(RichText::new(label).color(w::CYAN).size(10.5), url);
        }
        ui.add_space(14.0);

        ui.separator();
        ui.add_space(8.0);

        // ── 不再顯示 + 進入 ──
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut app.about_dont_show,
                RichText::new("不再顯示").size(10.5).color(w::DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("進入").size(12.0)).clicked() {
                    if app.about_dont_show {
                        mark_about_seen();
                    }
                    app.show_about = false;
                }
            });
        });
    });

    // 點 modal 外或 Esc → 同樣關閉（尊重「不再顯示」勾選）
    if resp.should_close() {
        if app.about_dont_show {
            mark_about_seen();
        }
        app.show_about = false;
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
            ui.label(RichText::new(app.backend.label()).color(w::DIM).size(11.0).monospace());

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
