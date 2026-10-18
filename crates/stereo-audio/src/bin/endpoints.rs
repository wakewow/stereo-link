//! 列出本机音频端点（名字 / id / 默认标记 / 混音格式）。
//!
//! ```text
//! cargo run -p stereo-audio --bin endpoints
//! ```

use stereo_audio::{
    default_endpoint, list_endpoints, resolve_endpoint, CaptureConfig, CaptureMode, DeviceSelector,
};

fn main() {
    println!("== 本机音频端点 ==");
    let list = match list_endpoints() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("list_endpoints() 失败：{e}");
            std::process::exit(1);
        }
    };
    println!("共 {} 个活动端点：", list.len());
    for ep in &list {
        println!("  {}", ep.describe());
        if let Some(err) = &ep.error {
            println!("      ⚠ 无法读混音格式：{err}");
        }
    }

    println!();
    println!("== 默认端点 ==");
    match default_endpoint() {
        Ok(ep) => println!("  {}", ep.describe()),
        Err(e) => println!("  读默认端点失败：{e}"),
    }

    println!();
    println!("== 默认回环采集会选中的端点（不打开流）==");
    match resolve_endpoint(&DeviceSelector::Default, CaptureMode::Loopback) {
        Ok(ep) => println!("  {}", ep.describe()),
        Err(e) => println!("  解析失败：{e}"),
    }

    let cfg = CaptureConfig::default();
    println!();
    println!("== 默认采集配置 ==");
    println!(
        "  mode={} device={} 输出={} Hz/{} ch/16 bit 环形缓冲={} 秒 WASAPI 缓冲={} ms",
        cfg.mode.as_str(),
        cfg.device.describe(),
        cfg.samplerate,
        cfg.channels,
        cfg.ring_seconds,
        cfg.buffer_ms
    );
    println!(
        "  环形缓冲字节数={}（Python 参考实现 pc_stereo_hub.py 也是 5 秒 / 44100 / 2ch）",
        cfg.ring_bytes()
    );
}
