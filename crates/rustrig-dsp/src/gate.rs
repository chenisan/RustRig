//! 雜訊閘（noise gate）。
//!
//! 高增益破音會把不彈時的底噪/嗡聲一起放大。閘擺在**破音之前**：偵測乾淨吉他
//! 訊號的包絡，低於門檻時把訊號淡出（關閘），破音就沒東西可放大 → 安靜。
//!
//! 設計：峰值包絡跟隨（瞬間上衝、指數釋放）+ 遲滯（開/關門檻分離防抖動）+
//! 快開慢關的增益平滑（音頭不被切、尾音平順淡出）。

use crate::{AudioProcessor, SharedParam};

/// 雜訊閘。`amount` 0..1 對應門檻 −70..−35 dBFS（0 = 不作用）。
pub struct Gate {
    /// 強度 0..1（→ 門檻 dBFS）
    pub amount: SharedParam,
    /// >0.5 = 開
    pub enabled: SharedParam,

    sr: f32,
    env: f32,
    gain: f32,
    open: bool,
    env_rel: f32, // 包絡釋放係數（~10ms）
    atk: f32,     // 增益開啟係數（~1ms，快）
    rel: f32,     // 增益關閉係數（~100ms，慢）
}

impl Gate {
    pub fn new(amount: SharedParam, enabled: SharedParam) -> Self {
        Self {
            amount,
            enabled,
            sr: 48_000.0,
            env: 0.0,
            gain: 0.0,
            open: false,
            env_rel: 0.0,
            atk: 0.0,
            rel: 0.0,
        }
    }

    #[inline]
    fn coeff(sr: f32, secs: f32) -> f32 {
        (-1.0 / (secs * sr)).exp()
    }
}

impl AudioProcessor for Gate {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        self.sr = sample_rate;
        self.env_rel = Self::coeff(sample_rate, 0.010);
        self.atk = Self::coeff(sample_rate, 0.001);
        self.rel = Self::coeff(sample_rate, 0.100);
        self.reset();
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            return;
        }
        let amt = self.amount.get().clamp(0.0, 1.0);
        if amt < 0.01 {
            return; // 旋鈕歸零 = 不作用
        }
        // 門檻：amt 0..1 → −70..−35 dBFS。遲滯：關門檻比開門檻低 6dB。
        let open_thr = 10f32.powf((-70.0 + 35.0 * amt) / 20.0);
        let close_thr = open_thr * 0.5;

        for s in buf {
            let x = *s;
            let rect = x.abs();
            // 峰值包絡：瞬間上衝、指數釋放
            self.env = if rect > self.env {
                rect
            } else {
                self.env_rel * self.env
            };
            // 遲滯切換
            if self.open {
                if self.env < close_thr {
                    self.open = false;
                }
            } else if self.env > open_thr {
                self.open = true;
            }
            // 快開慢關的增益平滑
            let target = if self.open { 1.0 } else { 0.0 };
            let c = if target > self.gain { self.atk } else { self.rel };
            self.gain = c * self.gain + (1.0 - c) * target;
            *s = x * self.gain;
        }
    }

    fn reset(&mut self) {
        self.env = 0.0;
        self.gain = 0.0;
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_on_quiet_noise() {
        // 強度高、訊號遠低於門檻 → 最終應被壓到接近靜音
        let mut g = Gate::new(SharedParam::new(1.0), SharedParam::new(1.0));
        g.prepare(48_000.0, 512);
        let mut buf = vec![0.0001f32; 48_000]; // 約 −80dBFS，跑 1 秒讓增益關到底
        g.process(&mut buf);
        assert!(buf.last().unwrap().abs() < 1e-5);
    }

    #[test]
    fn opens_for_loud_signal() {
        let mut g = Gate::new(SharedParam::new(0.5), SharedParam::new(1.0));
        g.prepare(48_000.0, 4096);
        // 滿振幅正弦遠高於門檻 → 開閘、訊號大致通過
        let mut buf: Vec<f32> = (0..4096)
            .map(|i| 0.8 * (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin())
            .collect();
        g.process(&mut buf);
        assert!(buf[2048..].iter().any(|s| s.abs() > 0.5));
    }

    #[test]
    fn bypass_when_disabled() {
        let mut g = Gate::new(SharedParam::new(1.0), SharedParam::new(0.0));
        g.prepare(48_000.0, 8);
        let mut buf = [0.0001f32; 8];
        let before = buf;
        g.process(&mut buf);
        assert_eq!(buf, before);
    }
}
