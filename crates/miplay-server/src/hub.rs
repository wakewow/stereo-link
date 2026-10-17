//! `hub` 子命令：本机系统声音 → 局域网 WAV 服务 → 小米云 `play_url` → 音箱自己来拉。
//!
//! 这是用户选定的**唯一路线**（AirPlay 已废弃）：音箱不做 AirPlay 接收端，
//! 而是由小米云下发一条 `mediaplayer.player_play_url`，让音箱主动来拉我们本机的
//! HTTP WAV 流。链路：
//!
//! ```text
//! 系统声音 --WASAPI 回环--> 44100/2ch/s16le
//!     --ChannelRouter--> 每台音箱一条 RingBuffer（左右分离或同一混音）
//!     --HTTP GET /hub/<did>.wav--> 44 字节 WAV 头 + 无限 PCM
//!     --MiNA player_play_url--> 音箱自己拉流出声
//! ```
//!
//! 与老 Python 参考实现（`mixplay-pc-audio/pc_stereo_hub.py`）的对应关系：
//! * 环形缓冲 → [`miplay_audio::RingBuffer`]（同样是「读者从当前时刻开始、落后就跳帧」）；
//! * 左右分离 → [`miplay_audio::ChannelRouter`] + `ChannelSource::{Left,Right,Mixdown}`；
//! * 44 字节 WAV 头（data size 假装 2GB）→ [`miplay_audio::build_stream_wav_header`]；
//! * 下发指令 → [`miplay_core::MinaClient::play_on`]（不再依赖 MiXPlay 的本地 API）。
//!
//! 相比 Python 版新增：
//! * 断线自动重发（音箱流断了会重连，也会干脆不再来——判据见 [`should_reissue`]）；
//! * 网页控制（多选设备 + 投放/停止 + 实时缓冲秒数）；
//! * 停止时**立刻**切断所有拉流连接（把设备标成 inactive，流循环随即退出）。
//!
//! 只有 Windows 有 WASAPI 回环，非 Windows 平台编译成一句 `bail!`。

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// 默认端口（老 Python hub 占着 8930）。
pub const DEFAULT_PORT: u16 = 8931;
/// 音箱固件只接受 44100。
pub const SAMPLE_RATE: u32 = 44100;
/// 立体声。
pub const CHANNELS: u16 = 2;
/// 44100 × 2ch × 2B。
pub const BYTES_PER_SECOND: u32 = SAMPLE_RATE * CHANNELS as u32 * 2;
/// 每条流保留的秒数（与 Python 版一致）。
pub const RING_SECONDS: f32 = 5.0;
/// WASAPI 共享缓冲时长；默认 200ms 对「拉到就放」来说太肥，压到 20ms。
pub const CAPTURE_BUFFER_MS: u32 = 20;
/// 每次读环形缓冲最多取多少字节（Python 版 32768）。
pub const STREAM_CHUNK: usize = 32 * 1024;
/// 环形缓冲的读超时（也是流循环的节拍）。
pub const READ_TIMEOUT: Duration = Duration::from_millis(200);
/// 采集泵每块最多读多少字节。task-14 实测（`research/latency/latency-report.md`）：
/// 50 ms 的读块让 PC 侧采集链路多出约 19 ms（31.1 → 12.4 ms），所以这里用 10 ms
/// （44100/2ch/s16 下 1764 B，正好 441 帧），而不是更大更「省」的块。
const PUMP_CHUNK: usize = 1764;
/// 某设备**从没连上过**时，多久重发一次 `play_url`。
pub const REISSUE_AFTER: Duration = Duration::from_secs(8);
/// 某设备**连上过又掉线**时，多久重发一次。task-14 实测：立体声对的两只箱子先后下发时，
/// 先连上的那只可能在另一只连上后约 0.2 s 断开；纯静音时音箱约 6.4 s 也会自己断开。
/// 所以「掉线重连」是常态，必须快，且必须带退避上限。
const RECONNECT_AFTER: Duration = Duration::from_secs(2);
/// 重连退避上限（连续重发都没人连上时逐次加倍，封顶到这里）。
const RECONNECT_BACKOFF_CAP: Duration = Duration::from_secs(30);
/// 重发扫描周期。
const REISSUE_SCAN: Duration = Duration::from_secs(2);
/// 同一设备两次重发之间的最小间隔（`play_url` 的 POST 往返 125~157 ms，2 s 足够温和）。
const REISSUE_MIN_GAP: Duration = Duration::from_secs(2);
/// 多久算「采集正在出声音」：有声音时重连走快路径，不吃退避。
const AUDIO_RECENT: Duration = Duration::from_millis(1500);
/// 采集静默多久在日志里告警一次（音箱约 6.4 s 后会主动断开，之前要能看见）。
const AUDIO_SILENT_WARN: Duration = Duration::from_secs(3);
/// 采集增益上限（与 `stream` 子命令一致）。
const MAX_GAIN: f32 = 8.0;
/// 自动投放前先垫多久数据（与 Python 版的 `sleep(1.2)` 一致）。
const PREFILL: Duration = Duration::from_millis(1200);

const HELP: &str = "\
miplay-bridge hub —— 把本机系统声音经局域网 WAV 服务投放给小米音箱（妙播拉流）

用法:
  miplay-bridge hub --devices <did[,did…]> [选项]

必需:
  --devices <列表>       目标设备，逗号分隔；did 或 MiNA uuid 都行
                         （`miplay-bridge devices` 可以列出这两者）

选项:
  --port <端口>          HTTP 服务端口（默认 8931；老 Python hub 用 8930）
  --split                左右分离：第 1 台只放左声道、第 2 台只放右声道
                         （不写就是所有设备听同一份混音）
  --swap                 --split 时左右对调
  --gain <倍数>          采集增益 0~8（默认 1.0）
  --volume <0~100>       投放前把设备音量设为该值（只发指令，不改硬件配置）
  --capture-device <子串> 采集哪个输出端点（默认系统默认输出）
  --url-base <URL>       覆盖下发给音箱的地址前缀（默认自动探测本机 LAN IP）
  --no-firewall          不尝试添加 Windows 防火墙入站规则
  --no-autostart         只起服务和采集，不自动下发播放（由网页点按钮）
  --seconds <秒>         跑够这么久自动停止（默认一直跑，Ctrl-C 停）
  --dry-run              只打印计划（含真实设备名与 URL），不建流、不下发
  --stop                 只停止这些设备然后退出（不起服务、不采集）
  -h, --help             显示本帮助

说明:
  * 服务监听 0.0.0.0，音箱按 http://<本机LAN IP>:<端口>/hub/<did>.wav 来拉；
    Windows 防火墙默认拦入站，所以启动时会尽力加一条 TCP 放行规则（可用
    --no-firewall 关掉；手动命令见 docs/hub.md）。
  * Ctrl-C 会先给每台设备发 stop，再切断所有拉流连接。
";

// ---------------------------------------------------------------- 参数解析

#[derive(Debug, Clone)]
struct Args {
    devices: Vec<String>,
    port: u16,
    split: bool,
    swap: bool,
    gain: f32,
    volume: Option<u8>,
    capture_device: Option<String>,
    url_base: Option<String>,
    no_firewall: bool,
    no_autostart: bool,
    seconds: Option<f64>,
    dry_run: bool,
    stop_only: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            port: DEFAULT_PORT,
            split: false,
            swap: false,
            gain: 1.0,
            volume: None,
            capture_device: None,
            url_base: None,
            no_firewall: false,
            no_autostart: false,
            seconds: None,
            dry_run: false,
            stop_only: false,
        }
    }
}

/// 取 `--flag` 后面的值（`i` 前进到取值处）。
fn take(argv: &[String], i: &mut usize, name: &str) -> anyhow::Result<String> {
    *i += 1;
    argv.get(*i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{name} 后面缺少取值"))
}

impl Args {
    /// `Ok(None)` = 请求打印帮助。
    fn parse(argv: &[String]) -> anyhow::Result<Option<Self>> {
        let mut a = Args::default();
        let mut i = 0;
        while i < argv.len() {
            let arg = argv[i].as_str();
            match arg {
                "-h" | "--help" | "help" => return Ok(None),
                "--devices" => a.devices = parse_devices(&take(argv, &mut i, "--devices")?),
                "--port" => {
                    a.port = take(argv, &mut i, "--port")?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--port 必须是 1~65535"))?;
                    anyhow::ensure!(a.port != 0, "--port 不能是 0");
                }
                "--split" => a.split = true,
                "--swap" => a.swap = true,
                "--gain" => {
                    let g: f32 = take(argv, &mut i, "--gain")?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--gain 必须是数字"))?;
                    anyhow::ensure!(
                        g.is_finite() && (0.0..=MAX_GAIN).contains(&g),
                        "--gain 必须在 0~{MAX_GAIN}"
                    );
                    a.gain = g;
                }
                "--volume" => {
                    let v: u8 = take(argv, &mut i, "--volume")?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--volume 必须是整数"))?;
                    anyhow::ensure!(v <= 100, "--volume 必须在 0~100");
                    a.volume = Some(v);
                }
                "--capture-device" => {
                    a.capture_device = Some(take(argv, &mut i, "--capture-device")?)
                }
                "--url-base" => a.url_base = Some(take(argv, &mut i, "--url-base")?),
                "--no-firewall" => a.no_firewall = true,
                "--no-autostart" => a.no_autostart = true,
                "--seconds" => {
                    let s: f64 = take(argv, &mut i, "--seconds")?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--seconds 必须是数字"))?;
                    anyhow::ensure!(s.is_finite() && s > 0.0, "--seconds 必须大于 0");
                    a.seconds = Some(s);
                }
                "--dry-run" => a.dry_run = true,
                "--stop" => a.stop_only = true,
                other => anyhow::bail!("未知参数 `{other}`（--help 看用法）"),
            }
            i += 1;
        }
        anyhow::ensure!(
            !a.devices.is_empty() || a.dry_run,
            "必须用 --devices 指定至少一台设备（--help 看用法）"
        );
        Ok(Some(a))
    }
}

/// `"a, b,,c"` → `["a","b","c"]`（去空白、去空项、按首次出现去重）。
fn parse_devices(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() || out.iter().any(|x| x == p) {
            continue;
        }
        out.push(p.to_string());
    }
    out
}

/// `/hub/501718792.wav` → `"501718792"`；查询串、大小写、结尾斜杠都兼容。
fn strip_wav_suffix(raw: &str) -> String {
    let path = raw.split(['?', '#']).next().unwrap_or(raw);
    let path = path.trim_end_matches('/');
    let last = path.rsplit('/').next().unwrap_or(path);
    last.strip_suffix(".wav")
        .or_else(|| last.strip_suffix(".WAV"))
        .unwrap_or(last)
        .to_string()
}

// ---------------------------------------------------------------- 设备与状态

/// 一台目标音箱在 hub 里的全部状态。
struct HubDevice {
    /// URL 路径里的 id（米家 did）。
    id: String,
    /// 云端设备条目（要 `device_id` / `hardware` / `name`）。
    device: miplay_core::XiaomiDevice,
    /// 这条流取什么声道。
    source: miplay_audio::ChannelSource,
    /// 下发给音箱的完整 URL。
    url: String,
    /// 是否正在投放（决定流循环是否继续）。
    active: AtomicBool,
    /// 当前拉流客户端数。
    clients: AtomicU64,
    /// 累计发出的字节。
    bytes_sent: AtomicU64,
    /// 因为客户端太慢被跳掉的字节。
    skipped_bytes: AtomicU64,
    /// 最近一个拉流客户端读到的绝对位置（用来算「它落后直播位置多少」）。
    last_pos: AtomicU64,
    /// 累计下发次数 / 失败次数。
    plays_sent: AtomicU64,
    play_failures: AtomicU64,
    last_client_at: Mutex<Option<Instant>>,
    last_play_at: Mutex<Option<Instant>>,
    last_error: Mutex<Option<String>>,
    /// 自上次有客户端连上以来，连续重发了多少次（退避用；连上就清零）。
    reissues: AtomicU64,
}

/// 一个 URL/路径 key 是否指向这台设备（did、MiNA uuid、id 都认）。
fn matches_key(id: &str, device: &miplay_core::XiaomiDevice, key: &str) -> bool {
    let k = strip_wav_suffix(key);
    id.eq_ignore_ascii_case(&k)
        || device.did.eq_ignore_ascii_case(&k)
        || device.device_id.eq_ignore_ascii_case(&k)
}

impl HubDevice {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
    }

    fn client_connected(&self) {
        self.clients.fetch_add(1, Ordering::Relaxed);
        *self.last_client_at.lock().unwrap() = Some(Instant::now());
        // 有人连上了：退避清零，下次掉线重新从快路径开始。
        self.reissues.store(0, Ordering::Relaxed);
    }

    /// 记录一次重发（返回**本次之前**已经连续重发的次数，用于算退避）。
    fn note_reissue(&self) -> u64 {
        self.reissues.fetch_add(1, Ordering::Relaxed)
    }

    fn reissue_count(&self) -> u64 {
        self.reissues.load(Ordering::Relaxed)
    }

    fn client_disconnected(&self) {
        let _ = self
            .clients
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                Some(c.saturating_sub(1))
            });
    }

    fn note_play(&self, ok: bool, err: Option<String>) {
        self.plays_sent.fetch_add(1, Ordering::Relaxed);
        *self.last_play_at.lock().unwrap() = Some(Instant::now());
        if ok {
            *self.last_error.lock().unwrap() = None;
        } else {
            self.play_failures.fetch_add(1, Ordering::Relaxed);
            *self.last_error.lock().unwrap() = err;
        }
    }

    fn seconds_since_last_client(&self) -> Option<f64> {
        self.last_client_at
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs_f64())
    }

    fn seconds_since_last_play(&self) -> Duration {
        self.last_play_at
            .lock()
            .unwrap()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO)
    }

    /// `lag_seconds`：这个客户端比「刚写进去的位置」落后多少秒
    /// （不是环里保留了多少历史——`RingBuffer::available()` 是保留量，会误导）。
    fn status(&self, lag_seconds: f64) -> Value {
        json!({
            "did": self.device.did,
            "device_id": self.device.device_id,
            "name": self.device.name,
            "label": self.device.label(),
            "hardware": self.device.hardware,
            "online": self.device.online(),
            "source": self.source.as_str(),
            "url": self.url,
            "active": self.is_active(),
            "clients": self.clients.load(Ordering::Relaxed),
            "lag_seconds": round2(lag_seconds),
            "ring_seconds": round2(RING_SECONDS as f64),
            "bytes_sent": self.bytes_sent.load(Ordering::Relaxed),
            "skipped_bytes": self.skipped_bytes.load(Ordering::Relaxed),
            "plays_sent": self.plays_sent.load(Ordering::Relaxed),
            "play_failures": self.play_failures.load(Ordering::Relaxed),
            "reissues": self.reissue_count(),
            "last_client_seconds_ago": self.seconds_since_last_client().map(round2),
            "last_error": self.last_error.lock().unwrap().clone(),
        })
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 采集端在 hub 里的投影（真正的 `AudioCapture` 由泵线程与 `run` 持有）。
struct CaptureInfo {
    device_name: String,
    negotiated: String,
    ring: Arc<miplay_audio::RingBuffer>,
    ring_seconds: f32,
    /// 泵线程已读到采集环的哪个位置。
    pump_cursor: Arc<AtomicU64>,
}

struct Hub {
    client: miplay_core::MinaClient,
    port: u16,
    url_base: String,
    gain: f32,
    split: bool,
    devices: Vec<Arc<HubDevice>>,
    router: Arc<miplay_audio::ChannelRouter>,
    capture: CaptureInfo,
    pump_stop: Arc<AtomicBool>,
    pump: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 泵线程最近一次真的分发出去音频的时刻（采集静默检测用）。
    last_audio_at: Arc<Mutex<Option<Instant>>>,
    started_at: Instant,
}

impl Hub {
    fn device(&self, key: &str) -> Option<Arc<HubDevice>> {
        self.devices
            .iter()
            .find(|d| matches_key(&d.id, &d.device, key))
            .cloned()
    }

    fn ids(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.id.clone()).collect()
    }

    /// 选中设备（`None` = 全部在册设备）。
    fn select(&self, keys: &Option<Vec<String>>) -> anyhow::Result<Vec<Arc<HubDevice>>> {
        match keys {
            None => Ok(self.devices.clone()),
            Some(list) => {
                let mut out: Vec<Arc<HubDevice>> = Vec::new();
                for k in list {
                    let dev = self.device(k).ok_or_else(|| {
                        anyhow::anyhow!(
                            "不在 --devices 名单里：`{k}`（在册：{}）",
                            self.ids().join(", ")
                        )
                    })?;
                    if !out.iter().any(|d| d.id == dev.id) {
                        out.push(dev);
                    }
                }
                Ok(out)
            }
        }
    }

    /// 某设备「最慢客户端」落后直播写入位置多少秒。
    fn lag_seconds(&self, dev: &HubDevice) -> f64 {
        let Some(ring) = self.router.ring(&dev.id) else {
            return 0.0;
        };
        let write = ring.cursor();
        let read = dev.last_pos.load(Ordering::Relaxed);
        if write <= read {
            0.0
        } else {
            (write - read) as f64 / BYTES_PER_SECOND as f64
        }
    }

    /// 采集环形缓冲的积压（泵线程落后采集多少秒）。
    fn capture_backlog_seconds(&self) -> f64 {
        let write = self.capture.ring.cursor();
        let read = self.capture.pump_cursor.load(Ordering::Relaxed);
        if write <= read {
            0.0
        } else {
            (write - read) as f64 / BYTES_PER_SECOND as f64
        }
    }

    /// 最近一次真的把音频分发出去到现在过了多久（服务刚起来还没数据时 = 启动至今）。
    fn seconds_since_audio(&self) -> f64 {
        self.last_audio_at
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or_else(|| self.started_at.elapsed().as_secs_f64())
    }

    fn audio_recent(&self) -> bool {
        self.seconds_since_audio() < AUDIO_RECENT.as_secs_f64()
    }

    fn status_json(&self) -> Value {
        let devices: Vec<Value> = self
            .devices
            .iter()
            .map(|d| d.status(self.lag_seconds(d)))
            .collect();
        let clients: u64 = self
            .devices
            .iter()
            .map(|d| d.clients.load(Ordering::Relaxed))
            .sum();
        json!({
            "ok": true,
            "running": true,
            "port": self.port,
            "url_base": self.url_base,
            "uptime_seconds": round2(self.started_at.elapsed().as_secs_f64()),
            "gain": self.gain,
            "split": self.split,
            "clients_total": clients,
            "capture": {
                "device": self.capture.device_name,
                "negotiated": self.capture.negotiated,
                "format": format!("{SAMPLE_RATE} Hz / {CHANNELS} ch / 16 bit"),
                "ring_seconds": self.capture.ring_seconds,
                "ring_retained_seconds": round2(
                    self.capture.ring.available() as f64 / BYTES_PER_SECOND as f64
                ),
                "backlog_seconds": round2(self.capture_backlog_seconds()),
                "seconds_since_audio": round2(self.seconds_since_audio()),
                "audio_recent": self.audio_recent(),
                "frames": self.router.frames(),
                "capturing": !self.pump_stop.load(Ordering::Relaxed),
            },
            "devices": devices,
        })
    }

    /// 给一台或多台设备下发 `play_url` 并置为 active。
    ///
    /// task-14 实测：立体声对的两只箱子若**串行**下发，先连上的那只可能在另一只连上后
    /// 约 0.2 s 断开；所以多台时**并发**下发（`join_all`），把这个窗口压到最小。
    async fn start_devices(&self, targets: &[Arc<HubDevice>]) -> Vec<Value> {
        let futures = targets.iter().map(|dev| async move {
            dev.set_active(true);
            let res = self.client.play_on(&dev.device, &dev.url).await;
            match &res {
                Ok(api) => {
                    dev.note_play(true, None);
                    tracing::info!(
                        "[下发] {} ({}) <- {}  [{}]",
                        dev.device.label(),
                        dev.id,
                        dev.url,
                        api.as_str()
                    );
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    dev.note_play(false, Some(msg.clone()));
                    tracing::warn!("[下发] {} ({}) 失败: {msg}", dev.device.label(), dev.id);
                }
            }
            json!({
                "did": dev.id,
                "label": dev.device.label(),
                "ok": res.is_ok(),
                "error": res.err().map(|e| format!("{e:#}")),
            })
        });
        futures_util::future::join_all(futures).await
    }

    /// 停止一台或多台设备：先关流（active=false），再让云端停播。
    async fn stop_devices(&self, targets: &[Arc<HubDevice>]) -> Vec<Value> {
        let mut out = Vec::new();
        for dev in targets {
            dev.set_active(false);
            let res = self.client.stop(&dev.device.device_id).await;
            match &res {
                Ok(ok) => tracing::info!("[停止] {} ({}) => {ok}", dev.device.label(), dev.id),
                Err(e) => tracing::warn!("[停止] {} ({}) 失败: {e:#}", dev.device.label(), dev.id),
            }
            out.push(json!({
                "did": dev.id,
                "label": dev.device.label(),
                "ok": res.unwrap_or(false),
            }));
        }
        out
    }

    /// 收尾：停播所有在投放的设备、停采集泵、关所有流。
    async fn shutdown(&self) {
        let active: Vec<Arc<HubDevice>> = self
            .devices
            .iter()
            .filter(|d| d.is_active())
            .cloned()
            .collect();
        if !active.is_empty() {
            self.stop_devices(&active).await;
        }
        self.pump_stop.store(true, Ordering::Relaxed);
        let handle = self.pump.lock().unwrap().take();
        if let Some(h) = handle {
            // 泵线程 ≤200ms 就会看到停止标志；join 放阻塞池，别卡住 runtime。
            let _ = tokio::task::spawn_blocking(move || h.join()).await;
        }
        self.router.close();
    }

    /// 断线重发：`active` 但长时间没有客户端 → 再下一次 `play_url`（带退避）。
    ///
    /// task-14 实测两条音箱行为决定了这里的策略：
    /// ① 立体声对的两只箱子先后下发时，先连上的那只可能在另一只连上后约 0.2 s 断开；
    /// ② 纯静音时音箱约 6.4 s 后主动关闭连接（10 次全部如此）。
    /// 所以「掉线重连」是**常态**：掉线走 2 s 快路径 + 退避上限，有声音时不吃退避。
    async fn reissue_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(REISSUE_SCAN);
        let mut last_silent_warn: Option<Instant> = None;
        loop {
            tick.tick().await;
            if self.pump_stop.load(Ordering::Relaxed) {
                return;
            }
            let audio_recent = self.audio_recent();
            // 采集断流要能看见，不能静默（Audacity 式地等用户来问「为什么没声音」）。
            if !audio_recent && self.devices.iter().any(|d| d.is_active()) {
                let silent = self.seconds_since_audio();
                let due = last_silent_warn
                    .map(|t| t.elapsed() >= Duration::from_secs(10))
                    .unwrap_or(true);
                if silent >= AUDIO_SILENT_WARN.as_secs_f64() && due {
                    tracing::warn!(
                        "[采集] 已经 {silent:.1} 秒没有新音频（音箱约 6.4 秒后会主动断开，重连会自动补上）"
                    );
                    last_silent_warn = Some(Instant::now());
                }
            } else {
                last_silent_warn = None;
            }
            for dev in self.devices.clone() {
                if !dev.is_active() || !dev.device.online() {
                    continue;
                }
                let since_client = dev.seconds_since_last_client();
                if !should_reissue(
                    dev.clients.load(Ordering::Relaxed),
                    since_client,
                    dev.seconds_since_last_play(),
                    dev.reissue_count(),
                    audio_recent,
                ) {
                    continue;
                }
                let wait = reissue_wait(since_client, dev.reissue_count(), audio_recent);
                let n = dev.note_reissue();
                tracing::info!(
                    "[重发] {} ({}) 已经 {} 秒没有客户端接入（第 {} 次，等待阈值 {} ms）",
                    dev.device.label(),
                    dev.id,
                    since_client
                        .map(|s| format!("{s:.1}"))
                        .unwrap_or_else(|| "∞".into()),
                    n + 1,
                    wait.as_millis()
                );
                let _ = self.start_devices(&[dev]).await;
            }
        }
    }
}

/// 重发判据（纯函数，好测）：在投放中、一个客户端都没有、距上次下发超过 [`REISSUE_MIN_GAP`]，
/// 并且「距上次客户端接入」已经超过 [`reissue_wait`] 算出来的等待时间。
///
/// * **从没连上过** → [`REISSUE_AFTER`]（8 s）；
/// * **连上过又掉了** → [`RECONNECT_AFTER`]（2 s）——task-14 实测音箱在纯静音约 6.4 s 后
///   会主动断开，所以掉线重连是常态，必须快；
/// * 连续重发都没人连上时按次数加倍退避，封顶 [`RECONNECT_BACKOFF_CAP`]；
/// * 但只要有声音正在播（`audio_recent`）就走快路径、不吃退避——否则静音一段时间后
///   用户重新放声音，可能要等满 30 s 才有人来拉流。
fn should_reissue(
    clients: u64,
    since_client: Option<f64>,
    since_play: Duration,
    reissues: u64,
    audio_recent: bool,
) -> bool {
    if clients > 0 || since_play < REISSUE_MIN_GAP {
        return false;
    }
    // 从没连上过时，「等了多久」只能看下发之后过了多久；连过又掉才是「距上次客户端接入」。
    let waited = match since_client {
        None => since_play.as_secs_f64(),
        Some(s) => s,
    };
    waited >= reissue_wait(since_client, reissues, audio_recent).as_secs_f64()
}

/// 下一次重发前要等多久（也是退避的唯一定义处）。
fn reissue_wait(since_client: Option<f64>, reissues: u64, audio_recent: bool) -> Duration {
    let base = match since_client {
        None => REISSUE_AFTER,
        Some(_) => RECONNECT_AFTER,
    };
    if audio_recent {
        return base;
    }
    let factor = 1u32 << reissues.min(4); // 1,2,4,8,16 倍
    base.checked_mul(factor)
        .unwrap_or(RECONNECT_BACKOFF_CAP)
        .min(RECONNECT_BACKOFF_CAP)
}

// ---------------------------------------------------------------- 网络与防火墙

/// 本机 LAN IP：UDP connect 一个外网地址，取内核选中的源地址（不会真发包）。
fn local_ip() -> anyhow::Result<IpAddr> {
    let sock = UdpSocket::bind(("0.0.0.0", 0))?;
    sock.connect(("8.8.8.8", 80))?;
    Ok(sock.local_addr()?.ip())
}

/// 尽力加一条 TCP 入站放行规则（失败只警告，不影响服务）。
fn ensure_firewall(port: u16) -> String {
    let name = format!("miplay-bridge hub {port}");
    let ps = format!(
        "if (-not (Get-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue)) \
         {{ New-NetFirewallRule -DisplayName '{name}' -Direction Inbound -Action Allow \
         -Protocol TCP -LocalPort {port} -Profile Any | Out-Null; 'added' }} else {{ 'exists' }}"
    );
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .output()
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if text.is_empty() {
                format!("未确认（{}）", String::from_utf8_lossy(&out.stderr).trim())
            } else {
                text
            }
        }
        Err(e) => format!("跳过（{e}）"),
    }
}

// ---------------------------------------------------------------- HTTP

/// 识别虚拟串流声卡端点用的名字关键字。
///
/// `miplay-bridge vad install` 会把机器上一个已有虚拟声卡（默认 Steam Streaming
/// Speakers）的设备名改掉，其端点显示名就变成 `扬声器 (小米妙播设备)`。
#[cfg(windows)]
const VAD_NAME_HINTS: &[&str] = &["小米妙播", "妙播设备"];

/// 找出虚拟串流声卡的 render 端点名。
///
/// 用户只要把播放器切到「扬声器 (小米妙播设备)」，声音就整条链走到 hub，
/// 不需要改系统默认播放设备。没装虚拟声卡时返回 `None`，行为与以前一致。
#[cfg(windows)]
fn virtual_endpoint_name() -> Option<String> {
    let eps = miplay_audio::list_endpoints().ok()?;
    eps.iter()
        .filter(|e| e.flow == miplay_audio::DataFlow::Render)
        .find(|e| VAD_NAME_HINTS.iter().any(|h| e.name.contains(h)))
        .map(|e| e.name.clone())
}

struct HubError(anyhow::Error);

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        tracing::warn!("hub API 错误: {msg}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": msg })),
        )
            .into_response()
    }
}

impl<E> From<E> for HubError
where
    E: Into<anyhow::Error>,
{
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

type HubResult<T> = std::result::Result<T, HubError>;

async fn hub_index() -> Html<String> {
    Html(crate::load_index_html())
}

async fn hub_health() -> &'static str {
    "ok"
}

#[derive(serde::Deserialize, Default)]
struct HubBody {
    #[serde(default)]
    devices: Option<Vec<String>>,
}

fn parse_body(body: &Bytes) -> HubBody {
    if body.is_empty() {
        return HubBody::default();
    }
    serde_json::from_slice(body).unwrap_or_default()
}

async fn api_hub_status(State(hub): State<Arc<Hub>>) -> Json<Value> {
    Json(hub.status_json())
}

async fn api_hub_start(State(hub): State<Arc<Hub>>, body: Bytes) -> HubResult<Json<Value>> {
    let req = parse_body(&body);
    let targets = hub.select(&req.devices)?;
    let results = hub.start_devices(&targets).await;
    let mut out = hub.status_json();
    out["started"] = json!(results);
    Ok(Json(out))
}

async fn api_hub_stop(State(hub): State<Arc<Hub>>, body: Bytes) -> HubResult<Json<Value>> {
    let req = parse_body(&body);
    let targets = hub.select(&req.devices)?;
    let results = hub.stop_devices(&targets).await;
    let mut out = hub.status_json();
    out["stopped"] = json!(results);
    Ok(Json(out))
}

/// 一个客户端的生命周期守卫：连接计数 + 结束日志。
struct ClientGuard {
    dev: Arc<HubDevice>,
    peer: SocketAddr,
    started: Instant,
    sent: u64,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.dev.client_disconnected();
        tracing::info!(
            "[流] {} ({}) 客户端 {} 结束：{:.2} MB / {:.1} 秒",
            self.dev.device.label(),
            self.dev.id,
            self.peer,
            self.sent as f64 / 1024.0 / 1024.0,
            self.started.elapsed().as_secs_f64()
        );
    }
}

struct StreamState {
    dev: Arc<HubDevice>,
    ring: Arc<miplay_audio::RingBuffer>,
    pos: u64,
    header: Option<Bytes>,
    done: bool,
    idle: bool,
    guard: ClientGuard,
}

async fn hub_stream(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(dev) = hub.device(&id) else {
        return (
            StatusCode::NOT_FOUND,
            format!("未知设备 `{id}`；在册：{}", hub.ids().join(", ")),
        )
            .into_response();
    };
    let Some(ring) = hub.router.ring(&dev.id) else {
        return (
            StatusCode::NOT_FOUND,
            format!("设备 {} 没有对应的流", dev.id),
        )
            .into_response();
    };

    dev.client_connected();
    tracing::info!(
        "[流] {} ({}) 客户端 {} 接入，从当前时刻开始",
        dev.device.label(),
        dev.id,
        peer
    );

    let header = miplay_audio::build_stream_wav_header(CHANNELS, SAMPLE_RATE);
    let state = StreamState {
        dev: dev.clone(),
        pos: ring.cursor(), // 从现在开始，不补历史
        ring,
        header: Some(Bytes::copy_from_slice(&header)),
        done: false,
        idle: false,
        guard: ClientGuard {
            dev,
            peer,
            started: Instant::now(),
            sent: 0,
        },
    };

    // 环形缓冲是阻塞 API → 必须 spawn_blocking。
    let stream = futures_util::stream::unfold(state, |mut st| async move {
        if let Some(h) = st.header.take() {
            return Some((Ok::<Bytes, std::convert::Infallible>(h), st));
        }
        if st.done || !st.dev.is_active() {
            // 返回 None 会 drop st → 守卫 Drop 里记账并打结束日志。
            return None;
        }
        let ring = st.ring.clone();
        let pos = st.pos;
        let read =
            tokio::task::spawn_blocking(move || ring.read_from(pos, STREAM_CHUNK, READ_TIMEOUT))
                .await;
        match read {
            Ok(Ok(out)) => {
                st.pos = out.next;
                st.dev.last_pos.store(out.next, Ordering::Relaxed);
                if out.skipped > 0 {
                    st.dev
                        .skipped_bytes
                        .fetch_add(out.skipped, Ordering::Relaxed);
                    tracing::debug!(
                        "[流] {} 客户端 {} 太慢，跳过 {} 字节",
                        st.dev.id,
                        st.guard.peer,
                        out.skipped
                    );
                }
                if !out.data.is_empty() {
                    let n = out.data.len() as u64;
                    st.guard.sent += n;
                    st.dev.bytes_sent.fetch_add(n, Ordering::Relaxed);
                    st.idle = false;
                    Some((Ok(Bytes::from(out.data)), st))
                } else if out.closed {
                    // 缓冲被 close（收尾）→ 结束响应；drop st 时守卫记账。
                    None
                } else {
                    // 读超时（采集还没数据）：吐一个空块保住连接。每次读都阻塞
                    // READ_TIMEOUT，所以这里天然不会忙等。
                    st.idle = true;
                    Some((Ok(Bytes::new()), st))
                }
            }
            // 采集/缓冲出错或读线程挂了：结束这条流（音箱会自己重连）。
            _ => {
                None
            }
        }
    });

    (
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (
                header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate",
            ),
            (header::PRAGMA, "no-cache"),
            (header::CONNECTION, "close"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// 跨源支持：`web/index.html` 也由 `serve`(8900) 提供，那时本面板是跨源调用 8931。
/// 只放开必要的头，且不涉及凭据（`*` 与 credentials 互斥，这里本来也不需要）。
async fn cors(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut res = if req.method() == axum::http::Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static("content-type"),
    );
    res
}

fn hub_router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(hub_index))
        .route("/health", get(hub_health))
        .route("/hub/{id}", get(hub_stream))
        .route("/api/hub/status", get(api_hub_status))
        .route("/api/hub/start", post(api_hub_start))
        .route("/api/hub/stop", post(api_hub_stop))
        .with_state(hub)
        .layer(axum::middleware::from_fn(cors))
}

// ---------------------------------------------------------------- 采集泵

/// 把采集环形缓冲里的 PCM 分发给每台设备的流（阻塞线程）。
fn spawn_pump(
    capture_ring: Arc<miplay_audio::RingBuffer>,
    router: Arc<miplay_audio::ChannelRouter>,
    stop: Arc<AtomicBool>,
    pump_cursor: Arc<AtomicU64>,
    last_audio_at: Arc<Mutex<Option<Instant>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hub-pump".into())
        .spawn(move || {
            let mut cursor = capture_ring.cursor();
            let mut total: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                match capture_ring.read_from(cursor, PUMP_CHUNK, READ_TIMEOUT) {
                    Ok(out) => {
                        cursor = out.next;
                        pump_cursor.store(cursor, Ordering::Relaxed);
                        if !out.data.is_empty() {
                            total += out.data.len() as u64;
                            router.write_s16_stereo(&out.data);
                            *last_audio_at.lock().unwrap() = Some(Instant::now());
                        }
                        if out.closed {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("采集环形缓冲读取失败: {e}");
                        std::thread::sleep(READ_TIMEOUT);
                    }
                }
            }
            tracing::info!(
                "[采集] 泵线程退出，共分发 {:.1} MB",
                total as f64 / 1024.0 / 1024.0
            );
        })
        .expect("起采集泵线程失败")
}

// ---------------------------------------------------------------- 入口

#[cfg(not(windows))]
pub async fn run(_argv: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("`hub` 依赖 WASAPI 回环采集，只在 Windows 上可用")
}

#[cfg(windows)]
pub use win::run;

/// 桌面 GUI 用的引擎外壳（把 `win::run` 那套装配流程塞进后台线程）。
#[cfg(windows)]
pub mod engine;

#[cfg(windows)]
mod win {
    use super::*;
    use anyhow::{bail, Context, Result};
    use miplay_audio::{
        AudioCapture, CaptureConfig, CaptureMode, ChannelRoute, ChannelRouter, ChannelSource,
    };
    use miplay_core::{MiToken, MinaClient, XiaomiDevice};

    pub async fn run(argv: &[String]) -> Result<()> {
        let Some(args) = Args::parse(argv)? else {
            println!("{HELP}");
            return Ok(());
        };

        // 1) 凭据 + 设备列表（下发改走小米云，必须要账号）
        let token = MiToken::load_default().context("加载凭据失败")?;
        let client = MinaClient::new(token)?;
        let all = client.device_list().await.context("获取设备列表失败")?;

        // 2) --stop：只停设备，不起服务
        if args.stop_only {
            let roster = build_roster(&all, &args, "http://127.0.0.1")?;
            for dev in &roster {
                match client.stop(&dev.device.device_id).await {
                    Ok(ok) => println!("[停止] {} ({}) => {ok}", dev.device.label(), dev.id),
                    Err(e) => println!("[停止] {} ({}) 失败: {e:#}", dev.device.label(), dev.id),
                }
            }
            return Ok(());
        }

        // 3) 本机 LAN IP / URL 前缀
        let ip = local_ip().context("探测本机 LAN IP 失败（网卡都断了吗？）")?;
        let url_base = args
            .url_base
            .clone()
            .unwrap_or_else(|| format!("http://{ip}:{}", args.port));
        let roster = build_roster(&all, &args, &url_base)?;

        // 4) 采集端点（先解析，别在采集坏掉时把音箱占了）
        let mut cap_cfg = CaptureConfig::default()
            .with_mode(CaptureMode::Loopback)
            .with_samplerate(SAMPLE_RATE)
            .with_channels(CHANNELS);
        cap_cfg.buffer_ms = CAPTURE_BUFFER_MS;
        let mut auto_vad: Option<String> = None;
        if let Some(name) = &args.capture_device {
            cap_cfg = cap_cfg.with_device_name(name.clone());
        } else if let Some(vad) = virtual_endpoint_name() {
            // 没显式指定采集端点时，优先抓虚拟串流声卡：用户在 Windows 输出列表里
            // 选中「扬声器 (小米妙播设备)」，声音就整条链走到我们的 hub。
            auto_vad = Some(vad.clone());
            cap_cfg = cap_cfg.with_device_name(vad);
        }
        cap_cfg.validate().context("采集参数不合法")?;
        let planned = miplay_audio::resolve_endpoint(&cap_cfg.device, CaptureMode::Loopback)
            .map(|ep| ep.describe())
            .unwrap_or_else(|e| format!("（解析失败：{e}）"));
        let capture = AudioCapture::new(cap_cfg).context("打开采集端点失败")?;

        println!("== miplay-bridge hub ==");
        println!("监听地址     {url_base}（0.0.0.0:{}）", args.port);
        println!("采集端点     {planned}");
        if let Some(vad) = &auto_vad {
            println!("采集来源     虚拟串流声卡「{vad}」——播放器切到它即可（--capture-device 可覆盖）");
        }
        println!(
            "采集格式     {SAMPLE_RATE} Hz / {CHANNELS} ch / 16 bit，每条流环形缓冲 {RING_SECONDS} 秒"
        );
        println!(
            "分发模式     {}",
            if args.split {
                "左右分离（第 1 台=左，第 2 台=右）"
            } else {
                "所有设备同一混音"
            }
        );
        println!("增益         {:.2}×", args.gain);
        for dev in &roster {
            println!(
                "目标设备     {} ({}) {} ← {} [{}]",
                dev.device.label(),
                dev.id,
                dev.device.hardware,
                dev.url,
                dev.source.as_str()
            );
        }

        if args.dry_run {
            println!("\n--dry-run：不建流、不下发、不起服务。");
            return Ok(());
        }

        // 5) 防火墙（尽力）
        if args.no_firewall {
            println!("防火墙       跳过（--no-firewall）");
        } else {
            println!("防火墙       TCP {} 入站: {}", args.port, ensure_firewall(args.port));
        }

        // 6) 路由器 + 采集
        let routes: Vec<ChannelRoute> = roster
            .iter()
            .map(|d| ChannelRoute::new(d.id.clone(), d.source).with_gain(args.gain))
            .collect();
        ChannelRouter::validate(&routes)?;
        let router = Arc::new(ChannelRouter::for_seconds(
            routes,
            SAMPLE_RATE,
            CHANNELS,
            RING_SECONDS,
        ));

        let mut capture = capture;
        capture.start().context("启动采集失败")?;
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
            port: args.port,
            url_base: url_base.clone(),
            gain: args.gain,
            split: args.split,
            devices: roster.clone(),
            router: router.clone(),
            capture: CaptureInfo {
                device_name: capture.device_name().unwrap_or("（未知）").to_string(),
                negotiated: negotiated.clone(),
                ring: capture_ring,
                ring_seconds: RING_SECONDS,
                pump_cursor: pump_cursor.clone(),
            },
            pump_stop: pump_stop.clone(),
            pump: Mutex::new(Some(pump)),
            last_audio_at,
            started_at: Instant::now(),
        });

        // 7) 起服务
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], args.port)))
            .await
            .with_context(|| format!("端口 {} 被占用？", args.port))?;
        println!("\n网页控制     http://{ip}:{}/", args.port);
        println!("拉流地址     {url_base}/hub/<did>.wav");
        println!("已协商格式   {negotiated}");
        println!(
            "实际端点     {}",
            capture.device_name().unwrap_or("（未知）")
        );

        if !args.no_autostart {
            // 让环形缓冲先垫一点数据，音箱一连上就有声音（与 Python 版一致）。
            tokio::time::sleep(PREFILL).await;
            if let Some(v) = args.volume {
                for dev in &roster {
                    match hub.client.set_volume(&dev.device.device_id, v).await {
                        Ok(ok) => println!("[音量] {} => {v}% ({ok})", dev.device.label()),
                        Err(e) => println!("[音量] {} 失败: {e:#}", dev.device.label()),
                    }
                }
            }
            for r in &hub.start_devices(&roster).await {
                if r["ok"] != json!(true) {
                    println!("[下发] {} 失败：{}", r["label"], r["error"]);
                }
            }
        } else {
            println!("--no-autostart：等网页里点「投放到选中设备」");
        }

        let reissue = hub.clone();
        tokio::spawn(async move { reissue.reissue_loop().await });

        let seconds = args.seconds;
        let app = hub_router(hub.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let mut serve = std::pin::pin!(std::future::IntoFuture::into_future(axum::serve(
            listener, app
        )));
        let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
        let mut deadline = std::pin::pin!(async move {
            match seconds {
                Some(s) => tokio::time::sleep(Duration::from_secs_f64(s)).await,
                None => std::future::pending::<()>().await,
            }
        });
        // 采集健康看护：拔耳机 / 切默认输出端点会让采集线程直接退出，那时流还在「投放中」，
        // 光看「已静默」分不出「系统本来就没声音」和「采集死了」，所以要单独喊一声。
        let mut health = tokio::time::interval(Duration::from_secs(3));
        // Option<Option<String>>：外层 None = 还没看过一眼，内层 None = 健康。
        // 这样第一拍就能把「采集线程正常」打出来，之后只在状态**变化**时再喊。
        let mut health_seen: Option<Option<String>> = None;
        loop {
            tokio::select! {
                r = &mut serve => {
                    r.context("HTTP 服务异常退出")?;
                    break;
                }
                _ = &mut ctrl_c => {
                    println!("\n收到 Ctrl-C，停止投放…");
                    break;
                }
                _ = &mut deadline => {
                    println!("\n--seconds 到点，停止投放…");
                    break;
                }
                _ = health.tick() => {
                    let now = match (capture.is_running(), capture.last_error()) {
                        (true, None) => None,
                        (true, Some(e)) => Some(format!("采集异常：{e}")),
                        (false, Some(e)) => Some(format!("采集线程已退出：{e}")),
                        (false, None) => Some("采集线程已退出（输出设备被拔掉或切换了？）".to_string()),
                    };
                    if health_seen.as_ref() != Some(&now) {
                        match &now {
                            Some(msg) => tracing::warn!("[采集] {msg}"),
                            None => tracing::info!("[采集] 采集线程正常"),
                        }
                        health_seen = Some(now);
                    }
                }
            }
        }

        hub.shutdown().await;

        println!("\n=== 结束 ===");
        println!("运行时长     {:.1} s", hub.started_at.elapsed().as_secs_f64());
        println!("分发帧数     {}", router.frames());
        for dev in &roster {
            println!(
                "{:<16} 下发 {} 次（失败 {}）· 发流 {:.2} MB · 跳帧 {} B · 残留客户端 {}",
                dev.device.label(),
                dev.plays_sent.load(Ordering::Relaxed),
                dev.play_failures.load(Ordering::Relaxed),
                dev.bytes_sent.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0,
                dev.skipped_bytes.load(Ordering::Relaxed),
                dev.clients.load(Ordering::Relaxed),
            );
        }
        if let Some(e) = capture.last_error() {
            println!("采集错误     {e}");
        }
        Ok(())
    }

    /// 名单 → 设备列表（did/uuid 都认），并给每条流定好声道源。
    ///
    /// `pub(crate)`：桌面 GUI 的引擎（`hub::engine`）要复用它装配同一份名单。
    pub(crate) fn build_roster(
        all: &[XiaomiDevice],
        args: &Args,
        url_base: &str,
    ) -> Result<Vec<Arc<HubDevice>>> {
        let mut out: Vec<Arc<HubDevice>> = Vec::new();
        for (idx, key) in args.devices.iter().enumerate() {
            let dev = MinaClient::find(all, key)
                .with_context(|| format!("设备 `{key}` 不在账号下"))?
                .clone();
            let id = if dev.did.is_empty() {
                dev.device_id.clone()
            } else {
                dev.did.clone()
            };
            if out.iter().any(|d| d.id == id) {
                continue;
            }
            // 声道分配：--split 时前两台一左一右，其余（以及非 split 模式）用混音。
            let source = if args.split {
                match (idx, args.swap) {
                    (0, false) | (1, true) => ChannelSource::Left,
                    (0, true) | (1, false) => ChannelSource::Right,
                    _ => ChannelSource::Mixdown,
                }
            } else {
                ChannelSource::Mixdown
            };
            let url = format!("{url_base}/hub/{id}.wav");
            out.push(Arc::new(HubDevice {
                id,
                device: dev,
                source,
                url,
                active: AtomicBool::new(false),
                clients: AtomicU64::new(0),
                bytes_sent: AtomicU64::new(0),
                skipped_bytes: AtomicU64::new(0),
                last_pos: AtomicU64::new(0),
                plays_sent: AtomicU64::new(0),
                play_failures: AtomicU64::new(0),
                last_client_at: Mutex::new(None),
                last_play_at: Mutex::new(None),
                last_error: Mutex::new(None),
                reissues: AtomicU64::new(0),
            }));
        }
        if out.is_empty() {
            bail!("名单是空的");
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use miplay_audio::{ChannelRoute, ChannelRouter, ChannelSource};

    fn s16(v: i16) -> [u8; 2] {
        v.to_le_bytes()
    }

    fn test_device(did: &str, uuid: &str, name: &str) -> Arc<HubDevice> {
        Arc::new(HubDevice {
            id: did.to_string(),
            device: miplay_core::XiaomiDevice::from_value(json!({
                "deviceID": uuid,
                "miotDID": did,
                "name": name,
                "hardware": "L16A",
            })),
            source: ChannelSource::Left,
            url: format!("http://192.168.1.4:8931/hub/{did}.wav"),
            active: AtomicBool::new(false),
            clients: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            skipped_bytes: AtomicU64::new(0),
            last_pos: AtomicU64::new(0),
            plays_sent: AtomicU64::new(0),
            play_failures: AtomicU64::new(0),
            last_client_at: Mutex::new(None),
            last_play_at: Mutex::new(None),
            last_error: Mutex::new(None),
            reissues: AtomicU64::new(0),
        })
    }

    #[test]
    fn stream_wav_header_is_44_bytes_and_well_formed() {
        let h = miplay_audio::build_stream_wav_header(CHANNELS, SAMPLE_RATE);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(&h[36..40], b"data");
        // riff size = 36 + data size（假装 2GB 的流式写法）
        let riff = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        let data = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        assert_eq!(riff, 36u32.wrapping_add(data));
        assert_eq!(data, miplay_audio::FAKE_DATA_SIZE);
        // fmt 块：PCM / 2ch / 44100 / byte_rate / block_align / 16bit
        assert_eq!(u16::from_le_bytes([h[20], h[21]]), 1);
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), CHANNELS);
        assert_eq!(
            u32::from_le_bytes([h[24], h[25], h[26], h[27]]),
            SAMPLE_RATE
        );
        assert_eq!(
            u32::from_le_bytes([h[28], h[29], h[30], h[31]]),
            BYTES_PER_SECOND
        );
        assert_eq!(u16::from_le_bytes([h[32], h[33]]), 4);
        assert_eq!(u16::from_le_bytes([h[34], h[35]]), 16);
    }

    /// task-14 实测：读块从 50 ms 缩到 10 ms 能把 PC 侧采集链路从 31.1 ms 压到 12.4 ms。
    /// 这个常量很容易被后来的人「优化」成更大的块，所以钉住它。
    #[test]
    fn pump_chunk_is_ten_milliseconds_of_audio() {
        assert_eq!(PUMP_CHUNK, 1764);
        assert_eq!(PUMP_CHUNK % 4, 0, "必须按 4 字节帧对齐");
        let ms = PUMP_CHUNK as f64 * 1000.0 / BYTES_PER_SECOND as f64;
        assert!((ms - 10.0).abs() < 0.5, "读块应当约 10 ms，实际 {ms} ms");
    }

    #[test]
    fn left_and_right_routes_split_channels() {
        let router = ChannelRouter::new(
            vec![
                ChannelRoute::new("L", ChannelSource::Left),
                ChannelRoute::new("R", ChannelSource::Right),
            ],
            4096,
        );
        // 两帧：(L=1000, R=-2000)
        let mut pcm = Vec::new();
        for _ in 0..2 {
            pcm.extend_from_slice(&s16(1000));
            pcm.extend_from_slice(&s16(-2000));
        }
        assert_eq!(router.write_s16_stereo(&pcm), 2);

        let l = router.ring("L").unwrap();
        let r = router.ring("R").unwrap();
        let lo = l.read_from(0, 4096, Duration::ZERO).unwrap();
        let ro = r.read_from(0, 4096, Duration::ZERO).unwrap();
        // 每条流都是 2ch：左流两声道都是 1000，右流两声道都是 -2000
        assert_eq!(lo.data.len(), 8);
        assert_eq!(ro.data.len(), 8);
        for chunk in lo.data.chunks_exact(2) {
            assert_eq!(i16::from_le_bytes([chunk[0], chunk[1]]), 1000);
        }
        for chunk in ro.data.chunks_exact(2) {
            assert_eq!(i16::from_le_bytes([chunk[0], chunk[1]]), -2000);
        }
    }

    #[test]
    fn mixdown_route_averages_and_gain_scales() {
        let router = ChannelRouter::new(
            vec![ChannelRoute::new("M", ChannelSource::Mixdown).with_gain(0.5)],
            4096,
        );
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&s16(1000));
        pcm.extend_from_slice(&s16(3000));
        router.write_s16_stereo(&pcm);
        let m = router.ring("M").unwrap();
        let out = m.read_from(0, 4096, Duration::ZERO).unwrap();
        // 混音 = (1000+3000)/2 = 2000，再乘 0.5 = 1000
        assert_eq!(out.data.len(), 4);
        assert_eq!(i16::from_le_bytes([out.data[0], out.data[1]]), 1000);
        assert_eq!(i16::from_le_bytes([out.data[2], out.data[3]]), 1000);
    }

    #[test]
    fn slow_client_skips_instead_of_blocking() {
        let router = ChannelRouter::new(vec![ChannelRoute::new("A", ChannelSource::Stereo)], 1024);
        let ring = router.ring("A").unwrap();
        // 写 2 倍容量的数据，读者游标停在 0（太慢了）：
        // 每轮 64 帧 × 4 字节 = 256 B，8 轮 = 2048 B > 1024 B 容量。
        for i in 0..8 {
            let mut pcm = Vec::new();
            for _ in 0..64 {
                pcm.extend_from_slice(&s16(i as i16));
                pcm.extend_from_slice(&s16(i as i16));
            }
            router.write_s16_stereo(&pcm);
        }
        let started = Instant::now();
        let out = ring
            .read_from(0, 4096, Duration::from_millis(50))
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "慢客户端必须立刻返回，不能等"
        );
        assert!(out.skipped > 0, "落后超过容量应该报告跳帧");
        assert!(!out.data.is_empty(), "跳帧后仍应拿到最新数据");
        assert!(out.next > out.skipped, "游标必须往前推进");
    }

    #[test]
    fn empty_ring_read_times_out_without_error() {
        let ring = miplay_audio::RingBuffer::new(1024);
        let started = Instant::now();
        let out = ring.read_from(0, 512, Duration::from_millis(60)).unwrap();
        assert!(out.data.is_empty());
        assert!(!out.closed);
        assert_eq!(out.skipped, 0);
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn strip_wav_suffix_handles_real_url_shapes() {
        assert_eq!(strip_wav_suffix("501718792.wav"), "501718792");
        assert_eq!(strip_wav_suffix("/hub/501718792.wav"), "501718792");
        assert_eq!(strip_wav_suffix("/hub/501718792.wav?x=1"), "501718792");
        assert_eq!(strip_wav_suffix("/hub/501718792"), "501718792");
        assert_eq!(strip_wav_suffix("/hub/501718792.WAV"), "501718792");
        assert_eq!(strip_wav_suffix("/hub/501718792/"), "501718792");
    }

    #[test]
    fn parse_devices_trims_and_dedupes() {
        assert_eq!(
            parse_devices(" 501718792 , 934947888 ,,501718792 "),
            vec!["501718792".to_string(), "934947888".to_string()]
        );
        assert!(parse_devices(" , ").is_empty());
    }

    #[test]
    fn reissue_rules() {
        let gap = Duration::from_secs(30);
        // 有客户端 → 永不重发
        assert!(!should_reissue(1, Some(60.0), gap, 0, true));
        // 刚下发过 → 先等等（最小间隔 2 s）
        assert!(!should_reissue(0, Some(60.0), Duration::from_secs(1), 0, true));
        // 从没来过客户端：阈值是 8 s（下发后 6 s 还不够，9 s 够）
        assert!(!should_reissue(0, None, Duration::from_secs(6), 0, false));
        assert!(should_reissue(0, None, Duration::from_secs(9), 0, false));
        // 连上过又掉了：阈值 2 s（task-14：音箱静音 6.4 s 会自己断开，必须快）
        assert!(should_reissue(0, Some(2.5), gap, 0, false));
        assert!(!should_reissue(0, Some(1.0), gap, 0, false));
        // 没有声音时的退避：连掉 3 次后阈值 = 2 s × 8 = 16 s
        assert!(!should_reissue(0, Some(9.0), gap, 3, false));
        assert!(should_reissue(0, Some(17.0), gap, 3, false));
        // 有声音正在播 → 走快路径，不吃退避（否则重新放声音要等满 30 s）
        assert!(should_reissue(0, Some(2.5), gap, 9, true));
        // 退避封顶 30 s
        assert_eq!(
            reissue_wait(Some(1.0), 99, false),
            RECONNECT_BACKOFF_CAP,
            "退避必须封顶，否则一次长静音会退到几小时之后"
        );
    }

    #[test]
    fn args_parse_rejects_bad_values_and_accepts_good_ones() {
        let ok = Args::parse(&[
            "--devices".into(),
            "501718792,934947888".into(),
            "--gain".into(),
            "0.7".into(),
            "--port".into(),
            "8931".into(),
            "--split".into(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(ok.devices.len(), 2);
        assert!((ok.gain - 0.7).abs() < 1e-6);
        assert!(ok.split);

        assert!(
            Args::parse(&["--devices".into(), "1".into(), "--gain".into(), "9".into()]).is_err()
        );
        assert!(
            Args::parse(&["--devices".into(), "1".into(), "--port".into(), "0".into()]).is_err()
        );
        assert!(Args::parse(&["--devices".into(), "1".into(), "--bogus".into()]).is_err());
        // 没有 --devices 且不是 dry-run → 报错
        assert!(Args::parse(&["--gain".into(), "1".into()]).is_err());
        // --help 走 Ok(None)
        assert!(Args::parse(&["--help".into()]).unwrap().is_none());
    }

    #[test]
    fn device_lookup_matches_did_uuid_and_url_shapes() {
        let dev = test_device("501718792", "f58c6453-0268-4df5-a220-1335bb84172b", "左");
        assert!(matches_key(&dev.id, &dev.device, "501718792"));
        assert!(matches_key(&dev.id, &dev.device, "501718792.wav"));
        assert!(matches_key(&dev.id, &dev.device, "/hub/501718792.wav"));
        assert!(matches_key(
            &dev.id,
            &dev.device,
            "f58c6453-0268-4df5-a220-1335bb84172b"
        ));
        assert!(!matches_key(&dev.id, &dev.device, "934947888"));
    }

    #[test]
    fn client_counter_never_underflows() {
        let dev = test_device("x", "y", "x");
        dev.client_disconnected();
        assert_eq!(dev.clients.load(Ordering::Relaxed), 0);
        dev.client_connected();
        dev.client_connected();
        assert_eq!(dev.clients.load(Ordering::Relaxed), 2);
        dev.client_disconnected();
        dev.client_disconnected();
        dev.client_disconnected();
        assert_eq!(dev.clients.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn device_status_reports_expected_fields() {
        let dev = test_device("501718792", "uuid", "左");
        dev.set_active(true);
        dev.client_connected();
        dev.note_play(false, Some("boom".into()));
        let s = dev.status(4.25);
        assert_eq!(s["active"], json!(true));
        assert_eq!(s["clients"], json!(1));
        assert_eq!(s["lag_seconds"], json!(4.25));
        assert_eq!(s["source"], json!("left"));
        assert_eq!(s["play_failures"], json!(1));
        assert_eq!(s["last_error"], json!("boom"));
        assert_eq!(s["url"], json!("http://192.168.1.4:8931/hub/501718792.wav"));
    }
}
