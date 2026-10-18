//! 关闭确认流程：窗口右上角点「关闭」时，按配置决定「缩小到托盘 / 直接退出 / 先问一句」。
//!
//! 用法（app.rs 的 `update()` 开头，每帧调用一次）：
//!
//! ```ignore
//! let outcome = crate::close_flow::on_close_requested(ctx, &mut self.cfg, &mut self.close_state);
//! match outcome {
//!     CloseOutcome::None => {}
//!     CloseOutcome::MinimizeToTray => ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)),
//!     CloseOutcome::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
//! }
//! ```
//!
//! 约定：本模块**不写配置文件**。需要落盘时由调用方在拿到 outcome / 保存状态后统一
//! `cfg.save()`（例如选择「下次不再提示」后，`cfg.close_behavior` 已被本模块改好，
//! 调用方照常 save 即可）。
//!
//! 实现细节：确认框出现在「已经取消掉本次关闭请求」之后，所以用户点「直接关闭」时
//! 并不会再收到一次 `close_requested`；本模块用一个线程内的 one-shot 标记
//! [`PENDING_QUIT`] 记住「这是确认框里选的退出」，下一帧直接放行关闭。

use eframe::egui;

use crate::config::{AppConfig, CloseBehavior};

/// `on_close_requested` 的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// 无事发生（没点关闭，或确认框还开着）。
    None,
    /// 调用方应把主窗口隐藏到托盘（`ViewportCommand::Visible(false)`）。
    MinimizeToTray,
    /// 调用方应放行关闭（`ViewportCommand::Close`），让 eframe 走 `on_exit`。
    Quit,
}

/// 关闭流程的跨帧状态，放在 app 结构体里，默认值即可。
#[derive(Debug, Clone, Copy, Default)]
pub struct CloseState {
    /// 确认框是否正在显示。
    pub open: bool,
    /// 确认框里「下次不再提示」是否被勾上。
    pub remember: bool,
}

impl CloseState {
    /// 「用户选择了某个行为，并且勾了下次不再提示」→ 应该写回的配置值。
    ///
    /// 单独抽出来是为了让调用方也能复用（例如托盘菜单里选了「退出」也要记住）。
    pub fn remembered_behavior(outcome: CloseOutcome) -> Option<CloseBehavior> {
        match outcome {
            CloseOutcome::MinimizeToTray => Some(CloseBehavior::Minimize),
            CloseOutcome::Quit => Some(CloseBehavior::Quit),
            CloseOutcome::None => None,
        }
    }
}

// 确认框里用户选了「直接关闭」、但没勾「下次不再提示」时的 one-shot 标记：
// 下一帧的关闭请求是确认框自己触发的，必须放行而不是再弹一次框。
//
// 用线程局部变量而不是给 CloseState 加字段：接口是冻结的，UI 只在一个线程上跑。
thread_local! {
    static PENDING_QUIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn take_pending_quit() -> bool {
    PENDING_QUIT.with(|pending| pending.replace(false))
}

fn set_pending_quit() {
    PENDING_QUIT.with(|pending| pending.set(true));
}

/// 每帧 `update()` 里调用：检测关闭请求，必要时取消关闭并弹出确认框，返回调用方要执行的动作。
pub fn on_close_requested(
    ctx: &egui::Context,
    cfg: &mut AppConfig,
    state: &mut CloseState,
) -> CloseOutcome {
    let close_requested = ctx.input(|i| i.viewport().close_requested());

    if close_requested {
        // 确认框里点「直接关闭」→ 这一帧的关闭请求是我们自己发的，直接放行。
        if take_pending_quit() {
            state.open = false;
            return CloseOutcome::Quit;
        }

        match cfg.close_behavior {
            CloseBehavior::Minimize => {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                return CloseOutcome::MinimizeToTray;
            }
            CloseBehavior::Quit => {
                // 放行关闭，由 eframe 走 on_exit（停路由 / 恢复默认输出）。
                return CloseOutcome::Quit;
            }
            CloseBehavior::Ask => {
                // 先取消这次关闭，改为弹确认框。
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                state.open = true;
                // 关闭请求的同一帧不画框（本帧的 UI 已经画完，交给下一帧）。
                ctx.request_repaint();
                return CloseOutcome::None;
            }
        }
    }

    if !state.open {
        return CloseOutcome::None;
    }

    // 对话框开着：ESC = 取消（不退出、不缩托盘）。
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.open = false;
        return CloseOutcome::None;
    }

    let enter_pressed = ctx.input(|i| i.key_pressed(egui::Key::Enter));
    let mut open = state.open;
    let mut remember = state.remember;
    let mut chosen: Option<CloseOutcome> = None;

    egui::Window::new("关闭 StereoLink")
        .collapsible(false)
        .resizable(false)
        // 居中：主窗口无论在哪里，确认框都在用户眼前。
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_min_width(380.0);

            ui.label("要把 StereoLink 缩小到系统托盘吗？");
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "「缩小至托盘」：窗口藏起来，路由与左右声道输出继续在后台运行，\
                     双击托盘图标可唤回。",
                )
                .weak(),
            );
            ui.label(
                egui::RichText::new("「直接关闭」：停止路由、恢复系统默认输出并退出。").weak(),
            );

            ui.add_space(10.0);
            ui.checkbox(&mut remember, "下次不再提示（记住我的选择）");

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new("缩小至托盘")).clicked() {
                    chosen = Some(CloseOutcome::MinimizeToTray);
                }
                if ui.button("直接关闭").clicked() {
                    chosen = Some(CloseOutcome::Quit);
                }
            });
        });

    // 弹框右上角的 × 等价于取消。
    state.open = open;
    state.remember = remember;

    // 用户按回车 = 默认按钮「缩小至托盘」。
    if chosen.is_none() && state.open && enter_pressed {
        chosen = Some(CloseOutcome::MinimizeToTray);
    }

    let Some(outcome) = chosen else {
        if state.open {
            ctx.request_repaint();
        }
        return CloseOutcome::None;
    };

    // 勾了「下次不再提示」才写配置；没勾就保持原行为（下次还问）。
    if state.remember {
        if let Some(behavior) = CloseState::remembered_behavior(outcome) {
            cfg.close_behavior = behavior;
        }
    }

    state.open = false;
    if outcome == CloseOutcome::Quit && !state.remember {
        // 没记住选择时，下一步（app 发 ViewportCommand::Close）还会再来一次关闭请求，
        // 那一帧必须放行，不能再弹框。
        set_pending_quit();
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_behavior_maps_outcomes() {
        assert_eq!(
            CloseState::remembered_behavior(CloseOutcome::MinimizeToTray),
            Some(CloseBehavior::Minimize)
        );
        assert_eq!(
            CloseState::remembered_behavior(CloseOutcome::Quit),
            Some(CloseBehavior::Quit)
        );
        assert_eq!(CloseState::remembered_behavior(CloseOutcome::None), None);
    }

    #[test]
    fn close_state_defaults_to_closed_and_unremembered() {
        let state = CloseState::default();
        assert!(!state.open);
        assert!(!state.remember);
    }
}
