//! NAM（Neural Amp Modeler）擴大機模型。
//!
//! 用 `nam-rs`（純 Rust、即時安全、對官方逐 sample parity）載入 `.nam` 檔做推論。
//! NAM 是這條鏈的「真實音箱」——破音（Drive）只是它前面的 boost；NAM 之後接 cab IR。
//!
//! 取樣率：NAM 模型多訓在 48kHz，dilation 以 sample 數定義、不以秒。引擎取樣率與
//! 模型不同時音色會偏移（不會壞，但不準）——建議把介面設成模型的取樣率（多為 48k）。
//! 重採樣留待後續（rubato）。

use crate::{AudioProcessor, SharedParam};
use nam_rs::{Model, NamModel};

/// 載入時的模型資訊（給 GUI 顯示）。
pub struct NamInfo {
    pub sample_rate: f64,
}

/// 驗證 `.nam` JSON 是否可載入並建置推論（app 端載檔時用，提早回報錯誤）。
/// 成功回模型資訊，失敗回人類可讀訊息。
pub fn validate(json: &str) -> Result<NamInfo, String> {
    let model = NamModel::from_json_str(json).map_err(|e| format!("解析失敗：{e}"))?;
    let sr = model.expected_sample_rate();
    // 真的建一次推論，確認架構支援（不支援的 WaveNet/LSTM 變體會在此被擋下）。
    Model::from_nam(&model).map_err(|e| format!("建置失敗：{e}"))?;
    Ok(NamInfo { sample_rate: sr })
}

/// NAM 擴大機 processor。`json` 是 `.nam` 檔內容（app 端已驗證）。
pub struct Nam {
    json: String,
    enabled: SharedParam,
    model: Option<Model>,
}

impl Nam {
    pub fn new(json: String, enabled: SharedParam) -> Self {
        Self {
            json,
            enabled,
            model: None,
        }
    }
}

impl AudioProcessor for Nam {
    fn prepare(&mut self, sample_rate: f32, _max_block: usize) {
        // 在 prepare（非即時）解析 + 建推論；hot path 只跑 process_buffer。
        let built = NamModel::from_json_str(&self.json).and_then(|m| {
            let sr = m.expected_sample_rate();
            Model::from_nam(&m).map(|model| (model, sr))
        });
        match built {
            Ok((model, sr)) => {
                if (sr as f32 - sample_rate).abs() > 1.0 {
                    eprintln!(
                        "[nam] ⚠ 模型 {sr}Hz ≠ 引擎 {sample_rate}Hz，音色會偏移；建議介面設 {sr}Hz"
                    );
                } else {
                    eprintln!("[nam] 模型載入：{sr}Hz");
                }
                self.model = Some(model);
            }
            Err(e) => {
                eprintln!("[nam] 模型建置失敗：{e}");
                self.model = None;
            }
        }
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled.get() < 0.5 {
            return;
        }
        if let Some(model) = &mut self.model {
            // in-place、零配置；模型狀態跨呼叫保留
            model.process_buffer(buf);
        }
    }

    fn reset(&mut self) {
        if let Some(model) = &mut self.model {
            model.reset();
        }
    }
}
