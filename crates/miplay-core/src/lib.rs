//! miplay-bridge 核心库
//!
//! 目前包含三块：
//! - [`token`]：小米账号凭据（与 MiService / MiXPlay 文件格式兼容）
//! - [`device`]：设备模型
//! - [`mina`]：小米云 MiNA API 客户端（设备发现 / 播放控制 / 音量）

pub mod device;
pub mod mina;
pub mod rand;
pub mod token;

pub use device::XiaomiDevice;
pub use mina::{MinaClient, DEFAULT_AUDIO_ID, DEFAULT_CP_ID, PLAY_MUSIC_HARDWARE};
pub use token::MiToken;
