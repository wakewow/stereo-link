//! StereoLink 立体声链 —— 库入口（GUI 与命令行两个 exe 共用这一份实现）。
//!
//! 为什么拆成 lib + 两个 bin：
//! * `stereo-link.exe` 是窗口子系统（`windows_subsystem = "windows"`，双击不弹黑框）；
//! * `stereo-link-cli.exe` 是控制台子系统，`status | ConvertFrom-Json` 这类管道才成立。
//!
//! 两个 exe 共享同一套引擎、接收端、配置和控制面代码——界面与命令行的行为不会漂。

pub mod app;
pub mod audio_default;
pub mod cli;
pub mod close_flow;
pub mod config;
pub mod control;
pub mod endpoint_poll;
pub mod fonts;
pub mod icons;
pub mod logbuf;
pub mod receiver;
pub mod serve;
pub mod theme;
pub mod toast;
pub mod tray;

pub use icons::app_icon;
