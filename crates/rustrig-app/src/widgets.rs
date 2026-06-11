//! RustRig 自訂 widget — 視覺對標 AudioSFX 的霓虹合成器風格：
//! 黑底、紫→洋紅 accent、發光 LED tick 環旋鈕、DAW 式 dB fader + 峰值表。
//!
//! 旋鈕幾何移植自 AudioSFX `Knob.jsx`（225° 起點、270° 行程、底部 90° 缺口、
//! 金屬斜面 + 圓頂面 + 頂部高光）；channel strip 移植自 `ChannelStrip.jsx`
//! （-60…+12 dB 刻度、0 dB 紫色基準線、綠→黃→紅峰值表、削波鎖存）。

use eframe::egui::{
    Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

// ── 調色盤（取自 AudioSFX / 海報）─────────────────────────────
pub const BG: Color32 = Color32::from_rgb(10, 10, 14);
pub const PANEL: Color32 = Color32::from_rgb(20, 20, 24); // #141417
pub const PANEL_EDGE: Color32 = Color32::from_rgb(42, 42, 47); // #2a2a2f
pub const PURPLE: Color32 = Color32::from_rgb(109, 94, 252); // #6d5efc 品牌紫
pub const VIOLET: Color32 = Color32::from_rgb(155, 141, 255); // #9b8dff 亮紫
pub const MAGENTA: Color32 = Color32::from_rgb(236, 72, 153); // #ec4899
pub const PINK: Color32 = Color32::from_rgb(233, 69, 96); // #e94560
pub const AMBER: Color32 = Color32::from_rgb(245, 158, 11); // #f59e0b
pub const CYAN: Color32 = Color32::from_rgb(34, 211, 238); // #22d3ee
pub const GREEN: Color32 = Color32::from_rgb(34, 197, 94); // #22c55e
pub const YELLOW: Color32 = Color32::from_rgb(234, 179, 8); // #eab308
pub const RED: Color32 = Color32::from_rgb(239, 68, 68); // #ef4444
pub const TEXT: Color32 = Color32::from_rgb(232, 232, 234); // #e8e8ea
pub const DIM: Color32 = Color32::from_rgb(110, 110, 122);
pub const FAINT: Color32 = Color32::from_rgb(70, 70, 78);

pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// 螢幕角度（0°=正上、順時針）→ 單位方向向量（egui y 軸朝下）。
fn screen_dir(deg: f32) -> eframe::egui::Vec2 {
    let r = deg.to_radians();
    vec2(r.sin(), -r.cos())
}

// ── dB 工具（與 AudioSFX ChannelStrip 同一套刻度）──────────────
pub const TOP_DB: f32 = 12.0;
pub const BOT_DB: f32 = -60.0;
const DB_RANGE: f32 = TOP_DB - BOT_DB;
pub const SCALE_TICKS: [f32; 9] = [12.0, 6.0, 0.0, -6.0, -12.0, -24.0, -36.0, -48.0, -60.0];

pub fn gain_to_db(g: f32) -> f32 {
    if g > 1e-4 { 20.0 * g.log10() } else { BOT_DB }
}
pub fn db_to_gain(db: f32) -> f32 {
    if db <= BOT_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}
pub fn db_to_frac(db: f32) -> f32 {
    ((db - BOT_DB) / DB_RANGE).clamp(0.0, 1.0)
}
pub fn fmt_db(db: f32) -> String {
    if db <= BOT_DB {
        "-∞".into()
    } else if db > 0.0 {
        format!("+{db:.1}")
    } else {
        format!("{db:.1}")
    }
}

// ── 旋鈕 ───────────────────────────────────────────────────────
const KNOB_START: f32 = 225.0; // frac=0 的螢幕角度
const KNOB_SWEEP: f32 = 270.0; // 底部留 90° 缺口
const N_TICKS: usize = 25;

/// 海報風霓虹旋鈕。垂直拖曳調整、雙擊重設。`enabled=false` 畫成佔位 ghost。
/// 回傳值是否改變。
#[allow(clippy::too_many_arguments)]
pub fn knob(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    min: f32,
    max: f32,
    default_v: f32,
    accent: Color32,
    enabled: bool,
    fmt: &dyn Fn(f32) -> String,
) -> bool {
    let size = 72.0;
    let cell = vec2(size + 6.0, size + 34.0);
    let (rect, mut resp) = ui.allocate_exact_size(
        cell,
        if enabled { Sense::click_and_drag() } else { Sense::hover() },
    );
    let center = pos2(rect.center().x, rect.top() + 14.0 + size / 2.0);
    let k = size / 78.0; // 幾何比例同 Knob.jsx（SIZE=78）
    let dial = 18.0 * k;
    let tick_ri = 28.0 * k;
    let tick_ro = 31.0 * k;

    let mut changed = false;
    if enabled {
        if resp.dragged() {
            let delta = -resp.drag_delta().y / 150.0 * (max - min);
            *value = (*value + delta).clamp(min, max);
            changed = true;
        }
        if resp.double_clicked() {
            *value = default_v;
            changed = true;
        }
        if changed {
            resp.mark_changed();
        }
    }
    let frac = ((*value - min) / (max - min)).clamp(0.0, 1.0);

    let p = ui.painter();

    // 霓虹 bloom（層疊半透明圓模擬 radial glow）
    if enabled {
        for (r_add, a) in [(12.0, 22), (8.0, 36), (4.0, 50)] {
            p.circle_filled(center, dial + r_add * k, with_alpha(accent, a));
        }
    }

    // tick 環：亮到目前值
    for i in 0..N_TICKS {
        let tf = i as f32 / (N_TICKS - 1) as f32;
        let dir = screen_dir(KNOB_START + tf * KNOB_SWEEP);
        let active = enabled && tf <= frac + 0.001;
        let c = if active {
            accent
        } else {
            Color32::from_rgb(58, 58, 64)
        };
        if active {
            // 發光：先畫粗的半透明，再畫細的實線
            p.line_segment(
                [center + dir * tick_ri, center + dir * tick_ro],
                Stroke::new(2.2, with_alpha(accent, 70)),
            );
        }
        p.line_segment(
            [center + dir * tick_ri, center + dir * tick_ro],
            Stroke::new(if active { 1.0 } else { 0.7 }, c),
        );
    }

    // 影子 → 金屬斜面 → 圓頂面 → 高光
    p.circle_filled(center + vec2(0.0, 2.5 * k), dial + 2.0 * k, with_alpha(Color32::BLACK, 110));
    let bezel_tint = if enabled { with_alpha(accent, 90) } else { with_alpha(FAINT, 90) };
    p.circle_filled(center, dial + 2.5 * k, Color32::from_rgb(43, 43, 49));
    p.circle_stroke(center, dial + 2.5 * k, Stroke::new(1.0, bezel_tint));
    p.circle_filled(center, dial, Color32::from_rgb(28, 28, 33));
    // 圓頂高光（偏左上）
    p.circle_filled(
        center + vec2(-dial * 0.22, -dial * 0.25),
        dial * 0.58,
        with_alpha(Color32::from_rgb(62, 62, 72), 120),
    );
    // 頂部光澤弧（±85°）
    let gloss_r = dial - 4.0 * k;
    let pts: Vec<Pos2> = (0..=24)
        .map(|i| {
            let a = -85.0 + 170.0 * i as f32 / 24.0;
            center + screen_dir(a) * gloss_r
        })
        .collect();
    p.add(eframe::egui::Shape::line(
        pts,
        Stroke::new(1.4, with_alpha(Color32::WHITE, 70)),
    ));

    // 指針
    let dir = screen_dir(KNOB_START + frac * KNOB_SWEEP);
    let ptr_c = if enabled { accent } else { FAINT };
    if enabled {
        p.line_segment(
            [center + dir * (dial - 12.0 * k), center + dir * (dial - 5.0 * k)],
            Stroke::new(2.6, with_alpha(accent, 80)),
        );
    }
    p.line_segment(
        [center + dir * (dial - 12.0 * k), center + dir * (dial - 5.0 * k)],
        Stroke::new(1.3, ptr_c),
    );

    // label（上、accent 色）與數值（下、mono）
    p.text(
        pos2(center.x, rect.top() + 5.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(9.5),
        if enabled { accent } else { FAINT },
    );
    p.text(
        pos2(center.x, rect.bottom() - 8.0),
        Align2::CENTER_CENTER,
        if enabled { fmt(*value) } else { "—".into() },
        FontId::monospace(10.0),
        if enabled { Color32::from_rgb(187, 187, 187) } else { FAINT },
    );

    if enabled {
        resp.on_hover_text("拖曳調整（雙擊重設）");
    }
    changed
}

// ── Channel strip：dB 刻度 | fader | 峰值表 | dB 讀數 ───────────
/// 回傳音量是否改變。`gain_lin` 是線性增益（1.0 = 0 dB）。
pub fn channel_strip(
    ui: &mut Ui,
    gain_lin: &mut f32,
    meter_db: f32,
    clip: bool,
) -> bool {
    let strip_h = 196.0;
    let (rect, _) = ui.allocate_exact_size(vec2(118.0, strip_h + 40.0), Sense::hover());
    let top = rect.top() + 6.0;
    let h = strip_h - 12.0;
    let scale_right = rect.left() + 30.0;
    let fader_rect = Rect::from_min_size(pos2(scale_right + 4.0, top), vec2(26.0, h));
    let meter_rect = Rect::from_min_size(pos2(fader_rect.right() + 8.0, top), vec2(11.0, h));

    let mut changed = false;
    let mut db = gain_to_db(*gain_lin);

    // fader 互動：點/拖直接以 y 設值（同 ChannelStrip.jsx），雙擊回 0 dB
    let resp = ui.interact(
        fader_rect.expand2(vec2(6.0, 2.0)),
        ui.id().with("fader"),
        Sense::click_and_drag(),
    );
    if resp.double_clicked() {
        *gain_lin = 1.0;
        db = 0.0;
        changed = true;
    } else if resp.dragged() || resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let frac = (1.0 - (pos.y - top) / h).clamp(0.0, 1.0);
            db = BOT_DB + frac * DB_RANGE;
            *gain_lin = db_to_gain(db);
            changed = true;
        }
    }
    let fader_frac = db_to_frac(db);
    let zero_y = top + (1.0 - db_to_frac(0.0)) * h;

    let p = ui.painter();

    // dB 刻度
    for t in SCALE_TICKS {
        let y = top + (1.0 - db_to_frac(t)) * h;
        let is_zero = t == 0.0;
        p.text(
            pos2(scale_right - 6.0, y),
            Align2::RIGHT_CENTER,
            if t > 0.0 { format!("+{}", t as i32) } else { format!("{}", t as i32) },
            FontId::monospace(8.0),
            if is_zero { VIOLET } else { Color32::from_rgb(85, 85, 85) },
        );
        p.line_segment(
            [pos2(scale_right - 4.0, y), pos2(scale_right, y)],
            Stroke::new(1.0, if is_zero { PURPLE } else { Color32::from_rgb(58, 58, 58) }),
        );
    }

    // fader 溝槽
    let groove_x = fader_rect.center().x;
    let groove = Rect::from_center_size(pos2(groove_x, top + h / 2.0), vec2(3.0, h));
    p.rect_filled(groove, CornerRadius::same(2), Color32::from_rgb(12, 12, 13));
    p.rect_stroke(groove, CornerRadius::same(2), Stroke::new(1.0, Color32::from_rgb(42, 42, 42)), StrokeKind::Outside);
    // 填充段（底→指位，紫色漸層用切片模擬）
    let fill_top = top + (1.0 - fader_frac) * h;
    let n = 18;
    for i in 0..n {
        let y0 = fill_top + (h + top - fill_top) * i as f32 / n as f32;
        let y1 = fill_top + (h + top - fill_top) * (i + 1) as f32 / n as f32;
        let tcol = i as f32 / (n - 1) as f32; // 0=頂(亮) 1=底(暗)
        p.rect_filled(
            Rect::from_min_max(pos2(groove_x - 1.5, y0), pos2(groove_x + 1.5, y1.min(top + h))),
            CornerRadius::ZERO,
            with_alpha(PURPLE, (255.0 - tcol * 178.0) as u8),
        );
    }
    // 0 dB 基準線
    p.line_segment(
        [pos2(fader_rect.left() - 2.0, zero_y), pos2(fader_rect.right() + 2.0, zero_y)],
        Stroke::new(1.0, with_alpha(PURPLE, 64)),
    );
    // thumb
    let thumb = Rect::from_center_size(pos2(groove_x, fill_top), vec2(24.0, 14.0));
    p.rect_filled(thumb, CornerRadius::same(3), Color32::from_rgb(46, 46, 51));
    p.rect_stroke(thumb, CornerRadius::same(3), Stroke::new(1.0, Color32::from_rgb(74, 74, 82)), StrokeKind::Inside);
    for (dy, c) in [(-3.0, PURPLE), (0.0, Color32::from_rgb(85, 85, 85)), (3.0, Color32::from_rgb(85, 85, 85))] {
        p.line_segment(
            [pos2(groove_x - 7.0, fill_top + dy), pos2(groove_x + 7.0, fill_top + dy)],
            Stroke::new(1.0, c),
        );
    }

    // 峰值表：綠 →(62%) 綠 →(84%) 黃 → 紅（切片漸層），削波鎖存紅燈
    p.rect_filled(meter_rect, CornerRadius::same(2), Color32::BLACK);
    p.rect_stroke(meter_rect, CornerRadius::same(2), Stroke::new(1.0, Color32::from_rgb(51, 51, 51)), StrokeKind::Outside);
    let m_frac = db_to_frac(meter_db);
    if m_frac > 0.0 {
        let fill_h = h * m_frac;
        let seg = 32;
        for i in 0..seg {
            let f0 = i as f32 / seg as f32;
            let f1 = (i + 1) as f32 / seg as f32;
            if f0 * h > fill_h {
                break;
            }
            // f 由底(0)往上(1)；顏色停在 [0,.62]=綠 [.62,.84]=綠→黃 [.84,1]=黃→紅
            let c = if f0 < 0.62 {
                GREEN
            } else if f0 < 0.84 {
                lerp_color(GREEN, YELLOW, (f0 - 0.62) / 0.22)
            } else {
                lerp_color(YELLOW, RED, (f0 - 0.84) / 0.16)
            };
            let y1 = top + h - f0 * h;
            let y0 = top + h - (f1 * h).min(fill_h);
            p.rect_filled(
                Rect::from_min_max(pos2(meter_rect.left() + 1.0, y0), pos2(meter_rect.right() - 1.0, y1)),
                CornerRadius::ZERO,
                c,
            );
        }
    }
    p.line_segment(
        [pos2(meter_rect.left(), zero_y), pos2(meter_rect.right(), zero_y)],
        Stroke::new(1.0, with_alpha(Color32::WHITE, 38)),
    );
    if clip {
        p.rect_filled(
            Rect::from_min_size(meter_rect.min, vec2(meter_rect.width(), 4.0)),
            CornerRadius::ZERO,
            RED,
        );
    }

    // 讀數
    p.text(
        pos2(rect.center().x, rect.bottom() - 22.0),
        Align2::CENTER_CENTER,
        format!("{} dB", fmt_db(db)),
        FontId::monospace(15.0),
        TEXT,
    );
    p.text(
        pos2(rect.center().x, rect.bottom() - 7.0),
        Align2::CENTER_CENTER,
        format!("VOLUME · {}%", (*gain_lin * 100.0).round() as i32),
        FontId::monospace(8.5),
        Color32::from_rgb(102, 102, 102),
    );

    changed
}

// ── 大播放鍵：紫→洋紅漸層發光環 ────────────────────────────────
pub fn play_button(ui: &mut Ui, running: bool) -> Response {
    let d = 84.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(d, d), Sense::click());
    let c = rect.center();
    let r = d / 2.0 - 6.0;
    let p = ui.painter();
    let hot = resp.hovered();

    // 外圈光暈
    for (r_add, a) in [(10.0, 16), (6.0, 26), (3.0, 40)] {
        p.circle_filled(c, r + r_add, with_alpha(if running { MAGENTA } else { PURPLE }, if hot { a + 12 } else { a }));
    }
    // 本體
    p.circle_filled(c, r, Color32::from_rgb(18, 16, 26));
    // 漸層環：64 段，紫→洋紅→紫（左右對稱）
    let segs = 64;
    for i in 0..segs {
        let a0 = std::f32::consts::TAU * i as f32 / segs as f32;
        let a1 = std::f32::consts::TAU * (i + 1) as f32 / segs as f32;
        let mid = (a0 + a1) / 2.0;
        // 對稱混色：頂=紫、底=洋紅
        let t = (1.0 - mid.cos()) / 2.0;
        let col = lerp_color(PURPLE, MAGENTA, t);
        let p0 = c + vec2(a0.sin(), -a0.cos()) * r;
        let p1 = c + vec2(a1.sin(), -a1.cos()) * r;
        p.line_segment([p0, p1], Stroke::new(if hot { 3.0 } else { 2.2 }, col));
    }
    // 圖示
    if running {
        // stop：圓角方塊
        let s = Rect::from_center_size(c, vec2(20.0, 20.0));
        p.rect_filled(s, CornerRadius::same(4), MAGENTA);
    } else {
        // play：三角形（重心微右修）
        let pts = vec![
            c + vec2(-6.0, -11.0),
            c + vec2(-6.0, 11.0),
            c + vec2(13.0, 0.0),
        ];
        p.add(eframe::egui::Shape::convex_polygon(
            pts,
            VIOLET,
            Stroke::NONE,
        ));
    }
    resp.on_hover_text(if running { "停止引擎" } else { "啟動引擎" })
}

/// 海報式分隔標題：左紫線 ─ 文字 ─ 右洋紅線。
pub fn divider_title(ui: &mut Ui, text: &str, half_line: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    p.text(c, Align2::CENTER_CENTER, text, FontId::proportional(11.5), DIM);
    let gap = text.chars().count() as f32 * 7.0 + 18.0;
    p.line_segment(
        [pos2(c.x - gap - half_line, c.y), pos2(c.x - gap, c.y)],
        Stroke::new(1.0, PURPLE),
    );
    p.line_segment(
        [pos2(c.x + gap, c.y), pos2(c.x + gap + half_line, c.y)],
        Stroke::new(1.0, PINK),
    );
}
