//! IR cab 模擬 — 分割 FFT 卷積（fft-convolver）。
//!
//! IR（喇叭箱體脈衝響應 .wav）在 `prepare` 階段重採樣到引擎取樣率、
//! 能量正規化、初始化卷積器；`process` 階段 fft-convolver 內部零 alloc。
//! 取樣率不合時用線性插值重採樣（cab IR 高頻內容有限，線性插值夠用）。

use fft_convolver::FFTConvolver;

use crate::{AudioProcessor, SharedParam};

/// IR 最長 1 秒：cab IR 通常 20–170ms，超長的多半是誤載 reverb IR，
/// 截斷以保護 CPU。
const MAX_IR_SECS: f32 = 1.0;

/// 卷積分割大小（2 的次方）。小→低延遲高 CPU，大→反之。
const PARTITION: usize = 256;

pub struct CabIr {
    /// >0.5 = 開
    pub enabled: SharedParam,

    /// 原始 IR 樣本與其取樣率（來自 wav 檔，由 app 層讀入）
    raw: Vec<f32>,
    raw_sr: u32,

    conv: FFTConvolver<f32>,
    wet: Vec<f32>,
    ready: bool,
}

impl CabIr {
    /// `raw` 為單聲道 IR 樣本，`raw_sr` 是檔案的取樣率。
    pub fn new(raw: Vec<f32>, raw_sr: u32, enabled: SharedParam) -> Self {
        Self {
            enabled,
            raw,
            raw_sr,
            conv: FFTConvolver::default(),
            wet: Vec::new(),
            ready: false,
        }
    }

    /// 線性插值重採樣 + 能量正規化 + 截斷。
    fn build_ir(&self, target_sr: f32) -> Vec<f32> {
        if self.raw.is_empty() {
            return Vec::new();
        }
        let ratio = target_sr / self.raw_sr as f32;
        let out_len = ((self.raw.len() as f32 * ratio) as usize)
            .min((target_sr * MAX_IR_SECS) as usize)
            .max(1);
        let mut ir = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let pos = i as f32 / ratio;
            let i0 = pos as usize;
            let frac = pos - i0 as f32;
            let s0 = self.raw.get(i0).copied().unwrap_or(0.0);
            let s1 = self.raw.get(i0 + 1).copied().unwrap_or(0.0);
            ir.push(s0 + (s1 - s0) * frac);
        }
        // 能量正規化：卷積後整體響度大致不變
        let energy: f32 = ir.iter().map(|s| s * s).sum();
        if energy > 1e-12 {
            let g = 1.0 / energy.sqrt();
            for s in &mut ir {
                *s *= g;
            }
        }
        ir
    }
}

impl AudioProcessor for CabIr {
    fn prepare(&mut self, sample_rate: f32, max_block: usize) {
        let ir = self.build_ir(sample_rate);
        self.ready = !ir.is_empty() && self.conv.init(PARTITION, &ir).is_ok();
        self.wet = vec![0.0; max_block];
    }

    fn process(&mut self, buf: &mut [f32]) {
        if !self.ready || self.enabled.get() < 0.5 {
            return;
        }
        let n = buf.len();
        if self.conv.process(buf, &mut self.wet[..n]).is_ok() {
            buf.copy_from_slice(&self.wet[..n]);
        }
    }

    fn reset(&mut self) {
        self.conv.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_impulse_ir_is_identity() {
        // IR = [1.0] → 卷積等於原訊號（能量正規化後仍是 1.0）
        let mut cab = CabIr::new(vec![1.0], 48_000, SharedParam::new(1.0));
        cab.prepare(48_000.0, 64);
        let mut buf: Vec<f32> = (0..64).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
        let before = buf.clone();
        cab.process(&mut buf);
        for (a, b) in buf.iter().zip(before.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn empty_ir_passes_through() {
        let mut cab = CabIr::new(Vec::new(), 48_000, SharedParam::new(1.0));
        cab.prepare(48_000.0, 64);
        let mut buf = [0.3f32, -0.3, 0.6];
        let before = buf;
        cab.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn resamples_when_rates_differ() {
        // 44.1k 的 IR 在 48k 引擎下仍要能初始化並出聲
        let ir: Vec<f32> = (0..441).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
        let mut cab = CabIr::new(ir, 44_100, SharedParam::new(1.0));
        cab.prepare(48_000.0, 128);
        let mut buf = vec![0.5f32; 128];
        cab.process(&mut buf);
        assert!(buf.iter().all(|s| s.is_finite()));
        assert!(buf.iter().any(|s| s.abs() > 0.01));
    }
}
