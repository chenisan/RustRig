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

use rustrig_dsp::AudioProcessor;
use windows::Win32::Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, EDataFlow, IAudioCaptureClient,
    IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
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
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// 從 mix format 取出我們需要的欄位。
#[derive(Clone, Copy)]
struct Fmt {
    channels: usize,
    sample_rate: u32,
    /// 每 frame 位元組數（= channels × 4，float32）。
    block_align: usize,
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
        let stop = Arc::new(AtomicBool::new(false));
        let xruns = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let (ready_tx, ready_rx) = channel::<Result<LatencyInfo, BackendError>>();

        let stop_t = stop.clone();
        let xruns_t = xruns.clone();
        let alive_t = alive.clone();
        let config = self.config;
        let join: JoinHandle<()> = thread::spawn(move || {
            let mut processor = processor;
            if let Err(e) = audio_thread(config, processor.as_mut(), &stop_t, &xruns_t, &ready_tx) {
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
fn audio_thread(
    config: StreamConfig,
    processor: &mut dyn AudioProcessor,
    stop: &AtomicBool,
    xruns: &AtomicU64,
    ready: &Sender<Result<LatencyInfo, BackendError>>,
) -> WinResult<()> {
    // 即時保護：denormal 歸零 + Pro Audio 排程優先級
    rt::enable_flush_denormals();
    let _priority = rt::ProAudioPriority::register();

    // SAFETY: 整段在本執行緒內初始化、使用、銷毀所有 COM 物件，不跨執行緒。
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let result = unsafe { run_inner(config, processor, stop, xruns, ready) };
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
