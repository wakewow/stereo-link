//! StereoLink 的持久化配置。
//!
//! 存 `%APPDATA%\stereo-link\config.json`（没有 APPDATA 时退到 exe 同目录）。
//! 读失败/文件不存在一律回默认值——配置文件坏掉不该让面板起不来；
//! 解析失败时原文件会先改名备份成 `config.json.bad-<unix秒>`，绝不被默认值静默覆盖。
//!
//! 枚举字段（`volume_follow` / `close_behavior`）解析时大小写不敏感：
//! `"Auto"` / `"AUTO"` / `"Off"` 都能读，写回时统一用 snake_case 小写。

use std::path::{Path, PathBuf};

use eframe::egui;
use serde::{Deserialize, Serialize};

/// 声道路由参数（两条链路：左声道、右声道）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitConfig {
    /// 打开面板时自动开始（默认开：开着就有声音）。
    pub enabled: bool,
    /// 左声道推给哪个端点（完整友好名，例如 `扬声器 (Living Room Speaker)`）；空 = 还没选。
    pub left_endpoint: String,
    /// 右声道推给哪个端点；空 = 还没选。
    pub right_endpoint: String,
    /// 采集增益（1.0 = 原样）。
    pub gain: f32,
    /// 抖动缓冲：每条链路预排队的音频量（毫秒），越大越稳、延迟越高。
    pub queue_ms: u32,
    /// 左链路额外延迟（毫秒），用于校正两箱固定时差。
    pub left_delay_ms: u32,
    /// 右链路额外延迟（毫秒）。
    pub right_delay_ms: u32,
    /// 左链路反相 180°（两箱极性相反时，居中的人声/低音会互相抵消——勾上这个修）。
    pub invert_left: bool,
    /// 右链路反相 180°。
    pub invert_right: bool,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            left_endpoint: String::new(),
            right_endpoint: String::new(),
            gain: 1.0,
            queue_ms: 40,
            left_delay_ms: 0,
            right_delay_ms: 0,
            invert_left: false,
            invert_right: false,
        }
    }
}

/// 采集源设置（D 模式：任意播放端点都能当采集源，不再限定虚拟声卡）。
///
/// 引擎从「采集端点的 WASAPI 回环流」取声，再把左右声道分别推给两台蓝牙音箱。
/// 因此采集端点必须同时是 Windows 默认输出（否则播放器的声音不会流进它）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// 记住的采集端点 id（`{0.0.0.00000000}.{guid}`）。
    ///
    /// 端点名字可以改，id 不会变——所以优先认 id，认不出再回退到按名字提示自动挑。
    pub endpoint_id: Option<String>,
    /// 显示用的短名（`扬声器 (X)` → `X`）；只用于界面展示/兜底，不参与匹配。
    #[serde(alias = "name")]
    pub endpoint_name: String,
    /// 开始路由时自动把 Windows 默认输出切到采集端点（停止/退出时切回）。
    pub auto_switch_default: bool,
    /// 自动切换前记录的默认输出端点 id，用于恢复（None = 不需要恢复）。
    pub previous_output: Option<String>,
    /// 最后一次看到的「真实」默认输出（不是采集端点）。
    ///
    /// 兜底用：如果启动时默认输出已经是采集端点（例如上次被强杀、没来得及恢复），
    /// `previous_output` 会是 None，此时退出就靠它切回去。
    pub last_real_output: Option<String>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            endpoint_id: None,
            // 零安装默认：本机自研虚拟声卡优先（历史上的 Steam 改名设备也能被名字提示认出来）。
            endpoint_name: "立体声链虚拟声卡".to_string(),
            auto_switch_default: true,
            previous_output: None,
            last_real_output: None,
        }
    }
}

/// 音量联动策略。
///
/// 采集源分两类：**受系统音量控制**的（自研虚拟声卡、板载/HDMI 等物理输出——WASAPI 回环是
/// 后置音量，调音量采集幅度就跟着变）和**不受控制**的（UU远程、Steam 等第三方虚拟设备——
/// 采集幅度恒定，调它们的音量对音箱毫无作用）。后者必须把采集源的音量/静音镜像到每台目标
/// 设备，用户拖动系统音量条时音箱才会跟着变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VolumeFollow {
    /// 自动判断：虚拟设备（非自研）→ 镜像；物理输出/自研虚拟声卡 → 不镜像（否则音量会双重生效）。
    #[default]
    Auto,
    /// 总是把采集源的音量与静音镜像到每台目标设备。
    On,
    /// 完全不碰目标设备的音量。
    Off,
}

impl<'de> Deserialize<'de> for VolumeFollow {
    /// 大小写不敏感地解析（`"Auto"`/`"AUTO"`/`"Off"` 都接受），写回仍用 snake_case。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(VolumeFollow::Auto),
            "on" => Ok(VolumeFollow::On),
            "off" => Ok(VolumeFollow::Off),
            _ => Err(D::Error::custom(format!(
                "volume_follow 取值 {raw:?} 不合法（可选 auto / on / off，大小写不限）"
            ))),
        }
    }
}

impl VolumeFollow {
    /// 界面上显示的文字。
    pub fn label(self) -> &'static str {
        match self {
            VolumeFollow::Auto => "自动",
            VolumeFollow::On => "跟随系统音量",
            VolumeFollow::Off => "不联动",
        }
    }
}

/// 点窗口右上角关闭按钮时的行为。
///
/// 默认是 [`CloseBehavior::Ask`]：第一次关闭弹确认框，用户勾了「下次不再提示」后写回这里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CloseBehavior {
    /// 每次关闭都问一下（默认）。
    #[default]
    Ask,
    /// 直接缩到系统托盘，路由继续在后台跑。
    Minimize,
    /// 直接退出（停止路由并恢复默认输出）。
    Quit,
}

impl<'de> Deserialize<'de> for CloseBehavior {
    /// 大小写不敏感地解析（`"Ask"`/`"MINIMIZE"`/`"Quit"` 都接受），写回仍用 snake_case。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(CloseBehavior::Ask),
            "minimize" => Ok(CloseBehavior::Minimize),
            "quit" => Ok(CloseBehavior::Quit),
            _ => Err(D::Error::custom(format!(
                "close_behavior 取值 {raw:?} 不合法（可选 ask / minimize / quit，大小写不限）"
            ))),
        }
    }
}

/// 界面主题模式。
///
/// - [`ThemeMode::System`]（默认）：跟随 Windows 的浅色/深色设置，系统切换时界面即时跟着变；
/// - [`ThemeMode::Light`] / [`ThemeMode::Dark`]：用户手动锁定，不受系统影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// 跟随系统。
    #[default]
    System,
    /// 强制浅色。
    Light,
    /// 强制深色。
    Dark,
}

impl<'de> Deserialize<'de> for ThemeMode {
    /// 大小写不敏感地解析（`"System"`/`"LIGHT"`/`"dark"` 都接受），写回仍用 snake_case。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "system" => Ok(ThemeMode::System),
            "light" => Ok(ThemeMode::Light),
            "dark" => Ok(ThemeMode::Dark),
            _ => Err(D::Error::custom(format!(
                "theme 取值 {raw:?} 不合法（可选 system / light / dark，大小写不限）"
            ))),
        }
    }
}

impl ThemeMode {
    /// 全部模式（下拉框 / 循环切换用）。
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark];

    /// 界面上显示的短文字。
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "跟随系统",
            ThemeMode::Light => "浅色",
            ThemeMode::Dark => "深色",
        }
    }

    /// 循环切到下一个模式（侧边栏收起时点一下切换）。
    pub fn next(self) -> Self {
        match self {
            ThemeMode::System => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
            ThemeMode::Dark => ThemeMode::System,
        }
    }

    /// 转成 egui 的主题偏好（`System` = `None`，交给 winit/系统上报）。
    pub fn preference(self) -> Option<egui::Theme> {
        match self {
            ThemeMode::System => None,
            ThemeMode::Light => Some(egui::Theme::Light),
            ThemeMode::Dark => Some(egui::Theme::Dark),
        }
    }
}

/// 音量平衡校准的一个声学锚点（在某个主音量下测得的左右电平差）。
///
/// 与 [`stereo_audio::BalanceAnchor`] 一一对应；单独定义是因为 stereo-audio 不依赖 serde。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceAnchorConfig {
    /// 测量时的端点标量（两端相同，0..=1）。
    pub master: f32,
    /// `taper_db(master)`：插值在 dB 域做，比标量域更接近线性。
    pub master_db: f32,
    /// 实测电平差（dB，左 − 右）：正 = 左边更响。
    pub delta_db: f32,
    /// 实测左路电平（dBFS）。
    pub left_db: f32,
    /// 实测右路电平（dBFS）。
    pub right_db: f32,
    /// 这次测量的可信度（0..=1，由测试音信噪比决定）。
    pub confidence: f32,
}

impl Default for BalanceAnchorConfig {
    fn default() -> Self {
        Self {
            master: 0.0,
            master_db: 0.0,
            delta_db: 0.0,
            left_db: 0.0,
            right_db: 0.0,
            confidence: 0.0,
        }
    }
}

impl From<&stereo_audio::BalanceAnchor> for BalanceAnchorConfig {
    fn from(anchor: &stereo_audio::BalanceAnchor) -> Self {
        Self {
            master: anchor.master,
            master_db: anchor.master_db,
            delta_db: anchor.delta_db,
            left_db: anchor.left_db,
            right_db: anchor.right_db,
            confidence: anchor.confidence,
        }
    }
}

impl From<&BalanceAnchorConfig> for stereo_audio::BalanceAnchor {
    fn from(anchor: &BalanceAnchorConfig) -> Self {
        Self {
            master: anchor.master,
            master_db: anchor.master_db,
            delta_db: anchor.delta_db,
            left_db: anchor.left_db,
            right_db: anchor.right_db,
            confidence: anchor.confidence,
        }
    }
}

/// 音量平衡校准（持久化部分）：校准一次存这里，之后调系统音量不再需要重测。
///
/// 为什么不再存「左右端点音量百分比」：那种做法把平衡信息编码在两端标量之差里，而音量联动
/// 会把采集源标量原样镜像到每台目标设备 —— 差值瞬间归零，这就是旧版「平衡后一调音量就失衡」
/// 的根因。现在存的是「响度差」，播放时由路由引擎给较响的一路乘数字增益（见
/// [`stereo_audio::balance`]）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceCalConfig {
    /// 数据结构版本（见 [`stereo_audio::BALANCE_CAL_VERSION`]）。0 = 老配置/没校准过。
    pub version: u32,
    /// 校准时用的左端点选择串；与 `split.left_endpoint` 不一致即视为作废（换了音箱要重测）。
    pub left_endpoint: String,
    /// 校准时用的右端点选择串。
    pub right_endpoint: String,
    /// 声学锚点（按 master 升序）。
    pub anchors: Vec<BalanceAnchorConfig>,
    /// 左端点的「标量 → 端点自报 dB」曲线。
    pub left_curve: Vec<(f32, f32)>,
    /// 右端点的「标量 → 端点自报 dB」曲线。
    pub right_curve: Vec<(f32, f32)>,
    /// 满音量处的灵敏度差（dB，左 − 右）：扣掉端点电平后两台音箱本身的响度差。
    pub sens_diff_db: f32,
    /// 校准完成时间（Unix 秒；0 = 未知）。
    pub calibrated_at: u64,
}

impl BalanceCalConfig {
    /// 转成运行时可用的 [`stereo_audio::BalanceCalibration`]。
    pub fn to_calibration(&self) -> stereo_audio::BalanceCalibration {
        stereo_audio::BalanceCalibration {
            version: if self.version == 0 {
                stereo_audio::BALANCE_CAL_VERSION
            } else {
                self.version
            },
            left_endpoint: self.left_endpoint.clone(),
            right_endpoint: self.right_endpoint.clone(),
            anchors: self.anchors.iter().map(Into::into).collect(),
            left_curve: stereo_audio::EndpointCurve::new(self.left_curve.clone()),
            right_curve: stereo_audio::EndpointCurve::new(self.right_curve.clone()),
            sens_diff_db: self.sens_diff_db,
            calibrated_at: self.calibrated_at,
        }
    }
}

impl From<&stereo_audio::BalanceCalibration> for BalanceCalConfig {
    fn from(cal: &stereo_audio::BalanceCalibration) -> Self {
        Self {
            version: cal.version,
            left_endpoint: cal.left_endpoint.clone(),
            right_endpoint: cal.right_endpoint.clone(),
            anchors: cal.anchors.iter().map(Into::into).collect(),
            left_curve: cal.left_curve.points.clone(),
            right_curve: cal.right_curve.points.clone(),
            sens_diff_db: cal.sens_diff_db,
            calibrated_at: cal.calibrated_at,
        }
    }
}

/// 手机投放（AirPlay / 小米妙播）设置。
///
/// 只记「面板启动时要不要自动拉起接收端」；输出声卡固定跟随 Windows 默认播放
/// 设备，不做持久化。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CastConfig {
    /// 面板启动时自动拉起 AirPlay 接收端。
    pub auto_airplay: bool,
    /// 面板启动时自动拉起小米妙播接收端。
    pub auto_miplay: bool,
    /// 妙播低延迟模式：把目标延迟从 800 ms 压到 220 ms（代价：和小米音箱一起
    /// 「全屋播放」时会和它们不同步）。
    pub low_latency: bool,
}

/// 面板持久化配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// 声道路由参数。
    pub split: SplitConfig,
    /// 采集源设置。
    ///
    /// `alias = "virtual_card"`：兼容 1.x 的旧配置（那时采集源写死是虚拟声卡），
    /// 旧字段会按 [`CaptureConfig`] 的 `name`/`endpoint_id` 等字段迁移过来，
    /// 尤其不能丢 `previous_output`（否则退出时恢复不了默认输出）。
    #[serde(alias = "virtual_card")]
    pub capture: CaptureConfig,
    /// 是否展开日志面板。
    pub show_log: bool,
    /// 自检音时长（秒）。
    pub tone_seconds: f64,
    /// 面板启动时自动开始路由。
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// 声学自校验用的麦克风端点 id（`{GUID}`），空 = 还没选。
    pub sync_mic: String,
    /// 音量联动策略（默认自动）。
    #[serde(default)]
    pub volume_follow: VolumeFollow,
    /// 关闭窗口时的行为（默认每次询问）。
    ///
    /// `#[serde(default)]`：老配置文件里没有这个字段时按 [`CloseBehavior::Ask`] 处理。
    #[serde(default)]
    pub close_behavior: CloseBehavior,
    /// 界面主题（默认跟随系统）。
    ///
    /// `#[serde(default)]`：老配置文件没有这个字段时按 [`ThemeMode::System`] 处理。
    #[serde(default)]
    pub theme: ThemeMode,
    /// 音量平衡校准结果（端点曲线 + 声学锚点）；空 = 没校准过，不做平衡补偿。
    #[serde(default)]
    pub balance: BalanceCalConfig,
    /// 手机投放接收端设置。
    #[serde(default)]
    pub cast: CastConfig,
}

fn default_auto_start() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            split: SplitConfig::default(),
            capture: CaptureConfig::default(),
            show_log: true,
            tone_seconds: 3.0,
            auto_start: true,
            sync_mic: String::new(),
            volume_follow: VolumeFollow::Auto,
            close_behavior: CloseBehavior::Ask,
            theme: ThemeMode::System,
            balance: BalanceCalConfig::default(),
            cast: CastConfig::default(),
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
        Self::load_from(&Self::path())
    }

    /// 从指定路径读配置（单测/真机复现用，避免碰真实的 `%APPDATA%`）。
    ///
    /// 解析失败时**先把坏文件改名备份**再回默认值，避免之后 [`AppConfig::save`]
    /// 用默认值把用户原配置静默覆盖掉。
    fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("没有配置文件，使用默认值（{}）", path.display());
                return Self::default();
            }
            Err(e) => {
                tracing::warn!("读取配置 {} 失败：{e}，改用默认值", path.display());
                return Self::default();
            }
        };
        // 记事本等编辑器可能写 UTF-8 BOM，serde_json 不认，先去掉。
        let text = text.trim_start_matches('\u{feff}');
        match serde_json::from_str::<Self>(text) {
            Ok(cfg) => {
                tracing::info!("已读取配置 {}", path.display());
                cfg
            }
            Err(e) => {
                match backup_bad_config(path) {
                    Some(backup) => tracing::warn!(
                        "配置解析失败（{}），原文件已备份为 {}，改用默认值：{e}",
                        path.display(),
                        backup.display()
                    ),
                    None => tracing::warn!(
                        "配置解析失败（{}），备份原文件也失败（下次保存会覆盖原文件），改用默认值：{e}",
                        path.display()
                    ),
                }
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

/// 把解析失败的配置文件改名成 `config.json.bad-<unix秒>`，返回备份路径。
///
/// 同一秒内连续启动可能重名，冲突时追加 `-<序号>`；改名失败返回 `None`（调用方会记日志）。
fn backup_bad_config(path: &Path) -> Option<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    for attempt in 0..100u32 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let candidate = path.with_file_name(format!("{file_name}.bad-{stamp}{suffix}"));
        if candidate.exists() {
            continue;
        }
        return match std::fs::rename(path, &candidate) {
            Ok(()) => Some(candidate),
            Err(e) => {
                tracing::warn!("备份坏配置到 {} 失败：{e}", candidate.display());
                None
            }
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1.x 的配置文件里采集源叫 `virtual_card`、名字字段叫 `name`；
    /// 升级后必须能原样迁移，尤其是 previous_output（否则退出恢复不了默认输出）。
    #[test]
    fn legacy_virtual_card_section_migrates_to_capture() {
        let old = r#"{
            "split": { "left_endpoint": "扬声器 (Living Room Speaker)" },
            "virtual_card": {
                "name": "我的声卡",
                "endpoint_id": "{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}",
                "auto_switch_default": false,
                "previous_output": "{0.0.0.00000000}.{d7dda16f-8446-42dd-ac1f-afa5d9d07e19}",
                "last_real_output": "{0.0.0.00000000}.{4f24a0b8-7ad0-4c62-8db7-b15fba978b01}"
            },
            "show_log": false
        }"#;
        let cfg: AppConfig = serde_json::from_str(old).expect("旧配置必须能解析");
        assert_eq!(cfg.capture.endpoint_name, "我的声卡");
        assert_eq!(
            cfg.capture.endpoint_id.as_deref(),
            Some("{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}")
        );
        assert!(!cfg.capture.auto_switch_default);
        assert_eq!(
            cfg.capture.previous_output.as_deref(),
            Some("{0.0.0.00000000}.{d7dda16f-8446-42dd-ac1f-afa5d9d07e19}")
        );
        assert_eq!(
            cfg.capture.last_real_output.as_deref(),
            Some("{0.0.0.00000000}.{4f24a0b8-7ad0-4c62-8db7-b15fba978b01}")
        );
        assert!(!cfg.show_log);
        // 新配置写回时用新字段名，不再写 virtual_card。
        let json = serde_json::to_string(&cfg).expect("配置要能序列化");
        assert!(json.contains("\"capture\""));
        assert!(!json.contains("\"virtual_card\""));
    }

    /// 开源默认值不能写死任何本机设备名：首次启动左右声道应为空，由用户在设备表里选。
    #[test]
    fn default_split_has_no_hardcoded_devices() {
        let cfg = AppConfig::default();
        assert!(cfg.split.left_endpoint.is_empty());
        assert!(cfg.split.right_endpoint.is_empty());
    }

    /// 音量平衡校准要能整份持久化：锚点/曲线/灵敏度差写出去再读回来必须一致；
    /// 老配置（没有 `balance` 字段）必须回默认 —— 没校准过就等于不补偿。
    #[test]
    fn balance_calibration_round_trips_and_defaults_to_empty() {
        let old: AppConfig = serde_json::from_str(r#"{"show_log": false}"#).expect("老配置");
        assert!(old.balance.anchors.is_empty());
        assert_eq!(old.balance.sens_diff_db, 0.0);

        let cal = stereo_audio::BalanceCalibration {
            version: stereo_audio::BALANCE_CAL_VERSION,
            left_endpoint: "扬声器 (Xiaomi Sound-1695)".to_string(),
            right_endpoint: "扬声器 (Xiaomi Speaker-4162)".to_string(),
            anchors: vec![stereo_audio::BalanceAnchor {
                master: 0.6,
                master_db: -7.6,
                delta_db: 4.25,
                left_db: -22.5,
                right_db: -26.75,
                confidence: 0.92,
            }],
            left_curve: stereo_audio::EndpointCurve::new(vec![(0.3, -17.8), (1.0, 0.0)]),
            right_curve: stereo_audio::EndpointCurve::new(vec![(0.3, -19.1), (1.0, 0.0)]),
            sens_diff_db: 5.1,
            calibrated_at: 1_760_000_000,
        };
        let cfg = BalanceCalConfig::from(&cal);
        let json = serde_json::to_string(&cfg).expect("序列化");
        let back: BalanceCalConfig = serde_json::from_str(&json).expect("反序列化");
        let restored = back.to_calibration();
        assert_eq!(restored.left_endpoint, cal.left_endpoint);
        assert_eq!(restored.right_endpoint, cal.right_endpoint);
        assert_eq!(restored.anchors, cal.anchors);
        assert_eq!(restored.left_curve, cal.left_curve);
        assert_eq!(restored.right_curve, cal.right_curve);
        assert!((restored.sens_diff_db - cal.sens_diff_db).abs() < 1e-6);
    }

    /// 关闭行为：老配置没有该字段时默认「每次询问」，写回时是 snake_case。
    #[test]
    fn close_behavior_defaults_to_ask_and_round_trips() {
        assert_eq!(AppConfig::default().close_behavior, CloseBehavior::Ask);

        let old = r#"{"show_log": false}"#;
        let cfg: AppConfig = serde_json::from_str(old).expect("缺字段的旧配置要能解析");
        assert_eq!(cfg.close_behavior, CloseBehavior::Ask);

        let behavior: CloseBehavior = serde_json::from_str("\"minimize\"").unwrap();
        assert_eq!(behavior, CloseBehavior::Minimize);
        let json = serde_json::to_string(&CloseBehavior::Quit).unwrap();
        assert_eq!(json, "\"quit\"");
    }

    /// 用户手改配置时大小写经常不一致：`"Off"` / `"AUTO"` 之类必须能解析，
    /// 写回时仍统一输出 snake_case 小写。
    #[test]
    fn volume_follow_accepts_mixed_case() {
        for raw in ["\"auto\"", "\"Auto\"", "\"AUTO\"", "\"aUtO\""] {
            let value: VolumeFollow = serde_json::from_str(raw).unwrap_or_else(|e| {
                panic!("{raw} 应能解析成 Auto：{e}");
            });
            assert_eq!(value, VolumeFollow::Auto, "{raw}");
        }
        for raw in ["\"on\"", "\"On\"", "\"ON\""] {
            let value: VolumeFollow = serde_json::from_str(raw).expect("On 大小写不敏感");
            assert_eq!(value, VolumeFollow::On, "{raw}");
        }
        for raw in ["\"off\"", "\"Off\"", "\"OFF\""] {
            let value: VolumeFollow = serde_json::from_str(raw).expect("Off 大小写不敏感");
            assert_eq!(value, VolumeFollow::Off, "{raw}");
        }
        // 写回仍是 snake_case 小写。
        assert_eq!(
            serde_json::to_string(&VolumeFollow::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&VolumeFollow::Off).unwrap(),
            "\"off\""
        );
        // 真正非法的值仍要报错（不能悄悄变成默认值）。
        assert!(serde_json::from_str::<VolumeFollow>("\"always\"").is_err());
    }

    /// `close_behavior` 同样大小写不敏感，写回仍是 snake_case 小写。
    #[test]
    fn close_behavior_accepts_mixed_case() {
        for raw in ["\"ask\"", "\"Ask\"", "\"ASK\""] {
            let value: CloseBehavior = serde_json::from_str(raw).expect("Ask 大小写不敏感");
            assert_eq!(value, CloseBehavior::Ask, "{raw}");
        }
        for raw in ["\"minimize\"", "\"Minimize\"", "\"MINIMIZE\""] {
            let value: CloseBehavior = serde_json::from_str(raw).expect("Minimize 大小写不敏感");
            assert_eq!(value, CloseBehavior::Minimize, "{raw}");
        }
        for raw in ["\"quit\"", "\"Quit\"", "\"QUIT\""] {
            let value: CloseBehavior = serde_json::from_str(raw).expect("Quit 大小写不敏感");
            assert_eq!(value, CloseBehavior::Quit, "{raw}");
        }
        assert_eq!(
            serde_json::to_string(&CloseBehavior::Minimize).unwrap(),
            "\"minimize\""
        );
        assert!(serde_json::from_str::<CloseBehavior>("\"exit\"").is_err());
    }

    /// 整份配置里写 `"Off"`（大小写不符）也要能正常解析，不能整份回默认。
    #[test]
    fn config_with_mixed_case_enum_values_parses() {
        let json = r#"{
            "volume_follow": "Off",
            "close_behavior": "QUIT",
            "show_log": false
        }"#;
        let cfg: AppConfig = serde_json::from_str(json).expect("大小写混合的枚举值必须能解析");
        assert_eq!(cfg.volume_follow, VolumeFollow::Off);
        assert_eq!(cfg.close_behavior, CloseBehavior::Quit);
        assert!(!cfg.show_log);
        // 其他缺失字段仍按默认值。
        assert_eq!(cfg.split.left_endpoint, "");
        assert_eq!(cfg.capture.endpoint_name, "立体声链虚拟声卡");
    }

    /// 完全非法的 JSON：回默认值，且原文件必须先改名成 `config.json.bad-*`，
    /// 原内容仍可读——绝不能留给之后的 save() 静默覆盖。
    #[test]
    fn invalid_config_is_backed_up_before_falling_back() {
        let dir = std::env::temp_dir().join(format!(
            "stereo-link-config-bad-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("config.json");
        let broken = "{ 这不是合法 JSON: [1, 2, 3 ";
        std::fs::write(&path, broken).expect("写坏配置");

        let cfg = AppConfig::load_from(&path);

        // 回默认值。
        assert_eq!(cfg.volume_follow, VolumeFollow::Auto);
        assert_eq!(cfg.close_behavior, CloseBehavior::Ask);
        // 原路径已不存在，出现且仅出现一个 config.json.bad-*。
        assert!(!path.exists(), "坏配置应被改名备份");
        let mut backups: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("列临时目录")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("config.json.bad-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 1, "应恰好有一个 .bad-* 备份：{backups:?}");
        let backup = backups.pop().unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).expect("备份内容可读"),
            broken,
            "备份内容必须是原文件原样"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧配置缺新字段时仍按默认值解析（迁移不报错）。
    #[test]
    fn old_config_without_new_fields_still_parses() {
        let cfg: AppConfig = serde_json::from_str(r#"{"split": {"enabled": false}}"#)
            .expect("缺字段的旧配置要能解析");
        assert!(!cfg.split.enabled);
        assert_eq!(cfg.volume_follow, VolumeFollow::Auto);
        assert_eq!(cfg.close_behavior, CloseBehavior::Ask);
        assert_eq!(cfg.capture.endpoint_name, "立体声链虚拟声卡");
        assert!(cfg.split.left_endpoint.is_empty());
    }
}
