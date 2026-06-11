//! RustRig — 獨立電吉他即時效果處理 app（P0 骨架）。
//!
//! P0 目標：插上吉他 → 直通 → 喇叭，印出實測延遲與 xrun 計數，跑數分鐘零爆音。
//!
//! 用法：`rustrig [秒數]`（預設 10 秒後乾淨關閉）。

use std::time::Duration;

use rustrig_audio::{AudioBackend, StreamConfig, WasapiShared};
use rustrig_dsp::Passthrough;

fn main() -> anyhow::Result<()> {
    println!("════════════════════════════════════════");
    println!(" RustRig P0 — WASAPI 直通延遲測試");
    println!("════════════════════════════════════════");

    // 跑多久（秒）。給 0 = 一直跑到 Ctrl+C。
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let backend = WasapiShared::open(StreamConfig::default())?;
    println!("後端：{}", WasapiShared::name());
    println!("啟動音訊引擎中…（吉他輸入用系統預設擷取裝置）");

    let stream = backend.run(Box::new(Passthrough))?;

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
