//! StereoLink 立体声链 —— 桌面面板。
//!
//! 界面按「一屏看懂」组织：顶栏是身份与总状态，中间左侧是声道路由与设备表，
//! 右侧是虚拟声卡、诊断与说明。所有动作（开始/停止/自检音/切默认输出）都在一屏内。
//!
//! 设计要点：
//! - 面板只做两件事：把虚拟声卡的回环流拆成左右两路推给两台蓝牙音箱；把 Windows
//!   默认输出切到虚拟声卡（并在退出时切回）。
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
use crate::virtual_card;

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

/// 小圆角标签。
fn pill(ui: &mut egui::Ui, text: &str, bg: Color32) {
    let font = FontId::proportional(12.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, Color32::BLACK);
    let pad = Vec2::new(9.0, 3.0);
    let size = galley.size() + pad * 2.0;
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), bg);
    painter.galley(rect.min + pad, galley, Color32::BLACK);
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

fn mini(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(TEXT).monospace()
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
    live_default_is_virtual: bool,
    live_default_at: Option<Instant>,
    auto_started: bool,
    exit_handled: bool,
    /// 虚拟声卡设备改名输入框。
    rename_input: String,
    /// 改名进行中（等 UAC / 脚本执行）。
    rename_busy: bool,
    /// 后台改名线程的结果通道。
    rename_rx: Option<std::sync::mpsc::Receiver<Result<Vec<String>, String>>>,
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
            rename_input: cfg.virtual_card.name.clone(),
            cfg,
            engine: None,
            stats: None,
            endpoints: Vec::new(),
            endpoints_error: None,
            log,
            notice: None,
            font,
            live_default_output: None,
            live_default_is_virtual: false,
            live_default_at: None,
            auto_started: false,
            exit_handled: false,
            rename_busy: false,
            rename_rx: None,
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
        self.poll_rename();
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
        let is_virtual = ep
            .as_ref()
            .map(|e| {
                audio_default::is_virtual(e)
                    || self
                        .cfg
                        .virtual_card
                        .endpoint_id
                        .as_deref()
                        .map(|id| id.eq_ignore_ascii_case(&e.id))
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        self.live_default_is_virtual = is_virtual;
        // 记住最后一次见到的「真实」默认输出：即使启动时默认已经是虚拟声卡
        // （上次被强杀没来得及恢复），退出时也能切回去。
        if !is_virtual {
            if let Some(id) = ep.as_ref().map(|e| e.id.clone()) {
                if self.cfg.virtual_card.last_real_output.as_deref() != Some(id.as_str()) {
                    self.cfg.virtual_card.last_real_output = Some(id);
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
        self.remember_virtual_endpoint();
    }

    /// 把「当前虚拟声卡端点」的 id/名字记进配置。
    ///
    /// 名字可以被用户改，端点 id 不会变，所以优先认 id：改过名的虚拟声卡
    /// （比如用户起名「我的声卡」）也能继续被识别、自动切默认输出、当采集源。
    fn remember_virtual_endpoint(&mut self) {
        let remembered = self.cfg.virtual_card.endpoint_id.clone();
        let Some(ep) = audio_default::resolve_virtual_endpoint(remembered.as_deref()) else {
            return;
        };
        if ep.flow != DataFlow::Render {
            return;
        }
        // 配置里存“设备名”（去掉 `扬声器 (` 前后缀），不是完整端点名——改名输入框用它。
        let device_name = short_endpoint(&ep.name);
        let changed = self.cfg.virtual_card.endpoint_id.as_deref() != Some(ep.id.as_str())
            || self.cfg.virtual_card.name != device_name;
        self.cfg.virtual_card.endpoint_id = Some(ep.id.clone());
        self.cfg.virtual_card.name = device_name;
        if changed {
            self.save();
        }
    }

    /// 记住的虚拟声卡端点（在已枚举的列表里找，避免重复枚举设备）。
    fn remembered_virtual_endpoint(&self) -> Option<&EndpointInfo> {
        let id = self.cfg.virtual_card.endpoint_id.as_deref()?;
        self.endpoints
            .iter()
            .find(|e| e.flow == DataFlow::Render && e.id.eq_ignore_ascii_case(id))
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

    /// 启动虚拟声卡改名（后台线程 + UAC，不阻塞界面）。
    fn start_rename(&mut self) {
        let name = self.rename_input.trim().to_string();
        if let Err(e) = virtual_card::validate_name(&name) {
            self.toast(format!("{e}"));
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.rename_rx = Some(rx);
        self.rename_busy = true;
        tracing::info!("[改名] 请求把虚拟声卡改名为「{name}」，等待管理员授权…");
        std::thread::spawn(move || {
            let result = virtual_card::rename_virtual_card(&name).map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    /// 轮询改名线程的结果。
    fn poll_rename(&mut self) {
        let Some(rx) = &self.rename_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(lines)) => {
                self.rename_rx = None;
                self.rename_busy = false;
                let name = self.rename_input.trim().to_string();
                for line in lines.iter().rev().take(3).rev() {
                    tracing::info!("[改名] {line}");
                }
                self.refresh_endpoints();
                // 改名会重建端点，端点 id 可能变；按新名字重新锁定虚拟声卡。
                if let Some(ep) = self
                    .endpoints
                    .iter()
                    .find(|e| e.flow == DataFlow::Render && e.name.contains(&name))
                {
                    self.cfg.virtual_card.endpoint_id = Some(ep.id.clone());
                    self.cfg.virtual_card.name = short_endpoint(&ep.name);
                }
                self.cfg.save();
                self.refresh_formats();
                self.toast(format!("虚拟声卡已改名为「{name}」"));
            }
            Ok(Err(e)) => {
                self.rename_rx = None;
                self.rename_busy = false;
                tracing::warn!("[改名] 失败：{e}");
                self.toast(format!("改名失败：{e}"));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.rename_rx = None;
                self.rename_busy = false;
                self.toast("改名线程异常结束");
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn render_names(&self) -> Vec<String> {
        self.render_devices().into_iter().map(|e| e.name).collect()
    }

    /// 虚拟声卡端点名（优先记住的端点 id，其次按名字提示找）。
    fn virtual_name(&self) -> Option<String> {
        if let Some(ep) = self.remembered_virtual_endpoint() {
            return Some(ep.name.clone());
        }
        audio_default::virtual_endpoint().map(|e| e.name)
    }

    fn source_endpoint_name(&self) -> Option<String> {
        let configured = self.cfg.split.source_endpoint.trim();
        if !configured.is_empty() {
            return Some(configured.to_string());
        }
        self.virtual_name()
    }

    fn route_of(&self, channel: RouteChannel) -> Option<&RouteStats> {
        self.stats
            .as_ref()?
            .routes
            .iter()
            .find(|r| r.channel == channel)
    }

    fn route_engine_config(&self) -> Option<RouteEngineConfig> {
        let source = self.source_endpoint_name()?;
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
            tracing::warn!("开始路由失败：还没有为左/右声道选择输出设备，或找不到虚拟声卡");
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

    fn apply_default_output(&mut self) {
        if !self.cfg.virtual_card.auto_switch_default {
            self.refresh_default_output(true);
            return;
        }
        let remembered = self.cfg.virtual_card.endpoint_id.clone();
        let vad = audio_default::resolve_virtual_endpoint(remembered.as_deref());
        let outcome = match &vad {
            Some(ep) => audio_default::switch_to_endpoint_id(&ep.id, ep),
            None => audio_default::switch_to_virtual(),
        };
        match outcome {
            Ok(outcome) => {
                if let Some(id) = outcome.previous_id {
                    if self.cfg.virtual_card.previous_output.is_none() {
                        self.cfg.virtual_card.previous_output = Some(id);
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
        // 优先用本次切换前记录的；没有（启动时默认已经是虚拟声卡）就用最后一次
        // 见到的真实默认输出兜底。
        let id = self
            .cfg
            .virtual_card
            .previous_output
            .clone()
            .or_else(|| self.cfg.virtual_card.last_real_output.clone());
        let Some(id) = id else {
            if self.live_default_is_virtual {
                tracing::warn!(
                    "[默认输出] 没有可恢复的记录（启动时默认输出已经是虚拟声卡），退出后请在 Windows 声音设置里手动切回音箱"
                );
            }
            return;
        };
        match audio_default::switch_to_id(&id) {
            Ok(_) => {
                self.cfg.virtual_card.previous_output = None;
                self.save();
                self.refresh_default_output(true);
                tracing::info!("[默认输出] 已恢复切换前的默认输出");
            }
            Err(e) => tracing::warn!("[默认输出] 恢复失败：{e}"),
        }
    }

    fn play_test_tone(&mut self) {
        let Some(source) = self.source_endpoint_name() else {
            self.toast("找不到虚拟声卡采集源，无法播放自检音");
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
        let virtual_name = self.virtual_name();

        match &virtual_name {
            Some(name) => hints.push(Hint {
                tone: Tone::Ok,
                text: format!("虚拟声卡就绪：{}", short_endpoint(name)),
            }),
            None => hints.push(Hint {
                tone: Tone::Err,
                text: "没找到虚拟声卡端点（驱动可能被卸载）".to_string(),
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

        if self.engine.is_some() && !self.live_default_is_virtual {
            let current = self
                .live_default_output
                .as_deref()
                .map(short_endpoint)
                .unwrap_or_else(|| "未知".to_string());
            hints.push(Hint {
                tone: Tone::Warn,
                text: format!("系统默认输出是「{current}」：播放器的声音不会进分离链路"),
            });
        } else if self.live_default_is_virtual {
            hints.push(Hint {
                tone: Tone::Ok,
                text: "系统默认输出 = 虚拟声卡".to_string(),
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
                        pill(ui, "运行中", OK);
                    } else {
                        pill(ui, "已停止", Color32::from_rgb(120, 128, 142));
                    }
                    ui.add_space(4.0);

                    if self.live_default_is_virtual {
                        pill(ui, "默认输出 = 虚拟声卡", ACCENT);
                    } else {
                        pill(ui, "默认输出未切换", WARN);
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
                        if let Some((text, _)) = &self.notice {
                            ui.label(RichText::new(text.clone()).size(13.0).color(ACCENT));
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
                        self.virtual_card(ui);
                        ui.add_space(12.0);
                        self.diagnostics_card(ui);
                        ui.add_space(12.0);
                        self.about_card(ui);
                    });
            });
    }

    fn virtual_card(&mut self, ui: &mut egui::Ui) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("虚拟声卡").size(15.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    badge(ui, "WASAPI 回环", ACCENT);
                });
            });
            ui.add_space(6.0);

            let name = self
                .virtual_name()
                .map(|n| short_endpoint(&n))
                .unwrap_or_else(|| "未找到".to_string());
            ui.label(RichText::new(name).size(17.0).strong().color(TEXT));
            ui.add_space(2.0);

            let (current, is_virtual) = (
                self.live_default_output
                    .as_deref()
                    .map(short_endpoint)
                    .unwrap_or_else(|| "未知".to_string()),
                self.live_default_is_virtual,
            );
            ui.horizontal(|ui| {
                ui.label(muted("系统默认输出："));
                if is_virtual {
                    ui.label(RichText::new(format!("{current} √")).size(12.5).color(OK));
                } else {
                    ui.label(RichText::new(current).size(12.5).color(WARN));
                }
            });
            ui.add_space(6.0);

            if ui
                .checkbox(
                    &mut self.cfg.virtual_card.auto_switch_default,
                    "开始路由时自动切为默认输出（退出时切回）",
                )
                .changed()
            {
                self.cfg.save();
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("切为默认输出").clicked() {
                    let remembered = self.cfg.virtual_card.endpoint_id.clone();
                    let vad = audio_default::resolve_virtual_endpoint(remembered.as_deref());
                    let outcome = match &vad {
                        Some(ep) => audio_default::switch_to_endpoint_id(&ep.id, ep),
                        None => audio_default::switch_to_virtual(),
                    };
                    match outcome {
                        Ok(outcome) => {
                            if let Some(id) = outcome.previous_id {
                                if self.cfg.virtual_card.previous_output.is_none() {
                                    self.cfg.virtual_card.previous_output = Some(id);
                                    self.cfg.save();
                                }
                            }
                            self.refresh_default_output(true);
                            self.toast("已把默认输出切到虚拟声卡");
                            tracing::info!("[默认输出] {}", outcome.message);
                        }
                        Err(e) => {
                            tracing::warn!("切换默认输出失败：{e}");
                            self.toast(format!("切换失败：{e}"));
                        }
                    }
                }
                let has_previous = self.cfg.virtual_card.previous_output.is_some();
                if ui
                    .add_enabled(has_previous, egui::Button::new("恢复原输出"))
                    .on_hover_text("切回开始路由前的默认输出")
                    .clicked()
                {
                    self.restore_previous_output();
                    self.refresh_default_output(true);
                }
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(muted("设备名"));
                let edit = ui.add_enabled(
                    !self.rename_busy,
                    egui::TextEdit::singleline(&mut self.rename_input)
                        .desired_width(150.0)
                        .hint_text(virtual_card::SUGGESTED_NAME),
                );
                edit.on_hover_text("Windows 声音列表里显示成「扬声器 (这个名字)」");
                if ui
                    .add_enabled(
                        !self.rename_busy,
                        egui::Button::new("应用改名").small(),
                    )
                    .on_hover_text("需要管理员授权（会弹 UAC），改完自动禁用/启用设备")
                    .clicked()
                {
                    self.start_rename();
                }
                if self.rename_busy {
                    ui.spinner();
                    ui.label(muted("等待授权…"));
                }
            });

            ui.add_space(6.0);
            ui.label(muted(
                "把 Windows 输出选成虚拟声卡后，播放器声音才会流进分离链路。",
            ));
        });
    }

    fn diagnostics_card(&mut self, ui: &mut egui::Ui) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("诊断").size(15.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .small_button("复制")
                        .on_hover_text("复制全部诊断信息到剪贴板")
                        .clicked()
                    {
                        let text = self
                            .hints()
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
            for hint in self.hints() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(hint.icon())
                            .size(12.0)
                            .color(hint.color())
                            .monospace(),
                    );
                    ui.label(RichText::new(hint.text).size(12.5).color(TEXT));
                });
            }
        });
    }

    fn about_card(&mut self, ui: &mut egui::Ui) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("怎么用").size(15.0).strong());
            ui.add_space(4.0);
            for (i, line) in [
                "为左、右声道各选一台蓝牙音箱（先在本机蓝牙里连上）。",
                "点「开始路由」，面板会把默认输出切到虚拟声卡。",
                "用播放器放歌/看视频，声音就被拆成左右两路。",
                "退出面板会自动停路由并恢复原来的默认输出。",
            ]
            .iter()
            .enumerate()
            {
                ui.label(muted(format!("{}. {line}", i + 1)));
            }
            ui.add_space(6.0);
            let latency = self
                .route_engine_config()
                .map(|c| c.software_latency_ms())
                .unwrap_or(0);
            ui.label(muted(format!(
                "软件侧延迟约 {latency} ms（抖动缓冲 {} ms + 渲染缓冲 40 ms）",
                self.cfg.split.queue_ms
            )));
            ui.add_space(4.0);
            if ui.checkbox(&mut self.cfg.show_log, "显示日志面板").changed() {
                self.cfg.save();
            }
            if let Some(font) = &self.font {
                ui.label(muted(format!("字体：{font}")));
            } else {
                ui.label(
                    RichText::new("没找到中文字体：中文可能显示为方块")
                        .size(12.0)
                        .color(WARN),
                );
            }
            let present = fonts::candidates_present();
            ui.label(muted(format!("系统候选字体：{}", present.join(" / "))));
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
                    pill(ui, "分离运行中", OK);
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

            for channel in [RouteChannel::Left, RouteChannel::Right] {
                self.route_row(ui, channel);
                ui.add_space(6.0);
            }

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label(muted("增益"));
                if ui
                    .add(egui::Slider::new(&mut self.cfg.split.gain, 0.0..=2.0).step_by(0.05))
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
            ui.label(RichText::new("声学自校验：用麦克风量出两台音箱的真实时差").strong());

            let mic_label = self
                .sync_mics
                .iter()
                .find(|m| m.id() == self.sync_mic)
                .map(|m| m.name().to_string())
                .unwrap_or_default();
            let mut pick_mic: Option<String> = None;
            let mut action: Option<&'static str> = None;
            ui.horizontal_wrapped(|ui| {
                egui::ComboBox::from_id_salt("sync_mic")
                    .width(230.0)
                    .selected_text(if mic_label.is_empty() {
                        RichText::new("（未选择麦克风）").color(MUTED)
                    } else {
                        RichText::new(short_endpoint(&mic_label)).color(TEXT)
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
            if !self.sync_status.is_empty() {
                ui.label(RichText::new(&self.sync_status).color(MUTED).size(12.0));
            }

            if let Some(stats) = &self.stats {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(muted(format!(
                        "采集 {:.1}s · {:.2} MB",
                        stats.captured_frames as f64 / 44_100.0,
                        stats.frame_bytes as f64 / 1_048_576.0
                    )));
                    ui.add_space(8.0);
                    ui.label(muted(format!(
                        "源：{}",
                        short_endpoint(&stats.source_device)
                    )));
                });
            }
        });
    }

    fn route_row(&mut self, ui: &mut egui::Ui, channel: RouteChannel) {
        let color = channel_color(channel);
        let names = self.render_names();
        let current = self.configured_endpoint(channel).to_string();
        let mut chosen = current.clone();
        let label = channel_label(channel);

        ui.horizontal(|ui| {
            badge(ui, label, color);
            ui.add_space(2.0);
            let width = (ui.available_width() - 260.0).max(160.0);
            egui::ComboBox::from_id_salt(format!("route_{label}"))
                .width(width)
                .selected_text(if current.is_empty() {
                    RichText::new("（未选择设备）").color(MUTED)
                } else {
                    RichText::new(short_endpoint(&current)).color(TEXT)
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
                });

            if let Some(route) = self.route_of(channel) {
                // 电平条用感知刻度（sqrt），否则 -30 dB 的音乐只有一两像素，看着像没声音。
                level_bar(ui, 64.0, route.level.sqrt(), color);
                let stats = ui.label(mini(format!(
                    "{:>3.0}% · {:.2} MB · {:.0} ms · 欠载 {}",
                    route.level * 100.0,
                    route.sent_bytes as f64 / 1_048_576.0,
                    route.queued_ms,
                    route.underruns
                )));
                // 开流走的是哪条路（自动转换 / 原生混音格式 + 软件转换），排障时很关键。
                if let Some(path) = route.format_path.as_ref() {
                    stats.on_hover_text(format!("开流路径：{path}"));
                }
                if let Some(err) = route.error.as_ref() {
                    ui.label(RichText::new("链路错误").color(ERR).size(12.0))
                        .on_hover_text(err);
                }
            } else {
                level_bar(ui, 64.0, 0.0, color);
                ui.label(muted("未运行"));
            }
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
            ui.label(muted(
                "所有已启用的播放设备都在这里，可以任意指定哪一个当左声道、哪一个当右声道。",
            ));
            ui.add_space(6.0);

            if devices.is_empty() {
                ui.label(RichText::new("没有枚举到播放设备").color(WARN));
                return;
            }

            egui::Grid::new("device_grid")
                .num_columns(4)
                .spacing([14.0, 7.0])
                .show(ui, |ui| {
                    ui.label(muted("设备"));
                    ui.label(muted("格式"));
                    ui.label(muted("角色"));
                    ui.label(muted("操作"));
                    ui.end_row();

                    for ep in &devices {
                        let is_virtual = audio_default::is_virtual(ep);
                        let is_default = live_default.as_deref() == Some(ep.name.as_str());
                        let is_left = left == ep.name;
                        let is_right = right == ep.name;

                        let mut title = RichText::new(short_endpoint(&ep.name)).color(TEXT);
                        if is_default {
                            title = title.strong();
                        }
                        ui.label(title);
                        let current = self.format_cache.get(&ep.id).cloned().flatten();
                        let supported = self.format_supported.get(&ep.id).cloned().flatten();
                        let probing = self.format_probing.contains(&ep.id);
                        ui.horizontal(|ui| {
                            match (&supported, probing) {
                                // 只列这个端点**真正支持**的格式。
                                (Some(list), _) if !list.is_empty() => {
                                    let choice = self
                                        .format_choices
                                        .entry(ep.id.clone())
                                        .or_insert(list[0]);
                                    let before = *choice;
                                    let in_list = list.contains(choice);
                                    let selected = if in_list {
                                        choice.describe()
                                    } else {
                                        format!("{}（当前格式不受支持）", choice.describe())
                                    };
                                    egui::ComboBox::from_id_salt(format!("fmt_{}", ep.id))
                                        .width(190.0)
                                        .selected_text(RichText::new(selected).size(12.0))
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
                                        format_change = Some((
                                            ep.id.clone(),
                                            short_endpoint(&ep.name),
                                        ));
                                    }
                                }
                                (Some(_), _) => {
                                    badge(ui, "无支持格式", ERR);
                                }
                                (None, true) => {
                                    ui.label(muted("探测格式中…"));
                                }
                                (None, false) => {
                                    if ui
                                        .small_button("探测格式")
                                        .on_hover_text("用独占模式逐个试探这个端点支持的格式")
                                        .clicked()
                                    {
                                        probe_one = Some(ep.id.clone());
                                    }
                                }
                            }
                            match &current {
                                Some(f) => {
                                    let ok = supported.as_ref().map(|l| l.contains(f));
                                    match ok {
                                        Some(false) => {
                                            let fixed = supported
                                                .as_ref()
                                                .and_then(|l| Self::closest_supported(l, f));
                                            let hint = match &fixed {
                                                Some(t) => format!(
                                                    "当前设备格式 {} 不在这个端点的独占支持列表里（共享模式仍可用）。\n最接近的受支持格式：{}",
                                                    f.describe(),
                                                    t.describe()
                                                ),
                                                None => format!(
                                                    "当前设备格式 {} 不在独占支持列表里（共享模式仍可用）",
                                                    f.describe()
                                                ),
                                            };
                                            badge(ui, "不受支持", WARN).on_hover_text(hint);
                                            if let Some(t) = fixed {
                                                if ui
                                                    .small_button("修正")
                                                    .on_hover_text(format!("改成 {}", t.describe()))
                                                    .clicked()
                                                {
                                                    fix_format =
                                                        Some((ep.id.clone(), short_endpoint(&ep.name), t));
                                                }
                                            }
                                        }
                                        _ => {
                                            ui.label(mini(f.describe()))
                                                .on_hover_text("当前设备默认格式");
                                        }
                                    }
                                }
                                None => {
                                    badge(ui, "读取失败", ERR);
                                }
                            }
                        });

                        ui.horizontal(|ui| {
                            if is_default {
                                badge(ui, "默认输出", OK);
                            }
                            if is_virtual {
                                badge(ui, "采集源", ACCENT);
                            }
                            if is_left {
                                badge(ui, "左", ACCENT);
                            }
                            if is_right {
                                badge(ui, "右", ACCENT2);
                            }
                            if !is_default && !is_virtual && !is_left && !is_right {
                                ui.label(muted("—"));
                            }
                        });

                        ui.horizontal(|ui| {
                            if is_virtual {
                                ui.label(muted("虚拟声卡是采集源"));
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
                            }
                        });
                        ui.end_row();
                    }
                });
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
                ui.label(muted(format!("{count} 行")));
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
}
