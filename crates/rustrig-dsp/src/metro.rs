//! 節拍器（metronome）。
//!
//! 不處理吉他訊號，而是依 BPM **產生 click 並疊加到輸出**。擺在**音量之後、峰值表
//! 之前**：click 電平穩定、不受吉他音量旋鈕影響。
//!
//! click = 短促衰減正弦；每小節第 1 拍用較高頻 + 重音（accent），其餘拍正常。BPM 來自
//! 與 delay 同步共用的全域 `SharedParam`。sample 計數器零配置；sin 為純算術，符合 RT 規則。

use crate::{AudioProcessor, SharedParam};
use std::f32::consts::TAU;

/// 每小節拍數（重音落在第 1 拍）。
const BEATS_PER_BAR: usize = 4;
/// 重音 / 一般 click 頻率（Hz）。
const ACCENT_HZ: f32 = 1600.0;
const NORMAL_HZ: f32 = 1000.0;

/// 節拍器。`bpm` 拍速（與 delay 同步共用）、`level` 0..1 音量、`enabled` >0.5 開。
pub struct Metronome {
    pub bpm: SharedParam,
    pub level: SharedParam,
    pub enabled: SharedParam,

    sr: f32,
    phase: f32, // 距下一拍的 sample 計數
    beat: usize,
    env: f32,   // click 振幅包絡
    osc: f32,   // 正弦相位（rad）
    freq: f32,  // 目前 click 頻率
    decay: f32, // 包絡衰減係數（~30ms）
    running: bool,
}

impl Metronome {
    pub fn new(bpm: SharedParam, level: SharedParam, enabled: SharedParam) -> Self {
        Self {
            bpm,
            level,
            enabled,
            sr: 48_000.0,
            phase: 0.0,
            beat: 0,
            env: 0.0,
            osc: 0.0,
            freq: NORMAL_HZ,
            decay: 0.0,
            running: false,
        }
    }

    #[inline]
    fn period(&self) -> f32 {
        let bpm = self.bpm.get().clamp(20.0, 300.0);
        self.sr * 60.0 / bpm
    }
}

impl AudioProcessor for Metronome {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        self.sr = sample_rate;
        self.decay = (-1.0 / (0.030 * sample_rate)).exp();
        self.phase = 0.0;
        self.beat = 0;
        self.env = 0.0;
        self.osc = 0.0;
        self.running = false;
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            self.running = false;
            return;
        }
        // 剛開啟 → 從第 1 拍重音立即起拍
        if !self.running {
            self.running = true;
            self.beat = 0;
            self.phase = self.period(); // 下一個 sample 即觸發
            self.env = 0.0;
        }
        let period = self.period();
        let level = self.level.get().clamp(0.0, 1.0);

        for s in buf.iter_mut() {
            if self.phase >= period {
                self.phase -= period;
                // 觸發 click：第 1 拍重音
                self.freq = if self.beat == 0 { ACCENT_HZ } else { NORMAL_HZ };
                self.env = if self.beat == 0 { 1.0 } else { 0.7 };
                self.osc = 0.0;
                self.beat = (self.beat + 1) % BEATS_PER_BAR;
            }
            if self.env > 1e-4 {
                *s += self.osc.sin() * self.env * level * 0.5;
                self.osc += TAU * self.freq / self.sr;
                if self.osc > TAU {
                    self.osc -= TAU;
                }
                self.env *= self.decay;
            }
            self.phase += 1.0;
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.beat = 0;
        self.env = 0.0;
        self.osc = 0.0;
        self.running = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_when_disabled() {
        let mut m = Metronome::new(
            SharedParam::new(120.0),
            SharedParam::new(1.0),
            SharedParam::new(0.0),
        );
        m.prepare(48_000.0, 512);
        let mut buf = vec![0.0f32; 48_000];
        m.process(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn emits_clicks_when_enabled() {
        // 120 BPM @48k：一拍 24000 samples，跑 1 秒應有 2 個 click（含起拍重音）
        let mut m = Metronome::new(
            SharedParam::new(120.0),
            SharedParam::new(1.0),
            SharedParam::new(1.0),
        );
        m.prepare(48_000.0, 48_000);
        let mut buf = vec![0.0f32; 48_000];
        m.process(&mut buf);
        let peak = buf.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        assert!(peak > 0.1, "沒有 click，peak={peak}");
        assert!(buf.iter().all(|s| s.is_finite()));
        // 起拍 click 應落在最前面（開啟即觸發）
        assert!(buf[..2000].iter().any(|s| s.abs() > 0.1), "起拍未立即觸發");
    }
}
