# RustRig

Rust + `windows` crate 寫的 Windows 獨立電吉他即時效果處理 app，對標 Neural DSP standalone 模式：插上吉他 → 訊號鏈 → 即時聽到處理後的聲音。

## 架構

```
crates/
├── rustrig-dsp      純 DSP 核心（#![forbid(unsafe_code)]，零 OS 依賴，未來可包 VST3）
├── rustrig-audio    音訊 I/O：AudioBackend trait + WASAPI Shared 後端 + RT 安全工具
└── rustrig-app      可執行檔：啟動引擎、印延遲、監看 xrun
```

即時鐵律：`AudioProcessor::process` 在音訊執行緒呼叫，禁止 allocation / lock / syscall。
RT 保護：FTZ/DAZ denormal 歸零、MMCSS Pro Audio、lock-free SPSC ring（rtrb）。

## 跑 P0 直通測試

1. Windows 聲音設定：把音訊介面設為**預設輸入與預設輸出**（in/out 同一台，共用 clock）
2. 戴耳機（喇叭+麥克風會回授嘯叫）
3. `cargo run -p rustrig-app -- 15`（跑 15 秒；`0` = 跑到 Ctrl+C）

成功標準：聽得到直通聲、印出延遲、全程 xrun 0。

## 路線圖

- **P0** 低延遲音訊骨幹 + 延遲探針 ✅（待實機驗證）
- **P1** NAM（Neural Amp Modeler）純 Rust `.nam` 載入 + 推論整合
- **P2** IR cab（partitioned FFT convolution）+ noise gate + dirt
- **P3** compressor / EQ / delay / convolution reverb
- 後端升級 TODO：IAudioClient3 低延遲共享 → WASAPI Exclusive → ASIO（需 ASIO SDK + libclang）
