//! 後端抽象：所有音訊 I/O 後端共用的型別與 trait。

use rustrig_dsp::AudioProcessor;

/// 串流參數。`block_size` 是 **DSP 端看到的固定 block**，與裝置實際 period
/// 解耦（中間靠 re-blocking 緩衝橋接），讓 FFT 階段永遠拿到 2 的次方大小。
#[derive(Clone, Debug)]
pub struct StreamConfig {
    pub sample_rate: u32,
    pub block_size: usize,
    pub channels: u16,
    /// 擷取裝置 ID（見 [`crate::devices::enumerate`]）。`None` = 系統預設。
    pub capture_id: Option<String>,
    /// 輸出裝置 ID。`None` = 系統預設。
    pub render_id: Option<String>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            block_size: 128,
            channels: 1,
            capture_id: None,
            render_id: None,
        }
    }
}

/// 延遲資訊。**注意**：驅動回報值（如 `GetStreamLatency`）常不準，
/// 真實 RTL 以 loopback 探針實測為準（見 `probe`）。
#[derive(Clone, Copy, Debug)]
pub struct LatencyInfo {
    pub capture_frames: u32,
    pub render_frames: u32,
    /// ring 預灌的起始水位（frames）。預灌的靜音是真實訊號路徑延遲的一部分，
    /// 不算進來會低估 RTL。
    pub ring_frames: u32,
    pub sample_rate: u32,
}

impl LatencyInfo {
    /// 後端緩衝貢獻的延遲（ms），不含驅動內部與 DA/AD 轉換。
    pub fn buffer_ms(&self) -> f32 {
        (self.capture_frames + self.render_frames + self.ring_frames) as f32
            / self.sample_rate as f32
            * 1000.0
    }
}

/// 可選的音訊後端種類（給 UI 下拉與 [`crate::open_stream`] 工廠用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendKind {
    /// WASAPI 共享：通用、相容性最好；但延遲受 Windows 音訊引擎 period 限制
    /// （多數驅動鎖在 ~10ms），無法做到吉他級低延遲。
    WasapiShared,
    /// WASAPI 獨佔：繞過共享引擎、用裝置真實最小 buffer，延遲低（個位數 ms），
    /// 代價是獨佔該輸入／輸出裝置。
    WasapiExclusive,
}

impl BackendKind {
    /// UI 顯示名稱。
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::WasapiShared => "WASAPI 共享",
            BackendKind::WasapiExclusive => "WASAPI 獨佔",
        }
    }

    /// 所有可選後端（給下拉選單列舉）。
    pub const ALL: [BackendKind; 2] = [BackendKind::WasapiShared, BackendKind::WasapiExclusive];
}

#[derive(thiserror::Error, Debug)]
pub enum BackendError {
    #[error("找不到可用的音訊裝置")]
    NoDevice,
    #[error("裝置不支援要求的格式：{0}")]
    UnsupportedFormat(String),
    #[error("裝置正被獨佔佔用（Exclusive 模式衝突）")]
    DeviceInUse,
    #[error("裝置已失效（被拔除或驅動更新）")]
    DeviceInvalidated,
    #[error("後端內部錯誤：{0}")]
    Os(String),
}

/// 一個正在跑的串流。drop 時停止並釋放裝置。
pub trait RunningStream {
    /// 目前實測到的 xrun（underrun/overflow）累計次數。
    fn xrun_count(&self) -> u64;
    /// 後端回報的緩衝延遲。
    fn latency(&self) -> LatencyInfo;
    /// 音訊執行緒是否仍在運作。`false` = 已因錯誤退出
    /// （最常見：裝置被拔除 / 驅動失效），此後 xrun 計數不再有意義。
    fn is_alive(&self) -> bool;
}

/// 可插拔音訊後端。`open` 在控制執行緒呼叫，`run` 交出 DSP processor 後，
/// 後端自行起 capture/render 執行緒並在 render 執行緒上跑 `processor.process`。
pub trait AudioBackend: Sized {
    /// 後端代號（"WASAPI-Shared" / "ASIO" …），用於記錄與 UI 顯示。
    fn name() -> &'static str;

    /// 開啟預設裝置。失敗回 [`BackendError`]，呼叫端可換後端重試。
    fn open(config: StreamConfig) -> Result<Self, BackendError>;

    /// 啟動串流。`processor` 在 render 執行緒上被即時呼叫。
    fn run(self, processor: Box<dyn AudioProcessor>)
        -> Result<Box<dyn RunningStream>, BackendError>;
}
