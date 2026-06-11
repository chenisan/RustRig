//! 破音（dirt / overdrive）。
//!
//! 訊號流：pre-HPF（收緊低頻）→ 前級增益（DRIVE）→ **4× oversampling
//! 非對稱 soft-clip**（防 aliasing）→ TONE 一階低通 → 自動電平補償。
//!
//! 非線性一定要在升頻域做：tanh 產生的高次諧波在原生取樣率會摺疊回
//! 可聽頻段（aliasing，數位破音的「沙沙感」元兇）。4× = 兩級 halfband 2×。

use crate::{AudioProcessor, SharedParam};

/// 31-tap windowed-sinc halfband FIR（fc = fs/4），prepare 期生成。
fn halfband_taps() -> Vec<f32> {
    const N: usize = 31;
    let mid = (N / 2) as f32;
    let mut taps = vec![0.0f32; N];
    let mut sum = 0.0;
    for (i, t) in taps.iter_mut().enumerate() {
        let x = i as f32 - mid;
        // sinc(x/2)：halfband 截止在 0.25 fs
        let sinc = if x == 0.0 {
            0.5
        } else {
            (std::f32::consts::PI * x / 2.0).sin() / (std::f32::consts::PI * x)
        };
        // Blackman 窗
        let w = 0.42 - 0.5 * (std::f32::consts::TAU * i as f32 / (N - 1) as f32).cos()
            + 0.08 * (2.0 * std::f32::consts::TAU * i as f32 / (N - 1) as f32).cos();
        *t = sinc * w;
        sum += *t;
    }
    // DC 增益正規化為 1
    for t in &mut taps {
        *t /= sum;
    }
    taps
}

/// 簡單 ring-state FIR，逐樣本。
struct Fir {
    taps: Vec<f32>,
    ring: Vec<f32>,
    pos: usize,
}

impl Fir {
    fn new(taps: Vec<f32>) -> Self {
        let n = taps.len();
        Self { taps, ring: vec![0.0; n], pos: 0 }
    }
    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        self.ring[self.pos] = x;
        let n = self.ring.len();
        let mut acc = 0.0;
        let mut idx = self.pos;
        for &t in &self.taps {
            acc += t * self.ring[idx];
            idx = if idx == 0 { n - 1 } else { idx - 1 };
        }
        self.pos = (self.pos + 1) % n;
        acc
    }
    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
    }
}

/// 一級 2× oversampler（上下各一條 halfband）。
struct Os2x {
    up: Fir,
    down: Fir,
}

impl Os2x {
    fn new() -> Self {
        Self { up: Fir::new(halfband_taps()), down: Fir::new(halfband_taps()) }
    }
    /// zero-stuff + 濾波（×2 補增益）。
    #[inline]
    fn upsample(&mut self, x: f32) -> [f32; 2] {
        [2.0 * self.up.tick(x), 2.0 * self.up.tick(0.0)]
    }
    /// 濾波 + 隔點抽取。
    #[inline]
    fn downsample(&mut self, a: f32, b: f32) -> f32 {
        let y = self.down.tick(a);
        self.down.tick(b);
        y
    }
    fn reset(&mut self) {
        self.up.reset();
        self.down.reset();
    }
}

/// 非對稱 soft-clip：正半 tanh、負半較快進入飽和 → 帶偶次諧波，
/// 比對稱 tanh 更接近真空管的不對稱壓縮感。
#[inline]
fn shape(x: f32) -> f32 {
    if x >= 0.0 { x.tanh() } else { (1.5 * x).tanh() / 1.5 }
}

/// 破音效果。參數全部 lock-free（GUI 寫、音訊執行緒讀）。
pub struct Drive {
    /// 前級增益 dB（0–40）
    pub drive_db: SharedParam,
    /// TONE 低通截止 Hz（800–8000）
    pub tone_hz: SharedParam,
    /// >0.5 = 開
    pub enabled: SharedParam,

    os1: Os2x,
    os2: Os2x,
    sr: f32,
    // 一階濾波器狀態
    hpf_y: f32,
    hpf_x: f32,
    hpf_a: f32,
    lpf_y: f32,
}

impl Drive {
    pub fn new(drive_db: SharedParam, tone_hz: SharedParam, enabled: SharedParam) -> Self {
        Self {
            drive_db,
            tone_hz,
            enabled,
            os1: Os2x::new(),
            os2: Os2x::new(),
            sr: 48_000.0,
            hpf_y: 0.0,
            hpf_x: 0.0,
            hpf_a: 0.0,
            lpf_y: 0.0,
        }
    }

    #[inline]
    fn one_pole_coeff(sr: f32, hz: f32) -> f32 {
        (-std::f32::consts::TAU * hz / sr).exp()
    }
}

impl AudioProcessor for Drive {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        self.sr = sample_rate;
        // pre-HPF 固定 120 Hz：進破音前收掉低頻轟隆，gain 高才不會糊
        self.hpf_a = Self::one_pole_coeff(sample_rate, 120.0);
        self.reset();
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            return;
        }
        let pre = 10f32.powf(self.drive_db.get() / 20.0);
        // 自動補償：drive 越大輸出修剪越多（取一半斜率，保留一點「推起來變大聲」的手感）
        let makeup = 10f32.powf(-self.drive_db.get() / 40.0);
        let lpf_a = Self::one_pole_coeff(self.sr, self.tone_hz.get().clamp(800.0, 8000.0));

        for s in buf {
            // 一階 HPF（y = a*(y_prev + x - x_prev)）
            let x = *s;
            self.hpf_y = self.hpf_a * (self.hpf_y + x - self.hpf_x);
            self.hpf_x = x;

            let driven = self.hpf_y * pre;

            // 4×：兩級 2× 升頻 → 非線性 → 兩級降頻
            let [a, b] = self.os1.upsample(driven);
            let [a1, a2] = self.os2.upsample(a);
            let [b1, b2] = self.os2.upsample(b);
            let da = self.os2.downsample(shape(a1), shape(a2));
            let db = self.os2.downsample(shape(b1), shape(b2));
            let clipped = self.os1.downsample(da, db);

            // TONE 一階 LPF
            self.lpf_y = lpf_a * self.lpf_y + (1.0 - lpf_a) * clipped;

            *s = self.lpf_y * makeup;
        }
    }

    fn reset(&mut self) {
        self.os1.reset();
        self.os2.reset();
        self.hpf_y = 0.0;
        self.hpf_x = 0.0;
        self.lpf_y = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_in_silence_out() {
        let mut d = Drive::new(
            SharedParam::new(20.0),
            SharedParam::new(4000.0),
            SharedParam::new(1.0),
        );
        d.prepare(48_000.0, 256);
        let mut buf = vec![0.0f32; 256];
        d.process(&mut buf);
        assert!(buf.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn bypass_leaves_signal_untouched() {
        let mut d = Drive::new(
            SharedParam::new(20.0),
            SharedParam::new(4000.0),
            SharedParam::new(0.0), // off
        );
        d.prepare(48_000.0, 8);
        let mut buf = [0.5f32, -0.5, 0.25, -0.25];
        let before = buf;
        d.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn output_is_bounded_even_at_max_drive() {
        let mut d = Drive::new(
            SharedParam::new(40.0),
            SharedParam::new(8000.0),
            SharedParam::new(1.0),
        );
        d.prepare(48_000.0, 4096);
        // 滿振幅正弦灌進去，輸出必須有限且不爆表
        let mut buf: Vec<f32> = (0..4096)
            .map(|i| (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin())
            .collect();
        d.process(&mut buf);
        assert!(buf.iter().all(|s| s.is_finite() && s.abs() <= 1.5));
        // 而且不是靜音
        assert!(buf[2048..].iter().any(|s| s.abs() > 0.01));
    }
}
