//! 殘響（mono Freeverb 風格）。
//!
//! 8 條並聯 comb filter（含阻尼回授）疊加 → 4 條串聯 allpass 擴散，最後與乾訊號
//! 混合。經典 Schroeder/Freeverb 結構，單聲道、效率高、適合吉他房間/板式殘響。
//! 所有延遲線在 prepare 配置（非即時），process 內零配置。
//!
//! 演算法 reverb（非卷積）：cab 用的卷積引擎適合「真實 IR」，但通用殘響用遞迴
//! 網路更省、更好調。延遲調音取自 Freeverb（44.1k 基準），其他取樣率按比例縮放。

use crate::{AudioProcessor, SharedParam};

/// Freeverb comb 延遲調音（samples @ 44.1kHz）。
const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
/// Freeverb allpass 延遲調音（samples @ 44.1kHz）。
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];

/// 含一階阻尼的回授 comb filter。
struct Comb {
    buf: Vec<f32>,
    pos: usize,
    store: f32,
    feedback: f32,
    damp: f32,
}

impl Comb {
    fn new(len: usize, feedback: f32, damp: f32) -> Self {
        Self {
            buf: vec![0.0; len.max(1)],
            pos: 0,
            store: 0.0,
            feedback,
            damp,
        }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.buf[self.pos];
        // 回授路徑上的一階低通（阻尼高頻，殘響尾巴變暗變自然）
        self.store = y * (1.0 - self.damp) + self.store * self.damp;
        self.buf[self.pos] = x + self.store * self.feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        y
    }
}

/// Schroeder allpass 擴散。
struct Allpass {
    buf: Vec<f32>,
    pos: usize,
    feedback: f32,
}

impl Allpass {
    fn new(len: usize, feedback: f32) -> Self {
        Self {
            buf: vec![0.0; len.max(1)],
            pos: 0,
            feedback,
        }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let buffed = self.buf[self.pos];
        let y = -x + buffed;
        self.buf[self.pos] = x + buffed * self.feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        y
    }
}

/// 殘響。`mix` 0..1 = 濕訊號量（0 = 全乾）。房間大小與阻尼固定為中大房間。
pub struct Reverb {
    /// 濕/乾混合 0..1
    pub mix: SharedParam,
    /// >0.5 = 開
    pub enabled: SharedParam,

    combs: Vec<Comb>,
    allpasses: Vec<Allpass>,
}

impl Reverb {
    pub fn new(mix: SharedParam, enabled: SharedParam) -> Self {
        Self {
            mix,
            enabled,
            combs: Vec::new(),
            allpasses: Vec::new(),
        }
    }
}

/// Freeverb 固定輸入增益（避免回授網路爆掉）。
const FIXED_GAIN: f32 = 0.015;

impl AudioProcessor for Reverb {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        let scale = sample_rate / 44_100.0;
        // 中大房間：roomsize 0.7 → feedback；damp 0.5。
        let roomsize = 0.7f32;
        let feedback = roomsize * 0.28 + 0.7; // ≈0.896
        let damp = 0.5f32 * 0.4; // Freeverb scaledamp

        self.combs = COMB_TUNING
            .iter()
            .map(|&t| Comb::new(((t as f32) * scale) as usize, feedback, damp))
            .collect();
        self.allpasses = ALLPASS_TUNING
            .iter()
            .map(|&t| Allpass::new(((t as f32) * scale) as usize, 0.5))
            .collect();
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            return;
        }
        let mix = self.mix.get().clamp(0.0, 1.0);
        if mix < 0.005 {
            return; // 全乾：直接 pass，省去殘響運算
        }
        // 乾訊號保持滿，濕訊號疊加（最高 100% 也只到 0.6 倍，避免糊掉主音色）
        let wet = mix * 0.6;

        for s in buf.iter_mut() {
            let input = *s * FIXED_GAIN;
            // 並聯 comb 疊加
            let mut acc = 0.0;
            for c in &mut self.combs {
                acc += c.process(input);
            }
            // 串聯 allpass 擴散
            for a in &mut self.allpasses {
                acc = a.process(acc);
            }
            *s += acc * wet;
        }
    }

    fn reset(&mut self) {
        for c in &mut self.combs {
            c.buf.fill(0.0);
            c.pos = 0;
            c.store = 0.0;
        }
        for a in &mut self.allpasses {
            a.buf.fill(0.0);
            a.pos = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_when_mix_zero() {
        let mut r = Reverb::new(SharedParam::new(0.0), SharedParam::new(1.0));
        r.prepare(48_000.0, 256);
        let mut buf = [0.5f32, -0.3, 0.2, -0.1];
        let before = buf;
        r.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn produces_tail_after_impulse() {
        let mut r = Reverb::new(SharedParam::new(1.0), SharedParam::new(1.0));
        r.prepare(48_000.0, 8192);
        let mut buf = vec![0.0f32; 8192];
        buf[0] = 1.0; // 脈衝
        r.process(&mut buf);
        // 脈衝之後應有殘響尾巴（非靜音）
        assert!(buf[4000..].iter().any(|s| s.abs() > 1e-4));
        assert!(buf.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn bypass_when_disabled() {
        let mut r = Reverb::new(SharedParam::new(1.0), SharedParam::new(0.0));
        r.prepare(48_000.0, 16);
        let mut buf = [0.5f32; 16];
        let before = buf;
        r.process(&mut buf);
        assert_eq!(buf, before);
    }
}
