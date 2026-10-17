//! 任意多台本机播放设备的声道路由引擎（仅 Windows）。
//!
//! 把虚拟声卡的回环采下来，按**每条路由**的配置（左 / 右 / 双声道混音）分别推给各自的
//! 输出端点。相对 [`crate::btsplit`]（固定两路、左右各一台）的差别：
//!
//! * 路由条数任意：`routes` 里每台设备一条独立的 render 线程 + 抖动队列，互不阻塞；
//! * 每条路由可选 [`RouteChannel::Left`] / [`RouteChannel::Right`] / [`RouteChannel::Both`]
//!   （`Both` = 采集左右混音 `(L+R)/2`，输出的两个声道都填这份混音）；
//! * 每条路由可以独立设增益和额外延迟（对齐两台音箱的固定时差）；
//! * **某条路由打开失败不影响其它路由**：失败只写进该路的 `error`、整体仍 running，
//!   只有全部路由都失败时 [`RouteEngineHandle::start`] 才返回错误、运行中全部失败才停引擎。
//!
//! ```text
//! Windows 播放器 → 虚拟声卡（采集源，WASAPI loopback 44.1k/2ch/s16le）
//!                    │
//!                    ├─ Left  ─► 端点 A（双声道都填 L）
//!                    ├─ Right ─► 端点 B（双声道都填 R）
//!                    └─ Both  ─► 端点 C（双声道都填 (L+R)/2）
//! ```
//!
//! 采集链路：1 条采集线程（[`AudioCapture`]，回环，20 ms WASAPI 缓冲）→ 每个采集块按
//! 路由的通道模式 + 两级增益（`capture_gain` × `RouteConfig::gain`）拆成单声道样本，推进
//! 各路的 `VecDeque<i16>`。每条路由的 sink 线程轮询 `GetCurrentPadding`，缓冲有空位就写
//! 10 ms；队列空补静音（计欠载），队列满丢最旧（计 `dropped_ms`）——两条链路的时钟互相
//! 独立，只能靠丢帧/补静音吸收漂移。
//!
//! 这是**用户态**实现，不碰内核驱动；虚拟声卡沿用已有的 Steam Streaming Speakers 改名端点。

use std::collections::VecDeque;
use std::fmt;
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

/// 路由输出的采样率（与采集、HTTP 流一致：44100 Hz / 2 ch / 16 bit）。
pub const ROUTE_SAMPLE_RATE: u32 = 44_100;
/// 每次写 render 端点的帧数（10 ms）。
const RENDER_CHUNK_FRAMES: usize = (ROUTE_SAMPLE_RATE / 100) as usize;
/// 每个 sink 在 `Start` 前预填多少毫秒静音（避免一开流就欠载）。
const RENDER_PREFILL_MS: u32 = 10;
/// 采集缓冲块大小（帧），与 sink 每次写入量一致（10 ms）。
const CAPTURE_BLOCK_FRAMES: usize = RENDER_CHUNK_FRAMES;
/// 电平条衰减系数：每次刷新只保留旧值的 80%（视觉上约 100 ms 的保持）。
const LEVEL_DECAY: f32 = 0.8;
/// 启动时等待所有路由流打开（或报错）的最长时间。
const START_TIMEOUT: Duration = Duration::from_secs(3);
/// 字节换算：44.1k / 2ch / 16 bit = 4 字节每帧。
const FRAME_BYTES: u64 = 4;

/// 一条路由从采集块里取哪个声道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RouteChannel {
    /// 只取采集块的左声道（左右分离的左路）。
    Left,
    /// 只取采集块的右声道（左右分离的右路）。
    Right,
    /// 取 `(L+R)/2` 混音，输出的两个声道都填这份混音（单只箱子也能听到完整节目）。
    #[default]
    Both,
}

impl RouteChannel {
    /// 稳定字符串形式（GUI / 日志 / 配置文件用）：`"left"` / `"right"` / `"both"`。
    pub fn as_str(self) -> &'static str {
        match self {
            RouteChannel::Left => "left",
            RouteChannel::Right => "right",
            RouteChannel::Both => "both",
        }
    }

    /// 从 `"left"` / `"right"` / `"both"`（大小写不敏感，允许首尾空白）解析。
    ///
    /// 其它输入返回 `None`（GUI 读配置文件时用）。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "left" => Some(RouteChannel::Left),
            "right" => Some(RouteChannel::Right),
            "both" => Some(RouteChannel::Both),
            _ => None,
        }
    }
}

impl fmt::Display for RouteChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条路由（一台输出设备）的配置。
#[derive(Debug, Clone, PartialEq)]
pub struct RouteConfig {
    /// 目标端点：友好名**子串**（大小写不敏感），例如 `"Xiaomi Sound-1695"`。
    pub endpoint: String,
    /// 送哪个声道。
    pub channel: RouteChannel,
    /// 这条链路的额外增益（0.0–8.0，1.0 = 原样；与全局 `capture_gain` 相乘）。
    pub gain: f32,
    /// 这条链路的额外延迟（毫秒，0–2000），启动时先输出这么多静音，用于两台箱子对齐。
    pub delay_ms: u32,
    /// `false` 时不打开这条路由（配置里可以留着，方便 GUI 暂存/临时停一路）。
    pub enabled: bool,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            channel: RouteChannel::Both,
            gain: 1.0,
            delay_ms: 0,
            enabled: true,
        }
    }
}

impl RouteConfig {
    /// 快捷构造：端点 + 声道，其余用默认值（增益 1.0、无额外延迟、启用）。
    pub fn new(endpoint: impl Into<String>, channel: RouteChannel) -> Self {
        Self {
            endpoint: endpoint.into(),
            channel,
            ..Self::default()
        }
    }

    /// 额外延迟换算成 44.1k 的帧数。
    pub fn delay_frames(&self) -> usize {
        delay_skip_frames(self.delay_ms)
    }

    /// 人读一行：`Left ← "Xiaomi Sound-1695" | 增益 1.00 | 延迟 0 ms`。
    pub fn describe(&self) -> String {
        format!(
            "{:?} ← \"{}\" | 增益 {:.2} | 延迟 {} ms{}",
            self.channel,
            self.endpoint,
            self.gain,
            self.delay_ms,
            if self.enabled { "" } else { " | 已禁用" }
        )
    }
}

/// 路由引擎的整体配置。
#[derive(Debug, Clone, PartialEq)]
pub struct RouteEngineConfig {
    /// 采集源端点：虚拟声卡的友好名**子串**；回环采集这个 **render** 端点的输出。
    pub source_endpoint: String,
    /// 路由列表（只有 `enabled = true` 的会真正打开设备）。
    pub routes: Vec<RouteConfig>,
    /// 采集增益（0.0–8.0，1.0 = 原样）。先作用在采集块上，电平条也按它算。
    pub capture_gain: f32,
    /// 每条路由 render 端的 WASAPI 共享模式缓冲（毫秒，10–500）。
    pub render_buffer_ms: u32,
    /// 每条路由预排队的音频长度（毫秒，0–500）。越大越抗抖动、延迟越高。
    pub queue_ms: u32,
}

impl Default for RouteEngineConfig {
    fn default() -> Self {
        Self {
            source_endpoint: "立体声链".to_owned(),
            routes: Vec::new(),
            capture_gain: 1.0,
            render_buffer_ms: 40,
            queue_ms: 40,
        }
    }
}

impl RouteEngineConfig {
    /// 校验配置（不打开设备）。
    ///
    /// 规则：
    /// * `routes` 非空，且至少有一条 `enabled = true`；
    /// * `source_endpoint` 非空；
    /// * `capture_gain` / 每条路由的 `gain` 在 `0.0..=8.0`；
    /// * `render_buffer_ms` 在 `10..=500`，`queue_ms` 在 `0..=500`；
    /// * 每条路由的 `delay_ms` 在 `0..=2000`；
    /// * **启用的路由之间端点不能重复**（同一端点被两条链路抢设备）；
    ///   禁用的路由只做暂存，不参与查重，`endpoint` 也可以先留空。
    pub fn validate(&self) -> Result<()> {
        if self.routes.is_empty() {
            return Err(AudioError::invalid(
                "routes 不能为空（至少配置一条输出路由）",
            ));
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

        let mut enabled = 0usize;
        let mut seen: Vec<String> = Vec::new();
        for (index, route) in self.routes.iter().enumerate() {
            if !route.enabled {
                continue;
            }
            enabled += 1;
            if route.endpoint.trim().is_empty() {
                return Err(AudioError::invalid(format!(
                    "第 {} 条路由（{:?}）的 endpoint 不能为空",
                    index + 1,
                    route.channel
                )));
            }
            if !(0.0..=8.0).contains(&route.gain) {
                return Err(AudioError::invalid(format!(
                    "第 {} 条路由 \"{}\" 的增益 {} 不在 0.0..=8.0",
                    index + 1,
                    route.endpoint,
                    route.gain
                )));
            }
            if route.delay_ms > 2_000 {
                return Err(AudioError::invalid(format!(
                    "第 {} 条路由 \"{}\" 的 delay_ms={} 超过 2000",
                    index + 1,
                    route.endpoint,
                    route.delay_ms
                )));
            }
            let key = route.endpoint.trim().to_lowercase();
            if seen.contains(&key) {
                return Err(AudioError::invalid(format!(
                    "端点 \"{}\" 被多条启用路由重复使用（同一端点会被两条链路抢设备）",
                    route.endpoint.trim()
                )));
            }
            seen.push(key);
        }
        if enabled == 0 {
            return Err(AudioError::invalid(
                "至少启用一条路由（routes 里没有 enabled=true 的项）",
            ));
        }
        Ok(())
    }

    /// 计划值：软件侧最少要排多少毫秒（不含蓝牙/USB 链路本身）。
    pub fn software_latency_ms(&self) -> u32 {
        // 采集 WASAPI 缓冲（20 ms，见 capture_loop）+ 预排队 + render 缓冲的一半（平均占用）
        20 + self.queue_ms + self.render_buffer_ms / 2
    }

    /// 人读一行：路由条数、采集源、缓冲参数、采集增益。
    pub fn describe(&self) -> String {
        let enabled = self.routes.iter().filter(|route| route.enabled).count();
        format!(
            "{} 条路由（{} 条启用）| 采集源 {} | 队列 {} ms + render {} ms | 采集增益 {:.2}",
            self.routes.len(),
            enabled,
            self.source_endpoint,
            self.queue_ms,
            self.render_buffer_ms,
            self.capture_gain
        )
    }
}

/// 单条路由的运行统计。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteStats {
    /// 路由目标端点（配置里的友好名子串）。
    pub endpoint: String,
    /// 这条路由送哪个声道。
    pub channel: RouteChannel,
    /// 已经写进 render 端缓冲的字节数（双声道 16 bit，一帧 4 字节）。
    pub sent_bytes: u64,
    /// 欠载次数：这次写缓冲时队列被抽干、只能补静音（采集已经出数据后才计）。
    pub underruns: u64,
    /// 因队列满丢弃的音频总时长（毫秒）：两条链路时钟漂移时用来观察积压是否失控。
    pub dropped_ms: f64,
    /// 当前队列里排队的音频时长（毫秒），稳态应接近配置的 `queue_ms`。
    pub queued_ms: f64,
    /// 这条链路输出的峰值电平（0..1，带衰减，GUI 电平条用）。
    pub level: f32,
    /// 打开/运行失败原因；`None` = 正常（含尚在打开中）。
    pub error: Option<String>,
}

impl RouteStats {
    /// 人读一行（GUI 日志 / 自检用）。
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{} → {} | 已发 {} B | 欠载 {} | 丢 {:.0} ms | 队列 {:.0} ms | 电平 {:.2}",
            self.channel.as_str(),
            if self.endpoint.is_empty() {
                "<未知端点>"
            } else {
                self.endpoint.as_str()
            },
            self.sent_bytes,
            self.underruns,
            self.dropped_ms,
            self.queued_ms,
            self.level
        );
        if let Some(err) = &self.error {
            text.push_str(&format!(" | 失败：{err}"));
        }
        text
    }
}

/// 路由引擎的整体快照（[`RouteEngineHandle::stats`] 返回）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteEngineStats {
    /// 引擎是否还在跑（采集线程 + 至少一条输出路由存活）。
    pub running: bool,
    /// 实际打开的回环采集设备友好名（空 = 还没打开）。
    pub source_device: String,
    /// 采集侧致命错误（设备失效等）；出现后引擎会停。
    pub source_error: Option<String>,
    /// 累计采集帧数（44.1k 帧）。
    pub captured_frames: u64,
    /// 累计采集字节数（44.1k/2ch/s16 = 4 B/帧）。
    pub frame_bytes: u64,
    /// 启动至今秒数。
    pub uptime_seconds: f64,
    /// 采集块左声道峰值电平（0..1，带衰减，GUI 电平条用）。
    pub level_l: f32,
    /// 采集块右声道峰值电平（0..1，带衰减）。
    pub level_r: f32,
    /// 每条**启用**路由的统计，顺序与配置里启用项一致（含打开失败的）。
    pub routes: Vec<RouteStats>,
}

impl RouteEngineStats {
    /// 人读一行：采集情况 + 每条路由一行（追加在同一行里）。
    pub fn describe(&self) -> String {
        let source = if self.source_device.is_empty() {
            "<采集源未打开>"
        } else {
            self.source_device.as_str()
        };
        let mut text = format!(
            "采集 {} 帧（{:.2} MB）| 源 {} | 电平 L {:.2} / R {:.2} | {} 条路由",
            self.captured_frames,
            self.frame_bytes as f64 / 1_048_576.0,
            source,
            self.level_l,
            self.level_r,
            self.routes.len()
        );
        if let Some(err) = &self.source_error {
            text.push_str(&format!(" | 采集故障：{err}"));
        }
        for route in &self.routes {
            text.push_str(" | ");
            text.push_str(&route.describe());
        }
        text
    }
}

/// 运行中的路由引擎句柄。`Drop` 时自动停止所有线程。
pub struct RouteEngineHandle {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    config: RouteEngineConfig,
}

impl RouteEngineHandle {
    /// 启动路由引擎：每条启用路由一条 sink 线程 + 1 条采集线程。
    ///
    /// 返回前最多等 [`START_TIMEOUT`]（3 秒）让各条路由的 render 流打开：
    /// * 单条（或部分）路由打开失败 → 只记进该路 `error`，其它路继续，整体仍 `running`；
    /// * **全部**路由都打开失败 → 清理线程并返回错误（错误里带每路的失败原因）；
    /// * 超时仍有路由没打开 → 先返回，没打开的路由在后台继续尝试，成功后自动补上。
    ///
    /// 采集线程在 render 流就绪后才启动，避免一开始就丢数据。
    pub fn start(config: RouteEngineConfig) -> Result<Self> {
        config.validate()?;
        let enabled: Vec<RouteConfig> = config
            .routes
            .iter()
            .filter(|route| route.enabled)
            .cloned()
            .collect();
        // validate() 保证至少一条启用。
        let sink_count = enabled.len();
        let queue_cap_samples = queue_capacity_samples(config.queue_ms);
        let sinks: Vec<Mutex<SinkState>> = enabled
            .iter()
            .map(|route| Mutex::new(SinkState::new(route, queue_cap_samples)))
            .collect();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            sinks_alive: AtomicU32::new(sink_count as u32),
            captured_frames: AtomicU64::new(0),
            level_l: AtomicU32::new(0),
            level_r: AtomicU32::new(0),
            started: Instant::now(),
            source_device: Mutex::new(String::new()),
            source_error: Mutex::new(None),
            sinks,
        });

        let mut threads: Vec<JoinHandle<()>> = Vec::with_capacity(sink_count + 1);
        for (index, route) in enabled.iter().enumerate() {
            let thread_shared = shared.clone();
            let thread_route = route.clone();
            let render_buffer_ms = config.render_buffer_ms;
            match thread::Builder::new()
                .name(format!("stereo-link-route-sink-{index}"))
                .spawn(move || run_sink(thread_shared, index, thread_route, render_buffer_ms))
            {
                Ok(handle) => threads.push(handle),
                Err(e) => {
                    return Err(spawn_failure(
                        &shared,
                        &mut threads,
                        format!("spawn 路由 sink 线程失败：{e}"),
                    ))
                }
            }
        }

        // 等各条路由的 render 流打开或报错，最多 3 秒（超时则先返回，后台继续尝试）。
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if count_pending(&shared) == 0 || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        if count_failed(&shared) >= sink_count {
            let reasons = failure_reasons(&shared);
            shared.stop.store(true, Ordering::Relaxed);
            for handle in threads.drain(..) {
                let _ = handle.join();
            }
            return Err(AudioError::invalid(format!(
                "所有 {sink_count} 条路由都没能打开：{}",
                reasons.join("；")
            )));
        }

        // 各路 render 就绪后再开采集（避免一开始就丢数据）。
        let capture_shared = shared.clone();
        let capture_config = config.clone();
        match thread::Builder::new()
            .name("stereo-link-route-capture".to_string())
            .spawn(move || run_capture(capture_shared, capture_config))
        {
            Ok(handle) => threads.push(handle),
            Err(e) => {
                return Err(spawn_failure(
                    &shared,
                    &mut threads,
                    format!("spawn 采集线程失败：{e}"),
                ))
            }
        }

        Ok(Self {
            shared,
            threads,
            config,
        })
    }

    /// 当前配置。
    pub fn config(&self) -> &RouteEngineConfig {
        &self.config
    }

    /// 是否还在跑：没被停止，且至少一条输出路由的线程还活着。
    pub fn alive(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed)
            && self.shared.sinks_alive.load(Ordering::Relaxed) > 0
    }

    /// 拍一张统计快照（加锁读，不影响音频线程）。
    pub fn stats(&self) -> RouteEngineStats {
        let source_device = self
            .shared
            .source_device
            .lock()
            .map(|name| name.clone())
            .unwrap_or_default();
        let source_error = self
            .shared
            .source_error
            .lock()
            .ok()
            .and_then(|error| error.clone());
        let captured_frames = self.shared.captured_frames.load(Ordering::Relaxed);
        let mut routes = Vec::with_capacity(self.shared.sinks.len());
        for slot in &self.shared.sinks {
            match slot.lock() {
                Ok(sink) => routes.push(RouteStats {
                    endpoint: sink.endpoint.clone(),
                    channel: sink.channel,
                    sent_bytes: sink.sent_bytes,
                    underruns: sink.underruns,
                    dropped_ms: samples_to_ms(sink.dropped_samples),
                    queued_ms: samples_to_ms(sink.queue.len() as u64),
                    level: sink.level,
                    error: sink.error.clone(),
                }),
                // 锁被 poison 只可能是音频线程 panic（正常情况下不会），给个占位不拖垮快照。
                Err(_) => routes.push(RouteStats::default()),
            }
        }
        RouteEngineStats {
            running: self.alive(),
            source_device,
            source_error,
            captured_frames,
            frame_bytes: captured_frames * FRAME_BYTES,
            uptime_seconds: self.shared.started.elapsed().as_secs_f64(),
            level_l: f32::from_bits(self.shared.level_l.load(Ordering::Relaxed)),
            level_r: f32::from_bits(self.shared.level_r.load(Ordering::Relaxed)),
            routes,
        }
    }

    /// 停止（幂等；再调一次 / `Drop` 都安全）。
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for RouteEngineHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

/// 一条路由 sink 的共享状态：采集线程写队列，sink 线程读队列，`stats()` 读计数。
struct SinkState {
    endpoint: String,
    channel: RouteChannel,
    gain: f32,
    /// 是否已经成功打开 render 流（start() 的等待条件之一）。
    opened: bool,
    /// 打开/运行失败原因；`Some` = 这条路由已判失败（sink 线程随后退出）。
    error: Option<String>,
    queue: VecDeque<i16>,
    queue_cap_samples: usize,
    sent_bytes: u64,
    underruns: u64,
    dropped_samples: u64,
    /// 输出峰值电平（0..1，带衰减）。
    level: f32,
}

impl SinkState {
    fn new(route: &RouteConfig, queue_cap_samples: usize) -> Self {
        let cap = queue_cap_samples.max(1);
        Self {
            endpoint: route.endpoint.clone(),
            channel: route.channel,
            gain: route.gain,
            opened: false,
            error: None,
            queue: VecDeque::with_capacity(cap),
            queue_cap_samples: cap,
            sent_bytes: 0,
            underruns: 0,
            dropped_samples: 0,
            level: 0.0,
        }
    }

    /// 推一个单声道采样：队列满时丢最旧的（两条链路时钟独立，不能让延迟无限涨）。
    fn push_sample(&mut self, sample: i16) {
        if self.queue.len() >= self.queue_cap_samples {
            self.queue.pop_front();
            self.dropped_samples += 1;
        }
        self.queue.push_back(sample);
    }
}

/// 引擎线程共享的运行时状态。
struct Shared {
    stop: AtomicBool,
    /// 还活着的 sink 线程数；掉到 0 表示所有输出都没了 → 停引擎。
    sinks_alive: AtomicU32,
    captured_frames: AtomicU64,
    /// 最近一段采集的左/右峰值（f32::to_bits），界面电平条用。
    level_l: AtomicU32,
    level_r: AtomicU32,
    started: Instant,
    source_device: Mutex<String>,
    source_error: Mutex<Option<String>>,
    /// 每条启用路由一份；下标与启用顺序一致。
    sinks: Vec<Mutex<SinkState>>,
}

/// 一块采集 PCM 解码后的结果。
struct DecodedBlock {
    left: Vec<i16>,
    right: Vec<i16>,
    peak_l: f32,
    peak_r: f32,
}

/// 把一块 `44100/2ch/s16le` 交错 PCM 解码成左右声道并应用采集增益，顺带算峰值。
///
/// 末尾不足一帧（4 字节）的残字节会被忽略。
fn decode_capture_block(pcm: &[u8], capture_gain: f32) -> DecodedBlock {
    let frames = pcm.len() / FRAME_BYTES as usize;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    let (mut peak_l, mut peak_r) = (0.0f32, 0.0f32);
    for frame in pcm.chunks_exact(FRAME_BYTES as usize) {
        let l = apply_gain(i16::from_le_bytes([frame[0], frame[1]]), capture_gain);
        let r = apply_gain(i16::from_le_bytes([frame[2], frame[3]]), capture_gain);
        peak_l = peak_l.max((l as f32 / 32_768.0).abs());
        peak_r = peak_r.max((r as f32 / 32_768.0).abs());
        left.push(l);
        right.push(r);
    }
    DecodedBlock {
        left,
        right,
        peak_l,
        peak_r,
    }
}

/// 按通道模式从采集的 L/R 取该路由的单声道样本（`Both` = 整数平均，不会溢出）。
fn mix_sample(channel: RouteChannel, left: i16, right: i16) -> i16 {
    match channel {
        RouteChannel::Left => left,
        RouteChannel::Right => right,
        // 先转 i32 再平均：i16::MAX + i16::MAX 也不会溢出，负半轴同理。
        RouteChannel::Both => ((left as i32 + right as i32) / 2) as i16,
    }
}

/// 对 i16 采样应用增益并钳制回 i16 范围（1.0 = 原样快路径）。
fn apply_gain(sample: i16, gain: f32) -> i16 {
    if (gain - 1.0).abs() < f32::EPSILON {
        return sample;
    }
    let scaled = sample as f32 * gain;
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// `delay_ms` 换算成 44.1k 下要跳过的帧数。
fn delay_skip_frames(delay_ms: u32) -> usize {
    (delay_ms as u64 * ROUTE_SAMPLE_RATE as u64 / 1000) as usize
}

/// `queue_ms` 换算成每条路由的队列容量（单声道采样数，至少 1）。
fn queue_capacity_samples(queue_ms: u32) -> usize {
    (ROUTE_SAMPLE_RATE as u64 * queue_ms as u64 / 1000).max(1) as usize
}

/// 采样数换算成毫秒。
fn samples_to_ms(samples: u64) -> f64 {
    samples as f64 * 1000.0 / ROUTE_SAMPLE_RATE as f64
}

/// 启动阶段：还有多少条路由既没打开也没失败。
fn count_pending(shared: &Shared) -> usize {
    shared
        .sinks
        .iter()
        .filter(|slot| match slot.lock() {
            Ok(sink) => !sink.opened && sink.error.is_none(),
            // 锁坏了当作已结束，别让 start() 白等 3 秒。
            Err(_) => false,
        })
        .count()
}

/// 已判失败的路由条数。
fn count_failed(shared: &Shared) -> usize {
    shared
        .sinks
        .iter()
        .filter(|slot| match slot.lock() {
            Ok(sink) => sink.error.is_some(),
            Err(_) => true,
        })
        .count()
}

/// 收集各条失败路由的原因，拼进 start() 的错误信息。
fn failure_reasons(shared: &Shared) -> Vec<String> {
    shared
        .sinks
        .iter()
        .filter_map(|slot| {
            let sink = slot.lock().ok()?;
            let error = sink.error.as_ref()?;
            let endpoint = if sink.endpoint.is_empty() {
                "<空端点>"
            } else {
                sink.endpoint.as_str()
            };
            Some(format!("{endpoint}: {error}"))
        })
        .collect()
}

/// 线程创建失败时的统一清理：置停止、join 已起的线程、返回错误。
fn spawn_failure(
    shared: &Arc<Shared>,
    threads: &mut Vec<JoinHandle<()>>,
    message: String,
) -> AudioError {
    shared.stop.store(true, Ordering::Relaxed);
    for handle in threads.drain(..) {
        let _ = handle.join();
    }
    AudioError::invalid(message)
}

// ---------------------------------------------------------------------------
// sink 线程：一条路由一个，独立打开、独立写、独立失败
// ---------------------------------------------------------------------------

fn run_sink(shared: Arc<Shared>, index: usize, route: RouteConfig, render_buffer_ms: u32) {
    if let Err(e) = sink_loop(&shared, index, &route, render_buffer_ms) {
        if let Ok(mut sink) = shared.sinks[index].lock() {
            if sink.error.is_none() {
                sink.error = Some(e.to_string());
            }
        }
    }
    // 全部输出路都没了：没有继续采集的意义。
    if shared.sinks_alive.fetch_sub(1, Ordering::Relaxed) == 1 {
        shared.stop.store(true, Ordering::Relaxed);
    }
}

fn sink_loop(
    shared: &Arc<Shared>,
    index: usize,
    route: &RouteConfig,
    render_buffer_ms: u32,
) -> Result<()> {
    let _com = ComGuard::new()?;
    let enumerator = com::enumerator()?;
    let selector = DeviceSelector::name_contains(route.endpoint.clone());
    let device = com::resolve(&enumerator, &selector, DataFlow::Render)?;
    let client = com::activate_client(&device.device)?;
    let _mix = com::mix_format(&client)?;

    let format = PcmFormat::new(ROUTE_SAMPLE_RATE, 2, 16);
    let wave_format = com::pcm16_waveformatex(format);
    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            com::ms_to_hns(render_buffer_ms),
            0,
            &wave_format,
            None,
        )
    }
    .map_err(|e| AudioError::wasapi(e.code().0, "Initialize(route render)"))?;

    let buffer_frames = unsafe { client.GetBufferSize() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBufferSize(route)"))? as usize;
    let render: IAudioRenderClient = unsafe { client.GetService() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetService(IAudioRenderClient, route)"))?;

    // 预填一点静音，避免 Start 后第一轮就欠载。
    let prefill = (RENDER_PREFILL_MS as usize * ROUTE_SAMPLE_RATE as usize / 1000)
        .min(buffer_frames);
    if prefill > 0 {
        write_silence(&render, prefill)?;
    }
    unsafe { client.Start() }.map_err(|e| AudioError::wasapi(e.code().0, "Start(route)"))?;

    // 从这里开始 start() 认为这条路由"已打开"。
    if let Ok(mut sink) = shared.sinks[index].lock() {
        sink.opened = true;
    }

    // 额外延迟：启动时先写 N 帧静音（采集数据照常进队列，填满后按上限丢最旧）。
    let mut skip_frames = route.delay_frames();
    let result = (|| -> Result<()> {
        while !shared.stop.load(Ordering::Relaxed) {
            let padding = unsafe { client.GetCurrentPadding() }
                .map_err(|e| AudioError::wasapi(e.code().0, "GetCurrentPadding(route)"))?
                as usize;
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
            let mut peak = 0.0f32;
            {
                let mut sink = shared.sinks[index]
                    .lock()
                    .map_err(|_| AudioError::invalid("route sink 状态锁被 poison"))?;
                while got < want {
                    match sink.queue.pop_front() {
                        Some(sample) => {
                            peak = peak.max((sample as f32 / 32_768.0).abs());
                            // 单声道样本同时填 L/R：只接一只箱子也不会少一半节目。
                            stereo.push(sample);
                            stereo.push(sample);
                            got += 1;
                        }
                        None => {
                            queue_empty = true;
                            break;
                        }
                    }
                }
                sink.sent_bytes += (got * FRAME_BYTES as usize) as u64;
                // 输出电平：本块峰值与旧值取大（旧值每块衰减 20%）。
                sink.level = peak.max(sink.level * LEVEL_DECAY).min(1.0);
                if queue_empty
                    && got == 0
                    && shared.captured_frames.load(Ordering::Relaxed) > 0
                {
                    // 采集已经在出数据、这一路却一个样本都没有：补静音并计一次欠载。
                    sink.underruns += 1;
                }
            }

            if got == 0 {
                write_silence(&render, want)?;
                continue;
            }
            write_stereo(&render, &stereo)?;
            if got < want {
                // 队列暂时不够：剩下的补静音，别让设备整块欠载。
                write_silence(&render, want - got)?;
            }
        }
        Ok(())
    })();

    unsafe { client.Stop() }.map_err(|e| AudioError::wasapi(e.code().0, "Stop(route)"))?;
    result
}

/// 往 render 缓冲写 `frames` 帧静音（双声道 16 bit）。
fn write_silence(render: &IAudioRenderClient, frames: usize) -> Result<()> {
    if frames == 0 {
        return Ok(());
    }
    let ptr = unsafe { render.GetBuffer(frames as u32) }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBuffer(silence)"))?;
    if ptr.is_null() {
        return Err(AudioError::invalid(
            "IAudioRenderClient::GetBuffer 返回空指针",
        ));
    }
    unsafe { std::ptr::write_bytes(ptr, 0, frames * FRAME_BYTES as usize) };
    unsafe { render.ReleaseBuffer(frames as u32, 0) }
        .map_err(|e| AudioError::wasapi(e.code().0, "ReleaseBuffer(silence)"))?;
    Ok(())
}

/// 往 render 缓冲写交错立体声（`samples.len()` 必须是偶数）。
fn write_stereo(render: &IAudioRenderClient, samples: &[i16]) -> Result<()> {
    let frames = samples.len() / 2;
    if frames == 0 {
        return Ok(());
    }
    let ptr = unsafe { render.GetBuffer(frames as u32) }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBuffer(stereo)"))?;
    if ptr.is_null() {
        return Err(AudioError::invalid(
            "IAudioRenderClient::GetBuffer 返回空指针",
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(samples.as_ptr() as *const u8, ptr, samples.len() * 2);
    }
    unsafe { render.ReleaseBuffer(frames as u32, 0) }
        .map_err(|e| AudioError::wasapi(e.code().0, "ReleaseBuffer(stereo)"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 采集线程：只开一条，把每个采集块拆进所有路由的队列
// ---------------------------------------------------------------------------

fn run_capture(shared: Arc<Shared>, config: RouteEngineConfig) {
    if let Err(e) = capture_loop(&shared, &config) {
        if let Ok(mut slot) = shared.source_error.lock() {
            *slot = Some(e.to_string());
        }
        shared.stop.store(true, Ordering::Relaxed);
    }
}

fn capture_loop(shared: &Arc<Shared>, config: &RouteEngineConfig) -> Result<()> {
    let mut capture_config = CaptureConfig::default();
    capture_config.device = DeviceSelector::name_contains(config.source_endpoint.clone());
    capture_config.mode = CaptureMode::Loopback;
    capture_config.samplerate = ROUTE_SAMPLE_RATE;
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

    let frame_bytes = PcmFormat::new(ROUTE_SAMPLE_RATE, 2, 16).frame_bytes();
    let mut buffer = vec![0u8; CAPTURE_BLOCK_FRAMES * frame_bytes];
    while !shared.stop.load(Ordering::Relaxed) {
        let n = match capture.read(&mut buffer, Duration::from_millis(200)) {
            Ok(n) => n,
            Err(e) => {
                // 采集线程正常结束（stop 已经置位）时也可能读到错误，只有还在跑才当故障。
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                return Err(e);
            }
        };
        if n == 0 {
            continue;
        }
        let block = decode_capture_block(&buffer[..n], config.capture_gain);
        let frames = block.left.len();
        shared
            .captured_frames
            .fetch_add(frames as u64, Ordering::Relaxed);

        // 电平条：本块峰值与旧值取大（旧值每块衰减 20%，约 100 ms 保持）。
        let prev_l = f32::from_bits(shared.level_l.load(Ordering::Relaxed));
        let prev_r = f32::from_bits(shared.level_r.load(Ordering::Relaxed));
        shared.level_l.store(
            block.peak_l.max(prev_l * LEVEL_DECAY).min(1.0).to_bits(),
            Ordering::Relaxed,
        );
        shared.level_r.store(
            block.peak_r.max(prev_r * LEVEL_DECAY).min(1.0).to_bits(),
            Ordering::Relaxed,
        );

        // 按每条路由的通道模式 + 增益，把这一块拆进各自的队列。
        for slot in &shared.sinks {
            let Ok(mut sink) = slot.lock() else {
                continue;
            };
            if sink.error.is_some() {
                // 这条路已经挂了，不再喂数据。
                continue;
            }
            let channel = sink.channel;
            let gain = sink.gain;
            for index in 0..frames {
                let mono = mix_sample(channel, block.left[index], block.right[index]);
                sink.push_sample(apply_gain(mono, gain));
            }
        }
    }
    capture.stop();
    Ok(())
}

// ---------------------------------------------------------------------------
// 单元测试：只测纯逻辑，不打开任何真实设备
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn route(endpoint: &str, channel: RouteChannel) -> RouteConfig {
        RouteConfig::new(endpoint, channel)
    }

    /// 两条路由（左 / 右）的合法配置。
    fn config() -> RouteEngineConfig {
        RouteEngineConfig {
            source_endpoint: "立体声链".to_owned(),
            routes: vec![
                route("Xiaomi Sound-1695", RouteChannel::Left),
                route("Xiaomi Sound-5320", RouteChannel::Right),
            ],
            ..Default::default()
        }
    }

    /// 把 `(L, R)` 帧序列编码成 44.1k/2ch/s16le 交错 PCM。
    fn pcm16_stereo(frames: &[(i16, i16)]) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames.len() * 4);
        for (l, r) in frames {
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
        }
        out
    }

    #[test]
    fn default_engine_config_is_incomplete_but_defaults_are_sane() {
        let default = RouteEngineConfig::default();
        assert!(default.routes.is_empty());
        // 空 routes 不合法：GUI 必须先配至少一条路由。
        assert!(default.validate().is_err());
        assert_eq!(default.source_endpoint, "立体声链");
        assert_eq!(default.capture_gain, 1.0);
        assert_eq!(default.render_buffer_ms, 40);
        assert_eq!(default.queue_ms, 40);
        let route = RouteConfig::default();
        assert_eq!(route.channel, RouteChannel::Both);
        assert_eq!(route.gain, 1.0);
        assert_eq!(route.delay_ms, 0);
        assert!(route.enabled);
    }

    #[test]
    fn channel_strings_round_trip() {
        assert_eq!(RouteChannel::Left.as_str(), "left");
        assert_eq!(RouteChannel::Right.as_str(), "right");
        assert_eq!(RouteChannel::Both.as_str(), "both");
        assert_eq!(format!("{}", RouteChannel::Both), "both");
        assert_eq!(RouteChannel::parse(" LEFT "), Some(RouteChannel::Left));
        assert_eq!(RouteChannel::parse("both"), Some(RouteChannel::Both));
        assert_eq!(RouteChannel::parse("right"), Some(RouteChannel::Right));
        assert_eq!(RouteChannel::parse("混音"), None);
        assert_eq!(RouteChannel::default(), RouteChannel::Both);
    }

    #[test]
    fn mix_picks_channels_and_averages_both() {
        assert_eq!(mix_sample(RouteChannel::Left, 1_000, -2_000), 1_000);
        assert_eq!(mix_sample(RouteChannel::Right, 1_000, -2_000), -2_000);
        // (1000 + -2000) / 2 = -500
        assert_eq!(mix_sample(RouteChannel::Both, 1_000, -2_000), -500);
        // 整数平均向零取整：(1+2)/2 = 1
        assert_eq!(mix_sample(RouteChannel::Both, 1, 2), 1);
        // 反相抵消
        assert_eq!(mix_sample(RouteChannel::Both, 5, -5), 0);
        // 极端值不溢出
        assert_eq!(
            mix_sample(RouteChannel::Both, i16::MAX, i16::MAX),
            i16::MAX
        );
        assert_eq!(
            mix_sample(RouteChannel::Both, i16::MIN, i16::MIN),
            i16::MIN
        );
    }

    #[test]
    fn gain_is_clamped_and_identity_preserved() {
        assert_eq!(apply_gain(1_000, 1.0), 1_000);
        assert_eq!(apply_gain(1_000, 1.5), 1_500);
        assert_eq!(apply_gain(0, 8.0), 0);
        assert_eq!(apply_gain(i16::MAX, 8.0), i16::MAX);
        assert_eq!(apply_gain(i16::MIN, 8.0), i16::MIN);
        assert_eq!(apply_gain(100, 0.0), 0);
    }

    #[test]
    fn decode_block_applies_capture_gain_and_reports_peaks() {
        let pcm = pcm16_stereo(&[(1_000, -2_000), (-32_768, 32_767)]);
        let block = decode_capture_block(&pcm, 1.0);
        assert_eq!(block.left, vec![1_000, -32_768]);
        assert_eq!(block.right, vec![-2_000, 32_767]);
        assert!((block.peak_l - 1.0).abs() < 1e-6);
        assert!((block.peak_r - 32_767.0 / 32_768.0).abs() < 1e-5);
    }

    #[test]
    fn decode_block_scales_with_capture_gain() {
        let pcm = pcm16_stereo(&[(1_000, -1_000)]);
        let block = decode_capture_block(&pcm, 2.0);
        assert_eq!(block.left, vec![2_000]);
        assert_eq!(block.right, vec![-2_000]);
        // 增益过大钳制到 i16 边界
        let block = decode_capture_block(&pcm16_stereo(&[(i16::MAX, i16::MIN)]), 8.0);
        assert_eq!(block.left, vec![i16::MAX]);
        assert_eq!(block.right, vec![i16::MIN]);
    }

    #[test]
    fn decode_block_ignores_trailing_partial_frame() {
        let mut pcm = pcm16_stereo(&[(100, 200)]);
        pcm.extend_from_slice(&[9, 9]); // 不完整的半帧
        let block = decode_capture_block(&pcm, 1.0);
        assert_eq!(block.left, vec![100]);
        assert_eq!(block.right, vec![200]);
    }

    #[test]
    fn delay_milliseconds_convert_to_frames() {
        assert_eq!(delay_skip_frames(0), 0);
        assert_eq!(delay_skip_frames(1), 44);
        assert_eq!(delay_skip_frames(10), 441);
        assert_eq!(delay_skip_frames(1_000), 44_100);
        assert_eq!(delay_skip_frames(2_000), 88_200);
        // 路由上的便捷方法走同一套换算
        let mut r = route("箱A", RouteChannel::Left);
        r.delay_ms = 500;
        assert_eq!(r.delay_frames(), 22_050);
    }

    #[test]
    fn queue_capacity_scales_with_milliseconds() {
        assert_eq!(queue_capacity_samples(0), 1);
        assert_eq!(queue_capacity_samples(10), 441);
        assert_eq!(queue_capacity_samples(40), 1_764);
        assert_eq!(queue_capacity_samples(500), 22_050);
    }

    #[test]
    fn sink_queue_drops_oldest_when_full() {
        let mut sink = SinkState::new(&route("箱A", RouteChannel::Left), 4);
        for sample in [1, 2, 3] {
            sink.push_sample(sample);
        }
        assert_eq!(sink.queue.len(), 3);
        for sample in [4, 5, 6] {
            sink.push_sample(sample);
        }
        assert_eq!(sink.queue.len(), 4);
        assert_eq!(sink.dropped_samples, 2);
        assert_eq!(
            sink.queue.iter().copied().collect::<Vec<_>>(),
            vec![3, 4, 5, 6]
        );
    }

    #[test]
    fn samples_convert_to_milliseconds() {
        assert_eq!(samples_to_ms(0), 0.0);
        assert_eq!(samples_to_ms(44_100), 1_000.0);
        assert!((samples_to_ms(441) - 10.0).abs() < 1e-9);
        // 40 ms 队列 = 1764 个单声道采样
        assert!((samples_to_ms(1_764) - 40.0).abs() < 1e-9);
    }

    #[test]
    fn valid_config_passes_validation() {
        assert!(config().validate().is_ok());
        // 上界附近的合法值
        let mut ok = config();
        ok.routes[0].gain = 8.0;
        ok.routes[0].delay_ms = 2_000;
        ok.capture_gain = 0.0;
        ok.render_buffer_ms = 500;
        ok.queue_ms = 500;
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn validation_rejects_empty_route_list() {
        let mut cfg = config();
        cfg.routes.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validation_rejects_empty_source_endpoint() {
        let mut cfg = config();
        cfg.source_endpoint = "   ".to_owned();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validation_rejects_empty_enabled_endpoint_but_allows_disabled_draft() {
        let mut cfg = config();
        cfg.routes[0].endpoint = "  ".to_owned();
        assert!(cfg.validate().is_err());

        // 禁用的路由只是 GUI 暂存，endpoint 空着/重复都不上线，不参与查重。
        let mut draft = config();
        draft.routes[1].enabled = false;
        draft.routes[1].endpoint = "  ".to_owned();
        assert!(draft.validate().is_ok());
    }

    #[test]
    fn validation_rejects_duplicate_enabled_endpoints_case_insensitively() {
        let mut cfg = config();
        cfg.routes[1].endpoint = "xiaomi sound-1695".to_owned();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("重复"), "{err}");

        // 大小写、首尾空白不同的同一个端点也算重复
        let mut cfg = config();
        cfg.routes[1].endpoint = "  XIAOMI SOUND-1695  ".to_owned();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validation_rejects_all_disabled_routes() {
        let mut cfg = config();
        for route in &mut cfg.routes {
            route.enabled = false;
        }
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("至少启用"), "{err}");
    }

    #[test]
    fn validation_rejects_out_of_range_gain() {
        let mut low = config();
        low.routes[0].gain = -0.1;
        assert!(low.validate().is_err());
        let mut high = config();
        high.routes[0].gain = 8.1;
        assert!(high.validate().is_err());
        let mut capture = config();
        capture.capture_gain = 9.0;
        assert!(capture.validate().is_err());
    }

    #[test]
    fn validation_rejects_out_of_range_delay() {
        let mut cfg = config();
        cfg.routes[0].delay_ms = 2_001;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validation_rejects_out_of_range_timings() {
        let mut small_buffer = config();
        small_buffer.render_buffer_ms = 5;
        assert!(small_buffer.validate().is_err());
        let mut big_buffer = config();
        big_buffer.render_buffer_ms = 501;
        assert!(big_buffer.validate().is_err());
        let mut big_queue = config();
        big_queue.queue_ms = 501;
        assert!(big_queue.validate().is_err());
    }

    #[test]
    fn software_latency_adds_up() {
        // 采集缓冲 20 + 预排队 40 + render 缓冲一半 20
        assert_eq!(config().software_latency_ms(), 20 + 40 + 20);
    }

    #[test]
    fn config_describe_mentions_every_route() {
        let describe = config().describe();
        assert!(describe.contains("2 条路由（2 条启用）"), "{describe}");
        assert!(describe.contains("立体声链"), "{describe}");
        let route = route("Xiaomi Sound-1695", RouteChannel::Left).describe();
        assert!(route.contains("Xiaomi Sound-1695"), "{route}");
        assert!(route.contains("Left"), "{route}");
        assert!(route.contains("延迟 0 ms"), "{route}");
    }

    #[test]
    fn disabled_route_describe_marks_it() {
        let mut route = route("箱A", RouteChannel::Right);
        route.enabled = false;
        assert!(route.describe().contains("已禁用"), "{}", route.describe());
    }

    #[test]
    fn stats_describe_lists_routes_and_errors() {
        let stats = RouteEngineStats {
            running: true,
            source_device: "立体声链".to_owned(),
            source_error: None,
            captured_frames: 44_100,
            frame_bytes: 176_400,
            uptime_seconds: 1.0,
            level_l: 0.5,
            level_r: 0.25,
            routes: vec![
                RouteStats {
                    endpoint: "箱A".to_owned(),
                    channel: RouteChannel::Left,
                    sent_bytes: 176_400,
                    underruns: 0,
                    dropped_ms: 0.0,
                    queued_ms: 10.0,
                    level: 0.5,
                    error: None,
                },
                RouteStats {
                    endpoint: "箱B".to_owned(),
                    channel: RouteChannel::Right,
                    sent_bytes: 0,
                    underruns: 2,
                    dropped_ms: 1.0,
                    queued_ms: 0.0,
                    level: 0.0,
                    error: Some("设备已拔出".to_owned()),
                },
            ],
        };
        let describe = stats.describe();
        assert!(describe.contains("箱A"), "{describe}");
        assert!(describe.contains("箱B"), "{describe}");
        assert!(describe.contains("设备已拔出"), "{describe}");
        assert!(describe.contains("44100 帧"), "{describe}");
    }

    #[test]
    fn stats_reports_queue_and_drop_milliseconds() {
        let mut state = SinkState::new(&route("箱A", RouteChannel::Left), 4);
        for sample in [1, 2, 3, 4, 5] {
            state.push_sample(sample); // 容量 4，推 5 个 → 丢 1 个
        }
        state.sent_bytes = 8;
        state.underruns = 1;
        state.level = 0.25;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            sinks_alive: AtomicU32::new(1),
            captured_frames: AtomicU64::new(0),
            level_l: AtomicU32::new(0.5f32.to_bits()),
            level_r: AtomicU32::new(0.25f32.to_bits()),
            started: Instant::now(),
            source_device: Mutex::new("立体声链".to_owned()),
            source_error: Mutex::new(None),
            sinks: vec![Mutex::new(state)],
        });
        let handle = RouteEngineHandle {
            shared,
            threads: Vec::new(),
            config: RouteEngineConfig {
                routes: vec![route("箱A", RouteChannel::Left)],
                ..Default::default()
            },
        };
        let stats = handle.stats();
        assert!(stats.running);
        assert_eq!(stats.source_device, "立体声链");
        assert_eq!(stats.routes.len(), 1);
        assert_eq!(stats.routes[0].endpoint, "箱A");
        assert_eq!(stats.routes[0].channel, RouteChannel::Left);
        assert_eq!(stats.routes[0].sent_bytes, 8);
        assert_eq!(stats.routes[0].underruns, 1);
        assert_eq!(stats.routes[0].queued_ms, samples_to_ms(4));
        assert_eq!(stats.routes[0].dropped_ms, samples_to_ms(1));
        assert!((stats.routes[0].level - 0.25).abs() < f32::EPSILON);
        assert!((stats.level_l - 0.5).abs() < f32::EPSILON);
        assert!((stats.level_r - 0.25).abs() < f32::EPSILON);
        assert_eq!(stats.uptime_seconds >= 0.0, true);
    }

    #[test]
    fn stats_marks_dead_engine_when_all_sinks_gone() {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            sinks_alive: AtomicU32::new(0),
            captured_frames: AtomicU64::new(0),
            level_l: AtomicU32::new(0),
            level_r: AtomicU32::new(0),
            started: Instant::now(),
            source_device: Mutex::new(String::new()),
            source_error: Mutex::new(None),
            sinks: vec![Mutex::new(SinkState::new(
                &route("箱A", RouteChannel::Left),
                4,
            ))],
        });
        let handle = RouteEngineHandle {
            shared,
            threads: Vec::new(),
            config: RouteEngineConfig::default(),
        };
        let stats = handle.stats();
        assert!(!stats.running);
        assert_eq!(stats.captured_frames, 0);
        assert_eq!(stats.frame_bytes, 0);
    }
}
