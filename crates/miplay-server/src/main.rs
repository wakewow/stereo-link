//! miplay-bridge 主程序。
//!
//! 子命令：
//! - `devices`  —— 命令行列出账号下的小米设备（用于快速验证凭据与连通性）
//! - `serve`    —— 启动 HTTP API + 网页前端
//!
//! 目前刻意不引入 clap，用手写参数解析保持依赖精简。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use miplay_core::mina::PlayApi;
use miplay_core::{MiToken, MinaClient, XiaomiDevice};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const INDEX_HTML: &str = include_str!("../../../web/index.html");
const DEVICE_CACHE_TTL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- 状态与错误

struct AppState {
    client: MinaClient,
    devices: Mutex<Option<(Instant, Vec<XiaomiDevice>)>>,
}

impl AppState {
    async fn devices(&self, force: bool) -> Result<Vec<XiaomiDevice>> {
        let mut guard = self.devices.lock().await;
        if !force {
            if let Some((at, list)) = guard.as_ref() {
                if at.elapsed() < DEVICE_CACHE_TTL {
                    return Ok(list.clone());
                }
            }
        }
        let list = self.client.device_list().await?;
        *guard = Some((Instant::now(), list.clone()));
        Ok(list)
    }

    async fn device(&self, key: &str) -> Result<XiaomiDevice> {
        let list = self.devices(false).await?;
        MinaClient::find(&list, key).cloned()
    }
}

/// 把 anyhow 错误变成 JSON 响应，前端好展示。
struct AppError {
    status: StatusCode,
    error: anyhow::Error,
}

impl AppError {
    /// 参数问题 —— 400。
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: anyhow::anyhow!(msg.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.error);
        tracing::warn!("API 错误 ({}): {msg}", self.status.as_u16());
        (self.status, Json(json!({ "ok": false, "error": msg }))).into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(e: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: e.into(),
        }
    }
}

type ApiResult<T> = std::result::Result<T, AppError>;

// ---------------------------------------------------------------- 处理函数

async fn index() -> Html<String> {
    Html(load_index_html())
}

/// 优先读磁盘上的 `web/index.html`（改前端不用重编译），
/// 读不到就回退到编译期嵌入的副本（保证单文件分发仍可用）。
fn load_index_html() -> String {
    for dir in [std::env::var("MIPLAY_WEB_DIR").ok(), Some("web".to_string())] {
        if let Some(dir) = dir {
            let p = std::path::Path::new(&dir).join("index.html");
            if let Ok(s) = std::fs::read_to_string(&p) {
                return s;
            }
        }
    }
    INDEX_HTML.to_string()
}

async fn api_health(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "miplay-bridge",
        "version": env!("CARGO_PKG_VERSION"),
        "account": st.client.token().redacted(),
    }))
}

async fn api_devices(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let list = st.devices(false).await?;
    let views: Vec<Value> = list.iter().map(XiaomiDevice::to_view).collect();
    Ok(Json(json!({ "ok": true, "count": views.len(), "devices": views })))
}

async fn api_device_status(
    State(st): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let dev = st.device(&key).await?;
    // 状态查询失败不该让整个请求 500，但要**说清楚失败原因**，不能静默返回 null。
    match st.client.play_status(&dev.device_id).await {
        Ok(status) => Ok(Json(json!({ "ok": true, "device": dev.to_view(), "play_status": status }))),
        Err(e) => Ok(Json(json!({
            "ok": false, "device": dev.to_view(), "play_status": null,
            "error": format!("{e:#}"),
        }))),
    }
}

#[derive(serde::Deserialize)]
struct VolumeBody {
    volume: u8,
}

async fn api_set_volume(
    State(st): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<VolumeBody>,
) -> ApiResult<Json<Value>> {
    if body.volume > 100 {
        return Err(AppError::bad_request("音量必须在 0..=100"));
    }
    let dev = st.device(&key).await?;
    let ok = st.client.set_volume(&dev.device_id, body.volume).await?;
    Ok(Json(json!({ "ok": ok, "device": dev.label(), "volume": body.volume })))
}

#[derive(serde::Deserialize)]
struct PlayBody {
    url: String,
    /// 可选：显式覆盖接口选择（`url` / `music`）
    #[serde(default)]
    api: Option<String>,
}

async fn api_play(
    State(st): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<PlayBody>,
) -> ApiResult<Json<Value>> {
    if !(body.url.starts_with("http://") || body.url.starts_with("https://")) {
        return Err(AppError::bad_request("url 必须以 http:// 或 https:// 开头"));
    }
    let dev = st.device(&key).await?;
    let used = match body.api.as_deref() {
        Some("url") => {
            st.client.play_by_url(&dev.device_id, &body.url, 2).await?;
            PlayApi::PlayUrl
        }
        Some("music") => {
            st.client
                .play_by_music_url(
                    &dev.device_id,
                    &body.url,
                    miplay_core::DEFAULT_AUDIO_ID,
                    miplay_core::DEFAULT_CP_ID,
                    2,
                )
                .await?;
            PlayApi::PlayMusic
        }
        Some(other) => {
            return Err(AppError::bad_request(format!(
                "api 只能是 `url` 或 `music`，收到 `{other}`"
            )))
        }
        None => st.client.play_on(&dev, &body.url).await?,
    };
    Ok(Json(json!({
        "ok": true, "device": dev.label(), "did": dev.did,
        "hardware": dev.hardware, "api": used.as_str(), "url": body.url,
    })))
}

async fn api_stop(
    State(st): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let dev = st.device(&key).await?;
    let ok = st.client.stop(&dev.device_id).await?;
    Ok(Json(json!({ "ok": ok, "device": dev.label() })))
}

async fn api_tts(
    State(st): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let text = body.get("text").and_then(Value::as_str).unwrap_or("你好");
    let dev = st.device(&key).await?;
    let ok = st.client.text_to_speech(&dev.device_id, text).await?;
    Ok(Json(json!({ "ok": ok, "device": dev.label() })))
}

// ---------------------------------------------------------------- CLI

fn usage() -> String {
    format!(
        "miplay-bridge {}\n\
         \n\
         用法:\n\
           miplay-bridge devices [--json]        列出账号下的设备\n\
           miplay-bridge serve [--port 8900]     启动 HTTP API + 网页前端\n\
           miplay-bridge token                   显示脱敏后的凭据信息\n\
miplay-bridge ubus <设备> <path> <method> [message-json]\n\
                                      直接调用 MiNA ubus 并原样打印响应\n\
         \n\
         凭据查找顺序:\n  {}\n",
        env!("CARGO_PKG_VERSION"),
        MiToken::default_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ")
    )
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,miplay_bridge=debug".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    match cmd {
        "devices" => {
            let token = MiToken::load_default().context("加载凭据失败")?;
            let client = MinaClient::new(token)?;
            let devices = client.device_list().await.context("获取设备列表失败")?;
            if args.iter().any(|a| a == "--json") {
                let views: Vec<Value> = devices.iter().map(XiaomiDevice::to_view).collect();
                println!("{}", serde_json::to_string_pretty(&views)?);
            } else {
                println!("共 {} 台设备:", devices.len());
                println!(
                    "{:<24} {:<12} {:<8} {:<7} {:<8} {}",
                    "名称", "did", "型号", "在线", "播放接口", "MiNA uuid"
                );
                for d in &devices {
                    println!(
                        "{:<24} {:<12} {:<8} {:<7} {:<8} {}",
                        d.label(),
                        if d.did.is_empty() { "-" } else { &d.did },
                        d.hardware,
                        if d.online() { "是" } else { "否" },
                        MinaClient::play_api_for(&d.hardware).as_str(),
                        d.device_id
                    );
                }
            }
            Ok(())
        }

        "token" => {
            let path = MiToken::find_default()?;
            let token = MiToken::load(&path)?;
            println!("凭据文件: {}", path.display());
            println!("{}", serde_json::to_string_pretty(&token.redacted())?);
            Ok(())
        }

        "ubus" => {
            let (Some(key), Some(path), Some(method)) =
                (args.get(1), args.get(2), args.get(3))
            else {
                eprintln!("用法: miplay-bridge ubus <设备> <path> <method> [message-json]");
                std::process::exit(2);
            };
            let message: Value = match args.get(4) {
                Some(s) => serde_json::from_str(s).context("message 不是合法 JSON")?,
                None => json!({}),
            };
            let token = MiToken::load_default().context("加载凭据失败")?;
            let client = MinaClient::new(token)?;
            let devices = client.device_list().await.context("获取设备列表失败")?;
            let dev = MinaClient::find(&devices, key)?;
            let resp = client.ubus_raw(&dev.device_id, method, path, message).await?;
            println!("设备: {} ({})", dev.label(), dev.device_id);
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        }

        "serve" => {
            let port: u16 = flag(&args, "--port")
                .map(|p| p.parse())
                .transpose()
                .context("--port 必须是数字")?
                .unwrap_or(8900);

            let token = MiToken::load_default().context("加载凭据失败")?;
            tracing::info!("凭据已加载: {}", token.redacted());

            let state = Arc::new(AppState {
                client: MinaClient::new(token)?,
                devices: Mutex::new(None),
            });

            // 启动时先探一次，让日志里立刻能看到设备清单
            match state.devices(true).await {
                Ok(list) => {
                    tracing::info!("发现 {} 台设备:", list.len());
                    for d in &list {
                        tracing::info!(
                            "  {} did={} {} {}",
                            d.label(),
                            d.did,
                            d.hardware,
                            if d.online() { "在线" } else { "离线" }
                        );
                    }
                }
                Err(e) => tracing::warn!("启动时获取设备列表失败（服务仍会启动）: {e:#}"),
            }

            let app = Router::new()
                .route("/", get(index))
                .route("/api/health", get(api_health))
                .route("/api/devices", get(api_devices))
                .route("/api/devices/{key}", get(api_device_status))
                .route("/api/devices/{key}/volume", post(api_set_volume))
                .route("/api/devices/{key}/play", post(api_play))
                .route("/api/devices/{key}/stop", post(api_stop))
                .route("/api/devices/{key}/tts", post(api_tts))
                .with_state(state);

            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("端口 {port} 被占用？"))?;
            tracing::info!("网页前端: http://127.0.0.1:{port}");
            axum::serve(listener, app).await.context("HTTP 服务异常退出")?;
            Ok(())
        }

        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        other => {
            eprintln!("未知子命令: {other}\n");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}
