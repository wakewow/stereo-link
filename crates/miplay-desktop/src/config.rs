//! 桌面端自己的配置：引擎参数 + 上次勾选 + 音量 + 是否展开日志。
//!
//! 存 `%APPDATA%\miplay-bridge\desktop.json`（没有 APPDATA 时退到 exe 同目录）。
//! 读失败/文件不存在一律回默认值——配置文件坏掉不该让 GUI 起不来。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use miplay_server::hub::engine::EngineConfig;

/// 桌面端持久化配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    /// 引擎（hub）参数。
    pub engine: EngineConfig,
    /// 上次用的音量（0~100）。
    pub volume: u8,
    /// 上次勾选的设备（空 = 全部在线设备）。
    pub selected: Vec<String>,
    /// 是否展开日志面板。
    pub show_log: bool,
    /// 测试音时长（秒）。
    pub tone_seconds: f64,
    /// 引擎就绪后自动投放勾选的设备（默认开，省得每次开面板都要点一下）。
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// 已排除的设备（did / uuid）：不显示在投放列表里，也永远不会被投放。
    #[serde(default = "default_ignored")]
    pub ignored_devices: Vec<String>,
}

/// `auto_start` 缺省值：老配置文件里没有这个字段时也按「开」处理。
fn default_auto_start() -> bool {
    true
}

/// `ignored_devices` 缺省值：默认排除「小爱音箱Play增强版」(L05C)。
///
/// 这台在卧室属于另一条使用场景，用户明确说「不需要在这台机器上面播放」；
/// 老配置文件里没有这个字段时也照样排除，避免升级后又开始往它推流。
fn default_ignored() -> Vec<String> {
    vec!["931892755".to_string()]
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            volume: 60,
            selected: Vec::new(),
            show_log: true,
            tone_seconds: 2.0,
            auto_start: true,
            ignored_devices: default_ignored(),
        }
    }
}

impl DesktopConfig {
    /// 配置文件路径。
    pub fn path() -> PathBuf {
        if let Some(base) = std::env::var_os("APPDATA") {
            let dir = PathBuf::from(base).join("miplay-bridge");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("desktop.json");
        }
        // 兜底：exe 同目录
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("desktop.json")))
            .unwrap_or_else(|| PathBuf::from("desktop.json"))
    }

    /// 读配置；任何问题都回默认值并记一条日志。
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(cfg) => {
                    tracing::info!("已读取配置 {}", path.display());
                    cfg
                }
                Err(e) => {
                    tracing::warn!("配置文件解析失败（{}），改用默认值：{e}", path.display());
                    Self::default()
                }
            },
            Err(_) => {
                tracing::info!("没有配置文件，使用默认值（{}）", path.display());
                Self::default()
            }
        }
    }

    /// 保存配置（失败只记日志，不弹错）。
    pub fn save(&self) {
        let path = Self::path();
        let text = match serde_json::to_string_pretty(self) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("配置序列化失败：{e}");
                return;
            }
        };
        if let Err(e) = std::fs::write(&path, text) {
            tracing::warn!("写配置 {} 失败：{e}", path.display());
        }
    }
}
