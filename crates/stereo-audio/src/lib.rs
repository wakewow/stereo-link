//! # stereo-audio
//!
//! StereoLink 的音频引擎：把**虚拟声卡上的系统声音**抓成 PCM，再按声道拆给
//! 本机任意输出设备（蓝牙音箱、声卡线路输出、USB 耳机……）。
//!
//! 数据流：
//!
//! ```text
//! 播放器 → 虚拟声卡端点（Windows 默认输出）
//!            │ WASAPI loopback（自动重采样到 44100/2ch/s16le）
//!            ▼
//!        AudioCapture
//!            │ 按配置把左/右声道分别写入
//!            ▼
//!        BtSplitHandle（多路 sink：每台设备一条独立播放流）
//! ```
//!
//! 设计要点：
//!
//! * 采集端固定 44100 Hz / 2 ch / 16 bit 交错 PCM；
//! * 每台输出设备一条独立线程 + 抖动队列，设备之间**互不阻塞**；
//! * 某台设备写失败/断开只影响它自己，其它设备继续出声。
//!
//! ## 最小用法
//!
//! ```no_run
//! use stereo_audio::{BtSplitConfig, BtSplitHandle};
//!
//! let cfg = BtSplitConfig {
//!     left_endpoint: "音箱A".into(),
//!     right_endpoint: "音箱B".into(),
//!     ..Default::default()
//! };
//! let handle = BtSplitHandle::start(cfg)?;
//! println!("{:?}", handle.stats());
//! # Ok::<(), stereo_audio::AudioError>(())
//! ```
//!
//! ## 平台
//!
//! `capture` / `playback` / `btsplit` 只在 Windows 上编译；
//! `convert` / `endpoint` / `ring` 是纯逻辑，任何平台都能跑单测。

#![deny(missing_docs)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod convert;
pub mod endpoint;
pub mod error;
pub mod ring;

#[cfg(windows)]
mod com;
#[cfg(windows)]
pub mod btsplit;
#[cfg(windows)]
pub mod route;
#[cfg(windows)]
pub mod sync;
#[cfg(windows)]
pub mod endpoint_format;
#[cfg(windows)]
pub mod format_support;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod playback;

pub use convert::{
    build_sine_s16, decode_s16le_into, deinterleave, encode_s16le_into, estimate_frequency,
    f32_to_s16, remap_channels, resample_f32, s16_to_f32, SampleFormat, StreamResampler,
};
pub use endpoint::{DataFlow, DeviceSelector, EndpointInfo, FormatInfo, PcmFormat};
pub use error::{AudioError, Result};
pub use ring::{ReadOutcome, RingBuffer};

/// 声学自校验（麦克风互相关测延迟 + 自动对齐）。
///
/// 注意：本机实测**只有独占模式**能采到真实声学信号（共享模式整段是 0.48 LSB 平底），
/// 所以自动标定走 `capture_mono_exclusive`，且必须丢弃开流后前 [`EXCLUSIVE_WARMUP_MS`]。
#[cfg(windows)]
pub use sync::{
    calibrate_pair, calibrate_pair_sequential, calibrate_pair_simultaneous, capture_mono,
    delay_ms_for, list_microphones, measure_endpoint_delay, mic_health, play_alignment_clicks,
    selector_for,
    CapturedAudio, CapturePath, Correlation, MicCandidate, MicHealth, MicKind, MicVerdict,
    PcmStats, SyncConfig, SyncMethod, SyncProbe, SyncResult, EXCLUSIVE_WARMUP_MS,
};

#[cfg(windows)]
pub use btsplit::{BtSplitConfig, BtSplitHandle, BtSplitStats, SplitSinkStats, SPLIT_SAMPLE_RATE};
#[cfg(windows)]
pub use route::{
    RouteChannel, RouteConfig, RouteEngineConfig, RouteEngineHandle, RouteEngineStats, RouteStats,
};
#[cfg(windows)]
pub use endpoint_format::{
    build_waveformat_ext, default_render as default_render_format, get_format as get_endpoint_format,
    parse_waveformat, set_format as set_endpoint_format, EndpointFormat, COMMON_FORMATS,
};
#[cfg(windows)]
pub use format_support::{
    format_supported, supported_formats, supported_summary,
};
#[cfg(windows)]
pub use capture::{
    default_endpoint, list_endpoints, resolve_endpoint, AudioCapture, CaptureConfig, CaptureMode,
    ConversionPath, NegotiatedFormat,
};
#[cfg(windows)]
pub use playback::{play_pcm_s16, play_sine, PlaybackInfo};
