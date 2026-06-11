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

pub mod backend;
pub mod devices;
pub mod ring;
pub mod rt;
pub mod wasapi;

pub use backend::{AudioBackend, BackendError, LatencyInfo, RunningStream, StreamConfig};
pub use devices::{DeviceInfo, DeviceLists, enumerate};
pub use wasapi::WasapiShared;
