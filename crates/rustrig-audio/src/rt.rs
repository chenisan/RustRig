//! 即時音訊執行緒的「非可選」保護措施。
//!
//! 報告 H4 列出：MMCSS 只管排程優先級，另外還有三個 RT 殺手要處理。
//! 這裡涵蓋 denormal 防護與 MMCSS；VirtualLock 在後端配置緩衝時呼叫。

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};
use windows::core::w;

/// 在當前執行緒開啟 FTZ（flush-to-zero）+ DAZ（denormals-are-zero）。
///
/// denormal 浮點數在 reverb / IR 卷積尾端會讓 CPU 暴增 10–100×。
/// 音訊執行緒一啟動就要呼叫一次。
#[cfg(target_arch = "x86_64")]
#[allow(deprecated)] // _mm_get/setcsr 被標 deprecated，但這是設 FTZ/DAZ 最直接的方式
pub fn enable_flush_denormals() {
    use core::arch::x86_64::{_mm_getcsr, _mm_setcsr};
    // MXCSR：FTZ = bit 15 (0x8000)，DAZ = bit 6 (0x0040)
    // SAFETY: x86_64 一律有 SSE2，設定 MXCSR 控制位元是無副作用的執行緒區域操作。
    unsafe {
        _mm_setcsr(_mm_getcsr() | 0x8040);
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn enable_flush_denormals() {}

/// MMCSS「Pro Audio」執行緒登記。drop 時自動還原。
///
/// 把音訊執行緒拉進 Multimedia Class Scheduler 的 Pro Audio 類別，
/// 拿到接近即時的排程優先級。報告 H4：這是必要非可選。
pub struct ProAudioPriority {
    handle: HANDLE,
}

impl ProAudioPriority {
    /// 嘗試把**當前執行緒**登記為 Pro Audio。失敗回 `None`（非致命，照跑）。
    pub fn register() -> Option<Self> {
        let mut task_index: u32 = 0;
        // SAFETY: 傳入合法的靜態寬字串與本地 task_index 指標。
        unsafe {
            AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index)
                .ok()
                .map(|handle| Self { handle })
        }
    }
}

impl Drop for ProAudioPriority {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle 由 AvSetMmThreadCharacteristicsW 取得，僅還原一次。
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(self.handle);
            }
        }
    }
}
