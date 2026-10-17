//! 往 render 端点播声音（仅 Windows 编译）。
//!
//! 存在的意义是**真机自测**：采集链路必须能在「真的在放声音」的时候采到非零数据。
//! 默认用 44100/2ch/s16 通过 `AUTOCONVERTPCM` 播放，与采集侧的对外契约一致。

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    IAudioRenderClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::capture::ConversionPath;
use crate::com::{self, ComGuard};
use crate::convert::build_sine_s16;
use crate::endpoint::{DataFlow, DeviceSelector, FormatInfo, PcmFormat};
use crate::error::{AudioError, Result};

/// 一次播放的实测信息。
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // 字段名即语义
pub struct PlaybackInfo {
    pub device_name: String,
    pub device_id: String,
    pub path: ConversionPath,
    pub requested: PcmFormat,
    pub mix_format: FormatInfo,
    pub frames: u64,
    pub seconds: f64,
}

impl PlaybackInfo {
    /// 人读一行：设备、混音格式、播放格式、时长、实际走的转换路径。
    pub fn describe(&self) -> String {
        format!(
            "{} [{}] 混音格式 {} ← 播放 {} ({:.3}s, {} 帧, {})",
            self.device_name,
            self.device_id,
            self.mix_format.describe(),
            self.requested,
            self.seconds,
            self.frames,
            self.path.as_str()
        )
    }
}

const PREFILL_MS: u32 = 1_000;
const WRITE_CHUNK_FRAMES: usize = 4_096;

fn map_err(e: windows::core::Error, ctx: &str) -> AudioError {
    AudioError::wasapi(e.code().0, ctx)
}

/// 往默认（或指定）render 端点播一段 s16le PCM。
pub fn play_pcm_s16(device: &DeviceSelector, format: PcmFormat, pcm: &[u8]) -> Result<PlaybackInfo> {
    if format.bits != 16 {
        return Err(AudioError::invalid(format!(
            "play_pcm_s16 只接受 16 bit，收到 {}",
            format.bits
        )));
    }
    let frame_bytes = format.frame_bytes();
    if frame_bytes == 0 || pcm.len() % frame_bytes != 0 {
        return Err(AudioError::invalid(format!(
            "PCM 长度 {} 不是帧长 {} 的整数倍",
            pcm.len(),
            frame_bytes
        )));
    }
    let total_frames = pcm.len() / frame_bytes;
    if total_frames == 0 {
        return Err(AudioError::invalid("PCM 是空的"));
    }

    let _com = ComGuard::new()?;
    let en = com::enumerator()?;
    let dev = com::resolve(&en, device, DataFlow::Render)?;
    let client = com::activate_client(&dev.device)?;
    let mix = com::mix_format(&client)?;
    let hns = com::ms_to_hns(PREFILL_MS);

    // 路径 A：请求目标格式，让 WASAPI 转（和采集侧同一套 flag）
    let wfx = com::pcm16_waveformatex(format);
    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let mut path = ConversionPath::WasapiAutoConvert;
    let mut init = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, hns, 0, &wfx, None) };
    if let Err(e_a) = init {
        // 路径 B：只有当目标格式与端点混音格式一致时才能直接打开
        let same = mix.channels == format.channels
            && mix.samplerate == format.samplerate
            && mix.bits == 16;
        if !same {
            return Err(AudioError::wasapi(
                e_a.code().0,
                format!(
                    "AUTOCONVERTPCM 播放失败，且目标格式 {} 与混音格式 {} 不一致，无法兜底",
                    format,
                    mix.describe()
                ),
            ));
        }
        let fmt_ptr =
            unsafe { client.GetMixFormat() }.map_err(|e| map_err(e, "GetMixFormat"))?;
        init = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, hns, 0, fmt_ptr, None) };
        unsafe { CoTaskMemFree(Some(fmt_ptr as *const c_void)) };
        path = ConversionPath::RustSideConversion;
    }
    init.map_err(|e| map_err(e, "IAudioClient::Initialize(render)"))?;

    let buffer_frames =
        unsafe { client.GetBufferSize() }.map_err(|e| map_err(e, "GetBufferSize"))?;
    let render: IAudioRenderClient =
        unsafe { client.GetService() }.map_err(|e| map_err(e, "GetService(IAudioRenderClient)"))?;

    // 预填静音（避免 Start() 后立刻 underrun）
    {
        let p = unsafe { render.GetBuffer(buffer_frames) }.map_err(|e| map_err(e, "GetBuffer"))?;
        if !p.is_null() {
            unsafe { std::ptr::write_bytes(p, 0, buffer_frames as usize * frame_bytes) };
        }
        unsafe { render.ReleaseBuffer(buffer_frames, 0) }
            .map_err(|e| map_err(e, "ReleaseBuffer"))?;
    }
    unsafe { client.Start() }.map_err(|e| map_err(e, "IAudioClient::Start"))?;

    let mut written = 0usize;
    while written < total_frames {
        let pad = unsafe { client.GetCurrentPadding() }.map_err(|e| map_err(e, "GetCurrentPadding"))?;
        let avail = (buffer_frames.saturating_sub(pad) as usize)
            .min(total_frames - written)
            .min(WRITE_CHUNK_FRAMES);
        if avail == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let n = avail as u32;
        let off = written * frame_bytes;
        let p = unsafe { render.GetBuffer(n) }.map_err(|e| map_err(e, "GetBuffer"))?;
        if p.is_null() {
            return Err(AudioError::invalid("IAudioRenderClient::GetBuffer 返回空指针"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                pcm[off..].as_ptr(),
                p,
                n as usize * frame_bytes,
            )
        };
        unsafe { render.ReleaseBuffer(n, 0) }.map_err(|e| map_err(e, "ReleaseBuffer"))?;
        written += n as usize;
    }

    // 等缓冲区放空（有上限，避免设备不出声时死等）
    let seconds = total_frames as f64 / format.samplerate as f64;
    let deadline = Instant::now() + Duration::from_secs_f64(seconds + 3.0);
    loop {
        let pad = unsafe { client.GetCurrentPadding() }.map_err(|e| map_err(e, "GetCurrentPadding"))?;
        if pad == 0 || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = unsafe { client.Stop() };

    Ok(PlaybackInfo {
        device_name: dev.name,
        device_id: dev.id,
        path,
        requested: format,
        mix_format: mix,
        frames: total_frames as u64,
        seconds,
    })
}

/// 播一段正弦（默认 44100/2ch/s16，可直接和回环采集对照）。
pub fn play_sine(
    device: &DeviceSelector,
    freq_hz: f64,
    seconds: f64,
    amplitude: f32,
) -> Result<PlaybackInfo> {
    let format = PcmFormat::new(44_100, 2, 16);
    let pcm = build_sine_s16(freq_hz, seconds, amplitude, format);
    play_pcm_s16(device, format, &pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_rejects_bad_pcm_length() {
        let err = play_pcm_s16(
            &DeviceSelector::Default,
            PcmFormat::new(44_100, 2, 16),
            &[0u8; 6],
        )
        .unwrap_err();
        assert!(matches!(err, AudioError::InvalidArgument(_)), "{err}");
        let err = play_pcm_s16(
            &DeviceSelector::Default,
            PcmFormat::new(44_100, 2, 24),
            &[0u8; 6],
        )
        .unwrap_err();
        assert!(matches!(err, AudioError::InvalidArgument(_)), "{err}");
    }

    #[test]
    fn playback_info_describe() {
        let info = PlaybackInfo {
            device_name: "测试".into(),
            device_id: "id".into(),
            path: ConversionPath::WasapiAutoConvert,
            requested: PcmFormat::new(44_100, 2, 16),
            mix_format: FormatInfo {
                sample_format: Some(crate::convert::SampleFormat::F32),
                channels: 2,
                samplerate: 48_000,
                bits: 32,
                block_align: 8,
                avg_bytes_per_sec: 384_000,
                raw_format_tag: 0xFFFE,
                is_extensible: true,
            },
            frames: 44_100,
            seconds: 1.0,
        };
        let d = info.describe();
        assert!(d.contains("44100 Hz"), "{d}");
        assert!(d.contains("wasapi-autoconvert"), "{d}");
    }
}
