//! 桌面端自己的配置：引擎参数 + 上次勾选 + 音量 + 是否展开日志。
//!
//! 存 `%APPDATA%\miplay-bridge\desktop.json`（没有 APPDATA 时退到 exe 同目录）。
//! 读失败/文件不存在一律回默认值——配置文件坏掉不该让 GUI 起不来。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use miplay_server::hub::engine::EngineConfig;

/// 声音输出模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// 妙播云端投放：每台音箱一条局域网 WAV 流，可多房间，延迟约 1 秒。
    #[default]
    Cloud,
    /// 蓝牙左右分离：虚拟声卡回环 → 左/右声道分别推给两台蓝牙音箱，
    /// 延迟约 0.2~0.35 秒（单房间看视频够用）。
    Bluetooth,
}

impl OutputMode {
    /// 面板上显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Cloud => "妙播云投放 · 多房间 ≈1s",
            Self::Bluetooth => "蓝牙左右分离 · 单房间 ≈0.2~0.35s",
        }
    }
}

/// 蓝牙左右分离的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitConfig {
    /// 面板启动时自动开始分离（默认开：打开面板就有低延迟声音）。
    pub enabled: bool,
    /// 左声道推给哪个 render 端点（名字子串，例如 `Xiaomi Sound-1695`）。
    pub left_endpoint: String,
    /// 右声道推给哪个 render 端点（名字子串，例如 `Xiaomi Sound-5320`）。
    pub right_endpoint: String,
    /// 采集增益（1.0 = 原样）。
    pub gain: f32,
    /// 抖动缓冲：每条链路保留的音频量，越大越稳但延迟越高。
    pub queue_ms: u32,
    /// 左链路额外延迟（毫秒），用来校正两箱链路的固定时差。
    pub left_delay_ms: u32,
    /// 右链路额外延迟（毫秒）。
    pub right_delay_ms: u32,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            left_endpoint: "Xiaomi Sound-1695".to_string(),
            right_endpoint: "Xiaomi Sound-5320".to_string(),
            gain: 1.0,
            queue_ms: 40,
            left_delay_ms: 0,
            right_delay_ms: 0,
        }
    }
}

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
    /// 当前输出模式（妙播云投放 / 蓝牙左右分离）。
    pub output_mode: OutputMode,
    /// 蓝牙左右分离参数。
    pub split: SplitConfig,
    /// 开始分离时自动把 Windows 默认输出切到虚拟声卡（停止/退出面板时切回）。
    #[serde(default = "default_switch_output")]
    pub switch_output_on_split: bool,
    /// 自动切换前记录的默认输出端点 id，用于之后恢复（None = 不需要恢复）。
    #[serde(default)]
    pub previous_output: Option<String>,
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

/// `switch_output_on_split` 缺省值：默认开（否则开始分离后播放器声音进不来，音箱没声音）。
fn default_switch_output() -> bool {
    true
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
            output_mode: OutputMode::default(),
            split: SplitConfig::default(),
            switch_output_on_split: true,
            previous_output: None,
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
            Ok(text) => {
                // 记事本等编辑器可能写 UTF-8 BOM，serde_json 不认，先去掉。
                let text = text.trim_start_matches('\u{feff}');
                match serde_json::from_str::<Self>(text) {
                    Ok(cfg) => {
                        tracing::info!("已读取配置 {}", path.display());
                        cfg
                    }
                    Err(e) => {
                        tracing::warn!("配置文件解析失败（{}），改用默认值：{e}", path.display());
                        Self::default()
                    }
                }
            }
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
