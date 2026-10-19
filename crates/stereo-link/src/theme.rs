//! 玻璃拟态（Glassmorphism）设计系统：设计 token、柔光背景纹理、玻璃卡片、
//! 行内波形/频谱与分段电平条。
//!
//! egui 是即时模式、**没有 `backdrop-filter`**，所以「毛玻璃」用三层近似：
//!
//! 1. 底层即时画一片「柔光底纹」：整屏竖直渐变 + 四角对称的柔光色斑 +
//!    极淡噪点，全部是 `Mesh` 三角（画在 `egui::LayerId::background()`）。
//!    色斑在**像素空间**是正圆，四团同半径、同透明度，漂移只改圆心；
//! 2. 卡片/侧边栏用**半透明**填充（alpha 0.05~0.20），让底纹透上来 —— 这是
//!    视觉上最接近「磨砂玻璃」的部分；
//! 3. 1px 描边 + 顶部内高光 + 阴影，制造玻璃的厚度与光感。
//!
//! ⚠️ egui 的 `Color32` 是**预乘 alpha** 格式，GL 混合是
//! `out = src.rgb + dst.rgb * (1 - src.a)`。所以「白色 + 低 alpha」必须写成
//! `from_rgba_premultiplied(a, a, a, a)`（即 `from_white_alpha(a)`），
//! 而不是 `from_rgba_premultiplied(255, 255, 255, a)` —— 后者 RGB(255) 远超
//! alpha(a)，混合时会过冲饱和成**刺眼的纯白 1px 实线**（曾经全站卡片/侧边栏/
//! 行内波形框都套着这样一圈白边）。合法预乘色的硬约束：任一通道 ≤ alpha。
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
    /// 「抬升玻璃」层级：留给浮层/提示气泡这类真正浮在卡片之上的面。
    ///
    /// 行内波形框原本用它当底，但设计原型里那一块是**无边框的极淡底色**，
    /// 所以暂时没有消费者；保留 token 以免下次做浮层时又要重新配色。
    #[allow(dead_code)]
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
    /// 色斑峰值不透明度（0..=255）。
    pub blob_alpha: u8,
    /// 色斑半径（px，像素空间正圆，不随窗口宽高比拉伸）。
    pub blob_radius: f32,
    /// 细噪点最大叠加 alpha（0 = 关闭）。
    pub grain_alpha: u8,
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
    // 白描边/内高光：预乘色必须 RGB == alpha，写 (255,255,255,38) 会变成纯白实线。
    stroke: Color32::from_rgba_premultiplied(38, 38, 38, 38),
    highlight: Color32::from_rgba_premultiplied(46, 46, 46, 46),
    accent: Color32::from_rgb(56, 189, 248),
    accent2: Color32::from_rgb(167, 139, 250),
    wave_l: Color32::from_rgb(96, 165, 250),
    wave_r: Color32::from_rgb(251, 191, 36),
    meter_low: Color32::from_rgb(74, 222, 128),
    meter_mid: Color32::from_rgb(251, 191, 36),
    meter_high: Color32::from_rgb(248, 113, 113),
    bg_top: Color32::from_rgb(20, 24, 33),
    bg_bottom: Color32::from_rgb(8, 10, 14),
    blob_alpha: 64,
    blob_radius: 520.0,
    // 噪点上限：渲染偏移 = (255-L_bg)·a/255，a 在 [0,α] 上均匀 → σ ≈ 0.247·α（L_bg≈37）。
    // α=7 时 σ≈1.7 luma，是设计原型的 2 倍多，3 倍裁切下肉眼可见颗粒；α=3 → σ≈0.74，与原型 0.82 同级。
    grain_alpha: 3,
    radius: 14,
    dur_micro: 0.18,
};

/// 浅色 token。
pub const LIGHT_TOKENS: Tokens = Tokens {
    // 白色玻璃同样必须预乘：白色 @ α 的预乘值是 (α, α, α, α)，写 (255,255,255,α)
    // 会变成「不透明的纯白卡片」——浅色主题会失去通透感，也会盖掉底层柔光。
    glass_base: Color32::from_rgba_premultiplied(186, 186, 186, 186),
    glass_raised: Color32::from_rgba_premultiplied(214, 214, 214, 214),
    // (248,251,255) @ 76.9% → (191,193,196,196)。
    sidebar: Color32::from_rgba_premultiplied(191, 193, 196, 196),
    // 浅色主题的描边是「深蓝 + 低 alpha」：预乘值 = 各通道 × alpha/255。
    // (15,23,42) @ 13.3% → (2,3,6,34)。写原始色会过冲成一条发亮的蓝灰实线。
    stroke: Color32::from_rgba_premultiplied(2, 3, 6, 34),
    highlight: Color32::from_rgba_premultiplied(220, 220, 220, 220),
    accent: Color32::from_rgb(2, 132, 199),
    accent2: Color32::from_rgb(109, 40, 217),
    wave_l: Color32::from_rgb(37, 99, 235),
    wave_r: Color32::from_rgb(217, 119, 6),
    meter_low: Color32::from_rgb(21, 128, 61),
    meter_mid: Color32::from_rgb(180, 83, 9),
    meter_high: Color32::from_rgb(185, 28, 28),
    bg_top: Color32::from_rgb(240, 245, 251),
    bg_bottom: Color32::from_rgb(222, 230, 241),
    blob_alpha: 80,
    blob_radius: 520.0,
    // 浅色底噪点本就极淡（(255-L_bg) 只有约 19），与深色取同一上限即可。
    grain_alpha: 3,
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

/// 取（并缓存）一层**无缝**细噪点贴图：128×128，alpha 落在 `0..=max_alpha`。
///
/// 白噪逐像素互不相关，平铺时不存在「接缝」可言 —— 这跟把一张**渐变**贴图拉去
/// 平铺是两回事：后者在环绕处必然出现一条硬边（本轮修掉的缺陷之一）。
fn grain_texture(ctx: &egui::Context, max_alpha: u8) -> egui::TextureHandle {
    let id = egui::Id::new(("stereo-link-grain", max_alpha));
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return tex;
    }
    const S: usize = 128;
    let mut pixels = Vec::with_capacity(S * S);
    for y in 0..S {
        for x in 0..S {
            let a = (hash01(x as u32, y as u32) * max_alpha as f32).round() as u8;
            pixels.push(Color32::from_rgba_premultiplied(a, a, a, a));
        }
    }
    let tex = ctx.load_texture(
        "stereo-link-grain",
        egui::ColorImage {
            size: [S, S],
            pixels,
        },
        egui::TextureOptions {
            magnification: egui::TextureFilter::Nearest,
            minification: egui::TextureFilter::Nearest,
            wrap_mode: egui::TextureWrapMode::Repeat,
            mipmap_mode: None,
        },
    );
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

/// 往网格里追加一个四边形：上边两顶点取 `c_top`、下边两顶点取 `c_bottom`，
/// 顶点色线性插值出来就是一条竖直渐变（顶点顺序与 `Mesh::add_colored_rect` 一致）。
fn push_gradient_quad(mesh: &mut egui::Mesh, rect: Rect, c_top: Color32, c_bottom: Color32) {
    let base = mesh.vertices.len() as u32;
    let v = |pos: egui::Pos2, color: Color32| egui::epaint::Vertex {
        pos,
        uv: egui::Pos2::ZERO,
        color,
    };
    mesh.vertices.push(v(rect.left_top(), c_top));
    mesh.vertices.push(v(rect.right_top(), c_top));
    mesh.vertices.push(v(rect.right_bottom(), c_bottom));
    mesh.vertices.push(v(rect.left_bottom(), c_bottom));
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// 往网格里追加一团柔光色斑：像素空间**正圆**，中心最亮、向外衰减到全透明。
///
/// 用同心环三角带近似径向渐变，环数越多过渡越接近真模糊。颜色全程保持预乘
/// （RGB 与 alpha 同比例衰减），插值时色相不会跑偏，也不会在边缘留下死黑圈。
fn push_blob(
    mesh: &mut egui::Mesh,
    center: egui::Pos2,
    radius: f32,
    color: Color32,
    alpha: u8,
    segments: usize,
    rings: usize,
) {
    if radius <= 1.0 || alpha == 0 || rings == 0 || segments < 3 {
        return;
    }
    let tint = |a: u8| {
        let s = a as u32;
        Color32::from_rgba_premultiplied(
            (color.r() as u32 * s / 255) as u8,
            (color.g() as u32 * s / 255) as u8,
            (color.b() as u32 * s / 255) as u8,
            a,
        )
    };
    let mut starts = Vec::with_capacity(rings + 1);
    for k in 0..=rings {
        starts.push(mesh.vertices.len() as u32);
        let x = k as f32 / rings as f32;
        // 1.2 次幂：比线性稍快地收到 0，边缘更柔，又不像高斯那样只剩中心一小坨。
        let a = ((1.0 - x).powf(1.2) * alpha as f32)
            .round()
            .clamp(0.0, 255.0) as u8;
        let c = tint(a);
        let r = radius * x;
        for i in 0..segments {
            let ang = i as f32 / segments as f32 * std::f32::consts::TAU;
            mesh.vertices.push(egui::epaint::Vertex {
                pos: center + Vec2::new(ang.cos(), ang.sin()) * r,
                uv: egui::Pos2::ZERO,
                color: c,
            });
        }
    }
    for k in 0..rings {
        let (inner, outer) = (starts[k], starts[k + 1]);
        for i in 0..segments as u32 {
            let j = (i + 1) % segments as u32;
            mesh.indices.extend_from_slice(&[
                inner + i,
                inner + j,
                outer + j,
                inner + i,
                outer + j,
                outer + i,
            ]);
        }
    }
}

/// 四角柔光色斑的取色：左上 / 右上 / 左下 / 右下。
///
/// 四团**同半径、同透明度**，只有色相不同 —— 这是「背景不像几块拼板」的关键。
/// 深色主题直接复用语义色（左声道蓝 / 次强调紫 / 主强调青 / 右声道琥珀）；浅色
/// 主题下同一组色相必须调淡，饱和色铺在浅底上会发脏。
fn blob_colors(dark: bool) -> [Color32; 4] {
    let t = tokens(dark);
    if dark {
        [t.wave_l, t.accent2, t.accent, t.wave_r]
    } else {
        [
            Color32::from_rgb(147, 197, 253),
            Color32::from_rgb(196, 181, 253),
            Color32::from_rgb(125, 211, 252),
            Color32::from_rgb(253, 186, 116),
        ]
    }
}

/// 把「柔光底纹」铺满 `rect`（画在窗口最底层）。`time` 用 `ctx.input(|i| i.time)`。
///
/// 全部即时画成网格，**不再用平铺贴图**：贴图方案有两处硬伤 ——
/// (1) 贴图在环绕处不连续，`uv` 越过 1.0 时留下一条笔直的接缝；
/// (2) 正方形贴图铺进非正方形窗口，色斑会被宽高比压成椭圆。
/// 现在色斑在像素空间是正圆、四团同半径同透明度四角对称，背景才是一片连贯
/// 的柔光，而不是几块颜色各异的拼板。漂移只改圆心，每帧不重新栅格化任何东西。
pub fn paint_background(ctx: &egui::Context, rect: Rect, dark: bool, time: f64) {
    let t = tokens(dark);
    let painter = ctx.layer_painter(egui::LayerId::background());
    let mut mesh = egui::Mesh::default();

    // 1) 基底：整屏竖直渐变（顶亮底暗）。
    push_gradient_quad(&mut mesh, rect, t.bg_top, t.bg_bottom);

    // 2) 四团柔光色斑：圆心固定在窗口的 20%/80% 与 26%/72% 处 —— 取位与设计原型
    //    一致，且天然关于窗口中心对称，窗口缩放不会失衡。
    let r = t.blob_radius;
    let cx = [
        rect.left() + rect.width() * 0.20,
        rect.left() + rect.width() * 0.80,
    ];
    let cy = [
        rect.top() + rect.height() * 0.26,
        rect.top() + rect.height() * 0.72,
    ];
    let spots = [
        egui::pos2(cx[0], cy[0]),
        egui::pos2(cx[1], cy[0]),
        egui::pos2(cx[0], cy[1]),
        egui::pos2(cx[1], cy[1]),
    ];
    // 原型 drift1..4 的周期（秒）：四团各漂各的，看起来不循环。
    const PERIODS: [f32; 4] = [46.0, 61.0, 74.0, 53.0];
    let blobs = blob_colors(dark);
    let now = time as f32;
    for (i, spot) in spots.iter().enumerate() {
        let phase = std::f32::consts::TAU * now / PERIODS[i] + i as f32 * 1.7;
        let drift = Vec2::new(phase.cos(), (phase * 0.77).sin()) * (r * 0.045);
        push_blob(&mut mesh, *spot + drift, r, blobs[i], t.blob_alpha, 48, 8);
    }
    painter.add(mesh);

    // 3) 极淡噪点：打散渐变的「塑料感」，也让玻璃的透光有颗粒。
    if t.grain_alpha > 0 {
        let tex = grain_texture(ctx, t.grain_alpha);
        let uv = Rect::from_min_max(
            egui::Pos2::ZERO,
            egui::pos2(rect.width() / 128.0, rect.height() / 128.0),
        );
        let mut grain = egui::Mesh::with_texture(tex.id());
        grain.add_rect_with_uv(rect, uv, Color32::WHITE);
        painter.add(grain);
    }
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
        let painter = ui.painter();
        // 底框：设计原型里这一块**没有描边**，只有一层几乎看不见的提亮/压暗。
        // 之前那圈 1px 白描边正是「莫名其妙的白边」的来源之一 —— 一行一个白框，
        // 把整张路由表切成一排小盒子。这里跟原型对齐：无边框、纯极淡底色。
        let radius = CornerRadius::same(8);
        let fill = if dark {
            Color32::from_white_alpha(9)
        } else {
            Color32::from_rgba_premultiplied(1, 1, 1, 8) // (15,23,42) @ 3%
        };
        painter.rect_filled(rect, radius, fill);
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
///
/// 底纹改成网格绘制后界面里暂时没人调它，但它是颜色工具的一部分、且被下面的
/// 单测覆盖，先留着备用（不要因为它「没人用」就把测试一起删了）。
#[allow(dead_code)]
pub fn blend(base: Color32, over: Color32, alpha: f32) -> Color32 {
    lerp_color(base, over, alpha.clamp(0.0, 1.0))
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

    /// `Color32` 是**预乘** alpha：任一通道大于 alpha 就是非法值，GL 的
    /// `ONE / ONE_MINUS_SRC_ALPHA` 混合会把它放大成过冲的刺眼纯白。
    /// 这条断言就是为了让「白边」以测试失败的形式复现，而不是靠肉眼看截图。
    #[test]
    fn all_token_colors_are_valid_premultiplied() {
        let check = |name: &str, c: Color32| {
            let a = c.a();
            assert!(
                c.r() <= a && c.g() <= a && c.b() <= a,
                "{name} 不是合法的预乘色：rgba=({}, {}, {}, {}) —— 通道值必须 <= alpha",
                c.r(),
                c.g(),
                c.b(),
                a
            );
        };
        for (theme, t) in [("dark", DARK_TOKENS), ("light", LIGHT_TOKENS)] {
            check(&format!("{theme}.glass_base"), t.glass_base);
            check(&format!("{theme}.glass_raised"), t.glass_raised);
            check(&format!("{theme}.sidebar"), t.sidebar);
            check(&format!("{theme}.stroke"), t.stroke);
            check(&format!("{theme}.highlight"), t.highlight);
            // 描边/高光本身也不能是「白到发光」的：1px 线的 alpha 必须收敛。
            assert!(
                t.stroke.a() <= 64,
                "{theme}.stroke 的 alpha 过大（{}），1px 描边会变成刺眼白线",
                t.stroke.a()
            );
            assert!(
                t.highlight.a() <= 230,
                "{theme}.highlight 的 alpha 过大（{}）",
                t.highlight.a()
            );
        }
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
