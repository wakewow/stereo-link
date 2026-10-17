//! # miplay-airplay
//!
//! AirPlay 2 **发送端**：把 44.1 kHz / 2 声道 / `s16le` 交错的 PCM 实时推给局域网里的
//! AirPlay 2 接收端（本项目里是 `192.168.1.21`，Xiaomi Sound-左 / L16A）。
//!
//! ## 为什么是这条路
//!
//! 现状（小米云 `player_play_url` + 音箱回拉本机 HTTP 流）的端到端延迟约 1~2 秒，因为链路上有
//! HTTPS 往返、云端下发、音箱自行预缓冲三段不可控的开销。真机探测（`research/lan/lan-paths.md`）
//! 证明 L16A **自己就是原生 AirPlay 2 接收端**（`Server: AirTunes/366.0`、`sdk=AirPlay;2.0.10`），
//! 于是可以完全绕开小米云：本机直连 7000 端口推流，延迟由上层的声明值决定（设备接受窗口
//! 250 ms ~ 2 s）。
//!
//! ## 用法
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use miplay_airplay::{AirPlaySender, SenderConfig};
//!
//! let addr = "192.168.1.21:7000".parse()?;
//! let mut sender = AirPlaySender::connect(addr, SenderConfig::default()).await?;
//! sender.set_volume(0.3).await?;
//! // 采集端按实时速率喂 20 ms 一块的 s16le 立体声 PCM
//! sender.send_pcm(&[0u8; 20 * 176_400 / 1000]).await?;
//! sender.stop().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## 线程与背压
//!
//! 协议栈是同步阻塞 I/O，所以会话跑在一条专用线程上，本类型只是它的异步句柄。
//! [`AirPlaySender::send_pcm`] 走**有界**通道：采集端喂得比实时快时会被反压，不会被无限缓冲成延迟。
//! 我们这一侧的排队上限是 120 ms（超出丢最老的音频，计入 [`SenderStats::dropped_pcm_bytes`]）。
//! 若采集端刚好按实时速率喂，稳态排队深度约为一个 RTP 包（8 ms）。
//!
//! ## 许可证
//!
//! 依赖的 [`cidermill_airplay`] 是 **GPL-3.0-or-later**，见 `README.md`「许可证」一节；
//! 协议后端集中在 `src/worker.rs` 一个文件里，替换实现只影响该文件。

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod latency;
pub mod pcm;
mod worker;

pub use latency::{
    LatencyBudget, DEVICE_LATENCY_MAX_FRAMES, DEVICE_LATENCY_MAX_MS, DEVICE_LATENCY_MIN_FRAMES,
    DEVICE_LATENCY_MIN_MS, PACKET_FRAMES, TARGET_TOTAL_MS,
};
pub use pcm::{apply_gain_s16le, PacketChunker, PcmFormat, MAX_GAIN};

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

/// 命令通道容量（块）。8 块 × 调用方块大小，配合 [`MAX_QUEUED_MS`] 一起限制我们这一侧的排队。
const QUEUE_CHUNKS: usize = 8;

/// 我们这一侧允许的最大排队音频（毫秒）：超出就丢最老的音频，宁可咔一下也不让延迟无界增长。
pub(crate) const MAX_QUEUED_MS: usize = 120;

/// 发送提前量（包）。
///
/// Windows 的默认定时器精度是 15.6 ms，而一个 RTP 包只有 7.98 ms——按「到期才发」会变成
/// 每 15.6 ms 突发送两个包。提前两个包（≈16 ms）发送可以抹平这个抖动，而**不增加延迟**：
/// 接收端仍然按 RTP 时间戳在 `declared` 之后渲染，提前到的包只是被它缓冲。
pub(crate) const SEND_AHEAD_PACKETS: u64 = 2;

/// 连接（含 HAP 配对、SETUP、RECORD）的总超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 音频编码方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// ALAC：与 pyatv 在 L16A 上实测成功的路径（默认）。
    #[default]
    Alac,
    /// 裸 LPCM（`L16`）：省掉编码器与编码延迟，局域网带宽约 1.4 Mbit/s。
    Lpcm,
}

impl Codec {
    /// 名字，用于日志/报告。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alac => "alac",
            Self::Lpcm => "lpcm",
        }
    }

    pub(crate) fn stream_format(self) -> cidermill_airplay::audio::StreamFormat {
        match self {
            Self::Alac => cidermill_airplay::audio::StreamFormat::ALAC_44100_16_STEREO,
            Self::Lpcm => cidermill_airplay::audio::StreamFormat::PCM_44100_16_STEREO,
        }
    }
}

/// 本 crate 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 协议栈错误（配对、RTSP、plist、加密……）。
    #[error("AirPlay 协议错误：{0}")]
    AirPlay(#[from] cidermill_airplay::AirPlayError),
    /// 套接字/线程错误。
    #[error("网络或 IO 错误：{0}")]
    Io(#[from] std::io::Error),
    /// 发送线程已退出（正常收尾或异常终止）。
    #[error("发送线程已退出")]
    WorkerGone,
    /// 配置无效。
    #[error("配置无效：{0}")]
    Config(String),
    /// 会话建立或运行失败。
    #[error("会话失败：{0}")]
    Session(String),
    /// 连接超时。
    #[error("连接超时：{0:?} 内未进入推流状态")]
    ConnectTimeout(Duration),
}

/// 本 crate 的结果类型。
pub type Result<T> = std::result::Result<T, Error>;

/// 发送端配置。
///
/// > 构造请用 [`SenderConfig::default`] / [`SenderConfig::new`] 加链式方法，
/// > 这样以后新增字段不会破坏调用方。
#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// 同步包里声明的延迟（帧，44.1 kHz）。窗口 [`DEVICE_LATENCY_MIN_FRAMES`]~[`DEVICE_LATENCY_MAX_FRAMES`]；
    /// 默认取窗口下限 = 250 ms。
    pub latency_frames: u32,
    /// 设备名（写进 RTSP/配对头，便于在抓包里认出来）。
    pub device_name: String,
    /// 音频编码方式。
    pub codec: Codec,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            latency_frames: DEVICE_LATENCY_MIN_FRAMES,
            device_name: "miplay-bridge".to_string(),
            codec: Codec::default(),
        }
    }
}

impl SenderConfig {
    /// 默认配置 + 指定设备名。
    #[must_use]
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            device_name: device_name.into(),
            ..Self::default()
        }
    }

    /// 声明延迟（帧）。
    #[must_use]
    pub const fn with_latency_frames(mut self, frames: u32) -> Self {
        self.latency_frames = frames;
        self
    }

    /// 声明延迟（毫秒，按 44.1 kHz 换算）。
    #[must_use]
    pub fn with_latency_ms(mut self, ms: u32) -> Self {
        self.latency_frames = latency::ms_to_frames(ms, latency::SAMPLE_RATE);
        self
    }

    /// 编码方式。
    #[must_use]
    pub const fn with_codec(mut self, codec: Codec) -> Self {
        self.codec = codec;
        self
    }

    /// 声明延迟对应的时长。
    #[must_use]
    pub fn declared_latency(&self) -> Duration {
        Duration::from_secs_f64(latency::frames_to_ms(
            self.latency_frames,
            latency::SAMPLE_RATE,
        ) / 1000.0)
    }

    /// 校验配置；越界直接报错而不是悄悄夹紧（延迟是产品指标，不该被静默改掉）。
    pub fn validate(&self) -> Result<()> {
        if self.device_name.trim().is_empty() {
            return Err(Error::Config("device_name 不能为空".to_string()));
        }
        if !(DEVICE_LATENCY_MIN_FRAMES..=DEVICE_LATENCY_MAX_FRAMES).contains(&self.latency_frames) {
            return Err(Error::Config(format!(
                "latency_frames={} 超出设备窗口 {}~{} 帧（{}~{} ms）",
                self.latency_frames,
                DEVICE_LATENCY_MIN_FRAMES,
                DEVICE_LATENCY_MAX_FRAMES,
                DEVICE_LATENCY_MIN_MS,
                DEVICE_LATENCY_MAX_MS,
            )));
        }
        Ok(())
    }
}

/// 运行期计数器快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SenderStats {
    /// 已发出的 RTP 音频包数。
    pub packets_sent: u64,
    /// 已发出的音频帧数。
    pub frames_sent: u64,
    /// 已发出的音频负载字节数（RTP 负载，不含头）。
    pub bytes_sent: u64,
    /// 已发出的同步包数（每秒一个）。
    pub syncs_sent: u64,
    /// 欠载包数（采集端没跟上，用静音补齐的包）。
    pub underrun_packets: u64,
    /// 因超出排队上限而丢弃的 PCM 字节数。
    pub dropped_pcm_bytes: u64,
    /// 当前排队字节数。
    pub queued_bytes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Counters {
    packets: AtomicU64,
    frames: AtomicU64,
    bytes: AtomicU64,
    syncs: AtomicU64,
    underruns: AtomicU64,
    dropped: AtomicU64,
    queued: AtomicU64,
}

impl Counters {
    pub(crate) fn add(&self, field: Counter, delta: u64) {
        self.field(field).fetch_add(delta, Ordering::Relaxed);
    }

    pub(crate) fn set(&self, field: Counter, value: u64) {
        self.field(field).store(value, Ordering::Relaxed);
    }

    pub(crate) fn value(&self, field: Counter) -> u64 {
        self.field(field).load(Ordering::Relaxed)
    }

    fn field(&self, field: Counter) -> &AtomicU64 {
        match field {
            Counter::Packets => &self.packets,
            Counter::Frames => &self.frames,
            Counter::Bytes => &self.bytes,
            Counter::Syncs => &self.syncs,
            Counter::Underruns => &self.underruns,
            Counter::Dropped => &self.dropped,
            Counter::Queued => &self.queued,
        }
    }

    fn snapshot(&self) -> SenderStats {
        let get = |f: Counter| self.field(f).load(Ordering::Relaxed);
        SenderStats {
            packets_sent: get(Counter::Packets),
            frames_sent: get(Counter::Frames),
            bytes_sent: get(Counter::Bytes),
            syncs_sent: get(Counter::Syncs),
            underrun_packets: get(Counter::Underruns),
            dropped_pcm_bytes: get(Counter::Dropped),
            queued_bytes: get(Counter::Queued),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counter {
    Packets,
    Frames,
    Bytes,
    Syncs,
    Underruns,
    Dropped,
    Queued,
}

/// 会话建立过程中从设备拿到的客观信息（用于报告与延迟预算）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    /// 设备给音频 RTP 的 UDP 端口。
    pub data_port: Option<u16>,
    /// 设备给控制 RTP 的 UDP 端口。
    pub control_port: Option<u16>,
    /// 设备分配的流 id。
    pub stream_id: Option<u64>,
    /// 设备上报的音频缓冲大小（字节或包，原样保留）。
    pub audio_buffer_size: Option<u64>,
    /// **设备上报的「到达→渲染」延迟（毫秒）**，延迟预算里最有用的一项。
    pub arrival_to_render_latency_ms: Option<u64>,
    /// 依次成功的关键 RTSP 步骤，形如 `"Setup -> 200 OK"`。
    pub steps: Vec<String>,
}

/// 发送端状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// 正在配对/建流。
    Connecting,
    /// 已 `RECORD`，正在推流。
    Streaming,
    /// 已停止（含已发过 TEARDOWN）。
    Stopped,
    /// 失败，附原因。
    Failed(String),
}

/// AirPlay 2 发送端（异步句柄）。
#[derive(Debug)]
pub struct AirPlaySender {
    addr: SocketAddr,
    config: SenderConfig,
    cmd: mpsc::Sender<worker::Command>,
    status: watch::Receiver<Status>,
    counters: Arc<Counters>,
    info: SessionInfo,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AirPlaySender {
    /// 连接设备并进入推流状态。
    ///
    /// 返回 `Ok` 意味着 HAP 配对、`SETUP`、`RECORD` 都已成功（设备已准备好收 RTP），
    /// 而不是仅仅 TCP 连上了。
    ///
    /// # Errors
    /// 配置越界、配对/建流被拒、超时等都会返回 [`Error`]。
    pub async fn connect(addr: SocketAddr, cfg: SenderConfig) -> Result<Self> {
        cfg.validate()?;
        let (cmd_tx, cmd_rx) = mpsc::channel::<worker::Command>(QUEUE_CHUNKS);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<SessionInfo>>();
        let (status_tx, status) = watch::channel(Status::Connecting);
        let counters = Arc::new(Counters::default());

        let cfg_for_thread = cfg.clone();
        let counters_for_thread = Arc::clone(&counters);
        let join = std::thread::Builder::new()
            .name("miplay-airplay".to_string())
            .spawn(move || {
                worker::run(
                    addr,
                    cfg_for_thread,
                    cmd_rx,
                    ready_tx,
                    status_tx,
                    counters_for_thread,
                );
            })?;

        let info = match tokio::time::timeout(CONNECT_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(info))) => info,
            Ok(Ok(Err(err))) => return Err(err),
            // 发送端在报 ready 之前就退出了（线程 panic 或通道被丢）
            Ok(Err(_)) => return Err(Error::WorkerGone),
            Err(_) => return Err(Error::ConnectTimeout(CONNECT_TIMEOUT)),
        };

        Ok(Self {
            addr,
            config: cfg,
            cmd: cmd_tx,
            status,
            counters,
            info,
            join: Some(join),
        })
    }

    /// 推一块 PCM（44.1 kHz / 2 声道 / `s16le` 交错，长度任意，帧对齐即可）。
    ///
    /// 内部走有界通道：喂得比实时快会被反压（这是特性，不是缺陷——它保证延迟不随时长增长）。
    ///
    /// # Errors
    /// 发送线程已退出时返回 [`Error::WorkerGone`]。
    pub async fn send_pcm(&mut self, pcm: &[u8]) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        self.cmd
            .send(worker::Command::Pcm(pcm.to_vec()))
            .await
            .map_err(|_| Error::WorkerGone)
    }

    /// 设置音量，`v` 为 0.0~1.0 的线性值。
    ///
    /// 映射到 AirPlay 的 `volume:` 参数（dB，上限 0 dB）：`30·log10(v)`，`v = 0` 视为静音（-144 dB）。
    ///
    /// # Errors
    /// `v` 非有限值或不在 `0.0..=1.0` 时返回 [`Error::Config`]。
    pub async fn set_volume(&mut self, v: f32) -> Result<()> {
        let db = linear_to_db(v)?;
        self.cmd
            .send(worker::Command::Volume(db))
            .await
            .map_err(|_| Error::WorkerGone)
    }

    /// 停止推流：发 TEARDOWN，等线程收尾。
    ///
    /// # Errors
    /// 目前总是 `Ok`；保留 `Result` 以便将来上报收尾失败。
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.cmd.send(worker::Command::Stop).await;
        if let Some(join) = self.join.take() {
            // 阻塞式 join 放到阻塞线程池，别占着 async 运行时
            let _ = tokio::task::spawn_blocking(move || join.join()).await;
        }
        Ok(())
    }

    /// 设备地址。
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 生效的配置。
    #[must_use]
    pub const fn config(&self) -> &SenderConfig {
        &self.config
    }

    /// 建流时从设备拿到的客观信息。
    #[must_use]
    pub const fn session_info(&self) -> &SessionInfo {
        &self.info
    }

    /// 当前状态。
    #[must_use]
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// 计数器快照。
    #[must_use]
    pub fn stats(&self) -> SenderStats {
        self.counters.snapshot()
    }

    /// 当前延迟预算（排队深度取运行期实测值，设备渲染延迟取 `SETUP` 上报值）。
    #[must_use]
    pub fn latency_budget(&self) -> LatencyBudget {
        let queued = self.counters.snapshot().queued_bytes;
        LatencyBudget::new(
            self.config.latency_frames,
            PACKET_FRAMES,
            PcmFormat::S16LE_44100_STEREO.ms_for_bytes(queued as usize),
            SEND_AHEAD_PACKETS,
            self.info
                .arrival_to_render_latency_ms
                .map(|ms| ms as f64),
        )
    }
}

impl Drop for AirPlaySender {
    fn drop(&mut self) {
        // 尽力而为：让 worker 收到 Stop 以便发 TEARDOWN。
        // 队列满或已断开也没关系——通道一关，worker 同样会走收尾路径。
        let _ = self.cmd.try_send(worker::Command::Stop);
    }
}

/// 线性音量（0.0~1.0）→ AirPlay 的 `volume:` 参数（dB）。
///
/// # Errors
/// `v` 非有限值或不在 `0.0..=1.0` 时返回 [`Error::Config`]。
pub fn linear_to_db(v: f32) -> Result<f64> {
    if !v.is_finite() || !(0.0..=1.0).contains(&v) {
        return Err(Error::Config(format!("音量必须在 0.0~1.0，收到 {v}")));
    }
    if v <= 0.0 {
        return Ok(-144.0);
    }
    Ok((30.0 * f64::from(v).log10()).min(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_the_window_floor() {
        let cfg = SenderConfig::default();
        assert_eq!(cfg.latency_frames, DEVICE_LATENCY_MIN_FRAMES);
        assert_eq!(cfg.declared_latency(), Duration::from_millis(250));
        assert_eq!(cfg.codec, Codec::Alac);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn latency_ms_builder_matches_frames() {
        let cfg = SenderConfig::default().with_latency_ms(400);
        assert_eq!(cfg.latency_frames, 17_640);
        assert_eq!(cfg.declared_latency(), Duration::from_millis(400));
    }

    #[test]
    fn out_of_window_latency_is_rejected_not_clamped() {
        let low = SenderConfig::default().with_latency_frames(11_024);
        assert!(matches!(low.validate(), Err(Error::Config(_))));
        let high = SenderConfig::default().with_latency_frames(88_201);
        assert!(matches!(high.validate(), Err(Error::Config(_))));
        assert!(SenderConfig::default()
            .with_latency_frames(DEVICE_LATENCY_MAX_FRAMES)
            .validate()
            .is_ok());
    }

    #[test]
    fn empty_device_name_is_rejected() {
        let cfg = SenderConfig::new("   ");
        assert!(matches!(cfg.validate(), Err(Error::Config(_))));
    }

    #[test]
    fn volume_maps_logarithmically_and_clamps_at_zero_db() {
        assert_eq!(linear_to_db(1.0).unwrap(), 0.0);
        assert!((linear_to_db(0.5).unwrap() + 9.0309).abs() < 0.001);
        // 0.1f32 不是精确的 0.1（转 f64 后是 0.10000000149…），所以只能容差比较。
        assert!((linear_to_db(0.1).unwrap() + 30.0).abs() < 0.001);
        assert_eq!(linear_to_db(0.0).unwrap(), -144.0);
    }

    #[test]
    fn volume_is_monotonic() {
        let mut previous = f64::NEG_INFINITY;
        for step in 1..=100 {
            let db = linear_to_db(step as f32 / 100.0).unwrap();
            assert!(db > previous, "{db} 应大于 {previous}");
            previous = db;
        }
    }

    #[test]
    fn volume_rejects_out_of_range() {
        assert!(matches!(linear_to_db(1.5), Err(Error::Config(_))));
        assert!(matches!(linear_to_db(-0.1), Err(Error::Config(_))));
        assert!(matches!(linear_to_db(f32::NAN), Err(Error::Config(_))));
    }

    #[test]
    fn counters_snapshot_round_trips() {
        let counters = Counters::default();
        counters.add(Counter::Packets, 3);
        counters.add(Counter::Frames, 3 * 352);
        counters.set(Counter::Queued, 1408);
        let stats = counters.snapshot();
        assert_eq!(stats.packets_sent, 3);
        assert_eq!(stats.frames_sent, 1056);
        assert_eq!(stats.queued_bytes, 1408);
        assert_eq!(stats.underrun_packets, 0);
    }

    #[test]
    fn codec_names_are_stable() {
        assert_eq!(Codec::Alac.as_str(), "alac");
        assert_eq!(Codec::Lpcm.as_str(), "lpcm");
        assert_eq!(Codec::default(), Codec::Alac);
    }
}
