//! RustRig 音訊後端層。
//!
//! 把「DSP」與「OS 即時細節」隔開。對外是可插拔的 [`AudioBackend`]：
//! ASIO（好驅動吃低延遲）/ WASAPI Shared（通用預設）/ WASAPI Exclusive（fallback），
//! 都藏在同一個 trait 後面。
//!
//! **關鍵架構事實**：WASAPI 雙工沒有 single callback。capture 與 render 是
//! 兩個獨立 `IAudioClient`、兩個獨立時鐘。資料流一律是
//! `capture thread → SPSC ring → render thread(DSP 在此)`，ring 的水位
//! 就是 drift / jitter 緩衝。

#[cfg(feature = "asio")]
pub mod asio;
pub mod backend;
pub mod devices;
pub mod ring;
pub mod rt;
pub mod wasapi;

pub use backend::{
    AudioBackend, BackendError, BackendKind, LatencyInfo, RunningStream, StreamConfig,
};
pub use devices::{DeviceInfo, DeviceLists, enumerate};
pub use wasapi::{WasapiExclusive, WasapiShared};

use rustrig_dsp::AudioProcessor;

/// 依 [`BackendKind`] 開啟並啟動串流。GUI / probe 用這個工廠選後端，
/// 不必各自 match 具體型別。
pub fn open_stream(
    kind: BackendKind,
    config: StreamConfig,
    processor: Box<dyn AudioProcessor>,
) -> Result<Box<dyn RunningStream>, BackendError> {
    match kind {
        BackendKind::WasapiShared => WasapiShared::open(config)?.run(processor),
        BackendKind::WasapiExclusive => WasapiExclusive::open(config)?.run(processor),
        #[cfg(feature = "asio")]
        BackendKind::Asio => asio::AsioBackend::open(config)?.run(processor),
        #[cfg(not(feature = "asio"))]
        BackendKind::Asio => Err(BackendError::Os(
            "ASIO 後端未編譯（請以 --features asio 重新編譯）".into(),
        )),
    }
}

/// 列出可用的 ASIO 驅動名稱。未以 `--features asio` 編譯時回空清單。
pub fn asio_driver_names() -> Vec<String> {
    #[cfg(feature = "asio")]
    {
        asio::driver_names()
    }
    #[cfg(not(feature = "asio"))]
    {
        Vec::new()
    }
}
