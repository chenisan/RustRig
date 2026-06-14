# RustRig

用 Rust 寫的 Windows **獨立電吉他即時效果處理器**——低延遲音訊引擎 + 破音 / 箱體 IR / 雜訊閘 / 殘響，對標 Neural DSP 的 standalone 模式。

> ⚠️ **早期版（alpha）**：目前是「直通 + 破音 + cab IR + gate + reverb」的低延遲框架。**真正的擴大機模型（NAM / amp sim）還沒做**——破音目前是 amp 前的 boost，不是完整音箱。拿來試延遲、試 cab IR、試效果鏈沒問題，但別期待完整 amp 音色。歡迎回饋。

---

## 特色

- **低延遲音訊引擎**（可即時切換後端）
  - **WASAPI 共享** — 免設定、相容性最好（延遲較高，約 60ms+）
  - **WASAPI 獨佔** — 繞過 Windows 音訊引擎，個位數～十幾 ms（實測 RME UCX II ~12ms）
  - **ASIO** — 專業驅動直連，最低延遲 3-7ms（需自行從原始碼編譯，見下）
- **效果鏈**（訊號順序）：`GATE 雜訊閘 → DRIVE 破音 → CAB 箱體 IR → REVERB 殘響 → 音量`
  - **DRIVE / TONE**：4× 升頻非對稱 soft-clip 破音 + 二階 TONE 低通
  - **CAB**：載入你自己的箱體脈衝響應（.wav IR），分割 FFT 卷積
  - **GATE**：高增益消底噪用的雜訊閘（快開慢關 + 遲滯）
  - **REVERB**：Freeverb 風格殘響
- 全程 lock-free（GUI ↔ 音訊執行緒），即時執行緒零配置／零鎖

## 系統需求

- Windows 10 / 11（64-bit）
- 建議搭配音效介面（走 WASAPI 獨佔或 ASIO 才有低延遲；內建音效通常鎖在 ~10ms period）
- 吉他輸入 + 喇叭／耳機輸出（**進出建議用同一台介面**，共用時鐘避免 drift）

## 快速開始（試用版，免安裝）

1. 下載 Release 的 zip，解壓，直接執行 `rustrig.exe`
2. **引擎**選 **WASAPI 獨佔**（低延遲），**輸入／輸出**選你的介面
3. 點中間的播放鍵 → 對吉他彈
4. 開 **DRIVE**，點「載入 IR…」選一個箱體 `.wav`（見下），開 **CAB**
5. 視需要開 **GATE**（消底噪）、**REVERB**（空間感）

> 直通（沒掛 cab）會很尖很刺，那是正常的——掛上 cab IR 才會是「音箱」的聲音。
> 用喇叭 + 麥克風會回授嘯叫，建議戴耳機。

## 箱體 IR（自備）

本專案**不附任何 IR 檔**。你可以用：
- 自己購買的商業 IR 包（如 ML Sound Lab、God's Cab、Celestion 等）
- 免費 IR（網路上很多廠商有提供免費包）

支援單聲道 `.wav`（16/24/32-bit 整數或 float，會自動重採樣到引擎取樣率）。

## 延遲說明

| 後端 | 實測延遲（RME UCX II） | 備註 |
|---|---|---|
| WASAPI 共享 | ~66ms | 相容性最好，免設定 |
| WASAPI 獨佔 | ~12ms | 獨佔裝置，多數情況夠用 |
| ASIO | ~7ms（buffer 可再調低） | 需自行編譯，最低延遲 |

## 啟用 ASIO（自行編譯）

試用版**不含 ASIO**——ASIO SDK 是 Steinberg 專有授權，無法隨附於本散佈檔。想要 ASIO 最低延遲，請自行從原始碼編譯：

1. 從 [Steinberg 開發者網站](https://www.steinberg.net/developers/) 下載 **ASIO SDK**（免費，需同意其授權），解壓到某路徑
2. 安裝 **libclang**（bindgen 需要）。最省的方式：`pip install --user libclang`
3. 設兩個環境變數（或專案根目錄放 `.cargo/config.toml`）：
   ```toml
   [env]
   CPAL_ASIO_DIR = "你的\\ASIOSDK\\路徑"
   LIBCLANG_PATH = "libclang.dll\\所在資料夾"
   ```
4. 編譯：`cargo run -p rustrig-app --bin rustrig --features asio`

> 你機器上的防毒（行為監控／勒索防護）可能會擋 bindgen/cc 的 build script 寫檔——把專案資料夾與 `~/.cargo` 加進例外清單即可。

## 從原始碼編譯（WASAPI 版）

```bash
git clone <repo>
cd RustWindows
cargo run --release -p rustrig-app --bin rustrig
```
需要 Rust（含 MSVC toolchain）。預設不含 ASIO，任何人都能直接編。

## 架構

```
crates/
├── rustrig-dsp      純 DSP 核心（零 OS 依賴）：drive / cab / gate / reverb / gain
├── rustrig-audio    音訊 I/O：AudioBackend trait + WASAPI 共享/獨佔 + ASIO + RT 安全工具
└── rustrig-app      GUI（egui）+ lock-free 橋接 + CLI 延遲探針
```

即時鐵律：`AudioProcessor::process` 在音訊執行緒呼叫，禁止 allocation / lock / syscall。
RT 保護：FTZ/DAZ denormal 歸零、MMCSS Pro Audio、lock-free SPSC ring（rtrb）。

## 授權

- 程式碼以 **Apache License 2.0** 釋出（見 [LICENSE](LICENSE)）。
- **ASIO** 是 Steinberg Media Technologies 的商標／專有 SDK；本 repo 不含 ASIO SDK，使用 ASIO 功能須自行取得 SDK 並遵守其授權。
- 任何箱體 IR 檔的版權屬各自權利人，本專案不隨附、不轉散。

## 致謝

Isan · 13soul ｜ 台灣
