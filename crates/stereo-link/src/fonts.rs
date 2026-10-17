//! 中文字体：egui 自带字体没有 CJK，直接显示中文会成方块。
//!
//! 从 Windows 系统字体目录里挑一个中文字体塞给 egui。优先微软雅黑（比黑体好看），
//! 退到黑体/等线/宋体；也可以用环境变量 `STEREOLINK_FONT`（兼容旧的 `MIPLAY_FONT`） 指定任意 .ttf/.ttc。
//!
//! 字体文件只读一次、进内存；中文 UI 的渲染质量完全取决于这一步是否成功，
//! 所以失败时会返回 `None` 并由界面显式提示「中文可能显示为方块」。

use std::path::{Path, PathBuf};

use eframe::egui;

/// 候选系统字体：(文件名, 字体集合索引)。
const CANDIDATES: &[(&str, u32)] = &[
    ("msyh.ttc", 0),   // 微软雅黑
    ("msyh.ttf", 0),   // 老版本 Windows 的雅黑
    ("simhei.ttf", 0), // 黑体
    ("Deng.ttf", 0),   // 等线
    ("simsun.ttc", 0), // 宋体
];

/// 系统字体目录。
fn font_dir() -> PathBuf {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
    Path::new(&windir).join("Fonts")
}

/// 装中文字体到 `egui` 上下文。返回实际用的字体文件路径。
pub fn install(ctx: &egui::Context) -> Option<String> {
    let (path, bytes, index) = load_font_bytes()?;

    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    let mut fonts = egui::FontDefinitions::default();
    let name = "cjk".to_string();
    fonts.font_data.insert(name.clone(), std::sync::Arc::new(data));
    // 中文字体放最前面：缺字时再回落到 egui 自带的英文/符号字体。
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, name.clone());
    }
    ctx.set_fonts(fonts);
    Some(path)
}

/// 按候选表读字体，返回 (路径, 字节, 集合索引)。
fn load_font_bytes() -> Option<(String, Vec<u8>, u32)> {
    // 显式指定优先
    if let Some(custom) = std::env::var_os("STEREOLINK_FONT").or_else(|| std::env::var_os("MIPLAY_FONT")) {
        let path = PathBuf::from(custom);
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() > 100_000 => {
                return Some((path.display().to_string(), bytes, 0));
            }
            _ => {
                tracing::warn!("STEREOLINK_FONT/ MIPLAY_FONT={} 读不到或不是有效字体，改用系统字体", path.display());
            }
        }
    }

    let dir = font_dir();
    for (file, index) in CANDIDATES {
        let path = dir.join(file);
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() > 100_000 => {
                return Some((path.display().to_string(), bytes, *index));
            }
            _ => continue,
        }
    }
    None
}

/// 当前系统里能用来显示中文的字体（给界面的自检提示用）。
pub fn candidates_present() -> Vec<String> {
    let dir = font_dir();
    CANDIDATES
        .iter()
        .map(|(file, _)| dir.join(file))
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .collect()
}
