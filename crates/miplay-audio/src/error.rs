//! miplay-audio 错误类型。

use thiserror::Error;

/// 采集 / 缓冲 / 分发链路上的所有错误。
///
/// 每个 variant 的语义由 `#[error(...)]` 的文案表达（会直接展示给用户）。
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum AudioError {
    #[error("WASAPI 错误 (HRESULT 0x{0:08X}): {1}")]
    Wasapi(i32, String),

    #[error("COM 错误 (HRESULT 0x{0:08X}): {1}")]
    Com(i32, String),

    #[error("找不到匹配的设备：{0}（可先调用 list_endpoints() 查看本机端点）")]
    DeviceNotFound(String),

    #[error("默认端点不存在：{0}")]
    NoDefaultDevice(String),

    #[error("不支持的混音格式：{0}")]
    UnsupportedFormat(String),

    #[error("采集线程初始化超时（{0:?}），设备可能被独占或无权限")]
    StartTimeout(std::time::Duration),

    #[error("采集线程没有运行，先调用 start()")]
    NotStarted,

    #[error("采集设备已失效（AUDCLNT_E_DEVICE_INVALIDATED），需要重新创建 AudioCapture")]
    DeviceInvalidated,

    #[error("参数非法：{0}")]
    InvalidArgument(String),

    #[error("环形缓冲已关闭")]
    Closed,

    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("采集线程内部错误：{0}")]
    CaptureThread(String),
}

impl AudioError {
    pub(crate) fn wasapi(hr: i32, context: impl Into<String>) -> Self {
        AudioError::Wasapi(hr, context.into())
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        AudioError::InvalidArgument(msg.into())
    }
}

#[cfg(windows)]
impl From<windows::core::Error> for AudioError {
    fn from(e: windows::core::Error) -> Self {
        AudioError::Wasapi(e.code().0, e.message().to_string())
    }
}

/// 本 crate 的 `Result`（错误固定为 [`AudioError`]）。
pub type Result<T> = std::result::Result<T, AudioError>;
