//! 无头运行模式（`stereo-link-cli serve`）：不建窗口、不碰 GPU，只跑
//!「路由引擎 + 手机投放接收端 + 控制面」。
//!
//! 这是命令行部署和自动化测试的入口：起来以后所有动作（开始/停止路由、拉接收端、
//! 读状态、读日志、播测试音、改配置）都能用 `stereo-link-cli` 驱动，不用点界面。
//!
//! 设计上它和 GUI 是同一套东西的两个宿主：引擎、接收端、控制面协议、状态 JSON 形状
//! 全部共用；差别只在「谁来回调时钟」——GUI 用 egui 的帧，这里用 200ms 的循环。

use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{json, Value};
use stereo_audio::{
    list_endpoints, DataFlow, DeviceSelector, EndpointInfo, RouteChannel, RouteConfig,
    RouteEngineConfig, RouteEngineHandle, RouteEngineStats,
};

use crate::config::AppConfig;
use crate::control::{self, ControlServer};
use crate::logbuf::LogBuffer;
use crate::receiver::{CastKind, ReceiverManager};

/// `serve` 的启动开关。
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// 按配置自动开始路由（`--no-routing` 关掉）。
    pub routing: bool,
    /// 按配置自动拉起接收端（`--no-receivers` 关掉）。
    pub receivers: bool,
}

struct State {
    cfg: AppConfig,
    cast: ReceiverManager,
    engine: Option<RouteEngineHandle>,
    stats: Option<RouteEngineStats>,
    endpoints: Vec<EndpointInfo>,
    log: LogBuffer,
    started: Instant,
    last_endpoint_refresh: Instant,
    quit: bool,
}

impl State {
    fn new(log: LogBuffer) -> Self {
        let cfg = AppConfig::load();
        let mut cast = ReceiverManager::new();
        cast.set_low_latency(cfg.cast.low_latency);
        Self {
            cfg,
            cast,
            engine: None,
            stats: None,
            endpoints: Vec::new(),
            log,
            started: Instant::now(),
            last_endpoint_refresh: Instant::now() - Duration::from_secs(60),
            quit: false,
        }
    }

    fn uptime(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn refresh_endpoints(&mut self) {
        match list_endpoints() {
            Ok(mut endpoints) => {
                endpoints.sort_by(|a, b| a.name.cmp(&b.name));
                self.endpoints = endpoints;
                self.last_endpoint_refresh = Instant::now();
                let default = self
                    .endpoints
                    .iter()
                    .find(|ep| ep.flow == DataFlow::Render && ep.is_default);
                if let Some(ep) = default {
                    let (id, name) = (ep.id.clone(), ep.name.clone());
                    self.cast.sync_default_output(Some((&id, &name)));
                }
            }
            Err(error) => tracing::warn!("枚举端点失败：{error}"),
        }
    }

    fn renders(&self) -> Vec<EndpointInfo> {
        self.endpoints
            .iter()
            .filter(|ep| ep.flow == DataFlow::Render)
            .cloned()
            .collect()
    }

    /// 采集源：配置里记住的 id 优先，找不到再按虚拟声卡提示挑（和 GUI 同一条规则）。
    fn capture_endpoint(&self, renders: &[EndpointInfo]) -> Option<EndpointInfo> {
        if let Some(id) = self
            .cfg
            .capture
            .endpoint_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if let Some(ep) = renders.iter().find(|ep| ep.id.eq_ignore_ascii_case(id)) {
                return Some(ep.clone());
            }
        }
        crate::audio_default::pick_virtual(renders)
    }

    fn build_engine_config(&self) -> Option<RouteEngineConfig> {
        let renders = self.renders();
        let capture = self.capture_endpoint(&renders)?;
        let source = selector_for(&capture, &renders);

        let mut routes = Vec::new();
        let left = self.cfg.split.left_endpoint.trim();
        if !left.is_empty() {
            routes.push(RouteConfig {
                endpoint: left.to_owned(),
                channel: RouteChannel::Left,
                gain: self.cfg.split.gain,
                delay_ms: self.cfg.split.left_delay_ms,
                enabled: true,
                invert: self.cfg.split.invert_left,
            });
        }
        let right = self.cfg.split.right_endpoint.trim();
        if !right.is_empty() {
            routes.push(RouteConfig {
                endpoint: right.to_owned(),
                channel: RouteChannel::Right,
                gain: self.cfg.split.gain,
                delay_ms: self.cfg.split.right_delay_ms,
                enabled: true,
                invert: self.cfg.split.invert_right,
            });
        }
        if routes.is_empty() {
            return None;
        }
        Some(RouteEngineConfig {
            source_endpoint: source,
            routes,
            capture_gain: 1.0,
            render_buffer_ms: 40,
            queue_ms: self.cfg.split.queue_ms,
            rate: None,
            capture_buffer_ms: None,
            effect: (&self.cfg.effect).into(),
        })
    }

    fn start_routing(&mut self) -> Result<(), String> {
        if self.engine.is_some() {
            return Err("路由已经在跑".to_owned());
        }
        let Some(config) = self.build_engine_config() else {
            return Err(
                "还没配好：需要「采集源」和左/右声道输出设备（配置文件 split.* / capture.*）"
                    .to_owned(),
            );
        };
        let summary = format!(
            "{} → {}",
            config.source_endpoint,
            config
                .routes
                .iter()
                .map(|route| format!("{:?}={}", route.channel, route.endpoint))
                .collect::<Vec<_>>()
                .join(", ")
        );
        match RouteEngineHandle::start(config) {
            Ok(handle) => {
                self.stats = Some(handle.stats());
                self.engine = Some(handle);
                tracing::info!("[路由] 已启动：{summary}");
                Ok(())
            }
            Err(error) => {
                tracing::error!("[路由] 启动失败：{error}");
                Err(format!("启动失败：{error}"))
            }
        }
    }

    fn stop_routing(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            engine.stop();
            tracing::info!("[路由] 已停止");
        }
        self.stats = None;
    }

    fn start_receivers_from_config(&mut self) {
        // 上次被强杀时子进程会留下（占着 5000/8899），先清掉再把新的拉起来。
        crate::receiver::kill_stale();
        let wanted = [
            (CastKind::AirPlay, self.cfg.cast.auto_airplay),
            (CastKind::MiPlay, self.cfg.cast.auto_miplay),
        ];
        for (kind, enabled) in wanted {
            if !enabled {
                continue;
            }
            match self.cast.start(kind) {
                Ok(()) => tracing::info!("[投放] 已拉起 {}", kind.label()),
                Err(error) => tracing::warn!("[投放] {} 启动失败：{error}", kind.label()),
            }
        }
    }

    fn tick(&mut self) {
        self.cast.poll();
        if let Some(engine) = &self.engine {
            self.stats = Some(engine.stats());
        }
        if self.last_endpoint_refresh.elapsed() >= Duration::from_secs(3) {
            self.refresh_endpoints();
        }
    }

    fn status(&self) -> Value {
        let default_output = self
            .endpoints
            .iter()
            .find(|ep| ep.flow == DataFlow::Render && ep.is_default)
            .map(|ep| json!({ "id": ep.id, "name": ep.name }));
        control::status_json(
            "serve",
            self.uptime(),
            self.engine.is_some(),
            control::engine_json(self.stats.as_ref()),
            control::receivers_json(&self.cast),
            control::endpoints_json(&self.endpoints),
            control::config_summary(&self.cfg),
            json!({
                "default_output": default_output,
                "log_path": control::log_path().display().to_string(),
                "receivers_managed": true,
            }),
        )
    }

    fn log_json(&self, lines: usize, level: Option<&str>) -> Value {
        let want_level = level.map(|level| level.to_ascii_uppercase());
        let all = self.log.lines();
        let filtered: Vec<_> = all
            .into_iter()
            .filter(|line| match &want_level {
                Some(want) => format!("{}", line.level)
                    .to_ascii_uppercase()
                    .contains(want.as_str()),
                None => true,
            })
            .rev()
            .take(lines)
            .collect();
        let items: Vec<Value> = filtered
            .into_iter()
            .rev()
            .map(|line| {
                json!({
                    "at_ms": line.at.as_millis() as u64,
                    "level": format!("{}", line.level),
                    "text": line.text,
                })
            })
            .collect();
        json!({ "count": items.len(), "lines": items })
    }

    /// 控制面命令入口：GUI 里同名命令走 `app.rs` 的实现，语义保持一致。
    fn handle(&mut self, request: &Value) -> Value {
        let Some(cmd) = control::str_field(request, "cmd") else {
            return control::err("请求缺少 cmd 字段");
        };
        match cmd {
            "ping" => control::ok(json!({
                "pong": true,
                "mode": "serve",
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
            })),
            "status" => control::ok(self.status()),
            "log" => {
                let lines = request
                    .get("lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(100)
                    .min(2000) as usize;
                let level = control::str_field(request, "level");
                control::ok(self.log_json(lines, level))
            }
            "routing" => {
                self.handle_routing(control::str_field(request, "action").unwrap_or("status"))
            }
            "effect" => control::effect_command(&mut self.cfg, self.engine.as_ref(), request),
            "receiver" => {
                let action = control::str_field(request, "action").unwrap_or("list");
                let kind = control::str_field(request, "kind").unwrap_or("all");
                self.handle_receiver(action, kind, request)
            }
            "endpoints" => {
                self.refresh_endpoints();
                control::ok(control::endpoints_json(&self.endpoints))
            }
            "config" => self.handle_config(request),
            "tone" => self.handle_tone(request),
            "doctor" => control::ok(doctor_json()),
            "quit" => {
                self.quit = true;
                control::ok(json!({ "stopping": true }))
            }
            other => control::err(format!("未知命令：{other}")),
        }
    }

    fn handle_routing(&mut self, action: &str) -> Value {
        match action {
            "status" => control::ok(json!({
                "running": self.engine.is_some(),
                "engine": control::engine_json(self.stats.as_ref()),
            })),
            "start" => match self.start_routing() {
                Ok(()) => control::ok(json!({ "running": true })),
                Err(error) => control::err(error),
            },
            "stop" => {
                self.stop_routing();
                control::ok(json!({ "running": false }))
            }
            "restart" => {
                self.stop_routing();
                match self.start_routing() {
                    Ok(()) => control::ok(json!({ "running": true })),
                    Err(error) => control::err(error),
                }
            }
            "mute" | "unmute" => {
                let Some(engine) = &self.engine else {
                    return control::err("路由没在跑");
                };
                let muted = action == "mute";
                if !engine.set_muted(muted) {
                    return control::err("切换静音失败（引擎可能已经停了）");
                }
                control::ok(json!({ "muted": engine.muted() }))
            }
            other => control::err(format!("routing 不支持 action={other}")),
        }
    }

    fn handle_receiver(&mut self, action: &str, kind: &str, request: &Value) -> Value {
        let kinds: Vec<CastKind> = if kind == "all" {
            CastKind::ALL.to_vec()
        } else {
            match parse_kind(kind) {
                Some(kind) => vec![kind],
                None => {
                    return control::err(format!("kind 只支持 airplay / miplay / all，收到 {kind}"))
                }
            }
        };
        match action {
            "list" => control::ok(control::receivers_json(&self.cast)),
            // 音量走 control.rs 的共用实现：GUI 的 handle_control 调的是同一份代码。
            "volume" => control::receiver_volume(&mut self.cast, kind, request),
            "start" | "restart" => {
                let mut errors = Vec::new();
                for kind in kinds {
                    let result = if action == "restart" {
                        self.cast.stop(kind);
                        self.cast.start(kind)
                    } else {
                        self.cast.start(kind)
                    };
                    if let Err(error) = result {
                        errors.push(format!("{}：{error}", kind.label()));
                    }
                }
                if errors.is_empty() {
                    control::ok(control::receivers_json(&self.cast))
                } else {
                    control::err(errors.join("；"))
                }
            }
            "stop" => {
                for kind in kinds {
                    self.cast.stop(kind);
                }
                control::ok(control::receivers_json(&self.cast))
            }
            other => control::err(format!("receiver 不支持 action={other}")),
        }
    }

    fn handle_config(&mut self, request: &Value) -> Value {
        let action = control::str_field(request, "action").unwrap_or("show");
        match action {
            "path" => control::ok(json!({
                "path": AppConfig::path().display().to_string(),
                "log_path": control::log_path().display().to_string(),
            })),
            "show" => control::ok(serde_json::to_value(&self.cfg).unwrap_or(Value::Null)),
            "get" => {
                let key = control::str_field(request, "key").unwrap_or("");
                match control::config_get(&self.cfg, key) {
                    Ok(value) => control::ok(json!({ "key": key, "value": value })),
                    Err(error) => control::err(error),
                }
            }
            "set" => {
                let key = control::str_field(request, "key").unwrap_or("");
                let Some(value) = request.get("value") else {
                    return control::err("config set 需要 value 字段");
                };
                match control::config_set(&mut self.cfg, key, value.clone()) {
                    Ok(result) => {
                        self.cfg.save();
                        self.cast.set_low_latency(self.cfg.cast.low_latency);
                        if key.starts_with("effect") {
                            if let Some(engine) = self.engine.as_ref() {
                                engine.set_effect((&self.cfg.effect).into());
                            }
                        }
                        tracing::info!("[配置] {key} = {value}");
                        control::ok(json!({ "updated": result, "saved": true }))
                    }
                    Err(error) => control::err(error),
                }
            }
            "reload" => {
                self.cfg = AppConfig::load();
                if let Some(engine) = self.engine.as_ref() {
                    engine.set_effect((&self.cfg.effect).into());
                }
                control::ok(json!({ "reloaded": true }))
            }
            other => control::err(format!("config 不支持 action={other}")),
        }
    }

    fn handle_tone(&mut self, request: &Value) -> Value {
        let freq = request.get("freq").and_then(Value::as_f64).unwrap_or(440.0);
        let seconds = request
            .get("seconds")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        let selector = match control::str_field(request, "device") {
            Some(device) if !device.is_empty() && device != "default" => {
                DeviceSelector::name_contains(device)
            }
            _ => DeviceSelector::Default,
        };
        match stereo_audio::play_sine(&selector, freq, seconds, 0.25) {
            Ok(info) => control::ok(json!({
                "played": true,
                "device": info.device_name,
                "seconds": info.seconds,
                "path": format!("{:?}", info.path),
            })),
            Err(error) => control::err(format!("播放测试音失败：{error}")),
        }
    }
}

impl State {
    fn stop_all(&mut self) {
        self.stop_routing();
        self.cast.stop_all();
    }
}

/// `serve` 主循环。
pub fn run(options: Options) -> anyhow::Result<()> {
    // 无头模式必须看得到日志：install() 读这个变量决定要不要加控制台层。
    std::env::set_var("STEREOLINK_LOG_CONSOLE", "1");
    let log = LogBuffer::new(2000);
    crate::logbuf::install(log.clone());

    let mut server = ControlServer::start("serve").context("控制面启动失败")?;
    // Ctrl+C / SIGTERM（systemd stop、`kill`）：只置标志，让主循环自己收尾。
    // 直接让信号杀掉进程会把子进程接收端留在后台占着 5000/8899。
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop = stop.clone();
        if let Err(error) = ctrlc::set_handler(move || {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }) {
            tracing::warn!("信号处理注册失败（Ctrl+C 可能不会干净退出）：{error}");
        }
    }
    tracing::info!(
        "StereoLink 无头服务已启动（serve），版本 {}，控制面：{}",
        env!("CARGO_PKG_VERSION"),
        control::control_path().display()
    );
    println!(
        "StereoLink v{} 无头服务运行中（mode=serve）：控制面 127.0.0.1:{}，Ctrl+C 退出",
        env!("CARGO_PKG_VERSION"),
        server.info().port
    );

    let mut state = State::new(log);
    state.refresh_endpoints();
    if options.receivers {
        state.start_receivers_from_config();
    }
    if options.routing {
        if !state.cfg.split.enabled {
            tracing::info!("[路由] 配置里 split.enabled=false，不自动启动");
        } else if let Err(error) = state.start_routing() {
            tracing::warn!("[路由] 自动启动失败：{error}");
        }
    }

    while !state.quit && !stop.load(std::sync::atomic::Ordering::Relaxed) {
        server.drain(|request| state.handle(request));
        state.tick();
        std::thread::sleep(Duration::from_millis(200));
    }

    tracing::info!("收到退出信号，正在停止…");
    state.stop_all();
    // ControlServer 的 Drop 只在自己 pid 仍是文件主人时删 control.json；
    // 这里再显式清一次，保证 CLI 下一次看到的是「没有运行中的实例」。
    drop(server);
    control::clear_control_file();
    println!("StereoLink 无头服务已停止。");
    Ok(())
}

fn parse_kind(kind: &str) -> Option<CastKind> {
    match kind.to_ascii_lowercase().as_str() {
        "airplay" | "ap" => Some(CastKind::AirPlay),
        "miplay" | "mi" | "妙播" => Some(CastKind::MiPlay),
        _ => None,
    }
}

/// 引擎要的采集源字符串：完整名字在 render 列表里重复时用端点 id（精确匹配）。
fn selector_for(ep: &EndpointInfo, renders: &[EndpointInfo]) -> String {
    let duplicated = renders.iter().filter(|other| other.name == ep.name).count() > 1;
    if duplicated {
        ep.id.clone()
    } else {
        ep.name.clone()
    }
}

/// 环境自检：`stereo-link-cli doctor`（离线也能跑，不依赖实例）。
pub fn doctor_json() -> Value {
    let cfg_path = AppConfig::path();
    let log_path = control::log_path();
    let mut checks: Vec<Value> = Vec::new();
    let mut push = |name: &str, ok: bool, detail: String| {
        checks.push(json!({ "name": name, "ok": ok, "detail": detail }));
    };

    push(
        "配置文件",
        cfg_path.exists(),
        cfg_path.display().to_string(),
    );
    let log_dir_ok = log_path
        .parent()
        .map(|dir| std::fs::create_dir_all(dir).is_ok())
        .unwrap_or(false);
    push("日志目录可写", log_dir_ok, log_path.display().to_string());

    match control::running() {
        Some(info) => push(
            "运行实例",
            true,
            format!("mode={} pid={} port={}", info.mode, info.pid, info.port),
        ),
        None => push(
            "运行实例",
            false,
            "没有实例在跑（只有 GUI/serve 才提供控制面）".to_owned(),
        ),
    }

    let endpoints = list_endpoints().unwrap_or_default();
    let renders: Vec<&EndpointInfo> = endpoints
        .iter()
        .filter(|ep| ep.flow == DataFlow::Render)
        .collect();
    let default = renders.iter().find(|ep| ep.is_default);
    push(
        "播放端点",
        !renders.is_empty(),
        format!(
            "{} 个；系统默认 {}",
            renders.len(),
            default.map(|ep| ep.name.as_str()).unwrap_or("<读不到>")
        ),
    );
    let virtual_card = renders
        .iter()
        .any(|ep| ep.name.contains("立体声链虚拟声卡"));
    push(
        "虚拟声卡",
        virtual_card,
        if virtual_card {
            "已安装「立体声链虚拟声卡」".to_owned()
        } else {
            "没找到自研虚拟声卡：可以用任意播放端点当采集源，或先装驱动".to_owned()
        },
    );

    for kind in CastKind::ALL {
        match crate::receiver::locate(kind) {
            Some(path) => push(kind.label(), true, format!("接收端 {}", path.display())),
            None => push(
                kind.label(),
                false,
                format!(
                    "找不到 {}：{}",
                    kind.exe_name(),
                    crate::receiver::BUILD_HINT
                ),
            ),
        }
    }

    let ok = checks
        .iter()
        .all(|check| check.get("ok").and_then(Value::as_bool).unwrap_or(false));

    json!({
        "ok": ok,
        "version": env!("CARGO_PKG_VERSION"),
        "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
        "config_path": cfg_path.display().to_string(),
        "log_path": log_path.display().to_string(),
        "control_path": control::control_path().display().to_string(),
        "checks": checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_uses_id_when_names_duplicate() {
        let ep = EndpointInfo {
            id: "{a}".into(),
            name: "扬声器".into(),
            flow: DataFlow::Render,
            is_default: false,
            mix_format: None,
            error: None,
        };
        assert_eq!(selector_for(&ep, std::slice::from_ref(&ep)), "扬声器");
        assert_eq!(selector_for(&ep, &[ep.clone(), ep.clone()]), "{a}");
    }

    #[test]
    fn kind_parsing_is_forgiving() {
        assert!(matches!(parse_kind("AirPlay"), Some(CastKind::AirPlay)));
        assert!(matches!(parse_kind("miplay"), Some(CastKind::MiPlay)));
        assert!(parse_kind("dlna").is_none());
    }
}
