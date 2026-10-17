//! StereoLink 的持久化配置。
//!
//! 存 `%APPDATA%\stereo-link\config.json`（没有 APPDATA 时退到 exe 同目录）。
//! 读失败/文件不存在一律回默认值——配置文件坏掉不该让面板起不来。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 声道路由参数（两条链路：左声道、右声道）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitConfig {
    /// 打开面板时自动开始（默认开：开着就有声音）。
    pub enabled: bool,
    /// 左声道推给哪个端点（完整友好名，例如 `扬声器 (Xiaomi Sound-1695)`）。
    pub left_endpoint: String,
    /// 右声道推给哪个端点。
    pub right_endpoint: String,
    /// 采集源（默认空 = 自动找虚拟声卡）。
    pub source_endpoint: String,
    /// 采集增益（1.0 = 原样）。
    pub gain: f32,
    /// 抖动缓冲：每条链路预排队的音频量（毫秒），越大越稳、延迟越高。
    pub queue_ms: u32,
    /// 左链路额外延迟（毫秒），用于校正两箱固定时差。
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
            source_endpoint: String::new(),
            gain: 1.0,
            queue_ms: 40,
            left_delay_ms: 0,
            right_delay_ms: 0,
        }
    }
}

/// 虚拟声卡相关设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VirtualCardConfig {
    /// 用户给虚拟声卡起的名字（用于重命名、匹配端点）。
    pub name: String,
    /// 记住的虚拟声卡端点 id（`{0.0.0.00000000}.{guid}`）。
    ///
    /// 名字可以随时改，端点 id 不会变——所以识别虚拟声卡优先认 id，其次才按名字猜。
    pub endpoint_id: Option<String>,
    /// 开始路由时自动把 Windows 默认输出切到虚拟声卡（停止/退出时切回）。
    pub auto_switch_default: bool,
    /// 自动切换前记录的默认输出端点 id，用于恢复（None = 不需要恢复）。
    pub previous_output: Option<String>,
    /// 最后一次看到的「真实」默认输出（非虚拟声卡）。
    ///
    /// 兜底用：如果启动时默认输出已经是虚拟声卡（例如上次被强杀、没来得及恢复），
    /// `previous_output` 会是 None，此时退出就靠它切回去。
    pub last_real_output: Option<String>,
}

impl Default for VirtualCardConfig {
    fn default() -> Self {
        Self {
            name: "立体声链虚拟声卡".to_string(),
            endpoint_id: None,
            auto_switch_default: true,
            previous_output: None,
            last_real_output: None,
        }
    }
}

/// 面板持久化配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// 声道路由参数。
    pub split: SplitConfig,
    /// 虚拟声卡设置。
    pub virtual_card: VirtualCardConfig,
    /// 是否展开日志面板。
    pub show_log: bool,
    /// 自检音时长（秒）。
    pub tone_seconds: f64,
    /// 面板启动时自动开始路由。
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// 声学自校验用的麦克风端点 id（`{GUID}`），空 = 还没选。
    pub sync_mic: String,
}

fn default_auto_start() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            split: SplitConfig::default(),
            virtual_card: VirtualCardConfig::default(),
            show_log: true,
            tone_seconds: 3.0,
            auto_start: true,
            sync_mic: String::new(),
        }
    }
}

impl AppConfig {
    /// 配置文件路径。
    pub fn path() -> PathBuf {
        if let Some(base) = std::env::var_os("APPDATA") {
            let dir = PathBuf::from(base).join("stereo-link");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("config.json");
        }
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("config.json")))
            .unwrap_or_else(|| PathBuf::from("config.json"))
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
                        tracing::warn!("配置解析失败（{}），改用默认值：{e}", path.display());
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
