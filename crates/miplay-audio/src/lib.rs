//! # miplay-audio
//!
//! 把 **Windows 正在播放的系统声音**抓成 PCM，并分发给多个输出目标（音箱 / HTTP 流）。
//!
//! 数据流：
//!
//! ```text
//! 扬声器端点 ──WASAPI loopback──▶ AudioCapture ──44100/2ch/s16le──▶ RingBuffer
//!                                                                     │
//!                                              ChannelRouter ─────────┘
//!                                                    ├─ left  流 (RingBuffer)
//!                                                    ├─ right 流 (RingBuffer)
//!                                                    └─ 任意 N 条流
//! ```
//!
//! 与 Python 参考实现 `mixplay-pc-audio/pc_stereo_hub.py` 对齐的语义：
//! * 44100 Hz / 2 ch / 16 bit 交错 PCM；
//! * WAV 头 `data` 大小写死 [`FAKE_DATA_SIZE`]，当成无限长流；
//! * 环形缓冲默认 5 秒，新读者从**当前**时刻开始听，慢读者跳帧；
//! * 左右分离 = 把 L / R 各复制成 2ch 各成一条流。
//!
//! ## 最小用法
//!
//! ```no_run
//! use miplay_audio::{AudioCapture, CaptureConfig, ChannelRouter, ChannelSource, ChannelRoute};
//! use std::time::Duration;
//!
//! let mut capture = AudioCapture::new(CaptureConfig::default())?;
//! capture.start()?;
//!
//! let router = ChannelRouter::for_seconds(
//!     vec![
//!         ChannelRoute::new("left", ChannelSource::Left),
//!         ChannelRoute::new("right", ChannelSource::Right),
//!     ],
//!     44_100,
//!     2,
//!     5.0,
//! );
//!
//! loop {
//!     let chunk = capture.read_vec(44_100 / 50, Duration::from_millis(200))?;
//!     if chunk.is_empty() {
//!         continue;
//!     }
//!     router.write_s16_stereo(&chunk);
//! }
//! # Ok::<(), miplay_audio::AudioError>(())
//! ```
//!
//! HTTP 层（`miplay-server`）要做的只是：`GET /stream/left` → 先写
//! [`build_stream_wav_header`]，然后循环 `ring.read_from(pos, 8192, 200ms)`，
//! 把 `data` 写进响应体，直到连接断开或 `closed`。
//!
//! ## 平台
//!
//! `capture` / `playback` 只在 Windows 上编译；`channels` / `ring` / `wav` /
//! `convert` / `endpoint` 是纯逻辑，任何平台都能跑单测。

#![deny(missing_docs)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod channels;
pub mod convert;
pub mod endpoint;
pub mod error;
pub mod ring;
pub mod wav;

#[cfg(windows)]
mod com;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod playback;

pub use channels::{
    mixdown_routes, stereo_routes, ChannelRoute, ChannelRouter, ChannelSource,
};
pub use convert::{
    build_sine_s16, decode_s16le_into, deinterleave, encode_s16le_into, estimate_frequency,
    f32_to_s16, remap_channels, resample_f32, s16_to_f32, SampleFormat, StreamResampler,
};
pub use endpoint::{DataFlow, DeviceSelector, EndpointInfo, FormatInfo, PcmFormat};
pub use error::{AudioError, Result};
pub use ring::{ReadOutcome, RingBuffer};
pub use wav::{build_stream_wav_header, build_wav_header, write_wav, FAKE_DATA_SIZE, WAV_HEADER_LEN};

#[cfg(windows)]
pub use capture::{
    default_endpoint, list_endpoints, resolve_endpoint, AudioCapture, CaptureConfig, CaptureMode,
    ConversionPath, NegotiatedFormat,
};
#[cfg(windows)]
pub use playback::{play_pcm_s16, play_sine, PlaybackInfo};
