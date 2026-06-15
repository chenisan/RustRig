//! 調音器（tuner）。
//!
//! **唯讀** processor：不改訊號，只偵測基頻寫進 [`PitchHandle`]，給 GUI 顯示音名 +
//! cents 偏移。擺在**輸入增益之後、閘之前**，看的是乾淨吉他輸入。
//!
//! 演算法：累積分析窗 → 降採樣到 ~12kHz（吉他基頻 < 1.4kHz，降採樣大幅省 CPU）→
//! **YIN**（difference function + 累積平均正規化 + 門檻 + 拋物線內插）。YIN 比純
//! autocorrelation 不易抓錯八度。RMS 太低（沒彈）就回 0，避免讀數亂跳。
//! 窗 / 暫存全在 prepare 配置，process 內零配置（只算術，符合 RT 規則）。

use crate::{AudioProcessor, PitchHandle, SharedParam};

/// 分析窗長（samples @原始取樣率）。
const WIN_LEN: usize = 2048;
/// 每隔多少 sample 偵測一次（≈ 每 hop/sr 秒）。
const HOP: usize = 512;
/// 降採樣目標取樣率（Hz）。
const TARGET_RATE: f32 = 12_000.0;
/// 偵測上限頻率（Hz）→ 決定最小週期 tau。
const MAX_HZ: f32 = 1400.0;
/// YIN 判定門檻（越小越嚴）。
const YIN_THRESH: f32 = 0.15;
/// RMS 低於此值視為沒在彈。
const RMS_GATE: f32 = 1e-3;

/// 調音器。`enabled` >0.5 才偵測；偵測結果寫進建構時給的 [`PitchHandle`]。
pub struct Tuner {
    pub enabled: SharedParam,
    handle: PitchHandle,

    sr: f32,
    decim: usize,
    win: Vec<f32>, // 環形分析窗
    wpos: usize,
    filled: usize,
    hop_counter: usize,
    scratch: Vec<f32>, // 解環後的時序窗
    ds: Vec<f32>,      // 降採樣後序列
    dp: Vec<f32>,      // YIN 累積平均正規化差值
}

impl Tuner {
    pub fn new(enabled: SharedParam, handle: PitchHandle) -> Self {
        Self {
            enabled,
            handle,
            sr: 48_000.0,
            decim: 4,
            win: Vec::new(),
            wpos: 0,
            filled: 0,
            hop_counter: 0,
            scratch: Vec::new(),
            ds: Vec::new(),
            dp: Vec::new(),
        }
    }

    /// 對目前分析窗做一次 YIN 偵測，回傳基頻 Hz（0 = 無可信音高）。
    fn detect(&mut self) -> f32 {
        let n = self.win.len();
        // 解環：最舊的樣本在 wpos
        for i in 0..n {
            self.scratch[i] = self.win[(self.wpos + i) % n];
        }
        // RMS 閘
        let energy: f32 = self.scratch.iter().map(|&x| x * x).sum();
        let rms = (energy / n as f32).sqrt();
        if rms < RMS_GATE {
            return 0.0;
        }
        // 降採樣
        let d = self.decim;
        let dn = n / d;
        for i in 0..dn {
            self.ds[i] = self.scratch[i * d];
        }
        let half = dn / 2;
        if half < 4 {
            return 0.0;
        }
        let sr_d = self.sr / d as f32;
        let min_tau = ((sr_d / MAX_HZ) as usize).max(2);
        let max_tau = half;

        // YIN difference + 累積平均正規化
        let mut running = 0.0f32;
        self.dp[0] = 1.0;
        for tau in 1..max_tau {
            let mut sum = 0.0f32;
            for j in 0..half {
                let diff = self.ds[j] - self.ds[j + tau];
                sum += diff * diff;
            }
            running += sum;
            self.dp[tau] = if running > 0.0 {
                sum * tau as f32 / running
            } else {
                1.0
            };
        }

        // 挑 tau：第一個低於門檻處往下走到區域最小
        let mut found = 0usize;
        let mut tau = min_tau;
        while tau < max_tau {
            if self.dp[tau] < YIN_THRESH {
                while tau + 1 < max_tau && self.dp[tau + 1] < self.dp[tau] {
                    tau += 1;
                }
                found = tau;
                break;
            }
            tau += 1;
        }
        if found == 0 {
            // 沒有低於門檻 → 取全域最小；仍偏高就視為無音高
            let mut mt = min_tau;
            for t in (min_tau + 1)..max_tau {
                if self.dp[t] < self.dp[mt] {
                    mt = t;
                }
            }
            if self.dp[mt] > YIN_THRESH * 2.0 {
                return 0.0;
            }
            found = mt;
        }

        // 拋物線內插精修週期
        let t = found;
        let period = if t > 0 && t + 1 < max_tau {
            let x0 = self.dp[t - 1];
            let x1 = self.dp[t];
            let x2 = self.dp[t + 1];
            let denom = x0 + x2 - 2.0 * x1;
            let shift = if denom.abs() > 1e-12 {
                (0.5 * (x0 - x2) / denom).clamp(-1.0, 1.0)
            } else {
                0.0
            };
            t as f32 + shift
        } else {
            t as f32
        };
        if period > 0.0 {
            sr_d / period
        } else {
            0.0
        }
    }
}

impl AudioProcessor for Tuner {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        self.sr = sample_rate;
        self.decim = (sample_rate / TARGET_RATE).round().max(1.0) as usize;
        self.win = vec![0.0; WIN_LEN];
        self.scratch = vec![0.0; WIN_LEN];
        self.ds = vec![0.0; WIN_LEN];
        self.dp = vec![0.0; WIN_LEN / 2 + 1];
        self.wpos = 0;
        self.filled = 0;
        self.hop_counter = 0;
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            return; // 不偵測（GUI 自行顯示「—」）
        }
        let n = self.win.len();
        // 唯讀：只把樣本灌進分析窗，不改 *s
        for &x in buf.iter() {
            self.win[self.wpos] = x;
            self.wpos = (self.wpos + 1) % n;
            if self.filled < n {
                self.filled += 1;
            }
            self.hop_counter += 1;
            if self.hop_counter >= HOP && self.filled >= n {
                self.hop_counter = 0;
                let f = self.detect();
                self.handle.set(f);
            }
        }
    }

    fn reset(&mut self) {
        self.win.fill(0.0);
        self.wpos = 0;
        self.filled = 0;
        self.hop_counter = 0;
        self.handle.set(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn run_sine(freq: f32) -> f32 {
        let h = PitchHandle::new();
        let mut t = Tuner::new(SharedParam::new(1.0), h.clone());
        t.prepare(48_000.0, 1024);
        // 餵足夠長的正弦讓窗填滿且觸發數次偵測
        let mut buf: Vec<f32> = (0..8192)
            .map(|i| 0.5 * (TAU * freq * i as f32 / 48_000.0).sin())
            .collect();
        t.process(&mut buf);
        h.read()
    }

    #[test]
    fn detects_a3_220hz() {
        let f = run_sine(220.0);
        assert!((f - 220.0).abs() < 3.0, "偵測={f}");
    }

    #[test]
    fn detects_low_e_82hz() {
        let f = run_sine(82.41); // 低音 E 弦
        assert!((f - 82.41).abs() < 2.0, "偵測={f}");
    }

    #[test]
    fn silence_reports_no_pitch() {
        let h = PitchHandle::new();
        let mut t = Tuner::new(SharedParam::new(1.0), h.clone());
        t.prepare(48_000.0, 1024);
        let mut buf = vec![0.0f32; 8192];
        t.process(&mut buf);
        assert_eq!(h.read(), 0.0);
    }

    #[test]
    fn passes_signal_through_unchanged() {
        let h = PitchHandle::new();
        let mut t = Tuner::new(SharedParam::new(1.0), h);
        t.prepare(48_000.0, 16);
        let mut buf = [0.1f32, -0.4, 0.7, -0.2, 0.9];
        let before = buf;
        t.process(&mut buf);
        assert_eq!(buf, before); // 唯讀，不改訊號
    }

    #[test]
    fn disabled_does_not_detect() {
        let h = PitchHandle::new();
        let mut t = Tuner::new(SharedParam::new(0.0), h.clone());
        t.prepare(48_000.0, 1024);
        let mut buf: Vec<f32> = (0..8192)
            .map(|i| 0.5 * (TAU * 220.0 * i as f32 / 48_000.0).sin())
            .collect();
        t.process(&mut buf);
        assert_eq!(h.read(), 0.0); // 關閉時不寫
    }
}
