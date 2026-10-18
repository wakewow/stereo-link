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
//! Windows 播放器 → 采集源端点（WASAPI loopback，按端点原生混音格式开流）
//!                    │
//!                    │  Rust 侧解码混音格式 → f32 → i16（必要时重采样到引擎采样率）
//!                    ├─ Left  ─► 端点 A（双声道都填 L）
//!                    ├─ Right ─► 端点 B（双声道都填 R）
//!                    └─ Both  ─► 端点 C（双声道都填 (L+R)/2）
//! ```
//!
//! **原生采样率端到端**：引擎采样率默认取采集源端点的混音采样率（本机全部 render 端点
//! 都是 48000 Hz）。每个目标端点的混音采样率与它一致时，render 端不让 Windows 做任何
//! 重采样（`AUTOCONVERTPCM` 只负责采样格式转换）；只有目标端点不一致时才走
//! [`MixConverter`] + [`StreamResampler`] 的软件回退路径。旧实现硬编码 44100 Hz，采集端
//! 48k→44.1k、渲染端 44.1k→48k 两趟级联非整数比 SRC，是音质损失与杂音的首要来源；现在
//! 可用 [`RouteEngineConfig::rate`] 显式指定，`None` 表示自动。
//!
//! 采集链路：1 条采集线程（**端点原生混音格式**直接 loopback，**不使用
//! `AUTOCONVERTPCM`**）→ 解码成 f32、应用 `capture_gain`，再按路由通道模式 +
//! `RouteConfig::gain`（可选 `invert` 反相）拆成单声道 i16 推进各路 `VecDeque<i16>`。
//! 每条路由的 sink 线程轮询 `GetCurrentPadding`，缓冲有空位就写 10 ms；队列空补静音
//! （计欠载），队列满丢最旧（计 `dropped_ms`）——两条链路的时钟互相独立，只能靠丢帧/
//! 补静音吸收漂移。
//!
//! 开流顺序：目标端点混音采样率 == 引擎采样率 → `PcmFormat(引擎采样率, 2, 16)` +
//! `AUTOCONVERTPCM`（零 SRC，只转采样格式）；不一致、或该自动转换路径被拒 → 按端点
//! 混音格式（`GetMixFormat`）打开 + Rust 侧用 [`MixConverter`] 重采样 + 采样格式转换
//! （真机实测蓝牙端点混音 44100 Hz 时 `AUTOCONVERTPCM` 必定失败 `0x88890008`，所以必须
//! 保留回退）。实际走通的路径记录在 [`RouteStats::format_path`]。
//!
//! 这是**用户态**实现，不碰内核驱动；虚拟声卡沿用已有的 Steam Streaming Speakers 改名端点。

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioRenderClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::com::{self, ComGuard};
use crate::convert::{f32_to_s16, s16_to_f32, SampleFormat, StreamResampler};
use crate::endpoint::{DataFlow, FormatInfo, PcmFormat};
use crate::error::{AudioError, Result};

/// 旧版本的固定采样率（44.1k）。现在只在探测不到采集源混音格式时兜底；正常路径会走
/// 采集源的原生采样率，见 [`RouteEngineConfig::rate`] 与 [`RouteEngineStats::rate`]。
pub const ROUTE_SAMPLE_RATE: u32 = 44_100;
/// 引擎采样率的合法范围（WASAPI 共享模式常见范围；超出直接拒绝，不做静默钳制）。
const RATE_MIN: u32 = 8_000;
/// 引擎采样率上限（见 [`RATE_MIN`]）。
const RATE_MAX: u32 = 192_000;
/// 每个 sink 在 `Start` 前预填多少毫秒静音（避免一开流就欠载）。
const RENDER_PREFILL_MS: u32 = 10;
/// 默认采集 WASAPI 缓冲（毫秒）。物理输出空闲/切换时，大一点的缓冲更不容易爆音。
const DEFAULT_CAPTURE_BUFFER_MS: u32 = 40;
/// 采集线程没数据时的轮询间隔（毫秒）。
const CAPTURE_POLL_MS: u64 = 5;
/// 电平条衰减系数：每次刷新只保留旧值的 80%（视觉上约 100 ms 的保持）。
const LEVEL_DECAY: f32 = 0.8;
/// 启动时等待所有路由流打开（或报错）的最长时间。
const START_TIMEOUT: Duration = Duration::from_secs(3);
/// 引擎内部格式（2ch / 16 bit）一帧的字节数，`RouteEngineStats::frame_bytes` 用。
const FRAME_BYTES: u64 = 4;
/// 退避重试的起始间隔（毫秒）：设备暂时不可用时第一次重试前等多久。
const RETRY_BASE_MS: u64 = 1_000;
/// 退避重试间隔的上限（毫秒）：指数退避不会等超过这个值。
const RETRY_MAX_MS: u64 = 30_000;
/// 重试等待切成小片，每片检查一次停止标志（保证 `stop()` 最多等这么久，毫秒）。
const RETRY_SLEEP_SLICE_MS: u64 = 100;

/// 10 ms 对应多少帧（sink 每轮最多消费的量，按引擎采样率算）。
fn render_chunk_frames(rate: u32) -> usize {
    (rate as usize / 100).max(1)
}

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
    /// 是否把这条链路的样本**反相**（默认 `false`）。某只音箱接线极性相反、两只箱子
    /// 面对面摆放导致低频抵消时，打开它就能把该路倒过来补偿。
    pub invert: bool,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            channel: RouteChannel::Both,
            gain: 1.0,
            delay_ms: 0,
            enabled: true,
            invert: false,
        }
    }
}

impl RouteConfig {
    /// 快捷构造：端点 + 声道，其余用默认值（增益 1.0、无额外延迟、不反相、启用）。
    pub fn new(endpoint: impl Into<String>, channel: RouteChannel) -> Self {
        Self {
            endpoint: endpoint.into(),
            channel,
            ..Self::default()
        }
    }

    /// 额外延迟换算成 44.1k 的帧数（旧接口；实际播放按引擎采样率换算，见
    /// [`RouteEngineStats::rate`]）。
    pub fn delay_frames(&self) -> usize {
        delay_frames_for_ms(self.delay_ms, ROUTE_SAMPLE_RATE)
    }

    /// 额外延迟换算成 `rate` 下的帧数（sink 线程用）。
    fn delay_frames_at(&self, rate: u32) -> usize {
        delay_frames_for_ms(self.delay_ms, rate)
    }

    /// 人读一行：`Left ← "Xiaomi Sound-1695" | 增益 1.00 | 延迟 0 ms`。
    pub fn describe(&self) -> String {
        format!(
            "{:?} ← \"{}\" | 增益 {:.2} | 延迟 {} ms{}{}",
            self.channel,
            self.endpoint,
            self.gain,
            self.delay_ms,
            if self.invert { " | 反相" } else { "" },
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
    /// 引擎采样率（Hz）。`None` = **自动**：取采集源端点的混音采样率（推荐；本机全部
    /// render 端点都是 48000 Hz），这样采集和输出两端都跑原生采样率、完全不经过
    /// Windows 的重采样。也可以显式指定（8000–192000）——指定值与某个端点混音采样率
    /// 不一致时，该端点会走 [`MixConverter`] 的软件重采样回退路径。
    pub rate: Option<u32>,
    /// 采集端 WASAPI 缓冲（毫秒，20–2000）。`None` = 默认
    /// [`DEFAULT_CAPTURE_BUFFER_MS`]（40 ms）。回环采集**物理输出端点**时（空闲/切换
    /// 设备容易爆音滋啦），可以适当加大。
    pub capture_buffer_ms: Option<u32>,
}

impl Default for RouteEngineConfig {
    fn default() -> Self {
        Self {
            // 自研虚拟声卡的端点名（`扬声器 (立体声链虚拟声卡)`）。历史安装里借用改名过的
            // Steam 设备时，请显式传 `--source "<你的虚拟声卡名>"`。
            source_endpoint: "立体声链虚拟声卡".to_owned(),
            routes: Vec::new(),
            capture_gain: 1.0,
            render_buffer_ms: 40,
            queue_ms: 40,
            rate: None,
            capture_buffer_ms: None,
        }
    }
}

impl RouteEngineConfig {
    /// 实际使用的采集 WASAPI 缓冲（毫秒）：`capture_buffer_ms` 或默认值。
    pub fn effective_capture_buffer_ms(&self) -> u32 {
        self.capture_buffer_ms
            .unwrap_or(DEFAULT_CAPTURE_BUFFER_MS)
    }

    /// 校验配置（不打开设备）。
    ///
    /// 规则：
    /// * `routes` 非空，且至少有一条 `enabled = true`；
    /// * `source_endpoint` 非空；
    /// * `capture_gain` / 每条路由的 `gain` 在 `0.0..=8.0`；
    /// * `render_buffer_ms` 在 `10..=500`，`queue_ms` 在 `0..=500`；
    /// * 每条路由的 `delay_ms` 在 `0..=2000`；
    /// * `rate`（若指定）在 `8000..=192000`，`capture_buffer_ms`（若指定）在 `20..=2000`；
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
        if let Some(rate) = self.rate {
            if !(RATE_MIN..=RATE_MAX).contains(&rate) {
                return Err(AudioError::invalid(format!(
                    "rate={rate} 不在 {RATE_MIN}..={RATE_MAX}"
                )));
            }
        }
        if let Some(buffer_ms) = self.capture_buffer_ms {
            if !(20..=2_000).contains(&buffer_ms) {
                return Err(AudioError::invalid(format!(
                    "capture_buffer_ms={buffer_ms} 不在 20..=2000"
                )));
            }
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
        // 采集 WASAPI 缓冲 + 预排队 + render 缓冲的一半（平均占用）
        self.effective_capture_buffer_ms() + self.queue_ms + self.render_buffer_ms / 2
    }

    /// 人读一行：路由条数、采集源、缓冲参数、采集增益、采样率。
    pub fn describe(&self) -> String {
        let enabled = self.routes.iter().filter(|route| route.enabled).count();
        let rate = match self.rate {
            Some(rate) => format!("{rate} Hz（指定）"),
            None => "自动（采集源混音）".to_owned(),
        };
        format!(
            "{} 条路由（{} 条启用）| 采集源 {} | 队列 {} ms + render {} ms | 采集增益 {:.2} | 采样率 {} | 采集缓冲 {} ms",
            self.routes.len(),
            enabled,
            self.source_endpoint,
            self.queue_ms,
            self.render_buffer_ms,
            self.capture_gain,
            rate,
            self.effective_capture_buffer_ms()
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
    /// 已经写进 render 端缓冲的字节数。
    ///
    /// 原生 `AUTOCONVERTPCM` 路径是 44100/2ch/s16（一帧 4 字节）；回退到「混音格式 +
    /// 软件转换」时按实际混音格式的一帧字节数（`channels × bytes_per_sample`）计算。
    pub sent_bytes: u64,
    /// 欠载次数：这次写缓冲时队列被抽干、只能补静音（采集已经出数据后才计）。
    pub underruns: u64,
    /// 因队列满丢弃的音频总时长（毫秒）：两条链路时钟漂移时用来观察积压是否失控。
    pub dropped_ms: f64,
    /// 当前队列里排队的音频时长（毫秒），稳态应接近配置的 `queue_ms`。
    pub queued_ms: f64,
    /// 这条链路**实际生效**的微调延迟（毫秒），来自延迟线当前长度。
    pub delay_ms: f64,
    /// 这条链路输出的峰值电平（0..1，带衰减，GUI 电平条用）。
    pub level: f32,
    /// 实际走通的 render 开流/转换路径：`None` = 还没打开或失败。
    ///
    /// * `wasapi-autoconvert（…）`：WASAPI 自己把 44100/2ch/s16 转成端点混音格式；
    /// * `mix-format 软件转换（…）`：按端点混音格式打开，Rust 侧重采样 + 采样格式转换。
    pub format_path: Option<String>,
    /// 打开/运行失败原因；`None` = 正常（含尚在打开中）。
    pub error: Option<String>,
    /// 这条路由累计重试打开的次数（`0` = 从未失败过；GUI 用来显示「重试中」）。
    pub retry_count: u64,
    /// 当前正在等待的重试间隔（毫秒）；`0` = 现在没有在等重试（正常或已放弃）。
    pub retry_in_ms: u64,
}

impl RouteStats {
    /// 人读一行（GUI 日志 / 自检用）。
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{} → {} | 已发 {} B | 欠载 {} | 丢 {:.0} ms | 队列 {:.0} ms | 延迟 {:.0} ms | 电平 {:.2}",
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
            self.delay_ms,
            self.level
        );
        if let Some(path) = &self.format_path {
            text.push_str(&format!(" | 路径：{path}"));
        }
        if self.retry_in_ms > 0 {
            text.push_str(&format!(
                " | 重试中：第 {} 次失败，{} ms 后重试",
                self.retry_count, self.retry_in_ms
            ));
        }
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
    /// 采集侧最近一次错误。
    ///
    /// 设备暂时失效（拔掉/休眠/默认输出切换）这类错误会自动退避重试，重试期间引擎继续
    /// 跑（输出补静音，`source_retrying` 反映重试次数）；只有配置/格式类不可重试错误才停引擎。
    pub source_error: Option<String>,
    /// 是否处于「降级」状态：引擎还在跑，但**一条输出路由都没能打开**（正在后台重试）。
    pub degraded: bool,
    /// 采集线程当前连续重试次数（`0` = 采集正常；`>0` = 正在等设备回来）。
    pub source_retrying: u64,
    /// 实际使用的引擎采样率（Hz）。`None` 不是值——自动模式下这就是采集源混音采样率。
    pub rate: u32,
    /// 实际使用的采集 WASAPI 缓冲（毫秒）。
    pub capture_buffer_ms: u32,
    /// 累计采集帧数（引擎采样率下的帧）。
    pub captured_frames: u64,
    /// 累计采集字节数（引擎内部 2ch/s16 = 4 B/帧）。
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
            "采集 {} 帧（{:.2} MB，原生 {} Hz）| 源 {} | 电平 L {:.2} / R {:.2} | {} 条路由",
            self.captured_frames,
            self.frame_bytes as f64 / 1_048_576.0,
            self.rate,
            source,
            self.level_l,
            self.level_r,
            self.routes.len()
        );
        if let Some(err) = &self.source_error {
            text.push_str(&format!(" | 采集故障：{err}"));
        }
        if self.degraded {
            text.push_str(" | 降级：没有输出路由打开，正在后台重试");
        }
        if self.source_retrying > 0 {
            text.push_str(&format!(" | 采集重试中：第 {} 次", self.source_retrying));
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
    ///   打不开的路由会在后台按 1s→2s→4s→8s→（封顶 30s）指数退避重试，
    ///   设备休眠/掉线恢复后自动补上，不用重启引擎；
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
        // 原生采样率：默认取采集源端点的混音采样率（全部一致 → 采集/输出两端都零 SRC）。
        let engine_rate = effective_engine_rate(
            config.rate,
            if config.rate.is_none() {
                probe_source_mix_rate(&config.source_endpoint)
            } else {
                None
            },
        );
        let queue_cap_samples = queue_capacity_samples(config.queue_ms, engine_rate);
        let sinks: Vec<Mutex<SinkState>> = enabled
            .iter()
            .map(|route| Mutex::new(SinkState::new(route, queue_cap_samples)))
            .collect();
        let delays_ms: Vec<AtomicU32> = enabled
            .iter()
            .map(|route| AtomicU32::new(route.delay_ms))
            .collect();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            captured_frames: AtomicU64::new(0),
            rate: engine_rate,
            level_l: AtomicU32::new(0),
            level_r: AtomicU32::new(0),
            started: Instant::now(),
            source_device: Mutex::new(String::new()),
            source_error: Mutex::new(None),
            source_retrying: AtomicU64::new(0),
            sinks,
            delays_ms,
        });

        let mut threads: Vec<JoinHandle<()>> = Vec::with_capacity(sink_count + 1);
        for (index, route) in enabled.iter().enumerate() {
            let thread_shared = shared.clone();
            let thread_route = route.clone();
            let render_buffer_ms = config.render_buffer_ms;
            match thread::Builder::new()
                .name(format!("stereo-link-route-sink-{index}"))
                .spawn(move || {
                    run_sink(thread_shared, index, thread_route, render_buffer_ms, engine_rate)
                })
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

        // 契约：3 秒内一条路都没打开 → 启动失败（≥1 条 → Ok，其余在后台继续重试）。
        if count_opened(&shared) == 0 {
            let mut reasons = failure_reasons(&shared);
            if reasons.is_empty() {
                reasons.push(format!(
                    "{} 秒内没有任何路由完成打开",
                    START_TIMEOUT.as_secs()
                ));
            }
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

    /// 是否还在跑：只看有没有被显式停止。
    ///
    /// 单条路由打开失败、采集源暂时掉线都会在后台重试，引擎仍算 running（此时
    /// [`RouteEngineStats::degraded`] / [`RouteEngineStats::source_error`] 会如实反映状态）。
    pub fn alive(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed)
    }

    /// 播放中实时调整某条路由的微调延迟（毫秒），**不需要重启引擎**。
    ///
    /// `index` 是**启用**路由里的下标，与 [`RouteEngineStats::routes`] 的顺序一致
    /// （0 = 配置里第一条 `enabled=true` 的路由）。
    ///
    /// 变大 = 立刻多垫一段静音（这条链路慢一点）；变小 = 立刻丢掉最老的样本追上。
    /// 越界或引擎已停返回 `false`。
    pub fn set_delay_ms(&self, index: usize, delay_ms: u32) -> bool {
        match self.shared.delays_ms.get(index) {
            Some(slot) if !self.shared.stop.load(Ordering::Relaxed) => {
                slot.store(delay_ms, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// 批量设置微调延迟（下标同 [`Self::set_delay_ms`]）；返回成功设置的条数。
    pub fn set_delays_ms(&self, delays_ms: &[u32]) -> usize {
        delays_ms
            .iter()
            .enumerate()
            .filter(|(index, ms)| self.set_delay_ms(*index, **ms))
            .count()
    }

    /// 某条路由当前生效的微调延迟（毫秒）；越界返回 `None`。
    pub fn delay_ms(&self, index: usize) -> Option<u32> {
        self.shared
            .delays_ms
            .get(index)
            .map(|slot| slot.load(Ordering::Relaxed))
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
                    dropped_ms: samples_to_ms(sink.dropped_samples, self.shared.rate),
                    queued_ms: samples_to_ms(sink.queue.len() as u64, self.shared.rate),
                    delay_ms: samples_to_ms(sink.delay_frames as u64, self.shared.rate),
                    level: sink.level,
                    format_path: sink.format_path.clone(),
                    error: sink.error.clone(),
                    retry_count: sink.retry_count,
                    retry_in_ms: sink.retry_in_ms,
                }),
                // 锁被 poison 只可能是音频线程 panic（正常情况下不会），给个占位不拖垮快照。
                Err(_) => routes.push(RouteStats::default()),
            }
        }
        RouteEngineStats {
            running: self.alive(),
            source_device,
            source_error,
            degraded: count_opened(&self.shared) == 0,
            source_retrying: self.shared.source_retrying.load(Ordering::Relaxed),
            rate: self.shared.rate,
            capture_buffer_ms: self.config.effective_capture_buffer_ms(),
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
    /// 是否把该路样本反相（写入队列前取反）。
    invert: bool,
    /// 是否已经成功打开 render 流（运行中失效后被重试流程重开；`stats()` 用它判断「路径」）。
    opened: bool,
    /// 首次启动结论是否已定（打开成功或第一次失败）；`start()` 的等待条件。
    settled: bool,
    /// 打开/运行失败原因；`Some` = 当前这次打开失败（这条路由会在后台退避重试）。
    error: Option<String>,
    /// 累计重试打开的次数（含正在等待的这一次）。
    retry_count: u64,
    /// 当前等待中的重试间隔（毫秒）；`0` = 没在等重试。
    retry_in_ms: u64,
    queue: VecDeque<i16>,
    queue_cap_samples: usize,
    sent_bytes: u64,
    underruns: u64,
    dropped_samples: u64,
    /// 延迟线当前长度（帧），`stats()` 用它报「实际生效的微调延迟」。
    delay_frames: usize,
    /// 输出峰值电平（0..1，带衰减）。
    level: f32,
    /// 实际走通的输出路径描述（开流成功后写入；见 [`RouteStats::format_path`]）。
    format_path: Option<String>,
}

impl SinkState {
    fn new(route: &RouteConfig, queue_cap_samples: usize) -> Self {
        let cap = queue_cap_samples.max(1);
        Self {
            endpoint: route.endpoint.clone(),
            channel: route.channel,
            gain: route.gain,
            invert: route.invert,
            opened: false,
            settled: false,
            error: None,
            retry_count: 0,
            retry_in_ms: 0,
            queue: VecDeque::with_capacity(cap),
            queue_cap_samples: cap,
            sent_bytes: 0,
            underruns: 0,
            dropped_samples: 0,
            delay_frames: 0,
            level: 0.0,
            format_path: None,
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
    captured_frames: AtomicU64,
    /// 引擎采样率（Hz）：采集侧转换目标、延迟/队列换算基准。启动时确定，运行中不变。
    rate: u32,
    /// 最近一段采集的左/右峰值（f32::to_bits），界面电平条用。
    level_l: AtomicU32,
    level_r: AtomicU32,
    started: Instant,
    source_device: Mutex<String>,
    source_error: Mutex<Option<String>>,
    /// 采集线程当前连续重试次数（0 = 采集正常，>0 = 正在等设备回来）。
    source_retrying: AtomicU64,
    /// 每条启用路由一份；下标与启用顺序一致。
    sinks: Vec<Mutex<SinkState>>,
    /// 每条启用路由的微调延迟（毫秒），下标与 `sinks` 一致；可在播放中随时改。
    delays_ms: Vec<AtomicU32>,
}

/// 采集源端点的混音格式（回环按它原样开流，转换全部在 Rust 侧做）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CaptureFormat {
    samplerate: u32,
    channels: u16,
    sample_format: SampleFormat,
    block_align: usize,
}

impl CaptureFormat {
    /// 从端点混音格式里取出可解码的采集格式。
    ///
    /// 支持 1/2 声道 + 16/24/32-bit int + 32-bit float（都是 [`SampleFormat`] 能解码的
    /// 格式）；**不支持的组合返回明确错误**，绝不静默写错格式（例如 6 声道混音会把
    /// 声道顺序写错）。
    fn from_info(info: &FormatInfo) -> Result<Self> {
        if !info.is_valid() {
            return Err(AudioError::UnsupportedFormat(format!(
                "采集源混音格式无效：{}",
                info.describe()
            )));
        }
        let sample_format = info.sample_format.ok_or_else(|| {
            AudioError::UnsupportedFormat(format!(
                "采集源混音格式的采样类型不认识：{}",
                info.describe()
            ))
        })?;
        if info.channels == 0 || info.channels > 2 {
            return Err(AudioError::UnsupportedFormat(format!(
                "采集源混音格式暂不支持 {} 声道（{}），请把采集源默认格式改成 1 或 2 声道",
                info.channels,
                info.describe()
            )));
        }
        let expected_align = info.channels as usize * sample_format.bytes_per_sample();
        if info.block_align as usize != expected_align {
            return Err(AudioError::UnsupportedFormat(format!(
                "采集源混音格式的 nBlockAlign={} 与 {} 声道 × {sample_format:?} 不一致（{}）",
                info.block_align,
                info.channels,
                info.describe()
            )));
        }
        Ok(Self {
            samplerate: info.samplerate,
            channels: info.channels,
            sample_format,
            block_align: expected_align,
        })
    }

    /// 一帧字节数（= `nBlockAlign`）。
    fn bytes_per_frame(&self) -> usize {
        self.block_align
    }

    /// 人读描述：`48000 Hz / 2 ch / F32`。
    fn describe(&self) -> String {
        format!(
            "{} Hz / {} ch / {:?}",
            self.samplerate, self.channels, self.sample_format
        )
    }
}

/// 采集端 DSP：端点原生混音格式字节 → 引擎采样率左右两路 i16，顺带应用采集增益。
///
/// 采样率不一致时只在这一处做一次流式重采样（`StreamResampler` 保留跨包相位，不会在
/// 包边界爆音/漂移）；默认自动模式下采集源采样率 == 引擎采样率，这次重采样不会发生。
#[derive(Debug)]
struct CaptureDsp {
    format: CaptureFormat,
    capture_gain: f32,
    /// 采集源采样率 ≠ 引擎采样率才需要（2 声道交错流式重采样）。
    resampler: Option<StreamResampler>,
    /// 混音格式解码后的 f32（交错）。
    decoded: Vec<f32>,
    /// 统一成 2 声道交错后的 f32（采集源是单声道时复制成 L/R）。
    stereo: Vec<f32>,
    /// 重采样后的 f32（仅 `resampler` 为 `Some` 时用）。
    resampled: Vec<f32>,
    /// 最终交给路由队列的左右声道 i16（引擎采样率）。
    left: Vec<i16>,
    right: Vec<i16>,
}

impl CaptureDsp {
    fn new(format: CaptureFormat, engine_rate: u32, capture_gain: f32) -> Self {
        let resampler = if format.samplerate == engine_rate {
            None
        } else {
            Some(StreamResampler::new(format.samplerate, engine_rate, 2))
        };
        Self {
            format,
            capture_gain,
            resampler,
            decoded: Vec::new(),
            stereo: Vec::new(),
            resampled: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    /// 一包采集数据写进 `left/right`，返回（引擎采样率帧数，左峰值，右峰值）。
    ///
    /// `silent = true` 表示 WASAPI 置了 `AUDCLNT_BUFFERFLAGS_SILENT`：这时缓冲内容未定义。
    /// 必须按 `frames` 补零，绝不能把未初始化的内存当音频写出去。
    fn process(&mut self, bytes: &[u8], frames: usize, silent: bool) -> Result<(usize, f32, f32)> {
        // 16-bit 整数混音 + 采集增益 1.0 + 采样率一致：直接搬 i16，避免 s16→f32→s16
        // 往返带来的 ±1 LSB 量化误差（这条快路径正是本机绝大多数端点的路径）。
        if self.format.sample_format == SampleFormat::S16
            && (self.capture_gain - 1.0).abs() <= f32::EPSILON
            && self.resampler.is_none()
        {
            let mut left = std::mem::take(&mut self.left);
            let mut right = std::mem::take(&mut self.right);
            left.clear();
            right.clear();
            let mut peak_l = 0.0f32;
            let mut peak_r = 0.0f32;
            if silent {
                left.resize(frames, 0);
                right.resize(frames, 0);
            } else {
                left.reserve(frames);
                right.reserve(frames);
                for frame in bytes.chunks_exact(self.format.block_align) {
                    let l = i16::from_le_bytes([frame[0], frame[1]]);
                    let r = if self.format.channels == 1 {
                        l
                    } else {
                        i16::from_le_bytes([frame[2], frame[3]])
                    };
                    peak_l = peak_l.max((l as f32 / 32_768.0).abs());
                    peak_r = peak_r.max((r as f32 / 32_768.0).abs());
                    left.push(l);
                    right.push(r);
                }
            }
            let out_frames = left.len();
            self.left = left;
            self.right = right;
            return Ok((out_frames, peak_l, peak_r));
        }

        self.decoded.clear();
        if silent {
            self.decoded
                .resize(frames.saturating_mul(self.format.channels as usize), 0.0);
        } else {
            self.format
                .sample_format
                .decode_into(bytes, &mut self.decoded)?;
        }
        if (self.capture_gain - 1.0).abs() > f32::EPSILON {
            for sample in &mut self.decoded {
                *sample *= self.capture_gain;
            }
        }

        // 统一成 2 声道交错（单声道复制成 L/R）。
        let mut stereo = std::mem::take(&mut self.stereo);
        stereo.clear();
        if self.format.channels == 1 {
            stereo.reserve(self.decoded.len() * 2);
            for &sample in &self.decoded {
                stereo.push(sample);
                stereo.push(sample);
            }
        } else {
            stereo.extend_from_slice(&self.decoded);
        }

        // 采样率不一致时，只在这里重采样一次（流式，跨包保留相位）。
        let mut resampled = std::mem::take(&mut self.resampled);
        resampled.clear();
        let converted: &[f32] = match &mut self.resampler {
            Some(resampler) => {
                resampler.process(&stereo, &mut resampled);
                &resampled
            }
            None => &stereo,
        };
        let out_frames = converted.len() / 2;

        let mut left = std::mem::take(&mut self.left);
        let mut right = std::mem::take(&mut self.right);
        left.clear();
        right.clear();
        left.reserve(out_frames);
        right.reserve(out_frames);
        let (mut peak_l, mut peak_r) = (0.0f32, 0.0f32);
        for frame in converted.chunks_exact(2) {
            let l = f32_to_s16(frame[0]);
            let r = f32_to_s16(frame[1]);
            peak_l = peak_l.max((l as f32 / 32_768.0).abs());
            peak_r = peak_r.max((r as f32 / 32_768.0).abs());
            left.push(l);
            right.push(r);
        }

        self.stereo = stereo;
        self.resampled = resampled;
        self.left = left;
        self.right = right;
        Ok((out_frames, peak_l, peak_r))
    }
}

/// 采集源是否发生采样率转换（自动模式下应为 `false`，表示全程零 SRC）。测试与诊断用。
#[cfg(test)]
fn capture_resamples(dsp: &CaptureDsp) -> bool {
    dsp.resampler.is_some()
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

/// 一条路由的微调延迟线。
///
/// 与输入侧的抖动队列分工不同：抖动队列是「攒够就发、满了丢最旧」，它的占用时长由
/// 采集/输出两条链路的时钟差决定，**不能被用来做固定延时**（丢最旧会把多出来的延迟
/// 吃回去，这正是之前「左右微调延迟没有效果」的原因）。延迟线则是一个固定长度的
/// FIFO：先把输出垫 `frames` 个静音，之后每个采样都晚 `frames` 个样本出来，容量可以
/// 在播放中随时增减（增 = 立刻多等一会儿，减 = 立刻追上）。
#[derive(Debug, Default)]
struct DelayLine {
    line: VecDeque<i16>,
}

impl DelayLine {
    /// 空延迟线（0 ms，直通）。
    fn new() -> Self {
        Self {
            line: VecDeque::new(),
        }
    }

    /// 当前延迟帧数。
    fn frames(&self) -> usize {
        self.line.len()
    }

    /// 把延迟调整到 `frames`：变长就垫静音，变短就丢掉最老的样本（立即生效）。
    fn set_frames(&mut self, frames: usize) {
        while self.line.len() < frames {
            self.line.push_back(0);
        }
        while self.line.len() > frames {
            self.line.pop_front();
        }
    }

    /// 送一个采样进去，取出延迟后的采样（延迟为 0 时直通）。
    fn process(&mut self, sample: i16) -> i16 {
        if self.line.is_empty() {
            return sample;
        }
        self.line.push_back(sample);
        self.line.pop_front().unwrap_or(0)
    }
}

/// `delay_ms` 换算成 `rate` 下延迟线要保留的帧数。
fn delay_frames_for_ms(delay_ms: u32, rate: u32) -> usize {
    (delay_ms as u64 * rate as u64 / 1000) as usize
}

/// `queue_ms` 换算成每条路由的队列容量（单声道采样数，至少 1）。
fn queue_capacity_samples(queue_ms: u32, rate: u32) -> usize {
    (rate as u64 * queue_ms as u64 / 1000).max(1) as usize
}

/// 采样数换算成毫秒。
fn samples_to_ms(samples: u64, rate: u32) -> f64 {
    if rate == 0 {
        return 0.0;
    }
    samples as f64 * 1000.0 / rate as f64
}

/// i16 取反（相位反转）。用 i32 取负再钳制，`i16::MIN` 不会溢出。
fn invert_sample(sample: i16) -> i16 {
    (-(sample as i32)).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// 纯逻辑：由配置/探测结果决定引擎采样率。
///
/// * `config_rate = Some(r)` → 用 r（`validate()` 已校验范围）；
/// * `None` + 探测到采集源混音采样率 → 用采集源原生采样率（**默认、零 SRC** 的路径）；
/// * `None` + 探测失败 → 回退 [`ROUTE_SAMPLE_RATE`] 并继续（采集侧会按需要软件重采样，
///   不会静默写错格式）。
fn effective_engine_rate(config_rate: Option<u32>, source_mix_rate: Option<u32>) -> u32 {
    config_rate.or(source_mix_rate).unwrap_or(ROUTE_SAMPLE_RATE)
}

/// 探测采集源端点的混音采样率（自动模式用）。
///
/// 打不开设备时返回 `None`，由 [`effective_engine_rate`] 兜底；真正的采集错误会在采集
/// 线程里如实上报到 `RouteEngineStats::source_error`。
fn probe_source_mix_rate(source_endpoint: &str) -> Option<u32> {
    let _com = ComGuard::new().ok()?;
    let enumerator = com::enumerator().ok()?;
    let selector = crate::sync::selector_for(source_endpoint);
    let device = com::resolve(&enumerator, &selector, DataFlow::Render).ok()?;
    let client = com::activate_client(&device.device).ok()?;
    let info = com::mix_format(&client).ok()?;
    if info.is_valid() {
        Some(info.samplerate)
    } else {
        None
    }
}

/// 启动阶段：还有多少条路由的首次结论没定（既没打开也没第一次失败）。
fn count_pending(shared: &Shared) -> usize {
    shared
        .sinks
        .iter()
        .filter(|slot| match slot.lock() {
            Ok(sink) => !sink.settled,
            // 锁坏了当作已结束，别让 start() 白等 3 秒。
            Err(_) => false,
        })
        .count()
}

/// 当前成功打开着 render 流的路由条数（`degraded` 判定用）。
fn count_opened(shared: &Shared) -> usize {
    shared
        .sinks
        .iter()
        .filter(|slot| match slot.lock() {
            Ok(sink) => sink.opened,
            Err(_) => false,
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
// 自愈重试：设备暂时不可用（休眠/掉线/默认输出切换）时退避重试，不判死引擎
// ---------------------------------------------------------------------------

/// 这个错误是否值得退避重试（设备暂时不可用类错误）。
///
/// 可重试：设备找不到、没有默认设备、设备已失效（`AUDCLNT_E_DEVICE_INVALIDATED`）、
/// 音频服务没在跑（`AUDCLNT_E_SERVICE_NOT_RUNNING`）、设备被占用/权限暂时不足
/// （`E_ACCESSDENIED`）。不可重试：参数/格式类错误（重试也不会变）。
fn is_retryable(error: &AudioError) -> bool {
    match error {
        AudioError::DeviceNotFound(_) | AudioError::NoDefaultDevice(_) => true,
        AudioError::DeviceInvalidated => true,
        AudioError::Wasapi(hr, _) => is_retryable_hresult(*hr),
        _ => false,
    }
}

/// HRESULT 是否属于「设备暂时不可用」的可重试集合。
fn is_retryable_hresult(hr: i32) -> bool {
    /// `AUDCLNT_E_DEVICE_INVALIDATED`：端点被拔掉/切换/格式变化。
    const AUDCLNT_E_DEVICE_INVALIDATED: u32 = 0x8889_0004;
    /// `AUDCLNT_E_SERVICE_NOT_RUNNING`：音频服务暂时没跑。
    const AUDCLNT_E_SERVICE_NOT_RUNNING: u32 = 0x8889_0010;
    /// `E_ACCESSDENIED`：设备被独占/权限暂时不足（切端点时常见）。
    const E_ACCESSDENIED: u32 = 0x8007_0005;
    matches!(
        hr as u32,
        AUDCLNT_E_DEVICE_INVALIDATED | AUDCLNT_E_SERVICE_NOT_RUNNING | E_ACCESSDENIED
    )
}

/// 第 `attempt` 次重试前的等待时长（毫秒）：1s→2s→4s→8s→…→封顶 30s。
///
/// `attempt` 从 1 开始（第 1 次失败后等 1s）；0 也按 1s 处理，防御非法调用。
fn retry_backoff_ms(attempt: u32) -> u64 {
    let exponent = attempt.saturating_sub(1).min(8);
    RETRY_BASE_MS
        .saturating_mul(1u64 << exponent)
        .min(RETRY_MAX_MS)
}

/// 分片睡眠 `total_ms`，期间每 [`RETRY_SLEEP_SLICE_MS`] 检查一次停止标志。
///
/// 返回 `false` = 睡到一半发现引擎要停，调用方应立刻退出（`stop()` 不会被长退避卡住）。
fn sleep_unless_stopped(shared: &Shared, total_ms: u64) -> bool {
    let mut remaining = total_ms;
    while remaining > 0 {
        if shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        let slice = remaining.min(RETRY_SLEEP_SLICE_MS);
        thread::sleep(Duration::from_millis(slice));
        remaining -= slice;
    }
    !shared.stop.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// sink 线程：一条路由一个，独立打开、独立写、独立失败
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 输出格式：AUTOCONVERTPCM 优先，失败则按端点混音格式 + 软件转换
// ---------------------------------------------------------------------------

/// render 流实际走通的输出路径。
///
/// `AutoConvert`（WASAPI 自动转换）只在**目标端点混音采样率 == 引擎采样率**时使用：
/// 请求 `PcmFormat(引擎采样率, 2, 16)` + `AUTOCONVERTPCM`，WASAPI 只做采样格式转换
/// （不重采样，零 SRC）。混音采样率不一致、或自动转换被拒（真机实测蓝牙端点混音
/// 44100 Hz 时必定 `0x88890008 AUDCLNT_E_UNSUPPORTED_FORMAT`）时回退成 `Converted`：
/// 按端点自己的混音格式开流，Rust 侧把引擎采样率的单声道 i16 重采样/编码成混音格式。
#[derive(Debug)]
enum SinkOutput {
    /// WASAPI 自动转换到 `引擎采样率/2ch/s16`（采样率与端点混音一致时零 SRC）。
    AutoConvert {
        /// 引擎采样率（= 原生路径的输出采样率）。
        rate: u32,
    },
    /// 按端点混音格式打开，Rust 侧转换（目标采样率可能和引擎采样率不同）。
    Converted {
        /// 端点混音格式（决定写数据的采样率 / 声道 / 采样类型）。
        mix: MixOutputFormat,
        /// 流式转换器（引擎采样率单声道 i16 → 混音格式字节）。
        converter: MixConverter,
    },
}

impl SinkOutput {
    /// 写进 [`RouteStats::format_path`] 的路径描述。
    fn describe(&self) -> String {
        match self {
            SinkOutput::AutoConvert { rate } => {
                format!("wasapi-autoconvert（{rate} Hz / 2 ch / 16-bit int，零 SRC）")
            }
            SinkOutput::Converted { mix, .. } => {
                format!("mix-format 软件转换（{}）", mix.describe())
            }
        }
    }

    /// 一帧输出的字节数。
    fn bytes_per_frame(&self) -> usize {
        match self {
            SinkOutput::AutoConvert { .. } => FRAME_BYTES as usize,
            SinkOutput::Converted { mix, .. } => mix.bytes_per_frame(),
        }
    }

    /// 实际输出采样率（预填静音帧数用）。
    fn samplerate(&self) -> u32 {
        match self {
            SinkOutput::AutoConvert { rate } => *rate,
            SinkOutput::Converted { mix, .. } => mix.samplerate,
        }
    }

    /// render 缓冲还剩 `available` 帧时，本次最多消费多少 *引擎采样率* 输入帧。
    ///
    /// 原生路径就是 10 ms 一块；回退路径要先按重采样比例反推，保证转换后不超缓冲。
    fn input_frames_for_available(&self, available: usize) -> usize {
        match self {
            SinkOutput::AutoConvert { rate } => available.min(render_chunk_frames(*rate)),
            SinkOutput::Converted { converter, .. } => converter
                .input_frames_for_available(available)
                .min(render_chunk_frames(converter.engine_rate())),
        }
    }
}

/// 回退路径的目标输出格式：端点混音格式里我们支持软件转换的子集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MixOutputFormat {
    samplerate: u32,
    channels: u16,
    sample_format: SampleFormat,
}

impl MixOutputFormat {
    /// 从端点混音格式里取出可转换的目标格式。
    ///
    /// 只支持 1/2 声道 + 16/32-bit int + 32-bit float。**不支持的组合返回明确错误**，
    /// 绝不静默写错格式（例如 6 声道混音下把单声道塞进前置声道会错位）。
    fn from_info(info: &FormatInfo) -> Result<Self> {
        if !info.is_valid() {
            return Err(AudioError::UnsupportedFormat(format!(
                "端点混音格式无效：{}",
                info.describe()
            )));
        }
        let sample_format = info.sample_format.ok_or_else(|| {
            AudioError::UnsupportedFormat(format!(
                "回退路径不认识的混音采样格式：{}",
                info.describe()
            ))
        })?;
        if !matches!(
            sample_format,
            SampleFormat::S16 | SampleFormat::S32 | SampleFormat::F32
        ) {
            return Err(AudioError::UnsupportedFormat(format!(
                "回退路径暂不支持 {sample_format:?} 混音格式（{}），需要 16/32-bit int 或 32-bit float",
                info.describe()
            )));
        }
        if info.channels == 0 || info.channels > 2 {
            return Err(AudioError::UnsupportedFormat(format!(
                "回退路径暂不支持 {} 声道混音格式（{}），请把设备默认格式改成 1 或 2 声道",
                info.channels,
                info.describe()
            )));
        }
        let expected_align = info.channels as usize * sample_format.bytes_per_sample();
        if info.block_align as usize != expected_align {
            return Err(AudioError::UnsupportedFormat(format!(
                "回退路径混音格式的 nBlockAlign={} 与 {} 声道 × {sample_format:?} 不一致（{}）",
                info.block_align,
                info.channels,
                info.describe()
            )));
        }
        Ok(Self {
            samplerate: info.samplerate,
            channels: info.channels,
            sample_format,
        })
    }

    /// 一帧字节数（= `nBlockAlign`）。
    fn bytes_per_frame(&self) -> usize {
        self.channels as usize * self.sample_format.bytes_per_sample()
    }

    /// 人读描述：`48000 Hz / 2 ch / 32-bit float`。
    fn describe(&self) -> String {
        let kind = match self.sample_format {
            SampleFormat::S16 => "16-bit int",
            SampleFormat::S24 => "24-bit int",
            SampleFormat::S32 => "32-bit int",
            SampleFormat::F32 => "32-bit float",
        };
        format!("{} Hz / {} ch / {}", self.samplerate, self.channels, kind)
    }
}

/// 引擎采样率单声道 i16 → 端点混音格式字节的流式转换器（纯逻辑，可离线单测）。
#[derive(Debug)]
struct MixConverter {
    mix: MixOutputFormat,
    /// 输入侧采样率（= 引擎采样率）。
    engine_rate: u32,
    /// 引擎采样率单声道 → 混音采样率的流式线性重采样器。
    resampler: StreamResampler,
    /// 中间缓冲（避免每个音频块都分配）。
    mono_f32: Vec<f32>,
    resampled: Vec<f32>,
    interleaved: Vec<f32>,
}

impl MixConverter {
    /// 为指定混音格式建转换器（`engine_rate` = 输入侧引擎采样率）。
    fn new(mix: MixOutputFormat, engine_rate: u32) -> Self {
        Self {
            mix,
            engine_rate,
            resampler: StreamResampler::new(engine_rate, mix.samplerate, 1),
            mono_f32: Vec::new(),
            resampled: Vec::new(),
            interleaved: Vec::new(),
        }
    }

    /// 输入侧引擎采样率。
    fn engine_rate(&self) -> u32 {
        self.engine_rate
    }

    /// 给定 render 缓冲剩余帧数，最多能消费多少引擎采样率输入帧（转换后不超容量）。
    fn input_frames_for_available(&self, available: usize) -> usize {
        if available == 0 {
            return 0;
        }
        // 输出帧数 ≈ 输入帧数 / step（step = 引擎采样率 / 目标采样率，见 StreamResampler）。
        let frames = (available as f64 * self.resampler.step()).floor() as usize;
        if self.resampler.is_passthrough() {
            frames
        } else {
            // 线性插值要等下一帧才能出点，输出可能比换算值少 1；再留 1 帧余量。
            frames.saturating_sub(1)
        }
    }

    /// 把一块单声道引擎采样率 i16 采样编码成混音格式字节（追加到 `out`），返回输出帧数。
    ///
    /// 采样率不同则流式重采样；单声道复制到左右两个输出声道（只接一只箱子也不缺节目）。
    fn convert(&mut self, mono: &[i16], out: &mut Vec<u8>) -> usize {
        out.clear();
        // 16-bit 整数混音 + 采样率一致：直接复制 i16（要求「16bit 直接写」，避免
        // i16→f32→i16 往返带来的 ±1 LSB 量化误差）。
        if self.mix.sample_format == SampleFormat::S16 && self.resampler.is_passthrough() {
            out.reserve(mono.len() * self.mix.channels as usize * 2);
            for &sample in mono {
                let bytes = sample.to_le_bytes();
                for _ in 0..self.mix.channels {
                    out.extend_from_slice(&bytes);
                }
            }
            return mono.len();
        }
        self.mono_f32.clear();
        self.mono_f32
            .extend(mono.iter().map(|&sample| s16_to_f32(sample)));
        self.resampled.clear();
        self.resampler.process(&self.mono_f32, &mut self.resampled);
        let frames = self.resampled.len();
        if self.mix.channels == 1 {
            self.mix.sample_format.encode_into(&self.resampled, out);
        } else {
            self.interleaved.clear();
            self.interleaved.reserve(frames * 2);
            for &sample in &self.resampled {
                self.interleaved.push(sample);
                self.interleaved.push(sample);
            }
            self.mix.sample_format.encode_into(&self.interleaved, out);
        }
        frames
    }
}

/// 回退路径：按端点自己的混音格式（原始 `WAVEFORMATEX*`）打开 render 流。
///
/// `GetMixFormat` 返回的内存由 COM 分配，初始化后立刻 `CoTaskMemFree`。不加
/// `AUTOCONVERTPCM`（这正是失败的那条路），由 [`MixConverter`] 负责数据格式转换。
fn initialize_with_mix_format(
    client: &windows::Win32::Media::Audio::IAudioClient,
    render_buffer_ms: u32,
) -> Result<MixOutputFormat> {
    let format = unsafe { client.GetMixFormat() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetMixFormat(route fallback)"))?;
    if format.is_null() {
        return Err(AudioError::invalid("GetMixFormat 返回空指针"));
    }
    // SAFETY: GetMixFormat 成功时返回至少 18 字节的 WAVEFORMATEX（扩展格式更长）。
    let info = unsafe { com::format_info_from_raw(format as *const u8) };
    let init = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            0,
            com::ms_to_hns(render_buffer_ms),
            0,
            format,
            None,
        )
    };
    // SAFETY: format 来自 GetMixFormat，所有权在本函数，Initialize 已复制所需字段。
    unsafe { CoTaskMemFree(Some(format as *const c_void)) };
    init.map_err(|e| {
        AudioError::wasapi(
            e.code().0,
            format!("Initialize(route fallback {})", info.describe()),
        )
    })?;
    MixOutputFormat::from_info(&info)
}

/// 两条开流路径都失败时的统一错误文案（纯逻辑，单测覆盖关键字）。
fn render_init_error_text(primary: &str, fallback: &AudioError) -> String {
    format!("初始化 render 失败：自动转换路径不可用（{primary}），回退混音格式也失败：{fallback}")
}

fn run_sink(
    shared: Arc<Shared>,
    index: usize,
    route: RouteConfig,
    render_buffer_ms: u32,
    engine_rate: u32,
) {
    let mut consecutive_failures: u32 = 0;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let started = Instant::now();
        match sink_loop(&shared, index, &route, render_buffer_ms, engine_rate) {
            // sink_loop 只在引擎 stop 时正常返回。
            Ok(()) => break,
            Err(error) => {
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                // 这次流跑了超过 1 秒才坏 = 健康过：退避从头算（掉线后不会越等越久）。
                if started.elapsed() >= Duration::from_secs(1) {
                    consecutive_failures = 0;
                }
                let retryable = is_retryable(&error);
                let delay_ms = if retryable {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    retry_backoff_ms(consecutive_failures)
                } else {
                    0
                };
                let Ok(mut sink) = shared.sinks[index].lock() else {
                    break;
                };
                sink.settled = true;
                sink.opened = false;
                sink.error = Some(error.to_string());
                // 当前这次流已经没了，路径也失效（重开后由 sink_loop 重新写入）。
                sink.format_path = None;
                if retryable {
                    sink.retry_count = sink.retry_count.saturating_add(1);
                    sink.retry_in_ms = delay_ms;
                } else {
                    sink.retry_in_ms = 0;
                }
                drop(sink);
                if !retryable {
                    // 参数/格式类错误重试也没用：这条路由彻底放弃（其它路继续跑）。
                    break;
                }
                if !sleep_unless_stopped(&shared, delay_ms) {
                    break;
                }
            }
        }
    }
    // 线程退出（显式 stop 或不可重试错误）：收掉「重试中」，避免 GUI 一直显示倒计时。
    if let Ok(mut sink) = shared.sinks[index].lock() {
        sink.opened = false;
        sink.settled = true;
        sink.retry_in_ms = 0;
    }
}

fn sink_loop(
    shared: &Arc<Shared>,
    index: usize,
    route: &RouteConfig,
    render_buffer_ms: u32,
    engine_rate: u32,
) -> Result<()> {
    let _com = ComGuard::new()?;
    let enumerator = com::enumerator()?;
    let selector = crate::sync::selector_for(&route.endpoint);
    let device = com::resolve(&enumerator, &selector, DataFlow::Render)?;
    let mut client = com::activate_client(&device.device)?;
    let mix_info = com::mix_format(&client)?;
    // 目标端点混音采样率 == 引擎采样率时，才让 WASAPI 自动转换（只转采样格式、零 SRC）；
    // 不一致时直接走软件回退路径（Windows 的 AUTOCONVERTPCM 会再做一次非整数比 SRC，
    // 正是我们要消除的音质损失来源）。
    let target_mix = MixOutputFormat::from_info(&mix_info).ok();
    let rate_matches = target_mix
        .map(|mix| mix.samplerate == engine_rate)
        .unwrap_or(false);
    // 诊断开关：设置 MIPLAY_ROUTE_FORCE_MIX_FORMAT=1 可跳过自动转换、强制走混音格式
    // 回退路径（排障/真机验证用，正常播放不要设置）。
    let force_mix_fallback = std::env::var_os("MIPLAY_ROUTE_FORCE_MIX_FORMAT").is_some();

    // 路径一（首选）：WASAPI 自动转换到 `引擎采样率/2ch/s16`（采样率一致 → 零 SRC）。
    let mut init_error: Option<String> = None;
    if force_mix_fallback {
        init_error = Some("设置 MIPLAY_ROUTE_FORCE_MIX_FORMAT 强制回退".to_owned());
    } else if !rate_matches {
        init_error = Some(format!(
            "目标端点混音采样率 {} Hz 与引擎 {engine_rate} Hz 不一致",
            mix_info.samplerate
        ));
    } else {
        let format = PcmFormat::new(engine_rate, 2, 16);
        let wave_format = com::pcm16_waveformatex(format);
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        match unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                com::ms_to_hns(render_buffer_ms),
                0,
                &wave_format,
                None,
            )
        } {
            Ok(()) => {}
            Err(e) => {
                init_error = Some(format!(
                    "WASAPI 错误 (0x{:08X}): {}",
                    e.code().0 as u32,
                    e.message()
                ));
            }
        }
    }

    // 路径二：自动转换不可用时按端点自己的混音格式开流，数据在 Rust 侧转换。
    let mut output = match init_error {
        None => SinkOutput::AutoConvert { rate: engine_rate },
        Some(primary) => {
            // 失败后这个 client 的状态不可靠，重新 Activate 一个干净的再试。
            client = com::activate_client(&device.device)?;
            let mix = initialize_with_mix_format(&client, render_buffer_ms)
                .map_err(|fallback| AudioError::invalid(render_init_error_text(&primary, &fallback)))?;
            SinkOutput::Converted {
                mix,
                converter: MixConverter::new(mix, engine_rate),
            }
        }
    };

    let buffer_frames = unsafe { client.GetBufferSize() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBufferSize(route)"))? as usize;
    let render: IAudioRenderClient = unsafe { client.GetService() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetService(IAudioRenderClient, route)"))?;

    // 预填一点静音，避免 Start 后第一轮就欠载。帧数按实际输出采样率算。
    let bytes_per_frame = output.bytes_per_frame();
    let prefill = (RENDER_PREFILL_MS as usize * output.samplerate() as usize / 1000)
        .min(buffer_frames);
    if prefill > 0 {
        write_silence(&render, prefill, bytes_per_frame)?;
    }
    unsafe { client.Start() }.map_err(|e| AudioError::wasapi(e.code().0, "Start(route)"))?;

    // 从这里开始 start() 认为这条路由"已打开"；重开时清掉旧错误、丢弃陈旧积压。
    if let Ok(mut sink) = shared.sinks[index].lock() {
        sink.opened = true;
        sink.settled = true;
        sink.error = None;
        sink.retry_in_ms = 0;
        // 重开（设备回来）时不要吐出断线前积压的陈旧音频，否则会听到一段旧声音。
        sink.queue.clear();
        sink.format_path = Some(output.describe());
    }

    // 左右微调延迟：真正的延迟线（见 DelayLine 的注释——"跳过 N 帧"会被抖动队列的
    // 丢最旧吃回去，所以之前怎么调都没效果）。
    let mut delay = DelayLine::new();
    delay.set_frames(route.delay_frames_at(engine_rate));

    let result = (|| -> Result<()> {
        // 编码后的输出字节缓冲，循环里复用（不每块分配）。
        let mut bytes: Vec<u8> = Vec::with_capacity(render_chunk_frames(engine_rate) * 16);
        while !shared.stop.load(Ordering::Relaxed) {
            // 延迟可以在播放中实时调整（GUI 拖动即时生效，不用重启引擎）。
            if let Some(slot) = shared.delays_ms.get(index) {
                delay.set_frames(delay_frames_for_ms(
                    slot.load(Ordering::Relaxed),
                    engine_rate,
                ));
            }
            let padding = unsafe { client.GetCurrentPadding() }
                .map_err(|e| AudioError::wasapi(e.code().0, "GetCurrentPadding(route)"))?
                as usize;
            let available = buffer_frames.saturating_sub(padding);
            if available == 0 {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            let want = output.input_frames_for_available(available);
            if want == 0 {
                thread::sleep(Duration::from_millis(1));
                continue;
            }

            let mut mono: Vec<i16> = Vec::with_capacity(want);
            let mut starved = 0usize;
            let mut peak = 0.0f32;
            {
                let mut sink = shared.sinks[index]
                    .lock()
                    .map_err(|_| AudioError::invalid("route sink 状态锁被 poison"))?;
                for _ in 0..want {
                    let sample = match sink.queue.pop_front() {
                        Some(sample) => sample,
                        None => {
                            // 队列暂时空了：补静音，但延迟线照常前进（延迟量不因此漂移）。
                            starved += 1;
                            0
                        }
                    };
                    let out = delay.process(sample);
                    peak = peak.max((out as f32 / 32_768.0).abs());
                    // 单声道样本交给输出编码：原生路径填 L/R，回退路径按混音格式转换。
                    mono.push(out);
                }
                // 输出电平：本块峰值与旧值取大（旧值每块衰减 20%）。
                sink.level = peak.max(sink.level * LEVEL_DECAY).min(1.0);
                sink.delay_frames = delay.frames();
                if starved > 0 && shared.captured_frames.load(Ordering::Relaxed) > 0 {
                    // 采集已经在出数据、这一路却断供：计一次欠载。
                    sink.underruns += 1;
                }
            }

            let frames = match &mut output {
                SinkOutput::AutoConvert { .. } => encode_native_stereo(&mono, &mut bytes),
                SinkOutput::Converted { converter, .. } => converter.convert(&mono, &mut bytes),
            };
            if frames == 0 {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            if frames > available {
                return Err(AudioError::invalid(format!(
                    "输出转换后 {frames} 帧超过 render 缓冲剩余 {available} 帧"
                )));
            }
            write_bytes(&render, &bytes, frames)?;
            if let Ok(mut sink) = shared.sinks[index].lock() {
                sink.sent_bytes += bytes.len() as u64;
            }
        }
        Ok(())
    })();

    unsafe { client.Stop() }.map_err(|e| AudioError::wasapi(e.code().0, "Stop(route)"))?;
    result
}

/// 往 render 缓冲写 `frames` 帧静音（每帧 `bytes_per_frame` 字节）。
fn write_silence(render: &IAudioRenderClient, frames: usize, bytes_per_frame: usize) -> Result<()> {
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
    unsafe { std::ptr::write_bytes(ptr, 0, frames * bytes_per_frame) };
    unsafe { render.ReleaseBuffer(frames as u32, 0) }
        .map_err(|e| AudioError::wasapi(e.code().0, "ReleaseBuffer(silence)"))?;
    Ok(())
}

/// 把已编码好的输出格式字节写进 render 缓冲（`frames` 帧）。
fn write_bytes(render: &IAudioRenderClient, bytes: &[u8], frames: usize) -> Result<()> {
    if frames == 0 {
        return Ok(());
    }
    let ptr = unsafe { render.GetBuffer(frames as u32) }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetBuffer(route)"))?;
    if ptr.is_null() {
        return Err(AudioError::invalid(
            "IAudioRenderClient::GetBuffer 返回空指针",
        ));
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
    unsafe { render.ReleaseBuffer(frames as u32, 0) }
        .map_err(|e| AudioError::wasapi(e.code().0, "ReleaseBuffer(route)"))?;
    Ok(())
}

/// 原生路径：单声道样本直接交织成 `引擎采样率 2ch/s16` 字节，返回帧数。
///
/// 同一个 i16 直接复制成 L/R，避免 i16→f32→i16 的额外舍入。
fn encode_native_stereo(mono: &[i16], out: &mut Vec<u8>) -> usize {
    out.clear();
    out.reserve(mono.len() * FRAME_BYTES as usize);
    for &sample in mono {
        let bytes = sample.to_le_bytes();
        out.extend_from_slice(&bytes);
        out.extend_from_slice(&bytes);
    }
    mono.len()
}

// ---------------------------------------------------------------------------
// 采集线程：只开一条，把每个采集块拆进所有路由的队列
// ---------------------------------------------------------------------------

fn run_capture(shared: Arc<Shared>, config: RouteEngineConfig) {
    let mut consecutive_failures: u32 = 0;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let started = Instant::now();
        match capture_loop(&shared, &config) {
            // capture_loop 只在引擎 stop 时正常返回。
            Ok(()) => break,
            Err(error) => {
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                if started.elapsed() >= Duration::from_secs(1) {
                    consecutive_failures = 0;
                }
                if let Ok(mut slot) = shared.source_error.lock() {
                    *slot = Some(error.to_string());
                }
                if !is_retryable(&error) {
                    // 配置/格式类错误重试也没用：保持旧行为，停引擎让 GUI 报「采集故障」。
                    shared.stop.store(true, Ordering::Relaxed);
                    break;
                }
                consecutive_failures = consecutive_failures.saturating_add(1);
                shared
                    .source_retrying
                    .store(consecutive_failures as u64, Ordering::Relaxed);
                let delay_ms = retry_backoff_ms(consecutive_failures);
                if !sleep_unless_stopped(&shared, delay_ms) {
                    break;
                }
            }
        }
    }
    shared.source_retrying.store(0, Ordering::Relaxed);
}

fn capture_loop(shared: &Arc<Shared>, config: &RouteEngineConfig) -> Result<()> {
    let _com = ComGuard::new()?;
    let enumerator = com::enumerator()?;
    let selector = crate::sync::selector_for(&config.source_endpoint);
    let device = com::resolve(&enumerator, &selector, DataFlow::Render)?;
    if let Ok(mut slot) = shared.source_device.lock() {
        *slot = device.name.clone();
    }

    let client = com::activate_client(&device.device)?;
    let info = com::mix_format(&client)?;
    let format = CaptureFormat::from_info(&info)?;
    let buffer_ms = config.effective_capture_buffer_ms();

    // 注意：这里**绝不**加 `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`。
    //
    // 坑：回环采集 + AUTOCONVERTPCM 会把端点的原生混音格式（本机全部 render 端点都是
    // 48000 Hz）先转到请求的固定 44.1k，再让上层以为拿到的是 44.1k——相当于 Windows 在
    // 采集端就悄悄插入一次非整数比 SRC。采集源是**空闲物理输出**时（省电/设备切换导致
    // 包不连续），这个重采样器还会吐出咔哒/滋啦声。正确做法是：按端点原生混音格式原样
    // 开流（下面用 GetMixFormat + Initialize(仅 LOOPBACK 标志)），格式转换全部在
    // [`CaptureDsp`] 里 Rust 侧完成。
    let mix_ptr = unsafe { client.GetMixFormat() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetMixFormat(route capture)"))?;
    if mix_ptr.is_null() {
        return Err(AudioError::invalid(
            "GetMixFormat(route capture) 返回空指针",
        ));
    }
    let init = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            // 回环采集必须显式带 LOOPBACK，否则 GetService(IAudioCaptureClient) 会报
            // AUDCLNT_E_WRONG_ENDPOINT_TYPE (0x88890003)。除此之外只保留格式转换相关
            // 标志的默认值（0），不使用 AUTOCONVERTPCM。
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            com::ms_to_hns(buffer_ms),
            0,
            mix_ptr,
            None,
        )
    };
    // SAFETY: mix_ptr 由 GetMixFormat 分配，Initialize 已复制所需字段，这里释放。
    unsafe { CoTaskMemFree(Some(mix_ptr as *const c_void)) };
    init.map_err(|e| {
        AudioError::wasapi(
            e.code().0,
            format!(
                "Initialize(route capture 原生混音格式 {}，未加 AUTOCONVERTPCM)",
                format.describe()
            ),
        )
    })?;

    let capture: IAudioCaptureClient = unsafe { client.GetService() }
        .map_err(|e| AudioError::wasapi(e.code().0, "GetService(IAudioCaptureClient, route)"))?;
    unsafe { client.Start() }
        .map_err(|e| AudioError::wasapi(e.code().0, "Start(route capture)"))?;

    // 采集已经打开：清掉上一次失败的重试状态（音频又流动起来，GUI 的错误提示该消失）。
    if let Ok(mut slot) = shared.source_error.lock() {
        *slot = None;
    }
    shared.source_retrying.store(0, Ordering::Relaxed);

    let mut dsp = CaptureDsp::new(format, shared.rate, config.capture_gain);
    let result = (|| -> Result<()> {
        while !shared.stop.load(Ordering::Relaxed) {
            let pending = match unsafe { capture.GetNextPacketSize() } {
                Ok(pending) => pending,
                Err(e) => {
                    if shared.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    return Err(AudioError::wasapi(
                        e.code().0,
                        "GetNextPacketSize(route capture)",
                    ));
                }
            };
            if pending == 0 {
                thread::sleep(Duration::from_millis(CAPTURE_POLL_MS));
                continue;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            if let Err(e) = unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
            {
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                return Err(AudioError::wasapi(
                    e.code().0,
                    "GetBuffer(route capture)",
                ));
            }
            let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let byte_len = frames as usize * format.bytes_per_frame();
            let bytes: &[u8] = if silent || data.is_null() {
                // SILENT 标志表示缓冲内容未定义：只按帧数补零，不读内存。
                &[]
            } else {
                // SAFETY: GetBuffer 成功后 data 指向 byte_len 字节的有效缓冲，直到 ReleaseBuffer。
                unsafe { std::slice::from_raw_parts(data, byte_len) }
            };
            let processed = dsp.process(bytes, frames as usize, silent);
            let released = unsafe { capture.ReleaseBuffer(frames) };
            let (captured, peak_l, peak_r) = processed?;
            released.map_err(|e| AudioError::wasapi(e.code().0, "ReleaseBuffer(route capture)"))?;
            if captured == 0 {
                continue;
            }
            shared
                .captured_frames
                .fetch_add(captured as u64, Ordering::Relaxed);

            // 电平条：本包峰值与旧值取大（旧值每包衰减 20%，约 100 ms 保持）。
            let prev_l = f32::from_bits(shared.level_l.load(Ordering::Relaxed));
            let prev_r = f32::from_bits(shared.level_r.load(Ordering::Relaxed));
            shared.level_l.store(
                peak_l.max(prev_l * LEVEL_DECAY).min(1.0).to_bits(),
                Ordering::Relaxed,
            );
            shared.level_r.store(
                peak_r.max(prev_r * LEVEL_DECAY).min(1.0).to_bits(),
                Ordering::Relaxed,
            );

            // 按每条路由的通道模式 + 增益 +（可选）反相，把这一包拆进各自的队列。
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
                let invert = sink.invert;
                for index in 0..captured {
                    let mono = mix_sample(channel, dsp.left[index], dsp.right[index]);
                    let mut sample = apply_gain(mono, gain);
                    if invert {
                        sample = invert_sample(sample);
                    }
                    sink.push_sample(sample);
                }
            }
        }
        Ok(())
    })();

    unsafe { client.Stop() }.map_err(|e| AudioError::wasapi(e.code().0, "Stop(route capture)"))?;
    result
}

// ---------------------------------------------------------------------------
// 单元测试：只测纯逻辑，不打开任何真实设备
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::DeviceSelector;

    fn route(endpoint: &str, channel: RouteChannel) -> RouteConfig {
        RouteConfig::new(endpoint, channel)
    }

    /// 两条路由（左 / 右）的合法配置。
    fn config() -> RouteEngineConfig {
        RouteEngineConfig {
            source_endpoint: "立体声链虚拟声卡".to_owned(),
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

    /// 把 `(L, R)` 帧序列编码成 2ch/f32 交错 PCM（标称 -1.0..1.0）。
    fn pcm_f32_stereo(frames: &[(f32, f32)]) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames.len() * 8);
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
        assert_eq!(default.source_endpoint, "立体声链虚拟声卡");
        assert_eq!(default.capture_gain, 1.0);
        assert_eq!(default.render_buffer_ms, 40);
        assert_eq!(default.queue_ms, 40);
        // task-9 新增：采样率默认自动（跟随采集源混音格式）、采集缓冲默认 40 ms。
        assert_eq!(default.rate, None);
        assert_eq!(default.capture_buffer_ms, None);
        assert_eq!(default.effective_capture_buffer_ms(), 40);
        let route = RouteConfig::default();
        assert_eq!(route.channel, RouteChannel::Both);
        assert_eq!(route.gain, 1.0);
        assert_eq!(route.delay_ms, 0);
        assert!(!route.invert);
        assert!(route.enabled);
    }

    #[test]
    fn effective_engine_rate_follows_source_native_rate() {
        // 显式配置优先。
        assert_eq!(effective_engine_rate(Some(48_000), Some(44_100)), 48_000);
        // 自动：跟随采集源原生混音采样率（本机全部端点都是 48 kHz）→ 全程零 SRC。
        assert_eq!(effective_engine_rate(None, Some(48_000)), 48_000);
        // 探测不到时退回传统 44.1 kHz（兼容既有行为）。
        assert_eq!(effective_engine_rate(None, None), ROUTE_SAMPLE_RATE);
    }

    #[test]
    fn render_chunk_frames_scale_with_rate() {
        assert_eq!(render_chunk_frames(44_100), 441);
        assert_eq!(render_chunk_frames(48_000), 480);
        assert_eq!(render_chunk_frames(1_000), 10);
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
    fn capture_format_accepts_supported_mix_formats() {
        let f32_48k =
            CaptureFormat::from_info(&mix_info(48_000, 2, Some(SampleFormat::F32))).unwrap();
        assert_eq!(f32_48k.samplerate, 48_000);
        assert_eq!(f32_48k.channels, 2);
        assert_eq!(f32_48k.bytes_per_frame(), 8);
        assert_eq!(f32_48k.describe(), "48000 Hz / 2 ch / F32");
        // 单声道 + 16-bit 回环（某些虚拟声卡）也要能采集。
        assert!(CaptureFormat::from_info(&mix_info(44_100, 1, Some(SampleFormat::S16))).is_ok());
        // 24-bit 混音也能解码（S24 是 SampleFormat 支持的）。
        assert!(CaptureFormat::from_info(&mix_info(48_000, 2, Some(SampleFormat::S24))).is_ok());
    }

    #[test]
    fn capture_format_rejects_unsupported_mix_formats() {
        // 6 声道混音暂不支持（明确报错，不能按错误声道顺序静默写出）。
        assert!(CaptureFormat::from_info(&mix_info(48_000, 6, Some(SampleFormat::F32))).is_err());
        // 未知采样格式。
        assert!(CaptureFormat::from_info(&mix_info(48_000, 2, None)).is_err());
        // nBlockAlign 与声道 × 采样字节数不一致。
        let mut broken = mix_info(48_000, 2, Some(SampleFormat::F32));
        broken.block_align = 4;
        assert!(CaptureFormat::from_info(&broken).is_err());
        // 混音格式本身无效（channels/block_align 为 0）。
        let mut invalid = mix_info(48_000, 0, Some(SampleFormat::F32));
        invalid.block_align = 0;
        assert!(CaptureFormat::from_info(&invalid).is_err());
    }

    #[test]
    fn capture_dsp_f32_native_rate_uses_capture_gain_and_peaks() {
        // 采集源 48 kHz（本机所有端点的真实混音格式），引擎 48 kHz：零 SRC。
        let format =
            CaptureFormat::from_info(&mix_info(48_000, 2, Some(SampleFormat::F32))).unwrap();
        let mut dsp = CaptureDsp::new(format, 48_000, 2.0);
        assert!(!capture_resamples(&dsp));
        let pcm = pcm_f32_stereo(&[(0.5, -0.25), (1.0, -1.0)]);
        let (frames, peak_l, peak_r) = dsp.process(&pcm, 2, false).unwrap();
        assert_eq!(frames, 2);
        // 增益 2.0：0.5 → 1.0（i16 满幅），-0.25 → -0.5，-1.0 → 钳制到满幅。
        assert_eq!(dsp.left, vec![32_767, 32_767]);
        assert_eq!(dsp.right, vec![-16_384, -32_767]);
        assert!(peak_l > 0.9999, "{peak_l}");
        // 右声道第二帧是 -1.0 × 增益 2.0 = -2.0，被钳制到 -32767 → 峰值约 0.99997。
        assert!(peak_r > 0.9999, "{peak_r}");
    }

    #[test]
    fn capture_dsp_silent_packets_become_zeros() {
        let format =
            CaptureFormat::from_info(&mix_info(48_000, 2, Some(SampleFormat::F32))).unwrap();
        let mut dsp = CaptureDsp::new(format, 48_000, 1.0);
        // SILENT：bytes 传入空切片，必须按 frames 补零。
        let (frames, peak_l, peak_r) = dsp.process(&[], 3, true).unwrap();
        assert_eq!(frames, 3);
        assert_eq!(dsp.left, vec![0, 0, 0]);
        assert_eq!(dsp.right, vec![0, 0, 0]);
        assert_eq!(peak_l, 0.0);
        assert_eq!(peak_r, 0.0);
    }

    #[test]
    fn capture_dsp_s16_native_fast_path_is_bit_exact() {
        // 16-bit 整数混音 + 增益 1.0：直接搬 i16，不经过 f32 往返。
        let format =
            CaptureFormat::from_info(&mix_info(48_000, 2, Some(SampleFormat::S16))).unwrap();
        let mut dsp = CaptureDsp::new(format, 48_000, 1.0);
        let pcm = pcm16_stereo(&[(1_000, -2_000), (i16::MAX, i16::MIN)]);
        let (frames, _, _) = dsp.process(&pcm, 2, false).unwrap();
        assert_eq!(frames, 2);
        assert_eq!(dsp.left, vec![1_000, i16::MAX]);
        assert_eq!(dsp.right, vec![-2_000, i16::MIN]);
    }

    #[test]
    fn capture_dsp_upmixes_mono_to_both_channels() {
        let format =
            CaptureFormat::from_info(&mix_info(48_000, 1, Some(SampleFormat::S16))).unwrap();
        let mut dsp = CaptureDsp::new(format, 48_000, 1.0);
        let mut mono = Vec::new();
        for sample in [100i16, -200] {
            mono.extend_from_slice(&sample.to_le_bytes());
        }
        let (frames, _, _) = dsp.process(&mono, 2, false).unwrap();
        assert_eq!(frames, 2);
        assert_eq!(dsp.left, vec![100, -200]);
        assert_eq!(dsp.right, vec![100, -200]);
    }

    #[test]
    fn capture_dsp_resamples_only_when_rates_differ() {
        // 采集源 44.1 kHz，引擎配置 48 kHz（显式 rate 才会发生）：走唯一一次 SRC。
        let format =
            CaptureFormat::from_info(&mix_info(44_100, 2, Some(SampleFormat::F32))).unwrap();
        let mut dsp = CaptureDsp::new(format, 48_000, 1.0);
        assert!(capture_resamples(&dsp));
        let pcm = pcm_f32_stereo(&vec![(0.25, 0.25); 441]);
        let (frames, _, _) = dsp.process(&pcm, 441, false).unwrap();
        // 441 @44.1k → 480 @48k。
        assert!((470..=482).contains(&frames), "{frames}");
        assert_eq!(dsp.left.len(), frames);
        assert_eq!(dsp.right.len(), frames);
    }

    #[test]
    fn delay_milliseconds_convert_to_frames() {
        assert_eq!(delay_frames_for_ms(0, 44_100), 0);
        assert_eq!(delay_frames_for_ms(1, 44_100), 44);
        assert_eq!(delay_frames_for_ms(10, 44_100), 441);
        assert_eq!(delay_frames_for_ms(1_000, 44_100), 44_100);
        assert_eq!(delay_frames_for_ms(2_000, 44_100), 88_200);
        // 引擎采样率跟随采集源（48 kHz）时同样按 48k 换算。
        assert_eq!(delay_frames_for_ms(10, 48_000), 480);
        assert_eq!(delay_frames_for_ms(1_000, 48_000), 48_000);
        // 路由上的便捷方法走同一套换算（显式指定采样率）。
        let mut r = route("箱A", RouteChannel::Left);
        r.delay_ms = 500;
        assert_eq!(r.delay_frames_at(44_100), 22_050);
        assert_eq!(r.delay_frames_at(48_000), 24_000);
    }

    #[test]
    fn queue_capacity_scales_with_milliseconds() {
        assert_eq!(queue_capacity_samples(0, 44_100), 1);
        assert_eq!(queue_capacity_samples(10, 44_100), 441);
        assert_eq!(queue_capacity_samples(40, 44_100), 1_764);
        assert_eq!(queue_capacity_samples(500, 44_100), 22_050);
        // 48 kHz 引擎下 40 ms 队列容量按 48k 算。
        assert_eq!(queue_capacity_samples(40, 48_000), 1_920);
    }

    #[test]
    fn delay_line_passes_through_when_empty() {
        let mut delay = DelayLine::new();
        assert_eq!(delay.frames(), 0);
        assert_eq!(delay.process(123), 123);
        assert_eq!(delay.frames(), 0);
    }

    #[test]
    fn delay_line_shifts_signal_exactly() {
        let mut delay = DelayLine::new();
        delay.set_frames(3);
        let input = [1i16, 2, 3, 4, 5, 6];
        let out: Vec<i16> = input.iter().map(|sample| delay.process(*sample)).collect();
        // 先垫 3 帧静音，之后严格延迟 3 个样本。
        assert_eq!(out, vec![0, 0, 0, 1, 2, 3]);
        assert_eq!(delay.frames(), 3);
    }

    #[test]
    fn delay_line_resize_takes_effect_immediately() {
        let mut delay = DelayLine::new();
        delay.set_frames(2);
        assert_eq!(delay.process(7), 0);
        assert_eq!(delay.process(8), 0);
        assert_eq!(delay.process(9), 7);
        // 变短 = 立刻追上（丢掉最老的样本）。
        delay.set_frames(0);
        assert_eq!(delay.frames(), 0);
        assert_eq!(delay.process(10), 10);
        // 变长 = 立刻多等两帧。
        delay.set_frames(2);
        assert_eq!(delay.process(11), 0);
        assert_eq!(delay.process(12), 0);
        assert_eq!(delay.process(13), 11);
    }

    /// 回归测试：**抖动队列丢最旧不能把微调延迟吃掉**。
    ///
    /// 旧实现是「启动时跳过 N 帧」，而采集快于输出时队列会丢最旧样本，多出来的延迟
    /// 被吃回去，用户看到的「左右微调延迟没有效果」就是这个原因。现在延迟由独立的
    /// 延迟线承担，队列再丢也不影响延迟量。
    #[test]
    fn delay_line_survives_queue_drops() {
        let mut sink = SinkState::new(&route("箱A", RouteChannel::Left), 4);
        let mut delay = DelayLine::new();
        delay.set_frames(10);
        let mut out = Vec::new();
        for i in 0..30i16 {
            // 每轮塞 3 个采样、只取 1 个：队列必然打满并丢最旧（模拟采集快于输出）。
            for _ in 0..3 {
                sink.push_sample(100 + i);
            }
            let sample = sink.queue.pop_front().unwrap_or(0);
            out.push(delay.process(sample));
        }
        assert!(sink.dropped_samples > 0, "这个测试需要队列真的丢过样本");
        assert!(out[..10].iter().all(|value| *value == 0), "前 10 帧应是延迟线垫的静音");
        assert!(
            out[10..].iter().all(|value| *value >= 100),
            "延迟线之后的每一帧都必须是真实音频（丢最旧只发生在队列侧）"
        );
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
        assert_eq!(samples_to_ms(0, 44_100), 0.0);
        assert_eq!(samples_to_ms(44_100, 44_100), 1_000.0);
        assert!((samples_to_ms(441, 44_100) - 10.0).abs() < 1e-9);
        // 40 ms 队列 = 44100 下 1764 个单声道采样
        assert!((samples_to_ms(1_764, 44_100) - 40.0).abs() < 1e-9);
        // 48 kHz 引擎下 1920 个采样 = 40 ms
        assert!((samples_to_ms(1_920, 48_000) - 40.0).abs() < 1e-9);
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
    fn selector_for_treats_guid_shape_as_exact_endpoint_id() {
        // 零安装 D 模式：GUI 传端点 id（`{...}` 形状）时必须精确锁定，不能退化成
        // 名字子串——本机有两个同名 NVIDIA 端点，按名字匹配会撞名被拒。
        let id = "{0.0.0.00000000}.{fc933984-2634-48ec-855d-f51bbddfdd71}";
        assert!(
            matches!(crate::sync::selector_for(id), DeviceSelector::Id(found) if found == id),
            "id 形状应解析成 DeviceSelector::Id"
        );
    }

    #[test]
    fn selector_for_treats_plain_names_as_substring_match() {
        let name = "Xiaomi Sound-1695";
        assert!(
            matches!(crate::sync::selector_for(name), DeviceSelector::NameContains(found) if found == name),
            "普通名字应保留 DeviceSelector::NameContains 语义"
        );
    }

    #[test]
    fn validation_accepts_guid_shaped_source_endpoint() {
        let mut cfg = config();
        cfg.source_endpoint = "{0.0.0.00000000}.{6e6d7f6c-a61a-4fb2-9e4c-0fdef7303832}".to_owned();
        assert!(
            cfg.validate().is_ok(),
            "id 形状的采集源应通过校验，而不是被当成非法名字"
        );
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
        // 采集缓冲 40 + 预排队 40 + render 缓冲一半 20
        assert_eq!(config().software_latency_ms(), 40 + 40 + 20);
        // 采集缓冲可配后，延迟估计跟着变。
        let mut cfg = config();
        cfg.capture_buffer_ms = Some(100);
        assert_eq!(cfg.software_latency_ms(), 100 + 40 + 20);
    }

    #[test]
    fn validation_rejects_out_of_range_native_rate() {
        let mut too_low = config();
        too_low.rate = Some(0);
        assert!(too_low.validate().is_err());
        let mut too_high = config();
        too_high.rate = Some(192_001);
        assert!(too_high.validate().is_err());
        let mut bad_capture = config();
        bad_capture.capture_buffer_ms = Some(5);
        assert!(bad_capture.validate().is_err());
        // 合法值。
        let mut ok = config();
        ok.rate = Some(48_000);
        ok.capture_buffer_ms = Some(80);
        assert!(ok.validate().is_ok());
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
            degraded: false,
            source_retrying: 0,
            captured_frames: 44_100,
            frame_bytes: 176_400,
            uptime_seconds: 1.0,
            level_l: 0.5,
            level_r: 0.25,
            rate: 48_000,
            capture_buffer_ms: 40,
            routes: vec![
                RouteStats {
                    endpoint: "箱A".to_owned(),
                    channel: RouteChannel::Left,
                    sent_bytes: 176_400,
                    underruns: 0,
                    dropped_ms: 0.0,
                    queued_ms: 10.0,
                    delay_ms: 30.0,
                    level: 0.5,
                    format_path: Some(
                        "mix-format 软件转换（44100 Hz / 2 ch / 32-bit float）".to_owned(),
                    ),
                    error: None,
                    retry_count: 0,
                    retry_in_ms: 0,
                },
                RouteStats {
                    endpoint: "箱B".to_owned(),
                    channel: RouteChannel::Right,
                    sent_bytes: 0,
                    underruns: 2,
                    dropped_ms: 1.0,
                    queued_ms: 0.0,
                    delay_ms: 0.0,
                    level: 0.0,
                    format_path: None,
                    error: Some("设备已拔出".to_owned()),
                    retry_count: 2,
                    retry_in_ms: 4_000,
                },
            ],
        };
        let describe = stats.describe();
        assert!(describe.contains("箱A"), "{describe}");
        assert!(describe.contains("箱B"), "{describe}");
        assert!(describe.contains("设备已拔出"), "{describe}");
        assert!(describe.contains("44100 帧"), "{describe}");
        // 回退路径（混音格式 + 软件转换）要在 describe() 里能看到，便于 GUI 显示。
        assert!(describe.contains("mix-format"), "{describe}");
        // 重试中的路由要能看出来（GUI 日志/自检用）。
        assert!(describe.contains("重试中：第 2 次失败，4000 ms 后重试"), "{describe}");
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
        state.delay_frames = 441;
        state.opened = true;
        state.settled = true;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            captured_frames: AtomicU64::new(0),
            level_l: AtomicU32::new(0.5f32.to_bits()),
            level_r: AtomicU32::new(0.25f32.to_bits()),
            started: Instant::now(),
            source_device: Mutex::new("立体声链".to_owned()),
            source_error: Mutex::new(None),
            source_retrying: AtomicU64::new(0),
            sinks: vec![Mutex::new(state)],
            delays_ms: vec![AtomicU32::new(10)],
            rate: 48_000,
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
        assert!(!stats.degraded);
        assert_eq!(stats.source_device, "立体声链");
        assert_eq!(stats.routes.len(), 1);
        assert_eq!(stats.routes[0].endpoint, "箱A");
        assert_eq!(stats.routes[0].channel, RouteChannel::Left);
        assert_eq!(stats.routes[0].sent_bytes, 8);
        assert_eq!(stats.routes[0].underruns, 1);
        assert_eq!(stats.routes[0].retry_count, 0);
        assert_eq!(stats.routes[0].retry_in_ms, 0);
        assert_eq!(stats.rate, 48_000);
        assert_eq!(stats.capture_buffer_ms, 40);
        assert_eq!(stats.routes[0].queued_ms, samples_to_ms(4, stats.rate));
        assert_eq!(stats.routes[0].dropped_ms, samples_to_ms(1, stats.rate));
        assert!((stats.routes[0].level - 0.25).abs() < f32::EPSILON);
        assert!((stats.level_l - 0.5).abs() < f32::EPSILON);
        assert!((stats.level_r - 0.25).abs() < f32::EPSILON);
        assert_eq!(stats.uptime_seconds >= 0.0, true);
    }

    /// 造一个引擎内部状态（不启动线程），供 stats()/重试字段测试用。
    fn shared_with(states: Vec<SinkState>, stop: bool) -> Arc<Shared> {
        let delays_ms = states
            .iter()
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>();
        Arc::new(Shared {
            stop: AtomicBool::new(stop),
            captured_frames: AtomicU64::new(0),
            rate: 48_000,
            level_l: AtomicU32::new(0),
            level_r: AtomicU32::new(0),
            started: Instant::now(),
            source_device: Mutex::new(String::new()),
            source_error: Mutex::new(None),
            source_retrying: AtomicU64::new(0),
            sinks: states.into_iter().map(Mutex::new).collect(),
            delays_ms,
        })
    }

    /// 用现成的共享状态构造一个不启动线程的句柄（`stats()`/`alive()` 测试用）。
    fn handle_for(shared: Arc<Shared>) -> RouteEngineHandle {
        RouteEngineHandle {
            shared,
            threads: Vec::new(),
            config: RouteEngineConfig::default(),
        }
    }

    #[test]
    fn stopped_engine_reports_not_running() {
        let stats = handle_for(shared_with(Vec::new(), true)).stats();
        assert!(!stats.running);
        assert!(stats.degraded);
        assert_eq!(stats.captured_frames, 0);
        assert_eq!(stats.frame_bytes, 0);
        assert_eq!(stats.rate, 48_000);
        assert_eq!(stats.capture_buffer_ms, 40);
    }

    #[test]
    fn retry_backoff_sequence_is_exponential_and_capped() {
        assert_eq!(retry_backoff_ms(0), 1_000);
        assert_eq!(retry_backoff_ms(1), 1_000);
        assert_eq!(retry_backoff_ms(2), 2_000);
        assert_eq!(retry_backoff_ms(3), 4_000);
        assert_eq!(retry_backoff_ms(4), 8_000);
        assert_eq!(retry_backoff_ms(5), 16_000);
        assert_eq!(retry_backoff_ms(6), 30_000);
        assert_eq!(retry_backoff_ms(30), 30_000);
    }

    #[test]
    fn retryable_errors_are_classified() {
        assert!(is_retryable(&AudioError::DeviceNotFound("箱".to_owned())));
        assert!(is_retryable(&AudioError::NoDefaultDevice("没有".to_owned())));
        assert!(is_retryable(&AudioError::DeviceInvalidated));
        assert!(is_retryable(&AudioError::wasapi(
            0x8889_0004u32 as i32,
            "设备已失效"
        )));
        assert!(is_retryable(&AudioError::wasapi(
            0x8007_0005u32 as i32,
            "设备被占用"
        )));
        assert!(is_retryable(&AudioError::wasapi(
            0x8889_0010u32 as i32,
            "音频服务没跑"
        )));
        // 格式/参数类错误重试也不会变。
        assert!(!is_retryable(&AudioError::wasapi(
            0x8889_0003u32 as i32,
            "端点类型错"
        )));
        assert!(!is_retryable(&AudioError::invalid("参数不对")));
        assert!(!is_retryable(&AudioError::UnsupportedFormat(
            "6 声道".to_owned()
        )));
        assert!(!is_retryable(&AudioError::NotStarted));
    }

    #[test]
    fn count_pending_waits_for_first_conclusion() {
        let mut open = SinkState::new(&route("箱A", RouteChannel::Left), 4);
        open.opened = true;
        open.settled = true;
        let mut failed = SinkState::new(&route("箱B", RouteChannel::Right), 4);
        failed.settled = true;
        failed.error = Some("设备找不到".to_owned());
        let pending = SinkState::new(&route("箱C", RouteChannel::Both), 4);
        let shared = shared_with(vec![open, failed, pending], false);
        assert_eq!(count_pending(&shared), 1);
        if let Ok(mut sink) = shared.sinks[2].lock() {
            sink.settled = true;
        }
        assert_eq!(count_pending(&shared), 0);
    }

    #[test]
    fn one_failed_route_keeps_engine_alive_and_not_degraded() {
        let mut open = SinkState::new(&route("箱A", RouteChannel::Left), 4);
        open.opened = true;
        open.settled = true;
        let mut failed = SinkState::new(&route("箱B", RouteChannel::Right), 4);
        failed.settled = true;
        failed.error = Some("找不到匹配的设备：箱B".to_owned());
        failed.retry_count = 3;
        failed.retry_in_ms = 4_000;
        let handle = handle_for(shared_with(vec![open, failed], false));
        assert!(handle.alive(), "还有一条路由在跑，引擎不能算死");
        let stats = handle.stats();
        assert!(stats.running);
        assert!(!stats.degraded);
        assert_eq!(stats.routes.len(), 2);
        assert_eq!(stats.routes[1].retry_count, 3);
        assert_eq!(stats.routes[1].retry_in_ms, 4_000);
        let describe = stats.describe();
        assert!(describe.contains("箱B"), "{describe}");
        assert!(describe.contains("重试中"), "{describe}");
    }

    #[test]
    fn degraded_engine_when_no_route_can_open() {
        let mut failed = SinkState::new(&route("箱A", RouteChannel::Left), 4);
        failed.settled = true;
        failed.error = Some("设备休眠".to_owned());
        failed.retry_count = 1;
        failed.retry_in_ms = 1_000;
        let handle = handle_for(shared_with(vec![failed], false));
        assert!(handle.alive(), "后台重试期间引擎仍算 running");
        let stats = handle.stats();
        assert!(stats.running);
        assert!(stats.degraded);
        let describe = stats.describe();
        assert!(describe.contains("降级"), "{describe}");
    }

    #[test]
    fn engine_stats_describe_mentions_source_retry() {
        let shared = shared_with(Vec::new(), false);
        shared.source_retrying.store(2, Ordering::Relaxed);
        let mut stats = handle_for(shared).stats();
        stats.degraded = true;
        let describe = stats.describe();
        assert!(describe.contains("采集重试中：第 2 次"), "{describe}");
        assert!(describe.contains("降级"), "{describe}");
    }

    // ---- 回退路径（混音格式 + 软件转换）的纯逻辑测试 ----

    /// 造一个混音格式（回退路径的目标）。
    fn mix_info(
        samplerate: u32,
        channels: u16,
        sample_format: Option<SampleFormat>,
    ) -> FormatInfo {
        let bytes = sample_format.map(SampleFormat::bytes_per_sample).unwrap_or(2);
        FormatInfo {
            sample_format,
            channels,
            samplerate,
            bits: (bytes * 8) as u16,
            block_align: channels * bytes as u16,
            avg_bytes_per_sec: samplerate * channels as u32 * bytes as u32,
            raw_format_tag: match sample_format {
                Some(SampleFormat::F32) => 3,
                _ => 1,
            },
            is_extensible: false,
        }
    }

    fn mix_output(samplerate: u32, channels: u16, sample_format: SampleFormat) -> MixOutputFormat {
        MixOutputFormat {
            samplerate,
            channels,
            sample_format,
        }
    }

    #[test]
    fn native_stereo_encoding_duplicates_mono_sample() {
        let mut out = Vec::new();
        let frames = encode_native_stereo(&[0x1234i16, -2], &mut out);
        assert_eq!(frames, 2);
        assert_eq!(out.len(), 8);
        // 小端、同一个样本同时填 L/R。
        assert_eq!(&out[0..4], &[0x34, 0x12, 0x34, 0x12]);
        assert_eq!(&out[4..8], &[0xFE, 0xFF, 0xFE, 0xFF]);
        out.clear();
        assert_eq!(encode_native_stereo(&[], &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn mix_output_format_accepts_convertible_mix_formats() {
        let float_44k =
            MixOutputFormat::from_info(&mix_info(44_100, 2, Some(SampleFormat::F32))).unwrap();
        assert_eq!(float_44k.describe(), "44100 Hz / 2 ch / 32-bit float");
        assert_eq!(float_44k.bytes_per_frame(), 8);

        let int_48k =
            MixOutputFormat::from_info(&mix_info(48_000, 2, Some(SampleFormat::S16))).unwrap();
        assert_eq!(int_48k.describe(), "48000 Hz / 2 ch / 16-bit int");
        assert_eq!(int_48k.bytes_per_frame(), 4);

        let mono_48k =
            MixOutputFormat::from_info(&mix_info(48_000, 1, Some(SampleFormat::S32))).unwrap();
        assert_eq!(mono_48k.bytes_per_frame(), 4);

        // 44.1k 混音（正是 AUTOCONVERTPCM 失败的那种端点）也必须能回退。
        assert!(MixOutputFormat::from_info(&mix_info(44_100, 2, Some(SampleFormat::F32))).is_ok());
    }

    #[test]
    fn mix_output_format_rejects_unsupported_mix_formats() {
        // 24-bit 不在软件转换支持范围：必须明确报错，不能静默写错格式。
        let err = MixOutputFormat::from_info(&mix_info(48_000, 2, Some(SampleFormat::S24)));
        assert!(err.is_err());

        // 多声道（6 ch）暂不支持。
        assert!(MixOutputFormat::from_info(&mix_info(48_000, 6, Some(SampleFormat::F32))).is_err());

        // 未知采样格式。
        assert!(MixOutputFormat::from_info(&mix_info(48_000, 2, None)).is_err());

        // nBlockAlign 与声道 × 采样字节数不一致。
        let mut broken = mix_info(48_000, 2, Some(SampleFormat::F32));
        broken.block_align = 4;
        assert!(MixOutputFormat::from_info(&broken).is_err());

        // 混音格式本身无效（channels/block_align 为 0）。
        let mut invalid = mix_info(48_000, 0, Some(SampleFormat::F32));
        invalid.block_align = 0;
        assert!(MixOutputFormat::from_info(&invalid).is_err());
    }

    #[test]
    fn converter_passthrough_44100_s16_duplicates_channels() {
        let mix = mix_output(44_100, 2, SampleFormat::S16);
        let mut converter = MixConverter::new(mix, 44_100);
        let mut out = Vec::new();
        let frames = converter.convert(&[1_000, -1_000, 0], &mut out);
        assert_eq!(frames, 3);
        assert_eq!(out.len(), 3 * 4);
        // 左右声道都是同一个样本（单声道复制到双声道）。
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 1_000);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 1_000);
        assert_eq!(i16::from_le_bytes([out[4], out[5]]), -1_000);
        assert_eq!(i16::from_le_bytes([out[6], out[7]]), -1_000);
    }

    #[test]
    fn converter_resamples_to_48000_f32() {
        let mix = mix_output(48_000, 2, SampleFormat::F32);
        let mut converter = MixConverter::new(mix, 44_100);
        let mut out = Vec::new();
        let frames = converter.convert(&vec![1_000i16; render_chunk_frames(44_100)], &mut out);
        // 44100 → 48000：441 帧大约变 480 帧（流式重采样可能有 1 帧出入）。
        assert!((470..=482).contains(&frames), "{frames}");
        assert_eq!(out.len(), frames * 8);
        // 每帧 2 个 f32，左右相同，且是正数。
        let left = f32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        let right = f32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        assert!(left > 0.0, "{left}");
        assert_eq!(left, right);
    }

    #[test]
    fn converter_handles_mono_mix_format() {
        let mix = mix_output(48_000, 1, SampleFormat::F32);
        let mut converter = MixConverter::new(mix, 44_100);
        let mut out = Vec::new();
        let frames = converter.convert(&vec![-2_000i16; render_chunk_frames(44_100)], &mut out);
        assert!((470..=482).contains(&frames), "{frames}");
        assert_eq!(out.len(), frames * 4);
        let first = f32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        assert!(first < 0.0, "{first}");
    }

    #[test]
    fn converter_s32_frames_are_four_bytes() {
        let mix = mix_output(44_100, 2, SampleFormat::S32);
        let mut converter = MixConverter::new(mix, 44_100);
        let mut out = Vec::new();
        let frames = converter.convert(&[1_000, 0, -1_000], &mut out);
        assert_eq!(frames, 3);
        assert_eq!(out.len(), 3 * 8);
        let left = i32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        let right = i32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        assert!(left > 0, "{left}");
        assert_eq!(left, right);
    }

    #[test]
    fn input_frame_limit_never_exceeds_available() {
        // 原生路径（采集源原生 48k）：最多 10 ms 一块 = 480 帧，且随采样率变化。
        assert_eq!(
            SinkOutput::AutoConvert { rate: 48_000 }.input_frames_for_available(10_000),
            480
        );
        assert_eq!(
            SinkOutput::AutoConvert { rate: 44_100 }.input_frames_for_available(10_000),
            441
        );
        assert_eq!(SinkOutput::AutoConvert { rate: 48_000 }.input_frames_for_available(0), 0);
        assert_eq!(
            SinkOutput::AutoConvert { rate: 48_000 }.input_frames_for_available(100),
            100
        );

        // 回退路径：引擎 44.1k + 混音 48k。
        let mix = mix_output(48_000, 2, SampleFormat::F32);
        let mut converter = MixConverter::new(mix, 44_100);
        for available in [100usize, render_chunk_frames(44_100), 480, 960] {
            let want = converter.input_frames_for_available(available);
            assert!(want > 0, "available={available}");
            let mut out = Vec::new();
            let frames = converter.convert(&vec![1_000i16; want], &mut out);
            // 转换后绝不能超过 render 缓冲剩余帧数。
            assert!(frames <= available, "available={available} frames={frames}");
        }
        // 真正的 sink 侧入口还要再夹到 10 ms 一块，防止一块吃太多造成延迟抖动。
        let converted = SinkOutput::Converted {
            mix,
            converter: MixConverter::new(mix, 44_100),
        };
        assert!(converted.input_frames_for_available(960) <= render_chunk_frames(44_100));
    }

    #[test]
    fn converter_passthrough_48000_s16_is_bit_exact() {
        // 48k 引擎 + 16-bit 混音（零 SRC）：直接复制 i16，不能有 ±1 LSB 误差。
        let mix = mix_output(48_000, 2, SampleFormat::S16);
        let mut converter = MixConverter::new(mix, 48_000);
        let mut out = Vec::new();
        let frames = converter.convert(&[1_000, -2_000, i16::MAX], &mut out);
        assert_eq!(frames, 3);
        assert_eq!(out.len(), 3 * 4);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 1_000);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 1_000);
        assert_eq!(i16::from_le_bytes([out[4], out[5]]), -2_000);
        assert_eq!(i16::from_le_bytes([out[8], out[9]]), i16::MAX);
    }

    #[test]
    fn sink_output_describe_mentions_actual_path() {
        let native = SinkOutput::AutoConvert { rate: 48_000 };
        let describe = native.describe();
        assert!(describe.contains("wasapi-autoconvert"), "{describe}");
        assert!(describe.contains("48000 Hz"), "{describe}");
        assert_eq!(native.samplerate(), 48_000);
        assert_eq!(native.bytes_per_frame(), 4);

        let mix = mix_output(44_100, 2, SampleFormat::F32);
        let converted = SinkOutput::Converted {
            mix,
            converter: MixConverter::new(mix, 44_100),
        };
        let describe = converted.describe();
        assert!(describe.contains("mix-format"), "{describe}");
        assert!(describe.contains("44100 Hz"), "{describe}");
        assert_eq!(converted.bytes_per_frame(), 8);
        assert_eq!(converted.samplerate(), 44_100);
    }

    #[test]
    fn fallback_initialize_error_mentions_both_paths() {
        // 这条纯逻辑不碰设备：只校验失败时的错误文案同时包含两条路径的关键字。
        let primary = AudioError::wasapi(0x88890008u32 as i32, "Initialize(route render)");
        let fallback = AudioError::invalid("回退也失败");
        let text = render_init_error_text(
            &format!("WASAPI 错误 (0x{:08X})", primary_hr(&primary)),
            &fallback,
        );
        assert!(text.contains("0x88890008"), "{text}");
        assert!(text.contains("自动转换"), "{text}");
        assert!(text.contains("回退混音格式"), "{text}");
    }

    #[test]
    fn invert_flips_and_saturates() {
        assert_eq!(invert_sample(0), 0);
        assert_eq!(invert_sample(1_000), -1_000);
        assert_eq!(invert_sample(-1_000), 1_000);
        // -32768 取反会溢出：饱和到 +32767。
        assert_eq!(invert_sample(i16::MIN), i16::MAX);
        assert_eq!(invert_sample(i16::MAX), -i16::MAX);
        // describe 里能看到反相开关。
        let mut route = route("箱A", RouteChannel::Left);
        route.invert = true;
        assert!(route.describe().contains("反相"), "{}", route.describe());
    }

    /// 取错误里记录的 HRESULT（测试辅助）。
    fn primary_hr(error: &AudioError) -> u32 {
        match error {
            AudioError::Wasapi(hr, _) => *hr as u32,
            _ => 0,
        }
    }
}
