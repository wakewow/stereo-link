//! StereoLink 立体声链 —— 桌面面板。
//!
//! 界面按「左侧可收起侧边栏 + 五个页面」组织：
//! - 侧边栏：导航（声道路由 / 采集源 / 本机播放设备 / 手机投放 / 日志，可收起成图标条）
//!   + 底部身份、运行状态徽章、主题切换、系统声音设置；
//! - 声道路由页：操作按钮（开始 / 停止 / 重启路由、左右自检音）、三项诊断徽章、
//!   路由表（每路音量条 + 反相开关）、高级（增益 / 抖动缓冲 / 左右微调延迟）、声学自校验；
//! - 采集源页：采集端点选择、默认输出联动、采集电平与音量；
//! - 本机播放设备页：设备表（格式下拉、探测、刷新设备）；
//! - 手机投放页：AirPlay / 小米妙播接收端的开关与状态（设备名固定 StereoLink，
//!   声音直接进 Windows 当前默认输出声卡）；
//! - 日志页：运行日志（自动滚动、复制诊断、复制全部、清空）。
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
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui::{
    Align, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2,
};
use stereo_audio::{
    BalanceCalibration, BalanceCalibrationOutcome, BalanceReport, DataFlow, EffectSettings,
    EndpointFormat, EndpointInfo, EndpointVolume, MicCandidate, MicHealth, PcmFormat, RouteChannel,
    RouteConfig, RouteEngineConfig, RouteEngineHandle, RouteEngineStats, RouteStats, SyncConfig,
    SyncMethod, SyncResult, BALANCE_ANCHOR_SCALARS, BALANCE_GAIN_DEADBAND_DB,
};

use crate::audio_default;
use crate::close_flow::{self, CloseOutcome, CloseState};
use crate::config::{AppConfig, BalanceCalConfig, EffectPresetConfig, ThemeMode, VolumeFollow};
use crate::control::{self, ControlServer};
use crate::endpoint_poll::{EndpointPoll, PollInput};
use crate::fonts;
use crate::logbuf::{LogBuffer, LogLine};
use crate::receiver::{CastKind, CastStatus, ReceiverManager};
use crate::theme::{self, GlassLevel};
use crate::tray::{Tray, TrayEvent};

// ---------------------------------------------------------------------------
// 主题：浅色 / 深色两套调色板（跟随系统或手动切换）
// ---------------------------------------------------------------------------
//
// 之前颜色是一组 `const`，只支持深色。现在改成「全局当前深浅 + `palette()` 取色」：
// 界面里所有颜色都在运行时从调色板取，切换主题只要下一帧重画。
// 深浅状态由 `update()` 每帧按 `ctx.theme()`（已由 eframe 按系统/用户偏好解析）写入。

/// 当前是否深色（由 `update()` 每帧更新；纯读，无锁）。
static DARK_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// 写入当前主题深浅（只在 `update()` 里调用）。
fn set_dark_mode(dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
}

/// 当前是否深色主题。
fn is_dark() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

/// 让原生标题栏跟随应用主题。
///
/// Windows 11 上 winit 会把它翻成 `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`；
/// 不设的话，浅色界面会顶着一条系统深色标题栏——窗口上沿和下方 UI 风格割裂。
fn sync_window_theme(ctx: &egui::Context, dark: bool) {
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(if dark {
        egui::SystemTheme::Dark
    } else {
        egui::SystemTheme::Light
    }));
}

/// 一套调色板：深浅主题各一份，颜色语义与旧常量一一对应。
#[derive(Debug, Clone, Copy)]
struct Palette {
    /// 是否深色（少数需要按主题调透明度的绘制用）。
    dark: bool,
    bg: Color32,
    panel: Color32,
    card: Color32,
    card_soft: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
    accent2: Color32,
    ok: Color32,
    warn: Color32,
    err: Color32,
    /// 列表行 hover 底色。
    hover: Color32,
    /// 左上角 logo 方块底色。
    icon_tile: Color32,
    /// 日志面板深色底。
    log_bg: Color32,
    /// 极端背景（输入框/滚动条槽）。
    extreme_bg: Color32,
    /// 普通控件底色。
    widget_inactive: Color32,
    /// 普通控件弱底色。
    widget_weak: Color32,
    /// 悬停控件底色。
    widget_hovered: Color32,
    /// 悬停控件弱底色。
    widget_hover_weak: Color32,
}

/// 深色调色板（与旧版视觉一致）。
const DARK: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(11, 13, 17),
    panel: Color32::from_rgb(18, 21, 27),
    card: Color32::from_rgb(22, 26, 33),
    card_soft: Color32::from_rgb(27, 32, 40),
    border: Color32::from_rgb(38, 44, 54),
    text: Color32::from_rgb(232, 236, 244),
    muted: Color32::from_rgb(132, 142, 158),
    accent: Color32::from_rgb(56, 189, 248),   // 左声道：青
    accent2: Color32::from_rgb(167, 139, 250), // 右声道：紫
    ok: Color32::from_rgb(74, 222, 128),
    warn: Color32::from_rgb(251, 191, 36),
    err: Color32::from_rgb(248, 113, 113),
    hover: Color32::from_rgb(26, 31, 39),
    icon_tile: Color32::from_rgb(23, 30, 40),
    log_bg: Color32::from_rgb(13, 16, 21),
    extreme_bg: Color32::from_rgb(28, 32, 40),
    widget_inactive: Color32::from_rgb(35, 41, 51),
    widget_weak: Color32::from_rgb(31, 36, 45),
    widget_hovered: Color32::from_rgb(46, 54, 67),
    widget_hover_weak: Color32::from_rgb(42, 49, 61),
};

/// 浅色调色板：白底、深字、同色相强调色（保证对比度够、没有白字白底）。
const LIGHT: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(240, 244, 249),
    panel: Color32::from_rgb(250, 252, 255),
    card: Color32::from_rgb(255, 255, 255),
    card_soft: Color32::from_rgb(233, 239, 246),
    border: Color32::from_rgb(203, 212, 224),
    text: Color32::from_rgb(23, 32, 45),
    muted: Color32::from_rgb(90, 104, 121),
    accent: Color32::from_rgb(2, 132, 199),   // 左声道：深青
    accent2: Color32::from_rgb(109, 40, 217), // 右声道：深紫
    ok: Color32::from_rgb(21, 128, 61),
    warn: Color32::from_rgb(180, 83, 9),
    err: Color32::from_rgb(185, 28, 28),
    hover: Color32::from_rgb(226, 233, 242),
    icon_tile: Color32::from_rgb(226, 233, 242),
    log_bg: Color32::from_rgb(246, 249, 252),
    extreme_bg: Color32::from_rgb(255, 255, 255),
    widget_inactive: Color32::from_rgb(228, 234, 242),
    widget_weak: Color32::from_rgb(236, 241, 247),
    widget_hovered: Color32::from_rgb(213, 223, 236),
    widget_hover_weak: Color32::from_rgb(222, 229, 238),
};

/// 取当前主题的调色板（深浅由 `update()` 每帧写好）。
fn palette() -> Palette {
    if is_dark() {
        DARK
    } else {
        LIGHT
    }
}

/// 在彩色底上选可读的文字颜色（浅底用黑、深底用白）。
fn text_on(bg: Color32) -> Color32 {
    let lum = 0.299 * bg.r() as f32 + 0.587 * bg.g() as f32 + 0.114 * bg.b() as f32;
    if lum > 120.0 {
        Color32::BLACK
    } else {
        Color32::WHITE
    }
}

/// 卡片外框：玻璃拟态（半透明填充 + 1px 描边 + 阴影，见 `theme::glass_frame`）。
///
/// egui 没有 `backdrop-filter`，毛玻璃靠「底层柔光纹理 + 半透明卡片 + 描边高光」
/// 三层近似，所以这里必须是半透明填充，不能退回不透明色。
fn card_frame() -> egui::Frame {
    theme::glass_frame(GlassLevel::Base, is_dark())
}

/// 玻璃卡片：`card_frame().show(..)` 的薄包装，调用点写法与它完全一样。
fn card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    card_frame().show(ui, add_contents).inner
}

/// 统一按钮（自动带当前主题）。主操作 `Primary`、次操作 `Secondary`、
/// 轻量操作 `Ghost`、停止/破坏性 `Danger`。
fn btn(ui: &mut egui::Ui, label: &str, kind: theme::ButtonKind) -> egui::Response {
    theme::button(ui, label, kind, is_dark())
}

/// 带禁用态的按钮：禁用时置灰但不改变布局水位。
fn btn_enabled(
    ui: &mut egui::Ui,
    enabled: bool,
    label: &str,
    kind: theme::ButtonKind,
) -> egui::Response {
    theme::button_sized(
        ui,
        enabled,
        label,
        kind,
        theme::ButtonSize::Normal,
        is_dark(),
    )
}

/// 紧凑按钮：表格行、内嵌工具栏用。
fn btn_small(
    ui: &mut egui::Ui,
    enabled: bool,
    label: &str,
    kind: theme::ButtonKind,
) -> egui::Response {
    theme::button_sized(
        ui,
        enabled,
        label,
        kind,
        theme::ButtonSize::Small,
        is_dark(),
    )
}

/// 方形图标按钮（侧边栏收起、主题快切、声音设置这类轻量入口）。
/// 点击区域 26×26 比图形本身大一圈；悬停浮出一层极淡的底色，不画描边、不抢视线。
fn icon_button(
    ui: &mut egui::Ui,
    icon: crate::icons::Icon,
    tint: Color32,
    tip: &str,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    // 即时反馈，不做淡入：界面里没有随时间变化的动效。
    if resp.hovered() {
        let fill = if is_dark() {
            Color32::from_white_alpha(11)
        } else {
            Color32::from_black_alpha(9)
        };
        ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
    }
    crate::icons::paint(
        &ui.painter().with_clip_rect(rect),
        rect.shrink(6.5),
        icon,
        if resp.hovered() { palette().text } else { tint },
        1.45,
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, icon.label()));
    resp.on_hover_text(tip)
}

/// 固定宽度的药丸标签：宽度写死，内容居中，不随文字伸缩。
fn pill(ui: &mut egui::Ui, text: &str, bg: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), bg);
    let fg = text_on(bg);
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(12.0), fg);
    let pos = egui::pos2(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    painter.galley(pos, galley, fg);
}

/// 小圆圈「i」：hover 才显示完整说明（说明不再常驻占位）。
fn help(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.circle_stroke(
            rect.center(),
            8.0,
            Stroke::new(1.0_f32, palette().accent.gamma_multiply(0.8)),
        );
        crate::icons::paint(
            painter,
            rect.shrink(4.5),
            crate::icons::Icon::Info,
            palette().accent,
            1.3,
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
/// 行内「波形 + 频谱」装饰列（真实电平驱动，见 `theme::WaveBuf`）。
const ROUTE_COL_WAVE: f32 = 112.0;
const ROUTE_COL_LEVEL: f32 = 92.0;
/// 每路「实机输出」音量条：占用原先「已发流 / 欠载」两列的位置。
///
/// 显示和拖动都按「实机输出」= 端点音量 + 引擎补偿（dB 域相加）算，见
/// StereoLinkApp::actual_output_scalar。
const ROUTE_COL_VOL: f32 = 150.0;
const ROUTE_COL_BUF: f32 = 72.0;
const ROUTE_COL_PATH: f32 = 44.0;
/// 反相开关列：从「高级」里移出来，每路都直接可见。
const ROUTE_COL_INV: f32 = 96.0;
/// 行高：给行内波形/频谱留出呼吸（原型 44~48px）。
const ROUTE_ROW_H: f32 = 40.0;

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
        + ROUTE_COL_WAVE
        + ROUTE_COL_LEVEL
        + ROUTE_COL_VOL
        + ROUTE_COL_BUF
        + ROUTE_COL_PATH
        + ROUTE_COL_INV;
    (available - fixed - spacing * 8.0).max(150.0)
}

/// 描边小徽标。
fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let font = FontId::proportional(11.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let pad = Vec2::new(7.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), palette().card_soft);
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, color.gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.min + pad, galley, color);
    response
}

/// 电平条已被 `theme::segmented_level`（分段电平条 + 峰值保持线）取代，
/// 见声道路由表的「电平」列。此处不再保留旧实现，避免死代码。
fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(palette().muted)
}

/// 诊断条里的小状态点：绿 = 正常，黄 = 注意。
fn status_dot(ui: &mut egui::Ui, ok: bool, ok_text: &str, bad_text: &str, tip: &str) {
    let (text, color, icon) = if ok {
        (ok_text, palette().ok, crate::icons::Icon::Check)
    } else {
        (bad_text, palette().warn, crate::icons::Icon::Cross)
    };
    ui.add_space(6.0);
    let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    crate::icons::paint(ui.painter(), icon_rect.shrink(1.0), icon, color, 1.7);
    ui.add_space(3.0);
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(color)).selectable(false))
        .on_hover_text(tip);
    ui.add_space(10.0);
}

/// 画应用图标：深色圆角底 + 两条竖条（左声道 / 右声道）。
fn paint_logo(ui: &egui::Ui, rect: Rect) {
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), palette().icon_tile);
    painter.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0_f32, palette().border),
        egui::StrokeKind::Inside,
    );
    painter.rect_filled(
        Rect::from_min_size(rect.min + Vec2::new(8.0, 11.0), Vec2::new(6.0, 13.0)),
        CornerRadius::same(2),
        palette().accent,
    );
    painter.rect_filled(
        Rect::from_min_size(rect.min + Vec2::new(19.0, 8.0), Vec2::new(6.0, 16.0)),
        CornerRadius::same(2),
        palette().accent2,
    );
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
        RouteChannel::Left => palette().accent,
        RouteChannel::Right => palette().accent2,
        RouteChannel::Both => palette().ok,
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
            CaptureKind::Virtual => palette().ok,
            CaptureKind::Bluetooth => palette().warn,
            CaptureKind::Physical => palette().muted,
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
    /// 复制到剪贴板时用的纯文本标记：图标是画出来的，没法进文本。
    fn glyph(&self) -> &'static str {
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

/// 左侧导航的五个页面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    /// 声道路由（主页）：左右设备、每路音量、反相、校准与音量平衡、高级。
    Routing,
    /// 采集源：端点选择、默认输出切换、系统音量联动、采集源音量诊断、测试音。
    Capture,
    /// 本机播放设备：格式 / 角色 / 设为左 / 设为右。
    Devices,
    /// 手机投放：AirPlay / 小米妙播接收端开关（声音直接进本机默认声卡）。
    Cast,
    /// 日志：运行日志（默认关闭，只在点进来时渲染）。
    Log,
}

impl Page {
    /// 导航栏里的全部页面（顺序即显示顺序）。
    const ALL: [Page; 5] = [
        Page::Routing,
        Page::Capture,
        Page::Devices,
        Page::Cast,
        Page::Log,
    ];

    /// 导航栏里的中文标题。
    fn label(self) -> &'static str {
        match self {
            Page::Routing => "声道路由",
            Page::Capture => "采集源",
            Page::Devices => "本机播放设备",
            Page::Cast => "手机投放",
            Page::Log => "日志",
        }
    }

    /// 导航栏图标：手绘线性图标（见 `crate::icons`），不再用中文字代替。
    fn icon(self) -> crate::icons::Icon {
        match self {
            Page::Routing => crate::icons::Icon::Route,
            Page::Capture => crate::icons::Icon::Capture,
            Page::Devices => crate::icons::Icon::Devices,
            Page::Cast => crate::icons::Icon::Cast,
            Page::Log => crate::icons::Icon::Log,
        }
    }
}

/// 后台校准线程的返回值：延迟对齐 + 音量平衡校准（平衡失败不拖累对齐结果）。
struct CalibrateOutcome {
    sync: Result<SyncResult, String>,
    calibration: Result<BalanceCalibrationOutcome, String>,
}

pub struct StereoLinkApp {
    cfg: AppConfig,
    engine: Option<RouteEngineHandle>,
    stats: Option<RouteEngineStats>,
    endpoints: Vec<EndpointInfo>,
    endpoints_error: Option<String>,
    log: LogBuffer,
    /// 右下角浮层提示：真正的 toast（淡入淡出、可叠 3 条），取代原来那格固定宽度文字。
    toasts: crate::toast::Toasts,
    /// 本帧要弹出的提示文案：`toast()` 在任意时刻都能调，累到 `update()` 里再入队。
    pending_toast: Option<String>,
    font: Option<String>,
    /// 默认输出（每 ~1s 实时探测；引擎快照里的值只是启动时的静态值）。
    live_default_output: Option<String>,
    /// 默认输出的端点 id：WASAPI 里好几块声卡都叫「扬声器」，判断「有没有换声卡」得看 id。
    live_default_output_id: Option<String>,
    /// 当前 Windows 默认输出是不是采集端点（不是就采不到声）。
    live_default_is_capture: bool,
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
    /// 后台体检线程的结果通道（麦克风能不能采到声音；`Err` 只用于参数非法）。
    check_rx: Option<std::sync::mpsc::Receiver<Result<MicHealth, String>>>,
    /// 最近一次自校验结果。
    sync_result: Option<SyncResult>,
    /// 手动对齐点击串正在播。
    clicks_busy: bool,
    /// 点击串开始时间（用于自动解除忙碌标记）。
    clicks_at: Option<Instant>,
    /// 音量联动：最近一次读到的采集源音量（由后台轮询线程提供）。
    volume_source: Option<EndpointVolume>,
    /// 音量联动失败原因（后台线程写，UI 只读）。
    volume_error: Option<String>,
    /// 后台端点轮询线程：所有周期性 WASAPI/COM 调用都在它里面。
    poll: EndpointPoll,
    /// 上次应用的采集源音量变化计数（避免重复处理同一份快照）。
    source_volume_revision: u64,
    /// 最近一次应用的界面深浅（主题切换时重设样式）。
    theme_dark: bool,
    /// 最近一次下发给系统的窗口主题；`None` 表示还没发过（首帧补发）。
    window_theme: Option<bool>,
    /// 帧耗时日志级别：0=关（默认）；1=只打 >16 ms 的慢帧；2=每帧都打（实测用）。
    /// 环境变量 `STEREOLINK_FRAME_LOG` 只在启动时读一次，平时零开销。
    frame_log: u8,
    /// 上一帧总耗时（ms），帧日志用。
    last_frame_ms: f32,
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
    /// 当前生效的音量平衡校准（启动时从配置加载；左右端点换了就作废）。
    balance_cal: BalanceCalibration,
    /// 是否正在做音量平衡校准：期间暂停音量跟随，免得跟随线程把校准锚点音量覆盖掉。
    calibrating: bool,
    /// 上一次写进引擎的平衡补偿（dB，左/右），用来避免每帧重复写引擎原子量。
    last_balance_gains: [f32; 2],
    /// 上一次写进引擎的「数字静音」状态：系统静音时让引擎直接把输出推 0。
    ///
    /// 端点静音靠不住：实测 Xiaomi Sound（蓝牙声卡）上 `SetMute(true)` 会被驱动折算成
    /// 「把端点音量压到最低可用档（约 8%）」并立刻读回「未静音」，物理音箱仍会漏出小声音。
    /// 所以系统静音时同时在引擎侧推 0（精确数字静音），不依赖端点/驱动行为。
    last_engine_muted: bool,
    /// 上一次写进引擎的音效设置（含调音预设）：和配置比较，变了才写引擎原子量。
    last_effect: EffectSettings,
    /// 日志页是否自动滚动到最新一行。
    log_autoscroll: bool,
    /// 左声道行内波形/频谱（幅度来自真实 `RouteStats.level`）。
    wave_l: theme::WaveBuf,
    /// 右声道行内波形/频谱。
    wave_r: theme::WaveBuf,
    /// 行内波形是否被点停：`[左, 右]`。暂停只是定格动画，不影响音频与电平表。
    wave_paused: [bool; 2],
    /// 上一帧时间：给波形/频谱算 dt。
    last_frame_at: Instant,
    /// 临时诊断开关（`STEREOLINK_LEVEL_LOG=1`）：把引擎上报的电平周期性写进日志。
    level_log: bool,
    /// 上一次电平诊断日志的时间。
    last_level_log: Option<Instant>,
    /// 手机投放接收端（AirPlay / 小米妙播）：只管子进程，不碰音频数据。
    cast: ReceiverManager,
    /// 「妙播音量」滑杆最近一次设置失败的原因（成功后清空），显示在该行下面。
    cast_volume_error: Option<String>,
    /// 命令行控制面：`stereo-link-cli` 靠它读状态、发命令（开发规范：不用点按钮测试）。
    control: Option<ControlServer>,
    /// 启动时刻（`status` 里的 uptime）。
    started_at: Instant,
    /// 命令行发了 `quit`：下一帧关窗口（走正常退出流程，保存配置、停引擎）。
    pending_quit: bool,
}

impl StereoLinkApp {
    pub fn new(cc: &eframe::CreationContext<'_>, log: LogBuffer) -> Self {
        let cfg = AppConfig::load();
        // 主题：配置里是 system 就交给 eframe 跟随系统（不设偏好），
        // light/dark 则明确覆盖。`ctx.theme()` 返回的是最终解析结果。
        if let Some(theme) = cfg.theme.preference() {
            cc.egui_ctx.set_theme(theme);
        }
        let dark = matches!(cc.egui_ctx.theme(), egui::Theme::Dark);
        set_dark_mode(dark);
        Self::setup_style(&cc.egui_ctx);

        let font = fonts::install(&cc.egui_ctx);
        match &font {
            Some(path) => tracing::info!("中文字体：{path}"),
            None => tracing::warn!("没找到可用的中文字体，中文可能显示为方块"),
        }

        let cfg_sync_mic = cfg.sync_mic.clone();
        // 配置里的平衡校准直接转成运行时可用的形式（端点没换就一直有效）。
        let cfg_balance = cfg.balance.to_calibration();
        // 后台轮询线程尽量先出第一份快照（最多等 800 ms），首帧就有设备列表。
        let poll = EndpointPoll::spawn(cc.egui_ctx.clone(), Duration::from_millis(800));
        let mut app = Self {
            cfg,
            engine: None,
            stats: None,
            endpoints: Vec::new(),
            endpoints_error: None,
            log,
            toasts: crate::toast::Toasts::new(),
            pending_toast: None,
            font,
            live_default_output: None,
            live_default_output_id: None,
            live_default_is_capture: false,
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
            check_rx: None,
            sync_result: None,
            clicks_busy: false,
            clicks_at: None,
            volume_source: None,
            volume_error: None,
            poll,
            source_volume_revision: 0,
            theme_dark: dark,
            window_theme: None,
            frame_log: match std::env::var("STEREOLINK_FRAME_LOG") {
                Ok(value) if value.trim() == "2" => 2,
                Ok(_) => 1,
                Err(_) => 0,
            },
            last_frame_ms: 0.0,
            page: Page::Routing,
            sidebar_open: true,
            close_state: CloseState::default(),
            tray: None,
            tray_attempted: false,
            window_hidden: false,
            route_volumes: std::collections::HashMap::new(),
            route_volume_at: std::collections::HashMap::new(),
            last_stats_log: None,
            last_stats_marks: Vec::new(),
            balance_result: None,
            balance_error: None,
            balance_status: String::new(),
            balance_cal: cfg_balance,
            calibrating: false,
            last_balance_gains: [0.0, 0.0],
            last_engine_muted: false,
            last_effect: EffectSettings::default(),
            log_autoscroll: true,
            wave_l: theme::WaveBuf::new(),
            wave_r: theme::WaveBuf::new(),
            wave_paused: [false, false],
            last_frame_at: Instant::now(),
            level_log: std::env::var_os("STEREOLINK_LEVEL_LOG").is_some(),
            last_level_log: None,
            cast: ReceiverManager::new(),
            cast_volume_error: None,
            control: None,
            started_at: Instant::now(),
            pending_quit: false,
        };
        // 首帧直接用第一份快照（触点、采集源、默认输出都在里面）。
        if !app.balance_cal.is_empty() {
            tracing::info!("[音量平衡] 已加载校准：{}", app.balance_cal.describe());
        }
        app.apply_endpoint_snapshot();
        app.refresh_mics();
        // 上次被强杀时子进程会留下（占着 5000/8899），先清掉再把新的拉起来。
        crate::receiver::kill_stale();
        // 手机投放：配置里勾了「随面板启动」就把对应接收端拉起来。
        // 低延迟开关要在自动拉起之前生效，否则第一次启动还是按默认 800 ms 起。
        app.cast.set_low_latency(app.cfg.cast.low_latency);
        for kind in CastKind::ALL {
            let auto = match kind {
                CastKind::AirPlay => app.cfg.cast.auto_airplay,
                CastKind::MiPlay => app.cfg.cast.auto_miplay,
            };
            if auto {
                let _ = app.cast.start(kind);
            }
        }
        tracing::info!("StereoLink 面板启动");
        // 命令行控制面：起在 127.0.0.1 的随机端口，地址写进 control.json。
        // 起不来（端口耗尽/权限异常）不影响界面，只是 CLI 连不上。
        match ControlServer::start("gui") {
            Ok(server) => {
                tracing::info!(
                    "控制面已就绪：127.0.0.1:{}（{}）",
                    server.info().port,
                    control::control_path().display()
                );
                app.control = Some(server);
            }
            Err(error) => tracing::warn!("控制面启动失败，命令行将连不上：{error}"),
        }
        app
    }

    // ---------------------------------------------------------------------
    // 命令行控制面：界面能做的动作这里都要有同名入口（docs/dev-conventions.md）
    // ---------------------------------------------------------------------

    /// 处理一条控制面请求，返回 `{ok, data|error}`。
    fn handle_control(&mut self, request: &serde_json::Value) -> serde_json::Value {
        let Some(cmd) = control::str_field(request, "cmd") else {
            return control::err("请求缺少 cmd 字段");
        };
        match cmd {
            "ping" => control::ok(serde_json::json!({
                "pong": true,
                "mode": "gui",
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
            })),
            "status" => control::ok(self.control_status_json()),
            "log" => {
                let lines = request
                    .get("lines")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(100)
                    .min(2000) as usize;
                let all = self.log.lines();
                let items: Vec<serde_json::Value> = all
                    .into_iter()
                    .rev()
                    .take(lines)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(|line| {
                        serde_json::json!({
                            "at_ms": line.at.as_millis() as u64,
                            "level": format!("{}", line.level),
                            "text": line.text,
                        })
                    })
                    .collect();
                control::ok(serde_json::json!({ "count": items.len(), "lines": items }))
            }
            "routing" => {
                let action = control::str_field(request, "action").unwrap_or("status");
                match action {
                    "status" => control::ok(serde_json::json!({
                        "running": self.engine.is_some(),
                        "engine": control::engine_json(self.stats.as_ref()),
                    })),
                    "start" => {
                        let started = self.start_routing(false);
                        if started {
                            control::ok(serde_json::json!({ "running": true }))
                        } else {
                            control::err("启动失败：还没有为左/右声道选好输出设备，或找不到采集源")
                        }
                    }
                    "stop" => {
                        self.stop_routing(true);
                        control::ok(serde_json::json!({ "running": false }))
                    }
                    "restart" => {
                        self.restart_routing();
                        control::ok(serde_json::json!({ "running": self.engine.is_some() }))
                    }
                    "mute" | "unmute" => {
                        let Some(engine) = &self.engine else {
                            return control::err("路由没在跑");
                        };
                        let muted = action == "mute";
                        if !engine.set_muted(muted) {
                            return control::err("切换静音失败（引擎可能已经停了）");
                        }
                        self.last_engine_muted = muted;
                        control::ok(serde_json::json!({ "muted": engine.muted() }))
                    }
                    other => control::err(format!("routing 不支持 action={other}")),
                }
            }
            "effect" => control::effect_command(&mut self.cfg, self.engine.as_ref(), request),
            "receiver" => {
                let action = control::str_field(request, "action").unwrap_or("list");
                let kind = control::str_field(request, "kind").unwrap_or("all");
                let kinds: Vec<CastKind> = if kind == "all" {
                    CastKind::ALL.to_vec()
                } else {
                    match kind {
                        "airplay" => vec![CastKind::AirPlay],
                        "miplay" => vec![CastKind::MiPlay],
                        other => {
                            return control::err(format!(
                                "kind 只支持 airplay / miplay / all，收到 {other}"
                            ));
                        }
                    }
                };
                match action {
                    "list" => control::ok(control::receivers_json(&self.cast)),
                    // 音量走 control.rs 的共用实现：serve 的 State::handle 调的是同一份代码。
                    "volume" => control::receiver_volume(&mut self.cast, kind, request),
                    "start" | "restart" => {
                        let mut errors = Vec::new();
                        for kind in kinds {
                            if action == "restart" {
                                self.cast.stop(kind);
                            }
                            if let Err(error) = self.cast.start(kind) {
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
            "endpoints" => control::ok(control::endpoints_json(&self.endpoints)),
            "config" => {
                let action = control::str_field(request, "action").unwrap_or("show");
                match action {
                    "path" => control::ok(serde_json::json!({
                        "path": AppConfig::path().display().to_string(),
                        "log_path": control::log_path().display().to_string(),
                    })),
                    "show" => control::ok(
                        serde_json::to_value(&self.cfg).unwrap_or(serde_json::Value::Null),
                    ),
                    "get" => {
                        let key = control::str_field(request, "key").unwrap_or("");
                        match control::config_get(&self.cfg, key) {
                            Ok(value) => {
                                control::ok(serde_json::json!({ "key": key, "value": value }))
                            }
                            Err(error) => control::err(error),
                        }
                    }
                    "set" => {
                        let key = control::str_field(request, "key").unwrap_or("");
                        let Some(value) = request.get("value") else {
                            return control::err("config set 需要 value 字段");
                        };
                        match control::config_set(&mut self.cfg, key, value.clone()) {
                            Ok(updated) => {
                                self.cfg.save();
                                self.cast.set_low_latency(self.cfg.cast.low_latency);
                                tracing::info!("[配置] {key} = {value}");
                                control::ok(
                                    serde_json::json!({ "updated": updated, "saved": true }),
                                )
                            }
                            Err(error) => control::err(error),
                        }
                    }
                    "reload" => {
                        self.cfg = AppConfig::load();
                        control::ok(serde_json::json!({ "reloaded": true }))
                    }
                    other => control::err(format!("config 不支持 action={other}")),
                }
            }
            "tone" => {
                let freq = request
                    .get("freq")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(440.0);
                let seconds = request
                    .get("seconds")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(1.0);
                let selector = match control::str_field(request, "device") {
                    Some(device) if !device.is_empty() && device != "default" => {
                        stereo_audio::DeviceSelector::name_contains(device)
                    }
                    _ => stereo_audio::DeviceSelector::Default,
                };
                match stereo_audio::play_sine(&selector, freq, seconds, 0.25) {
                    Ok(info) => control::ok(serde_json::json!({
                        "played": true,
                        "device": info.device_name,
                        "seconds": info.seconds,
                    })),
                    Err(error) => control::err(format!("播放测试音失败：{error}")),
                }
            }
            "doctor" => control::ok(crate::serve::doctor_json()),
            "quit" => {
                self.pending_quit = true;
                control::ok(serde_json::json!({ "stopping": true }))
            }
            other => control::err(format!("未知命令：{other}")),
        }
    }

    /// `status` 命令的 JSON（形状与无头模式共用，见 `control::status_json`）。
    fn control_status_json(&self) -> serde_json::Value {
        control::status_json(
            "gui",
            self.started_at.elapsed().as_secs_f64(),
            self.engine.is_some(),
            control::engine_json(self.stats.as_ref()),
            control::receivers_json(&self.cast),
            control::endpoints_json(&self.endpoints),
            control::config_summary(&self.cfg),
            serde_json::json!({
                "page": self.page.label(),
                "window_hidden": self.window_hidden,
                "live_default_output": self.live_default_output,
                "live_default_output_id": self.live_default_output_id,
                "log_path": control::log_path().display().to_string(),
            }),
        )
    }

    fn setup_style(ctx: &egui::Context) {
        let mut style = egui::Style::default();
        Self::apply_visuals_to(&mut style);
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

    /// 主题切换后重设视觉样式（保留间距/字号等不随主题变化的部分）。
    fn apply_visuals(ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        Self::apply_visuals_to(&mut style);
        ctx.set_style(style);
    }

    /// 按当前调色板生成 egui 视觉样式（深浅两套）。
    fn apply_visuals_to(style: &mut egui::Style) {
        let pal = palette();
        let mut visuals = if pal.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = pal.bg;
        visuals.window_fill = pal.panel;
        visuals.extreme_bg_color = pal.extreme_bg;
        visuals.faint_bg_color = pal.card_soft;
        visuals.override_text_color = Some(pal.text);
        visuals.selection.bg_fill = pal.accent.gamma_multiply(0.35);
        visuals.selection.stroke = Stroke::new(1.0_f32, pal.accent);
        for w in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }
        visuals.widgets.noninteractive.bg_fill = pal.card;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal.border);
        visuals.widgets.inactive.bg_fill = pal.widget_inactive;
        visuals.widgets.inactive.weak_bg_fill = pal.widget_weak;
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, pal.text);
        visuals.widgets.hovered.bg_fill = pal.widget_hovered;
        visuals.widgets.hovered.weak_bg_fill = pal.widget_hover_weak;
        visuals.widgets.hovered.fg_stroke =
            Stroke::new(1.0_f32, if pal.dark { Color32::WHITE } else { pal.text });
        visuals.widgets.active.bg_fill = pal.accent.gamma_multiply(0.28);
        visuals.widgets.active.weak_bg_fill = pal.accent.gamma_multiply(0.22);
        visuals.widgets.active.fg_stroke =
            Stroke::new(1.0_f32, if pal.dark { Color32::WHITE } else { pal.text });
        style.visuals = visuals;
        // 连带把 egui 自带的过渡也关掉：折叠区展开、下拉弹出、控件悬停/按下的
        // 渐变都按 `animation_time` 补间。设成 0 就全部变成瞬时切换，
        // 界面里再没有任何随时间变化的东西（只剩用户在看的波形/电平）。
        style.animation_time = 0.0;
    }

    // ---- 状态 ----

    /// 推进每路行内「波形 + 频谱」。
    ///
    /// 幅度取引擎上报的**真实**电平（`RouteStats.level`），包络在 `WaveBuf` 里做
    /// 快攻慢放；波形形状是合成信号（egui 侧拿不到原始 PCM），所以它反映的是
    /// 「这一路现在有多响」，不是真实波形。
    fn advance_waves(&mut self, dt: f32) {
        let l = self
            .route_of(RouteChannel::Left)
            .map(|r| r.level)
            .unwrap_or(0.0);
        let r = self
            .route_of(RouteChannel::Right)
            .map(|r| r.level)
            .unwrap_or(0.0);
        // 暂停的那一路不推进：波形会定格在最后一帧（点一下行内波形即可暂停）。
        if !self.wave_paused[0] {
            self.wave_l.advance(dt, l);
        }
        if !self.wave_paused[1] {
            self.wave_r.advance(dt, r);
        }
        if self.level_log
            && self
                .last_level_log
                .map(|t| t.elapsed() >= Duration::from_millis(500))
                .unwrap_or(true)
        {
            self.last_level_log = Some(Instant::now());
            let (frames, src, err) = self
                .stats
                .as_ref()
                .map(|s| {
                    (
                        s.captured_frames,
                        s.source_device.clone(),
                        s.source_error.clone(),
                    )
                })
                .unwrap_or((0, String::new(), None));
            tracing::info!(
                "[电平] 引擎 L {:.3} R {:.3} | 平滑 L {:.3} R {:.3} | 采集帧 {} | 源 {} | 采集错误 {:?} | 引擎 {}",
                l,
                r,
                self.wave_l.level(),
                self.wave_r.level(),
                frames,
                src,
                err,
                self.engine.is_some()
            );
        }
    }

    fn tick(&mut self) {
        self.poll_format_probe();
        self.poll_sync();
        self.poll_check();
        // 手机投放：转发接收端输出到日志、刷新存活状态，并让它们跟着系统默认声卡走。
        // 接收端打开的是 Windows 默认**输出**设备，所以这里用 render 方向上标了
        // `is_default` 的端点（`live_default_output` 走回环采集解析，可能是采集端点）。
        self.cast.poll();
        let default_output = match (&self.live_default_output_id, &self.live_default_output) {
            (Some(id), Some(name)) => Some((id.as_str(), name.as_str())),
            _ => None,
        };
        self.cast.sync_default_output(default_output);
        // 把「要看什么」推给后台轮询线程，再把它的最新快照应用到界面。
        self.sync_poll_input();
        self.apply_endpoint_snapshot();
        // 主音量变了就重新算平衡补偿（实时改引擎每路数字增益，不重启引擎）。
        self.refresh_balance_gains();
        // 音效开关/参数变了就热写进引擎（原子量，不重启、不断音）。
        self.refresh_effect();

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

    /// 把界面当前关心的一切推给后台轮询线程。
    ///
    /// **UI 线程不做任何周期性 WASAPI/COM 调用**：端点列表、设备格式、默认输出、
    /// 采集源音量、每路输出音量、音量跟随读/写全部由 `endpoint_poll` 线程完成。
    fn sync_poll_input(&self) {
        let capture_ep = self.capture_endpoint();
        let capture_selector = self.capture_selector().or_else(|| {
            capture_ep
                .as_ref()
                .map(|ep| capture_selector_for(ep, &self.render_devices()))
        });
        // 校准期间必须暂停跟随：校准会主动把两端写到手选的锚点音量，跟随线程若同时
        // 把采集源标量镜像回两端，锚点就不是我们以为的那个音量了，测出来的差会错。
        let follow_active =
            self.engine.is_some() && self.volume_follow_active() && !self.calibrating;
        let follow_targets = if follow_active {
            self.target_endpoints()
        } else {
            Vec::new()
        };
        self.poll.set_input(PollInput {
            capture_selector,
            capture_id: capture_ep.map(|ep| ep.id),
            route_selectors: [
                self.configured_endpoint(RouteChannel::Left).to_string(),
                self.configured_endpoint(RouteChannel::Right).to_string(),
            ],
            follow_active,
            follow_targets,
        });
    }

    /// 把后台线程的最新快照应用到界面状态（纯内存操作）。
    fn apply_endpoint_snapshot(&mut self) {
        let snap = self.poll.snapshot();

        // ---- 端点列表 ----
        if snap.endpoints_loaded {
            let list_changed = snap.endpoints.len() != self.endpoints.len()
                || snap
                    .endpoints
                    .iter()
                    .zip(self.endpoints.iter())
                    .any(|(a, b)| a.id != b.id);
            if list_changed {
                tracing::info!("[端点] 枚举到 {} 个端点", snap.endpoints.len());
            }
            self.endpoints = snap.endpoints;
            self.endpoints_error = snap.endpoints_error;
        }

        // ---- 设备默认格式 + 后台探测支持列表 ----
        if snap.formats_loaded {
            let mut cache = BTreeMap::new();
            for (id, fmt) in snap.formats {
                if let Some(f) = fmt {
                    self.format_choices.entry(id.clone()).or_insert(f);
                }
                cache.insert(id, fmt);
            }
            let newly_seen: Vec<String> = cache
                .keys()
                .filter(|id| {
                    !self.format_supported.contains_key(*id) && !self.format_probing.contains(*id)
                })
                .cloned()
                .collect();
            if cache != self.format_cache {
                self.format_cache = cache;
            }
            self.format_choices
                .retain(|id, _| self.format_cache.contains_key(id));
            if !newly_seen.is_empty() {
                self.spawn_format_probe(newly_seen);
            }
        }

        // ---- 默认输出 ----
        self.live_default_output = snap.default_output.as_ref().map(|ep| ep.name.clone());
        self.live_default_output_id = snap.default_output.as_ref().map(|ep| ep.id.clone());
        self.live_default_is_capture = snap.default_is_capture;
        // 记住最后一次见到的「真实」默认输出：即使启动时默认已经是采集端点
        // （上次被强杀没来得及恢复），退出时也能切回去。
        if let Some(default) = &snap.default_output {
            if !snap.default_is_capture
                && self.cfg.capture.last_real_output.as_deref() != Some(default.id.as_str())
            {
                self.cfg.capture.last_real_output = Some(default.id.clone());
                self.save();
            }
        }

        // ---- 采集源音量（跟随失败原因也放这里） ----
        if self.volume_source.is_none() || snap.source_volume_revision > self.source_volume_revision
        {
            self.source_volume_revision = snap.source_volume_revision;
            self.volume_source = snap.source_volume;
        }
        self.volume_error = snap.follow_error;

        // ---- 每路输出音量（设备回读；拖动后的 600 ms 内不覆盖用户输入） ----
        // 校准期间更不能覆盖：两端音量正被校准流程按锚点写入，界面显示以校准为准。
        for (index, channel) in [RouteChannel::Left, RouteChannel::Right]
            .into_iter()
            .enumerate()
        {
            if self.calibrating {
                continue;
            }
            let Some(scalar) = snap.route_volumes[index] else {
                continue;
            };
            let just_dragged = self
                .route_volume_at
                .get(&channel)
                .map(|t| t.elapsed() < Duration::from_millis(600))
                .unwrap_or(false);
            if !just_dragged {
                self.route_volumes.insert(channel, scalar);
            }
        }
    }

    /// 主题跟随：`System` 模式下 egui 会随系统切换 `ctx.theme()`，这里检测变化后重设样式。
    fn sync_theme(&mut self, ctx: &egui::Context) {
        let dark = matches!(ctx.theme(), egui::Theme::Dark);
        set_dark_mode(dark);
        if self.window_theme != Some(dark) {
            self.window_theme = Some(dark);
            sync_window_theme(ctx, dark);
        }
        if dark != self.theme_dark {
            self.theme_dark = dark;
            Self::apply_visuals(ctx);
            ctx.request_repaint();
        }
    }

    /// 「刷新设备」：请求后台线程重新枚举端点 / 设备默认格式 / 默认输出。
    ///
    /// 真正的 COM 调用在 `endpoint_poll` 线程里做（UI 线程不阻塞）。
    fn refresh_endpoints(&mut self) {
        self.poll.request_refresh();
    }

    /// 当前选中的采集端点：优先配置里记住的 id，解析不到再回退到按虚拟声卡名字提示自动挑。
    ///
    /// 名字可以被用户改、端点也可能被移除，所以优先认 id；id 找不到时按
    /// [`audio_default`] 的提示顺序挑一块虚拟声卡（自研驱动 → UU远程 → Steam）。
    ///
    /// 这里只读 UI 线程持有的快照（后台线程枚举），UI 线程不做任何 COM 调用；
    /// 快照还没加载完时按“暂未就绪”处理，下一帧再试。
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
        }
        // 2) 已枚举列表里按名字提示挑（id 失效/从未选过时的主要路径，零开销）。
        let renders = self.render_devices();
        audio_default::pick_virtual(&renders)
    }

    fn render_devices(&self) -> Vec<EndpointInfo> {
        self.endpoints
            .iter()
            .filter(|e| e.flow == DataFlow::Render)
            .cloned()
            .collect()
    }

    /// 请求后台线程刷新端点/设备格式，并在后台重新探测每个端点**真正支持**哪些格式
    /// （独占模式，见 `format_support`；探测本身就在线程里，不阻塞 UI）。
    fn refresh_formats(&mut self) {
        self.poll.request_refresh();
        let ids: Vec<String> = self
            .render_devices()
            .iter()
            .map(|ep| ep.id.clone())
            .collect();
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
            effect: (&self.cfg.effect).into(),
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
                // 新引擎的两路增益都从 0 dB 起、也未静音：立刻把平衡补偿和静音状态补上，
                // 避免重启后短暂失衡 / 系统本来就静音却又漏出声音。
                self.last_balance_gains = [0.0, 0.0];
                self.last_engine_muted = false;
                self.refresh_balance_gains();
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
                self.poll.request_default_now();
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
        let current_default = self.poll.snapshot().default_output;
        let default_was_old = match (current_default, old_id.as_deref()) {
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
                    self.poll.request_default_now();
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
            // 没开自动切换：只让后台线程把「默认输出」状态读新，不做切换。
            self.poll.request_default_now();
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
                self.poll.request_default_now();
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
                self.poll.request_default_now();
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
    ///
    /// 体检要真的录 1 s（独占模式优先，失败还要按几种格式重试），**必须放到后台线程**：
    /// 早先是同步调用的，点一下「体检」整个界面会僵住约 1 秒。
    fn check_mic(&mut self) {
        if self.check_rx.is_some() {
            return;
        }
        if self.sync_mic.is_empty() {
            self.sync_status = "先选一个麦克风。".to_string();
            return;
        }
        let mic = self.sync_mic.clone();
        tracing::info!("[自校验] 开始体检麦克风：{mic}");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let health = stereo_audio::mic_health(&mic, 1_000).map_err(|e| e.to_string());
            let _ = tx.send(health);
        });
        self.check_rx = Some(rx);
        self.sync_status = "正在体检麦克风（请保持环境安静）…".to_string();
    }

    /// 收后台体检结果。
    fn poll_check(&mut self) {
        let Some(rx) = self.check_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.check_rx = None;
                self.sync_status = "体检线程异常退出。".to_string();
            }
            Ok(Ok(h)) => {
                self.check_rx = None;
                self.sync_status = h.describe();
                tracing::info!("[自校验] 麦克风体检：{}", h.describe());
                self.sync_health = Some(h);
            }
            Ok(Err(e)) => {
                self.check_rx = None;
                self.sync_status = format!("体检失败：{e}");
                self.sync_health = None;
            }
        }
    }

    /// 后台跑一次「延迟对齐 + 音量平衡校准」：先量两台箱的到达时差，再扫端点曲线 +
    /// 多档主音量下的声学锚点，算出「只压不抬」的数字增益存进配置。
    fn start_sync(&mut self) {
        if self.sync_busy {
            return;
        }
        // 体检线程正在采麦（独占模式优先），此刻开校准必然抢不到设备。
        if self.check_rx.is_some() {
            self.sync_status = "正在体检麦克风，等体检结束再校准。".to_string();
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
        // 校准结束把两端写回什么：
        // * 跟随生效 → 写回主音量（维持「两端 = 系统音量」的不变量，校准不该改变响度）；
        // * 跟随关闭 → 恢复校准前的原值（用户自己拧的音量别动）。
        let following = self.engine.is_some() && self.volume_follow_active();
        let restore_scalar = if following {
            self.volume_source.map(|v| v.scalar)
        } else {
            None
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let mic_t = mic.clone();
        let (left_t, right_t) = (left.clone(), right.clone());
        std::thread::spawn(move || {
            let cfg = SyncConfig::new(mic_t.clone(), left_t.clone());
            let sync = stereo_audio::calibrate_pair(&mic_t, &left_t, &right_t, &cfg)
                .map_err(|e| e.to_string());
            // 延迟对齐之后接着校准平衡：扫两端曲线 + 三个音量锚点，一次性把
            // 「音箱灵敏度差随音量变化」测全，运行时就不再需要麦克风、不需要再校准。
            let calibration = stereo_audio::calibrate_balance_with_progress(
                &mic_candidate,
                &left_t,
                &right_t,
                &cfg,
                &BALANCE_ANCHOR_SCALARS,
                restore_scalar,
                |step| {
                    tracing::info!("[音量平衡] [{}/{}] {}", step.done, step.total, step.message);
                },
            )
            .map_err(|e| e.to_string());
            let _ = tx.send(CalibrateOutcome { sync, calibration });
        });
        self.sync_rx = Some(rx);
        self.sync_busy = true;
        self.calibrating = true;
        self.balance_result = None;
        self.balance_error = None;
        self.balance_status = String::new();
        self.sync_status = if self.engine.is_some() {
            "正在标定延迟并校准音量平衡（约 30 秒）… 会依次在两台音箱上放 1 kHz 测试音、并短暂把音量拉到 30%/60%/90%；请先「停止」正在播放的音乐，期间别调系统音量，保持环境安静。".to_string()
        } else {
            "正在标定延迟并校准音量平衡（约 30 秒）… 会依次在两台音箱上放 1 kHz 测试音、并短暂调整音量；请保持环境安静。".to_string()
        };
        tracing::info!(
            "[自校验] 开始标定 + 平衡校准：mic={mic} left={left} right={right} restore_scalar={restore_scalar:?}"
        );
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
                // 线程不正常退出时也要放开跟随，否则音量联动会一直停着。
                self.calibrating = false;
                self.sync_status = "标定线程异常退出。".to_string();
            }
            Ok(CalibrateOutcome { sync, calibration }) => {
                self.sync_rx = None;
                self.sync_busy = false;
                self.calibrating = false;
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
                match calibration {
                    Ok(outcome) => {
                        let cal = outcome.calibration;
                        self.balance_result = outcome.last_report;
                        self.balance_error = None;
                        self.balance_cal = cal.clone();
                        self.cfg.balance = BalanceCalConfig::from(&cal);
                        self.save();
                        // 立刻把补偿写进引擎（不等下一帧），校准结束的瞬间也不要失衡。
                        self.refresh_balance_gains();
                        let gains = self.last_balance_gains;
                        self.balance_status = format!(
                            "已校准：{}；当前补偿 左 {:.1} dB / 右 {:.1} dB",
                            cal.describe(),
                            gains[0],
                            gains[1]
                        );
                        tracing::info!("[音量平衡] {}", self.balance_status);
                        for warning in &outcome.warnings {
                            tracing::warn!("[音量平衡] {warning}");
                        }
                        if outcome.warnings.is_empty() {
                            self.toast("已校准音量平衡：以后调系统音量会保持平衡");
                        } else {
                            self.toast(format!(
                                "已校准音量平衡（{} 条提醒见日志）",
                                outcome.warnings.len()
                            ));
                        }
                    }
                    Err(e) => {
                        self.balance_status = format!("音量平衡校准失败（延迟对齐不受影响）：{e}");
                        tracing::warn!("[音量平衡] 校准失败：{e}");
                        self.balance_result = None;
                        self.balance_error = Some(e);
                        self.toast("音量平衡校准失败，详见日志");
                    }
                }
            }
        }
    }

    /// 当前生效的平衡校准：没校准过、或左右端点换过 → None（旧校准不能套到别的设备上）。
    fn active_balance(&self) -> Option<&BalanceCalibration> {
        let cal = &self.balance_cal;
        if cal.anchors.is_empty() {
            return None;
        }
        let left = self.cfg.split.left_endpoint.trim();
        let right = self.cfg.split.right_endpoint.trim();
        if cal.left_endpoint.trim() != left || cal.right_endpoint.trim() != right {
            return None;
        }
        Some(cal)
    }

    /// 当前主音量（系统音量 = 采集源标量）：插值「灵敏度差随音量变化」要用它。
    fn balance_master(&self) -> Option<f32> {
        self.volume_source
            .map(|v| v.scalar)
            .or_else(|| self.route_volumes.get(&RouteChannel::Left).copied())
    }

    /// 当前应该给两路的补偿增益（dB，均 ≤ 0，只压不抬，以较安静一侧为基准）。
    ///
    /// 跟随生效时两端标量都等于主音量；用户手动拧过某一路时用各自的实际音量。
    fn desired_balance_gains(&self) -> [f32; 2] {
        let (Some(cal), Some(master)) = (self.active_balance(), self.balance_master()) else {
            return [0.0, 0.0];
        };
        if self.engine.is_some() && self.volume_follow_active() {
            return cal.gains_following(master);
        }
        let left = self
            .route_volumes
            .get(&RouteChannel::Left)
            .copied()
            .unwrap_or(master);
        let right = self
            .route_volumes
            .get(&RouteChannel::Right)
            .copied()
            .unwrap_or(master);
        cal.gains_at(left, right, master)
    }

    /// 把音效设置（含调音预设，5 个原子量）实时写进引擎（不重启引擎、不断音）。
    ///
    /// 和 [`Self::refresh_balance_gains`] 同一个模式：每帧调用，只有配置真的变了才写，
    /// 免得每帧都去碰音频线程的共享内存。开关也走这里：引擎内部做约 15 ms 的干湿
    /// 淡入淡出，所以切换不会「啪」一声。
    fn refresh_effect(&mut self) {
        let desired: EffectSettings = (&self.cfg.effect).into();
        if desired == self.last_effect {
            return;
        }
        self.last_effect = desired;
        if let Some(engine) = self.engine.as_ref() {
            if engine.set_effect(desired) {
                tracing::info!(
                    "[音效] 引擎热更新：{}（{}），强度 {:.2}，低音 +{:.1} dB，限幅 {}",
                    if desired.enabled { "开" } else { "关" },
                    desired.preset.label(),
                    desired.amount,
                    desired.bass_boost_db,
                    if desired.limiter { "开" } else { "关" }
                );
            }
        }
    }
    /// 把平衡补偿 + 系统静音实时写进引擎（每路数字增益 / 引擎级数字静音，都是原子量；
    /// 不重启引擎、不断音）。
    ///
    /// 每帧调用；增益变化小于 1/4 死区（0.05 dB）**且**静音状态没变就什么都不写，免得每帧
    /// 都去碰音频线程的共享内存。
    ///
    /// 为什么静音也要写引擎：端点静音不是所有设备都支持 —— 实测蓝牙端点（Xiaomi Sound）
    /// 上 `SetMute(true)` 会被驱动折算成「把端点音量压到最低可用档（约 8%）」并立刻读回
    /// 「未静音」，系统静音后物理音箱还会漏出小声音。引擎侧推 0 才是确定的数字静音。
    fn refresh_balance_gains(&mut self) {
        let gains = self.desired_balance_gains();
        // 采集源 = 用户眼里的「系统音量」；它静音了就代表系统静音。
        let muted = self
            .volume_source
            .map(|volume| volume.muted)
            .unwrap_or(false);
        let threshold = BALANCE_GAIN_DEADBAND_DB * 0.25;
        let gains_changed = (gains[0] - self.last_balance_gains[0]).abs() >= threshold
            || (gains[1] - self.last_balance_gains[1]).abs() >= threshold;
        let mute_changed = muted != self.last_engine_muted;
        if !gains_changed && !mute_changed {
            return;
        }
        let before = self.last_balance_gains;
        let loud = (gains[0] - before[0]).abs() >= 1.0 || (gains[1] - before[1]).abs() >= 1.0;
        self.last_balance_gains = gains;
        self.last_engine_muted = muted;
        if let Some(engine) = self.engine.as_ref() {
            if gains_changed {
                engine.set_route_gains_db(&gains);
            }
            if mute_changed {
                engine.set_muted(muted);
                tracing::info!(
                    "[音量] 系统{} → 引擎输出{}",
                    if muted { "静音" } else { "取消静音" },
                    if muted {
                        "已推 0（数字静音）"
                    } else {
                        "恢复"
                    }
                );
            }
        }
        // 变化超过 1 dB 才记一行：拖音量时每帧都在变，全记会把日志刷满；
        // 「调音量后补偿跟着变、有效差仍为 0」这条证据只在这几行里有。
        if loud && self.engine.is_some() {
            let master = self.balance_master().unwrap_or(0.0);
            let residual = self
                .active_balance()
                .map(|cal| {
                    let left = self
                        .route_volumes
                        .get(&RouteChannel::Left)
                        .copied()
                        .unwrap_or(master);
                    let right = self
                        .route_volumes
                        .get(&RouteChannel::Right)
                        .copied()
                        .unwrap_or(master);
                    cal.residual_db(left, right, master)
                })
                .unwrap_or(0.0);
            tracing::info!(
                "[音量平衡] 主音量 {:.0}% → 补偿 左 {:+.1} dB / 右 {:+.1} dB（有效差 {:+.1} dB）",
                master * 100.0,
                gains[0],
                gains[1],
                residual
            );
        }
    }

    /// 这一路的端点音量标量（还没回读到就用主音量/100% 兜底）。
    fn route_volume_scalar(&self, channel: RouteChannel) -> f32 {
        self.route_volumes
            .get(&channel)
            .copied()
            .or_else(|| self.balance_master())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0)
    }

    /// 某一路在给定端点标量下、端点自己报的 dB（校准时扫过曲线就用它，否则用典型曲线）。
    fn endpoint_db_of(&self, channel: RouteChannel, scalar: f32) -> f32 {
        let curve = self.active_balance().map(|cal| match channel {
            RouteChannel::Right => &cal.right_curve,
            // Both 只出现在「两路一起」的 UI 路径里，拿左曲线当代表足够。
            RouteChannel::Left | RouteChannel::Both => &cal.left_curve,
        });
        match curve {
            Some(curve) if !curve.is_empty() => curve.db_at(scalar),
            _ => stereo_audio::taper_db(scalar) as f32,
        }
    }

    /// 这一路的补偿增益（dB，均 ≤ 0）。
    fn channel_gain_db(&self, channel: RouteChannel) -> f32 {
        match channel {
            RouteChannel::Right => self.last_balance_gains[1],
            RouteChannel::Left | RouteChannel::Both => self.last_balance_gains[0],
        }
    }

    /// 这一路「实机输出」的 dB：端点 dB + 引擎补偿，左路再加校准出的音箱灵敏度差。
    ///
    /// 灵敏度差 S 定义为「扣掉端点电平后两台音箱天生的响度差」：Δ = c_L − c_R + S。
    /// 所以左路声学电平要再加 S —— 只有这样残差为 0 时两路显示才真的一致。
    fn channel_output_db(&self, channel: RouteChannel) -> f32 {
        let db = self.endpoint_db_of(channel, self.route_volume_scalar(channel))
            + self.channel_gain_db(channel);
        match channel {
            RouteChannel::Right => db,
            // Both 只出现在「两路一起」的 UI 路径里，按左路显示。
            RouteChannel::Left | RouteChannel::Both => {
                db + self
                    .active_balance()
                    .map(|cal| cal.sens_at(self.balance_master().unwrap_or(1.0)))
                    .unwrap_or(0.0)
            }
        }
    }

    /// 「实机输出」音量：声学电平（端点 + 补偿 + 灵敏度差）折回 0..=1 标量。
    ///
    /// 路由表里那一列显示的就是它 —— 跟随生效时两端端点音量会被镜像成同一个值，
    /// 那个数字完全看不出两台音箱响度差多少；实机输出才是用户耳朵听到的量。
    fn actual_output_scalar(&self, channel: RouteChannel) -> f32 {
        if self
            .volume_source
            .map(|volume| volume.muted)
            .unwrap_or(false)
        {
            return 0.0;
        }
        stereo_audio::taper_scalar(self.channel_output_db(channel) as f64).clamp(0.0, 1.0)
    }

    /// 反解：想让这一路「实机输出」达到 target，端点音量该写多少（没有校准的普通路径用）。
    fn endpoint_scalar_for_output(&self, channel: RouteChannel, target: f32) -> f32 {
        let target_db = stereo_audio::taper_db(target.clamp(0.0, 1.0)) as f32;
        let db = target_db - self.channel_gain_db(channel);
        stereo_audio::taper_scalar(db as f64).clamp(0.0, 1.0)
    }

    /// 反解：跟随生效时想让这一路「实机输出」达到 target，系统音量（主音量标量）该是多少。
    ///
    /// 跟随下两端端点音量都等于主音量，实机输出 = 端点曲线(m) + 补偿(m)，随 m 单调递增，
    /// 直接二分反解（24 次精度足够，只有拖动的那几帧会算）。
    fn master_for_output(&self, channel: RouteChannel, target: f32) -> f32 {
        let target_db = stereo_audio::taper_db(target.clamp(0.0, 1.0)) as f32;
        let Some(cal) = self.active_balance() else {
            return target.clamp(0.0, 1.0);
        };
        let index = usize::from(channel == RouteChannel::Right);
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..24 {
            let mid = 0.5 * (lo + hi);
            let sens = if index == 0 { cal.sens_at(mid) } else { 0.0 };
            let output_db =
                self.endpoint_db_of(channel, mid) + cal.gains_following(mid)[index] + sens;
            if output_db < target_db {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }

    /// 平衡校准生效时两路是一体的：把整对音箱的实机输出抬/降到 target，
    /// 保持相对平衡不变，返回左、右端点该写的标量。
    ///
    /// 校准生效时补偿永远把两端拉回等响，单独改一侧端点音量只会被补偿吃掉（界面看起来
    /// 就是拖不动），所以这里按「整对」调：两端端点音量同加一个 dB 偏移。
    fn balanced_pair_scalars(&self, target: f32) -> [f32; 2] {
        let target_db = stereo_audio::taper_db(target.clamp(0.0, 1.0)) as f32;
        let mut out = [0.0f32; 2];
        for (index, channel) in [RouteChannel::Left, RouteChannel::Right]
            .into_iter()
            .enumerate()
        {
            let scalar = self.route_volume_scalar(channel);
            let endpoint_db = self.endpoint_db_of(channel, scalar);
            let current_db = self.channel_output_db(channel);
            let shifted = endpoint_db + (target_db - current_db);
            out[index] = stereo_audio::taper_scalar(shifted as f64).clamp(0.0, 1.0);
        }
        out
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

    // 音量跟随（读采集源 + 写目标设备）已全部搬进 `endpoint_poll` 后台线程：
    // UI 线程每帧只把「是否跟随 / 目标设备」推给线程，不再做任何 COM 调用。

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
    ///
    /// 写入走后台线程：按钮点击后立即返回，避免一次 COM 调用卡住 UI 一帧。
    fn normalize_source_volume(&mut self) {
        if self.capture_endpoint().is_none() {
            self.toast("先选好采集源");
            return;
        }
        self.poll.request_set_source_volume(1.0);
        self.toast("采集源音量已设为 100%（响度请用系统音量或音箱音量控制）");
    }

    /// 停止路由时还原目标设备被联动改掉的音量（best-effort：强杀/崩溃不会还原）。
    ///
    /// 正常停止走后台线程（异步、不卡 UI）；强杀/退出由 [`Self::restore_target_volumes_blocking`] 兜底。
    fn restore_target_volumes(&mut self) {
        self.poll.request_restore_volumes();
        self.volume_error = None;
    }

    /// 退出路径的**同步**音量还原（只在 `on_exit` 调用一次）。
    ///
    /// 进程马上要退出，后台线程可能来不及再跑一次 tick，所以这里直接走 COM 还原。
    fn restore_target_volumes_blocking(&mut self) {
        let saved = self.poll.snapshot().follow_saved;
        crate::endpoint_poll::restore_volumes_blocking(&saved);
    }

    /// 记一条右下角浮层提示。这里不直接 push（`toast()` 常常在 `&mut self` 的
    /// 深层调用里、拿不到 `ctx`），先攒成一条待发文案，`update()` 尾巴统一入队。
    fn toast(&mut self, text: impl Into<String>) {
        self.pending_toast = Some(text.into());
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

    /// 声道路由页顶部的操作按钮：开始 / 停止 / 重启 / 左右自检音。
    ///
    /// （顶栏已取消：身份和状态在侧边栏底部，操作按钮集中到路由页顶部。）
    fn routing_actions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.engine.is_some() {
                if btn(ui, "停止路由", theme::ButtonKind::Danger).clicked() {
                    self.stop_routing(true);
                    self.toast("已停止路由并恢复默认输出");
                }
                if btn(ui, "重启路由", theme::ButtonKind::Secondary).clicked() {
                    self.restart_routing();
                    self.toast("路由已重启");
                }
            } else if btn(ui, "开始路由", theme::ButtonKind::Primary).clicked() {
                self.start_routing(true);
            }
            if btn(ui, "左右自检音", theme::ButtonKind::Ghost)
                .on_hover_text("向两台音箱分别放一声测试音，快速确认左右没接反")
                .clicked()
            {
                self.play_test_tone();
            }
            // 操作结果不再占一行固定宽度的文字格，改由右下角 toast 浮层提示：
            // 成功/失败只在发生的瞬间出现，不挤动按钮，也不留下过期文案。
        });
    }

    /// 声道路由页顶部的三枚诊断徽章：只显示三项状态，不带任何解释文字。
    fn diagnostics_row(&mut self, ui: &mut egui::Ui) {
        let capture_ready = self.capture_endpoint().is_some();
        let (link_ok, link_text, link_tip) = self.link_status();
        let default_ready = self.live_default_is_capture;
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
    ///
    /// 底部身份/主题/声音设置用内嵌的 `TopBottomPanel::bottom` 固定贴底，
    /// 页面导航放在上面可滚动的中央区——不依赖 `bottom_up` 布局，避免行序被反转。
    fn sidebar(&mut self, ctx: &egui::Context) {
        let width = if self.sidebar_open {
            SIDEBAR_W
        } else {
            SIDEBAR_W_COLLAPSED
        };
        let margin_x = if self.sidebar_open { 10 } else { 6 };
        let panel = egui::SidePanel::left("sidebar")
            .exact_width(width)
            .resizable(false)
            .frame(
                // 侧边栏也是玻璃：半透明底色，透出底层的柔光底纹。
                //
                // 这里**故意不描边**：侧边栏的左/上/下三条边本身就贴着窗口边框，四边描一圈
                // 只会在窗口边缘凭空多出一道浅色实线——上沿那条尤其像「第二个标题栏」，
                // 也就是用户说的「莫名其妙的白边」。设计原型里侧边栏是
                // `border: 0; border-right: 1px solid`，所以这里也只在右缘补一条分隔线。
                egui::Frame::default()
                    .fill(theme::tokens(is_dark()).sidebar)
                    .inner_margin(Margin::symmetric(margin_x, 12)),
            )
            .show(ctx, |ui| {
                // 页脚先声明，占掉底部空间：主题 → 系统声音设置 → 身份（最下方）。
                egui::TopBottomPanel::bottom("sidebar_footer")
                    .frame(egui::Frame::default())
                    .show_inside(ui, |ui| {
                        self.theme_selector(ui, ctx);
                        ui.add_space(4.0);
                        self.sound_settings_button(ui);
                        ui.add_space(8.0);
                        self.identity_block(ui);
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::default())
                    .show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            if self.sidebar_open {
                                ui.label(
                                    RichText::new("导航")
                                        .size(12.0)
                                        .color(palette().muted)
                                        .strong(),
                                );
                            }
                            let (collapsed_icon, tip) = if self.sidebar_open {
                                (crate::icons::Icon::ChevronLeft, "收起侧边栏")
                            } else {
                                (crate::icons::Icon::ChevronRight, "展开侧边栏")
                            };
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if icon_button(ui, collapsed_icon, palette().muted, tip).clicked() {
                                    self.sidebar_open = !self.sidebar_open;
                                    self.cfg.save();
                                }
                            });
                        });
                        ui.add_space(8.0);
                        let mut target: Option<Page> = None;
                        for page in Page::ALL {
                            let selected = self.page == page;
                            let color = if selected {
                                palette().accent
                            } else {
                                palette().muted
                            };
                            let (rect, response) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 34.0),
                                Sense::click(),
                            );
                            let hovered = response.hovered();
                            // 选中项 = 玻璃胶囊（accent 半透明底 + 1px 描边 + 左侧竖条）。
                            if selected {
                                theme::nav_capsule(ui, rect, is_dark());
                            } else if hovered {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(10),
                                    palette().hover.gamma_multiply(0.75),
                                );
                            }
                            // 展开：图标 + 文字左对齐；收起：只有居中图标。
                            let painter = ui.painter().with_clip_rect(rect);
                            if self.sidebar_open {
                                let icon_rect = egui::Rect::from_min_size(
                                    egui::pos2(rect.left() + 12.0, rect.center().y - 9.0),
                                    Vec2::splat(18.0),
                                );
                                crate::icons::paint(&painter, icon_rect, page.icon(), color, 1.5);
                                painter.text(
                                    egui::pos2(icon_rect.right() + 9.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    page.label(),
                                    FontId::proportional(13.5),
                                    color,
                                );
                            } else {
                                crate::icons::paint(
                                    &painter,
                                    egui::Rect::from_center_size(rect.center(), Vec2::splat(18.0)),
                                    page.icon(),
                                    color,
                                    1.5,
                                );
                            }
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
            });

        // 侧边栏右缘的 1px 分隔线（对应设计原型的 `border-right`）。
        // 面板自身不描边，这条线单独用一层画在面板之外，免得被 `inner_margin` 裁掉。
        let panel_rect = panel.response.rect;
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("sidebar_separator"),
        ))
        .rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(panel_rect.right() - 1.0, panel_rect.top()),
                panel_rect.right_bottom(),
            ),
            0.0,
            theme::tokens(is_dark()).stroke,
        );
    }

    /// 侧边栏底部：应用身份（图标 + 名称 + 简介），上方是运行状态胶囊。
    ///
    /// 侧边栏收起时只显示图标 + 一个状态圆点（悬停显示完整状态文字）。
    fn identity_block(&mut self, ui: &mut egui::Ui) {
        let expanded = self.sidebar_open;
        let running = self.engine.is_some();
        // 整块作为一个 top-down 单元，避免父级 bottom_up 布局把内部行序打反。
        ui.vertical(|ui| {
            if expanded {
                if running {
                    pill(ui, "运行中", palette().ok, 84.0);
                } else {
                    pill(ui, "已停止", palette().muted, 84.0);
                }
                ui.add_space(6.0);
            }
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                paint_logo(ui, rect);
                if expanded {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("StereoLink")
                                .size(15.0)
                                .strong()
                                .color(palette().text),
                        );
                        ui.label(
                            RichText::new("本机设备左右分声道")
                                .size(11.0)
                                .color(palette().muted),
                        );
                    });
                }
            });
            if !expanded {
                let color = if running {
                    palette().ok
                } else {
                    palette().muted
                };
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 6.0, color);
                response.on_hover_text(if running { "运行中" } else { "已停止" });
            }
        });
    }

    /// 主题切换：展开时是下拉框，收起时是循环按钮（跟随系统 → 浅色 → 深色）。
    fn theme_selector(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // 当前模式的图标：跟随系统 / 深色 / 浅色各一个。展开时放在「主题」左边，
        // 收起时它就是唯一的控件（点一下循环到下一个模式）。
        let mode_icon = match self.cfg.theme {
            ThemeMode::System => crate::icons::Icon::Auto,
            ThemeMode::Dark => crate::icons::Icon::Moon,
            ThemeMode::Light => crate::icons::Icon::Sun,
        };
        if self.sidebar_open {
            ui.horizontal(|ui| {
                let _ = crate::icons::show(ui, mode_icon, 13.0, palette().muted);
                ui.label(RichText::new("主题").size(11.0).color(palette().muted));
                let mut chosen = self.cfg.theme;
                egui::ComboBox::from_id_salt("theme_mode")
                    .selected_text(chosen.label())
                    .width(84.0)
                    .show_ui(ui, |ui| {
                        for mode in ThemeMode::ALL {
                            if ui.selectable_label(chosen == mode, mode.label()).clicked() {
                                chosen = mode;
                            }
                        }
                    });
                if chosen != self.cfg.theme {
                    self.set_theme(ctx, chosen);
                }
            });
        } else {
            let tip = format!("主题：{}（点击切换）", self.cfg.theme.label());
            if icon_button(ui, mode_icon, palette().muted, &tip).clicked() {
                let next = self.cfg.theme.next();
                self.set_theme(ctx, next);
            }
        }
    }

    /// 应用主题模式：写配置 + 立刻重设 egui 主题与配色（跟着系统时交给 eframe 上报）。
    fn set_theme(&mut self, ctx: &egui::Context, mode: ThemeMode) {
        self.cfg.theme = mode;
        self.cfg.save();
        if let Some(preference) = mode.preference() {
            ctx.set_theme(preference);
        } else {
            // 跟随系统：清掉手动偏好，让 egui 回到「系统是什么就是什么」。
            ctx.set_theme(egui::ThemePreference::System);
        }
        let dark = matches!(ctx.theme(), egui::Theme::Dark);
        self.theme_dark = dark;
        set_dark_mode(dark);
        Self::apply_visuals(ctx);
        ctx.request_repaint();
        tracing::info!("[主题] 已切换为 {}", mode.label());
    }

    /// 侧边栏底部的「系统声音设置」入口。
    fn sound_settings_button(&mut self, ui: &mut egui::Ui) {
        let clicked = if self.sidebar_open {
            btn(ui, "系统声音设置", theme::ButtonKind::Ghost)
                .on_hover_text("打开 Windows 声音设置")
                .clicked()
        } else {
            icon_button(
                ui,
                crate::icons::Icon::Gear,
                palette().muted,
                "打开 Windows 声音设置",
            )
            .clicked()
        };
        if clicked {
            let _ = std::process::Command::new("cmd")
                .args(["/c", "start", "ms-settings:sound"])
                .spawn();
        }
    }

    /// 采集源卡：D 模式下采集源可以是任意播放端点（虚拟声卡 / UU远程 / 物理输出）。
    fn capture_card(&mut self, ui: &mut egui::Ui) {
        let devices = self.render_devices();
        let capture = self.capture_endpoint();
        let capture_id = capture.as_ref().map(|ep| ep.id.clone());
        let kind_now = capture.as_ref().map(|ep| capture_kind(&ep.name));
        let uu_present = devices.iter().any(|ep| ep.name.contains("UU远程"));
        let mut chosen_id: Option<String> = None;

        card(ui, |ui| {
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
                        None => badge(ui, "未找到", palette().err),
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
            text_cell(
                ui,
                width,
                26.0,
                &name,
                FontId::proportional(17.0),
                palette().text,
                false,
            )
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
                    palette().muted,
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
                    if self.live_default_is_capture {
                        palette().ok
                    } else {
                        palette().warn
                    },
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
                palette().muted,
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
                    _ => RichText::new("（未选择采集源）")
                        .color(palette().muted)
                        .size(12.5),
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
                if btn(ui, "切为默认输出", theme::ButtonKind::Secondary).clicked() {
                    self.switch_default_to_capture();
                }
                let has_previous = self.cfg.capture.previous_output.is_some();
                if btn_enabled(ui, has_previous, "恢复原输出", theme::ButtonKind::Ghost)
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
                ui.label(
                    RichText::new(format!("混音 {fmt}"))
                        .size(12.0)
                        .color(palette().text),
                )
                .on_hover_text("采集端点的混音格式：路由引擎的采样率跟随它（零重采样）。");
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
                    }
                }
                let (text, color) = if self.volume_follow_active() {
                    match self.poll.snapshot().source_volume {
                        Some(volume) => (
                            format!(
                                "跟随中：{:.0}%{}",
                                volume.scalar * 100.0,
                                if volume.muted { "（静音）" } else { "" }
                            ),
                            palette().ok,
                        ),
                        None => ("跟随中".to_string(), palette().ok),
                    }
                } else {
                    ("不联动".to_string(), palette().muted)
                };
                ui.add(
                    egui::Label::new(RichText::new(text).size(12.0).color(color)).selectable(false),
                );
                if let Some(err) = self.volume_error.clone() {
                    ui.add(
                        egui::Label::new(RichText::new(err).size(11.0).color(palette().err))
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
                            .color(palette().warn),
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
                    .fill(palette().warn.gamma_multiply(0.10))
                    .stroke(Stroke::new(1.0_f32, palette().warn.gamma_multiply(0.45)))
                    .inner_margin(Margin::symmetric(10, 8))
                    .corner_radius(CornerRadius::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(tip).color(palette().warn).size(12.0));
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
                // 透明底：柔光背景纹理画在窗口最底层（见 update() 里的 paint_background），
                // 中央面板不铺不透明色，玻璃卡片才透得出底纹。
                egui::Frame::default()
                    .fill(Color32::TRANSPARENT)
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
                        Page::Cast => self.cast_card(ui),
                        Page::Log => {}
                    });
                ui.add_space(10.0);
            });
    }

    fn routing_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("声道路由").size(16.0).strong());
                ui.add_space(6.0);
                if self.engine.is_some() {
                    pill(ui, "分离运行中", palette().ok, 96.0);
                    if let Some(stats) = &self.stats {
                        if stats.degraded {
                            pill(ui, "等待设备", palette().warn, 84.0);
                        } else if stats.routes.iter().any(|r| r.retry_in_ms > 0) {
                            pill(ui, "重连中", palette().warn, 72.0);
                        }
                    }
                } else {
                    pill(ui, "未运行", Color32::from_rgb(120, 128, 142), 96.0);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .button("刷新设备")
                        .on_hover_text("重新枚举本机播放设备、设备默认格式与系统默认输出")
                        .clicked()
                    {
                        self.refresh_endpoints();
                    }
                    ui.add_space(8.0);
                    let latency = self
                        .route_engine_config()
                        .map(|c| c.software_latency_ms())
                        .unwrap_or(0);
                    ui.label(muted(format!("软件侧延迟约 {latency} ms")));
                });
            });
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            // 操作按钮与三项诊断都在本页顶部（顶栏已取消）。
            self.routing_actions(ui);
            ui.add_space(4.0);
            self.diagnostics_row(ui);
            ui.add_space(6.0);
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
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        device_w,
                        18.0,
                        "设备",
                        header_font.clone(),
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_WAVE,
                        18.0,
                        "波形",
                        header_font.clone(),
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_LEVEL,
                        18.0,
                        "电平",
                        header_font.clone(),
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_VOL,
                        18.0,
                        "实机输出",
                        header_font.clone(),
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_BUF,
                        18.0,
                        "缓冲",
                        header_font.clone(),
                        palette().muted,
                        true,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_PATH,
                        18.0,
                        "路径",
                        header_font.clone(),
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        ROUTE_COL_INV,
                        18.0,
                        "反相",
                        header_font,
                        palette().muted,
                        false,
                    );
                });
            }
            ui.add_space(2.0);
            for channel in [RouteChannel::Left, RouteChannel::Right] {
                self.route_row(ui, channel, device_w);
            }
            ui.label(muted(
                "反相：两台音箱接线/极性相反时，勾选任一路 180°，居中人声与低音就不会互相抵消。",
            ));

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut self.cfg.effect.enabled, "音效")
                    .on_hover_text(
                        "三种调音预设（类杜比 / 丹拿 / 哈曼卡顿）给两箱一起用的声音加宽声场、\n\
                         补低频、调音色（类似杜比/DTS 的音效模式，不是 Dolby / Dynaudio /\n\
                         Harman 官方认证或授权算法）。关闭时采集数据 bit-exact 直通，不做\n\
                         任何处理；开关和切预设都是热切换，不会断音。",
                    )
                    .changed()
                {
                    self.cfg.save();
                    self.refresh_effect();
                }
                ui.add_space(10.0);
                ui.label(muted("调音"));
                let current = self.cfg.effect.preset;
                egui::ComboBox::from_id_salt("effect_preset")
                    .selected_text(current.label())
                    .show_ui(ui, |ui| {
                        for preset in EffectPresetConfig::ALL {
                            if ui
                                .selectable_label(preset == current, preset.label())
                                .on_hover_text(match preset {
                                    EffectPresetConfig::Dolby => {
                                        "类杜比：宽声场 + 扎实低频 + 4.5 kHz 空气感，包围感最强。"
                                    }
                                    EffectPresetConfig::Dynaudio => {
                                        "丹拿：几乎不加宽、90 Hz 以下单声道化、高频略收，干净、结像稳。"
                                    }
                                    EffectPresetConfig::Harman => {
                                        "哈曼卡顿：中等加宽 + V 型曲线（人声与 10 kHz 高频抬升），更亮更有氛围。"
                                    }
                                })
                                .clicked()
                                && preset != current
                            {
                                self.cfg.effect.preset = preset;
                                // 切预设时套用推荐值，否则「丹拿」还挂着类杜比的 +7 dB 低音。
                                let (amount, bass_boost_db, limiter) = preset.recommended();
                                self.cfg.effect.amount = amount;
                                self.cfg.effect.bass_boost_db = bass_boost_db;
                                self.cfg.effect.limiter = limiter;
                                self.cfg.save();
                                self.refresh_effect();
                            }
                        }
                    });
                if self.cfg.effect.enabled {
                    ui.add_space(10.0);
                    ui.label(muted("强度"));
                    if ui
                        .add(
                            egui::Slider::new(&mut self.cfg.effect.amount, 0.0..=1.5).step_by(0.05),
                        )
                        .on_hover_text("预设的整体强度：1.0 为预设的完整效果，越大越夸张。")
                        .changed()
                    {
                        self.cfg.save();
                        self.refresh_effect();
                    }
                }
            });
            if self.cfg.effect.enabled {
                ui.horizontal(|ui| {
                    ui.label(muted("低音"));
                    if ui
                        .add(
                            egui::Slider::new(&mut self.cfg.effect.bass_boost_db, 0.0..=12.0)
                                .suffix(" dB")
                                .step_by(0.5),
                        )
                        .on_hover_text("低频搁架增益：给节奏和氛围补厚度（默认 +4 dB）。")
                        .changed()
                    {
                        self.cfg.save();
                        self.refresh_effect();
                    }
                    ui.add_space(10.0);
                    if ui
                        .checkbox(&mut self.cfg.effect.limiter, "软限幅")
                        .on_hover_text(
                            "立体声联动软限幅（天花板 -1 dBFS）：加宽 + 低音提升后防止削顶破音。\
                             建议保持开启。",
                        )
                        .changed()
                    {
                        self.cfg.save();
                        self.refresh_effect();
                    }
                });
                ui.label(muted(format!(
                    "{} 调音：{}；切换预设会套用该预设的推荐强度/低音/限幅（之后仍可微调）。",
                    self.cfg.effect.preset.label(),
                    match self.cfg.effect.preset {
                        EffectPresetConfig::Dolby =>
                            "低频单声道化 + 声场加宽 + 4.5 kHz 空气感 + 软限幅",
                        EffectPresetConfig::Dynaudio =>
                            "低频收得更早 + 几乎不加宽 + 高频柔化 + 软限幅",
                        EffectPresetConfig::Harman =>
                            "中等加宽 + V 型曲线（低频/人声/高频都抬） + 软限幅",
                    }
                )));
            }
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
                let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(17.0), Sense::hover());
                crate::icons::paint(
                    ui.painter(),
                    icon_rect.shrink(0.5),
                    crate::icons::Icon::Mic,
                    palette().muted,
                    1.5,
                );
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
                                RichText::new("（未选择麦克风）").color(palette().muted)
                            } else {
                                RichText::new(clip_text(&short_endpoint(&mic_label), 26))
                                    .color(palette().text)
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
                if btn(ui, "刷新", theme::ButtonKind::Ghost).clicked() {
                    action = Some("refresh");
                }
                // 体检在后台线程跑（界面不再僵住），体检期间禁止重复点击，也禁止同时开校准——
                // 两者都会短暂独占麦克风。
                let check_busy = self.check_rx.is_some();
                if btn_enabled(
                    ui,
                    !check_busy,
                    if check_busy { "体检中…" } else { "体检" },
                    theme::ButtonKind::Secondary,
                )
                .clicked()
                {
                    action = Some("check");
                }
                if btn_enabled(
                    ui,
                    !self.sync_busy && !check_busy && !self.sync_mic.is_empty(),
                    if self.sync_busy {
                        "校准中…"
                    } else {
                        "校准延迟并平衡音量"
                    },
                    theme::ButtonKind::Primary,
                )
                .on_hover_text(
                    "一次完成：量两台音箱的真实时差并自动垫延迟，再分别量左右电平、给出音量平衡建议（约 8 秒）",
                )
                .clicked()
                {
                    action = Some("sync");
                }
                if btn_enabled(ui, !self.clicks_busy, "点击串试听", theme::ButtonKind::Ghost)
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
                palette().muted,
                false,
            )
            .on_hover_text(status);

            // 音量平衡：一次校准（两端音量曲线 + 3 档声学锚点）存进配置；运行时只按当前
            // 主音量重算「每路数字增益」，所以调系统音量不会再破坏平衡 —— 这是这个 bug 的
            // 结构性修复：平衡不再是「两个端点的音量差」，跟随怎么镜像都不会把它抹掉。
            let balance_ready = !self.balance_cal.is_empty();
            let balance_stale = balance_ready && self.active_balance().is_none();
            if self.calibrating
                || self.balance_error.is_some()
                || !self.balance_status.is_empty()
                || balance_ready
            {
                ui.add_space(2.0);
                let mut recalc = false;
                let gains = self.last_balance_gains;
                // 当前「有效差」= 实测差 + 补偿后还剩多少；应用补偿后应落在死区内。
                let residual = self.active_balance().and_then(|cal| {
                    let master = self.balance_master()?;
                    let left = self
                        .route_volumes
                        .get(&RouteChannel::Left)
                        .copied()
                        .unwrap_or(master);
                    let right = self
                        .route_volumes
                        .get(&RouteChannel::Right)
                        .copied()
                        .unwrap_or(master);
                    Some(cal.residual_db(left, right, master))
                });
                ui.horizontal(|ui| {
                    ui.label(muted("音量平衡"));
                    let (text, color) = if self.calibrating {
                        (self.sync_status.clone(), palette().muted)
                    } else if let Some(err) = &self.balance_error {
                        (format!("校准失败：{err}"), palette().warn)
                    } else if balance_stale {
                        (
                            "校准记录已失效（左右端点换过），请重新校准".to_string(),
                            palette().warn,
                        )
                    } else if balance_ready {
                        let mut text =
                            format!("当前补偿 左 {:.1} dB / 右 {:.1} dB", gains[0], gains[1]);
                        if let Some(residual) = residual {
                            text.push_str(&format!("（有效差 {residual:+.1} dB）"));
                        }
                        let balanced = residual
                            .map(|r| r.abs() <= BALANCE_GAIN_DEADBAND_DB)
                            .unwrap_or(false);
                        (
                            text,
                            if balanced {
                                palette().ok
                            } else {
                                palette().accent
                            },
                        )
                    } else {
                        (self.balance_status.clone(), palette().muted)
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
                    let label = if balance_ready {
                        "重新校准"
                    } else {
                        "校准音量平衡"
                    };
                    if btn(ui, label, theme::ButtonKind::Secondary)
                        .on_hover_text(
                            "一次校准做完所有事：先标定延迟，再静音扫描两台音箱各自的音量曲线，\
                             然后在 30%/60%/90% 三档音量各放一次 1 kHz 测试音测左右电平，\
                             算出「只压不抬」的补偿增益存进配置。约 30 秒。\n\
                             校准一次就够：以后调系统音量只会按当前音量重算补偿，不需要再校准、\
                             也不需要麦克风。",
                        )
                        .clicked()
                    {
                        recalc = true;
                    }
                });
                if balance_ready && !self.calibrating {
                    ui.horizontal(|ui| {
                        ui.label(muted("校准数据"));
                        let mut text = format!(
                            "灵敏度差 {:+.1} dB（左−右）· {} 个锚点",
                            self.balance_cal.sens_diff_db,
                            self.balance_cal.anchors.len()
                        );
                        if !self.balance_cal.left_curve.is_empty()
                            && !self.balance_cal.right_curve.is_empty()
                        {
                            text.push_str(" · 端点曲线已扫描");
                        }
                        let tip = match &self.balance_result {
                            Some(report) => format!(
                                "{}\n上次实测：左 {:.1} dB / 右 {:.1} dB，差 {:+.1} dB，置信度 {:.0}%",
                                self.balance_cal.describe(),
                                report.left_db,
                                report.right_db,
                                report.delta_db,
                                report.confidence * 100.0
                            ),
                            None => self.balance_cal.describe(),
                        };
                        ui.label(RichText::new(text).size(12.0).color(palette().text))
                            .on_hover_text(tip);
                    });
                }
                if recalc {
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
                    palette().muted,
                    false,
                );
                let rest = ui.available_width().max(120.0);
                text_cell(
                    ui,
                    rest,
                    20.0,
                    &source_line,
                    FontId::proportional(12.0),
                    palette().muted,
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
        // 这一行显示的是「实机输出」音量：端点音量 + 引擎补偿。跟随生效时两端端点音量会被
        // 镜像成同一个值，那个数字看不出两台音箱的响度差，实机输出才是用户耳朵听到的量。
        let current_volume = self.actual_output_scalar(channel);
        let following = self.engine.is_some() && self.volume_follow_active();
        let balanced = self.active_balance().is_some();
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
                            RichText::new("（未选择设备）").color(palette().muted).size(13.0)
                        } else {
                            RichText::new(clip_text(&short_endpoint(&current), 32))
                                .color(palette().text)
                                .size(13.0)
                        })
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    current.is_empty(),
                                    RichText::new("（未选择设备）").color(palette().muted),
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

            // 行内「波形 + 频谱」装饰：幅度取这一路的真实电平（波形形状是合成的，
            // 见 theme::WaveBuf 的说明）。左侧声道蓝、右侧声道琥珀。
            // 悬停出读数（峰值 dBFS），点一下定格/恢复这一路的动画——只停画面，不影响声音。
            let (wave, wave_idx) = if channel == RouteChannel::Left {
                (&self.wave_l, 0usize)
            } else {
                (&self.wave_r, 1usize)
            };
            let wave_peak = wave.peak();
            let wave_level = wave.level();
            let wave_paused = self.wave_paused[wave_idx];
            let dark = is_dark();
            let peak_db = if wave_peak > 1.0e-6 {
                20.0 * wave_peak.log10()
            } else {
                -120.0
            };
            let chan_name = channel_label(channel);
            let mut toggle_wave_pause = false;
            cell(
                ui,
                ROUTE_COL_WAVE,
                ROUTE_ROW_H,
                Layout::left_to_right(Align::Center),
                |ui| {
                    let (rect, resp) =
                        ui.allocate_exact_size(Vec2::new(104.0, 30.0), Sense::click());
                    wave.paint(ui, rect, color, dark);
                    // 暂停时常驻一个播放三角（提示"点一下继续"）；没暂停则只在悬停时
                    // 浮出一个淡淡的暂停标记，平时完全不挡波形。
                    let hovered = resp.hovered();
                    if wave_paused || hovered {
                        let painter = ui.painter().with_clip_rect(rect);
                        painter.rect_filled(
                            rect,
                            CornerRadius::same(8),
                            Color32::from_black_alpha(if wave_paused { 56 } else { 26 }),
                        );
                        let glyph = if wave_paused {
                            crate::icons::Icon::Play
                        } else {
                            crate::icons::Icon::Pause
                        };
                        let tint = if wave_paused {
                            palette().text
                        } else {
                            palette().muted
                        };
                        crate::icons::paint(&painter, rect.shrink(9.0), glyph, tint, 1.6);
                    }
                    if resp.clicked() {
                        toggle_wave_pause = true;
                    }
                    resp.on_hover_text(format!(
                        // chan_name 本身已经带「声道」二字（左声道 / 右声道），这里不要再拼一次。
                        "{chan_name} · 峰值 {peak_db:.1} dBFS\n点击{}波形动画（不影响声音）",
                        if wave_paused { "恢复" } else { "暂停" }
                    ));
                },
            );
            if toggle_wave_pause {
                self.wave_paused[wave_idx] = !wave_paused;
            }

            // 电平：分段电平条（12 段绿→黄→红 + 峰值保持线）+ 右对齐百分比。
            let linear = route.as_ref().map(|route| route.level).unwrap_or(0.0);
            cell(
                ui,
                ROUTE_COL_LEVEL,
                ROUTE_ROW_H,
                Layout::left_to_right(Align::Center),
                |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(44.0, 12.0), Sense::hover());
                    theme::segmented_level(ui, rect, wave_level.sqrt(), wave_peak, dark);
                    text_cell(
                        ui,
                        36.0,
                        ROUTE_ROW_H,
                        &format!("{:>3.0}%", linear * 100.0),
                        FontId::monospace(12.0),
                        palette().text,
                        true,
                    );
                },
            );

            // 「实机输出」音量列：百分比 = 端点音量 × 引擎补偿后的实际响度。
            // 拖动也按实机输出调：跟随生效时改系统音量（两端本来就被镜像成同一个值）；
            // 平衡校准生效时两端一起按比例变（保持平衡）；都没有时才写单侧端点音量。
            // 写回走后台线程并节流，避免拖动中频繁调 WASAPI。
            let mut volume = current_volume;
            let volume_ready = self
                .route_volume_at
                .get(&channel)
                .map(|at| at.elapsed() >= Duration::from_millis(250))
                .unwrap_or(true);
            let endpoint_scalar = self.route_volume_scalar(channel);
            let gain_db = self.channel_gain_db(channel);
            let muted_now = self
                .volume_source
                .map(|source| source.muted)
                .unwrap_or(false);
            let volume_tip = if muted_now {
                "系统静音中：引擎输出已经推 0，这台音箱现在不出声（端点音量仍保留，取消静音即恢复）。"
                    .to_string()
            } else {
                format!(
                    "实机输出：端点 {:.0}% ＋ 引擎补偿 {gain_db:+.1} dB。\n拖动直接按「实机输出」调节：{}。",
                    endpoint_scalar * 100.0,
                    if following {
                        "跟随中，改的是系统音量（两端一起变）"
                    } else if balanced {
                        "平衡校准生效中，两端一起变（保持平衡）"
                    } else {
                        "写回这台设备的端点音量"
                    }
                )
            };
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
                        palette().text,
                        true,
                    );
                    if slider.changed() {
                        if following {
                            let master = self.master_for_output(channel, volume);
                            if let Some(source) = self.volume_source.as_mut() {
                                source.scalar = master;
                            }
                        } else if balanced {
                            // 平衡生效时按「整对」调，单独改一侧会被补偿吃回去。
                            let scalars = self.balanced_pair_scalars(volume);
                            self.route_volumes.insert(RouteChannel::Left, scalars[0]);
                            self.route_volumes.insert(RouteChannel::Right, scalars[1]);
                        } else {
                            let endpoint = self.endpoint_scalar_for_output(channel, volume);
                            self.route_volumes.insert(channel, endpoint);
                        }
                        if (volume_ready || slider.drag_stopped()) && !current.is_empty() {
                            commit_volume = Some(volume);
                        }
                    }
                    slider
                },
            )
            .on_hover_text(volume_tip);

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
                palette().text,
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
                    palette().warn,
                ),
                Some(route) => match route.format_path.as_deref() {
                    Some(path) => (
                        if path.contains("autoconvert") {
                            "Auto"
                        } else {
                            "转换"
                        },
                        format!("开流路径：{path}"),
                        palette().muted,
                    ),
                    None => ("—", "开流路径：尚未开流".to_string(), palette().muted),
                },
                None => ("—", "开流路径：尚未开流".to_string(), palette().muted),
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

        // 音量写回：跟随生效时改系统音量（端点会被镜像，改端点毫无意义），
        // 否则把「实机输出」换算回端点音量再写回设备（节流：拖动中不频繁调 WASAPI）。
        if let Some(actual) = commit_volume {
            if !current.is_empty() {
                if following {
                    let master = self.master_for_output(channel, actual);
                    self.poll.request_set_source_volume(master);
                } else if balanced {
                    let scalars = self.balanced_pair_scalars(actual);
                    for (index, side) in [RouteChannel::Left, RouteChannel::Right]
                        .into_iter()
                        .enumerate()
                    {
                        let endpoint = self.configured_endpoint(side).to_string();
                        if endpoint.is_empty() {
                            continue;
                        }
                        self.route_volumes.insert(side, scalars[index]);
                        self.route_volume_at.insert(side, Instant::now());
                        self.poll.request_set_route_volume(endpoint, scalars[index]);
                    }
                } else {
                    let endpoint = self.endpoint_scalar_for_output(channel, actual);
                    self.route_volumes.insert(channel, endpoint);
                    self.route_volume_at.insert(channel, Instant::now());
                    self.poll
                        .request_set_route_volume(current.clone(), endpoint);
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

        card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("本机播放设备（{}）", devices.len()))
                        .size(16.0)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .button("刷新设备")
                        .on_hover_text("重新枚举本机播放设备、设备默认格式与系统默认输出")
                        .clicked()
                    {
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
                    palette().warn,
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
            let col_format =
                (available - col_device - col_role - col_action - spacing * 3.0).max(150.0);

            {
                let header_font = FontId::proportional(12.0);
                ui.horizontal(|ui| {
                    // 「设备」列表头：在列宽内画一个喇叭图标 + 文字，不改列宽，
                    // 避免表头和数据行错位。
                    {
                        let (cell_rect, _) =
                            ui.allocate_exact_size(Vec2::new(col_device, 18.0), Sense::hover());
                        let painter = ui.painter();
                        crate::icons::paint(
                            painter,
                            Rect::from_min_size(
                                cell_rect.left_top() + Vec2::new(1.0, 2.0),
                                Vec2::splat(14.0),
                            ),
                            crate::icons::Icon::Speaker,
                            palette().muted,
                            1.3,
                        );
                        painter.text(
                            cell_rect.left_center() + Vec2::new(19.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            "设备",
                            header_font.clone(),
                            palette().muted,
                        );
                    }
                    text_cell(
                        ui,
                        col_format,
                        18.0,
                        "格式",
                        header_font.clone(),
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        col_role,
                        18.0,
                        "角色",
                        header_font.clone(),
                        palette().muted,
                        false,
                    );
                    text_cell(
                        ui,
                        col_action,
                        18.0,
                        "操作",
                        header_font,
                        palette().muted,
                        false,
                    );
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
                        if is_default { palette().ok } else { palette().text },
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
                                    palette().err,
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
                                    palette().muted,
                                    false,
                                );
                            }
                            (None, false) => {
                                let cell_w = (col_format - 8.0).max(120.0);
                                ui.allocate_ui_with_layout(
                                    Vec2::new(cell_w, 22.0),
                                    Layout::left_to_right(Align::Center),
                                    |ui| {
                                        if btn_small(ui, true, "探测格式", theme::ButtonKind::Secondary)
                                            .on_hover_text("用独占模式逐个试探这个端点支持的格式")
                                            .clicked()
                                        {
                                            probe_one = Some(ep.id.clone());
                                        }
                                    },
                                );
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
                        palette().accent
                    } else if is_right {
                        palette().accent2
                    } else if is_default {
                        palette().ok
                    } else if is_capture {
                        palette().accent
                    } else {
                        palette().muted
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
                                    palette().muted,
                                    false,
                                );
                            } else {
                                if btn_small(ui, !is_left, "设为左", theme::ButtonKind::Secondary)
                                    .clicked()
                                {
                                    set_left = Some(ep.name.clone());
                                }
                                if btn_small(ui, !is_right, "设为右", theme::ButtonKind::Secondary)
                                    .clicked()
                                {
                                    set_right = Some(ep.name.clone());
                                }
                                if let Some(t) = &fixed {
                                    if btn_small(ui, true, "修正", theme::ButtonKind::Ghost)
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

    /// 手机投放页：AirPlay / 小米妙播接收端的开关、状态与手机端操作提示。
    ///
    /// 两个接收端都是独立 exe，声音直接写进 Windows 当前默认输出声卡；本页只负责
    /// 拉起/停止子进程，以及默认声卡变化时让它们跟着重启（见 [`ReceiverManager`]）。
    fn cast_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("手机投放").size(16.0).strong());
                ui.add_space(6.0);
                let running = self.cast.running();
                if !self.cast.is_any_running() {
                    pill(ui, "未接收", Color32::from_rgb(120, 128, 142), 84.0);
                } else {
                    pill(ui, "接收中", palette().ok, 72.0);
                }
                help(
                    ui,
                    "手机把音乐直接投到这台电脑：手机与电脑连同一个 Wi-Fi，在手机的隔空播放 / 小米妙播里选设备名 StereoLink。\n声音走 Windows 当前默认播放设备，和「声道路由」的左右分声道互不影响。",
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !running.is_empty() && btn(ui, "全部停止", theme::ButtonKind::Ghost).clicked()
                    {
                        self.cast.stop_all();
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(muted("设备名"));
                ui.label(RichText::new(crate::receiver::DEVICE_NAME).strong());
                ui.add_space(12.0);
                ui.label(muted("输出声卡"));
                match self.cast.output_device() {
                    Some(name) => ui.label(RichText::new(name).strong()),
                    None if self.cast.is_any_running() => ui.label(muted("读取中…")),
                    None => ui.label(muted("系统默认（接收端启动时读取）")),
                };
                ui.add_space(8.0);
                ui.label(muted("（跟随系统默认，切换后会自动重启接收端）"));
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let mut low_latency = self.cfg.cast.low_latency;
                if ui
                    .checkbox(
                        &mut low_latency,
                        RichText::new("妙播低延迟（约 0.2 秒）").size(12.0),
                    )
                    .on_hover_text(
                        "妙播 SDK 默认要攒 0.65~0.8 秒才出声（为了和小米「全屋播放」对齐），勾上后压到约 0.22 秒，只投本机时明显更跟手。\n代价：和小米音箱一起全屋播放时会和它们不同步。",
                    )
                    .changed()
                {
                    self.cfg.cast.low_latency = low_latency;
                    self.cfg.save();
                    self.cast.set_low_latency(low_latency);
                    self.toast(if low_latency {
                        "妙播已切到低延迟模式（约 0.2 秒）"
                    } else {
                        "妙播已恢复标准延迟（约 0.8 秒）"
                    });
                }
                ui.add_space(8.0);
                ui.label(muted("只影响小米妙播，AirPlay 不受影响"));
            });
            if self
                .cast
                .last_follow()
                .is_some_and(|at| at.elapsed().as_secs() < 90)
            {
                ui.label(muted("刚跟着系统默认声卡重启过一次接收端。"));
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);

            for kind in CastKind::ALL {
                self.cast_receiver_row(ui, kind);
            }

            ui.separator();
            ui.add_space(6.0);
            ui.label(muted(
                "第一次跑如果手机里看不到 StereoLink：让 Windows 防火墙放行接收端程序（首次启动会弹窗，选「允许」）。",
            ));
            ui.label(muted(
                "为什么是独立进程：AirPlay 接收端基于 LGPL-3.0 的 shairplay、妙播接收端基于 AGPL-3.0 的 FusionPlay SDK，都不能链接进主程序；独立 exe 运行，许可互不影响。",
            ));
        });
    }

    /// 手机投放页里的一行：状态、开始/停止、随面板启动开关、手机端操作提示。
    fn cast_receiver_row(&mut self, ui: &mut egui::Ui, kind: CastKind) {
        let status = self.cast.status(kind).clone();
        let exe = self.cast.exe(kind).map(|path| path.to_path_buf());
        let running = status.is_running();
        let mut toggle = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(kind.label()).size(14.0).strong());
            ui.add_space(6.0);
            let (color, text) = match status {
                CastStatus::Running => (palette().ok, status.text()),
                CastStatus::Failed(_) => (palette().err, status.text()),
                CastStatus::Stopped => (palette().muted, status.text()),
            };
            pill(ui, text, color, 72.0);
            if let Some(error) = status.error() {
                help(ui, error);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let button_kind = if running {
                    theme::ButtonKind::Danger
                } else {
                    theme::ButtonKind::Primary
                };
                let button_label = if running { "停止" } else { "开始" };
                if btn_enabled(ui, running || exe.is_some(), button_label, button_kind).clicked() {
                    toggle = true;
                }
                ui.add_space(10.0);
                let mut auto = match kind {
                    CastKind::AirPlay => self.cfg.cast.auto_airplay,
                    CastKind::MiPlay => self.cfg.cast.auto_miplay,
                };
                if ui
                    .checkbox(&mut auto, RichText::new("随面板启动").size(12.0))
                    .on_hover_text("勾选后 StereoLink 一启动就把这个接收端拉起来")
                    .changed()
                {
                    match kind {
                        CastKind::AirPlay => self.cfg.cast.auto_airplay = auto,
                        CastKind::MiPlay => self.cfg.cast.auto_miplay = auto,
                    }
                    self.cfg.save();
                }
            });
        });
        if let Some(error) = status.error() {
            ui.label(RichText::new(error).size(12.0).color(palette().err));
        }
        ui.label(muted(kind.phone_hint()));
        // 音量：妙播写接收端 stdin 实时调（手机端也能继续调，事件会回来更新这里）；
        // AirPlay 的音量协商在手机端，面板不给控件。
        match kind {
            CastKind::MiPlay => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("音量").size(12.0));
                    // 以接收端自报值为准：拖动 → 写 stdin → 事件行回来更新 `cast.volume`。
                    let mut volume = self.cast.volume(kind).unwrap_or(50);
                    let slider = ui
                        .add_enabled(running, egui::Slider::new(&mut volume, 0..=100).suffix("%"));
                    if slider.changed() {
                        match self.cast.set_volume(kind, volume) {
                            Ok(()) => self.cast_volume_error = None,
                            Err(error) => {
                                tracing::warn!("[投放] 设置妙播音量失败：{error}");
                                self.cast_volume_error = Some(error);
                            }
                        }
                    }
                    // 拖动过程只更新滑杆（每帧都写 stdin，接收端实时生效），松手才记一行日志。
                    if (slider.changed() && !slider.dragged())
                        || (slider.drag_stopped() && !slider.changed())
                    {
                        if let Some(volume) = self.cast.volume(kind) {
                            tracing::info!("[投放] 妙播音量设为 {volume}（面板）");
                        }
                    }
                    if !running {
                        ui.label(muted("开始接收后可调"));
                    }
                });
                if let Some(error) = &self.cast_volume_error {
                    ui.label(RichText::new(error).size(12.0).color(palette().err));
                }
            }
            CastKind::AirPlay => {
                ui.label(muted("音量由手机端控制"));
            }
        }
        match &exe {
            Some(path) => ui.label(
                RichText::new(format!("接收端：{}", path.display()))
                    .size(11.5)
                    .color(palette().muted),
            ),
            None => ui.label(
                RichText::new(format!(
                    "还没找到 {}：{}",
                    kind.exe_name(),
                    crate::receiver::BUILD_HINT
                ))
                .size(12.0)
                .color(palette().warn),
            ),
        };
        if toggle {
            match self.cast.toggle(kind) {
                Ok(()) => {}
                Err(error) => self.toast(error),
            }
        }
        ui.add_space(10.0);
    }

    fn log_card(&mut self, ui: &mut egui::Ui) {
        let count = self.log.len();
        let lines = self.log.lines();
        let font = self.font.clone();
        let mut clear = false;
        let mut copy_diag = false;
        let mut copy_log = false;
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("日志").size(16.0).strong());
                text_cell(
                    ui,
                    72.0,
                    18.0,
                    &format!("{count} 行"),
                    FontId::proportional(12.0),
                    palette().muted,
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
                .fill(palette().log_bg)
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
                                .color(palette().warn),
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
                .map(|hint| format!("{} {}", hint.glyph(), hint.text))
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
                    // 重绘，CPU 回到个位数；窗口本身创建时是 `with_taskbar(true)`，
                    // 最小化后任务栏按钮仍在（点任务栏或托盘图标都能唤回窗口）。
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
}

fn log_line(ui: &mut egui::Ui, line: &LogLine) {
    let color = match line.level {
        tracing::Level::ERROR => palette().err,
        tracing::Level::WARN => palette().warn,
        tracing::Level::INFO => palette().text,
        _ => palette().muted,
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
        let frame_start = Instant::now();
        // 命令行控制面：把排队的请求处理掉（纯内存操作，几十微秒）。
        // 「取出→处理→放回」是为了在闭包里能可变借用 self。
        if let Some(mut server) = self.control.take() {
            server.drain(|request| self.handle_control(request));
            self.control = Some(server);
        }
        if self.pending_quit {
            self.pending_quit = false;
            tracing::info!("收到命令行退出请求，立即退出");
            // 给控制面线程一点时间把 {"stopping":true} 写回客户端，
            // 否则脚本看到的是「连接被重置」而不是正常应答。
            std::thread::sleep(Duration::from_millis(150));
            // 不能只发 ViewportCommand::Close：关闭行为可能是「缩小至托盘」，
            // 命令行部署的退出必须真退出（systemd/launchd 停服务同理）。
            self.stop_routing(true);
            self.cast.stop_all();
            self.restore_target_volumes_blocking();
            self.cfg.save();
            // `process::exit` 不跑析构：控制文件得自己删，否则下一条命令看到陈文件。
            control::clear_control_file();
            std::process::exit(0);
        }
        // 帧率上限（60 fps）。
        //
        // winit 每收到一个鼠标移动事件就请求一次重绘，鼠标快速扫过时实测能跑到 170~200 fps。
        // 在这种远高于显示器刷新率的呈现节奏下，会偶发「整窗只剩清屏色」的一帧被推上屏
        // （DXGI 桌面复制 120 fps 采集：不设上限时 6 轮里 3 轮抓到 1~4 帧，每帧 8~33 ms；
        // 限制到 60 fps 后 12 轮全部为 0；App 自身的帧日志显示这些时刻它正在正常整帧绘制，
        // 所以问题出在 present/swap 侧，不是内容）。限制到 60 fps 后不再复现，顺带省 CPU/GPU。
        //
        // 放在 update 开头而不是末尾：睡够间隔再画，呈现时刻就被压到 60 Hz 附近。
        const MAX_FPS: f64 = 60.0;
        let min_frame = Duration::from_secs_f64(1.0 / MAX_FPS);
        let since_last = self.last_frame_at.elapsed();
        if since_last < min_frame {
            std::thread::sleep(min_frame - since_last);
        }
        // 最小化/隐藏期间把心跳降到 1s：既省 CPU，又给托盘事件留一层兜底刷新。
        // 窗口不可见时**不能**用 250ms 心跳（见 handle_close 的注释）。
        // 声道路由页在路由运行时要画波形/频谱，心跳提到 100ms（10 fps）；
        // 只有这一页、只有引擎在跑时才加速，其它情况维持 250ms。
        // 动效拆完后这里不再有 60 fps 分支（toast 自己按过期时间唤醒，见 `Toasts::show`）。
        let routing_animating =
            !self.window_hidden && self.page == Page::Routing && self.engine.is_some();
        ctx.request_repaint_after(if self.window_hidden {
            Duration::from_secs(1)
        } else if routing_animating {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(250)
        });
        self.sync_theme(ctx);
        self.ensure_tray(ctx);
        self.handle_close(ctx);
        self.poll_tray(ctx);

        let tick_start = Instant::now();
        self.tick();
        let tick_ms = tick_start.elapsed().as_secs_f32() * 1000.0;

        // 行内波形/频谱按真实帧间隔推进（幅度来自引擎上报的 RouteStats.level）。
        let dt = self.last_frame_at.elapsed().as_secs_f32();
        self.last_frame_at = Instant::now();
        self.advance_waves(dt);

        // 柔光背景铺满整窗、画在最底层（LayerId::background），玻璃面板/卡片半透明叠在上面。
        theme::paint_background(ctx, ctx.screen_rect(), is_dark());

        // 顶栏已取消：身份/状态在侧边栏底部，诊断徽章在路由页顶部。
        self.sidebar(ctx);
        self.main_panel(ctx);

        // 浮层提示：先入队本帧攒下的文案，再画到右下角（最上层，不参与布局）。
        if let Some(text) = self.pending_toast.take() {
            self.toasts.push(ctx, text);
        }
        self.toasts.show(ctx, is_dark());

        if self.frame_log > 0 {
            let total_ms = frame_start.elapsed().as_secs_f32() * 1000.0;
            self.last_frame_ms = total_ms;
            if self.frame_log >= 2 {
                tracing::info!(
                    "[帧] {:.1} ms tick={:.1} ms draw={:.1} ms",
                    total_ms,
                    tick_ms,
                    (total_ms - tick_ms).max(0.0)
                );
            } else if total_ms > 16.0 {
                tracing::warn!(
                    "[帧] {:.1} ms tick={:.1} ms draw={:.1} ms",
                    total_ms,
                    tick_ms,
                    (total_ms - tick_ms).max(0.0)
                );
            }
        }
    }

    /// 清屏色：玻璃面板是半透明的，底层必须是不透明的主题底色（否则边缘会透黑）。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        palette().bg.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.exit_handled {
            return;
        }
        self.exit_handled = true;
        self.stop_routing(true);
        // 手机投放接收端是子进程，面板退出时要一起收掉，否则会留下占着端口的孤儿进程。
        self.cast.stop_all();
        // 进程马上要退出：后台线程可能来不及再跑一次 tick，这里同步兜底还原音量。
        self.restore_target_volumes_blocking();
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
