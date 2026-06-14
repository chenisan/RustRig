//! RustRig — 獨立電吉他即時效果處理 app（延遲探針 CLI）。
//!
//! 目標：插上吉他 → 直通 → 喇叭，印出實測延遲與 xrun 計數，跑數分鐘零爆音。
//!
//! 用法：`rustrig-probe [秒數] [裝置名稱關鍵字] [後端]`
//!   - 秒數：純數字參數，預設 10；給 0 = 跑到 Ctrl+C。
//!   - 裝置名稱關鍵字：非數字參數，會在 in/out 兩端各挑第一個名稱含此字的裝置
//!     （不分大小寫）。例如 `rustrig-probe 8 fireface` 量 RME UCX II。
//!     省略時 in/out 都用系統預設裝置。
//!   - 後端：`exclusive`（或 `ex`）走 WASAPI 獨佔；`shared` 走共享（預設）。
//!     例如 `rustrig-probe 8 fireface ex` 量 UCX II 獨佔模式延遲。
//!
//! 低延遲只在獨佔模式或驅動開放小 period 時才有效——共享模式多數驅動鎖在 10ms。

use std::time::Duration;

use rustrig_audio::{BackendKind, DeviceInfo, StreamConfig, enumerate, open_stream};
use rustrig_dsp::Passthrough;

/// 在清單裡找第一個名稱含 `needle`（不分大小寫）的裝置。
fn find_device<'a>(list: &'a [DeviceInfo], needle: &str) -> Option<&'a DeviceInfo> {
    let lower = needle.to_lowercase();
    list.iter().find(|d| d.name.to_lowercase().contains(&lower))
}

fn print_list(label: &str, list: &[DeviceInfo]) {
    println!("  {label}：");
    if list.is_empty() {
        println!("    （無）");
    }
    for d in list {
        let mark = if d.is_default { " ★預設" } else { "" };
        println!("    • {}{}", d.name, mark);
    }
}

fn main() -> anyhow::Result<()> {
    println!("════════════════════════════════════════");
    println!(" RustRig — WASAPI 直通延遲測試");
    println!("════════════════════════════════════════");

    // 參數：數字 = 秒數，後端關鍵字 = 切換後端，其餘 = 裝置名稱關鍵字。
    let mut secs: u64 = 10;
    let mut filter: Option<String> = None;
    let mut backend = BackendKind::WasapiShared;
    for arg in std::env::args().skip(1) {
        let low = arg.to_lowercase();
        if let Ok(n) = arg.parse::<u64>() {
            secs = n;
        } else if matches!(low.as_str(), "exclusive" | "excl" | "ex" | "獨佔") {
            backend = BackendKind::WasapiExclusive;
        } else if matches!(low.as_str(), "shared" | "share" | "共享") {
            backend = BackendKind::WasapiShared;
        } else {
            filter = Some(arg);
        }
    }

    // 列出所有裝置，方便對照關鍵字。
    let devices = enumerate()?;
    println!("可用裝置：");
    print_list("輸入(capture)", &devices.capture);
    print_list("輸出(render)", &devices.render);
    println!("────────────────────────────────────────");

    // 依關鍵字挑 in/out 裝置；挑不到就退回系統預設。
    let mut config = StreamConfig::default();
    if let Some(f) = &filter {
        match find_device(&devices.capture, f) {
            Some(d) => {
                println!("輸入 → {}", d.name);
                config.capture_id = Some(d.id.clone());
            }
            None => println!("⚠ 找不到名稱含「{f}」的輸入裝置，改用系統預設。"),
        }
        match find_device(&devices.render, f) {
            Some(d) => {
                println!("輸出 → {}", d.name);
                config.render_id = Some(d.id.clone());
            }
            None => println!("⚠ 找不到名稱含「{f}」的輸出裝置，改用系統預設。"),
        }
    } else {
        println!("（未指定關鍵字，in/out 用系統預設裝置）");
    }

    println!("後端：{}", backend.label());
    println!("啟動音訊引擎中…");

    let stream = open_stream(backend, config, Box::new(Passthrough))?;

    let lat = stream.latency();
    println!("─ 取樣率　：{} Hz", lat.sample_rate);
    println!(
        "─ 緩衝　　：capture {} frames / render {} frames",
        lat.capture_frames, lat.render_frames
    );
    println!(
        "─ 後端緩衝延遲：約 {:.2} ms（WASAPI 緩衝 + ring 水位 {} frames，不含驅動內部與 DA/AD 轉換）",
        lat.buffer_ms(),
        lat.ring_frames
    );
    println!("────────────────────────────────────────");
    println!("直通中，對著輸入裝置彈／講話應能從喇叭聽到。");
    if secs == 0 {
        println!("（持續執行，Ctrl+C 結束）");
    } else {
        println!("（跑 {secs} 秒後自動關閉）");
    }

    let mut last_xrun = 0u64;
    let mut elapsed = 0u64;
    let mut died = false;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        elapsed += 1;

        if !stream.is_alive() {
            died = true;
            println!("[{elapsed:>3}s] ⚠ 音訊執行緒已停止（裝置被拔除或驅動錯誤），提前結束。");
            break;
        }

        let total = stream.xrun_count();
        let delta = total - last_xrun;
        last_xrun = total;
        println!("[{elapsed:>3}s] xrun 累計 {total}（本秒 +{delta}）");

        if secs != 0 && elapsed >= secs {
            break;
        }
    }

    let final_xrun = stream.xrun_count();
    drop(stream); // 觸發乾淨關閉：停止串流、join 音訊執行緒
    println!("────────────────────────────────────────");
    println!("結束。總 xrun：{final_xrun}");
    if died {
        println!("⚠ 音訊引擎異常終止，本次數據不可信。");
    } else if final_xrun == 0 {
        println!("✓ 全程零爆音。");
    } else {
        println!("⚠ 有 {final_xrun} 次 xrun，需調整 buffer / 檢查 DPC（LatencyMon）。");
    }
    Ok(())
}
