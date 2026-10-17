//! 蓝牙左右声道分离（仅 Windows）：把虚拟声卡的回环采下来，左声道送一台音箱、右声道送另一台。
//!
//! 用途：两台小米 Sound 各自通过蓝牙连到本机后，Windows 只能把同一条立体声混音送给
//! 「一个」输出端点，没法左右分开。这里做的是：
//!
//! ```text
//! Windows 播放器 → 扬声器 (立体声链设备) 虚拟声卡
//!                   │ WASAPI loopback 采集（44.1k/2ch/s16）
//!                   ▼
//!              L ──► render 端点 A（蓝牙音箱 1），双声道都填 L
//!              R ──► render 端点 B（蓝牙音箱 2），双声道都填 R
//! ```
//!
//! 每个端点一条独立线程：轮询 `IAudioClient::GetCurrentPadding`，缓冲有空位就从队列里
//! 取 10 ms 写进去；队列空了就补静音避免欠载。两条蓝牙链路时钟独立，靠队列长度吸收
//! 漂移（超过上限丢最老的采样，见 `queue_ms`）。
//!
//! 这是**用户态**实现，不碰内核驱动；虚拟声卡沿用已有的 Steam Streaming Speakers 改名端点。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    IAudioRenderClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};

use crate::capture::{AudioCapture, CaptureConfig, CaptureMode};
use crate::com::{self, ComGuard};
use crate::endpoint::{DataFlow, DeviceSelector, PcmFormat};
use crate::error::{AudioError, Result};

/// 分离输出的采样率（与 Windows 侧采集、HTTP 流一致）。
pub const SPLIT_SAMPLE_RATE: u32 = 44_100;
/// 每次写 render 端点的帧数（10 ms）。
const RENDER_CHUNK_FRAMES: usize = (SPLIT_SAMPLE_RATE / 100) as usize;
/// 每个 sink 每次最多往缓冲里预填多少帧静音（避免 `Start` 后立刻欠载）。
const RENDER_PREFILL_MS: u32 = 10;

/// 左右分离的配置。
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // 字段名即语义
pub struct BtSplitConfig {
    /// 左声道目标端点（友好名子串，如 `Xiaomi Sound-1695`）。
    pub left_endpoint: String,
    /// 右声道目标端点（友好名子串，如 `Xiaomi Sound-5320`）。
    pub right_endpoint: String,
    /// 采集源端点（回环）。虚拟声卡友好名子串，默认 `立体声链`。
    pub source_endpoint: String,
    /// 采集增益（0.0–8.0）。
    pub capture_gain: f32,
    /// render 端 WASAPI 共享模式缓冲（毫秒，10–500）。
    pub render_buffer_ms: u32,
    /// 每个 sink 预排队的音频长度（毫秒，0–500）。越大越抗抖动，延迟越高。
    pub queue_ms: u32,
    /// 左声道额外延迟（毫秒，0–1000），用于两条链路听起来错开时对齐。
    pub left_delay_ms: u32,
    /// 右声道额外延迟（毫秒，0–1000）。
    pub right_delay_ms: u32,
}

impl Default for BtSplitConfig {
    fn default() -> Self {
        Self {
            left_endpoint: String::new(),
            right_endpoint: String::new(),
            source_endpoint: "立体声链".to_owned(),
            capture_gain: 1.0,
            render_buffer_ms: 40,
            queue_ms: 40,
            left_delay_ms: 0,
            right_delay_ms: 0,
        }
    }
}

impl BtSplitConfig {
    /// 校验配置（不打开设备）。
    pub fn validate(&self) -> Result<()> {
        if self.left_endpoint.trim().is_empty() {
            return Err(AudioError::invalid("左声道端点不能为空"));
        }
        if self.right_endpoint.trim().is_empty() {
            return Err(AudioError::invalid("右声道端点不能为空"));
        }
        if self.left_endpoint.trim() == self.right_endpoint.trim() {
            return Err(AudioError::invalid("左右声道不能选同一个端点"));
        }
        if self.source_endpoint.trim().is_empty() {
            return Err(AudioError::invalid("采集源端点不能为空"));
        }
        if !(0.0..=8.0).contains(&self.capture_gain) {
            return Err(AudioError::invalid(format!(
                "采集增益 {} 不在 0.0..=8.0",
                self.capture_gain
            )));
        }
        if !(10..=500).contains(&self.render_buffer_ms) {
            return Err(AudioError::invalid(format!(
                "render_buffer_ms={} 不在 10..=500",
                self.render_buffer_ms
            )));
        }
        if self.queue_ms > 500 {
            return Err(AudioError::invalid(format!(
                "queue_ms={} 超过 500",
                self.queue_ms
            )));
        }
        for (name, ms) in [("left_delay_ms", self.left_delay_ms), ("right_delay_ms", self.right_delay_ms)]
        {
            if ms > 1_000 {
                return Err(AudioError::invalid(format!("{name}={ms} 超过 1000")));
            }
        }
        Ok(())
    }

    /// 计划值：软件侧最少要排多少毫秒（不含蓝牙链路本身）。
    pub fn software_latency_ms(&self) -> u32 {
        // 采集 WASAPI 缓冲（20 ms，见 run_capture）+ 预排队 + render 缓冲的一半（平均占用）
        20 + self.queue_ms + self.render_buffer_ms / 2
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        format!(
            "L→{} / R→{} | 采集源 {} | 队列 {} ms + render {} ms | 增益 {:.2} | 左延迟 {} ms / 右延迟 {} ms",
            self.left_endpoint,
            self.right_endpoint,
            self.source_endpoint,
            self.queue_ms,
            self.render_buffer_ms,
            self.capture_gain,
            self.left_delay_ms,
            self.right_delay_ms
        )
    }
}

/// 单边（左或右）sink 的运行统计。
#[derive(Debug, Clone, Default, PartialEq)]
#[allow(missing_docs)] // 字段名即语义
pub struct SplitSinkStats {
    pub endpoint: String,
    pub sent_bytes: u64,
    pub underruns: u64,
    pub dropped_ms: f64,
    pub queued_ms: f64,
    pub error: Option<String>,
}

/// 左右分离的整体快照。
#[derive(Debug, Clone, Default, PartialEq)]
#[allow(missing_docs)] // 字段名即语义
pub struct BtSplitStats {
    pub running: bool,
    pub source_device: String,
    pub source_error: Option<String>,
    pub captured_frames: u64,
    pub frame_bytes: u64,
    pub uptime_seconds: f64,
    /// 采集到的左声道峰值（0..1，带衰减，界面电平条用）。
    pub level_l: f32,
    /// 采集到的右声道峰值（0..1）。
    pub level_r: f32,
    pub left: SplitSinkStats,
    pub right: SplitSinkStats,
}

impl BtSplitStats {
    /// 人读一行（GUI 日志/自检用）。
    pub fn describe(&self) -> String {
        format!(
            "采集 {} 帧（{:.2} MB）| L {} 已发 {} B 欠载 {} 丢 {} ms 队列 {:.0} ms | R {} 已发 {} B 欠载 {} 丢 {} ms 队列 {:.0} ms",
            self.captured_frames,
            self.frame_bytes as f64 / 1_048_576.0,
            self.left.endpoint,
            self.left.sent_bytes,
            self.left.underruns,
            self.left.dropped_ms,
            self.left.queued_ms,
            self.right.endpoint,
            self.right.sent_bytes,
            self.right.underruns,
            self.right.dropped_ms,
            self.right.queued_ms
        )
    }
}

struct SinkShared {
    endpoint: String,
    error: Option<String>,
    sent_bytes: u64,
    underruns: u64,
    dropped_samples: u64,
    queue: VecDeque<i16>,
    queue_cap_samples: usize,
}

impl SinkShared {
    fn new(endpoint: &str, queue_cap_samples: usize) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            error: None,
            sent_bytes: 0,
            underruns: 0,
            dropped_samples: 0,
            queue: VecDeque::with_capacity(queue_cap_samples.max(1)),
            queue_cap_samples: queue_cap_samples.max(1),
        }
    }

    fn push(&mut self, samples: impl Iterator<Item = i16>) {
        for sample in samples {
            if self.queue.len() >= self.queue_cap_samples {
                // 两条链路时钟独立：积压超过上限就丢最老的，保证延迟不无限涨。
                self.queue.pop_front();
                self.dropped_samples += 1;
            }
            self.queue.push_back(sample);
        }
    }
}

struct Shared {
    stop: AtomicBool,
    threads_alive: AtomicU32,
    captured_frames: AtomicU64,
    /// 最近一段采集的左/右峰值（f32::to_bits），界面电平条用。
    level_l: AtomicU32,
    level_r: AtomicU32,
    started: Instant,
    left: Mutex<SinkShared>,
    right: Mutex<SinkShared>,
    source_device: Mutex<String>,
    source_error: Mutex<Option<String>>,
}

impl Shared {
    fn sink(&self, left: bool) -> &Mutex<SinkShared> {
        if left {
            &self.left
        } else {
            &self.right
        }
    }

    fn set_sink_error(&self, left: bool, msg: String) {
        if let Ok(mut s) = self.sink(left).lock() {
            if s.error.is_none() {
                s.error = Some(msg);
            }
        }
        self.stop.store(true, Ordering::Relaxed);
    }

    fn sink_stats(&self, left: bool) -> SplitSinkStats {
        match self.sink(left).lock() {
            Ok(s) => SplitSinkStats {
                endpoint: s.endpoint.clone(),
                sent_bytes: s.sent_bytes,
                underruns: s.underruns,
                dropped_ms: s.dropped_samples as f64 * 1000.0 / SPLIT_SAMPLE_RATE as f64,
                queued_ms: s.queue.len() as f64 * 1000.0 / SPLIT_SAMPLE_RATE as f64,
                error: s.error.clone(),
            },
            Err(_) => SplitSinkStats::default(),
        }
    }
}

/// 运行中的左右分离句柄。`Drop` 时自动停止所有线程。
pub struct BtSplitHandle {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    config: BtSplitConfig,
}

impl BtSplitHandle {
    /// 启动分离输出：两条 render 线程 + 一条采集线程。
    ///
    /// 返回前会等两条 render 流都打开（最多 3 秒），任一端点打不开就返回错误。
    pub fn start(config: BtSplitConfig) -> Result<Self> {
        config.validate()?;
        let queue_cap_samples =
            (SPLIT_SAMPLE_RATE as u64 * config.queue_ms as u64 / 1000) as usize;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            threads_alive: AtomicU32::new(2),
            captured_frames: AtomicU64::new(0),
            level_l: AtomicU32::new(0),
            level_r: AtomicU32::new(0),
            started: Instant::now(),
            left: Mutex::new(SinkShared::new(&config.left_endpoint, queue_cap_samples)),
            right: Mutex::new(SinkShared::new(&config.right_endpoint, queue_cap_samples)),
            source_device: Mutex::new(String::new()),
            source_error: Mutex::new(None),
        });

        let mut threads = Vec::new();
        for is_left in [true, false] {
            let shared = shared.clone();
            let cfg = config.clone();
            threads.push(
                thread::Builder::new()
                    .name(if is_left {
                        "stereo-link-split-left".to_string()
                    } else {
                        "stereo-link-split-right".to_string()
                    })
                    .spawn(move || run_sink(shared, cfg, is_left))
                    .map_err(|e| AudioError::invalid(format!("spawn sink 线程失败：{e}")))?,
            );
        }

        // 等两条 render 流就绪（或报错），最多 3 秒。
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let err = {
                let l = shared.left.lock().ok().and_then(|s| s.error.clone());
                let r = shared.right.lock().ok().and_then(|s| s.error.clone());
                l.or(r)
            };
            if let Some(err) = err {
                shared.stop.store(true, Ordering::Relaxed);
                for t in threads.drain(..) {
                    let _ = t.join();
                }
                return Err(AudioError::invalid(format!("打开 render 端点失败：{err}")));
            }
            let alive = shared.threads_alive.load(Ordering::Relaxed);
            if alive < 2 || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        // render 就绪后再开采集（避免一开始就丢数据）。
        {
            let shared = shared.clone();
            let cfg = config.clone();
            threads.push(
                thread::Builder::new()
                    .name("stereo-link-split-capture".to_string())
                    .spawn(move || run_capture(shared, cfg))
                    .map_err(|e| AudioError::invalid(format!("spawn capture 线程失败：{e}")))?,
            );
        }

        Ok(Self {
            shared,
            threads,
            config,
        })
    }

    /// 当前配置。
    pub fn config(&self) -> &BtSplitConfig {
        &self.config
    }

    /// 是否还在跑。
    pub fn alive(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed)
            && self.shared.threads_alive.load(Ordering::Relaxed) > 1
    }

    /// 拍一张统计快照。
    pub fn stats(&self) -> BtSplitStats {
        let source_device = self
            .shared
            .source_device
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        let source_error = self
            .shared
            .source_error
            .lock()
            .ok()
            .and_then(|s| s.clone());
        let frames = self.shared.captured_frames.load(Ordering::Relaxed);
        BtSplitStats {
            running: self.alive(),
            source_device,
            source_error,
            captured_frames: frames,
            frame_bytes: frames * 4, // 44.1k/2ch/s16 = 4 B/帧
            uptime_seconds: self.shared.started.elapsed().as_secs_f64(),
            level_l: f32::from_bits(self.shared.level_l.load(Ordering::Relaxed)),
            level_r: f32::from_bits(self.shared.level_r.load(Ordering::Relaxed)),
            left: self.shared.sink_stats(true),
            right: self.shared.sink_stats(false),
        }
    }

    /// 停止（幂等）。
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for BtSplitHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_sink(shared: Arc<Shared>, cfg: BtSplitConfig, is_left: bool) {
    let endpoint = if is_left {
        cfg.left_endpoint.clone()
    } else {
        cfg.right_endpoint.clone()
    };
    let delay_ms = if is_left {
        cfg.left_delay_ms
    } else {
        cfg.right_delay_ms
    };
    let res = sink_loop(&shared, &endpoint, delay_ms, cfg.render_buffer_ms, is_left);
    if let Err(e) = res {
        shared.set_sink_error(is_left, e.to_string());
    }
    if shared.threads_alive.fetch_sub(1, Ordering::Relaxed) == 1 {
        shared.stop.store(true, Ordering::Relaxed);
    }
}

fn sink_loop(
    shared: &Arc<Shared>,
    endpoint: &str,
    delay_ms: u32,
    render_buffer_ms: u32,
    is_left: bool,
) -> Result<()> {
    let _com = ComGuard::new()?;
    let en = com::enumerator()?;
    let selector = DeviceSelector::name_contains(endpoint);
    let device = com::resolve(&en, &selector, DataFlow::Render)?;
    let client = com::activate_client(&device.device)?;
    let _mix = com::mix_format(&client)?;

    let format = PcmFormat::new(SPLIT_SAMPLE_RATE, 2, 16);
    let wfx = com::pcm16_waveformatex(format);
    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            com::ms_to_hns(render_buffer_ms),
            0,
            &wfx,
            None,
        )
    }
    .map_err(|e| AudioError::wasapi(e.code().0, "Initialize(bt-split render)"))?;

    let buffer_frames = unsafe { client.GetBufferSize() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBufferSize(bt-split)"))? as usize;
    let render: IAudioRenderClient = unsafe { client.GetService() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetService(IAudioRenderClient)"))?;

    // 预填一点静音，避免 Start 后第一轮就欠载。
    let prefill = (RENDER_PREFILL_MS as usize * SPLIT_SAMPLE_RATE as usize / 1000).min(buffer_frames);
    if prefill > 0 {
        write_silence(&render, prefill)?;
    }
    unsafe { client.Start() }.map_err(|e| AudioError::wasapi(e.code().0, "Start(bt-split)"))?;

    let mut skip_frames = (delay_ms as u64 * SPLIT_SAMPLE_RATE as u64 / 1000) as usize;
    let result = (|| -> Result<()> {
        while !shared.stop.load(Ordering::Relaxed) {
            let padding = unsafe { client.GetCurrentPadding() }
                .map_err(|e| AudioError::wasapi(e.code().0, "GetCurrentPadding(bt-split)"))? as usize;
            let available = buffer_frames.saturating_sub(padding);
            if available == 0 {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            let want = available.min(RENDER_CHUNK_FRAMES);

            if skip_frames > 0 {
                let n = want.min(skip_frames);
                write_silence(&render, n)?;
                skip_frames -= n;
                continue;
            }

            let mut stereo: Vec<i16> = Vec::with_capacity(want * 2);
            let mut got = 0usize;
            let mut queue_empty = false;
            {
                let mut sink = shared
                    .sink(is_left)
                    .lock()
                    .map_err(|_| AudioError::invalid("sink 状态锁被 poison"))?;
                while got < want {
                    match sink.queue.pop_front() {
                        Some(sample) => {
                            stereo.push(sample); // L→两个声道都填同一路
                            stereo.push(sample);
                            got += 1;
                        }
                        None => {
                            queue_empty = true;
                            break;
                        }
                    }
                }
                sink.sent_bytes += (got * 4) as u64;
                if queue_empty && got == 0 && shared.captured_frames.load(Ordering::Relaxed) > 0 {
                    // 已经有声音在采、但这一路没数据了：补静音并计一次欠载。
                    sink.underruns += 1;
                }
            }

            if got == 0 {
                write_silence(&render, want)?;
                continue;
            }
            write_stereo(&render, &stereo)?;
            if got < want {
                // 队列短暂不够：剩下的补静音，别让设备欠载。
                write_silence(&render, want - got)?;
            }
        }
        Ok(())
    })();

    unsafe { client.Stop() }.map_err(|e| AudioError::wasapi(e.code().0, "Stop(bt-split)"))?;
    result
}

fn write_silence(render: &IAudioRenderClient, frames: usize) -> Result<()> {
    if frames == 0 {
        return Ok(());
    }
    let ptr = unsafe { render.GetBuffer(frames as u32) }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBuffer(silence)"))?;
    if ptr.is_null() {
        return Err(AudioError::invalid("IAudioRenderClient::GetBuffer 返回空指针"));
    }
    unsafe { std::ptr::write_bytes(ptr, 0, frames * 4) };
    unsafe { render.ReleaseBuffer(frames as u32, 0) }
        .map_err(|e| AudioError::wasapi(e.code().0, "ReleaseBuffer(silence)"))?;
    Ok(())
}

fn write_stereo(render: &IAudioRenderClient, samples: &[i16]) -> Result<()> {
    let frames = samples.len() / 2;
    if frames == 0 {
        return Ok(());
    }
    let ptr = unsafe { render.GetBuffer(frames as u32) }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBuffer(stereo)"))?;
    if ptr.is_null() {
        return Err(AudioError::invalid("IAudioRenderClient::GetBuffer 返回空指针"));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(samples.as_ptr() as *const u8, ptr, samples.len() * 2);
    }
    unsafe { render.ReleaseBuffer(frames as u32, 0) }
        .map_err(|e| AudioError::wasapi(e.code().0, "ReleaseBuffer(stereo)"))?;
    Ok(())
}

fn run_capture(shared: Arc<Shared>, cfg: BtSplitConfig) {
    if let Err(e) = capture_loop(&shared, &cfg) {
        if let Ok(mut slot) = shared.source_error.lock() {
            *slot = Some(e.to_string());
        }
        shared.stop.store(true, Ordering::Relaxed);
    }
    shared.threads_alive.fetch_sub(1, Ordering::Relaxed);
}

fn capture_loop(shared: &Arc<Shared>, cfg: &BtSplitConfig) -> Result<()> {
    let mut capture_config = CaptureConfig::default();
    capture_config.device = DeviceSelector::name_contains(cfg.source_endpoint.clone());
    capture_config.mode = CaptureMode::Loopback;
    capture_config.samplerate = SPLIT_SAMPLE_RATE;
    capture_config.channels = 2;
    capture_config.buffer_ms = 20;
    capture_config.ring_seconds = 1.0;

    let mut capture = AudioCapture::new(capture_config)?;
    capture.start()?;
    if let Some(name) = capture.device_name() {
        if let Ok(mut slot) = shared.source_device.lock() {
            *slot = name.to_string();
        }
    }

    let mut buf = vec![0u8; RENDER_CHUNK_FRAMES * 4];
    while !shared.stop.load(Ordering::Relaxed) {
        let n = match capture.read(&mut buf, Duration::from_millis(200)) {
            Ok(n) => n,
            Err(e) => {
                // 采集线程正常结束时也会读到错误，只有还在跑才当成故障。
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                return Err(e);
            }
        };
        if n == 0 {
            continue;
        }
        let frames = n / 4;
        shared
            .captured_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        let gain = cfg.capture_gain;
        let mut left: Vec<i16> = Vec::with_capacity(frames);
        let mut right: Vec<i16> = Vec::with_capacity(frames);
        let (mut peak_l, mut peak_r) = (0.0f32, 0.0f32);
        for frame in buf[..n].chunks_exact(4) {
            let l = apply_gain(i16::from_le_bytes([frame[0], frame[1]]), gain);
            let r = apply_gain(i16::from_le_bytes([frame[2], frame[3]]), gain);
            peak_l = peak_l.max((l as f32 / 32768.0).abs());
            peak_r = peak_r.max((r as f32 / 32768.0).abs());
            left.push(l);
            right.push(r);
        }
        if let Ok(mut sink) = shared.left.lock() {
            sink.push(left.into_iter());
        }
        if let Ok(mut sink) = shared.right.lock() {
            sink.push(right.into_iter());
        }
        // 更新电平条：本块峰值与旧值取大（旧值每块衰减 20%，视觉上约 100 ms 的保持）。
        let prev_l = f32::from_bits(shared.level_l.load(Ordering::Relaxed));
        let prev_r = f32::from_bits(shared.level_r.load(Ordering::Relaxed));
        shared
            .level_l
            .store(peak_l.max(prev_l * 0.8).min(1.0).to_bits(), Ordering::Relaxed);
        shared
            .level_r
            .store(peak_r.max(prev_r * 0.8).min(1.0).to_bits(), Ordering::Relaxed);
    }
    capture.stop();
    Ok(())
}

fn apply_gain(sample: i16, gain: f32) -> i16 {
    if (gain - 1.0).abs() < f32::EPSILON {
        return sample;
    }
    let scaled = sample as f32 * gain;
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BtSplitConfig {
        BtSplitConfig {
            left_endpoint: "Xiaomi Sound-1695".into(),
            right_endpoint: "Xiaomi Sound-5320".into(),
            ..Default::default()
        }
    }

    #[test]
    fn default_config_is_valid() {
        let c = cfg();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_empty_and_same_endpoints() {
        let mut c = cfg();
        c.left_endpoint = "  ".into();
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.right_endpoint = c.left_endpoint.clone();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_out_of_range_numbers() {
        let mut c = cfg();
        c.capture_gain = 9.0;
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.render_buffer_ms = 5;
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.queue_ms = 501;
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.left_delay_ms = 1_500;
        assert!(c.validate().is_err());
    }

    #[test]
    fn software_latency_adds_up() {
        let c = cfg();
        assert_eq!(c.software_latency_ms(), 20 + 40 + 20);
    }

    #[test]
    fn gain_is_clamped_and_identity_preserves() {
        assert_eq!(apply_gain(1_000, 1.0), 1_000);
        assert_eq!(apply_gain(i16::MAX, 8.0), i16::MAX);
        assert_eq!(apply_gain(i16::MIN, 8.0), i16::MIN);
        assert_eq!(apply_gain(100, 0.0), 0);
    }

    #[test]
    fn queue_drops_oldest_when_full() {
        let mut sink = SinkShared::new("x", 4);
        sink.push([1, 2, 3].into_iter());
        assert_eq!(sink.queue.len(), 3);
        sink.push([4, 5, 6].into_iter());
        assert_eq!(sink.queue.len(), 4);
        assert_eq!(sink.dropped_samples, 2);
        assert_eq!(sink.queue.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5, 6]);
    }
}
