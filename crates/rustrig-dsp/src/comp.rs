//! 壓縮器（feed-forward compressor）。
//!
//! 動態控制：把過門檻的訊號依比例壓低，縮小強弱差距 → clean/funk 的「貼地」與
//! 長音 sustain。擺在**閘之後、破音之前**：閘先把底噪清掉，壓縮再把動態壓平，
//! 才送進破音／音箱。
//!
//! 設計：log 域 gain computer（軟膝）+ 增益衰減的快攻慢放 ballistics。單一
//! `amount` 旋鈕同時帶動門檻與比例（像 pedal 的 sustain 鈕），`makeup` 補回音量。
//! 延遲線無、濾波記憶極小，prepare 只算係數，process 零配置。

use crate::{AudioProcessor, SharedParam};

/// 壓縮器。`amount` 0..1 → 門檻 0..−36 dBFS、比例 2:1..8:1；`makeup` 為補償 dB。
pub struct Compressor {
    /// 壓縮量 0..1（→ 門檻 + 比例）
    pub amount: SharedParam,
    /// 補償增益（dB，0..+18）
    pub makeup_db: SharedParam,
    /// >0.5 = 開
    pub enabled: SharedParam,

    gr_db: f32, // 目前增益衰減（dB，≤0）
    atk: f32,   // 增益加深係數（~5ms，快）
    rel: f32,   // 增益回復係數（~120ms，慢）
}

/// 軟膝寬度（dB）。
const KNEE_DB: f32 = 6.0;

impl Compressor {
    pub fn new(amount: SharedParam, makeup_db: SharedParam, enabled: SharedParam) -> Self {
        Self {
            amount,
            makeup_db,
            enabled,
            gr_db: 0.0,
            atk: 0.0,
            rel: 0.0,
        }
    }

    #[inline]
    fn coeff(sr: f32, secs: f32) -> f32 {
        (-1.0 / (secs * sr)).exp()
    }
}

impl AudioProcessor for Compressor {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        self.atk = Self::coeff(sample_rate, 0.005);
        self.rel = Self::coeff(sample_rate, 0.120);
        self.gr_db = 0.0;
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            return;
        }
        let amount = self.amount.get().clamp(0.0, 1.0);
        let makeup = 10f32.powf(self.makeup_db.get() / 20.0);
        // amount 0..1 → 門檻 0..−36 dBFS、比例 2:1..8:1
        let thresh_db = -36.0 * amount;
        let ratio = 2.0 + 6.0 * amount;
        let slope = 1.0 - 1.0 / ratio;

        for s in buf.iter_mut() {
            let x = *s;
            let level_db = 20.0 * (x.abs() + 1e-9).log10();
            let over = level_db - thresh_db;
            // 軟膝 gain computer：膝內二次過渡，膝外線性
            let target_gr_db = if over <= -KNEE_DB * 0.5 {
                0.0
            } else if over >= KNEE_DB * 0.5 {
                -slope * over
            } else {
                let t = over + KNEE_DB * 0.5; // 0..KNEE
                -slope * t * t / (2.0 * KNEE_DB)
            };
            // 快攻（衰減加深）慢放（衰減回復）
            let c = if target_gr_db < self.gr_db {
                self.atk
            } else {
                self.rel
            };
            self.gr_db = c * self.gr_db + (1.0 - c) * target_gr_db;
            let g = 10f32.powf(self.gr_db / 20.0) * makeup;
            *s = x * g;
        }
    }

    fn reset(&mut self) {
        self.gr_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_when_disabled() {
        let mut c = Compressor::new(
            SharedParam::new(1.0),
            SharedParam::new(12.0),
            SharedParam::new(0.0),
        );
        c.prepare(48_000.0, 8);
        let mut buf = [0.5f32, -0.7, 0.9, -0.2];
        let before = buf;
        c.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn compresses_loud_signal() {
        // 高壓縮量、無補償：穩態大訊號的輸出振幅應明顯低於輸入
        let mut c = Compressor::new(
            SharedParam::new(1.0),
            SharedParam::new(0.0),
            SharedParam::new(1.0),
        );
        c.prepare(48_000.0, 48_000);
        let mut buf = vec![0.8f32; 48_000]; // 直流等振幅，跑滿讓 ballistics 收斂
        c.process(&mut buf);
        assert!(buf.last().unwrap().abs() < 0.7, "尾端={}", buf.last().unwrap());
        assert!(buf.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn quiet_signal_passes_through() {
        // 遠低於門檻（−60dBFS）、無補償 → 幾乎不壓，輸出≈輸入
        let mut c = Compressor::new(
            SharedParam::new(1.0),
            SharedParam::new(0.0),
            SharedParam::new(1.0),
        );
        c.prepare(48_000.0, 4800);
        let mut buf = vec![0.001f32; 4800];
        c.process(&mut buf);
        assert!((buf.last().unwrap() - 0.001).abs() < 1e-4);
    }
}
