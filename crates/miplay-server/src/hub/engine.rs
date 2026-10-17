//! 桌面 GUI 用的 hub 引擎。
//!
//! `miplay-bridge hub` 是一个阻塞的 CLI 进程：自己解析参数、建采集、起 axum、等 Ctrl-C。
//! GUI 需要的是「同一套 hub 放在后台线程里，UI 线程发命令、读快照」，所以这里把
//! `win::run` 里的装配流程抽出来，包成 [`EngineHandle`]：
//!
//! * UI → 引擎：[`EngineHandle::start_devices`] / [`EngineHandle::stop_devices`] /
//!   [`EngineHandle::test_tone`] 只是往 `tokio::sync::mpsc` 里塞命令，**不阻塞 UI**。
//! * 引擎 → UI：[`EngineHandle::snapshot`] 返回一份 `Clone` 的状态快照，
//!   UI 每 200~300 ms 拉一次即可（不打 HTTP、不碰锁内部）。
//!
//! 采集、HTTP 服务、泵线程的生命周期都归引擎线程所有：drop [`EngineHandle`] 会先给
//! 在投放的音箱下发 `stop`、停泵、关流，再结束线程（避免音箱对着死 URL 反复重连）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use miplay_audio::{
    AudioCapture, CaptureConfig, CaptureMode, ChannelRoute, ChannelRouter, DeviceSelector,
};
use miplay_core::{MiToken, MinaClient};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::*;

/// 快照刷新周期：250 ms 足够让电平表看起来连续，又不会让 UI 线程忙。
const TICK: Duration = Duration::from_millis(250);
/// 电平表统计窗口（字节）：4096 B ≈ 23 ms 的 44.1k/2ch/s16。
const LEVEL_WINDOW_BYTES: usize = 4096;

// ---------------------------------------------------------------- 配置

/// 引擎启动参数。字段语义与 `hub` 子命令的 `--xxx` 参数一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// HTTP 监听端口（默认 8931，与老 hub / 防火墙规则一致）。
    pub port: u16,
    /// 左右分离：第 1 台只听左声道、第 2 台只听右声道。
    pub split: bool,
    /// `--split` 时左右对调。
    pub swap: bool,
    /// 采集增益 0~8。
    pub gain: f32,
    /// 投放前把音箱音量设成该值（仅发指令，不改硬件配置）。
    pub volume: Option<u8>,
    /// 采集哪个输出端点（默认优先「小米妙播设备」虚拟声卡，其次系统默认输出）。
    pub capture_device: Option<String>,
    /// 覆盖下发给音箱的地址前缀（默认自动探测本机 LAN IP）。
    pub url_base: Option<String>,
    /// 是否尝试添加 Windows 防火墙入站规则。
    pub firewall: bool,
    /// 启动后自动投放的设备；空 = 只起服务与采集，等 UI 点「开始投放」。
    pub start_devices: Vec<String>,
    /// 只把这些设备放进名单（空 = 账号下所有设备）。
    pub allow_devices: Vec<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            port: Args::default().port,
            split: false,
            swap: false,
            gain: 1.0,
            volume: None,
            capture_device: None,
            url_base: None,
            firewall: true,
            start_devices: Vec::new(),
            allow_devices: Vec::new(),
        }
    }
}

/// 引擎生命周期阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// 线程已起，token / 设备列表 / 采集 / 监听还在初始化。
    #[default]
    Booting,
    /// 一切就绪：HTTP 在监听、采集在跑、可以投放。
    Running,
    /// 初始化失败（原因见 [`EngineSnapshot::error`]）。
    Failed,
    /// 已正常收尾。
    Stopped,
}

impl Phase {
    /// 中文短标签，GUI 直接显示。
    pub fn label(self) -> &'static str {
        match self {
            Phase::Booting => "启动中",
            Phase::Running => "运行中",
            Phase::Failed => "启动失败",
            Phase::Stopped => "已停止",
        }
    }

    /// 是否已经可以投放。
    pub fn is_running(self) -> bool {
        matches!(self, Phase::Running)
    }
}

// ---------------------------------------------------------------- 快照

/// 采集电平（0.0~1.0，线性幅度）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Level {
    /// 左声道峰值。
    pub peak_l: f32,
    /// 右声道峰值。
    pub peak_r: f32,
    /// 左右合并 RMS。
    pub rms: f32,
}

/// 采集侧状态。
#[derive(Debug, Clone, Default)]
pub struct CaptureSnapshot {
    /// 实际打开的端点名，如 `扬声器 (小米妙播设备)`。
    pub device: String,
    /// WASAPI 协商出来的格式描述（很长，只适合 tooltip）。
    pub negotiated: String,
    /// 采集格式一句话，如 `44100 Hz / 2 ch / 16 bit`。
    pub format: String,
    /// 采集的是不是「小米妙播设备」虚拟声卡。
    pub using_vad: bool,
    /// 采集线程是否活着。
    pub running: bool,
    /// 采集线程的错误（退出原因）。
    pub error: Option<String>,
    /// 最近 1.5 s 内是否真的有新音频。
    pub audio_recent: bool,
    /// 距最近一次新音频多少秒。
    pub seconds_since_audio: f64,
    /// 泵线程落后采集多少秒（正常应接近 0）。
    pub backlog_seconds: f64,
    /// 每条流环形缓冲长度（秒）。
    pub ring_seconds: f32,
    /// 实时电平。
    pub level: Level,
    /// 已分发给各流的音频帧数。
    pub frames: u64,
}

impl CaptureSnapshot {
    /// 采集链一句话状态。
    pub fn state_label(&self) -> &'static str {
        if !self.running {
            "采集已停止"
        } else if self.error.is_some() {
            "采集异常"
        } else if self.audio_recent {
            "采集中"
        } else {
            "静默"
        }
    }
}

/// 单台设备状态。
#[derive(Debug, Clone, Default)]
pub struct DeviceSnapshot {
    /// 流 id（通常 = did）。
    pub id: String,
    /// 小米 MIoT did。
    pub did: String,
    /// MiNA uuid。
    pub device_id: String,
    /// 展示名，如 `Xiaomi Sound-左`。
    pub label: String,
    /// 型号，如 `L16A`。
    pub hardware: String,
    /// 云端是否在线。
    pub online: bool,
    /// 声道源：`mixdown` / `left` / `right`。
    pub source: String,
    /// 下发给音箱的拉流地址。
    pub url: String,
    /// 是否处于投放状态。
    pub active: bool,
    /// 当前拉流客户端数（>0 = 音箱真的连上来了）。
    pub clients: u64,
    /// 客户端落后直播写入位置多少秒。
    pub lag_seconds: f64,
    /// 已发出的音频字节数。
    pub bytes_sent: u64,
    /// 因客户端太慢被丢掉的字节数。
    pub skipped_bytes: u64,
    /// 下发 `play_url` 次数。
    pub plays_sent: u64,
    /// 下发失败次数。
    pub play_failures: u64,
    /// 掉线重发次数。
    pub reissues: u64,
    /// 距上次有客户端接入多少秒。
    pub last_client_seconds_ago: Option<f64>,
    /// 距上次下发 `play_url` 多少秒。
    pub last_play_seconds_ago: Option<f64>,
    /// 最近一次下发/拉流的错误。
    pub last_error: Option<String>,
}

impl DeviceSnapshot {
    /// 一句话状态：`未投放` / `等待音箱接入` / `已连上` / `下发失败`。
    pub fn state_label(&self) -> &'static str {
        if !self.active {
            "未投放"
        } else if self.clients > 0 {
            "已连上"
        } else if self.play_failures > 0 {
            "下发失败"
        } else if self.reissues > 0 {
            "重发中"
        } else {
            "等待音箱接入"
        }
    }
}

/// 引擎完整状态快照。
#[derive(Debug, Clone, Default)]
pub struct EngineSnapshot {
    /// 生命周期阶段。
    pub phase: Phase,
    /// `Phase::Failed` 时的失败原因（或非致命警告）。
    pub error: Option<String>,
    /// HTTP 端口。
    pub port: u16,
    /// 下发给音箱的地址前缀。
    pub url_base: String,
    /// HTTP 是否在监听。
    pub listening: bool,
    /// 防火墙规则结果（`added` / `exists` / `跳过` ...）。
    pub firewall: String,
    /// 引擎已运行多少秒。
    pub uptime_seconds: f64,
    /// 所有设备当前客户端总数。
    pub clients_total: u64,
    /// 采集侧状态。
    pub capture: CaptureSnapshot,
    /// 全部在册设备。
    pub devices: Vec<DeviceSnapshot>,
    /// 系统里是否存在「小米妙播设备」虚拟声卡。
    pub vad_present: bool,
    /// 系统默认输出端点名。
    pub default_output: Option<String>,
    /// 系统默认输出是不是「小米妙播设备」。
    pub default_output_is_vad: bool,
}

impl EngineSnapshot {
    /// 正在投放的设备数。
    pub fn active_count(&self) -> usize {
        self.devices.iter().filter(|d| d.active).count()
    }

    /// 当前连上的客户端总数（>0 说明链条真的通了）。
    pub fn connected_count(&self) -> u64 {
        self.devices.iter().filter(|d| d.active && d.clients > 0).count() as u64
    }
}

// ---------------------------------------------------------------- 命令

/// UI 发给引擎的命令。
#[derive(Debug, Clone)]
pub enum Cmd {
    /// 投放到这些设备；空 vec = 全部在册设备。
    Start(Vec<String>),
    /// 停止这些设备；`None` = 全部在投放的设备。
    Stop(Option<Vec<String>>),
    /// 把所有在册设备的音量设为该值。
    SetVolume(u8),
    /// 往采集端点播一段正弦音（自测整条「采集 → 分发 → 音箱」链路）。
    TestTone {
        /// 频率（Hz）。
        freq: f64,
        /// 时长（秒）。
        seconds: f64,
    },
    /// 收尾退出。
    Shutdown,
}

// ---------------------------------------------------------------- 句柄

/// 引擎句柄：UI 线程持有的唯一入口。
///
/// 内部是一个后台线程（自带 tokio runtime）+ 两条通道：
/// 命令走 `mpsc`，状态是一把 `Mutex<EngineSnapshot>` 的镜像。
pub struct EngineHandle {
    tx: UnboundedSender<Cmd>,
    state: Arc<StdMutex<EngineSnapshot>>,
    alive: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl EngineHandle {
    /// 起一个引擎线程并立刻返回（初始化在后台进行，进度看 [`EngineHandle::snapshot`]）。
    pub fn start(cfg: EngineConfig) -> Self {
        let (tx, rx) = unbounded_channel();
        let engine_tx = tx.clone();
        let port = cfg.port;
        let state = Arc::new(StdMutex::new(EngineSnapshot {
            phase: Phase::Booting,
            port,
            ..Default::default()
        }));
        let alive = Arc::new(AtomicBool::new(true));

        let thread_state = state.clone();
        let thread_alive = alive.clone();
        let join = std::thread::Builder::new()
            .name("miplay-engine".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        set_failed(&thread_state, format!("创建 tokio runtime 失败：{e}"));
                        thread_alive.store(false, Ordering::Release);
                        return;
                    }
                };
                rt.block_on(run_engine(
                    cfg,
                    rx,
                    engine_tx,
                    thread_state.clone(),
                    thread_alive.clone(),
                ));
                thread_alive.store(false, Ordering::Release);
            })
            .expect("起引擎线程失败");
        Self {
            tx,
            state,
            alive,
            join: Some(join),
        }
    }

    /// 投放到指定设备（key 可以是 did 或 MiNA uuid；空 = 全部）。
    pub fn start_devices(&self, keys: Vec<String>) -> bool {
        self.tx.send(Cmd::Start(keys)).is_ok()
    }

    /// 停止投放（`None` = 全部）。
    pub fn stop_devices(&self, keys: Option<Vec<String>>) -> bool {
        self.tx.send(Cmd::Stop(keys)).is_ok()
    }

    /// 设置所有设备音量。
    pub fn set_volume(&self, volume: u8) -> bool {
        self.tx.send(Cmd::SetVolume(volume)).is_ok()
    }

    /// 播一段测试音（异步，结果看日志）。
    pub fn test_tone(&self, seconds: f64) -> bool {
        self.tx
            .send(Cmd::TestTone {
                freq: 440.0,
                seconds,
            })
            .is_ok()
    }

    /// 拿一份当前状态快照（克隆，不阻塞引擎）。
    pub fn snapshot(&self) -> EngineSnapshot {
        snapshot_of(&self.state)
    }

    /// 引擎线程是否还活着。
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// 收尾：让引擎停播、关流、退线程。可重复调用。
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------- 引擎线程

fn snapshot_of(state: &Arc<StdMutex<EngineSnapshot>>) -> EngineSnapshot {
    match state.lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn publish(state: &Arc<StdMutex<EngineSnapshot>>, snap: EngineSnapshot) {
    match state.lock() {
        Ok(mut g) => *g = snap,
        Err(poisoned) => *poisoned.into_inner() = snap,
    }
}

fn set_failed(state: &Arc<StdMutex<EngineSnapshot>>, msg: String) {
    let mut snap = snapshot_of(state);
    snap.phase = Phase::Failed;
    snap.error = Some(msg.clone());
    snap.listening = false;
    publish(state, snap);
    tracing::error!("[引擎] 启动失败：{msg}");
}

/// 引擎初始化产物：所有运行时对象的所有权都在这里。
struct Boot {
    hub: Arc<Hub>,
    capture: AudioCapture,
    listener: tokio::net::TcpListener,
    firewall: String,
    vad_present: bool,
    default_output: Option<String>,
    test_selector: DeviceSelector,
}

/// 装配：凭据 → 设备列表 → 采集 → 分声道 → 泵 → Hub → 监听。
///
/// 与 `win::run` 的顺序保持一致（先解析采集端点、再占端口），失败原因用
/// `anyhow` 上下文说清楚，GUI 直接显示。
async fn boot_engine(cfg: &EngineConfig) -> anyhow::Result<Boot> {
    // 1) 凭据与设备列表（下发改走小米云，必须要账号）
    let token = MiToken::load_default()
        .context("加载小米账号凭据失败（用 `miplay-bridge token` 看它找的路径）")?;
    let client = MinaClient::new(token)?;
    let all = client
        .device_list()
        .await
        .context("获取设备列表失败（检查网络/代理）")?;
    if all.is_empty() {
        anyhow::bail!("账号下没有任何设备");
    }

    // 2) 本机 LAN IP / URL 前缀
    let ip = local_ip().context("探测本机 LAN IP 失败（网卡都断了吗？）")?;
    let url_base = cfg
        .url_base
        .clone()
        .unwrap_or_else(|| format!("http://{ip}:{}", cfg.port));
    let vad_present = virtual_endpoint_name().is_some();
    let default_output = miplay_audio::default_endpoint().ok().map(|ep| ep.name);

    // 3) 名单：默认把账号下所有有 did/uuid 的设备都建流（GUI 里再挑要投哪几台）。
    let mut args = Args::default();
    args.devices = if cfg.allow_devices.is_empty() {
        all.iter()
            .map(|d| {
                if d.did.is_empty() {
                    d.device_id.clone()
                } else {
                    d.did.clone()
                }
            })
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        cfg.allow_devices.clone()
    };
    args.port = cfg.port;
    args.split = cfg.split;
    args.swap = cfg.swap;
    args.gain = cfg.gain;
    args.volume = cfg.volume;
    args.capture_device = cfg.capture_device.clone();
    args.url_base = Some(url_base.clone());
    args.no_firewall = !cfg.firewall;
    args.no_autostart = true;

    let roster = super::win::build_roster(&all, &args, &url_base).context("没有可投放的设备")?;
    if roster.is_empty() {
        anyhow::bail!("名单是空的（账号下的设备都没有 did/uuid）");
    }

    // 4) 采集端点（先解析，别在采集坏掉时把音箱占了）
    let mut cap_cfg = CaptureConfig::default()
        .with_mode(CaptureMode::Loopback)
        .with_samplerate(SAMPLE_RATE)
        .with_channels(CHANNELS);
    cap_cfg.buffer_ms = CAPTURE_BUFFER_MS;
    if let Some(name) = &cfg.capture_device {
        cap_cfg = cap_cfg.with_device_name(name.clone());
    } else if let Some(vad) = virtual_endpoint_name() {
        // 没显式指定采集端点时优先抓虚拟串流声卡：用户在 Windows 输出列表里
        // 选中「扬声器 (小米妙播设备)」，声音就整条链走到这里。
        cap_cfg = cap_cfg.with_device_name(vad);
    }
    cap_cfg.validate().context("采集参数不合法")?;
    let mut capture = AudioCapture::new(cap_cfg).context("打开采集端点失败")?;
    capture.start().context("启动采集失败")?;
    let capture_name = capture
        .device_name()
        .unwrap_or("（未知采集端点）")
        .to_string();
    let test_selector = DeviceSelector::name_contains(capture_name.clone());

    // 5) 防火墙（尽力，失败不影响服务）
    let firewall = if cfg.firewall {
        ensure_firewall(cfg.port)
    } else {
        "跳过".to_string()
    };

    // 6) 路由器 + 采集泵
    let routes: Vec<ChannelRoute> = roster
        .iter()
        .map(|d| ChannelRoute::new(d.id.clone(), d.source).with_gain(cfg.gain))
        .collect();
    ChannelRouter::validate(&routes)?;
    let router = Arc::new(ChannelRouter::for_seconds(
        routes,
        SAMPLE_RATE,
        CHANNELS,
        RING_SECONDS,
    ));
    let negotiated = capture
        .negotiated_format()
        .map(|f| f.describe())
        .unwrap_or_else(|| "未知".into());
    let capture_ring = capture.ring();
    let pump_stop = Arc::new(AtomicBool::new(false));
    let pump_cursor = Arc::new(AtomicU64::new(0));
    let last_audio_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let pump = spawn_pump(
        capture_ring.clone(),
        router.clone(),
        pump_stop.clone(),
        pump_cursor.clone(),
        last_audio_at.clone(),
    );

    let hub = Arc::new(Hub {
        client,
        port: cfg.port,
        url_base: url_base.clone(),
        gain: cfg.gain,
        split: cfg.split,
        devices: roster,
        router: router.clone(),
        capture: CaptureInfo {
            device_name: capture_name.clone(),
            negotiated: negotiated.clone(),
            ring: capture_ring,
            ring_seconds: RING_SECONDS,
            pump_cursor,
        },
        pump_stop,
        pump: Mutex::new(Some(pump)),
        last_audio_at,
        started_at: Instant::now(),
    });

    // 7) 起 HTTP 服务
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], cfg.port)))
            .await
            .with_context(|| {
                format!(
                    "监听 0.0.0.0:{} 失败：端口被别的程序占用？（可换端口或关掉旧进程）",
                    cfg.port
                )
            })?;
    tracing::info!(
        "[引擎] 采集端点 {capture_name}（{negotiated}），监听 {url_base}，{} 台设备在册",
        hub.devices.len()
    );

    Ok(Boot {
        hub,
        capture,
        listener,
        firewall,
        vad_present,
        default_output,
        test_selector,
    })
}

async fn run_engine(
    cfg: EngineConfig,
    mut rx: UnboundedReceiver<Cmd>,
    tx: UnboundedSender<Cmd>,
    state: Arc<StdMutex<EngineSnapshot>>,
    alive: Arc<AtomicBool>,
) {
    let boot = match boot_engine(&cfg).await {
        Ok(b) => b,
        Err(e) => {
            set_failed(&state, format!("{e:#}"));
            alive.store(false, Ordering::Release);
            return;
        }
    };
    let Boot {
        hub,
        capture,
        listener,
        firewall,
        vad_present,
        default_output,
        test_selector,
    } = boot;

    // HTTP 服务独立 task；退出时 abort 即可。
    //
    // 额外挂一个 `POST /api/hub/tone?freq=440&seconds=2`：它把同一条 `Cmd::TestTone`
    // 塞进引擎命令通道，于是网页、curl、脚本都能触发整条链路自测（GUI 按钮走同一条路）。
    let tone_tx = tx.clone();
    let tone_route = axum::routing::post(
        move |q: axum::extract::Query<std::collections::HashMap<String, String>>| {
            let tx = tone_tx.clone();
            async move {
                let freq = q
                    .get("freq")
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(440.0)
                    .clamp(40.0, 8000.0);
                let seconds = q
                    .get("seconds")
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(2.0)
                    .clamp(0.5, 10.0);
                let ok = tx.send(Cmd::TestTone { freq, seconds }).is_ok();
                axum::Json(serde_json::json!({
                    "ok": ok,
                    "freq": freq,
                    "seconds": seconds,
                }))
            }
        },
    );
    let app = hub_router(hub.clone())
        .route("/api/hub/tone", tone_route)
        .into_make_service_with_connect_info::<SocketAddr>();
    let serve = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("[引擎] HTTP 服务异常退出：{e:#}");
        }
    });

    publish(
        &state,
        build_snapshot(
            &hub,
            &capture,
            &cfg,
            Level::default(),
            &firewall,
            vad_present,
            &default_output,
        ),
    );
    tracing::info!("[引擎] 就绪：http://{}/", hub.url_base.trim_start_matches("http://"));

    // 需要自动投放时先垫一点数据，音箱一连上就有声音。
    if !cfg.start_devices.is_empty() {
        tokio::time::sleep(PREFILL).await;
        do_start(&hub, &cfg.start_devices, cfg.volume).await;
    }

    let reissue = hub.clone();
    let reissue_task = tokio::spawn(async move { reissue.reissue_loop().await });

    let mut tick = tokio::time::interval(TICK);
    let mut health_seen: Option<Option<String>> = None;
    loop {
        tokio::select! {
            biased;
            cmd = rx.recv() => {
                match cmd {
                    None | Some(Cmd::Shutdown) => break,
                    Some(Cmd::Start(keys)) => do_start(&hub, &keys, cfg.volume).await,
                    Some(Cmd::Stop(keys)) => do_stop(&hub, keys).await,
                    Some(Cmd::SetVolume(v)) => do_volume(&hub, v).await,
                    Some(Cmd::TestTone { freq, seconds }) => {
                        do_tone(test_selector.clone(), freq, seconds);
                    }
                }
            }
            _ = tick.tick() => {
                // 采集健康看护：只在状态变化时喊一声（拔耳机/切端点会让采集线程退出）。
                let now = capture_health(&capture);
                if health_seen.as_ref() != Some(&now) {
                    match &now {
                        Some(msg) => tracing::warn!("[采集] {msg}"),
                        None => tracing::info!("[采集] 采集线程正常"),
                    }
                    health_seen = Some(now);
                }
                let level = audio_level(hub.capture.ring.as_ref());
                publish(
                    &state,
                    build_snapshot(&hub, &capture, &cfg, level, &firewall, vad_present, &default_output),
                );
            }
        }
    }

    tracing::info!("[引擎] 收尾：停止投放并关闭服务");
    reissue_task.abort();
    serve.abort();
    hub.shutdown().await;
    let mut snap = build_snapshot(
        &hub,
        &capture,
        &cfg,
        Level::default(),
        &firewall,
        vad_present,
        &default_output,
    );
    snap.phase = Phase::Stopped;
    snap.listening = false;
    publish(&state, snap);
}

async fn do_start(hub: &Arc<Hub>, keys: &[String], volume: Option<u8>) {
    let targets = if keys.is_empty() {
        hub.devices.clone()
    } else {
        match hub.select(&Some(keys.to_vec())) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("[投放] 设备选择失败：{e:#}");
                return;
            }
        }
    };
    let targets: Vec<Arc<HubDevice>> = targets
        .into_iter()
        .filter(|d| d.device.online())
        .collect();
    if targets.is_empty() {
        tracing::warn!("[投放] 选中的设备都不在线，没有可投放的");
        return;
    }
    if let Some(v) = volume {
        for dev in &targets {
            if let Err(e) = hub.client.set_volume(&dev.device.device_id, v).await {
                tracing::warn!("[音量] {} 设置失败：{e:#}", dev.device.label());
            }
        }
    }
    let results = hub.start_devices(&targets).await;
    let ok = results.iter().filter(|r| r["ok"] == json!(true)).count();
    tracing::info!(
        "[投放] {} 台设备：成功 {}，失败 {}",
        results.len(),
        ok,
        results.len().saturating_sub(ok)
    );
}

async fn do_stop(hub: &Arc<Hub>, keys: Option<Vec<String>>) {
    let targets: Vec<Arc<HubDevice>> = match keys {
        None => hub
            .devices
            .iter()
            .filter(|d| d.is_active())
            .cloned()
            .collect(),
        Some(k) => match hub.select(&Some(k)) {
            Ok(t) => t.into_iter().filter(|d| d.is_active()).collect(),
            Err(e) => {
                tracing::warn!("[停止] 设备选择失败：{e:#}");
                return;
            }
        },
    };
    if targets.is_empty() {
        tracing::info!("[停止] 没有在投放的设备");
        return;
    }
    hub.stop_devices(&targets).await;
    tracing::info!("[停止] 已停止 {} 台设备", targets.len());
}

async fn do_volume(hub: &Arc<Hub>, volume: u8) {
    for dev in &hub.devices {
        match hub.client.set_volume(&dev.device.device_id, volume).await {
            Ok(ok) => tracing::info!(
                "[音量] {} => {volume}%（{ok}）",
                dev.device.label()
            ),
            Err(e) => tracing::warn!("[音量] {} 设置失败：{e:#}", dev.device.label()),
        }
    }
}

fn do_tone(selector: DeviceSelector, freq: f64, seconds: f64) {
    tracing::info!(
        "[自测] 往 {} 播 {seconds:.1}s / {freq:.0}Hz 正弦音",
        selector.describe()
    );
    std::thread::spawn(move || match miplay_audio::play_sine(&selector, freq, seconds, 0.25) {
        Ok(info) => tracing::info!("[自测] 播放完成：{}", info.describe()),
        Err(e) => tracing::warn!("[自测] 播放失败：{e}"),
    });
}

// ---------------------------------------------------------------- 状态组装

fn capture_health(capture: &AudioCapture) -> Option<String> {
    match (capture.is_running(), capture.last_error()) {
        (true, None) => None,
        (true, Some(e)) => Some(format!("采集异常：{e}")),
        (false, Some(e)) => Some(format!("采集线程已退出：{e}")),
        (false, None) => Some("采集线程已退出（输出设备被拔掉或切换了？）".to_string()),
    }
}

/// 从采集环形缓冲的**最新**一段算实时电平（只读，不推进任何读者游标）。
fn audio_level(ring: &miplay_audio::RingBuffer) -> Level {
    let cursor = ring.cursor();
    let earliest = ring.earliest_cursor();
    if cursor <= earliest {
        return Level::default();
    }
    let want = (LEVEL_WINDOW_BYTES as u64).min(cursor - earliest) as usize;
    if want < 4 {
        return Level::default();
    }
    match ring.read_from(cursor - want as u64, want, Duration::ZERO) {
        Ok(out) => level_of(&out.data),
        Err(_) => Level::default(),
    }
}

fn level_of(pcm: &[u8]) -> Level {
    let frames = pcm.len() / 4;
    if frames == 0 {
        return Level::default();
    }
    let (mut peak_l, mut peak_r) = (0f32, 0f32);
    let mut sum_sq = 0f64;
    for f in pcm.chunks_exact(4) {
        let l = i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0;
        let r = i16::from_le_bytes([f[2], f[3]]) as f32 / 32768.0;
        peak_l = peak_l.max(l.abs());
        peak_r = peak_r.max(r.abs());
        sum_sq += (l as f64) * (l as f64) + (r as f64) * (r as f64);
    }
    Level {
        peak_l: peak_l.min(1.0),
        peak_r: peak_r.min(1.0),
        rms: ((sum_sq / (frames as f64 * 2.0)).sqrt() as f32).min(1.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    hub: &Hub,
    capture: &AudioCapture,
    cfg: &EngineConfig,
    level: Level,
    firewall: &str,
    vad_present: bool,
    default_output: &Option<String>,
) -> EngineSnapshot {
    let devices: Vec<DeviceSnapshot> = hub
        .devices
        .iter()
        .map(|d| DeviceSnapshot {
            id: d.id.clone(),
            did: d.device.did.clone(),
            device_id: d.device.device_id.clone(),
            label: d.device.label(),
            hardware: d.device.hardware.clone(),
            online: d.device.online(),
            source: d.source.as_str().to_string(),
            url: d.url.clone(),
            active: d.is_active(),
            clients: d.clients.load(Ordering::Relaxed),
            lag_seconds: round2(hub.lag_seconds(d)),
            bytes_sent: d.bytes_sent.load(Ordering::Relaxed),
            skipped_bytes: d.skipped_bytes.load(Ordering::Relaxed),
            plays_sent: d.plays_sent.load(Ordering::Relaxed),
            play_failures: d.play_failures.load(Ordering::Relaxed),
            reissues: d.reissue_count(),
            last_client_seconds_ago: d.seconds_since_last_client().map(round2),
            last_play_seconds_ago: d
                .last_play_at
                .lock()
                .unwrap()
                .map(|t| round2(t.elapsed().as_secs_f64())),
            last_error: d.last_error.lock().unwrap().clone(),
        })
        .collect();
    let clients_total = devices.iter().map(|d| d.clients).sum();
    let device_name = hub.capture.device_name.clone();
    let using_vad = VAD_NAME_HINTS.iter().any(|h| device_name.contains(h));
    let seconds_since_audio = round2(hub.seconds_since_audio());
    let audio_recent = hub.audio_recent();

    EngineSnapshot {
        phase: Phase::Running,
        error: None,
        port: cfg.port,
        url_base: hub.url_base.clone(),
        listening: true,
        firewall: firewall.to_string(),
        uptime_seconds: round2(hub.started_at.elapsed().as_secs_f64()),
        clients_total,
        capture: CaptureSnapshot {
            device: device_name,
            negotiated: hub.capture.negotiated.clone(),
            format: format!(
                "{} Hz / {} ch / 16 bit",
                super::SAMPLE_RATE,
                super::CHANNELS
            ),
            using_vad,
            running: capture.is_running(),
            error: capture.last_error(),
            audio_recent,
            seconds_since_audio,
            backlog_seconds: round2(hub.capture_backlog_seconds()),
            ring_seconds: hub.capture.ring_seconds,
            level,
            frames: hub.router.frames(),
        },
        devices,
        vad_present,
        default_output: default_output.clone(),
        default_output_is_vad: default_output
            .as_ref()
            .map(|n| VAD_NAME_HINTS.iter().any(|h| n.contains(h)))
            .unwrap_or(false),
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_of_silence_is_zero() {
        let pcm = vec![0u8; 4096];
        let lv = level_of(&pcm);
        assert_eq!(lv.peak_l, 0.0);
        assert_eq!(lv.peak_r, 0.0);
        assert_eq!(lv.rms, 0.0);
    }

    #[test]
    fn level_of_full_scale_is_one() {
        let mut pcm = Vec::new();
        for _ in 0..1024 {
            pcm.extend_from_slice(&i16::MIN.to_le_bytes());
            pcm.extend_from_slice(&i16::MAX.to_le_bytes());
        }
        let lv = level_of(&pcm);
        assert!((lv.peak_l - 1.0).abs() < 1e-6, "{lv:?}");
        // i16::MAX = 32767，除以 32768 后是 0.99997，不可能正好 1.0。
        assert!(lv.peak_r > 0.9999, "{lv:?}");
        assert!(lv.rms > 0.9999, "{lv:?}");
    }

    #[test]
    fn level_of_half_scale() {
        let mut pcm = Vec::new();
        for _ in 0..512 {
            pcm.extend_from_slice(&(16384i16).to_le_bytes());
            pcm.extend_from_slice(&(-16384i16).to_le_bytes());
        }
        let lv = level_of(&pcm);
        assert!((lv.peak_l - 0.5).abs() < 1e-3, "{lv:?}");
        assert!((lv.peak_r - 0.5).abs() < 1e-3, "{lv:?}");
    }

    #[test]
    fn level_of_empty_is_zero() {
        assert_eq!(level_of(&[]), Level::default());
        assert_eq!(level_of(&[0, 0]), Level::default());
    }

    #[test]
    fn device_state_labels() {
        let mut d = DeviceSnapshot::default();
        assert_eq!(d.state_label(), "未投放");
        d.active = true;
        assert_eq!(d.state_label(), "等待音箱接入");
        d.reissues = 2;
        assert_eq!(d.state_label(), "重发中");
        d.clients = 1;
        assert_eq!(d.state_label(), "已连上");
        d.clients = 0;
        d.play_failures = 3;
        assert_eq!(d.state_label(), "下发失败");
    }

    #[test]
    fn phase_labels() {
        assert_eq!(Phase::Booting.label(), "启动中");
        assert!(Phase::Running.is_running());
        assert!(!Phase::Failed.is_running());
        assert_eq!(Phase::Stopped.label(), "已停止");
    }

    #[test]
    fn engine_config_default_uses_hub_defaults() {
        let cfg = EngineConfig::default();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.gain, 1.0);
        assert!(cfg.firewall);
        assert!(cfg.start_devices.is_empty());
    }
}
