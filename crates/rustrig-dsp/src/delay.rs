//! 延遲（delay / echo）。
//!
//! 單聲道回授延遲線，擺在 **cab 之後、殘響之前**（時間系效果在音箱聲之後才合理）。
//! 回授路徑上掛一階低通做 analog 風阻尼（每次回授高頻略減，重複聲變暗、不刺）。
//!
//! 延遲時間以 ms 餵進來（BPM 同步在 GUI 端換算後寫入同一個 `time_ms`，DSP 不需知道
//! 拍速）。改延遲時間時對「目標長度」做一階平滑 → 不爆 click（過渡會有 tape 式微微
//! 變調，屬正常聽感）。讀取點用線性內插取分數延遲。緩衝在 prepare 配置，process 零配置。

use crate::{AudioProcessor, SharedParam};

/// 最大延遲時間（秒）。緩衝按此 × 取樣率配置。
const MAX_DELAY_SEC: f32 = 2.0;
/// 回授路徑阻尼（一階低通係數，越大越暗）。
const FB_DAMP: f32 = 0.35;

/// 延遲。`time_ms` 延遲時間、`feedback` 0..0.95 回授、`mix` 0..1 濕量、`enabled` >0.5 開。
pub struct Delay {
    pub time_ms: SharedParam,
    pub feedback: SharedParam,
    pub mix: SharedParam,
    pub enabled: SharedParam,

    sr: f32,
    buf: Vec<f32>,
    pos: usize,
    cur_delay: f32, // 平滑後的目前延遲（samples）
    smooth: f32,    // 延遲長度平滑係數（~50ms）
    fb_lp: f32,     // 回授低通記憶
}

impl Delay {
    pub fn new(
        time_ms: SharedParam,
        feedback: SharedParam,
        mix: SharedParam,
        enabled: SharedParam,
    ) -> Self {
        Self {
            time_ms,
            feedback,
            mix,
            enabled,
            sr: 48_000.0,
            buf: Vec::new(),
            pos: 0,
            cur_delay: 0.0,
            smooth: 0.0,
            fb_lp: 0.0,
        }
    }
}

impl AudioProcessor for Delay {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        self.sr = sample_rate;
        let len = (MAX_DELAY_SEC * sample_rate) as usize + 4;
        self.buf = vec![0.0; len];
        self.pos = 0;
        self.smooth = (-1.0 / (0.050 * sample_rate)).exp();
        self.cur_delay = (self.time_ms.get() / 1000.0 * sample_rate).clamp(1.0, len as f32 - 2.0);
        self.fb_lp = 0.0;
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            return;
        }
        let mix = self.mix.get().clamp(0.0, 1.0);
        let fb = self.feedback.get().clamp(0.0, 0.95);
        let len = self.buf.len();
        if len < 4 {
            return;
        }
        let target = (self.time_ms.get() / 1000.0 * self.sr).clamp(1.0, len as f32 - 2.0);

        for s in buf.iter_mut() {
            let x = *s;
            // 平滑趨近目標延遲長度（避免硬切 click）
            self.cur_delay = target + (self.cur_delay - target) * self.smooth;
            // 分數延遲讀取點（線性內插）
            let mut rp = self.pos as f32 - self.cur_delay;
            if rp < 0.0 {
                rp += len as f32;
            }
            let i0 = rp.floor() as usize % len;
            let i1 = (i0 + 1) % len;
            let frac = rp - rp.floor();
            let delayed = self.buf[i0] * (1.0 - frac) + self.buf[i1] * frac;
            // 回授路徑阻尼（一階低通）
            self.fb_lp += FB_DAMP * (delayed - self.fb_lp);
            // 寫入：乾訊號 + 阻尼後的回授
            self.buf[self.pos] = x + self.fb_lp * fb;
            self.pos = (self.pos + 1) % len;
            // 輸出：乾訊號全保留，疊加濕訊號
            *s = x + delayed * mix;
        }
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
        self.fb_lp = 0.0;
        self.cur_delay = (self.time_ms.get() / 1000.0 * self.sr).clamp(1.0, self.buf.len() as f32 - 2.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(time_ms: f32, fb: f32, mix: f32, on: f32) -> Delay {
        Delay::new(
            SharedParam::new(time_ms),
            SharedParam::new(fb),
            SharedParam::new(mix),
            SharedParam::new(on),
        )
    }

    #[test]
    fn bypass_when_disabled() {
        let mut d = mk(200.0, 0.5, 0.5, 0.0);
        d.prepare(48_000.0, 64);
        let mut buf = [0.5f32, -0.3, 0.2, -0.1];
        let before = buf;
        d.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn echo_appears_after_delay_time() {
        // 10ms 延遲 @48k = 480 samples；脈衝後約該處應出現回聲
        let mut d = mk(10.0, 0.0, 1.0, 1.0);
        d.prepare(48_000.0, 4096);
        let mut buf = vec![0.0f32; 4096];
        buf[0] = 1.0;
        d.process(&mut buf);
        // 回聲落在 ~480 附近（平滑使起始延遲略短，給寬窗）
        let echo_region = &buf[300..700];
        assert!(echo_region.iter().any(|s| s.abs() > 0.3), "找不到回聲");
        assert!(buf.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn feedback_sustains_and_stays_bounded() {
        // 一段持續 burst（一個延遲週期長）+ 高回授：後段仍應有能量、且不發散
        let mut d = mk(5.0, 0.7, 1.0, 1.0);
        d.prepare(48_000.0, 8192);
        let mut buf = vec![0.0f32; 8192];
        for s in buf.iter_mut().take(240) {
            *s = 0.5;
        }
        d.process(&mut buf);
        assert!(buf[4000..].iter().any(|s| s.abs() > 1e-3), "回授未持續");
        assert!(buf.iter().all(|s| s.is_finite() && s.abs() < 10.0), "回授發散");
    }
}
