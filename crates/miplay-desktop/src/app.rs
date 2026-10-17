//! 小米妙播投放面板 —— 桌面窗口主体。
//!
//! 结构：
//! * 顶栏：引擎状态、网页控制地址、重启引擎
//! * 采集栏：当前采集端点、实时左右声道电平、静默提示、测试音
//! * 右侧栏：引擎设置（端口/增益/左右分离/采集端点/音量）、自检信息
//! * 底部：日志面板（tracing 事件）
//! * 中间：设备列表 + 诊断建议，可勾选后一键投放

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use eframe::egui;
use miplay_server::hub::engine::{DeviceSnapshot, EngineConfig, EngineHandle, EngineSnapshot, Phase};

use crate::config::DesktopConfig;
use crate::logbuf::{LogBuffer, LogLine};

const GREEN: egui::Color32 = egui::Color32::from_rgb(96, 200, 132);
const AMBER: egui::Color32 = egui::Color32::from_rgb(236, 184, 76);
const RED: egui::Color32 = egui::Color32::from_rgb(232, 110, 110);
const GREY: egui::Color32 = egui::Color32::from_rgb(150, 156, 164);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(124, 172, 255);

/// 一条短暂提示（10 秒后自动消失）。
struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

/// 主窗口。
pub struct DesktopApp {
    /// 持久化配置（含引擎参数草稿）。
    cfg: DesktopConfig,
    /// 引擎当前真的在用的参数，用来提示「有改动未应用」。
    running: EngineConfig,
    /// 引擎句柄；重启期间会短暂为 `None`。
    engine: Option<EngineHandle>,
    /// 最新状态快照（每帧拉一次）。
    snap: EngineSnapshot,
    /// 日志缓冲。
    log: LogBuffer,
    /// 设备勾选状态（id → 是否勾选）。
    selected: BTreeMap<String, bool>,
    /// 系统输出端点名单（设置里的采集端点下拉框）。
    endpoints: Vec<String>,
    /// 一次性提示。
    notice: Option<Notice>,
    /// 实际加载的中文字体路径。
    font: Option<String>,
    /// 是否已经提示过「字体缺失」。
    font_warned: bool,
    /// 这一轮引擎是否已经做过「自动投放」（引擎重启后重置）。
    auto_started: bool,
    /// 上一帧「系统默认输出是否虚拟声卡」，用来检测用户手动切换输出设备。
    last_output_was_vad: bool,
}

impl DesktopApp {
    /// 建窗口：装字体、读配置、起引擎。
    pub fn new(cc: &eframe::CreationContext<'_>, log: LogBuffer) -> Self {
        Self::setup_style(&cc.egui_ctx);

        let font = crate::fonts::install(&cc.egui_ctx);
        match &font {
            Some(path) => tracing::info!("中文字体：{path}"),
            None => tracing::warn!(
                "系统里没找到可用的中文字体，界面中文可能显示为方块（可用环境变量 MIPLAY_FONT 指定 .ttf/.ttc）"
            ),
        }

        let cfg = DesktopConfig::load();
        let running = cfg.engine.clone();
        let engine = Some(EngineHandle::start(cfg.engine.clone()));
        let snap = engine
            .as_ref()
            .map(|e| e.snapshot())
            .unwrap_or_default();

        let mut endpoints: Vec<String> = miplay_audio::list_endpoints()
            .map(|list| list.into_iter().map(|e| e.name).collect())
            .unwrap_or_default();
        endpoints.sort();
        endpoints.dedup();
        tracing::info!("系统里共有 {} 个音频端点", endpoints.len());

        let mut selected = BTreeMap::new();
        for d in &snap.devices {
            let want = if cfg.selected.is_empty() {
                d.online && !cfg.ignored_devices.contains(&d.id)
            } else {
                cfg.selected.contains(&d.id) && !cfg.ignored_devices.contains(&d.id)
            };
            selected.insert(d.id.clone(), want);
        }

        Self {
            cfg,
            running,
            engine,
            snap,
            log,
            selected,
            endpoints,
            notice: None,
            font,
            font_warned: false,
            auto_started: false,
            last_output_was_vad: false,
        }
    }

    fn setup_style(ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = egui::Color32::from_rgb(23, 25, 29);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(22.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(15.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(15.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        );
        ctx.set_style(style);
    }

    // ------------------------------------------------------------ 引擎动作

    fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    fn start_devices(&mut self, keys: Vec<String>) {
        // 双保险：被排除的设备即使被别处塞进来也不投放。
        let keys: Vec<String> = keys
            .into_iter()
            .filter(|k| !self.cfg.ignored_devices.contains(k))
            .collect();
        if keys.is_empty() {
            self.toast("没有勾选在线设备：先在列表里勾选，或点「全部投放」", true);
            return;
        }
        if !self.snap.default_output_is_vad {
            let device_out = self.snap.default_output.clone().unwrap_or_default();
            self.toast(
                format!("注意：系统输出是「{device_out}」而非虚拟声卡，妙播链路采不到声音；低延迟请保持蓝牙直连、不要投放"),
                true,
            );
        }
        if let Some(e) = self.engine() {
            if e.start_devices(keys.clone()) {
                self.toast(format!("已下发投放命令（{} 台设备）", keys.len()), false);
            }
        }
        self.cfg.selected = keys;
        self.cfg.save();
    }

    fn stop_all(&mut self) {
        if let Some(e) = self.engine() {
            if e.stop_devices(None) {
                self.toast("已下发停止命令", false);
            }
        }
    }

    fn set_volume(&mut self) {
        if let Some(e) = self.engine() {
            let v = self.cfg.volume;
            if e.set_volume(v) {
                self.toast(format!("已把音量设为 {v}%"), false);
            }
        }
        self.cfg.save();
    }

    fn test_tone(&mut self) {
        if let Some(e) = self.engine() {
            if e.test_tone(self.cfg.tone_seconds) {
                self.toast(
                    format!(
                        "正在往采集端点播 {:.1}s 测试音：若音箱有声音，说明采集与分发链路是通的",
                        self.cfg.tone_seconds
                    ),
                    false,
                );
            }
        }
    }

    fn restart_engine(&mut self) {
        self.cfg.save();
        if let Some(mut old) = self.engine.take() {
            old.shutdown();
        }
        let cfg = self.cfg.engine.clone();
        self.engine = Some(EngineHandle::start(cfg.clone()));
        self.running = cfg;
        self.auto_started = false;
        self.snap = self
            .engine
            .as_ref()
            .map(|e| e.snapshot())
            .unwrap_or_default();
        self.toast("引擎已重启，改动已生效", false);
    }

    fn refresh_endpoints(&mut self) {
        match miplay_audio::list_endpoints() {
            Ok(list) => {
                let mut names: Vec<String> = list.into_iter().map(|e| e.name).collect();
                names.sort();
                names.dedup();
                let n = names.len();
                self.endpoints = names;
                self.toast(format!("已刷新系统音频端点（{n} 个）"), false);
            }
            Err(e) => self.toast(format!("枚举音频端点失败：{e}"), true),
        }
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    // ------------------------------------------------------------ 每帧

    fn tick(&mut self, ctx: &egui::Context) {
        self.snap = self
            .engine
            .as_ref()
            .map(|e| e.snapshot())
            .unwrap_or_else(|| EngineSnapshot {
                phase: Phase::Stopped,
                ..Default::default()
            });

        // 设备名单会变（刷新设备列表后），勾选状态跟着对齐：新出现且在线的默认勾上。
        let ids: BTreeSet<String> = self.snap.devices.iter().map(|d| d.id.clone()).collect();
        self.selected.retain(|id, _| ids.contains(id));
        for d in &self.snap.devices {
            let want = d.online && !self.cfg.ignored_devices.contains(&d.id);
            self.selected.entry(d.id.clone()).or_insert(want);
        }

        // 引擎第一次就绪时，按配置自动投放一次勾选的设备（每轮引擎只做一次）。
        // 这样打开面板（或重启引擎）后，直接用播放器放歌就该有声音。
        //
        // 但「系统输出不是虚拟声卡」时不要自动投放：比如用户在用蓝牙直连，
        // 这时妙播投放只会把音箱从蓝牙抢回云端，反而把正在听的音频打断。
        // 用户后来把输出切回虚拟声卡时，再补一次自动投放。
        let vad_now = self.snap.default_output_is_vad;
        if vad_now && !self.last_output_was_vad {
            self.auto_started = false;
        }
        self.last_output_was_vad = vad_now;
        if self.snap.phase == Phase::Running && !self.auto_started {
            self.auto_started = true;
            if !self.snap.default_output_is_vad {
                if self.cfg.auto_start {
                    tracing::info!(
                        "[自动投放] 跳过：系统默认输出不是虚拟声卡（当前：{}），先不抢音箱",
                        self.snap.default_output.as_deref().unwrap_or("无")
                    );
                }
            } else if self.cfg.auto_start {
                let keys = self.checked_keys();
                if !keys.is_empty() {
                    if let Some(e) = self.engine.as_ref() {
                        if e.start_devices(keys.clone()) {
                            tracing::info!("[自动投放] 已投放到 {} 台勾选设备", keys.len());
                            self.cfg.selected = keys;
                            self.cfg.save();
                        }
                    }
                }
            }
        }

        if let Some(n) = &self.notice {
            if n.at.elapsed() > Duration::from_secs(10) {
                self.notice = None;
            }
        }

        // 字体缺失只在第一次提醒（否则每帧刷日志）。
        if self.font.is_none() && !self.font_warned {
            self.font_warned = true;
            let candidates = crate::fonts::candidates_present();
            tracing::warn!("可用中文字体：{candidates:?}");
        }

        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn checked_keys(&self) -> Vec<String> {
        self.snap
            .devices
            .iter()
            .filter(|d| d.online && !self.cfg.ignored_devices.contains(&d.id))
            .filter(|d| self.selected.get(&d.id).copied().unwrap_or(false))
            .map(|d| d.id.clone())
            .collect()
    }

    fn restart_needed(&self) -> Vec<&'static str> {
        let (a, b) = (&self.cfg.engine, &self.running);
        let mut out = Vec::new();
        if a.port != b.port {
            out.push("端口");
        }
        if (a.gain - b.gain).abs() > f32::EPSILON {
            out.push("增益");
        }
        if a.split != b.split {
            out.push("左右分离");
        }
        if a.swap != b.swap {
            out.push("左右对调");
        }
        if a.capture_device != b.capture_device {
            out.push("采集端点");
        }
        if a.firewall != b.firewall {
            out.push("防火墙");
        }
        if a.url_base != b.url_base {
            out.push("URL 前缀");
        }
        out
    }

    // ------------------------------------------------------------ 顶栏

    fn top_bar(&mut self, ctx: &egui::Context) {
        let mut open_web = false;
        let mut restart = false;
        let url = format!("{}/", self.snap.url_base.trim_end_matches('/'));
        let phase = (self.snap.phase.label(), phase_color(self.snap.phase));
        let notice = self
            .notice
            .as_ref()
            .map(|n| (n.text.clone(), n.error));

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.heading("小米妙播投放面板");
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("● {}", phase.0))
                        .color(phase.1)
                        .strong(),
                );
                ui.separator();
                if self.snap.phase.is_running() {
                    ui.label(egui::RichText::new(&url).color(ACCENT).monospace());
                    if ui.button("打开网页控制").clicked() {
                        open_web = true;
                    }
                } else if let Some(err) = &self.snap.error {
                    ui.label(egui::RichText::new(err).color(RED).small());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("重启引擎").clicked() {
                        restart = true;
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "端口 {} · 运行 {:.0}s",
                            self.snap.port, self.snap.uptime_seconds
                        ))
                        .color(GREY)
                        .small(),
                    );
                });
            });
            if let Some((text, error)) = notice {
                ui.label(egui::RichText::new(text).color(if error { RED } else { GREEN }));
            }
            ui.add_space(3.0);
        });

        if open_web {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        if restart {
            self.restart_engine();
        }
    }

    // ------------------------------------------------------------ 采集栏

    fn capture_panel(&mut self, ctx: &egui::Context) {
        let mut tone = false;
        let snap = self.snap.clone();
        let cap = &snap.capture;
        let state_color = if cap.error.is_some() {
            RED
        } else if cap.audio_recent {
            GREEN
        } else {
            AMBER
        };

        egui::TopBottomPanel::top("capture")
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("采集端点").color(GREY));
                    ui.label(
                        egui::RichText::new(if cap.device.is_empty() {
                            "（未知）".to_string()
                        } else {
                            cap.device.clone()
                        })
                        .strong(),
                    );
                    if cap.using_vad {
                        ui.label(egui::RichText::new("虚拟声卡").color(GREEN).small());
                    }
                    ui.separator();
                    ui.label(egui::RichText::new(cap.state_label()).color(state_color).strong());
                    let short_format = if cap.format.is_empty() {
                        "—".to_string()
                    } else {
                        cap.format.clone()
                    };
                    ui.label(
                        egui::RichText::new(format!("格式 {short_format}"))
                            .color(GREY)
                            .small(),
                    )
                    .on_hover_text(if cap.negotiated.is_empty() {
                        "WASAPI 协商格式未知".to_string()
                    } else {
                        cap.negotiated.clone()
                    });
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("电平").color(GREY));
                    ui.add(
                        egui::ProgressBar::new(cap.level.peak_l)
                            .desired_width(130.0)
                            .text(format!("左 {:>3.0}%", cap.level.peak_l * 100.0)),
                    );
                    ui.add(
                        egui::ProgressBar::new(cap.level.peak_r)
                            .desired_width(130.0)
                            .text(format!("右 {:>3.0}%", cap.level.peak_r * 100.0)),
                    );
                    ui.add(
                        egui::ProgressBar::new(cap.level.rms)
                            .desired_width(130.0)
                            .text(format!("RMS {:>3.0}%", cap.level.rms * 100.0)),
                    );
                    ui.separator();
                    if cap.audio_recent {
                        ui.label(egui::RichText::new("有声音").color(GREEN).small());
                    } else {
                        ui.label(
                            egui::RichText::new(format!(
                                "静默 {:.1}s",
                                cap.seconds_since_audio
                            ))
                            .color(AMBER)
                            .small(),
                        );
                    }
                    if let Some(err) = &cap.error {
                        ui.label(egui::RichText::new(err).color(RED).small());
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "缓冲 {:.1}s · 泵落后 {:.2}s · 已分发 {} 帧",
                            cap.ring_seconds, cap.backlog_seconds, cap.frames
                        ))
                        .color(GREY)
                        .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("测试音").clicked() {
                            tone = true;
                        }
                        if ui.button("刷新端点").clicked() {
                            self.refresh_endpoints();
                        }
                    });
                });
                ui.add_space(2.0);
            });

        if tone {
            self.test_tone();
        }
    }

    // ------------------------------------------------------------ 右侧设置

    fn settings_panel(&mut self, ctx: &egui::Context) {
        let snap = self.snap.clone();
        let mut apply_restart = false;
        let mut apply_volume = false;
        let needed = self.restart_needed();
        let pending_color = if needed.is_empty() { GREY } else { AMBER };

        egui::SidePanel::right("settings")
            .resizable(true)
            .default_width(340.0)
            .min_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.heading("引擎设置");
                ui.separator();

                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("HTTP 端口");
                        ui.add(
                            egui::DragValue::new(&mut self.cfg.engine.port)
                                .range(1024..=65535)
                                .speed(1.0),
                        );
                        ui.end_row();

                        ui.label("采集增益");
                        ui.add(
                            egui::Slider::new(&mut self.cfg.engine.gain, 0.0..=8.0)
                                .fixed_decimals(2)
                                .text("×"),
                        );
                        ui.end_row();

                        ui.label("左右分离");
                        ui.checkbox(&mut self.cfg.engine.split, "第 1 台左声道 / 第 2 台右声道");
                        ui.end_row();

                        ui.label("左右对调");
                        ui.checkbox(&mut self.cfg.engine.swap, "分离时交换左右");
                        ui.end_row();

                        ui.label("音量");
                        ui.add(egui::Slider::new(&mut self.cfg.volume, 0..=100).suffix("%"));
                        ui.end_row();

                        ui.label("自动投放");
                        if ui
                            .checkbox(&mut self.cfg.auto_start, "引擎就绪后自动投放勾选设备")
                            .changed()
                        {
                            self.cfg.save();
                        }
                        ui.end_row();

                        ui.label("防火墙");
                        ui.checkbox(&mut self.cfg.engine.firewall, "尝试添加 8931 入站规则");
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.label(egui::RichText::new("采集端点").color(GREY));
                let auto_label = match &snap.capture.device {
                    d if snap.capture.using_vad => format!("自动（当前 {d}）"),
                    _ => "自动（优先小米妙播设备）".to_string(),
                };
                egui::ComboBox::from_id_salt("capture_endpoint")
                    .selected_text(match &self.cfg.engine.capture_device {
                        Some(name) => name.clone(),
                        None => auto_label,
                    })
                    .width(ui.available_width() - 8.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.cfg.engine.capture_device,
                            None,
                            "自动（优先小米妙播设备）",
                        );
                        for name in &self.endpoints {
                            ui.selectable_value(
                                &mut self.cfg.engine.capture_device,
                                Some(name.clone()),
                                name,
                            );
                        }
                    });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("应用并重启引擎").clicked() {
                        apply_restart = true;
                    }
                    if ui.button("立即应用音量").clicked() {
                        apply_volume = true;
                    }
                });
                if !needed.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("有未应用改动：{}", needed.join("、")))
                            .color(pending_color)
                            .small(),
                    );
                }
                ui.separator();

                ui.label(egui::RichText::new("自检").color(GREY));
                check_row(ui, snap.vad_present, "虚拟声卡「小米妙播设备」");
                let out_bt = snap
                    .default_output
                    .as_deref()
                    .map(|o| o.contains("立体声组合") || o.contains("Bluetooth") || o.contains("蓝牙"))
                    .unwrap_or(false);
                if out_bt && !snap.default_output_is_vad {
                    ui.label(
                        egui::RichText::new("✔ 系统默认输出 = 蓝牙直连（低延迟模式）")
                            .color(GREEN)
                            .small(),
                    );
                } else {
                    check_row(ui, snap.default_output_is_vad, "系统默认输出 = 虚拟声卡");
                }
                check_row(ui, snap.capture.using_vad, "引擎正在采集虚拟声卡");
                check_row(ui, snap.capture.running, "采集线程在跑");
                check_row(ui, snap.listening, "HTTP 服务在监听");
                check_row(ui, snap.active_count() > 0, "有设备在投放");
                check_row(ui, snap.connected_count() > 0, "有音箱连上来");
                ui.label(
                    egui::RichText::new(format!("默认输出：{}", snap.default_output.as_deref().unwrap_or("无")))
                        .small()
                        .color(GREY),
                );
                ui.label(
                    egui::RichText::new(format!("防火墙：{}", snap.firewall))
                        .small()
                        .color(GREY),
                );
                ui.label(
                    egui::RichText::new(format!("字体：{}", self.font.as_deref().unwrap_or("缺失（中文可能是方块）")))
                        .small()
                        .color(if self.font.is_some() { GREY } else { RED }),
                );

                ui.separator();
                ui.label(egui::RichText::new("输出模式 / 延迟").color(GREY));
                let device_out = snap.default_output.clone().unwrap_or_default();
                let is_bt = device_out.contains("立体声组合")
                    || device_out.contains("Bluetooth")
                    || device_out.contains("蓝牙");
                let (mode_color, mode, advice) = if snap.default_output_is_vad {
                    (
                        GREEN,
                        "妙播链路（虚拟声卡）".to_string(),
                        "约 1 秒 · 适合音乐/多房间；看视频会明显滞后",
                    )
                } else if is_bt {
                    (
                        GREEN,
                        "蓝牙直连（立体声组合）".to_string(),
                        "约 0.2~0.35 秒 · 适合看视频；游戏仍需有线耳机",
                    )
                } else if device_out.is_empty() {
                    (AMBER, "未知".to_string(), "点「刷新端点」重新枚举")
                } else {
                    (
                        AMBER,
                        format!("其它输出：{device_out}"),
                        "声音不经过虚拟声卡，妙播投放没有内容可采",
                    )
                };
                ui.label(egui::RichText::new(mode).color(mode_color).strong());
                ui.label(egui::RichText::new(advice).color(GREY).small());
                ui.collapsing("延迟对照（实测 / 官方）", |ui| {
                    for (k, v) in [
                        ("妙播云端 play_url", "约 1 秒（音箱固件缓冲主导；PC 侧仅 ~0.25s）"),
                        ("蓝牙直连 A2DP", "约 0.2~0.35 秒（单房间、免云、无多房间）"),
                        ("AirPlay 2", "设计 0.25~0.4 秒；本机这套设备目前 AirPlay 出不了声"),
                        ("游戏 <0.1s", "只有有线耳机能做到（L16A 无 AUX 输入）"),
                    ] {
                        ui.label(egui::RichText::new(format!("{k}：{v}")).small());
                    }
                });
                if ui.button("打开 Windows 声音设置").clicked() {
                    let _ = std::process::Command::new("cmd")
                        .args(["/c", "start", "ms-settings:sound"])
                        .spawn();
                }

                ui.separator();
                ui.checkbox(&mut self.cfg.show_log, "显示日志面板");
                ui.horizontal(|ui| {
                    if ui.button("保存配置").clicked() {
                        self.cfg.save();
                        self.toast("配置已保存", false);
                    }
                    if ui.button("打开配置目录").clicked() {
                        let dir = DesktopConfig::path()
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                        let _ = std::process::Command::new("explorer").arg(dir).spawn();
                    }
                });
            });

        if apply_restart {
            self.restart_engine();
        }
        if apply_volume {
            self.set_volume();
        }
    }

    // ------------------------------------------------------------ 日志

    fn log_panel(&mut self, ctx: &egui::Context) {
        let mut clear = false;
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(210.0)
            .min_height(80.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("日志（{} 行）", self.log.len()));
                    if ui.button("清空").clicked() {
                        clear = true;
                    }
                });
                ui.separator();
                let lines = self.log.lines();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in lines.iter().rev().take(400).rev() {
                            ui.label(log_text(line));
                        }
                    });
            });
        if clear {
            self.log.clear();
            self.toast("日志已清空", false);
        }
    }

    // ------------------------------------------------------------ 设备列表

    fn devices_panel(&mut self, ui: &mut egui::Ui) {
        let snap = self.snap.clone();
        let mut start_checked = false;
        let mut stop_everything = false;
        let mut start_one: Option<String> = None;
        let mut stop_one: Option<String> = None;
        let mut ignore_one: Option<String> = None;
        let mut restore_one: Option<String> = None;
        let ignored_count = self.cfg.ignored_devices.len();

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(format!("设备（{}）", snap.devices.len()));
            let mut summary = format!(
                "在线 {} · 投放中 {} · 已连上 {}",
                snap.devices.iter().filter(|d| d.online).count(),
                snap.active_count(),
                snap.connected_count()
            );
            if ignored_count > 0 {
                summary.push_str(&format!(" · 已排除 {ignored_count}"));
            }
            ui.label(egui::RichText::new(summary).color(GREY).small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("停止全部").clicked() {
                    stop_everything = true;
                }
                if ui.button("投放勾选").clicked() {
                    start_checked = true;
                }
            });
        });

        for (color, text) in self.hints(&snap) {
            ui.label(egui::RichText::new(text).color(color).small());
        }

        // 已排除的设备：不参与投放，可以一键恢复。放在列表上方，避免被滚动区挤到看不见。
        let ignored: Vec<_> = snap
            .devices
            .iter()
            .filter(|d| self.cfg.ignored_devices.contains(&d.id))
            .collect();
        if !ignored.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("已排除 {} 台（永不投放）：", ignored.len()))
                        .color(GREY)
                        .small(),
                );
                for d in &ignored {
                    if ui
                        .small_button(format!("恢复 {}", d.label))
                        .on_hover_text(format!("did: {}", d.did))
                        .clicked()
                    {
                        restore_one = Some(d.id.clone());
                    }
                }
            });
        }
        ui.separator();

        if snap.devices.is_empty() {
            ui.label(
                egui::RichText::new("还没有设备：引擎可能还在启动，或账号下没有设备。看下方日志。")
                    .color(AMBER),
            );
        } else {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("devices")
                        .num_columns(10)
                        .striped(true)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("选").color(GREY));
                            ui.label(egui::RichText::new("设备").color(GREY));
                            ui.label(egui::RichText::new("状态").color(GREY));
                            ui.label(egui::RichText::new("云端").color(GREY));
                            ui.label(egui::RichText::new("声道").color(GREY));
                            ui.label(egui::RichText::new("客户端").color(GREY));
                            ui.label(egui::RichText::new("延迟").color(GREY));
                            ui.label(egui::RichText::new("已发流").color(GREY));
                            ui.label(egui::RichText::new("下发/失败").color(GREY));
                            ui.label(egui::RichText::new("操作").color(GREY));
                            ui.end_row();

                            for d in snap
                                .devices
                                .iter()
                                .filter(|d| !self.cfg.ignored_devices.contains(&d.id))
                            {
                                // 勾选
                                let mut checked = self.selected.get(&d.id).copied().unwrap_or(false);
                                if ui.checkbox(&mut checked, "").changed() {
                                    self.selected.insert(d.id.clone(), checked);
                                }
                                // 设备名 + 型号 + 地址悬浮说明
                                ui.label(egui::RichText::new(&d.label).strong())
                                    .on_hover_text(format!(
                                        "did: {}\nuuid: {}\n型号: {}\n拉流地址: {}",
                                        d.did, d.device_id, d.hardware, d.url
                                    ));
                                // 状态
                                ui.label(
                                    egui::RichText::new(d.state_label()).color(device_color(d)),
                                );
                                // 云端
                                if d.online {
                                    ui.label(egui::RichText::new("在线").color(GREEN).small());
                                } else {
                                    ui.label(egui::RichText::new("离线").color(GREY).small());
                                }
                                // 声道
                                ui.label(egui::RichText::new(source_label(&d.source)).small());
                                // 客户端
                                ui.label(
                                    egui::RichText::new(format!("{}", d.clients))
                                        .color(if d.clients > 0 { GREEN } else { GREY }),
                                );
                                // 延迟
                                ui.label(
                                    egui::RichText::new(format!("{:.1}s", d.lag_seconds))
                                        .color(if d.active && d.lag_seconds > 2.0 { AMBER } else { GREY })
                                        .small(),
                                );
                                // 已发流
                                ui.label(
                                    egui::RichText::new(format!("{:.2} MB", d.bytes_sent as f64 / 1048576.0))
                                        .small()
                                        .color(GREY),
                                );
                                // 下发/失败
                                let mut txt = format!("{}", d.plays_sent);
                                if d.play_failures > 0 {
                                    txt.push_str(&format!(" / {}失败", d.play_failures));
                                }
                                if d.reissues > 0 {
                                    txt.push_str(&format!(" · 重发{}", d.reissues));
                                }
                                ui.label(
                                    egui::RichText::new(txt)
                                        .small()
                                        .color(if d.play_failures > 0 { AMBER } else { GREY }),
                                )
                                .on_hover_text(
                                    d.last_error.clone().unwrap_or_else(|| "（无错误）".to_string()),
                                );
                                // 操作
                                ui.horizontal(|ui| {
                                    if d.active {
                                        if ui.small_button("停止").clicked() {
                                            stop_one = Some(d.id.clone());
                                        }
                                    } else if d.online {
                                        if ui.small_button("投放").clicked() {
                                            start_one = Some(d.id.clone());
                                        }
                                    } else {
                                        ui.label(egui::RichText::new("—").color(GREY));
                                    }
                                    if ui
                                        .small_button("排除")
                                        .on_hover_text("从投放列表里永久移除（可随时恢复）")
                                        .clicked()
                                    {
                                        ignore_one = Some(d.id.clone());
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });
        }

        if start_checked {
            let keys = self.checked_keys();
            self.start_devices(keys);
        }
        if stop_everything {
            self.stop_all();
        }
        if let Some(key) = start_one {
            self.start_devices(vec![key]);
        }
        if let Some(key) = stop_one {
            if let Some(e) = self.engine() {
                e.stop_devices(Some(vec![key]));
            }
        }
        if let Some(key) = ignore_one {
            if self
                .snap
                .devices
                .iter()
                .any(|d| d.id == key && d.active)
            {
                if let Some(e) = self.engine() {
                    e.stop_devices(Some(vec![key.clone()]));
                }
            }
            if !self.cfg.ignored_devices.contains(&key) {
                self.cfg.ignored_devices.push(key.clone());
                self.cfg.ignored_devices.sort();
                self.cfg.ignored_devices.dedup();
            }
            self.selected.remove(&key);
            self.cfg.selected.retain(|k| k != &key);
            self.cfg.save();
            let name = self
                .snap
                .devices
                .iter()
                .find(|d| d.id == key)
                .map(|d| d.label.clone())
                .unwrap_or(key);
            self.toast(format!("已排除「{name}」，不会再向它推流"), false);
        }
        if let Some(key) = restore_one {
            self.cfg.ignored_devices.retain(|k| k != &key);
            self.cfg.save();
            self.selected.insert(key.clone(), true);
            if !self.cfg.selected.contains(&key) {
                self.cfg.selected.push(key.clone());
            }
            let name = self
                .snap
                .devices
                .iter()
                .find(|d| d.id == key)
                .map(|d| d.label.clone())
                .unwrap_or(key);
            self.toast(format!("已恢复「{name}」"), false);
        }
    }

    /// 诊断建议：按「最可能的失败点」顺序给，没有问题时返回一条绿色确认。
    fn hints(&self, snap: &EngineSnapshot) -> Vec<(egui::Color32, String)> {
        let mut out: Vec<(egui::Color32, String)> = Vec::new();
        let cap = &snap.capture;

        if snap.phase == Phase::Failed {
            let msg = snap.error.clone().unwrap_or_else(|| "引擎启动失败".to_string());
            out.push((RED, format!("引擎启动失败：{msg}")));
        }
        if !snap.vad_present {
            out.push((
                RED,
                "系统里找不到「小米妙播设备」虚拟声卡。先运行 scripts/install-virtual-device.ps1 装上，再在 Windows 声音设置里选中它。".to_string(),
            ));
        } else if !cap.using_vad && !snap.default_output_is_vad {
            out.push((
                AMBER,
                "引擎采集的不是虚拟声卡。请把 Windows 输出设备切换成「扬声器 (小米妙播设备)」，或到设置里手动指定采集端点。".to_string(),
            ));
        }
        // 蓝牙直连模式：声音本来就不经过虚拟声卡，这是「低延迟看视频」的正确用法，
        // 不能按「采集端点不对」去报错。
        if let Some(device_out) = snap.default_output.as_deref() {
            let is_bt = device_out.contains("立体声组合")
                || device_out.contains("Bluetooth")
                || device_out.contains("蓝牙");
            if is_bt && !snap.default_output_is_vad {
                if snap.active_count() > 0 {
                    out.push((
                        AMBER,
                        format!(
                            "系统输出是蓝牙「{device_out}」：声音已经直接走蓝牙（约 0.2~0.35s，看视频够用），此时妙播投放是多余的；建议点「停止全部」，要玩多房间再切回虚拟声卡。"
                        ),
                    ));
                } else {
                    out.push((
                        GREEN,
                        format!(
                            "系统输出是蓝牙「{device_out}」：这就是低延迟模式（约 0.2~0.35s），看视频/听歌直接用系统播放即可，不需要妙播投放。"
                        ),
                    ));
                }
            }
        }
        let bt_idle = !snap.default_output_is_vad
            && snap
                .default_output
                .as_deref()
                .map(|o| o.contains("立体声组合") || o.contains("Bluetooth") || o.contains("蓝牙"))
                .unwrap_or(false)
            && snap.active_count() == 0;
        if cap.error.is_some() || (snap.phase.is_running() && !cap.running) {
            out.push((
                RED,
                "采集线程没在跑：检查输出设备是否被拔掉/切换，然后重启引擎。".to_string(),
            ));
        } else if snap.phase.is_running() && !cap.audio_recent && !bt_idle {
            out.push((
                AMBER,
                format!(
                    "采集侧已经静默 {:.1}s：确认播放器正在放歌、输出选的是虚拟声卡；也可以点「测试音」验证链路。",
                    cap.seconds_since_audio
                ),
            ));
        }
        if snap.phase.is_running() && snap.active_count() > 0 && snap.connected_count() == 0 && snap.uptime_seconds > 15.0 {
            out.push((
                AMBER,
                "已下发投放但音箱还没连上来：确认音箱和电脑在同一局域网，防火墙放行 8931，点「打开网页控制」看 /health。".to_string(),
            ));
        }
        if snap.devices.iter().any(|d| d.play_failures > 0) {
            out.push((
                AMBER,
                "有设备下发 play_url 失败：看下方日志（常见原因：账号凭据过期、设备离线、URL 不在同一网段）。".to_string(),
            ));
        }
        if out.is_empty() && snap.phase.is_running() {
            if snap.connected_count() > 0 {
                out.push((GREEN, "链路正常：音箱已连上，采集有声音。".to_string()));
            } else {
                out.push((
                    GREEN,
                    "引擎就绪：勾选设备后点「投放勾选」即可开始。".to_string(),
                ));
            }
        }
        out
    }
}

impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
        self.top_bar(ctx);
        self.capture_panel(ctx);
        self.settings_panel(ctx);
        if self.cfg.show_log {
            self.log_panel(ctx);
        }
        egui::CentralPanel::default().show(ctx, |ui| self.devices_panel(ui));
    }
}

// ---------------------------------------------------------------- 小工具

fn phase_color(phase: Phase) -> egui::Color32 {
    match phase {
        Phase::Booting => AMBER,
        Phase::Running => GREEN,
        Phase::Failed => RED,
        Phase::Stopped => GREY,
    }
}

fn device_color(d: &DeviceSnapshot) -> egui::Color32 {
    match d.state_label() {
        "未投放" => GREY,
        "已连上" => GREEN,
        "下发失败" => RED,
        "重发中" => AMBER,
        _ => AMBER,
    }
}

fn source_label(source: &str) -> &str {
    match source {
        "left" => "左声道",
        "right" => "右声道",
        _ => "混合",
    }
}

fn check_row(ui: &mut egui::Ui, ok: bool, label: &str) {
    ui.label(
        egui::RichText::new(format!("{} {}", if ok { "✔" } else { "✘" }, label))
            .color(if ok { GREEN } else { RED })
            .small(),
    );
}

fn log_text(line: &LogLine) -> egui::RichText {
    let color = match line.level {
        tracing::Level::ERROR => RED,
        tracing::Level::WARN => AMBER,
        tracing::Level::INFO => egui::Color32::from_rgb(206, 210, 216),
        _ => GREY,
    };
    egui::RichText::new(format!("{:>7.1}s  {}", line.at.as_secs_f64(), line.text))
        .monospace()
        .color(color)
}
