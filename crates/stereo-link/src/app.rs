//! StereoLink 立体声链 —— 桌面面板。
//!
//! 界面按「左侧可收起侧边栏 + 四个页面」组织：
//! - 顶栏：身份、运行状态、开始 / 停止路由；
//! - 顶栏下方诊断条：只有「采集源就绪 / 链路正常 / 系统默认输出」三个状态徽章；
//! - 左侧导航：声道路由 / 采集源 / 本机播放设备 / 日志（可收起成图标条）；
//! - 页面内容：路由表（每路音量条 + 反相开关）、采集源设置、设备表、日志。
//!
//! 设计要点：
//! - 面板只做两件事：把「采集源」的 WASAPI 回环流拆成左右两路推给两台蓝牙音箱；
//!   把 Windows 默认输出切到采集源（并在退出时切回）。
//! - 采集源不再限定虚拟声卡：任意播放端点都能选（自研虚拟声卡、UU远程虚拟设备、
//!   没接音箱的物理输出都行），只要它同时是 Windows 默认输出就能采到声音。
//! - 引擎跑在自己的线程里（见 `stereo_audio::route`），UI 线程只读快照 + 发命令。
//! - 已发流 / 欠载这类运行数据只写日志，界面上不再常驻显示，避免表格越来越宽。
//! - 状态用少量自定义绘制（电平条、胶囊、徽章、卡片）代替默认控件外观。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui::{
    Align, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2,
};
use stereo_audio::{
    get_endpoint_volume, set_endpoint_mute, set_endpoint_volume, BalanceReport, DataFlow,
    EndpointFormat, EndpointInfo, EndpointVolume, MicCandidate, MicHealth, PcmFormat, RouteChannel,
    RouteConfig, RouteEngineConfig, RouteEngineHandle, RouteEngineStats, RouteStats, SyncConfig,
    SyncMethod, SyncResult, BALANCE_DEADBAND_DB,
};

use crate::audio_default;
use crate::close_flow::{self, CloseOutcome, CloseState};
use crate::config::{AppConfig, VolumeFollow};
use crate::fonts;
use crate::logbuf::{LogBuffer, LogLine};
use crate::tray::{Tray, TrayEvent};

// ---------------------------------------------------------------------------
// 主题：深色 + 青/紫双声道配色
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(11, 13, 17);
const PANEL: Color32 = Color32::from_rgb(18, 21, 27);
const CARD: Color32 = Color32::from_rgb(22, 26, 33);
const CARD_SOFT: Color32 = Color32::from_rgb(27, 32, 40);
const BORDER: Color32 = Color32::from_rgb(38, 44, 54);
const TEXT: Color32 = Color32::from_rgb(232, 236, 244);
const MUTED: Color32 = Color32::from_rgb(132, 142, 158);
const ACCENT: Color32 = Color32::from_rgb(56, 189, 248); // 左声道：青
const ACCENT2: Color32 = Color32::from_rgb(167, 139, 250); // 右声道：紫
const OK: Color32 = Color32::from_rgb(74, 222, 128);
const WARN: Color32 = Color32::from_rgb(251, 191, 36);
const ERR: Color32 = Color32::from_rgb(248, 113, 113);

/// 卡片外框。
fn card_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .inner_margin(Margin::symmetric(18, 16))
        .corner_radius(CornerRadius::same(12))
}

/// 固定宽度的药丸标签：宽度写死，内容居中，不随文字伸缩。
fn pill(ui: &mut egui::Ui, text: &str, bg: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), bg);
    let galley =
        painter.layout_no_wrap(text.to_owned(), FontId::proportional(12.0), Color32::BLACK);
    let pos = egui::pos2(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    painter.galley(pos, galley, Color32::BLACK);
}

/// 小圆圈「?」：hover 才显示完整说明（说明不再常驻占位）。
fn help(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.circle_stroke(
            rect.center(),
            8.0,
            Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.8)),
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "?",
            FontId::proportional(11.0),
            ACCENT,
        );
    }
    response.on_hover_text(text)
}

/// 固定宽高的布局槽：里面的内容怎么变，占位宽高都不变。
fn cell<R>(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    layout: Layout,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.allocate_ui_with_layout(Vec2::new(width, height), layout, |ui| {
        ui.set_min_size(Vec2::new(width, height));
        add_contents(ui)
    })
    .inner
}

/// 量出一行文本的排版；超过 `max_w` 就截断并加省略号。
fn fit_one_line(
    ui: &egui::Ui,
    text: &str,
    font: &FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::text::Galley> {
    let painter = ui.painter();
    let full = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    if full.size().x <= max_w || text.is_empty() {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    let mut best = "…".to_owned();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let mut candidate: String = chars[..mid].iter().collect();
        candidate.push('…');
        let galley = painter.layout_no_wrap(candidate.clone(), font.clone(), color);
        if galley.size().x <= max_w {
            best = candidate;
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    painter.layout_no_wrap(best, font.clone(), color)
}

/// 固定宽度的单行文本单元格：内容再长也不撑宽这一列（截断，全文放 tooltip）。
fn text_cell(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    text: &str,
    font: FontId,
    color: Color32,
    right_aligned: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    if ui.is_rect_visible(rect) {
        let galley = fit_one_line(ui, text, &font, color, rect.width());
        let x = if right_aligned {
            rect.right() - galley.size().x
        } else {
            rect.left()
        };
        let pos = egui::pos2(x, rect.center().y - galley.size().y * 0.5);
        // 注意：with_clip_rect 是「替换」裁剪矩形，必须与 UI 自身的裁剪区求交，
        // 否则当单元格只被部分裁掉时，文本会画到卡片/面板之外（左侧边缘出现残字）。
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        painter.galley(pos, galley, color);
    }
    response
}

/// 太长时截成「前缀…」，用于 ComboBox 选中文本（避免换行把行高撑高）。
fn clip_text(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

// ---- 声道路由表：列宽写死，数值列右对齐 ----
const ROUTE_COL_CH: f32 = 52.0;
const ROUTE_COL_LEVEL: f32 = 84.0;
/// 每路音量条：占用原先「已发流 / 欠载」两列的位置。
const ROUTE_COL_VOL: f32 = 150.0;
const ROUTE_COL_BUF: f32 = 72.0;
const ROUTE_COL_PATH: f32 = 44.0;
/// 反相开关列：从「高级」里移出来，每路都直接可见。
const ROUTE_COL_INV: f32 = 96.0;
const ROUTE_ROW_H: f32 = 28.0;

/// 左侧导航栏展开 / 收起宽度。
const SIDEBAR_W: f32 = 176.0;
const SIDEBAR_W_COLLAPSED: f32 = 48.0;

/// 运行数据（已发流 / 欠载 / 缓冲）写日志的周期：界面上不再显示，只进日志。
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// 「系统音量」联动的悬停说明。
const VOLUME_FOLLOW_HELP: &str = "\
系统音量怎么作用到音箱：\n\
· 第三方虚拟声卡（UU远程、Steam 等）不吃系统音量 —— 调它自己的音量音箱毫无反应，\
所以自动模式会把采集源的音量/静音镜像到每台目标设备，拖系统音量条时音箱就跟着变；\n\
· 自研虚拟声卡与板载/HDMI 等物理输出是「后置音量」—— 回环采集到的幅度已经跟着音量变过了，\
再镜像一次会双重生效（音量变化过陡），所以自动模式不联动；\n\
· 联动期间改过的目标设备音量，会在停止路由/退出面板时还原成原值（强杀不还原）。";

/// 设备列吃掉剩余宽度：同一窗口尺寸下恒定，不随数值变化。
fn route_device_width(available: f32, spacing: f32) -> f32 {
    let fixed = ROUTE_COL_CH
        + ROUTE_COL_LEVEL
        + ROUTE_COL_VOL
        + ROUTE_COL_BUF
        + ROUTE_COL_PATH
        + ROUTE_COL_INV;
    (available - fixed - spacing * 7.0).max(150.0)
}

/// 描边小徽标。
fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let font = FontId::proportional(11.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let pad = Vec2::new(7.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), CARD_SOFT);
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, color.gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.min + pad, galley, color);
    response
}

/// 电平条。
fn level_bar(ui: &mut egui::Ui, width: f32, level: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 9.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(5), Color32::from_rgb(30, 35, 44));
    painter.rect_stroke(
        rect,
        CornerRadius::same(5),
        Stroke::new(1.0_f32, Color32::from_rgb(44, 51, 62)),
        egui::StrokeKind::Inside,
    );
    let ratio = level.clamp(0.0, 1.0);
    let filled = rect.width() * ratio;
    if filled > 1.5 {
        let bar = Rect::from_min_size(
            rect.min + Vec2::new(1.0, 1.0),
            Vec2::new((filled - 2.0).max(1.0), rect.height() - 2.0),
        );
        let c = if ratio > 0.85 {
            ERR
        } else if ratio > 0.6 {
            WARN
        } else {
            color
        };
        painter.rect_filled(bar, CornerRadius::same(4), c);
    }
    let mid = rect.min.x + rect.width() * 0.5;
    painter.line_segment(
        [egui::pos2(mid, rect.min.y), egui::pos2(mid, rect.max.y)],
        Stroke::new(1.0_f32, Color32::from_rgb(58, 65, 78)),
    );
}

fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(MUTED)
}

/// 诊断条里的小状态点：绿 = 正常，黄 = 注意。
fn status_dot(ui: &mut egui::Ui, ok: bool, ok_text: &str, bad_text: &str, tip: &str) {
    let (text, color) = if ok { (ok_text, OK) } else { (bad_text, WARN) };
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(RichText::new(format!("● {text}")).size(12.0).color(color))
            .selectable(false),
    )
    .on_hover_text(tip);
    ui.add_space(10.0);
}

/// `扬声器 (Living Room Speaker)` → `Living Room Speaker`。
fn short_endpoint(name: &str) -> String {
    let trimmed = name.trim();
    if let (Some(open), Some(close)) = (trimmed.find('('), trimmed.rfind(')')) {
        if close > open {
            return trimmed[open + 1..close].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn endpoint_format(ep: &EndpointInfo) -> String {
    match &ep.mix_format {
        Some(m) => format!("{} Hz / {} ch / {} bit", m.samplerate, m.channels, m.bits),
        None => "格式未知".to_string(),
    }
}

fn channel_color(channel: RouteChannel) -> Color32 {
    match channel {
        RouteChannel::Left => ACCENT,
        RouteChannel::Right => ACCENT2,
        RouteChannel::Both => OK,
    }
}

/// 声道的中文名（引擎只提供 `left/right/both` 字符串）。
fn channel_label(channel: RouteChannel) -> &'static str {
    match channel {
        RouteChannel::Left => "左声道",
        RouteChannel::Right => "右声道",
        RouteChannel::Both => "立体声",
    }
}

/// 采集端点的类别：决定界面标注、是否推荐、以及要不要给兜底提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureKind {
    /// 虚拟声卡/虚拟音频设备：无声，最适合当采集源。
    Virtual,
    /// 蓝牙音箱：本身会出声，当采集源会重复出声。
    Bluetooth,
    /// 其他物理输出：可能从接着的音箱漏声。
    Physical,
}

impl CaptureKind {
    /// 下拉框/诊断里的完整说明。
    fn label(self) -> &'static str {
        match self {
            CaptureKind::Virtual => "虚拟声卡（无声，推荐）",
            CaptureKind::Bluetooth => "蓝牙音箱（会重复出声，别选）",
            CaptureKind::Physical => "物理输出（可能出声）",
        }
    }

    /// 卡片右上角徽标用的短名。
    fn short_label(self) -> &'static str {
        match self {
            CaptureKind::Virtual => "虚拟设备",
            CaptureKind::Bluetooth => "蓝牙音箱",
            CaptureKind::Physical => "物理输出",
        }
    }

    fn color(self) -> Color32 {
        match self {
            CaptureKind::Virtual => OK,
            CaptureKind::Bluetooth => WARN,
            CaptureKind::Physical => MUTED,
        }
    }

    fn is_virtual(self) -> bool {
        self == CaptureKind::Virtual
    }
}

/// 按端点名判断采集源类别（虚拟声卡 / 蓝牙音箱 / 其他物理输出）。
fn capture_kind(name: &str) -> CaptureKind {
    if audio_default::is_virtual_name(name) {
        return CaptureKind::Virtual;
    }
    // 通用蓝牙判据：只认系统/协议层关键词，不写死任何厂商名。
    const BLUETOOTH_HINTS: &[&str] = &["Bluetooth", "蓝牙", "立体声组合", "Hands-Free", "A2DP"];
    if BLUETOOTH_HINTS.iter().any(|hint| name.contains(hint)) {
        return CaptureKind::Bluetooth;
    }
    CaptureKind::Physical
}

/// 采集源是不是「调它自己的音量对音箱没用」的第三方虚拟设备（自动策略的依据）。
///
/// 自研虚拟声卡与物理输出的 WASAPI 回环是**后置音量**：调音量采集幅度就跟着变，
/// 再镜像到目标设备就会双重生效（音量变化过陡），所以这两类不联动。
fn ignores_system_volume(name: &str) -> bool {
    capture_kind(name) == CaptureKind::Virtual && !name.contains("立体声链")
}

/// 引擎要的采集源字符串：完整名字在 render 列表里重复时用端点 id（精确匹配），否则用完整名。
///
/// 本机两个 NVIDIA 端点同名，按名字匹配会选错；`{...}` 形状由 `stereo-audio`
/// 的路由引擎按端点 id 精确匹配。
fn capture_selector_for(ep: &EndpointInfo, renders: &[EndpointInfo]) -> String {
    let duplicated = renders.iter().filter(|e| e.name == ep.name).count() > 1;
    if duplicated {
        ep.id.clone()
    } else {
        ep.name.clone()
    }
}

// ---------------------------------------------------------------------------
// 诊断条目
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Ok,
    Warn,
    Err,
}

struct Hint {
    tone: Tone,
    text: String,
}

impl Hint {
    fn icon(&self) -> &'static str {
        match self.tone {
            Tone::Ok => "√",
            Tone::Warn => "!",
            Tone::Err => "×",
        }
    }
}

// ---------------------------------------------------------------------------
// 应用
// ---------------------------------------------------------------------------

/// 左侧导航的四个页面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    /// 声道路由（主页）：左右设备、每路音量、反相、校准与音量平衡、高级。
    Routing,
    /// 采集源：端点选择、默认输出切换、系统音量联动、采集源音量诊断、测试音。
    Capture,
    /// 本机播放设备：格式 / 角色 / 设为左 / 设为右。
    Devices,
    /// 日志：运行日志（默认关闭，只在点进来时渲染）。
    Log,
}

impl Page {
    /// 导航栏里的全部页面（顺序即显示顺序）。
    const ALL: [Page; 4] = [Page::Routing, Page::Capture, Page::Devices, Page::Log];

    /// 导航栏里的中文标题。
    fn label(self) -> &'static str {
        match self {
            Page::Routing => "声道路由",
            Page::Capture => "采集源",
            Page::Devices => "本机播放设备",
            Page::Log => "日志",
        }
    }

    /// 收起侧边栏时显示的单字图标（用中文字，避免符号字体缺字）。
    fn icon(self) -> &'static str {
        match self {
            Page::Routing => "路",
            Page::Capture => "源",
            Page::Devices => "设",
            Page::Log => "志",
        }
    }
}

/// 后台校准线程的返回值：延迟对齐 + 音量平衡（平衡失败不拖累对齐结果）。
struct CalibrateOutcome {
    sync: Result<SyncResult, String>,
    balance: Result<BalanceReport, String>,
}

pub struct StereoLinkApp {
    cfg: AppConfig,
    engine: Option<RouteEngineHandle>,
    stats: Option<RouteEngineStats>,
    endpoints: Vec<EndpointInfo>,
    endpoints_error: Option<String>,
    log: LogBuffer,
    notice: Option<(String, Instant)>,
    font: Option<String>,
    /// 默认输出（每 ~1s 实时探测；引擎快照里的值只是启动时的静态值）。
    live_default_output: Option<String>,
    /// 当前 Windows 默认输出是不是采集端点（不是就采不到声）。
    live_default_is_capture: bool,
    live_default_at: Option<Instant>,
    auto_started: bool,
    exit_handled: bool,
    /// 每个端点当前设备默认格式（含读取失败）。
    format_cache: BTreeMap<String, Option<EndpointFormat>>,
    /// 每个端点**真实支持**的格式（独占模式 `IsFormatSupported` 探测；后台线程填充）。
    format_supported: BTreeMap<String, Option<Vec<EndpointFormat>>>,
    /// 正在后台探测格式的端点 id。
    format_probing: BTreeSet<String>,
    /// 格式探测线程的结果通道。
    format_rx: Option<std::sync::mpsc::Receiver<(String, Option<Vec<EndpointFormat>>)>>,
    /// 每个端点选中的格式（只会从该端点的支持列表里挑）。
    format_choices: BTreeMap<String, EndpointFormat>,
    /// 正在应用格式。
    format_busy: bool,
    /// 声学自校验：本机可选麦克风（含虚拟/回环端点，标出类别）。
    sync_mics: Vec<MicCandidate>,
    /// 自校验选中的麦克风端点 id（`{GUID}`）；空 = 还没选。
    sync_mic: String,
    /// 选中麦克风的体检结果（决定能不能自动对齐）。
    sync_health: Option<MicHealth>,
    /// 自校验状态说明，直接显示在按钮下方。
    sync_status: String,
    /// 自校验（测量）进行中。
    sync_busy: bool,
    /// 后台自校验线程的结果通道（延迟对齐 + 音量平衡一起返回）。
    sync_rx: Option<std::sync::mpsc::Receiver<CalibrateOutcome>>,
    /// 最近一次自校验结果。
    sync_result: Option<SyncResult>,
    /// 手动对齐点击串正在播。
    clicks_busy: bool,
    /// 点击串开始时间（用于自动解除忙碌标记）。
    clicks_at: Option<Instant>,
    /// 音量联动：目标设备的原始音量（写之前记下，停止路由时还原）。
    volume_saved: BTreeMap<String, (f32, bool)>,
    /// 上一次镜像出去的音量与静音，避免每帧都写 COM。
    volume_last: Option<(f32, bool)>,
    /// 上次轮询采集源音量的时间。
    volume_at: Option<Instant>,
    /// 最近一次音量联动失败的原因（显示在界面上）。
    volume_error: Option<String>,
    /// 采集源端点的音量（每 ~1 s 读一次；与「是否联动」无关，用来诊断「信号被衰减」）。
    volume_source: Option<EndpointVolume>,
    /// 上次读采集源音量的时间。
    volume_source_at: Option<Instant>,
    /// 当前页面。
    page: Page,
    /// 左侧侧边栏是否展开（收起后只剩图标条）。
    sidebar_open: bool,
    /// 关闭流程（缩小至托盘 / 直接关闭 / 先问一句）的跨帧状态。
    close_state: CloseState,
    /// 系统托盘（装不上时为 None：此时「缩小至托盘」退化为普通最小化）。
    tray: Option<Tray>,
    /// 是否已经尝试装过托盘（只尝试一次，失败写日志）。
    tray_attempted: bool,
    /// 窗口当前是否隐藏到托盘（或退化为普通最小化）。
    ///
    /// 隐藏期间**不能**再周期性请求重绘：窗口不可见时重绘请求无法真正呈现，
    /// eframe 的 `ControlFlow::WaitUntil` 一到点就立刻返回，会把一个核跑满
    /// （实测隐藏后 97–99% 单核，可见时 1.35%）。唤醒改由托盘事件驱动
    /// （`Tray` 持有 `egui::Context`，事件入队后 `request_repaint()`）。
    window_hidden: bool,
    /// 隐藏期间的第二个唤醒源：轮询采集源音量，变化时唤醒主循环。
    volume_waker: Option<VolumeWaker>,
    /// 每路输出设备音量（0.0..=1.0），拖音量条时实时改这里，节流后写回设备。
    /// 用 HashMap：RouteChannel 只有 Hash 没有 Ord（引擎侧不给它排全序）。
    route_volumes: std::collections::HashMap<RouteChannel, f32>,
    /// 上次把某路音量写回设备的时间（节流，避免拖动时每帧都走 COM）。
    route_volume_at: std::collections::HashMap<RouteChannel, Instant>,
    /// 上次周期性写运行统计日志的时间。
    last_stats_log: Option<Instant>,
    /// 每路最后一次记录的（欠载数, 是否处于错误/重试中），用于变化时立刻写日志。
    last_stats_marks: Vec<(u64, bool)>,
    /// 最近一次音量平衡测量结果。
    balance_result: Option<BalanceReport>,
    /// 音量平衡失败原因（延迟对齐仍可能成功）。
    balance_error: Option<String>,
    /// 音量平衡的状态说明。
    balance_status: String,
    /// 日志页是否自动滚动到最新一行。
    log_autoscroll: bool,
    /// 上次刷新每路输出设备音量的时间（从设备读回，覆盖拖动之外的显示值）。
    route_volume_refresh_at: Option<Instant>,
}

impl StereoLinkApp {
    pub fn new(cc: &eframe::CreationContext<'_>, log: LogBuffer) -> Self {
        Self::setup_style(&cc.egui_ctx);
        let font = fonts::install(&cc.egui_ctx);
        match &font {
            Some(path) => tracing::info!("中文字体：{path}"),
            None => tracing::warn!("没找到可用的中文字体，中文可能显示为方块"),
        }

        let cfg = AppConfig::load();
        let cfg_sync_mic = cfg.sync_mic.clone();
        let mut app = Self {
            cfg,
            engine: None,
            stats: None,
            endpoints: Vec::new(),
            endpoints_error: None,
            log,
            notice: None,
            font,
            live_default_output: None,
            live_default_is_capture: false,
            live_default_at: None,
            auto_started: false,
            exit_handled: false,
            format_cache: BTreeMap::new(),
            format_supported: BTreeMap::new(),
            format_probing: BTreeSet::new(),
            format_rx: None,
            format_choices: BTreeMap::new(),
            format_busy: false,
            sync_mics: Vec::new(),
            sync_mic: cfg_sync_mic,
            sync_health: None,
            sync_status: "还没选麦克风。".to_string(),
            sync_busy: false,
            sync_rx: None,
            sync_result: None,
            clicks_busy: false,
            clicks_at: None,
            volume_saved: BTreeMap::new(),
            volume_last: None,
            volume_at: None,
            volume_error: None,
            volume_source: None,
            volume_source_at: None,
            page: Page::Routing,
            sidebar_open: true,
            close_state: CloseState::default(),
            tray: None,
            tray_attempted: false,
            window_hidden: false,
            volume_waker: None,
            route_volumes: std::collections::HashMap::new(),
            route_volume_at: std::collections::HashMap::new(),
            last_stats_log: None,
            last_stats_marks: Vec::new(),
            balance_result: None,
            balance_error: None,
            balance_status: String::new(),
            log_autoscroll: true,
            route_volume_refresh_at: None,
        };
        app.refresh_endpoints();
        app.refresh_formats();
        app.refresh_default_output(true);
        app.refresh_mics();
        tracing::info!("StereoLink 面板启动");
        app
    }

    fn setup_style(ctx: &egui::Context) {
        let mut style = egui::Style::default();
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        visuals.window_fill = PANEL;
        visuals.extreme_bg_color = Color32::from_rgb(28, 32, 40);
        visuals.faint_bg_color = CARD_SOFT;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
        for w in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }
        visuals.widgets.noninteractive.bg_fill = CARD;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(35, 41, 51);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(31, 36, 45);
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(46, 54, 67);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(42, 49, 61);
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.28);
        visuals.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.22);
        visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        style.visuals = visuals;
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style
            .text_styles
            .insert(egui::TextStyle::Heading, FontId::proportional(20.0));
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(12.0));
        ctx.set_style(style);
    }

    // ---- 状态 ----

    fn tick(&mut self) {
        self.poll_format_probe();
        self.poll_sync();
        self.refresh_default_output(false);
        self.tick_volume_follow();
        self.tick_volume_watch();

        if self.clicks_busy
            && self
                .clicks_at
                .map(|t| t.elapsed() > Duration::from_millis(3_500))
                .unwrap_or(true)
        {
            self.clicks_busy = false;
        }

        if let Some(engine) = &self.engine {
            if engine.alive() {
                self.stats = Some(engine.stats());
            } else {
                // 引擎已经停止：把最后一份快照里的每路错误打出来，方便定位「为什么停了」。
                let final_stats = engine.stats();
                tracing::warn!(
                    "[路由诊断] 引擎已停止：running={} 采集源={} 采集错误={:?} 采集帧={} 采样率={}",
                    final_stats.running,
                    final_stats.source_device,
                    final_stats.source_error,
                    final_stats.captured_frames,
                    final_stats.rate
                );
                for r in &final_stats.routes {
                    tracing::warn!(
                        "[路由诊断] {} → {} | 已发 {} B | 欠载 {} | 错误 {:?}",
                        r.channel.as_str(),
                        r.endpoint,
                        r.sent_bytes,
                        r.underruns,
                        r.error
                    );
                }
                tracing::warn!("路由线程已退出（可能输出设备被拔掉/切换）");
                self.engine = None;
                self.stats = None;
            }
        }

        // 已发流 / 欠载 / 缓冲不再显示在界面上：定期（或状态变化时）写进日志。
        self.log_runtime_stats();

        // 启动后自动开始一次。放在 tick 里而不是 new()，让首帧先把界面画出来。
        if !self.auto_started {
            self.auto_started = true;
            if self.cfg.auto_start && self.engine.is_none() {
                self.start_routing(false);
            }
        }

        if let Some((_, at)) = &self.notice {
            if at.elapsed() > Duration::from_secs(8) {
                self.notice = None;
            }
        }
    }

    /// 把运行统计（已发流 / 欠载 / 缓冲）写进日志：每 30 秒一次，
    /// 欠载数或错误/重试状态发生变化时立即写一条。（界面表格里不再显示这些数字。）
    fn log_runtime_stats(&mut self) {
        let Some(stats) = self.stats.clone() else {
            return;
        };
        let due = self
            .last_stats_log
            .map(|t| t.elapsed() >= STATS_LOG_INTERVAL)
            .unwrap_or(true);
        let marks: Vec<(u64, bool)> = stats
            .routes
            .iter()
            .map(|r| (r.underruns, r.error.is_some() || r.retry_in_ms > 0))
            .collect();
        let changed = marks != self.last_stats_marks;
        if !due && !changed {
            return;
        }
        self.last_stats_log = Some(Instant::now());
        self.last_stats_marks = marks;
        for route in &stats.routes {
            let extra = if route.retry_in_ms > 0 {
                format!(" | 重连中（第 {} 次）", route.retry_count)
            } else if let Some(err) = &route.error {
                format!(" | 错误：{err}")
            } else {
                String::new()
            };
            tracing::info!(
                "[统计] {} → {} | 已发 {:.2} MB | 欠载 {} | 缓冲 {:.0} ms{}",
                route.channel.as_str(),
                short_endpoint(&route.endpoint),
                route.sent_bytes as f64 / 1_048_576.0,
                route.underruns,
                route.queued_ms,
                extra
            );
        }
        if let Some(err) = &stats.source_error {
            tracing::warn!("[统计] 采集源错误：{err}");
        }
    }

    fn refresh_default_output(&mut self, force: bool) {
        let stale = force
            || self
                .live_default_at
                .map(|t| t.elapsed() > Duration::from_millis(900))
                .unwrap_or(true);
        if !stale {
            return;
        }
        let ep = audio_default::default_render();
        self.live_default_at = Some(Instant::now());
        // 「默认输出 == 采集端点」才采得到声（D 模式下采集源可以是任意端点，不再只看名字）。
        let is_capture = match (&ep, self.capture_endpoint()) {
            (Some(current), Some(capture)) => current.id.eq_ignore_ascii_case(&capture.id),
            _ => false,
        };
        self.live_default_is_capture = is_capture;
        // 记住最后一次见到的「真实」默认输出：即使启动时默认已经是采集端点
        // （上次被强杀没来得及恢复），退出时也能切回去。
        if !is_capture {
            if let Some(id) = ep.as_ref().map(|e| e.id.clone()) {
                if self.cfg.capture.last_real_output.as_deref() != Some(id.as_str()) {
                    self.cfg.capture.last_real_output = Some(id);
                    self.save();
                }
            }
        }
        self.live_default_output = ep.map(|e| e.name);
    }

    fn refresh_endpoints(&mut self) {
        match stereo_audio::list_endpoints() {
            Ok(list) => {
                self.endpoints = list;
                self.endpoints_error = None;
            }
            Err(e) => {
                self.endpoints_error = Some(e.to_string());
                tracing::warn!("枚举播放端点失败：{e}");
            }
        }
    }

    /// 当前选中的采集端点：优先配置里记住的 id，解析不到再回退到按虚拟声卡名字提示自动挑。
    ///
    /// 名字可以被用户改、端点也可能被移除，所以优先认 id；id 找不到时按
    /// [`audio_default`] 的提示顺序挑一块虚拟声卡（自研驱动 → UU远程 → Steam）。
    fn capture_endpoint(&self) -> Option<EndpointInfo> {
        if let Some(id) = self
            .cfg
            .capture
            .endpoint_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if let Some(ep) = self
                .endpoints
                .iter()
                .find(|e| e.flow == DataFlow::Render && e.id.eq_ignore_ascii_case(id))
            {
                return Some(ep.clone());
            }
            if let Some(ep) = audio_default::endpoint_by_id(id) {
                return Some(ep);
            }
        }
        // 2) 已枚举列表里按名字提示挑（id 失效/从未选过时的主要路径，零开销）。
        let renders = self.render_devices();
        if let Some(ep) = audio_default::pick_virtual(&renders) {
            return Some(ep);
        }
        // 3) 列表为空（枚举失败/启动早期）才再全量解析一次。
        if self.endpoints.is_empty() {
            return audio_default::resolve_virtual_endpoint(None);
        }
        None
    }

    fn render_devices(&self) -> Vec<EndpointInfo> {
        self.endpoints
            .iter()
            .filter(|e| e.flow == DataFlow::Render)
            .cloned()
            .collect()
    }

    /// 读取每个播放端点的“设备默认格式”（Windows 声音设置里那一项），
    /// 并在后台探测每个端点**真正支持**哪些格式（独占模式，见 `format_support`）。
    fn refresh_formats(&mut self) {
        let mut cache = BTreeMap::new();
        let mut ids = Vec::new();
        for ep in self.render_devices() {
            let format = stereo_audio::get_endpoint_format(&ep.id).ok();
            if let Some(f) = &format {
                self.format_choices.entry(ep.id.clone()).or_insert(*f);
            }
            cache.insert(ep.id.clone(), format);
            ids.push(ep.id);
        }
        self.format_cache = cache;
        self.spawn_format_probe(ids);
    }

    /// 后台探测端点支持的格式。
    ///
    /// 每个端点要试 90 个候选（6 采样率 × 5 声道 × 3 位深），同步做会卡住界面，
    /// 所以丢到线程里、结果通过通道回到 [`Self::poll_format_probe`]。
    fn spawn_format_probe(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.format_rx = Some(rx);
        self.format_supported.clear();
        self.format_probing = ids.iter().cloned().collect();
        std::thread::spawn(move || {
            for id in ids {
                let list = stereo_audio::supported_formats(&id).ok();
                if tx.send((id, list)).is_err() {
                    break;
                }
            }
        });
    }

    /// 只探测一个端点（界面上「探测格式」按钮用）。
    fn probe_one_format(&mut self, id: String) {
        if self.format_probing.contains(&id) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.format_rx = Some(rx);
        self.format_probing.insert(id.clone());
        std::thread::spawn(move || {
            let list = stereo_audio::supported_formats(&id).ok();
            let _ = tx.send((id, list));
        });
    }

    /// 轮询格式探测线程的结果。
    fn poll_format_probe(&mut self) {
        let mut received: Vec<(String, Option<Vec<EndpointFormat>>)> = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.format_rx {
            loop {
                match rx.try_recv() {
                    Ok(item) => received.push(item),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for (id, list) in received {
            self.format_probing.remove(&id);
            self.format_supported.insert(id, list);
        }
        if disconnected {
            self.format_rx = None;
            self.format_probing.clear();
        }
    }

    /// 把某个播放设备的默认格式改成下拉框里选的那一项。
    fn apply_format(&mut self, endpoint_id: &str, label: &str) {
        let Some(chosen) = self.format_choices.get(endpoint_id).copied() else {
            self.toast("没有选中格式");
            return;
        };
        self.format_busy = true;
        match stereo_audio::set_endpoint_format(
            endpoint_id,
            chosen.samplerate,
            chosen.channels,
            chosen.valid_bits,
            chosen.is_float,
        ) {
            Ok(applied) => {
                tracing::info!("[设备格式] {label} → {}", applied.describe());
                self.toast(format!("{label} 已改为 {}", applied.describe()));
                self.refresh_formats();
                self.refresh_endpoints();
            }
            Err(e) => {
                tracing::warn!("[设备格式] {label} 设置失败：{e}");
                self.toast(format!("设置失败：{e}"));
            }
        }
        self.format_busy = false;
    }

    /// 从支持列表里挑一个「最接近当前格式」的候选（优先同采样率同声道，其次同声道）。
    fn closest_supported(
        list: &[EndpointFormat],
        current: &EndpointFormat,
    ) -> Option<EndpointFormat> {
        list.iter()
            .find(|f| {
                f.samplerate == current.samplerate
                    && f.channels == current.channels
                    && f.is_float == current.is_float
            })
            .or_else(|| {
                list.iter()
                    .find(|f| f.samplerate == current.samplerate && f.channels == current.channels)
            })
            .or_else(|| list.iter().find(|f| f.channels == current.channels))
            .or_else(|| list.first())
            .copied()
    }

    fn render_names(&self) -> Vec<String> {
        self.render_devices().into_iter().map(|e| e.name).collect()
    }

    /// 引擎要的采集源字符串：完整名字在 render 列表里重复时填 id（精确匹配），否则填完整名。
    fn capture_selector(&self) -> Option<String> {
        let ep = self.capture_endpoint()?;
        Some(capture_selector_for(&ep, &self.render_devices()))
    }

    /// 采集源完整端点名（界面展示/自检音用）。
    fn capture_full_name(&self) -> Option<String> {
        self.capture_endpoint().map(|ep| ep.name)
    }

    fn route_of(&self, channel: RouteChannel) -> Option<&RouteStats> {
        self.stats
            .as_ref()?
            .routes
            .iter()
            .find(|r| r.channel == channel)
    }

    fn route_engine_config(&self) -> Option<RouteEngineConfig> {
        // D 模式：采集源是用户选的任意端点；同名端点（本机两个 NVIDIA）用 id 精确匹配。
        let source = self.capture_selector()?;
        let mut routes = Vec::new();
        let left = self.cfg.split.left_endpoint.trim();
        if !left.is_empty() {
            routes.push(RouteConfig {
                endpoint: left.to_string(),
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
                endpoint: right.to_string(),
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
            // 自动：引擎采样率跟随采集源的原生混音格式 → 全程零 SRC（这是「杂音/音质差」的修复）。
            rate: None,
            capture_buffer_ms: None,
        })
    }

    fn start_routing(&mut self, from_ui: bool) -> bool {
        if self.engine.is_some() {
            return true;
        }
        let Some(config) = self.route_engine_config() else {
            tracing::warn!("开始路由失败：还没有为左/右声道选择输出设备，或找不到采集源");
            if from_ui {
                self.toast("先在「声道路由」里为左/右声道选好输出设备");
            }
            return false;
        };
        let summary = format!(
            "采集源 {} → {}",
            short_endpoint(&config.source_endpoint),
            config
                .routes
                .iter()
                .map(|r| format!(
                    "{}={}",
                    channel_label(r.channel),
                    short_endpoint(&r.endpoint)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        match RouteEngineHandle::start(config) {
            Ok(handle) => {
                self.stats = Some(handle.stats());
                self.engine = Some(handle);
                tracing::info!("[路由] 已启动：{summary}");
                if from_ui {
                    self.toast("已开始路由");
                }
                self.apply_default_output();
                true
            }
            Err(e) => {
                tracing::error!("[路由] 启动失败：{e}");
                if from_ui {
                    self.toast(format!("启动失败：{e}"));
                }
                false
            }
        }
    }

    fn stop_routing(&mut self, restore_output: bool) {
        if let Some(mut engine) = self.engine.take() {
            engine.stop();
            tracing::info!("[路由] 已停止");
        }
        self.stats = None;
        self.restore_target_volumes();
        if restore_output {
            self.restore_previous_output();
        }
    }

    fn restart_routing(&mut self) {
        let was_running = self.engine.is_some();
        self.stop_routing(false);
        if was_running {
            self.start_routing(false);
        }
    }

    /// 手动把默认输出切到当前采集端点（「切为默认输出」按钮）。
    fn switch_default_to_capture(&mut self) {
        let Some(capture) = self.capture_endpoint() else {
            self.toast("还没有可用的采集端点");
            return;
        };
        match audio_default::switch_to_endpoint_id(&capture.id, &capture) {
            Ok(outcome) => {
                if let Some(id) = outcome.previous_id {
                    if self.cfg.capture.previous_output.is_none() {
                        self.cfg.capture.previous_output = Some(id);
                        self.cfg.save();
                    }
                }
                self.refresh_default_output(true);
                self.toast(format!(
                    "已把默认输出切到「{}」",
                    short_endpoint(&capture.name)
                ));
                tracing::info!("[默认输出] {}", outcome.message);
            }
            Err(e) => {
                tracing::warn!("切换默认输出失败：{e}");
                self.toast(format!("切换失败：{e}"));
            }
        }
    }

    /// 切换采集源：写配置 + 保存；正在路由就重启，默认输出正指旧采集端点就顺手切过去。
    fn select_capture_endpoint(&mut self, id: &str) {
        let Some(ep) = self
            .render_devices()
            .into_iter()
            .find(|e| e.id.eq_ignore_ascii_case(id))
        else {
            return;
        };
        let old_id = self.cfg.capture.endpoint_id.clone();
        self.cfg.capture.endpoint_id = Some(ep.id.clone());
        self.cfg.capture.endpoint_name = short_endpoint(&ep.name);
        self.cfg.save();
        tracing::info!("[采集源] 切换为「{}」", ep.name);
        // 默认输出正指着旧采集端点的话顺手切到新端点（否则用户会突然听不到声）。
        let default_was_old = match (audio_default::default_render(), old_id.as_deref()) {
            (Some(current), Some(old)) => current.id.eq_ignore_ascii_case(old),
            _ => false,
        };
        if self.engine.is_some() {
            self.restart_routing();
        } else if default_was_old && self.cfg.capture.auto_switch_default {
            match audio_default::switch_to_endpoint_id(&ep.id, &ep) {
                Ok(outcome) => {
                    if let Some(previous) = outcome.previous_id {
                        if self.cfg.capture.previous_output.is_none() {
                            self.cfg.capture.previous_output = Some(previous);
                            self.save();
                        }
                    }
                    self.refresh_default_output(true);
                }
                Err(e) => {
                    tracing::warn!("[采集源] 切换默认输出失败：{e}");
                    self.toast(format!("切换默认输出失败：{e}"));
                }
            }
        }
        self.toast(format!("采集源已切换为「{}」", short_endpoint(&ep.name)));
    }

    /// 开始路由/切换采集源后，把 Windows 默认输出切到采集端点。
    fn apply_default_output(&mut self) {
        if !self.cfg.capture.auto_switch_default {
            self.refresh_default_output(true);
            return;
        }
        let Some(capture) = self.capture_endpoint() else {
            tracing::warn!("[默认输出] 没有可用的采集端点，跳过自动切换");
            return;
        };
        match audio_default::switch_to_endpoint_id(&capture.id, &capture) {
            Ok(outcome) => {
                if let Some(id) = outcome.previous_id {
                    if self.cfg.capture.previous_output.is_none() {
                        self.cfg.capture.previous_output = Some(id);
                        self.save();
                    }
                }
                tracing::info!("[默认输出] {}", outcome.message);
                self.refresh_default_output(true);
            }
            Err(e) => {
                tracing::warn!("[默认输出] 自动切换失败：{e}");
                self.toast("自动切换默认输出失败，请点「切为默认输出」或手动切换");
            }
        }
    }

    fn restore_previous_output(&mut self) {
        // 优先用本次切换前记录的；没有（启动时默认已经是采集端点）就用最后一次
        // 见到的真实默认输出兜底。
        let id = self
            .cfg
            .capture
            .previous_output
            .clone()
            .or_else(|| self.cfg.capture.last_real_output.clone());
        let Some(id) = id else {
            if self.live_default_is_capture {
                tracing::warn!(
                    "[默认输出] 没有可恢复的记录（启动时默认输出已经是采集端点），退出后请在 Windows 声音设置里手动切回音箱"
                );
            }
            return;
        };
        match audio_default::switch_to_id(&id) {
            Ok(_) => {
                self.cfg.capture.previous_output = None;
                self.save();
                self.refresh_default_output(true);
                tracing::info!("[默认输出] 已恢复切换前的默认输出");
            }
            Err(e) => tracing::warn!("[默认输出] 恢复失败：{e}"),
        }
    }

    fn play_test_tone(&mut self) {
        let Some(source) = self.capture_full_name() else {
            self.toast("找不到采集源，无法播放自检音");
            return;
        };
        let seconds = self.cfg.tone_seconds.clamp(0.5, 20.0);
        let pcm = stereo_audio::build_sine_s16(
            440.0,
            seconds,
            0.25,
            PcmFormat::new(stereo_audio::SPLIT_SAMPLE_RATE, 2, 16),
        );
        tracing::info!(
            "[自检] 往 {} 播 {} 秒 440Hz 正弦音",
            short_endpoint(&source),
            seconds
        );
        std::thread::spawn(move || {
            if let Err(e) = stereo_audio::play_pcm_s16(
                &stereo_audio::DeviceSelector::NameContains(source),
                PcmFormat::new(stereo_audio::SPLIT_SAMPLE_RATE, 2, 16),
                &pcm,
            ) {
                tracing::warn!("[自检] 播放失败：{e}");
            }
        });
    }

    fn set_route_endpoint(&mut self, channel: RouteChannel, name: String) {
        match channel {
            RouteChannel::Left => self.cfg.split.left_endpoint = name,
            RouteChannel::Right => self.cfg.split.right_endpoint = name,
            RouteChannel::Both => return,
        }
        self.cfg.save();
        if self.engine.is_some() {
            self.restart_routing();
            self.toast("已应用新的声道路由");
        }
    }

    fn save(&mut self) {
        self.cfg.save();
    }

    // -----------------------------------------------------------------------
    // 声学自校验：用麦克风量出两台音箱的真实到达时间差，自动垫延迟
    // -----------------------------------------------------------------------

    /// 枚举本机采集端点（含虚拟/回环，标出类别供用户判断）。
    fn refresh_mics(&mut self) {
        self.sync_mics = stereo_audio::list_microphones();
        if self.sync_mic.is_empty() {
            // 默认挑第一个「可能可用」的；没有就挑第一个。
            if let Some(m) = self
                .sync_mics
                .iter()
                .find(|m| m.likely_usable)
                .or_else(|| self.sync_mics.first())
            {
                self.sync_mic = m.id().to_string();
                self.cfg.sync_mic = self.sync_mic.clone();
                self.save();
            }
        }
        // 自动选中后给一句明确的状态（只在还没体检过时写，避免覆盖体检/测量结果）。
        if !self.sync_mic.is_empty()
            && (self.sync_status.is_empty() || self.sync_status == "还没选麦克风。")
        {
            let name = self
                .sync_mics
                .iter()
                .find(|m| m.id() == self.sync_mic)
                .map(|m| m.name().to_string())
                .unwrap_or_else(|| self.sync_mic.clone());
            self.sync_status = format!("已选中「{name}」，点「体检」看它能不能采到声音。");
        }
        tracing::info!(
            "[自校验] 找到 {} 个采集端点（{} 个可能可用）",
            self.sync_mics.len(),
            self.sync_mics.iter().filter(|m| m.likely_usable).count()
        );
    }

    /// 对选中麦克风做一次体检（决定「自动对齐」能不能用）。
    fn check_mic(&mut self) {
        if self.sync_mic.is_empty() {
            self.sync_status = "先选一个麦克风。".to_string();
            return;
        }
        let mic = self.sync_mic.clone();
        self.sync_status = "正在体检麦克风（请保持环境安静）…".to_string();
        match stereo_audio::mic_health(&mic, 1_000) {
            Ok(h) => {
                self.sync_status = h.describe();
                tracing::info!("[自校验] 麦克风体检：{}", h.describe());
                self.sync_health = Some(h);
            }
            Err(e) => {
                self.sync_status = format!("体检失败：{e}");
                self.sync_health = None;
            }
        }
    }

    /// 后台跑一次「延迟对齐 + 音量平衡」：先量两台箱的到达时差，再分别量左右电平。
    fn start_sync(&mut self) {
        if self.sync_busy {
            return;
        }
        let mic = self.sync_mic.clone();
        if mic.is_empty() {
            self.sync_status = "先选一个麦克风。".to_string();
            return;
        }
        let left = self.cfg.split.left_endpoint.clone();
        let right = self.cfg.split.right_endpoint.clone();
        if left.is_empty() || right.is_empty() {
            self.sync_status = "先选好左右两台音箱。".to_string();
            return;
        }
        let Some(mic_candidate) = self.sync_mics.iter().find(|m| m.id() == mic).cloned() else {
            self.sync_status = "麦克风列表里找不到这个设备，先点「刷新」。".to_string();
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let mic_t = mic.clone();
        let (left_t, right_t) = (left.clone(), right.clone());
        std::thread::spawn(move || {
            let cfg = SyncConfig::new(mic_t.clone(), left_t.clone());
            let sync = stereo_audio::calibrate_pair(&mic_t, &left_t, &right_t, &cfg)
                .map_err(|e| e.to_string());
            // 延迟对齐之后，接着用同一个麦克风量左右两路的电平，算音量平衡建议。
            let balance =
                stereo_audio::sync::measure_balance(&mic_candidate, &left_t, &right_t, &cfg)
                    .map_err(|e| e.to_string());
            let _ = tx.send(CalibrateOutcome { sync, balance });
        });
        self.sync_rx = Some(rx);
        self.sync_busy = true;
        self.balance_result = None;
        self.balance_error = None;
        self.balance_status = String::new();
        self.sync_status = if self.engine.is_some() {
            "正在测量延迟并平衡音量（约 8 秒）… 提示：路由正在运行，若此刻正在放音乐请先「停止」，否则麦克风会同时收到音乐、测量会不准。".to_string()
        } else {
            "正在测量延迟并平衡音量（约 8 秒）… 请保持环境安静。".to_string()
        };
        tracing::info!("[自校验] 开始标定 + 平衡：mic={mic} left={left} right={right}");
    }

    /// 收后台结果：延迟写进两条路由（运行中实时生效），音量平衡结果留待用户确认。
    fn poll_sync(&mut self) {
        let Some(rx) = self.sync_rx.as_ref() else {
            return;
        };
        let received = rx.try_recv();
        match received {
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.sync_rx = None;
                self.sync_busy = false;
                self.sync_status = "标定线程异常退出。".to_string();
            }
            Ok(CalibrateOutcome { sync, balance }) => {
                self.sync_rx = None;
                self.sync_busy = false;
                match sync {
                    Ok(result) => {
                        let (l, r) = result.delays();
                        self.cfg.split.left_delay_ms = l;
                        self.cfg.split.right_delay_ms = r;
                        self.save();
                        if let Some(engine) = self.engine.as_ref() {
                            engine.set_delays_ms(&[l, r]);
                        }
                        self.sync_status = format!(
                            "{}  → 已应用左垫 {l} ms、右垫 {r} ms{}",
                            result.describe(),
                            if result.method == SyncMethod::Sequential {
                                "（依次法精度只有 ±4 ms，建议再用手动点击串微调）"
                            } else {
                                ""
                            }
                        );
                        tracing::info!("[自校验] {}", self.sync_status);
                        self.sync_result = Some(result);
                        self.toast("已按测量结果自动对齐左右延迟");
                    }
                    Err(e) => {
                        self.sync_status = format!("延迟测量失败：{e}");
                        tracing::warn!("[自校验] 测量失败：{e}");
                    }
                }
                match balance {
                    Ok(report) => {
                        self.balance_status = format!(
                            "音量平衡：{}  → 点「应用建议」按建议百分比写回左右端点音量",
                            report.describe()
                        );
                        tracing::info!("[音量平衡] {}", report.describe());
                        self.balance_result = Some(report);
                        self.balance_error = None;
                    }
                    Err(e) => {
                        self.balance_status = format!("音量平衡测量失败（延迟对齐不受影响）：{e}");
                        tracing::warn!("[音量平衡] 测量失败：{e}");
                        self.balance_result = None;
                        self.balance_error = Some(e);
                    }
                }
            }
        }
    }

    /// 把音量平衡建议写回左右端点音量（不改延迟；可反复应用）。
    fn apply_balance(&mut self) {
        let Some(report) = self.balance_result else {
            return;
        };
        let left = self.cfg.split.left_endpoint.clone();
        let right = self.cfg.split.right_endpoint.clone();
        let targets = [
            (
                RouteChannel::Left,
                left.as_str(),
                report.suggested_left_percent,
            ),
            (
                RouteChannel::Right,
                right.as_str(),
                report.suggested_right_percent,
            ),
        ];
        let mut errors: Vec<String> = Vec::new();
        for (channel, endpoint, percent) in targets {
            if endpoint.is_empty() {
                continue;
            }
            match set_endpoint_volume(endpoint, percent as f32 / 100.0) {
                Ok(volume) => {
                    self.route_volumes.insert(channel, volume.scalar);
                    self.route_volume_at.insert(channel, Instant::now());
                }
                Err(e) => errors.push(format!("{}：{e}", short_endpoint(endpoint))),
            }
        }
        if errors.is_empty() {
            self.toast(format!(
                "已应用音量平衡：左 {}% / 右 {}%",
                report.suggested_left_percent, report.suggested_right_percent
            ));
            self.balance_status = format!(
                "已应用：左 {}% / 右 {}%（可再点「测量并对齐」复测）",
                report.suggested_left_percent, report.suggested_right_percent
            );
        } else {
            self.toast(format!("应用音量平衡部分失败：{}", errors.join("；")));
            self.balance_status = format!("应用失败：{}", errors.join("；"));
        }
    }

    /// 手动对齐：按当前左右延迟值同时向两台音箱放点击串，人耳听是否重合成一下。
    fn play_alignment(&mut self) {
        if self.clicks_busy {
            return;
        }
        let left = self.cfg.split.left_endpoint.clone();
        let right = self.cfg.split.right_endpoint.clone();
        let (l, r) = (self.cfg.split.left_delay_ms, self.cfg.split.right_delay_ms);
        if left.is_empty() || right.is_empty() {
            self.toast("先选好左右两台音箱");
            return;
        }
        self.clicks_busy = true;
        self.clicks_at = Some(Instant::now());
        std::thread::spawn(move || {
            if let Err(e) = stereo_audio::play_alignment_clicks(&left, &right, l, r, 3.0) {
                tracing::warn!("[自校验] 点击串播放失败：{e}");
            }
        });
        self.toast("正在放点击串：听到两下就是没对齐，调到重合成一下");
    }

    // ---- 音量联动 ---------------------------------------------------------

    /// 采集源是不是「调它自己的音量对音箱没用」的虚拟设备（自动策略的依据）。
    ///
    /// 自研虚拟声卡与物理输出的 WASAPI 回环是**后置音量**：调音量采集幅度就跟着变，
    /// 再镜像到目标设备就会双重生效（音量变化过陡），所以这两类不联动。
    fn capture_ignores_volume(&self) -> bool {
        let Some(ep) = self.capture_endpoint() else {
            return false;
        };
        ignores_system_volume(&ep.name)
    }

    /// 当前是否要把采集源的音量/静音镜像到目标设备。
    fn volume_follow_active(&self) -> bool {
        match self.cfg.volume_follow {
            VolumeFollow::On => true,
            VolumeFollow::Off => false,
            VolumeFollow::Auto => self.capture_ignores_volume(),
        }
    }

    /// 目标端点（左/右，按配置里的名字或 id 子串），去重。
    fn target_endpoints(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for name in [
            self.cfg.split.left_endpoint.trim(),
            self.cfg.split.right_endpoint.trim(),
        ] {
            if !name.is_empty() && !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
        }
        out
    }

    /// 按需维持「音量跟随」唤醒线程：仅在引擎运行且跟随开启时存在。
    ///
    /// 线程只负责在采集源音量变化时唤醒主循环；写入逻辑仍在 `tick_volume_follow()`。
    fn tick_volume_waker(&mut self, ctx: &egui::Context) {
        let wanted = self.engine.is_some() && self.volume_follow_active();
        if !wanted {
            if self.volume_waker.take().is_some() {
                tracing::debug!("音量跟随唤醒线程已停止");
            }
            return;
        }
        if self.volume_waker.is_some() {
            return;
        }
        let Some(source) = self.capture_selector() else {
            return;
        };
        self.volume_waker = Some(VolumeWaker::spawn(ctx.clone(), source));
        tracing::debug!("音量跟随唤醒线程已启动（隐藏到托盘后仍会跟随系统音量）");
    }

    /// 每 ~400 ms 把采集源的音量与静音镜像到目标设备（虚拟声卡场景）。
    fn tick_volume_follow(&mut self) {
        if !self.volume_follow_active() || self.engine.is_none() {
            return;
        }
        let due = self
            .volume_at
            .map(|t| t.elapsed() > Duration::from_millis(400))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.volume_at = Some(Instant::now());

        let Some(source) = self.capture_selector() else {
            return;
        };
        let volume = match get_endpoint_volume(&source) {
            Ok(v) => v,
            Err(e) => {
                if self.volume_error.is_none() {
                    self.volume_error = Some(format!(
                        "读不到采集源音量（{}）：{e}",
                        short_endpoint(&source)
                    ));
                    tracing::warn!("[音量] 读采集源音量失败：{e}");
                }
                return;
            }
        };

        let changed = match self.volume_last {
            Some((scalar, muted)) => {
                (scalar - volume.scalar).abs() >= 0.005 || muted != volume.muted
            }
            None => true,
        };
        if !changed {
            return;
        }
        let big = match self.volume_last {
            Some((scalar, muted)) => {
                (scalar - volume.scalar).abs() >= 0.03 || muted != volume.muted
            }
            None => true,
        };
        self.volume_last = Some((volume.scalar, volume.muted));
        self.volume_error = None;

        let targets = self.target_endpoints();
        for target in &targets {
            if !self.volume_saved.contains_key(target) {
                if let Ok(original) = get_endpoint_volume(target) {
                    self.volume_saved
                        .insert(target.clone(), (original.scalar, original.muted));
                }
            }
            if let Err(e) = set_endpoint_volume(target, volume.scalar) {
                self.volume_error = Some(format!("写音量失败（{}）：{e}", short_endpoint(target)));
                tracing::warn!("[音量] 写 {target} 音量失败：{e}");
                continue;
            }
            if let Err(e) = set_endpoint_mute(target, volume.muted) {
                tracing::warn!("[音量] 写 {target} 静音失败：{e}");
            }
        }
        if big {
            tracing::info!(
                "[音量] 跟随采集源 {:.0}%{} → {}",
                volume.scalar * 100.0,
                if volume.muted { "（静音）" } else { "" },
                targets.join(" / ")
            );
        }
    }

    /// 每 ~1 s 读一次采集源音量（不依赖「是否联动」），供「信号被衰减」诊断用。
    ///
    /// 实测：自研虚拟声卡的端点音量是**后置**的（0.14 → 回环幅度 -29.7 dB），
    /// 而 Realtek 等物理输出是**前置**的（调它不影响回环）。所以只有虚拟声卡
    /// 需要这条诊断。
    fn tick_volume_watch(&mut self) {
        let due = self
            .volume_source_at
            .map(|t| t.elapsed() > Duration::from_millis(1000))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.volume_source_at = Some(Instant::now());
        let Some(source) = self.capture_selector() else {
            self.volume_source = None;
            return;
        };
        self.volume_source = get_endpoint_volume(&source).ok();
    }

    /// 采集源音量是否低到「整条链路被衰减」的程度（只对后置音量的虚拟声卡报警）。
    fn source_volume_warning(&self) -> Option<(f32, f32)> {
        let ep = self.capture_endpoint()?;
        if !ep.name.contains("立体声链") {
            return None;
        }
        let v = self.volume_source.as_ref()?;
        if v.muted || v.scalar >= 0.9 {
            return None;
        }
        // Windows 的音量曲线不是简单的 20·log10（本机 0.14 对应 -29.7 dB 而不是 -17.1 dB），
        // 优先用 IAudioEndpointVolume 报的 dB，跟系统音量界面/探针保持一致。
        let db = v.db.unwrap_or_else(|| 20.0 * v.scalar.max(1e-4).log10());
        Some((v.scalar, db))
    }

    /// 把采集源音量设为 100%（保住有效位深：这是「人声低 / 音质差」的直接原因）。
    fn normalize_source_volume(&mut self) {
        let Some(source) = self.capture_selector() else {
            self.toast("先选好采集源");
            return;
        };
        match set_endpoint_volume(&source, 1.0) {
            Ok(v) => {
                self.volume_source = Some(v);
                self.volume_source_at = Some(Instant::now());
                self.toast("采集源音量已设为 100%（响度请用系统音量或音箱音量控制）");
            }
            Err(e) => self.toast(format!("设置失败：{e}")),
        }
    }

    /// 还原目标设备被联动改掉的音量（best-effort：强杀/崩溃不会还原）。
    fn restore_target_volumes(&mut self) {
        let saved = std::mem::take(&mut self.volume_saved);
        for (target, (scalar, muted)) in saved {
            let _ = set_endpoint_volume(&target, scalar);
            let _ = set_endpoint_mute(&target, muted);
        }
        self.volume_last = None;
        self.volume_at = None;
        self.volume_error = None;
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    fn route_label(&self, channel: RouteChannel) -> String {
        channel_label(channel).to_string()
    }

    fn configured_endpoint(&self, channel: RouteChannel) -> &str {
        match channel {
            RouteChannel::Left => self.cfg.split.left_endpoint.trim(),
            RouteChannel::Right => self.cfg.split.right_endpoint.trim(),
            RouteChannel::Both => "",
        }
    }

    fn hints(&self) -> Vec<Hint> {
        let mut hints = Vec::new();
        let capture = self.capture_endpoint();

        match &capture {
            Some(ep) => {
                let kind = capture_kind(&ep.name);
                hints.push(Hint {
                    tone: if kind.is_virtual() {
                        Tone::Ok
                    } else {
                        Tone::Warn
                    },
                    text: format!(
                        "采集源就绪：{}（{}）",
                        short_endpoint(&ep.name),
                        kind.label()
                    ),
                });
            }
            None => hints.push(Hint {
                tone: Tone::Err,
                text: "没有可用的采集源：在「采集源」卡里选一个播放端点".to_string(),
            }),
        }

        // 首次启动（没有配置文件）时左右声道是空串：明确告诉用户怎么选。
        if self.cfg.split.left_endpoint.trim().is_empty()
            || self.cfg.split.right_endpoint.trim().is_empty()
        {
            hints.push(Hint {
                tone: Tone::Warn,
                text: "还没选左右输出设备：在下方「本机播放设备」表里给两台设备分别点「设为左 / 设为右」"
                    .to_string(),
            });
        }

        if let Some(err) = &self.endpoints_error {
            hints.push(Hint {
                tone: Tone::Err,
                text: format!("枚举播放设备失败：{err}"),
            });
        }

        if self.engine.is_none() {
            hints.push(Hint {
                tone: Tone::Warn,
                text: "路由未运行：点「开始路由」".to_string(),
            });
        } else if let Some(stats) = &self.stats {
            if let Some(err) = &stats.source_error {
                hints.push(Hint {
                    tone: Tone::Err,
                    text: format!("采集失败：{err}"),
                });
            }
            let mut retrying = false;
            for route in &stats.routes {
                if route.retry_in_ms > 0 {
                    retrying = true;
                    hints.push(Hint {
                        tone: Tone::Warn,
                        text: format!(
                            "{}正在自动重连（第 {} 次失败，约 {:.0} 秒后重试）：设备可能没开机或没连上",
                            self.route_label(route.channel),
                            route.retry_count,
                            route.retry_in_ms as f64 / 1000.0
                        ),
                    });
                } else if let Some(err) = &route.error {
                    hints.push(Hint {
                        tone: Tone::Err,
                        text: format!("{}链路错误：{err}", self.route_label(route.channel)),
                    });
                } else if route.underruns > 200 {
                    hints.push(Hint {
                        tone: Tone::Warn,
                        text: format!(
                            "{}链路欠载 {} 次，可把抖动缓冲调大一点",
                            self.route_label(route.channel),
                            route.underruns
                        ),
                    });
                }
            }
            if stats.source_retrying > 0 {
                retrying = true;
                hints.push(Hint {
                    tone: Tone::Warn,
                    text: format!(
                        "采集源正在自动重连（第 {} 次）：采集端点可能被切换或占用",
                        stats.source_retrying
                    ),
                });
            }
            if stats.degraded {
                retrying = true;
                hints.push(Hint {
                    tone: Tone::Warn,
                    text: "还没有任何一路打开，正在后台自动重试（设备恢复后会自动开始）"
                        .to_string(),
                });
            }
            if hints.iter().all(|h| h.tone != Tone::Err) && !retrying {
                hints.push(Hint {
                    tone: Tone::Ok,
                    text: "链路正常，音频在走".to_string(),
                });
            }
        }

        if self.engine.is_some() && !self.live_default_is_capture {
            let current = self
                .live_default_output
                .as_deref()
                .map(short_endpoint)
                .unwrap_or_else(|| "未知".to_string());
            hints.push(Hint {
                tone: Tone::Warn,
                text: format!("系统默认输出是「{current}」：播放器的声音不会进分离链路"),
            });
        } else if self.live_default_is_capture {
            hints.push(Hint {
                tone: Tone::Ok,
                text: "系统默认输出 = 采集源".to_string(),
            });
        }

        if let Some((scalar, db)) = self.source_volume_warning() {
            hints.push(Hint {
                tone: Tone::Warn,
                text: format!(
                    "采集源音量只有 {:.0}%（整条链路衰减 {:.1} dB）——「人声低/音质差」的直接原因；\
                     点声道路由卡里的「设为 100%」",
                    scalar * 100.0,
                    db
                ),
            });
        }

        hints
    }

    // ---- 界面 ----

    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(18, 12))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::hover());
                    let painter = ui.painter();
                    painter.rect_filled(rect, CornerRadius::same(9), Color32::from_rgb(23, 30, 40));
                    painter.rect_stroke(
                        rect,
                        CornerRadius::same(9),
                        Stroke::new(1.0_f32, BORDER),
                        egui::StrokeKind::Inside,
                    );
                    painter.rect_filled(
                        Rect::from_min_size(rect.min + Vec2::new(8.0, 11.0), Vec2::new(6.0, 13.0)),
                        CornerRadius::same(2),
                        ACCENT,
                    );
                    painter.rect_filled(
                        Rect::from_min_size(rect.min + Vec2::new(19.0, 8.0), Vec2::new(6.0, 16.0)),
                        CornerRadius::same(2),
                        ACCENT2,
                    );

                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("StereoLink").size(19.0).strong().color(TEXT));
                        ui.label(muted("立体声链 · 本机设备左右分声道"));
                    });
                    ui.add_space(14.0);

                    if self.engine.is_some() {
                        pill(ui, "运行中", OK, 76.0);
                    } else {
                        pill(ui, "已停止", Color32::from_rgb(120, 128, 142), 76.0);
                    }

                    // 开始 / 停止 / 重启放在顶栏，主页面只放路由表。
                    ui.add_space(10.0);
                    if self.engine.is_some() {
                        if ui.button("停止路由").clicked() {
                            self.stop_routing(true);
                            self.toast("已停止路由并恢复默认输出");
                        }
                        if ui.button("重启路由").clicked() {
                            self.restart_routing();
                            self.toast("路由已重启");
                        }
                    } else if ui
                        .add(
                            egui::Button::new(
                                RichText::new("开始路由").color(Color32::BLACK).strong(),
                            )
                            .fill(ACCENT),
                        )
                        .clicked()
                    {
                        self.start_routing(true);
                    }
                    if ui
                        .button("左右自检音")
                        .on_hover_text("向两台音箱分别放一声测试音，快速确认左右没接反")
                        .clicked()
                    {
                        self.play_test_tone();
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .button("声音设置")
                            .on_hover_text("打开 Windows 声音设置")
                            .clicked()
                        {
                            let _ = std::process::Command::new("cmd")
                                .args(["/c", "start", "ms-settings:sound"])
                                .spawn();
                        }
                        if ui.button("刷新设备").clicked() {
                            self.refresh_endpoints();
                            self.refresh_default_output(true);
                            self.toast("设备列表已刷新");
                        }
                        // 提示条也占固定宽度：出现/消失都不会挤动顶栏。
                        let notice = self.notice.as_ref().map(|(text, _)| text.clone());
                        match &notice {
                            Some(text) => {
                                text_cell(
                                    ui,
                                    300.0,
                                    24.0,
                                    text,
                                    FontId::proportional(13.0),
                                    ACCENT,
                                    true,
                                )
                                .on_hover_text(text.clone());
                            }
                            None => {
                                ui.allocate_exact_size(Vec2::new(300.0, 24.0), Sense::hover());
                            }
                        }
                    });
                });
            });
    }

    /// 顶部诊断条：只显示三项状态，不带任何解释文字（细节都在日志里）。
    fn diagnostic_bar(&mut self, ctx: &egui::Context) {
        let capture_ready = self.capture_endpoint().is_some();
        let (link_ok, link_text, link_tip) = self.link_status();
        let default_ready = self.live_default_is_capture;
        egui::TopBottomPanel::top("diagnostics")
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(18, 6))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    status_dot(
                        ui,
                        capture_ready,
                        "采集源就绪",
                        "未选采集源",
                        "在「采集源」页选择作为声音来源的端点",
                    );
                    status_dot(ui, link_ok, link_text, link_text, link_tip);
                    status_dot(
                        ui,
                        default_ready,
                        "默认输出 = 采集源",
                        "默认输出未切到采集源",
                        "把 Windows 默认播放设备切到采集源端点，否则回环采不到声音",
                    );
                });
            });
    }

    /// 链路状态：`(是否正常, 文案, 悬停说明)`，只给顶部诊断条用。
    fn link_status(&self) -> (bool, &'static str, &'static str) {
        let Some(stats) = self.stats.as_ref() else {
            return (false, "链路未运行", "点顶栏「开始路由」启动");
        };
        if stats.degraded {
            return (false, "链路等待设备", "没有路由开流成功，正在后台重试");
        }
        if stats.source_retrying > 0 {
            return (false, "采集源重连中", "采集源暂时不可用，正在后台重试");
        }
        let route_bad = stats
            .routes
            .iter()
            .any(|r| r.error.is_some() || r.retry_in_ms > 0);
        if route_bad {
            return (
                false,
                "链路部分异常",
                "有路由正在重连或未能开流，详情见日志",
            );
        }
        if stats.source_error.is_some() {
            return (false, "采集源异常", "采集线程报错，详情见日志");
        }
        (true, "链路正常", "左右两路都在正常输出")
    }

    /// 左侧导航栏：四个页面入口，可收起成图标条（收起状态记忆在内存里）。
    fn sidebar(&mut self, ctx: &egui::Context) {
        let width = if self.sidebar_open {
            SIDEBAR_W
        } else {
            SIDEBAR_W_COLLAPSED
        };
        let margin_x = if self.sidebar_open { 10 } else { 6 };
        egui::SidePanel::left("sidebar")
            .exact_width(width)
            .resizable(false)
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(margin_x, 12))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if self.sidebar_open {
                        ui.label(RichText::new("导航").size(12.0).color(MUTED).strong());
                    }
                    let (collapsed_icon, tip) = if self.sidebar_open {
                        ("«", "收起侧边栏")
                    } else {
                        ("»", "展开侧边栏")
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(collapsed_icon).size(15.0).color(MUTED),
                                )
                                .frame(false),
                            )
                            .on_hover_text(tip)
                            .clicked()
                        {
                            self.sidebar_open = !self.sidebar_open;
                            self.cfg.save();
                        }
                    });
                });
                ui.add_space(8.0);
                let mut target: Option<Page> = None;
                for page in Page::ALL {
                    let selected = self.page == page;
                    let text = if self.sidebar_open {
                        page.label().to_string()
                    } else {
                        page.icon().to_string()
                    };
                    let fill = if selected { CARD_SOFT } else { PANEL };
                    let color = if selected { ACCENT } else { MUTED };
                    let (rect, response) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());
                    let hovered = response.hovered();
                    let painter = ui.painter();
                    painter.rect_filled(
                        rect,
                        CornerRadius::same(8),
                        if hovered {
                            Color32::from_rgb(26, 31, 39)
                        } else {
                            fill
                        },
                    );
                    if selected {
                        painter.rect_filled(
                            Rect::from_min_size(
                                rect.min + Vec2::new(0.0, 7.0),
                                Vec2::new(3.0, 20.0),
                            ),
                            CornerRadius::same(2),
                            ACCENT,
                        );
                    }
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        text,
                        FontId::proportional(13.0),
                        color,
                    );
                    let response = response.on_hover_text(page.label());
                    if response.clicked() {
                        target = Some(page);
                    }
                    ui.add_space(4.0);
                }
                if let Some(page) = target {
                    self.page = page;
                }
            });
    }

    /// 采集源卡：D 模式下采集源可以是任意播放端点（虚拟声卡 / UU远程 / 物理输出）。
    fn capture_card(&mut self, ui: &mut egui::Ui) {
        let devices = self.render_devices();
        let capture = self.capture_endpoint();
        let capture_id = capture.as_ref().map(|ep| ep.id.clone());
        let kind_now = capture.as_ref().map(|ep| capture_kind(&ep.name));
        let uu_present = devices.iter().any(|ep| ep.name.contains("UU远程"));
        let mut chosen_id: Option<String> = None;

        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(268.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("采集源").size(15.0).strong());
                help(
                    ui,
                    "采集源 = 引擎从哪个播放端点的 WASAPI 回环流里取声。\n\
                     不用装虚拟声卡也能用：任意一个播放端点都行，只要把 Windows 默认输出切到它。\n\
                     推荐选无声的虚拟设备（自研虚拟声卡 / UU远程虚拟音频设备 / Steam Streaming）；\n\
                     选蓝牙音箱会重复出声，选物理输出可能从音箱漏音。",
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    match kind_now {
                        Some(kind) => badge(ui, kind.short_label(), kind.color()),
                        None => badge(ui, "未找到", ERR),
                    };
                });
            });
            ui.add_space(6.0);

            // 当前采集端点（短名 + 全名 tooltip）。
            let (name, full_name) = match &capture {
                Some(ep) => (short_endpoint(&ep.name), ep.name.clone()),
                None => (
                    "未找到可用端点".to_string(),
                    "系统里没有可用的播放端点".to_string(),
                ),
            };
            let width = ui.available_width().max(120.0);
            text_cell(ui, width, 26.0, &name, FontId::proportional(17.0), TEXT, false)
                .on_hover_text(full_name);
            ui.add_space(2.0);

            // Windows 默认输出现在是不是采集端点（不是就采不到声）。
            let (current_short, current_full) = match &self.live_default_output {
                Some(full) => (short_endpoint(full), full.clone()),
                None => ("未知".to_string(), "未知".to_string()),
            };
            ui.horizontal(|ui| {
                text_cell(
                    ui,
                    96.0,
                    20.0,
                    "系统默认输出：",
                    FontId::proportional(12.0),
                    MUTED,
                    false,
                );
                let rest = ui.available_width().max(120.0);
                let text = if self.live_default_is_capture {
                    format!("{current_short} √")
                } else {
                    current_short
                };
                text_cell(
                    ui,
                    rest,
                    20.0,
                    &text,
                    FontId::proportional(12.5),
                    if self.live_default_is_capture { OK } else { WARN },
                    false,
                )
                .on_hover_text(current_full);
            });

            // 常驻说明：这是 D 模式最关键的一条。
            let width = ui.available_width().max(120.0);
            text_cell(
                ui,
                width,
                18.0,
                "采集源必须同时是 Windows 默认输出，否则采不到声音。",
                FontId::proportional(12.0),
                MUTED,
                false,
            );

            ui.add_space(6.0);
            // 采集源下拉：全部播放端点 + 类别标注（虚拟声卡/蓝牙音箱/物理输出）。
            egui::ComboBox::from_id_salt("capture_source")
                .width(ui.available_width().max(160.0))
                .selected_text(match (&capture, kind_now) {
                    (Some(ep), Some(kind)) => RichText::new(format!(
                        "{} · {}",
                        clip_text(&short_endpoint(&ep.name), 24),
                        kind.short_label()
                    ))
                    .size(12.5),
                    _ => RichText::new("（未选择采集源）").color(MUTED).size(12.5),
                })
                .show_ui(ui, |ui| {
                    for ep in &devices {
                        let kind = capture_kind(&ep.name);
                        let selected = capture_id
                            .as_deref()
                            .map(|id| id.eq_ignore_ascii_case(&ep.id))
                            .unwrap_or(false);
                        let text = format!(
                            "{}  ·  {}",
                            clip_text(&short_endpoint(&ep.name), 26),
                            kind.label()
                        );
                        if ui
                            .selectable_label(
                                selected,
                                RichText::new(text).size(12.5).color(kind.color()),
                            )
                            .on_hover_text(ep.name.clone())
                            .clicked()
                        {
                            chosen_id = Some(ep.id.clone());
                        }
                    }
                });

            ui.add_space(6.0);
            if ui
                .checkbox(
                    &mut self.cfg.capture.auto_switch_default,
                    "开始路由时自动切为默认输出（退出时切回）",
                )
                .changed()
            {
                self.cfg.save();
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("切为默认输出").clicked() {
                    self.switch_default_to_capture();
                }
                let has_previous = self.cfg.capture.previous_output.is_some();
                if ui
                    .add_enabled(has_previous, egui::Button::new("恢复原输出"))
                    .on_hover_text("切回开始路由前的默认输出")
                    .clicked()
                {
                    self.restore_previous_output();
                }
            });

            // 端点格式：引擎采样率跟随采集源混音格式（零重采样），这里只做展示。
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(muted("端点格式"));
                let fmt = capture
                    .as_ref()
                    .and_then(|ep| ep.mix_format.as_ref())
                    .map(|f| f.describe())
                    .unwrap_or_else(|| "未知".to_string());
                ui.label(RichText::new(format!("混音 {fmt}")).size(12.0).color(TEXT))
                    .on_hover_text(
                        "采集端点的混音格式：路由引擎的采样率跟随它（零重采样）。",
                    );
            });

            // 系统音量联动（从声道路由页挪过来：它只和采集源有关）。
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(muted("系统音量"));
                let mut follow = self.cfg.volume_follow;
                egui::ComboBox::from_id_salt("volume_follow")
                    .width(150.0)
                    .selected_text(follow.label())
                    .show_ui(ui, |ui| {
                        for option in [VolumeFollow::Auto, VolumeFollow::On, VolumeFollow::Off] {
                            ui.selectable_value(&mut follow, option, option.label());
                        }
                    });
                if follow != self.cfg.volume_follow {
                    self.cfg.volume_follow = follow;
                    self.cfg.save();
                    if follow == VolumeFollow::Off {
                        self.restore_target_volumes();
                    } else {
                        self.volume_last = None;
                        self.volume_at = None;
                    }
                }
                let (text, color) = if self.volume_follow_active() {
                    match self.volume_last {
                        Some((scalar, muted)) => (
                            format!(
                                "跟随中：{:.0}%{}",
                                scalar * 100.0,
                                if muted { "（静音）" } else { "" }
                            ),
                            OK,
                        ),
                        None => ("跟随中".to_string(), OK),
                    }
                } else {
                    ("不联动".to_string(), MUTED)
                };
                ui.add(
                    egui::Label::new(RichText::new(text).size(12.0).color(color)).selectable(false),
                );
                if let Some(err) = self.volume_error.clone() {
                    ui.add(
                        egui::Label::new(RichText::new(err).size(11.0).color(ERR))
                            .selectable(false),
                    );
                }
            })
            .response
            .on_hover_text(VOLUME_FOLLOW_HELP);

            // 采集源音量诊断：后置音量的虚拟声卡如果被调低，整条链路会平白衰减几十 dB，
            // 听感就是「人声低、没劲、音质差」（数字链路已实测无损）。
            if let Some((scalar, db)) = self.source_volume_warning() {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "⚠ 采集源音量 {:.0}%（衰减 {:.1} dB）",
                                scalar * 100.0,
                                db
                            ))
                            .size(11.0)
                            .color(WARN),
                        )
                        .selectable(false),
                    )
                    .on_hover_text(
                        "实测：自研虚拟声卡的端点音量是后置的，它直接衰减送进回环的音频。\
                         音量只有 14% 时整条链路少 29.7 dB —— 这正是「人声太低/发闷」的原因\
                         （引擎数字通路实测无损）。把采集源音量拉满，再用系统音量或音箱音量\
                         控制响度即可。",
                    );
                    if ui
                        .small_button("设为 100%")
                        .on_hover_text("把采集源端点音量设为 100%，保住有效位深")
                        .clicked()
                    {
                        self.normalize_source_volume();
                    }
                });
            }

            // 测试音：直接往采集源端点播一段正弦，验证「采集→路由→两台音箱」整条链路。
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .button("播放测试音")
                    .on_hover_text("往采集源端点播放几秒 440 Hz 正弦音（声音会从两台音箱出来）")
                    .clicked()
                {
                    self.play_test_tone();
                }
                ui.label(muted("验证采集源与左右两路是否正常。"));
            });

            // 非虚拟类采集源：琥珀色兜底提示（有 UU 远程时换成正向提示）。
            if kind_now.map(|kind| !kind.is_virtual()).unwrap_or(false) {
                ui.add_space(6.0);
                let tip = if uu_present {
                    "已检测到 UU远程虚拟音频设备，可直接选它做采集源（它是无声的虚拟设备，不会重复出声）。"
                        .to_string()
                } else {
                    "本机没有虚拟声卡时：① 运行一次 UU远程（免费，会装好一个虚拟音频设备）→ 点「刷新设备」，选『扬声器 (UU远程虚拟音频设备)』；② 安装 StereoLink 虚拟声卡（高级：需测试签名 + 重启一次）；③ 选一个没接音箱的物理输出（可能出声）。"
                        .to_string()
                };
                egui::Frame::default()
                    .fill(WARN.gamma_multiply(0.10))
                    .stroke(Stroke::new(1.0_f32, WARN.gamma_multiply(0.45)))
                    .inner_margin(Margin::symmetric(10, 8))
                    .corner_radius(CornerRadius::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(tip).color(WARN).size(12.0));
                    });
            }
        });

        if let Some(id) = chosen_id {
            self.select_capture_endpoint(&id);
        }
    }

    fn main_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(16, 14)),
            )
            .show(ctx, |ui| {
                // 日志页自己带滚动区域，不再套外层 ScrollArea（否则贴底滚动会打架）。
                if self.page == Page::Log {
                    self.log_card(ui);
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("main_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.page {
                        Page::Routing => self.routing_card(ui),
                        Page::Capture => self.capture_card(ui),
                        Page::Devices => self.devices_card(ui),
                        Page::Log => {}
                    });
                ui.add_space(10.0);
            });
    }

    fn routing_card(&mut self, ui: &mut egui::Ui) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("声道路由").size(16.0).strong());
                ui.add_space(6.0);
                if self.engine.is_some() {
                    pill(ui, "分离运行中", OK, 96.0);
                    if let Some(stats) = &self.stats {
                        if stats.degraded {
                            pill(ui, "等待设备", WARN, 84.0);
                        } else if stats.routes.iter().any(|r| r.retry_in_ms > 0) {
                            pill(ui, "重连中", WARN, 72.0);
                        }
                    }
                } else {
                    pill(ui, "未运行", Color32::from_rgb(120, 128, 142), 96.0);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let latency = self
                        .route_engine_config()
                        .map(|c| c.software_latency_ms())
                        .unwrap_or(0);
                    ui.label(muted(format!("软件侧延迟约 {latency} ms")));
                });
            });
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            let spacing = ui.spacing().item_spacing.x;
            let available = ui.available_width();
            let device_w = route_device_width(available, spacing);
            // 固定列表头：字段名常驻在数字上方（用户要求「像表格那样」）。
            {
                let header_font = FontId::proportional(12.0);
                ui.horizontal(|ui| {
                    text_cell(
                        ui,
                        ROUTE_COL_CH,
                        18.0,
                        "声道",
                        header_font.clone(),
                        MUTED,
                        false,
                    );
                    text_cell(
                        ui,
                        device_w,
                        18.0,
                        "设备",
                        header_font.clone(),
                        MUTED,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_LEVEL,
                        18.0,
                        "电平",
                        header_font.clone(),
                        MUTED,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_VOL,
                        18.0,
                        "音量",
                        header_font.clone(),
                        MUTED,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_BUF,
                        18.0,
                        "缓冲",
                        header_font.clone(),
                        MUTED,
                        true,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_PATH,
                        18.0,
                        "路径",
                        header_font.clone(),
                        MUTED,
                        false,
                    );
                    text_cell(ui, ROUTE_COL_INV, 18.0, "反相", header_font, MUTED, false);
                });
            }
            ui.add_space(2.0);
            for channel in [RouteChannel::Left, RouteChannel::Right] {
                self.route_row(ui, channel, device_w);
            }
            ui.label(muted(
                "反相：两台音箱接线/极性相反时，勾选任一路 180°，居中人声与低音就不会互相抵消。",
            ));

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(8.0);

            ui.collapsing("高级：增益 / 抖动缓冲 / 左右微调延迟", |ui| {
                ui.horizontal(|ui| {
                    ui.label(muted("增益"));
                    if ui
                        .add(egui::Slider::new(&mut self.cfg.split.gain, 0.0..=2.0).step_by(0.05))
                        .on_hover_text(
                            "数字增益：1.0 = 原样不放大；大于 1 会在数字域放大，\
                             超过设备余量就会削顶（听感上是破音）。",
                        )
                        .changed()
                    {
                        self.cfg.save();
                    }
                    ui.add_space(10.0);
                    ui.label(muted("抖动缓冲"));
                    if ui
                        .add(
                            egui::Slider::new(&mut self.cfg.split.queue_ms, 10..=200)
                                .suffix(" ms")
                                .step_by(5.0),
                        )
                        .on_hover_text(
                            "每路独立队列，用来吸收两条蓝牙链路各自的时钟漂移与调度抖动：\
                             数值越大越抗卡顿，但软件侧延迟会随之增加\
                             （软件延迟 ≈ 20 ms 采集 + 这个值 + 渲染缓冲/2）。",
                        )
                        .changed()
                    {
                        self.cfg.save();
                        if self.engine.is_some() {
                            self.restart_routing();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(muted("左额外延迟"));
                    let changed_left = ui
                        .add(
                            egui::DragValue::new(&mut self.cfg.split.left_delay_ms)
                                .range(0..=500)
                                .suffix(" ms"),
                        )
                        .changed();
                    ui.add_space(12.0);
                    ui.label(muted("右额外延迟"));
                    let changed_right = ui
                        .add(
                            egui::DragValue::new(&mut self.cfg.split.right_delay_ms)
                                .range(0..=500)
                                .suffix(" ms"),
                        )
                        .changed();
                    if changed_left || changed_right {
                        self.cfg.save();
                        self.restart_routing();
                    }
                });
                ui.label(muted("两箱固定时差（比如左箱偏慢）可以在这里补齐。"));
            });

            // ---- 声学自校验 ----
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("声学自校验").size(14.0).strong());
                help(
                    ui,
                    "用麦克风量出两台音箱的真实到达时间差，自动给较慢的一路垫延迟；\n\
                     同一次测量还会量左右音量并给出平衡建议。\n\
                     · 先选一个麦克风，「体检」确认它真能采到声音；\n\
                     · 「测量并对齐」：先试同时法（精度约 ±1 ms），失败退回依次法（约 ±4 ms），\
                     随后量左右电平、给出音量平衡建议；\n\
                     · 没有麦克风也能用「点击串试听」靠人耳手动微调。",
                );
            });

            let mic_label = self
                .sync_mics
                .iter()
                .find(|m| m.id() == self.sync_mic)
                .map(|m| m.name().to_string())
                .unwrap_or_default();
            let mut pick_mic: Option<String> = None;
            let mut action: Option<&'static str> = None;
            // 固定高度的控件行（不再 horizontal_wrapped，避免换行导致卡片高度跳）。
            ui.horizontal(|ui| {
                cell(
                    ui,
                    240.0,
                    26.0,
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        egui::ComboBox::from_id_salt("sync_mic")
                            .width(236.0)
                            .selected_text(if mic_label.is_empty() {
                                RichText::new("（未选择麦克风）").color(MUTED)
                            } else {
                                RichText::new(clip_text(&short_endpoint(&mic_label), 26))
                                    .color(TEXT)
                            })
                            .show_ui(ui, |ui| {
                                for m in &self.sync_mics {
                                    let id = m.id().to_string();
                                    let text = format!(
                                        "{} [{}]{}",
                                        short_endpoint(m.name()),
                                        m.kind.as_str(),
                                        if m.likely_usable {
                                            ""
                                        } else {
                                            " · 可能不可用"
                                        }
                                    );
                                    if ui.selectable_label(self.sync_mic == id, text).clicked() {
                                        pick_mic = Some(id);
                                    }
                                }
                            });
                    },
                );
                if ui.button("刷新").clicked() {
                    action = Some("refresh");
                }
                if ui.button("体检").clicked() {
                    action = Some("check");
                }
                if ui
                    .add_enabled(
                        !self.sync_busy && !self.sync_mic.is_empty(),
                        egui::Button::new(if self.sync_busy {
                            "校准中…"
                        } else {
                            "校准延迟并平衡音量"
                        }),
                    )
                    .on_hover_text(
                        "一次完成：量两台音箱的真实时差并自动垫延迟，再分别量左右电平、给出音量平衡建议（约 8 秒）",
                    )
                    .clicked()
                {
                    action = Some("sync");
                }
                if ui
                    .add_enabled(!self.clicks_busy, egui::Button::new("点击串试听"))
                    .on_hover_text("不需要麦克风：按当前延迟值同时放点击串，听到两下就手动微调")
                    .clicked()
                {
                    action = Some("clicks");
                }
            });
            if let Some(id) = pick_mic {
                self.sync_mic = id.clone();
                self.cfg.sync_mic = id;
                self.cfg.save();
                self.sync_health = None;
                self.sync_result = None;
                self.sync_status = "已选麦克风，点「体检」看它能不能采到声音。".to_string();
            }
            match action {
                Some("refresh") => self.refresh_mics(),
                Some("check") => self.check_mic(),
                Some("sync") => self.start_sync(),
                Some("clicks") => self.play_alignment(),
                _ => {}
            }
            // 状态行固定高度：文字长短变化不改变卡片高度。
            let status = if self.sync_status.is_empty() {
                " ".to_string()
            } else {
                self.sync_status.clone()
            };
            let width = ui.available_width().max(120.0);
            text_cell(
                ui,
                width,
                20.0,
                &status,
                FontId::proportional(12.0),
                MUTED,
                false,
            )
            .on_hover_text(status);

            // 音量平衡：与延迟对齐同一次测量完成；结果展示测量值 + 建议值，可反复迭代复测。
            if self.balance_result.is_some()
                || self.balance_error.is_some()
                || !self.balance_status.is_empty()
            {
                ui.add_space(2.0);
                let mut apply_and_retest = false;
                ui.horizontal(|ui| {
                    ui.label(muted("音量平衡"));
                    let (text, color) = if let Some(report) = &self.balance_result {
                        let within = report.delta_db.abs() <= BALANCE_DEADBAND_DB;
                        let text = if within {
                            format!(
                                "左 {:.1} dB / 右 {:.1} dB → 已平衡（差 {:.1} dB 在 ±{:.0} dB 内）",
                                report.left_db,
                                report.right_db,
                                report.delta_db.abs(),
                                BALANCE_DEADBAND_DB
                            )
                        } else if report.delta_db < 0.0 {
                            format!(
                                "左 {:.1} dB / 右 {:.1} dB → 右边轻了 {:.1} dB",
                                report.left_db,
                                report.right_db,
                                report.delta_db.abs()
                            )
                        } else {
                            format!(
                                "左 {:.1} dB / 右 {:.1} dB → 左边轻了 {:.1} dB",
                                report.left_db,
                                report.right_db,
                                report.delta_db.abs()
                            )
                        };
                        (text, if within { OK } else { ACCENT })
                    } else if let Some(err) = &self.balance_error {
                        (format!("测量失败：{err}"), WARN)
                    } else {
                        (self.balance_status.clone(), MUTED)
                    };
                    let tip = text.clone();
                    text_cell(
                        ui,
                        (ui.available_width() - 190.0).max(120.0),
                        20.0,
                        &text,
                        FontId::proportional(12.0),
                        color,
                        false,
                    )
                    .on_hover_text(tip);
                    if let Some(report) = &self.balance_result {
                        let within = report.delta_db.abs() <= BALANCE_DEADBAND_DB;
                        if !within {
                            let low_confidence = report.confidence < 0.5;
                            let button = egui::Button::new(RichText::new("应用并复测").color(
                                if low_confidence {
                                    Color32::BLACK
                                } else {
                                    TEXT
                                },
                            ));
                            let button = if low_confidence { button.fill(WARN) } else { button };
                            if ui
                                .add(button)
                                .on_hover_text(if low_confidence {
                                    "置信度偏低：把麦克风放到两箱之间的听音位置、关掉音乐并保持安静后再测。\n点一次会按建议值调音量并自动复测（蓝牙音箱通常 2～3 轮收敛）"
                                } else {
                                    "按建议值调两台音箱的音量，然后自动复测一轮；蓝牙音箱自报音量与实际声学衰减有偏差，迭代 2～3 轮更准"
                                })
                                .clicked()
                            {
                                apply_and_retest = true;
                            }
                        }
                    }
                });
                if let Some(report) = &self.balance_result {
                    ui.horizontal(|ui| {
                        ui.label(muted("建议音量"));
                        let text = format!(
                            "左 {}% / 右 {}%（置信度 {:.0}%）",
                            report.suggested_left_percent,
                            report.suggested_right_percent,
                            report.confidence * 100.0
                        );
                        ui.label(RichText::new(text).size(12.0).color(
                            if report.confidence < 0.5 { WARN } else { TEXT },
                        ));
                    });
                }
                if apply_and_retest {
                    self.apply_balance();
                    self.start_sync();
                }
            }

            // 采集统计行常驻（未运行时显示占位），避免行出现/消失导致卡片高度跳。
            ui.add_space(4.0);
            let (capture_line, source_line) = match &self.stats {
                Some(stats) => (
                    format!(
                        "采集 {:.1}s · {:.2} MB",
                        stats.captured_frames as f64 / 44_100.0,
                        stats.frame_bytes as f64 / 1_048_576.0
                    ),
                    format!("源：{}", short_endpoint(&stats.source_device)),
                ),
                None => ("采集 —".to_string(), "源：—".to_string()),
            };
            ui.horizontal(|ui| {
                text_cell(
                    ui,
                    220.0,
                    20.0,
                    &capture_line,
                    FontId::monospace(12.0),
                    MUTED,
                    false,
                );
                let rest = ui.available_width().max(120.0);
                text_cell(
                    ui,
                    rest,
                    20.0,
                    &source_line,
                    FontId::proportional(12.0),
                    MUTED,
                    false,
                )
                .on_hover_text(source_line);
            });
        });
    }

    fn route_row(&mut self, ui: &mut egui::Ui, channel: RouteChannel, device_w: f32) {
        let color = channel_color(channel);
        let names = self.render_names();
        let current = self.configured_endpoint(channel).to_string();
        let mut chosen = current.clone();
        let label = channel_label(channel);
        let route = self.route_of(channel).cloned();
        let current_volume = self.route_volumes.get(&channel).copied().unwrap_or(1.0);
        let current_invert = match channel {
            RouteChannel::Left => self.cfg.split.invert_left,
            RouteChannel::Right => self.cfg.split.invert_right,
            RouteChannel::Both => false,
        };
        let mut commit_volume: Option<f32> = None;
        let mut commit_invert: Option<bool> = None;

        ui.horizontal(|ui| {
            // 声道：固定宽度小标签，不随内容伸缩。
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(ROUTE_COL_CH, ROUTE_ROW_H), Sense::hover());
            if ui.is_rect_visible(rect) {
                let painter = ui.painter();
                painter.rect_filled(
                    rect.shrink2(Vec2::new(0.0, 3.0)),
                    CornerRadius::same(6),
                    color.gamma_multiply(0.16),
                );
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    label,
                    FontId::proportional(12.0),
                    color,
                );
            }
            let mut tip = format!(
                "目标端点：{}",
                if current.is_empty() {
                    "（未选择）"
                } else {
                    current.as_str()
                }
            );
            if let Some(route) = &route {
                tip.push_str(&format!(
                    "\n开流路径：{}",
                    route.format_path.as_deref().unwrap_or("尚未开流")
                ));
                if let Some(err) = &route.error {
                    tip.push_str(&format!("\n链路错误：{err}"));
                }
            }
            response.on_hover_text(tip);

            // 设备：固定宽度下拉框 + 截断 + tooltip 全名。
            cell(
                ui,
                device_w,
                ROUTE_ROW_H,
                Layout::left_to_right(Align::Center),
                |ui| {
                    egui::ComboBox::from_id_salt(format!("route_{label}"))
                        .width((device_w - 8.0).max(120.0))
                        .selected_text(if current.is_empty() {
                            RichText::new("（未选择设备）").color(MUTED).size(13.0)
                        } else {
                            RichText::new(clip_text(&short_endpoint(&current), 32))
                                .color(TEXT)
                                .size(13.0)
                        })
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    current.is_empty(),
                                    RichText::new("（未选择设备）").color(MUTED),
                                )
                                .clicked()
                            {
                                chosen = String::new();
                            }
                            for name in &names {
                                let selected = *name == current;
                                if ui
                                    .selectable_label(selected, short_endpoint(name))
                                    .on_hover_text(name)
                                    .clicked()
                                {
                                    chosen = name.clone();
                                }
                            }
                        })
                        .response
                        .on_hover_text(if current.is_empty() {
                            "点击选择这个声道的输出设备".to_string()
                        } else {
                            current.clone()
                        });
                },
            );

            // 电平：固定 rect 电平条 + 右对齐百分比。
            let linear = route.as_ref().map(|route| route.level).unwrap_or(0.0);
            cell(
                ui,
                ROUTE_COL_LEVEL,
                ROUTE_ROW_H,
                Layout::left_to_right(Align::Center),
                |ui| {
                    level_bar(ui, 40.0, linear.sqrt(), color);
                    text_cell(
                        ui,
                        36.0,
                        ROUTE_ROW_H,
                        &format!("{:>3.0}%", linear * 100.0),
                        FontId::monospace(12.0),
                        TEXT,
                        true,
                    );
                },
            );

            // 音量列：每路输出设备的音量条（替代原先的「已发流」列）。
            // 拖动时即时更新内存值；节流写回端点音量，避免拖动中频繁调用 WASAPI。
            let mut volume = current_volume;
            let volume_ready = self
                .route_volume_at
                .get(&channel)
                .map(|at| at.elapsed() >= Duration::from_millis(250))
                .unwrap_or(true);
            cell(
                ui,
                ROUTE_COL_VOL,
                ROUTE_ROW_H,
                Layout::left_to_right(Align::Center),
                |ui| {
                    let slider = ui.add_sized(
                        [96.0, 16.0],
                        egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                    );
                    text_cell(
                        ui,
                        40.0,
                        ROUTE_ROW_H,
                        &format!("{:>3.0}%", volume * 100.0),
                        FontId::monospace(11.0),
                        TEXT,
                        true,
                    );
                    if slider.changed() {
                        self.route_volumes.insert(channel, volume);
                        if (volume_ready || slider.drag_stopped()) && !current.is_empty() {
                            commit_volume = Some(volume);
                        }
                    }
                    slider
                },
            )
            .on_hover_text("这台音箱的输出音量（直接写回它的音量端点）；拖动结束自动生效。");

            let buffered = route
                .as_ref()
                .map(|route| format!("{:>5.0} ms", route.queued_ms))
                .unwrap_or_else(|| "    —".to_string());
            text_cell(
                ui,
                ROUTE_COL_BUF,
                ROUTE_ROW_H,
                &buffered,
                FontId::monospace(12.0),
                TEXT,
                true,
            );

            let (path_short, path_tip, path_color) = match route.as_ref() {
                Some(route) if route.retry_in_ms > 0 => (
                    "重连",
                    format!(
                        "正在自动重连：第 {} 次失败，约 {:.0} 秒后重试。\n{}",
                        route.retry_count,
                        route.retry_in_ms as f64 / 1000.0,
                        route
                            .error
                            .as_deref()
                            .unwrap_or("端点暂时不可用（设备没开机或没连上）")
                    ),
                    WARN,
                ),
                Some(route) => match route.format_path.as_deref() {
                    Some(path) => (
                        if path.contains("autoconvert") {
                            "Auto"
                        } else {
                            "转换"
                        },
                        format!("开流路径：{path}"),
                        MUTED,
                    ),
                    None => ("—", "开流路径：尚未开流".to_string(), MUTED),
                },
                None => ("—", "开流路径：尚未开流".to_string(), MUTED),
            };
            text_cell(
                ui,
                ROUTE_COL_PATH,
                ROUTE_ROW_H,
                path_short,
                FontId::proportional(12.0),
                path_color,
                false,
            )
            .on_hover_text(path_tip);

            // 反相列：勾选后这一路输出反相 180°（两台音箱极性/接线相反时用）。
            let mut invert = current_invert;
            cell(
                ui,
                ROUTE_COL_INV,
                ROUTE_ROW_H,
                Layout::left_to_right(Align::Center),
                |ui| {
                    if ui
                        .checkbox(&mut invert, "180°")
                        .on_hover_text(
                            "勾选后这一路输出反相 180°：两台音箱极性相反时，可避免人声与低音互相抵消。",
                        )
                        .changed()
                    {
                        commit_invert = Some(invert);
                    }
                },
            );
        });

        // 音量写回端点（节流：拖动中不频繁调 WASAPI，松手或间隔够了再写）。
        if let Some(volume) = commit_volume {
            if !current.is_empty() {
                match set_endpoint_volume(&current, volume) {
                    Ok(_) => {
                        self.route_volume_at.insert(channel, Instant::now());
                    }
                    Err(err) => {
                        self.toast(format!("写回音量失败：{err}"));
                    }
                }
            }
        }
        // 反相立即生效：存配置并重启路由（sink 线程按新配置重开，延迟线会重建）。
        if let Some(invert) = commit_invert {
            match channel {
                RouteChannel::Left => self.cfg.split.invert_left = invert,
                RouteChannel::Right => self.cfg.split.invert_right = invert,
                RouteChannel::Both => {}
            }
            self.save();
            self.restart_routing();
        }

        if chosen != current {
            self.set_route_endpoint(channel, chosen);
        }
    }

    fn devices_card(&mut self, ui: &mut egui::Ui) {
        let devices = self.render_devices();
        let live_default = self.live_default_output.clone();
        let left = self.cfg.split.left_endpoint.clone();
        let right = self.cfg.split.right_endpoint.clone();
        let capture_id = self.capture_endpoint().map(|ep| ep.id);

        let mut set_left: Option<String> = None;
        let mut set_right: Option<String> = None;
        let mut refresh = false;
        let mut format_change: Option<(String, String)> = None;
        let mut probe_one: Option<String> = None;
        let mut fix_format: Option<(String, String, EndpointFormat)> = None;

        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("本机播放设备（{}）", devices.len()))
                        .size(16.0)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("刷新").clicked() {
                        refresh = true;
                    }
                });
            });
            ui.add_space(2.0);

            if devices.is_empty() {
                let width = ui.available_width().max(120.0);
                text_cell(
                    ui,
                    width,
                    20.0,
                    "没有枚举到播放设备",
                    FontId::proportional(13.0),
                    WARN,
                    false,
                );
                return;
            }

            // 列宽固定：内容变化只影响单元格内部，不再重排整张表。
            let spacing = ui.spacing().item_spacing.x;
            let available = ui.available_width();
            let col_action = 190.0;
            let col_role = 140.0;
            let col_device = 230.0;
            let col_format = (available - col_device - col_role - col_action - spacing * 3.0).max(150.0);

            {
                let header_font = FontId::proportional(12.0);
                ui.horizontal(|ui| {
                    text_cell(ui, col_device, 18.0, "设备", header_font.clone(), MUTED, false);
                    text_cell(ui, col_format, 18.0, "格式", header_font.clone(), MUTED, false);
                    text_cell(ui, col_role, 18.0, "角色", header_font.clone(), MUTED, false);
                    text_cell(ui, col_action, 18.0, "操作", header_font, MUTED, false);
                });
            }
            ui.add_space(2.0);

            for ep in &devices {
                let is_capture = capture_id
                    .as_deref()
                    .map(|id| id.eq_ignore_ascii_case(&ep.id))
                    .unwrap_or(false);
                let is_default = live_default.as_deref() == Some(ep.name.as_str());
                let is_left = left == ep.name;
                let is_right = right == ep.name;

                let current = self.format_cache.get(&ep.id).cloned().flatten();
                let supported = self.format_supported.get(&ep.id).cloned().flatten();
                let probing = self.format_probing.contains(&ep.id);

                let mut current_tip = format!(
                    "当前设备格式：{}",
                    current
                        .as_ref()
                        .map(EndpointFormat::describe)
                        .unwrap_or_else(|| "读取中".to_string())
                );
                let mut fixed: Option<EndpointFormat> = None;
                if let (Some(f), Some(list)) = (&current, &supported) {
                    if !list.contains(f) {
                        fixed = Self::closest_supported(list, f);
                        current_tip = match &fixed {
                            Some(t) => format!(
                                "当前设备格式 {} 不在这个端点的独占支持列表里（共享模式仍可用）。\n最接近的受支持格式：{}",
                                f.describe(),
                                t.describe()
                            ),
                            None => format!(
                                "当前设备格式 {} 不在这个端点的独占支持列表里（共享模式仍可用）",
                                f.describe()
                            ),
                        };
                    }
                }

                ui.horizontal(|ui| {
                    // 设备：固定宽 + 截断，全名 / 默认输出 / 当前格式都进 tooltip。
                    let name_tip = format!(
                        "{}{}\n{}",
                        if is_default {
                            "（Windows 默认输出）\n"
                        } else {
                            ""
                        },
                        ep.name,
                        current_tip
                    );
                    text_cell(
                        ui,
                        col_device,
                        ROUTE_ROW_H,
                        &short_endpoint(&ep.name),
                        FontId::proportional(13.0),
                        if is_default { OK } else { TEXT },
                        false,
                    )
                    .on_hover_text(name_tip);

                    // 格式：固定宽度单元格里放下拉框 / 探测按钮，单元格尺寸恒定。
                    cell(
                        ui,
                        col_format,
                        ROUTE_ROW_H,
                        Layout::left_to_right(Align::Center),
                        |ui| match (&supported, probing) {
                            // 只列这个端点**真正支持**的格式。
                            (Some(list), _) if !list.is_empty() => {
                                let choice =
                                    self.format_choices.entry(ep.id.clone()).or_insert(list[0]);
                                let before = *choice;
                                let in_list = list.contains(choice);
                                let selected = if in_list {
                                    choice.describe()
                                } else {
                                    format!("{}（当前格式不受支持）", choice.describe())
                                };
                                egui::ComboBox::from_id_salt(format!("fmt_{}", ep.id))
                                    .width((col_format - 8.0).max(140.0))
                                    .selected_text(
                                        RichText::new(clip_text(&selected, 26)).size(12.0),
                                    )
                                    .show_ui(ui, |ui| {
                                        for f in list {
                                            ui.selectable_value(
                                                choice,
                                                *f,
                                                RichText::new(f.describe()).size(12.0),
                                            );
                                        }
                                    })
                                    .response
                                    .on_hover_text(format!(
                                        "只列出这个端点真正支持的 {} 种格式（独占模式 IsFormatSupported 实测）\n当前混音格式：{}",
                                        list.len(),
                                        endpoint_format(ep)
                                    ));
                                if *choice != before {
                                    format_change =
                                        Some((ep.id.clone(), short_endpoint(&ep.name)));
                                }
                            }
                            (Some(_), _) => {
                                text_cell(
                                    ui,
                                    (col_format - 8.0).max(120.0),
                                    20.0,
                                    "无支持格式",
                                    FontId::proportional(12.0),
                                    ERR,
                                    false,
                                );
                            }
                            (None, true) => {
                                text_cell(
                                    ui,
                                    (col_format - 8.0).max(120.0),
                                    20.0,
                                    "探测格式中…",
                                    FontId::proportional(12.0),
                                    MUTED,
                                    false,
                                );
                            }
                            (None, false) => {
                                if ui
                                    .add_sized(
                                        [(col_format - 8.0).max(120.0), 20.0],
                                        egui::Button::new(RichText::new("探测格式").size(12.0)),
                                    )
                                    .on_hover_text("用独占模式逐个试探这个端点支持的格式")
                                    .clicked()
                                {
                                    probe_one = Some(ep.id.clone());
                                }
                            }
                        },
                    );

                    // 角色：合成一行固定宽度文字，超长截断。
                    let mut roles: Vec<&str> = Vec::new();
                    if is_default {
                        roles.push("默认输出");
                    }
                    if is_capture {
                        roles.push("采集源");
                    }
                    if is_left {
                        roles.push("左声道");
                    }
                    if is_right {
                        roles.push("右声道");
                    }
                    let role_text = if roles.is_empty() {
                        "—".to_string()
                    } else {
                        roles.join(" · ")
                    };
                    let role_color = if is_left {
                        ACCENT
                    } else if is_right {
                        ACCENT2
                    } else if is_default {
                        OK
                    } else if is_capture {
                        ACCENT
                    } else {
                        MUTED
                    };
                    let role_tip = role_text.clone();
                    text_cell(
                        ui,
                        col_role,
                        ROUTE_ROW_H,
                        &role_text,
                        FontId::proportional(12.0),
                        role_color,
                        false,
                    )
                    .on_hover_text(role_tip);

                    // 操作：固定宽度单元格，按钮出现/消失不会影响列宽。
                    cell(
                        ui,
                        col_action,
                        ROUTE_ROW_H,
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            if is_capture {
                                text_cell(
                                    ui,
                                    (col_action - 8.0).max(120.0),
                                    20.0,
                                    "当前采集源",
                                    FontId::proportional(12.0),
                                    MUTED,
                                    false,
                                );
                            } else {
                                if ui
                                    .add_enabled(!is_left, egui::Button::new("设为左").small())
                                    .clicked()
                                {
                                    set_left = Some(ep.name.clone());
                                }
                                if ui
                                    .add_enabled(!is_right, egui::Button::new("设为右").small())
                                    .clicked()
                                {
                                    set_right = Some(ep.name.clone());
                                }
                                if let Some(t) = &fixed {
                                    if ui
                                        .add(egui::Button::new("修正").small())
                                        .on_hover_text(format!("改成 {}", t.describe()))
                                        .clicked()
                                    {
                                        fix_format = Some((
                                            ep.id.clone(),
                                            short_endpoint(&ep.name),
                                            *t,
                                        ));
                                    }
                                }
                            }
                        },
                    );
                });
            }
        });

        if refresh {
            self.refresh_endpoints();
            self.refresh_formats();
        }
        if let Some(name) = set_left {
            self.set_route_endpoint(RouteChannel::Left, name);
            self.toast("左声道已改为新设备（已应用）");
        }
        if let Some(name) = set_right {
            self.set_route_endpoint(RouteChannel::Right, name);
            self.toast("右声道已改为新设备（已应用）");
        }
        if let Some((id, label)) = format_change {
            self.apply_format(&id, &label);
        }
        if let Some(id) = probe_one {
            self.probe_one_format(id);
        }
        if let Some((id, label, format)) = fix_format {
            self.format_choices.insert(id.clone(), format);
            self.apply_format(&id, &label);
        }
    }

    fn log_card(&mut self, ui: &mut egui::Ui) {
        let count = self.log.len();
        let lines = self.log.lines();
        let font = self.font.clone();
        let mut clear = false;
        let mut copy_diag = false;
        let mut copy_log = false;
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("日志").size(16.0).strong());
                text_cell(
                    ui,
                    72.0,
                    18.0,
                    &format!("{count} 行"),
                    FontId::proportional(12.0),
                    MUTED,
                    false,
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("清空").clicked() {
                        clear = true;
                    }
                    if ui.small_button("复制全部").clicked() {
                        copy_log = true;
                    }
                    if ui.small_button("复制诊断").clicked() {
                        copy_diag = true;
                    }
                });
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.log_autoscroll, "自动滚动");
                ui.label(muted(
                    "运行统计每 30 秒写入一次（每路已发流 / 欠载 / 缓冲 / 重连状态）；故障与重连会立即记录。",
                ));
            });
            ui.add_space(6.0);
            let height = (ui.available_height() - 52.0).max(180.0);
            egui::Frame::default()
                .fill(Color32::from_rgb(13, 16, 21))
                .inner_margin(Margin::symmetric(10, 8))
                .corner_radius(CornerRadius::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("log_scroll")
                        .max_height(height)
                        .stick_to_bottom(self.log_autoscroll)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for line in &lines {
                                log_line(ui, line);
                            }
                        });
                });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(muted("界面字体："));
                let candidates = fonts::candidates_present();
                let tip = if candidates.is_empty() {
                    "本机没有找到候选中文字体；界面会退回默认字体，中文可能显示为方块".to_string()
                } else {
                    format!("本机可用的候选字体：{}", candidates.join("、"))
                };
                match &font {
                    Some(path) => {
                        ui.label(muted(path.clone())).on_hover_text(tip);
                    }
                    None => {
                        ui.label(
                            RichText::new("未找到中文字体，界面可能显示为方块（详见启动日志）")
                                .size(12.0)
                                .color(WARN),
                        )
                        .on_hover_text(tip);
                    }
                };
            });
        });
        if clear {
            self.log.clear();
            self.toast("日志已清空");
        }
        if copy_diag {
            let text = self
                .hints()
                .iter()
                .map(|hint| format!("{} {}", hint.icon(), hint.text))
                .collect::<Vec<_>>()
                .join("\n");
            ui.ctx().copy_text(text);
            self.toast("诊断信息已复制");
        }
        if copy_log {
            let text = lines
                .iter()
                .map(|line| format!("{:>6.1}s  {}", line.at.as_secs_f64(), line.text.trim_end()))
                .collect::<Vec<_>>()
                .join("\n");
            ui.ctx().copy_text(text);
            self.toast("日志已复制");
        }
    }

    /// 首次需要时安装系统托盘；失败只用日志警告，绝不阻塞主界面。
    fn ensure_tray(&mut self, ctx: &egui::Context) {
        if self.tray.is_some() || self.tray_attempted {
            return;
        }
        self.tray_attempted = true;
        let icon = crate::app_icon();
        match Tray::install("StereoLink 立体声链", &icon.rgba, icon.width, ctx.clone()) {
            Ok(tray) => {
                self.tray = Some(tray);
                tracing::info!("系统托盘图标已创建");
            }
            Err(err) => {
                tracing::warn!("系统托盘不可用：{err}");
            }
        }
    }

    /// 处理托盘事件：左键/「显示主界面」唤回窗口，「退出」走正常关闭流程。
    fn poll_tray(&mut self, ctx: &egui::Context) {
        loop {
            let event = self.tray.as_ref().and_then(|tray| tray.poll());
            match event {
                Some(TrayEvent::Show) => {
                    self.window_hidden = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Some(TrayEvent::Quit) => {
                    self.cfg.save();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                None => return,
            }
        }
    }

    /// 关闭窗口：按「先问 / 缩小至托盘 / 直接关闭」三态处理。
    fn handle_close(&mut self, ctx: &egui::Context) {
        let outcome = close_flow::on_close_requested(ctx, &mut self.cfg, &mut self.close_state);
        match outcome {
            CloseOutcome::None => {}
            CloseOutcome::MinimizeToTray => {
                self.window_hidden = true;
                if self.tray.is_some() {
                    // 用「最小化」而不是 `Visible(false)`：窗口一旦不可见，winit 就不再派发
                    // `RedrawRequested`，eframe 的重绘队列清空后 `ControlFlow` 会永远停在
                    // `Poll`（eframe-0.31.1 `native/run.rs:170-193` 的 check_redraw_requests），
                    // 表现为隐藏期间忙等跑满一个核（实测 97–100% 单核）。最小化窗口仍会收到
                    // 重绘，CPU 回到个位数；窗口本身创建时已 `with_taskbar(false)`，
                    // 所以最小化后任务栏也不留按钮，观感与「收进托盘」一致。
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                } else {
                    // 托盘不可用：退化为普通最小化，避免窗口「凭空消失」。
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    self.toast("系统托盘不可用，已最小化为普通窗口（请查看日志）");
                }
                self.cfg.save();
            }
            CloseOutcome::Quit => {
                self.cfg.save();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// 定期从设备读回两路音量，保证首次显示就是设备当前音量（拖动中不覆盖）。
    fn refresh_route_volumes(&mut self) {
        if let Some(at) = self.route_volume_refresh_at {
            if at.elapsed() < Duration::from_millis(1_000) {
                return;
            }
        }
        self.route_volume_refresh_at = Some(Instant::now());
        for channel in [RouteChannel::Left, RouteChannel::Right] {
            let endpoint = self.configured_endpoint(channel).to_string();
            if endpoint.is_empty() {
                continue;
            }
            // 刚写过音量（拖动中）就别读回，避免条跳。
            let recently_written = self
                .route_volume_at
                .get(&channel)
                .map(|at| at.elapsed() < Duration::from_millis(600))
                .unwrap_or(false);
            if recently_written {
                continue;
            }
            if let Ok(volume) = get_endpoint_volume(&endpoint) {
                self.route_volumes
                    .insert(channel, volume.scalar.clamp(0.0, 1.0));
            }
        }
    }
}

fn log_line(ui: &mut egui::Ui, line: &LogLine) {
    let color = match line.level {
        tracing::Level::ERROR => ERR,
        tracing::Level::WARN => WARN,
        tracing::Level::INFO => TEXT,
        _ => MUTED,
    };
    ui.label(
        RichText::new(format!(
            "{:>6.1}s  {}",
            line.at.as_secs_f64(),
            line.text.trim_end()
        ))
        .monospace()
        .size(12.0)
        .color(color),
    );
}

impl eframe::App for StereoLinkApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 隐藏（最小化到托盘）期间把心跳降到 1s：既省 CPU，又给托盘事件与音量跟随
        // 留一层兜底刷新。窗口不可见时**不能**用 250ms 心跳（见 handle_close 的注释）。
        ctx.request_repaint_after(if self.window_hidden {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(250)
        });
        self.tick_volume_waker(ctx);
        self.ensure_tray(ctx);
        self.handle_close(ctx);
        self.poll_tray(ctx);
        self.tick();
        self.refresh_route_volumes();
        self.header(ctx);
        self.diagnostic_bar(ctx);
        self.sidebar(ctx);
        self.main_panel(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.exit_handled {
            return;
        }
        self.exit_handled = true;
        self.stop_routing(true);
        self.cfg.save();
        tracing::info!("面板退出：已停止路由并保存配置");
    }
}

/// 隐藏到托盘期间的「音量跟随」唤醒源。
///
/// 隐藏后不能请求周期重绘（不可见窗口上 `ControlFlow::WaitUntil` 会忙等，实测跑满
/// 一个核），但「系统音量跟随」必须在隐藏时继续生效 —— 否则用户缩小到托盘后调音量
/// 就失灵了。所以这条后台线程只做廉价轮询（4 Hz 读一次采集源端点音量），检测到变化
/// 才 `request_repaint()` 唤醒主循环；真正的写入仍由
/// `update()` → `tick_volume_follow()` 完成（复用同一份节流/还原逻辑）。
struct VolumeWaker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl VolumeWaker {
    /// 起一条轮询线程；线程名固定便于排查。
    fn spawn(ctx: egui::Context, source: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("stereo-link-volume-wake".to_string())
            .spawn(move || {
                let mut last: Option<f32> = None;
                while !flag.load(Ordering::Relaxed) {
                    if let Ok(volume) = get_endpoint_volume(&source) {
                        let changed = last
                            .map(|prev| (prev - volume.scalar).abs() >= 0.005)
                            .unwrap_or(true);
                        if changed {
                            last = Some(volume.scalar);
                            ctx.request_repaint();
                            // 窗口收进托盘（最小化）时 `update()` 不会因为重绘请求而跑：
                            // winit 的 `request_redraw` 在窗口不可见/最小化时不产生 WM_PAINT。
                            // 这里补一次和托盘事件相同的唤醒，保证托盘期间调系统音量也能跟随。
                            unsafe { crate::tray::wake_hidden_main_window() };
                        }
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            })
            .ok();
        Self { stop, handle }
    }
}

impl Drop for VolumeWaker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_endpoint_strips_speaker_prefix() {
        assert_eq!(
            short_endpoint("扬声器 (Living Room Speaker)"),
            "Living Room Speaker"
        );
        assert_eq!(short_endpoint("Living Room Speaker"), "Living Room Speaker");
        assert_eq!(short_endpoint(" 耳机 (Chu2 DSP) "), "Chu2 DSP");
    }

    #[test]
    fn channel_colors_are_distinct() {
        assert_ne!(
            channel_color(RouteChannel::Left),
            channel_color(RouteChannel::Right)
        );
    }

    #[test]
    fn only_third_party_virtual_sources_ignore_system_volume() {
        // 第三方虚拟设备：不吃系统音量 → 需要镜像。
        assert!(ignores_system_volume("扬声器 (UU远程虚拟音频设备)"));
        assert!(ignores_system_volume("扬声器 (Steam Streaming Microphone)"));
        // 自研虚拟声卡与物理输出：后置音量，系统音量本来就已经作用在采集幅度里 → 不镜像。
        assert!(!ignores_system_volume("扬声器 (立体声链虚拟声卡)"));
        assert!(!ignores_system_volume(
            "扬声器 (Realtek High Definition Audio)"
        ));
        assert!(!ignores_system_volume(
            "CG345UK (NVIDIA High Definition Audio)"
        ));
        assert!(!ignores_system_volume("扬声器 (Living Room Speaker)"));
    }

    fn test_endpoint(id: &str, name: &str) -> EndpointInfo {
        EndpointInfo {
            id: id.to_string(),
            name: name.to_string(),
            flow: DataFlow::Render,
            is_default: false,
            mix_format: None,
            error: None,
        }
    }

    #[test]
    fn duplicate_endpoint_names_fall_back_to_id_selector() {
        // 同名端点（本机两个 NVIDIA）必须用 id，否则引擎按名字会选错设备。
        let nvidia_a = test_endpoint(
            "{0.0.0.00000000}.{aaaaaaaa-1111-2222-3333-444444444444}",
            "扬声器 (NVIDIA High Definition Audio)",
        );
        let nvidia_b = test_endpoint(
            "{0.0.0.00000000}.{bbbbbbbb-1111-2222-3333-444444444444}",
            "扬声器 (NVIDIA High Definition Audio)",
        );
        let virtual_card = test_endpoint(
            "{0.0.0.00000000}.{6e6d7f6c-a61a-4fb2-9e4c-0fdef7303832}",
            "扬声器 (立体声链虚拟声卡)",
        );
        let renders = vec![nvidia_a.clone(), nvidia_b.clone(), virtual_card.clone()];
        assert_eq!(capture_selector_for(&nvidia_a, &renders), nvidia_a.id);
        assert_eq!(
            capture_selector_for(&virtual_card, &renders),
            virtual_card.name
        );
    }
}
