//! 同时往两个 render 端点播不同频率，验证「Windows 能否两条蓝牙链路同时出声」。
//!
//! 用法：
//! ```powershell
//! cargo run -p miplay-server --example probe_dual_bt -- <左端点名子串> <右端点名子串> [秒数]
//! ```
//! 默认：左 `Xiaomi Sound-5320`（单台音箱）440 Hz，右 `立体声组合` 880 Hz，各 4 秒。
//! 两个端点若同时出声 = 双 A2DP 并存可行；若只有一边响/报错 = 需要第二只蓝牙适配器。

use std::thread;
use std::time::Duration;

use miplay_audio::{play_sine, DeviceSelector};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let left = args
        .first()
        .cloned()
        .unwrap_or_else(|| "Xiaomi Sound-5320".to_owned());
    let right = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "立体声组合".to_owned());
    let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4.0);

    println!("== 双蓝牙端点并发播放测试 ==");
    println!("左端点: {left}  <= 440 Hz，{secs}s");
    println!("右端点: {right} <= 880 Hz，{secs}s");
    println!("请听：两台音箱是否同时响、同时停。");

    let l = left.clone();
    let h1 = thread::spawn(move || play_sine(&DeviceSelector::name_contains(l), 440.0, secs, 0.6));
    thread::sleep(Duration::from_millis(50));
    let r = right.clone();
    let h2 = thread::spawn(move || play_sine(&DeviceSelector::name_contains(r), 880.0, secs, 0.6));

    match h1.join().expect("左线程 panic") {
        Ok(info) => println!("左 OK: {}", info.describe()),
        Err(e) => println!("左 失败: {e}"),
    }
    match h2.join().expect("右线程 panic") {
        Ok(info) => println!("右 OK: {}", info.describe()),
        Err(e) => println!("右 失败: {e}"),
    }
    println!("== 测试结束 ==");
}
