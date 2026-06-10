//! RustRig 純 DSP 核心。
//!
//! 這個 crate **不依賴任何 OS API**，可離線單元測試、未來能直接包成 VST3。
//! 所有即時規則都圍繞一條鐵律：[`AudioProcessor::process`] 在音訊執行緒呼叫，
//! **禁止** allocation、lock、syscall、println。記憶體一律在
//! [`AudioProcessor::prepare`] 配置完。

#![forbid(unsafe_code)]

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
}
