# Third-Party Notices · 第三方授權聲明

Copyright (c) 2026 Isan (13soul)

RustRig 的程式碼以 **Apache License 2.0** 授權（見 [LICENSE](./LICENSE)）。
下列第三方元件以各自授權散布／使用。
RustRig's own source is under **Apache-2.0**; the third-party components below keep their own licenses.

---

## ASIO SDK（非必要、不隨本工具散布 / optional, not bundled）

RustRig 的 **ASIO 後端為可選功能**（`--features asio`），需在編譯期連結 **Steinberg ASIO SDK** 的標頭。
RustRig's **ASIO backend is optional** (`--features asio`) and links the **Steinberg ASIO SDK** headers at build time.

- **授權 / License**：**Steinberg ASIO SDK Licensing Agreement**（專有授權；需向 Steinberg 取得並同意）。
  Proprietary; obtain it yourself from Steinberg and agree to its terms.
- **不隨附 / Not bundled**：本 repo 與公開 binary **皆不含 ASIO SDK**；預設組建（`default = []`）為純 WASAPI，不需要它。
  Neither the repo nor the public binary ships the SDK; the default build is WASAPI-only and does not need it.
- **ASIO 是 Steinberg Media Technologies GmbH 的商標。** ASIO is a trademark of Steinberg Media Technologies GmbH.
- **取得 / Get it**：<https://www.steinberg.net/developers/>
- 透過 crate `asio-sys`（Apache-2.0）綁定；該 crate 只含繫結程式碼，不含 SDK 本體。

---

## NAM 擴大機模型 · NAM amp models（不隨本工具散布 / not bundled）

由使用者自行下載放入 `models/`。User-installed into `models/`.

- **`.nam` 模型權重各自授權**：社群模型來源眾多，授權不一（常見 CC-BY / CC-BY-NC，亦有 **GPL-3.0**，如 Fortin/ML 系列）。
  Community `.nam` weights carry their own licenses (often CC-BY / CC-BY-NC, sometimes **GPL-3.0**).
- 因此 RustRig **不內建任何模型**，改為從本機 `models/` 載入——避免把不相容授權的權重併入 Apache-2.0 散布檔。
  RustRig bundles **no model**; it loads from your local `models/` instead, to avoid mixing incompatible weights into an Apache-2.0 distribution.
- 推論引擎 / Inference engine：**`nam-rs`**（純 Rust 移植，逐 sample 對齊官方 NAM）— **MIT**。
  <https://crates.io/crates/nam-rs>
- NAM 專案 / Project：<https://github.com/sdatkinson/neural-amp-modeler>

---

## 箱體脈衝響應 · Cabinet impulse responses（不隨本工具散布 / not bundled）

由使用者自備 `.wav` IR。User-supplied `.wav` IRs.

- 商業 IR 包（ML Sound Lab、God's Cab、Celestion…）與免費 IR 皆**版權屬各自權利人**。
  Commercial and free IR packs remain **copyright of their respective owners**.
- RustRig **不附任何 IR、不轉散**；僅在執行期讀取你指定的檔案。
  RustRig bundles and redistributes **no IR**; it only reads files you point it at.

---

## Rust 套件 · Rust crates

RustRig 連結下列 crate（皆與 Apache-2.0 相容）。完整相依樹見 `Cargo.lock`。
RustRig links the crates below (all Apache-2.0-compatible). Full tree in `Cargo.lock`.

| Crate | 用途 / Purpose | License |
|---|---|---|
| `windows` (windows-rs) | WASAPI / MMCSS（音訊 I/O） | MIT OR Apache-2.0 |
| `eframe` / `egui` | GUI | MIT OR Apache-2.0 |
| `nam-rs` | NAM 擴大機推論 | MIT |
| `fft-convolver` | cab IR 分割卷積 | MIT |
| `hound` | WAV 讀寫（IR 載入） | Apache-2.0 |
| `rtrb` | lock-free SPSC ring | MIT OR Apache-2.0 |
| `rfd` | 原生檔案對話框 | MIT |
| `asio-sys` | ASIO 繫結（feature-gated） | Apache-2.0 |
| `thiserror` / `anyhow` | 錯誤處理 | MIT OR Apache-2.0 |

> 各 crate 之授權全文隨其原始碼散布；上表為摘要。完整版本與雜湊見 `Cargo.lock`。
> Each crate ships its full license text with its source; the table is a summary. See `Cargo.lock` for exact versions.
