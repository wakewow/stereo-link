//! 端点状态后台轮询：把「周期性」的 WASAPI / COM 调用全部搬离 UI 线程。
//!
//! 背景：这些调用单次 5–30 ms（实测本机 `list_endpoints()` 平均 18 ms、最坏 29 ms，
//! `get_endpoint_volume()` 5–8 ms），之前都塞在 `update()` 里每 250–900 ms 跑一次，
//! 拖动窗口时直接堵住 winit 的消息循环（每帧掉到 30–50 ms）。
//!
//! 现在只有一条后台线程 `stereo-link-endpoint-poll` 每 250 ms 醒一次，分时轮转：
//! - 每 tick：默认输出（**默认输出放在主线程是拖动卡顿的主因**，这里 ≤7 ms）；
//! - 1/4 tick：采集源音量；跟随开启时在这里完成音量/静音镜像；
//! - 2/4 tick：两路输出设备音量（音量条回读）；
//! - 每 16 tick（约 4 s）：枚举端点 + 读每个端点的设备格式（这块最贵，低频跑）。
//!
//! UI 线程只读 [`EndpointSnapshot`]（纯内存克隆），每帧零 COM。
//! 采集源音量变化时线程会 `request_repaint()` 并唤醒隐藏到托盘的主窗口，
//! 因此原先独立的 `VolumeWaker` 线程已合并进来。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use stereo_audio::{
    get_endpoint_volume, list_endpoints, set_endpoint_mute, set_endpoint_volume, EndpointFormat,
    EndpointInfo, EndpointVolume,
};

/// 轮询周期（UI 的心跳也按这个节奏请求重绘）。
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 枚举端点 + 读设备格式的周期（tick 数）：约 4 秒一次，足够，且单次 ~60 ms 在后台无感。
const ENUMERATE_EVERY_TICKS: u64 = 16;

/// UI 写给后台线程的「要盯什么」（纯内存拷贝，构造它本身零 COM）。
#[derive(Clone, Default)]
pub(crate) struct PollInput {
    /// 采集源的引擎 selector（名字或 `{GUID}`），读/写音量用。
    pub capture_selector: Option<String>,
    /// 采集端点 id，判断「默认输出 == 采集源」用。
    pub capture_id: Option<String>,
    /// 左/右两路输出设备的 selector（按 [左, 右] 顺序，空串表示没配置）。
    pub route_selectors: [String; 2],
    /// 是否开启系统音量跟随（引擎运行 + 联动模式允许）。
    pub follow_active: bool,
    /// 需要镜像音量的目标 selector（去重后的左/右名字）。
    pub follow_targets: Vec<String>,
}

/// 后台线程发布的只读快照。UI 每帧克隆一份，字段都是纯数据。
#[derive(Clone, Default)]
pub(crate) struct EndpointSnapshot {
    /// 全部端点（首次枚举完成前为空）。
    pub endpoints: Vec<EndpointInfo>,
    /// 是否成功枚举过至少一次。
    pub endpoints_loaded: bool,
    /// 最近一次枚举失败的原因。
    pub endpoints_error: Option<String>,
    /// 每个端点的设备默认格式（`None` = 该端点读失败）。
    pub formats: Vec<(String, Option<EndpointFormat>)>,
    /// 是否已经带上设备格式（跟着枚举一起更新）。
    pub formats_loaded: bool,
    /// Windows 当前默认播放端点。
    pub default_output: Option<EndpointInfo>,
    /// 默认输出是否就是采集源。
    pub default_is_capture: bool,
    /// 采集源端点当前音量。
    pub source_volume: Option<EndpointVolume>,
    /// 采集源音量变化计数（UI 只用于判断「有新值可显示」）。
    pub source_volume_revision: u64,
    /// 两路输出设备的音量（`[左, 右]`，没配置的为 `None`）。
    pub route_volumes: [Option<f32>; 2],
    /// 音量跟随期间写失败的错误（显示在界面上）。
    pub follow_error: Option<String>,
    /// 跟随期间记下的「目标设备原始音量」，退出时据此还原。
    pub follow_saved: Vec<(String, f32, bool)>,
    /// 快照最近更新时间。
    pub updated_at: Option<Instant>,
}

/// UI 发给后台线程的一次性命令。
#[derive(Default)]
struct Requests {
    refresh_endpoints: bool,
    refresh_default: bool,
    restore_volumes: bool,
    set_source_volume: Option<f32>,
    /// `(selector, scalar)`：音量条拖完后写回设备。
    set_route_volumes: Vec<(String, f32)>,
}

struct Inner {
    input: Mutex<PollInput>,
    requests: Mutex<Requests>,
    snapshot: Mutex<EndpointSnapshot>,
    stop: AtomicBool,
    ctx: egui::Context,
}

/// 端点轮询线程的句柄（Drop 时停线程并 join）。
pub(crate) struct EndpointPoll {
    inner: Arc<Inner>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl EndpointPoll {
    /// 起线程并尽量等到第一份快照（最多 `timeout`），让首帧就有端点列表。
    pub(crate) fn spawn(ctx: egui::Context, timeout: Duration) -> Self {
        let inner = Arc::new(Inner {
            input: Mutex::new(PollInput::default()),
            requests: Mutex::new(Requests::default()),
            snapshot: Mutex::new(EndpointSnapshot::default()),
            stop: AtomicBool::new(false),
            ctx,
        });
        let (initial_tx, initial_rx) = mpsc::channel::<()>();
        let thread_inner = Arc::clone(&inner);
        let join = std::thread::Builder::new()
            .name("stereo-link-endpoint-poll".to_string())
            .spawn(move || run(thread_inner, Some(initial_tx)))
            .ok();
        let poll = Self { inner, join };
        // 等第一份快照：首帧就能画出端点列表（避免启动瞬间的空白/自动开始失败）。
        let deadline = Instant::now() + timeout;
        if let Ok(()) = initial_rx.recv_timeout(timeout) {
            // 收到「首份快照已发布」信号。
        } else {
            let _ = deadline;
        }
        poll
    }

    /// UI 每帧把最新配置写进来（下一次 tick 生效）。
    pub(crate) fn set_input(&self, input: PollInput) {
        if let Ok(mut slot) = self.inner.input.lock() {
            *slot = input;
        }
    }

    /// 请求重新枚举端点（「刷新设备」按钮）。
    pub(crate) fn request_refresh(&self) {
        if let Ok(mut req) = self.inner.requests.lock() {
            req.refresh_endpoints = true;
            req.refresh_default = true;
        }
    }

    /// 请求立即刷新默认输出（切换默认输出之后调用）。
    pub(crate) fn request_default_now(&self) {
        if let Ok(mut req) = self.inner.requests.lock() {
            req.refresh_default = true;
        }
    }

    /// 请求还原跟随期间的音量（停止路由时调用）。
    pub(crate) fn request_restore_volumes(&self) {
        if let Ok(mut req) = self.inner.requests.lock() {
            req.restore_volumes = true;
        }
    }

    /// 请求把采集源音量设为固定值（「设为 100%」按钮）。
    pub(crate) fn request_set_source_volume(&self, scalar: f32) {
        if let Ok(mut req) = self.inner.requests.lock() {
            req.set_source_volume = Some(scalar);
        }
    }

    /// 请求把某路输出设备音量写回端点（音量条拖完）。
    pub(crate) fn request_set_route_volume(&self, selector: String, scalar: f32) {
        if let Ok(mut req) = self.inner.requests.lock() {
            req.set_route_volumes.push((selector, scalar));
        }
    }

    /// 读当前快照（克隆，无锁竞争：后台只在 tick 末尾短暂持锁替换）。
    pub(crate) fn snapshot(&self) -> EndpointSnapshot {
        self.inner
            .snapshot
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }
}

impl Drop for EndpointPoll {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

/// 退出时同步还原目标设备音量。
///
/// 退出路径必须**同步**完成（后台线程可能来不及跑一次 tick 进程就没了），所以保留
/// 这个直连 COM 的辅助函数；它只在 `on_exit` 里调用一次，不在每帧路径上。
pub(crate) fn restore_volumes_blocking(saved: &[(String, f32, bool)]) {
    for (selector, scalar, muted) in saved {
        let _ = set_endpoint_volume(selector, *scalar);
        let _ = set_endpoint_mute(selector, *muted);
    }
}

fn is_recent(now: Instant, at: Instant, threshold: Duration) -> bool {
    now.saturating_duration_since(at) < threshold
}

/// 后台线程主循环。
fn run(inner: Arc<Inner>, initial_tx: Option<mpsc::Sender<()>>) {
    let mut tick: u64 = 0;
    let mut snapshot = EndpointSnapshot::default();
    let mut saved: Vec<(String, f32, bool)> = Vec::new();
    let mut last_source: Option<(f32, bool)> = None;
    let mut follow_error: Option<String> = None;
    let mut initial_sent = false;
    // 记录「最近一次写回」的时间，避免后台读回时把刚拖好的音量又覆盖掉。
    let mut written_at: [Option<Instant>; 2] = [None, None];

    while !inner.stop.load(Ordering::Relaxed) {
        let input = inner
            .input
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let mut req = inner
            .requests
            .lock()
            .map(|mut slot| std::mem::take(&mut *slot))
            .unwrap_or_default();

        let now = Instant::now();
        let force_enum = req.refresh_endpoints || snapshot.updated_at.is_none();
        let force_default = req.refresh_default || snapshot.updated_at.is_none();

        // ---- 1) 默认输出（每 tick 都读：它变了 UI 要立刻反映） ----
        if force_default || tick.is_multiple_of(2) {
            let default = crate::audio_default::default_render();
            snapshot.default_is_capture = match (&default, input.capture_id.as_deref()) {
                (Some(current), Some(capture_id)) => current.id.eq_ignore_ascii_case(capture_id),
                _ => false,
            };
            snapshot.default_output = default;
        }

        // ---- 2) 采集源音量（每 tick；跟随写也在这一步做完） ----
        if let Some(source) = input.capture_selector.as_deref().filter(|s| !s.is_empty()) {
            match get_endpoint_volume(source) {
                Ok(volume) => {
                    let changed = last_source
                        .map(|(scalar, muted)| {
                            (scalar - volume.scalar).abs() >= 0.005 || muted != volume.muted
                        })
                        .unwrap_or(true);
                    if changed {
                        let big = last_source
                            .map(|(scalar, muted)| {
                                (scalar - volume.scalar).abs() >= 0.03 || muted != volume.muted
                            })
                            .unwrap_or(true);
                        last_source = Some((volume.scalar, volume.muted));
                        snapshot.source_volume_revision =
                            snapshot.source_volume_revision.wrapping_add(1);
                        snapshot.source_volume = Some(volume);
                        follow_error = None;
                        if big {
                            tracing::info!(
                                "[音量] 跟随采集源 {:.0}%{}",
                                volume.scalar * 100.0,
                                if volume.muted { "（静音）" } else { "" }
                            );
                        }
                        if input.follow_active {
                            apply_follow(&input, &volume, &mut saved, &mut follow_error);
                        }
                        // 唤醒 UI：可见窗口靠重绘；隐藏到托盘的窗口靠 tray 的唤醒消息。
                        inner.ctx.request_repaint();
                        unsafe { crate::tray::wake_hidden_main_window() };
                    } else {
                        snapshot.source_volume = Some(volume);
                    }
                }
                Err(err) => {
                    if follow_error.is_none() {
                        follow_error = Some(format!("读不到采集源音量：{err}"));
                    }
                }
            }
        }

        // ---- 3) 两路输出设备音量（每 2 tick 一次即可） ----
        if tick % 2 == 1 {
            for (index, selector) in input.route_selectors.iter().enumerate() {
                let selector = selector.trim();
                if selector.is_empty() {
                    snapshot.route_volumes[index] = None;
                    continue;
                }
                // 刚拖完的 600 ms 内不读回，避免音量条跳。
                if written_at[index].map(|at| is_recent(now, at, Duration::from_millis(600)))
                    == Some(true)
                {
                    continue;
                }
                snapshot.route_volumes[index] = get_endpoint_volume(selector)
                    .ok()
                    .map(|v| v.scalar.clamp(0.0, 1.0));
            }
        }

        // ---- 4) 一次性命令：还原音量 / 写采集源音量 / 写某路音量 ----
        if req.restore_volumes {
            restore_volumes_blocking(&saved);
            saved.clear();
            last_source = None;
            follow_error = None;
            snapshot.follow_saved.clear();
            snapshot.follow_error = None;
        }
        if let Some(scalar) = req.set_source_volume.take() {
            if let Some(source) = input.capture_selector.as_deref().filter(|s| !s.is_empty()) {
                match set_endpoint_volume(source, scalar) {
                    Ok(v) => {
                        snapshot.source_volume = Some(v);
                        last_source = Some((v.scalar, v.muted));
                    }
                    Err(err) => follow_error = Some(format!("设置采集源音量失败：{err}")),
                }
            }
        }
        for (selector, scalar) in req.set_route_volumes.drain(..) {
            match set_endpoint_volume(&selector, scalar) {
                Ok(v) => {
                    if let Some(index) = input
                        .route_selectors
                        .iter()
                        .position(|s| s.eq_ignore_ascii_case(&selector))
                    {
                        snapshot.route_volumes[index] = Some(v.scalar.clamp(0.0, 1.0));
                        written_at[index] = Some(Instant::now());
                    }
                }
                Err(err) => follow_error = Some(format!("写设备音量失败：{err}")),
            }
        }

        // ---- 5) 低频：枚举端点 + 读设备格式 ----
        if force_enum || tick.is_multiple_of(ENUMERATE_EVERY_TICKS) {
            match list_endpoints() {
                Ok(endpoints) => {
                    let formats = endpoints
                        .iter()
                        .filter(|ep| ep.flow == stereo_audio::DataFlow::Render)
                        .map(|ep| {
                            (
                                ep.id.clone(),
                                stereo_audio::get_endpoint_format(&ep.id).ok(),
                            )
                        })
                        .collect();
                    snapshot.endpoints = endpoints;
                    snapshot.formats = formats;
                    snapshot.formats_loaded = true;
                    snapshot.endpoints_loaded = true;
                    snapshot.endpoints_error = None;
                }
                Err(err) => {
                    snapshot.endpoints_error = Some(err.to_string());
                    tracing::warn!("[端点轮询] 枚举端点失败：{err}");
                }
            }
        }

        snapshot.follow_error = follow_error.clone();
        snapshot.follow_saved = saved.clone();
        snapshot.updated_at = Some(Instant::now());
        if let Ok(mut slot) = inner.snapshot.lock() {
            *slot = snapshot.clone();
        }
        if !initial_sent {
            initial_sent = true;
            if let Some(tx) = initial_tx.as_ref() {
                let _ = tx.send(());
            }
        }

        // 睡到下个 tick（请求会在 ≤250 ms 内被处理，不需要额外唤醒）。
        let sleep = POLL_INTERVAL;
        let deadline = Instant::now() + sleep;
        while Instant::now() < deadline {
            if inner.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        tick = tick.wrapping_add(1);
    }
}

/// 把采集源的音量/静音镜像到目标设备（原 `tick_volume_follow()` 的后台版本）。
fn apply_follow(
    input: &PollInput,
    volume: &EndpointVolume,
    saved: &mut Vec<(String, f32, bool)>,
    follow_error: &mut Option<String>,
) {
    for target in &input.follow_targets {
        if !saved.iter().any(|(selector, _, _)| selector == target) {
            if let Ok(original) = get_endpoint_volume(target) {
                saved.push((target.clone(), original.scalar, original.muted));
            }
        }
        if let Err(err) = set_endpoint_volume(target, volume.scalar) {
            *follow_error = Some(format!("写音量失败：{err}"));
            tracing::warn!("[音量] 写 {target} 音量失败：{err}");
            continue;
        }
        if let Err(err) = set_endpoint_mute(target, volume.muted) {
            tracing::warn!("[音量] 写 {target} 静音失败：{err}");
        }
    }
}
