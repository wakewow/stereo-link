//! 手绘线性图标：全部用 `egui::Painter` 画线段/圆弧，不依赖字体、SVG 或图片资源。
//!
//! 为什么不继续用字符当图标：中文单字（路 / 源 / 设 / 志）在不同字体下粗细不一、
//! 基线还会飘；`√ ! ×` 这类符号又依赖字体是否带字形。手绘图标在任何机器上长得一样，
//! 而且能跟着主题色、悬停动画一起变。
//!
//! 约定：所有图标都画在 24×24 的名义网格里（`rect` 会被映射进这个网格），
//! 线宽默认 1.6（相对 24 网格），圆头圆角，只用描边不用填充——这样在 14~20px
//! 的显示尺寸下依然清晰。
//!
//! 结构：几何全部在纯函数 [`shapes`] 里生成（不碰 `Painter`），[`paint`] 只是把它们
//! 交给画笔。这样单元测试可以直接检查顶点数量与绘制范围，不必真的渲染。

use eframe::egui::{self, Color32, Pos2, Rect, Shape, Stroke, Vec2};

/// 图标种类。新增图标要同时更新 [`Icon::ALL`]（测试会遍历它）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// 声道路由：两条平行声道 + 中间的链节。
    Route,
    /// 采集源：环形回路线 + 下行箭头。
    Capture,
    /// 本机播放设备：耳机（和输出用的 [`Icon::Speaker`] 区分开）。
    Devices,
    /// 日志：带折角的文档 + 两行文字。
    Log,
    /// 侧边栏收起。
    ChevronLeft,
    /// 侧边栏展开。
    ChevronRight,
    /// 跟随系统主题。
    Auto,
    /// 浅色主题：太阳。
    Sun,
    /// 深色主题：月亮。
    Moon,
    /// 系统声音设置：齿轮。
    Gear,
    /// 通过 / 正常。
    Check,
    /// 需要注意。
    Warn,
    /// 明确有问题。
    Cross,
    /// 提示。
    Info,
    /// 麦克风。
    Mic,
    /// 扬声器 / 输出。
    Speaker,
    /// 暂停（行内波形暂停态）。
    Pause,
    /// 播放。
    Play,
    /// 手机投放：手机轮廓 + 右侧两道投放波纹。
    Cast,
}

impl Icon {
    /// 全部图标（测试用；也方便将来做「图标一览」调试页）。
    #[allow(dead_code)] // 只在测试里遍历；正式构建没有调用方。
    pub const ALL: [Icon; 19] = [
        Icon::Route,
        Icon::Capture,
        Icon::Devices,
        Icon::Log,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::Auto,
        Icon::Sun,
        Icon::Moon,
        Icon::Gear,
        Icon::Check,
        Icon::Warn,
        Icon::Cross,
        Icon::Info,
        Icon::Mic,
        Icon::Speaker,
        Icon::Pause,
        Icon::Play,
        Icon::Cast,
    ];

    /// 无障碍/调试用的中文名。
    pub fn label(self) -> &'static str {
        match self {
            Icon::Route => "声道路由",
            Icon::Capture => "采集源",
            Icon::Devices => "本机播放设备",
            Icon::Log => "日志",
            Icon::ChevronLeft => "收起",
            Icon::ChevronRight => "展开",
            Icon::Auto => "跟随系统",
            Icon::Sun => "浅色",
            Icon::Moon => "深色",
            Icon::Gear => "设置",
            Icon::Check => "正常",
            Icon::Warn => "注意",
            Icon::Cross => "异常",
            Icon::Info => "提示",
            Icon::Mic => "麦克风",
            Icon::Speaker => "输出",
            Icon::Pause => "暂停",
            Icon::Play => "播放",
            Icon::Cast => "手机投放",
        }
    }
}

// ---------------------------------------------------------------------------
// 几何工具：全部在 24×24 网格坐标系里（x 向右、y 向下）
// ---------------------------------------------------------------------------

/// 24 网格里的一个点。
type Pt = (f32, f32);

/// 圆上一段圆弧的折线点（含首尾两个端点）。角度按度给，y 轴向下。
fn arc(center: Pt, r: f32, from_deg: f32, to_deg: f32, segments: usize) -> Vec<Pt> {
    let segments = segments.max(1);
    (0..=segments)
        .map(|i| {
            let t = i as f32 / segments as f32;
            let a = (from_deg + (to_deg - from_deg) * t).to_radians();
            (center.0 + r * a.cos(), center.1 + r * a.sin())
        })
        .collect()
}

/// `p` 相对圆心 `center` 的角度（度）。
fn angle_deg(center: Pt, p: Pt) -> f32 {
    (p.1 - center.1).atan2(p.0 - center.0).to_degrees()
}

/// 圆上从 `from` 点走到 `to` 点的那段弧。
///
/// `sweep` 取 `+1` 表示角度增大的方向，`-1` 表示角度减小的方向。端点精确落在
/// `from` / `to` 上，方便和别的弧首尾相接。
fn arc_between(center: Pt, r: f32, from: Pt, to: Pt, sweep: f32, segments: usize) -> Vec<Pt> {
    let start = angle_deg(center, from);
    let mut delta = (angle_deg(center, to) - start).rem_euclid(360.0);
    if sweep < 0.0 && delta > 0.0 {
        delta -= 360.0;
    }
    arc(center, r, start, start + delta, segments)
}

/// 两个圆的交点（相离/包含时返回 `None`）。
fn circle_intersections(c0: Pt, r0: f32, c1: Pt, r1: f32) -> Option<[Pt; 2]> {
    let (dx, dy) = (c1.0 - c0.0, c1.1 - c0.1);
    let d = (dx * dx + dy * dy).sqrt();
    if d < 1e-4 || d > r0 + r1 || d < (r0 - r1).abs() {
        return None;
    }
    let a = (d * d + r0 * r0 - r1 * r1) / (2.0 * d);
    let h = (r0 * r0 - a * a).max(0.0).sqrt();
    let (ux, uy) = (dx / d, dy / d);
    let (mx, my) = (c0.0 + a * ux, c0.1 + a * uy);
    Some([(mx - h * uy, my + h * ux), (mx + h * uy, my - h * ux)])
}

/// 箭头两翼的端点：`tip` 是箭尖，`dir` 是箭头指向（单位向量）。
fn arrow_wings(tip: Pt, dir: Pt, len: f32, spread: f32) -> [Pt; 2] {
    let back = (tip.0 - dir.0 * len, tip.1 - dir.1 * len);
    let perp = (-dir.1, dir.0);
    let w = len * spread;
    [
        (back.0 + perp.0 * w, back.1 + perp.1 * w),
        (back.0 - perp.0 * w, back.1 - perp.1 * w),
    ]
}

/// 把 24 网格坐标映射成屏幕像素、并往 `Vec<Shape>` 里攒形状的小画布。
///
/// 它不持有 `Painter`：调用方拿到 `Vec<Shape>` 后自己决定怎么画，所以
/// [`shapes`] 是纯函数，几何可以单测。
struct Canvas<'a> {
    /// 24 网格原点在屏幕上的位置。
    origin: Pos2,
    /// 网格 → 像素的缩放系数（`side / 24`）。
    k: f32,
    color: Color32,
    stroke: Stroke,
    out: &'a mut Vec<Shape>,
}

impl Canvas<'_> {
    fn map(&self, (x, y): Pt) -> Pos2 {
        self.origin + Vec2::new(x * self.k, y * self.k)
    }

    /// 一条线段（两段线各自用 `seg` 画时，接头处不会做斜接，适合本来就不相连的笔画）。
    fn seg(&mut self, a: Pt, b: Pt) {
        self.out.push(Shape::LineSegment {
            points: [self.map(a), self.map(b)],
            stroke: self.stroke,
        });
    }

    /// 开放折线：同一笔画里的拐角由 epaint 正常斜接。
    fn line(&mut self, pts: &[Pt]) {
        let points: Vec<Pos2> = pts.iter().map(|&p| self.map(p)).collect();
        self.out.push(Shape::line(points, self.stroke));
    }

    /// 闭合折线（轮廓）。
    fn closed(&mut self, pts: &[Pt]) {
        let points: Vec<Pos2> = pts.iter().map(|&p| self.map(p)).collect();
        self.out.push(Shape::closed_line(points, self.stroke));
    }

    /// 描边圆（圆环 / 圆形轮廓）。
    fn circle(&mut self, c: Pt, r: f32) {
        self.out
            .push(Shape::circle_stroke(self.map(c), r * self.k, self.stroke));
    }

    /// 实心小圆点：只用于「点」（箭头中心、感叹号下的点、i 上的点），绝不当填充块。
    fn dot(&mut self, c: Pt, r: f32) {
        self.out
            .push(Shape::circle_filled(self.map(c), r * self.k, self.color));
    }
}

/// 返回该图标在 `rect` 里要画的全部 Shape（不触碰 Painter）。
///
/// `rect` 建议是正方形；不是正方形时取短边做正方形并居中。`stroke_w` 以 24 网格
/// 为单位（默认 1.6），会按 `rect` 的实际尺寸换算成像素。
pub fn shapes(icon: Icon, rect: Rect, color: Color32, stroke_w: f32) -> Vec<Shape> {
    let side = rect.width().min(rect.height());
    let grid = Rect::from_center_size(rect.center(), Vec2::splat(side));
    // 名义网格 24×24 → 实际像素。
    let k = side / 24.0;
    let stroke = Stroke::new((stroke_w * k).max(1.0), color);
    let mut out = Vec::with_capacity(16);
    let mut c = Canvas {
        origin: grid.min,
        k,
        color,
        stroke,
        out: &mut out,
    };

    match icon {
        // 声道路由：两条竖直声道轨（左短右长，暗示左右声道）+ 中间的路由节点。
        Icon::Route => {
            c.seg((7.5, 6.5), (7.5, 17.5));
            c.seg((16.5, 4.0), (16.5, 20.0));
            c.seg((7.5, 12.0), (9.8, 12.0));
            c.seg((14.2, 12.0), (16.5, 12.0));
            c.circle((12.0, 12.0), 2.2);
        }
        // 采集源：环回弧（缺口朝上）+ 切线箭头 + 中心点，读作「回环采集」。
        Icon::Capture => {
            let ring = arc((12.0, 12.0), 7.0, -55.0, 235.0, 12);
            c.line(&ring);
            let tip = ring.last().copied().unwrap_or((12.0, 5.0));
            // 235° 处、角度增大方向的切线。
            let a = 235.0f32.to_radians();
            let dir = (-a.sin(), a.cos());
            let wings = arrow_wings(tip, dir, 2.7, 0.58);
            c.line(&[wings[0], tip, wings[1]]);
            c.dot((12.0, 12.0), 1.25);
        }
        // 本机播放设备：耳机（头梁 + 两个耳罩）。
        Icon::Devices => {
            c.line(&arc((12.0, 12.5), 7.6, 180.0, 360.0, 10));
            c.line(&[(4.4, 12.5), (4.4, 18.8), (6.9, 18.8), (6.9, 15.4)]);
            c.line(&[(19.6, 12.5), (19.6, 18.8), (17.1, 18.8), (17.1, 15.4)]);
        }
        // 日志：文档轮廓 + 折角 + 两行文字。
        Icon::Log => {
            c.closed(&[
                (6.0, 3.5),
                (13.5, 3.5),
                (18.0, 8.0),
                (18.0, 20.5),
                (6.0, 20.5),
            ]);
            c.line(&[(13.5, 3.5), (13.5, 8.0), (18.0, 8.0)]);
            c.seg((9.0, 12.0), (15.0, 12.0));
            c.seg((9.0, 16.0), (13.6, 16.0));
        }
        Icon::ChevronLeft => {
            c.seg((14.6, 6.0), (8.4, 12.0));
            c.seg((8.4, 12.0), (14.6, 18.0));
        }
        Icon::ChevronRight => {
            c.seg((9.4, 6.0), (15.6, 12.0));
            c.seg((15.6, 12.0), (9.4, 18.0));
        }
        // 跟随系统：左半边是太阳（半圆 + 三道光线），右半边是半个月亮（半圆 + 竖直平边）。
        //
        // 平边故意用直线：14px 下直线最锐利；它把圆分成「左太阳 / 右半月」，又不会像
        // 再画一段深弧那样糊成两个重叠圆（Venn）。平边两端正好落在半圆的上/下顶点，
        // 与圆弧端帽重合，看不出接缝。
        Icon::Auto => {
            c.line(&arc((12.0, 12.0), 6.2, 90.0, 270.0, 9));
            c.line(&arc((12.0, 12.0), 6.2, 270.0, 450.0, 9));
            for deg in [135.0f32, 180.0, 225.0] {
                let a = deg.to_radians();
                c.seg(
                    (12.0 + 7.6 * a.cos(), 12.0 + 7.6 * a.sin()),
                    (12.0 + 9.7 * a.cos(), 12.0 + 9.7 * a.sin()),
                );
            }
            // 半个月亮的平边 = 圆的竖直直径。
            c.seg((12.0, 5.8), (12.0, 18.2));
        }
        // 浅色主题：太阳（圆 + 八道光线）。
        Icon::Sun => {
            c.circle((12.0, 12.0), 4.3);
            for i in 0..8 {
                let a = (i as f32 * 45.0).to_radians();
                c.seg(
                    (12.0 + 6.3 * a.cos(), 12.0 + 6.3 * a.sin()),
                    (12.0 + 8.8 * a.cos(), 12.0 + 8.8 * a.sin()),
                );
            }
        }
        // 深色主题：月牙。两圆相交取「外弧 + 内弧」。
        //
        // 月牙的两个尖角是 cusp（转向近 180°），如果拆成两条各自到尖角的折线，
        // 尖角处会缺一个斜接三角。这里改在「两段弧的中点」拆开：拆分点切线连续，
        // 两条折线在那里只是平接；两个 cusp 落在折线内部，由 epaint 正常斜接。
        Icon::Moon => {
            if let Some([a, b]) = circle_intersections((12.5, 12.0), 7.2, (9.6, 9.8), 6.4) {
                let outer = arc_between((12.5, 12.0), 7.2, a, b, 1.0, 12);
                let inner = arc_between((9.6, 9.8), 6.4, b, a, -1.0, 10);
                let s = outer.len() / 2;
                let t = inner.len() / 2;
                // 外弧中点 → B → 内弧中点
                let mut head = outer[s..].to_vec();
                head.extend_from_slice(&inner[1..=t]);
                // 内弧中点 → A → 外弧中点
                let mut tail = inner[t..].to_vec();
                tail.extend_from_slice(&outer[1..=s]);
                c.line(&head);
                c.line(&tail);
            }
        }
        // 设置：齿圈 + 8 根短齿 + 中心孔。齿做得短、间距大，16px 下不会糊成实心。
        Icon::Gear => {
            c.circle((12.0, 12.0), 6.9);
            c.circle((12.0, 12.0), 2.7);
            for i in 0..8 {
                let a = (i as f32 * 45.0).to_radians();
                c.seg(
                    (12.0 + 6.9 * a.cos(), 12.0 + 6.9 * a.sin()),
                    (12.0 + 8.7 * a.cos(), 12.0 + 8.7 * a.sin()),
                );
            }
        }
        Icon::Check => {
            c.seg((5.6, 12.4), (10.2, 17.0));
            c.seg((10.2, 17.0), (18.6, 6.8));
        }
        // 注意：三角形 + 感叹号。
        Icon::Warn => {
            c.closed(&[(12.0, 3.8), (21.0, 19.7), (3.0, 19.7)]);
            c.seg((12.0, 9.3), (12.0, 14.4));
            c.dot((12.0, 17.1), 0.95);
        }
        Icon::Cross => {
            c.seg((6.2, 6.2), (17.8, 17.8));
            c.seg((17.8, 6.2), (6.2, 17.8));
        }
        // 提示：圆 + i。
        Icon::Info => {
            c.circle((12.0, 12.0), 8.0);
            c.seg((12.0, 11.0), (12.0, 17.0));
            c.dot((12.0, 7.6), 0.95);
        }
        // 麦克风：圆角胶囊拾音头 + 拾音罩 + 支架。
        Icon::Mic => {
            let mut capsule = arc((12.0, 6.4), 3.0, 180.0, 360.0, 6);
            capsule.push((15.0, 10.4));
            capsule.extend(arc((12.0, 10.4), 3.0, 0.0, 180.0, 6).into_iter().skip(1));
            c.closed(&capsule);
            // 9 个点、8 段，保证正下方那个点（90°）正好落在折线上，支架能接住。
            c.line(&arc((12.0, 10.2), 6.3, 0.0, 180.0, 8));
            c.seg((12.0, 16.5), (12.0, 20.5));
            c.seg((8.8, 20.5), (15.2, 20.5));
        }
        // 扬声器：箱体轮廓 + 两道声音弧。
        Icon::Speaker => {
            c.closed(&[
                (4.0, 9.3),
                (7.6, 9.3),
                (12.4, 4.6),
                (12.4, 19.4),
                (7.6, 14.7),
                (4.0, 14.7),
            ]);
            c.line(&arc((13.4, 12.0), 3.4, -50.0, 50.0, 6));
            c.line(&arc((13.4, 12.0), 6.4, -50.0, 50.0, 8));
        }
        Icon::Pause => {
            c.seg((9.2, 5.5), (9.2, 18.5));
            c.seg((14.8, 5.5), (14.8, 18.5));
        }
        // 播放：三角形。按「两条开放折线」画，拆点取在边的中点（切线连续处），
        // 折线内部的三个拐角由 epaint 正常斜接，尖角不会被切掉。
        Icon::Play => {
            let a = (8.4, 5.2);
            let b = (18.6, 12.0);
            let d = (8.4, 18.8);
            let m1 = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            let m2 = ((b.0 + d.0) / 2.0, (b.1 + d.1) / 2.0);
            c.line(&[m1, b, m2]);
            c.line(&[m2, d, a, m1]);
        }
        // 手机投放：左边手机轮廓（圆角用折线切角近似）+ 右边两道投放波纹。
        Icon::Cast => {
            c.closed(&[
                (5.6, 4.4),
                (9.6, 4.4),
                (10.6, 5.4),
                (10.6, 18.6),
                (9.6, 19.6),
                (5.6, 19.6),
                (4.6, 18.6),
                (4.6, 5.4),
                (5.6, 4.4),
            ]);
            // 底部听筒横线：让轮廓一眼看出是手机而不是卡片。
            c.seg((6.8, 17.4), (8.4, 17.4));
            c.line(&arc((10.7, 12.0), 3.3, -52.0, 52.0, 6));
            c.line(&arc((10.7, 12.0), 6.5, -52.0, 52.0, 8));
        }
    }

    out
}

/// 在 `rect` 里画一个图标（描边色 `color`，线宽按 24 网格换算成像素）。
///
/// `rect` 建议是正方形；不是正方形时取短边做正方形并居中。
pub fn paint(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32, stroke_w: f32) {
    painter.extend(shapes(icon, rect, color, stroke_w));
}

/// 分配 `size × size` 的位置并画图标；返回的 `Response` 可以接 `on_hover_text`。
pub fn show(ui: &mut egui::Ui, icon: Icon, size: f32, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), rect, icon, color, 1.6);
    }
    response
}

/// 程序图标：深色圆角底 + 青/紫两道声道条（64×64，纯代码画，不依赖图片资源）。
///
/// 窗口图标和托盘图标共用这一份 RGBA（`egui::IconData` 的 `rgba` + 边长）。
pub fn app_icon() -> egui::IconData {
    const N: usize = 64;
    const SS: usize = 3; // 每像素 3×3 超采样，边缘抗锯齿
    let mut rgba = vec![0u8; N * N * 4];

    // 单个采样点的颜色（None = 透明背景）
    let sample = |px: f32, py: f32| -> Option<[f32; 3]> {
        // 圆角方块（中心 32,32，半宽 28，圆角 12）
        let (cx, cy, half, radius) = (32.0f32, 32.0f32, 28.0f32, 12.0f32);
        let qx = (px - cx).abs() - (half - radius);
        let qy = (py - cy).abs() - (half - radius);
        let ax = qx.max(0.0);
        let ay = qy.max(0.0);
        let sd = (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - radius;
        if sd > 0.0 {
            return None;
        }

        // 左声道：青色竖条（低）
        if (16.0..=24.0).contains(&px) && (30.0..=46.0).contains(&py) {
            return Some([56.0, 189.0, 248.0]);
        }
        // 右声道：紫色竖条（高）
        if (30.0..=38.0).contains(&px) && (20.0..=46.0).contains(&py) {
            return Some([167.0, 139.0, 250.0]);
        }
        // 中间连接线：把两路连起来（"Link"）
        if (24.0..=30.0).contains(&px) && (36.0..=39.0).contains(&py) {
            return Some([120.0, 200.0, 240.0]);
        }

        // 背景：上浅下深的石墨渐变
        let t = (py / N as f32).clamp(0.0, 1.0);
        Some([
            30.0 + (11.0 - 30.0) * t,
            34.0 + (13.0 - 34.0) * t,
            43.0 + (17.0 - 43.0) * t,
        ])
    };

    for y in 0..N {
        for x in 0..N {
            let mut acc = [0.0f32; 3];
            let mut hits = 0.0f32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if let Some(c) = sample(px, py) {
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        hits += 1.0;
                    }
                }
            }
            let total = (SS * SS) as f32;
            let idx = (y * N + x) * 4;
            if hits > 0.0 {
                rgba[idx] = (acc[0] / hits).round().clamp(0.0, 255.0) as u8;
                rgba[idx + 1] = (acc[1] / hits).round().clamp(0.0, 255.0) as u8;
                rgba[idx + 2] = (acc[2] / hits).round().clamp(0.0, 255.0) as u8;
                rgba[idx + 3] = (255.0 * hits / total).round() as u8;
            }
        }
    }

    egui::IconData {
        rgba,
        width: N as u32,
        height: N as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试统一用 24×24、原点在 (0,0) 的 rect，这样网格坐标 == 像素坐标。
    fn square() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::splat(24.0))
    }

    /// 取出一个 Shape 上的全部顶点（圆用 center ± radius 表示外框）。
    fn shape_points(shape: &Shape, out: &mut Vec<Pos2>) {
        match shape {
            Shape::LineSegment { points, .. } => out.extend_from_slice(points),
            Shape::Path(path) => out.extend_from_slice(&path.points),
            Shape::Circle(circle) => {
                out.push(circle.center + Vec2::new(-circle.radius, -circle.radius));
                out.push(circle.center + Vec2::new(circle.radius, circle.radius));
            }
            _ => {}
        }
    }

    /// 顶点个数：线段算 2 个，折线算点数，圆算 1 个。
    fn vertex_count(shape: &Shape) -> usize {
        match shape {
            Shape::LineSegment { .. } => 2,
            Shape::Path(path) => path.points.len(),
            Shape::Circle(_) => 1,
            _ => 0,
        }
    }

    /// 图标中文名不能重复（将来做图标一览表时会用名字当 key）。
    #[test]
    fn icon_labels_are_unique() {
        for (i, a) in Icon::ALL.iter().enumerate() {
            for b in &Icon::ALL[i + 1..] {
                assert_ne!(a.label(), b.label(), "图标中文名重复：{a:?}");
            }
        }
    }

    /// 每个图标至少两条笔画：只有一条连续折线的图标在 14px 下容易糊成一个点。
    #[test]
    fn every_icon_has_shapes() {
        for icon in Icon::ALL {
            let shapes = shapes(icon, square(), Color32::WHITE, 1.6);
            assert!(
                shapes.len() >= 2,
                "{icon:?} 只画了 {} 个 Shape，至少要有 2 个",
                shapes.len()
            );
        }
    }

    /// 不许画出 24 网格：所有顶点都要落在 rect 向外扩 1px 的范围内（-1..25）。
    #[test]
    fn every_icon_stays_inside_the_grid() {
        let rect = square();
        let bounds = rect.expand(1.0);
        for icon in Icon::ALL {
            let mut points = Vec::new();
            for shape in shapes(icon, rect, Color32::WHITE, 1.6) {
                shape_points(&shape, &mut points);
            }
            for point in points {
                assert!(
                    bounds.contains(point),
                    "{icon:?} 画出了 24 网格：{point:?}（允许范围 {bounds:?}）"
                );
            }
        }
    }

    /// 顶点总数上限 60：防止把圆弧切太碎（18 个图标全画也就千级顶点）。
    #[test]
    fn every_icon_has_a_bounded_vertex_count() {
        for icon in Icon::ALL {
            let total: usize = shapes(icon, square(), Color32::WHITE, 1.6)
                .iter()
                .map(vertex_count)
                .sum();
            assert!(total <= 60, "{icon:?} 用了 {total} 个顶点，超过 60 的上限");
        }
    }
}
