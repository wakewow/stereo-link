//! WASAPI 回环 / 采集端点的流式采集（仅 Windows 编译）。
//!
//! 对外契约：**44100 Hz / 2 ch / s16le 交错** 的裸 PCM，写进一个 [`RingBuffer`]。
//!
//! 打开设备有两条路径：
//! * **A（首选）**：`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`，让
//!   WASAPI 直接把设备混音格式转换到我们请求的格式（本机实测 48k/2ch/f32 → 44100/2ch/s16 可用）；
//! * **B（兜底）**：按设备混音格式打开，Rust 侧做格式转换 + 线性重采样
//!   （AUTOCONVERTPCM 被驱动拒绝时用）。
//!
//! 两条路径都会在 [`NegotiatedFormat::path`] 里如实报告走了哪条。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::com::{self, ComGuard};
use crate::convert::{remap_channels, SampleFormat, StreamResampler};
use crate::endpoint::{DataFlow, DeviceSelector, EndpointInfo, FormatInfo, PcmFormat};
use crate::error::{AudioError, Result};
use crate::ring::RingBuffer;

/// `AUDCLNT_E_DEVICE_INVALIDATED`（audioclient.h）：端点被拔掉/禁用/切换。
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004u32 as i32;

/// 采集模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// 采某个 **render** 端点的回环（系统正在播放的声音）。
    Loopback,
    /// 采某个 **capture** 端点（麦克风 / 立体声混音 / 将来的虚拟声卡）。
    Capture,
}

impl CaptureMode {
    /// `"loopback"` / `"capture"`。
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureMode::Loopback => "loopback",
            CaptureMode::Capture => "capture",
        }
    }
}

/// 采集配置。
#[derive(Debug, Clone)]
#[allow(missing_docs)] // device/samplerate/channels/mode 字段名即语义
pub struct CaptureConfig {
    pub device: DeviceSelector,
    pub samplerate: u32,
    pub channels: u16,
    pub mode: CaptureMode,
    /// 环形缓冲容量（秒）。
    pub ring_seconds: f32,
    /// WASAPI 共享缓冲时长（毫秒）。
    pub buffer_ms: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            device: DeviceSelector::Default,
            samplerate: 44_100,
            channels: 2,
            mode: CaptureMode::Loopback,
            ring_seconds: 5.0,
            buffer_ms: 200,
        }
    }
}

impl CaptureConfig {
    /// 默认 render 端点 + 回环 + 44100/2ch。
    pub fn loopback_default() -> Self {
        Self::default()
    }

    /// 按设备名子串选端点。
    pub fn with_device_name(mut self, needle: impl Into<String>) -> Self {
        self.device = DeviceSelector::name_contains(needle);
        self
    }

    /// 设置采集模式（回环 / 采集端点）。
    pub fn with_mode(mut self, mode: CaptureMode) -> Self {
        self.mode = mode;
        self
    }

    /// 设置对外输出的采样率。
    pub fn with_samplerate(mut self, samplerate: u32) -> Self {
        self.samplerate = samplerate;
        self
    }

    /// 设置对外输出的声道数。
    pub fn with_channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }

    /// 校验配置：采样率 8000..=192000、声道 1 或 2、`buffer_ms` 20..=2000、`ring_seconds` 0.5..=60。
    pub fn validate(&self) -> Result<()> {
        if !(8_000..=192_000).contains(&self.samplerate) {
            return Err(AudioError::invalid(format!(
                "samplerate={} 不在 8000..=192000",
                self.samplerate
            )));
        }
        if self.channels != 1 && self.channels != 2 {
            return Err(AudioError::invalid(format!(
                "channels={} 只支持 1 或 2",
                self.channels
            )));
        }
        if !(20..=2_000).contains(&self.buffer_ms) {
            return Err(AudioError::invalid(format!(
                "buffer_ms={} 不在 20..=2000",
                self.buffer_ms
            )));
        }
        if !(0.5..=60.0).contains(&self.ring_seconds) {
            return Err(AudioError::invalid(format!(
                "ring_seconds={} 不在 0.5..=60",
                self.ring_seconds
            )));
        }
        Ok(())
    }

    /// 期望的输出格式（目前固定 16 bit）。
    pub fn requested_format(&self) -> PcmFormat {
        PcmFormat::new(self.samplerate, self.channels, 16)
    }

    /// 环形缓冲需要的字节数。
    pub fn ring_bytes(&self) -> usize {
        (self.requested_format().bytes_per_second() as f64 * self.ring_seconds as f64) as usize
    }
}

/// 实际走通的转换路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionPath {
    /// WASAPI 自己转的（`AUTOCONVERTPCM`）。
    WasapiAutoConvert,
    /// 按混音格式打开，Rust 侧转换 + 重采样。
    RustSideConversion,
}

impl ConversionPath {
    /// `"wasapi-autoconvert"` / `"rust-side-conversion"`。
    pub fn as_str(self) -> &'static str {
        match self {
            ConversionPath::WasapiAutoConvert => "wasapi-autoconvert",
            ConversionPath::RustSideConversion => "rust-side-conversion",
        }
    }
}

/// 实际协商出来的格式（`start()` 成功后可用）。
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // 字段名即语义，describe() 给出一行摘要
pub struct NegotiatedFormat {
    pub path: ConversionPath,
    pub mode: CaptureMode,
    pub device_name: String,
    pub device_id: String,
    /// 端点自己的混音格式（设备属性）。
    pub mix_format: FormatInfo,
    /// 我们对外输出的格式（= 请求的 44100/2ch/16bit）。
    pub output: PcmFormat,
}

impl NegotiatedFormat {
    /// 人读一行：设备、模式、转换路径、混音格式、输出格式。
    pub fn describe(&self) -> String {
        format!(
            "{} ← {} [{}] 混音格式 {} → 输出 {}（{}）",
            self.device_name,
            self.mode.as_str(),
            self.path.as_str(),
            self.mix_format.describe(),
            self.output,
            self.device_id
        )
    }
}

/// 列出本机所有活动音频端点（render + capture），含默认标记与混音格式。
pub fn list_endpoints() -> Result<Vec<EndpointInfo>> {
    com::list_endpoints()
}

/// 解析选择器对应的端点（不打开流，预检用）。
pub fn resolve_endpoint(selector: &DeviceSelector, mode: CaptureMode) -> Result<EndpointInfo> {
    let flow = match mode {
        CaptureMode::Loopback => DataFlow::Render,
        CaptureMode::Capture => DataFlow::Capture,
    };
    com::resolve_endpoint(selector, flow)
}

/// 默认 render 端点的信息（回环采集的默认目标）。
pub fn default_endpoint() -> Result<EndpointInfo> {
    resolve_endpoint(&DeviceSelector::Default, CaptureMode::Loopback)
}

// ---------------------------------------------------------------------------
// 格式转换（路径 B）
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct StreamConverter {
    mix_format: SampleFormat,
    mix_channels: usize,
    out_channels: usize,
    resampler: Option<StreamResampler>,
    resample_scratch: Vec<f32>,
}

impl StreamConverter {
    fn new(mix: &FormatInfo, out: PcmFormat) -> Result<Self> {
        let mix_format = mix.sample_format.ok_or_else(|| {
            AudioError::UnsupportedFormat(format!("无法转换的混音格式：{}", mix.describe()))
        })?;
        if mix.channels == 0 {
            return Err(AudioError::UnsupportedFormat(mix.describe()));
        }
        let resampler = if mix.samplerate != out.samplerate {
            Some(StreamResampler::new(
                mix.samplerate,
                out.samplerate,
                out.channels as usize,
            ))
        } else {
            None
        };
        Ok(Self {
            mix_format,
            mix_channels: mix.channels as usize,
            out_channels: out.channels as usize,
            resampler,
            resample_scratch: Vec::new(),
        })
    }

    /// 把一块 WASAPI 缓冲转成 s16le 交错字节。
    fn convert(
        &mut self,
        data: *const u8,
        frames: u32,
        silent: bool,
        f32buf: &mut Vec<f32>,
        out_bytes: &mut Vec<u8>,
    ) -> Result<()> {
        out_bytes.clear();
        f32buf.clear();
        let n_f32 = frames as usize * self.mix_channels;
        if silent || data.is_null() {
            f32buf.resize(n_f32, 0.0);
        } else {
            let n_bytes = n_f32 * self.mix_format.bytes_per_sample();
            let bytes = unsafe { std::slice::from_raw_parts(data, n_bytes) };
            self.mix_format.decode_into(bytes, f32buf)?;
        }
        if self.mix_channels != self.out_channels {
            let remapped = remap_channels(f32buf, self.mix_channels, self.out_channels);
            f32buf.clear();
            f32buf.extend_from_slice(&remapped);
        }
        if let Some(rs) = self.resampler.as_mut() {
            self.resample_scratch.clear();
            let mut scratch = std::mem::take(&mut self.resample_scratch);
            rs.process(f32buf, &mut scratch);
            SampleFormat::S16.encode_into(&scratch, out_bytes);
            self.resample_scratch = scratch;
        } else {
            SampleFormat::S16.encode_into(f32buf, out_bytes);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 采集线程
// ---------------------------------------------------------------------------

struct OpenedStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    negotiated: NegotiatedFormat,
    converter: Option<StreamConverter>,
}

fn map_err(e: windows::core::Error, ctx: &str) -> AudioError {
    if e.code().0 == AUDCLNT_E_DEVICE_INVALIDATED {
        AudioError::DeviceInvalidated
    } else {
        AudioError::wasapi(e.code().0, ctx)
    }
}

fn open_stream(cfg: &CaptureConfig) -> Result<OpenedStream> {
    let en = com::enumerator()?;
    let flow = match cfg.mode {
        CaptureMode::Loopback => DataFlow::Render,
        CaptureMode::Capture => DataFlow::Capture,
    };
    let dev = com::resolve(&en, &cfg.device, flow)?;
    let requested = cfg.requested_format();
    let hns = com::ms_to_hns(cfg.buffer_ms);

    let mut base_flags = 0u32;
    if cfg.mode == CaptureMode::Loopback {
        base_flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }

    // 路径 A：WASAPI 自动转换到 44100/2ch/s16
    let client = com::activate_client(&dev.device)?;
    let mix = com::mix_format(&client)?;
    let wfx = com::pcm16_waveformatex(requested);
    let flags_a = base_flags
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let init_a = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags_a, hns, 0, &wfx, None) };

    let (client, path, converter) = match init_a {
        Ok(()) => (client, ConversionPath::WasapiAutoConvert, None),
        Err(e_a) => {
            // 路径 B：按混音格式打开，自己转
            let client_b = com::activate_client(&dev.device)?;
            let mix_ptr = unsafe { client_b.GetMixFormat() }.map_err(|e| map_err(e, "GetMixFormat"))?;
            let init_b = unsafe {
                client_b.Initialize(AUDCLNT_SHAREMODE_SHARED, base_flags, hns, 0, mix_ptr, None)
            };
            unsafe { CoTaskMemFree(Some(mix_ptr as *const c_void)) };
            match init_b {
                Ok(()) => {
                    let conv = StreamConverter::new(&mix, requested).map_err(|e| {
                        AudioError::wasapi(
                            e_a.code().0,
                            format!("AUTOCONVERTPCM 被拒且混音格式无法在 Rust 侧转换：{e}"),
                        )
                    })?;
                    (client_b, ConversionPath::RustSideConversion, Some(conv))
                }
                Err(e_b) => {
                    return Err(AudioError::wasapi(
                        e_b.code().0,
                        format!(
                            "Initialize 两条路径都失败：AUTOCONVERTPCM=0x{:08X}（{}），混音格式=0x{:08X}",
                            e_a.code().0,
                            e_a.message(),
                            e_b.code().0
                        ),
                    ));
                }
            }
        }
    };

    let capture: IAudioCaptureClient =
        unsafe { client.GetService() }.map_err(|e| map_err(e, "GetService(IAudioCaptureClient)"))?;
    unsafe { client.Start() }.map_err(|e| map_err(e, "IAudioClient::Start"))?;

    let negotiated = NegotiatedFormat {
        path,
        mode: cfg.mode,
        device_name: dev.name.clone(),
        device_id: dev.id.clone(),
        mix_format: mix,
        output: requested,
    };

    Ok(OpenedStream {
        client,
        capture,
        negotiated,
        converter,
    })
}

fn pump(
    cfg: &CaptureConfig,
    st: &mut OpenedStream,
    ring: &Arc<RingBuffer>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let mut f32buf: Vec<f32> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    let direct_frame_bytes = cfg.channels as usize * 2;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut packet = unsafe { st.capture.GetNextPacketSize() }
            .map_err(|e| map_err(e, "GetNextPacketSize"))?;
        if packet == 0 {
            std::thread::sleep(Duration::from_millis(3));
            continue;
        }
        while packet > 0 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe { st.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                .map_err(|e| map_err(e, "IAudioCaptureClient::GetBuffer"))?;
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;

            let converted = if let Some(conv) = st.converter.as_mut() {
                conv.convert(data, frames, silent, &mut f32buf, &mut out)
            } else {
                out.clear();
                let n = frames as usize * direct_frame_bytes;
                if silent || data.is_null() {
                    out.resize(n, 0);
                } else {
                    out.extend_from_slice(unsafe { std::slice::from_raw_parts(data, n) });
                }
                Ok(())
            };
            let released =
                unsafe { st.capture.ReleaseBuffer(frames) }.map_err(|e| map_err(e, "ReleaseBuffer"));
            converted?;
            released?;
            if !out.is_empty() {
                ring.write(&out);
            }
            packet = unsafe { st.capture.GetNextPacketSize() }
                .map_err(|e| map_err(e, "GetNextPacketSize"))?;
        }
    }
    let _ = unsafe { st.client.Stop() };
    Ok(())
}

fn run(
    cfg: CaptureConfig,
    ring: Arc<RingBuffer>,
    stop: Arc<AtomicBool>,
    error_slot: Arc<Mutex<Option<String>>>,
    startup: mpsc::Sender<Result<NegotiatedFormat>>,
) {
    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = startup.send(Err(e));
            ring.close();
            return;
        }
    };
    let mut st = match open_stream(&cfg) {
        Ok(s) => s,
        Err(e) => {
            let _ = startup.send(Err(e));
            ring.close();
            return;
        }
    };
    if startup.send(Ok(st.negotiated.clone())).is_err() {
        ring.close();
        return;
    }
    if let Err(e) = pump(&cfg, &mut st, &ring, &stop) {
        if let Ok(mut slot) = error_slot.lock() {
            *slot = Some(e.to_string());
        }
    }
    drop(st);
    ring.close();
}

// ---------------------------------------------------------------------------
// 公开 API
// ---------------------------------------------------------------------------

/// 一路 WASAPI 采集 → 一个环形缓冲。
///
/// ```no_run
/// use miplay_audio::{AudioCapture, CaptureConfig, DeviceSelector};
/// use std::time::Duration;
///
/// let cfg = CaptureConfig { device: DeviceSelector::Default, ..Default::default() };
/// let mut cap = AudioCapture::new(cfg)?;
/// cap.start()?;
/// let mut buf = vec![0u8; 44_100 * 4]; // 1 秒
/// let n = cap.read(&mut buf, Duration::from_millis(500))?;
/// cap.stop();
/// # Ok::<(), miplay_audio::AudioError>(())
/// ```
pub struct AudioCapture {
    config: CaptureConfig,
    ring: Arc<RingBuffer>,
    cursor: Mutex<u64>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    negotiated: Option<NegotiatedFormat>,
}

impl AudioCapture {
    /// 校验配置并分配环形缓冲（**不**打开设备；设备错误在 [`AudioCapture::start`] 返回）。
    pub fn new(config: CaptureConfig) -> Result<Self> {
        config.validate()?;
        let ring = Arc::new(RingBuffer::new(config.ring_bytes()));
        Ok(Self {
            config,
            ring,
            cursor: Mutex::new(0),
            stop: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            thread: None,
            negotiated: None,
        })
    }

    /// 打开端点并启动后台采集线程；返回时采集已经在跑（或返回具体错误）。
    pub fn start(&mut self) -> Result<()> {
        if self.thread.is_some() {
            return Err(AudioError::invalid("已经 start() 过了"));
        }
        if self.ring.is_closed() {
            return Err(AudioError::Closed);
        }
        let (tx, rx) = mpsc::channel::<Result<NegotiatedFormat>>();
        let cfg = self.config.clone();
        let ring = self.ring.clone();
        let stop = self.stop.clone();
        let error = self.error.clone();
        let handle = std::thread::Builder::new()
            .name("miplay-wasapi-capture".to_string())
            .spawn(move || run(cfg, ring, stop, error, tx))?;
        self.thread = Some(handle);
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(n)) => {
                self.negotiated = Some(n);
                Ok(())
            }
            Ok(Err(e)) => {
                self.join();
                Err(e)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.stop.store(true, Ordering::SeqCst);
                self.join();
                Err(AudioError::StartTimeout(Duration::from_secs(10)))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.join();
                Err(AudioError::CaptureThread(
                    "采集线程在初始化前就退出了".to_string(),
                ))
            }
        }
    }

    /// 停止采集并等线程退出。调用后不能再次 `start()`，请新建实例。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.join();
        self.ring.close();
    }

    fn join(&mut self) {
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }

    /// 从环形缓冲读最多 `buf.len()` 字节，最多等 `timeout`；返回实际字节数。
    ///
    /// * `Ok(0)` = 超时（或缓冲暂时没数据），不是错误；
    /// * 采集线程死亡后，数据被读干时会返回 [`AudioError::CaptureThread`]。
    pub fn read(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let outcome = {
            let mut cur = self.cursor.lock().unwrap();
            let o = self.ring.read_from(*cur, buf.len(), timeout)?;
            *cur = o.next;
            o
        };
        if !outcome.data.is_empty() {
            let n = outcome.data.len();
            buf[..n].copy_from_slice(&outcome.data);
            return Ok(n);
        }
        if let Some(msg) = self.error.lock().unwrap().clone() {
            return Err(AudioError::CaptureThread(msg));
        }
        Ok(0)
    }

    /// [`AudioCapture::read`] 的 Vec 版本（HTTP 层循环用它）。
    pub fn read_vec(&self, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; max_bytes];
        let n = self.read(&mut buf, timeout)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// 本实例的环形缓冲（HTTP 层可自己拿 `Arc` 做多读者分发）。
    pub fn ring(&self) -> Arc<RingBuffer> {
        self.ring.clone()
    }

    /// 当前读位置（绝对字节偏移）。
    pub fn cursor(&self) -> u64 {
        *self.cursor.lock().unwrap()
    }

    /// 采集是否在跑（线程已启动且缓冲未关闭）。
    pub fn is_running(&self) -> bool {
        self.thread.is_some() && !self.ring.is_closed()
    }

    /// 采集线程的致命错误（如果有）。
    pub fn last_error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// 本次采集用的配置。
    pub fn config(&self) -> &CaptureConfig {
        &self.config
    }

    /// `start()` 之后可用：实际协商的格式与转换路径。
    pub fn negotiated_format(&self) -> Option<&NegotiatedFormat> {
        self.negotiated.as_ref()
    }

    /// 端点友好名（`start()` 之后可用）。
    pub fn device_name(&self) -> Option<&str> {
        self.negotiated.as_ref().map(|n| n.device_name.as_str())
    }

    /// 端点 id（`start()` 之后可用）。
    pub fn device_id(&self) -> Option<&str> {
        self.negotiated.as_ref().map(|n| n.device_id.as_str())
    }

    /// 采集模式（回环 / 采集端点）。
    pub fn mode(&self) -> CaptureMode {
        self.config.mode
    }

    /// 对外输出的采样率（默认 44100）。
    pub fn samplerate(&self) -> u32 {
        self.config.samplerate
    }

    /// 对外输出的声道数（默认 2）。
    pub fn channels(&self) -> u16 {
        self.config.channels
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.join();
        self.ring.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_contract() {
        let c = CaptureConfig::default();
        assert_eq!(c.samplerate, 44_100);
        assert_eq!(c.channels, 2);
        assert_eq!(c.mode, CaptureMode::Loopback);
        assert_eq!(c.device, DeviceSelector::Default);
        assert_eq!(c.ring_seconds, 5.0);
        assert_eq!(c.requested_format().frame_bytes(), 4);
        assert_eq!(c.ring_bytes(), 882_000);
        c.validate().unwrap();
    }

    #[test]
    fn config_validate_rejects_nonsense() {
        let base = CaptureConfig::default();
        assert!(CaptureConfig {
            samplerate: 0,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(CaptureConfig {
            samplerate: 8_000,
            ..base.clone()
        }
        .validate()
        .is_ok());
        assert!(CaptureConfig {
            channels: 3,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(CaptureConfig {
            buffer_ms: 5,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(CaptureConfig {
            ring_seconds: 0.1,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(CaptureConfig {
            ring_seconds: 60.0,
            ..base
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn config_builders() {
        let c = CaptureConfig::loopback_default()
            .with_device_name("Chu2")
            .with_mode(CaptureMode::Capture)
            .with_samplerate(48_000)
            .with_channels(1);
        assert_eq!(c.device, DeviceSelector::name_contains("Chu2"));
        assert_eq!(c.mode, CaptureMode::Capture);
        assert_eq!(c.samplerate, 48_000);
        assert_eq!(c.channels, 1);
        assert_eq!(c.requested_format().frame_bytes(), 2);
    }

    #[test]
    fn conversion_path_labels() {
        assert_eq!(ConversionPath::WasapiAutoConvert.as_str(), "wasapi-autoconvert");
        assert_eq!(ConversionPath::RustSideConversion.as_str(), "rust-side-conversion");
        assert_eq!(CaptureMode::Loopback.as_str(), "loopback");
        assert_eq!(CaptureMode::Capture.as_str(), "capture");
    }

    fn f32_mix_format() -> FormatInfo {
        FormatInfo {
            sample_format: Some(SampleFormat::F32),
            channels: 2,
            samplerate: 48_000,
            bits: 32,
            block_align: 8,
            avg_bytes_per_sec: 384_000,
            raw_format_tag: 0xFFFE,
            is_extensible: true,
        }
    }

    /// 路径 B 的核心：48k/2ch/f32 → 44.1k/2ch/s16，频率与幅度要保持。
    #[test]
    fn stream_converter_downsample_f32_to_s16() {
        let out_fmt = PcmFormat::new(44_100, 2, 16);
        let mut conv = StreamConverter::new(&f32_mix_format(), out_fmt).unwrap();
        // 1 秒 1 kHz 正弦，0.5 幅度，左右同相
        let frames = 48_000usize;
        let mut pcm = Vec::with_capacity(frames * 8);
        for i in 0..frames {
            let s = 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin() as f32;
            pcm.extend_from_slice(&s.to_le_bytes());
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        let mut f32buf = Vec::new();
        let mut bytes = Vec::new();
        // 分块喂，模拟 WASAPI 包
        for chunk in pcm.chunks(48_000 * 8 / 10) {
            conv.convert(chunk.as_ptr(), (chunk.len() / 8) as u32, false, &mut f32buf, &mut bytes)
                .unwrap();
            assert_eq!(bytes.len() % 4, 0);
        }
        // 注意：上面每次 convert 都会覆盖 bytes，这里单独再跑一次收全程
        let mut all = Vec::new();
        let mut conv = StreamConverter::new(&f32_mix_format(), out_fmt).unwrap();
        for chunk in pcm.chunks(48_000 * 8 / 10) {
            conv.convert(chunk.as_ptr(), (chunk.len() / 8) as u32, false, &mut f32buf, &mut bytes)
                .unwrap();
            all.extend_from_slice(&bytes);
        }
        let expected_bytes = 44_100 * 4;
        assert!(
            (all.len() as i64 - expected_bytes as i64).abs() < 4_000,
            "len={}",
            all.len()
        );
        let samples: Vec<i16> = all
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let peak = samples.iter().map(|s| (*s as i32).abs()).max().unwrap();
        let expected_peak = (0.5 * 32_767.0) as i32;
        assert!(
            (peak - expected_peak).abs() <= expected_peak / 20,
            "peak={peak} expected≈{expected_peak}"
        );
        let left: Vec<f32> = samples
            .iter()
            .step_by(2)
            .map(|s| *s as f32 / 32_768.0)
            .collect();
        let freq = crate::convert::estimate_frequency(&left, 44_100).unwrap();
        assert!((freq - 1000.0).abs() < 5.0, "freq={freq}");
    }

    #[test]
    fn stream_converter_silent_flag_writes_zeros() {
        let out_fmt = PcmFormat::new(44_100, 2, 16);
        let mut conv = StreamConverter::new(&f32_mix_format(), out_fmt).unwrap();
        let mut f32buf = Vec::new();
        let mut bytes = Vec::new();
        conv.convert(std::ptr::null(), 480, true, &mut f32buf, &mut bytes)
            .unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.iter().all(|b| *b == 0));
    }

    #[test]
    fn stream_converter_rejects_unknown_format() {
        let mut mix = f32_mix_format();
        mix.sample_format = None;
        assert!(StreamConverter::new(&mix, PcmFormat::new(44_100, 2, 16)).is_err());
    }
}
