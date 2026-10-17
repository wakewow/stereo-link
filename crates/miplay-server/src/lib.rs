//! miplay-bridge 库入口。
//!
//! 这个 crate 同时被两种前端使用：
//! * `src/main.rs` —— 命令行（`devices` / `serve` / `hub` / `stream` / `token` / `ubus`）；
//! * `crates/miplay-desktop` —— Rust 桌面 GUI，复用 [`hub`] 里的引擎（[`hub::engine`]）。
//!
//! 把公共部分放在 lib 里，是为了让 GUI 不必复制一份「采集 → 分声道 → HTTP 拉流」的逻辑；
//! 命令行行为与以前完全一致（`miplay-bridge hub ...` 仍然可用）。

pub mod hub;
pub mod stream;

const INDEX_HTML: &str = include_str!("../../../web/index.html");

/// 优先读磁盘上的 `web/index.html`（改前端不用重编译），
/// 读不到就回退到编译期嵌入的副本（保证单文件分发仍可用）。
pub fn load_index_html() -> String {
    for dir in [std::env::var("MIPLAY_WEB_DIR").ok(), Some("web".to_string())] {
        if let Some(dir) = dir {
            let p = std::path::Path::new(&dir).join("index.html");
            if let Ok(s) = std::fs::read_to_string(&p) {
                return s;
            }
        }
    }
    INDEX_HTML.to_string()
}
