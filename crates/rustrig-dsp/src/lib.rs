//! RustRig 純 DSP 核心。
//!
//! 這個 crate **不依賴任何 OS API**，可離線單元測試、未來能直接包成 VST3。
//! 所有即時規則都圍繞一條鐵律：[`AudioProcessor::process`] 在音訊執行緒呼叫，
//! **禁止** allocation、lock、syscall、println。記憶體一律在
//! [`AudioProcessor::prepare`] 配置完。

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// 即時音訊處理單元。每個效果（EQ / 壓縮 / 破音 / cab / delay …）都實作它。
///
/// 串接成訊號鏈時，順序在 `prepare` 階段固定，`process` 階段零配置。
pub trait AudioProcessor: Send {
    /// 啟動／取樣率改變時呼叫一次。
    ///
    /// * `sample_rate` — Hz（如 48000.0）
    /// * `max_block` — 單次 `process` 會餵進來的最大 frame 數，用來預配置緩衝
    fn prepare(&mut self, sample_rate: f32, max_block: usize);

    /// 即時處理一個 block（單聲道、in-place）。
    ///
    /// **零 alloc / 零 lock。** `buf.len()` 不會超過 `prepare` 時的 `max_block`。
    fn process(&mut self, buf: &mut [f32]);

    /// 清空內部狀態（delay line、濾波器記憶、reverb 尾音）。預設 no-op。
    fn reset(&mut self) {}
}

/// 直通——不改動訊號。P0 用來驗證「吉他 → 引擎 → 喇叭」鏈路本身是通的。
#[derive(Default)]
pub struct Passthrough;

impl AudioProcessor for Passthrough {
    fn prepare(&mut self, _sample_rate: f32, _max_block: usize) {}
    fn process(&mut self, _buf: &mut [f32]) {}
}

/// 把一串 processor 接成固定順序的訊號鏈。
///
/// 本身也是一個 [`AudioProcessor`]，所以鏈可以巢狀。建構在非即時路徑完成，
/// `process` 只是依序呼叫，零配置。
#[derive(Default)]
pub struct Chain {
    stages: Vec<Box<dyn AudioProcessor>>,
}

impl Chain {
    pub fn new() -> Self {
        Self { stages: Vec::new() }
    }

    /// 在鏈尾加一個 stage。只在建構期（非即時）呼叫。
    pub fn push(&mut self, stage: Box<dyn AudioProcessor>) -> &mut Self {
        self.stages.push(stage);
        self
    }
}

impl AudioProcessor for Chain {
    fn prepare(&mut self, sample_rate: f32, max_block: usize) {
        for s in &mut self.stages {
            s.prepare(sample_rate, max_block);
        }
    }

    fn process(&mut self, buf: &mut [f32]) {
        for s in &mut self.stages {
            s.process(buf);
        }
    }

    fn reset(&mut self) {
        for s in &mut self.stages {
            s.reset();
        }
    }
}

/// Lock-free f32 參數：GUI 執行緒寫、音訊執行緒讀，雙方都不會 block。
///
/// f32 以位元存進 `AtomicU32`，clone 共享同一個值。
#[derive(Clone)]
pub struct SharedParam(Arc<AtomicU32>);

impl SharedParam {
    pub fn new(v: f32) -> Self {
        Self(Arc::new(AtomicU32::new(v.to_bits())))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// 峰值表的共享讀數：音訊執行緒累積峰值，GUI 取走並歸零。
#[derive(Clone, Default)]
pub struct MeterHandle(Arc<AtomicU32>);

impl MeterHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// GUI 端：取出自上次呼叫以來的峰值並歸零。
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }

    /// 音訊端：併入一個 block 的峰值。
    /// 非負 IEEE float 的位元序與數值序一致，可直接用整數 `fetch_max`。
    fn accumulate(&self, peak: f32) {
        self.0.fetch_max(peak.to_bits(), Ordering::Relaxed);
    }
}

/// 音量。目標值來自 [`SharedParam`]（線性增益），內部做 10ms 一階平滑，
/// 轉旋鈕／拉 fader 時不會有 zipper noise。
pub struct Gain {
    param: SharedParam,
    current: f32,
    coeff: f32,
}

impl Gain {
    pub fn new(param: SharedParam) -> Self {
        Self {
            param,
            current: 1.0,
            coeff: 0.0,
        }
    }
}

impl AudioProcessor for Gain {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        self.coeff = (-1.0 / (0.010 * sample_rate)).exp();
        self.current = self.param.get();
    }

    fn process(&mut self, buf: &mut [f32]) {
        let target = self.param.get();
        for s in buf {
            self.current = target + (self.current - target) * self.coeff;
            *s *= self.current;
        }
    }

    fn reset(&mut self) {
        self.current = self.param.get();
    }
}

/// 峰值表。不改動訊號，只把 block 峰值寫進 [`MeterHandle`]。
pub struct PeakMeter {
    handle: MeterHandle,
}

impl PeakMeter {
    pub fn new(handle: MeterHandle) -> Self {
        Self { handle }
    }
}

impl AudioProcessor for PeakMeter {
    fn prepare(&mut self, _sample_rate: f32, _max_block: usize) {}

    fn process(&mut self, buf: &mut [f32]) {
        let mut peak = 0.0f32;
        for &s in buf.iter() {
            peak = peak.max(s.abs());
        }
        self.handle.accumulate(peak);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_is_identity() {
        let mut p = Passthrough;
        p.prepare(48_000.0, 128);
        let mut buf = [0.1, -0.5, 0.9, -1.0];
        let before = buf;
        p.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn empty_chain_is_identity() {
        let mut c = Chain::new();
        c.prepare(48_000.0, 128);
        let mut buf = [0.2, 0.4, -0.3];
        let before = buf;
        c.process(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn gain_converges_to_target() {
        let param = SharedParam::new(0.5);
        let mut g = Gain::new(param.clone());
        g.prepare(48_000.0, 4800);
        // 跑 100ms（10× 平滑時間常數）應收斂到目標增益
        let mut buf = vec![1.0f32; 4800];
        g.process(&mut buf);
        assert!((buf[4799] - 0.5).abs() < 1e-3, "尾端={}", buf[4799]);
    }

    #[test]
    fn meter_reports_block_peak_and_resets() {
        let h = MeterHandle::new();
        let mut m = PeakMeter::new(h.clone());
        m.prepare(48_000.0, 8);
        let mut buf = [0.1, -0.7, 0.3, 0.0];
        m.process(&mut buf);
        assert_eq!(buf, [0.1, -0.7, 0.3, 0.0]); // 不改訊號
        assert!((h.take_peak() - 0.7).abs() < 1e-6);
        assert_eq!(h.take_peak(), 0.0); // 取走即歸零
    }
}
