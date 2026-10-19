//! 轻量 toast 浮层：右下角淡入淡出，不占布局、不挤动按钮。
//!
//! 改造前的反馈是「路由页操作行右侧一个固定宽度的文字格」——它不是浮层，
//! 会跟按钮抢位置，也没有出现/消失的过程，看起来像一行常驻说明。
//! 这里换成真浮层：`Area` + `Order::Foreground`，叠在内容之上，2.6 s 后自己走。

use eframe::egui::{self, CornerRadius, Margin, Stroke};

use crate::theme;

/// 一条 toast 存活的总时长（秒）。
const TTL: f32 = 2.6;
/// 淡入时长（秒）。
const FADE_IN: f32 = 0.16;
/// 淡出时长（秒）。
const FADE_OUT: f32 = 0.30;
/// 最多同时显示几条（再多的从最旧的开始丢）。
const MAX_VISIBLE: usize = 3;

struct Item {
    text: String,
    born: f64,
}

/// toast 队列。挂在 `StereoLinkApp` 上，每帧 `show` 一次。
#[derive(Default)]
pub struct Toasts {
    items: Vec<Item>,
}

impl Toasts {
    pub fn new() -> Self {
        Self::default()
    }

    /// 推入一条 toast（立刻显示在右下角，重复内容不合并）。
    pub fn push(&mut self, ctx: &egui::Context, text: impl Into<String>) {
        let now = ctx.input(|i| i.time);
        self.items.push(Item {
            text: text.into(),
            born: now,
        });
        if self.items.len() > MAX_VISIBLE {
            let drop = self.items.len() - MAX_VISIBLE;
            self.items.drain(..drop);
        }
        ctx.request_repaint();
    }

    /// 是否还有存活的 toast（调用方据此决定要不要继续重绘）。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 画一帧；返回是否还有存活项。
    pub fn show(&mut self, ctx: &egui::Context, dark: bool) -> bool {
        let now = ctx.input(|i| i.time);
        self.items.retain(|it| (now - it.born) < TTL as f64);
        if self.items.is_empty() {
            return false;
        }
        let t = theme::tokens(dark);
        let fill = if dark {
            egui::Color32::from_rgba_premultiplied(24, 28, 36, 236)
        } else {
            egui::Color32::from_rgba_premultiplied(250, 251, 253, 244)
        };
        egui::Area::new(egui::Id::new("stereo-link-toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-18.0, -18.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_max_width(340.0);
                // 越新的在越下面：Area 贴着右下角，内容变高就往上长。
                for item in &self.items {
                    let age = (now - item.born) as f32;
                    let alpha = if age < FADE_IN {
                        theme::ease_out(age / FADE_IN)
                    } else if age > TTL - FADE_OUT {
                        ((TTL - age) / FADE_OUT).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    ui.scope(|ui| {
                        ui.set_opacity(alpha);
                        egui::Frame::default()
                            .fill(fill)
                            .stroke(Stroke::new(1.0_f32, t.stroke.gamma_multiply(1.5)))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(13, 9))
                            .shadow(egui::epaint::Shadow {
                                offset: [0, 8],
                                blur: 22,
                                spread: 0,
                                color: egui::Color32::from_black_alpha(if dark { 90 } else { 40 }),
                            })
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(&item.text)
                                        .color(t.text)
                                        .size(12.5)
                                        .strong(),
                                );
                            });
                    });
                    ui.add_space(8.0);
                }
            });
        // 有东西在淡入/淡出就必须持续重绘，否则动画会一格一格跳。
        ctx.request_repaint();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_capped_at_max_visible() {
        let ctx = egui::Context::default();
        let mut toasts = Toasts::new();
        for i in 0..10 {
            toasts.push(&ctx, format!("第 {i} 条"));
        }
        assert_eq!(toasts.items.len(), MAX_VISIBLE);
        // 保留的是最新的三条。
        assert!(toasts.items.last().unwrap().text.contains("第 9 条"));
        assert!(toasts.items.first().unwrap().text.contains("第 7 条"));
    }

    #[test]
    fn expired_items_are_dropped() {
        let ctx = egui::Context::default();
        let mut toasts = Toasts::new();
        toasts.push(&ctx, "过期");
        // 手动把出生时间推到 TTL 之外，再跑一次 show 应该清空。
        toasts.items[0].born -= TTL as f64 + 1.0;
        let _ = toasts.show(&ctx, true);
        assert!(toasts.items.is_empty());
        assert!(toasts.is_empty());
    }
}
