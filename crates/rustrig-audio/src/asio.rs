//! ASIO 後端（feature = "asio"）。
//!
//! ASIO 跟 WASAPI 根本不同：**單一雙工 callback**（`bufferSwitch`）一次給齊輸入與
//! 輸出緩衝，capture→DSP→render 全在同一個 callback 內完成——不需要 ring、不需要
//! 兩條執行緒。緩衝是**每聲道一塊**（非交錯）、雙緩衝（兩個 index 輪流）。
//!
//! 驅動列舉走 ASIO 自己的註冊表清單（非 MMDevice）。延遲取決於驅動裡設的 buffer
//! size（RME 在 TotalMix/驅動設定），我們用 `None` 沿用該設定。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use asio_sys as sys;
use rustrig_dsp::AudioProcessor;

use crate::backend::{AudioBackend, BackendError, LatencyInfo, RunningStream, StreamConfig};

/// ASIO 後端。`config.asio_driver` 指定驅動名稱（`None` = 第一個可用驅動）。
pub struct AsioBackend {
    config: StreamConfig,
}

impl AudioBackend for AsioBackend {
    fn name() -> &'static str {
        "ASIO"
    }

    fn open(config: StreamConfig) -> Result<Self, BackendError> {
        Ok(Self { config })
    }

    fn run(
        self,
        processor: Box<dyn AudioProcessor>,
    ) -> Result<Box<dyn RunningStream>, BackendError> {
        run_asio(self.config, processor)
    }
}

/// 列出可用的 ASIO 驅動名稱（給 GUI 下拉）。
pub fn driver_names() -> Vec<String> {
    sys::Asio::new().driver_names()
}

/// 我們支援的 ASIO 樣本格式（皆 little-endian / Intel）。整數一律轉 f32。
#[derive(Clone, Copy)]
enum AsioFmt {
    I16,
    /// 24-bit 緊密封裝（每樣本 3 bytes）。
    I24,
    /// 32-bit 整數容器（含 Int32LSB 與 16/18/20/24 對齊變體，轉換相同）。
    I32,
    F32,
}

fn map_fmt(t: &sys::AsioSampleType) -> Result<AsioFmt, BackendError> {
    use sys::AsioSampleType::*;
    Ok(match t {
        ASIOSTInt16LSB => AsioFmt::I16,
        ASIOSTInt24LSB => AsioFmt::I24,
        ASIOSTInt32LSB | ASIOSTInt32LSB16 | ASIOSTInt32LSB18 | ASIOSTInt32LSB20
        | ASIOSTInt32LSB24 => AsioFmt::I32,
        ASIOSTFloat32LSB => AsioFmt::F32,
        other => {
            return Err(BackendError::UnsupportedFormat(format!(
                "ASIO 樣本格式 {other:?} 尚未支援（僅支援 LSB 的 Int16/24/32 與 Float32）"
            )));
        }
    })
}

/// 讀一個樣本 → f32。`base` 指向該聲道緩衝起點。
#[inline]
unsafe fn read_sample(base: *const u8, frame: usize, fmt: AsioFmt) -> f32 {
    match fmt {
        AsioFmt::F32 => unsafe { *(base as *const f32).add(frame) },
        AsioFmt::I32 => unsafe { *(base as *const i32).add(frame) as f32 / 2_147_483_648.0 },
        AsioFmt::I16 => unsafe { *(base as *const i16).add(frame) as f32 / 32_768.0 },
        AsioFmt::I24 => unsafe {
            let p = base.add(frame * 3);
            let mut v = (*p as i32) | ((*p.add(1) as i32) << 8) | ((*p.add(2) as i32) << 16);
            if v & 0x0080_0000 != 0 {
                v |= !0x00FF_FFFF; // 符號延伸
            }
            v as f32 / 8_388_608.0
        },
    }
}

/// 寫一個 f32 樣本到該聲道緩衝。
#[inline]
unsafe fn write_sample(base: *mut u8, frame: usize, fmt: AsioFmt, v: f32) {
    let v = v.clamp(-1.0, 1.0);
    match fmt {
        AsioFmt::F32 => unsafe { *(base as *mut f32).add(frame) = v },
        AsioFmt::I32 => unsafe { *(base as *mut i32).add(frame) = (v as f64 * 2_147_483_647.0) as i32 },
        AsioFmt::I16 => unsafe { *(base as *mut i16).add(frame) = (v * 32_767.0) as i16 },
        AsioFmt::I24 => unsafe {
            let i = (v * 8_388_607.0) as i32;
            let p = base.add(frame * 3);
            *p = i as u8;
            *p.add(1) = (i >> 8) as u8;
            *p.add(2) = (i >> 16) as u8;
        },
    }
}

/// 移交給 ASIO callback 的狀態。COM/ASIO 緩衝指標只在 callback（ASIO 執行緒）使用。
struct CallbackState {
    /// 第 0 輸入聲道的雙緩衝指標。
    input: sys::AsioBufferInfo,
    /// 各輸出聲道的雙緩衝指標。
    outputs: Vec<sys::AsioBufferInfo>,
    in_fmt: AsioFmt,
    out_fmt: AsioFmt,
    buffer_size: usize,
    processor: Box<dyn AudioProcessor>,
    scratch: Vec<f32>,
}
// SAFETY: state 移交給單一 ASIO callback 執行緒後僅該執行緒存取；裸指標指向 ASIO
// 配置的緩衝，於 dispose 前有效，而 drop 會先 stop+remove_callback 再 dispose。
unsafe impl Send for CallbackState {}

/// 正在跑的 ASIO 串流。
struct AsioStream {
    // 持有以保持驅動/子系統存活；drop 時依序收尾。
    driver: sys::Driver,
    _asio: sys::Asio,
    cb_id: sys::BufferCallbackId,
    latency: LatencyInfo,
    alive: Arc<AtomicBool>,
    xruns: Arc<AtomicU64>,
}

impl RunningStream for AsioStream {
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

impl Drop for AsioStream {
    fn drop(&mut self) {
        // 順序很重要：先停止（不再有 callback）→ 移除我們的 callback（連同 processor
        // 與緩衝指標一起釋放）→ 釋放緩衝。最後 driver 欄位 drop 收掉 ASIO 子系統。
        let _ = self.driver.stop();
        self.driver.remove_callback(self.cb_id);
        let _ = self.driver.dispose_buffers();
        self.alive.store(false, Ordering::Release);
    }
}

fn run_asio(
    config: StreamConfig,
    processor: Box<dyn AudioProcessor>,
) -> Result<Box<dyn RunningStream>, BackendError> {
    let asio = sys::Asio::new();

    // 選驅動：指定名稱，否則第一個可用。
    let names = asio.driver_names();
    let name = match &config.asio_driver {
        Some(n) => n.clone(),
        None => names
            .first()
            .cloned()
            .ok_or_else(|| BackendError::Os("找不到任何 ASIO 驅動".into()))?,
    };
    let driver = asio
        .load_driver(&name)
        .map_err(|e| BackendError::Os(format!("載入 ASIO 驅動「{name}」失敗：{e}")))?;

    let channels = driver
        .channels()
        .map_err(|e| BackendError::Os(format!("查 ASIO 通道失敗：{e}")))?;
    if channels.ins < 1 || channels.outs < 1 {
        return Err(BackendError::Os(format!(
            "ASIO 驅動「{name}」通道不足（in={} out={}）",
            channels.ins, channels.outs
        )));
    }
    // 吉他單聲道輸入（第 0 通道）；輸出走前兩個通道（主輸出 L/R），不足則單聲道。
    let in_ch = 1usize;
    let out_ch = if channels.outs >= 2 { 2usize } else { 1usize };

    let in_fmt = map_fmt(
        &driver
            .input_data_type()
            .map_err(|e| BackendError::Os(format!("查 ASIO 輸入格式失敗：{e}")))?,
    )?;
    let out_fmt = map_fmt(
        &driver
            .output_data_type()
            .map_err(|e| BackendError::Os(format!("查 ASIO 輸出格式失敗：{e}")))?,
    )?;

    // 若 config 指定了目標取樣率且驅動支援，切過去（NAM 開啟自動切 48k 用）。
    if config.sample_rate > 0 {
        let target = config.sample_rate as f64;
        let cur = driver.sample_rate().unwrap_or(0.0);
        if (cur - target).abs() > 1.0 {
            if driver.can_sample_rate(target).unwrap_or(false) {
                match driver.set_sample_rate(target) {
                    Ok(()) => eprintln!("[asio] 取樣率切到 {target}Hz"),
                    Err(e) => eprintln!("[asio] 切換取樣率到 {target}Hz 失敗：{e}"),
                }
            } else {
                eprintln!("[asio] 驅動不支援 {target}Hz，維持 {cur}Hz");
            }
        }
    }

    let sample_rate = driver
        .sample_rate()
        .map_err(|e| BackendError::Os(format!("查 ASIO 取樣率失敗：{e}")))? as u32;

    // 建立雙工緩衝（buffer_size=None 沿用驅動設定，即 RME 裡設的值）。先建輸入、
    // 再把輸入併進輸出，最後一次 ASIOCreateBuffers 同時涵蓋兩端。
    let streams = driver
        .prepare_input_stream(None, in_ch, None)
        .map_err(|e| BackendError::Os(format!("建立 ASIO 輸入緩衝失敗：{e}")))?;
    let streams = driver
        .prepare_output_stream(streams.input, out_ch, None)
        .map_err(|e| BackendError::Os(format!("建立 ASIO 輸出緩衝失敗：{e}")))?;

    let input_stream = streams
        .input
        .ok_or_else(|| BackendError::Os("ASIO 輸入緩衝缺失".into()))?;
    let output_stream = streams
        .output
        .ok_or_else(|| BackendError::Os("ASIO 輸出緩衝缺失".into()))?;
    let buffer_size = output_stream.buffer_size as usize;

    eprintln!(
        "[asio] {name}：{sample_rate}Hz buffer={buffer_size} frames，in={in_ch}（{}）out={out_ch}（{}）",
        fmt_name(in_fmt),
        fmt_name(out_fmt),
    );

    let mut processor = processor;
    processor.prepare(sample_rate as f32, buffer_size);

    let state = CallbackState {
        input: input_stream.buffer_infos[0],
        outputs: output_stream.buffer_infos.clone(),
        in_fmt,
        out_fmt,
        buffer_size,
        processor,
        scratch: vec![0.0f32; buffer_size],
    };

    // 註冊雙工 callback。每次 bufferSwitch：讀輸入第 0 聲道 → DSP → 寫所有輸出聲道。
    let cb_id = {
        let mut state = state;
        driver.add_callback(move |info: &sys::CallbackInfo| {
            // 整體借用 state，強制 closure 整體捕獲（CallbackState 有 unsafe Send）；
            // 否則 2021 edition 的 disjoint 捕獲會個別抓到非 Send 的緩衝指標欄位。
            let state = &mut state;
            let bi = (info.buffer_index as usize) & 1; // 0/1 雙緩衝
            let n = state.buffer_size;

            let inp = state.input.buffers[bi] as *const u8;
            let in_fmt = state.in_fmt;
            for f in 0..n {
                // SAFETY: bi∈{0,1}，f<buffer_size，指向 ASIO 該聲道緩衝範圍內
                state.scratch[f] = unsafe { read_sample(inp, f, in_fmt) };
            }

            state.processor.process(&mut state.scratch[..n]);

            let out_fmt = state.out_fmt;
            for out in &state.outputs {
                let op = out.buffers[bi] as *mut u8;
                for f in 0..n {
                    // SAFETY: 同上，輸出緩衝範圍內
                    unsafe { write_sample(op, f, out_fmt, state.scratch[f]) };
                }
            }
        })
    };

    // ASIO 的 latencies() 已含緩衝，準確（不像 WASAPI 要估）。無 ring。
    let lat = driver.latencies().unwrap_or(sys::Latencies {
        input: buffer_size as i32,
        output: buffer_size as i32,
    });
    let latency = LatencyInfo {
        capture_frames: lat.input.max(0) as u32,
        render_frames: lat.output.max(0) as u32,
        ring_frames: 0,
        sample_rate,
    };

    driver
        .start()
        .map_err(|e| BackendError::Os(format!("啟動 ASIO 失敗：{e}")))?;

    Ok(Box::new(AsioStream {
        driver,
        _asio: asio,
        cb_id,
        latency,
        alive: Arc::new(AtomicBool::new(true)),
        xruns: Arc::new(AtomicU64::new(0)),
    }))
}

fn fmt_name(f: AsioFmt) -> &'static str {
    match f {
        AsioFmt::I16 => "I16",
        AsioFmt::I24 => "I24",
        AsioFmt::I32 => "I32",
        AsioFmt::F32 => "F32",
    }
}
