//! WASAPI Shared 後端（windows crate 自刻）。
//!
//! P0 設計取捨：用**單一音訊執行緒**同時跑 capture（輪詢）與 render（event 驅動），
//! 兩端共用一條 SPSC ring。這樣 P0 先把「吉他進→直通→喇叭出」打通並量到真實 RTL，
//! 避開雙執行緒時鐘同步的複雜度。UCX II 的 in/out 同一實體晶振，drift 極小；
//! 不同裝置的 drift 由 ring 水位吸收並計入 xrun。
//!
//! 低延遲：用 IAudioClient3 `InitializeSharedAudioStream` 取 engine 最小 period
//! 把共享模式延遲壓到驅動下限（驅動不支援時自動退回 v1 預設 period）。
//! 後續升級點：Exclusive 模式、ASIO（UCX II 走 ASIO 可再下探到 3–5ms）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::{self, JoinHandle};

use rtrb::Producer;
use rustrig_dsp::AudioProcessor;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, EDataFlow, IAudioCaptureClient, IAudioClient, IAudioClient3,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Error as WinError, Interface, PCWSTR, Result as WinResult};

use crate::backend::{AudioBackend, BackendError, LatencyInfo, RunningStream, StreamConfig};
use crate::ring;
use crate::rt;

/// IEEE float32 的 WAVEFORMATEXTENSIBLE SubFormat GUID（避開不確定的常數 import）。
const SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);
/// PCM 整數的 WAVEFORMATEXTENSIBLE SubFormat GUID（獨佔模式格式協商用）。
const SUBTYPE_PCM: windows::core::GUID =
    windows::core::GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// 樣本格式：DSP 內部一律 f32，但裝置（尤其獨佔模式）可能要整數。
#[derive(Clone, Copy, PartialEq, Debug)]
enum SampleKind {
    F32,
    I32,
    /// 24-bit 裝在 32-bit 容器（RME 等常見）。轉換與 I32 相同（容器都是 i32）。
    I24in32,
    I16,
}

impl SampleKind {
    /// 每個樣本的容器位元組數。
    fn container_bytes(self) -> usize {
        match self {
            SampleKind::I16 => 2,
            _ => 4,
        }
    }
}

/// 從 mix format 取出我們需要的欄位。
#[derive(Clone, Copy)]
struct Fmt {
    channels: usize,
    sample_rate: u32,
    /// 每 frame 位元組數（= channels × container_bytes）。
    block_align: usize,
    /// 裝置樣本格式（共享模式恆為 F32；獨佔模式由協商決定）。
    kind: SampleKind,
}

/// 判斷 mix format 是不是 32-bit IEEE float（P0 只支援這個，現代 Windows mix 幾乎都是）。
unsafe fn is_float32(pwfx: *const WAVEFORMATEX) -> bool {
    let w = unsafe { &*pwfx };
    if w.wBitsPerSample != 32 {
        return false;
    }
    match w.wFormatTag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => {
            // WAVEFORMATEXTENSIBLE 是 packed struct，不能取欄位參考，須 read_unaligned
            let ext = pwfx as *const WAVEFORMATEXTENSIBLE;
            let sub = unsafe { std::ptr::addr_of!((*ext).SubFormat).read_unaligned() };
            sub == SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    }
}

unsafe fn read_fmt(pwfx: *const WAVEFORMATEX) -> Fmt {
    let w = unsafe { &*pwfx };
    Fmt {
        channels: w.nChannels as usize,
        sample_rate: w.nSamplesPerSec,
        block_align: w.nBlockAlign as usize,
        // 共享路徑前面已用 is_float32 確認過；獨佔路徑不走這裡（自建 Fmt）。
        kind: SampleKind::F32,
    }
}

/// 建一個 WAVEFORMATEXTENSIBLE（獨佔模式格式協商用）。
fn make_wfx_ext(
    sample_rate: u32,
    channels: u16,
    container_bits: u16,
    valid_bits: u16,
    float: bool,
) -> WAVEFORMATEXTENSIBLE {
    let block_align = channels * (container_bits / 8);
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: container_bits,
            cbSize: 22, // sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)
        },
        Samples: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: if channels <= 1 { 0x4 } else { 0x3 }, // FRONT_CENTER / FL|FR
        SubFormat: if float { SUBTYPE_IEEE_FLOAT } else { SUBTYPE_PCM },
    }
}

/// 獨佔模式格式協商：依序探測 float32 → int32 → int24-in-32 → int16，
/// 回第一個裝置接受的（WAVEFORMATEXTENSIBLE + 對應 [`SampleKind`]）。
unsafe fn negotiate_exclusive(
    client: &IAudioClient,
    sample_rate: u32,
    channels: u16,
) -> Option<(WAVEFORMATEXTENSIBLE, SampleKind)> {
    // (kind, 容器 bits, valid bits, 是否 float)
    let candidates = [
        (SampleKind::F32, 32u16, 32u16, true),
        (SampleKind::I32, 32, 32, false),
        (SampleKind::I24in32, 32, 24, false),
        (SampleKind::I16, 16, 16, false),
    ];
    for (kind, cbits, vbits, float) in candidates {
        let wfx = make_wfx_ext(sample_rate, channels, cbits, vbits, float);
        let hr = unsafe {
            client.IsFormatSupported(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                &wfx as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX,
                None,
            )
        };
        eprintln!(
            "[wasapi] 獨佔格式探測 {kind:?}（{vbits}bit）→ {}",
            if hr.0 == 0 { "OK" } else { "不支援" }
        );
        if hr.0 == 0 {
            return Some((wfx, kind));
        }
    }
    None
}

/// 讀裝置 buffer 第 0 聲道一個樣本 → f32（依 [`SampleKind`] 轉換）。
#[inline]
unsafe fn read_ch0(base: *const u8, frame: usize, block_align: usize, kind: SampleKind) -> f32 {
    let p = unsafe { base.add(frame * block_align) };
    match kind {
        SampleKind::F32 => unsafe { *(p as *const f32) },
        SampleKind::I32 | SampleKind::I24in32 => {
            unsafe { *(p as *const i32) as f32 / 2_147_483_648.0 }
        }
        SampleKind::I16 => unsafe { *(p as *const i16) as f32 / 32_768.0 },
    }
}

/// 把一個 f32 樣本寫到裝置 buffer 的 frame 全聲道（依 [`SampleKind`] 轉換）。
#[inline]
unsafe fn write_all_ch(
    base: *mut u8,
    frame: usize,
    channels: usize,
    block_align: usize,
    kind: SampleKind,
    v: f32,
) {
    let v = v.clamp(-1.0, 1.0);
    let frame_ptr = unsafe { base.add(frame * block_align) };
    let cbytes = kind.container_bytes();
    for c in 0..channels {
        let p = unsafe { frame_ptr.add(c * cbytes) };
        match kind {
            SampleKind::F32 => unsafe { *(p as *mut f32) = v },
            SampleKind::I32 | SampleKind::I24in32 => unsafe {
                *(p as *mut i32) = (v as f64 * 2_147_483_647.0) as i32
            },
            SampleKind::I16 => unsafe { *(p as *mut i16) = (v * 32_767.0) as i16 },
        }
    }
}

/// WASAPI Shared 後端。
pub struct WasapiShared {
    config: StreamConfig,
}

impl AudioBackend for WasapiShared {
    fn name() -> &'static str {
        "WASAPI-Shared"
    }

    fn open(config: StreamConfig) -> Result<Self, BackendError> {
        Ok(Self { config })
    }

    fn run(
        self,
        processor: Box<dyn AudioProcessor>,
    ) -> Result<Box<dyn RunningStream>, BackendError> {
        start_stream(self.config, processor, run_inner)
    }
}

/// WASAPI 獨佔模式後端。繞過共享音訊引擎，用裝置真實最小 buffer 換取個位數 ms
/// 延遲——代價是**獨佔裝置**（其他程式無法同時使用該輸入／輸出）。
pub struct WasapiExclusive {
    config: StreamConfig,
}

impl AudioBackend for WasapiExclusive {
    fn name() -> &'static str {
        "WASAPI-Exclusive"
    }

    fn open(config: StreamConfig) -> Result<Self, BackendError> {
        Ok(Self { config })
    }

    fn run(
        self,
        processor: Box<dyn AudioProcessor>,
    ) -> Result<Box<dyn RunningStream>, BackendError> {
        start_stream(self.config, processor, run_inner_exclusive)
    }
}

/// 音訊執行緒主體簽名：共享或獨佔各自實作初始化 + 即時迴圈。
type InnerFn = unsafe fn(
    StreamConfig,
    &mut dyn AudioProcessor,
    &AtomicBool,
    &AtomicU64,
    &Sender<Result<LatencyInfo, BackendError>>,
) -> WinResult<()>;

/// 後端共用的串流啟動骨架：起音訊執行緒、等初始化回報、組 [`WasapiStream`]。
/// `inner` 決定共享（[`run_inner`]）或獨佔（[`run_inner_exclusive`]）的行為。
fn start_stream(
    config: StreamConfig,
    processor: Box<dyn AudioProcessor>,
    inner: InnerFn,
) -> Result<Box<dyn RunningStream>, BackendError> {
    let stop = Arc::new(AtomicBool::new(false));
    let xruns = Arc::new(AtomicU64::new(0));
    let alive = Arc::new(AtomicBool::new(true));
    let (ready_tx, ready_rx) = channel::<Result<LatencyInfo, BackendError>>();

    let stop_t = stop.clone();
    let xruns_t = xruns.clone();
    let alive_t = alive.clone();
    let join: JoinHandle<()> = thread::spawn(move || {
        let mut processor = processor;
        if let Err(e) = audio_thread(config, processor.as_mut(), &stop_t, &xruns_t, &ready_tx, inner)
        {
            // 若初始化階段就失敗，ready_rx 還在等，把錯誤送回去；
            // 若已過初始化（recv 已完成），這個 send 沒人收，靠 alive 旗標通知
            let _ = ready_tx.send(Err(BackendError::Os(format!("{e}"))));
        }
        alive_t.store(false, Ordering::Release);
    });

    match ready_rx.recv() {
        Ok(Ok(latency)) => Ok(Box::new(WasapiStream {
            stop,
            xruns,
            alive,
            latency,
            join: Some(join),
        })),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(BackendError::Os("音訊執行緒未回報初始化結果".into()))
        }
    }
}

/// 正在跑的 WASAPI 串流。drop 時通知音訊執行緒停止並 join。
struct WasapiStream {
    stop: Arc<AtomicBool>,
    xruns: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    latency: LatencyInfo,
    join: Option<JoinHandle<()>>,
}

impl RunningStream for WasapiStream {
    fn xrun_count(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }
    fn latency(&self) -> LatencyInfo {
        self.latency
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 音訊執行緒主體。初始化成功後透過 `ready` 回報延遲，再進即時迴圈。
/// 共享／獨佔的差異全在 `inner`。
fn audio_thread(
    config: StreamConfig,
    processor: &mut dyn AudioProcessor,
    stop: &AtomicBool,
    xruns: &AtomicU64,
    ready: &Sender<Result<LatencyInfo, BackendError>>,
    inner: InnerFn,
) -> WinResult<()> {
    // 即時保護：denormal 歸零 + Pro Audio 排程優先級
    rt::enable_flush_denormals();
    let _priority = rt::ProAudioPriority::register();

    // SAFETY: 整段在本執行緒內初始化、使用、銷毀所有 COM 物件，不跨執行緒。
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let result = unsafe { inner(config, processor, stop, xruns, ready) };
    unsafe { CoUninitialize() };
    result
}

/// 依 ID 開裝置；`None` 用該方向的系統預設。
unsafe fn get_device(
    enumerator: &IMMDeviceEnumerator,
    id: Option<&str>,
    flow: EDataFlow,
) -> WinResult<IMMDevice> {
    match id {
        Some(id) => {
            let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
        }
        None => unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) },
    }
}

/// 一個初始化好的 client：base `IAudioClient`（IAudioClient3 經 cast 取得，
/// base 方法經繼承照用）、mix format、buffer 大小（frames）。實際採用的 engine
/// period 在 [`init_low_latency`] 內以 eprintln 回報。
struct InitedClient {
    client: IAudioClient,
    fmt: Fmt,
    buffer_frames: u32,
}

/// v1 預設 period 初始化（fallback 路徑）：重新 Activate 一顆乾淨 client，
/// 用 `Initialize` 走引擎預設 period。`pwfx` 由呼叫端負責釋放。
unsafe fn init_v1(
    dev: &IMMDevice,
    pwfx: *const WAVEFORMATEX,
    stream_flags: u32,
) -> WinResult<IAudioClient> {
    let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, stream_flags, 0, 0, pwfx, None)? };
    Ok(client)
}

/// 低延遲初始化：優先 `IAudioClient3` + `InitializeSharedAudioStream`（engine 最小
/// period，把共享模式延遲壓到驅動允許的下限）；任一步失敗就退回 v1 預設 period。
/// `stream_flags`：capture 傳 0（輪詢），render 傳 EVENTCALLBACK。
unsafe fn init_low_latency(dev: &IMMDevice, stream_flags: u32) -> WinResult<InitedClient> {
    let client3: IAudioClient3 = unsafe { dev.Activate(CLSCTX_ALL, None)? };
    let pwfx = unsafe { client3.GetMixFormat()? };
    if !unsafe { is_float32(pwfx) } {
        unsafe { CoTaskMemFree(Some(pwfx as *const _)) };
        return Err(WinError::new(
            windows::core::HRESULT(-1),
            "裝置非 float32 mix format",
        ));
    }
    let fmt = unsafe { read_fmt(pwfx) };

    // 查驅動允許的 engine period 範圍（frames）。
    let mut def = 0u32;
    let mut fund = 0u32;
    let mut min = 0u32;
    let mut max = 0u32;
    let period_query =
        unsafe { client3.GetSharedModeEnginePeriod(pwfx, &mut def, &mut fund, &mut min, &mut max) };

    let (client, period_frames) = match period_query {
        // 用最小 period 起最低延遲；若驅動拒絕就退回 v1 預設。
        Ok(()) => match unsafe { client3.InitializeSharedAudioStream(stream_flags, min, pwfx, None) }
        {
            Ok(()) => {
                eprintln!(
                    "[wasapi] IAudioClient3 低延遲：period={min} frames (def={def} min={min} max={max} fund={fund})"
                );
                (client3.cast::<IAudioClient>()?, min)
            }
            Err(e) => {
                eprintln!("[wasapi] InitializeSharedAudioStream 失敗（{e}）→ 退回 v1 預設 period");
                (unsafe { init_v1(dev, pwfx, stream_flags)? }, def)
            }
        },
        Err(e) => {
            eprintln!("[wasapi] GetSharedModeEnginePeriod 失敗（{e}）→ 退回 v1 預設 period");
            (unsafe { init_v1(dev, pwfx, stream_flags)? }, def)
        }
    };

    let _ = period_frames; // 已於上方 eprintln 回報，這裡不再用
    unsafe { CoTaskMemFree(Some(pwfx as *const _)) };
    let buffer_frames = unsafe { client.GetBufferSize()? };
    Ok(InitedClient {
        client,
        fmt,
        buffer_frames,
    })
}

unsafe fn run_inner(
    config: StreamConfig, // 取樣率/block 由 mix format 決定；裝置 ID 與低延遲 period 在此生效
    processor: &mut dyn AudioProcessor,
    stop: &AtomicBool,
    xruns: &AtomicU64,
    ready: &Sender<Result<LatencyInfo, BackendError>>,
) -> WinResult<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    // ── capture（吉他輸入）：poll 模式，不掛 event ──
    let cap_dev = unsafe { get_device(&enumerator, config.capture_id.as_deref(), eCapture)? };
    let InitedClient {
        client: cap_client,
        fmt: cap_fmt,
        buffer_frames: cap_frames,
    } = unsafe { init_low_latency(&cap_dev, 0)? };
    let cap_service: IAudioCaptureClient = unsafe { cap_client.GetService()? };

    // ── render（喇叭輸出）：event 驅動 ──
    let rnd_dev = unsafe { get_device(&enumerator, config.render_id.as_deref(), eRender)? };
    let InitedClient {
        client: rnd_client,
        fmt: rnd_fmt,
        buffer_frames: rnd_frames,
    } = unsafe { init_low_latency(&rnd_dev, AUDCLNT_STREAMFLAGS_EVENTCALLBACK)? };

    // P0 限制：兩端取樣率不同需要 ASRC，暫不支援（強制同一介面即可避免）
    if cap_fmt.sample_rate != rnd_fmt.sample_rate {
        return Err(WinError::new(
            windows::core::HRESULT(-1),
            "擷取與輸出取樣率不同，P0 尚未支援 ASRC（請 in/out 用同一介面）",
        ));
    }

    let rnd_service: IAudioRenderClient = unsafe { rnd_client.GetService()? };

    // render event handle（auto-reset、初始 unsignaled）
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
    unsafe { rnd_client.SetEventHandle(event)? };

    // ── 預配置所有即時緩衝（即時迴圈內零 alloc）──
    let max_block = cap_frames.max(rnd_frames) as usize;
    processor.prepare(rnd_fmt.sample_rate as f32, max_block);

    let ring_cap = (max_block * 8).max(4096);
    let (mut producer, mut consumer) = ring::channel(ring_cap);
    let mut scratch_in = vec![0.0f32; cap_frames as usize]; // capture → ring 暫存
    let mut scratch_out = vec![0.0f32; rnd_frames as usize]; // ring → processor → render

    // 預灌一個 render buffer 的靜音，給 capture 一點起跑空間，降低初期 underrun
    for _ in 0..rnd_frames {
        let _ = producer.push(0.0);
    }

    let latency = LatencyInfo {
        capture_frames: cap_frames,
        render_frames: rnd_frames,
        ring_frames: rnd_frames, // 上面預灌的靜音量，是真實路徑延遲
        sample_rate: rnd_fmt.sample_rate,
    };

    unsafe {
        cap_client.Start()?;
        rnd_client.Start()?;
    }
    // 初始化完成，回報主執行緒
    let _ = ready.send(Ok(latency));

    // ── 即時迴圈 ──
    // WASAPI 在 Start 後的第一個 capture packet 常態性帶 DATA_DISCONTINUITY，
    // 不是真掉資料，不算 xrun
    let mut first_packet = true;
    while !stop.load(Ordering::Relaxed) {
        // 等 render 要資料（100ms timeout 以便週期性檢查 stop）
        let wait = unsafe { WaitForSingleObject(event, 100) };
        if wait == WAIT_FAILED {
            // event handle 失效：再 continue 會變成緊密空轉，直接帶錯誤退出
            return Err(WinError::from_thread());
        }
        if wait != WAIT_OBJECT_0 {
            continue; // timeout：回頭檢查 stop flag
        }

        // 1) 抽乾 capture 的所有 packet → ring（取第 0 聲道為 mono）
        loop {
            let packet = unsafe { cap_service.GetNextPacketSize()? };
            if packet == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe {
                cap_service.GetBuffer(
                    &mut data,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )?;
            }
            let n = num_frames as usize;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                for s in &mut scratch_in[..n] {
                    *s = 0.0;
                }
            } else {
                let ch = cap_fmt.channels;
                let base = data as *const f32;
                for f in 0..n {
                    // SAFETY: f < num_frames，stride = channels，在 GetBuffer 給的範圍內
                    scratch_in[f] = unsafe { *base.add(f * ch) };
                }
            }
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && !first_packet {
                xruns.fetch_add(1, Ordering::Relaxed);
            }
            first_packet = false;
            let pushed = ring::push_all(&mut producer, &scratch_in[..n]);
            if pushed < n {
                xruns.fetch_add(1, Ordering::Relaxed); // ring overflow
            }
            unsafe { cap_service.ReleaseBuffer(num_frames)? };
        }

        // 2) 填 render：只填裝置現在缺的量
        let padding = unsafe { rnd_client.GetCurrentPadding()? };
        let avail = rnd_frames.saturating_sub(padding) as usize;
        if avail > 0 {
            let got = ring::pop_fill(&mut consumer, &mut scratch_out[..avail]);
            if got < avail {
                xruns.fetch_add(1, Ordering::Relaxed); // ring underrun（補了靜音）
            }
            // 即時 DSP：直通階段這裡是 no-op，之後接效果鏈
            processor.process(&mut scratch_out[..avail]);

            let data = unsafe { rnd_service.GetBuffer(avail as u32)? };
            let ch = rnd_fmt.channels;
            let out = data as *mut f32;
            for f in 0..avail {
                let v = scratch_out[f];
                for c in 0..ch {
                    // SAFETY: f < avail ≤ rnd_frames，c < channels，在 GetBuffer 範圍內
                    unsafe { *out.add(f * ch + c) = v };
                }
            }
            unsafe { rnd_service.ReleaseBuffer(avail as u32, 0)? };
        }
    }

    // ── 收尾 ──
    unsafe {
        let _ = cap_client.Stop();
        let _ = rnd_client.Stop();
        let _ = CloseHandle(event);
    }
    // 防 unused 警告：block_align 之後做固定 re-blocking 會用到
    let _ = (cap_fmt.block_align, rnd_fmt.block_align);
    Ok(())
}

/// 獨佔模式初始化：協商格式（獨佔不保證吃 mix format）+ 取裝置最小 period +
/// 處理 buffer 對齊。`stream_flags` 兩端都傳 EVENTCALLBACK（獨佔走 event 驅動）。
unsafe fn init_exclusive(dev: &IMMDevice, stream_flags: u32) -> WinResult<InitedClient> {
    let mut client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };
    // mix format 只拿來取 sample_rate 與 channels；獨佔實際格式另外協商。
    let pwfx = unsafe { client.GetMixFormat()? };
    let mix = unsafe { read_fmt(pwfx) };
    unsafe { CoTaskMemFree(Some(pwfx as *const _)) };

    // 協商獨佔支援的格式（float32/int32/int24/int16）。wfx 在本函式內存活，
    // pfmt 指向它，可跨兩次 Initialize 與重新 Activate 使用。
    let Some((wfx, kind)) =
        (unsafe { negotiate_exclusive(&client, mix.sample_rate, mix.channels as u16) })
    else {
        return Err(WinError::new(
            windows::core::HRESULT(-1),
            "獨佔模式找不到裝置支援的格式（float32/int32/int24/int16 全試過；請在 Windows 音效設定確認允許獨佔）",
        ));
    };
    let pfmt = &wfx as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
    let fmt = Fmt {
        channels: wfx.Format.nChannels as usize,
        sample_rate: wfx.Format.nSamplesPerSec,
        block_align: wfx.Format.nBlockAlign as usize,
        kind,
    };

    // 取裝置最小 period（hns = 100ns 單位）當最低延遲目標。
    let mut def_period = 0i64;
    let mut min_period = 0i64;
    unsafe { client.GetDevicePeriod(Some(&mut def_period), Some(&mut min_period))? };

    // 第一次用最小 period 初始化（獨佔模式 buffer 與 periodicity 必須相等）。
    let mut period = min_period;
    let mut init = unsafe {
        client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, stream_flags, period, period, pfmt, None)
    };

    // 經典「對齊舞」：若回 BUFFER_SIZE_NOT_ALIGNED，用回報的對齊 frame 數重算
    // period，並**重新 Activate 一顆乾淨 client**（失敗的 client 已不可再用）再試。
    if let Err(e) = &init
        && e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
    {
        let aligned = unsafe { client.GetBufferSize()? };
        period = (10_000_000.0 * aligned as f64 / fmt.sample_rate as f64).round() as i64;
        // 覆寫 client = 釋放舊的、換新的（COM Release 在 drop 發生）。
        client = unsafe { dev.Activate(CLSCTX_ALL, None)? };
        init = unsafe {
            client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, stream_flags, period, period, pfmt, None)
        };
    }
    init?; // 對齊後仍失敗（如裝置被佔用 / 不允許獨佔）就把錯誤往上拋

    let buffer_frames = unsafe { client.GetBufferSize()? };
    let _ = def_period;
    eprintln!(
        "[wasapi] 獨佔模式：{kind:?} {}ch {}Hz，buffer={buffer_frames} frames（period ≈ {:.2} ms）",
        fmt.channels,
        fmt.sample_rate,
        period as f64 / 10_000.0,
    );
    Ok(InitedClient {
        client,
        fmt,
        buffer_frames,
    })
}

/// 獨佔模式即時迴圈。capture 與 render 皆 event 驅動，單執行緒用
/// `WaitForMultipleObjects` 同時等兩個 event：capture 事件 → 抽 buffer 進 ring；
/// render 事件 → 從 ring 取 → DSP → 寫出。兩端同一介面共用時鐘，ring 吸收相位差。
unsafe fn run_inner_exclusive(
    config: StreamConfig,
    processor: &mut dyn AudioProcessor,
    stop: &AtomicBool,
    xruns: &AtomicU64,
    ready: &Sender<Result<LatencyInfo, BackendError>>,
) -> WinResult<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    // ── capture：獨佔、event 配置但輪詢消費 ──
    // 用 EVENTCALLBACK 讓 capture 拿到「一個 period」的小 buffer（不掛 event 會變成
    // 6 個 period 的大 buffer、突發式交付，害 render 在突發間欠載）；但實際消費走
    // 輪詢——只用 render event 當主時鐘，每個 render tick 抽乾 capture，避免雙 event
    // 在單執行緒下 WaitForMultipleObjects 的索引優先級偏差餓死 render。
    let cap_dev = unsafe { get_device(&enumerator, config.capture_id.as_deref(), eCapture)? };
    let InitedClient {
        client: cap_client,
        fmt: cap_fmt,
        buffer_frames: cap_frames,
    } = unsafe { init_exclusive(&cap_dev, AUDCLNT_STREAMFLAGS_EVENTCALLBACK)? };
    let cap_event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
    unsafe { cap_client.SetEventHandle(cap_event)? };
    let cap_service: IAudioCaptureClient = unsafe { cap_client.GetService()? };

    // ── render：獨佔 + event ──
    let rnd_dev = unsafe { get_device(&enumerator, config.render_id.as_deref(), eRender)? };
    let InitedClient {
        client: rnd_client,
        fmt: rnd_fmt,
        buffer_frames: rnd_frames,
    } = unsafe { init_exclusive(&rnd_dev, AUDCLNT_STREAMFLAGS_EVENTCALLBACK)? };
    let rnd_event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
    unsafe { rnd_client.SetEventHandle(rnd_event)? };
    let rnd_service: IAudioRenderClient = unsafe { rnd_client.GetService()? };

    if cap_fmt.sample_rate != rnd_fmt.sample_rate {
        return Err(WinError::new(
            windows::core::HRESULT(-1),
            "擷取與輸出取樣率不同（請 in/out 用同一介面）",
        ));
    }

    // ── 預配置即時緩衝 ──
    let max_block = cap_frames.max(rnd_frames) as usize;
    processor.prepare(rnd_fmt.sample_rate as f32, max_block);

    let ring_cap = (max_block * 8).max(4096);
    let (mut producer, mut consumer) = ring::channel(ring_cap);
    let mut scratch_out = vec![0.0f32; rnd_frames as usize];

    // 預灌兩個 render buffer 的靜音，吸收 capture/render 兩執行緒的相位差。
    let prime = rnd_frames * 2;
    for _ in 0..prime {
        let _ = producer.push(0.0);
    }

    let latency = LatencyInfo {
        capture_frames: cap_frames,
        render_frames: rnd_frames,
        ring_frames: prime,
        sample_rate: rnd_fmt.sample_rate,
    };
    let _ = ready.send(Ok(latency));

    // capture / render 各跑一條執行緒、各等自己的 event（避免單執行緒雙 event 的
    // 索引偏差餓死一端）。capture = producer、render = consumer，SPSC ring 解耦。
    // scoped thread 讓 capture 執行緒能借用 stop/xruns，scope 結束自動 join。
    let cap_side = CaptureSide {
        client: cap_client,
        service: cap_service,
        event: cap_event,
        frames: cap_frames,
        fmt: cap_fmt,
    };
    std::thread::scope(|s| {
        s.spawn(|| {
            if capture_thread(cap_side, producer, stop, xruns).is_err() {
                stop.store(true, Ordering::Relaxed); // capture 掛了 → render 也收工
            }
        });

        // ── render 迴圈（本執行緒）──
        let mut render = || -> WinResult<()> {
            // 獨佔 event 模式必須在 Start 前先預填一個 buffer，否則裝置起始即 underrun，
            // event 節奏會被打亂（每個 cycle 多 ~一個 period 的空檔，吞掉一半輸出）。
            unsafe {
                let data = rnd_service.GetBuffer(rnd_frames)?;
                let ba = rnd_fmt.block_align;
                for f in 0..rnd_frames as usize {
                    write_all_ch(data, f, rnd_fmt.channels, ba, rnd_fmt.kind, 0.0);
                }
                rnd_service.ReleaseBuffer(rnd_frames, 0)?;
                rnd_client.Start()?;
            }
            while !stop.load(Ordering::Relaxed) {
                let wait = unsafe { WaitForSingleObject(rnd_event, 100) };
                if wait == WAIT_FAILED {
                    return Err(WinError::from_thread());
                }
                if wait != WAIT_OBJECT_0 {
                    continue; // timeout：回頭檢查 stop
                }
                // 獨佔事件驅動 = 每次填滿整個 buffer
                let avail = rnd_frames as usize;
                let got = ring::pop_fill(&mut consumer, &mut scratch_out[..avail]);
                if got < avail {
                    xruns.fetch_add(1, Ordering::Relaxed); // ring underrun（補了靜音）
                }
                processor.process(&mut scratch_out[..avail]);

                let data = unsafe { rnd_service.GetBuffer(avail as u32)? };
                let ba = rnd_fmt.block_align;
                for f in 0..avail {
                    // SAFETY: f < avail = rnd_frames；write_all_ch 在 block_align stride 內定址
                    unsafe {
                        write_all_ch(data, f, rnd_fmt.channels, ba, rnd_fmt.kind, scratch_out[f]);
                    }
                }
                unsafe { rnd_service.ReleaseBuffer(avail as u32, 0)? };
            }
            Ok(())
        };
        let r = render();
        // render 收工：通知 capture 停、停 render client、釋放 event
        stop.store(true, Ordering::Relaxed);
        unsafe {
            let _ = rnd_client.Stop();
            let _ = CloseHandle(rnd_event);
        }
        let _ = rnd_fmt.block_align;
        r
    })
}

/// capture 端打包，移交給 capture 執行緒。
/// SAFETY 前提：COM 物件為 MTA，移交後僅 capture 執行緒使用，不再跨執行緒共享。
struct CaptureSide {
    client: IAudioClient,
    service: IAudioCaptureClient,
    event: HANDLE,
    frames: u32,
    fmt: Fmt,
}
// SAFETY: 見上。
unsafe impl Send for CaptureSide {}

/// capture 執行緒主體：自帶 COM/即時優先級，等 cap event → 抽 buffer → push ring。
fn capture_thread(
    cap: CaptureSide,
    mut producer: Producer<f32>,
    stop: &AtomicBool,
    xruns: &AtomicU64,
) -> WinResult<()> {
    rt::enable_flush_denormals();
    let _priority = rt::ProAudioPriority::register();
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let result = unsafe { capture_loop(&cap, &mut producer, stop, xruns) };
    unsafe {
        let _ = cap.client.Stop();
        let _ = CloseHandle(cap.event);
        CoUninitialize();
    }
    result
}

unsafe fn capture_loop(
    cap: &CaptureSide,
    producer: &mut Producer<f32>,
    stop: &AtomicBool,
    xruns: &AtomicU64,
) -> WinResult<()> {
    let mut scratch = vec![0.0f32; cap.frames as usize];
    let mut first_packet = true;
    unsafe { cap.client.Start()? };
    while !stop.load(Ordering::Relaxed) {
        let wait = unsafe { WaitForSingleObject(cap.event, 100) };
        if wait == WAIT_FAILED {
            return Err(WinError::from_thread());
        }
        if wait != WAIT_OBJECT_0 {
            continue; // timeout：回頭檢查 stop
        }
        // 抽乾目前可取的所有 frame → ring（取第 0 聲道為 mono）
        loop {
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe {
                cap.service
                    .GetBuffer(&mut data, &mut num_frames, &mut flags, None, None)?;
            }
            let n = num_frames as usize;
            if n == 0 {
                break;
            }
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                for s in &mut scratch[..n] {
                    *s = 0.0;
                }
            } else {
                let ba = cap.fmt.block_align;
                for f in 0..n {
                    // SAFETY: f < num_frames；read_ch0 依格式在 block_align stride 內定址
                    scratch[f] = unsafe { read_ch0(data, f, ba, cap.fmt.kind) };
                }
            }
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && !first_packet {
                xruns.fetch_add(1, Ordering::Relaxed);
            }
            first_packet = false;
            let pushed = ring::push_all(producer, &scratch[..n]);
            if pushed < n {
                xruns.fetch_add(1, Ordering::Relaxed); // ring overflow
            }
            unsafe { cap.service.ReleaseBuffer(num_frames)? };
        }
    }
    Ok(())
}
