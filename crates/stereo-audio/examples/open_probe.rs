//! 打开端点探针：对同一个 render 端点依次尝试多种 PCM 格式，报告哪一种能开流。
//!
//! 用法：`cargo run -q -p stereo-audio --example open_probe -- <端点名子串>`
//!
//! 用途：定位 `AUDCLNT_E_UNSUPPORTED_FORMAT (0x88890008)` 到底卡在哪种格式上
//! （蓝牙音箱 A2DP 常常是 44100，而另一台是 48000）。

use stereo_audio::{build_sine_s16, playback::play_pcm_s16, DeviceSelector, PcmFormat};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Xiaomi Sound-5320".to_string());
    println!("== 打开端点探针：{target} ==");

    let formats = [
        (44_100u32, 2u16, 16u16),
        (48_000, 2, 16),
        (44_100, 2, 24),
        (48_000, 2, 24),
        (44_100, 2, 32),
        (48_000, 2, 32),
    ];

    for (rate, channels, bits) in formats {
        let fmt = PcmFormat::new(rate, channels, bits);
        let pcm = build_sine_s16(440.0, 0.2, 0.15, fmt);
        let sel = DeviceSelector::name_contains(&target);
        match play_pcm_s16(&sel, fmt, &pcm) {
            Ok(info) => println!("  OK  {rate} Hz / {channels} ch / {bits} bit -> {}", info.describe()),
            Err(e) => println!("  ERR {rate} Hz / {channels} ch / {bits} bit -> {e}"),
        }
    }
    Ok(())
}
