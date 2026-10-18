//! 玻璃拟态（Glassmorphism）设计系统：设计 token、柔光背景纹理、玻璃卡片、
//! 行内波形/频谱与分段电平条。
//!
//! egui 是即时模式、**没有 `backdrop-filter`**，所以「毛玻璃」用三层近似：
//!
//! 1. 预渲染一张柔光渐变 + 模糊色斑 + 细噪点的纹理，铺在窗口最底层
//!    （`egui::LayerId::background()`），漂移只改 UV，不重新栅格化；
//! 2. 卡片/侧边栏用**半透明**填充（alpha 0.05~0.20），让底纹透上来 —— 这是
//!    视觉上最接近「磨砂玻璃」的部分；
//! 3. 1px 描边 + 顶部内高光 + 阴影，制造玻璃的厚度与光感。
//!
//! 颜色语义沿用界面既有的「颜色即信号」约定：左声道蓝、右声道琥珀，
//! 电平条绿→黄→红。深浅两套 token 保证对比度（深色底文字 #E8ECF4，
//! 浅色底文字 #17202D，均 ≥ 4.5:1）。

use eframe::egui::{self, Color32, CornerRadius, Margin, Rect, Stroke, Vec2};

/// 行内频谱柱数量（原型 `NBIN = 22`）。
const NBIN: usize = 22;
/// 行内波形采样点数。
const WAVE_POINTS: usize = 56;
/// 电平条分段数。
const METER_SEGMENTS: usize = 12;

/// 一套玻璃拟态 token：深浅主题各一份。
#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    /// 卡片（基础层）填充：半透明，底纹能透上来。
    pub glass_base: Color32,
    /// 抬升层（表格行、内嵌小块）。
    pub glass_raised: Color32,
    /// 侧边栏底色。
    pub sidebar: Color32,
    /// 1px 玻璃描边。
    pub stroke: Color32,
    /// 顶部内高光。
    pub highlight: Color32,
    /// 强调色（主色）。
    pub accent: Color32,
    /// 次强调色。
    pub accent2: Color32,
    /// 左声道波形色。
    pub wave_l: Color32,
    /// 右声道波形色。
    pub wave_r: Color32,
    /// 电平条低段（安全）。
    pub meter_low: Color32,
    /// 电平条中段（接近满刻度）。
    pub meter_mid: Color32,
    /// 电平条高段（削顶）。
    pub meter_high: Color32,
    /// 背景渐变顶部色。
    pub bg_top: Color32,
    /// 背景渐变底部色。
    pub bg_bottom: Color32,
    /// 卡片圆角。
    pub radius: u8,
    /// 微交互时长（秒）：悬停/切换/胶囊出现。
    pub dur_micro: f32,
}

/// 深色 token。
pub const DARK_TOKENS: Tokens = Tokens {
    glass_base: Color32::from_rgba_premultiplied(14, 17, 23, 150),
    glass_raised: Color32::from_rgba_premultiplied(22, 26, 34, 190),
    sidebar: Color32::from_rgba_premultiplied(11, 14, 19, 170),
    stroke: Color32::from_rgba_premultiplied(255, 255, 255, 38),
    highlight: Color32::from_rgba_premultiplied(255, 255, 255, 46),
    accent: Color32::from_rgb(56, 189, 248),
    accent2: Color32::from_rgb(167, 139, 250),
    wave_l: Color32::from_rgb(96, 165, 250),
    wave_r: Color32::from_rgb(251, 191, 36),
    meter_low: Color32::from_rgb(74, 222, 128),
    meter_mid: Color32::from_rgb(251, 191, 36),
    meter_high: Color32::from_rgb(248, 113, 113),
    bg_top: Color32::from_rgb(20, 24, 33),
    bg_bottom: Color32::from_rgb(8, 10, 14),
    radius: 14,
    dur_micro: 0.18,
};

/// 浅色 token。
pub const LIGHT_TOKENS: Tokens = Tokens {
    glass_base: Color32::from_rgba_premultiplied(255, 255, 255, 186),
    glass_raised: Color32::from_rgba_premultiplied(255, 255, 255, 214),
    sidebar: Color32::from_rgba_premultiplied(248, 251, 255, 196),
    stroke: Color32::from_rgba_premultiplied(15, 23, 42, 34),
    highlight: Color32::from_rgba_premultiplied(255, 255, 255, 220),
    accent: Color32::from_rgb(2, 132, 199),
    accent2: Color32::from_rgb(109, 40, 217),
    wave_l: Color32::from_rgb(37, 99, 235),
    wave_r: Color32::from_rgb(217, 119, 6),
    meter_low: Color32::from_rgb(21, 128, 61),
    meter_mid: Color32::from_rgb(180, 83, 9),
    meter_high: Color32::from_rgb(185, 28, 28),
    bg_top: Color32::from_rgb(240, 245, 251),
    bg_bottom: Color32::from_rgb(222, 230, 241),
    radius: 14,
    dur_micro: 0.18,
};

/// 取当前主题的 token。
pub fn tokens(dark: bool) -> Tokens {
    if dark {
        DARK_TOKENS
    } else {
        LIGHT_TOKENS
    }
}

/// 玻璃层级：页面主卡片（半透明，透出底层柔光）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassLevel {
    /// 页面主卡片。
    Base,
}

/// 玻璃卡片外框：半透明填充 + 1px 描边 + 阴影（egui 没有真模糊，靠这三样近似）。
pub fn glass_frame(level: GlassLevel, dark: bool) -> egui::Frame {
    let t = tokens(dark);
    let (fill, blur, alpha) = match level {
        GlassLevel::Base => (t.glass_base, 22u8, 70u8),
    };
    egui::Frame::default()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, t.stroke))
        .inner_margin(Margin::symmetric(18, 16))
        .corner_radius(CornerRadius::same(t.radius))
        .shadow(egui::epaint::Shadow {
            offset: [0, 6],
            blur,
            spread: 0,
            color: Color32::from_black_alpha(alpha),
        })
}

/// 取（并缓存）柔光背景纹理。同一主题只生成一次，之后复用。
pub fn background_texture(ctx: &egui::Context, dark: bool) -> egui::TextureHandle {
    let id = egui::Id::new(("stereo-link-bg", dark));
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return tex;
    }
    let size = 512usize;
    let t = tokens(dark);
    // 三团模糊色斑：左声道蓝、右声道琥珀、次强调紫。
    let blobs: [(f32, f32, f32, Color32, f32); 3] = [
        (0.20, 0.16, 0.52, t.wave_l, 0.42),
        (0.86, 0.30, 0.46, t.wave_r, 0.26),
        (0.52, 0.92, 0.60, t.accent2, 0.30),
    ];
    let mut pixels = Vec::with_capacity(size * size);
    for y in 0..size {
        let v = y as f32 / (size - 1) as f32;
        let base = lerp_color(t.bg_top, t.bg_bottom, v);
        for x in 0..size {
            let u = x as f32 / (size - 1) as f32;
            let mut c = base;
            for (bx, by, br, col, strength) in blobs {
                let d = ((u - bx).powi(2) + (v - by).powi(2)).sqrt();
                let w = (1.0 - (d / br)).clamp(0.0, 1.0);
                if w > 0.0 {
                    c = blend(c, col, w * w * strength);
                }
            }
            // 细噪点：打散渐变的「塑料感」，也让玻璃的透光有颗粒。
            let n = (hash01(x as u32, y as u32) - 0.5) * 7.0;
            pixels.push(shade(c, n));
        }
    }
    let image = egui::ColorImage {
        size: [size, size],
        pixels,
    };
    let tex = ctx.load_texture(
        if dark {
            "stereo-link-bg-dark"
        } else {
            "stereo-link-bg-light"
        },
        image,
        egui::TextureOptions {
            magnification: egui::TextureFilter::Linear,
            minification: egui::TextureFilter::Linear,
            wrap_mode: egui::TextureWrapMode::Repeat,
            mipmap_mode: None,
        },
    );
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

/// 把背景纹理铺满 `rect`（画在窗口最底层）。`time` 用 `ctx.input(|i| i.time)`，
/// 漂移靠 UV 偏移 + REPEAT 环绕，每帧零栅格化成本。
pub fn paint_background(ctx: &egui::Context, rect: Rect, dark: bool, time: f64) {
    let tex = background_texture(ctx, dark);
    let t = time as f32;
    // 两个不同周期的正弦，让漂移看起来不循环。
    let drift = Vec2::new((t * 0.035).sin() * 0.05, (t * 0.023).cos() * 0.05);
    let uv = Rect::from_min_max(
        egui::pos2(0.20 + drift.x, 0.20 + drift.y),
        egui::pos2(1.20 + drift.x, 1.20 + drift.y),
    );
    let mut mesh = egui::Mesh::with_texture(tex.id());
    mesh.add_rect_with_uv(rect, uv, Color32::WHITE);
    ctx.layer_painter(egui::LayerId::background()).add(mesh);
}

/// 每路一行内嵌的「波形 + 频谱」状态。
///
/// 幅度来自引擎上报的**真实** `RouteStats.level`（快攻慢放包络），波形形状是
/// 合成信号（egui 侧拿不到原始 PCM 采样）。接真实采样只需替换 `paint` 里的
/// 采样函数，接口不变。
#[derive(Debug, Clone)]
pub struct WaveBuf {
    level: f32,
    peak: f32,
    phase: f32,
    bins: [f32; NBIN],
    bin_phase: f32,
}

impl Default for WaveBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl WaveBuf {
    /// 新建（全静音）。
    pub fn new() -> Self {
        Self {
            level: 0.0,
            peak: 0.0,
            phase: 0.0,
            bins: [0.0; NBIN],
            bin_phase: 0.0,
        }
    }

    /// 推进一帧：`dt` 秒、`level` 为 0..1 的线性电平。
    pub fn advance(&mut self, dt: f32, level: f32) {
        let dt = dt.clamp(0.0, 0.25);
        let level = level.clamp(0.0, 1.0);
        // 快攻（约 60 ms）慢放（约 260 ms）：电平跳变时看起来像真实表头。
        let tau = if level > self.level { 0.06 } else { 0.26 };
        let k = 1.0 - (-dt / tau).exp();
        self.level += (level - self.level) * k;
        if self.level > self.peak {
            self.peak = self.level;
        } else {
            self.peak = (self.peak - dt / 1.1).max(self.level);
        }
        self.phase = (self.phase + dt * std::f32::consts::TAU * 3.2) % std::f32::consts::TAU;
        self.bin_phase =
            (self.bin_phase + dt * std::f32::consts::TAU * 0.55) % std::f32::consts::TAU;
        let amp = self.level.powf(0.55);
        for (i, bin) in self.bins.iter_mut().enumerate() {
            let f = i as f32 / (NBIN - 1) as f32;
            // 低频高、高频低的对数倾斜，再叠一点缓慢起伏，避免整排一起动。
            let tilt = (1.0 - f).powf(0.75);
            let wobble = 0.62 + 0.38 * (0.5 + 0.5 * (self.bin_phase + f * 5.1).sin());
            let target = (amp * tilt * wobble).clamp(0.0, 1.0);
            let tau_bin = if target > *bin { 0.05 } else { 0.22 };
            let kb = 1.0 - (-dt / tau_bin).exp();
            *bin += (target - *bin) * kb;
        }
    }

    /// 当前平滑后的电平（0..1）。
    pub fn level(&self) -> f32 {
        self.level
    }

    /// 峰值保持值（0..1）。
    pub fn peak(&self) -> f32 {
        self.peak
    }

    /// 画一行内嵌波形 + 频谱：频谱柱作底纹，波形线作前景。
    pub fn paint(&self, ui: &egui::Ui, rect: Rect, color: Color32, dark: bool) {
        let t = tokens(dark);
        let painter = ui.painter();
        let radius = CornerRadius::same(7);
        painter.rect_filled(rect, radius, t.glass_raised);
        painter.rect_stroke(
            rect,
            radius,
            Stroke::new(1.0_f32, t.stroke),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink2(Vec2::new(4.0, 3.0));
        if inner.width() <= 4.0 || inner.height() <= 4.0 {
            return;
        }
        // 底纹：NBIN 根对数频谱柱，低对比度。
        let gap = 1.0_f32;
        let bar_w = ((inner.width() - gap * (NBIN as f32 - 1.0)) / NBIN as f32).max(1.0);
        for (i, bin) in self.bins.iter().enumerate() {
            let h = (bin.clamp(0.0, 1.0) * inner.height()).max(1.0);
            let x = inner.left() + i as f32 * (bar_w + gap);
            let bar = Rect::from_min_max(
                egui::pos2(x, inner.bottom() - h),
                egui::pos2(x + bar_w, inner.bottom()),
            );
            let c = color.gamma_multiply(0.20 + 0.22 * bin.clamp(0.0, 1.0));
            painter.rect_filled(bar, CornerRadius::same(1), c);
        }
        // 中轴：极淡一条。
        let mid = inner.center().y;
        painter.line_segment(
            [
                egui::pos2(inner.left(), mid),
                egui::pos2(inner.right(), mid),
            ],
            Stroke::new(1.0_f32, color.gamma_multiply(0.16)),
        );
        // 前景：波形线（合成信号，幅度 = 真实电平）。
        let amp = self.level.clamp(0.0, 1.0).powf(0.7) * inner.height() * 0.46;
        let mut points = Vec::with_capacity(WAVE_POINTS);
        for i in 0..WAVE_POINTS {
            let f = i as f32 / (WAVE_POINTS - 1) as f32;
            let x = inner.left() + f * inner.width();
            let shape =
                (self.phase + f * 11.0).sin() * 0.72 + (self.phase * 1.7 + f * 23.0).sin() * 0.28;
            let env = 0.55 + 0.45 * (0.5 + 0.5 * (self.phase * 0.5 + f * 3.3).sin());
            points.push(egui::pos2(x, mid - shape * env * amp));
        }
        painter.add(egui::Shape::line(points, Stroke::new(1.4_f32, color)));
    }
}

/// 分段电平条：12 段（绿→黄→红）+ 0 点 + 峰值保持线。
///
/// `level`/`peak` 为 0..1 线性值（调用方自行决定要不要开方）。
pub fn segmented_level(ui: &egui::Ui, rect: Rect, level: f32, peak: f32, dark: bool) {
    let t = tokens(dark);
    let painter = ui.painter();
    let level = level.clamp(0.0, 1.0);
    let peak = peak.clamp(0.0, 1.0);
    let gap = 1.5_f32;
    let seg_w =
        ((rect.width() - gap * (METER_SEGMENTS as f32 - 1.0)) / METER_SEGMENTS as f32).max(1.0);
    for i in 0..METER_SEGMENTS {
        let f = i as f32 / (METER_SEGMENTS - 1) as f32;
        let x = rect.left() + i as f32 * (seg_w + gap);
        let seg = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(seg_w, rect.height()));
        // 0 点在 1/3 处（-20 dB 左右）留一道更亮的刻度感。
        let lit = (i as f32 + 1.0) / METER_SEGMENTS as f32 <= level + 1e-4;
        let base = if f < 0.58 {
            t.meter_low
        } else if f < 0.86 {
            t.meter_mid
        } else {
            t.meter_high
        };
        let color = if lit {
            base
        } else {
            base.gamma_multiply(if dark { 0.16 } else { 0.20 })
        };
        painter.rect_filled(seg, CornerRadius::same(1), color);
    }
    // 峰值保持线：白色 2px，带一点外发光。
    if peak > 0.001 {
        let x = rect.left() + peak * rect.width();
        painter.rect_filled(
            Rect::from_min_max(
                egui::pos2((x - 1.0).max(rect.left()), rect.top() - 1.0),
                egui::pos2((x + 1.0).min(rect.right()), rect.bottom() + 1.0),
            ),
            CornerRadius::same(1),
            Color32::from_white_alpha(if dark { 235 } else { 210 }),
        );
    }
}

/// 侧边栏选中项胶囊：accent 半透明底 + 1px accent 描边 + 左侧竖条。
///
/// `glow` 为 0..1 的动画量（选中时由 `animate_value_with_time` 推上去），
/// 让切换页面时胶囊「长出来」而不是硬切。
pub fn nav_capsule(ui: &egui::Ui, rect: Rect, dark: bool, glow: f32) {
    let t = tokens(dark);
    let glow = glow.clamp(0.0, 1.0);
    let painter = ui.painter();
    let radius = CornerRadius::same(10);
    painter.rect_filled(rect, radius, t.accent.gamma_multiply(0.10 + 0.10 * glow));
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, t.accent.gamma_multiply(0.30 + 0.35 * glow)),
        egui::StrokeKind::Inside,
    );
    // 顶部内高光：1px 的玻璃厚度感，随 glow 淡入。
    painter.line_segment(
        [
            egui::pos2(rect.left() + 8.0, rect.top() + 1.0),
            egui::pos2(rect.right() - 8.0, rect.top() + 1.0),
        ],
        Stroke::new(1.0_f32, t.highlight.gamma_multiply(0.30 + 0.45 * glow)),
    );
    let bar = Rect::from_min_size(
        egui::pos2(rect.left() + 1.0, rect.center().y - 9.0),
        Vec2::new(3.0, 18.0),
    );
    painter.rect_filled(bar, CornerRadius::same(2), t.accent);
}

// ---- 颜色工具 ----

/// 线性插值两个颜色（`t` 0..1）。
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// 把 `over` 按 `alpha` 叠到 `base` 上（都当不透明色处理）。
pub fn blend(base: Color32, over: Color32, alpha: f32) -> Color32 {
    lerp_color(base, over, alpha.clamp(0.0, 1.0))
}

/// 整体提亮/压暗 `delta`（-255..255）。
fn shade(c: Color32, delta: f32) -> Color32 {
    let f = |x: u8| (x as f32 + delta).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// 确定性伪随机（0..1）：背景噪点用，保证同一像素每次一致。
fn hash01(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 16;
    (h & 0xFFFF) as f32 / 65535.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_differ_between_themes() {
        assert_ne!(DARK_TOKENS.glass_base, LIGHT_TOKENS.glass_base);
        assert_ne!(DARK_TOKENS.wave_l, LIGHT_TOKENS.wave_l);
        assert_eq!(tokens(true).accent, DARK_TOKENS.accent);
        assert_eq!(tokens(false).accent, LIGHT_TOKENS.accent);
    }

    #[test]
    fn glass_frames_are_translucent() {
        // 半透明是「玻璃」的前提：填充 alpha 必须小于 255。
        assert!(DARK_TOKENS.glass_base.a() < 255);
        assert!(LIGHT_TOKENS.glass_base.a() < 255);
        let frame = glass_frame(GlassLevel::Base, true);
        assert!(frame.fill.a() < 255);
    }

    #[test]
    fn lerp_and_blend_stay_in_range() {
        let mid = lerp_color(Color32::BLACK, Color32::WHITE, 0.5);
        assert_eq!(mid, Color32::from_rgb(128, 128, 128));
        assert_eq!(
            lerp_color(Color32::BLACK, Color32::WHITE, -1.0),
            Color32::BLACK
        );
        assert_eq!(
            lerp_color(Color32::BLACK, Color32::WHITE, 2.0),
            Color32::WHITE
        );
        assert_eq!(blend(Color32::BLACK, Color32::WHITE, 0.0), Color32::BLACK);
        assert_eq!(blend(Color32::BLACK, Color32::WHITE, 1.0), Color32::WHITE);
    }

    #[test]
    fn hash_is_deterministic_and_bounded() {
        let a = hash01(3, 7);
        assert_eq!(a, hash01(3, 7));
        assert!((0.0..=1.0).contains(&a));
        assert_ne!(hash01(3, 7), hash01(7, 3));
    }

    #[test]
    fn wave_buf_tracks_level_and_peak() {
        let mut w = WaveBuf::new();
        assert_eq!(w.level(), 0.0);
        for _ in 0..60 {
            w.advance(0.1, 0.8);
        }
        assert!(w.level() > 0.7, "快攻后应接近目标电平：{}", w.level());
        assert!(w.peak() >= w.level());
        // 归零后峰值缓慢回落，但不低于当前电平。
        for _ in 0..20 {
            w.advance(0.1, 0.0);
        }
        assert!(w.level() < 0.2, "慢放后应明显下降：{}", w.level());
        assert!(w.peak() >= w.level());
    }

    #[test]
    fn wave_buf_clamps_out_of_range_input() {
        let mut w = WaveBuf::new();
        w.advance(0.05, 5.0);
        for _ in 0..20 {
            w.advance(0.05, 5.0);
        }
        assert!(w.level() <= 1.0);
        w.advance(0.05, -3.0);
        assert!(w.level() >= 0.0);
    }
}
