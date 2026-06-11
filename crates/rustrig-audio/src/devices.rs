//! 音訊端點列舉（WASAPI / MMDevice）。
//!
//! 給 GUI 列出輸入／輸出裝置清單用。列舉跑在**獨立短命執行緒**上
//! （自帶 COM MTA 初始化），避免跟 GUI 執行緒既有的 COM apartment 模式打架。

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole,
    eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::core::Result as WinResult;

use crate::backend::BackendError;

/// 一個音訊端點。`id` 是 WASAPI 裝置 ID（餵回 [`crate::StreamConfig`]），
/// `name` 是顯示給使用者的友善名稱。
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// 輸入與輸出裝置清單。
#[derive(Clone, Debug, Default)]
pub struct DeviceLists {
    pub capture: Vec<DeviceInfo>,
    pub render: Vec<DeviceInfo>,
}

/// 列舉所有作用中的輸入／輸出端點。可從任何執行緒呼叫（內部開執行緒處理 COM）。
pub fn enumerate() -> Result<DeviceLists, BackendError> {
    std::thread::spawn(|| {
        // SAFETY: 本執行緒專屬的 COM 初始化／反初始化，所有 COM 物件不離開此執行緒。
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| {
                BackendError::Os(format!("COM 初始化失敗：{e}"))
            })?;
        }
        let result = unsafe { enumerate_inner() };
        unsafe { CoUninitialize() };
        result.map_err(|e| BackendError::Os(format!("裝置列舉失敗：{e}")))
    })
    .join()
    .unwrap_or_else(|_| Err(BackendError::Os("裝置列舉執行緒 panic".into())))
}

unsafe fn enumerate_inner() -> WinResult<DeviceLists> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let mut lists = DeviceLists::default();
    for (flow, out) in [
        (eCapture, &mut lists.capture),
        (eRender, &mut lists.render),
    ] {
        *out = unsafe { list_flow(&enumerator, flow)? };
    }
    Ok(lists)
}

unsafe fn list_flow(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> WinResult<Vec<DeviceInfo>> {
    // 預設裝置 ID（拿不到就當沒有，例如該方向沒有任何裝置）
    let default_id = unsafe {
        enumerator
            .GetDefaultAudioEndpoint(flow, eConsole)
            .and_then(|d| d.GetId())
            .ok()
            .map(|pw| {
                let s = pw.to_string().unwrap_or_default();
                CoTaskMemFree(Some(pw.0 as *const _));
                s
            })
    };

    let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
    let count = unsafe { collection.GetCount()? };
    let mut devices = Vec::with_capacity(count as usize);
    for i in 0..count {
        let dev = unsafe { collection.Item(i)? };
        let pw = unsafe { dev.GetId()? };
        let id = unsafe { pw.to_string().unwrap_or_default() };
        unsafe { CoTaskMemFree(Some(pw.0 as *const _)) };

        let name = unsafe {
            dev.OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .map(|pv| pv.to_string())
                .unwrap_or_else(|_| "（未知裝置）".into())
        };

        devices.push(DeviceInfo {
            is_default: default_id.as_deref() == Some(id.as_str()),
            id,
            name,
        });
    }
    // 預設裝置排最前面
    devices.sort_by_key(|d| !d.is_default);
    Ok(devices)
}
