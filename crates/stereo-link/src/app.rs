//! StereoLink 立体声链 —— 桌面面板。
//!
//! 界面按「一屏看懂」组织：顶栏是身份与总状态，中间左侧是声道路由与设备表，
//! 右侧是采集源、诊断与说明。所有动作（开始/停止/自检音/切默认输出）都在一屏内。
//!
//! 设计要点：
//! - 面板只做两件事：把「采集源」的 WASAPI 回环流拆成左右两路推给两台蓝牙音箱；
//!   把 Windows 默认输出切到采集源（并在退出时切回）。
//! - 采集源不再限定虚拟声卡：任意播放端点都能选（自研虚拟声卡、UU远程虚拟设备、
//!   没接音箱的物理输出都行），只要它同时是 Windows 默认输出就能采到声音。
//! - 引擎跑在自己的线程里（见 `stereo_audio::route`），UI 线程只读快照 + 发命令。
//! - 状态用少量自定义绘制（电平条、胶囊、卡片）代替默认控件外观，避免糊成一团。

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui::{
    Align, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2,
};
use stereo_audio::{
    DataFlow, EndpointFormat, EndpointInfo, MicCandidate, MicHealth, PcmFormat, RouteChannel,
    RouteConfig, RouteEngineConfig, RouteEngineHandle, RouteEngineStats, RouteStats, SyncConfig,
    SyncMethod, SyncResult,
};

use crate::audio_default;
use crate::config::AppConfig;
use crate::fonts;
use crate::logbuf::{LogBuffer, LogLine};

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
        let mid = (lo + hi + 1) / 2;
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
const ROUTE_COL_SENT: f32 = 92.0;
const ROUTE_COL_BUF: f32 = 72.0;
const ROUTE_COL_UNDER: f32 = 52.0;
const ROUTE_COL_PATH: f32 = 44.0;
const ROUTE_ROW_H: f32 = 28.0;
/// 诊断卡固定槽位数：条数变化不改变卡片高度。
const DIAG_SLOTS: usize = 5;

/// 「欠载」的悬停说明：表头与数据行共用同一段话。
const UNDERRUN_HELP: &str = "\
欠载（underrun）= 这一路要往音箱写数据时，抖动缓冲里已经没数据了，\
只能补静音凑够这一轮（约 10 ms），每补一次记 1 次。\n\
· 0～几次：正常 —— 刚开始路由时队列还没攒满，头一两轮必然补静音；\n\
· 几十次：偶发轻微咔哒，多半听不出来；\n\
· 几百次以上：会听到断音/爆音，把「抖动缓冲」调大（40 → 80～120 ms，代价是延迟增加），\
并确认采集源就是 Windows 默认输出、播放器确实在放声音。\n\
反方向的问题是丢帧：缓冲满了就丢掉最旧的采样，用来吸收两条链路的时钟漂移。";

/// 设备列吃掉剩余宽度：同一窗口尺寸下恒定，不随数值变化。
fn route_device_width(available: f32, spacing: f32) -> f32 {
    let fixed = ROUTE_COL_CH
        + ROUTE_COL_LEVEL
        + ROUTE_COL_SENT
        + ROUTE_COL_BUF
        + ROUTE_COL_UNDER
        + ROUTE_COL_PATH;
    (available - fixed - spacing * 6.0).max(150.0)
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

/// `扬声器 (Xiaomi Sound-1695)` → `Xiaomi Sound-1695`。
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
    const BLUETOOTH_HINTS: &[&str] = &["Xiaomi Sound", "立体声组合", "Bluetooth", "蓝牙", "EDIFIER"];
    if BLUETOOTH_HINTS.iter().any(|hint| name.contains(hint)) {
        return CaptureKind::Bluetooth;
    }
    CaptureKind::Physical
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
    fn color(&self) -> Color32 {
        match self.tone {
            Tone::Ok => OK,
            Tone::Warn => WARN,
            Tone::Err => ERR,
        }
    }
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
    /// 后台自校验线程的结果通道。
    sync_rx: Option<std::sync::mpsc::Receiver<Result<SyncResult, String>>>,
    /// 最近一次自校验结果。
    sync_result: Option<SyncResult>,
    /// 手动对齐点击串正在播。
    clicks_busy: bool,
    /// 点击串开始时间（用于自动解除忙碌标记）。
    clicks_at: Option<Instant>,
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
                tracing::warn!("路由线程已退出（可能输出设备被拔掉/切换）");
                self.engine = None;
                self.stats = None;
            }
        }

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
                .map(|r| format!("{}={}", channel_label(r.channel), short_endpoint(&r.endpoint)))
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
        if !self.sync_mic.is_empty() && (self.sync_status.is_empty() || self.sync_status == "还没选麦克风。")
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

    /// 后台跑一次双路标定（同时法优先，失败退回依次法）。
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
        let (tx, rx) = std::sync::mpsc::channel();
        let mic_t = mic.clone();
        let (left_t, right_t) = (left.clone(), right.clone());
        std::thread::spawn(move || {
            let cfg = SyncConfig::new(mic_t.clone(), left_t.clone());
            let out = stereo_audio::calibrate_pair(&mic_t, &left_t, &right_t, &cfg)
                .map_err(|e| e.to_string());
            let _ = tx.send(out);
        });
        self.sync_rx = Some(rx);
        self.sync_busy = true;
        self.sync_status = if self.engine.is_some() {
            "正在测量（先试同时法、失败退回依次法，约 5 秒）… 提示：路由正在运行，若此刻正在放音乐请先「停止」，否则麦克风会同时收到音乐、测量会不准。".to_string()
        } else {
            "正在测量（先试同时法、失败退回依次法，约 5 秒）… 请保持环境安静。".to_string()
        };
        tracing::info!("[自校验] 开始标定：mic={mic} left={left} right={right}");
    }

    /// 收后台标定结果；成功后把建议延迟写进两条路由（运行中实时生效）。
    fn poll_sync(&mut self) {
        let Some(rx) = self.sync_rx.as_ref() else {
            return;
        };
        let received = rx.try_recv();
        match received {
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.sync_rx = None;
                self.sync_busy = false;
                self.sync_status = "标定线程异常退出。".to_string();
                return;
            }
            Ok(Ok(result)) => {
                self.sync_rx = None;
                self.sync_busy = false;
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
            Ok(Err(e)) => {
                self.sync_rx = None;
                self.sync_busy = false;
                self.sync_status = format!("测量失败：{e}");
                tracing::warn!("[自校验] 测量失败：{e}");
            }
        }
    }

    /// 手动对齐：按当前左右延迟值同时向两台音箱放点击串，人耳听是否重合成一下。
    fn play_alignment(&mut self) {
        if self.clicks_busy {
            return;
        }
        let left = self.cfg.split.left_endpoint.clone();
        let right = self.cfg.split.right_endpoint.clone();
        let (l, r) = (
            self.cfg.split.left_delay_ms,
            self.cfg.split.right_delay_ms,
        );
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
                    tone: if kind.is_virtual() { Tone::Ok } else { Tone::Warn },
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
            for route in &stats.routes {
                if let Some(err) = &route.error {
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
            if hints.iter().all(|h| h.tone != Tone::Err) {
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
                    ui.add_space(4.0);

                    if self.live_default_is_capture {
                        pill(ui, "默认输出 = 采集源", ACCENT, 160.0);
                    } else {
                        pill(ui, "默认输出未切换", WARN, 160.0);
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

    fn side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("side")
            .exact_width(384.0)
            .resizable(false)
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(16, 14))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("side_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.capture_card(ui);
                        ui.add_space(12.0);
                        self.diagnostics_card(ui);
                        ui.add_space(12.0);
                        self.about_card(ui);
                    });
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

    fn diagnostics_card(&mut self, ui: &mut egui::Ui) {
        let hints = self.hints();
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(178.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("诊断").size(15.0).strong());
                help(
                    ui,
                    "这里只列会影响出声的问题：\n\
                     · 采集源 / 播放设备是否就绪；\n\
                     · 路由是否在跑、两条链路有没有错误或欠载；\n\
                     · Windows 默认输出是不是采集源。",
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .small_button("复制")
                        .on_hover_text("复制全部诊断信息到剪贴板")
                        .clicked()
                    {
                        let text = hints
                            .iter()
                            .map(|h| format!("{} {}", h.icon(), h.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.ctx().copy_text(text);
                        self.toast("诊断信息已复制");
                    }
                });
            });
            ui.add_space(4.0);
            // 固定槽位：诊断条数变化不改变卡片高度。
            let width = ui.available_width().max(120.0);
            for i in 0..DIAG_SLOTS {
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::hover());
                let Some(hint) = hints.get(i) else {
                    continue;
                };
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                {
                    let painter = ui.painter();
                    painter.text(
                        rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        hint.icon(),
                        FontId::monospace(11.5),
                        hint.color(),
                    );
                    let galley = fit_one_line(
                        ui,
                        &hint.text,
                        &FontId::proportional(12.5),
                        TEXT,
                        (rect.width() - 18.0).max(40.0),
                    );
                    painter.galley(
                        egui::pos2(rect.left() + 18.0, rect.center().y - galley.size().y * 0.5),
                        galley,
                        TEXT,
                    );
                }
                response.on_hover_text(hint.text.clone());
            }
        });
    }

    fn about_card(&mut self, ui: &mut egui::Ui) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(118.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("怎么用").size(15.0).strong());
                help(
                    ui,
                    "1. 为左、右声道各选一台蓝牙音箱（先在本机蓝牙里连上）。\n\
                     2. 点「开始路由」，面板会把默认输出切到你选的采集源。\n\
                     3. 用播放器放歌/看视频，声音就被拆成左右两路。\n\
                     4. 退出面板会自动停路由并恢复原来的默认输出。\n\n\
                     软件侧延迟 ≈ 20 ms 采集缓冲 + 抖动缓冲 + 渲染缓冲 / 2；\n\
                     蓝牙链路本身的延迟另外算（约 100–200 ms）。",
                );
            });
            ui.add_space(6.0);
            let latency = self
                .route_engine_config()
                .map(|c| c.software_latency_ms())
                .unwrap_or(0);
            let line = format!(
                "软件侧延迟约 {latency} ms（抖动缓冲 {} ms + 渲染缓冲 40 ms）",
                self.cfg.split.queue_ms
            );
            let width = ui.available_width().max(120.0);
            let tip = line.clone();
            text_cell(ui, width, 18.0, &line, FontId::proportional(12.0), MUTED, false)
                .on_hover_text(tip);
            ui.add_space(4.0);
            if ui.checkbox(&mut self.cfg.show_log, "显示日志面板").changed() {
                self.cfg.save();
            }
            let (font_line, font_tip, font_color) = match &self.font {
                Some(path) => {
                    let file = std::path::Path::new(path)
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    let present = fonts::candidates_present();
                    (
                        format!("字体：{file}"),
                        format!("{path}\n\n候选字体：\n{}", present.join("\n")),
                        MUTED,
                    )
                }
                None => (
                    "没找到中文字体：中文可能显示为方块".to_string(),
                    "系统字体目录里没有候选字体，可用 STEREOLINK_FONT 指定".to_string(),
                    WARN,
                ),
            };
            let width = ui.available_width().max(120.0);
            text_cell(
                ui,
                width,
                18.0,
                &font_line,
                FontId::proportional(12.0),
                font_color,
                false,
            )
            .on_hover_text(font_tip);
        });
    }

    fn main_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(16, 14)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("main_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.routing_card(ui);
                        ui.add_space(12.0);
                        self.devices_card(ui);
                        if self.cfg.show_log {
                            ui.add_space(12.0);
                            self.log_card(ui);
                        }
                        ui.add_space(10.0);
                    });
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
                } else {
                    pill(ui, "未运行", Color32::from_rgb(120, 128, 142), 96.0);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.engine.is_some() {
                        if ui.button("停止").clicked() {
                            self.stop_routing(true);
                            self.toast("已停止路由并恢复默认输出");
                        }
                        if ui.button("重启").clicked() {
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
                    if ui.button("左右自检音").clicked() {
                        self.play_test_tone();
                    }
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
                    text_cell(ui, ROUTE_COL_CH, 18.0, "声道", header_font.clone(), MUTED, false);
                    text_cell(ui, device_w, 18.0, "设备", header_font.clone(), MUTED, false);
                    text_cell(ui, ROUTE_COL_LEVEL, 18.0, "电平", header_font.clone(), MUTED, false);
                    text_cell(ui, ROUTE_COL_SENT, 18.0, "已发流", header_font.clone(), MUTED, true);
                    text_cell(ui, ROUTE_COL_BUF, 18.0, "缓冲", header_font.clone(), MUTED, true);
                    text_cell(ui, ROUTE_COL_UNDER, 18.0, "欠载", header_font.clone(), MUTED, true)
                        .on_hover_text(UNDERRUN_HELP);
                    text_cell(ui, ROUTE_COL_PATH, 18.0, "路径", header_font, MUTED, false);
                });
            }
            ui.add_space(2.0);
            for channel in [RouteChannel::Left, RouteChannel::Right] {
                self.route_row(ui, channel, device_w);
            }

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(8.0);

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

            ui.collapsing("高级：左右微调延迟", |ui| {
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
                    "用麦克风量出两台音箱的真实到达时间差，自动给较慢的一路垫延迟。\n\
                     · 先选一个麦克风，「体检」确认它真能采到声音；\n\
                     · 「测量并对齐」：先试同时法（精度约 ±1 ms），失败退回依次法（约 ±4 ms）；\n\
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
                cell(ui, 240.0, 26.0, Layout::left_to_right(Align::Center), |ui| {
                    egui::ComboBox::from_id_salt("sync_mic")
                        .width(236.0)
                        .selected_text(if mic_label.is_empty() {
                            RichText::new("（未选择麦克风）").color(MUTED)
                        } else {
                            RichText::new(clip_text(&short_endpoint(&mic_label), 26)).color(TEXT)
                        })
                        .show_ui(ui, |ui| {
                            for m in &self.sync_mics {
                                let id = m.id().to_string();
                                let text = format!(
                                    "{} [{}]{}",
                                    short_endpoint(m.name()),
                                    m.kind.as_str(),
                                    if m.likely_usable { "" } else { " · 可能不可用" }
                                );
                                if ui.selectable_label(self.sync_mic == id, text).clicked() {
                                    pick_mic = Some(id);
                                }
                            }
                        });
                });
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
                            "测量中…"
                        } else {
                            "测量并对齐"
                        }),
                    )
                    .on_hover_text("向两台音箱依次放扫频声，用麦克风算出真实时差，自动垫延迟")
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
            text_cell(ui, width, 20.0, &status, FontId::proportional(12.0), MUTED, false)
                .on_hover_text(status);

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
                text_cell(ui, rest, 20.0, &source_line, FontId::proportional(12.0), MUTED, false)
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

            // 数值列：列宽写死 + 右对齐 + monospace，数值跳动不会改变列宽。
            let sent = route
                .as_ref()
                .map(|route| format!("{:>7.2} MB", route.sent_bytes as f64 / 1_048_576.0))
                .unwrap_or_else(|| "      —".to_string());
            text_cell(
                ui,
                ROUTE_COL_SENT,
                ROUTE_ROW_H,
                &sent,
                FontId::monospace(12.0),
                TEXT,
                true,
            );

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

            let underruns = route.as_ref().map(|route| route.underruns).unwrap_or(0);
            let under_text = route
                .as_ref()
                .map(|route| format!("{:>4}", route.underruns))
                .unwrap_or_else(|| "   —".to_string());
            let under_tip = match route.as_ref() {
                Some(route) if route.underruns > 0 => format!(
                    "这一路已欠载 {} 次。\n{}",
                    route.underruns,
                    if route.underruns > 200 {
                        "次数偏多：建议把「抖动缓冲」调到 80～120 ms（代价是延迟增加）。"
                    } else {
                        "少量欠载（尤其刚开始路由时的头几次）属正常。"
                    }
                ),
                _ => UNDERRUN_HELP.to_string(),
            };
            text_cell(
                ui,
                ROUTE_COL_UNDER,
                ROUTE_ROW_H,
                &under_text,
                FontId::monospace(12.0),
                if underruns > 0 { WARN } else { TEXT },
                true,
            )
            .on_hover_text(under_tip);

            let (path_short, path_tip) = match route
                .as_ref()
                .and_then(|route| route.format_path.as_deref())
            {
                Some(path) => (
                    if path.contains("autoconvert") {
                        "Auto"
                    } else {
                        "转换"
                    },
                    format!("开流路径：{path}"),
                ),
                None => ("—", "开流路径：尚未开流".to_string()),
            };
            text_cell(
                ui,
                ROUTE_COL_PATH,
                ROUTE_ROW_H,
                path_short,
                FontId::proportional(12.0),
                MUTED,
                false,
            )
            .on_hover_text(path_tip);
        });

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
                                            t.clone(),
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
        let mut clear = false;
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
                });
            });
            ui.add_space(4.0);
            egui::Frame::default()
                .fill(Color32::from_rgb(13, 16, 21))
                .inner_margin(Margin::symmetric(10, 8))
                .corner_radius(CornerRadius::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("log_scroll")
                        .max_height(230.0)
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for line in &lines {
                                log_line(ui, line);
                            }
                        });
                });
        });
        if clear {
            self.log.clear();
            self.toast("日志已清空");
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
        self.tick();
        self.header(ctx);
        self.side_panel(ctx);
        self.main_panel(ctx);
        ctx.request_repaint_after(Duration::from_millis(200));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_endpoint_strips_speaker_prefix() {
        assert_eq!(
            short_endpoint("扬声器 (Xiaomi Sound-1695)"),
            "Xiaomi Sound-1695"
        );
        assert_eq!(short_endpoint("Xiaomi Sound-5320"), "Xiaomi Sound-5320");
        assert_eq!(short_endpoint(" 耳机 (Chu2 DSP) "), "Chu2 DSP");
    }

    #[test]
    fn channel_colors_are_distinct() {
        assert_ne!(
            channel_color(RouteChannel::Left),
            channel_color(RouteChannel::Right)
        );
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
        assert_eq!(capture_selector_for(&virtual_card, &renders), virtual_card.name);
    }
}
