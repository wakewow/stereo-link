//! 本机控制面：GUI / `serve` 在 `127.0.0.1` 的随机端口上开一个 JSON 行协议，
//! `stereo-link-cli` 连上去读状态、发命令。
//!
//! 开发规范（见 `docs/dev-conventions.md`）要求「状态、日志、动作都必须有命令行入口」，
//! 这里就是那条通道：界面做的每件事都有同名命令，测试脚本不用去点按钮。
//!
//! # 协议
//! * 一次连接一问一答，UTF-8 JSON，每行一条；
//! * 请求：`{"token":"<control.json 里的 token>","cmd":"status",...}`；
//! * 响应：`{"ok":true,"data":…}` 或 `{"ok":false,"error":"…"}`。
//!
//! # 发现
//! 实例启动时把 `{port, token, pid, mode}` 写进
//! `%APPDATA%\stereo-link\control.json`，退出时删除；CLI 先读它再连。

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

/// 协议版本：字段只增不改；加破坏性变更时 +1，CLI 会提示升级。
pub const PROTOCOL_VERSION: u32 = 1;
/// 一问一答的超时（秒）：状态查询是纯内存操作，超过这个时间说明实例卡住了。
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// 控制文件路径：`%APPDATA%\stereo-link\control.json`（退化为当前目录）。
pub fn control_path() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(base) => PathBuf::from(base).join("stereo-link").join("control.json"),
        None => PathBuf::from("control.json"),
    }
}

/// 日志文件路径：`%APPDATA%\stereo-link\logs\stereo-link.log`。
pub fn log_path() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(base) => PathBuf::from(base)
            .join("stereo-link")
            .join("logs")
            .join("stereo-link.log"),
        None => PathBuf::from("stereo-link.log"),
    }
}

/// 控制文件内容。
#[derive(Debug, Clone)]
pub struct ControlInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub mode: String,
}

impl ControlInfo {
    fn to_json(&self) -> Value {
        json!({
            "protocol": PROTOCOL_VERSION,
            "port": self.port,
            "token": self.token,
            "pid": self.pid,
            "mode": self.mode,
            "started_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
    }

    /// 写控制文件（先建目录；覆盖旧的）。
    pub fn write(&self) -> std::io::Result<()> {
        let path = control_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&self.to_json())?)
    }

    /// 读控制文件；坏文件/缺字段返回 None。
    pub fn read() -> Option<Self> {
        let text = std::fs::read_to_string(control_path()).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let port = value.get("port")?.as_u64()? as u16;
        let token = value.get("token")?.as_str()?.to_owned();
        let info = Self {
            port,
            token,
            pid: value.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
            mode: value
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
        };
        // 进程被强杀 / `process::exit` 时来不及删 control.json，用一次很短的 TCP
        // 探测判断文件是不是陈的；陈的直接清掉，免得 CLI 反复报「连不上实例」。
        if TcpStream::connect_timeout(&info.addr(), Duration::from_millis(120)).is_err() {
            let _ = std::fs::remove_file(control_path());
            return None;
        }
        Some(info)
    }

    /// 删除控制文件（仅当它确实是本实例的，避免两个实例互删）。
    pub fn clear_own(&self) {
        if let Some(current) = Self::read() {
            if current.pid == self.pid && current.port == self.port {
                let _ = std::fs::remove_file(control_path());
            }
        }
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }
}

/// 一条待处理的请求：主线程处理完把响应写回 `reply`。
pub struct Request {
    pub payload: Value,
    reply: Sender<String>,
}

/// 控制面服务端（GUI / serve 各一个）。
pub struct ControlServer {
    info: ControlInfo,
    incoming: Receiver<Request>,
    stop: Arc<AtomicBool>,
}

impl ControlServer {
    /// 起监听线程。`mode` 只用于诊断（`"gui"` / `"serve"`）。
    pub fn start(mode: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let info = ControlInfo {
            port,
            token: random_token(),
            pid: std::process::id(),
            mode: mode.to_owned(),
        };
        info.write()?;

        let (tx, incoming) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let token = info.token.clone();
        let stop_thread = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("stereo-link-control".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stop_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let tx = tx.clone();
                    let token = token.clone();
                    std::thread::Builder::new()
                        .name("stereo-link-control-conn".to_owned())
                        .spawn(move || {
                            let _ = serve_connection(stream, &token, &tx);
                        })
                        .ok();
                }
            })?;
        Ok(Self {
            info,
            incoming,
            stop,
        })
    }

    pub fn info(&self) -> &ControlInfo {
        &self.info
    }

    /// 把当前排队的请求依次交给 `handle`，把返回值写回客户端。
    ///
    /// 用「取出→处理→放回」的写法规避 `self` 双重借用：调用方可以在这里直接改自己的状态。
    pub fn drain(&mut self, mut handle: impl FnMut(&Value) -> Value) {
        while let Ok(request) = self.incoming.try_recv() {
            let response = handle(&request.payload);
            let _ = request.reply.send(response.to_string());
        }
    }

    /// 还有没有没处理的请求（`serve` 用来决定要不要继续等待）。
    pub fn has_pending(&self) -> bool {
        // 没有 peek 接口，用 try_recv 会吞掉请求；这里只做保守判断。
        false
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.info.clear_own();
    }
}

fn serve_connection(stream: TcpStream, token: &str, tx: &Sender<Request>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        // 响应统一先构造 Value，再序列化一次——通道/分支都只处理一种类型。
        let response: Value = match serde_json::from_str::<Value>(text) {
            Ok(payload) => {
                let given = payload.get("token").and_then(Value::as_str).unwrap_or("");
                if given != token {
                    err("token 不对：control.json 可能是旧实例留下的")
                } else {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    if tx
                        .send(Request {
                            payload,
                            reply: reply_tx,
                        })
                        .is_err()
                    {
                        err("控制线程已退出")
                    } else {
                        match reply_rx.recv_timeout(REPLY_TIMEOUT) {
                            Ok(text) => {
                                serde_json::from_str::<Value>(&text).unwrap_or_else(|error| {
                                    err(format!("实例返回的不是 JSON：{error}"))
                                })
                            }
                            Err(_) => err("等待实例响应超时（实例可能卡住了）"),
                        }
                    }
                }
            }
            Err(error) => err(format!("请求不是合法 JSON：{error}")),
        };
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_owned())
        )?;
        writer.flush()?;
        // 一次连接只处理一条请求，然后由客户端关闭；保持简单。
        return Ok(());
    }
}

/// 删掉控制文件（退出前显式调用：`process::exit` 不会跑 `Drop`）。
pub fn clear_control_file() {
    let _ = std::fs::remove_file(control_path());
}

/// 客户端请求：连本机实例，发一条命令，返回解析后的响应体。
pub fn request(payload: Value) -> Result<Value, String> {
    let info = ControlInfo::read()
        .ok_or_else(|| "没有找到 control.json：实例没在运行，或从未启动过".to_owned())?;
    let mut stream = TcpStream::connect_timeout(&info.addr(), Duration::from_secs(2))
        .map_err(|error| format!("连不上实例 127.0.0.1:{}（{error}）", info.port))?;
    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let mut payload = payload;
    if let Some(map) = payload.as_object_mut() {
        map.insert("token".to_owned(), json!(info.token));
    }
    let line = payload.to_string();
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|error| format!("发送请求失败：{error}"))?;
    stream.flush().ok();
    let mut reader = BufReader::new(stream);
    let mut text = String::new();
    reader
        .read_line(&mut text)
        .map_err(|error| format!("读响应失败：{error}"))?;
    let value: Value =
        serde_json::from_str(text.trim()).map_err(|error| format!("响应不是合法 JSON：{error}"))?;
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(value.get("data").cloned().unwrap_or(Value::Null)),
        _ => Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("实例返回了未知错误")
            .to_owned()),
    }
}

/// 实例在不在跑（能连上并回 pong 才算）。
pub fn running() -> Option<ControlInfo> {
    let info = ControlInfo::read()?;
    match request(json!({"cmd": "ping"})) {
        Ok(_) => Some(info),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------- 响应构造

pub fn ok(data: Value) -> Value {
    json!({ "ok": true, "data": data })
}

pub fn err(message: impl Into<String>) -> Value {
    json!({ "ok": false, "error": message.into() })
}

// ---------------------------------------------------------------- 共享 JSON 形状

/// 引擎快照 → JSON（音频类型没有 serde 派生，这里手写一份稳定的字段名）。
pub fn engine_json(stats: Option<&stereo_audio::RouteEngineStats>) -> Value {
    let Some(stats) = stats else {
        return Value::Null;
    };
    let routes: Vec<Value> = stats
        .routes
        .iter()
        .map(|route| {
            json!({
                "channel": format!("{:?}", route.channel).to_lowercase(),
                "endpoint": route.endpoint,
                "sent_bytes": route.sent_bytes,
                "underruns": route.underruns,
                "dropped_ms": route.dropped_ms,
                "queued_ms": route.queued_ms,
                "delay_ms": route.delay_ms,
                "level": route.level,
                "format_path": route.format_path,
                "error": route.error,
                "retry_count": route.retry_count,
                "retry_in_ms": route.retry_in_ms,
            })
        })
        .collect();
    json!({
        "running": stats.running,
        "source_device": stats.source_device,
        "source_error": stats.source_error,
        "degraded": stats.degraded,
        "source_retrying": stats.source_retrying,
        "rate": stats.rate,
        "capture_buffer_ms": stats.capture_buffer_ms,
        "captured_frames": stats.captured_frames,
        "uptime_seconds": stats.uptime_seconds,
        "level_l": stats.level_l,
        "level_r": stats.level_r,
        "routes": routes,
    })
}

/// 接收端状态 → JSON。
pub fn receivers_json(cast: &crate::receiver::ReceiverManager) -> Value {
    use crate::receiver::CastKind;
    let mut list = Vec::new();
    for kind in CastKind::ALL {
        let status = cast.status(kind);
        list.push(json!({
            "kind": match kind { CastKind::AirPlay => "airplay", CastKind::MiPlay => "miplay" },
            "label": kind.label(),
            "running": status.is_running(),
            "status": status.text(),
            "error": status.error(),
            "exe": cast.exe(kind).map(|path| path.display().to_string()),
        }));
    }
    json!({
        "any_running": cast.is_any_running(),
        "device": cast.output_device(),
        "items": list,
    })
}

/// 端点列表 → JSON。
pub fn endpoints_json(endpoints: &[stereo_audio::EndpointInfo]) -> Value {
    let items: Vec<Value> = endpoints
        .iter()
        .map(|ep| {
            json!({
                "id": ep.id,
                "name": ep.name,
                "flow": match ep.flow {
                    stereo_audio::DataFlow::Render => "render",
                    stereo_audio::DataFlow::Capture => "capture",
                },
                "is_default": ep.is_default,
                "mix_format": ep.mix_format.as_ref().map(|format| json!({
                    "sample_rate": format.samplerate,
                    "channels": format.channels,
                    "bits": format.bits,
                    "format": format!("{:?}", format.sample_format),
                })),
                "error": ep.error,
            })
        })
        .collect();
    json!({ "count": items.len(), "items": items })
}

/// 配置摘要（`status` 里给个概览；完整配置用 `config show`）。
pub fn config_summary(cfg: &crate::config::AppConfig) -> Value {
    json!({
        "path": crate::config::AppConfig::path().display().to_string(),
        "split": {
            "enabled": cfg.split.enabled,
            "left_endpoint": cfg.split.left_endpoint,
            "right_endpoint": cfg.split.right_endpoint,
            "queue_ms": cfg.split.queue_ms,
            "gain": cfg.split.gain,
            "left_delay_ms": cfg.split.left_delay_ms,
            "right_delay_ms": cfg.split.right_delay_ms,
        },
        "capture": {
            "endpoint_id": cfg.capture.endpoint_id,
            "endpoint_name": cfg.capture.endpoint_name,
            "auto_switch_default": cfg.capture.auto_switch_default,
        },
        "cast": {
            "auto_airplay": cfg.cast.auto_airplay,
            "auto_miplay": cfg.cast.auto_miplay,
            "low_latency": cfg.cast.low_latency,
        },
        "theme": format!("{:?}", cfg.theme),
    })
}

/// 组装完整状态：两个宿主（GUI / serve）用同一份形状，脚本只写一次解析。
// 参数逐个对应 status 文档里的顶层分块，拆结构体反而更难读，这里放行。
#[allow(clippy::too_many_arguments)]
pub fn status_json(
    mode: &str,
    uptime: f64,
    routing_running: bool,
    engine: Value,
    receivers: Value,
    endpoints: Value,
    config: Value,
    extra: Value,
) -> Value {
    json!({
        "protocol": PROTOCOL_VERSION,
        "mode": mode,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "uptime_seconds": uptime,
        "control": { "path": control_path().display().to_string() },
        "config": config,
        "routing": { "running": routing_running, "engine": engine },
        "receivers": receivers,
        "endpoints": endpoints,
        "extra": extra,
    })
}

// ---------------------------------------------------------------- 配置读写

/// 按点号路径读配置，例如 `split.left_endpoint`、`cast.low_latency`。
pub fn config_get(cfg: &crate::config::AppConfig, key: &str) -> Result<Value, String> {
    let value = serde_json::to_value(cfg).map_err(|error| error.to_string())?;
    if key.trim().is_empty() {
        return Ok(value);
    }
    let pointer = to_pointer(key);
    value
        .pointer(&pointer)
        .cloned()
        .ok_or_else(|| format!("没有这个配置项：{key}（完整列表用 config show）"))
}

/// 按点号路径改配置：`value` 是 JSON 值（字符串/数字/布尔都行），改动后整份校验。
pub fn config_set(
    cfg: &mut crate::config::AppConfig,
    key: &str,
    value: Value,
) -> Result<Value, String> {
    let mut tree = serde_json::to_value(&*cfg).map_err(|error| error.to_string())?;
    let pointer = to_pointer(key);
    let slot = tree
        .pointer_mut(&pointer)
        .ok_or_else(|| format!("没有这个配置项：{key}"))?;
    *slot = value.clone();
    let next: crate::config::AppConfig = serde_json::from_value(tree)
        .map_err(|error| format!("值 {value} 不能写进 {key}：{error}"))?;
    *cfg = next;
    Ok(json!({ "key": key, "value": value }))
}

fn to_pointer(key: &str) -> String {
    let mut pointer = String::new();
    for part in key.trim().split('.').filter(|part| !part.is_empty()) {
        pointer.push('/');
        pointer.push_str(&part.replace('~', "~0").replace('/', "~1"));
    }
    pointer
}

fn random_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    // 不需要密码学强度：只防同机上的偶然误连，不进网络。
    format!("{:x}{:x}{:x}", nanos, pid, std::process::id() as u128 * 31)
}

/// 把 `Map` 里的键按需取出（命令解析用）。
pub fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// 取 `Map` 对象（构造请求时用）。
pub fn as_map(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_from_dotted_key() {
        assert_eq!(to_pointer("split.left_endpoint"), "/split/left_endpoint");
        assert_eq!(to_pointer(""), "");
    }

    #[test]
    fn config_roundtrip_via_json() {
        let mut cfg = crate::config::AppConfig::default();
        config_set(&mut cfg, "split.queue_ms", json!(77)).unwrap();
        assert_eq!(cfg.split.queue_ms, 77);
        assert_eq!(config_get(&cfg, "split.queue_ms").unwrap(), json!(77));
        assert!(config_set(&mut cfg, "split.queue_ms", json!("不是数字")).is_err());
        assert!(config_get(&cfg, "nope.nothing").is_err());
    }

    #[test]
    fn responses_have_stable_shape() {
        let ok = ok(json!({"a": 1}));
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["data"]["a"], json!(1));
        assert_eq!(err("boom")["ok"], json!(false));
    }
}
